#!/usr/bin/env python3
"""Benchmark: B-TEMPLATE-CLI (benchmark/README.md#b-template-cli), engineering A/B.

Motivation: how much less does a caller wait when the CLI restores a prepared VM?
Conclusion sought: five Ready statistics and a paired median-difference 95% CI.
Design: same CLI artifact, rootfs, task, network, workspace/stage and resources;
3 warmups and 60 randomized pairs. Preparation is excluded. All valid slow
samples remain. Output, normal exit, VM Bundle and input checks are mandatory.

Templates are prepared by this same product CLI, using its exact embedded guest
and a never-mounted workspace device. Unsupported selectors fail preflight;
SDK helpers and warm live VMs are never substituted.
"""

import argparse
import contextlib
import fcntl
import json
import os
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import hooks
import timing
import vm_ready

REPO = Path(__file__).resolve().parents[2]
MODES = ("cold", "template")
SEED = 20261027
TEMPLATE_ID = re.compile(r"[0-9a-f]{64}\Z")
INSTANCE_ID = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
# Identical task in both conditions. Reading a fresh token requires the actual
# trial workspace, rather than a workspace captured inside the template.
PAYLOAD = (
    "IFS= read -r token < startup-token || exit 81; "
    "printf 'REFERENCE_TOKEN %s\\n' \"$token\"; "
    "printf 'REFERENCE_INSTANCE %s\\n' \"${PVISOR_INSTANCE_ID:-cold}\"; "
    "printf 'REFERENCE_READY\\n"
    'REFERENCE_RESULT {"mode":"ready","correctness":"passed"}\\n\''
)


def read(path):
    return json.loads(path.read_text())


def save(path, value):
    timing.COMMON.save(path, value)


def inventory(root, catalog=False):
    """Inventory bytes and symlinks, without following directory symlinks."""
    result = {}
    for path in sorted(root.rglob("*")):
        name = str(path.relative_to(root))
        if catalog and (
            name == "catalog.lock"
            or re.fullmatch(
                r"locks/[0-9a-f]{64}\.lock|generations/[0-9a-f]{64}/(?:writer\.lock|store/references\.lock)",
                name,
            )
        ):
            continue  # Only known catalog lock files, never files inside an image.
        if path.is_symlink():
            result[name] = {"symlink": os.readlink(path)}
        elif path.is_file():
            result[name] = {"sha256": timing.COMMON.digest(path), "mode": path.stat().st_mode}
        elif path.is_dir():
            result[name] = {"directory": True, "mode": path.stat().st_mode}
        else:
            raise ValueError(f"unsupported prepared input: {path}")
    return result


def environment():
    env = os.environ.copy()
    env.pop("PVISOR_INSTANCE_ID", None)
    env.update(
        PVISOR_KRUN_ENOMEM_WORKAROUND="0",
        PVISOR_STARTUP_TIMING="0",
        PVISOR_STARTUP_PROFILE="0",
        PVISOR_FS_PROFILE="0",
    )
    return env


def command(binary, root, rootfs, config=None):
    """Use the actual product CLI; only template selection differs."""
    argv = [str(binary), "run", "--no-agent-defaults"]
    if config is not None:
        argv += ["--config", str(config)]
    return argv + [
        "--overlaynet",
        "auto",
        "--stdio",
        "inherit",
        "--timeout",
        "30s",
        "--stage",
        str(root / "current/stage"),
        "--vm",
        "--rootfs",
        str(rootfs),
        "--cpu",
        "2",
        "--memory",
        "128MiB",
        "--",
        "/bin/sh",
        "-c",
        PAYLOAD,
    ]


def order():
    rounds = [list(MODES), list(reversed(MODES))] * (timing.RUNS // 2)
    random.Random(SEED).shuffle(rounds)
    return rounds


def compare(samples):
    """Resample entire pairs, keeping the same indices in both conditions."""
    if set(samples) != set(MODES) or any(len(samples[m]) != timing.RUNS for m in MODES):
        raise ValueError("expected 60 complete cold/template pairs")
    result = {mode: timing.summarize_pairs(samples[mode]) for mode in MODES}
    a, b = ([s["ready_ns"] / 1e6 for s in samples[mode]] for mode in MODES)
    rng = random.Random(SEED + 1)
    differences = []
    for _ in range(10000):
        indices = [rng.randrange(timing.RUNS) for _ in range(timing.RUNS)]
        differences.append(
            statistics.median(b[i] for i in indices) - statistics.median(a[i] for i in indices)
        )
    differences.sort()
    lo, hi = differences[250], differences[9749]
    delta = statistics.median(b) - statistics.median(a)
    result["comparison"] = dict(
        template_minus_cold_ms=delta,
        relative_change_pct=delta / statistics.median(a) * 100,
        paired_bootstrap_95_ci_ms=[lo, hi],
        conclusion="template faster"
        if hi < 0
        else "template slower"
        if lo > 0
        else "no detected difference",
        excluded_samples=0,
    )
    return result


def validate_trial(trial, mode, token):
    """Additional checks beyond the original output/Bundle/stage hooks."""
    text = (trial / "stdout").read_text()
    hooks.validate_output(text, (trial / "stderr").read_text())
    lines = text.splitlines()
    if [line for line in lines if line.startswith("REFERENCE_TOKEN")] != [
        f"REFERENCE_TOKEN {token}"
    ]:
        raise ValueError("guest did not read this trial's fresh workspace token")
    instances = [
        line.removeprefix("REFERENCE_INSTANCE ")
        for line in lines
        if line.startswith("REFERENCE_INSTANCE")
    ]
    if len(instances) != 1:
        raise ValueError("missing or duplicate startup identity")
    identity = instances[0]
    if mode == "cold" and identity != "cold":
        raise ValueError("cold control used template handoff")
    if mode == "template" and (identity == "cold" or not INSTANCE_ID.fullmatch(identity)):
        raise ValueError(
            "template did not report a fresh handoff identity; cold fallback is forbidden"
        )
    if any(
        lines.index(line) > lines.index("REFERENCE_READY")
        for line in (f"REFERENCE_INSTANCE {identity}", f"REFERENCE_TOKEN {token}")
    ):
        raise ValueError("startup evidence must precede Ready")
    bundle = read(trial / "stage/run-bundle.json")
    if (
        bundle["run"]["state"] != "completed"
        or bundle["run"]["exit_code"] != 0
        or bundle["run"]["executor"]["isolation"] != "virtual_machine"
        or bundle["safety"]["filesystem_changes_staged"] is not True
        or any(p.is_file() for p in (trial / "stage/upper").rglob("*"))
    ):
        raise ValueError("invalid VM completion, isolation or staging evidence")
    if (trial / "workspace/startup-canary").read_bytes() != b"unchanged startup input\n" or (
        trial / "workspace/startup-token"
    ).read_text() != token + "\n":
        raise ValueError("host workspace input changed")
    if bundle["run"]["command"] != ["/bin/sh", "-c", PAYLOAD]:
        raise ValueError("Bundle command differs from the common workload")
    if bundle["run"]["executor"]["kind"] != "virtual_machine":
        raise ValueError("expected an actual VM executor")
    return dict(attempt_id=bundle["run"]["attempt_id"], instance_id=identity, token=token)


def trial(root, mode, phase, index, env):
    hooks.prepare(root, "pvisor-vm")
    token = os.urandom(24).hex()
    (root / "workspace/startup-token").write_text(token + "\n")
    archive_index = len(list((root / "trials").iterdir())) if (root / "trials").exists() else 0
    with_root = env | {
        "XDG_CONFIG_HOME": str(root.parent / "config"),
        "PVISOR_RUN_HOME": str(root / "current/runs"),
    }
    try:
        record = timing.capture(
            read(root / "protocol.json")["subject"],
            root / "workspace",
            with_root,
            root / "current/stdout",
        )
    except (ValueError, TimeoutError) as error:
        raise ValueError(
            f"{mode} {phase} failed; inspect {root / 'current/stdout'}: {error}"
        ) from error
    record.update(phase=phase, index=index)
    save(root / "current/timing.json", record)
    (root / "current/stdout.timing.json").unlink()
    hooks.validate(root, "pvisor-vm")
    proof = validate_trial(root / "current", mode, token)
    save(root / "current/startup-validation.json", dict(passed=True, **proof))
    # hooks' workspace symlink points at a reusable directory. Preserve this
    # trial's two input files for offline validation before the next reset.
    (root / "current/workspace").unlink()
    shutil.copytree(root / "workspace", root / "current/workspace")
    hooks.archive(root)
    archived = root / "trials" / f"{archive_index:05d}"
    if not (archived / "timing.json").exists():
        raise ValueError("trial archive is missing")
    return {"ready_ns": record["ready_duration_ns"], "exit_ns": record["exit_duration_ns"]}, proof


def cohort(batch, env, preflight_only=False):
    """Fail the whole cohort on any invalid preflight, warmup or formal trial."""
    samples = {mode: [] for mode in MODES}
    attempts, instances = set(), set()
    schedule = order()
    save(batch / "order.json", schedule)
    phases = [("preflight", 0, MODES)]
    if not preflight_only:
        phases += [("warmup", i, MODES) for i in range(timing.WARMUP)]
        phases += [("sample", i, modes) for i, modes in enumerate(schedule)]
    for phase, index, modes in phases:
        for mode in modes:
            value, proof = trial(batch / mode, mode, phase, index, env)
            if proof["attempt_id"] in attempts:
                raise ValueError("Attempt identity was reused")
            attempts.add(proof["attempt_id"])
            if mode == "template":
                if proof["instance_id"] in instances:
                    raise ValueError("template instance was reused")
                instances.add(proof["instance_id"])
            if phase == "sample":
                samples[mode].append(value)
                save(batch / "samples.json", samples)
            print(f"{phase} {index:02d} {mode}: {value['ready_ns'] / 1e6:.3f} ms", flush=True)
    return None if preflight_only else compare(samples)


def audit(batch):
    """Recompute the comparison from every original timestamp and proof."""
    inputs = read(batch / "inputs.json")
    for name, value in inputs["frozen"].items():
        if timing.COMMON.digest(batch / "frozen" / name) != value:
            raise ValueError("frozen artifact changed: " + name)
    config = read(batch / "configuration.json")
    if config != read(batch / "frozen/configuration.json"):
        raise ValueError("configuration changed")
    for tree in ("rootfs", "catalog"):
        if inventory(Path(config[tree]), catalog=tree == "catalog") != inputs["trees"][tree]:
            raise ValueError("prepared input changed: " + tree)
    cleanup = read(batch / "process-cleanup.json")
    executable = config["subjects"]["cold"][0]
    if (
        cleanup["errors"]
        or not cleanup.get("temporary_executable_removed")
        or any(p["executable"] == executable for p in cleanup["after"])
    ):
        raise ValueError("owned-process cleanup failed")
    if read(batch / "cpu-quiet-gate.json")["passed"] is not True:
        raise ValueError("CPU quiet gate did not pass")
    samples = {mode: [] for mode in MODES}
    attempts, instances = set(), set()
    expected = [("preflight", 0)] + [("warmup", i) for i in range(timing.WARMUP)]
    expected += [("sample", i) for i in range(timing.RUNS)]
    if read(batch / "order.json") != order():
        raise ValueError("predefined randomized order changed")
    observed_order = {}
    for mode in MODES:
        root = batch / mode
        subject = read(root / "protocol.json")["subject"]
        if subject != config["subjects"][mode]:
            raise ValueError("frozen CLI command changed")
        trials = sorted((root / "trials").iterdir())
        if len(trials) != len(expected):
            raise ValueError("missing or extra trials")
        for directory, (phase, index) in zip(trials, expected):
            record = read(directory / "timing.json")
            proof = read(directory / "startup-validation.json")
            for field in (
                "start_ns",
                "ready_ns",
                "end_ns",
                "ready_duration_ns",
                "exit_duration_ns",
            ):
                if type(record[field]) is not int or record[field] <= 0:
                    raise ValueError("invalid original CLI timestamp type/value")
            if (
                record["command"] != subject
                or (record["phase"], record["index"]) != (phase, index)
                or record["exit_code"] != 0
                or not record["start_ns"] < record["ready_ns"] <= record["end_ns"]
                or record["ready_duration_ns"] != record["ready_ns"] - record["start_ns"]
                or record["exit_duration_ns"] != record["end_ns"] - record["start_ns"]
            ):
                raise ValueError("invalid original CLI timing")
            if proof != dict(passed=True, **validate_trial(directory, mode, proof["token"])):
                raise ValueError("startup validation proof changed")
            if proof["attempt_id"] in attempts:
                raise ValueError("Attempt identity was reused")
            attempts.add(proof["attempt_id"])
            if mode == "template":
                if proof["instance_id"] in instances:
                    raise ValueError("template instance was reused")
                instances.add(proof["instance_id"])
            if phase == "sample":
                samples[mode].append(
                    dict(ready_ns=record["ready_duration_ns"], exit_ns=record["exit_duration_ns"])
                )
                observed_order.setdefault(index, []).append(
                    (record["start_ns"], record["end_ns"], mode)
                )
    if read(batch / "samples.json") != samples:
        raise ValueError("sample export differs from original timestamps")
    previous_end = 0
    for index in range(timing.RUNS):
        pairs = sorted(observed_order[index])
        if [entry[2] for entry in pairs] != order()[index]:
            raise ValueError("actual trial order differs from the predefined order")
        for start, end, _ in pairs:
            if start < previous_end:
                raise ValueError("measured trials overlap or are out of order")
            previous_end = end
    return compare(samples)


def monitored_cpus(cpus):
    result = set(cpus)
    for cpu in cpus:
        path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
        for part in path.read_text().strip().split(","):
            bounds = list(map(int, part.split("-")))
            result.update(range(bounds[0], bounds[-1] + 1))
    return result


def prepare_template(binary, batch, source_rootfs, catalog):
    """Untimed product preparation; the returned sealed rootfs serves both arms."""
    command = [
        str(binary),
        "cache",
        "prepare-template",
        "--rootfs",
        str(source_rootfs),
        "--catalog",
        str(catalog),
        "--work-dir",
        str(batch),
        "--name",
        "benchmark",
        "--cpus",
        "2",
        "--memory-mib",
        "128",
    ]
    start = time.perf_counter_ns()
    with (
        (batch / "preparation.stdout").open("wb") as stdout,
        (batch / "preparation.stderr").open("wb") as stderr,
    ):
        child = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env=environment(),
            start_new_session=True,
        )
        try:
            if child.wait(timeout=120) != 0:
                raise ValueError(
                    "CLI template preparation failed: " + (batch / "preparation.stderr").read_text()
                )
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    elapsed = (time.perf_counter_ns() - start) / 1e6
    prepared = read(batch / "preparation.stdout")
    rootfs = Path(prepared["rootfs"]).resolve(strict=True)
    if (
        not TEMPLATE_ID.fullmatch(prepared["version"])
        or not prepared["recipe"]["workspace_device"]
        or prepared["recipe"]["cpus"] != 2
        or prepared["recipe"]["memory_mib"] != 128
        or prepared["recipe"]["compatibility"]["build"] != timing.COMMON.digest(binary)
        or prepared["recipe"]["guest_sha256"] != timing.COMMON.digest(rootfs / "init.krun")
        or Path(prepared["catalog"]).resolve() != catalog.resolve()
    ):
        raise ValueError("preparer returned the wrong template profile")
    save(batch / "preparation.json", dict(command=command, elapsed_ms=elapsed, **prepared))
    return rootfs, prepared["version"], elapsed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--audit", type=Path, help="recompute an existing complete batch; do not launch VMs"
    )
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/pvisor")
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit one machine-readable paired result; progress goes to stderr",
    )
    parser.add_argument(
        "--rootfs",
        type=Path,
        help="source image for automatic preparation; with an existing version, its sealed rootfs",
    )
    parser.add_argument(
        "--catalog",
        type=Path,
        help="private catalog (default: fresh catalog under the result batch)",
    )
    parser.add_argument("--template-version", help="immutable 64-digit template version")
    parser.add_argument(
        "--preparation-ms", type=float, help="separately measured template preparation cost"
    )
    parser.add_argument("--cpu-affinity", default="2,3")
    parser.add_argument(
        "--preflight-only",
        action="store_true",
        help="validate one real startup per mode without producing a performance result",
    )
    args = parser.parse_args()
    if args.audit:
        result = audit(args.audit.resolve(strict=True))
        if result != read(args.audit / "summary.json"):
            raise ValueError("published summary differs from the audited result")
        print(json.dumps(dict(passed=True, result=result), indent=2))
        return
    if args.template_version is not None and (args.catalog is None or args.rootfs is None):
        parser.error("an existing version requires --catalog and its returned --rootfs")
    if args.template_version is not None and not TEMPLATE_ID.fullmatch(args.template_version):
        parser.error("template version must contain exactly 64 lowercase hexadecimal digits")
    source = args.binary.resolve(strict=True)
    rootfs = args.rootfs.resolve(strict=True) if args.rootfs else None
    catalog = args.catalog.resolve() if args.catalog else None
    if not source.is_file() or not os.access(source, os.X_OK):
        parser.error("--binary must be an executable product CLI")
    if rootfs is not None and (not rootfs.is_dir() or rootfs == Path("/")):
        parser.error("rootfs must be a prepared directory; host / is forbidden")
    cpus = set(map(int, args.cpu_affinity.split(",")))
    if len(cpus) != 2 or not cpus <= os.sched_getaffinity(0):
        parser.error("select exactly two available CPUs")
    if args.preparation_ms is not None and not 0 <= args.preparation_ms < float("inf"):
        parser.error("preparation cost must be finite and nonnegative")
    output = REPO / "target/tmp"
    output.mkdir(parents=True, exist_ok=True)
    lock = (output / "vm-ready.lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if vm_ready.processes():
        raise ValueError("existing pVisor processes must be stopped before measuring")
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, vm_ready.interrupted)
    batch = Path(tempfile.mkdtemp(prefix="template-cli-", dir=output))
    runtime = None
    result = None
    manifest = dict(status="preparing", benchmark_id="B-TEMPLATE-CLI", role="engineering A/B")
    save(batch / "manifest.json", manifest)
    print("Results:", batch, flush=True)
    try:
        for tree in (rootfs, catalog):
            if tree is not None and batch.is_relative_to(tree):
                raise ValueError("prepared inputs and benchmark output must be independent")
        os.sched_setaffinity(0, cpus)
        monitored = monitored_cpus(cpus)
        runtime = Path(tempfile.mkdtemp(prefix="pvisor-template-cli-"))
        binary = runtime / "pvisor"
        shutil.copyfile(source, binary)
        binary.chmod(0o755)
        catalog = catalog or batch / "catalog"
        if args.template_version is None:
            if rootfs is None:
                rootfs = batch / "source-rootfs"
                rootfs.mkdir()
                vm_ready.prepare_rootfs(rootfs, binary)
            rootfs, args.template_version, args.preparation_ms = prepare_template(
                binary, batch, rootfs, catalog
            )
        if catalog.is_relative_to(rootfs):
            raise ValueError("the rootfs must not expose its parent catalog")
        frozen = batch / "frozen"
        frozen.mkdir()
        shutil.copy2(binary, frozen / "pvisor")
        config = batch / "template.toml"
        cold_config = batch / "cold.toml"
        cold_config.write_text("[vm]\nrootfs_immutable = true\n")
        config.write_text(
            cold_config.read_text()
            + "\n[vm.template]\ncatalog = "
            + json.dumps(str(catalog))
            + "\nversion = "
            + json.dumps(args.template_version)
            + "\n"
        )
        paths = [
            source,
            binary,
            config,
            cold_config,
            Path(__file__).with_suffix(".json"),
            Path(__file__),
            Path(timing.__file__),
            Path(hooks.__file__),
            Path(vm_ready.__file__),
            Path(timing.COMMON.__file__),
        ]
        hashes = {str(path): timing.COMMON.digest(path) for path in paths}
        for path in paths[2:]:
            target = frozen / path.relative_to(REPO)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
        before = {"rootfs": inventory(rootfs), "catalog": inventory(catalog, catalog=True)}
        (batch / "config").mkdir(mode=0o700)
        subjects = {}
        for mode in MODES:
            root = batch / mode
            root.mkdir()
            subjects[mode] = command(
                binary, root, rootfs, config if mode == "template" else cold_config
            )
            save(
                root / "protocol.json",
                dict(backends=["pvisor-vm"], assets=None, subject=subjects[mode]),
            )
        save(
            batch / "configuration.json",
            dict(
                binary_sha256=hashes[str(source)],
                rootfs=str(rootfs),
                catalog=str(catalog),
                template_version=args.template_version,
                template_preparation_ms=args.preparation_ms,
                subjects=subjects,
                host_kernel=os.uname().release,
                cpus=sorted(cpus),
                monitored_cpus=sorted(monitored),
                guest_cpus=2,
                guest_memory_mib=128,
                network="auto",
                runs=timing.RUNS,
                warmup=timing.WARMUP,
                preflight_only=args.preflight_only,
                seed=SEED,
                metric_scope=timing.COMMON.READY_EXIT_SCOPE,
                checkout_head=subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=REPO, text=True
                ).strip(),
                artifact_source="not inferred from checkout; identify supplied binary by SHA-256",
            ),
        )
        shutil.copy2(batch / "configuration.json", frozen / "configuration.json")
        frozen_hashes = {
            str(p.relative_to(frozen)): timing.COMMON.digest(p)
            for p in frozen.rglob("*")
            if p.is_file()
        }
        save(batch / "inputs.json", dict(files=hashes, frozen=frozen_hashes, trees=before))
        save(batch / "environment-before.json", vm_ready.environment(monitored))
        vm_ready.wait_quiet(monitored, batch / "cpu-quiet-gate.json")
        manifest["status"] = "measuring"
        save(batch / "manifest.json", manifest)
        result = cohort(batch, environment(), preflight_only=args.preflight_only)
        if before != {"rootfs": inventory(rootfs), "catalog": inventory(catalog, catalog=True)}:
            raise ValueError("prepared rootfs or published catalog changed")
        if any(timing.COMMON.digest(path) != value for path, value in hashes.items()):
            raise ValueError("binary, configuration or observer changed")
    except BaseException as error:
        manifest.update(status="failed", error=str(error))
        save(batch / "manifest.json", manifest)
        raise
    finally:
        try:
            for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                signal.signal(signum, signal.SIG_IGN)
            vm_ready.cleanup(runtime, batch)
        except BaseException as error:
            manifest.update(status="failed", error=f"cleanup: {error}")
            save(batch / "manifest.json", manifest)
            raise
    if args.preflight_only:
        manifest["status"] = "preflight-passed"
        save(batch / "manifest.json", manifest)
    # Never expose a successful comparison before owned-process cleanup passes.
    if result is not None:
        try:
            if read(frozen / "configuration.json") != read(batch / "configuration.json"):
                raise ValueError("configuration changed")
            if audit(batch) != result:
                raise ValueError("independent audit disagrees with the measured result")
        except BaseException as error:
            manifest.update(status="failed", error=f"audit: {error}")
            save(batch / "manifest.json", manifest)
            raise
        save(
            batch / "audit.json",
            dict(passed=True, trials=2 * (1 + timing.WARMUP + timing.RUNS), excluded_samples=0),
        )
        save(batch / "summary.json", result)
        rows = [
            "| Condition | N | Ready median ms | Ready mean ms | Ready SD ms | Ready min ms | Ready max ms |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
        for mode in MODES:
            fields = [str(result[mode]["N"])] + [
                f"{result[mode][f]:.3f}" for f in timing.READY_FIELDS[1:]
            ]
            rows.append("| " + mode + " | " + " | ".join(fields) + " |")
        comparison = result["comparison"]
        lo, hi = comparison["paired_bootstrap_95_ci_ms"]
        report = "\n".join(rows) + (
            f"\n\nTemplate minus cold: {comparison['template_minus_cold_ms']:.3f} ms "
            f"({comparison['relative_change_pct']:+.2f}%). "
            f"95% paired bootstrap interval: [{lo:.3f}, {hi:.3f}] ms.\n"
            f"Conclusion: {comparison['conclusion']}; no samples excluded.\n"
            f"Template preparation (outside Ready): {args.preparation_ms} ms.\n"
        )
        (batch / "report.md").write_text(report)
        manifest["status"] = "passed"
        save(batch / "manifest.json", manifest)
        print(report, end="")
    return result


if __name__ == "__main__":
    try:
        if "--json" in sys.argv and "--audit" not in sys.argv:
            with contextlib.redirect_stdout(sys.stderr):
                result = main()
            if result is not None:
                row = {"N": result["cold"]["N"]}
                for mode in MODES:
                    row.update(
                        {mode + " " + key: result[mode][key] for key in timing.READY_FIELDS[1:]}
                    )
                row["template minus cold ms"] = result["comparison"]["template_minus_cold_ms"]
                print(json.dumps(row))
        else:
            main()
    except (ValueError, OSError, subprocess.SubprocessError, TimeoutError, RuntimeError) as error:
        sys.exit(str(error))
