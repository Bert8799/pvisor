#!/usr/bin/env python3
"""Fixed pVisor memory baseline wrapper (B-MEMORY-SCALE engineering preflight).

This script does not measure by itself. It freezes the host/input/command
manifest, checks the required host state, and then runs the existing
``pool_compare.py`` preflight with one of three fixed profiles.

Run as the benchmark user (not root). Host setup is an administrator action;
this script only checks and records KSM, userfaultfd, CPU controller, KVM and
frequency state.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import pwd
import re
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))

from scanner_compare import digest, kernel_state, require_default_ksm  # noqa: E402

PVISOR_ARMS = (
    "pvisor-off",
    "pvisor-physical",
    "pvisor-ksm",
    "pvisor-local",
    "pvisor-compressed",
)
PATTERNS = ("repeated", "random-shared", "random-unique")
PROFILES = {
    "quick": (("pvisor-off", "pvisor-physical", "pvisor-ksm"), ("repeated", "random-shared")),
    "standard": (PVISOR_ARMS, ("repeated", "random-shared")),
    "full": (PVISOR_ARMS, PATTERNS),
}
ESTIMATED_MINUTES = {"quick": (10, 15), "standard": (20, 25), "full": (30, 40)}
FIXED = {
    "vms": 4,
    "memory_mib": 256,
    "cpu_affinity": "0-3",
    "cpu_quota": "400000 100000",
    "memory_max": 2**31,
    "memory_swap_max": 0,
    "tasks_max": 256,
    "idle_seconds": 60,
    "post_write_seconds": 30,
}
STAMP_RE = re.compile(r"^\d{8}T\d{6}\.\d{9}Z$")


class BaselineError(RuntimeError):
    """The host or inputs do not match the fixed baseline contract."""


def utc_stamp_ns() -> str:
    """UTC timestamp with nanosecond precision for the evidence directory."""
    ns = time.time_ns()
    return time.strftime("%Y%m%dT%H%M%S", time.gmtime(ns // 1_000_000_000)) + (
        f".{ns % 1_000_000_000:09d}Z"
    )


def read_text(path: Path) -> str | None:
    try:
        return path.read_text().strip()
    except (OSError, UnicodeError):
        return None


def read_json(path: Path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise BaselineError(f"cannot read {path}: {error}") from error


def save(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def source_info() -> dict:
    def git(*args: str) -> str:
        return subprocess.check_output(["git", "-C", str(REPO), *args], text=True).strip()

    patch = subprocess.check_output(
        ["git", "-C", str(REPO), "diff", "--binary", "HEAD"], text=False
    )
    return {
        "head": git("rev-parse", "HEAD"),
        "status": git("status", "--porcelain=v1", "--untracked-files=all"),
        "dirty": bool(git("status", "--porcelain=v1", "--untracked-files=all")),
        "patch_sha256": digest_bytes(patch),
    }


def digest_bytes(data: bytes) -> str:
    import hashlib

    return hashlib.sha256(data).hexdigest()


def harness_info() -> dict:
    files = (
        "baseline.py",
        "baseline_analysis.py",
        "baseline_evidence.py",
        "plot_pool.py",
        "pool_compare.py",
        "prepare_pool.py",
        "scanner_compare.py",
        "docker_support.py",
    )
    return {name: digest(HERE / name) for name in files}


def create_run_directory(
    profile: str, *, parent: Path | None = None, stamp: str | None = None
) -> Path:
    """Create a real profile-date-sequence directory without replacing records."""
    if profile not in PROFILES:
        raise BaselineError("invalid baseline profile")
    stamp = stamp or utc_stamp_ns()
    if not STAMP_RE.fullmatch(stamp):
        raise BaselineError("invalid run UTC timestamp")
    parent = (parent or REPO / "target/tmp").resolve()
    parent.mkdir(parents=True, exist_ok=True)
    prefix = f"{profile}-{stamp[:8]}-"
    sequence = max(
        (
            int(p.name[len(prefix) :])
            for p in parent.glob(prefix + "*")
            if p.name[len(prefix) :].isdigit()
        ),
        default=0,
    )
    while True:
        sequence += 1
        output = parent / f"{prefix}{sequence}"
        try:
            output.mkdir()
            return output
        except FileExistsError:
            continue


def inputs_info(inputs: Path) -> dict:
    receipt_path = inputs / "receipt.json"
    if not receipt_path.is_file():
        raise BaselineError(f"missing frozen input receipt: {receipt_path}")
    receipt = read_json(receipt_path)
    hashes = receipt.get("hashes")
    if not isinstance(hashes, dict) or not hashes:
        raise BaselineError("input receipt has no hashes")
    for name, expected in hashes.items():
        path = inputs / name
        if not path.is_file() or digest(path) != expected:
            raise BaselineError(f"frozen input mismatch: {path}")
    for name, key in (
        ("source.tar.gz", "source_archive_sha256"),
        ("source-files.json", "source_manifest_sha256"),
    ):
        if digest(inputs / name) != receipt.get(key):
            raise BaselineError(f"frozen product source mismatch: {name}")
    from prepare_pool import source_inputs

    if read_json(inputs / "source-files.json") != source_inputs():
        raise BaselineError(
            "frozen binaries do not match current product source; prepare new inputs"
        )
    if not {"probe", "daemon", "firmware", "worker", "worker.rs", "root/worker"} <= set(hashes):
        raise BaselineError("missing required pVisor input hashes")
    return {
        "path": str(inputs.resolve()),
        "receipt_sha256": digest(receipt_path),
        "source_head": receipt.get("source_head"),
        "source_dirty": receipt.get("source_dirty"),
        "profile": receipt.get("profile"),
        "hashes": hashes,
    }


def collect_frequency() -> dict:
    cpus = {}
    for cpu in range(4):
        base = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        cpus[str(cpu)] = {
            name: read_text(base / name)
            for name in (
                "scaling_governor",
                "scaling_cur_freq",
                "scaling_min_freq",
                "scaling_max_freq",
                "cpuinfo_min_freq",
                "cpuinfo_max_freq",
                "cpb",
            )
        }
    return {
        "cpus": cpus,
        "boost": read_text(Path("/sys/devices/system/cpu/cpufreq/boost")),
        "amd_pstate_status": read_text(Path("/sys/devices/system/cpu/amd_pstate/status")),
    }


def collect_ksm() -> dict:
    return {
        path.name: path.read_text().strip()
        for path in sorted(Path("/sys/kernel/mm/ksm").iterdir())
        if path.is_file()
    }


def collect_numa() -> dict:
    root = Path("/sys/devices/system/node")
    nodes = {}
    for directory in sorted(root.glob("node[0-9]*")):
        nodes[directory.name] = {
            name: read_text(directory / name) for name in ("cpulist", "meminfo", "distance")
        }
    return {"online": read_text(root / "online"), "nodes": nodes}


def collect_cgroup() -> dict:
    uid = os.getuid()
    user_slice = Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice")
    user_service = user_slice / f"user@{uid}.service"
    app_slice = user_service / "app.slice"
    return {
        "root_controllers": read_text(Path("/sys/fs/cgroup/cgroup.controllers")),
        "root_subtree_control": read_text(Path("/sys/fs/cgroup/cgroup.subtree_control")),
        "user_slice": str(user_slice),
        "user_slice_controllers": read_text(user_slice / "cgroup.controllers"),
        "user_slice_subtree_control": read_text(user_slice / "cgroup.subtree_control"),
        "user_service_controllers": read_text(user_service / "cgroup.controllers"),
        "user_service_subtree_control": read_text(user_service / "cgroup.subtree_control"),
        "app_slice_controllers": read_text(app_slice / "cgroup.controllers"),
        "app_slice_subtree_control": read_text(app_slice / "cgroup.subtree_control"),
        "app_slice_cpu_max": read_text(app_slice / "cpu.max"),
    }


def collect_devices() -> dict:
    result = {}
    for name in ("kvm", "fuse", "userfaultfd"):
        path = Path("/dev") / name
        try:
            stat = path.stat()
            result[name] = {
                "mode": oct(stat.st_mode & 0o7777),
                "uid": stat.st_uid,
                "gid": stat.st_gid,
            }
        except OSError as error:
            result[name] = {"error": str(error)}
    return result


def collect_cpu() -> dict:
    wanted = {
        "vendor_id",
        "model name",
        "cpu family",
        "model",
        "stepping",
        "microcode",
        "physical id",
        "core id",
    }
    first = {}
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if ":" in line:
            key, value = (part.strip() for part in line.split(":", 1))
            if key in wanted and key not in first:
                first[key] = value
    return first


def collect_host() -> dict:
    meminfo = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            meminfo[key] = value.strip()
    return {
        "stamp_utc": utc_stamp_ns(),
        "hostname": socket.gethostname(),
        "uname": platform.uname()._asdict(),
        "os_release": read_text(Path("/etc/os-release")),
        "cpu": collect_cpu(),
        "cpu_frequency": collect_frequency(),
        "numa": collect_numa(),
        "page_size": os.sysconf("SC_PAGE_SIZE"),
        "meminfo": meminfo,
        "swap": read_text(Path("/proc/swaps")),
        "cgroup": collect_cgroup(),
        "devices": collect_devices(),
        "ksm": collect_ksm(),
        "unprivileged_userfaultfd": read_text(Path("/proc/sys/vm/unprivileged_userfaultfd")),
    }


def check_host(
    arms: tuple[str, ...], require_governor: str | None, require_boost: str | None
) -> None:
    if os.geteuid() == 0:
        raise BaselineError("run the baseline as the benchmark user, not as root")
    target_user = pwd.getpwuid(os.getuid()).pw_name
    if os.environ.get("PVISOR_BENCH_USER") not in (None, target_user):
        raise BaselineError("PVISOR_BENCH_USER does not match the current user")

    try:
        require_default_ksm(kernel_state())
    except Exception as error:
        raise BaselineError(
            "KSM must be run=1 with default parameters for this baseline; "
            "run the administrator pre script with --ksm"
        ) from error

    if "pvisor-local" in arms or "pvisor-compressed" in arms:
        try:
            fd = os.open("/dev/userfaultfd", os.O_RDWR | os.O_CLOEXEC)
            os.close(fd)
        except OSError as error:
            raise BaselineError(
                "pvisor-local/pvisor-compressed need /dev/userfaultfd access"
            ) from error

    try:
        fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
        os.close(fd)
    except OSError as error:
        raise BaselineError("the benchmark user cannot open /dev/kvm") from error

    uid = os.getuid()
    user_slice = Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice")
    user_service = user_slice / f"user@{uid}.service"
    app_slice = user_service / "app.slice"
    for directory in (Path("/sys/fs/cgroup/user.slice"), user_slice, user_service, app_slice):
        if not directory.is_dir():
            raise BaselineError(f"missing cgroup path: {directory}")
        control = read_text(directory / "cgroup.subtree_control") or ""
        if "cpu" not in control.split():
            raise BaselineError(
                f"cgroup cpu controller is not delegated at {directory}; "
                "run the administrator pre script"
            )
    if not (app_slice / "cpu.max").is_file():
        raise BaselineError(f"missing {app_slice / 'cpu.max'}; run the administrator pre script")

    frequency = collect_frequency()
    governors = {
        entry["scaling_governor"]
        for entry in frequency["cpus"].values()
        if entry["scaling_governor"] is not None
    }
    if require_governor is not None and governors != {require_governor}:
        raise BaselineError(
            f"CPU governor mismatch: expected {require_governor!r}, got {sorted(governors)!r}"
        )
    if require_boost is not None and frequency["boost"] != require_boost:
        raise BaselineError(
            f"CPU boost mismatch: expected {require_boost!r}, got {frequency['boost']!r}"
        )


def selected_arms(profile: str, selection: str | None) -> tuple[str, ...]:
    allowed = PROFILES[profile][0]
    if selection is None:
        return allowed
    arms = tuple(selection.split(","))
    if not arms or len(set(arms)) != len(arms) or not set(arms) <= set(allowed):
        raise BaselineError("arms must be a nonempty unique subset of the selected profile")
    return arms


def selected_patterns(profile: str, selection: str | None) -> tuple[str, ...]:
    allowed = PROFILES[profile][1]
    patterns = tuple(selection.split(",")) if selection is not None else allowed
    if not patterns or len(set(patterns)) != len(patterns) or not set(patterns) <= set(allowed):
        raise BaselineError("patterns must be a nonempty unique subset of the selected profile")
    return patterns


def build_command(
    profile: str,
    inputs: Path,
    output: Path,
    *,
    shared_file_cache: bool = False,
    zero_pages: bool = False,
    arms: tuple[str, ...] | None = None,
    patterns: tuple[str, ...] | None = None,
    firmware_copy: bool = False,
) -> list[str]:
    profile_arms, _ = PROFILES[profile]
    arms = selected_arms(profile, ",".join(arms)) if arms is not None else profile_arms
    patterns = selected_patterns(profile, ",".join(patterns) if patterns is not None else None)
    return [
        sys.executable,
        str(HERE / "pool_compare.py"),
        "--inputs",
        str(inputs),
        "--output",
        str(output),
        "--preflight",
        "--arms",
        ",".join(arms),
        "--patterns",
        ",".join(patterns),
        "--baseline-state",
        str(output.parent / "guard-state.json"),
        "--shared-file-cache",
        str(int(shared_file_cache)),
        "--zero-pages",
        str(int(zero_pages)),
        "--firmware-copy",
        str(int(firmware_copy)),
    ]


def write_summary(output: Path, cohort: Path) -> dict:
    from baseline_analysis import analyze

    summary = analyze(cohort, output / "analysis")
    save(output / "summary.json", summary)
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILES), default="full")
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument(
        "--arms", help="explicit engineering subset of profile arms; not a complete profile"
    )
    parser.add_argument("--patterns", help="explicit engineering subset of profile patterns")
    parser.add_argument(
        "--firmware-copy",
        type=int,
        choices=(0, 1),
        default=0,
        help="copy firmware to a fresh inode inside the measured group",
    )
    parser.add_argument(
        "--zero-pages",
        type=int,
        choices=(0, 1),
        default=0,
        help="experimental anonymous-zero replacement for the physical pool",
    )
    parser.add_argument(
        "--shared-file-cache",
        type=int,
        choices=(0, 1),
        default=0,
        help="experimental physical-pool descriptor authority cache A/B",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="write manifest and print command only"
    )
    parser.add_argument("--require-governor", help="reject if CPUs 0-3 use another governor")
    parser.add_argument("--require-boost", help="reject if cpufreq boost differs")
    args = parser.parse_args()

    try:
        arms = selected_arms(args.profile, args.arms)
        patterns = selected_patterns(args.profile, args.patterns)
    except BaselineError as error:
        parser.error(str(error))
    profile_arms, profile_patterns = PROFILES[args.profile]
    fraction = len(arms) * len(patterns) / (len(profile_arms) * len(profile_patterns))
    subset = fraction != 1
    inputs = args.inputs.expanduser().resolve()
    if args.output:
        output = args.output.expanduser().resolve()
        if output.exists():
            print(f"refusing to reuse output directory: {output}", file=sys.stderr)
            return 2
        output.mkdir(parents=True)
    else:
        output = create_run_directory(args.profile)
    if len(os.fsencode(output / "cohort" / "14")) > 70:
        print(
            "use a shorter output path: condition directory must be at most 70 bytes",
            file=sys.stderr,
        )
        output.rmdir()
        return 2

    try:
        from baseline_analysis import STEADY
        from baseline_evidence import (
            fingerprint,
            freeze_source,
            host_config,
            input_files,
            inventory,
            verify,
        )

        host = collect_host()
        if not args.dry_run:
            check_host(arms, args.require_governor, args.require_boost)
        inputs_manifest = inputs_info(inputs)
        source = source_info()
        source_files = freeze_source(output)
        (output / "source.patch").write_bytes(
            subprocess.check_output(["git", "-C", str(REPO), "diff", "--binary", "HEAD"])
        )
        command = build_command(
            args.profile,
            inputs,
            output / "cohort",
            shared_file_cache=bool(args.shared_file_cache),
            zero_pages=bool(args.zero_pages),
            arms=arms,
            patterns=patterns,
            firmware_copy=bool(args.firmware_copy),
        )
        manifest = {
            "schema": "pvisor-memory-baseline/v2",
            "created_utc": utc_stamp_ns(),
            "profile": args.profile,
            "shared_file_cache": bool(args.shared_file_cache),
            "zero_pages": bool(args.zero_pages),
            "firmware_copy": bool(args.firmware_copy),
            "profile_subset": subset,
            "estimated_minutes": [
                max(1, round(minutes * fraction)) for minutes in ESTIMATED_MINUTES[args.profile]
            ],
            "arms": arms,
            "patterns": patterns,
            "fixed": FIXED,
            "steady_rule": STEADY,
            "host": host,
            "inputs": inputs_manifest,
            "source": source,
            "source_archive_sha256": digest(output / "source.tar.gz"),
            "source_manifest_sha256": digest(output / "source-files.json"),
            "harness": harness_info(),
            "command": command,
            "evidence_scope": "engineering B-MEMORY-SCALE preflight; not a user benchmark",
        }
        save(output / "manifest.json", manifest)
        save(output / "command.json", {"command": command})
        frozen = output / "harness"
        frozen.mkdir()
        for name in manifest["harness"]:
            shutil.copy2(HERE / name, frozen / name)
        state = {
            "repo": str(REPO),
            "inputs_path": str(inputs),
            "source": source_files,
            "source_stat": inventory(content=False),
            "inputs": input_files(inputs),
            "inputs_stat": {
                str(p.relative_to(inputs)): fingerprint(p, content=False)
                for p in [inputs, *sorted(inputs.rglob("*"))]
            },
            "host_config": host_config(),
        }
        save(output / "guard-state.json", state)
        save(
            output / "baseline.json",
            {
                "complete": False,
                "evidence_valid": False,
                "status": "prepared" if args.dry_run else "running",
                "profile_subset": subset,
            },
        )
        print(f"baseline: {output}", file=sys.stderr)
        if args.dry_run:
            print(json.dumps(command))
            return 0
        verify(state)
        completed = subprocess.run(command, check=False, timeout=3600)
        save(output / "exit.json", {"returncode": completed.returncode})
        if completed.returncode:
            raise BaselineError(f"pool_compare failed with exit {completed.returncode}")
        verify(state)
        post_host = collect_host()
        save(output / "post-host.json", post_host)
        summary = write_summary(output, output / "cohort")
        if not summary["evidence_sufficient"]:
            raise BaselineError(
                "measurement complete but startup/steady sampling evidence insufficient"
            )
        verify(state)
        save(
            output / "baseline.json",
            {
                "complete": True,
                "evidence_valid": True,
                "host_stable": True,
                "profile_subset": subset,
                "status": "valid",
                "summary_sha256": digest(output / "summary.json"),
            },
        )
        print(output / "baseline.json")
        return 0
    except (ValueError, BaselineError, OSError, subprocess.SubprocessError) as error:
        save(
            output / "baseline.json",
            {
                "complete": False,
                "evidence_valid": False,
                "status": "failed",
                "error": str(error),
                "profile_subset": subset,
            },
        )
        save(output / "error.json", {"error": str(error)})
        print(str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
