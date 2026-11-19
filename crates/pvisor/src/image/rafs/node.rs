//! Explicit same-node source ownership and cache RPC; no per-VM native reader.
use super::*;
use crate::cache::{
    CacheBackend, CacheClient, CacheConfig, ClientBinding, RafsRequest, Request, Response,
};
use pvisor_journal::api::{DurableFiles, Persistence};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    path::PathBuf,
};

pub(super) const BINDING: &str = "rafs-node-source.json";

/// Explicit Linux VM source served by a same-UID, same-host cache service.
/// The local private metadata store and exact immutable handle are shared with
/// the service. Owner and runner each retain their own shared commit lease through
/// native teardown. Fresh execution and matching inert environment templates
/// retain private writes. This selector does not resolve a template alias or
/// enable native execution checkpoints; there is no fallback to another source.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RafsNodeRootfs {
    /// Private immutable metadata-only published store, outside all guest sources.
    pub store: PathBuf,
    /// Exact pvisor-rafs-merged-v1:<64 lowercase hex> identity; aliases rejected.
    pub handle: String,
    /// unix:///absolute/socket in a private same-UID directory. The cache service
    /// must be explicitly configured for this store; TCP and daemonless backends
    /// are unsupported. Existing cache token credentials stay in host-only binding.
    pub endpoint: String,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientLimits {
    pub(super) metadata_entries: usize,
    pub(super) directory_bytes: usize,
}
impl ClientLimits {
    pub(super) fn validate(&self) -> io::Result<()> {
        if !(1..=16384).contains(&self.metadata_entries)
            || !(65536..=64 * 1024 * 1024).contains(&self.directory_bytes)
        {
            return Err(invalid("invalid node client cache limits"));
        }
        Ok(())
    }
}

/// Authenticated host-only node handoff, including cache credentials.
///
/// Persist only in private owner records. Deserialization grants no authority:
/// [`Self::acquire`] revalidates the exact metadata inode, endpoint, service
/// generation and resource limits. A node restart invalidates this launch
/// binding; it never silently changes the selected service or image.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RafsNodeBinding {
    version: u32,
    source: RafsNodeRootfs,
    commit: (u64, u64),
    client: ClientBinding,
    private_sources: Vec<PathBuf>,
    generation: String,
    limits: ClientLimits,
}

/// Admission ownership of one authenticated node image. Keep this lease until
/// the receiving owner has acquired its own binding. Projections independently
/// retain the image; dropping the lease cannot invalidate an active view.
pub struct RafsNodeLease {
    image: RafsImage,
    binding: RafsNodeBinding,
}

impl RafsNodeBinding {
    /// Fixed artifact dependency, excluding service generation and credentials.
    /// Its local physical identity must match any selected template recipe.
    pub fn artifact(&self) -> RafsArtifact {
        RafsArtifact {
            store: self.source.store.clone(),
            handle: self.source.handle.clone(),
            device: self.commit.0,
            inode: self.commit.1,
        }
    }
    /// Exact canonical source authenticated at admission. Contains no token.
    pub fn source(&self) -> &RafsNodeRootfs {
        &self.source
    }

    /// Reacquire the bound image without environment credential discovery or
    /// alias resolution. Any changed identity fails before creating a view.
    pub fn acquire(&self) -> io::Result<RafsNodeLease> {
        if self.version != 1 {
            return Err(invalid("unsupported RAFS node binding"));
        }
        let (image, binding) = self.source.open(Some(self))?;
        Ok(RafsNodeLease { image, binding })
    }
}

impl RafsNodeLease {
    /// Clone the private handoff record while this lease fences image deletion.
    pub fn binding(&self) -> RafsNodeBinding {
        self.binding.clone()
    }

    /// Project the authenticated image below an existing independent parent.
    /// The returned view owns its attachment, private staging and runner binding;
    /// failure removes new private state without changing shared cache data.
    pub fn prepare(&self, parent: &Path) -> io::Result<RafsView> {
        let parent = parent.canonicalize()?;
        let store = &self.binding.source.store;
        if parent.starts_with(store)
            || store.starts_with(&parent)
            || self
                .image
                .source
                .private_sources
                .iter()
                .any(|authority| parent.starts_with(authority))
        {
            return Err(invalid("RAFS authority and view parent overlap"));
        }
        let view = self.image.project(&parent)?;
        // /tmp may also contain the node's private control directory. Compare
        // the actual unique view, not that common ancestor, to its authorities.
        let owner = view._directory.path();
        if self
            .image
            .source
            .private_sources
            .iter()
            .any(|authority| owner.starts_with(authority) || authority.starts_with(owner))
        {
            return Err(invalid("RAFS authority and view overlap"));
        }
        let bytes = serde_json::to_vec(&self.binding)?;
        if bytes.len() > 64 * 1024 {
            return Err(invalid("RAFS node binding exceeds limit"));
        }
        Persistence::atomic_write(&view._directory.path().join(BINDING), &bytes, 0o600)
            .map_err(io::Error::other)?;
        Ok(view)
    }
}

impl RafsNodeRootfs {
    /// Resolve only the exact local commit, without node credentials, a reader
    /// or payload requests. This is a metadata dependency identity, not runtime
    /// admission: execution must still authenticate the node with [`Self::bind`].
    /// Callers publishing a template must pin it before exposing that template.
    pub fn artifact(&self) -> io::Result<RafsArtifact> {
        let store = self.store.canonicalize()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(
                store
                    .join("merged")
                    .join(revision(&self.handle)?)
                    .join("MERGED.json"),
            )?;
        let (device, inode) = identity(&file)?;
        let artifact = RafsArtifact {
            store,
            handle: self.handle.clone(),
            device,
            inode,
        };
        artifact.open()?;
        Ok(artifact)
    }
    fn open(&self, bound: Option<&RafsNodeBinding>) -> io::Result<(RafsImage, RafsNodeBinding)> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let revision = revision(&self.handle)?;
        private(&self.store)?;
        let store = self.store.canonicalize()?;
        private(&store.join("merged"))?;
        let bundle = store.join("merged").join(revision);
        private(&bundle)?;
        let lease = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(bundle.join("MERGED.json"))?;
        let commit = identity(&lease)?;
        fs2::FileExt::try_lock_shared(&lease)?;
        if lease.metadata()?.len() > MAX as u64 {
            return Err(invalid("RAFS commit exceeds limit"));
        }
        let mut bytes = Vec::new();
        (&lease).take(MAX as u64 + 1).read_to_end(&mut bytes)?;
        use sha2::{Digest, Sha256};
        if crate::util::encode_hex(&Sha256::digest(&bytes)) != revision {
            return Err(invalid("RAFS node commit identity mismatch"));
        }
        let socket = Path::new(
            self.endpoint
                .strip_prefix("unix://")
                .ok_or_else(|| invalid("RAFS node requires a Unix endpoint"))?,
        );
        if !socket.is_absolute() {
            return Err(invalid("RAFS node socket must be absolute"));
        }
        private(
            socket
                .parent()
                .ok_or_else(|| invalid("missing socket parent"))?,
        )?;
        let m = fs::symlink_metadata(socket)?;
        if !m.file_type().is_socket()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
        {
            return Err(invalid("RAFS node socket must be private and same-UID"));
        }
        let source = Self {
            store,
            handle: self.handle.clone(),
            endpoint: format!("unix://{}", socket.canonicalize()?.display()),
        };
        if bound.is_some_and(|b| b.source != source || b.commit != commit) {
            return Err(invalid("RAFS node source changed during handoff"));
        }
        let client = if let Some(bound) = bound {
            CacheClient::from_binding(bound.client.clone())
        } else {
            CacheClient::from_config(CacheConfig {
                backend: CacheBackend::Server,
                location: source.endpoint.clone(),
                // CacheConfig's read_only flag selects immutable filesystem/S3
                // storage, not an RPC capability. This adapter only constructs
                // the typed read-only RAFS operations over the server transport.
                read_only: false,
                image_store: None,
            })
        }
        .map_err(io::Error::other)?;
        if client.address() != source.endpoint {
            return Err(invalid("RAFS node endpoint changed during handoff"));
        }
        let (reply, body) = client
            .request(Request::Rafs {
                handle: source.handle.clone(),
                generation: bound.map(|b| b.generation.clone()),
                request: RafsRequest::Open {
                    commit_device: commit.0,
                    commit_inode: commit.1,
                },
            })
            .map_err(io::Error::other)?;
        let Response::Rafs { frame, .. } = reply else {
            return Err(invalid("expected node open response"));
        };
        if !body.is_empty() || frame["status"] != "ok" || frame["handle"] != source.handle {
            return Err(invalid(
                "cache did not authenticate the selected RAFS node source",
            ));
        }
        let private_sources: Vec<PathBuf> =
            serde_json::from_value(frame["private_roots"].clone()).map_err(io::Error::other)?;
        if private_sources.len() > 8
            || private_sources.is_empty()
            || private_sources.iter().any(|p| !p.is_absolute())
            || bound.is_some_and(|b| b.private_sources != private_sources)
        {
            return Err(invalid("invalid or changed RAFS node authority roots"));
        }
        let generation = frame["node_generation"]
            .as_str()
            .filter(|g| uuid::Uuid::parse_str(g).is_ok())
            .ok_or_else(|| invalid("invalid node service generation"))?
            .to_owned();
        if bound.is_some_and(|b| b.generation != generation) {
            return Err(invalid("RAFS node service changed during handoff"));
        }
        let limits: ClientLimits =
            serde_json::from_value(frame["client_limits"].clone()).map_err(io::Error::other)?;
        limits.validate()?;
        if bound.is_some_and(|b| b.limits != limits) {
            return Err(invalid("RAFS client limits changed during handoff"));
        }
        let binding = RafsNodeBinding {
            version: 1,
            limits,
            generation: generation.clone(),
            source: source.clone(),
            commit,
            client: client.binding(),
            private_sources: private_sources.clone(),
        };
        let mut authorities = private_sources;
        authorities.push(source.store.clone());
        Ok((
            RafsImage {
                source: Arc::new(Engine {
                    worker: Mutex::new(None),
                    remote: Some((client, source.handle, generation)),
                    client_limits: Some(limits),
                    private_sources: authorities,
                    _lease: Some(lease),
                    _directory: None,
                }),
            },
            binding,
        ))
    }
    /// Verify the same physical generation with the service and create a private
    /// sparse projection for a fresh CLI VM. Retain the view through all native
    /// operations. Failure removes newly owned state; shared node cache survives
    /// VM exit. Inputs and node authority must remain outside guest-visible and
    /// writable sources. Missing/unavailable service fails without local fallback.
    pub fn prepare(&self, parent: &Path) -> io::Result<RafsView> {
        let parent = parent.canonicalize()?;
        let store = self.store.canonicalize()?;
        if parent.starts_with(&store) || store.starts_with(&parent) {
            return Err(invalid("RAFS store and view parent overlap"));
        }
        self.bind()?.prepare(&parent)
    }

    /// Authenticate an exact source using the current host cache credentials.
    /// Holds a shared commit lease across durable owner publication and handoff;
    /// failure creates no projection or persistent dependency pin.
    pub fn bind(&self) -> io::Result<RafsNodeLease> {
        let (image, binding) = self.open(None)?;
        Ok(RafsNodeLease { image, binding })
    }
    pub(crate) fn validate_view(&self, root: &Path) -> io::Result<()> {
        let binding = read_binding(root)?;
        if binding.source.store != self.store.canonicalize()?
            || binding.source.handle != self.handle
            || binding.source.endpoint != self.endpoint
        {
            return Err(invalid("RAFS node selector differs from prepared view"));
        }
        Ok(())
    }
}
fn read_binding(root: &Path) -> io::Result<RafsNodeBinding> {
    let parent =
        super::runner::private_owner(root).ok_or_else(|| invalid("not a RAFS projection"))?;
    private(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join(BINDING))?;
    identity(&file)?;
    if file.metadata()?.len() > 64 * 1024 {
        return Err(invalid("RAFS node binding exceeds limit"));
    }
    let binding: RafsNodeBinding = serde_json::from_reader(file.take(64 * 1024 + 1))?;
    if binding.version != 1 {
        return Err(invalid("unsupported RAFS node binding"));
    }
    Ok(binding)
}
pub(crate) fn view_artifact(root: &Path) -> io::Result<RafsArtifact> {
    Ok(read_binding(root)?.artifact())
}
pub(super) fn reattach(root: &Path) -> io::Result<(BackendAttachment, PathBuf)> {
    let binding = read_binding(root)?;
    let (image, _) = binding.source.open(Some(&binding))?;
    let attachment = pvisor_overlay_core::backend::attach_image_bounded(
        root,
        &root.parent().unwrap().join("objects"),
        image.source.clone(),
        binding.limits.metadata_entries,
    )?;
    Ok((attachment, binding.source.store))
}
