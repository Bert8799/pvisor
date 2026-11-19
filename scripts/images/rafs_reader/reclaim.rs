//! Whole-blob cache eviction. Intent is durable before unlink; credit follows sync.
use super::*;

#[derive(Debug)]
pub(super) struct CacheFull;
impl std::fmt::Display for CacheFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("node cache capacity exceeded")
    }
}
impl std::error::Error for CacheFull {}
pub(super) fn is_full(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.is::<CacheFull>()
            || e.downcast_ref::<io::Error>()
                .and_then(|v| v.get_ref())
                .is_some_and(|e| e.is::<CacheFull>())
    })
}
pub(super) fn full() -> anyhow::Error {
    io::Error::new(io::ErrorKind::WouldBlock, CacheFull).into()
}
const SUFFIXES: [&str; 6] = [
    ".blob.data",
    ".blob.meta",
    ".blob.digest",
    ".blob.data.chunk_map",
    ".blob.toc",
    ".blob.toc_downloading",
];
impl Shared {
    pub(super) fn validate_cache(&self, ledger: &Ledger) -> Result<()> {
        ensure!(
            ledger
                .deleting
                .is_subset(&ledger.blobs.keys().cloned().collect()),
            "unbound eviction intent"
        );
        for (id, entry) in &ledger.blobs {
            ensure!(hex_digest(id), "invalid cached blob identity");
            let charge = entry
                .decoded_bytes
                .checked_add(u64::from(entry.chunks) * 512 + 65536)
                .and_then(|v| v.checked_add(entry.blob.toc_size.checked_mul(2)?))
                .context("invalid reservation")?;
            ensure!(
                charge == entry.charged_bytes,
                "invalid disk reservation charge"
            );
        }
        for path in fs::read_dir(&self.cache)? {
            let path = path?.path();
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .context("invalid cache object")?;
            let blob_object = !matches!(name, "node.lock" | "registry.json" | "registry.new");
            if blob_object {
                let id = SUFFIXES
                    .iter()
                    .find_map(|suffix| name.strip_suffix(suffix))
                    .context("unknown native cache object")?;
                ensure!(ledger.blobs.contains_key(id), "unreserved cache object");
            }
            let file = match regular(&path) {
                Ok(file) => file,
                Err(error)
                    if blob_object
                        && error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
            let info = file.metadata()?;
            // Authenticated metadata downloads rename their temporary objects.
            // Concurrent validation may observe a vanished name or unlinked FD.
            ensure!(
                info.uid() == unsafe { libc::geteuid() }
                    && (info.nlink() == 1 || blob_object && info.nlink() == 0),
                "foreign or linked cache object"
            );
        }
        Ok(())
    }
    fn erase_blob(&self, id: &str) -> Result<()> {
        // Validate every member before the first unlink. No recursive deletion.
        for suffix in SUFFIXES {
            let path = self.cache.join(format!("{id}{suffix}"));
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    let f = regular(&path)?;
                    ensure!(f.metadata()?.nlink() == 1, "linked cache backing");
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for suffix in SUFFIXES {
            match fs::remove_file(self.cache.join(format!("{id}{suffix}"))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        File::open(&self.cache)?.sync_all()?;
        Ok(())
    }
    pub(super) fn recover_cache(&self, ledger: &mut Ledger) -> Result<()> {
        self.validate_cache(ledger)?;
        for id in ledger.deleting.clone() {
            self.erase_blob(&id)?;
            ledger.blobs.remove(&id);
            ledger.deleting.remove(&id);
            self.save(ledger)?;
        }
        Ok(())
    }
    pub(super) fn trim_locked(&self, ledger: &mut Ledger, target: u64) -> Result<Value> {
        ensure!(
            target <= self.max_bytes,
            "trim target exceeds node capacity"
        );
        self.recover_cache(ledger)?;
        let _gate = lifecycle::gate(&self.store, false)?;
        let mut protected = lifecycle::protected(&self.store)?;
        protected.extend(self.resources.protected_blobs());
        let mut used = ledger.blobs.values().map(|v| v.charged_bytes).sum::<u64>();
        let before = used;
        let mut removed = 0usize;
        let candidates: Vec<_> = ledger
            .blobs
            .keys()
            .filter(|id| !protected.contains(*id))
            .cloned()
            .collect();
        for id in candidates {
            if used <= target {
                break;
            }
            // Persist the intent before touching files. A failed save leaves the
            // reservation in place; restart/retry completes the same operation.
            ledger.deleting.insert(id.clone());
            self.save(ledger)?;
            self.erase_blob(&id)?;
            used -= ledger.blobs.remove(&id).unwrap().charged_bytes;
            ledger.deleting.remove(&id);
            self.save(ledger)?;
            removed += 1;
        }
        Ok(
            json!({"status":"ok","cache_trim":{"target_bytes":target,"remaining_bytes":used,"reclaimed_bytes":before-used,"reclaimed_blobs":removed,"target_reached":used<=target}}),
        )
    }
}
