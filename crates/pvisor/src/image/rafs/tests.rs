//! Fake peers isolate Rust transport/ownership; native format tests are separate.
use super::*;
use pvisor_overlay_core::backend;
use std::path::PathBuf;

fn fixture(program: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, String) {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let work = temp.path().join("work");
    for path in [&store, &store.join("merged"), &work] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let engine = temp.path().join("engine");
    fs::write(&engine, format!("#!/usr/bin/env python3\n{program}")).unwrap();
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
    (
        temp,
        store,
        work,
        engine,
        format!("{PREFIX}{}", "a".repeat(64)),
    )
}
const PEER: &str = r#"
import sys, json, os, hashlib, time
from pathlib import Path
(Path(sys.argv[3]).parent.parent / 'seen-pid').write_text(str(os.getpid()))
inp, out = sys.stdin.buffer, sys.stdout.buffer
def frame(value):
    data = json.dumps(value).encode(); out.write(len(data).to_bytes(4, 'big') + data); out.flush()
frame({'version': 1, 'handle': sys.argv[2]})
while head := inp.read(4):
    request = json.loads(inp.read(int.from_bytes(head, 'big')))
    path = request.get('path', '')
    if path == 'absent':
        frame({'status':'error', 'errno':2, 'message':'missing'})
    elif request['op'] == 'stat':
        frame({'status':'ok', 'node':{'kind':'directory' if path == '' else 'file',
          'inode':1 if path == '' else 2, 'mode':493 if path == '' else 420,
          'uid':os.getuid(), 'gid':os.getgid(), 'size':0 if path == '' else 4,
          'nlink':2 if path == '' else 1, 'mtime':1, 'mtime_nsec':1, 'target':None, 'xattrs':{}}})
    elif path == 'timeout':
        time.sleep(60)
    else:
        body=b'data'
        frame({'status':'ok', 'length':4, 'sha256':'sha256:' + ('0'*64 if path == 'bad' else hashlib.sha256(body).hexdigest())})
        out.write(body); out.flush()
"#;
fn reaped(pid: u32) {
    let mut status = 0;
    // SAFETY: this is our already-owned child; WNOHANG never blocks or signals.
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn views_share_one_owner_and_last_drop_reaps_it_and_removes_backing() {
    let (_temp, store, work, engine, handle) = fixture(PEER);
    let image = RafsImage::open(&store, &handle, &engine, &work, Duration::from_secs(5)).unwrap();
    let pid = image
        .source
        .worker
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .child
        .id();
    let cache = image
        .source
        ._directory
        .as_ref()
        .expect("local reader owns temporary cache")
        .path()
        .to_owned();
    let first = image.project(&work).unwrap();
    let second = image.project(&work).unwrap();
    assert_eq!(Arc::strong_count(&image.source), 3);
    let root = second.root().to_owned();
    drop(image);
    drop(first);
    assert_eq!(
        backend::read_at(&root.join("file"), 0, 4).unwrap().unwrap(),
        b"data"
    );
    drop(second);
    assert!(!cache.exists());
    assert!(!root.exists());
    reaped(pid);
}

#[test]
fn missing_paths_preserve_errno_and_protocol_failures_retire_the_source() {
    for bad in ["bad", "timeout"] {
        let (_temp, store, work, engine, handle) = fixture(PEER);
        let image =
            RafsImage::open(&store, &handle, &engine, &work, Duration::from_secs(5)).unwrap();
        let pid = image
            .source
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .child
            .id();
        assert_eq!(
            image
                .source
                .metadata(Path::new("absent"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
        assert_eq!(
            image.source.read_at(Path::new("file"), 0, 4).unwrap(),
            b"data"
        );
        image
            .source
            .worker
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .timeout = Duration::from_millis(100);
        assert!(image.source.read_at(Path::new(bad), 0, 4).is_err());
        assert!(image.source.worker.lock().unwrap().is_none());
        assert!(image.source.metadata(Path::new("")).is_err());
        reaped(pid);
    }
}

#[test]
fn failed_startup_reaps_the_child_and_never_leaves_a_cache_owner() {
    let script = "import sys,os,time\nfrom pathlib import Path\n(Path(sys.argv[3]).parent.parent/'seen-pid').write_text(str(os.getpid()))\ntime.sleep(60)";
    let (_temp, store, work, engine, handle) = fixture(script);
    let error = RafsImage::open(&store, &handle, &engine, &work, Duration::from_millis(200))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let pid: u32 = fs::read_to_string(work.join("seen-pid"))
        .unwrap()
        .parse()
        .unwrap();
    reaped(pid);
    assert!(
        fs::read_dir(work)
            .unwrap()
            .all(|e| e.unwrap().file_name() == "seen-pid")
    );
}

#[test]
fn cross_process_handoff_is_rejected_before_a_sparse_view_can_be_consumed() {
    let (_temp, store, work, engine, handle) = fixture(PEER);
    let image = RafsImage::open(&store, &handle, &engine, &work, Duration::from_secs(5)).unwrap();
    let view = image.project(&work).unwrap();
    let roots = [view.root().to_owned()];
    assert!(crate::image::cache::attach_runner_lowers(roots.iter()).is_err());
    assert_eq!(
        image.source.read_at(Path::new("file"), 0, 4).unwrap(),
        b"data"
    );
}

#[test]
fn symlink_metadata_is_kept_for_readonly_queries() {
    let metadata = Metadata {
        kind: "symlink".into(),
        inode: 2,
        mode: 0o777,
        uid: unsafe { libc::geteuid() }.wrapping_add(1),
        gid: unsafe { libc::getegid() },
        size: 1,
        nlink: 1,
        mtime: 1,
        mtime_nsec: 0,
        xattrs: BTreeMap::from([("user.example".into(), b"exact".to_vec())]),
        target: Some("a".into()),
    };
    let node = metadata.node().unwrap();
    assert_eq!(node.attr.uid, unsafe { libc::geteuid() }.wrapping_add(1));
    assert_eq!(node.xattrs[b"user.example".as_slice()], b"exact");
    assert_eq!(node.target.as_deref(), Some(b"a".as_slice()));
}

#[test]
fn runner_binding_roundtrips_and_owns_a_separate_artifact_lease() {
    const CHILD: &str = "PVISOR_RAFS_BINDING_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        let roots = [PathBuf::from(root)];
        let _owners = crate::image::cache::attach_runner_lowers(roots.iter()).unwrap();
        assert_eq!(
            backend::read_at(&roots[0].join("file"), 0, 4)
                .unwrap()
                .unwrap(),
            b"data"
        );
        return;
    }
    let (_temp, store, work, engine, handle) = fixture(PEER);
    let bundle = store.join("merged").join("a".repeat(64));
    fs::create_dir(&bundle).unwrap();
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700)).unwrap();
    let commit = bundle.join("MERGED.json");
    fs::write(&commit, b"fake authenticated by the isolated test peer").unwrap();
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).unwrap();
    let selected = RafsRootfs {
        store,
        handle,
        reader: engine,
    };
    let view = selected.prepare(&work).unwrap();
    selected.validate_view(view.root()).unwrap();
    let lease = File::open(&commit).unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&lease).is_err());
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "image::rafs::tests::runner_binding_roundtrips_and_owns_a_separate_artifact_lease",
            "--nocapture",
        ])
        .env(CHILD, view.root())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(fs2::FileExt::try_lock_exclusive(&lease).is_err());
    let mut other = selected.clone();
    other.handle = format!("{PREFIX}{}", "b".repeat(64));
    assert!(other.validate_view(view.root()).is_err());
    drop(view);
    fs2::FileExt::try_lock_exclusive(&lease).unwrap();
}

#[test]
fn runner_binding_refuses_symlinks_and_changed_artifact_identity() {
    let (_temp, store, work, engine, handle) = fixture(PEER);
    let bundle = store.join("merged").join("a".repeat(64));
    fs::create_dir(&bundle).unwrap();
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700)).unwrap();
    let commit = bundle.join("MERGED.json");
    fs::write(&commit, b"fixed").unwrap();
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).unwrap();
    let selected = RafsRootfs {
        store,
        handle,
        reader: engine,
    };
    let view = selected.prepare(&work).unwrap();
    let descriptor = view.root().parent().unwrap().join("rafs-source.json");
    fs::rename(&descriptor, descriptor.with_extension("saved")).unwrap();
    std::os::unix::fs::symlink(descriptor.with_extension("saved"), &descriptor).unwrap();
    assert!(reattach(view.root()).is_err());
    fs::remove_file(&descriptor).unwrap();
    fs::rename(descriptor.with_extension("saved"), &descriptor).unwrap();
    fs::rename(&commit, commit.with_extension("old")).unwrap();
    fs::write(&commit, b"fixed").unwrap();
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(reattach(view.root()).is_err());
    fs::remove_file(&commit).unwrap();
    fs::rename(commit.with_extension("old"), &commit).unwrap();
    let seen = work.join("seen-pid");
    let before = fs::read(&seen).unwrap();
    fs::rename(&selected.reader, selected.reader.with_extension("old")).unwrap();
    fs::write(
        &selected.reader,
        fs::read(selected.reader.with_extension("old")).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&selected.reader, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(reattach(view.root()).is_err());
    assert_eq!(
        fs::read(seen).unwrap(),
        before,
        "replaced reader must not execute before identity validation"
    );
    assert_eq!(
        backend::read_at(&view.root().join("file"), 0, 4)
            .unwrap()
            .unwrap(),
        b"data"
    );
}

#[test]
fn bounded_directory_inventory_rejects_growth_without_retiring_the_reader() {
    let program = PEER.replace(
        "    elif request['op'] == 'stat':",
        "    elif request['op'] == 'list':\n        names = [f'entry-{i:05}' for i in range(4000)] if path == 'wide' else ['one']\n        offset = request['offset']; page = names[offset:offset+request['limit']]\n        next_offset = offset+len(page) if offset+len(page)<len(names) else None\n        frame({'status':'ok','page':{'names':page,'next_offset':next_offset}})\n    elif request['op'] == 'stat':",
    );
    let (_temp, store, work, engine, handle) = fixture(&program);
    let mut image =
        RafsImage::open(&store, &handle, &engine, &work, Duration::from_secs(5)).unwrap();
    Arc::get_mut(&mut image.source).unwrap().client_limits = Some(node::ClientLimits {
        metadata_entries: 1,
        directory_bytes: 65536,
    });
    assert_eq!(
        image
            .source
            .entries(Path::new("wide"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EOVERFLOW)
    );
    assert_eq!(
        image.source.entries(Path::new("small")).unwrap(),
        [OsString::from("one")]
    );
    assert_eq!(
        image.source.read_at(Path::new("file"), 0, 4).unwrap(),
        b"data"
    );
    Arc::get_mut(&mut image.source).unwrap().client_limits = None;
    assert_eq!(image.source.entries(Path::new("wide")).unwrap().len(), 4000);
}
