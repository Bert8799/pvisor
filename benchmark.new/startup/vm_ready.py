#!/usr/bin/env python3
"""Measure one pVisor binary using the shared Ready / Exit benchmark.

Benchmark: B-STARTUP-REVISION-ENG (benchmark/README.md#b-startup-revision-eng), role engineering A/B.
Motivation: track VM startup latency while changing the implementation.
Conclusion sought: per-binary Ready / Exit statistics, without a paired speedup claim.
Design: 3 warmups, 60 samples, CPU 2,3, 2 vCPU / 128 MiB, warm host caches;
reuse timing.py and its correctness hooks, disable the 5 ms workaround, and
clean up owned processes after each batch. Results and prepared inputs remain
under target/tmp; the private executable copy under /tmp is removed on exit.
"""
import argparse
import ctypes
import fcntl
import select
import signal
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time

repo = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(repo / 'benchmark/pvisor'))
from run_all import prepare_rootfs
sys.path.insert(0, str(Path(__file__).resolve().parent))
import timing
from subjects import subject

def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def processes():
    found = []
    for directory in Path('/proc').iterdir():
        if not directory.name.isdecimal():
            continue
        try:
            if directory.stat().st_uid != os.getuid():
                continue
            name = (directory / 'comm').read_text().strip()
            if name != 'pvisor' and not name.startswith('pvisor-'):
                continue
            stat = (directory / 'stat').read_text()
            if stat[stat.rfind(')') + 2:].startswith('Z'):
                continue
            argv = (directory / 'cmdline').read_bytes().split(b'\0')
            found.append(dict(pid=int(directory.name), name=name, executable=os.fsdecode(argv[0])))
        except FileNotFoundError:
            continue
    return found


def pidfd_call(number, *arguments):
    result = ctypes.CDLL(None, use_errno=True).syscall(number, *arguments)
    if result < 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    return result


def cleanup(runtime, batch):
    before = processes()
    errors = []
    owned = str(runtime / 'pvisor') if runtime else None
    for process in before:
        if process['executable'] != owned:
            continue
        try:
            fd = pidfd_call(434, process['pid'], 0)
        except ProcessLookupError:
            continue
        try:
            # Recheck identity after obtaining the stable process handle.
            argv = Path(f"/proc/{process['pid']}/cmdline").read_bytes().split(b'\0')
            if os.fsdecode(argv[0]) != owned:
                continue
            try:
                pidfd_call(424, fd, signal.SIGTERM, 0, 0)
                poll = select.poll()
                poll.register(fd, select.POLLIN)
                if not poll.poll(5000):
                    pidfd_call(424, fd, signal.SIGKILL, 0, 0)
                    if not poll.poll(5000):
                        errors.append(f"process {process['pid']} did not exit")
            except ProcessLookupError:
                pass
        except FileNotFoundError:
            pass
        finally:
            os.close(fd)
    after = processes()
    if any(p['executable'] == owned for p in after):
        errors.append('benchmark process remains')
    if runtime and not errors:
        shutil.rmtree(runtime)
    record = dict(before=before, after=after, errors=errors,
                  temporary_executable_removed=runtime is None or not runtime.exists())
    if batch and batch.exists():
        save(batch / 'process-cleanup.json', record)
    print('Process cleanup:', json.dumps(record), flush=True)
    if errors:
        raise RuntimeError('; '.join(errors))


def cpu_ticks(cpus):
    result = {}
    for line in Path('/proc/stat').read_text().splitlines():
        name, *values = line.split()
        if name.startswith('cpu') and name[3:].isdigit() and int(name[3:]) in cpus:
            ticks = list(map(int, values[:8]))  # Exclude guest time, already counted in user/nice.
            result[int(name[3:])] = (sum(ticks), ticks[3] + ticks[4])
    if set(result) != cpus:
        raise RuntimeError('CPU accounting unavailable')
    return result


def environment(cpus):
    paths = ['/proc/loadavg', '/proc/pressure/cpu', '/proc/pressure/io',
             '/proc/pressure/memory', '/sys/devices/system/cpu/cpufreq/boost']
    for cpu in sorted(cpus):
        paths.extend(f'/sys/devices/system/cpu/cpu{cpu}/{name}' for name in
                     ('topology/thread_siblings_list', 'cpufreq/scaling_governor',
                      'cpufreq/scaling_min_freq', 'cpufreq/scaling_max_freq'))
    return {name: Path(name).read_text().strip() for name in paths if Path(name).exists()}


def wait_quiet(cpus, output):
    # One gate before the batch; never insert sleeps between measured trials.
    history = []
    consecutive = 0
    previous = cpu_ticks(cpus)
    for _ in range(30):
        time.sleep(1)
        current = cpu_ticks(cpus)
        busy = {}
        for cpu in sorted(cpus):
            total = current[cpu][0] - previous[cpu][0]
            idle = current[cpu][1] - previous[cpu][1]
            busy[cpu] = (1 - idle / total) if total > 0 else 1.0
        history.append(busy)
        consecutive = consecutive + 1 if max(busy.values()) <= 0.20 else 0
        if consecutive >= 3:
            save(output, dict(passed=True, threshold=0.20, consecutive_seconds=3, history=history))
            return
        previous = current
    save(output, dict(passed=False, threshold=0.20, consecutive_seconds=3, history=history))
    raise RuntimeError('CPU set was not quiet for 3 consecutive seconds within 30s; no measurement started')


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'received signal {signum}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=repo / 'target/release/pvisor',
                        help='executable to measure (default: target/release/pvisor)')
    args = parser.parse_args()
    try:
        source = args.binary.expanduser().resolve(strict=True)
    except OSError as error:
        parser.error(str(error))
    if not source.is_file() or not os.access(source, os.X_OK):
        parser.error(f'not an executable file: {source}')

    os.umask(0o022)
    output = repo / 'target/tmp'
    output.mkdir(parents=True, exist_ok=True)
    lock = (output / 'vm-ready.lock').open('a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit('another VM Ready measurement is running')
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    runtime = None
    batch = None
    CPUS = {2, 3}
    monitored = set(CPUS)
    for cpu in sorted(CPUS):
        text = Path(f'/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list').read_text().strip()
        for part in text.split(','):
            bounds = list(map(int, part.split('-')))
            monitored.update(range(bounds[0], bounds[-1] + 1))
    try:
        existing = processes()
        if existing:
            raise RuntimeError('existing pVisor processes must be stopped before measurement: ' + json.dumps(existing))
        os.umask(0o022)
        os.sched_setaffinity(0, CPUS)
        os.environ.update(PVISOR_KRUN_ENOMEM_WORKAROUND='0', PVISOR_STARTUP_PROFILE='0')
        timing.RUNS = 60
        batch = output / time.strftime('vm-ready-%Y%m%d-%H%M%S')
        batch.mkdir()
        print('Results:', batch, flush=True)
        rootfs = batch / 'rootfs'
        rootfs.mkdir()
        prepare_rootfs(rootfs, source)
        # The unchanged executable trust policy rejects the group-writable repo ancestry.
        runtime = Path(tempfile.mkdtemp(prefix='pvisor-ready-'))
        binary = runtime / 'pvisor'
        shutil.copyfile(source, binary)
        binary.chmod(0o755)
        argv = subject('pvisor-vm', dict(BINARY=binary, BATCH=batch, ASSETS=batch))
        save(batch / 'protocol.json', dict(backends=['pvisor-vm'], assets=None, subject=argv))
        inputs = [source, binary, batch/'protocol.json', Path(__file__).resolve(),
                  repo/'benchmark.new/startup/timing.py', repo/'benchmark.new/startup/hooks.py',
                  repo/'benchmark.new/startup/subjects.py', *sorted(p for p in rootfs.rglob('*') if p.is_file())]
        artifacts = {str(p):timing.COMMON.digest(p) for p in inputs}
        save(batch / 'manifest.json', dict(status='prepared', metric_scope=timing.COMMON.READY_EXIT_SCOPE,
                                         artifacts=artifacts))
        save(batch / 'configuration.json', dict(benchmark='B-STARTUP-REVISION-ENG', binary=str(source), binary_sha256=artifacts[str(source)],
            runtime_binary=str(binary), cpus=sorted(CPUS), monitored_cpus=sorted(monitored), guest_cpus=2, memory_mib=128,
            warmup=timing.WARMUP, samples=timing.RUNS, workaround='0', host_kernel=os.uname().release))
        save(batch / 'environment-before.json', environment(monitored))
        print('Waiting for quiet CPUs:', sorted(monitored), flush=True)
        wait_quiet(monitored, batch / 'cpu-quiet-gate.json')
        timing.run_trial(batch, 'preflight', 0)
        result = timing.measure(batch)
        if any(timing.COMMON.digest(p) != sha for p,sha in artifacts.items()):
            raise ValueError('measurement inputs changed')
        save(batch / 'summary.json', result)
        print(json.dumps(result, indent=2), flush=True)
        fields = list(timing.READY_FIELDS)
        labels = ['N', 'Ready 中位数 ms', 'Ready 均值 ms', 'Ready 标准差 ms', 'Ready 最小 ms', 'Ready 最大 ms']
        values = [str(result[fields[0]])] + [f'{result[key]:.3f}' for key in fields[1:]]
        table = '\t'.join(labels) + '\n' + '\t'.join(values) + '\n'
        (batch / 'summary.tsv').write_text(table)
        print(table, end='', flush=True)
    except BaseException as error:
        if batch and batch.exists():
            save(batch / 'failure.json', dict(error=str(error)))
        raise
    finally:
        # A second interrupt must not interrupt the cleanup itself.
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        try:
            if batch and batch.exists():
                save(batch / 'environment-after.json', environment(monitored))
        finally:
            cleanup(runtime, batch)


if __name__ == "__main__":
    main()
