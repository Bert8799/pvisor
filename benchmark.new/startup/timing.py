#!/usr/bin/env python3
"""B-STARTUP: observe paired Ready and normal Exit for a prepared backend.

Motivation: waiting for a usable prepared VM determines interaction latency.
Conclusion sought: launch-to-first-valid-output latency at 2 vCPU / 128 MiB.
Design: 3 warmups, 60 samples, fresh stage/workspace, warm host caches; output,
normal exit and any pVisor Run Bundle must pass after timing stops. Interpreter
startup, input preparation and teardown are excluded; Popen and pipe observation
are included. Exit uses the same start and includes work through normal exit.
Measurements use the Python perf_counter_ns clock.
"""

import argparse
import importlib.util
import json
import os
import selectors
import signal
import statistics
import subprocess
import time
from pathlib import Path

from hooks import archive, cleanup_container, prepare, validate

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("ready_common", ROOT / "common.py")
COMMON = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(COMMON)
RUNS = 60
WARMUP = 3
TIMEOUT = 30
READY_FIELDS = (
    "N",
    "Ready median ms",
    "Ready mean ms",
    "Ready SD ms",
    "Ready min ms",
    "Ready max ms",
)
EXIT_FIELDS = tuple(field.replace("Ready", "Exit") for field in READY_FIELDS)
FIELDS = (*READY_FIELDS, *EXIT_FIELDS[1:])


def summarize(samples, label="Ready"):
    if len(samples) < 2 or any(
        not isinstance(n, int) or isinstance(n, bool) or n <= 0 for n in samples
    ):
        raise ValueError("at least two positive integer Ready durations are required")
    ms = [n / 1_000_000 for n in samples]
    return dict(
        zip(
            tuple(field.replace("Ready", label) for field in READY_FIELDS),
            [
                len(ms),
                statistics.median(ms),
                statistics.mean(ms),
                statistics.stdev(ms),
                min(ms),
                max(ms),
            ],
        )
    )


def summarize_pairs(samples):
    if any(
        set(s) != {"ready_ns", "exit_ns"}
        or type(s["ready_ns"]) is not int
        or type(s["exit_ns"]) is not int
        or not 0 < s["ready_ns"] <= s["exit_ns"]
        for s in samples
    ):
        raise ValueError("Ready and Exit must share one valid trial")
    return summarize([s["ready_ns"] for s in samples]) | summarize(
        [s["exit_ns"] for s in samples], "Exit"
    )


def capture(argv, cwd, env, log, timeout=TIMEOUT):
    """Start the clock immediately before Popen; stop at the exact line receipt.

    Drain output and wait for normal exit afterwards. Timeout/failure kills only
    this trial's owned process group, preserving output and diagnostic timing.
    """
    record = {
        "command": argv,
        "start_ns": None,
        "ready_ns": None,
        "end_ns": None,
        "exit_code": None,
    }
    process = None
    try:
        with log.open("ab", buffering=0) as output, selectors.DefaultSelector() as selector:
            start = time.perf_counter_ns()
            record["start_ns"] = start
            process = subprocess.Popen(
                argv,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                bufsize=0,
                start_new_session=True,
            )
            selector.register(process.stdout, selectors.EVENT_READ)
            pending = b""
            deadline = start / 1e9 + timeout
            while selector.get_map():
                remaining = deadline - time.perf_counter()
                if remaining <= 0:
                    raise TimeoutError("Ready trial did not finish normally within timeout")
                for key, _ in selector.select(min(remaining, 0.1)):
                    block = os.read(key.fd, 65536)
                    received = time.perf_counter_ns()
                    if not block:
                        selector.unregister(key.fileobj)
                        continue
                    output.write(block)
                    pending += block
                    while b"\n" in pending:
                        line, pending = pending.split(b"\n", 1)
                        if line.rstrip(b"\r") == b"REFERENCE_READY":
                            if record["ready_ns"] is not None:
                                raise ValueError("duplicate Ready marker")
                            record["ready_ns"] = received
            record["exit_code"] = process.wait(timeout=max(0.001, deadline - time.perf_counter()))
            record["end_ns"] = time.perf_counter_ns()
            if record["exit_code"] != 0 or record["ready_ns"] is None:
                raise ValueError("trial failed or did not emit an exact Ready line")
            record["ready_duration_ns"] = record["ready_ns"] - record["start_ns"]
            record["exit_duration_ns"] = record["end_ns"] - record["start_ns"]
            return record
    except BaseException as error:
        record["error"] = str(error)
        raise
    finally:
        if process is not None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            process.stdout.close()
        COMMON.save(log.with_suffix(".timing.json"), record)


def run_trial(batch, phase, index, reset=True):
    protocol = json.loads((batch / "protocol.json").read_text())
    backend = protocol["backends"][0]
    if reset:
        prepare(batch, backend)
    env = os.environ.copy() | {
        "PVISOR_RUN_HOME": str(batch / "current/runs"),
        "PVISOR_STARTUP_TIMING": "0",
        "PVISOR_FS_PROFILE": "0",
    }
    record = capture(protocol["subject"], batch / "workspace", env, batch / "current/stdout")
    record.update(phase=phase, index=index)
    COMMON.save(batch / "current/timing.json", record)
    (batch / "current/stdout.timing.json").unlink()
    validate(batch, backend)
    archive(batch)
    return {"ready_ns": record["ready_duration_ns"], "exit_ns": record["exit_duration_ns"]}


def measure(batch):
    manifest = json.loads((batch / "manifest.json").read_text())
    if manifest["status"] != "prepared" or manifest["metric_scope"] != COMMON.READY_EXIT_SCOPE:
        raise ValueError("expected a freshly prepared Ready / Exit batch")
    if len(list((batch / "trials").glob("*/timing.json"))) != 1:
        raise ValueError("Ready batch must have only its preflight; do not reuse a measured batch")
    for path, sha in manifest["artifacts"].items():
        if COMMON.digest(path) != sha:
            raise ValueError("prepared artifact changed: " + path)
    for i in range(WARMUP):
        run_trial(batch, "warmup", i)
    samples = [run_trial(batch, "sample", i) for i in range(RUNS)]
    result = summarize_pairs(samples)
    COMMON.save(
        batch / "timing.samples.json",
        {
            "engine": "python-perf_counter_ns",
            "unit": "nanosecond",
            "samples": samples,
            "result": result,
        },
    )
    return result


def main(backend):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("batch", type=Path)
    args = parser.parse_args()
    try:
        batch = args.batch.resolve()
        protocol = json.loads((batch / "protocol.json").read_text())
        if protocol["backends"] != [backend]:
            raise ValueError("script backend differs from prepared protocol")
        print(json.dumps(measure(batch)))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        if (args.batch / "manifest.json").exists():
            manifest = json.loads((args.batch / "manifest.json").read_text())
            if manifest.get("status") == "prepared" and manifest.get("conditions") == [backend]:
                manifest.update(status="failed", error=str(error))
                COMMON.save(args.batch / "manifest.json", manifest)
        if (args.batch / "protocol.json").exists():
            cleanup_container(args.batch)
        parser.exit(1, f"{error}\n")
