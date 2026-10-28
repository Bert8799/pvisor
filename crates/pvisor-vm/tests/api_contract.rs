//! Architectural guards and ownership checks; no hypervisor is started.
use pvisor_vm::api::RestoreState;
use pvisor_vm::api::{
    self, SnapshotControl, VmBuilder, VmConfig, VmConfiguration, VmControl, VmRuntime,
};
use syn::visit::{self, Visit};

#[test]
fn snapshot_json_remains_an_owned_opaque_wire_payload() {
    use api::SnapshotState;
    let json = String::from(
        r#"{
        "version":1, "cpus":[null,null], "devices":[],
        "kernel_layout":null,
        "ram":[{"base":0,"len":65536,"file_offset":0}],
        "future_extension":{"integer":18446744073709551615,"text":"opaque"}
    }"#,
    );
    let expected: serde_json::Value = serde_json::from_str(&json).unwrap();
    let snapshot: api::MachineSnapshot = serde_json::from_str(&json).unwrap();
    let cloned = snapshot.clone();
    drop(snapshot);
    drop(json);
    assert_eq!(serde_json::to_value(&cloned).unwrap(), expected);
    assert_eq!(cloned.cpu_count().unwrap(), 2);
    assert!(!cloned.has_kernel_layout().unwrap());
    let ram = cloned.ram_mappings().unwrap();
    assert_eq!(ram.len(), 1);
    assert_eq!((ram[0].base, ram[0].len, ram[0].file_offset), (0, 65536, 0));
    let file = tempfile::tempfile().unwrap();
    file.set_len(65536).unwrap();
    let restore = api::MachineRestore {
        state: cloned,
        ram_file: std::sync::Arc::new(file),
    };
    assert!(
        restore.validate(2).is_err(),
        "opaque payload is not valid CPU/device state"
    );
}

#[test]
fn snapshot_queries_reject_malformed_inventories_and_geometry() {
    use api::SnapshotState;
    for json in [r#"{}"#, r#"{"cpus":3,"ram":null}"#] {
        let snapshot: api::MachineSnapshot = serde_json::from_str(json).unwrap();
        assert!(snapshot.cpu_count().is_err());
        assert!(snapshot.ram_mappings().is_err());
    }
    for geometry in [
        serde_json::json!(true),
        serde_json::json!(3),
        serde_json::json!([]),
        serde_json::json!("bad"),
    ] {
        let snapshot: api::MachineSnapshot =
            serde_json::from_value(serde_json::json!({"kernel_layout":geometry})).unwrap();
        assert_eq!(
            snapshot.has_kernel_layout().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn snapshot_rebind_failure_preserves_serialized_bytes() {
    use api::{SnapshotRebind, SnapshotState};
    let mut value = serde_json::json!({"version":1,"cpus":[],"devices":[],"ram":[]});
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        value["pio_devices"] = serde_json::json!([]);
    }
    let json = serde_json::to_string_pretty(&value).unwrap();
    let mut snapshot: api::MachineSnapshot = serde_json::from_str(&json).unwrap();
    let before = serde_json::to_vec(&snapshot).unwrap();
    assert!(snapshot
        .rebind_all(&[SnapshotRebind::VsockPorts { ports: &[] }])
        .is_err());
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), before);
}

#[test]
fn embedded_guest_is_an_immutable_optional_bootstrap() {
    use api::RuntimeSupport;
    let bytes = api::VmPlatform::embedded_guest();
    if cfg!(feature = "init-blob") {
        let bytes = bytes.expect("enabled embedded guest");
        assert!(bytes.starts_with(b"\x7fELF"));
        assert!(std::ptr::eq(
            bytes,
            api::VmPlatform::embedded_guest().unwrap()
        ));
    } else {
        assert!(bytes.is_none());
    }
}

#[test]
fn read_only_filesystem_configuration_is_atomic_without_dax() {
    let root = tempfile::tempdir().unwrap();
    let mut builder = VmBuilder::new(1, 128).unwrap();
    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    {
        assert_eq!(
            builder
                .read_only_filesystem("data", std::path::Path::new("relative"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        builder.read_only_filesystem("data", root.path()).unwrap();
        assert!(builder.read_only_filesystem("data", root.path()).is_err());
        // An ordinary read-only share does not acquire DAX's snapshot exclusion.
        builder.snapshot_profile().unwrap();
    }
    #[cfg(any(feature = "tee", feature = "aws-nitro"))]
    assert_eq!(
        builder
            .read_only_filesystem("data", root.path())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}

#[test]
fn immutable_dax_configuration_validates_before_attachment_and_rejects_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let mut builder = VmBuilder::new(1, 128).unwrap();
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        not(any(feature = "tee", feature = "aws-nitro"))
    ))]
    {
        for size in [0, 4096, (2 << 20) + 4096, (1 << 30) + (2 << 20)] {
            assert_eq!(
                builder
                    .immutable_filesystem("data", root.path(), size)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            builder
                .immutable_filesystem("data", std::path::Path::new("relative"), 2 << 20)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        builder
            .immutable_filesystem("data", root.path(), 2 << 20)
            .unwrap();
        assert!(builder
            .immutable_filesystem("data", root.path(), 2 << 20)
            .is_err());
        assert_eq!(
            builder.snapshot_profile().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        builder.filesystem("ordinary", root.path(), 0).unwrap();
        let mut snapshots = VmBuilder::new(1, 128).unwrap();
        snapshots.snapshot_profile().unwrap();
        assert_eq!(
            snapshots
                .immutable_filesystem("data", root.path(), 2 << 20)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        snapshots.filesystem("data", root.path(), 0).unwrap();
    }
    #[cfg(not(all(
        target_os = "linux",
        target_arch = "x86_64",
        not(any(feature = "tee", feature = "aws-nitro"))
    )))]
    assert_eq!(
        builder
            .immutable_filesystem("data", root.path(), 2 << 20)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}

#[test]
fn api_is_unconditional_and_contains_no_implementation() {
    struct Guard;
    impl<'ast> Visit<'ast> for Guard {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            assert!(
                !attr.path().is_ident("cfg") && !attr.path().is_ident("cfg_attr"),
                "API shape must not depend on a target/feature"
            );
            visit::visit_attribute(self, attr);
        }
        fn visit_item_impl(&mut self, _: &'ast syn::ItemImpl) {
            panic!("API implementations must live in private adapters")
        }
        fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {
            panic!("API function bodies must live in private adapters")
        }
        fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
            assert!(
                item.default.is_none(),
                "API traits declare contracts, not default implementations"
            );
            visit::visit_trait_item_fn(self, item);
        }
    }
    Guard.visit_file(&syn::parse_file(include_str!("../src/api.rs")).unwrap());
    let modules = syn::parse_file(include_str!("../src/runtime_modules.rs")).unwrap();
    let public = modules
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(m) if matches!(m.vis, syn::Visibility::Public(_)) => {
                Some(m.ident.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(public, ["api"]);
}

#[test]
fn concrete_owners_implement_the_declared_contracts() {
    fn config<T: VmConfiguration + VmRuntime>() {}
    fn control<
        T: VmControl
            + SnapshotControl
            + api::ColdRamControl
            + api::RamDedupControl
            + api::VcpuObservationControl,
    >() {
    }
    fn frozen<T: api::SnapshotCapture + api::FrozenMemory>() {}
    fn snapshot<T: api::SnapshotState>() {}
    fn restore<T: api::RestoreState>() {}
    fn ram<T: api::RamAccess>() {}
    config::<VmBuilder>();
    control::<api::VmmHandle>();
    frozen::<api::FrozenMachine<'_>>();
    snapshot::<api::MachineSnapshot>();
    restore::<api::MachineRestore>();
    ram::<api::RamBlock>();
    fn platform<T: api::RuntimeSupport>() {}
    platform::<api::VmPlatform>();
    let _object_safe: Option<&dyn VmControl> = None;
    let _dedup_object_safe: Option<&dyn api::RamDedupControl> = None;
    let _observation_object_safe: Option<&dyn api::VcpuObservationControl> = None;
    assert!(VmBuilder::from_config(VmConfig {
        cpus: 0,
        memory_mib: 128
    })
    .is_err());
    assert!(VmBuilder::from_config(VmConfig {
        cpus: 1,
        memory_mib: 0
    })
    .is_err());
}

#[test]
fn invalid_attachment_does_not_poison_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut vm = VmBuilder::new(1, 128).unwrap();
    vm.filesystem("/dev/root", root.path(), 0).unwrap();
    assert!(vm.disable_implicit_init().is_err());
    assert!(vm.filesystem("/dev/root", root.path(), 0).is_err());
    assert!(vm
        .virtual_file("/dev/root", "/../escape", vec![1], 0o400, false)
        .is_err());
    vm.virtual_file("/dev/root", "/config", vec![1, 2, 3], 0o400, false)
        .unwrap();
    assert!(vm
        .virtual_file("/dev/root", "/config", vec![4], 0o400, false)
        .is_err());
    vm.virtual_file("/dev/root", "/another", vec![5], 0o400, false)
        .unwrap();
}

#[test]
fn snapshot_ram_shape_is_portable_and_mapping_is_private() {
    use std::sync::Arc;
    use vm_memory::{Bytes, GuestAddress};
    let file = tempfile::tempfile().unwrap();
    // macOS host pages are 16 KiB, Linux usually uses 4 KiB.
    file.set_len(65536).unwrap();
    let state = serde_json::from_value(serde_json::json!({
        "version":1,"cpus":[],"devices":[],
        "ram":[{"base":0,"len":65536,"file_offset":0}]
    }))
    .unwrap();
    let restore = api::MachineRestore {
        state,
        ram_file: Arc::new(file),
    };
    let first = restore.map_ram(&[(GuestAddress(0), 65536)]).unwrap();
    let second = restore.map_ram(&[(GuestAddress(0), 65536)]).unwrap();
    first.write_slice(b"private", GuestAddress(0)).unwrap();
    let mut bytes = [1; 7];
    second.read_slice(&mut bytes, GuestAddress(0)).unwrap();
    assert_eq!(bytes, [0; 7]);
    assert!(restore.map_ram(&[(GuestAddress(4096), 65536)]).is_err());
    assert!(restore.validate(1).is_err()); // RAM mapping is not full-machine validation.
}

#[test]
fn private_adapters_do_not_define_public_inherent_methods() {
    for source in [
        include_str!("../src/portable.rs"),
        include_str!("../src/handle.rs"),
        include_str!("../src/memory.rs"),
        include_str!("../src/ram_dedup.rs"),
        include_str!("../src/vcpu_observation.rs"),
        include_str!("../src/firmware_store.rs"),
        include_str!("../src/cold_ram.rs"),
        include_str!("../src/ram_file.rs"),
    ] {
        for item in syn::parse_file(source).unwrap().items {
            if let syn::Item::Impl(implementation) = item {
                if implementation.trait_.is_none() {
                    for member in implementation.items {
                        if let syn::ImplItem::Fn(method) = member {
                            assert!(
                                !matches!(method.vis, syn::Visibility::Public(_)),
                                "public methods must be declared in api.rs traits: {}",
                                method.sig.ident
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn invalid_overlay_path_does_not_poison_the_builder() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let lower = root.path().join("lower");
    let upper = root.path().join("upper");
    std::fs::create_dir_all(&lower).unwrap();
    std::fs::create_dir_all(&upper).unwrap();
    let mut builder = VmBuilder::new(1, 64).unwrap();
    let mut overlay = api::OverlayConfig {
        lower_mutability: Vec::new(),
        lower_dirs: vec![lower],
        upper_dir: upper,
        work_dir: None,
        preimage_dir: None,
        apply_target: None,
        baseline_lower: None,
        baseline_content_index: None,
        excluded_paths: vec![],
        access_policy: Default::default(),
        semantics: api::PermissionSemantics::LinuxComplete,
    };
    assert!(overlay.lower_mutability.is_empty());
    let valid = overlay.clone();
    let mut invalid_mutability = valid.clone();
    invalid_mutability.lower_mutability = vec![pvisor_overlay_core::LayerMutability::Immutable; 2];
    assert_eq!(
        builder
            .overlay("workspace", invalid_mutability, 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    overlay
        .excluded_paths
        .push(std::ffi::OsString::from_vec(vec![0xff]).into());
    assert_eq!(
        builder.overlay("workspace", overlay, 0).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    let mut invalid_index = valid.clone();
    invalid_index.baseline_content_index = Some(api::BaselineContentIndex {
        root: std::ffi::OsString::from_vec(vec![0xff]).into(),
        file: root.path().join("index"),
        sha256: "00".repeat(32),
    });
    assert_eq!(
        builder
            .overlay("workspace", invalid_index, 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    builder.overlay("workspace", valid, 0).unwrap();
}

#[test]
fn embedded_kernel_identity_matches_uncompressed_image() {
    use api::RuntimeSupport;
    use sha2::{Digest, Sha256};

    let digest = api::VmPlatform::embedded_kernel_sha256();
    let image = api::VmPlatform::embedded_kernel();
    assert_eq!(digest.is_some(), image.is_some());
    if let Some(image) = image {
        let expected: [u8; 32] = Sha256::digest(&image.bytes).into();
        assert_eq!(digest, Some(expected));
    }
    assert_eq!(api::VmPlatform::embedded_kernel_sha256(), digest);
}

#[test]
fn snapshot_batch_contract_rejects_missing_devices_without_changing_state() {
    use api::{SnapshotRebind, SnapshotState};
    let mut original = serde_json::json!({
        "version": 1, "cpus": [], "devices": [], "ram": []
    });
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        original["pio_devices"] = serde_json::json!([]);
    }
    let mut state: api::MachineSnapshot = serde_json::from_value(original.clone()).unwrap();
    let empty = state.rebind_all(&[]);
    if cfg!(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    )) {
        empty.unwrap();
        for changes in [
            vec![SnapshotRebind::VsockPorts { ports: &[] }],
            vec![SnapshotRebind::UnmountedFilesystem { tag: "workspace" }],
            vec![SnapshotRebind::FilesystemExclusions {
                tag: "",
                paths: &[],
            }],
        ] {
            assert!(state.rebind_all(&changes).is_err());
            assert_eq!(serde_json::to_value(&state).unwrap(), original);
        }
    } else {
        assert_eq!(empty.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }
}
