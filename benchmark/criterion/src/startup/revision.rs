//! Benchmark: B-STARTUP-REVISION-ENG; one frozen CLI version per invocation.
//! Ready and normal Exit use the shared observer and fresh VM Sessions.
//! Criterion controls sampling; correctness, evidence and teardown are untimed.
mod source;
mod statistics;

use super::{
    evidence::{self, digest, read, save},
    observer::{self, Observation},
    prepared::Affinity,
    subjects::PAYLOAD,
};
use anyhow::{Context, Result, ensure};
use criterion::{Criterion, SamplingMode};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::{fs::MetadataExt, fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

const WORKER: &str = "PVISOR_BENCH_REVISION_WORKER";
const CANARY: &[u8] = b"unchanged startup input\n";

struct Namespace(Option<Child>);
impl Drop for Namespace {
    fn drop(&mut self) {
        // unshare forwards its death to PID 1; the kernel reaps the namespace.
        if let Some(child) = &mut self.0 {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
        }
    }
}

pub(super) fn enter_worker() -> Result<()> {
    if env::var_os(WORKER).is_some() {
        ensure!(
            unsafe { libc::getpid() } == 1,
            "worker requires private PID namespace"
        );
        private_root()?;
    }
    Ok(())
}

fn mount(args: &[&std::ffi::OsStr]) -> Result<()> {
    ensure!(
        Command::new("mount").args(args).status()?.success(),
        "private mount failed"
    );
    Ok(())
}

fn private_root() -> Result<()> {
    use std::ffi::{CString, OsStr};
    let root =
        PathBuf::from(env::var_os("PVISOR_BENCH_REVISION_ROOT").context("missing private root")?);
    mount(&[
        OsStr::new("-t"),
        OsStr::new("tmpfs"),
        OsStr::new("-o"),
        OsStr::new("mode=700,nosuid,nodev"),
        OsStr::new("tmpfs"),
        root.as_os_str(),
    ])?;
    for name in [
        "/home", "/usr", "/etc", "/dev", "/proc", "/bin", "/sbin", "/lib", "/lib64", "/run",
        "/var", "/sys",
    ] {
        let source = Path::new(name);
        if !source.exists() {
            continue;
        }
        let dest = root.as_path().join(name.trim_start_matches('/'));
        if fs::symlink_metadata(source)?.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(source)?, dest)?;
        } else {
            fs::create_dir(&dest)?;
            mount(&[OsStr::new("--rbind"), source.as_os_str(), dest.as_os_str()])?;
        }
    }
    fs::create_dir(root.as_path().join("tmp"))?;
    fs::set_permissions(
        root.as_path().join("tmp"),
        fs::Permissions::from_mode(0o1777),
    )?;
    fs::create_dir(root.as_path().join(".old-root"))?;
    let new = CString::new(root.as_path().as_os_str().as_encoded_bytes())?;
    let old = CString::new(
        root.as_path()
            .join(".old-root")
            .as_os_str()
            .as_encoded_bytes(),
    )?;
    std::env::set_current_dir(root.as_path())?;
    ensure!(
        unsafe { libc::syscall(libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) } == 0,
        "private pivot_root: {}",
        std::io::Error::last_os_error()
    );
    std::env::set_current_dir("/")?;
    ensure!(
        unsafe { libc::umount2(c"/.old-root".as_ptr(), libc::MNT_DETACH) } == 0,
        "detach original root: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn output() -> Result<PathBuf> {
    let path = PathBuf::from(env::var_os("PVISOR_BENCH_OUTPUT").context("missing output")?);
    ensure!(
        path.is_absolute(),
        "revision evidence path must be absolute"
    );
    fs::create_dir_all(path.parent().context("output parent")?)?;
    let parent = path.parent().unwrap().canonicalize()?;
    ensure!(
        parent.components().any(|p| p.as_os_str() == ".data"),
        "evidence requires .data"
    );
    ensure!(
        !parent.starts_with("/tmp"),
        "evidence must survive private /tmp"
    );
    Ok(parent.join(path.file_name().context("output leaf")?))
}

pub(super) fn register(criterion: &mut Criterion) -> Result<()> {
    if env::var_os(WORKER).is_none() {
        return supervise();
    }
    let output = output()?;
    fs::create_dir(&output).context("evidence directory must be new")?;
    save(output.join("status.json"), &json!({"status":"preparing"}))?;
    let result = run(criterion, &output);
    if let Err(error) = &result {
        save(
            output.join("status.json"),
            &json!({"status":"failed","error":format!("{error:#}")}),
        )?;
    }
    result
}

fn supervise() -> Result<()> {
    let output = output()?;
    ensure!(!output.exists(), "evidence directory must be new");
    let private_root = tempfile::Builder::new()
        .prefix("pvisor-revision-root-")
        .tempdir_in("/tmp")?;
    let parent = unsafe { libc::getpid() };
    let mut command = Command::new("unshare");
    command
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "--kill-child=KILL",
            "--mount-proc",
            "--propagation",
            "private",
        ])
        .arg(env::current_exe()?)
        .args(env::args_os().skip(1))
        .env(WORKER, "1")
        .env("PVISOR_BENCH_REVISION_ROOT", private_root.path())
        .process_group(0);
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent
            {
                return Err(std::io::Error::other("benchmark supervisor disappeared"));
            }
            Ok(())
        });
    }
    let mut child = Namespace(Some(
        command
            .spawn()
            .context("spawn private benchmark namespace")?,
    ));
    let status = child.0.as_mut().unwrap().wait()?;
    child.0.take();
    private_root
        .close()
        .context("remove owned namespace mountpoint")?;
    let containment = read(output.join("containment.json"))?;
    let namespace = containment["pid_namespace"]
        .as_str()
        .context("missing namespace identity")?;
    let mut survivors = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let path = entry?.path();
        if fs::read_link(path.join("ns/pid"))
            .ok()
            .is_some_and(|p| p.to_string_lossy() == namespace)
        {
            survivors.push(path);
        }
    }
    save(
        output.join("cleanup.json"),
        &json!({"namespace_exit":status.code(),
        "owned_namespace_survivors":survivors,"valid":status.success() && survivors.is_empty(),
        "scope":"private PID/mount/user namespaces; no existing service is stopped"}),
    )?;
    ensure!(
        status.success() && survivors.is_empty(),
        "revision cohort or namespace cleanup failed"
    );
    Ok(())
}

fn required_path(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(env::var_os(name).with_context(|| format!("{name} is required"))?)
        .canonicalize()?;
    ensure!(
        !path.starts_with("/tmp"),
        "{name} must survive private /tmp"
    );
    Ok(path)
}

fn tree(root: &Path) -> Result<BTreeMap<PathBuf, Value>> {
    fn walk(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Value>) -> Result<()> {
        let info = fs::symlink_metadata(path)?;
        let value = if info.is_symlink() {
            json!({"symlink":fs::read_link(path)?})
        } else if info.is_file() {
            json!({"sha256":digest(path)?})
        } else {
            ensure!(info.is_dir(), "unsupported input node {}", path.display());
            for entry in fs::read_dir(path)? {
                walk(root, &entry?.path(), result)?;
            }
            json!({"directory":true})
        };
        result.insert(path.strip_prefix(root)?.to_path_buf(), json!({"content":value,
            "device":info.dev(),"inode":info.ino(),"mode":info.mode(),"uid":info.uid(),"gid":info.gid(),
            "mtime":[info.mtime(),info.mtime_nsec()],"ctime":[info.ctime(),info.ctime_nsec()]}));
        Ok(())
    }
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "rootfs must be a real directory"
    );
    let mut result = BTreeMap::new();
    walk(root, root, &mut result)?;
    Ok(result)
}

struct Cohort {
    output: PathBuf,
    binary: PathBuf,
    rootfs: PathBuf,
    firmware: PathBuf,
    artifacts: BTreeMap<PathBuf, String>,
    inputs: BTreeMap<PathBuf, Value>,
    observations: Vec<Observation>,
    source: source::Source,
}
impl Cohort {
    fn check_inputs(&self) -> Result<()> {
        ensure!(tree(&self.rootfs)? == self.inputs, "rootfs changed");
        self.source.check()?;
        for (path, hash) in &self.artifacts {
            ensure!(
                digest(path)? == *hash,
                "artifact changed: {}",
                path.display()
            );
        }
        Ok(())
    }
    fn trial(&mut self, phase: &str) -> Result<usize> {
        self.check_inputs()?;
        let index = self.observations.len();
        let directory = self.output.join(format!("trial-{index:08}"));
        fs::create_dir(&directory)?;
        for name in ["workspace", "runs"] {
            fs::create_dir(directory.join(name))?;
        }
        fs::write(directory.join("workspace/startup-canary"), CANARY)?;
        fs::write(directory.join("config.toml"), &self.source.config)?;
        let path = |name: &str| directory.join(name).to_string_lossy().into_owned();
        let mut argv = vec![
            self.binary.display().to_string(),
            "spawn".into(),
            "--config".into(),
            path("config.toml"),
            "--no-agent-defaults".into(),
            "--overlaynet".into(),
            "auto".into(),
            "--stdio".into(),
            "inherit".into(),
            "--timeout".into(),
            "30s".into(),
            "--stage".into(),
            path("stage"),
            "--vm".into(),
            "--vm-library-dir".into(),
            self.firmware.display().to_string(),
            "--cpu".into(),
            "2".into(),
            "--memory".into(),
            "128MiB".into(),
            "--".into(),
            "/bin/sh".into(),
            "-c".into(),
            PAYLOAD.into(),
        ];
        if let Some(rootfs) = &self.source.rootfs {
            let command = argv
                .iter()
                .position(|s| s == "--")
                .context("workload delimiter")?;
            argv.splice(
                command..command,
                ["--rootfs".into(), rootfs.display().to_string()],
            );
        }
        let environment = BTreeMap::from([
            ("PVISOR_RUN_HOME".into(), path("runs")),
            ("PVISOR_KRUN_ENOMEM_WORKAROUND".into(), "0".into()),
            ("PVISOR_STARTUP_TIMING".into(), "0".into()),
            ("PVISOR_STARTUP_PROFILE".into(), "0".into()),
            ("PVISOR_FS_PROFILE".into(), "0".into()),
            ("XDG_RUNTIME_DIR".into(), "/tmp/pvisor-revision".into()),
        ]);
        let result = (|| {
            let observation = observer::capture(
                &argv,
                &directory.join("workspace"),
                &environment,
                &directory,
                b"REFERENCE_READY",
                Duration::from_secs(45),
            )?;
            evidence::validate_output(
                &fs::read_to_string(directory.join("stdout"))?,
                &fs::read_to_string(directory.join("stderr"))?,
                false,
            )?;
            ensure!(
                fs::read(directory.join("workspace/startup-canary"))? == CANARY,
                "canary changed"
            );
            let bundle = read(directory.join("stage/run-bundle.json"))?;
            validate_bundle(&bundle)?;
            evidence::require_empty_upper(&directory.join("stage/upper"))?;
            self.check_inputs()?;
            save(
                directory.join("condition.json"),
                &json!({"phase":phase,"validated":true,
                "stdout_sha256":digest(&directory.join("stdout"))?,"stderr_sha256":digest(&directory.join("stderr"))?}),
            )?;
            Ok(observation)
        })();
        match result {
            Ok(observation) => {
                self.observations.push(observation);
                Ok(index)
            }
            Err(error) => {
                save(
                    directory.join("failure.json"),
                    &json!({"error":format!("{error:#}")}),
                )?;
                Err(error)
            }
        }
    }
}

fn validate_bundle(bundle: &Value) -> Result<()> {
    ensure!(
        bundle["run"]["state"] == "completed"
            && bundle["run"]["exit_code"] == 0
            && bundle["run"]["executor"]["isolation"] == "virtual_machine"
            && bundle["safety"]["filesystem_changes_staged"] == true,
        "wrong completed VM/staging observation"
    );
    Ok(())
}

fn run(criterion: &mut Criterion, output: &Path) -> Result<()> {
    let (_affinity, cpus) = Affinity::set(env::var("PVISOR_BENCH_CPUS").ok())?;
    save(
        output.join("containment.json"),
        &json!({
        "pid_namespace":fs::read_link("/proc/self/ns/pid")?,
        "mount_namespace":fs::read_link("/proc/self/ns/mnt")?,
        "user_namespace":fs::read_link("/proc/self/ns/user")?,"private_tmp":true}),
    )?;
    let source_binary = required_path("PVISOR_BENCH_BINARY")?;
    let rootfs = required_path("PVISOR_BENCH_REVISION_ROOTFS")?;
    let source_firmware = required_path("PVISOR_BENCH_FIRMWARE")?;
    let private = Path::new("/tmp/pvisor-revision");
    fs::create_dir(private)?;
    fs::set_permissions(private, fs::Permissions::from_mode(0o700))?;
    let binary = private.join("pvisor");
    fs::copy(&source_binary, &binary)?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
    let firmware = private.join("firmware");
    fs::create_dir(&firmware)?;
    fs::copy(&source_firmware, firmware.join("libkrunfw.so.5"))?;
    let mut artifacts = BTreeMap::new();
    for path in [
        &source_binary,
        &binary,
        &source_firmware,
        &firmware.join("libkrunfw.so.5"),
    ] {
        artifacts.insert(path.clone(), digest(path)?);
    }
    ensure!(
        digest(&source_binary)? == digest(&binary)?,
        "executable copy differs"
    );
    ensure!(
        digest(&source_firmware)? == digest(&firmware.join("libkrunfw.so.5"))?,
        "firmware copy differs"
    );
    let inputs = tree(&rootfs)?;
    save(output.join("inputs.json"), &inputs)?;
    let executable = env::current_exe()?;
    save(
        output.join("provenance.json"),
        &json!({"source_binary":source_binary,"source_firmware":source_firmware,
        "artifacts":artifacts,"rootfs":rootfs,"host_cpus":cpus,"guest_cpus":2,"guest_memory_mib":128,
        "payload":PAYLOAD,"harness_binary_sha256":digest(&executable)?,
        "host_kernel":Command::new("uname").arg("-a").output()?.stdout,
        "sampling":"Criterion 0.7.0; Flat; 60 statistical samples per metric; operations include calibration",
        "clock":"shared observer; immediately before spawn to complete Ready line and normal zero exit",
        "build_correspondence":"external build receipt required; binary digest alone does not prove source origin",
        "environment": (["PATH","LD_LIBRARY_PATH","LANG","TZ","PVISOR_BENCH_CPUS"].into_iter().map(|key| (key,env::var(key).ok())).collect::<BTreeMap<_,_>>())}),
    )?;
    let frozen = output.join("harness");
    fs::create_dir(&frozen)?;
    for (name, source) in [
        ("revision.rs", include_str!("revision.rs")),
        ("statistics.rs", include_str!("revision/statistics.rs")),
        ("source.rs", include_str!("revision/source.rs")),
        ("observer.rs", include_str!("observer.rs")),
        ("evidence.rs", include_str!("evidence.rs")),
        ("subjects.rs", include_str!("subjects.rs")),
        ("Cargo.lock", include_str!("../../Cargo.lock")),
    ] {
        fs::write(frozen.join(name), source)?;
    }
    let source = source::Source::prepare(&binary, &rootfs, &firmware, output, private)?;
    let mut cohort = Cohort {
        output: output.to_path_buf(),
        binary,
        rootfs,
        firmware,
        artifacts,
        inputs,
        observations: Vec::new(),
        source,
    };
    cohort.trial("preflight")?;
    for _ in 0..3 {
        cohort.trial("protocol-warmup")?;
    }
    let mut group = criterion.benchmark_group("startup-revision");
    group
        .sample_size(60)
        .sampling_mode(SamplingMode::Flat)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    for metric in ["ready", "exit"] {
        let mut callbacks = Vec::new();
        group.bench_function(metric, |b| b.iter_custom(|iterations| {
            let first = cohort.observations.len();
            let mut total = 0u64;
            for _ in 0..iterations {
                if let Err(error) = cohort.trial(metric) {
                    let _ = save(output.join("status.json"), &json!({"status":"failed","error":format!("{error:#}")}));
                    panic!("revision correctness gate failed: {error:#}");
                }
                let value = cohort.observations.last().unwrap();
                total = total.checked_add(if metric == "ready" { value.ready_ns } else { value.exit_ns }).expect("duration overflow");
            }
            callbacks.push(json!({"first":first,"end":cohort.observations.len(),"iterations":iterations,"nanoseconds":total}));
            save(output.join(format!("callbacks-{metric}.json")), &callbacks).expect("retain callback evidence");
            Duration::from_nanos(total)
        }));
        let criterion_output = PathBuf::from(
            env::var_os("PVISOR_CRITERION_OUTPUT").context("--criterion-output is required")?,
        );
        let baseline = env::var("PVISOR_CRITERION_SAVE_BASELINE").unwrap_or_else(|_| "new".into());
        let samples = read(
            criterion_output
                .join("startup-revision")
                .join(metric)
                .join(baseline)
                .join("sample.json"),
        )?;
        let summary = statistics::formal(&samples, &callbacks, &cohort.observations, metric)?;
        save(output.join(format!("statistics-{metric}.json")), &summary)?;
    }
    group.finish();
    cohort.check_inputs()?;
    save(
        output.join("status.json"),
        &json!({"status":"complete","operations":cohort.observations.len()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_detects_empty_directory_mode_and_symlink_changes() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        let first = tree(root.path()).unwrap();
        fs::set_permissions(root.path().join("empty"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_ne!(tree(root.path()).unwrap(), first);
        std::os::unix::fs::symlink("missing", root.path().join("link")).unwrap();
        assert!(tree(root.path()).unwrap().contains_key(Path::new("link")));
        fs::remove_dir(root.path().join("empty")).unwrap();
        assert_ne!(tree(root.path()).unwrap(), first);
    }
    #[test]
    fn bundle_gate_rejects_missing_wrong_isolation_and_failed_runs() {
        let valid = json!({"run":{"state":"completed","exit_code":0,"executor":{"isolation":"virtual_machine"}},
            "safety":{"filesystem_changes_staged":true}});
        validate_bundle(&valid).unwrap();
        for wrong in [Value::Null, json!({"run":{"state":"failed"}})] {
            assert!(validate_bundle(&wrong).is_err());
        }
        let mut wrong = valid;
        wrong["run"]["executor"]["isolation"] = "rootless_process".into();
        assert!(validate_bundle(&wrong).is_err());
    }
}
