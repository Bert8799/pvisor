//! Cache-service owner for one bounded, multi-image native process.
use super::node::ClientLimits;
use super::*;
#[path = "node_metadata.rs"]
mod metadata;
use crate::cache::{RafsRequest, Response};
use anyhow::Context;
use anyhow::ensure;
#[cfg(target_os = "linux")]
use pvisor_journal::api::{DurableFiles, Persistence};
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(target_os = "linux")]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

const EMPTY_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[cfg(target_os = "linux")]
fn images() -> usize {
    4
}
#[cfg(target_os = "linux")]
fn capacity() -> u64 {
    1024 * 1024 * 1024
}
#[cfg(target_os = "linux")]
fn workers() -> usize {
    4
}
#[cfg(target_os = "linux")]
fn metadata_bytes() -> u64 {
    64 * 1024 * 1024
}
#[cfg(target_os = "linux")]
fn io_bytes() -> u64 {
    64 * 1024 * 1024
}
#[cfg(target_os = "linux")]
fn downloads() -> usize {
    4
}
#[cfg(target_os = "linux")]
fn address_space() -> u64 {
    1024 * 1024 * 1024
}
#[cfg(target_os = "linux")]
fn client_entries() -> usize {
    1024
}
#[cfg(target_os = "linux")]
fn client_directory() -> usize {
    4 * 1024 * 1024
}
#[cfg(target_os = "linux")]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    store: std::path::PathBuf,
    reader: std::path::PathBuf,
    cache: std::path::PathBuf,
    origin: String,
    #[serde(default = "images")]
    max_images: usize,
    #[serde(default = "capacity")]
    max_cache_bytes: u64,
    #[serde(default = "workers")]
    workers: usize,
    #[serde(default = "metadata_bytes")]
    max_metadata_bytes: u64,
    #[serde(default = "io_bytes")]
    max_io_bytes: u64,
    #[serde(default = "downloads")]
    max_downloads: usize,
    #[serde(default = "address_space")]
    max_address_space_bytes: u64,
    #[serde(default = "client_entries")]
    client_metadata_entries: usize,
    #[serde(default = "client_directory")]
    client_directory_bytes: usize,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Control {
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
}

pub(crate) struct RafsEndpoint {
    socket: std::path::PathBuf,
    alive: AtomicBool,
    private_roots: Vec<std::path::PathBuf>,
    generation: String,
    client_limits: ClientLimits,
    store: std::path::PathBuf,
    metadata: std::sync::Mutex<metadata::Cache>,
}
impl RafsEndpoint {
    pub(crate) fn request(
        &self,
        handle: &str,
        request: RafsRequest,
        generation: Option<&str>,
    ) -> anyhow::Result<(Response, Vec<u8>)> {
        ensure!(
            generation == Some(self.generation.as_str())
                || (generation.is_none()
                    && matches!(&request, RafsRequest::Open { .. } | RafsRequest::Stats)),
            "RAFS service generation changed or was not opened"
        );
        revision(handle)?;
        request.validate()?;
        ensure!(self.alive.load(Ordering::Acquire), "RAFS node is stopping");
        let epoch = {
            let mut cache = self.metadata.lock().unwrap();
            if matches!(request, RafsRequest::Trim { .. } | RafsRequest::Remove {}) {
                cache.clear();
            }
            if !matches!(request, RafsRequest::Stats | RafsRequest::Open { .. }) {
                cache.admit(handle);
            }
            if let RafsRequest::Open {
                commit_device,
                commit_inode,
            } = &request
            {
                cache.select(handle, (*commit_device, *commit_inode))?;
            }
            if let RafsRequest::Stat { path } = &request
                && let Some(frame) = cache.get(handle, path)
            {
                return Ok((
                    Response::Rafs {
                        frame,
                        length: 0,
                        sha256: EMPTY_SHA256.into(),
                    },
                    vec![],
                ));
            }
            cache.epoch
        };
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let operation = serde_json::to_value(request)?;
        let call = json!({"handle":handle,"request":operation});
        let bytes = serde_json::to_vec(&call)?;
        ensure!(bytes.len() <= MAX, "RAFS node request exceeds frame limit");
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        let mut header = [0; 4];
        stream.read_exact(&mut header)?;
        let count = u32::from_be_bytes(header) as usize;
        ensure!(count > 0 && count <= MAX, "invalid RAFS node frame");
        let mut bytes = vec![0; count];
        stream.read_exact(&mut bytes)?;
        let mut frame: Value = serde_json::from_slice(&bytes)?;
        let mut body = Vec::new();
        if operation["op"] == "open" && frame["status"] == "ok" {
            ensure!(
                frame == json!({"status":"ok","handle":handle}),
                "RAFS node opened another image"
            );
            frame["client_limits"] = serde_json::to_value(self.client_limits)?;
            frame["node_generation"] = serde_json::to_value(&self.generation)?;
            frame["private_roots"] = serde_json::to_value(&self.private_roots)?;
            let artifact = RafsArtifact {
                store: self.store.clone(),
                handle: handle.into(),
                device: operation["commit_device"].as_u64().unwrap(),
                inode: operation["commit_inode"].as_u64().unwrap(),
            };
            if let Ok(lease) = artifact.open() {
                let mut cache = self.metadata.lock().unwrap();
                if cache.epoch == epoch {
                    cache.pin(handle, lease)?;
                }
            }
        } else if operation["op"] == "stats" && frame["status"] == "ok" {
            ensure!(
                frame["stats"]["version"] == 1
                    && frame["stats"]["resources"].is_object()
                    && frame["stats"]["process"].is_object(),
                "invalid node resource snapshot"
            );
            frame["stats"]["client_limits"] = serde_json::to_value(self.client_limits)?;
            frame["node_generation"] = serde_json::to_value(&self.generation)?;
            frame["stats"]["shared_metadata_cache"] = self.metadata.lock().unwrap().snapshot();
        } else if frame["status"] == "ok"
            && matches!(operation["op"].as_str(), Some("lease" | "trim" | "remove"))
        {
            let valid = match operation["op"].as_str() {
                Some("lease") => {
                    let r = &frame["dependency"];
                    r["version"] == 1
                        && r["kind"] == operation["kind"]
                        && r["owner"] == operation["owner"]
                        && r["handle"] == handle
                        && r["retained"].is_boolean()
                        && r["key"].as_str().is_some_and(|v| {
                            v.len() == 64
                                && v.bytes()
                                    .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
                        })
                }
                Some("trim") => {
                    frame["cache_trim"]["target_bytes"] == operation["target_bytes"]
                        && frame["cache_trim"]["remaining_bytes"].is_u64()
                        && frame["cache_trim"]["reclaimed_bytes"].is_u64()
                        && frame["cache_trim"]["reclaimed_blobs"].is_u64()
                        && frame["cache_trim"]["target_reached"].is_boolean()
                }
                Some("remove") => frame["removed"] == handle,
                _ => false,
            };
            ensure!(valid, "invalid node lifecycle acknowledgement");
        } else {
            decode_reply(&operation, frame.clone(), |buffer| {
                stream.read_exact(buffer)?;
                body.extend_from_slice(buffer);
                Ok(())
            })?;
            if frame["status"] == "ok" {
                let mut cache = self.metadata.lock().unwrap();
                if operation["op"] == "stat" {
                    cache.insert(handle, operation["path"].as_str().unwrap(), &frame, epoch);
                } else if operation["op"] == "list"
                    && let Some(batch) = frame.as_object_mut().unwrap().remove("metadata_batch")
                {
                    let rows = batch
                        .as_array()
                        .filter(|v| v.len() <= 128)
                        .context("invalid metadata batch")?;
                    for row in rows {
                        let name = row["name"].as_str().context("missing batch name")?;
                        ensure!(
                            frame["page"]["names"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|v| v == name),
                            "metadata outside directory page"
                        );
                        let parent = operation["path"].as_str().unwrap();
                        let path = if parent.is_empty() {
                            name.into()
                        } else {
                            format!("{parent}/{name}")
                        };
                        let stat = json!({"status":"ok","node":row["node"]});
                        // Unsupported child attributes still fail their own
                        // Stat operation; name-only List keeps its contract.
                        if decode_reply(&json!({"op":"stat"}), stat.clone(), |_| unreachable!())
                            .is_ok()
                        {
                            cache.insert(handle, &path, &stat, epoch);
                        }
                    }
                }
            }
        }
        use sha2::{Digest, Sha256};
        let sha256 = format!("sha256:{}", crate::util::encode_hex(&Sha256::digest(&body)));
        Ok((
            Response::Rafs {
                frame,
                length: body.len() as u32,
                sha256,
            },
            body,
        ))
    }
}

pub(crate) struct RafsCache {
    endpoint: Arc<RafsEndpoint>,
    worker: Option<Worker>,
    pidfd: File,
    _directory: tempfile::TempDir,
    cache: std::path::PathBuf,
    _lock: File,
}
impl Drop for RafsCache {
    fn drop(&mut self) {
        self.endpoint.alive.store(false, Ordering::Release);
        drop(self.worker.take());
        let _ = fs::remove_file(self.cache.join("control.json"));
    }
}
impl RafsCache {
    pub(crate) fn endpoint(&self) -> Arc<RafsEndpoint> {
        self.endpoint.clone()
    }
    pub(crate) fn pidfd(&self) -> i32 {
        self.pidfd.as_raw_fd()
    }

    pub(crate) fn start(path: &Path) -> anyhow::Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            anyhow::bail!("RAFS node cache requires Linux");
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::{fd::FromRawFd, unix::process::CommandExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            identity(&file)?;
            ensure!(
                file.metadata()?.len() <= 64 * 1024,
                "RAFS service config exceeds limit"
            );
            let mut options: Config = serde_json::from_reader(file.take(64 * 1024 + 1))?;
            ensure!(
                (1..=16).contains(&options.max_images)
                    && (1..=16).contains(&options.workers)
                    && options.max_cache_bytes > 0
                    && options.max_cache_bytes <= 1 << 40
                    && (4 * 1024 * 1024..=1024 * 1024 * 1024).contains(&options.max_metadata_bytes)
                    && (4 * 1024 * 1024..=1024 * 1024 * 1024).contains(&options.max_io_bytes)
                    && (1..=16).contains(&options.max_downloads)
                    && (128 * 1024 * 1024..=1u64 << 40).contains(&options.max_address_space_bytes),
                "invalid RAFS node limits"
            );
            let client_limits = ClientLimits {
                metadata_entries: options.client_metadata_entries,
                directory_bytes: options.client_directory_bytes,
            };
            client_limits.validate()?;
            private(&options.store)?;
            options.store = options.store.canonicalize()?;
            private(&options.store.join("merged"))?;
            options.reader = options.reader.canonicalize()?;
            let reader = File::open(&options.reader)?;
            identity(&reader)?;
            ensure!(
                reader.metadata()?.mode() & 0o111 != 0,
                "RAFS reader is not executable"
            );
            let cache_parent = options
                .cache
                .parent()
                .context("node cache needs a parent")?
                .canonicalize()?;
            let candidate =
                cache_parent.join(options.cache.file_name().context("invalid cache path")?);
            ensure!(
                !candidate.starts_with(&options.store)
                    && !options.store.starts_with(&candidate)
                    && !options.reader.starts_with(&candidate),
                "RAFS metadata, tool and cache must be independent"
            );
            match fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            private(&candidate)?;
            options.cache = candidate.canonicalize()?;
            let mut origin = reqwest::Url::parse(&options.origin)?;
            ensure!(
                origin.host_str().is_some()
                    && origin.username().is_empty()
                    && origin.password().is_none()
                    && origin.query().is_none()
                    && origin.fragment().is_none(),
                "invalid RAFS origin"
            );
            ensure!(
                origin.scheme() == "https"
                    || (origin.scheme() == "http"
                        && matches!(origin.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))),
                "RAFS origin requires HTTPS or explicit loopback HTTP"
            );
            origin.set_fragment(None);
            options.origin = origin.to_string().trim_end_matches('/').to_owned();
            let cache = options.cache.clone();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(cache.join("service.lock"))?;
            identity(&lock)?;
            fs2::FileExt::try_lock_exclusive(&lock)
                .context("RAFS node service cache is already owned")?;
            let data = cache.join("data");
            match fs::DirBuilder::new().mode(0o700).create(&data) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            private(&data)?;
            let native_lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(data.join("node.lock"))?;
            identity(&native_lock)?;
            fs2::FileExt::try_lock_exclusive(&native_lock)
                .context("previous native node still owns cache")?;
            cleanup_previous(&cache)?;
            let directory = tempfile::Builder::new()
                .prefix("pvisor-rafs-node-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")?;
            let m = directory.path().metadata()?;
            Persistence::atomic_write(
                &cache.join("control.json"),
                &serde_json::to_vec(&Control {
                    path: directory.path().into(),
                    device: m.dev(),
                    inode: m.ino(),
                })?,
                0o600,
            )?;
            options.cache = data;
            let config_path = directory.path().join("node.json");
            let mut native = serde_json::to_value(&options)?;
            for key in [
                "reader",
                "client_metadata_entries",
                "client_directory_bytes",
            ] {
                native.as_object_mut().unwrap().remove(key);
            }
            Persistence::atomic_write(&config_path, &serde_json::to_vec(&native)?, 0o600)?;
            let socket = directory.path().join("reader.sock");
            let mut command = Command::new(&options.reader);
            command
                .env("MALLOC_ARENA_MAX", "2")
                .arg("--node")
                .arg(&config_path)
                .arg(&socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(File::create(directory.path().join("reader.log"))?)
                .process_group(0);
            let parent = std::process::id() as libc::pid_t;
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        return Err(io::Error::from_raw_os_error(libc::ESRCH));
                    }
                    Ok(())
                });
            }
            drop(native_lock);
            let mut child = command.spawn()?;
            let pid = child.id();
            let mut worker = Worker {
                input: child.stdin.take(),
                output: child.stdout.take().unwrap(),
                child,
                timeout: Duration::from_secs(10),
            };
            for fd in [
                worker.input.as_ref().unwrap().as_raw_fd(),
                worker.output.as_raw_fd(),
            ] {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                ensure!(
                    flags >= 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                    "cannot configure node readiness pipe"
                );
            }
            ensure!(
                worker.frame(Instant::now() + Duration::from_secs(10))?
                    == json!({"version":1,"node":true}),
                "native RAFS node did not become ready"
            );
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            ensure!(fd >= 0, "cannot supervise native RAFS node");
            let endpoint = Arc::new(RafsEndpoint {
                socket,
                alive: AtomicBool::new(true),
                generation: uuid::Uuid::new_v4().to_string(),
                client_limits,
                store: options.store,
                metadata: std::sync::Mutex::new(metadata::Cache::new()),
                private_roots: vec![
                    cache.clone(),
                    directory.path().into(),
                    options.reader,
                    path.canonicalize()?,
                ],
            });
            Ok(Self {
                endpoint,
                worker: Some(worker),
                pidfd: unsafe { File::from_raw_fd(fd) },
                _directory: directory,
                cache,
                _lock: lock,
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn cleanup_previous(cache: &Path) -> anyhow::Result<()> {
    let path = cache.join("control.json");
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    identity(&file)?;
    ensure!(
        file.metadata()?.len() < 4096,
        "invalid node control receipt"
    );
    let control: Control = serde_json::from_reader(file.take(4096))?;
    ensure!(
        control.path.parent() == Some(Path::new("/tmp"))
            && control
                .path
                .file_name()
                .is_some_and(|n| n.as_encoded_bytes().starts_with(b"pvisor-rafs-node-")),
        "unsafe node recovery path"
    );
    match fs::symlink_metadata(&control.path) {
        Ok(m) => {
            private(&control.path)?;
            ensure!(
                (m.dev(), m.ino()) == (control.device, control.inode),
                "stale node control ownership changed"
            );
            fs::remove_dir_all(&control.path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    fs::remove_file(path)?;
    Ok(())
}
