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
`GuestConfig.bridges` optionally adds AF_VSOCK-to-guest-TCP relays (advisory:
a failed listener is reported and never affects the workload), and
`GuestConfig.shares` lists extra virtio-fs mounts (read-only shares mount
`ro,dax=never`; read-write plain passthrough; a failed mount is fatal).

## One workload supervisor

Jobs and controlled workloads enter the same `workload` function and use
`linux/process.rs` for command validation, resource limits, process creation,
signals, terminal allocation and reaping. There is no Docker-specific guest
entry point or supervisor. The host translates Docker image/CLI policy into
generic launch options, including its capability bounding set.

`GuestConfig.process` optionally supplies guest identity, capability bounds,
session creation, stdin ownership, PTY dimensions and working-directory creation.
Omission retains bootstrap identity/session and inherited console descriptors.
`GuestConfig.mounts` declares additional shares, optionally populating an empty
share from its image destination. Shares carry VMM tags, never host source paths.

One blocking PID-1 reaper handles the main workload, exec processes and adopted
orphans. It observes exits before consuming them, serializes process registration
and signalling with reaping, and routes each exec status to its own waiter.
Exec handlers never call waitpid themselves. Main exit stops new admissions,
terminates remaining guest processes and reaps them before the existing final
virtio-fs synchronization and exit report. Process groups created by the launcher
are cleaned up when their leader exits, including exec groups.

## Optional control adapter

`GuestConfig.control` enables a vsock listener on port 1025; omission starts no
listener. The adapter accepts only host-CID peers. Bounded
frames support ping, main stdin attachment, process-group signals, exec and
TCP tunnels to guest loopback services. Exec inherits launch environment and
working directory unless overridden, keeps stdout/stderr separate without a
PTY, and reports 126/127 for execution errors. TTY mode combines output on the
PTY and supports initial dimensions; live resizing is not implemented.

Both host and guest use `control`'s shared protocol definitions. Raw vsock
accept is required: `UnixListener::accept` rejects AF_VSOCK peer addresses.
Protocol tests reject oversized and unknown-field frames; real VM tests remain
necessary for terminal, mount, networking and process-lifecycle behavior.
Architecture guards keep spawning/signalling/reaping out of the transport
adapter. Linux supervisor tests cover limits, exit statuses, orphan reaping and
concurrent exec routing; native CLI acceptance exercises the same behavior in a VM.

## Inert template handoff

The optional boot document below initializes Linux virtual filesystems and
console ports, then blocks on one task. It starts no workload, agent, sidecar,
bridge, workload mount or network configuration before the handoff:

```json
{"template":{"version":1,"template_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdio_ports":[true,true,false]}}
```

The ordinary `GuestConfig` document remains supported. Template documents reject
workload fields and credentials. The host binds the 64-digit content identity to
its immutable inputs and machine topology; the guest verifies only syntax and
the received task's matching identity. This is a separate template handoff
protocol, not the existing port-1025 live-workload adapter.

After setup the supervisor writes and flushes a framed `template::Ready` on
stdout, then reads exactly one `template::Task` from stdin. Both use the existing
four-byte big-endian length and bounded JSON framing (1 MiB maximum). This Ready
means **waiting for a task**, not application readiness or `REFERENCE_READY`.
The host must capture before sending any task bytes; capture admission, device
drain, console state and restore compatibility still belong to the host/VMM.

The task supplies complete command, environment, working directory and launch
options, a new instance ID (up to 128 ASCII letters, digits, hyphens or
underscores) and 32 fresh host-CSPRNG bytes generated for
that restored instance. Before any task setup or process starts, PID 1 mixes the
bytes into `/dev/urandom` and requires Linux `RNDRESEEDCRNG` to succeed. Unsupported
or denied reseeding aborts launch; it never falls back to cloned random state.
`PVISOR_INSTANCE_ID` is injected from the task and cannot be overridden by its
environment. Initial stdin must be `Closed`, with no PTY: the handoff owns stdin.
Stdout becomes ordinary workload output after the readiness frame. The host must
consume that frame separately. No host environment is inherited or shell text
evaluated by the handoff. Existing launch validation, supervision and final
virtio-fs synchronization apply after receipt.

Malformed, oversized, truncated, foreign-template or invalid tasks fail closed
before reseeding. Failed reseeding returns no launch options. There is no second
task, acknowledgement retry or instance recycling. The host owns timeouts,
cancellation, instance uniqueness, seed quality and termination of abandoned VMs.

The native executor and Daemon's optional local template selection use this
protocol with versioned catalog leases, shared read-only baselines, private
writes and fresh network backends. Ordinary execution restore retains its
separate original-task/no-network contract. The handoff does not reset arbitrary
image/application identity, cached user-space randomness or existing network
connections: capture must precede application startup, and images must contain
no per-instance secrets. Bootstrap code must use the new instance identity and
task credentials rather than treating shared image files or the fixed virtual
MAC/IP/CID as globally unique identities. Protocol tests alone do not establish
snapshot cloning, SDK conformance, startup gains or high-density deployment.
