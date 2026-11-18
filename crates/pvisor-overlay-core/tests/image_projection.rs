//! Shared image projection behavior through Core, independent of a wire format.
use pvisor_overlay_core::{
    OverlayCore,
    backend::{self, FileAttr, FileType, ImageNode, ImageSource},
    sys,
};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

struct Source {
    policy: backend::SymlinkMetadataPolicy,
    nodes: BTreeMap<PathBuf, ImageNode>,
    reads: AtomicUsize,
    fail_after: AtomicUsize,
    metadata_error: AtomicBool,
}
impl Source {
    fn new() -> Arc<Self> {
        let node = |id, kind, size, nlink| {
            let time = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
            ImageNode {
                object_id: id,
                target: None,
                xattrs: BTreeMap::new(),
                attr: FileAttr {
                    ino: id,
                    size,
                    blocks: size.div_ceil(512),
                    atime: time,
                    mtime: time,
                    ctime: time,
                    crtime: time,
                    kind,
                    perm: if kind == FileType::Directory {
                        0o755
                    } else {
                        0o640
                    },
                    uid: unsafe { libc::geteuid() },
                    gid: unsafe { libc::getegid() },
                    nlink,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                },
            }
        };
        let mut file = node(2, FileType::RegularFile, 1024 * 1024 + 3, 2);
        file.xattrs
            .insert(b"user.example".to_vec(), b"exact metadata".to_vec());
        Arc::new(Self {
            policy: backend::SymlinkMetadataPolicy::Preserve,
            nodes: BTreeMap::from([
                (PathBuf::new(), node(1, FileType::Directory, 0, 3)),
                ("a".into(), file.clone()),
                ("alias".into(), file),
                ("nested".into(), node(3, FileType::Directory, 0, 2)),
                (
                    "nested/unvisited".into(),
                    node(4, FileType::RegularFile, 4, 1),
                ),
            ]),
            reads: AtomicUsize::new(0),
            fail_after: AtomicUsize::new(usize::MAX),
            metadata_error: AtomicBool::new(false),
        })
    }
}
impl ImageSource for Source {
    fn symlink_metadata_policy(&self) -> backend::SymlinkMetadataPolicy {
        self.policy
    }
    fn metadata(&self, relative: &Path) -> io::Result<ImageNode> {
        OverlayCore::validate_rel(relative)?;
        if self.metadata_error.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        self.nodes
            .get(relative)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
    fn entries(&self, relative: &Path) -> io::Result<Vec<OsString>> {
        if self.metadata(relative)?.attr.kind != FileType::Directory {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        Ok(self
            .nodes
            .keys()
            .filter(|p| !p.as_os_str().is_empty() && p.parent() == Some(relative))
            .map(|p| p.file_name().unwrap().to_owned())
            .collect())
    }
    fn read_at(&self, relative: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let node = self.metadata(relative)?;
        if self.reads.fetch_add(1, Ordering::SeqCst) >= self.fail_after.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(vec![
            42;
            node.attr.size.saturating_sub(offset).min(u64::from(size))
                as usize
        ])
    }
}
fn attach(root: &Path, source: Arc<Source>) -> backend::BackendAttachment {
    fs::create_dir(root).unwrap();
    backend::attach_image(root, &root.parent().unwrap().join("objects"), source).unwrap()
}

#[test]
fn lazy_projection_preserves_metadata_and_materializes_a_hardlink_group_once() {
    let temp = private_temp();
    let source = Source::new();
    let attachment = attach(&temp.path().join("lower"), source.clone());
    let root = attachment.root();
    assert!(!root.join("nested").exists());
    let a = backend::symlink_metadata(root.join("a")).unwrap();
    let alias = backend::symlink_metadata(root.join("alias")).unwrap();
    assert_eq!(a.ino(), alias.ino());
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        sys::get_xattr(&root.join("a"), OsStr::new("user.example")).unwrap(),
        b"exact metadata"
    );
    assert_eq!(
        backend::read_at(&root.join("a"), 0, 64).unwrap().unwrap(),
        [42; 64]
    );
    assert!(!root.join("nested").exists());
    let core = OverlayCore::new(vec![root.to_owned()], temp.path().join("upper"), None).unwrap();
    let first = core.copy_up(Path::new("a")).unwrap();
    let second = core.copy_up(Path::new("alias")).unwrap();
    assert_eq!(
        fs::metadata(&first).unwrap().ino(),
        fs::metadata(&second).unwrap().ino()
    );
    assert_eq!(
        sys::get_xattr(&first, OsStr::new("user.example")).unwrap(),
        b"exact metadata"
    );
    fs::write(&first, b"upper only").unwrap();
    assert_eq!(fs::read(second).unwrap(), b"upper only");
    assert_eq!(
        backend::read_at(&root.join("a"), 0, 10).unwrap().unwrap(),
        [42; 10]
    );
}

#[test]
fn full_materialization_includes_unvisited_files_and_retires_external_index_links() {
    let temp = private_temp();
    let source = Source::new();
    let attachment = attach(&temp.path().join("lower"), source.clone());
    let root = attachment.root().to_owned();
    backend::materialize_tree(&root).unwrap();
    assert_eq!(fs::metadata(root.join("a")).unwrap().nlink(), 2);
    assert_eq!(
        fs::metadata(root.join("a")).unwrap().ino(),
        fs::metadata(root.join("alias")).unwrap().ino()
    );
    assert_eq!(fs::read(root.join("nested/unvisited")).unwrap(), [42; 4]);
    let reads = source.reads.load(Ordering::SeqCst);
    drop(attachment);
    let attachment =
        backend::attach_image(&root, &temp.path().join("objects"), source.clone()).unwrap();
    backend::materialize_file(&root.join("a")).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), reads);
    drop(attachment);
    assert!(backend::attributes(&root).unwrap().is_none());
    drop(source);
    assert_eq!(fs::read(root.join("a")).unwrap(), vec![42; 1024 * 1024 + 3]);
}

#[test]
fn source_failure_does_not_publish_an_upper_or_complete_receipt_and_can_retry() {
    let temp = private_temp();
    let source = Source::new();
    source.fail_after.store(1, Ordering::SeqCst);
    let attachment = attach(&temp.path().join("lower"), source.clone());
    let core = OverlayCore::new(
        vec![attachment.root().to_owned()],
        temp.path().join("upper"),
        None,
    )
    .unwrap();
    assert_eq!(
        core.copy_up(Path::new("a")).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert!(!temp.path().join("upper/a").exists());
    assert!(!temp.path().join("objects/2.complete").exists());
    source.fail_after.store(usize::MAX, Ordering::SeqCst);
    let upper = core.copy_up(Path::new("a")).unwrap();
    assert_eq!(fs::read(upper).unwrap(), vec![42; 1024 * 1024 + 3]);
}

#[test]
fn separate_views_share_a_source_and_keep_private_writes_independent() {
    let temp = private_temp();
    let source = Source::new();
    let mut owners = Vec::new();
    let mut cores = Vec::new();
    for index in 0..2 {
        let owner = temp.path().join(index.to_string());
        fs::create_dir(&owner).unwrap();

        fs::set_permissions(&owner, fs::Permissions::from_mode(0o700)).unwrap();
        let attachment = attach(&owner.join("lower"), source.clone());
        cores.push(
            OverlayCore::new(
                vec![attachment.root().to_owned()],
                owner.join("upper"),
                None,
            )
            .unwrap(),
        );
        owners.push(attachment);
    }
    let first = cores[0].copy_up(Path::new("a")).unwrap();
    fs::write(first, b"first instance").unwrap();
    let second = cores[1].copy_up(Path::new("a")).unwrap();
    assert_eq!(fs::read(second).unwrap(), vec![42; 1024 * 1024 + 3]);
    assert_eq!(Arc::strong_count(&source), 3);
}

#[test]
fn conflicting_identity_or_unrepresentable_metadata_fail_without_registration() {
    for invalid_identity in [false, true] {
        let temp = private_temp();
        let mut source = Source::new();
        let node = Arc::get_mut(&mut source)
            .unwrap()
            .nodes
            .get_mut(Path::new(""))
            .unwrap();
        node.xattrs.insert(
            if invalid_identity {
                b"user.containers.override_stat".to_vec()
            } else {
                b"user.bad\0name".to_vec()
            },
            b"invalid".to_vec(),
        );
        let root = temp.path().join("lower");
        fs::create_dir(&root).unwrap();
        assert!(backend::attach_image(&root, &temp.path().join("objects"), source).is_err());
        assert!(backend::attributes(&root).unwrap().is_none());
    }
}

fn private_temp() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    temp
}

#[cfg(target_os = "linux")]
fn source_with_symlink() -> Arc<Source> {
    let mut source = Source::new();
    let nodes = &mut Arc::get_mut(&mut source).unwrap().nodes;
    let mut node = nodes[Path::new("nested/unvisited")].clone();
    node.object_id = 5;
    node.attr.ino = 5;
    node.attr.kind = FileType::Symlink;
    node.attr.perm = 0o777;
    node.attr.size = 1;
    node.target = Some(b"a".to_vec());
    nodes.insert("link".into(), node);
    source
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_reads_keep_foreign_identity_mode_and_exact_xattrs_without_following_target() {
    let temp = private_temp();
    let mut source = source_with_symlink();
    let node = Arc::get_mut(&mut source)
        .unwrap()
        .nodes
        .get_mut(Path::new("link"))
        .unwrap();
    node.attr.uid = node.attr.uid.wrapping_add(1);
    node.attr.gid = node.attr.gid.wrapping_add(1);
    node.attr.perm = 0o751;
    node.target = Some(b"/unvisited-outside-target".to_vec());
    node.xattrs = BTreeMap::from([
        (b"user.empty".to_vec(), vec![]),
        (b"user.binary".to_vec(), vec![0, 255, 7]),
    ]);
    let expected = node.clone();
    let attachment = attach(&temp.path().join("lower"), source.clone());
    let path = attachment.root().join("link");
    assert!(fs::symlink_metadata(&path).is_err());
    assert_eq!(
        sys::get_xattr(&path, OsStr::new("user.binary")).unwrap(),
        [0, 255, 7]
    );
    assert!(fs::symlink_metadata(&path).is_err());
    let core = OverlayCore::new(
        vec![attachment.root().to_owned()],
        temp.path().join("upper"),
        None,
    )
    .unwrap();
    let entry = core.metadata_resolved(Path::new("link")).unwrap();
    assert!(entry.metadata.file_type().is_symlink());
    assert_eq!(
        backend::read_link(&path).unwrap().as_os_str().as_bytes(),
        expected.target.as_ref().unwrap()
    );
    let logical = backend::attributes(&path).unwrap().unwrap();
    assert_eq!(
        (logical.uid, logical.gid, logical.perm),
        (expected.attr.uid, expected.attr.gid, 0o751)
    );
    assert_eq!(
        sys::list_xattrs(&path).unwrap(),
        expected.xattrs.keys().cloned().collect::<Vec<_>>()
    );
    for (name, value) in &expected.xattrs {
        assert_eq!(
            sys::get_xattr(&path, OsStr::from_bytes(name)).unwrap(),
            *value
        );
    }
    assert_eq!(
        sys::get_xattr(&path, OsStr::new("user.absent"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(
        sys::get_xattr(&path, OsStr::from_bytes(b"bad\0name"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
    let fingerprint =
        pvisor_overlay_core::fingerprint_at(attachment.root(), Path::new("link")).unwrap();
    let json = serde_json::to_value(fingerprint).unwrap();
    assert_eq!(json["uid"], expected.attr.uid);
    assert_eq!(json["gid"], expected.attr.gid);
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    // Dropping the source removes logical lookup, rather than writing the link
    // or its target with an ownership override or ordinary user attributes.
    drop(core);
    drop(attachment);
    assert!(sys::list_xattrs(&path).unwrap().is_empty());
    assert!(backend::attributes(&path).unwrap().is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn logical_symlink_attributes_survive_copy_up_rename_reopen_and_materialization() {
    for attribute in ["uid", "gid", "mode", "xattr"] {
        let temp = private_temp();
        let mut source = source_with_symlink();
        let node = Arc::get_mut(&mut source)
            .unwrap()
            .nodes
            .get_mut(Path::new("link"))
            .unwrap();
        match attribute {
            "uid" => node.attr.uid = node.attr.uid.wrapping_add(1),
            "gid" => node.attr.gid = node.attr.gid.wrapping_add(1),
            "mode" => node.attr.perm = 0o751,
            "xattr" => {
                node.xattrs
                    .insert(b"user.example".to_vec(), b"exact".to_vec());
            }
            _ => unreachable!(),
        }
        let attachment = attach(&temp.path().join("lower"), source);
        let root = attachment.root();
        let upper = temp.path().join("upper");
        let core = OverlayCore::new(vec![root.to_owned()], upper.clone(), None).unwrap();
        assert!(
            core.metadata(Path::new("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let before = pvisor_overlay_core::fingerprint_at(root, Path::new("link")).unwrap();
        {
            let copied = core.copy_up(Path::new("link")).unwrap();
            let expected = backend::attributes(&root.join("link")).unwrap().unwrap();
            let actual = backend::attributes(&copied).unwrap().unwrap();
            assert_eq!(
                (actual.uid, actual.gid, actual.perm, actual.nlink),
                (expected.uid, expected.gid, expected.perm, 1)
            );
            core.rename(Path::new("link"), Path::new("moved"), false)
                .unwrap();
            assert_eq!(
                pvisor_overlay_core::fingerprint_at(&upper, Path::new("moved")).unwrap(),
                before
            );
            drop(core);
            let core = OverlayCore::open_existing(
                vec![root.to_owned()],
                upper.clone(),
                None,
                vec![],
                None,
            )
            .unwrap();
            let moved = core.metadata_resolved(Path::new("moved")).unwrap();
            let actual = backend::attributes(&moved.resolved.path).unwrap().unwrap();
            assert_eq!(
                (actual.uid, actual.gid, actual.perm, actual.nlink),
                (expected.uid, expected.gid, expected.perm, 1)
            );
            assert!(
                !core
                    .list_names(Path::new(""))
                    .unwrap()
                    .iter()
                    .any(|name| name.to_string_lossy().contains("metadata-v1"))
            );
            drop(core);
            assert_eq!(
                pvisor_overlay_core::fingerprint_at(&upper, Path::new("moved")).unwrap(),
                before
            );
        }
        backend::materialize_tree(root).unwrap();
        let root = root.to_owned();
        drop(attachment);
        let _metadata = backend::metadata_owner(&root, false).unwrap();
        assert_eq!(
            pvisor_overlay_core::fingerprint_at(&root, Path::new("link")).unwrap(),
            before
        );
        assert!(backend::read_link(root.join("link")).is_ok());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_symlink_copy_up_retains_fingerprint_and_time_after_source_detaches() {
    let temp = private_temp();
    let source = source_with_symlink();
    let attachment = attach(&temp.path().join("lower"), source);
    let root = attachment.root();
    let before = pvisor_overlay_core::fingerprint_at(root, Path::new("link")).unwrap();
    let time = backend::attributes(&root.join("link"))
        .unwrap()
        .unwrap()
        .mtime;
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![root.to_owned()], upper.clone(), None).unwrap();
    let copy = core.copy_up(Path::new("link")).unwrap();
    assert_eq!(
        fs::symlink_metadata(copy).unwrap().modified().unwrap(),
        time
    );
    drop(core);
    drop(attachment);
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        before
    );
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_logical_xattr_errors_never_fall_back_to_native_empty_attributes() {
    let temp = private_temp();
    let source = source_with_symlink();
    let attachment = attach(&temp.path().join("lower"), source.clone());
    let path = attachment.root().join("link");
    backend::symlink_metadata(&path).unwrap();
    source.metadata_error.store(true, Ordering::SeqCst);
    assert_eq!(
        sys::list_xattrs(&path).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(
        sys::get_xattr(&path, OsStr::new("user.example"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    source.metadata_error.store(false, Ordering::SeqCst);
    assert!(sys::list_xattrs(&path).unwrap().is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_symlink_override_stays_internal_and_native_copy_keeps_its_fingerprint() {
    let temp = private_temp();
    let mut source = source_with_symlink();
    let node = Arc::get_mut(&mut source)
        .unwrap()
        .nodes
        .get_mut(Path::new("link"))
        .unwrap();
    node.xattrs.insert(
        b"user.containers.override_stat".to_vec(),
        format!("{}:{}:0120777", node.attr.uid, node.attr.gid).into_bytes(),
    );
    let attachment = attach(&temp.path().join("lower"), source);
    let root = attachment.root();
    let before = pvisor_overlay_core::fingerprint_at(root, Path::new("link")).unwrap();
    assert!(sys::list_xattrs(&root.join("link")).unwrap().is_empty());
    assert_eq!(
        sys::get_xattr(
            &root.join("link"),
            OsStr::new("user.containers.override_stat")
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::ENODATA)
    );
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![root.to_owned()], upper.clone(), None).unwrap();
    core.copy_up(Path::new("link")).unwrap();
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        before
    );
}

#[cfg(target_os = "linux")]
fn foreign_symlink_source() -> Arc<Source> {
    let mut source = source_with_symlink();
    let node = Arc::get_mut(&mut source)
        .unwrap()
        .nodes
        .get_mut(Path::new("link"))
        .unwrap();
    node.attr.uid = node.attr.uid.wrapping_add(1);
    node.attr.gid = node.attr.gid.wrapping_add(1);
    node.attr.perm = 0o751;
    node.xattrs.insert(b"user.binary".to_vec(), vec![0, 255, 7]);
    source
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_aliases_keep_updated_metadata_in_copy_up_and_independent_upper_snapshots() {
    let temp = private_temp();
    let mut source = foreign_symlink_source();
    let nodes = &mut Arc::get_mut(&mut source).unwrap().nodes;
    nodes.get_mut(Path::new("link")).unwrap().attr.nlink = 2;
    nodes.insert("link-alias".into(), nodes[Path::new("link")].clone());
    let attachment = attach(&temp.path().join("lower"), source);
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![attachment.root().to_owned()], upper.clone(), None).unwrap();
    let original =
        pvisor_overlay_core::fingerprint_at(attachment.root(), Path::new("link")).unwrap();
    let first = core.copy_up(Path::new("link")).unwrap();
    assert!(sys::set_symlink_attributes(&first, Some(3456), None, Some(0o740)).unwrap());
    sys::set_xattr(&first, OsStr::new("user.binary"), b"updated", 0).unwrap();
    let alias = core.copy_up(Path::new("link-alias")).unwrap();
    assert_eq!(
        fs::symlink_metadata(&first).unwrap().ino(),
        fs::symlink_metadata(&alias).unwrap().ino()
    );
    let expected = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
    assert_ne!(expected, original);
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link-alias")).unwrap(),
        expected
    );
    assert_eq!(backend::attributes(&alias).unwrap().unwrap().nlink, 2);
    let links = core.capture_hard_links().unwrap();
    let sources = core.capture_hard_link_sources().unwrap();
    core.restore_hard_links(&links).unwrap();
    core.restore_hard_link_sources(&sources).unwrap();
    core.rename(Path::new("link"), Path::new("moved"), false)
        .unwrap();
    let snapshot = temp.path().join("snapshot");
    pvisor_overlay_core::apply::restore_overlay_upper(&upper, &snapshot).unwrap();
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&snapshot, Path::new("moved")).unwrap(),
        expected
    );
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&snapshot, Path::new("link-alias")).unwrap(),
        expected
    );
    assert_ne!(
        fs::symlink_metadata(snapshot.join("moved")).unwrap().ino(),
        fs::symlink_metadata(&alias).unwrap().ino()
    );
    assert_eq!(
        fs::symlink_metadata(snapshot.join("moved")).unwrap().ino(),
        fs::symlink_metadata(snapshot.join("link-alias"))
            .unwrap()
            .ino()
    );
    drop(core);
    drop(attachment);
    fs::remove_dir_all(&upper).unwrap();
    let _owner = backend::metadata_owner(&snapshot, false).unwrap();
    let attr = backend::attributes(&snapshot.join("moved"))
        .unwrap()
        .unwrap();
    assert_eq!((attr.uid, attr.perm, attr.nlink), (3456, 0o740, 2));
    assert_eq!(
        sys::get_xattr(&snapshot.join("link-alias"), OsStr::new("user.binary")).unwrap(),
        b"updated"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_state_follows_inode_across_work_and_directory_renames_not_reused_names() {
    let temp = private_temp();
    let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
    let root = attachment.root().to_owned();
    let upper = temp.path().join("upper");
    let work = temp.path().join("work");
    let core = OverlayCore::new(vec![root.clone()], upper.clone(), Some(work.clone())).unwrap();
    assert!(fs::read_dir(&upper).unwrap().next().is_none());
    assert!(fs::read_dir(&work).unwrap().next().is_none());
    let before = pvisor_overlay_core::fingerprint_at(&root, Path::new("link")).unwrap();
    let old = core.copy_up(Path::new("link")).unwrap();
    let old_inode = fs::symlink_metadata(&old).unwrap().ino();
    assert!(fs::read_dir(&work).unwrap().next().is_none());
    core.create_dir(Path::new("directory"), 0o755).unwrap();
    core.rename(Path::new("link"), Path::new("directory/link"), false)
        .unwrap();
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("directory/link")).unwrap(),
        before
    );
    core.remove(Path::new("directory/link"), false).unwrap();
    core.create_symlink(Path::new("directory/link"), Path::new("new-target"))
        .unwrap();
    let new = upper.join("directory/link");
    assert_ne!(
        fs::symlink_metadata(&new).unwrap().ino(),
        old_inode,
        "internal anchor must prevent reuse"
    );
    assert!(backend::attributes(&new).unwrap().is_none());
    assert!(sys::list_xattrs(&new).unwrap().is_empty());
    assert_eq!(fs::read_link(&new).unwrap(), Path::new("new-target"));
    drop(core);
    let reopened =
        OverlayCore::open_existing(vec![root], upper.clone(), Some(work), vec![], None).unwrap();
    assert!(
        backend::attributes(
            &reopened
                .metadata_resolved(Path::new("directory/link"))
                .unwrap()
                .resolved
                .path
        )
        .unwrap()
        .is_none()
    );
    drop(reopened);
}

#[cfg(target_os = "linux")]
#[test]
fn corrupt_symlink_state_and_rebound_metadata_namespace_fail_instead_of_falling_back() {
    for fault in [
        "json",
        "missing-record",
        "missing-anchor",
        "record-symlink",
        "directory",
        "invalid-ctime",
    ] {
        let temp = private_temp();
        let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
        let upper = temp.path().join("upper");
        let core =
            OverlayCore::new(vec![attachment.root().to_owned()], upper.clone(), None).unwrap();
        let path = core.copy_up(Path::new("link")).unwrap();
        let m = fs::symlink_metadata(&path).unwrap();
        let directory = upper.join(".wh..pvisor-symlink-metadata-v1");
        let key = format!("{:x}-{:x}", m.dev(), m.ino());
        let record = directory.join(format!("{key}.json"));
        let anchor = directory.join(format!("{key}.link"));
        match fault {
            "json" => fs::write(&record, b"{invalid").unwrap(),
            "invalid-ctime" => {
                let mut data: serde_json::Value =
                    serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
                data["ctime"] = serde_json::json!([0, 1_000_000_000]);
                fs::write(&record, serde_json::to_vec(&data).unwrap()).unwrap();
            }
            "missing-record" => fs::remove_file(&record).unwrap(),
            "missing-anchor" => fs::remove_file(&anchor).unwrap(),
            "record-symlink" => {
                fs::remove_file(&record).unwrap();
                let outside = temp.path().join("outside");
                fs::write(&outside, b"not an authorized record").unwrap();
                std::os::unix::fs::symlink(outside, record).unwrap();
            }
            "directory" => {
                fs::rename(&directory, temp.path().join("old-metadata")).unwrap();
                fs::create_dir(&directory).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(backend::attributes(&path).is_err(), "{fault}");
        assert!(sys::list_xattrs(&path).is_err(), "{fault}");
        assert!(sys::chown(&path, 1, 2, true).is_err(), "{fault}");
        assert!(
            sys::set_xattr(&path, OsStr::new("user.binary"), b"bad", 0).is_err(),
            "{fault}"
        );
        assert!(
            sys::remove_xattr(&path, OsStr::new("user.binary")).is_err(),
            "{fault}"
        );
        assert!(
            pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).is_err(),
            "{fault}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn logical_setters_survive_reopen_without_changing_native_ownership_or_link_target() {
    let temp = private_temp();
    let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![attachment.root().to_owned()], upper.clone(), None).unwrap();
    let path = core.copy_up(Path::new("link")).unwrap();
    let before = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
    let native_before = fs::symlink_metadata(&path).unwrap();
    fs::write(upper.join("a"), "unchanged target").unwrap();
    let target_before = fs::metadata(upper.join("a")).unwrap();
    let time_before = std::time::SystemTime::now();
    sys::chown(&path, 3456, u32::MAX, true).unwrap();
    sys::chown(&path, u32::MAX, 4567, true).unwrap();
    sys::set_xattr(
        &path,
        OsStr::new("user.binary"),
        &[255, 0, 128],
        libc::XATTR_REPLACE,
    )
    .unwrap();
    assert_eq!(
        sys::get_xattr(&path, OsStr::new("user.binary")).unwrap(),
        [255, 0, 128]
    );
    sys::set_xattr(
        &path,
        OsStr::from_bytes(b"user.\xff"),
        b"",
        libc::XATTR_CREATE,
    )
    .unwrap();
    sys::remove_xattr(&path, OsStr::new("user.binary")).unwrap();
    assert_ne!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        before
    );
    let expected = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(attachment.root(), Path::new("link")).unwrap(),
        before
    );
    let updated = backend::attributes(&path).unwrap().unwrap();
    assert_eq!(
        (updated.uid, updated.gid, updated.perm, updated.nlink),
        (3456, 4567, 0o751, 1)
    );
    assert!(updated.ctime >= time_before);
    let native_after = fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (native_after.uid(), native_after.gid(), native_after.ino()),
        (
            native_before.uid(),
            native_before.gid(),
            native_before.ino()
        )
    );
    let target_after = fs::metadata(upper.join("a")).unwrap();
    assert_eq!(
        (
            target_after.uid(),
            target_after.gid(),
            target_after.ctime(),
            target_after.ctime_nsec()
        ),
        (
            target_before.uid(),
            target_before.gid(),
            target_before.ctime(),
            target_before.ctime_nsec()
        )
    );
    assert_eq!(
        fs::read_to_string(upper.join("a")).unwrap(),
        "unchanged target"
    );
    drop(core);
    drop(attachment);
    // No image reader or old metadata owner remains: reopen ordinary backing.
    let lower = temp.path().join("empty");
    fs::create_dir(&lower).unwrap();
    let reopened =
        OverlayCore::open_existing(vec![lower], upper.clone(), None, vec![], None).unwrap();
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        expected
    );
    assert_eq!(
        backend::attributes(&path).unwrap().unwrap().ctime,
        updated.ctime
    );
    assert_eq!(
        sys::get_xattr(&path, OsStr::from_bytes(b"user.\xff")).unwrap(),
        b""
    );
    assert_eq!(
        sys::get_xattr(&path, OsStr::new("user.binary"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(fs::read_link(&path).unwrap(), Path::new("a"));
    drop(reopened);
    // A cold metadata copy reconstructs binding in the independently owned parent.
    let destination = temp.path().join("destination");
    std::os::unix::fs::symlink("original", &destination).unwrap();
    pvisor_overlay_core::apply::copy_host_metadata(&path, &destination).unwrap();
    assert_eq!(fs::read_link(&destination).unwrap(), Path::new("original"));
    let _metadata = backend::metadata_owner(temp.path(), false).unwrap();
    let attr = backend::attributes(&destination).unwrap().unwrap();
    assert_eq!((attr.uid, attr.gid, attr.perm), (3456, 4567, 0o751));
    assert_eq!(
        sys::get_xattr(&destination, OsStr::from_bytes(b"user.\xff")).unwrap(),
        b""
    );
}

#[cfg(target_os = "linux")]
#[test]
fn logical_xattr_flags_match_native_and_failures_leave_the_record_unchanged() {
    let temp = private_temp();
    let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![attachment.root().to_owned()], upper.clone(), None).unwrap();
    let path = core.copy_up(Path::new("link")).unwrap();
    let oracle = temp.path().join("native");
    fs::write(&oracle, b"").unwrap();
    sys::set_xattr(&oracle, OsStr::new("user.binary"), &[0, 255, 7], 0).unwrap();
    for (name, flags) in [
        ("user.binary", libc::XATTR_CREATE),
        ("user.absent", libc::XATTR_REPLACE),
        ("user.binary", libc::XATTR_CREATE | libc::XATTR_REPLACE),
        ("user.absent", libc::XATTR_CREATE | libc::XATTR_REPLACE),
        ("user.binary", 4),
        ("", 0),
    ] {
        let old = backend::attributes(&path).unwrap().unwrap();
        let before = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
        let native = sys::set_xattr(&oracle, OsStr::new(name), b"new", flags)
            .unwrap_err()
            .raw_os_error();
        assert_eq!(
            sys::set_xattr(&path, OsStr::new(name), b"new", flags)
                .unwrap_err()
                .raw_os_error(),
            native,
            "{name:?}/{flags}"
        );
        assert_eq!(
            pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
            before
        );
        assert_eq!(
            backend::attributes(&path).unwrap().unwrap().ctime,
            old.ctime
        );
    }
    let before = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
    for (name, value, errno) in [
        (
            OsStr::from_bytes(b"user.bad\0name"),
            b"value".to_vec(),
            libc::EINVAL,
        ),
        (
            OsStr::new("user.containers.override_stat"),
            b"1:2:777".to_vec(),
            libc::EPERM,
        ),
        (OsStr::new("user.large"), vec![0; 65537], libc::E2BIG),
        (
            OsStr::new(&"n".repeat(256)),
            b"value".to_vec(),
            libc::ERANGE,
        ),
    ] {
        assert_eq!(
            sys::set_xattr(&path, name, &value, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(errno)
        );
    }
    assert_eq!(
        sys::remove_xattr(&path, OsStr::new("user.absent"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(
        sys::remove_xattr(&path, OsStr::new("user.containers.override_stat"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        before
    );
}

#[cfg(target_os = "linux")]
#[test]
fn logical_mutations_through_aliases_do_not_lose_concurrent_updates_and_readonly_owners_refuse_writes()
 {
    let temp = private_temp();
    let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![attachment.root().to_owned()], upper.clone(), None).unwrap();
    let path = core.copy_up(Path::new("link")).unwrap();
    let alias = upper.join("alias");
    fs::hard_link(&path, &alias).unwrap();
    let gate = std::sync::Barrier::new(10);
    std::thread::scope(|scope| {
        for i in 0..10 {
            let (gate, path) = (&gate, if i % 2 == 0 { &path } else { &alias });
            scope.spawn(move || {
                gate.wait();
                if i == 8 {
                    sys::chown(path, 6789, u32::MAX, true).unwrap();
                } else if i == 9 {
                    sys::chown(path, u32::MAX, 7890, true).unwrap();
                } else {
                    sys::set_xattr(
                        path,
                        OsStr::new(&format!("user.concurrent{i}")),
                        &[i],
                        libc::XATTR_CREATE,
                    )
                    .unwrap();
                }
            });
        }
    });
    for name in [&path, &alias] {
        let attr = backend::attributes(name).unwrap().unwrap();
        assert_eq!((attr.uid, attr.gid, attr.nlink), (6789, 7890, 2));
        for i in 0..8 {
            assert_eq!(
                sys::get_xattr(name, OsStr::new(&format!("user.concurrent{i}"))).unwrap(),
                [i]
            );
        }
    }
    drop(core);
    // Admit this persisted tree as a readonly lower instead of as a writable upper.
    let _reader =
        OverlayCore::new(vec![upper.clone()], temp.path().join("other-upper"), None).unwrap();
    let before = pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap();
    for result in [
        sys::chown(&alias, 1, 2, true),
        sys::set_xattr(&path, OsStr::new("user.binary"), b"bad", 0),
        sys::remove_xattr(&alias, OsStr::new("user.binary")),
    ] {
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EROFS));
    }
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&upper, Path::new("link")).unwrap(),
        before
    );
}

#[cfg(target_os = "linux")]
#[test]
fn native_copy_admission_and_reopen_reject_untranslated_inode_bound_state() {
    let temp = private_temp();
    let attachment = attach(&temp.path().join("lower"), foreign_symlink_source());
    let upper = temp.path().join("upper");
    let alias = temp.path().join("upper-alias");
    fs::create_dir(&upper).unwrap();
    std::os::unix::fs::symlink(&upper, &alias).unwrap();
    let core = OverlayCore::new(vec![attachment.root().to_owned()], alias.clone(), None).unwrap();
    core.copy_up(Path::new("link")).unwrap();
    let control = ".wh..pvisor-symlink-metadata-v1";
    assert_eq!(
        backend::validate_native_copy_entry(&alias.join(control))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EOPNOTSUPP)
    );
    let clone = temp.path().join("clone");
    fs::create_dir(&clone).unwrap();
    fs::create_dir(clone.join(control)).unwrap();
    fs::set_permissions(clone.join(control), fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink("a", clone.join("link")).unwrap();
    for entry in fs::read_dir(upper.join(control)).unwrap() {
        let entry = entry.unwrap();
        let target = clone.join(control).join(entry.file_name());
        if entry.file_type().unwrap().is_symlink() {
            fs::hard_link(clone.join("link"), target).unwrap();
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
    drop(core);
    assert_eq!(
        OverlayCore::open_existing(
            vec![attachment.root().to_owned()],
            clone,
            None,
            vec![],
            None
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::EIO)
    );
    // Moving the whole original root retains its native identity and is valid.
    let moved = temp.path().join("moved-root");
    fs::rename(&upper, &moved).unwrap();
    let core = OverlayCore::open_existing(
        vec![attachment.root().to_owned()],
        moved.clone(),
        None,
        vec![],
        None,
    )
    .unwrap();
    assert_eq!(
        backend::attributes(&moved.join("link"))
            .unwrap()
            .unwrap()
            .perm,
        0o751
    );
    drop(core);
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_native_symlink_profile_retains_best_effort_copy_and_materialization() {
    let temp = private_temp();
    let mut source = source_with_symlink();
    let source_data = Arc::get_mut(&mut source).unwrap();
    source_data.policy = backend::SymlinkMetadataPolicy::LegacyNative;
    let node = source_data.nodes.get_mut(Path::new("link")).unwrap();
    node.attr.uid = node.attr.uid.wrapping_add(1);
    let attachment = attach(&temp.path().join("lower"), source);
    let root = attachment.root();
    let upper = temp.path().join("upper");
    let core = OverlayCore::new(vec![root.to_owned()], upper.clone(), None).unwrap();
    let copied = core.copy_up(Path::new("link")).unwrap();
    assert!(backend::attributes(&copied).unwrap().is_none());
    assert_eq!(fs::symlink_metadata(&copied).unwrap().nlink(), 1);
    assert!(!upper.join(".wh..pvisor-symlink-metadata-v1").exists());
    backend::materialize_tree(root).unwrap();
    assert_eq!(fs::read_link(root.join("link")).unwrap(), Path::new("a"));
}

#[test]
fn bounded_hints_preserve_aliases_receipts_copy_up_and_owned_export() {
    let temp = private_temp();
    let source = Source::new();
    let root = temp.path().join("lower");
    fs::create_dir(&root).unwrap();
    let objects = temp.path().join("objects");
    for invalid in [0, 16385] {
        assert!(backend::attach_image_bounded(&root, &objects, source.clone(), invalid).is_err());
        assert!(backend::attributes(&root).unwrap().is_none());
    }
    let attachment = backend::attach_image_bounded(&root, &objects, source.clone(), 1).unwrap();
    for _ in 0..3 {
        backend::symlink_metadata(root.join("a")).unwrap();
        backend::symlink_metadata(root.join("nested/unvisited")).unwrap();
        backend::symlink_metadata(root.join("alias")).unwrap();
        assert_eq!(
            fs::metadata(root.join("a")).unwrap().ino(),
            fs::metadata(root.join("alias")).unwrap().ino()
        );
    }
    backend::materialize_file(&root.join("a")).unwrap();
    let inode = fs::metadata(root.join("a")).unwrap().ino();
    backend::materialize_file(&root.join("nested/unvisited")).unwrap();
    let reads = source.reads.load(Ordering::SeqCst);
    backend::materialize_file(&root.join("alias")).unwrap();
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        reads,
        "durable completion survives hint eviction"
    );
    assert_eq!(fs::metadata(root.join("a")).unwrap().ino(), inode);
    let core = OverlayCore::new(vec![root.clone()], temp.path().join("upper"), None).unwrap();
    let copied = core.copy_up(Path::new("a")).unwrap();
    fs::write(copied, b"private writes").unwrap();
    assert_eq!(
        backend::read_at(&root.join("alias"), 0, 4)
            .unwrap()
            .unwrap(),
        [42; 4]
    );
    backend::materialize_tree(&root).unwrap();
    drop(core);
    drop(attachment);
    drop(source);
    assert_eq!(fs::read(root.join("nested/unvisited")).unwrap(), [42; 4]);
    assert_eq!(fs::metadata(root.join("a")).unwrap().nlink(), 2);
}
