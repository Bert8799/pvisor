//! Image archives preserve logical ownership independently of host writeback.
#![cfg(unix)]
use proptest::prelude::*;
use pvisor::rafs::export_image;
use pvisor_overlay_core::{OverlayCore, OverlayLayout, backend, sys};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Debug)]
struct Entry {
    path: PathBuf,
    uid: u64,
    gid: u64,
    mode: u32,
    kind: tar::EntryType,
    target: Option<PathBuf>,
    pax: BTreeMap<Vec<u8>, Vec<u8>>,
    bytes: Vec<u8>,
}
fn entries(bytes: &[u8]) -> Vec<Entry> {
    let mut rows = Vec::new();
    let mut pax = BTreeMap::new();
    for entry in tar::Archive::new(bytes).entries().unwrap().raw(true) {
        let mut entry = entry.unwrap();
        if entry.header().entry_type().is_pax_local_extensions() {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                let space = rest.iter().position(|b| *b == b' ').unwrap();
                let length: usize = std::str::from_utf8(&rest[..space])
                    .unwrap()
                    .parse()
                    .unwrap();
                let record = &rest[space + 1..length - 1];
                assert_eq!(rest[length - 1], b'\n');
                let equals = record.iter().position(|b| *b == b'=').unwrap();
                assert!(
                    pax.insert(record[..equals].to_vec(), record[equals + 1..].to_vec())
                        .is_none()
                );
                rest = &rest[length..];
            }
            continue;
        }
        let path = entry.path().unwrap().into_owned();
        let target = entry.link_name().unwrap().map(|p| p.into_owned());
        let uid = entry.header().uid().unwrap();
        let gid = entry.header().gid().unwrap();
        let mode = entry.header().mode().unwrap();
        let kind = entry.header().entry_type();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        rows.push(Entry {
            path,
            uid,
            gid,
            mode,
            kind,
            target,
            pax: std::mem::take(&mut pax),
            bytes,
        });
    }
    assert!(pax.is_empty());
    rows
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/image_export.proptest-regressions")
        ))),
        ..ProptestConfig::default()
    })]
    #[test]
    fn image_archive_keeps_logical_metadata_links_and_changes_without_chown(
        uid in 1u32..65536, gid in 1u32..65536, mode in 0u32..0o10000,
        payload in prop::collection::vec(any::<u8>(), 0..257),
        xattr in prop::collection::vec(any::<u8>(), 0..65),
    ) {
        let temp = tempfile::tempdir().unwrap();
        let lower = temp.path().join("lower"); fs::create_dir(&lower).unwrap();
        fs::write(lower.join("file"), b"original").unwrap();
        fs::hard_link(lower.join("file"), lower.join("alias")).unwrap();
        fs::write(lower.join("deleted"), b"removed").unwrap();
        fs::write(lower.join("unvisited"), b"retained").unwrap();
        sys::set_xattr(&lower.join("file"), "user.containers.override_stat".as_ref(),
            format!("{uid}:{gid}:0{:o}", 0o100000 | mode).as_bytes(), 0).unwrap();
        sys::set_xattr(&lower.join("file"), "user.binary".as_ref(), &xattr, 0).unwrap();
        let layout = OverlayLayout::new(vec![lower.clone()], lower.clone()).unwrap().with_private_backing();
        let core = OverlayCore::new_for_layout(layout, temp.path().join("upper"), None, vec![], None).unwrap();
        core.remove(Path::new("deleted"), false).unwrap();
        let copied = core.copy_up(Path::new("file")).unwrap();
        fs::write(&copied, &payload).unwrap();
        let time = sys::unix_time(1_700_000_000, 1);
        sys::set_times(&copied, Some(time), Some(time), false).unwrap();
        let before = fs::symlink_metadata(&copied).unwrap();
        let mut archive = Vec::new(); export_image(&core, &mut archive).unwrap();
        let rows = entries(&archive);
        prop_assert_eq!(rows.len(), 4);
        prop_assert!(!rows.iter().any(|e| e.path == Path::new("deleted")));
        let linked: Vec<_> = rows.iter().filter(|e| matches!(e.path.to_str(), Some("file" | "alias"))).collect();
        prop_assert_eq!(linked.len(), 2);
        for row in &linked {
            prop_assert_eq!((row.uid, row.gid, row.mode), (u64::from(uid), u64::from(gid), mode));
            prop_assert_eq!(row.pax.get(b"mtime".as_slice()), Some(&b"1700000000.000000001".to_vec()));
            prop_assert_eq!(row.pax.get(b"SCHILY.xattr.user.binary".as_slice()), Some(&xattr));
            prop_assert_eq!(row.pax.len(), 2);
        }
        let file = linked.iter().find(|e| e.kind.is_file()).unwrap();
        let alias = linked.iter().find(|e| e.kind.is_hard_link()).unwrap();
        prop_assert_eq!(&file.bytes, &payload);
        prop_assert_eq!(alias.target.as_ref(), Some(&file.path));
        prop_assert_eq!(&rows.iter().find(|e| e.path == Path::new("unvisited")).unwrap().bytes, b"retained");
        let after = fs::symlink_metadata(&copied).unwrap();
        prop_assert_eq!((before.uid(), before.gid(), before.mode(), before.ino()),
            (after.uid(), after.gid(), after.mode(), after.ino()));
        prop_assert_eq!(fs::read(lower.join("file")).unwrap(), b"original");
        prop_assert!(!core.upper_path(Path::new("alias")).exists());
    }
}

#[test]
fn symlink_image_archive_preserves_foreign_identity_xattrs_times_and_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let lower = temp.path().join("lower");
    fs::create_dir(&lower).unwrap();
    let _owner = backend::metadata_owner(&lower, true).unwrap();
    std::os::unix::fs::symlink("destination", lower.join("link")).unwrap();
    backend::restore_symlink_metadata(
        &lower.join("link"),
        backend::UnixIdentity {
            uid: 4321,
            gid: 5432,
            mode: 0o751,
        },
        &[(b"user.binary".to_vec(), vec![0, 255, b'\n'])],
    )
    .unwrap();
    let time = sys::unix_time(0, 1);
    sys::set_times(&lower.join("link"), Some(time), Some(time), true).unwrap();
    let core = OverlayCore::new(vec![lower.clone()], temp.path().join("upper"), None).unwrap();
    core.hard_link(Path::new("link"), Path::new("alias"))
        .unwrap();
    let mut archive = Vec::new();
    export_image(&core, &mut archive).unwrap();
    let rows = entries(&archive);
    let rows: Vec<_> = rows.iter().filter(|e| e.path != Path::new(".")).collect();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!((row.uid, row.gid, row.mode), (4321, 5432, 0o751));
        assert_eq!(
            row.pax[b"SCHILY.xattr.user.binary".as_slice()],
            [0, 255, b'\n']
        );
        assert_eq!(row.pax[b"mtime".as_slice()], b"0.000000001");
    }
    let symlink = rows.iter().find(|e| e.kind.is_symlink()).unwrap();
    assert_eq!(symlink.target.as_deref(), Some(Path::new("destination")));
    let alias = rows.iter().find(|e| e.kind.is_hard_link()).unwrap();
    assert_eq!(alias.target.as_ref(), Some(&symlink.path));
    assert_eq!(
        fs::read_link(lower.join("link")).unwrap(),
        Path::new("destination")
    );
}

#[test]
fn image_export_errors_preserve_source_and_do_not_consume_staged_changes() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let lower = temp.path().join("lower");
    fs::create_dir(&lower).unwrap();
    fs::write(lower.join("file"), b"source").unwrap();
    let core = OverlayCore::new(vec![lower.clone()], temp.path().join("upper"), None).unwrap();
    let copied = core.copy_up(Path::new("file")).unwrap();
    fs::write(&copied, b"staged").unwrap();
    assert!(export_image(&core, &mut Broken).is_err());
    assert_eq!(fs::read(&copied).unwrap(), b"staged");
    assert_eq!(fs::read(lower.join("file")).unwrap(), b"source");
    sys::set_xattr(
        &copied,
        "user.containers.override_stat".as_ref(),
        b"invalid",
        0,
    )
    .unwrap();
    assert!(export_image(&core, &mut Vec::new()).is_err());
    assert_eq!(
        sys::get_xattr(&copied, "user.containers.override_stat".as_ref()).unwrap(),
        b"invalid"
    );
}

#[test]
fn image_export_refuses_special_nodes_instead_of_saving_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    let lower = temp.path().join("lower");
    fs::create_dir(&lower).unwrap();
    fs::write(lower.join("placeholder"), b"").unwrap();
    let core = OverlayCore::new(vec![lower.clone()], temp.path().join("upper"), None).unwrap();
    sys::set_xattr(
        &lower.join("placeholder"),
        pvisor_overlay_core::VIRTUAL_NODE_XATTR.as_ref(),
        b"c:0:0",
        0,
    )
    .unwrap();
    assert!(export_image(&core, &mut Vec::new()).is_err());
    assert_eq!(fs::read(lower.join("placeholder")).unwrap(), b"");
    sys::remove_xattr(
        &lower.join("placeholder"),
        pvisor_overlay_core::VIRTUAL_NODE_XATTR.as_ref(),
    )
    .unwrap();
    core.create_node(Path::new("fifo"), libc::S_IFIFO | 0o600, 0)
        .unwrap();
    assert!(export_image(&core, &mut Vec::new()).is_err());
    use std::os::unix::fs::FileTypeExt;
    assert!(
        fs::symlink_metadata(core.upper_path(Path::new("fifo")))
            .unwrap()
            .file_type()
            .is_fifo()
    );
}
