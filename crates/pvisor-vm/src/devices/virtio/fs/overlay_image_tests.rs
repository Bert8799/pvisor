// Logical image metadata through real virtio-fs request handlers, without KVM.
use pvisor_overlay_core::backend::{FileAttr, FileType, ImageNode, ImageSource};
use std::{collections::BTreeMap, time::UNIX_EPOCH};

struct Image;
impl ImageSource for Image {
    fn metadata(&self, path: &Path) -> io::Result<ImageNode> {
        let (id, kind, links, perm) = match path.to_str() {
            Some("") => (1, FileType::Directory, 2, 0o755),
            Some("link" | "alias") => (2, FileType::Symlink, 2, 0o751),
            Some("target") => (3, FileType::RegularFile, 1, 0o644),
            _ => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
        };
        let time = UNIX_EPOCH + Duration::from_secs(1000);
        Ok(ImageNode {
            object_id: id,
            attr: FileAttr {
                ino: id,
                size: 6,
                blocks: 1,
                atime: time,
                mtime: time,
                ctime: time,
                crtime: time,
                kind,
                perm,
                uid: 1234,
                gid: 2345,
                nlink: links,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            },
            target: (kind == FileType::Symlink).then(|| b"target".to_vec()),
            xattrs: if kind == FileType::Symlink {
                BTreeMap::from([(b"user.binary".to_vec(), vec![0, 255, 7])])
            } else {
                BTreeMap::new()
            },
        })
    }
    fn entries(&self, path: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        if path.as_os_str().is_empty() {
            Ok(["link", "alias", "target"]
                .into_iter()
                .map(Into::into)
                .collect())
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTDIR))
        }
    }
    fn read_at(&self, _: &Path, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        Ok(b"target"
            .get(offset as usize..)
            .unwrap_or_default()
            .iter()
            .copied()
            .take(size as usize)
            .collect())
    }
}

#[test]
fn simplified_profile_rejects_logical_sources_before_creating_upper_backing() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lower = temp.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let _attachment =
        backend::attach_image(&lower, &temp.path().join("objects"), Arc::new(Image)).unwrap();
    let fs = fixture(temp.path());
    let mut config = fs.snapshot_config.clone();
    drop(fs);
    std::fs::remove_dir_all(temp.path().join("upper")).unwrap();
    config.semantics = passthrough::PermissionSemantics::LinuxSimplified;
    assert_eq!(
        OverlayFs::new(config, Arc::new(InodeAllocator::new()))
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!temp.path().join("upper").exists());
}

fn fixture(root: &Path) -> OverlayFs {
    let fs = OverlayFs::new(
        Config {
            lower_dirs: vec![root.join("lower").to_string_lossy().into_owned()],
            lower_mutability: vec![pvisor_overlay_core::LayerMutability::Mutable],
            apply_target: None,
            baseline_lower: None,
            baseline_content_index: None,
            upper_dir: root.join("upper").to_string_lossy().into_owned(),
            work_dir: None,
            preimage_dir: None,
            excluded_paths: vec![],
            access_policy: Default::default(),
            semantics: passthrough::PermissionSemantics::LinuxComplete,
        },
        Arc::new(InodeAllocator::new()),
    )
    .unwrap();
    fs.init(FsOptions::empty()).unwrap();
    fs
}

#[test]
fn symlink_requests_preserve_logical_attributes_aliases_and_targets_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lower = temp.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let attachment =
        backend::attach_image(&lower, &temp.path().join("objects"), Arc::new(Image)).unwrap();
    let fs = fixture(temp.path());
    let ctx = root_context();
    let first = fs.lookup(ctx, fuse::ROOT_ID, c"link").unwrap();
    let alias = fs.lookup(ctx, fuse::ROOT_ID, c"alias").unwrap();
    assert_eq!(first.inode, alias.inode);
    assert_eq!(
        (first.attr.st_uid, first.attr.st_gid, first.attr.st_nlink),
        (1234, 2345, 2)
    );
    assert!(matches!(
        fs.getxattr(ctx, first.inode, c"user.binary", 0).unwrap(),
        GetxattrReply::Count(3)
    ));
    assert_eq!(
        fs.getxattr(ctx, first.inode, c"user.binary", 2)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::ERANGE)
    );
    assert!(
        matches!(fs.getxattr(ctx,first.inode,c"user.binary",3).unwrap(),GetxattrReply::Value(v) if v==[0,255,7])
    );
    assert!(matches!(
        fs.listxattr(ctx, first.inode, 0).unwrap(),
        ListxattrReply::Count(12)
    ));
    assert!(
        matches!(fs.listxattr(ctx,first.inode,12).unwrap(),ListxattrReply::Names(v) if v==b"user.binary\0")
    );
    assert_eq!(
        fs.listxattr(ctx, first.inode, 1)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::ERANGE)
    );
    let original = pvisor_overlay_core::fingerprint_at(&lower, Path::new("link")).unwrap();
    backend::prepare_metadata(&lower.join("target")).unwrap();
    let target_before = std::fs::symlink_metadata(lower.join("target")).unwrap();
    fs.setxattr(
        ctx,
        first.inode,
        c"user.binary",
        b"changed",
        libc::XATTR_REPLACE as u32,
    )
    .unwrap();
    assert!(
        matches!(fs.getxattr(ctx,alias.inode,c"user.binary",7).unwrap(),GetxattrReply::Value(v) if v==b"changed")
    );
    assert_eq!(
        fs.setxattr(
            ctx,
            first.inode,
            c"user.binary",
            b"bad",
            libc::XATTR_CREATE as u32
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::EEXIST)
    );
    assert_eq!(
        fs.setxattr(
            ctx,
            first.inode,
            c"user.containers.override_stat",
            b"1:2:777",
            0
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::EPERM)
    );
    let mut requested = first.attr;
    requested.st_uid = 3456;
    requested.st_mode = libc::S_IFLNK | 0o740;
    let changed = fs
        .setattr(
            ctx,
            first.inode,
            requested,
            None,
            SetattrValid::UID | SetattrValid::MODE,
        )
        .unwrap()
        .0;
    assert_eq!(
        (changed.st_uid, changed.st_gid, changed.st_mode & 0o7777),
        (3456, 2345, 0o740)
    );
    assert_eq!(fs.readlink(ctx, alias.inode).unwrap(), b"target");
    let before_now = std::time::SystemTime::now();
    let changed = fs
        .setattr(
            ctx,
            first.inode,
            requested,
            None,
            SetattrValid::MTIME | SetattrValid::MTIME_NOW,
        )
        .unwrap()
        .0;
    assert!(
        pvisor_overlay_core::sys::unix_time(changed.st_mtime, changed.st_mtime_nsec) >= before_now
    );
    fs.removexattr(ctx, alias.inode, c"user.binary").unwrap();
    assert_eq!(
        fs.getxattr(ctx, first.inode, c"user.binary", 0)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&lower, Path::new("link")).unwrap(),
        original
    );
    let target_after = std::fs::symlink_metadata(lower.join("target")).unwrap();
    assert_eq!(
        (
            target_before.uid(),
            target_before.gid(),
            target_before.mode(),
            target_before.ctime_nsec()
        ),
        (
            target_after.uid(),
            target_after.gid(),
            target_after.mode(),
            target_after.ctime_nsec()
        )
    );
    // Both aliases are copied before tearing down the immutable source.
    fs.core.copy_up(Path::new("link")).unwrap();
    fs.core.copy_up(Path::new("alias")).unwrap();
    let upper = temp.path().join("upper");
    assert_eq!(
        std::fs::symlink_metadata(upper.join("link")).unwrap().ino(),
        std::fs::symlink_metadata(upper.join("alias"))
            .unwrap()
            .ino()
    );
    let saved = fs.capture_state().unwrap();
    let config = fs.snapshot_config.clone();
    let allocator = fs.inode_alloc.clone();
    drop(fs);
    let fs = OverlayFs::open_existing(config, allocator).unwrap();
    fs.restore_state(&saved).unwrap();
    let restored = fs.lookup(ctx, fuse::ROOT_ID, c"alias").unwrap();
    assert_eq!(
        (
            restored.attr.st_uid,
            restored.attr.st_gid,
            restored.attr.st_mode & 0o7777
        ),
        (3456, 2345, 0o740)
    );
    assert_eq!(fs.readlink(ctx, restored.inode).unwrap(), b"target");
    drop(fs);
    drop(attachment);
    std::fs::remove_dir_all(&lower).unwrap();
    std::fs::create_dir(&lower).unwrap();
    let fs = fixture(temp.path());
    let first = fs.lookup(ctx, fuse::ROOT_ID, c"link").unwrap();
    let alias = fs.lookup(ctx, fuse::ROOT_ID, c"alias").unwrap();
    assert_eq!(first.inode, alias.inode);
    assert_eq!(
        (
            first.attr.st_uid,
            first.attr.st_gid,
            first.attr.st_mode & 0o7777,
            first.attr.st_nlink
        ),
        (3456, 2345, 0o740, 2)
    );
    assert_eq!(
        fs.getxattr(ctx, first.inode, c"user.binary", 0)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(fs.readlink(ctx, alias.inode).unwrap(), b"target");
}

#[test]
fn virtual_symlink_creation_retains_guest_root_and_setgid_parent() {
    let temp = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(temp.path().join("lower")).unwrap();
    let _attachment = backend::attach_image(
        &temp.path().join("lower"),
        &temp.path().join("objects"),
        Arc::new(Image),
    )
    .unwrap();
    let fs = fixture(temp.path());
    let denied = Context {
        uid: 4567,
        gid: 5678,
        pid: 1,
    };
    assert_eq!(
        fs.symlink(
            denied,
            c"target",
            fuse::ROOT_ID,
            c"denied",
            Extensions::default()
        )
        .err()
        .unwrap()
        .raw_os_error(),
        Some(libc::EPERM)
    );
    let ctx = root_context();
    let link = fs
        .symlink(
            ctx,
            c"target",
            fuse::ROOT_ID,
            c"created",
            Extensions::default(),
        )
        .unwrap();
    assert_eq!((link.attr.st_uid, link.attr.st_gid), (0, 0));
    assert_eq!(fs.readlink(ctx, link.inode).unwrap(), b"target");
    pvisor_overlay_core::sys::set_xattr(
        &fs.core.upper_path(Path::new("")),
        OsStr::new("user.containers.override_stat"),
        b"1234:6789:042755",
        0,
    )
    .unwrap();
    let link = fs
        .symlink(
            ctx,
            c"target",
            fuse::ROOT_ID,
            c"inherited",
            Extensions::default(),
        )
        .unwrap();
    assert_eq!((link.attr.st_uid, link.attr.st_gid), (0, 6789));
}

#[rstest::rstest]
#[case::guest_root(0, 0, 3456, 4567, SetattrValid::UID | SetattrValid::GID, Some((3456, 4567)))]
#[case::kernel_authorized(u32::MAX, u32::MAX, 3456, 4567, SetattrValid::UID | SetattrValid::GID, Some((3456, 4567)))]
#[case::owner_keeps_uid(1234, 2345, 1234, 9999, SetattrValid::UID, Some((1234, 2345)))]
#[case::owner_primary_group(1234, 3456, 9999, 3456, SetattrValid::GID, Some((1234, 3456)))]
#[case::owner_retains_group(1234, 3456, 9999, 2345, SetattrValid::GID, Some((1234, 2345)))]
#[case::non_owner(9876, 2345, 1234, 2345, SetattrValid::UID, None)]
#[case::owner_cannot_change_uid(1234, 2345, 3456, 2345, SetattrValid::UID | SetattrValid::MODE, None)]
#[case::owner_unrelated_group(1234, 2345, 1234, 3456, SetattrValid::GID, None)]
#[case::partial_kernel_credentials(u32::MAX, 2345, 3456, 2345, SetattrValid::UID, None)]
fn logical_symlink_owner_changes_use_the_private_metadata_authorization(
    #[case] uid: u32,
    #[case] gid: u32,
    #[case] requested_uid: u32,
    #[case] requested_gid: u32,
    #[case] valid: SetattrValid,
    #[case] expected: Option<(u32, u32)>,
) {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lower = temp.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let _attachment =
        backend::attach_image(&lower, &temp.path().join("objects"), Arc::new(Image)).unwrap();
    let fs = fixture(temp.path());
    let root = root_context();
    let entry = fs.lookup(root, fuse::ROOT_ID, c"link").unwrap();
    let original = pvisor_overlay_core::fingerprint_at(&lower, Path::new("link")).unwrap();
    let mut requested = entry.attr;
    requested.st_uid = requested_uid;
    requested.st_gid = requested_gid;
    requested.st_mode = libc::S_IFLNK | 0o700;
    let result = fs.setattr(
        Context { uid, gid, pid: 1 },
        entry.inode,
        requested,
        None,
        valid,
    );
    match expected {
        Some((uid, gid)) => {
            let changed = result.unwrap().0;
            assert_eq!(
                (changed.st_uid, changed.st_gid, changed.st_mode & 0o7777),
                (uid, gid, 0o751)
            );
        }
        None => assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPERM)),
    }
    let actual = fs.getattr(root, entry.inode, None).unwrap().0;
    let (uid, gid) = expected.unwrap_or((1234, 2345));
    assert_eq!(
        (actual.st_uid, actual.st_gid, actual.st_mode & 0o7777),
        (uid, gid, 0o751)
    );
    assert_eq!(fs.readlink(root, entry.inode).unwrap(), b"target");
    assert!(
        matches!(fs.getxattr(root, entry.inode, c"user.binary", 3).unwrap(), GetxattrReply::Value(value) if value == [0,255,7])
    );
    assert_eq!(
        pvisor_overlay_core::fingerprint_at(&lower, Path::new("link")).unwrap(),
        original
    );
}

#[rstest::rstest]
#[case::invalid_timestamp(SetattrValid::UID | SetattrValid::MTIME)]
#[case::symlink_size(SetattrValid::UID | SetattrValid::SIZE)]
fn invalid_logical_symlink_updates_preserve_ownership_and_mode(#[case] valid: SetattrValid) {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lower = temp.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let _attachment =
        backend::attach_image(&lower, &temp.path().join("objects"), Arc::new(Image)).unwrap();
    let fs = fixture(temp.path());
    let ctx = root_context();
    let entry = fs.lookup(ctx, fuse::ROOT_ID, c"link").unwrap();
    let mut requested = entry.attr;
    requested.st_uid = 3456;
    requested.st_mtime_nsec = 1_000_000_000;
    assert_eq!(
        fs.setattr(ctx, entry.inode, requested, None, valid)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EINVAL)
    );
    let actual = fs.getattr(ctx, entry.inode, None).unwrap().0;
    assert_eq!(
        (actual.st_uid, actual.st_gid, actual.st_mode & 0o7777),
        (1234, 2345, 0o751)
    );
    assert_eq!(fs.readlink(ctx, entry.inode).unwrap(), b"target");
}

#[test]
fn ordinary_virtual_symlinks_retain_the_native_inode_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let lower = temp.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    pvisor_overlay_core::sys::set_xattr(
        &lower,
        OsStr::new("user.containers.override_stat"),
        b"0:0:040755",
        0,
    )
    .unwrap();
    let fs = fixture(temp.path());
    let ctx = root_context();
    let entry = fs
        .symlink(
            ctx,
            c"target",
            fuse::ROOT_ID,
            c"native",
            Extensions::default(),
        )
        .unwrap();
    let physical = std::fs::symlink_metadata(fs.core.upper().join("native")).unwrap();
    assert_eq!(
        (entry.attr.st_uid, entry.attr.st_gid),
        (physical.uid(), physical.gid())
    );
    let mut requested = entry.attr;
    requested.st_uid = 0;
    requested.st_gid = 0;
    let changed = fs
        .setattr(
            ctx,
            entry.inode,
            requested,
            None,
            SetattrValid::UID | SetattrValid::GID,
        )
        .unwrap()
        .0;
    assert_eq!((changed.st_uid, changed.st_gid), (0, 0));
    assert!(!backend::requires_unix_metadata(fs.core.upper()).unwrap());
    let after = std::fs::symlink_metadata(fs.core.upper().join("native")).unwrap();
    assert_eq!(
        (after.uid(), after.gid(), after.ino()),
        (physical.uid(), physical.gid(), physical.ino())
    );
    assert_eq!(fs.readlink(ctx, entry.inode).unwrap(), b"target");
    let config = fs.snapshot_config.clone();
    let allocator = fs.inode_alloc.clone();
    drop(fs);
    let fs = OverlayFs::open_existing(config, allocator).unwrap();
    fs.init(FsOptions::empty()).unwrap();
    let restored = fs.lookup(ctx, fuse::ROOT_ID, c"native").unwrap();
    assert_eq!((restored.attr.st_uid, restored.attr.st_gid), (0, 0));
    assert!(!backend::requires_unix_metadata(fs.core.upper()).unwrap());
}
