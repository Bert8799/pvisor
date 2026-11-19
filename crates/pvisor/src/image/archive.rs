//! Shared merged-tree archive writer for container and exact logical-image exports.
use anyhow::{bail, ensure};
use pvisor_overlay_core::{OverlayCore, ResolvedMetadata, backend, sys};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, Metadata, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

type Identity = fn(&Path, &Metadata) -> anyhow::Result<(u32, u32, u32)>;
pub(crate) enum MetadataProfile {
    Container(Identity),
    LogicalImage,
}

fn image_backing(
    core: &OverlayCore,
    relative: &Path,
    entry: ResolvedMetadata,
    copied: &BTreeMap<(u64, u64), Vec<PathBuf>>,
) -> anyhow::Result<(Metadata, PathBuf)> {
    if !entry.resolved.is_upper
        && (entry.metadata.is_file() || entry.metadata.file_type().is_symlink())
        && let (_, Some(current)) = core.metadata_with_copied_hard_link(relative)?
    {
        let paths = copied
            .get(&(entry.metadata.dev(), entry.metadata.ino()))
            .ok_or_else(|| io::Error::other("missing copied image alias"))?;
        for relative in paths {
            let path = core.upper().join(relative);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if (metadata.dev(), metadata.ino()) == (current.dev(), current.ino()) {
                return Ok((metadata, path));
            }
        }
        bail!("copied image alias changed during export");
    }
    Ok((entry.metadata, entry.resolved.path))
}

// Read a logical lower without materializing its complete raw contents on disk.
struct ImageReader {
    path: PathBuf,
    file: File,
    offset: u64,
}
impl io::Read for ImageReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let length = bytes.len().min(crate::image::cache::MAX_READ as usize) as u32;
        let size = if let Some(body) =
            pvisor_overlay_core::backend::read_at(&self.path, self.offset, length)?
        {
            bytes[..body.len()].copy_from_slice(&body);
            body.len()
        } else {
            use std::os::unix::fs::FileExt;
            self.file
                .read_at(&mut bytes[..length as usize], self.offset)?
        };
        self.offset += size as u64;
        Ok(size)
    }
}

pub(crate) fn write<W: io::Write>(
    core: &OverlayCore,
    output: &mut W,
    profile: MetadataProfile,
) -> anyhow::Result<()> {
    let mut archive = tar::Builder::new(output);
    let copied = if matches!(profile, MetadataProfile::LogicalImage) {
        core.capture_hard_links()?
            .into_iter()
            .map(|(dev, ino, paths)| ((dev, ino), paths))
            .collect()
    } else {
        BTreeMap::new()
    };
    let mut links = BTreeMap::new();
    fn walk<W: io::Write>(
        core: &OverlayCore,
        archive: &mut tar::Builder<W>,
        rel: &Path,
        links: &mut BTreeMap<(u64, u64), PathBuf>,
        profile: &MetadataProfile,
        copied: &BTreeMap<(u64, u64), Vec<PathBuf>>,
    ) -> anyhow::Result<()> {
        let resolved = core.metadata_resolved(rel)?;
        let (m, backing) = match profile {
            MetadataProfile::Container(_) => (resolved.metadata, resolved.resolved.path),
            MetadataProfile::LogicalImage => image_backing(core, rel, resolved, copied)?,
        };
        let (uid, gid, mode) = match profile {
            MetadataProfile::Container(identity) => identity(&backing, &m)?,
            MetadataProfile::LogicalImage => {
                let identity = pvisor_overlay_core::logical_identity(&backing, &m)?;
                (identity.uid, identity.gid, identity.mode)
            }
        };
        ensure!(
            matches!(mode & 0o170000, 0o100000 | 0o040000 | 0o120000),
            "unsupported image special file: {}",
            rel.display()
        );
        let mut header = tar::Header::new_gnu();
        header.set_uid(uid as u64);
        header.set_gid(gid as u64);
        header.set_mode(mode & 0o7777);
        header.set_mtime(m.mtime().max(0) as u64);
        let name = if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel
        };
        let mut pax = Vec::new();
        if matches!(profile, MetadataProfile::LogicalImage) {
            let (seconds, nanos) = match backend::attributes(&backing)? {
                Some(attr) => sys::unix_timestamp(attr.mtime),
                None => (m.mtime(), m.mtime_nsec()),
            };
            ensure!(
                seconds >= 0 && (0..1_000_000_000).contains(&nanos),
                "unsupported image timestamp"
            );
            header.set_mtime(seconds as u64);
            pax.push(("mtime".into(), format!("{seconds}.{nanos:09}").into_bytes()));
        }
        for key in sys::list_xattrs(&backing)? {
            ensure!(
                !matches!(profile, MetadataProfile::LogicalImage)
                    || key != pvisor_overlay_core::VIRTUAL_NODE_XATTR.as_bytes(),
                "virtual special nodes cannot be exported as ordinary image files"
            );
            if matches!(profile, MetadataProfile::LogicalImage)
                && (pvisor_overlay_core::is_private_metadata_xattr(OsStr::from_bytes(&key))
                    || pvisor_overlay_core::validate_guest_xattr(OsStr::from_bytes(&key)).is_err())
            {
                continue;
            }
            let key = OsStr::from_bytes(&key);
            let text = key.to_string_lossy();
            ensure!(
                text != "security.capability",
                "file capabilities cannot yet be safely exported"
            );
            let include = match profile {
                MetadataProfile::Container(_) => {
                    text.starts_with("user.")
                        && text != "user.containers.override_stat"
                        && !text.starts_with("user.overlay.")
                }
                MetadataProfile::LogicalImage => {
                    ensure!(
                        !pvisor_overlay_core::is_stored_overlay_xattr(key),
                        "stored overlay xattrs require namespace translation"
                    );
                    ensure!(key.to_str().is_some(), "non-UTF-8 image xattr name");
                    true
                }
            };
            if include {
                pax.push((
                    format!("SCHILY.xattr.{text}"),
                    sys::get_xattr(&backing, key)?,
                ));
            }
        }
        if !pax.is_empty() {
            archive.append_pax_extensions(pax.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        }
        let object = (m.dev(), m.ino());
        if matches!(profile, MetadataProfile::LogicalImage)
            && !m.is_dir()
            && links.contains_key(&object)
        {
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            archive.append_link(&mut header, name, &links[&object])?;
        } else if m.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            archive.append_data(&mut header, name, io::empty())?;
            for child in core.list_names(rel)? {
                walk(core, archive, &rel.join(child), links, profile, copied)?;
            }
        } else if m.file_type().is_symlink() {
            if matches!(profile, MetadataProfile::LogicalImage) {
                links.insert(object, rel.to_owned());
            }
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            archive.append_link(&mut header, name, fs::read_link(&backing)?)?;
        } else if m.is_file() {
            let identity = (m.dev(), m.ino());
            if m.nlink() > 1 && links.contains_key(&identity) {
                header.set_entry_type(tar::EntryType::Link);
                header.set_size(0);
                archive.append_link(&mut header, name, &links[&identity])?;
            } else {
                links.insert(identity, rel.to_path_buf());
                header.set_size(m.len());
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                archive.append_data(
                    &mut header,
                    name,
                    ImageReader {
                        path: backing.clone(),
                        offset: 0,
                        file: OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                            .open(&backing)?,
                    },
                )?;
            }
        } else {
            bail!("cannot export special image file: {}", rel.display());
        }
        Ok(())
    }
    walk(
        core,
        &mut archive,
        Path::new(""),
        &mut links,
        &profile,
        &copied,
    )?;
    archive.finish()?;
    Ok(())
}
