//! Inert templates reuse the native runner, live controls and Attempt teardown.
use super::supported::{OverlayDeviceSpec, RunnerSpec};
use crate::environment_snapshot::templates::{TemplateCatalog, TemplateLease, TemplateSpec};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::DirBuilderExt;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launch {
    binding: crate::environment_snapshot::templates::TemplateBinding,
    source: PathBuf,
    boot: pvisor_guest::template::Template,
    instance_id: String,
    copies: Vec<(PathBuf, PathBuf)>,
}

pub(super) struct Prepared {
    pub ram: Arc<File>,
    pub ram_path: PathBuf,
    root: OverlayDeviceSpec,
    workspace_device: bool,
    launch: Launch,
    _lease: TemplateLease,
    claimed: std::sync::atomic::AtomicBool,
}
impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedTemplate")
            .field("id", &self.launch.boot.template_id)
            .finish_non_exhaustive()
    }
}

impl Prepared {
    pub(super) fn root_overlay(&self) -> OverlayDeviceSpec {
        self.root.clone()
    }
    pub(super) fn rootfs(&self) -> PathBuf {
        self.root
            .lowers
            .last()
            .expect("template root lower")
            .clone()
    }
}

fn root(capsule: &Path, network: bool) -> OverlayDeviceSpec {
    let mut lowers = Vec::new();
    let mut mutability = Vec::new();
    if network {
        lowers.push(capsule.join("network-lower"));
        mutability.push(pvisor_overlay_core::LayerMutability::Immutable);
    }
    lowers.push(capsule.join("rootfs"));
    mutability.push(pvisor_overlay_core::LayerMutability::Mutable);
    OverlayDeviceSpec {
        lowers,
        lower_mutability: mutability,
        apply_target: None,
        baseline_lower: None,
        upper: capsule.join("upper"),
        work: Some(capsule.join("work")),
        preimages: None,
        excluded: vec![],
        access_policy: Default::default(),
    }
}

pub(super) fn prepare(
    settings: &crate::VmSettings,
    lease: TemplateLease,
    storage: &Path,
) -> anyhow::Result<Prepared> {
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "native templates require Linux x86_64"
    );
    ensure!(
        settings.prepared_rootfs.is_none()
            && settings.image.is_none()
            && settings.ram_backing.is_none()
            && settings.memory_pool.is_none()
            && settings.node_socket.is_none()
            && settings.snapshot_filesystem_pool.is_none()
            && !settings.ram_dedup
            && !settings.ram_compression
            && !settings.cold_pager_requested(),
        "template restore requires private COW RAM without additional backing/pool modes"
    );
    let spec = &lease.version().spec;
    ensure!(
        lease.environment().manifest().ram_blocks.is_none(),
        "native templates require a raw shared RAM baseline, without eager decode"
    );
    let mut expected = super::supported::VmExecutor::checkpoint_compatibility(settings)?;
    expected.profile = crate::environment_snapshot::templates::PROFILE.into();
    ensure!(
        spec.compatibility == expected
            && spec.native_overlay
            && spec.cpus as u16 == settings.cpus
            && spec.memory_mib == settings.memory_mib
            && spec.stdio_ports == [true; 3]
            && spec.network_mac == Some(pvisor_overlaynet::vm::VM_MAC),
        "native template build, resources or device profile mismatch"
    );
    fs::create_dir_all(storage)?;
    let preparation = storage.canonicalize()?.join("template-restore");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&preparation)
        .context("template preparation already exists")?;
    let capsule = preparation.join("capsule");
    fs::create_dir(&capsule)?;
    for name in ["upper", "work"] {
        lease
            .environment()
            .materialize_layer(Path::new(name), &capsule.join(name))?;
    }
    use std::os::unix::ffi::OsStrExt;
    let source = PathBuf::from(std::ffi::OsStr::from_bytes(
        &lease.environment().manifest().source_root,
    ));
    let ram = Arc::new(lease.environment().template_ram_file()?);
    // Path is used only for authority exclusions/physical observation. The exact
    // read-only inode is passed to the child by descriptor and mapped MAP_PRIVATE.
    let ram_path = fs::read_link(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&*ram)
    ))?;
    let mut restored_root = root(&capsule, true);
    // Keep sealed lower trees under the generation lease and the runner's
    // read-only authority, sharing image file-cache pages between instances.
    restored_root.lowers = ["network-lower", "rootfs"]
        .iter()
        .map(|name| lease.environment().owned_layer_path(Path::new(name)))
        .collect::<anyhow::Result<_>>()?;
    let copies = vec![
        (
            source.join("network-lower"),
            restored_root.lowers[0].clone(),
        ),
        (source.join("rootfs"), restored_root.lowers[1].clone()),
        (source.join("upper"), restored_root.upper.clone()),
        (source.join("work"), restored_root.work.clone().unwrap()),
    ];
    // SnapshotState drops old immutable-owner promises when rebinding copies.
    // Configure the replacement devices with that same new backing lifetime.
    restored_root
        .lower_mutability
        .fill(pvisor_overlay_core::LayerMutability::Mutable);
    Ok(Prepared {
        ram,
        ram_path,
        root: restored_root,
        workspace_device: spec.workspace_device,
        launch: Launch {
            binding: lease.binding(),
            source,
            boot: spec.boot()?,
            instance_id: String::new(),
            copies,
        },
        _lease: lease,
        claimed: std::sync::atomic::AtomicBool::new(false),
    })
}

pub(super) fn apply(
    prepared: &Prepared,
    runner: &mut RunnerSpec,
    instance_id: &str,
) -> anyhow::Result<()> {
    ensure!(
        runner.restore.is_none()
            && runner.checkpoint.is_none()
            && runner.cpu_qos.is_none()
            && runner.guest.network.is_some(),
        "template launch cannot override checkpoint or CPU QoS profiles"
    );
    ensure!(
        runner.workspace.is_some() == prepared.workspace_device,
        "template workspace device profile mismatch"
    );
    ensure!(
        !prepared
            .claimed
            .swap(true, std::sync::atomic::Ordering::AcqRel),
        "template executor was already consumed"
    );
    ensure!(
        runner.root.lowers == prepared.root.lowers && runner.root.upper == prepared.root.upper,
        "template root backing changed"
    );
    let mut launch = prepared.launch.clone();
    launch.instance_id = instance_id.into();
    runner.template = Some(launch);
    Ok(())
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub(super) fn machine_restore(
    spec: &RunnerSpec,
    ram: File,
    lease: &TemplateLease,
) -> anyhow::Result<pvisor_vm::api::MachineRestore> {
    use pvisor_vm::api::{RestoreState, SnapshotState};
    let launch = spec.template.as_ref().context("missing template launch")?;
    let mut state: pvisor_vm::api::MachineSnapshot =
        serde_json::from_slice(&lease.environment().machine_bytes()?)?;
    let shared = [
        launch.source.join("rootfs"),
        launch.source.join("network-lower"),
    ];
    ensure!(
        state.rebind_filesystem_shared_lowers("/dev/root", &launch.copies, &shared)? == 1,
        "template root device is missing"
    );
    ensure!(
        state.rebind_filesystem_exclusions("/dev/root", &spec.root.excluded)? == 1,
        "template exclusions are missing"
    );
    ensure!(
        state.rebind_filesystem_policy("/dev/root", &spec.root.access_policy)? == 1,
        "template policy is missing"
    );
    if let Some(workspace) = &spec.workspace {
        ensure!(
            lease.version().spec.workspace_device,
            "template lacks an unused workspace device"
        );
        ensure!(
            state.reset_unmounted_filesystem("pvisor-workspace")? == 1,
            "template workspace device is missing or already used"
        );
        ensure!(!workspace.lowers.is_empty(), "workspace lower is missing");
    }
    let ports = spec
        .vsock_ports
        .iter()
        .map(|(port, path)| (*port, path.clone(), true))
        .collect::<Vec<_>>();
    ensure!(
        state.rebind_vsock_ports(&ports)? == 1,
        "template vsock is missing"
    );
    let restore = pvisor_vm::api::MachineRestore {
        state,
        ram_file: Arc::new(ram),
    };
    restore
        .validate(spec.cpus as usize)
        .map_err(anyhow::Error::msg)?;
    Ok(restore)
}

pub(super) fn runner_lease(spec: &RunnerSpec) -> anyhow::Result<Option<TemplateLease>> {
    let Some(launch) = &spec.template else {
        return Ok(None);
    };
    ensure!(
        launch.binding.id == launch.boot.template_id,
        "template runner identity mismatch"
    );
    let lease = TemplateCatalog::new(&launch.binding.catalog)?
        .acquire_version(&launch.binding.id, &launch.binding.compatibility)?;
    let recipe = &lease.version().spec;
    ensure!(
        recipe.native_overlay && recipe.cpus == spec.cpus && recipe.memory_mib == spec.memory_mib,
        "template runner resource profile mismatch"
    );
    ensure!(
        recipe.workspace_device == spec.workspace.is_some(),
        "template workspace topology changed"
    );
    let lowers = ["network-lower", "rootfs"]
        .iter()
        .map(|name| lease.environment().owned_layer_path(Path::new(name)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    ensure!(
        lowers == spec.root.lowers,
        "template runner shared lower binding changed"
    );
    Ok(Some(lease))
}

pub(super) struct Handoff {
    pub writer: std::os::unix::net::UnixStream,
    pub reader: std::os::unix::net::UnixStream,
    pub task: pvisor_guest::template::Task,
}

pub(super) fn handoff(
    spec: &RunnerSpec,
    guest: &pvisor_guest::GuestConfig,
) -> anyhow::Result<Option<Handoff>> {
    let Some(launch) = &spec.template else {
        return Ok(None);
    };
    ensure!(
        guest.stdio_ports == Some(launch.boot.stdio_ports),
        "template stdio topology changed"
    );
    ensure!(
        !guest.env.contains_key(pvisor_guest::template::INSTANCE_ENV),
        "template instance identity is reserved"
    );
    let mut config = guest.clone();
    let process = config.process.get_or_insert_with(Default::default);
    ensure!(
        process.tty.is_none(),
        "template handoff does not support a PTY"
    );
    process.stdin = pvisor_guest::StdinMode::Closed;
    let entropy = entropy()?;
    let task = pvisor_guest::template::Task {
        version: 1,
        template_id: launch.boot.template_id.clone(),
        instance_id: launch.instance_id.clone(),
        entropy,
        config,
    };
    let (writer, reader) = std::os::unix::net::UnixStream::pair()?;
    writer.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    Ok(Some(Handoff {
        writer,
        reader,
        task,
    }))
}

fn entropy() -> anyhow::Result<[u8; 32]> {
    #[cfg(target_os = "linux")]
    {
        let mut entropy = [0u8; 32];
        let mut offset = 0;
        while offset < entropy.len() {
            let count = unsafe {
                libc::getrandom(
                    entropy[offset..].as_mut_ptr().cast(),
                    entropy.len() - offset,
                    0,
                )
            };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            ensure!(count > 0, "host entropy unavailable");
            offset += count as usize;
        }
        Ok(entropy)
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("native template handoff requires Linux")
}

/// This function is invoked only in the daemon's isolated, synchronous preparer.
#[allow(clippy::too_many_arguments)]
pub(super) fn capture(
    executor: &super::supported::VmExecutor,
    catalog: TemplateCatalog,
    name: &str,
    expected: Option<&str>,
    guest: &Path,
    stage: &Path,
    generation: &str,
    workspace_device: bool,
) -> anyhow::Result<()> {
    use pvisor_vm::api::{SnapshotCapture, SnapshotControl, VmConfiguration, VmRuntime};
    use std::{io::Write, os::fd::AsRawFd, os::unix::fs::OpenOptionsExt};
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "native templates require Linux x86_64"
    );
    let settings = executor.settings();
    ensure!(
        settings.prepared_rootfs.is_none()
            && settings.memory_pool.is_none()
            && settings.ram_backing.is_none(),
        "template preparation requires a plain immutable image"
    );
    let image = settings
        .rootfs
        .as_ref()
        .context("template rootfs is required")?;
    let image = catalog.validate_source(image)?;
    let stage = catalog.validate_source(stage)?;
    ensure!(
        !image.starts_with(&stage) && !stage.starts_with(&image),
        "template image and staging must be independent"
    );
    let mut compatibility = super::supported::VmExecutor::checkpoint_compatibility(settings)?;
    compatibility.profile = crate::environment_snapshot::templates::PROFILE.into();
    let spec = TemplateSpec {
        version: 1,
        generation: generation.into(),
        image_sha256: TemplateSpec::image_id(&image)?,
        guest_sha256: crate::environment_snapshot::file_hash(guest)?,
        cpus: u8::try_from(settings.cpus)?,
        memory_mib: settings.memory_mib,
        compatibility,
        stdio_ports: [true; 3],
        network_mac: Some(pvisor_overlaynet::vm::VM_MAC),
        native_overlay: true,
        workspace_device,
    };
    let capsule = stage.canonicalize()?.join("capsule");
    spec.prepare_rootfs(&image, guest, &capsule)?;
    let mut overlay = root(&capsule, false);
    super::supported::prepare_network_resolver(&mut overlay, &capsule, true)?;
    super::supported::protect_overlay_backing(&mut overlay, None)?;
    let mut vm = pvisor_vm::api::VmBuilder::new(spec.cpus, spec.memory_mib)?;
    super::supported::configure_runner_firmware(&mut vm, executor.firmware_path())?;
    vm.disable_implicit_init()?;
    vm.snapshot_profile()?;
    super::supported::add_vm_overlay(&mut vm, "/dev/root", &overlay, None)?;
    if workspace_device {
        let unused = capsule.join("unused-workspace");
        fs::create_dir(&unused)?;
        // PID 1 has no workspace configuration before handoff, so this device
        // is discovered but never mounted or issued a filesystem request.
        vm.filesystem("pvisor-workspace", &unused, 0)?;
    }
    let (input, input_vm) = std::os::unix::net::UnixStream::pair()?;
    let (mut output, output_vm) = std::os::unix::net::UnixStream::pair()?;
    let (mut errors, errors_vm) = std::os::unix::net::UnixStream::pair()?;
    output.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    vm.task_stdio(
        input_vm.as_raw_fd(),
        output_vm.as_raw_fd(),
        errors_vm.as_raw_fd(),
    )?;
    let (backend, backend_peer) = std::os::unix::net::UnixStream::pair()?;
    vm.network(backend, pvisor_overlaynet::vm::VM_MAC)?;
    let name = name.to_owned();
    let expected = expected.map(str::to_owned);
    let stage = stage.to_owned();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut errors, &mut std::io::stderr());
    });
    let result = vm.run(move |handle| {
        std::thread::Builder::new()
            .name("pvisor-template-capture".into())
            .spawn(move || {
                let outcome = (|| -> anyhow::Result<()> {
                    let ready: pvisor_guest::template::Ready =
                        pvisor_guest::control::read_frame(&mut output)?;
                    handle
                        .with_snapshot_frozen(std::time::Duration::from_secs(20), |machine| {
                            let result = (|| -> anyhow::Result<()> {
                                let ram = fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .create_new(true)
                                    .mode(0o600)
                                    .open(stage.join("capture.ram"))?;
                                let state = machine
                                    .capture_machine_state(&ram)
                                    .map_err(anyhow::Error::msg)?;
                                let record = catalog.publish(
                                    &name,
                                    expected.as_deref(),
                                    &spec,
                                    crate::environment_snapshot::templates::TemplateCapture {
                                        ready: &ready,
                                        machine: &serde_json::to_vec(&state)?,
                                        ram: &ram,
                                        rootfs: &capsule,
                                    },
                                )?;
                                serde_json::to_writer(std::io::stdout(), &record)?;
                                std::io::stdout().write_all(b"\n")?;
                                Ok(())
                            })();
                            result.map_err(|e| e.to_string())
                        })
                        .map_err(anyhow::Error::msg)
                })();
                if let Err(error) = outcome {
                    eprintln!("template capture failed: {error:#}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            })?;
        Ok(())
    });
    drop((input, input_vm, output_vm, errors_vm, backend_peer));
    result?;
    anyhow::bail!("template VM exited without publication")
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    fn runner() -> RunnerSpec {
        let mut spec: RunnerSpec = serde_json::from_value(serde_json::json!({
            "setup_attestation":"/tmp/attestation",
            "root":{"lowers":["/tmp/rootfs"],"upper":"/tmp/upper"},
            "guest":{"argv":["/bin/echo","new-argument"],"env":{"SECRET":"new-secret"},"cwd":"/new-directory","stdio_ports":[true,true,true]},
            "cpus":2,"memory_mib":128,
        })).unwrap();
        spec.template = Some(Launch {
            binding: crate::environment_snapshot::templates::TemplateBinding {
                catalog: "/tmp/catalog".into(),
                id: "a".repeat(64),
                compatibility: crate::environment_snapshot::Compatibility {
                    host_boot: "boot".into(),
                    build: "build".into(),
                    firmware: "firmware".into(),
                    profile: "profile".into(),
                },
            },
            source: "/tmp/source".into(),
            boot: pvisor_guest::template::Template {
                version: 1,
                template_id: "a".repeat(64),
                stdio_ports: [true; 3],
            },
            instance_id: "new-attempt".into(),
            copies: vec![],
        });
        spec
    }
    #[test]
    fn host_handoff_uses_fresh_entropy_and_the_new_task_contract() {
        let spec = runner();
        let first = handoff(&spec, &spec.guest).unwrap().unwrap().task;
        let second = handoff(&spec, &spec.guest).unwrap().unwrap().task;
        assert_ne!(first.entropy, [0; 32]);
        assert_ne!(first.entropy, second.entropy);
        assert!(spec.guest.process.is_none()); // Do not mutate the caller's options.
        let mut wire = Vec::new();
        pvisor_guest::control::write_frame(&mut wire, &first).unwrap();
        let mut output = Vec::new();
        let guest = spec
            .template
            .as_ref()
            .unwrap()
            .boot
            .receive(&mut std::io::Cursor::new(wire), &mut output, |entropy| {
                assert_eq!(*entropy, first.entropy);
                Ok(())
            })
            .unwrap();
        assert_eq!(guest.argv, ["/bin/echo", "new-argument"]);
        assert_eq!(guest.cwd, Path::new("/new-directory"));
        assert_eq!(guest.env["SECRET"], "new-secret");
        assert_eq!(
            guest.env[pvisor_guest::template::INSTANCE_ENV],
            "new-attempt"
        );
        assert!(matches!(
            guest.process.unwrap().stdin,
            pvisor_guest::StdinMode::Closed
        ));
    }
    #[test]
    fn handoff_rejects_console_or_reserved_identity_overrides() {
        let mut spec = runner();
        spec.guest.stdio_ports = Some([true, true, false]);
        assert!(handoff(&spec, &spec.guest).is_err());
        spec.guest.stdio_ports = Some([true; 3]);
        spec.guest.env.insert(
            pvisor_guest::template::INSTANCE_ENV.into(),
            "old-instance".into(),
        );
        assert!(handoff(&spec, &spec.guest).is_err());
        spec.guest.env.clear();
        spec.guest.process = Some(pvisor_guest::ProcessConfig {
            tty: Some([24, 80]),
            new_session: true,
            ..Default::default()
        });
        assert!(handoff(&spec, &spec.guest).is_err());
    }
}
