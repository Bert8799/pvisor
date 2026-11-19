//! Experimental local RAFS source and explicit fresh-VM runner handoff.
use pvisor_overlay_core::backend::{BackendAttachment, FileAttr, FileType, ImageNode, ImageSource};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

const MAX: usize = 1024 * 1024;
const PREFIX: &str = "pvisor-rafs-merged-v1:";
const VIEW_PREFIX: &str = ".pvisor-rafs-view-";
mod artifact;
mod node;
mod node_service;
mod runner;
pub use artifact::RafsArtifact;
pub(crate) use node::view_artifact as node_artifact;
pub use node::{RafsNodeBinding, RafsNodeLease, RafsNodeRootfs};
pub(crate) use node_service::{RafsCache, RafsEndpoint};
pub use runner::RafsRootfs;
pub(crate) use runner::{private_owner, reattach};
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub(crate) fn private(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(invalid(
            "RAFS control directory must be private, owned and non-symlink",
        ));
    }
    Ok(())
}
pub(crate) fn revision(handle: &str) -> io::Result<&str> {
    handle
        .strip_prefix(PREFIX)
        .filter(|v| {
            v.len() == 64
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| invalid("expected fixed RAFS merged handle"))
}
pub(crate) fn identity(file: &File) -> io::Result<(u64, u64)> {
    let m = file.metadata()?;
    if !m.is_file()
        || m.mode() & 0o022 != 0
        || (m.uid() != unsafe { libc::geteuid() } && m.uid() != 0)
    {
        return Err(invalid("RAFS input must be a protected regular owned file"));
    }
    Ok((m.dev(), m.ino()))
}

/// One authenticated local image engine shared by its Core projections.
///
/// This Linux prototype consumes fixed merged artifacts with the explicitly
/// supplied trusted native reader from scripts/images. It never resolves tags,
/// imports OCI or launches a VM. The caller keeps the private store immutable and
/// outside guest access until all views stop; no cache/GC lease is acquired here.
/// Dropping this owner leaves active views valid. The final view/owner drop stops
/// and reaps the one engine and removes its private cache/log directory.
pub struct RafsImage {
    source: Arc<Engine>,
}

/// One private sparse native projection, registered in the current process only.
///
/// Retain it through Core I/O and teardown. Read data comes from the shared image;
/// copy-up and explicit owned export materialize only the required native bytes.
/// Drop retires the attachment before deleting its private backing. This is not
/// a portable rootfs path: only `RafsRootfs::prepare` writes a runner binding;
/// ordinary SDK projections cannot be passed to self-exec runners.
pub struct RafsView {
    attachment: BackendAttachment,
    _directory: tempfile::TempDir,
    private_sources: Vec<std::path::PathBuf>,
}
impl RafsView {
    /// Canonical lower path for same-process Core consumers while this view lives.
    /// Do not expose the owner/control directory. Cross-process use requires the
    /// explicit `RafsRootfs::prepare` binding and retaining the parent owner.
    pub fn root(&self) -> &Path {
        self.attachment.root()
    }
    /// Host authority paths that must stay outside every guest-visible/writable
    /// input, including workspace, home, mounts and stage. The slice is immutable
    /// and valid while this view lives. Node views include the service's attested
    /// cache, tool and control roots; callers must reject overlapping inputs
    /// before exposing the projection or starting native execution.
    pub fn private_sources(&self) -> &[std::path::PathBuf] {
        &self.private_sources
    }
}
impl RafsImage {
    /// Authenticate a fixed local merged handle and start one demand reader.
    ///
    /// store must be an existing private owned directory; work_parent must exist
    /// and be disjoint from store. engine is an explicitly trusted executable.
    /// timeout must be positive and representable by Instant, and bounds each
    /// serialized exchange, including authentication at open. All new cache/log
    /// state is private and temporary. Failures stop/reap the child before return.
    /// Unsupported hosts return EOPNOTSUPP before creating state. Metadata opens
    /// do not scan/hash all payloads; the engine authenticates requested chunks.
    pub fn open(
        store: &Path,
        handle: &str,
        engine: &Path,
        work_parent: &Path,
        timeout: Duration,
    ) -> io::Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        if timeout.is_zero() || Instant::now().checked_add(timeout).is_none() {
            return Err(invalid("invalid reader timeout"));
        }
        let revision = handle
            .strip_prefix(PREFIX)
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(|| invalid("expected a fixed merged-image handle"))?;
        private(store)?;
        let store = store.canonicalize()?;
        private(&store.join("merged"))?;
        let parent = work_parent.canonicalize()?;
        if parent.starts_with(&store) || store.starts_with(&parent) {
            return Err(invalid("store and work parent must be disjoint"));
        }
        let directory = tempfile::Builder::new()
            .prefix(".pvisor-rafs-reader-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let log_path = directory.path().join("reader.log");
        let log = File::create(&log_path)?;
        let mut command = Command::new(engine);
        command
            .arg(store.join("merged").join(revision))
            .arg(handle)
            .arg(directory.path().join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            let parent = libc::getpid();
            command.pre_exec(move || {
                // The VMM may use _exit: do not depend on Rust destructors to
                // retire its reader. Cover the race before installing the signal.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut worker = Worker {
            input: child.stdin.take(),
            output: child.stdout.take().unwrap(),
            child,
            timeout,
        };
        for fd in [
            worker.input.as_ref().unwrap().as_raw_fd(),
            worker.output.as_raw_fd(),
        ] {
            // SAFETY: live owned pipe fds; preserve their existing status flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let greeting = worker.frame(Instant::now() + timeout);
        if greeting.as_ref().ok() != Some(&json!({"version":1,"handle":handle})) {
            let kind = greeting
                .as_ref()
                .err()
                .map_or(io::ErrorKind::InvalidData, io::Error::kind);
            drop(worker);
            let mut detail = String::new();
            File::open(log_path)?
                .take(8192)
                .read_to_string(&mut detail)?;
            return Err(io::Error::new(
                kind,
                format!(
                    "RAFS reader did not authenticate the requested image: {greeting:?}; {detail}"
                ),
            ));
        }
        Ok(Self {
            source: Arc::new(Engine {
                worker: Mutex::new(Some(worker)),
                remote: None,
                client_limits: None,
                private_sources: vec![store, engine.canonicalize()?],
                _lease: None,
                _directory: Some(directory),
            }),
        })
    }

    /// Create a new private view of this same image without starting another engine.
    /// parent must already exist. Keep the returned view alive through Core I/O;
    /// source and cache remain shared, while native projections and uppers are private.
    /// Host-unrepresentable metadata/xattrs fail access explicitly. No guest or
    /// cross-process binding is installed. An unsuccessful projection is removed.
    pub fn project(&self, parent: &Path) -> io::Result<RafsView> {
        let directory = tempfile::Builder::new()
            .prefix(VIEW_PREFIX)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let root = directory.path().join("lower");
        fs::create_dir(&root)?;
        let objects = directory.path().join("objects");
        let attachment = if let Some(limits) = self.source.client_limits {
            pvisor_overlay_core::backend::attach_image_bounded(
                &root,
                &objects,
                self.source.clone(),
                limits.metadata_entries,
            )?
        } else {
            pvisor_overlay_core::backend::attach_image(&root, &objects, self.source.clone())?
        };
        Ok(RafsView {
            attachment,
            _directory: directory,
            private_sources: self.source.private_sources.clone(),
        })
    }
}

pub(crate) fn is_view(root: &Path) -> bool {
    root.file_name().is_some_and(|name| name == "lower")
        && root
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.as_encoded_bytes().starts_with(VIEW_PREFIX.as_bytes()))
}
struct Engine {
    worker: Mutex<Option<Worker>>,
    remote: Option<(crate::cache::CacheClient, String, String)>,
    client_limits: Option<node::ClientLimits>,
    private_sources: Vec<std::path::PathBuf>,
    _lease: Option<File>,
    _directory: Option<tempfile::TempDir>,
}
struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    timeout: Duration,
}
enum Reply {
    Node(ImageNode),
    Names(Vec<OsString>, Option<u32>),
    Data(Vec<u8>),
    Error(i32),
}
impl Engine {
    fn request(&self, request: Value) -> io::Result<Reply> {
        if let Some((client, handle, generation)) = &self.remote {
            let operation = serde_json::from_value(request.clone()).map_err(io::Error::other)?;
            let (reply, body) = client
                .request(crate::cache::Request::Rafs {
                    handle: handle.clone(),
                    request: operation,
                    generation: Some(generation.clone()),
                })
                .map_err(io::Error::other)?;
            let crate::cache::Response::Rafs { frame, .. } = reply else {
                return Err(invalid("expected RAFS node response"));
            };
            let mut input = body.as_slice();
            let reply = decode_reply(&request, frame, |bytes| input.read_exact(bytes))?;
            if !input.is_empty() {
                return Err(invalid("unexpected RAFS node body"));
            }
            return match reply {
                Reply::Error(errno) => Err(io::Error::from_raw_os_error(errno)),
                reply => Ok(reply),
            };
        }
        let mut guard = self
            .worker
            .lock()
            .map_err(|_| invalid("RAFS reader lock poisoned"))?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| invalid("RAFS reader is closed"))?;
        match worker.request(request) {
            Ok(Reply::Error(errno)) => Err(io::Error::from_raw_os_error(errno)),
            Ok(reply) => Ok(reply),
            Err(error) => {
                drop(guard.take());
                Err(error)
            }
        }
    }
}
impl ImageSource for Engine {
    fn metadata(&self, relative: &Path) -> io::Result<ImageNode> {
        pvisor_overlay_core::OverlayCore::validate_rel(relative)?;
        match self.request(json!({"op":"stat","path":text(relative)?}))? {
            Reply::Node(node) => Ok(node),
            _ => Err(invalid("expected metadata")),
        }
    }
    fn entries(&self, relative: &Path) -> io::Result<Vec<OsString>> {
        pvisor_overlay_core::OverlayCore::validate_rel(relative)?;
        let path = text(relative)?;
        let mut names = Vec::new();
        let mut retained_bytes = 0usize;
        let mut offset = 0;
        loop {
            match self.request(json!({"op":"list","path":path,"offset":offset,"limit":128}))? {
                Reply::Names(page, next) => {
                    if let Some(limits) = self.client_limits {
                        for name in &page {
                            retained_bytes = retained_bytes
                                .checked_add(name.capacity() + std::mem::size_of::<OsString>())
                                .filter(|v| *v <= limits.directory_bytes)
                                .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
                        }
                    }
                    if let Some(limits) = self.client_limits {
                        names
                            .try_reserve_exact(page.len())
                            .map_err(|_| io::Error::from_raw_os_error(libc::ENOMEM))?;
                        let spare = (names.capacity() - names.len() - page.len())
                            * std::mem::size_of::<OsString>();
                        if retained_bytes.saturating_add(spare) > limits.directory_bytes {
                            return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
                        }
                    }
                    names.extend(page);
                    if names.len() > 200_000 {
                        return Err(invalid("image directory too large"));
                    }
                    match next {
                        None => return Ok(names),
                        Some(n) if n > offset => offset = n,
                        _ => return Err(invalid("invalid directory cursor")),
                    }
                }
                _ => return Err(invalid("expected directory page")),
            }
        }
    }
    fn read_at(&self, relative: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        pvisor_overlay_core::OverlayCore::validate_rel(relative)?;
        if size as usize > MAX || offset.checked_add(size as u64).is_none() {
            return Err(invalid("invalid read range"));
        }
        match self
            .request(json!({"op":"read","path":text(relative)?,"offset":offset,"length":size}))?
        {
            Reply::Data(data) => Ok(data),
            _ => Err(invalid("expected file bytes")),
        }
    }
}
fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EILSEQ))
}
impl Worker {
    fn transfer(&mut self, bytes: &mut [u8], writing: bool, deadline: Instant) -> io::Result<()> {
        let fd = if writing {
            self.input.as_ref().unwrap().as_raw_fd()
        } else {
            self.output.as_raw_fd()
        };
        let mut done = 0;
        while done < bytes.len() {
            let wait = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "RAFS reader deadline"))?;
            let mut poll = libc::pollfd {
                fd,
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            };
            // SAFETY: one valid pollfd and live pipe; the buffer remains exclusively
            // borrowed for the synchronous read/write call and is never retained.
            let ready = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    wait.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RAFS reader deadline",
                ));
            }
            let n = unsafe {
                if writing {
                    libc::write(fd, bytes[done..].as_ptr().cast(), bytes.len() - done)
                } else {
                    libc::read(fd, bytes[done..].as_mut_ptr().cast(), bytes.len() - done)
                }
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RAFS reader pipe closed",
                ));
            }
            done += n as usize;
        }
        Ok(())
    }
    fn frame(&mut self, deadline: Instant) -> io::Result<Value> {
        let mut head = [0; 4];
        self.transfer(&mut head, false, deadline)?;
        let size = u32::from_be_bytes(head) as usize;
        if size == 0 || size > MAX {
            return Err(invalid("invalid reader frame"));
        }
        let mut raw = vec![0; size];
        self.transfer(&mut raw, false, deadline)?;
        serde_json::from_slice(&raw).map_err(io::Error::other)
    }
    fn request(&mut self, request: Value) -> io::Result<Reply> {
        let mut bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
        if bytes.len() > MAX {
            return Err(invalid("request too large"));
        }
        let deadline = Instant::now() + self.timeout;
        self.transfer(&mut (bytes.len() as u32).to_be_bytes(), true, deadline)?;
        self.transfer(&mut bytes, true, deadline)?;
        let reply = self.frame(deadline)?;
        decode_reply(&request, reply, |bytes| {
            self.transfer(bytes, false, deadline)
        })
    }
}

fn decode_reply(
    request: &Value,
    reply: Value,
    mut read: impl FnMut(&mut [u8]) -> io::Result<()>,
) -> io::Result<Reply> {
    if reply["status"] == "error" {
        let errno = reply["errno"]
            .as_u64()
            .filter(|n| (1..=4095).contains(n))
            .ok_or_else(|| invalid("invalid reader errno"))?;
        return Ok(Reply::Error(errno as i32));
    }
    if reply["status"] != "ok" {
        return Err(invalid("invalid reader status"));
    }
    match request["op"].as_str() {
        Some("stat") => Ok(Reply::Node(
            serde_json::from_value::<Metadata>(reply["node"].clone())
                .map_err(io::Error::other)?
                .node()?,
        )),
        Some("list") => {
            let page: Page =
                serde_json::from_value(reply["page"].clone()).map_err(io::Error::other)?;
            if page.names.len() > 128
                || page.next_offset.is_some_and(|next| {
                    page.names.is_empty()
                        || u64::from(next)
                            != request["offset"].as_u64().unwrap() + page.names.len() as u64
                })
                || page.names.iter().any(|n| {
                    n.is_empty() || matches!(n.as_str(), "." | "..") || n.contains(['/', '\0'])
                })
            {
                return Err(invalid("invalid directory names"));
            }
            Ok(Reply::Names(
                page.names.into_iter().map(OsString::from).collect(),
                page.next_offset,
            ))
        }
        Some("read") => {
            let length = reply["length"]
                .as_u64()
                .filter(|n| *n <= request["length"].as_u64().unwrap())
                .ok_or_else(|| invalid("invalid body length"))?;
            let mut data = vec![0; length as usize];
            read(&mut data)?;
            use sha2::{Digest, Sha256};
            if reply["sha256"]
                != format!("sha256:{}", crate::util::encode_hex(&Sha256::digest(&data)))
            {
                return Err(invalid("reader body checksum mismatch"));
            }
            Ok(Reply::Data(data))
        }
        _ => Err(invalid("unsupported operation")),
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.input.take();
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: the unreaped child's process group was created exclusively for
        // this owner. No host/guest process outside that group is targeted.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        for _ in 0..50 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    names: Vec<String>,
    next_offset: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    kind: String,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    nlink: u32,
    mtime: u64,
    mtime_nsec: u32,
    xattrs: BTreeMap<String, Vec<u8>>,
    target: Option<String>,
}
impl Metadata {
    fn node(self) -> io::Result<ImageNode> {
        for name in self.xattrs.keys() {
            if name == "user.containers.override_stat"
                || pvisor_overlay_core::validate_guest_xattr(std::ffi::OsStr::new(name)).is_err()
            {
                return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
            }
        }
        let kind = match self.kind.as_str() {
            "file" => FileType::RegularFile,
            "directory" => FileType::Directory,
            "symlink" => FileType::Symlink,
            _ => return Err(invalid("unsupported image node")),
        };
        if self.mode > 0o7777
            || self.mtime_nsec >= 1_000_000_000
            || self.nlink == 0
            || self.inode == 0
            || (kind == FileType::Symlink) != self.target.is_some()
        {
            return Err(invalid("invalid image attributes"));
        }
        let time = UNIX_EPOCH
            .checked_add(Duration::new(self.mtime, self.mtime_nsec))
            .ok_or_else(|| invalid("image timestamp overflow"))?;
        Ok(ImageNode {
            object_id: self.inode,
            target: self.target.map(String::into_bytes),
            xattrs: self
                .xattrs
                .into_iter()
                .map(|(k, v)| (k.into_bytes(), v))
                .collect(),
            attr: FileAttr {
                ino: self.inode,
                size: self.size,
                blocks: self.size.div_ceil(512),
                atime: time,
                mtime: time,
                ctime: time,
                crtime: time,
                kind,
                perm: self.mode as u16,
                uid: self.uid,
                gid: self.gid,
                nlink: self.nlink,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            },
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// Save a quiescent merged image as a flattened tar with logical Unix metadata.
///
/// UID/GID, permissions, nanosecond mtime, supported xattrs, symlink targets and
/// regular-file hardlinks are recorded in the archive without host chown. Whiteouts
/// and internal ownership records are omitted. Existing container exports and
/// host workspace apply retain their own contracts. No per-VM cache is allocated.
///
/// Retain every lower/reader owner and exclude all writers for the entire call.
/// `output` must be independent of the source trees. This call neither applies
/// nor consumes staged changes and does not freeze a VM or publish an image.
/// The caller owns output flushing/publication; an error can leave partial bytes.
/// Security capabilities, stored overlay-control xattrs, non-UTF-8 xattr names,
/// negative timestamps and special files are rejected rather than approximated.
///
/// ```
/// use std::{fs, path::Path};
/// use pvisor_overlay_core::OverlayCore;
/// let temp = tempfile::tempdir()?;
/// let lower = temp.path().join("lower");
/// fs::create_dir(&lower)?;
/// fs::write(lower.join("file"), b"image bytes")?;
/// let core = OverlayCore::new(vec![lower], temp.path().join("upper"), None)?;
/// let mut archive = Vec::new();
/// pvisor::rafs::export_image(&core, &mut archive)?;
/// assert!(archive.len() >= 1024);
/// assert_eq!(fs::read(core.metadata_resolved(Path::new("file"))?.resolved.path)?, b"image bytes");
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn export_image(
    core: &pvisor_overlay_core::OverlayCore,
    output: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    crate::image::archive::write(
        core,
        output,
        crate::image::archive::MetadataProfile::LogicalImage,
    )
}
