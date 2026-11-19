//! Engineering conformance probe: real RAFS source through existing Core/export.
use anyhow::{Context, ensure};
use pvisor::rafs::{RafsImage, RafsView};
use pvisor_overlay_core::{
    OverlayCore, OverlayLayout,
    apply::{OverlayRecord, OverlayState, OverlayUpper, apply_overlay, write_overlay_record},
    backend,
    service::FilesystemService,
    stage, sys,
};
use std::{
    fs::{self, File},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    time::Duration,
};

fn original(root: &Path, path: &Path) -> anyhow::Result<Vec<u8>> {
    let metadata = backend::symlink_metadata(root.join(path))?;
    ensure!(
        metadata.len() <= 8 * 1024 * 1024,
        "probe selected file exceeds 8 MiB"
    );
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < metadata.len() {
        let data = backend::read_at(&root.join(path), bytes.len() as u64, 64 * 1024)?
            .context("RAFS attachment missing")?;
        ensure!(!data.is_empty(), "short source data");
        bytes.extend(data);
    }
    Ok(bytes)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_vm(root: &Path, out: &Path, init: &Path, firmware: &Path) -> anyhow::Result<()> {
    use pvisor_vm::api::{
        OverlayConfig, PermissionSemantics, VmBuilder, VmConfiguration, VmRuntime,
    };
    let root = root.canonicalize()?;
    let out = out.canonicalize()?;
    let mut vm = VmBuilder::new(2, 128)?;
    vm.set_firmware_path(firmware.canonicalize()?)?;
    vm.disable_implicit_init()?;
    vm.overlay(
        "/dev/root",
        OverlayConfig {
            lower_dirs: vec![root.to_owned()],
            lower_mutability: vec![pvisor_overlay_core::LayerMutability::Mutable],
            upper_dir: out.join("vm-upper"),
            work_dir: Some(out.join("vm-work")),
            preimage_dir: None,
            apply_target: None,
            baseline_lower: None,
            baseline_content_index: None,
            excluded_paths: vec![],
            access_policy: Default::default(),
            semantics: PermissionSemantics::LinuxComplete,
        },
        0,
    )?;
    vm.virtual_file("/dev/root", "/init.krun", fs::read(init)?, 0o755, false)?;
    vm.run(|_| Ok(()))?;
    anyhow::bail!("VM run returned without guest poweroff")
}

// The VM API intentionally exits its dedicated runner. Its parent owns cleanup.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn supervise_vm(arguments: &[std::ffi::OsString], out: &Path) -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "enable owned-descendant reaping"
    );
    let parent = unsafe { libc::getpid() };
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("--vm-worker").args(arguments);
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0
                || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::other("supervisor disappeared"));
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let group = child.id() as i32;
    let deadline = std::time::Instant::now() + Duration::from_secs(110);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            child.wait()?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The reader inherited only this worker's process group. Stop/reap it even
    // when the worker exited via _exit and skipped its Rust Drop implementations.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = unsafe { libc::waitpid(-group, std::ptr::null_mut(), libc::WNOHANG) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error.into());
            }
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "VM descendants did not exit"
        );
        if result == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Only the exclusively created output directory is cleanup authority.
    for entry in fs::read_dir(out)? {
        let entry = entry?;
        if ["vm-upper", "vm-work"]
            .iter()
            .any(|name| entry.file_name() == *name)
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    ensure!(
        status.context("VM validation timed out")?.success(),
        "VM validation failed"
    );
    let report = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(out.join("vm-upper/pvisor-vm-probe-result"))?;
    ensure!(report.metadata()?.is_file(), "guest report is not a file");
    let mut bytes = Vec::new();
    report.take(128).read_to_end(&mut bytes)?;
    ensure!(
        bytes == b"PASS\n",
        "guest validation failed: {}",
        String::from_utf8_lossy(&bytes)
    );
    println!(
        "{}",
        serde_json::json!({"guest":"PASS","vm_exit":0,"descendants_reaped":true,"transient_cache_removed":true})
    );
    Ok(())
}
fn reopen_symlink(upper: &Path, selected: &Path) -> anyhow::Result<serde_json::Value> {
    let lower = tempfile::Builder::new()
        .prefix(".pvisor-reopen-probe-")
        .tempdir_in(upper.parent().context("upper parent")?)?;
    let core = OverlayCore::open_existing(
        vec![lower.path().to_owned()],
        upper.to_owned(),
        None,
        vec![],
        None,
    )?;
    let backing = core.metadata_resolved(selected)?.resolved.path;
    let attr = backend::attributes(&backing)?.context("persisted metadata missing")?;
    Ok(
        serde_json::json!({"uid":attr.uid,"gid":attr.gid,"mode":attr.perm,
        "mtime":sys::unix_timestamp(attr.mtime),
        "fingerprint":pvisor_overlay_core::fingerprint_at(upper, selected)?}),
    )
}
fn qualify_apply(
    view: &RafsView,
    out: &Path,
    selected: &Path,
    alias: Option<&Path>,
) -> anyhow::Result<()> {
    let symlink = backend::symlink_metadata(view.root().join(selected))?
        .file_type()
        .is_symlink();
    let before = pvisor_overlay_core::fingerprint_at(view.root(), selected)?;
    let source_mode = backend::attributes(&view.root().join(selected))?
        .context("source attributes")?
        .perm;
    for conflict in [false, true] {
        let name = if conflict { "conflict" } else { "apply" };
        let target = out.join(format!("{name}-target"));
        pvisor::environment_snapshot::copy_owned_tree(view.root(), &target)?;
        let stage_dir = out.join(format!("{name}-stage"));
        let mut record = OverlayRecord {
            id: name.into(),
            generation: 0,
            target: target.clone(),
            baseline_lower: Some(view.root().to_owned()),
            upper: OverlayUpper {
                upper_dir: stage_dir.join("upper"),
                work_dir: stage_dir.join("work"),
            },
            merged_dir: stage_dir.join("merged"),
            stage_dir: stage_dir.clone(),
            excluded_paths: vec![],
            access_policy: Default::default(),
            auto_apply: false,
            auto_discard: false,
            protect_target: false,
            state: OverlayState::Staged,
        };
        let layout = OverlayLayout::with_baseline(
            vec![view.root().to_owned()],
            target.clone(),
            Some(view.root()),
        )?;
        let core = OverlayCore::new_for_layout(
            layout,
            record.upper.upper_dir.clone(),
            Some(record.upper.work_dir.clone()),
            vec![],
            Some(stage_dir.join("preimages")),
        )?;
        write_overlay_record(&record)?;
        let changed = core.copy_up(selected)?;
        if let Some(alias) = alias {
            let linked = core.copy_up(alias)?;
            ensure!(
                fs::symlink_metadata(&changed)?.ino() == fs::symlink_metadata(linked)?.ino(),
                "stage hardlink mismatch"
            );
        }
        if symlink {
            sys::set_symlink_attributes(&changed, Some(3456), None, Some(0o750))?;
            sys::set_xattr(
                &changed,
                std::ffi::OsStr::new("user.changed"),
                b"accepted change",
                0,
            )?;
        } else {
            fs::write(&changed, b"accepted change")?;
        }
        let expected = pvisor_overlay_core::fingerprint_at(record.upper.path(), selected)?;
        ensure!(
            pvisor_overlay_core::fingerprint_at(&target, selected)? == before,
            "staging changed the target"
        );
        if conflict {
            if symlink {
                let _owner = backend::metadata_owner(&target, true)?;
                sys::set_symlink_attributes(
                    &target.join(selected),
                    None,
                    None,
                    Some(source_mode ^ 1),
                )?;
            } else {
                fs::write(target.join(selected), b"external edit")?;
            }
        }
        let concurrent = pvisor_overlay_core::fingerprint_at(&target, selected)?;
        if !conflict {
            let mut archive = File::create(out.join("image-export.tar"))?;
            pvisor::rafs::export_image(&core, &mut archive)?;
            archive.sync_all()?;
            let m = fs::symlink_metadata(&changed)?;
            let mtime = backend::attributes(&changed)?.map_or((m.mtime(), m.mtime_nsec()), |a| {
                sys::unix_timestamp(a.mtime)
            });
            fs::write(
                out.join("image-export-expected.json"),
                serde_json::to_vec(&serde_json::json!({
                    "path":selected, "fingerprint":expected, "mtime":mtime,
                }))?,
            )?;
        }
        drop(core);
        stage::seal(&record.upper.upper_dir, &stage_dir.join("preimages"))?;
        if conflict {
            let error = apply_overlay(&mut record)
                .err()
                .context("external edit was overwritten")?;
            ensure!(
                error.to_string().contains("target changed after staging"),
                "unexpected apply failure: {error}"
            );
            ensure!(
                pvisor_overlay_core::fingerprint_at(&target, selected)? == concurrent,
                "conflicting target changed"
            );
        } else {
            let mut host_expected = expected.clone();
            if let pvisor_core::overlay::PathFingerprint::File { uid, gid, .. } = &mut host_expected
            {
                use std::os::fd::AsRawFd;
                let control = tempfile::tempfile_in(out)?;
                // Ask the host kernel which ownership this user can publish.
                let rc = unsafe { libc::fchown(control.as_raw_fd(), *uid, *gid) };
                if rc != 0 {
                    let error = std::io::Error::last_os_error();
                    ensure!(
                        matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)),
                        "ownership oracle: {error}"
                    );
                }
                let native = control.metadata()?;
                *uid = native.uid();
                *gid = native.gid();
            }
            apply_overlay(&mut record)?;
            let actual = pvisor_overlay_core::fingerprint_at(&target, selected)?;
            ensure!(
                actual == host_expected,
                "host apply fingerprint mismatch: expected {host_expected:?}, got {actual:?}"
            );
            if let Some(alias) = alias {
                ensure!(
                    fs::symlink_metadata(target.join(selected))?.ino()
                        == fs::symlink_metadata(target.join(alias))?.ino(),
                    "apply split hardlinks"
                );
            }
        }
    }
    Ok(())
}
fn inspect_symlink(
    image: RafsImage,
    first: RafsView,
    second: RafsView,
    out: &Path,
    selected: &Path,
) -> anyhow::Result<()> {
    let core = OverlayCore::new(vec![first.root().to_owned()], out.join("upper"), None)?;
    let entry = core.metadata_resolved(selected)?;
    ensure!(entry.metadata.file_type().is_symlink(), "expected symlink");
    let path = &entry.resolved.path;
    let attr = backend::attributes(path)?.context("missing RAFS attributes")?;
    let target = backend::read_link(path)?;
    let mut xattrs = std::collections::BTreeMap::new();
    for name in sys::list_xattrs(path)? {
        let value = sys::get_xattr(path, std::ffi::OsStr::from_bytes(&name))?;
        xattrs.insert(String::from_utf8(name)?, value);
    }
    let before = pvisor_overlay_core::fingerprint_at(first.root(), selected)?;
    core.copy_up(selected)?;
    core.rename(selected, Path::new("moved"), false)?;
    ensure!(
        pvisor_overlay_core::fingerprint_at(&out.join("upper"), Path::new("moved"))? == before,
        "copy-up or rename lost symlink metadata"
    );
    for (source, name) in [
        (second.root().to_owned(), "export"),
        (out.join("upper"), "upper-export"),
    ] {
        pvisor::environment_snapshot::copy_owned_tree(&source, &out.join(name))?;
        let entry = if name == "export" {
            selected
        } else {
            Path::new("moved")
        };
        ensure!(
            pvisor_overlay_core::fingerprint_at(&out.join(name), entry)? == before,
            "export lost symlink metadata"
        );
    }
    ensure!(
        pvisor_overlay_core::fingerprint_at(first.root(), selected)? == before,
        "private writes changed the source"
    );
    qualify_apply(&second, out, selected, None)?;
    let root = first.root().to_owned();
    drop(core);
    drop(first);
    drop(second);
    drop(image);
    ensure!(
        fs::read_dir(out.join("readers"))?.next().is_none(),
        "reader owner leaked"
    );
    ensure!(
        !backend::attachment_overlaps(&root)?,
        "attachment survived teardown"
    );
    let store = pvisor::environment_snapshot::SnapshotStore::new(&out.join("base-store"))?;
    let base = store.import_base(&out.join("export"))?;
    pvisor::environment_snapshot::copy_owned_tree(&base.root(), &out.join("base-export"))?;
    ensure!(
        pvisor_overlay_core::fingerprint_at(&out.join("base-export"), selected)? == before,
        "base import/export lost logical symlink metadata"
    );
    let output = std::process::Command::new(std::env::current_exe()?)
        .arg("--reopen-symlink")
        .arg(out.join("upper"))
        .arg("moved")
        .output()?;
    ensure!(
        output.status.success(),
        "new-process reopen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let restored: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        restored
            == serde_json::json!({"uid":attr.uid,"gid":attr.gid,"mode":attr.perm,
        "mtime":sys::unix_timestamp(attr.mtime),"fingerprint":before}),
        "new-process reopen changed metadata"
    );
    for (name, path) in [("export", selected), ("upper-export", Path::new("moved"))] {
        ensure!(
            pvisor_overlay_core::fingerprint_at(&out.join(name), path)? == before,
            "export depends on source owner"
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "readonly_symlink":true,"uid":attr.uid,"gid":attr.gid,"mode":attr.perm,
            "target":target.as_os_str().as_bytes(),"xattrs":xattrs,
            "copy_up":true,"rename":true,"reopen_without_source":true,"reopen_in_new_process":true,
            "owned_export":true,"base_roundtrip":true,"stage_apply":true,"conflict_rejected":true,"reader_reaped":true
        })
    );
    Ok(())
}
fn main() -> anyhow::Result<()> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let worker = args.first().is_some_and(|arg| arg == "--vm-worker");
    if worker {
        args.remove(0);
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !worker
        && (args.first().is_some_and(|arg| arg == "--vm-export")
            || args.get(5).is_some_and(|arg| arg == "--vm"))
    {
        let out = if args.first().is_some_and(|arg| arg == "--vm-export") {
            ensure!(
                args.len() == 5,
                "usage: --vm-export ROOT NEW_OUTPUT INIT FIRMWARE"
            );
            Path::new(&args[2])
        } else {
            ensure!(
                args.len() == 8,
                "--vm requires static guest init and firmware paths"
            );
            Path::new(&args[3])
        };
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(out)?;
        return supervise_vm(&args, out);
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if args.first().is_some_and(|arg| arg == "--vm-export") {
        ensure!(
            args.len() == 5,
            "usage: --vm-export ROOT NEW_OUTPUT INIT FIRMWARE"
        );
        use std::os::unix::fs::DirBuilderExt;
        if !worker {
            fs::DirBuilder::new().mode(0o700).create(&args[2])?;
        }
        return run_vm(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
        );
    }
    if args.first().is_some_and(|arg| arg == "--reopen-symlink") {
        ensure!(args.len() == 3, "usage: --reopen-symlink UPPER PATH");
        println!(
            "{}",
            reopen_symlink(Path::new(&args[1]), Path::new(&args[2]))?
        );
        return Ok(());
    }
    ensure!(
        (5..=8).contains(&args.len()),
        "usage: rafs_core_probe STORE HANDLE READER NEW_OUTPUT PATH [HARDLINK_ALIAS | --inspect-symlink]"
    );
    let store = Path::new(&args[0]);
    let handle = args[1].to_str().context("handle encoding")?;
    let engine = Path::new(&args[2]);
    let out = Path::new(&args[3]);
    let selected = Path::new(&args[4]);
    let alias = args.get(5).map(Path::new);
    use std::os::unix::fs::DirBuilderExt;
    if !worker {
        fs::DirBuilder::new().mode(0o700).create(out)?;
    }
    let work = out.join("readers");
    fs::create_dir(&work)?;
    let image = RafsImage::open(store, handle, engine, &work, Duration::from_secs(30))?;
    let first = image.project(out)?;
    let second = image.project(out)?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if args.get(5).is_some_and(|arg| arg == "--vm") {
        ensure!(
            args.len() == 8,
            "--vm requires static guest init and firmware paths"
        );
        return run_vm(first.root(), out, Path::new(&args[6]), Path::new(&args[7]));
    }
    if args.get(5).is_some_and(|arg| arg == "--inspect-symlink") {
        return inspect_symlink(image, first, second, out, selected);
    }
    let source = original(first.root(), selected)?;
    let core = OverlayCore::new(vec![first.root().to_owned()], out.join("upper"), None)?;
    let service = FilesystemService::new(core);
    let backing = first.root().join(selected);
    let file = File::open(&backing)?;
    let mut header = vec![0; source.len().min(64)];
    let count = service.read_at(&backing, &file, &mut header, 0)?;
    ensure!(
        header[..count] == source[..count],
        "service read changed source bytes"
    );
    let upper = service.copy_up(selected)?;
    if let Some(alias) = alias {
        let linked = service.copy_up(alias)?;
        ensure!(
            fs::metadata(&upper)?.ino() == fs::metadata(linked)?.ino(),
            "copy-up split a hardlink group"
        );
    }
    fs::write(&upper, b"private first view")?;
    ensure!(
        original(second.root(), selected)? == source,
        "private write changed another view"
    );
    drop(file);
    drop(service);

    // The real export entry point must request unvisited entries and content.
    let export = out.join("export");
    pvisor::environment_snapshot::copy_owned_tree(second.root(), &export)?;
    qualify_apply(&second, out, selected, alias)?;
    let root = second.root().to_owned();
    drop(first);
    drop(second);
    drop(image);
    ensure!(
        fs::read_dir(&work)?.next().is_none(),
        "reader owner leaked after final view"
    );
    ensure!(
        !backend::attachment_overlaps(&root)?,
        "attachment survived teardown"
    );
    ensure!(
        fs::read(export.join(selected))? == source,
        "owned export depends on source"
    );
    println!(
        "{}",
        serde_json::json!({"same_process_core":true,"shared_source_views":2,"service_read":true,"copy_up":true,"stage_apply":true,
        "conflict_rejected":true,"owned_export":export,"reader_reaped":true,"runtime_vm_integrated":false})
    );
    Ok(())
}
