//! Node-wide immutable metadata hints. No file data or per-VM cache is retained.
use super::*;
use std::collections::VecDeque;

const ENTRIES: usize = 1024;
const BYTES: usize = 4 * 1024 * 1024;
struct Entry {
    path: String,
    frame: Box<[u8]>,
}
impl Entry {
    fn charge(&self) -> usize {
        self.path.len() + self.frame.len() + 64
    }
}
pub(super) struct Cache {
    image: Option<(String, File)>,
    entries: VecDeque<Entry>,
    used: usize,
    pub(super) epoch: u64,
    hits: u64,
    misses: u64,
}
impl Cache {
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn new() -> Self {
        Self {
            image: None,
            entries: VecDeque::with_capacity(ENTRIES),
            used: 0,
            epoch: 0,
            hits: 0,
            misses: 0,
        }
    }
    fn base(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + std::mem::size_of::<Option<(String, File)>>()
            + 128
    }
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.image = None;
        self.used = 0;
        self.epoch = self.epoch.wrapping_add(1);
    }
    pub(super) fn pin(&mut self, handle: &str, lease: File) -> io::Result<()> {
        if let Some((key, existing)) = &self.image {
            if key == handle && identity(existing)? == identity(&lease)? {
                return Ok(());
            }
            self.clear();
        }
        self.image = Some((handle.into(), lease));
        Ok(())
    }
    pub(super) fn select(&mut self, handle: &str, physical: (u64, u64)) -> io::Result<()> {
        if let Some((key, lease)) = &self.image {
            // Release idle metadata before native admission/automatic trimming.
            if key != handle || identity(lease)? != physical {
                self.clear();
            }
        }
        // Fence older opens too, including two concurrent opens of an empty cache.
        self.epoch = self.epoch.wrapping_add(1);
        Ok(())
    }
    pub(super) fn admit(&mut self, handle: &str) {
        // Read/list/stat/retain can load an image without an explicit Open.
        // Fence pending opens too, so they cannot repin an older image during admission.
        if !self.image.as_ref().is_some_and(|(key, _)| key == handle) {
            self.clear();
        }
    }
    pub(super) fn get(&mut self, handle: &str, path: &str) -> Option<Value> {
        let Some(index) = self.entries.iter().position(|v| {
            self.image.as_ref().is_some_and(|(key, _)| key == handle) && v.path == path
        }) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        let entry = self.entries.remove(index).unwrap();
        let result = serde_json::from_slice(&entry.frame).ok();
        self.entries.push_back(entry);
        result
    }
    pub(super) fn insert(&mut self, handle: &str, path: &str, frame: &Value, epoch: u64) {
        if epoch != self.epoch || !self.image.as_ref().is_some_and(|(key, _)| key == handle) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(frame) else {
            return;
        };
        if bytes.len() > 65536 {
            return;
        }
        let entry = Entry {
            path: path.into(),
            frame: bytes.into_boxed_slice(),
        };
        if let Some(index) = self.entries.iter().position(|v| v.path == path) {
            self.used -= self.entries.remove(index).unwrap().charge();
        }
        while self.entries.len() >= ENTRIES || self.base() + self.used + entry.charge() > BYTES {
            let Some(old) = self.entries.pop_front() else {
                return;
            };
            self.used -= old.charge();
        }
        self.used += entry.charge();
        self.entries.push_back(entry);
    }
    pub(super) fn snapshot(&self) -> Value {
        json!({"entries":self.entries.len(),"bytes":self.base()+self.used,
            "max_entries":ENTRIES,"max_bytes":BYTES,"image_leases":usize::from(self.image.is_some()),
            "hits":self.hits,"misses":self.misses})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_are_bounded_and_invalidated_without_retaining_inflight_results() {
        let file = tempfile::tempfile().unwrap();
        let mut cache = Cache::new();
        cache.pin("image", file).unwrap();
        let epoch = cache.epoch;
        cache.select("other", (0, 0)).unwrap();
        assert_ne!(cache.epoch, epoch);
        assert!(cache.image.is_none());
        cache.pin("image", tempfile::tempfile().unwrap()).unwrap();
        let epoch = cache.epoch;
        for n in 0..2000 {
            cache.insert("image", &n.to_string(), &json!({"n":n}), epoch);
        }
        assert_eq!(cache.entries.len(), ENTRIES);
        assert!(cache.base() + cache.used <= BYTES);
        assert!(cache.get("image", "0").is_none());
        assert_eq!(cache.get("image", "1999"), Some(json!({"n":1999})));
        cache.clear();
        cache.pin("image", tempfile::tempfile().unwrap()).unwrap();
        cache.insert("image", "old", &json!({}), epoch);
        assert!(cache.get("image", "old").is_none());
        let epoch = cache.epoch;
        for n in 0..200 {
            cache.insert(
                "image",
                &n.to_string(),
                &json!({"s":"x".repeat(60000)}),
                epoch,
            );
        }
        assert!(cache.entries.len() < ENTRIES);
        assert!(cache.base() + cache.used <= BYTES);
        cache.pin("other", tempfile::tempfile().unwrap()).unwrap();
        assert!(cache.get("image", "199").is_none());
        assert!(cache.image.is_some());
    }
}
