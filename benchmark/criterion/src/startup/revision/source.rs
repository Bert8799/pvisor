use super::{digest, read, required_path, save, tree};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub(super) struct Source {
    pub name: String,
    pub config: String,
    pub rootfs: Option<PathBuf>,
    immutable: BTreeMap<PathBuf, BTreeMap<PathBuf, Value>>,
    artifacts: BTreeMap<PathBuf, String>,
}
impl Source {
    pub fn prepare(
        binary: &Path,
        rootfs: &Path,
        firmware: &Path,
        output: &Path,
        private: &Path,
    ) -> Result<Self> {
        let name = env::var("PVISOR_BENCH_REVISION_SOURCE").unwrap_or_else(|_| "directory".into());
        let mut immutable = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        let (config, selected) = match name.as_str() {
            "directory" => (String::new(), Some(rootfs.to_path_buf())),
            "template" => {
                let catalog = output.join("catalog");
                let mut command = Command::new(binary);
                command
                    .args(["cache", "prepare-template", "--rootfs"])
                    .arg(rootfs)
                    .arg("--catalog")
                    .arg(&catalog)
                    .arg("--work-dir")
                    .arg(private)
                    .args(["--cpus", "2", "--memory-mib", "128", "--library-dir"])
                    .arg(firmware)
                    .env("PVISOR_KRUN_ENOMEM_WORKAROUND", "0")
                    .env("PVISOR_STARTUP_PROFILE", "0");
                let result = crate::command::observe(&mut command, Duration::from_secs(120))?;
                save(output.join("template-preparation.json"), &result)?;
                ensure!(
                    result.exit_code == Some(0) && !result.timed_out,
                    "template preparation failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let prepared: Value = serde_json::from_slice(&result.stdout)?;
                let version = prepared["version"].as_str().context("template version")?;
                ensure!(
                    version.len() == 64 && version.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid template version"
                );
                immutable.insert(catalog.clone(), source_tree(&catalog)?);
                save(output.join("template-selection.json"), &prepared)?;
                (
                    format!(
                        "[vm.template]\ncatalog = {}\nversion = {}\n",
                        json!(catalog),
                        json!(version)
                    ),
                    None,
                )
            }
            "rafs-local" => {
                let store = required_path("PVISOR_BENCH_RAFS_STORE")?;
                let reader_source = required_path("PVISOR_BENCH_RAFS_READER")?;
                let handle = env::var("PVISOR_BENCH_RAFS_HANDLE").context("missing RAFS handle")?;
                let revision = handle
                    .strip_prefix("pvisor-rafs-merged-v1:")
                    .filter(|s| {
                        s.len() == 64
                            && s.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                    .context("fixed RAFS merged handle required")?;
                let merged = store.join("merged").join(revision);
                verify_files(rootfs, &merged)?;
                let reader = private.join("rafs-reader");
                fs::copy(&reader_source, &reader)?;
                fs::set_permissions(&reader, fs::Permissions::from_mode(0o700))?;
                ensure!(
                    digest(&reader_source)? == digest(&reader)?,
                    "reader copy differs"
                );
                immutable.insert(merged.clone(), source_tree(&merged)?);
                let native = BTreeMap::from([
                    (reader_source.clone(), digest(&reader_source)?),
                    (reader.clone(), digest(&reader)?),
                ]);
                artifacts.extend(native.clone());
                save(
                    output.join("rafs-selection.json"),
                    &json!({"store":store,"handle":handle,"reader":reader,"tools":native,
                    "profile":"local prepublished RAFS; fresh per-operation reader; warm host page cache; no node cache or network transfer claim"}),
                )?;
                (
                    format!(
                        "[vm.rafs]\nstore = {}\nhandle = {}\nreader = {}\n",
                        json!(store),
                        json!(handle),
                        json!(reader)
                    ),
                    None,
                )
            }
            _ => bail!(
                "unknown revision source {name:?}; expected directory, template or rafs-local"
            ),
        };
        save(
            output.join("source.json"),
            &json!({"kind":name,"config":config,"rootfs_argument":selected,"overlaynet":"auto"}),
        )?;
        save(output.join("source-inputs.json"), &immutable)?;
        Ok(Self {
            name,
            config,
            rootfs: selected,
            immutable,
            artifacts,
        })
    }
    pub fn check(&self) -> Result<()> {
        for (path, hash) in &self.artifacts {
            ensure!(digest(path)? == *hash, "reader changed: {}", path.display());
        }
        for (root, expected) in &self.immutable {
            ensure!(
                source_tree(root)? == *expected,
                "{} source changed: {}",
                self.name,
                root.display()
            );
        }
        Ok(())
    }
}

// Opening SnapshotStore reapplies directory permissions. Preserve its semantic
// metadata and full inventory while allowing that directory-only ctime update.
fn source_tree(root: &Path) -> Result<BTreeMap<PathBuf, Value>> {
    let mut snapshot = tree(root)?;
    for node in snapshot.values_mut() {
        if node["content"]["directory"] == true {
            node.as_object_mut().unwrap().remove("ctime");
        }
    }
    Ok(snapshot)
}

// The source image and unpacked comparison tree must provide the same file
// bytes, directory inventory, modes and symlink targets before any sampling.
fn verify_files(rootfs: &Path, merged: &Path) -> Result<()> {
    let commit = read(merged.join("MERGED.json"))?;
    let inventory_path = merged.join("metadata/inventory.json");
    ensure!(
        format!("sha256:{}", digest(&inventory_path)?) == commit["inventory"]["sha256"],
        "inventory digest mismatch"
    );
    let inventory = read(&inventory_path)?;
    verify_inventory(rootfs, &inventory)
}

fn map_id(id: u64, mapping: &str) -> Result<u64> {
    for line in mapping.lines() {
        let values = line
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(values.len() == 3, "invalid namespace ID mapping");
        if id >= values[1] && id - values[1] < values[2] {
            return Ok(values[0] + id - values[1]);
        }
    }
    Ok(65534)
}

fn verify_inventory(rootfs: &Path, inventory: &Value) -> Result<()> {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let actual = tree(rootfs)?;
    let expected = inventory.as_object().context("RAFS inventory object")?;
    ensure!(
        actual.len() == expected.len(),
        "RAFS/rootfs inventory size differs"
    );
    let uids = fs::read_to_string("/proc/self/uid_map")?;
    let gids = fs::read_to_string("/proc/self/gid_map")?;
    for (path, node) in &actual {
        let key = path.to_str().context("non-UTF8 image path")?;
        let wanted = expected
            .get(key)
            .with_context(|| format!("extra rootfs path {key}"))?;
        let full = rootfs.join(path);
        let info = fs::symlink_metadata(&full)?;
        ensure!(
            node["mode"].as_u64().unwrap() & 0o7777
                == wanted["mode"].as_u64().context("image mode")?,
            "image mode differs: {key}"
        );
        for (field, mapping) in [("uid", &uids), ("gid", &gids)] {
            ensure!(
                node[field].as_u64()
                    == Some(map_id(
                        wanted[field].as_u64().context("image ID")?,
                        mapping
                    )?),
                "image {field} differs: {key}"
            );
        }
        ensure!(
            i128::from(info.mtime()) * 1_000_000_000 + i128::from(info.mtime_nsec())
                == wanted["mtime_ns"].as_i64().context("image mtime")? as i128,
            "image mtime differs: {key}"
        );
        let native = std::ffi::CString::new(full.as_os_str().as_bytes())?;
        let xattrs = unsafe { libc::llistxattr(native.as_ptr(), std::ptr::null_mut(), 0) };
        ensure!(
            xattrs == 0 && wanted["xattrs"].as_object().is_some_and(|v| v.is_empty()),
            "revision image fixture requires no xattrs: {key}"
        );
        match wanted["kind"].as_str() {
            Some("file") => {
                ensure!(
                    info.is_file()
                        && Some(info.len()) == wanted["size"].as_u64()
                        && format!(
                            "sha256:{}",
                            node["content"]["sha256"].as_str().context("file digest")?
                        ) == wanted["sha256"],
                    "image file differs: {key}"
                );
                for alias in wanted["hardlinks"].as_array().context("image hardlinks")? {
                    let alias = alias.as_str().context("hardlink path")?;
                    ensure!(expected.contains_key(alias), "hardlink outside image");
                    let other = fs::symlink_metadata(rootfs.join(alias))?;
                    ensure!(
                        (info.dev(), info.ino()) == (other.dev(), other.ino()),
                        "image hardlink differs: {key}"
                    );
                }
            }
            Some("directory") => ensure!(info.is_dir(), "image directory differs: {key}"),
            Some("symlink") => ensure!(
                info.is_symlink() && node["content"]["symlink"] == wanted["target"],
                "image symlink differs: {key}"
            ),
            _ => bail!("unsupported image node: {key}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_snapshot_keeps_file_identity_content_and_directory_membership() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("data"), b"before").unwrap();
        let before = source_tree(root.path()).unwrap();
        assert!(before[Path::new("")].get("ctime").is_none());
        assert!(before[Path::new("data")].get("ctime").is_some());
        fs::write(root.path().join("data"), b"after").unwrap();
        assert_ne!(source_tree(root.path()).unwrap(), before);
        fs::create_dir(root.path().join("extra")).unwrap();
        assert_ne!(
            source_tree(root.path()).unwrap().keys().collect::<Vec<_>>(),
            before.keys().collect::<Vec<_>>()
        );
    }
    #[test]
    fn namespace_mapping_preserves_ids_and_marks_unmapped_owners() {
        assert_eq!(map_id(1018, "0 1018 1\n").unwrap(), 0);
        assert_eq!(map_id(0, "0 1018 1\n").unwrap(), 65534);
        assert_eq!(map_id(1019, "0 1000 100\n").unwrap(), 19);
        assert!(map_id(1, "invalid").is_err());
    }
}
