//! Node-owned native state. Included only by the pinned RAFS reader example.
use super::*;
#[path = "budget.rs"]
pub(crate) mod budget;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "reclaim.rs"]
mod reclaim;
use nydus_storage::device::BlobInfo;
use std::{
    collections::BTreeMap,
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
    time::Duration,
};

fn metadata_bytes() -> u64 {
    64 * 1024 * 1024
}
fn io_bytes() -> u64 {
    64 * 1024 * 1024
}
fn downloads() -> usize {
    4
}
fn address_space() -> u64 {
    1024 * 1024 * 1024
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    store: PathBuf,
    cache: PathBuf,
    origin: String,
    max_images: usize,
    max_cache_bytes: u64,
    workers: usize,
    #[serde(default = "metadata_bytes")]
    max_metadata_bytes: u64,
    #[serde(default = "io_bytes")]
    max_io_bytes: u64,
    #[serde(default = "downloads")]
    max_downloads: usize,
    #[serde(default = "address_space")]
    max_address_space_bytes: u64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Reservation {
    blob: Blob,
    decoded_bytes: u64,
    chunks: u32,
    charged_bytes: u64,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    origin: String,
    blobs: BTreeMap<String, Reservation>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    deleting: std::collections::BTreeSet<String>,
}

pub(super) struct Shared {
    pub(super) config: Arc<ConfigV2>,
    pub(super) resources: Arc<budget::Resources>,
    cache: PathBuf,
    store: PathBuf,
    max_bytes: u64,
    ledger: Mutex<Ledger>,
    _lock: File,
}
impl Shared {
    fn open(options: &Config) -> Result<Self> {
        private_directory(&options.cache)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(options.cache.join("node.lock"))?;
        ensure!(lock.metadata()?.is_file(), "invalid node cache lock");
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "node cache already owned"
        );
        let ledger_path = options.cache.join("registry.json");
        let ledger = match regular(&ledger_path) {
            Ok(_) => serde_json::from_slice::<Ledger>(&control(&ledger_path)?)?,
            Err(e)
                if e.downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                ensure!(
                    fs::read_dir(&options.cache)?
                        .all(|e| e.is_ok_and(|e| e.file_name() == "node.lock")),
                    "unregistered node cache contents"
                );
                Ledger {
                    origin: options.origin.clone(),
                    blobs: BTreeMap::new(),
                    deleting: Default::default(),
                }
            }
            Err(e) => return Err(e),
        };
        ensure!(
            ledger.origin == options.origin && ledger.blobs.len() <= 1024,
            "node cache origin or registry mismatch"
        );
        let used = ledger.blobs.values().try_fold(0u64, |n, v| {
            n.checked_add(v.charged_bytes)
                .context("node cache size overflow")
        })?;
        ensure!(
            used <= options.max_cache_bytes,
            "existing node cache exceeds capacity"
        );
        // One identical config shares Nydus's process-global BlobFactory/cache manager.
        // Direct remote HTTP uses bounded reader buffers; the UDS HTTP backend's
        // unbounded body collector is deliberately not used.
        let config = Arc::new(serde_json::from_value::<ConfigV2>(json!({
            "version":2,
            "backend":{"type":"http-proxy","http-proxy":{"addr":options.origin,"path":"/","skip_verify":false,"timeout":5,"connect_timeout":3,"retry_limit":0,"max_concurrent_requests":options.max_downloads}},
            "cache":{"type":"blobcache","validate":true,"prefetch":{"enable":false},"filecache":{"work_dir":options.cache,"sync_metadata":true,"sync_data":true}},
            "rafs":{"batch_size":0,"mode":"direct","validate":false,"enable_xattr":true,"prefetch":{"enable":false}}
        }))?);
        // Compile-time and runtime checks prevent an unpatched config parser
        // from silently ignoring the admission controls.
        ensure!(
            config
                .get_cache_config()?
                .get_filecache_config()?
                .sync_metadata
                && config.get_cache_config()?.get_filecache_config()?.sync_data,
            "node requires synchronous metadata and data persistence support"
        );
        ensure!(
            config
                .get_backend_config()?
                .get_http_proxy_config()?
                .max_concurrent_requests
                == options.max_downloads,
            "node HTTP admission not enabled"
        );
        let shared = Self {
            config,
            resources: budget::Resources::new(options.max_metadata_bytes, options.max_io_bytes),
            cache: options.cache.clone(),
            store: options.store.clone(),
            max_bytes: options.max_cache_bytes,
            ledger: Mutex::new(ledger),
            _lock: lock,
        };
        {
            let _gate = lifecycle::gate(&options.store, true)?;
            lifecycle::recover(&options.store)?;
            shared.recover_cache(&mut shared.ledger.lock().unwrap())?;
        }
        shared.save(&shared.ledger.lock().unwrap())?;
        Ok(shared)
    }
    fn save(&self, ledger: &Ledger) -> Result<()> {
        let path = self.cache.join("registry.new");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let bytes = serde_json::to_vec(ledger)?;
        ensure!(
            bytes.len() <= MAX_FRAME,
            "node cache registry exceeds limit"
        );
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(path, self.cache.join("registry.json"))?;
        File::open(&self.cache)?.sync_all()?;
        Ok(())
    }
    pub(super) fn reserve(
        &self,
        descriptions: &BTreeMap<String, Blob>,
        blobs: &[Arc<BlobInfo>],
    ) -> Result<()> {
        let mut ledger = self.ledger.lock().unwrap();
        self.recover_cache(&mut ledger)?;
        let mut additions = Vec::new();
        let mut used = ledger.blobs.values().try_fold(0u64, |n, v| {
            n.checked_add(v.charged_bytes)
                .context("node cache size overflow")
        })?;
        for info in blobs {
            let id = info.blob_id();
            let blob = descriptions.get(&id).context("unbound node blob")?.clone();
            ensure!(blob.toc_size <= 16 * 1024 * 1024, "node ToC exceeds limit");
            let decoded_bytes = info.uncompressed_size();
            let chunks = info.chunk_count();
            let charged_bytes = decoded_bytes
                .checked_add(u64::from(chunks) * 512 + 65536)
                .and_then(|v| v.checked_add(blob.toc_size.checked_mul(2)?))
                .context("node backing size overflow")?;
            let entry = Reservation {
                blob,
                decoded_bytes,
                chunks,
                charged_bytes,
            };
            if let Some(existing) = ledger.blobs.get(&id) {
                ensure!(existing == &entry, "shared blob identity collision");
            } else {
                used = used
                    .checked_add(charged_bytes)
                    .context("node cache size overflow")?;

                additions.push((id, entry));
            }
        }
        let extra = additions.iter().map(|(_, v)| v.charged_bytes).sum::<u64>();
        if used > self.max_bytes || ledger.blobs.len() + additions.len() > 1024 {
            if extra > self.max_bytes {
                return Err(reclaim::full());
            }
            self.trim_locked(&mut ledger, self.max_bytes - extra)?;
            used = ledger.blobs.values().map(|v| v.charged_bytes).sum::<u64>() + extra;
            if used > self.max_bytes || ledger.blobs.len() + additions.len() > 1024 {
                return Err(reclaim::full());
            }
        }
        for (id, entry) in additions {
            ledger.blobs.insert(id, entry);
        }
        // A failed subsequent read retains its reservation: any partial native
        // backing remains accounted. Reclamation is explicit, not silent overcommit.
        self.save(&ledger)
    }
}

struct Opened {
    image: Image,
}
type Cell = Arc<OnceLock<std::result::Result<Arc<Opened>, (i32, String)>>>;
struct Slot {
    cell: Cell,
    touched: u64,
}
struct Images {
    slots: BTreeMap<String, Slot>,
    tick: u64,
}
struct Node {
    options: Config,
    shared: Shared,
    images: Mutex<Images>,
}
impl Node {
    fn get(&self, handle: &str) -> Result<Arc<Opened>> {
        ensure!(
            handle.strip_prefix(PREFIX).is_some_and(hex_digest),
            "invalid node handle"
        );
        let cell = {
            let mut images = self.images.lock().unwrap();
            images.tick = images.tick.wrapping_add(1);
            let tick = images.tick;
            if !images.slots.contains_key(handle) {
                if images.slots.len() == self.options.max_images {
                    let victim = images
                        .slots
                        .iter()
                        .filter(|(_, slot)| {
                            slot.cell.get().is_some_and(|v| match v {
                                Ok(image) => Arc::strong_count(image) == 1,
                                Err(_) => true,
                            })
                        })
                        .min_by_key(|(_, slot)| slot.touched)
                        .map(|(key, _)| key.clone());
                    let key = victim.ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
                    images.slots.remove(&key);
                    nydus_storage::factory::BLOB_FACTORY.gc(None);
                }
                images.slots.insert(
                    handle.into(),
                    Slot {
                        cell: Arc::new(OnceLock::new()),
                        touched: tick,
                    },
                );
            }
            let slot = images.slots.get_mut(handle).unwrap();
            slot.touched = tick;
            slot.cell.clone()
        };
        let result = cell.get_or_init(|| {
            self.load(handle)
                .map(Arc::new)
                .map_err(|e| (error_code(&e), format!("{e:#}")))
        });
        match result {
            Ok(image) => Ok(image.clone()),
            Err(error) => {
                let mut images = self.images.lock().unwrap();
                if images
                    .slots
                    .get(handle)
                    .is_some_and(|slot| Arc::ptr_eq(&slot.cell, &cell))
                {
                    images.slots.remove(handle);
                }
                drop(images);
                nydus_storage::factory::BLOB_FACTORY.gc(None);
                Err(anyhow::Error::from(io::Error::from_raw_os_error(error.0))
                    .context(format!("node image open failed: {}", error.1)))
            }
        }
    }
    fn evict_idle(&self, keep: &str) -> bool {
        let mut images = self.images.lock().unwrap();
        let victim = images
            .slots
            .iter()
            .filter(|(key, slot)| {
                key.as_str() != keep
                    && slot
                        .cell
                        .get()
                        .is_some_and(|v| v.as_ref().is_ok_and(|v| Arc::strong_count(v) == 1))
            })
            .min_by_key(|(_, slot)| slot.touched)
            .map(|(key, _)| key.clone());
        let Some(key) = victim else {
            return false;
        };
        images.slots.remove(&key);
        true
    }
    fn load(&self, handle: &str) -> Result<Opened> {
        for _ in 0..=self.options.max_images {
            match self.load_once(handle) {
                Err(error)
                    if (budget::is_metadata_full(&error) || reclaim::is_full(&error))
                        && self.evict_idle(handle) =>
                {
                    continue
                }
                result => return result,
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, "node metadata budget exhausted").into())
    }
    fn load_once(&self, handle: &str) -> Result<Opened> {
        let bundle = self
            .options
            .store
            .join("merged")
            .join(&handle[PREFIX.len()..]);
        let image = Image::open_with(&bundle, handle, &self.options.cache, Some(&self.shared))?;
        Ok(Opened { image })
    }
    fn request(&self, call: Value, io: Option<&mut budget::IoLease>) -> Result<(Value, Vec<u8>)> {
        let handle = call
            .get("handle")
            .and_then(Value::as_str)
            .context("missing handle")?;
        let request = call.get("request").context("missing request")?;
        ensure!(
            call.as_object().is_some_and(|m| m.len() == 2),
            "unexpected node fields"
        );
        ensure!(
            handle.strip_prefix(PREFIX).is_some_and(hex_digest),
            "invalid node handle"
        );
        match request.get("op").and_then(Value::as_str) {
            Some("lease") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Lease {
                    op: String,
                    kind: String,
                    owner: String,
                    action: String,
                }
                let r: Lease = serde_json::from_value(request.clone())?;
                ensure!(r.op == "lease", "invalid lease operation");
                let image = if r.action == "retain" {
                    Some(self.get(handle)?)
                } else {
                    None
                };
                return Ok((
                    lifecycle::pin(
                        &self.options.store,
                        image.as_ref().map(|v| &v.image),
                        handle,
                        &r.kind,
                        &r.owner,
                        &r.action,
                    )?,
                    vec![],
                ));
            }
            Some("trim" | "remove") => {
                #[derive(Deserialize)]
                #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
                enum Change {
                    Trim { target_bytes: u64 },
                    Remove {},
                }
                let change: Change = serde_json::from_value(request.clone())?;
                while self.evict_idle("") {}
                self.shared.resources.collect();
                let result = match change {
                    Change::Trim { target_bytes } => self
                        .shared
                        .trim_locked(&mut self.shared.ledger.lock().unwrap(), target_bytes)?,
                    Change::Remove {} => lifecycle::remove(&self.options.store, handle)?,
                };
                return Ok((result, vec![]));
            }
            _ => {}
        }
        if request.get("op").and_then(Value::as_str) == Some("stats") {
            ensure!(
                request.as_object().is_some_and(|m| m.len() == 1),
                "unexpected statistics fields"
            );
            let images = self.images.lock().unwrap();
            let ledger = self.shared.ledger.lock().unwrap();
            let status = fs::read_to_string("/proc/self/status")?;
            let mut process = BTreeMap::new();
            for (name, key) in [
                ("VmSize:", "virtual_bytes"),
                ("VmRSS:", "rss_bytes"),
                ("RssAnon:", "anonymous_bytes"),
                ("RssFile:", "file_bytes"),
            ] {
                let value = status
                    .lines()
                    .find(|v| v.starts_with(name))
                    .and_then(|v| v.split_whitespace().nth(1))
                    .context("missing process memory counter")?
                    .parse::<u64>()?;
                process.insert(key, value * 1024);
            }
            return Ok((
                json!({"status":"ok","stats":{"version":1,"resources":self.shared.resources.snapshot(),
                "resident_images":images.slots.values().filter(|v|v.cell.get().is_some_and(Result::is_ok)).count(),
                "max_images":self.options.max_images,"workers":self.options.workers,"max_downloads":self.options.max_downloads,
                "max_address_space_bytes":self.options.max_address_space_bytes,"disk_reserved_bytes":ledger.blobs.values().map(|v|v.charged_bytes).sum::<u64>(),
                "max_cache_bytes":self.options.max_cache_bytes,"process":process}}),
                Vec::new(),
            ));
        }
        if request.get("op").and_then(Value::as_str) == Some("open") {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Open {
                op: String,
                commit_device: u64,
                commit_inode: u64,
            }
            let selected: Open = serde_json::from_value(request.clone())?;
            let commit = regular(
                &self
                    .options
                    .store
                    .join("merged")
                    .join(&handle[PREFIX.len()..])
                    .join("MERGED.json"),
            )?;
            let metadata = commit.metadata()?;
            ensure!(
                selected.op == "open"
                    && metadata.dev() == selected.commit_device
                    && metadata.ino() == selected.commit_inode,
                "node metadata ownership mismatch"
            );
            let opened = self.get(handle)?;
            let loaded = opened.image._commit.metadata()?;
            ensure!(
                (loaded.dev(), loaded.ino()) == (metadata.dev(), metadata.ino()),
                "node metadata changed during open"
            );
            Ok((json!({"status":"ok","handle":handle}), vec![]))
        } else {
            let opened = self.get(handle)?;
            execute(&opened.image, serde_json::from_value(request.clone())?, io)
        }
    }
}

fn connection(mut stream: UnixStream, node: &Node) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } == 0
            && credentials.uid == unsafe { libc::geteuid() },
        "node peer must have the same UID"
    );
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let count = u32::from_be_bytes(header) as usize;
    ensure!(count > 0 && count <= 32768, "invalid node frame");
    let mut bytes = vec![0; count];
    stream.read_exact(&mut bytes)?;
    let result = (|| -> Result<_> {
        let call: Value = serde_json::from_slice(&bytes)?;
        let stats = call["request"]["op"] == "stats";
        let lease = if stats {
            None
        } else {
            Some(node.shared.resources.io()?)
        };
        Ok((call, lease))
    })();
    match result {
        Ok((call, mut lease)) => {
            let result = node.request(call, lease.as_mut());
            // The quota covers the reply until the socket write completes,
            // including a blocked/slow consumer and an error response.
            write_result(&mut stream, result)
        }
        Err(error) => write_result(&mut stream, Err(error)),
    }
}

pub(super) fn serve(config_path: &Path, socket: &Path) -> Result<()> {
    let options: Config = serde_json::from_slice(&control(config_path)?)?;
    ensure!(
        (1..=16).contains(&options.workers)
            && (1..=16).contains(&options.max_images)
            && options.max_cache_bytes > 0
            && (4 * 1024 * 1024..=1024 * 1024 * 1024).contains(&options.max_metadata_bytes)
            && (budget::CONTROL_BYTES..=1024 * 1024 * 1024).contains(&options.max_io_bytes)
            && (1..=16).contains(&options.max_downloads)
            && (128 * 1024 * 1024..=1u64 << 40).contains(&options.max_address_space_bytes),
        "invalid node limits"
    );
    let mut inherited = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    ensure!(
        unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut inherited) } == 0,
        "cannot inspect node address space limit"
    );
    let cap = options
        .max_address_space_bytes
        .min(inherited.rlim_cur)
        .min(inherited.rlim_max);
    let limit = libc::rlimit {
        rlim_cur: cap,
        rlim_max: cap,
    };
    ensure!(
        unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } == 0,
        "cannot limit node address space"
    );
    private_directory(&options.store)?;
    private_directory(&options.store.join("merged"))?;
    private_directory(socket.parent().context("missing node socket parent")?)?;
    ensure!(
        options.origin.starts_with("https://")
            || options.origin == "http://127.0.0.1"
            || options.origin.starts_with("http://127.0.0.1/")
            || options.origin.starts_with("http://127.0.0.1:")
            || options.origin == "http://[::1]"
            || options.origin.starts_with("http://[::1]/")
            || options.origin.starts_with("http://[::1]:"),
        "origin requires HTTPS or explicit loopback HTTP"
    );
    ensure!(
        !options.origin.ends_with('/') && !options.origin.contains(['?', '#', '@']),
        "invalid node origin"
    );
    ensure!(
        !options.cache.starts_with(&options.store) && !options.store.starts_with(&options.cache),
        "node metadata/cache overlap"
    );
    let shared = Shared::open(&options)?;
    let node = Arc::new(Node {
        options,
        shared,
        images: Mutex::new(Images {
            slots: BTreeMap::new(),
            tick: 0,
        }),
    });
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let (send, receive) = mpsc::sync_channel::<UnixStream>(16);
    let receive = Arc::new(Mutex::new(receive));
    for _ in 0..node.options.workers {
        let receive = receive.clone();
        let node = node.clone();
        std::thread::spawn(move || loop {
            let Ok(stream) = receive.lock().unwrap().recv() else {
                break;
            };
            if let Err(error) = connection(stream, &node) {
                eprintln!("RAFS node request: {error:#}");
            }
        });
    }
    frame(&mut io::stdout(), &json!({"version":1,"node":true}))?;
    io::stdout().flush()?;
    for stream in listener.incoming() {
        match send.try_send(stream?) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(mut stream)) => {
                stream.set_write_timeout(Some(Duration::from_millis(100)))?;
                let _ = write_result(
                    &mut stream,
                    Err(io::Error::from(io::ErrorKind::WouldBlock).into()),
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => anyhow::bail!("node workers stopped"),
        }
    }
    Ok(())
}
