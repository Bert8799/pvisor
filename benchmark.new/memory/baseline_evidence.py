"""Freeze and guard local baseline inputs without changing host settings."""

import json
import os
import stat
import subprocess
import tarfile
import time
from pathlib import Path

from scanner_compare import TUNABLES, digest, kernel_state

REPO = Path(__file__).resolve().parents[2]


def source_files(repo=REPO):
    """Inventory tracked and nonignored untracked source, including deletions."""
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo
    )
    return sorted({os.fsdecode(n) for n in names.split(b"\0") if n})


def fingerprint(path, *, content=True):
    try:
        s = path.lstat()
    except FileNotFoundError:
        return {"absent": True}
    result = {"mode": stat.S_IMODE(s.st_mode)}
    if path.is_symlink():
        result["symlink"] = os.readlink(path)
    elif path.is_file():
        result.update(bytes=s.st_size)
        if content:
            result["sha256"] = digest(path)
        else:
            result.update(mtime_ns=s.st_mtime_ns, ctime_ns=s.st_ctime_ns, inode=s.st_ino)
    elif path.is_dir():
        result["directory"] = True
    else:
        raise ValueError(f"unsupported source object: {path}")
    return result


def inventory(repo=REPO, *, content=True):
    return {n: fingerprint(repo / n, content=content) for n in source_files(repo)}


def freeze_source(output, repo=REPO):
    before = inventory(repo)
    with tarfile.open(output / "source.tar.gz", "w:gz", dereference=False) as archive:
        for name, item in before.items():
            if not item.get("absent"):
                archive.add(repo / name, arcname=name, recursive=False)
    if inventory(repo) != before:
        raise ValueError("source changed while archiving")
    (output / "source-files.json").write_text(json.dumps(before, indent=2) + "\n")
    return before


def frequency_config(frequency):
    """Compare each CPU's policy; observed instantaneous frequency may vary."""
    return {
        **{k: v for k, v in frequency.items() if k != "cpus"},
        "cpus": {
            cpu: {k: v for k, v in entry.items() if k != "scaling_cur_freq"}
            for cpu, entry in frequency["cpus"].items()
        },
    }


def host_config():
    from baseline import collect_cgroup, collect_frequency

    ksm = kernel_state()["files"]
    return {
        "frequency": frequency_config(collect_frequency()),
        "ksm": {n: ksm[n] for n in TUNABLES},
        "cgroup": collect_cgroup(),
        "uffd": Path("/proc/sys/vm/unprivileged_userfaultfd").read_text().strip(),
    }


def input_files(inputs):
    """Cover every frozen input and rootfs directory, including extra files."""
    return {
        str(p.relative_to(inputs)): fingerprint(p) for p in [inputs, *sorted(inputs.rglob("*"))]
    }


def verify(state, *, content=True):
    repo, inputs = Path(state["repo"]), Path(state["inputs_path"])
    if inventory(repo, content=content) != state["source" if content else "source_stat"]:
        raise ValueError("baseline source changed (including untracked files)")
    if content:
        current = input_files(inputs)
    else:
        current = {
            str(p.relative_to(inputs)): fingerprint(p, content=False)
            for p in [inputs, *sorted(inputs.rglob("*"))]
        }
    if current != state["inputs" if content else "inputs_stat"]:
        raise ValueError("baseline frozen inputs changed")
    if host_config() != state["host_config"]:
        raise ValueError("baseline CPU/KSM/cgroup/UFFD configuration changed")


def watch(unit, stop, root, state_path):
    """Run outside the measured cgroup; fail and stop only the owned scope."""
    state = json.loads(state_path.read_text())
    rows = []
    try:
        while not stop.is_set():
            verify(state, content=False)
            rows.append({"monotonic_ns": time.monotonic_ns(), "valid": True})
            if stop.wait(1):
                break
    except Exception as error:
        failure = {"monotonic_ns": time.monotonic_ns(), "error": str(error)}
        rows.append(failure)
        (root / "baseline-error.json").write_text(json.dumps(failure) + "\n")
        subprocess.run(
            ["systemctl", "--user", "kill", "--kill-whom=all", unit],
            capture_output=True,
            timeout=10,
            check=False,
        )
    finally:
        (root / "baseline-guard.json").write_text(json.dumps(rows, indent=2) + "\n")
