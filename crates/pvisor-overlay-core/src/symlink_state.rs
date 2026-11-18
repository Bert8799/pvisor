//! Inode-bound logical metadata for Linux symlinks in exclusively owned uppers.
//! Internal anchors prevent inode reuse; names are never metadata identities.
use crate::backend::{FileAttr, FileType};
use pvisor_journal::api::{DurableFiles, Persistence};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const DIRECTORY: &str = ".wh..pvisor-symlink-metadata-v1";
const FORMAT: &[u8] = b"pvisor-symlink-metadata-v1\n";
const LIMIT: u64 = 8 * 1024 * 1024;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Root {
    path: PathBuf,
    directory: Mutex<Option<Directory>>,
    writers: AtomicUsize,
}

#[derive(Debug)]
struct Directory {
    file: File,
    binding: Vec<u8>,
}

/// Presence/identity of the metadata namespace is stable for this owner lifetime.
/// Writers already exclusively own their upper; readers never create or repair.
#[derive(Debug)]
pub(crate) struct Owner(Arc<Root>, bool);
impl Owner {
    pub(crate) fn restore(
        &self,
        destination: &Path,
        identity: crate::backend::UnixIdentity,
        attrs: &[(Vec<u8>, Vec<u8>)],
    ) -> io::Result<()> {
        if !self.1 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        self.0.restore(destination, identity, attrs)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if self.1 {
            self.0.writers.fetch_sub(1, Ordering::Release);
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        if self
            .directory
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            ACTIVE.fetch_sub(1, Ordering::Release);
        }
        let mut registry = roots().lock().unwrap_or_else(|e| e.into_inner());
        if registry
            .get(&self.path)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            registry.remove(&self.path);
        }
    }
}
fn roots() -> &'static Mutex<BTreeMap<PathBuf, Weak<Root>>> {
    static ROOTS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Root>>>> = OnceLock::new();
    ROOTS.get_or_init(Mutex::default)
}
fn error() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}
fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.len() > LIMIT
    {
        return Err(error());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(error());
    }
    Ok(bytes)
}
fn binding_prefix(root: &Path) -> io::Result<Vec<u8>> {
    let m = fs::metadata(root)?;
    if !m.is_dir() {
        return Err(error());
    }
    let mut prefix = FORMAT.to_vec();
    prefix.extend_from_slice(format!("{:x}:{:x}:", m.dev(), m.ino()).as_bytes());
    Ok(prefix)
}
fn validate_binding(root: &Path, bytes: &[u8]) -> io::Result<()> {
    let prefix = binding_prefix(root)?;
    let suffix = bytes.strip_prefix(prefix.as_slice()).ok_or_else(error)?;
    if suffix.len() != 36
        || std::str::from_utf8(suffix)
            .ok()
            .and_then(|text| uuid::Uuid::parse_str(text).ok())
            .is_none()
    {
        return Err(error());
    }
    Ok(())
}
fn open_directory(root: &Path) -> io::Result<Option<Directory>> {
    let path = root.join(DIRECTORY);
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let m = directory.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(error());
    }
    if !cfg!(target_os = "linux") {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    let binding = read_file(&path.join("FORMAT")).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            error()
        } else {
            e
        }
    })?;
    validate_binding(root, &binding)?;
    Ok(Some(Directory {
        file: directory,
        binding,
    }))
}
pub(crate) fn internal_directory(entry: &fs::DirEntry) -> io::Result<bool> {
    if entry.file_name() != DIRECTORY {
        return Ok(false);
    }
    let path = entry.path();
    validate_namespace(path.parent().ok_or_else(error)?)?;
    Ok(true)
}
pub(crate) fn validate_namespace(root: &Path) -> io::Result<()> {
    open_directory(root)?.ok_or_else(error)?;
    Ok(())
}
pub(crate) fn contains(root: &Path) -> io::Result<bool> {
    Ok(open_directory(root)?.is_some())
}
pub(crate) fn attach(root: &Path, writable: bool) -> io::Result<Option<Owner>> {
    let path = root.canonicalize()?;
    let mut registry = roots().lock().map_err(|_| error())?;
    let root = match registry.get(&path).and_then(Weak::upgrade) {
        Some(root) => root,
        None => {
            let directory = open_directory(&path)?;
            if !writable && directory.is_none() {
                return Ok(None);
            }
            if directory.is_some() {
                ACTIVE.fetch_add(1, Ordering::Release);
            }
            let root = Arc::new(Root {
                path,
                directory: Mutex::new(directory),
                writers: AtomicUsize::new(0),
            });
            registry.insert(root.path.clone(), Arc::downgrade(&root));
            root
        }
    };
    if writable {
        root.writers.fetch_add(1, Ordering::Release);
    }
    Ok(Some(Owner(root, writable)))
}
fn find(path: &Path) -> io::Result<Option<Arc<Root>>> {
    let registry = roots().lock().map_err(|_| error())?;
    let select = |path: &Path| {
        for ancestor in path.ancestors() {
            if let Some(entry) = registry.get(ancestor) {
                let relative = path.strip_prefix(ancestor).ok()?;
                if relative.starts_with(DIRECTORY)
                    || !relative
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
                {
                    return None;
                }
                if let Some(root) = entry.upgrade() {
                    return Some(root);
                }
            }
        }
        None
    };
    if let Some(root) = select(path) {
        return Ok(Some(root));
    }
    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(select(&parent.canonicalize()?.join(name)))
}

/// Cold copy/apply helpers discover existing state, without creation or repair.
pub(crate) fn attach_containing(path: &Path) -> io::Result<Option<Owner>> {
    if !fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Ok(None);
    }
    let mut parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    loop {
        match fs::symlink_metadata(parent.join(DIRECTORY)) {
            Ok(_) => return attach(&parent, false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if !parent.pop() {
            return Ok(None);
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    device: u64,
    inode: u64,
    host_uid: u32,
    host_gid: u32,
    uid: u32,
    gid: u32,
    perm: u16,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ctime: Option<(i64, i64)>,
}
fn key(m: &fs::Metadata) -> String {
    format!("{:x}-{:x}", m.dev(), m.ino())
}
impl Root {
    fn restore(
        &self,
        destination: &Path,
        identity: crate::backend::UnixIdentity,
        attrs: &[(Vec<u8>, Vec<u8>)],
    ) -> io::Result<()> {
        if self.writers.load(Ordering::Acquire) == 0 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let mut xattrs = attrs.to_vec();
        xattrs.sort_by(|a, b| a.0.cmp(&b.0));
        if identity.mode & !0o177777 != 0
            || identity.mode & 0o170000 != 0 && identity.mode & 0o170000 != 0o120000
            || xattrs.windows(2).any(|p| p[0].0 == p[1].0)
            || xattrs.iter().any(|(n, _)| {
                n.is_empty() || n.contains(&0) || n == b"user.containers.override_stat"
            })
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let m = fs::symlink_metadata(destination)?;
        if !m.file_type().is_symlink() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        self.read(&m)?;
        self.write(
            destination,
            Record {
                device: 0,
                inode: 0,
                host_uid: 0,
                host_gid: 0,
                uid: identity.uid,
                gid: identity.gid,
                perm: (identity.mode & 0o7777) as u16,
                xattrs,
                ctime: None,
            },
        )
    }

    fn directory_path(&self, directory: &Directory) -> io::Result<PathBuf> {
        let current = fs::symlink_metadata(self.path.join(DIRECTORY)).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                error()
            } else {
                e
            }
        })?;
        let pinned = directory.file.metadata()?;
        if !current.is_dir() || (current.dev(), current.ino()) != (pinned.dev(), pinned.ino()) {
            return Err(error());
        }
        validate_binding(&self.path, &directory.binding)?;
        if read_file(&self.path.join(DIRECTORY).join("FORMAT"))? != directory.binding {
            return Err(error());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            Ok(PathBuf::from(format!(
                "/proc/self/fd/{}",
                directory.file.as_raw_fd()
            )))
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
    }
    fn read(&self, m: &fs::Metadata) -> io::Result<Option<Record>> {
        let guard = self.directory.lock().map_err(|_| error())?;
        self.read_record(guard.as_ref(), m)
    }
    fn read_record(
        &self,
        directory: Option<&Directory>,
        m: &fs::Metadata,
    ) -> io::Result<Option<Record>> {
        let Some(directory) = directory else {
            return Ok(None);
        };
        let directory = self.directory_path(directory)?;
        let anchor = directory.join(format!("{}.link", key(m)));
        let bytes = match read_file(&directory.join(format!("{}.json", key(m)))) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return match fs::symlink_metadata(anchor) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                    _ => Err(error()),
                };
            }
            Err(e) => return Err(e),
        };
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| error())?;
        let pinned = fs::symlink_metadata(anchor).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                error()
            } else {
                e
            }
        })?;
        if !pinned.file_type().is_symlink()
            || m.nlink() < 2
            || (pinned.dev(), pinned.ino()) != (m.dev(), m.ino())
            || (record.device, record.inode) != (m.dev(), m.ino())
            || (record.host_uid, record.host_gid) != (m.uid(), m.gid())
            || record.perm > 0o7777
            || record
                .ctime
                .is_some_and(|(_, ns)| !(0..1_000_000_000).contains(&ns))
            || record.xattrs.windows(2).any(|p| p[0].0 >= p[1].0)
            || record.xattrs.iter().any(|(name, _)| {
                name.is_empty() || name.contains(&0) || name == b"user.containers.override_stat"
            })
        {
            return Err(error());
        }
        Ok(Some(record))
    }
    fn write(&self, destination: &Path, mut record: Record) -> io::Result<()> {
        if !cfg!(target_os = "linux") || self.writers.load(Ordering::Acquire) == 0 {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let mut guard = self.directory.lock().map_err(|_| error())?;
        if guard.is_none() {
            let mut binding = binding_prefix(&self.path)?;
            binding.extend_from_slice(uuid::Uuid::new_v4().to_string().as_bytes());
            fs::DirBuilder::new()
                .mode(0o700)
                .create(self.path.join(DIRECTORY))?;
            // Incomplete creation is an explicit error on reopen, never adopted.
            Persistence::atomic_write(&self.path.join(DIRECTORY).join("FORMAT"), &binding, 0o600)
                .map_err(io::Error::other)?;
            *guard = Some(open_directory(&self.path)?.ok_or_else(error)?);
            ACTIVE.fetch_add(1, Ordering::Release);
            File::open(&self.path)?.sync_all()?;
        }
        let directory = self.directory_path(guard.as_ref().ok_or_else(error)?)?;
        let m = fs::symlink_metadata(destination)?;
        if !m.file_type().is_symlink() {
            return Err(error());
        }
        record.device = m.dev();
        record.inode = m.ino();
        record.host_uid = m.uid();
        record.host_gid = m.gid();
        let anchor = directory.join(format!("{}.link", key(&m)));
        match fs::hard_link(destination, &anchor) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let pinned = fs::symlink_metadata(&anchor)?;
        if !pinned.file_type().is_symlink() || (pinned.dev(), pinned.ino()) != (m.dev(), m.ino()) {
            return Err(error());
        }
        Self::publish_record(&directory, &record)
    }
    fn publish_record(directory: &Path, record: &Record) -> io::Result<()> {
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        Persistence::atomic_write(
            &directory.join(format!("{:x}-{:x}.json", record.device, record.inode)),
            &bytes,
            0o600,
        )
        .map_err(io::Error::other)?;
        Ok(())
    }
}
fn lookup(path: &Path) -> io::Result<Option<(fs::Metadata, Record)>> {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return Ok(None);
    }
    let Some(root) = find(path)? else {
        return Ok(None);
    };
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_symlink() {
        return Ok(None);
    }
    Ok(root.read(&m)?.map(|r| (m, r)))
}
pub(crate) fn attributes(path: &Path) -> io::Result<Option<FileAttr>> {
    let Some((m, r)) = lookup(path)? else {
        return Ok(None);
    };
    Ok(Some(FileAttr {
        ino: m.ino(),
        size: m.len(),
        blocks: m.blocks(),
        atime: crate::sys::unix_time(m.atime(), m.atime_nsec()),
        mtime: crate::sys::unix_time(m.mtime(), m.mtime_nsec()),
        ctime: r.ctime.map_or_else(
            || crate::sys::unix_time(m.ctime(), m.ctime_nsec()),
            |(seconds, nanos)| {
                crate::sys::unix_time(seconds, nanos)
                    .max(crate::sys::unix_time(m.ctime(), m.ctime_nsec()))
            },
        ),
        crtime: crate::sys::unix_time(m.ctime(), m.ctime_nsec()),
        kind: FileType::Symlink,
        perm: r.perm,
        uid: r.uid,
        gid: r.gid,
        nlink: (m.nlink() - 1).try_into().map_err(|_| error())?,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }))
}
pub(crate) fn xattrs(path: &Path) -> io::Result<Option<BTreeMap<Vec<u8>, Vec<u8>>>> {
    Ok(lookup(path)?.map(|(_, r)| r.xattrs.into_iter().collect()))
}

/// Preserve a symlink's logical fields before publishing its native destination.
/// false retains the ordinary native path. Only a live writable owner may create
/// records. Failed writes can retain private orphan pins, never a false receipt.
pub(crate) fn preserve(source: &Path, destination: &Path, upper: &Path) -> io::Result<bool> {
    let native = fs::symlink_metadata(source)?;
    if !native.file_type().is_symlink() {
        return Ok(false);
    }
    let remote = crate::backend::attributes(source)?;
    if !cfg!(target_os = "linux")
        || remote.is_none()
        || crate::backend::symlink_metadata_policy(source)?
            == Some(crate::backend::SymlinkMetadataPolicy::LegacyNative)
    {
        // Do not turn native lowers or the old cache's best-effort metadata
        // behavior into a new storage format with stricter export requirements.
        return Ok(false);
    }
    let uid = remote.as_ref().map_or(native.uid(), |a| a.uid);
    let gid = remote.as_ref().map_or(native.gid(), |a| a.gid);
    let perm = remote
        .as_ref()
        .map_or((native.mode() & 0o7777) as u16, |a| a.perm);
    let mut attrs = crate::sys::list_xattrs(source)?
        .into_iter()
        .map(|name| {
            let value = crate::sys::get_xattr(source, std::ffi::OsStr::from_bytes(&name))?;
            Ok((name, value))
        })
        .collect::<io::Result<Vec<_>>>()?;
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    if perm > 0o7777 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // Core's unpublished inode may live in its separate, same-filesystem work
    // directory. Its eventual upper inode, not the temporary path, owns metadata.
    let root = find(&upper.canonicalize()?)?
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EOPNOTSUPP))?;
    root.write(
        destination,
        Record {
            device: 0,
            inode: 0,
            host_uid: 0,
            host_gid: 0,
            uid,
            gid,
            perm,
            xattrs: attrs,
            ctime: None,
        },
    )?;
    Ok(true)
}

/// Serialize the complete read/modify/publish cycle, including across aliases.
/// Namespace mutation remains serialized by the caller's upper ownership.
fn update(path: &Path, change: impl FnOnce(&mut Record) -> io::Result<()>) -> io::Result<bool> {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return Ok(false);
    }
    let Some(root) = find(path)? else {
        return Ok(false);
    };
    let guard = root.directory.lock().map_err(|_| error())?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let Some(mut record) = root.read_record(guard.as_ref(), &metadata)? else {
        return Ok(false);
    };
    if root.writers.load(Ordering::Acquire) == 0 {
        return Err(io::Error::from_raw_os_error(libc::EROFS));
    }
    change(&mut record)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    record.ctime = Some((
        now.as_secs().try_into().map_err(|_| error())?,
        now.subsec_nanos().into(),
    ));
    Root::publish_record(
        &root.directory_path(guard.as_ref().ok_or_else(error)?)?,
        &record,
    )?;
    Ok(true)
}

pub(crate) fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<bool> {
    set_attributes(
        path,
        (uid != u32::MAX).then_some(uid),
        (gid != u32::MAX).then_some(gid),
        None,
    )
}
pub(crate) fn set_attributes(
    path: &Path,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u16>,
) -> io::Result<bool> {
    update(path, |record| {
        if let Some(uid) = uid {
            record.uid = uid;
        }
        if let Some(gid) = gid {
            record.gid = gid;
        }
        if let Some(mode) = mode {
            if mode > 0o7777 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            record.perm = mode;
        }
        Ok(())
    })
}

pub(crate) fn set_xattr(
    path: &Path,
    name: &[u8],
    value: Option<&[u8]>,
    flags: i32,
) -> io::Result<bool> {
    update(path, |record| {
        // Linux no-follow xattr mechanics; authorization belongs to the adapter.
        if name.contains(&0) || flags & !3 != 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if name.is_empty() || name.len() > 255 {
            return Err(io::Error::from_raw_os_error(libc::ERANGE));
        }
        if name == b"user.containers.override_stat" {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        if value.is_some_and(|v| v.len() > 65536) {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        let index = record
            .xattrs
            .binary_search_by(|(key, _)| key.as_slice().cmp(name));
        match (index, value) {
            (Ok(_), Some(_)) if flags & 1 != 0 => Err(io::Error::from_raw_os_error(libc::EEXIST)),
            (Err(_), _) if value.is_none() || flags & 2 != 0 => {
                Err(io::Error::from_raw_os_error(libc::ENODATA))
            }
            (Ok(i), Some(value)) => {
                record.xattrs[i].1 = value.to_vec();
                Ok(())
            }
            (Err(i), Some(value)) => {
                record.xattrs.insert(i, (name.to_vec(), value.to_vec()));
                Ok(())
            }
            (Ok(i), None) => {
                record.xattrs.remove(i);
                Ok(())
            }
            (Err(_), None) => unreachable!(),
        }
    })
}

/// Use an already admitted writable tree; never infer mutation authority from a name.
pub(crate) fn restore_at(
    path: &Path,
    identity: crate::backend::UnixIdentity,
    attrs: &[(Vec<u8>, Vec<u8>)],
) -> io::Result<()> {
    find(path)?
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EOPNOTSUPP))?
        .restore(path, identity, attrs)
}

/// Nested traversals reuse their outer writer instead of creating nested stores.
pub(crate) fn write_owner(root: &Path) -> io::Result<Option<Owner>> {
    if find(root)?.is_some_and(|r| r.writers.load(Ordering::Acquire) > 0) {
        return Ok(None);
    }
    attach(root, true)
}

pub(crate) fn copy_metadata(source: &Path, destination: &Path) -> io::Result<bool> {
    let Some(attr) = crate::backend::logical_symlink_attributes(source)? else {
        return Ok(false);
    };
    let _owner = write_owner(destination.parent().ok_or_else(error)?)?;
    let attrs = crate::sys::list_xattrs(source)?
        .into_iter()
        .map(|name| {
            let value = crate::sys::get_xattr(source, std::ffi::OsStr::from_bytes(&name))?;
            Ok((name, value))
        })
        .collect::<io::Result<Vec<_>>>()?;
    restore_at(
        destination,
        crate::backend::UnixIdentity {
            uid: attr.uid,
            gid: attr.gid,
            mode: u32::from(attr.perm),
        },
        &attrs,
    )?;
    crate::sys::set_times(destination, Some(attr.atime), Some(attr.mtime), true)?;
    Ok(true)
}
