//! Explicit, fixed local source handoff. No portable sparse-directory fallback.
use super::*;
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::PathBuf};

const BINDING: &str = "rafs-source.json";

/// Explicit local RAFS selection for a fresh Linux VM.
///
/// Fields identify immutable published input and an explicitly trusted reader;
/// tags, remote stores, templates and execution checkpoint overrides are unsupported.
/// The private store must remain immutable and outside guest-visible/writable trees.
/// Cooperating reclamation must take an exclusive lock on the selected MERGED.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RafsRootfs {
    /// Private published store containing `merged/<revision>/MERGED.json` and blobs.
    pub store: PathBuf,
    /// Exact pvisor-rafs-merged-v1:<64 lowercase hex> identity; no alias resolution.
    pub handle: String,
    /// Explicit trusted native reader executable, owned by the current user or root
    /// and not writable by group/others. It must stay unchanged through execution.
    pub reader: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    source: RafsRootfs,
    commit: (u64, u64),
    reader: (u64, u64),
}

impl RafsRootfs {
    pub(crate) fn validate_view(&self, root: &Path) -> io::Result<()> {
        let binding = read_binding(root)?;
        if binding.source.store != self.store.canonicalize()?
            || binding.source.reader != self.reader.canonicalize()?
            || binding.source.handle != self.handle
        {
            return Err(invalid("RAFS selector does not match its prepared view"));
        }
        Ok(())
    }
    fn open(&self, parent: &Path, expected: Option<&Binding>) -> io::Result<(RafsImage, Binding)> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let revision = self
            .handle
            .strip_prefix(PREFIX)
            .filter(|v| {
                v.len() == 64
                    && v.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(|| invalid("expected fixed RAFS merged handle"))?;
        private(&self.store)?;
        let source = Self {
            store: self.store.canonicalize()?,
            reader: self.reader.canonicalize()?,
            handle: self.handle.clone(),
        };
        private(&source.store.join("merged"))?;
        private(&source.store.join("merged").join(revision))?;
        let lease = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(
                source
                    .store
                    .join("merged")
                    .join(revision)
                    .join("MERGED.json"),
            )?;
        let commit = identity(&lease)?;
        fs2::FileExt::try_lock_shared(&lease)?;
        let executable = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&source.reader)?;
        let reader = identity(&executable)?;
        if executable.metadata()?.mode() & 0o111 == 0 {
            return Err(invalid("RAFS reader is not executable"));
        }
        if expected.is_some_and(|binding| binding.commit != commit || binding.reader != reader) {
            return Err(invalid(
                "RAFS source identity changed before runner handoff",
            ));
        }
        let mut image = RafsImage::open(
            &source.store,
            &source.handle,
            &source.reader,
            parent,
            Duration::from_secs(30),
        )?;
        Arc::get_mut(&mut image.source).unwrap()._lease = Some(lease);
        Ok((
            image,
            Binding {
                version: 1,
                source,
                commit,
                reader,
            },
        ))
    }

    /// Authenticate a local selection and create a private CLI/runner projection.
    ///
    /// parent must exist outside the store. Retain the view until all native
    /// operations and child runners stop. Each runner independently authenticates
    /// the binding and holds a shared artifact lease and reader until teardown.
    /// Failures remove newly owned projection/cache state; inputs remain unchanged.
    /// Abrupt parent death kills its reader; abandoned host directories may require
    /// owner recovery. No node cache or persistent template/checkpoint lease is added.
    pub fn prepare(&self, parent: &Path) -> io::Result<RafsView> {
        let (image, binding) = self.open(parent, None)?;
        let view = image.project(parent)?;
        use pvisor_journal::api::{DurableFiles, Persistence};
        Persistence::atomic_write(
            &view._directory.path().join(BINDING),
            &serde_json::to_vec(&binding)?,
            0o600,
        )
        .map_err(io::Error::other)?;
        Ok(view)
    }
}

pub(crate) fn private_owner(root: &Path) -> Option<&Path> {
    super::is_view(root).then(|| root.parent().unwrap())
}

/// Caller retains the returned attachment before retiring its host-owned view.
fn read_binding(root: &Path) -> io::Result<Binding> {
    let parent = private_owner(root).ok_or_else(|| invalid("not a RAFS view"))?;
    private(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join(BINDING))?;
    identity(&file)?;
    if file.metadata()?.len() > 64 * 1024 {
        return Err(invalid("RAFS binding too large"));
    }
    let binding: Binding = serde_json::from_reader(file.take(64 * 1024 + 1))?;
    if binding.version != 1 {
        return Err(invalid("unsupported RAFS binding version"));
    }
    Ok(binding)
}

pub(crate) fn reattach(root: &Path) -> io::Result<(BackendAttachment, PathBuf)> {
    if root
        .parent()
        .is_some_and(|p| p.join(super::node::BINDING).exists())
    {
        if root.parent().unwrap().join(BINDING).exists() {
            return Err(invalid("conflicting RAFS handoff bindings"));
        }
        return super::node::reattach(root);
    }
    let binding = read_binding(root)?;
    let parent = root.parent().unwrap();
    let (image, _) = binding.source.open(parent, Some(&binding))?;
    let attachment = pvisor_overlay_core::backend::attach_image(
        root,
        &parent.join("objects"),
        image.source.clone(),
    )?;
    Ok((attachment, binding.source.store))
}
