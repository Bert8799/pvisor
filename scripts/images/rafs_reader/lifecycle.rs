//! Durable dependencies and metadata reclamation for the explicit node profile.
use super::*;
use std::collections::BTreeSet;

pub(super) fn directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => File::open(path.parent().context("missing parent")?)?.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    private_directory(path)
}
fn sync(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub(super) fn gate(store: &Path, exclusive: bool) -> Result<File> {
    directory(&store.join("locks"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(store.join("locks/publish.lock"))?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
        "invalid publication gate"
    );
    ensure!(
        unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )
        } == 0,
        io::Error::last_os_error()
    );
    Ok(file)
}
fn names(path: &Path, limit: usize) -> Result<Vec<String>> {
    private_directory(path)?;
    let mut result = Vec::new();
    for entry in fs::read_dir(path)? {
        ensure!(result.len() < limit, "node catalog exceeds traversal limit");
        result.push(
            entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid catalog name"))?,
        );
    }
    result.sort();
    Ok(result)
}
pub(super) fn revisions(store: &Path) -> Result<Vec<String>> {
    let result = names(&store.join("merged"), 4096)?;
    ensure!(
        result.iter().all(|id| hex_digest(id)),
        "unknown metadata revision"
    );
    Ok(result)
}
pub(super) fn commit(store: &Path, handle: &str) -> Result<(File, Commit)> {
    let id = handle
        .strip_prefix(PREFIX)
        .filter(|id| hex_digest(id))
        .context("invalid handle")?;
    let bundle = store.join("merged").join(id);
    private_directory(&bundle)?;
    let mut file = regular(&bundle.join("MERGED.json"))?;
    ensure!(
        file.metadata()?.len() <= MAX_FRAME as u64,
        "commit exceeds limit"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        sha256(&bytes) == format!("sha256:{id}"),
        "commit identity mismatch"
    );
    let record: Commit = serde_json::from_slice(&bytes)?;
    ensure!(
        record.schema == "pvisor-rafs-merged-v1"
            && record.blobs.len() <= 4096
            && record.blobs.keys().all(|id| hex_digest(id)),
        "invalid node commit"
    );
    Ok((file, record))
}
#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Pin {
    version: u32,
    kind: String,
    owner: String,
    handle: String,
    device: u64,
    inode: u64,
}
fn key(kind: &str, owner: &str, handle: &str) -> Result<String> {
    ensure!(
        matches!(kind, "template" | "checkpoint")
            && !owner.is_empty()
            && owner.len() <= 128
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')),
        "invalid dependency owner"
    );
    ensure!(
        handle.strip_prefix(PREFIX).is_some_and(hex_digest),
        "invalid dependency handle"
    );
    Ok(sha256(&serde_json::to_vec(&(kind, owner, handle))?)[7..].into())
}
fn read_pin(store: &Path, path: &Path, expected: &str) -> Result<Pin> {
    private_directory(path)?;
    let pin: Pin = serde_json::from_slice(&control(&path.join("PIN.json"))?)?;
    ensure!(
        pin.version == 1 && key(&pin.kind, &pin.owner, &pin.handle)? == expected,
        "dependency identity mismatch"
    );
    let file = regular(&path.join("MERGED.json"))?;
    let info = file.metadata()?;
    ensure!(
        (info.dev(), info.ino()) == (pin.device, pin.inode)
            && sha256(&control(&path.join("MERGED.json"))?)
                == format!("sha256:{}", &pin.handle[PREFIX.len()..]),
        "dependency commit rebound"
    );
    ensure!(
        names(path, 4)? == ["MERGED.json", "PIN.json"],
        "unexpected dependency objects"
    );
    let (source, _) = commit(store, &pin.handle)?;
    ensure!(
        (source.metadata()?.dev(), source.metadata()?.ino()) == (pin.device, pin.inode),
        "retained metadata disappeared or changed"
    );
    Ok(pin)
}
fn release_tree(path: &Path) -> Result<()> {
    // The commit link is removed first. Only this explicit release drops retention.
    for name in ["MERGED.json", "PIN.json"] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(_) => {
                regular(&path.join(name))?;
                fs::remove_file(path.join(name))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync(path)?;
    }
    fs::remove_dir(path)?;
    sync(path.parent().unwrap())
}
/// Under the exclusive publication gate. An acknowledged pin never expires.
pub(super) fn recover(store: &Path) -> Result<()> {
    directory(&store.join("pins"))?;
    directory(&store.join("retired"))?;
    for name in names(&store.join("pins"), 4096)? {
        let path = store.join("pins").join(&name);
        if let Some(id) = name.strip_prefix("pending-") {
            ensure!(hex_digest(id), "unknown pending dependency");
            private_directory(&path)?;
            if path.join("MERGED.json").try_exists()? {
                read_pin(store, &path, id)?;
                let final_path = store.join("pins").join(id);
                ensure!(
                    !final_path.try_exists()?,
                    "conflicting dependency transaction"
                );
                fs::rename(&path, &final_path)?;
                sync(&store.join("pins"))?;
            } else {
                for entry in names(&path, 3)? {
                    ensure!(
                        matches!(entry.as_str(), "PIN.json" | "PIN.new"),
                        "unknown unfinished dependency object"
                    );
                    regular(&path.join(&entry))?;
                    fs::remove_file(path.join(entry))?;
                }
                fs::remove_dir(&path)?;
                sync(&store.join("pins"))?;
            }
        } else if let Some(id) = name.strip_prefix("releasing-") {
            ensure!(hex_digest(id), "unknown releasing dependency");
            ensure!(
                names(&path, 3)?
                    .iter()
                    .all(|n| matches!(n.as_str(), "PIN.json" | "MERGED.json")),
                "unknown release objects"
            );
            release_tree(&path)?;
        } else {
            ensure!(hex_digest(&name), "unknown dependency record");
            read_pin(store, &path, &name)?;
        }
    }
    for name in names(&store.join("retired"), 4096)? {
        finish_remove(store, &name, None)?;
    }
    Ok(())
}
pub(super) fn pin(
    store: &Path,
    image: Option<&Image>,
    handle: &str,
    kind: &str,
    owner: &str,
    action: &str,
) -> Result<Value> {
    let id = key(kind, owner, handle)?;
    ensure!(
        matches!(action, "retain" | "release" | "inspect"),
        "invalid dependency action"
    );
    let _gate = gate(store, true)?;
    if action == "inspect" {
        ensure!(
            !store
                .join("pins")
                .join(format!("pending-{id}"))
                .try_exists()?
                && !store
                    .join("pins")
                    .join(format!("releasing-{id}"))
                    .try_exists()?,
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "dependency transaction unresolved; retry retain/release"
            )
        );
    } else {
        recover(store)?;
    }
    let path = store.join("pins").join(&id);
    let exists = path.try_exists()?;
    if exists {
        let pin = read_pin(store, &path, &id)?;
        ensure!(
            pin.handle == handle && pin.kind == kind && pin.owner == owner,
            "dependency collision"
        );
    }
    match action {
        "retain" if !exists => {
            ensure!(
                names(&store.join("pins"), 4096)?.len() < 4096,
                "dependency catalog full"
            );
            let image = image.context("retain requires authenticated image")?;
            let info = image._commit.metadata()?;
            let pending = store.join("pins").join(format!("pending-{id}"));
            directory(&pending)?;
            let record = Pin {
                version: 1,
                kind: kind.into(),
                owner: owner.into(),
                handle: handle.into(),
                device: info.dev(),
                inode: info.ino(),
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(pending.join("PIN.new"))?;
            file.write_all(&serde_json::to_vec(&record)?)?;
            file.sync_all()?;
            fs::rename(pending.join("PIN.new"), pending.join("PIN.json"))?;
            sync(&pending)?;
            fs::hard_link(
                store
                    .join("merged")
                    .join(&handle[PREFIX.len()..])
                    .join("MERGED.json"),
                pending.join("MERGED.json"),
            )?;
            read_pin(store, &pending, &id)?;
            File::open(pending.join("MERGED.json"))?.sync_all()?;
            sync(&pending)?;
            fs::rename(&pending, &path)?;
            sync(&store.join("pins"))?;
        }
        "release" if exists => {
            let releasing = store.join("pins").join(format!("releasing-{id}"));
            fs::rename(&path, &releasing)?;
            sync(&store.join("pins"))?;
            release_tree(&releasing)?;
        }
        _ => {}
    }
    Ok(
        json!({"status":"ok","dependency":{"version":1,"key":id,"kind":kind,"owner":owner,"handle":handle,"retained":action=="retain" || action=="inspect" && exists}}),
    )
}
/// References protected by a commit lock or durable hardlink. Caller holds shared gate.
pub(super) fn protected(store: &Path) -> Result<BTreeSet<String>> {
    let mut protected = BTreeSet::new();
    // Validate the catalog first; corruption must not turn a dependency into a miss.
    for name in names(&store.join("pins"), 4096)? {
        ensure!(hex_digest(&name), "dependency recovery required");
        read_pin(store, &store.join("pins").join(&name), &name)?;
    }
    for id in revisions(store)? {
        let (file, record) = commit(store, &format!("{PREFIX}{id}"))?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let error = io::Error::last_os_error();
            ensure!(error.kind() == io::ErrorKind::WouldBlock, error);
        }
        if locked != 0 || file.metadata()?.nlink() > 1 {
            protected.extend(record.blobs.into_keys());
        }
    }
    Ok(protected)
}
fn node_subset(bundle: &Path, record: &Commit, partial: bool) -> Result<()> {
    for name in names(bundle, 4)? {
        ensure!(
            matches!(name.as_str(), "MERGED.json" | "metadata" | "blobs"),
            "remove requires metadata-only node profile"
        );
    }
    if bundle.join("blobs").try_exists()? {
        ensure!(
            names(&bundle.join("blobs"), 1)?.is_empty(),
            "node profile contains payload"
        );
    } else {
        ensure!(partial, "missing blob directory");
    }
    if bundle.join("metadata").try_exists()? {
        for name in names(&bundle.join("metadata"), 3)? {
            let desc = match name.as_str() {
                "image.bootstrap" => &record.bootstrap,
                "source.config.json" => &record.config,
                _ => anyhow::bail!("unknown node metadata object"),
            };
            let mut f = regular(&bundle.join("metadata").join(&name))?;
            ensure!(
                f.metadata()?.len() == desc.size,
                "node metadata size mismatch"
            );
            let mut hash = digest::RafsDigest::hasher(digest::Algorithm::Sha256);
            let mut buf = [0; 65536];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.digest_update(&buf[..n]);
            }
            ensure!(
                format!("sha256:{}", hash.digest_finalize()) == desc.sha256,
                "node metadata digest mismatch"
            );
        }
        if !partial {
            ensure!(
                names(&bundle.join("metadata"), 3)?.len() == 2,
                "incomplete node metadata"
            );
        }
    } else {
        ensure!(partial, "missing metadata directory");
    }
    Ok(())
}
fn finish_remove(store: &Path, name: &str, admitted: Option<File>) -> Result<()> {
    let parts: Vec<_> = name.split('-').collect();
    ensure!(
        parts.len() == 3 && hex_digest(parts[0]),
        "unknown metadata tombstone"
    );
    let identity = (
        u64::from_str_radix(parts[1], 16)?,
        u64::from_str_radix(parts[2], 16)?,
    );
    let path = store.join("retired").join(name);
    private_directory(&path)?;
    if path.join("MERGED.json").try_exists()? {
        let current = regular(&path.join("MERGED.json"))?;
        ensure!(
            (current.metadata()?.dev(), current.metadata()?.ino()) == identity,
            "rebound tombstone path"
        );
        // Continue the original open-file-description lock after rename. Opening
        // and locking a second descriptor would conflict with our own flock.
        let file = admitted.unwrap_or(current);
        ensure!(
            (file.metadata()?.dev(), file.metadata()?.ino()) == identity
                && file.metadata()?.nlink() == 1,
            "rebound or retained metadata tombstone"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            io::Error::last_os_error()
        );
        let raw = control(&path.join("MERGED.json"))?;
        ensure!(
            sha256(&raw) == format!("sha256:{}", parts[0]),
            "tombstone identity mismatch"
        );
        let record: Commit = serde_json::from_slice(&raw)?;
        node_subset(&path, &record, true)?;
        for dir in ["metadata", "blobs"] {
            if !path.join(dir).try_exists()? {
                continue;
            }
            for file in names(&path.join(dir), 3)? {
                fs::remove_file(path.join(dir).join(file))?;
            }
            sync(&path.join(dir))?;
            fs::remove_dir(path.join(dir))?;
        }
        sync(&path)?;
        fs::remove_file(path.join("MERGED.json"))?;
        sync(&path)?;
    }
    ensure!(
        names(&path, 1)?.is_empty(),
        "unrecognized remaining tombstone"
    );
    fs::remove_dir(&path)?;
    sync(&store.join("retired"))
}
pub(super) fn remove(store: &Path, handle: &str) -> Result<Value> {
    let _gate = gate(store, true)?;
    recover(store)?;
    let (file, record) = commit(store, handle)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        io::Error::last_os_error()
    );
    ensure!(
        file.metadata()?.nlink() == 1,
        io::Error::from_raw_os_error(libc::EBUSY)
    );
    let id = &handle[PREFIX.len()..];
    let path = store.join("merged").join(id);
    node_subset(&path, &record, false)?;
    let name = format!(
        "{id}-{:x}-{:x}",
        file.metadata()?.dev(),
        file.metadata()?.ino()
    );
    fs::rename(&path, store.join("retired").join(&name))?;
    sync(&store.join("merged"))?;
    sync(&store.join("retired"))?;
    finish_remove(store, &name, Some(file))?;
    Ok(json!({"status":"ok","removed":handle}))
}
