//! Shared native projection for immutable image sources.
use super::{BackendAttachment, FileAttr, FileType, ReadOnlyBackend, UnixIdentity};
use crate::sys;
use fs2::FileExt;
use pvisor_journal::api::{DurableFiles, Persistence};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt as UnixFileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
const MAX_READ: u32 = 1024 * 1024;

/// Metadata for one immutable image entry. Native projection ownership is separate
/// from these logical attributes; equal object IDs denote hardlink aliases only.
#[derive(Clone, Debug)]
pub struct ImageNode {
    /// Stable source object identity; do not deduplicate unrelated equal contents.
    pub object_id: u64,
    /// Logical Unix attributes, stable for the entire source/attachment lifetime.
    pub attr: FileAttr,
    /// Raw symlink target; required only for symlinks and never followed here.
    pub target: Option<Vec<u8>>,
    /// Exact xattrs with nonempty, NUL-free names. A reserved ownership override
    /// supplied by an existing source must agree with attr.
    /// Linux symlink xattrs stay in the source;
    /// native copies that cannot preserve them fail explicitly.
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Immutable, confined image operations shared by any number of native views.
/// Implementations authenticate metadata and bytes, reject traversal through
/// non-directory parents, and preserve errors. There is no mutable tag resolution
/// or implicit prefetch/materialization. Returned data belongs to the caller.
/// Callers retain the source's leases/owners until every attachment is dropped.
pub trait ImageSource: Send + Sync {
    /// Keep the source's symlink copy contract stable for its entire lifetime.
    /// New qualified sources preserve logical fields; only existing adapters
    /// retaining a documented legacy contract should select LegacyNative.
    fn symlink_metadata_policy(&self) -> SymlinkMetadataPolicy {
        SymlinkMetadataPolicy::Preserve
    }
    /// Query one checked root-relative path (empty is root), without fetching data.
    fn metadata(&self, relative: &Path) -> io::Result<ImageNode>;
    /// Return one directory's child names; no dot, parent, slash or NUL components.
    /// This transient inventory must be bounded by the source's supported profile.
    fn entries(&self, relative: &Path) -> io::Result<Vec<OsString>>;
    /// Read up to size bytes (at most 1 MiB), EOF shortened. Authenticate every
    /// involved chunk before returning any bytes; reject non-regular entries.
    fn read_at(&self, relative: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>>;
}

/// Ownership/xattr copy contract for an immutable image's symlinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkMetadataPolicy {
    /// Preserve logical fields or fail; Linux Core uses its private upper store.
    Preserve,
    /// Keep an existing adapter's native best-effort behavior. This is not a
    /// qualification of arbitrary foreign symlink metadata for the new format.
    LegacyNative,
}

struct Projection {
    root: PathBuf,
    objects: PathBuf,
    source: Arc<dyn ImageSource>,
    projected: Mutex<HashMap<PathBuf, FileType>>,
    complete: Mutex<HashSet<u64>>,
    cache_limit: Option<usize>,
    metadata: Option<super::MetadataOwner>,
}

fn identity(node: &ImageNode) -> io::Result<Vec<u8>> {
    if let Some(value) = node.xattrs.get(b"user.containers.override_stat".as_slice()) {
        let identity = UnixIdentity::parse(value)?;
        if identity.uid != node.attr.uid
            || identity.gid != node.attr.gid
            || identity.mode & 0o7777 != u32::from(node.attr.perm)
        {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        return Ok(value.clone());
    }
    Ok(format!(
        "{}:{}:0{:o}",
        node.attr.uid,
        node.attr.gid,
        node.attr.kind.mode() | u32::from(node.attr.perm)
    )
    .into_bytes())
}

/// Attach an immutable image to an existing root, with sparse metadata projected
/// only on access. root and objects must be different real sibling directories
/// under one user-private owned parent. The caller exclusively controls them and
/// the source until teardown; keep the returned owner alive through all Core I/O.
/// The owner retires registration on drop, without deleting caller-owned backing.
/// Existing completion receipts permit reattachment to the same immutable source;
/// never reuse backing with a different source. Errors leave no attachment but
/// may retain partial private projection state. No FUSE mount is created.
/// Reads use the source directly. Copy-up materializes one file with a durable
/// receipt; full owned export explicitly materializes unvisited entries too.
/// Native metadata/xattr errors propagate rather than silently losing attributes.
pub fn attach_image(
    root: &Path,
    objects: &Path,
    source: Arc<dyn ImageSource>,
) -> io::Result<BackendAttachment> {
    attach(root, objects, source, None)
}

/// Attach the same immutable source with bounded, disposable per-view hints.
/// Each projected-kind and completed-object set retains at most `max_entries`
/// entries (1..=16384). Paths over 4096 bytes are never retained. Capacity clears
/// release their allocations; misses reconstruct metadata or verify the durable
/// completion receipt. Eviction does not unlink files, change source identities,
/// weaken copy-up checks or invalidate active readers. Ownership, lifecycle and
/// error behavior otherwise match [`attach_image`]. No file-content cache is added.
pub fn attach_image_bounded(
    root: &Path,
    objects: &Path,
    source: Arc<dyn ImageSource>,
    max_entries: usize,
) -> io::Result<BackendAttachment> {
    if !(1..=16384).contains(&max_entries) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid image projection cache capacity",
        ));
    }
    attach(root, objects, source, Some(max_entries))
}

fn attach(
    root: &Path,
    objects: &Path,
    source: Arc<dyn ImageSource>,
    cache_limit: Option<usize>,
) -> io::Result<BackendAttachment> {
    let owner = root
        .parent()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let m = fs::symlink_metadata(owner)?;
    if !m.is_dir()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || objects.parent() != Some(owner)
        || objects == root
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    match fs::create_dir(objects) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    for path in [root, objects] {
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
    }
    let projection = Arc::new(Projection {
        root: root.canonicalize()?,
        objects: objects.canonicalize()?,
        projected: Mutex::default(),
        complete: Mutex::default(),
        cache_limit,
        metadata: if cfg!(target_os = "linux")
            && source.symlink_metadata_policy() == SymlinkMetadataPolicy::Preserve
        {
            Some(super::metadata_owner(root, true)?)
        } else {
            None
        },
        source,
    });
    if projection.node(Path::new(""))?.attr.kind != FileType::Directory {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    projection.prepare_metadata(Path::new(""))?;
    BackendAttachment::new(root, projection)
}

impl Projection {
    fn remember_projected(&self, relative: &Path, kind: FileType) {
        if self.cache_limit.is_some() && relative.as_os_str().as_bytes().len() > 4096 {
            return;
        }
        let mut cache = self.projected.lock().unwrap();
        if self
            .cache_limit
            .is_some_and(|limit| cache.len() >= limit && !cache.contains_key(relative))
        {
            *cache = HashMap::new();
        }
        cache.insert(relative.to_owned(), kind);
    }
    fn remember_complete(&self, object: u64) {
        let mut cache = self.complete.lock().unwrap();
        if self
            .cache_limit
            .is_some_and(|limit| cache.len() >= limit && !cache.contains(&object))
        {
            *cache = HashSet::new();
        }
        cache.insert(object);
    }
    fn node(&self, relative: &Path) -> io::Result<ImageNode> {
        crate::OverlayCore::validate_rel(relative)?;
        let node = self.source.metadata(relative)?;
        if node
            .xattrs
            .keys()
            .any(|name| name.is_empty() || name.contains(&0))
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(node)
    }
    fn project(&self, relative: &Path) -> io::Result<ImageNode> {
        let node = self.node(relative)?;
        if self.projected.lock().unwrap().contains_key(relative) {
            return Ok(node);
        }
        if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.project(parent)?;
        }
        let path = self.root.join(relative);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.objects.join("projection.lock"))?;
        lock.lock_exclusive()?;
        let missing = match fs::symlink_metadata(&path) {
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e),
        };
        if missing {
            match node.attr.kind {
                FileType::Directory => fs::create_dir(&path)?,
                FileType::RegularFile => {
                    let object = self.objects.join(node.object_id.to_string());
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&object)?;
                    if !file.metadata()?.is_file() {
                        return Err(io::Error::from_raw_os_error(libc::EINVAL));
                    }
                    if file.metadata()?.len() != node.attr.size {
                        file.set_len(node.attr.size)?;
                    }
                    match fs::hard_link(&object, &path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                }
                FileType::Symlink => {
                    let target = OsStr::from_bytes(
                        node.target
                            .as_deref()
                            .ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?,
                    );
                    if self.metadata.is_some() {
                        let object = self.objects.join(node.object_id.to_string());
                        match std::os::unix::fs::symlink(target, &object) {
                            Ok(()) => {}
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(e),
                        }
                        if fs::read_link(&object)?.as_os_str() != target {
                            return Err(io::Error::from_raw_os_error(libc::EIO));
                        }
                        fs::hard_link(object, &path)?;
                    } else {
                        std::os::unix::fs::symlink(target, &path)?;
                    }
                }
                FileType::NamedPipe => sys::mknod(&path, FileType::NamedPipe.mode() | 0o600, 0)?,
                FileType::Socket => {
                    drop(std::os::unix::net::UnixListener::bind(&path)?);
                }
                _ => return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
            }
        }
        let identity = identity(&node)?;
        // Projection permissions protect host cache files; guest Unix ownership
        // is supplied by the immutable metadata and the existing override contract.
        if node.attr.kind != FileType::Symlink {
            let permission = if node.attr.kind == FileType::Directory {
                0o700
            } else {
                0o600
            };
            if fs::symlink_metadata(&path)?.mode() & 0o7777 != permission {
                fs::set_permissions(&path, fs::Permissions::from_mode(permission))?;
            }
            let key = OsStr::new("user.containers.override_stat");
            if sys::native_get_xattr(&path, key).ok().as_deref() != Some(identity.as_slice()) {
                sys::set_xattr(&path, key, &identity, 0)?;
            }
        }
        #[cfg(target_os = "macos")]
        if node.attr.kind == FileType::Symlink {
            // macOS supports no-follow symlink xattrs; passthrough and owned
            // copies use this same contract for Linux guest ownership.
            let key = OsStr::new("user.containers.override_stat");
            if sys::native_get_xattr(&path, key).ok().as_deref() != Some(identity.as_slice()) {
                sys::set_xattr(&path, key, &identity, 0)?;
            }
        }
        // Linux cannot store user xattrs on symlinks. Reads use the immutable
        // source; native copy/export admission below must still reject loss.
        let logical_symlink = cfg!(target_os = "linux") && node.attr.kind == FileType::Symlink;
        for (name, value) in &node.xattrs {
            if logical_symlink {
                continue;
            }
            if name.as_slice() != b"user.containers.override_stat" {
                let name = OsStr::from_bytes(name);
                if sys::native_get_xattr(&path, name).ok().as_deref() != Some(value.as_slice()) {
                    sys::set_xattr(&path, name, value, 0)?;
                }
            }
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.modified()? != node.attr.mtime {
            sys::set_times(
                &path,
                Some(node.attr.atime),
                Some(node.attr.mtime),
                node.attr.kind == FileType::Symlink,
            )?;
        }
        self.remember_projected(relative, node.attr.kind);
        Ok(node)
    }
    fn read_node(&self, relative: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        self.source.read_at(relative, offset, size)
    }
    fn restore_directory_time(&self, relative: &Path, node: &ImageNode) -> io::Result<()> {
        let path = self.root.join(relative);
        if fs::symlink_metadata(&path)?.modified()? != node.attr.mtime {
            sys::set_times(&path, Some(node.attr.atime), Some(node.attr.mtime), false)?;
        }
        Ok(())
    }
}

impl ReadOnlyBackend for Projection {
    fn symlink_metadata_policy(&self) -> SymlinkMetadataPolicy {
        self.source.symlink_metadata_policy()
    }
    fn prepare_metadata(&self, relative: &Path) -> io::Result<()> {
        self.project(relative).map(|_| ())
    }
    fn attributes(&self, relative: &Path) -> io::Result<FileAttr> {
        self.node(relative).map(|node| node.attr)
    }
    fn logical_xattrs(&self, relative: &Path) -> io::Result<Option<BTreeMap<Vec<u8>, Vec<u8>>>> {
        #[cfg(target_os = "linux")]
        {
            // Retain only the already-projected kind, not a per-view metadata
            // copy. Ordinary file/directory xattrs keep the native fast path.
            if self
                .projected
                .lock()
                .unwrap()
                .get(relative)
                .is_some_and(|kind| *kind != FileType::Symlink)
            {
                return Ok(None);
            }
            let mut node = self.node(relative)?;
            if node.attr.kind == FileType::Symlink {
                // Legacy cache adapters carry this internal identity field even
                // for symlinks where it was never a native/visible xattr.
                identity(&node)?;
                node.xattrs
                    .remove(b"user.containers.override_stat".as_slice());
                return Ok(Some(node.xattrs));
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = relative;
        Ok(None)
    }
    fn prepare_directory(&self, relative: &Path) -> io::Result<()> {
        let node = self.project(relative)?;
        let entries = self.source.entries(relative)?;
        for name in entries {
            if name != "." && name != ".." {
                self.project(&relative.join(name))?;
            }
        }
        // Creating children must not change the immutable directory's guest mtime.
        self.restore_directory_time(relative, &node)?;
        Ok(())
    }
    fn read_at(&self, relative: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        crate::OverlayCore::validate_rel(relative)?;
        self.read_node(relative, offset, size)
    }
    fn materialize_file(&self, relative: &Path) -> io::Result<()> {
        let node = self.project(relative)?;
        #[cfg(target_os = "linux")]
        if node.attr.kind == FileType::Symlink
            && self.source.symlink_metadata_policy() == SymlinkMetadataPolicy::Preserve
        {
            self.metadata
                .as_ref()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?
                .restore_symlink(
                    &self.root.join(relative),
                    super::UnixIdentity {
                        uid: node.attr.uid,
                        gid: node.attr.gid,
                        mode: u32::from(node.attr.perm),
                    },
                    &node
                        .xattrs
                        .into_iter()
                        .filter(|(name, _)| name != b"user.containers.override_stat")
                        .collect::<Vec<_>>(),
                )?;
        }
        if node.attr.kind != FileType::RegularFile {
            return Ok(());
        }
        if self.complete.lock().unwrap().contains(&node.object_id) {
            return Ok(());
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(
                self.objects
                    .join(format!("{}.content.lock", node.object_id)),
            )?;
        lock.lock_exclusive()?;
        if self.complete.lock().unwrap().contains(&node.object_id) {
            return Ok(());
        }
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(relative))?;
        let metadata = file.metadata()?;
        let receipt = [
            metadata.dev().to_le_bytes(),
            metadata.ino().to_le_bytes(),
            node.attr.size.to_le_bytes(),
        ]
        .concat();
        let marker = self.objects.join(format!("{}.complete", node.object_id));
        if metadata.len() == node.attr.size
            && fs::read(&marker).ok().as_deref() == Some(receipt.as_slice())
        {
            self.remember_complete(node.object_id);
            return Ok(());
        }
        let mut offset = 0;
        while offset < node.attr.size {
            let bytes = self.read_node(relative, offset, MAX_READ)?;
            if bytes.is_empty()
                || bytes.len() as u64 > (node.attr.size - offset).min(MAX_READ as u64)
            {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            file.write_all_at(&bytes, offset)?;
            offset += bytes.len() as u64;
        }
        sys::set_times(
            &self.root.join(relative),
            Some(node.attr.atime),
            Some(node.attr.mtime),
            false,
        )?;
        // Publish only after the complete file is durable. A different VM
        // runner can reuse it without rewriting a checkpoint's backing inode.
        file.sync_all()?;
        Persistence::atomic_write(&marker, &receipt, 0o600).map_err(io::Error::other)?;
        self.remember_complete(node.object_id);
        Ok(())
    }
    fn materialize_tree(&self, relative: &Path) -> io::Result<()> {
        let node = self.project(relative)?;
        if node.attr.kind == FileType::Directory {
            self.prepare_directory(relative)?;
            let entries = self.source.entries(relative)?;
            for name in entries {
                if name != "." && name != ".." {
                    self.materialize_tree(&relative.join(name))?;
                }
            }
            self.restore_directory_time(relative, &node)?;
        } else {
            self.materialize_file(relative)?;
        }
        if relative.as_os_str().is_empty() {
            // Once every image name exists, aliases can use those native
            // links. Retire the private indexing links so an owned export
            // passes the ordinary no-external-hardlinks check unchanged.
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(self.objects.join("projection.lock"))?;
            lock.lock_exclusive()?;
            for entry in fs::read_dir(&self.objects)? {
                let entry = entry?;
                let name = entry.file_name();
                if !name.as_bytes().is_empty() && name.as_bytes().iter().all(u8::is_ascii_digit) {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
}
