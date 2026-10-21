# pvisor-guest

Rust PID 1 supervisor and the platform-independent launch message in `/.pvisor-guest.json`. It initializes Linux virtual filesystems, applies process limits, supervises the workload and reports its exit to the VMM.

Before reporting workload exit, PID 1 synchronizes every visible virtio-fs
mount and checks its result. This drains guest writes that outlive ordinary
file closes. A writeback failure is reported and changes the final status to
125, including when the workload itself exited successfully. Mount paths are
decoded from the bounded kernel mount table; a failure on one mount does not
skip attempts to drain the others. The host stage must be consumed after this
completion boundary, rather than after a guest close or a host-queue drain.

## Private temporary filesystem

`GuestConfig.temporary_filesystem` optionally requests a tmpfs before the workload starts. The contract is independent of CPU architecture:

```json
{"temporary_filesystem":{"path":"/.pvisor-tmp-unique-id","size_bytes":67108864}}
```

The path must be absolute, have an existing parent, and identify a fresh directory. Parent traversal, NUL, zero capacity and overlap with the workspace mount are rejected. Initialization creates the directory exclusively, then mounts tmpfs with `nodev,nosuid,relatime` and mode `1777`. Existing image contents or mounts are never silently covered; creation or mount failure aborts launch. Executable temporary files are supported.

Capacity is charged inside guest RAM; it does not allocate a second VM memory budget. A full filesystem reports normal write errors such as ENOSPC. tmpfs is ephemeral across cold launches. Its pages and kernel state are part of a whole-VM RAM snapshot; callers must preserve the existing RAM snapshot contract when restoring it. It is outside the host workspace review/apply stage.

The pVisor VM executor selects a unique directory and sets default TMPDIR to it. Capacity is one quarter of effective guest RAM, capped at 64 MiB. An explicit invocation TMPDIR disables this default, as does overlap with the workspace. The original image `/tmp` remains available. Other GuestConfig consumers that omit the optional field retain their existing behavior; JSON serialization omits it when absent.

The CLI snapshot launcher and container shim omit this option. Launch-message unit tests do not establish RAM snapshot/fork behavior with the executor-selected tmpfs.

## Sidecar processes

`GuestConfig.sidecars` optionally lists daemons started alongside the workload:

```json
{"sidecars":[{"argv":["/opt/payload","--serve"],"env":{"P":"1"},"log":"/.payload.log"}]}
```

Sidecars are advisory by contract. Each runs with the workload environment
overlaid by its own (`env` entries win on conflicts), stdin and stdout on
`/dev/null`, and stderr appended to the absolute guest `log` path. A spawn
failure or a sidecar exit is reported on the console and never affects the
workload, its exit code or the reported guest status; nothing restarts a
sidecar. argv[0] addresses a file the host executor staged into the rootfs;
the supervisor performs no lookup of its own. Validation (program present,
absolute log path, NUL-free words, valid environment names) runs with the
whole launch contract before any side effect. Consumers that omit the field
keep their existing behavior; JSON serialization omits it when absent.
