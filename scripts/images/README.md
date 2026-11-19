# Image format qualification

This directory contains engineering tools for qualifying the DSec-style
image supply path. Default CLI/Daemon VM image selection remains unchanged;
fresh Linux CLI Sessions can explicitly select a local merged artifact with `[vm.rafs]`
or the same-node remote profile with `[vm.rafs_node]` (see [NODE.md](NODE.md)).
Read-only metadata/data separation, shared node caching and independently
versioned environment layers are the target; converting an image alone is not
an end-to-end startup optimization.

`publish_nydus.py` adds offline conversion and atomic local publication on top of
the qualification probe. It preserves the OCI manifest and full configuration,
pins the bootstrap and authenticated blob descriptions, and returns an immutable
revision handle only after publication. See [PUBLICATION.md](PUBLICATION.md) for
the commands, store layout, failure semantics and validation. The existing pVisor
OCI importer remains available. Standalone publication handles are not runtime
image selectors; the explicit CLI adapter consumes a fixed merged handle.

`compose_nydus.py` publishes fixed environment recipes over a complete base image
and ordered, independently versioned raw OCI changesets. Incremental layers keep
whiteouts and opaque markers; a complete image cannot be used as a delta by
accident. See [COMPOSITION.md](COMPOSITION.md) for component publication, retained
dependencies and the boundary between a recipe and a merged runtime filesystem.

`merge_nydus.py` qualifies and atomically publishes an offline merged bootstrap
for a fixed environment. It preserves inherited directory attributes and OCI
overlay semantics while hardlinking the original data blobs. See
[MERGE.md](MERGE.md) for commands, supported inputs and failure behavior.

`rafs_reader.rs` and its `read_nydus.py` owner provide a Linux engineering
prototype for authenticated, bounded, on-demand reads from those merged images.
They preserve metadata and links without extracting the image or mounting FUSE.
See [READING.md](READING.md) for build/use, integrity checks and the remaining
Core/VM integration boundary.

`manage_node.py` explicitly retains/releases durable dependency pins, trims
unreferenced node blob backing and removes unused metadata-only node revisions.
It uses authenticated, generation-fenced cache requests and never retries an
uncertain mutation. See [NODE.md](NODE.md) for lifecycle ordering, crash recovery
and the distinction from template/checkpoint creation integration.

The experimental `pvisor::rafs` SDK now adapts that reader to Core's shared
projection implementation. [CORE.md](CORE.md) describes real copy-up, stage/apply
and owned-export qualification, plus the metadata and runner-handoff limits.

`probe_nydus.py` accepts an existing OCI image layout and explicitly supplied
`nydus-image` and `nydusd` executables. It verifies OCI identities, collapses layer
history into a normalized tar, builds RAFS v6, and compares a real
read-only FUSE mount with an independent OCI layer inventory. Numeric ownership
is carried in tar metadata, without `chown` or extracting paths onto the host.
No root privileges, tool installation, registry credentials or network access
are required by the probe. Linux FUSE access and
`fusermount3` are prerequisites for the actual mount comparison.

For real images containing root-owned private files, select the separate direct
inventory mode with `--inventory-reader /path/to/rafs_inventory`. It reads RAFS
metadata and every regular file's bytes through Nydus libraries, preserving
ownership and permissions without extracting paths or applying host DAC checks.
The OCI inventory remains the independent source oracle. This mode is explicit:
the report records `verification_mode=direct-reader` and `mount_checked=false`.
There is no automatic fallback from a failed FUSE comparison, no changed file
permissions, and no skipped private file. FUSE and authenticated HTTP Range tests
remain separate mandatory native regressions for their respective paths.

```bash
python3 scripts/images/probe_nydus.py \
  --oci-layout /path/to/oci-layout --platform linux/amd64 \
  --nydus-image /path/to/nydus-image --nydusd /path/to/nydusd \
  --output target/tmp/image-format-qualification
```

The input is an OCI layout with one directly indexed manifest for the selected
platform, matching configuration and uncompressed layer identities. Plain tar
and gzip layers are supported. The probe explicitly rejects other compression,
special nodes, unsupported xattr encodings and conflicting hardlink metadata.
Implicit parent directories have no source attribute declaration; only their
type is compared. Declared directory attributes, including root attributes,
are checked. Files with identical content must retain distinct inode identities
unless the source explicitly hardlinks them.

This is a qualification prototype with an independently tested OCI reference
model, not a replacement OCI importer. Broader images still need cross-checks
against the runtime's existing importer. The mount retains normal permission
checks: an unprivileged caller may be unable to traverse image-owned private
directories. Such reads fail qualification; the probe never changes image
ownership or permissions to make the comparison pass.
Use direct inventory to qualify the on-disk format in that case; it does not
claim the same image is usable through a host-permission-checked FUSE mount.

The native timestamp qualification uses Nydus v2.4.5 with the converter and
daemon patches described below. Stock v2.4.5 silently discards fractional tar
mtime; its daemon also substitutes the current time for valid epoch-second-zero
values. The probe preserves decimal nanoseconds, including `0.000000001`, and
rejects any conversion difference instead of rounding the comparison.

RAFS v6 in v2.4.5 rejects the legacy `digest_validate=true` / v2 `rafs.validate`
filesystem option. Its supported data verification path is different: build with
`--features blob-toc --digester sha256` and use config v2 `cache.validate=true`,
with `rafs.validate=false`. The ToC authenticates the inline chunk digest array
against the digest recorded in the bootstrap, then chunk hashes verify file data.
Both the build feature and cache option are required: native fault tests show
that either omission can allow corrupt bytes to be read.

The probe uses that configuration and additionally records every complete blob's
SHA-256 before mounting, compares all file contents and metadata against OCI, and
rechecks artifacts and OCI identities after unmounting. A Nydus blob-toc blob ID
is not the full file's SHA-256: the receipt binds each ID to a separate full-file
digest instead of assuming they are equal. `passed` means offline
format equivalence only; `runtime_qualified` and `runtime_integrated` remain false.
The separate native Range regression exercises partial HTTP reads without a
full-blob pre-download. A production reader still needs trusted bootstrap identity,
mandatory digest features, immutable publication and a protected shared cache.
Chunk integrity does not authenticate the publisher or an untrusted bootstrap.

## Converter and daemon patches

Use the [upstream v2.4.5 source](https://github.com/dragonflyoss/nydus/tree/v2.4.5)
at commit `e3190057422fee17f594bb3a5c10741b45dac6ce` with three local patches:

- [`patches/nydus-v2.4.5-pax-mtime.patch`](patches/nydus-v2.4.5-pax-mtime.patch)
  reads PAX decimal `mtime` using integer arithmetic, populates both
  seconds and nanoseconds for RAFS v5/v6, and rejects unsupported negative,
  overflowing or sub-nanosecond values. It includes a parser unit regression.
- [`patches/nydus-v2.4.5-v6-epoch-mtime.patch`](patches/nydus-v2.4.5-v6-epoch-mtime.patch)
  restricts the daemon's legacy missing-time fallback to v5. RAFS v6 stores
  timestamps explicitly, so both lookup and getattr retain valid zero seconds.

- [`patches/nydus-v2.4.5-node-admission.patch`](patches/nydus-v2.4.5-node-admission.patch)
  adds opt-in synchronous file-cache metadata loading/data persistence and a shared HTTP
  HEAD/Range concurrency gate held until body consumption completes. Defaults
  retain asynchronous metadata/deferred data writes and unlimited HTTP concurrency for old callers.
  The node helper requires both fields and enables them explicitly; the local
  reader keeps its existing behavior.

These are local patches, not upstream releases or accepted upstream fixes.

For source extracted to `target/tools/nydus-v2.4.5/source`:

```bash
patch -d target/tools/nydus-v2.4.5/source -p1 \
  < scripts/images/patches/nydus-v2.4.5-pax-mtime.patch
patch -d target/tools/nydus-v2.4.5/source -p1 \
  < scripts/images/patches/nydus-v2.4.5-v6-epoch-mtime.patch
patch -d target/tools/nydus-v2.4.5/source -p1 \
  < scripts/images/patches/nydus-v2.4.5-node-admission.patch
GIT_CEILING_DIRECTORIES="$PWD/target/tools/nydus-v2.4.5" \
  cargo build --locked \
  --manifest-path target/tools/nydus-v2.4.5/source/Cargo.toml \
  --target-dir target/tools/nydus-v2.4.5/build --bin nydus-image --bin nydusd \
  --no-default-features \
  --features fuse-backend-rs/fusedev,backend-http-proxy,dedup -j 4 \
  --config profile.dev.debug=0 --config profile.dev.opt-level=0
```

This produces `nydus-image` and `nydusd` under
`target/tools/nydus-v2.4.5/build/debug/`. These debug
tools qualify correctness, not conversion performance. The Git ceiling prevents
the external build from incorrectly reporting pVisor's enclosing Git revision;
record the source commit, patch and executable hashes as provenance instead.

## Direct inventory helper

Build `rafs_inventory.rs` as an example of the same pinned Nydus source. It stays
outside pVisor's Rust workspace and uses the already built Nydus dependencies:

```bash
mkdir -p target/tools/nydus-v2.4.5/source/examples
cp scripts/images/rafs_inventory.rs target/tools/nydus-v2.4.5/source/examples/
GIT_CEILING_DIRECTORIES="$PWD/target/tools/nydus-v2.4.5" \
  cargo build --locked \
  --manifest-path target/tools/nydus-v2.4.5/source/Cargo.toml \
  --target-dir target/tools/nydus-v2.4.5/build --example rafs_inventory \
  --no-default-features \
  --features fuse-backend-rs/fusedev,backend-http-proxy,dedup -j 4 \
  --config profile.dev.debug=0 --config profile.dev.opt-level=0
```

Then pass `--inventory-reader
target/tools/nydus-v2.4.5/build/debug/examples/rafs_inventory` to the probe. The
helper walks all children with errors propagated, computes whole-file SHA-256
incrementally, and records exact inode attributes, xattrs, links and directory
structure. It rejects unsupported nodes, non-UTF-8 names, encrypted/batched
chunks, out-of-bounds reads and chunks over 16 MiB. Like the Python probe it
limits the inventory to 200,000 nodes. It handles local v6 blobs produced by this
prototype, not arbitrary RAFS variants or a remote production reader. Its raw
data reads are checked against the full OCI file hashes; the separate Range
regression qualifies native chunk authentication.

Check helper formatting with
`rustfmt --edition 2021 --check scripts/images/rafs_inventory.rs`. Changes to the
helper require rebuilding the example before native validation; the report
records its executable hash alongside the converter and daemon hashes.
The additional `--describe BOOTSTRAP BLOBS NEW_CACHE_DIRECTORY` mode reports blob
IDs, ToC digests/sizes and actual digest features from the bootstrap. Publication
requires SHA-256 inline chunk digests and a nonzero authenticated ToC for every
data blob; passing file equivalence or specifying build flags alone is insufficient.

Nydusd's mount helper requests `allow_other` unconditionally. A private helper
removes `allow_other`/`allow_root` for this probe, restricting access to the mount
owner; it never edits `/etc/fuse.conf`. The real `fusermount3` still performs the
mount. Both normal and fallback unmount target only the owned private directory.

Keep tools, converted images, logs and qualification reports under ignored
`target/`. Each run requires a new output directory. Input layers are read without
extracting their paths onto the host. Always preserve the source image and
terminate the owned mount and daemon, including on failure.

Qualification must preserve file contents, names/types, UID/GID, mode, mtime,
symlink targets, hardlink relationships and supported xattrs. Whiteouts and opaque
directories must apply only to lower-layer entries. Unsupported inputs fail
explicitly; differences are not normalized away to obtain a passing result.
Tests and reports must distinguish converter limitations from probe limitations.

Run unit regressions with
`mise run test-py -- tests.unit.test_image_format tests.unit.test_image_publication`. Native tests
are opt-in, require both patched tools, and check exact fractional timestamps,
epoch and 1 ns boundaries, real Range reads, payload corruption and simultaneous
corruption of data and its chunk digest:

```bash
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
  mise run test-py -- tests.hardware.image_format.NativeImageFormatTests \
    tests.hardware.image_format.NativeDirectImageFormatTests \
    tests.hardware.image_integrity.NativeImageIntegrityTests \
    tests.hardware.image_publication.NativeImagePublicationTests
```

Stock v2.4.5 fails the timestamp conformance tests. The negative unit comparison
also remains: losing nanoseconds must always fail qualification. New versions
must requalify mount flags, format support and integrity requirements.

The output is a correctness/format report, not a VM Ready benchmark. Startup
measurements retain the existing CLI clock and correctness checks, with separate
first-import/publication, cold-node and warm-node conditions. No startup benefit
is claimed before a runtime reader is integrated and measured.
