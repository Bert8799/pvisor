//! Storage contracts; opaque fixture state is not a real VM snapshot.
use pvisor::environment_snapshot::templates::{TemplateCapture, TemplateCatalog, TemplateSpec};
use pvisor::environment_snapshot::{Compatibility, file_hash};
use pvisor_guest::template::Ready;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn fixture() -> (
    tempfile::TempDir,
    TemplateCatalog,
    TemplateSpec,
    PathBuf,
    File,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let image = root.join("image");
    fs::create_dir(&image).unwrap();
    fs::write(image.join("data"), b"immutable image").unwrap();
    let guest = root.join("guest");
    fs::write(&guest, b"fixture supervisor").unwrap();
    let spec = TemplateSpec {
        version: 1,
        generation: "generation-a".into(),
        image_sha256: TemplateSpec::image_id(&image).unwrap(),
        guest_sha256: file_hash(&guest).unwrap(),
        cpus: 2,
        memory_mib: 128,
        compatibility: Compatibility {
            host_boot: "host-boot".into(),
            build: "runner-build".into(),
            firmware: "firmware".into(),
            profile: pvisor::environment_snapshot::templates::PROFILE.into(),
        },
        stdio_ports: [true, true, false],
        network_mac: None,
        native_overlay: false,
    };
    let source = root.join("source");
    spec.prepare_rootfs(&image, &guest, &source).unwrap();
    let ram = File::create(root.join("ram")).unwrap();
    ram.set_len(4096).unwrap(); // Deliberately opaque storage fixture, not RAM geometry.
    let catalog = TemplateCatalog::new(&root.join("catalog")).unwrap();
    (
        temp,
        catalog,
        spec,
        source,
        File::open(root.join("ram")).unwrap(),
    )
}

fn publish(
    catalog: &TemplateCatalog,
    name: &str,
    expected: Option<&str>,
    spec: &TemplateSpec,
    source: &Path,
    ram: &File,
) -> anyhow::Result<pvisor::environment_snapshot::templates::TemplateVersion> {
    catalog.publish(
        name,
        expected,
        spec,
        TemplateCapture {
            ready: &Ready {
                version: 1,
                template_id: spec.id()?,
            },
            machine: b"opaque fixture CPU/device state",
            ram,
            rootfs: source,
        },
    )
}

#[test]
fn update_switches_new_consumers_and_gc_skips_only_live_generations() {
    let (temp, catalog, spec, source, ram) = fixture();
    assert!(
        catalog
            .acquire_version(&spec.id().unwrap(), &spec.compatibility)
            .is_err()
    );
    let a = publish(&catalog, "shell", None, &spec, &source, &ram).unwrap();
    let live_a = catalog.acquire("shell", &spec.compatibility).unwrap();
    let mut next = spec.clone();
    next.generation = "generation-b".into();
    let source_b = temp.path().join("source-b");
    next.prepare_rootfs(
        &temp.path().join("image"),
        &temp.path().join("guest"),
        &source_b,
    )
    .unwrap();
    let b = publish(&catalog, "shell", Some(&a.id), &next, &source_b, &ram).unwrap();
    assert_ne!(a.id, b.id);
    let live_b = catalog.acquire("shell", &spec.compatibility).unwrap();
    assert_eq!(live_a.version().id, a.id);
    assert_eq!(live_b.version().id, b.id);
    let detached = catalog.acquire_version(&a.id, &spec.compatibility).unwrap();
    assert_eq!(detached.version().id, a.id); // Alias has already moved to B.
    drop(live_a); // Admission can release its lease after the recipient acquired A.
    assert_eq!(catalog.collect().unwrap(), 0);
    drop(detached);
    // B stays leased: it must not prevent reclaiming unrelated A.
    assert_eq!(catalog.collect().unwrap(), 1);
    assert!(!temp.path().join("catalog/generations").join(&a.id).exists());
    assert!(catalog.acquire_version(&a.id, &spec.compatibility).is_err());
    assert!(publish(&catalog, "reuse", None, &spec, &source, &ram).is_err());
    assert!(catalog.selected("reuse").unwrap().is_none());
    catalog.remove("shell", &b.id).unwrap();
    assert_eq!(catalog.selected("shell").unwrap(), None);
    assert_eq!(catalog.collect().unwrap(), 0);
    assert_eq!(
        live_b.environment().machine_bytes().unwrap(),
        b"opaque fixture CPU/device state"
    );
    drop(live_b);
    assert_eq!(catalog.collect().unwrap(), 1);
}

#[test]
fn selected_version_survives_catalog_reopen_and_materializes_private_writes() {
    let (temp, catalog, spec, source, ram) = fixture();
    let record = publish(&catalog, "shell", None, &spec, &source, &ram).unwrap();
    drop(catalog);
    let catalog = TemplateCatalog::new(&temp.path().join("catalog")).unwrap();
    assert_eq!(
        catalog.selected("shell").unwrap().as_deref(),
        Some(record.id.as_str())
    );
    let lease = catalog.acquire("shell", &spec.compatibility).unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    lease.environment().materialize(&first).unwrap();
    lease.environment().materialize(&second).unwrap();
    fs::write(first.join("data"), b"private first instance").unwrap();
    assert_eq!(fs::read(second.join("data")).unwrap(), b"immutable image");
    assert_eq!(fs::read(source.join("data")).unwrap(), b"immutable image");
    let mut raw = lease.environment().ram_file().unwrap();
    let mut bytes = Vec::new();
    raw.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), 4096);
    assert!(raw.write_all(b"forbidden").is_err());
}

#[test]
fn compare_and_swap_and_compatibility_fences_preserve_current_generation() {
    let (_temp, catalog, spec, source, ram) = fixture();
    let record = publish(&catalog, "shell", None, &spec, &source, &ram).unwrap();
    assert!(publish(&catalog, "shell", None, &spec, &source, &ram).is_err());
    assert!(
        publish(
            &catalog,
            "shell",
            Some(&"f".repeat(64)),
            &spec,
            &source,
            &ram
        )
        .is_err()
    );
    assert!(catalog.remove("shell", &"f".repeat(64)).is_err());
    let mut wrong = spec.compatibility.clone();
    wrong.host_boot = "another-boot".into();
    assert!(catalog.acquire("shell", &wrong).is_err());
    assert!(catalog.acquire_version(&record.id, &wrong).is_err());
    assert_eq!(
        catalog.selected("shell").unwrap().as_deref(),
        Some(record.id.as_str())
    );
}

#[test]
fn preparation_checks_inputs_and_never_overwrites_an_existing_image() {
    let (temp, _catalog, spec, source, _ram) = fixture();
    let before = fs::read(source.join("data")).unwrap();
    assert!(
        spec.prepare_rootfs(
            &temp.path().join("image"),
            &temp.path().join("guest"),
            &source
        )
        .is_err()
    );
    assert_eq!(fs::read(source.join("data")).unwrap(), before);
    let mut wrong = spec.clone();
    wrong.guest_sha256 = "0".repeat(64);
    let target = temp.path().join("must-not-create");
    assert!(
        wrong
            .prepare_rootfs(
                &temp.path().join("image"),
                &temp.path().join("guest"),
                &target
            )
            .is_err()
    );
    assert!(!target.exists());
    fs::write(temp.path().join("image/data"), b"changed image").unwrap();
    assert!(
        spec.prepare_rootfs(
            &temp.path().join("image"),
            &temp.path().join("guest"),
            &target
        )
        .is_err()
    );
    assert!(!target.exists());
}

#[test]
fn network_topology_is_bound_without_changing_legacy_recipe_ids() {
    let (_temp, _catalog, spec, _source, _ram) = fixture();
    let old = serde_json::to_value(&spec).unwrap();
    assert!(old.get("network_mac").is_none());
    assert!(old.get("native_overlay").is_none());
    let legacy: TemplateSpec = serde_json::from_value(old).unwrap();
    assert!(legacy.network_mac.is_none());
    assert_eq!(legacy.id().unwrap(), spec.id().unwrap());
    let mut network = spec.clone();
    network.network_mac = Some([2, 0, 0, 0, 0, 1]);
    assert_ne!(network.id().unwrap(), spec.id().unwrap());
    for mac in [[0; 6], [1, 0, 0, 0, 0, 1], [255; 6]] {
        network.network_mac = Some(mac);
        assert!(network.id().is_err());
    }
}

#[test]
fn native_overlay_templates_capture_the_whole_private_layout() {
    let (temp, catalog, mut spec, _source, ram) = fixture();
    spec.native_overlay = true;
    spec.generation = "native-overlay".into();
    let source = temp.path().join("capsule");
    spec.prepare_rootfs(
        &temp.path().join("image"),
        &temp.path().join("guest"),
        &source,
    )
    .unwrap();
    for path in ["rootfs", "upper", "work"] {
        assert!(source.join(path).is_dir());
    }
    assert!(source.join("rootfs/init.krun").is_file());
    assert!(!source.join("init.krun").exists());
    fs::write(source.join("upper/private"), b"captured upper state").unwrap();
    let version = publish(&catalog, "native", None, &spec, &source, &ram).unwrap();
    let lease = catalog
        .acquire_version(&version.id, &spec.compatibility)
        .unwrap();
    for destination in ["a", "b"] {
        lease
            .environment()
            .materialize(&temp.path().join(destination))
            .unwrap();
    }
    fs::write(temp.path().join("a/upper/private"), b"one instance only").unwrap();
    assert_eq!(
        fs::read(temp.path().join("b/upper/private")).unwrap(),
        b"captured upper state"
    );
}

#[test]
fn malformed_names_versions_readiness_and_symlinks_are_rejected() {
    let (temp, catalog, spec, source, ram) = fixture();
    for name in ["", "../escape", "name/path", ".", "bad\0name"] {
        assert!(catalog.selected(name).is_err());
    }
    let mut unknown = spec.clone();
    unknown.version = 99;
    assert!(unknown.id().is_err());
    let mut wrong_profile = spec.clone();
    wrong_profile.compatibility.profile = "pvisor-job-owned-overlay-v1".into();
    assert!(wrong_profile.id().is_err());
    let result = catalog.publish(
        "shell",
        None,
        &spec,
        TemplateCapture {
            ready: &Ready {
                version: 1,
                template_id: "f".repeat(64),
            },
            machine: b"state",
            ram: &ram,
            rootfs: &source,
        },
    );
    assert!(result.is_err());
    assert!(catalog.selected("shell").unwrap().is_none());
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&other, temp.path().join("symlink")).unwrap();
    assert!(TemplateCatalog::new(&temp.path().join("symlink")).is_err());
    std::os::unix::fs::symlink(
        temp.path().join("missing"),
        temp.path().join("catalog/aliases/evil.json"),
    )
    .unwrap();
    assert!(catalog.selected("evil").is_err()); // A dangling symlink is corruption, not absence.
}

#[test]
fn competing_builders_cannot_silently_replace_the_same_alias() {
    let (temp, catalog, spec, source, ram) = fixture();
    let mut other = spec.clone();
    other.generation = "generation-b".into();
    let second = temp.path().join("second-source");
    other
        .prepare_rootfs(
            &temp.path().join("image"),
            &temp.path().join("guest"),
            &second,
        )
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            publish(&catalog, "shell", None, &spec, &source, &ram)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            publish(&catalog, "shell", None, &other, &second, &ram)
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(
        catalog.selected("shell").unwrap().as_deref(),
        Some(winner.id.as_str())
    );
    let lease = catalog.acquire("shell", &spec.compatibility).unwrap();
    assert_eq!(lease.version().id, winner.id);
    catalog.collect().unwrap();
}

#[test]
fn interrupted_build_and_alias_publication_are_reclaimed_after_writer_exit() {
    use std::os::unix::fs::OpenOptionsExt;
    let (temp, catalog, _spec, _source, _ram) = fixture();
    let pending = temp.path().join("catalog/pending/generation-interrupted");
    fs::create_dir(&pending).unwrap();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o700)).unwrap();
    let writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(pending.join("writer.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&writer).unwrap();
    let stale_alias = temp.path().join("catalog/aliases/.selection-interrupted");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stale_alias)
        .unwrap();
    assert_eq!(catalog.collect().unwrap(), 0);
    assert!(!stale_alias.exists());
    assert!(pending.exists()); // Still-owned builder must survive GC.
    drop(writer);
    assert_eq!(catalog.collect().unwrap(), 1);
    assert!(!pending.exists());
}
