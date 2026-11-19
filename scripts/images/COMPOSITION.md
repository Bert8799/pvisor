# Immutable environment components

An environment recipe pins one complete base image and an ordered sequence of
incremental components. Component data is published independently, so replacing
the Python toolchain does not require publishing a new base or workspace. The
recipe's identity changes when a component version, role, name or order changes.

Recipe publication does not merge filesystems, remap paths, start a VM or
implement a live-instance lease. The separate [offline merger](MERGE.md)
consumes a pinned recipe and publishes a qualified merged bootstrap. Runtime
integration remains a separate step.

## Publish the components

Use `publish_nydus.py publish` from [PUBLICATION.md](PUBLICATION.md) for the base.
To publish one OCI changeset, use the same command with `--layer-index N`:

```bash
python3 scripts/images/publish_nydus.py publish \
  --oci-layout /path/to/toolchain-oci --layer-index 0 \
  --nydus-image target/tools/nydus-v2.4.5/build/debug/nydus-image \
  --nydusd target/tools/nydus-v2.4.5/build/debug/nydusd \
  --inventory-reader target/tools/nydus-v2.4.5/build/debug/examples/rafs_inventory \
  --store target/tmp/rafs-store --work-dir target/tmp/toolchain-publication-1
```

`N` is zero-based within the selected platform manifest. Input manifest, config,
compressed-layer digests and uncompressed diff IDs are still verified. The raw
layer is normalized and compared against an independent raw inventory before
publication; numeric ownership, exact timestamps, xattrs and links remain checked.
Whiteout and opaque markers remain real entries in its bootstrap, allowing the
offline merger to apply them against lower components.

The returned handle is `pvisor-rafs-layer-v1:SHA256`. This domain is distinct from
complete-image `pvisor-rafs-v1:SHA256`; changing only the handle prefix cannot
change the artifact's meaning. `inspect` verifies the schema against the prefix.
The raw inventory is stored as `metadata/layer.inventory.json` and authenticated
by SHA-256 and byte size in the commit. It distinguishes implicitly created
directories from directories whose attributes were explicitly declared by OCI.
Empty-file hardlinks remain distinct from unrelated files with identical bytes.

Unsupported node types, duplicate paths, invalid whiteouts and unresolved
cross-layer hardlink targets fail explicitly. A raw layer must resolve its own
hardlinks; it cannot silently copy a lower layer's file into the component. The
existing complete-image importer and qualifier retain their original semantics.

## Publish and inspect a recipe

```bash
python3 scripts/images/compose_nydus.py publish --store target/tmp/rafs-store \
  --base pvisor-rafs-v1:BASE_REVISION \
  --layer toolchain:python=pvisor-rafs-layer-v1:PYTHON_REVISION \
  --layer workspace:initial=pvisor-rafs-layer-v1:WORKSPACE_REVISION

python3 scripts/images/compose_nydus.py inspect --store target/tmp/rafs-store \
  --handle pvisor-rafs-env-v1:ENVIRONMENT_REVISION
```

Repeated `--layer` options are ordered **lower to upper**. Names are lowercase
ASCII identifiers, up to 64 characters; accepted roles are `toolchain` and
`workspace`. At most one initial workspace, 64 overlay components, and no repeated
component handle or role/name pair are accepted. A base-only recipe is valid.
Roles describe intent and do not relocate paths: a workspace changeset must
already contain the desired root-relative workspace paths.

The canonical `ENVIRONMENT.json` has exactly `schema`, `platform`, `base`, and
`layers` fields. The latter contains ordered `{role, name, handle}` entries. Its
SHA-256 defines the `pvisor-rafs-env-v1` handle. Every dependency is opened and
verified; mixed platforms, mutable tags, missing components and wrong component
kinds are rejected. Image launch defaults remain pinned to the base's original
OCI config, rather than silently inheriting a toolchain image's ENTRYPOINT or
environment. Per-run overrides and VM binding belong to runtime integration.

```text
STORE/environments/ENVIRONMENT_REVISION/
  ENVIRONMENT.json
  components/base/{COMMIT.json,metadata/,blobs/}
  components/0000/{COMMIT.json,metadata/,blobs/}
  components/0001/{COMMIT.json,metadata/,blobs/}
```

Component metadata and data are retained by hardlink, with no data copy and no
daemon. Different recipes retain the same unchanged component inodes. Removing
the original component revision directory does not invalidate an environment.
Readers verify retained component contents and identities, without resolving tags
or depending on conversion work directories. This full offline inspection is not
the future lazy startup path.

Recipes use the same verified-directory commit helper and publication lock as
image publication. All retained components are prepared and verified before an
atomic directory rename. An identical recipe is verified and resynced. Pre-commit
errors leave old environments intact; post-rename directory-fsync failure returns
the same explicit `PublicationUncertain` contract as image publication. Abrupt
exit can leave unreachable staging; cleanup/GC and active VM leases are deferred.

## Merge qualification

Native tests publish a base, toolchain and workspace, retain them in a recipe,
remove their original revision directories, and invoke Nydus's metadata merge in
recipe order. They compare the merged tree against the original ordered OCI
changesets, including removal, opaque directories, overwritten files, split
hardlinks, ownership and nanosecond timestamps. Reversed order must produce a
different recipe and the corresponding different correct contents. Data blobs
are hashed before/after merge to prove they were reused unchanged.

The original merge fixture explicitly declares directory attributes.
`merge_nydus.py` additionally uses authenticated raw inventories to distinguish
implicit parents from explicit directory changes. Its independent overlay model
and metadata-only directory correction preserve inherited attributes; its final
full readback rejects any content, type, link or attribute mismatch. Unsupported
cross-layer hardlink targets still fail during raw publication. See [MERGE.md](MERGE.md)
for the supported merge profile and retained dependency layout.

```bash
mise run test-py -- tests.unit.test_image_composition tests.unit.test_image_publication tests.unit.test_image_merge
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
  mise run test-py -- tests.hardware.image_composition tests.hardware.image_merge
```

Storage tests isolate identity, ordering, version replacement, concurrent commit,
tampering and dependency retention. Real conversion and merge assertions remain
opt-in native tests. Keep experiment records under ignored `target/`; recipe
publication or metadata-merge success alone is not evidence of VM Ready speedup.
