# RAFS source through Core

The experimental Linux SDK `pvisor::rafs::{RafsImage, RafsView}` consumes the
[native demand reader](READING.md) and attaches it to the real shared Core
projection. Cache direct images retain their existing projection and streaming
read/direct-copy support.
It retains the normal Core copy-up, hardlink tracking, preimage journal,
stage/apply conflict checks and owned-tree export; there is no separate RAFS
implementation of these behaviors.

## Ownership and use

```rust,ignore
use pvisor::rafs::RafsImage;
use pvisor_overlay_core::OverlayCore;
use std::time::Duration;

let image = RafsImage::open(store, merged_handle, trusted_reader,
                           work_parent, Duration::from_secs(30))?;
let view = image.project(view_parent)?;
let core = OverlayCore::new(vec![view.root().to_owned()], private_upper, None)?;
// Perform same-process Core operations; retain view through Core teardown.
```

`store` is private, owned and immutable; `work_parent` exists and is disjoint
from it. New reader/cache and view directories are exclusively created with
mode 0700. The native reader authenticates necessary metadata and requested data
chunks rather than auditing every blob at open. Source metadata and the native
reader remain shared while views retain independent native placeholders/uppers.
Only copy-up or explicit owned export fills native file content. An owned export
also visits previously untouched objects and retires private indexing hardlinks.

Dropping `RafsImage` leaves its views valid. Final owner/view drop stops and reaps
the one reader before removing the private cache. View drop unregisters the Core
attachment before removing its backing. Keep each view alive through all its
Core I/O. Complete filesystem errors retain errno without resolving a new
version. Framing/checksum/timeout errors retire the source; later requests fail.

Ordinary `RafsImage::project` views remain process-local. The separate explicit
`RafsRootfs::prepare` path writes a protected fixed-source descriptor for fresh
Linux CLI `[vm.rafs]` Sessions. The runner reauthenticates input/tool identity before
starting its reader and registering the backend; unbound SDK paths remain errors.
The native reader, owner and runner retain independent shared MERGED.json locks.
Cooperating deletion requires its exclusive lock, while manual edits/unlink remain
unsupported. This local profile has no automatic full-publication GC. The
separate [node profile](NODE.md) supports shared caching, durable dependency pins
and controlled node reclamation. Neither path adds a Nydus Cargo dependency. See the application README for selection,
explicit command/environment, conflicts and lifecycle limitations.

## Metadata scope

Modes, UID/GID, exact mtime, supported xattrs and regular-file hardlinks remain
visible through Core. Native placeholders use protected host permissions and
the existing ownership override, retaining its legacy byte representation for
old cache callers. Native xattr-setting failures propagate; values are not
silently omitted. Source control/ownership xattrs are rejected by the RAFS
adapter rather than being reinterpreted as overlay commands.

Linux cannot put the ownership override or ordinary user xattrs on a native
symlink, or change its native 0777 permissions. Core now obtains exact logical
symlink attributes and no-follow xattrs from the authenticated source; readonly
lookup, readlink, enumeration and fingerprints work without retagging the image
to the host user. Source errors remain errors, not missing or empty attributes.

Core copy-up/rename now persist otherwise unrepresentable symlink UID/GID, mode
and xattrs in the owned upper. Private records are bound to native inodes and
hardlink anchors prevent their reuse; a native-root binding rejects untranslated
copies. Records follow entry renames and reopen independently of the image owner.
They are hidden from guest enumeration. Qualified copy-up records every symlink
so later updates retain the same logical contract; ordinary native inputs keep
their native representation.
RAFS uses the shared source's Preserve policy. The existing cache adapter retains the default
LegacyNative policy and ordinary native lowers keep their original best-effort behavior;
neither is used to relax RAFS qualification.

Core's no-follow sys helpers now atomically persist chown and xattr mutations of
recorded Linux symlinks, including ctime. Concurrent updates through aliases
retain each other's changes; readonly owners refuse writes. Native links without
records retain native behavior. Authorization remains the adapter's responsibility.

The LinuxComplete virtio-fs adapter now routes logical ownership, mode and xattr
requests through Core, including exact buffer-size errors and shared aliases.
Metadata-aware apply, upper snapshots, owned/sealed exports and block restoration
rebuild destination bindings, retain hardlinks and preserve conflict checks.
Ordinary native/legacy fingerprints retain their old encoding; qualified logical
symlink permissions participate in conflicts. Base import/export and filesystem
snapshot alias capture/restoration retain these fields too.

Keep the namespace with its tree. Untranslated native copies still fail; this is
not permission to tar/cp inode-bound metadata verbatim. Deleted records/anchors
remain until upper teardown. Simplified and ordinary native/DAX shares reject
qualified logical roots. Consume owned exports through LinuxComplete overlays.
Fresh CLI Sessions use explicit source handoff. Environment templates bind an
exact node image to a pristine device at task handoff. Durable node pins and shared-cache eviction are documented in
[NODE.md](NODE.md). No startup or density gain is inferred from backend correctness
checks.

## Run the integration qualification

Build the pinned native reader as in [READING.md](READING.md), then build the
probe using the normal runtime dependencies:

```bash
cargo build --locked -p pvisor --example rafs_core_probe
target/debug/examples/rafs_core_probe \
  /private/rafs-store pvisor-rafs-merged-v1:REVISION \
  /path/to/rafs_reader target/tmp/rafs-core-1 path/to/file optional/hardlink
```

The output directory must be new. This is an engineering correctness probe,
not a startup benchmark. It creates two Core views over one image owner, reads
through `FilesystemService`, checks private copy-up and optional alias identity,
then calls the real runtime `copy_owned_tree`. It stages and applies a change,
separately refuses an external target edit, and checks exported data after all
source views/reader owners are gone. Export, target and stage artifacts remain
under the selected output for inspection; native processes/views/cache retire
automatically on success or an ordinary error.

For a symlink that cannot be represented natively, replace the final arguments
with `path/to/link --inspect-symlink`. This mode reports logical identity, mode,
target and exact xattrs; checks copy-up, rename, source/upper export, apply and
mode-only target conflicts; then drops all source owners. It imports/exports an
independent SnapshotBase and starts a new process with an empty native lower to
reopen the upper. It compares identity, mode, mtime, target and xattrs and checks
reader cleanup. The complete exported root works without the original reader;
the upper alone still requires its ordinary lower stack.

```bash
mise run test -- pvisor-overlay-core
mise run test -- pvisor -- --lib -E 'test(image::rafs::tests) | test(image::cache::direct::tests)'
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
PVISOR_RAFS_READER=/absolute/path/to/rafs_reader \
PVISOR_RAFS_CORE_PROBE=/absolute/path/to/rafs_core_probe \
  mise run test-py -- tests.hardware.image_core
```

Rust tests cover generic projection behavior, unchanged old cache receipts and
exports, owner sharing/teardown, framing/timeouts, missing-path errors and explicit
handoff rejection and logical metadata conversion. Fake peers isolate process/wire behavior. Opt-in
native tests publish/merge real OCI fixtures, use the compiled runtime probe,
verify exact content/xattrs, overwritten and remaining hardlinks, unvisited
exports, successful apply and refused conflicts. Separate cases reject corrupt
payloads, qualify exact symlink export/apply, base roundtrips and new-process
reopen without the source, and check cleanup after failure. Native reader regressions also assert ENOENT,
ENOTDIR, EISDIR and ELOOP survive the wire protocol, with successful reads after
those errors; Core must not mistake a missing rename destination for I/O failure.


## Real KVM workload

For a qualified BusyBox-style image containing `/bin/busybox` and `/lib64 -> lib`,
compile the dedicated test init and pass explicit trusted firmware. The firmware
can be produced with `mise run fw -- build -j4`; the output is printed by that command.
All output directories must be new. The workload writes `PASS` or a failure line
to `vm-upper/pvisor-vm-probe-result` and reports the normal guest exit status.
Require both `PASS` and process exit 0; retain a supervisor timeout as protection.

```bash
cc -static -O2 -Wall -Wextra -Werror scripts/images/vm_probe.c -o target/vm-init
# Live on-demand view (STORE/HANDLE/READER as above):
timeout --kill-after=5s 120s target/debug/examples/rafs_core_probe \
  STORE HANDLE READER target/vm-live bin/busybox --vm target/vm-init FIRMWARE
# First generate the complete export using the ordinary Core probe:
target/debug/examples/rafs_core_probe STORE HANDLE READER target/owned bin/busybox
# No RAFS reader is opened in this mode:
timeout --kill-after=5s 120s target/debug/examples/rafs_core_probe \
  --vm-export target/owned/export target/vm-export target/vm-init FIRMWARE
```

These modes exercise real exec/read/write, regular-file xattrs, symlink chown,
hardlinks, unlink and rename in a 2-vCPU/128-MiB guest. One entry point supervises
the dedicated worker with a 110-second limit, requires exit 0 plus the guest report,
reaps its descendants and removes its transient source/cache directories. It keeps
`vm-upper` and `vm-work` for inspection and rejects existing output directories.
The outer timeout is additional protection. No benchmark timing or CLI product-
integration claim is derived from this test.

## Saving image changes and applying workspace changes

Use `pvisor::rafs::export_image(&core, &mut output)` to save a quiescent merged
image. It reuses the existing container tar traversal with an explicit logical
image metadata profile. UID/GID, modes, nanosecond timestamps, supported xattrs
and hardlinks are stored in the archive. Whiteouts affect the exported view;
private ownership controls do not become guest xattrs. Export does not mutate
host ownership, consume the stage or create a new per-VM cache. Keep reader/lower
owners alive, stop writers and publish output only after success.

Ordinary workspace apply retains host permission rules: an unprivileged caller
cannot assign foreign native owners. Image save qualification compares the full
logical fingerprint using Python's independent archive reader. Host writeback
qualification separately compares native ownership with a real kernel chown
control, while retaining content, metadata, hardlink and conflict checks.

The logical archive refuses unsupported special nodes, security capabilities,
stored overlay-control xattrs and negative timestamps. Arbitrary binary user
xattr values, including embedded newlines, remain length-framed PAX values.
