//! Resident backing admission and work-buffer leases, shared by all node images.
use super::*;
use nydus_storage::{cache::BlobCache, device::BlobInfo, factory::BLOB_FACTORY};
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub(crate) const CONTROL_BYTES: u64 = 4 * 1024 * 1024;
type Cell = Arc<OnceLock<std::result::Result<Arc<dyn BlobCache>, String>>>;
#[derive(Debug)]
pub(crate) struct MetadataFull;
impl std::fmt::Display for MetadataFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("node metadata budget exhausted")
    }
}
impl std::error::Error for MetadataFull {}
pub(crate) fn is_metadata_full(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.is::<MetadataFull>()
            || e.downcast_ref::<io::Error>()
                .and_then(|e| e.get_ref())
                .is_some_and(|e| e.is::<MetadataFull>())
    })
}
fn metadata_full() -> anyhow::Error {
    io::Error::new(io::ErrorKind::WouldBlock, MetadataFull).into()
}

#[derive(PartialEq, Eq)]
struct Shape {
    numbers: [u64; 8],
    toc: [u8; 32],
}
struct Blob {
    refs: usize,
    bytes: u64,
    shape: Shape,
    cell: Cell,
    retired: Option<Weak<dyn BlobCache>>,
}
#[derive(Default)]
struct State {
    metadata: u64,
    metadata_peak: u64,
    io: u64,
    io_peak: u64,
    io_active: u64,
    io_active_peak: u64,
    metadata_rejections: u64,
    io_rejections: u64,
    blobs: BTreeMap<String, Blob>,
}
pub(crate) struct Resources {
    max_metadata: u64,
    max_io: u64,
    state: Mutex<State>,
}
impl Resources {
    pub(crate) fn new(max_metadata: u64, max_io: u64) -> Arc<Self> {
        Arc::new(Self {
            max_metadata,
            max_io,
            state: Mutex::default(),
        })
    }
    pub(crate) fn collect(&self) {
        // Factory pins protect concurrent cache construction. Keep the charge
        // until the actual cache object disappears, even if GC must defer it.
        BLOB_FACTORY.gc(None);
        let mut state = self.state.lock().unwrap();
        let mut freed = 0;
        state.blobs.retain(|_, entry| {
            if entry.refs == 0 && entry.retired.as_ref().is_none_or(|v| v.upgrade().is_none()) {
                freed += entry.bytes;
                false
            } else {
                true
            }
        });
        state.metadata -= freed;
    }
    pub(crate) fn protected_blobs(&self) -> std::collections::BTreeSet<String> {
        self.collect();
        self.state.lock().unwrap().blobs.keys().cloned().collect()
    }
    pub(crate) fn image(self: &Arc<Self>, bytes: u64) -> Result<ImageLease> {
        self.collect();
        let mut state = self.state.lock().unwrap();
        if bytes > self.max_metadata.saturating_sub(state.metadata) {
            state.metadata_rejections += 1;
            return Err(metadata_full());
        }
        state.metadata += bytes;
        state.metadata_peak = state.metadata_peak.max(state.metadata);
        Ok(ImageLease {
            owner: self.clone(),
            bytes,
            blobs: Vec::new(),
        })
    }
    pub(crate) fn io(self: &Arc<Self>) -> Result<IoLease> {
        let mut lease = IoLease {
            owner: self.clone(),
            bytes: 0,
        };
        lease.grow(CONTROL_BYTES)?;
        let mut state = self.state.lock().unwrap();
        state.io_active += 1;
        state.io_active_peak = state.io_active_peak.max(state.io_active);
        Ok(lease)
    }
    pub(crate) fn snapshot(&self) -> Value {
        self.collect();
        let state = self.state.lock().unwrap();
        let blob_bytes: u64 = state.blobs.values().map(|v| v.bytes).sum();
        json!({"image_bytes":state.metadata-blob_bytes,"blob_metadata_bytes":blob_bytes,"blob_references":state.blobs.values().map(|v|v.refs).sum::<usize>(),
            "metadata_bytes":state.metadata,"metadata_peak_bytes":state.metadata_peak,
            "max_metadata_bytes":self.max_metadata,"io_bytes":state.io,"io_peak_bytes":state.io_peak,
            "max_io_bytes":self.max_io,"io_active":state.io_active,"io_active_peak":state.io_active_peak,
            "metadata_rejections":state.metadata_rejections,"io_rejections":state.io_rejections,
            "shared_blobs":state.blobs.len(),"referenced_blobs":state.blobs.values().filter(|v|v.refs>0).count()})
    }
}
pub(crate) struct ImageLease {
    owner: Arc<Resources>,
    bytes: u64,
    blobs: Vec<String>,
}
impl ImageLease {
    pub(crate) fn blobs(&mut self, infos: &[Arc<BlobInfo>]) -> Result<()> {
        let mut additions = BTreeMap::new();
        for info in infos {
            ensure!(
                info.meta_ci_is_valid(),
                "node requires indexed blob metadata"
            );
            let numbers = [
                info.meta_ci_offset(),
                info.meta_ci_compressed_size(),
                info.meta_ci_uncompressed_size(),
                info.compressed_size(),
                info.uncompressed_size(),
                u64::from(info.chunk_size()),
                u64::from(info.chunk_count()),
                u64::from(info.blob_toc_size()),
            ];
            let bytes = info
                .meta_ci_compressed_size()
                .checked_add(info.meta_ci_uncompressed_size())
                .and_then(|v| v.checked_add(u64::from(info.blob_toc_size())))
                .and_then(|v| v.checked_mul(2))
                .and_then(|v| v.checked_add(u64::from(info.chunk_count()) * 128 + 65536))
                .context("node metadata charge overflow")?;
            let shape = Shape {
                numbers,
                toc: *info.blob_toc_digest(),
            };
            ensure!(
                additions.insert(info.blob_id(), (bytes, shape)).is_none(),
                "duplicate blob in node image"
            );
        }
        self.owner.collect();
        let mut state = self.owner.state.lock().unwrap();
        let mut extra = 0u64;
        for (id, (bytes, shape)) in &additions {
            if let Some(entry) = state.blobs.get(id) {
                ensure!(
                    entry.shape == *shape && entry.bytes == *bytes,
                    "shared metadata identity collision"
                );
            } else {
                extra = extra
                    .checked_add(*bytes)
                    .context("node metadata charge overflow")?;
            }
        }
        if extra > self.owner.max_metadata.saturating_sub(state.metadata) {
            state.metadata_rejections += 1;
            return Err(metadata_full());
        }
        state.metadata += extra;
        state.metadata_peak = state.metadata_peak.max(state.metadata);
        for (id, (bytes, shape)) in additions {
            state
                .blobs
                .entry(id.clone())
                .or_insert_with(|| Blob {
                    refs: 0,
                    bytes,
                    shape,
                    cell: Arc::new(OnceLock::new()),
                    retired: None,
                })
                .refs += 1;
            self.blobs.push(id);
        }
        Ok(())
    }
    pub(crate) fn cache(
        &self,
        info: &Arc<BlobInfo>,
        config: &Arc<ConfigV2>,
    ) -> Result<Arc<dyn BlobCache>> {
        let cell = {
            self.owner
                .state
                .lock()
                .unwrap()
                .blobs
                .get(&info.blob_id())
                .context("unreserved node blob")?
                .cell
                .clone()
        };
        match cell.get_or_init(|| {
            let result = (|| -> Result<_> {
                let cache = BLOB_FACTORY.new_blob_cache(config, info, "/")?;
                // Keep the actual factory owner observable even if subsequent
                // metadata authentication fails and the cell stores an error.
                self.owner
                    .state
                    .lock()
                    .unwrap()
                    .blobs
                    .get_mut(&info.blob_id())
                    .context("unreserved native cache")?
                    .retired = Some(Arc::downgrade(&cache));
                ensure!(
                    cache.get_blob_meta_info()?.is_some(),
                    "node chunk metadata unavailable"
                );
                Ok(cache)
            })();
            result.map_err(|e| format!("{e:#}"))
        }) {
            Ok(cache) => Ok(cache.clone()),
            Err(error) => anyhow::bail!("node shared metadata open failed: {error}"),
        }
    }
}
impl Drop for ImageLease {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap();
        state.metadata -= self.bytes;
        let mut retired = Vec::new();
        for id in &self.blobs {
            let entry = state.blobs.get_mut(id).unwrap();
            entry.refs -= 1;
            if entry.refs == 0 {
                let cell = std::mem::replace(&mut entry.cell, Arc::new(OnceLock::new()));
                if let Some(Ok(cache)) = cell.get() {
                    entry.retired = Some(Arc::downgrade(cache));
                }
                retired.push(cell);
            }
        }
        drop(state);
        drop(retired);
        self.owner.collect();
    }
}
pub(crate) struct IoLease {
    owner: Arc<Resources>,
    bytes: u64,
}
impl IoLease {
    pub(crate) fn grow(&mut self, bytes: u64) -> Result<()> {
        let mut state = self.owner.state.lock().unwrap();
        if bytes > self.owner.max_io.saturating_sub(state.io) {
            state.io_rejections += 1;
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "node work buffer budget exhausted",
            )
            .into());
        }
        state.io += bytes;
        state.io_peak = state.io_peak.max(state.io);
        self.bytes += bytes;
        Ok(())
    }
}
impl Drop for IoLease {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap();
        state.io -= self.bytes;
        if self.bytes >= CONTROL_BYTES {
            state.io_active -= 1;
        }
    }
}
