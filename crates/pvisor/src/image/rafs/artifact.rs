//! Exact local artifact identity and template-owned durable commit reference.
use super::*;
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::PathBuf};

/// Fixed image dependency, without node credentials or a mutable alias.
/// The physical commit identity prevents replacing a publication at the same
/// content-addressed path. A template pins this inode before publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RafsArtifact {
    /// Canonical private node metadata store.
    pub store: PathBuf,
    /// Exact merged environment handle, binding ordered components and artifacts.
    pub handle: String,
    /// Device of the authenticated MERGED.json inode.
    pub device: u64,
    /// Inode of the authenticated MERGED.json file.
    pub inode: u64,
}

impl RafsArtifact {
    fn path(&self) -> io::Result<PathBuf> {
        Ok(self
            .store
            .join("merged")
            .join(revision(&self.handle)?)
            .join("MERGED.json"))
    }

    pub(crate) fn open(&self) -> io::Result<File> {
        private(&self.store)?;
        if self.store.canonicalize()? != self.store {
            return Err(invalid("noncanonical RAFS artifact store"));
        }
        let path = self.path()?;
        private(&self.store.join("merged"))?;
        private(path.parent().unwrap())?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if identity(&file)? != (self.device, self.inode) || file.metadata()?.len() > MAX as u64 {
            return Err(invalid("RAFS template artifact identity changed"));
        }
        fs2::FileExt::try_lock_shared(&file)?;
        let mut bytes = Vec::new();
        (&file).take(MAX as u64 + 1).read_to_end(&mut bytes)?;
        use sha2::{Digest, Sha256};
        if crate::util::encode_hex(&Sha256::digest(bytes)) != revision(&self.handle)? {
            return Err(invalid("RAFS template artifact digest changed"));
        }
        Ok(file)
    }

    pub(crate) fn pin(&self, destination: &Path) -> io::Result<File> {
        let file = self.open()?;
        // EXDEV is explicit: never substitute a copy, which would not protect
        // the canonical artifact from the node's link-count based reclamation.
        fs::hard_link(self.path()?, destination)?;
        let check = (|| {
            let pin = self.verify_pin(destination)?;
            pin.sync_all()?;
            File::open(destination.parent().unwrap())?.sync_all()?;
            Ok(pin)
        })();
        if check.is_err() {
            let _ = fs::remove_file(destination);
        }
        drop(file);
        check
    }

    pub(crate) fn verify_pin(&self, path: &Path) -> io::Result<File> {
        let _canonical = self.open()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if identity(&file)? != (self.device, self.inode) || file.metadata()?.nlink() < 2 {
            return Err(invalid("RAFS template dependency pin changed"));
        }
        fs2::FileExt::try_lock_shared(&file)?;
        Ok(file)
    }
}
