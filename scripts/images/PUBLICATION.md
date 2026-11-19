# Offline RAFS publication

The publisher converts an existing OCI layout, checks complete file equivalence,
and publishes a self-contained immutable local revision. It is an engineering
entry point for the image supply path. Runtime CLI/Daemon/cache integration,
remote storage, layer composition and automatic garbage collection are later
steps; existing OCI imports continue to work.

Selected raw OCI changesets are also supported with `publish --layer-index N`.
They use the distinct `pvisor-rafs-layer-v1` schema/handle and preserve whiteout
markers instead of applying them. The selected zero-based index is bound to the
original manifest and config; `qualification.method` is `raw-oci-layer-v1` and
also binds `inventory_size`. Their metadata additionally retains authenticated
`layer.inventory.json`, including whiteouts, link relationships and implicit
directory declarations. Complete-image v1 records and handles are unchanged.
See [COMPOSITION.md](COMPOSITION.md) before combining these component kinds.

Use the pinned tools and rebuilt `rafs_inventory` described in [README.md](README.md).
The reader must include `--describe` support. No FUSE mount or registry request is
needed during publication. All tools and store paths can remain under `target/`.

```bash
python3 scripts/images/publish_nydus.py publish \
  --oci-layout /path/to/oci-layout --platform linux/amd64 \
  --nydus-image target/tools/nydus-v2.4.5/build/debug/nydus-image \
  --nydusd target/tools/nydus-v2.4.5/build/debug/nydusd \
  --inventory-reader target/tools/nydus-v2.4.5/build/debug/examples/rafs_inventory \
  --store target/tmp/rafs-store --work-dir target/tmp/rafs-publication-1

python3 scripts/images/publish_nydus.py inspect \
  --store target/tmp/rafs-store --handle pvisor-rafs-v1:REVISION_SHA256
```

The store's parent must exist. A new store and its internal directories are
private to the current UID (0700); existing shared, foreign-owned or symlink
directories are rejected. The source layout, store and work directory must be
disjoint. The work directory must be new and retains conversion/qualification
evidence even when a run fails. The daemon executable is pinned in the tool
receipt alongside the converter and reader; this direct qualification path does
not launch it.

## Identity and data layout

```text
STORE/
  blobs/sha256/FULL_FILE_SHA256
  revisions/REVISION_SHA256/
    COMMIT.json
    metadata/image.bootstrap
    metadata/source.manifest.json
    metadata/source.config.json
    blobs/NATIVE_NYDUS_BLOB_ID
  staging/
  locks/publish.lock
```

The returned handle is `pvisor-rafs-v1:` followed by the SHA-256 of canonical
`COMMIT.json` bytes. Neither tag names, timestamps nor temporary paths determine
this identity. The commit binds these contracts:

- `schema`: `pvisor-rafs-v1`, representing the qualified v6 format and SHA-256
  chunk-validation requirements.
- `source`: platform and exact manifest/config SHA-256 and byte sizes. Original
  control bytes are retained, including the image's launch configuration.
- `bootstrap`: full metadata SHA-256 and size.
- `blobs`: native blob ID mapped to full data-file SHA-256/size and the ToC
  SHA-256/size read from the bootstrap. The native ID is not assumed to equal the
  full file hash. Missing inline chunk digests, a wrong digest algorithm or zero
  ToC identity reject publication before the store is changed.
- `qualification`: method version, independent OCI inventory digest and hashes
  of the converter, daemon and direct reader used for qualification.

Each bundle's native blob names hardlink to the content-addressed data pool.
Different revisions can share data without extra copies; their metadata and
source configuration remain pinned independently. A retained bundle hardlink
also survives deletion of a pool name. No background service, VM or per-instance
data buffer is created by this publisher.

`inspect` verifies the handle, strict record structure, complete object set,
platform/config relationship, every metadata digest and every full blob digest.
Files must be regular, not symlinks. It neither repairs corruption nor falls back
to an OCI tag or another revision. These are offline full-file checks; a future
lazy runtime reader must verify pinned metadata and authenticate requested chunks
without calling full `inspect` on each startup. A content hash authenticates the
selected bytes, not a publisher's identity or signature. The caller must obtain
its immutable handle through a trusted publication/distribution path.

## Publication and failure contract

1. Verify OCI identities, convert and compare every supported file and attribute.
2. Read actual blob requirements from the bootstrap. Copy and verify metadata;
   stream data into fsynced, read-only pool files using atomic no-overwrite links.
   Existing objects are verified and reused; corrupt pool objects cause failure.
3. Build a complete private staging bundle, write canonical `COMMIT.json`, verify
   it again, and fsync its files and directories.
4. Under the publication lock, atomically rename the complete bundle into
   `revisions/`, then fsync that directory before returning its handle. An
   identical existing revision is verified and resynced instead of replaced.

Before the rename, failure exposes no new revision. Ordinary exceptions remove
the publisher's private staging. Existing revisions are never replaced. Separate
publishers can convert concurrently; only the final visibility step is locked.
They converge on one revision when the complete commit bytes match.

After the rename, a directory-fsync error raises `PublicationUncertain` with the
visible handle. That revision is complete, but power-loss durability has not been
confirmed. Inspect the handle and retry publication; the retry verifies and
resyncs the same revision. Removing a possibly observed revision to hide this
uncertainty would break the immutable-handle contract.

An abruptly killed publisher can leave unreachable pool objects or private
staging, which readers ignore. A later publication can proceed. Automatic cleanup
of such objects is deferred until reference/lease handling is implemented; do
not remove directories belonging to concurrent publishers. This private store
assumes cooperating writers under one UID, and is not a cross-user writable cache
or a lease mechanism. Filesystem rename, hardlinks, flock and directory fsync are
required; unsupported storage semantics fail explicitly.

## Validation

```bash
mise run test-py -- tests.unit.test_image_publication
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
  mise run test-py -- tests.hardware.image_publication
```

Storage fault tests use a fake converter only to isolate publication behavior:
concurrent writers, deterministic identity, data reuse, tampering, missing files,
symlinks, abrupt process exit and failures before/after the commit point. Native
tests use real conversion, verify readback after removing source/work directories,
and prove that a real bootstrap without inline digest features is rejected even
when its file-equivalence comparison succeeds. Format and HTTP Range regressions
remain independent requirements from [README.md](README.md).

Keep measurements and execution records under ignored `target/`. Successful
publication is correctness evidence, not a startup-latency measurement.
