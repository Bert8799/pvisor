# Qualified offline metadata merge

`merge_nydus.py` turns a fixed `pvisor-rafs-env-v1` recipe into one RAFS v6
bootstrap. Its data references reuse the original blobs: file payloads are not
recompressed or copied into a new image. Full offline qualification still reads
and decompresses them for verification.
This is offline publication. Fresh Linux CLI Sessions may consume fixed merged handles
through the explicit `[vm.rafs]` selector; Daemon/template and remote/node-cache
integration remain separate. Publication does not establish a VM Ready improvement.

The separate [demand reader](READING.md) can inspect this fixed result without
running the full offline audit on every open. It is a prerequisite for runtime
backend integration, not an implementation of VM handoff or overlay mutation.

## Use

Publish components and a recipe as described in [COMPOSITION.md](COMPOSITION.md),
then use the pinned patched tools from [README.md](README.md):

```bash
python3 scripts/images/merge_nydus.py merge --store target/tmp/rafs-store \
  --environment pvisor-rafs-env-v1:ENVIRONMENT_REVISION \
  --nydus-image target/tools/nydus-v2.4.5/build/debug/nydus-image \
  --inventory-reader target/tools/nydus-v2.4.5/build/debug/examples/rafs_inventory \
  --work-dir target/tmp/environment-merge-1

python3 scripts/images/merge_nydus.py inspect --store target/tmp/rafs-store \
  --handle pvisor-rafs-merged-v1:MERGED_REVISION
```

The work directory must be new and disjoint from the store. The private store
must already exist. `--timeout` bounds each native subprocess, whose process group
is terminated and reaped on completion or failure. No mount or persistent daemon
is needed. Work logs remain available for diagnostics after an error.

## Semantics and qualification

The merger verifies the recipe and retains every input dependency before native
processing. A separate model computes the expected tree from the base and the
authenticated raw inventories in recipe order. It applies deletion and opaque
markers only to lower entries, then applies the upper entries. Files, symlinks
and directories can replace each other; child traversal through a lower file or
symlink requires an explicit replacement directory. Hardlink groups retain
object identity across deletion and partial overwrite; identical contents alone
do not make two files hardlinks.

An implicit upper directory inherits the lower directory's attributes. An explicit
directory replaces its attributes while retaining children unless made opaque.
New implicit directories retain the defaults frozen by their published raw
bootstrap. Nydus's native merge alone overwrites inherited attributes with upper
defaults. To correct this, the tool creates a final layer containing only the
expected directories and their mode, UID/GID, xattrs and exact nanosecond mtime.
This layer must generate zero data blobs. The original component bootstraps and
this correction layer are merged in one operation; file data remains unchanged.

The merged tree is read back in full and compared with the independent model:
all paths, types, file contents, ownership, permissions, timestamps, xattrs,
symlink targets and hardlink relationships must match. The final bootstrap's
referenced blob IDs, ToC hashes/sizes and SHA-256 chunk-digest features must match
the published inputs. Every input data blob is rehashed before commit. Any
discrepancy fails publication, rather than weakening the comparison or falling
back to another image. Inventories are processed one component at a time, with
the same 200,000-node and 256 MiB inventory limits as qualification.

This profile accepts a complete base and the recipe's raw OCI components, with
at most one data blob per component (a Nydus v2.4.5 merger constraint). A base-only
recipe and metadata-only layers are valid. Merged handles cannot be recursively
used as component handles. Unsupported nodes, unsafe paths and unresolved
cross-layer hardlinks remain explicit import errors. Launch defaults always
come from the pinned base config; component roles do not relocate paths.

## Identity, sharing and failure behavior

The canonical `MERGED.json` binds the algorithm, environment, platform, converter
and reader hashes, metadata hashes and original blob descriptors. Its SHA-256
defines the distinct `pvisor-rafs-merged-v1` handle. Repeating the same inputs and
tools produces the same handle; changing a component/order changes the pinned
environment. `inspect` verifies the entire result and retained dependencies.
It is an offline audit, not the future lazy startup reader.

```text
STORE/merged/MERGED_REVISION/
  MERGED.json
  metadata/{image.bootstrap,inventory.json,source.config.json}
  blobs/NATIVE_BLOB_ID
  environment/ENVIRONMENT.json
  environment/components/{base,0000,...}/{COMMIT.json,metadata/,blobs/}
```

Only new metadata is copied. Data files and retained dependency metadata use
hardlinks in the same store; original component/recipe directories and conversion
inputs can be removed without invalidating a merged revision. Retention includes
fully hidden source data for provenance, though the top-level blob table contains
only blobs referenced by the result. Garbage collection and live VM leases are
separate work; this tool does not delete shared cache objects.

The common publication lock and verified atomic directory rename commit the
whole revision. Pre-commit errors remove owned staging and preserve old versions.
Concurrent identical merges verify/reuse the existing result. A directory-sync
failure after rename returns `PublicationUncertain` with a complete verifiable
handle; retry verifies and resyncs it. Abrupt process termination can leave
unreachable staging; it never makes that staging a valid revision. The private,
single-UID store and full source/digest verification retain the publication
prototype's trust boundary.

## Validation

```bash
mise run test-py -- tests.unit.test_image_merge
PVISOR_NYDUS_IMAGE=/absolute/path/to/nydus-image \
PVISOR_NYDUSD=/absolute/path/to/nydusd \
PVISOR_RAFS_INVENTORY=/absolute/path/to/rafs_inventory \
  mise run test-py -- tests.hardware.image_merge
```

The first entry tests the independent overlay model and metadata correction.
Opt-in native tests additionally exercise exact inherited attributes, lower-only
whiteouts, delete/recreate, file/directory/symlink replacement, split hardlinks,
metadata-only layers, unchanged data inodes, retained dependencies, repeat and
concurrent publication, tampering, missing digest features, readback/commit
failures and post-commit durability uncertainty. They use real patched Nydus
conversion and merge; fault injection tests publication safeguards separately.
