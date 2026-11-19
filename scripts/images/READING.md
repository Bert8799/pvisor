# Authenticated demand reader

`rafs_reader.rs` implements Linux-only read-only metadata, directory pages and
file-range reads for a fixed `pvisor-rafs-merged-v1` image. `read_nydus.py` owns
the native subprocess and supplies a persistent, serialized Python interface.
This is an engineering prototype. The separate [Rust SDK adapter](CORE.md)
connects it to Core and LinuxComplete guest virtio-fs. Fresh Linux CLI Sessions can
select fixed local input through `[vm.rafs]`; the owner and runner reopen it with
independent artifact leases and reader ownership. Daemon/template selection,
the [explicit node profile](NODE.md) supplies remote data and a shared disk cache.

## Build and use

Use the pinned Nydus source and patches from [README.md](README.md). Build the
helper outside pVisor's Cargo workspace, without adding Nydus dependencies to
production crates:

```bash
mkdir -p target/tools/nydus-v2.4.5/source/examples/rafs_reader
cp scripts/images/rafs_reader.rs target/tools/nydus-v2.4.5/source/examples/
cp scripts/images/rafs_reader/{node,budget,lifecycle,reclaim}.rs \
  target/tools/nydus-v2.4.5/source/examples/rafs_reader/
GIT_CEILING_DIRECTORIES="$PWD/target/tools/nydus-v2.4.5" \
  cargo build --locked \
  --manifest-path target/tools/nydus-v2.4.5/source/Cargo.toml \
  --target-dir target/tools/nydus-v2.4.5/build --example rafs_reader \
  --no-default-features \
  --features fuse-backend-rs/fusedev,backend-http-proxy,dedup -j 4 \
  --config profile.dev.debug=0 --config profile.dev.opt-level=0

python3 scripts/images/read_nydus.py \
  --store target/tmp/rafs-store \
  --handle pvisor-rafs-merged-v1:MERGED_REVISION \
  --reader target/tools/nydus-v2.4.5/build/debug/examples/rafs_reader \
  --work-dir target/tmp/rafs-read-1 stat usr/bin/python3
```

Use `list DIRECTORY --offset 0 --limit 128` for a directory page, or
`read FILE --offset 0 --length 64` for raw file bytes on stdout. Paths are
canonical and relative to the image root; the empty path identifies the root.
Each CLI invocation requires a new work directory and always stops/reaps its
subprocess. The private cache and log remain for diagnostics.

Code can retain one reader for multiple operations:

```python
from pathlib import Path
from scripts.images.read_nydus import RafsReader

with RafsReader(Path(store), handle, Path(engine), Path(new_work)) as image:
    metadata = image.stat("usr/bin/python3")
    page = image.entries("usr/bin", offset=0, limit=16)
    header = image.read_at("usr/bin/python3", 0, 64)
```

Close the owner before removing its work directory. Calls on one owner are
serialized; callers must coordinate explicit close with in-flight calls. The
native stdio frames are private engineering transport, not the pVisor cache
protocol or a production node service.

## Integrity and resource bounds

Open authenticates the commit against the fixed handle, verifies base config
and bootstrap hashes, and pins the exact bootstrap inode through its Linux
file descriptor. Blob files must be regular, non-symlink files of the recorded
size. Native blob references must match the recorded ToC digests and sizes,
with inline SHA-256 chunk digests enabled. Unsupported encoding features,
non-v6 data and chunks over 1 MiB are rejected. The reader has no option to
disable validation.

It deliberately does not traverse all files, read audit inventories/retained
sources, or hash all data blobs at open. Nydus loads and authenticates the
required blob metadata/ToC; `BlobDevice` then validates requested chunks through
its native cache path. All chunks of one read must pass before any response
body is emitted. The wire checksum detects response corruption; it is not a
substitute for chunk authentication. Full publication/audit remains available
through [MERGE.md](MERGE.md).

The image store must remain private, immutable and outside guest access for the
whole owner lifetime. Pinned metadata identity does not authenticate a publisher
or defend against an attacker with the same host UID. The native reader independently holds a shared MERGED.json lock through its
lifetime, including raw stdio/SDK use. The explicit CLI adapter additionally
holds independent owner and runner locks. Cooperating reclamation must lock
exclusively; node management supplies controlled deletion for metadata-only
node revisions. It does not garbage-collect full source publications.
Cache directories are exclusively created with mode 0700;
existing cache state is not adopted. The native local cache fills on demand;
both storage and filesystem prefetch are disabled. This stage does not yet
provide shared node cache ownership, eviction or a high-density memory result.

Reads and frames are at most 1 MiB, directory pages contain at most 128 names,
and paths are at most 4096 bytes/256 components. Parent traversal through a
file or symlink is rejected; a symlink's target is metadata, never followed by
the reader. Mode, numeric UID/GID, exact mtime, xattrs, inode identity and link
count remain visible independently of host DAC. Directory pages return names
only, with `stat` available as needed. There are no mutation operations.

The Python owner applies a deadline to each serialized exchange. Timeout,
framing failure, wrong handshake or checksum failure terminates and reaps the
owned process group and closes pipes. A complete filesystem error propagates
without silently switching image versions; the owner can serve subsequent valid
requests. Keep context ownership even when a call fails.

## Validation and remaining work

```bash
mise run test-py -- tests.regression.test_image_reader
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
PVISOR_RAFS_READER=/absolute/path/to/rafs_reader \
  mise run test-py -- tests.hardware.image_reader
rustfmt --edition 2021 --check scripts/images/rafs_reader.rs
```

Process tests use fake engines only for framing, serialization and cleanup.
Opt-in native tests compare demand reads and paginated traversal against OCI,
including inherited metadata, xattrs, replacements and split hardlinks. A
corrupt unread payload remains openable, good ranges remain readable, and
corrupt ranges fail. Combined payload/digest corruption, metadata tampering and
symlinked blobs are rejected. A cross-chunk failure must not release the valid
prefix. Timeouts and malformed responses must leave no owned process behind.

The [Core adapter](CORE.md) verifies logical metadata, copy-up, stage/apply, owned
exports and actual LinuxComplete KVM execution. Explicit CLI handoff is documented
in the application README; unbound SDK projections are still refused. Native
reader tests alone do not establish CLI functionality, startup latency or density.
