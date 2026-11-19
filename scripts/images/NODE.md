# Remote RAFS through the cache service

A Linux cache node owns one multi-image native reader and one shared disk cache.
Fresh CLI VMs use a private projection and upper, without starting a native reader
per instance. Local `[vm.rafs]` and existing OCI/lazy inputs remain available.

## Prepare and start

Build `rafs_reader` using [READING.md](READING.md), then install a protected copy
outside guest sources. Start with a fully qualified, immutable merged artifact.
Stage only its authenticated runtime metadata on the node:

```bash
python3 scripts/images/stage_node.py \
  --source-store /private/published-store \
  --handle pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION \
  --node-store /private/node-metadata
```

Staging performs the full offline source audit before atomically publishing
`MERGED.json`, `metadata/image.bootstrap`, `metadata/source.config.json` and an
empty `blobs/` directory. It preserves the original handle. It copies no data
payload, OCI history or audit inventory. Repeating it verifies the existing
metadata; conflicting state fails. Failed post-rename durability confirmation
returns `PublicationUncertain` with the visible handle. Source and destination
stores must be separate private directories with existing parents.

Publish the qualified source's `merged/REVISION/blobs/` at an immutable HTTPS
origin. Every blob ID must resolve to its original bytes; different combinations
may share the same objects. The origin must implement HEAD with Content-Length
and bounded HTTP Range responses. Staging does not upload or authenticate an
origin. HTTPS uses normal certificate validation; literal loopback HTTP is
accepted for testing. Private origins with authentication and arbitrary mirrors
are not supported by this profile.

Create a protected operator JSON configuration (current user/root owned, no
group/world writes), with absolute, independent host paths:

```json
{
  "store": "/private/node-metadata",
  "reader": "/private/tools/rafs-reader",
  "cache": "/private/node-cache",
  "origin": "https://images.example/rafs-blobs",
  "max_images": 4,
  "max_cache_bytes": 1073741824,
  "workers": 4,
  "max_metadata_bytes": 67108864,
  "max_io_bytes": 67108864,
  "max_downloads": 4,
  "max_address_space_bytes": 1073741824,
  "client_metadata_entries": 1024,
  "client_directory_bytes": 4194304
}
```

The fields from `max_images` onward are optional and show defaults. Parent directories must
exist; metadata/cache directories must be private, owned and non-symlink.
The executable must be protected and executable. The cache is exclusively owned
by one service and persists across VM/service exit. Use a private socket parent:

```bash
pvisor-daemon serve --cache \
  --cache-listen unix:///private/cache-control/cache.sock \
  --cache-rafs-config /private/node-config.json
```

The node is ready only after its native helper handshake. An image open then
verifies the fixed commit, bootstrap, source config and authenticated chunk
metadata/ToC before acknowledgement; file payloads remain demand loaded. Cache
requests retain the existing token authorization and bounded connection/worker
queues. Clients cannot choose an executable, origin or native command.

## Select from a CLI VM

```toml
[run]
executor = "vm"

[vm.rafs_node]
store = "/private/node-metadata"
handle = "pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION"
endpoint = "unix:///private/cache-control/cache.sock"
```

Run `pvisor spawn --vm --config node.toml --stage NEW_STAGE -- COMMAND` from an
independent workspace. Supply the command/environment explicitly. The metadata
store must be the same physical store opened by the service, and the socket
must be private, same-user and local. Both owner and runner retain independent
shared `MERGED.json` locks; a replacement commit is rejected before remote I/O.
The service also retains resident image leases until eviction or shutdown.
Requests bind the service generation returned at open. A restart rejects old
owners and runner handoffs; new instances may open the same persistent cache.
Keep its cache, tool, control directory and operator config outside every guest
source; the CLI checks the service's host authority paths before starting a VM.

The CLI selector supports normal private writes and workspace stage/review/apply.
It cannot combine with other rootfs selectors, prepared workspaces, templates or
execution checkpoint capture/restore. Cache failure fails reads; there is no
fallback to host files or another image. Stopping the daemon closes its owner
pipe, reaps the helper and removes private control state; persistent cache data
remains. After abrupt termination, restart checks cache ownership before reclaiming
an identity-matched stale control directory. Never manually remove active input
or cache data. Cooperating artifact reclamation must obtain exclusive commit locks.

## Version leases, dependencies and reclamation

Daemon manifests can select the same fixed environment for ordinary creation
and inert template restore. See [the daemon integration](../../crates/pvisor-daemon/README.md#fixed-node-environments)
for source selection, offline template preparation and restart behavior. A
template generation retains a direct `image.commit` hardlink to the exact
metadata inode before publication. Catalog and metadata store must share a
filesystem. Alias removal followed by `TemplateCatalog::collect` releases it
only when no reader remains. These references protect trim/removal through the
same commit link-count checks, without requiring a live cache or an RPC during
template publication/collection. They are not entries in `STORE/pins`, and the
management tool's named-owner inspect/release does not manage them.

The native reader, CLI owner and runner independently retain shared commit locks.
A live owner remains protected even between RPCs or after another owner exits.
Metadata/data sharing and cache retention are separate: stopping a VM releases
its transient locks, but never implicitly removes another consumer's durable pin.

Use the explicit management entry, which accepts a private same-UID Linux Unix
socket and existing `PVISOR_CACHE_TOKEN` credentials:

```bash
python3 scripts/images/manage_node.py --endpoint unix:///private/cache-control/cache.sock \
  --handle pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION \
  retain --kind template --owner IMMUTABLE_TEMPLATE_VERSION

python3 scripts/images/manage_node.py --endpoint unix:///private/cache-control/cache.sock \
  --handle pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION \
  inspect --kind template --owner IMMUTABLE_TEMPLATE_VERSION
```

`--kind checkpoint` retains a checkpoint dependency in the same way. Owner IDs
contain 1..128 ASCII letters, digits, `_` or `-`; use immutable versions rather
than mutable aliases, and never reuse an ID for an unrelated owner. A pin is
keyed by kind/owner/exact handle, so one owner can independently retain several
handles and several owners can retain one handle. Retain authenticates the image
and admits its cache reservation before publishing the pin; payload remains lazy.

**Retain before publishing the dependent owner. Release only after its durable
deletion and dependent readers have stopped.** `release` uses the same kind and
owner arguments. Retain/release are idempotent for this exact key. Pins have no
TTL and survive process death, helper termination and service restart. Merely
saving a handle/path does not retain it. This storage interface provides the
lifecycle building block. TemplateCatalog uses the generation-owned references
above; native execution-checkpoint creation retains its existing admission limits.
Workspace-only checkpoints retain their separate filesystem contracts.

Pins live under `STORE/pins/KEY/{PIN.json,MERGED.json}`. The commit is a hardlink
to the exact physical source inode; the versioned record authenticates the
kind/owner/handle and device/inode. Retain writes/fsyncs a private pending record
and link before atomically publishing it. Recovery conservatively completes a
pending pin that already has its commit link; incomplete pre-link state can be
removed. Release first publishes a releasing transaction, then removes the link
and record. Recovery finishes that explicitly requested release. Inspection does
not repair a transaction: unresolved state returns EAGAIN for explicit retain/
release reconciliation. Corrupt/rebound/unknown records stop recovery/reclamation
instead of becoming an absent dependency. Catalogs are limited to 4096 entries;
this profile targets private same-UID stores, not hostile shared writers.

```bash
# Best effort: reach zero only if no active or persistent dependency needs data.
python3 scripts/images/manage_node.py --endpoint unix:///private/cache-control/cache.sock \
  --handle pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION trim --target-bytes 0

# Remove the exact node metadata revision, after releasing all its dependencies.
python3 scripts/images/manage_node.py --endpoint unix:///private/cache-control/cache.sock \
  --handle pvisor-rafs-merged-v1:REPLACE_WITH_64_HEX_REVISION remove
```

Trim drops idle native image owners, then reclaims only whole unique blobs with
no actual native backing references, active commit locks or durable dependency
links. Public base blobs remain protected if any retained combination uses them.
The cache reservation remains charged until its known files are unlinked and
the directory is synced. A bounded `deleting` intent in the existing disk ledger
is synced first; restart/retry completes partial deletion before crediting space.
Unknown files and malformed intents fail closed. If a new image exceeds capacity,
this same policy reclaims unused blobs and retries after releasing idle images;
protected data is not removed to force admission. Remaining pressure returns
EAGAIN. Evicted data may later be downloaded and authenticated again.

Trim does not change capacity and is best effort: `cache_trim.target_reached`
can be false when pins prevent reaching the requested reservation target. It
reports reclaimed/remaining **reserved bytes**, not exact freed filesystem blocks.
Remove rejects active locks and durable links, requires the metadata-only node
profile, and uses the publisher's stable gate plus inode-bound deletion tombstones.
Interrupted tombstones are completed before readiness. It never removes the full
source publication, origin blobs, arbitrary paths or another metadata inode.
Cache data can be trimmed separately after its last reference is removed.

The script discovers the service generation once through Stats, then fences the
requested operation to it. The cache transport authenticates each request; reads
still use typed read-only operations. Lifecycle effects are not auto-replayed.
Timeout/disconnection or a directory-sync error may follow an accepted change:
inspect the exact dependency and reconcile before retrying; Remove may already
have made the metadata absent, and an incomplete Trim may already have freed
some blobs. A restart rejects requests from old generations. Automatic pin expiry,
owner-record discovery, full publication-store GC and portable cross-filesystem
pin migration are not provided.

## Bounds and remaining work

Native operations use 1..16 workers (default 4), a queue of 16, at most 16 resident
images and 1 MiB response bodies. Native request frames are limited to 32 KiB.
Identical blobs share one authenticated metadata owner, chunk state and decoded
disk backing across combinations and client processes. Concurrent construction
is coalesced. Idle image eviction releases owners; busy images remain pinned.
Blob metadata stays charged until its actual native cache owner is gone.

The operator can set these independent limits:

The parent cache keeps one additional bounded hot-image metadata cache: at most
1024 serialized attribute replies and 4 MiB including hint storage. It is shared
across clients, not added to every VM, and contains no file data. A directory
page may carry up to 64 KiB of native child attributes in its existing request
work lease (with an extra 512 KiB reservation); minimum work budgets keep plain
listing. Older native readers without batches still work. Unsupported child
attributes are not cached and retain their ordinary Stat errors.

Only an authenticated Open can establish its exact commit lease. Switching to
another physical image releases the old cache before native admission, so hints
cannot block automatic capacity reclamation. Trim/remove clear hints and leases
before their native mutation; clearing/admission epochs reject stale in-flight
fills. Authentication and service-generation checks precede cache hits. Metadata
hints may avoid native lookups; file data always uses the authenticated reader.
`Stats.shared_metadata_cache` reports these bytes, entries, leases and hits
separately from the following native budgets and native process counters.

| Field | Default | Accepted range and meaning |
| --- | --- | --- |
| `max_metadata_bytes` | 64 MiB | 4 MiB..1 GiB; admitted bootstrap/control backing and unique blob metadata |
| `max_io_bytes` | 64 MiB | 4 MiB..1 GiB; admitted in-flight control, decode and response buffers |
| `max_downloads` | 4 | 1..16; shared HEAD/Range concurrency, held through body consumption |
| `max_address_space_bytes` | 1 GiB | 128 MiB..1 TiB; native helper RLIMIT_AS safety ceiling |
| `client_metadata_entries` | 1024 | 1..16384; entries in each projection type/completion hint cache |
| `client_directory_bytes` | 4 MiB | 64 KiB..64 MiB; retained names and vector element storage per directory inventory |

Metadata admission charges each image twice its bootstrap size plus 2 MiB of
control space. Each unique blob is charged twice its compressed/uncompressed
chunk metadata and ToC sizes, plus 128 bytes per chunk and 64 KiB. The work
ledger first reserves 4 MiB per request; reads add two response lengths plus,
for each requested chunk, four decoded sizes, two compressed sizes and 256
bytes. These are conservative admission formulas for the declared objects,
not exact allocator or RSS accounting. Decode geometry is checked before buffer
allocation. Metadata loading is synchronous; prefetch and implicit adjacent
chunk reads are disabled, so background work cannot escape request admission.
Node data persistence runs in the request thread, including failed multi-chunk
reads, so no delayed write keeps an unobserved file handle after reclamation.
This preserves Nydus's default deferred-write behavior for other callers.
Work leases remain held through socket response completion or failure.

If metadata is full, the node retries after evicting idle images. Busy owners
are never evicted to admit another image. Exhausted metadata/work budgets return
`EAGAIN`; callers may retry after capacity is released. A read whose working set
exceeds the entire budget requires a larger operator limit. Cache reservation
exhaustion remains an explicit error. No partial successful response is emitted.
The native address-space ceiling also covers mappings, allocator overhead,
thread stacks and library state, and respects a stricter inherited limit. It is
an address-space limit, **not an RSS/cgroup quota**. Hitting that final ceiling
can terminate the helper; the product fails closed and cleans up owned services.
The child uses two glibc arenas to avoid excessive unused virtual reservations.

The parent cache process retains its existing connection, frame and worker
bounds independently of the native ledgers. Private client payload caches are
not added. Node limits are attested at open and checked on owner/runner handoff.
Projection hint eviction retains files, hardlinks and persistent completion
receipts; copy-up and export still work after eviction. A directory larger than
its client inventory budget returns `EOVERFLOW` instead of growing without bound.
Projection files, kernel dentries, host page cache and guest page caches are not
part of these hint counters; guest caches remain inside configured VM RAM.
Guest page mappings and all decoded pages are not physically shared. Node-wide
cgroup enforcement and shared guest mappings are separate deployment work.

An authenticated cache request with `op=rafs`, the fixed `handle`, and
`request={"op":"stats"}` returns resource counters without opening the image.
As with open, generation may be absent for discovery; a supplied generation must
match. The response is the usual `status=rafs` envelope with an empty checksummed
body and `frame.stats` version 1. It reports current/peak metadata and work
charges, unique/referenced blobs, logical blob references, rejections, disk
reservations, configured limits and native `/proc/self/status` memory counters.
It does not report an aggregate VM or parent-cache RSS limit.

Capacity reserves each unique blob's decoded size plus conservative chunk/ToC
and file metadata overhead before creating backing. Existing reservations must
match the configured origin and blob descriptions. Capacity exhaustion fails
explicitly; failed reads retain reservations so partial files stay accounted.
The limit covers reserved cache objects, not unrelated files, filesystem block
rounding, filesystem overhead or total process RAM. Per-chunk eviction and native
execution-checkpoint integration remain separate work. Actual memory measurements must include the parent service and
VMs as well as the native helper; admission counters alone do not prove density. The private cache trusts its host UID; external mutation
of cached READY data is unsupported. Origin data is authenticated before caching,
not rehashed in full on every warm read. A successful node read joins the
requested chunks' delayed disk writes before acknowledgement; this supports
immediate process restart, not a power-loss durability guarantee.

Opt-in native regressions in `tests/hardware/image_node.py` and
`tests/hardware/image_resources.py` and `tests/hardware/image_lifecycle.py` require the same tool
variables as the reader tests plus `PVISOR_RAFS_DAEMON` for product lifecycle tests.
They use real Range responses and separate clients; they are correctness tests,
not startup or deployment-density measurements.
