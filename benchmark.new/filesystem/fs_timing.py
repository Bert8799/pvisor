"""B-FS-TOOLS: complete native Python commands, 3 warmups and 30 samples.

Motivation: quantify native metadata/read command costs on a prepared warm tree.
Design: 2048 files / 32 directories, one shared 64 MiB payload, perf_counter_ns
around process launch and normal exit. Interpreter, pipes and subject validation
are included; fixture preparation, observer startup and later audits are excluded.
These probes are distinct from seven-tool internal timings and cross-backend tasks.
"""

import argparse
import json
import statistics
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import FILESYSTEM_SCOPE as SCOPE
from common import ROOT, digest, save
from workload import MODES, validate_fixture

RUNS = 30
WARMUP = 3
TIMEOUT = 30
FIXTURE_SCOPE = "native Python whole-command probes; 2048 files in 32 directories; 64 MiB read with SHA256; warm cache; distinct from seven-tool internal timings"
FIELDS = ("N", "Median ms", "Mean ms", "SD ms", "Min ms", "Max ms")
ROWS = {"native-" + mode: mode for mode in MODES}


def command(batch, mode):
    return [sys.executable, str(ROOT / "filesystem/workload.py"), mode, str(batch / "fixture")]


def summarize(samples):
    if len(samples) < 2 or any(type(n) is not int or n <= 0 for n in samples):
        raise ValueError("at least two positive integer durations required")
    values = [n / 1_000_000 for n in samples]
    return dict(
        zip(
            FIELDS,
            (
                len(values),
                statistics.median(values),
                statistics.mean(values),
                statistics.stdev(values),
                min(values),
                max(values),
            ),
        )
    )


def validate_result(record, mode):
    if record["exit_code"] != 0 or json.loads(record["stdout"]) != dict(
        workload=mode, correctness="passed"
    ):
        raise ValueError("filesystem subject failed or returned incorrect output")


def run_trial(batch, mode, phase, index):
    argv = command(batch, mode)
    record = dict(
        command=argv,
        phase=phase,
        index=index,
        start_ns=None,
        end_ns=None,
        duration_ns=None,
        exit_code=None,
        stdout="",
        stderr="",
    )
    log = batch / "trials" / mode / f"{phase}-{index:03d}.json"
    try:
        record["start_ns"] = time.perf_counter_ns()
        result = subprocess.run(argv, cwd=batch, capture_output=True, text=True, timeout=TIMEOUT)
        record["end_ns"] = time.perf_counter_ns()
        record.update(
            duration_ns=record["end_ns"] - record["start_ns"],
            exit_code=result.returncode,
            stdout=result.stdout,
            stderr=result.stderr,
        )
        validate_result(record, mode)
        return record["duration_ns"]
    except subprocess.TimeoutExpired as error:
        record.update(
            error=str(error),
            stdout=(error.stdout or b"").decode(errors="replace"),
            stderr=(error.stderr or b"").decode(errors="replace"),
        )
        raise
    except BaseException as error:
        record["error"] = str(error)
        raise
    finally:
        save(log, record)


def measure(batch, mode):
    manifest = json.loads((batch / "manifest.json").read_text())
    if (
        manifest["status"] != "prepared"
        or manifest["metric_scope"] != SCOPE
        or "native-" + mode not in manifest["rows"]
    ):
        raise ValueError("expected a freshly prepared filesystem row")
    if list((batch / "trials" / mode).glob("*.json")) != [
        batch / "trials" / mode / "preflight-000.json"
    ]:
        raise ValueError("do not reuse a measured filesystem row")
    for path, expected in manifest["artifacts"].items():
        if digest(path) != expected:
            raise ValueError("prepared artifact changed: " + path)
    validate_fixture(batch / "fixture")
    for index in range(WARMUP):
        run_trial(batch, mode, "warmup", index)
    samples = [run_trial(batch, mode, "sample", index) for index in range(RUNS)]
    validate_fixture(batch / "fixture")
    result = summarize(samples)
    save(
        batch / f"{mode}.samples.json",
        dict(engine="python-perf_counter_ns", unit="nanosecond", samples=samples, result=result),
    )
    return result


def main(mode):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("batch", type=Path)
    args = parser.parse_args()
    batch = args.batch.resolve()
    try:
        print(json.dumps(measure(batch, mode)))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        manifest = json.loads((batch / "manifest.json").read_text())
        manifest.update(status="failed", error=str(error))
        save(batch / "manifest.json", manifest)
        parser.exit(1, f"{error}\n")
