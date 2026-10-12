# How long do development tools take across pVisor, Docker and lightweight VMs?

## Conclusions {#conclusions}

**With two CPUs, warm host caches and task-local tool caches, seven tools through exit take 1.07 s in pVisor host, 1.80 s staged and 5.46 s in VM; Docker takes 1.27 s, Firecracker 2.67 s and QEMU microvm 2.35 s. Budget complete waiting and individual operations separately; separated clusters retain their medians and counts.**

| Need | Selection implication |
| --- | --- |
| Local execution with retained changes | Evaluate host/staged and the complete review workflow |
| Independent guest kernel | Budget both startup and VM tool waiting |
| Concurrent or idle environments | Require fixed-budget throughput and physical-memory measurements |

This cross-runtime seven-tool cohort uses the selected frozen source/artifact and passed full acceptance on 2026-10-12. Ordinary rootfs/workspace DAX is disabled; new VM writeback is disabled by default and stage synchronization is serial. Historical cohorts stay separate. Isolation, caches, listeners and artifacts differ, so no cross-batch optimization speedup is calculated.

<!-- measurement-status:start -->

Accepted cohort `fo5a4e7d2b-f`: thirty trials per backend across eight backends, with tool-internal phases and complete tasks reported separately. Historical data and engineering A/B remain separate; samples are not pooled.

<!-- measurement-status:end -->

## Motivation {#motivation}

Agent tool loops traverse, read, write, search, compile and install dependencies. Compare individual operations and complete waiting so that fast startup does not hide file-heavy task costs.

## Experiment design {#interpretation}

Linux x86_64, AMD Ryzen 7 9700X, Fedora 7.2.8-200.fc44.x86_64, Btrfs workspaces. Coordinator, payloads and the private rootless Docker daemon are pinned to CPUs 0,1 and alternate within one private cgroup parent: total CPU quota two, memory limit 16 GiB, zero swap. Each VM has 2 vCPU and 16 GiB guest RAM; guest configured capacity differs from the host-group limit. Shared prepared tool/image caches charged before this parent started remain outside its accounting. Process/thread checks observe visible moments and do not prove complete CPU placement of every short-lived process. Host/staged use rootless_process. Private user/mount namespaces and an owned pivot_root isolate existing host listeners. Namespace preparation is untimed; the private listener is reused after capability checks and warmups. Private /tmp is a 2 GiB tmpfs; workspace and stage remain on Btrfs, with Docker binds pointing to the same physical directories. The user cgroup lacks delegated cpuset, so inherited affinity and the two-core quota are used, retaining the limits of visible process/thread observations.

All backends share offline tools and fixed inputs, with a new workspace per trial. Warm host caches, three warmups and 30 measured samples per cell; seed 20261012 randomizes backends each round. Preparation, builds, image import and input resets are outside timing; complete tasks include launch and exit. Executor-provided TMPDIR is preserved, with fresh private HOME and Cargo caches per task and the `NODE_COMPILE_CACHE` environment variable configured; Python does not read/write bytecode. Seven-operation Cargo uses independent `_cargo-home`, `_tmp` and `_cargo-target` in the workspace. Temporary storage varies with executor configuration and contributes to task cost; directory policy and child inheritance are validated. Per-trial gates check cache variables, parent/child inheritance and actual Python cache behavior; actual Node API enablement and the Node cache-directory filesystem type are unknown for this cohort. Independent October 10 environment checks observe Node caches on tmpfs for native/host/staged/pVisor VM and overlayfs for Docker. Reference VM output reports ext4, but its environment Ready gate fails. The [cache observation CSV](filesystem-cache-policy.csv) retains types and failure states separately; these historical checks supply no timing or new cache-type evidence for this cohort; cache directories and parent/child inheritance are checked in every new trial.

Docker Engine 29.7.2 uses a private rootless overlay2 daemon, classic image storage and a writable bind mount. Firecracker PCI uses a frozen independent custom reference kernel without jailer; QEMU q35/microvm use the same reference kernel and private ext4. pVisor VM uses virtio-fs and frozen pVisor firmware. These Firecracker/QEMU tool samples do not measure stock distribution kernels, and the [independent stock startup results](startup.md) cannot substitute for them. Kernels, storage and staging semantics differ: these are task costs for the stated configurations, not pure VMM or security rankings.

The workload traverses 2,048 files in 32 directories, reads and SHA256-checks 64 MiB, writes 256 × 64 KiB, runs git status and rg, compiles 64 dependency-free Cargo modules and installs 32 offline npm packages. Operations exclude launch/exit; traversal and reading include their workload checks. Full-byte validation after writing is outside the write timer but remains in complete-task timing. The complete task includes all seven, validation and exit. Large repositories, cold disks, online registries and concurrent throughput are unmeasured.

## Data and analysis {#results}

Measured on 2026-10-12: each backend/workload has 30/30 valid samples, totaling 240 formal tasks with zero formal failures. Capability checks and warmups are excluded. Each sample must pass unique output, exit and execution-record checks; staged/VM also require no written results in the host workspace and complete matching bytes in all 256 upper files. Common input inventories are checked before/after sampling, and parent/child cache policies are verified. Any error or detected same-user Rust build/Python test interference rejects the whole cohort. None was detected, and every valid slow sample remains. Process polling every 0.5 s can miss short, inaccessible or unrecognized host activity.

### Launch through exit {#complete-task}

Seconds, N=30 per backend. Normally P50; separated distributions show cluster medians and counts. P95 is descriptive only, with no P99.

| Runtime | Valid / failed | Completion P50 s | Completion P95 s (reference) |
| --- | --- | --- | --- |
| Native | 30 / 0 | 1.00 | 1.01 |
| pVisor host | 30 / 0 | 1.07 | 1.13 |
| pVisor staged | 30 / 0 | 1.80 | 1.87 |
| pVisor VM | 30 / 0 | 5.46 | 6.40 |
| Docker rootless / overlay2 | 30 / 0 | 1.27 | 1.52 |
| Firecracker PCI | 30 / 0 | 2.67 | 2.74 |
| QEMU q35 | 30 / 0 | 2.19 | 2.53 |
| QEMU microvm | 30 / 0 | 2.35 | 2.79 |

| Same-cohort completion comparison | Median difference ms | 95% CI ms | Conclusion |
| --- | --- | --- | --- |
| pVisor staged − Docker rootless / overlay2 | +538.35 | [+497.98, +570.62] | Slower |
| pVisor VM − QEMU microvm | +3104.84 | [+2836.53, +3317.20] | Slower |

pVisor staged minus Docker rootless / overlay2 completion median difference is +538.35 ms, 95% CI [+497.98, +570.62] ms; pVisor VM minus QEMU microvm completion median difference is +3104.84 ms, 95% CI [+2836.53, +3317.20] ms. Intervals use 5,000 same-cohort paired-round bootstrap resamples (seed 20261006). Valid slow samples remain; separated distributions have no single median ranking. Storage and staging capabilities differ, so total gaps cannot identify one component's cost.

### Seven tool operations {#reference-fs}

Milliseconds, N=30 per backend/operation; P50 or separated-cluster medians and counts. Operation times exclude startup and teardown; summing their medians cannot replace complete-task timing.

| Operation | Native | pVisor host | pVisor staged | pVisor VM | Docker rootless / overlay2 | Firecracker PCI | QEMU q35 | QEMU microvm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Traverse 2,048 files | 4.74 | 4.73 | 82.91 | 177.28 | 4.82 | 21.97 | 16.74 | 19.98 |
| Read and verify 64 MiB | 32.18 | 32.83 | 77.54 | 135.60 | 32.51 | 80.22 | 41.18 (n=19); 92.98 (n=11) | 74.23 |
| Write 256 files | 3.71 | 3.79 | 32.00 | 155.56 | 3.87 | 43.01 | 8.04 (n=22); 43.34 (n=8) | 9.77 (n=17); 43.54 (n=13) |
| git status | 15.51 | 16.67 | 163.46 | 503.53 (n=24); 869.80 (n=6) | 15.71 | 82.33 (n=10); 131.00 (n=20) | 89.16 | 168.14 |
| Ripgrep search | 7.83 | 7.85 | 107.10 | 427.32 | 8.33 | 16.05 | 14.67 | 16.13 |
| Offline Cargo build | 52.96 | 58.93 | 90.02 | 579.04 (n=23); 897.47 (n=7) | 48.80 | 386.11 | 373.75 | 430.34 |
| Offline npm install | 212.55 | 225.99 | 244.92 | 1115.77 | 216.83 | 556.76 | 534.19 | 581.33 |


Traversal, Git and search access the staged file view. Reading, compilation and offline installation also include validation, tool execution and each executor's temporary storage. Set timeouts using the actual tool mix and complete waiting, beyond startup speed alone.

<a id="full-ubuntu"></a>
Full Ubuntu filesystem workloads have not been measured with these current build and resource conditions.

### Where filesystem optimizations apply {#cache-status}

The frozen build includes host copy-up scheduling, read queues, journal locks, idle dispatch and subsequent VM queue/I/O-fence fixes. Independent engineering A/B is in the [complete evidence scope](../design/filesystem-performance-analysis.md#filesystem-status). Cross-runtime results measure current default configurations; local engineering percentages do not rescale these data.

All eight backends have now been remeasured with the current frozen artifact and passed correctness, input, budget and interference gates. Earlier unaccepted attempts remain separate and contribute no samples. This new cross-runtime cohort alone does not establish the causal benefit of individual changes.

Ordinary rootfs/workspace DAX is disabled by default and allocates no window. Immutable shares retain explicit opt-in; see the [DAX comparison](../design/filesystem-performance-analysis.md#readonly-dax-ab). New VM writeback is disabled by default, with explicit `PVISOR_VM_FS_WRITEBACK=1` / `0` configuration. By default, restoring a negotiated snapshot retains its mode; explicit `PVISOR_VM_FS_WRITEBACK=0` rejects a snapshot with writeback enabled. The independent [writeback comparison](../design/filesystem-performance-analysis.md#writeback-ab) retains its own frozen artifact and does not rescale this cohort's task times.

Immutable-lower physical metadata caching requires an explicit caller declaration and lifetime stability. Host rootfs, OCI extraction directories and lazy-image local projections receive no automatic promise. Extended Linux HOST kernel caching is not integrated into default `pvisor run` or VM, and its formal timing has not passed acceptance. See the [immutable-lower cache analysis](../design/filesystem-performance-analysis.md#immutable-lower-cache) and [kernel-cache analysis](../design/filesystem-performance-analysis.md#host-kernel-cache) for capability boundaries.

Parallel stage synchronization through `PVISOR_STAGE_SYNC_WORKERS` is disabled and still lacks an independent serial/parallel performance comparison. Implementation availability does not establish it as a source of speedup in these tables.

### Downloads and reproduction {#run}

[Accepted filesystem statistics CSV](filesystem.csv) · [Same-cohort differences and 95% confidence intervals](filesystem-comparisons.csv) · [Measured artifact, source and input provenance](filesystem-provenance.csv) · [Acceptance record](../assets/benchmarks/filesystem-publication-audit.json) · [Method](methodology.md) · [Reproduction manual](https://github.com/DeepLink-org/pvisor/blob/main/benchmark/pvisor/README.md#same-host-runtime-comparison) and [whole-parent resource budget](https://github.com/DeepLink-org/pvisor/blob/main/benchmark/pvisor/README.md#explicit-whole-parent-resource-budget)

Raw evidence is local at `benchmark/.data/fo5a4e7d2b-f/report.json`; frozen source and build receipts are under `benchmark.new/filesystem/.data/fs-next-20261011/trim/`, with execution/acceptance records under `benchmark.new/filesystem/.data/fs-docs-20261012/`. Raw samples and artifacts are not site downloads. Prior accepted cohorts and rejected attempts remain separate in the [source inventory](evidence-sources.csv). Historical [runtime statistics](runtime-summary.csv) and [provenance](runtime-provenance.csv) are not pooled with this cohort.
