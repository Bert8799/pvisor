//! Process-local digests of a pinned executable, never caller-supplied identity.
use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, PartialEq, Eq, Debug)]
struct Identity {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Identity {
    fn of(file: &File) -> std::io::Result<Self> {
        let m = file.metadata()?;
        ensure_regular(&m)?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
        })
    }
}

fn ensure_regular(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::other("executable is not a regular file"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Digests {
    pub(crate) blake3: String,
    pub(crate) sha256: Option<String>,
}

struct Verified {
    identity: Identity,
    digests: Digests,
    // Pin this inode until the entry is replaced, including across path upgrades.
    _file: File,
}

pub(crate) fn hashes(file: File, sha256: bool) -> anyhow::Result<Digests> {
    // Only Linux's executing inode has the kernel's write exclusion. An
    // upgraded pathname may identify a writable replacement: always rehash it,
    // even if coarse ctime would otherwise make metadata look unchanged.
    #[cfg(target_os = "linux")]
    let cacheable = Identity::of(&file)? == Identity::of(&File::open("/proc/self/exe")?)?;
    #[cfg(not(target_os = "linux"))]
    let cacheable = false;
    if !cacheable {
        return verified_hashes(file, sha256, &mut None);
    }
    static CACHE: OnceLock<Mutex<Option<Verified>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("executable verification lock poisoned"))?;
    verified_hashes(file, sha256, &mut cache)
}

fn verified_hashes(
    mut file: File,
    sha256: bool,
    cache: &mut Option<Verified>,
) -> anyhow::Result<Digests> {
    let before = Identity::of(&file)?;
    if let Some(verified) = cache
        && verified.identity == before
        && (!sha256 || verified.digests.sha256.is_some())
    {
        return Ok(verified.digests.clone());
    }
    let mut blake3 = blake3::Hasher::new();
    let mut sha256 = sha256.then(Sha256::new);
    let mut bytes = [0; 64 * 1024];
    file.rewind()?;
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        blake3.update(&bytes[..count]);
        if let Some(hash) = &mut sha256 {
            hash.update(&bytes[..count]);
        }
    }
    ensure!(
        before == Identity::of(&file)?,
        "executable changed during content verification"
    );
    let digests = Digests {
        blake3: blake3.finalize().to_hex().to_string(),
        sha256: sha256.map(|hash| crate::util::encode_hex(&hash.finalize())),
    };
    *cache = Some(Verified {
        identity: before,
        digests: digests.clone(),
        _file: file,
    });
    Ok(digests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    #[test]
    fn executing_inode_is_write_excluded_and_reuses_verified_digests() {
        let error = File::options()
            .write(true)
            .open("/proc/self/exe")
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
        let expected = hashes(File::open("/proc/self/exe").unwrap(), true).unwrap();
        let mut file = File::open("/proc/self/exe").unwrap();
        file.seek(SeekFrom::Start(17)).unwrap();
        let mut observer = file.try_clone().unwrap();
        assert_eq!(hashes(file, true).unwrap(), expected);
        assert_eq!(observer.stream_position().unwrap(), 17);
    }

    #[test]
    fn writable_replacement_paths_are_never_metadata_cache_hits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replacement");
        std::fs::write(&path, b"before").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let first = hashes(File::open(&path).unwrap(), true).unwrap();
        std::fs::write(&path, b"after!").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        let changed = hashes(File::open(&path).unwrap(), true).unwrap();
        assert_ne!(first.blake3, changed.blake3);
        assert_ne!(first.sha256, changed.sha256);
    }

    #[test]
    fn both_digests_match_independent_hashes_and_cache_hits_do_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let bytes = vec![42; 130_001];
        std::fs::write(&path, &bytes).unwrap();
        let mut cache = None;
        let actual = verified_hashes(File::open(&path).unwrap(), true, &mut cache).unwrap();
        assert_eq!(actual.blake3, blake3::hash(&bytes).to_hex().as_str());
        assert_eq!(
            actual.sha256,
            Some(crate::util::encode_hex(&Sha256::digest(&bytes)))
        );
        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(17)).unwrap();
        let mut observer = file.try_clone().unwrap();
        assert_eq!(verified_hashes(file, true, &mut cache).unwrap(), actual);
        assert_eq!(observer.stream_position().unwrap(), 17);
    }

    #[test]
    fn adding_sha256_or_changing_identity_requires_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        std::fs::write(&path, b"before").unwrap();
        let mut cache = None;
        let first = verified_hashes(File::open(&path).unwrap(), false, &mut cache).unwrap();
        assert!(first.sha256.is_none());
        let both = verified_hashes(File::open(&path).unwrap(), true, &mut cache).unwrap();
        assert_eq!(both.blake3, first.blake3);
        assert!(both.sha256.is_some());
        std::fs::write(&path, b"after!").unwrap();
        // Force an observable identity change without depending on kernel ctime
        // granularity; the existing CLI test owns the timestamp-only case.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let changed = verified_hashes(File::open(&path).unwrap(), true, &mut cache).unwrap();
        assert_ne!(changed.blake3, both.blake3);
        assert_ne!(changed.sha256, both.sha256);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"before").unwrap();
        assert_eq!(
            verified_hashes(File::open(&path).unwrap(), true, &mut cache).unwrap(),
            both
        );
        assert!(verified_hashes(File::open(dir.path()).unwrap(), true, &mut cache).is_err());
    }
}
