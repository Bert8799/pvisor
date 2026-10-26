use pvisor_guest::template::{BootConfig, INSTANCE_ENV, Ready, Task, Template, VERSION};
use pvisor_guest::{GuestConfig, ProcessConfig, StdinMode, control};
use std::io::{self, Cursor};
use std::sync::atomic::{AtomicBool, Ordering};

fn template() -> Template {
    Template {
        version: VERSION,
        template_id: "a".repeat(64),
        stdio_ports: [true, true, false],
    }
}

fn task() -> Task {
    Task {
        version: VERSION,
        template_id: "a".repeat(64),
        instance_id: "sb-12345678-1234-4234-8234-123456789abc".into(),
        entropy: [42; 32],
        config: GuestConfig {
            argv: vec!["/bin/true".into()],
            cwd: "/".into(),
            process: Some(ProcessConfig {
                stdin: StdinMode::Closed,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn frame(value: &impl serde::Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    control::write_frame(&mut bytes, value).unwrap();
    bytes
}

#[test]
fn boot_documents_preserve_existing_launches_and_reject_mixed_templates() {
    assert!(matches!(
        serde_json::from_str::<BootConfig>(r#"{"argv":["/bin/true"],"env":{},"cwd":"/"}"#),
        Ok(BootConfig::Workload(_))
    ));
    let boot = serde_json::json!({"template":template()});
    assert!(matches!(
        serde_json::from_value::<BootConfig>(boot.clone()),
        Ok(BootConfig::Template { .. })
    ));
    for field in ["argv", "env", "cwd", "unexpected"] {
        let mut mixed = boot.clone();
        mixed[field] = serde_json::json!("must not be captured");
        assert!(serde_json::from_value::<BootConfig>(mixed).is_err());
    }
    let mut unknown = boot;
    unknown["template"]["secret"] = serde_json::json!("forbidden");
    assert!(serde_json::from_value::<BootConfig>(unknown).is_err());
}

#[test]
fn task_replaces_command_environment_and_directory_only_after_reseeding() {
    let root = tempfile::tempdir().unwrap();
    let mut request = task();
    request.config.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "test \"$TASK_VALUE\" = 'literal $value' && test \"$PVISOR_INSTANCE_ID\" = \"$1\" && test -z \"${HOME+x}\" && pwd".into(),
        "probe".into(),
        request.instance_id.clone(),
    ];
    request.config.cwd = root.path().canonicalize().unwrap();
    request
        .config
        .env
        .insert("TASK_VALUE".into(), "literal $value".into());
    let called = AtomicBool::new(false);
    let mut output = Vec::new();
    let config = template()
        .receive(&mut &frame(&request)[..], &mut output, |entropy| {
            assert_eq!(entropy, &[42; 32]);
            called.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert!(called.load(Ordering::SeqCst));
    let ready: Ready = control::read_frame(&mut &output[..]).unwrap();
    assert_eq!(ready.version, VERSION);
    assert_eq!(ready.template_id, request.template_id);
    assert_eq!(config.argv, request.config.argv);
    assert_eq!(config.env.len(), 2); // No captured or host environment.
    let result = config.command().unwrap().output().unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        config.cwd.to_str().unwrap()
    );
}

#[test]
fn invalid_tasks_fail_before_reseeding_or_launch() {
    for change in 0..14 {
        let mut request = task();
        match change {
            0 => request.version += 1,
            1 => request.template_id = "c".repeat(64),
            2 => request.instance_id = "../instance".into(),
            3 => request.entropy = [0; 32],
            4 => request.config.argv.clear(),
            5 => request.config.cwd = "relative".into(),
            6 => request.config.process = None,
            7 => request.config.process.as_mut().unwrap().stdin = StdinMode::Pipe,
            8 => {
                let process = request.config.process.as_mut().unwrap();
                process.new_session = true;
                process.tty = Some([24, 80]);
            }
            9 => {
                request
                    .config
                    .env
                    .insert(INSTANCE_ENV.into(), "override".into());
            }
            10 => request.config.stdio_ports = Some([false; 3]),
            11 => {
                request.config.env.insert("bad=name".into(), "value".into());
            }
            12 => request.instance_id.clear(),
            13 => request.instance_id = "x".repeat(129),
            _ => unreachable!(),
        }
        let result = template().receive(&mut &frame(&request)[..], &mut Vec::new(), |_| {
            panic!("invalid task {change} reached reseeding")
        });
        assert!(result.is_err(), "accepted invalid task {change}");
    }
}

#[test]
fn reseeding_failure_withholds_launch_options() {
    let result = template().receive(&mut &frame(&task())[..], &mut Vec::new(), |_| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reseed denied",
        ))
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn handoff_is_bounded_truncation_is_fatal_and_only_one_frame_is_consumed() {
    let valid = frame(&task());
    for bytes in [
        vec![],
        valid[..valid.len() - 1].to_vec(),
        ((control::MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
        frame(&serde_json::json!({"unknown":"field"})),
    ] {
        assert!(
            template()
                .receive(&mut &bytes[..], &mut Vec::new(), |_| panic!(
                    "invalid frame reached reseeding"
                ))
                .is_err()
        );
    }
    let mut two = valid.clone();
    two.extend_from_slice(&valid);
    let mut input = Cursor::new(two);
    template()
        .receive(&mut input, &mut Vec::new(), |_| Ok(()))
        .unwrap();
    assert_eq!(input.position() as usize, valid.len());
}

#[test]
fn invalid_template_never_announces_readiness() {
    for change in 0..4 {
        let mut descriptor = template();
        match change {
            0 => descriptor.version += 1,
            1 => descriptor.template_id = "A".repeat(64),
            2 => descriptor.stdio_ports[0] = false,
            3 => descriptor.stdio_ports[1] = false,
            _ => unreachable!(),
        }
        let mut output = Vec::new();
        assert!(
            descriptor
                .receive(&mut io::empty(), &mut output, |_| panic!())
                .is_err()
        );
        assert!(output.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn guest_announces_readiness_before_blocking_and_disconnect_cannot_launch() {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let (mut host, mut guest) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let reseeded = std::sync::Arc::new(AtomicBool::new(false));
    let observed = reseeded.clone();
    let waiting = std::thread::spawn(move || {
        template().receive(&mut guest.try_clone().unwrap(), &mut guest, |_| {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })
    });
    let ready: Ready = control::read_frame(&mut host).unwrap();
    assert_eq!(ready.template_id, template().template_id);
    assert!(!reseeded.load(Ordering::SeqCst));
    assert!(!waiting.is_finished());
    drop(host);
    assert_eq!(
        waiting.join().unwrap().unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(!reseeded.load(Ordering::SeqCst));
}

#[test]
fn independent_handoffs_do_not_retain_another_instances_environment() {
    let handles: Vec<_> = (1..=4)
        .map(|n| {
            std::thread::spawn(move || {
                let mut request = task();
                request.instance_id = format!("{n:064x}");
                request.entropy = [n; 32];
                request
                    .config
                    .env
                    .insert(format!("TASK_{n}"), "private".into());
                let config = template()
                    .receive(&mut &frame(&request)[..], &mut Vec::new(), |entropy| {
                        assert_eq!(entropy, &[n; 32]);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(config.env.len(), 2);
                assert_eq!(config.env[INSTANCE_ENV], request.instance_id);
                assert!(config.env.contains_key(&format!("TASK_{n}")));
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
}
