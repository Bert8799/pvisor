# Startup experiments

Question: with prepared inputs, how long does an environment take to produce
verified Ready output and exit normally? Read [B-STARTUP](../REGISTRY.md#b-startup)
and the [benchmark entry guide](../README.md).

## Prepared-environment measurements

The [Rust implementation](../criterion/src/startup/mod.rs) supports native,
pVisor host/staged/VM, Docker, Firecracker, QEMU q35 and QEMU microvm:

```sh
mise run benchmark -- startup/native_timing \
  --output benchmark/startup/.data/NEW-native
```

`startup/timing.json` selects all eight conditions. Individual registered
`*_timing.json` entries select one condition.

| Variable | Input |
| --- | --- |
| `PVISOR_BENCH_BINARY` | Prebuilt pVisor binary |
| `PVISOR_BENCH_CPUS` | Two distinct available CPUs; defaults to the first two allowed CPUs |
| `PVISOR_BENCHMARK_ASSETS` | Frozen assets with `input-manifest.json`, for non-native conditions |
| `PVISOR_BENCHMARK_DOCKER_IMAGE` | Optional immutable local image ID; otherwise from `assets.json` |
| `PVISOR_BENCHMARK_KERNEL_RECEIPT` | Stock kernel receipt for Firecracker/QEMU, including QEMU initrd |

Never pull images or build subjects during sampling. VM and staged conditions
need KVM/FUSE, matching firmware, prepared userspace and the required permissions.
A live Host listener must match the selected executable digest.

An untimed preflight and three correctness warmups precede Criterion's adaptive
calibration. Each operation is a fresh execution. Ready stops at the complete,
exact `REFERENCE_READY` line; Completion requires normal zero exit.
Both clocks start immediately before spawn, and raw records retain both.
Criterion's streams perform separate fresh operations, not reuse cached values.
Input hashing, preparation, validation, logging and cleanup are outside the
returned durations. A controlled VM stop is not normal Completion.

Every invocation validates output ordering, input identities, canary and actual
isolation/Run Bundle where applicable. Pipes are drained concurrently with
bounded output and deadlines. Failure retains evidence and cleans owned process
groups. Disposable disks are removed only after successful validation; Docker
cleanup requires exact cohort ownership labels.

## Frozen CLI version comparisons

`startup/revision` implements B-STARTUP-REVISION-ENG for ordinary, template and local RAFS VM startup.
Use the same release build options, frozen userspace and firmware for both
versions, and retain the source/build receipt beside the engineering evidence.
Each invocation accepts exactly one binary and one typed source profile:

```sh
PVISOR_BENCH_REVISION_ROOTFS=/prepared/rootfs \
PVISOR_BENCH_FIRMWARE=/prepared/libkrunfw.so.5 \
PVISOR_BENCH_CPUS=0,1 \
mise run benchmark -- startup/revision --binary /prepared/baseline/pvisor \
  --output /absolute/path/target/.data/baseline \
  --criterion-output /absolute/path/target/.data/criterion -- --save-baseline baseline
```

Repeat with the candidate binary, a new evidence directory and `--baseline
baseline`. Paths must be absolute and available outside `/tmp`, which is private
inside the worker. Linux user/mount/PID namespaces and `pivot_root`, KVM, `mount`
and `unshare` are required. The harness copies the executable and firmware into
its owned temporary root before timing, uses a fresh stage/Run store per
operation and automatically destroys owned processes when the namespace exits.
No existing Host or Daemon is stopped or reused; no shared memory pool is enabled.

`PVISOR_BENCH_REVISION_SOURCE` selects `directory` (default), `template` or
`rafs-local`. Every profile uses automatic networking, 2 vCPU / 128 MiB and the
same shell Ready workload. For a fair matrix, the ordinary/template rootfs must
be the exact native tree used to publish the RAFS image. `rafs-local` additionally
requires `PVISOR_BENCH_RAFS_STORE`, `PVISOR_BENCH_RAFS_HANDLE` and
`PVISOR_BENCH_RAFS_READER`; it validates the authenticated publication inventory
against the reference rootfs before sampling (bytes, modes, namespace-mapped
ownership, mtime, symlinks and hardlinks). This fixture profile requires no xattrs;
unsupported inputs fail rather than silently dropping attributes.

The template profile prepares a fresh immutable generation using the selected
CLI's embedded guest before timing. `template-preparation.json` records that
one-time cost separately. Each measured invocation restores a fresh instance;
no live VM is reused. Source checks retain inode/content/permission/mtime and
complete inventory checks. Directory ctime may change when SnapshotStore
reapplies its existing permissions; reference-rootfs and file ctime checks stay
strict. Local RAFS starts a fresh reader per invocation over already published
blobs. Its result does not measure cold network pulls or shared-node caching.
Publication, template preparation and source hashing remain outside Ready/Exit.

Ready and Exit each use 60 Flat Criterion samples, following preflight and three
correctness warmups. Calibration can add operations. `statistics-ready.json` and
`statistics-exit.json` report median, mean, sample standard deviation, minimum
and maximum from formal **operations**, uniquely joined to saved Criterion
samples; ambiguous matching fails. Both clocks remain in every raw operation.
Independent version calls are engineering comparisons, without paired causal
confidence intervals. Preserve slow valid samples and failed cohorts.

## Image and same-stage comparisons {#stage-lazy}

Select `startup/image_compare.json`, `startup/image_lazy_compare.json` or
`startup/stage_lazy_compare.json`. These require a dedicated private
[OverlayBD lab](OVERLAYBD_LAB.md), not an arbitrary existing Docker daemon.

- `PVISOR_BENCH_LAB`: owned `.data` lab directory.
- `PVISOR_BENCH_PREPARED_STORE`: verified immutable cache store.
- `PVISOR_BENCH_BINARY_DIR`: matching prepared `pvisor` and `pvisor-daemon`.
- `PVISOR_BENCH_CPUS` and `PVISOR_BENCH_SERVICE_CPUS`: disjoint available sets,
  defaulting to `12,13` and `14,15`.
- `PVISOR_BENCH_FIRMWARE`: dynamic firmware; default `libkrunfw.so.5` beside binaries.
- `PVISOR_BENCH_RUNTIME_RLIB`: release runtime rlib for same-stage measurements.

The lab requires Linux x86_64 namespaces, pivot-root permission, KVM and matching
dynamic tools/libraries. Same-stage preparation links the Rust owner outside the
clock and records compiler, rlib and command identities.
Both arms read and verify the same four OCI files with the same host probe.
Require real readonly mounts, successful rootless-process Bundle, exact bytes,
empty changeset/upper, cold nonzero/hot zero content and restored object inventory.

Each operation is a randomized paired block. A provider's cold client is
immediately followed by its hot client with fresh execution/mount/stage state.
Source and host page caches are retained; private reset is outside the client
interval. Ready, Completion, mounted-only Completion and transfer bytes are
separate quantities. Raw authentication-bearing evidence uses mode `0700`.

## Evidence and validation

Criterion owns sampling and statistics. Keep raw per-operation clocks, pipe
events, pairing order, source/artifact identities and cleanup results.
Do not infer operation P95/P99 from batch averages, combine incompatible clocks
or interpret independent-arm intervals as paired treatment effects.
Formal publication remains subject to [registry rules](../REGISTRY.md).

```sh
mise run test -- pvisor-benchmarks -- \
  --manifest-path benchmark/criterion/Cargo.toml -E 'test(startup::)'
```

Fixtures do not establish real KVM, FUSE, Docker, stock-kernel or lab capability.
Diagnostic protocols remain in [TURBO_OCI.md](TURBO_OCI.md) and
[STARTUP_EXPERIMENTS.md](STARTUP_EXPERIMENTS.md); they do not fill unprepared entries.
