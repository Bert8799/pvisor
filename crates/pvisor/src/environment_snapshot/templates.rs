//! Versioned inert templates, atomic selection and lifetime-aware reclamation.
//!
//! These synchronous host APIs own storage, not VM execution. Capture and publish
//! must occur in the same full-machine freeze, after the guest's handoff Ready
//! and before sending any task. The host owns VM topology, source quiescence and
//! CPU/device validation. The dedicated catalog must not be modified externally.
#![deny(missing_docs)]

use super::store::digest;
use super::{Compatibility, PublishedEnvironment, SnapshotStore, copy_owned_tree, file_hash};
use anyhow::{Context, ensure};
use pvisor_guest::template::{BootConfig, Ready, Template};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const SCHEMA: u32 = 1;
const RECORD_LIMIT: u64 = 64 * 1024;

/// Storage role for inert template captures. Execution checkpoints and prepared
/// filesystem bases must not authenticate as this profile. Device eligibility
/// and machine validation remain VMM responsibilities.
pub const PROFILE: &str = "pvisor-inert-template-v1";

/// All inputs that distinguish one immutable template generation. No workload,
/// application credential or per-instance identity belongs here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TemplateSpec {
    /// Storage recipe schema; must be 1. Guest handoff uses its own version.
    pub version: u32,
    /// New host-assigned build generation, e.g. a UUID. Never reuse it to replace
    /// an existing capture, even when the other inputs are identical.
    pub generation: String,
    /// SHA-256 of the immutable source image's complete inventory, obtained with
    /// [`Self::image_id`] before platform initialization modifies its private copy.
    pub image_sha256: String,
    /// SHA-256 of the exact guest supervisor installed in the private image.
    pub guest_sha256: String,
    /// Exact vCPU count. The VM builder must enforce it when capturing/restoring.
    pub cpus: u8,
    /// Exact guest RAM budget in MiB; host overhead is additional.
    pub memory_mib: u32,
    /// Same-host boot, runner build, firmware and device-profile binding.
    pub compatibility: Compatibility,
    /// Handoff console topology, including required stdin and stdout ports.
    pub stdio_ports: [bool; 3],
    /// Optional single pristine virtio-net device's exact nonzero unicast MAC.
    /// Each restored instance supplies a fresh isolated backend; no connection
    /// state is captured. None preserves the original network-free recipe bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mac: Option<[u8; 6]>,
    /// Native executor layout: rootfs lower plus private upper/work and optional
    /// resolver lower inside the captured tree. False retains plain-root recipes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub native_overlay: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl TemplateSpec {
    /// Hash a quiescent source image's verified filesystem inventory. Source
    /// content, native metadata, namespace and hardlinks must remain stable.
    pub fn image_id(root: &Path) -> anyhow::Result<String> {
        Ok(digest(&serde_json::to_vec(&super::inventory(root)?)?))
    }

    /// Validate the recipe and derive its immutable generation ID. This identity
    /// binds the serialized version-1 recipe; it is not a VM-state validator.
    pub fn id(&self) -> anyhow::Result<String> {
        ensure!(
            self.version == SCHEMA,
            "unsupported template recipe version"
        );
        ensure!(
            self.compatibility.profile == PROFILE,
            "unsupported template profile"
        );
        valid_name(&self.generation)?;
        valid_digest(&self.image_sha256)?;
        valid_digest(&self.guest_sha256)?;
        ensure!(
            self.cpus > 0 && self.memory_mib > 0,
            "empty template resource budget"
        );
        if let Some(mac) = self.network_mac {
            ensure!(
                mac != [0; 6] && mac[0] & 1 == 0,
                "invalid template network MAC"
            );
        }
        for value in [
            &self.compatibility.host_boot,
            &self.compatibility.build,
            &self.compatibility.firmware,
            &self.compatibility.profile,
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 4096,
                "invalid template compatibility"
            );
        }
        let id = digest(&serde_json::to_vec(self)?);
        Template {
            version: pvisor_guest::template::VERSION,
            template_id: id.clone(),
            stdio_ports: self.stdio_ports,
        }
        .validate()?;
        Ok(id)
    }

    /// Construct the credential-free boot descriptor for this generation.
    pub fn boot(&self) -> anyhow::Result<Template> {
        Ok(Template {
            version: pvisor_guest::template::VERSION,
            template_id: self.id()?,
            stdio_ports: self.stdio_ports,
        })
    }

    /// Prepare a fresh independently owned rootfs, authenticating source and
    /// supervisor before copying. Source/destination must be independent trees;
    /// source writers must remain stopped through the copy. Existing init or
    /// config paths are rejected, never overwritten. Failure may leave a partial
    /// private destination; do not boot or reuse it. With native_overlay, the
    /// destination is a capsule containing rootfs/, upper/ and work/; boot files
    /// are placed under rootfs/. Capture must retain the entire capsule, including
    /// any native resolver lower added before boot. No VM is started here.
    pub fn prepare_rootfs(
        &self,
        source: &Path,
        guest: &Path,
        destination: &Path,
    ) -> anyhow::Result<()> {
        let boot = self.boot()?;
        ensure!(
            Self::image_id(source)? == self.image_sha256,
            "template image changed"
        );
        ensure!(
            file_hash(guest)? == self.guest_sha256,
            "template supervisor changed"
        );
        ensure!(
            !destination.try_exists()?,
            "template destination already exists"
        );
        let root = if self.native_overlay {
            fs::DirBuilder::new().mode(0o700).create(destination)?;
            for directory in ["upper", "work"] {
                fs::create_dir(destination.join(directory))?;
            }
            destination.join("rootfs")
        } else {
            destination.to_owned()
        };
        copy_owned_tree(source, &root)?;
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(guest)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("init.krun"))?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        ensure!(
            file_hash(&root.join("init.krun"))? == self.guest_sha256,
            "template supervisor changed during copy"
        );
        write_new(
            &root.join(".pvisor-guest.json"),
            &serde_json::to_vec(&BootConfig::Template { template: boot })?,
            0o400,
        )?;
        File::open(&root)?.sync_all()?;
        File::open(destination)?.sync_all()?;
        Ok(())
    }
}

/// Identity selected by an alias. Records and snapshots never change in place.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateVersion {
    /// Generation ID, equal to the recipe hash.
    pub id: String,
    /// Authenticated capture recipe.
    pub spec: TemplateSpec,
    snapshot_id: String,
}

/// Borrowed parts of one full-machine capture. All sources must remain frozen
/// and exclusively owned until publish returns; this object owns no VM barrier.
pub struct TemplateCapture<'a> {
    /// Handoff readiness observed before admitting any task bytes.
    pub ready: &'a Ready,
    /// Opaque CPU/device state from the same capture as RAM and filesystem.
    pub machine: &'a [u8],
    /// Frozen, captured raw RAM. Copying is sparse and uses bounded buffers.
    pub ram: &'a File,
    /// Private prepared root filesystem belonging to that same frozen VM, or
    /// the entire native-overlay capsule when that recipe option is enabled.
    pub rootfs: &'a Path,
}

/// Private, dedicated template catalog. Clone/open separate handles to use from
/// concurrent callers. Operations use process-independent file locks, with no
/// global memory cache or resident VM. It is not a general snapshot-store root.
/// Catalog metadata transactions wait for each other; capture copies and retired
/// payload deletion run outside that gate. Generation leases do not hold it.
pub struct TemplateCatalog {
    root: PathBuf,
}

/// A selected generation. Keep this owner through preparation and VM teardown;
/// changing/removing its alias does not revoke it. GC skips this generation while
/// allowing unrelated retired generations to be reclaimed.
pub struct TemplateLease {
    version: TemplateVersion,
    environment: PublishedEnvironment,
    _generation: File,
    catalog: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateBinding {
    pub catalog: PathBuf,
    pub id: String,
    pub compatibility: Compatibility,
}

impl TemplateLease {
    pub(crate) fn binding(&self) -> TemplateBinding {
        TemplateBinding {
            catalog: self.catalog.clone(),
            id: self.version.id.clone(),
            compatibility: self.version.spec.compatibility.clone(),
        }
    }
    /// Immutable identity selected when the lease was acquired.
    pub fn version(&self) -> &TemplateVersion {
        &self.version
    }
    /// Authenticated state, raw read-only RAM file and independent filesystem
    /// materialization. The caller must rebind saved filesystem paths and install
    /// the exact CPU/device topology through the VM API before resuming. This
    /// accessor grants no mutation or deletion authority over catalog payloads.
    pub fn environment(&self) -> &PublishedEnvironment {
        &self.environment
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    version: u32,
    id: String,
}

impl TemplateCatalog {
    pub(crate) fn validate_source(&self, source: &Path) -> anyhow::Result<PathBuf> {
        let source = source.canonicalize()?;
        ensure!(
            source != Path::new("/")
                && !source.starts_with(&self.root)
                && !self.root.starts_with(&source),
            "template source overlaps catalog or host root"
        );
        Ok(source)
    }
    /// Open/create private catalog directories beneath an existing parent. Paths
    /// must be absolute; existing symlinks, foreign owners or nonprivate entries
    /// fail. Use a trusted host parent and keep the entire tree guest-inaccessible.
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        ensure!(root.is_absolute(), "template catalog path must be absolute");
        directory(root)?;
        let catalog = Self {
            root: root.canonicalize()?,
        };
        let _gate = catalog.gate(true)?;
        for name in ["aliases", "generations", "pending", "deleted", "locks"] {
            directory(&catalog.root.join(name))?;
        }
        Ok(catalog)
    }

    /// Read an alias's selected generation, or None if it does not exist. This
    /// alone pins no payload; acquire a lease before preparing a VM.
    pub fn selected(&self, name: &str) -> anyhow::Result<Option<String>> {
        valid_name(name)?;
        let _gate = self.gate(false)?;
        self.selection(name)
    }

    /// Publish frozen machine/RAM/filesystem bytes and atomically select them.
    /// `expected` is compare-and-swap: None requires an absent alias; Some requires
    /// its exact old generation. Losing builders leave the alias untouched.
    ///
    /// Call within the SAME full CPU/device/RAM freeze as machine capture, after
    /// receiving handoff Ready and before sending any task bytes. Source must be
    /// the private rootfs prepared by this recipe, without external writers or
    /// overlapping catalog storage. No network/credential migration is implied;
    /// typed machine validation and supported device profiles belong to the VMM.
    /// Generation is immutable. A failed publication can leave an unselected
    /// generation or interrupted staging data for collect(); alias fsync failure
    /// can follow selection, so inspect selected() before retrying.
    pub fn publish(
        &self,
        name: &str,
        expected: Option<&str>,
        spec: &TemplateSpec,
        capture: TemplateCapture<'_>,
    ) -> anyhow::Result<TemplateVersion> {
        let TemplateCapture {
            ready,
            machine,
            ram,
            rootfs: source,
        } = capture;
        valid_name(name)?;
        let id = spec.id()?;
        if let Some(expected) = expected {
            valid_digest(expected)?;
        }
        let source = self.validate_source(source)?;
        ensure!(
            ready.version == pvisor_guest::template::VERSION && ready.template_id == id,
            "template readiness identity mismatch"
        );
        let boot_root = if spec.native_overlay {
            source.join("rootfs")
        } else {
            source.clone()
        };
        let boot: BootConfig = read_json(&boot_root.join(".pvisor-guest.json"))?;
        ensure!(
            matches!(boot, BootConfig::Template { template }
            if template.template_id == id && template.version == ready.version
            && template.stdio_ports == spec.stdio_ports),
            "template boot recipe mismatch"
        );
        ensure!(
            file_hash(&boot_root.join("init.krun"))? == spec.guest_sha256,
            "template supervisor changed before capture"
        );
        let (staging, _writer) = {
            let _gate = self.gate(true)?;
            ensure!(
                self.selection(name)?.as_deref() == expected,
                "template selection changed"
            );
            let staging = tempfile::Builder::new()
                .prefix("generation-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(self.root.join("pending"))?;
            let writer = lock(&staging.path().join("writer.lock"), true)?;
            (staging, writer)
        };
        let store = SnapshotStore::new(&staging.path().join("store"))?;
        let pending = store.begin()?;
        pending.copy_ram_from(ram)?;
        let snapshot_id = pending.publish(&source, machine, spec.compatibility.clone())?;
        let record = TemplateVersion {
            id: id.clone(),
            spec: spec.clone(),
            snapshot_id,
        };
        write_new(
            &staging.path().join("version.json"),
            &serde_json::to_vec(&record)?,
            0o600,
        )?;
        File::open(staging.path())?.sync_all()?;
        let _gate = self.gate(true)?;
        ensure!(
            self.selection(name)?.as_deref() == expected,
            "template selection changed"
        );
        let destination = self.root.join("generations").join(&id);
        ensure!(
            !destination.try_exists()?,
            "template generation already exists"
        );
        // A permanent, empty marker prevents reusing an identity after GC.
        // Reserve before publishing; a late failed build must use a fresh recipe.
        let reservation = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.root.join("locks").join(format!("{id}.lock")))
            .context("template generation was already used; choose a fresh generation")?;
        fs2::FileExt::try_lock_shared(&reservation)?;
        reservation.sync_all()?;
        File::open(self.root.join("locks"))?.sync_all()?;
        fs::rename(staging.path(), &destination)?;
        File::open(self.root.join("generations"))?.sync_all()?;
        File::open(self.root.join("pending"))?.sync_all()?;
        let mut file = tempfile::Builder::new()
            .prefix(".selection-")
            .tempfile_in(self.root.join("aliases"))?;
        serde_json::to_writer(
            &mut file,
            &Selection {
                version: SCHEMA,
                id,
            },
        )?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("aliases").join(format!("{name}.json")))?;
        File::open(self.root.join("aliases"))?.sync_all()?;
        Ok(record)
    }

    /// Acquire an alias's current immutable generation and validate exact restore
    /// compatibility. Each generation has its own reader gate, so running one VM
    /// does not pin every retired template. Partial failure releases all leases.
    pub fn acquire(&self, name: &str, expected: &Compatibility) -> anyhow::Result<TemplateLease> {
        valid_name(name)?;
        let (version, generation) = {
            let _gate = self.gate(false)?;
            let id = self.selection(name)?.context("template alias not found")?;
            self.version_lease(&id)?
        };
        self.open_lease(version, generation, expected)
    }

    /// Acquire the exact generation fixed by an earlier admission, without
    /// consulting a mutable alias. Keep the admitting lease until the recipient
    /// has acquired this one. Reclaimed or unknown versions fail; they are never
    /// rebuilt, substituted or revived. Compatibility and lease lifetime are the
    /// same as acquire(). This supports handoff to detached VM supervisors.
    pub fn acquire_version(
        &self,
        id: &str,
        expected: &Compatibility,
    ) -> anyhow::Result<TemplateLease> {
        valid_digest(id)?;
        let (version, generation) = {
            let _gate = self.gate(false)?;
            self.version_lease(id)?
        };
        self.open_lease(version, generation, expected)
    }

    fn version_lease(&self, id: &str) -> anyhow::Result<(TemplateVersion, File)> {
        let version = self.version(id)?;
        let generation = private_file(&self.root.join("locks").join(format!("{id}.lock")), false)?;
        fs2::FileExt::try_lock_shared(&generation)?;
        Ok((version, generation))
    }

    fn open_lease(
        &self,
        version: TemplateVersion,
        generation: File,
        expected: &Compatibility,
    ) -> anyhow::Result<TemplateLease> {
        ensure!(
            version.spec.compatibility == *expected,
            "template compatibility mismatch"
        );
        let environment = SnapshotStore::new(
            &self
                .root
                .join("generations")
                .join(&version.id)
                .join("store"),
        )?
        .open_for_restore(&version.snapshot_id, expected)?;
        Ok(TemplateLease {
            version,
            environment,
            _generation: generation,
            catalog: self.root.clone(),
        })
    }

    /// Stop selecting this alias, requiring its exact current generation. Existing
    /// leases remain usable; payload deletion is a separate collect operation.
    /// A sync failure can follow unlink; inspect selected() before retrying.
    pub fn remove(&self, name: &str, expected: &str) -> anyhow::Result<()> {
        valid_name(name)?;
        valid_digest(expected)?;
        let _gate = self.gate(true)?;
        ensure!(
            self.selection(name)?.as_deref() == Some(expected),
            "template selection changed"
        );
        fs::remove_file(self.root.join("aliases").join(format!("{name}.json")))?;
        File::open(self.root.join("aliases"))?.sync_all()?;
        Ok(())
    }

    /// Reclaim unselected, unleased generations, abandoned builders and deletion
    /// tombstones. Live builders/readers are skipped. Payload removal runs outside
    /// the catalog gate, so slow deletion does not hold alias admission. GC is
    /// serialized separately; errors may follow partial reclamation and are
    /// retryable. Returns the number of private trees removed, not bytes saved.
    pub fn collect(&self) -> anyhow::Result<usize> {
        let _collector = lock(&self.root.join("collect.lock"), true)?;
        {
            let _gate = self.gate(true)?;
            let mut selected = BTreeSet::new();
            for entry in fs::read_dir(self.root.join("aliases"))? {
                let path = entry?.path();
                if path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with(".selection-"))
                {
                    private_file(&path, false)?;
                    fs::remove_file(path)?;
                    continue;
                }
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .context("invalid template alias")?;
                ensure!(
                    path.extension().is_some_and(|ext| ext == "json"),
                    "invalid template alias file"
                );
                selected.insert(
                    self.selection(name)?
                        .context("template alias disappeared")?,
                );
            }
            File::open(self.root.join("aliases"))?.sync_all()?;
            // Validate every selected target before allowing any reclamation.
            for id in &selected {
                self.version(id)?;
            }
            for entry in fs::read_dir(self.root.join("generations"))? {
                let path = entry?.path();
                let id = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("invalid generation name")?;
                self.version(id)?;
                if selected.contains(id) {
                    continue;
                }
                let file = private_file(&self.root.join("locks").join(format!("{id}.lock")), true)?;
                match fs2::FileExt::try_lock_exclusive(&file) {
                    Ok(()) => self.retire(&path)?,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            for entry in fs::read_dir(self.root.join("pending"))? {
                let path = entry?.path();
                directory_existing(&path)?;
                let file = private_file(&path.join("writer.lock"), true)?;
                match fs2::FileExt::try_lock_exclusive(&file) {
                    Ok(()) => self.retire(&path)?,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let mut removed = 0;
        for entry in fs::read_dir(self.root.join("deleted"))? {
            let path = entry?.path();
            directory_existing(&path)?;
            super::layers::remove_private_tree(&path)?;
            removed += 1;
        }
        File::open(self.root.join("deleted"))?.sync_all()?;
        Ok(removed)
    }

    fn retire(&self, path: &Path) -> anyhow::Result<()> {
        let destination = self
            .root
            .join("deleted")
            .join(uuid::Uuid::new_v4().to_string());
        fs::rename(path, destination)?;
        File::open(path.parent().context("missing template parent")?)?.sync_all()?;
        File::open(self.root.join("deleted"))?.sync_all()?;
        Ok(())
    }
    fn gate(&self, exclusive: bool) -> anyhow::Result<File> {
        let file = private_file(&self.root.join("catalog.lock"), true)?;
        // Contention between independent admissions or a short alias update is
        // normal. Do not turn it into a spurious EWOULDBLOCK creation failure.
        if exclusive {
            fs2::FileExt::lock_exclusive(&file)?;
        } else {
            fs2::FileExt::lock_shared(&file)?;
        }
        Ok(file)
    }
    fn selection(&self, name: &str) -> anyhow::Result<Option<String>> {
        valid_name(name)?;
        let path = self.root.join("aliases").join(format!("{name}.json"));
        let selection: Selection = match read_json(&path) {
            Ok(value) => value,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        ensure!(
            selection.version == SCHEMA,
            "unsupported template selection version"
        );
        valid_digest(&selection.id)?;
        Ok(Some(selection.id))
    }
    fn version(&self, id: &str) -> anyhow::Result<TemplateVersion> {
        valid_digest(id)?;
        let directory = self.root.join("generations").join(id);
        directory_existing(&directory)?;
        let record: TemplateVersion = read_json(&directory.join("version.json"))?;
        ensure!(
            record.id == id && record.spec.id()? == id,
            "template generation identity mismatch"
        );
        valid_digest(&record.snapshot_id)?;
        Ok(record)
    }
}

fn valid_digest(value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid template digest"
    );
    Ok(())
}
fn valid_name(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "invalid template name"
    );
    Ok(())
}
fn directory_existing(path: &Path) -> anyhow::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "template directory must be private and owned: {} (mode {:o}, uid {})",
        path.display(),
        meta.mode(),
        meta.uid()
    );
    Ok(())
}
fn directory(path: &Path) -> anyhow::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            File::open(path)?.sync_all()?;
            File::open(path.parent().context("missing template parent")?)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    directory_existing(path)
}
fn private_file(path: &Path, create: bool) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && meta.nlink() == 1,
        "invalid private template file"
    );
    Ok(file)
}
fn lock(path: &Path, exclusive: bool) -> anyhow::Result<File> {
    let file = private_file(path, true)?;
    if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)?;
    } else {
        fs2::FileExt::try_lock_shared(&file)?;
    }
    Ok(file)
}
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = private_file(path, false)?;
    ensure!(
        file.metadata()?.len() <= RECORD_LIMIT,
        "template record exceeds limit"
    );
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "template record exceeds limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
