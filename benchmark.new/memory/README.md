# Memory experiments

Read the [benchmark registry](../../benchmark/README.md) and
[table framework](../README.md) first. `scanner_compare.py` serves the
B-MEMORY-SCALE engineering extension; results do not populate user benchmarks.
The other null JSON templates remain unprepared and are not elapsed-time
substitutes for phase memory accounting. `scanner_compare.json` is a Python paired
row; `--prepare` expands its inputs, and `semspec bench` executes the coordinator.

## Adaptive page scanner versus KSM

Question: at the same 60-second observation window, how much memory does the
pVisor UKSM-inspired physical page pool save relative to default-parameter kernel
KSM, and what CPU work accompanies that saving?

This measures pVisor's implementation, not a kernel with the upstream UKSM patch.
The three arms are private anonymous RAM without advice, the same RAM with KSM
advice, and the daemon physical page pool. Modern KSM's `smart_scan` stays enabled.
Do not change host sysfs from the coordinator. An administrator must configure
KSM before the full comparison. Record all tunables; a run with changed values
must not be called default KSM.

Each condition has four new 256 MiB/one-vCPU VMs, each with a fully verified
64 MiB payload. Repeated pages, identical random pages across VMs, and unique
random pages are separate conditions. The product group includes the SDK, VM
processes and daemon, with four CPU quota units, affinity 0–3, 2 GiB memory and
zero swap. All-ready starts the common 60-second window; scanners can also work
during sequential startup. This is not a synchronized scanner-start experiment
or proof of convergence. The existing worker then checks 25% and 100% private
writes, unchanged peers, independent exit and survivor integrity. Post-write
snapshots do not use an additional 60-second convergence window.

PSS includes daemon/process mappings; cgroup memory also captures charged cache
and kernel memory. Neither alone measures all host costs. Whole-group CPU during
the scan window includes guest heartbeat and observer work. Global ksmd CPU is
recorded separately across admission and the whole attempt, outside the group's
quota; it cannot be attributed exactly to this group or compared as a pure
algorithm CPU cost. Global KSM page counters likewise are not group savings.
The native worker verifies positive accepted advice and mergeable VM mappings.

Preflight uses one fresh group per condition. Formal sampling uses at least five
rounds with predetermined randomized condition order, no speed-based exclusion,
30-second quiet admission and continuous foreign VM/build checks. Any integrity,
budget, interference or cleanup failure stops the campaign and retains evidence.
Preflight and incomplete cohorts cannot supply formal comparisons. Inspect raw
distributions before using medians; separated distributions require cluster
reporting. Paired bootstrap intervals use 5,000 resamples. No tail latency or
production-density claim is supported. No upstream UKSM versus KSM claim is
supported, and the daemon comparison includes architecture costs beyond scanning.

Build the release `vm_memory_scale` example and `pvisor-daemon` before sampling.
Freeze them as `probe` and `pool-daemon` in a build directory with `source.tar.gz`
and `build-receipt.json`. The receipt must bind `example_sha256`, `binary_sha256`
and `source_archive_sha256`, and record the actual build command/compiler/source
identity. Use identical prepared rootfs and firmware inputs for every condition.

```sh
python3 benchmark.new/memory/scanner_compare.py \
  --build benchmark.new/memory/.data/build-20261018 \
  --rootfs benchmark/.data/full-retest-20261006/density-env4/rootfs \
  --firmware benchmark.new/memory/.data/build-20261018/firmware \
  --output benchmark.new/memory/.data/p2 --preflight
# After every preflight condition passes, select a NEW short directory.
python3 benchmark.new/memory/scanner_compare.py \
  --build benchmark.new/memory/.data/build-20261018 \
  --rootfs benchmark/.data/full-retest-20261006/density-env4/rootfs \
  --firmware benchmark.new/memory/.data/build-20261018/firmware \
  --output benchmark.new/memory/.data/f1 --samples 5 --prepare
target/semspec/debug/semspec bench --cwd "$PWD" \
  benchmark.new/memory/.data/f1.input.json
```

`--without-ksm` creates a separate off/adaptive subset when the host scanner is
unavailable. It does not establish a KSM comparison and must not be pooled with a
later full cohort. Keep all raw mappings, monitors, guest logs, source/binary
receipts, frozen harness and summary under `.data/`. Do not rebuild during
sampling. The worker output path must be at most 70 bytes and on real disk.

Conventional harness checks, performed outside sampling:

```sh
just test-py discover -s benchmark.new/memory -p test_scanner_compare.py
```

## Firecracker and pooled-memory curves

### Docker comparison and qualification

The requested Docker baseline uses a private rootless Docker daemon and four
containers in one owned systemd slice, including dockerd, containerd, shims and
attach clients in the same 4-CPU/2-GiB/zero-swap budget. Each container has a
256-MiB limit and one CPU quota. It shares the host kernel; its image page cache
and daemon costs differ from a VM. The existing host Docker service is not the
measured daemon. Record application PSS separately from complete-group costs.
This is a rootless Docker configuration, not evidence for every Docker setup.

`prepare_docker.py --inputs FROZEN_POOL_INPUTS --output NEW_DOCKER_INPUTS` imports
the exact frozen Python userspace, `.so`, library worker and static byte oracle,
plus `docker_init.rs`, which pins CPUs 0–3 before exec. This launcher is necessary
on this host because rootless Docker lacks delegated cpuset control; requested
Docker flags alone are not proof of placement. Actual host PID, affinity,
container limits, image identity and recursive cgroup/PSS evidence are required.
Rootless resource limits require cgroup v2 and systemd; see the
[Docker resource-limit documentation](https://docs.docker.com/engine/security/rootless/tips/#limiting-resources).
Each condition gets a private reflink copy of a stopped image-store seed and
fresh containers, with network disabled and a read-only image. Preparation and
image import occur before measurement; daemon startup remains in startup curves.
The input receipt binds image/archive bytes, source bytes and runtime binaries.
The static and library Docker qualifications and both full seven-arm preflights
passed as recorded below. The current report uses these audited N=1 preflights;
the requested scope reduction cancels further randomized performance sampling.

Protocol v4 gives every arm a 256-task ceiling, retaining four CPUs, 2 GiB
and zero swap. The actual limits and task exhaustion events are checked. The
rootless runtime uses RootlessKit network/port drivers `none` and Docker
`--network=none`; complete runtime PSS remains required.

Docker library evidence uses four independent OverlayFS-view byte/PTE proofs
and hash-bound smaps. Cross-view same-slot count is unavailable. Physical-pool
and explicit DAX evidence retains cross-process backing/offset checks.

### Accepted evidence and measurement status

Both current preflights are complete and audited: `.data/q8/` passes all
21 static conditions and 1,218 data-path checks; `.data/l4/` passes all seven
library conditions and 406 checks. Every owned resource group is quiescent.
At the user's request to shorten the experiment and write from existing data,
the serial driver was stopped after eight successful, cleaned-up `fm7` conditions,
before condition 8 started. `lf7` never started. All 42 owned service/slice units
across `q8`, `l4` and `fm7` were verified inactive, and the coordinator exited.
The partial formal cohort remains incomplete and excluded from aggregate results;
do not restart it or launch replacement formal rounds without a new request.
These preflights remain separate N=1 observations, with no confidence
intervals or formal performance ranking. All six replacement plots have been
visually inspected: static [idle](.data/q8/derived/memory-curves.svg),
[startup](.data/q8/derived/startup-memory-curves.svg),
[post-write](.data/q8/derived/post-write-curves.svg), and library
[idle](.data/l4/derived/memory-curves.svg),
[startup](.data/l4/derived/startup-memory-curves.svg),
[post-write](.data/l4/derived/post-write-curves.svg).

In q8's shared-random condition, the physical pool's sustained 128 MiB
charged-memory drop is bracketed by [1.688, 1.739] seconds, versus
[27.723, 27.773] for KSM. Their 60-second charged footprints are 222.61 and
177.33 MiB, and idle group CPU costs are 5.924 and 0.384 seconds; global ksmd
separately costs 0.41 seconds outside quota. Faster early reclamation therefore
coexists with higher steady memory and CPU costs. Repeated-content readback
changes sealed compression from 186.17 to 419.73 MiB, while physical sharing
stays at 149.14 to 149.10 MiB. The source of this contrast includes private
restoration versus shared reads, not only compression itself.

In l4, physical hints on/off both retain all 16,384 target pages on common
canonical slots across four VMs after readback; actual host settings are checked
as `1`/`0`. Their 2-second charged footprints are 304.89/348.43 MiB and their
60-second footprints are 256.52/259.02 MiB. Steady process PSS is
252.94/252.46 MiB. Explicit DAX also passes the complete target-page proof;
Docker retains its four independent file-view proofs. These observations support
early-reclaim and read-sharing measurements, but do not establish a repeatable
hint speedup or attribute whole-group savings solely to library bytes.

### Preparation and measurement protocol

`pool_compare.py` uses stock Firecracker as the primary baseline. All arms run
the same static 64 MiB worker, including rootless Docker: no Python payload, periodic file heartbeat or
SDK checkpoint producer. Supplemental pVisor arms are off, KSM, physical pool,
sealed compressed pool and local compression. Different guest kernels,
virtio-blk/virtio-fs root paths and the pVisor guest supervisor remain explicit
product differences. Stock Firecracker's unsupported KSM option is not injected.

The parent provides one four-CPU/2 GiB/zero-swap cgroup per four-VM group. An
external observer captures charged memory every 50 ms and PSS every 500 ms;
actual timestamps, missing reads and all failures are retained. Daemon mappings
are included. The guest waits for stdin between full byte/SHA-256 checks. Ready,
1/2/5/10/20/40/60-second snapshots, first full readback, 25/100% COW and survivor
checks are separate. Profile v3 additionally holds all four VMs for 30 seconds
after full private writes and peer checks, with 1/2/5/10/20/30-second snapshots.
This distinguishes delayed old-slot release from persistent metadata overhead;
v2 cohorts retain their original immediate post-write protocol and are not pooled
with v3. Scanners may work during startup. CPU includes validation
and sampling work inside the group; external monitor and global ksmd are outside.
Compression also changes granularity and restore path, so this matrix alone does
not isolate codec CPU. Physical mode never compresses payloads.

Inputs must be frozen under a NEW `.data/` directory: `probe` is the release
`pool_memory_case` example, `daemon` is `pvisor-daemon`, `firecracker` is stock,
`kernel` is its reference ELF, `firmware` is libkrunfw, and `worker` is the static
`firecracker_ksm.WORKER` executable. `root/worker` contains the same executable;
`guest.ext4` is a read-only boot disk built from that root. `receipt.json` binds
all input hashes and compiler/source/build provenance. Never rebuild while
sampling. Harness files are frozen and checked before every condition and at end.
The read-only pool FD survey records unique memfd allocated blocks separately
from its logical reservation; these are storage diagnostics, not net savings.
The static-worker comparison clears inherited `PVISOR_MEMORY_FILE_HINTS` in
child environments so the frozen product's default policy is measured; explicit
hint-off/on controls belong to the separate library cohort.

Prepare reproducible current-source inputs before any VM sampling:

```sh
python3 benchmark.new/memory/prepare_pool.py --output /short/new-inputs \
  --firmware /absolute/libkrunfw.so.5 --kernel /absolute/fc-vmlinux
```

The helper archives actual repository build inputs, records dirty state and
checks they stay unchanged across the targeted release build. External Cargo
registry sources are not archived; exact firmware and kernel bytes are retained.
To use a source-built stock Firecracker, pass `--firecracker`,
`--firecracker-filter` and `--firecracker-build-receipt`. The receipt must match
the binary and supplied filter hashes; retain its source/build provenance.
Optional `--library-rootfs DIR` also prepares a Python root, its complete
file/link/mode manifest and read-only Firecracker `library.ext4`, and an aligned
64 MiB `.so` from `library_payload.c`, plus `library_worker.py`. The immutable
`libraries/` directory contains only the read-only library; stages/logs remain
outside it. Use it with `pool_memory_case --mode off --libraries DIR`, optionally
`--buffered-libraries`. The loader checks STATX DAX, full SHA-256 and private
25/100% writes. This is an explicit attachment correctness path, not automatic
daemon rootfs-library discovery.

`library_compare.py` separately compares actual `dlopen` through Firecracker
virtio-blk (stock Firecracker), rootless Docker, ordinary pVisor rootfs (off, KSM,
physical pool with file READ hints off/on) and an explicit immutable DAX
attachment. The two hint arms use one binary and explicit host-only `0`/`1`
settings; normal rootfs reads remain mutable and are never promoted to DAX.
All seven arms use the same prepared Python userspace and exact library bytes,
four 256 MiB VMs or limited containers, 60-second idle and 30-second post-write windows and all 58
full-payload/COW/peer/exit checks. Prepared input cache is retained; no global
drop-cache operation is used. This is a separate cohort from the static worker.
Prepared source cache may be charged to its preparer outside the product group.
PSS includes mapped DAX pages regardless of charge ownership; report both curves,
and do not treat cgroup memory alone as whole-host physical savings.

After idle timing and timed first full readback, a bounded observer outside the product cgroup checks target
library pages against complete canonical bytes and rechecks present/file PTEs,
inode and file offsets without reading PFNs. It reports target pages sharing
the same slot across all four VMs. Counts are conservative observations, not
net memory savings or an atomic VM snapshot.
The content match identifies controlled library data in guest RAM; it does not
walk guest page tables to attribute each match to an application virtual address.
The additional audit wait is recorded separately, before private writes, and
cannot prewarm the timed first readback. No target sharing in a physical
arm is valid zero benefit; a DAX arm without any shared target page fails its
data-path check. Guest STATX DAX attributes, rootfs/source hashes and all failed
conditions are retained. Use the common preparation helper's `--library-rootfs`
option, then the independent semspec input:

```sh
python3 benchmark.new/memory/library_compare.py --inputs /short/frozen-inputs \
  --docker-inputs /short/frozen-docker-inputs \
  --output /short/new-library-preflight --preflight --prepare
target/semspec/debug/semspec bench --cwd "$PWD" /short/new-library-preflight.input.json
```

Formal library sampling requires all seven variants and at least five randomized
rounds in a NEW directory. It cannot be substituted for the three-content
static-payload matrix or pooled with its results.

```sh
python3 benchmark.new/memory/pool_compare.py \
  --inputs benchmark.new/memory/.data/pool-build-5 \
  --docker-inputs benchmark.new/memory/.data/docker-inputs-1 \
  --output /short/new-static-preflight --preflight --prepare
target/semspec/debug/semspec bench --cwd "$PWD" \
  /short/new-static-preflight.input.json
```

Full preflight covers all seven arms and three patterns. Subsets are diagnostic
only. A new formal output requires all arms/patterns and at least five randomized
rounds. Preserve failed and changed-harness cohorts; never use them for a formal
comparison. `just test-py discover -s benchmark.new/memory -p test_pool_compare.py`
checks missing integrity acknowledgements, wrong digests, budget/swap failures,
short scan windows, incomplete ownership and unexpected stock-Firecracker advice.

### pVisor-only preflight

For a pVisor-only trend check on the current host, omit the Firecracker and
Docker arms and let the preparation helper skip their unused inputs:

```sh
python3 benchmark.new/memory/prepare_pool.py --pvisor-only \
  --output /short/new-pvisor-inputs \
  --firmware /absolute/libkrunfw.so.5
python3 benchmark.new/memory/pool_compare.py \
  --inputs /short/new-pvisor-inputs \
  --output /short/new-pvisor-preflight \
  --preflight \
  --arms pvisor-off,pvisor-physical,pvisor-compressed,pvisor-local \
  --patterns repeated,random-shared,random-unique
```

This mode omits `guest.ext4`, `kernel` and `firecracker` from the input receipt;
those files are not used by the pVisor arms. The report uses `pvisor-off` as its
primary baseline and cannot be pooled with a Firecracker/Docker cohort or with
the historical q8/l4 preflights.

`pvisor-off` and `pvisor-physical` need no KSM or userfaultfd. `pvisor-local`
and `pvisor-compressed` require kernel-fault userfaultfd access (for example a
`/dev/userfaultfd` ACL for the invoking user); do not change the global
`vm.unprivileged_userfaultfd` setting. `pvisor-ksm` is separate: add it only when
the host already has default-parameter KSM enabled. Non-KSM arms run with KSM
disabled and do not require `run=1`. Keep KSM at `run=0` for non-KSM cohorts and
do not use `run=2` during a shared-host campaign because it globally unmerges
other KSM users' pages.

### Fixed baseline wrapper

`baseline.py` freezes the pVisor-only baseline contract and runs
`pool_compare.py` with one of two profiles (default: `full`):

| Profile | Arms | Patterns | Time |
|---|---|---|---|
| `quick` | off/physical/ksm | repeated, random-shared | 10–15 min |
| `full` | five pVisor arms | all three patterns | 30–40 min |

Profiles select conditions, not memory mechanisms: there is no `pvisor-quick`
arm. Full plots have three content rows and two metric columns (charge/PSS),
with five pVisor curves per panel. Quick plots have two rows and two columns,
with off/physical/KSM curves; they are a separate repeat of six conditions.

The initial local baseline uses `full`; a separate `quick` run checks
repeatability without pooling samples. All profiles remain N=1 engineering
preflights. They answer how soon pVisor reaches an observed steady footprint
and how low that footprint is, without external-runtime or density claims.

The frozen `pvisor-observed-steady/v1` rule selects the earliest suffix in
the fixed window that lasts at least 10 seconds and whose charged-memory
range is at most `max(4 MiB, 2% of its median)`. It must remain within that
range through the end of the window. Report both group-start-to-steady and
all-ready-to-steady time, median/min/max, Ready and 2/5/10/30/60-second memory,
startup/whole-condition peaks, CPU, and memory-time integrals. An unreached
suffix is `not_reached`, not zero or 60 seconds. The 128 MiB reclamation
threshold is a separate diagnostic. Readback and post-write memory are
reported separately, with a fresh steady-state check over the 30-second
post-write window.

Analysis clips window boundaries by linear interpolation between neighboring
observations and integrates using actual timestamps. No interior gaps are
filled. A charged-memory gap over 250 ms or PSS gap over 1.5 seconds makes
the affected statistics insufficient evidence. Plots retain raw points.
Before VM startup the parent records initial charge/PSS outside the measured
group and releases the worker; each VM's Ready acknowledgement is timestamped.
This preserves sharing that happened before all-ready.

The wrapper requires KSM `run=1` with default parameters for the KSM arm,
checks `/dev/userfaultfd` for local/compressed, checks `/dev/kvm`, and verifies
the cgroup CPU controller. It records the host frequency/governor/boost,
kernel, CPU, NUMA, page size, KSM/UFFD state, inputs and source/harness hashes
to a real directory named `target/tmp/full-YYYYMMDD-N/` or
`target/tmp/quick-YYYYMMDD-N/` (short enough for VM sockets). Each profile uses
the UTC date and its own next available daily integer; no symlink is created.
Source and input versions stay in `manifest.json`; status stays in `baseline.json`.
The wrapper does not modify host settings. Product source must match the frozen build receipt. The source
archive includes tracked and nonignored untracked files, with patch and manifest.
The wrapper and analysis/supporting harness are retained too. An observer
outside the measured cgroup checks source/input metadata and per-CPU policy,
KSM, UFFD and controller configuration every second; full content hashes are
checked at condition boundaries. A change stops only the owned scope and fails
the batch. Instantaneous frequency is recorded but is not required to be constant.
Successful `baseline.json` is published only after evidence and environment checks.

```sh
# With devices and CPU delegation already prepared and original KSM run=0:
sudo sh -c 'echo 1 > /sys/kernel/mm/ksm/run'
python3 benchmark.new/memory/baseline.py \
  --profile full \
  --inputs target/tmp/inputs
# Restore the original KSM state, including after measurement failure:
sudo sh -c 'echo 0 > /sys/kernel/mm/ksm/run'
```

Use `--dry-run` to write the manifest and print the exact `pool_compare`
command without starting VMs; this records the current host without requiring
its devices or KSM to be ready. Actual execution performs the mandatory checks.
The wrapper refuses an existing output directory
and refuses to run as root; run it as the benchmark user after the admin pre
script.

The output contains `analysis/summary.json`, `analysis/README.md` and SVG/PNG
plots for startup-to-steady, idle, and readback/write. Open circles mark Ready;
crosses and dotted bounds mark the charged-memory steady suffix. PSS stays a
separate metric. Offline reanalysis writes a new directory and launches no VM:

```sh
python3 benchmark.new/memory/baseline_analysis.py /absolute/baseline/cohort \
  --output /absolute/new-analysis
just test-py discover -s benchmark.new/memory -p 'test_baseline*.py'
```

The runner cgroup must expose `cpu.max` for the four-CPU quota check. If
`systemd-run --user` produces a cgroup with only `memory`/`pids`, delegate the
cgroup v2 `cpu` controller to the user slice or run the harness under a
system-level cgroup; the harness reports this as a missing `cpu.max` rather than
sampling an unbounded run. The harness installs the exact `400000 100000` quota
into its own service cgroup before launching VMs and verifies it. This avoids
relying on a systemd `--user` version applying `CPUQuota`; it is the same
four-CPU budget, not a relaxed check. Non-Docker arms run under
`systemd-run --user --scope`, which keeps the invoking shell's `kvm`
supplementary group; a transient service would otherwise start without it and
fail to open `/dev/kvm`.

`plot_pool.py` audits complete N=1 diagnostic cohorts. Historical cohorts with
retired conditions require an explicit `--arms` selection of supported conditions.
This produces a separate
projection, records included/excluded identities and the original report hash,
and leaves raw measurements unchanged. Projection is forbidden for formal
analysis; new formal cohorts must match the current seven-arm matrix exactly.
For a separate complete formal cohort,
`analyze_pool.py REPORT --output NEW_DIRECTORY` requires every
condition in at least five randomized rounds and current placement/KSM evidence.
It retains all samples, per-process/global-ksmd CPU, observed boundary timestamps,
individual curves and medians. Differences are paired by round, with 5,000 paired
bootstrap resamples; a negative difference means lower memory/CPU/read cost.
Separate startup curves retain initial group setup through the all-ready
snapshot, with actual timestamps and a visible readiness marker. This matters
when file hints reclaim pages during loading, before the 60-second idle window
starts. Ready footprint and setup-to-ready duration are reported separately;
an unreached additional 128 MiB idle drop is still censored, even if sharing
already occurred during startup.
Inspect individual distributions and curves for clusters before drawing a
conclusion. Global ksmd cost remains separately scoped, outside the group quota.
Neither renderer accepts incomplete or failed cohorts for aggregate conclusions.
The sampled time to a sustained 128 MiB drop is bracketed by actual observations,
relative to that condition's all-ready footprint. Unreached targets remain
censored, never imputed as zero or 60 seconds. This is whole-group memory,
including kernel/runtime differences; it is not application-only reclaim or an
exact convergence time. Per-process CPU separates VM, pool and control/sampling
work; process counters have clock-tick resolution. The external observers remain
outside this accounting. Whole-condition kernel peak and sampled idle peak are
separate metrics. Formal analysis also rechecks all input/build/source receipts,
frozen supporting harnesses and the hashes of monitor, PSS and guard observations.

## Published curves and archived history

The retained pVisor-only baseline includes steady-state time, charged memory,
PSS, first readback and CPU.
Keep its raw records unchanged. Future roadmap work measures the new candidate's
three content patterns and compares with these saved results; it does not rerun
the five baseline modes.

The [bilingual technical analysis](../../docs/src/zh/lab-notes/vm-memory-performance-analysis.md)
uses only the complete `q8` and `l4` preflights. It publishes idle charged-memory
and PSS curves, with readback and CPU observations; post-write curves remain
local. The public SVG files are byte-identical copies of each cohort's
`derived/memory-curves.svg`. The additional library 0–5-second SVG is rendered
from hash-verified original monitor/PSS samples for all seven variants; its
script and output receipt are retained in `.data/docs-cleanup-20261018/`.

The documentation before cleanup is preserved in
`.data/docs-cleanup-20261018/`, with original paths and SHA-256 values in
`manifest.json`. It retains historical macOS/snapshot sections, the older
512 MiB user comparison, intermediate cohorts and failure investigations.
Original reports, failed attempts, frozen sources and raw curves remain in their
own directories. Historical and incomplete measurements are excluded from the
current comparison. This documentation cleanup starts no new measurements.

## Retained optimization settings

`pvisor-latest` is a separate candidate name: physical pool, adaptive scan order,
batch publication enabled, zero-page replacement disabled. The wrapper fixes
those settings when selecting the candidate; it never adds the old five modes.
To measure only its three content patterns with freshly prepared inputs:

```sh
python3 benchmark.new/memory/baseline.py --profile full \
  --arms pvisor-latest --inputs /short/current-inputs
```

Compare with the saved baseline and keep new results with the run artifacts.
The two named baseline profiles and the historical formal comparison matrix
are unchanged.

The implementation retains three physical-pool settings. Each is recorded in
the manifest, command and worker configuration; the worker overrides inherited
environment values so the selected configuration is reproducible.

| Wrapper option | Runtime setting | Behavior |
| --- | --- | --- |
| `--zero-pages 0\|1` | `PVISOR_MEMORY_ZERO_PAGES` | Replace verified zero pages with private anonymous zero mappings. |
| `--batch-put 0\|1` | `PVISOR_MEMORY_BATCH_PUT` | Publish up to sixteen independent pages per exchange. |
| `--scan-order random\|adaptive` | `PVISOR_MEMORY_SCAN_ORDER` | Keep random scanning or adjust order after a region shows sharing benefits. |

All retain full byte checks, private writes, reference ownership and the two VM
barriers. Defaults remain zero replacement off, batch publication off and random
order. These options configure the implementation; they do not establish a
measured improvement by themselves.

## Host process inspection

Process/FD inspection is best effort. Unreadable descriptors and short-lived
processes remain diagnostics and do not stop measurement by themselves. Known
competing VM/build jobs still block measurement. Host inspection runs separately
from memory sampling. Resource limits, content checks and source/input checks
remain in place. An administrator prepares the required permissions before
measurement.
