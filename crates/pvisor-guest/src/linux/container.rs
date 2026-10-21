//! Container-only control path; no host filesystem or host lifecycle authority.
use super::{check, mount, resource};
use pvisor_guest::{
    GuestConfig,
    container::{self, ContainerConfig, Reply, Request, StdinFrame, read_frame, write_frame},
};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn identity(user: &str) -> io::Result<(u32, u32, Vec<u32>)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "unknown guest user/group");
    let (name, group) = user
        .split_once(':')
        .map_or((user, None), |(u, g)| (u, Some(g)));
    let name = if name.is_empty() { "0" } else { name };
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let entry = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|p| p.len() >= 4 && (p[0] == name || name.parse::<u32>().ok() == p[2].parse().ok()));
    let uid = name
        .parse()
        .ok()
        .or_else(|| entry.as_ref()?.get(2)?.parse().ok())
        .ok_or_else(invalid)?;
    let mut gid = entry.as_ref().and_then(|p| p[3].parse().ok()).unwrap_or(0);
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    let mut additional = Vec::new();
    if let Some(group) = group {
        gid = group
            .parse()
            .ok()
            .or_else(|| {
                groups.lines().find_map(|l| {
                    let p: Vec<_> = l.split(':').collect();
                    (p.len() >= 3 && p[0] == group)
                        .then(|| p[2].parse().ok())
                        .flatten()
                })
            })
            .ok_or_else(invalid)?;
    } else if let Some(entry) = entry {
        for line in groups.lines() {
            let p: Vec<_> = line.split(':').collect();
            if p.len() >= 4
                && p[3].split(',').any(|member| member == entry[0])
                && let Ok(group) = p[2].parse()
            {
                additional.push(group);
            }
        }
    }
    Ok((uid, gid, additional))
}

fn command(config: &GuestConfig, user: &str) -> io::Result<Command> {
    let mut command = config.command()?;
    let (uid, gid, groups) = identity(user)?;
    if !config.env.contains_key("HOME") {
        let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
        let home = passwd
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split(':').collect();
                (fields.len() >= 6 && fields[2].parse::<u32>().ok() == Some(uid)).then(|| fields[5])
            })
            .unwrap_or("/");
        command.env("HOME", home);
    }
    let limits = config
        .limits
        .iter()
        .map(|(name, (soft, hard))| {
            Ok((
                resource(name)?,
                libc::rlimit {
                    rlim_cur: *soft,
                    rlim_max: *hard,
                },
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
    unsafe {
        command.pre_exec(move || {
            check(libc::setsid())?;
            for (resource, limit) in &limits {
                check(libc::syscall(
                    libc::SYS_prlimit64,
                    0,
                    *resource,
                    limit,
                    std::ptr::null::<libc::rlimit>(),
                ) as i32)?;
            }
            // Match Docker's default capability bounding set. In particular,
            // workload root cannot remount a read-only share.
            const CAPS: &[u32] = &[0, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 27, 29, 31];
            for capability in 0..=63 {
                if !CAPS.contains(&capability) {
                    let result = libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0);
                    if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
                    {
                        check(result)?;
                    }
                }
            }
            check(libc::setgroups(groups.len(), groups.as_ptr()))?;
            check(libc::setgid(gid))?;
            check(libc::setuid(uid))?;
            Ok(())
        });
    }
    Ok(command)
}

fn listener() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    check(fd)?;
    let listener = unsafe { OwnedFd::from_raw_fd(fd) };
    let address = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as _,
        svm_reserved1: 0,
        svm_port: container::CONTROL_PORT,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    check(unsafe {
        libc::bind(
            fd,
            (&address as *const libc::sockaddr_vm).cast(),
            std::mem::size_of_val(&address) as _,
        )
    })?;
    check(unsafe { libc::listen(fd, 32) })?;
    Ok(listener)
}

type Input = Arc<Mutex<Option<Box<dyn Write + Send>>>>;
type ProcessOutput = Vec<(Box<dyn Read + Send>, u8)>;
struct Spawned {
    child: std::process::Child,
    input: Option<Box<dyn Write + Send>>,
    outputs: ProcessOutput,
}

fn spawn(
    config: &GuestConfig,
    user: &str,
    interactive: bool,
    tty: Option<[u16; 2]>,
    capture: bool,
) -> io::Result<Spawned> {
    let mut command = command(config, user)?;
    if let Some([rows, columns]) = tty {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        check(unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        })?;
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let fd = slave.as_raw_fd();
        unsafe { command.pre_exec(move || check(libc::ioctl(fd, libc::TIOCSCTTY, 0))) };
        command
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        let child = command.spawn()?;
        let input = if interactive {
            Some(Box::new(master.try_clone()?) as Box<dyn Write + Send>)
        } else {
            None
        };
        Ok(Spawned {
            child,
            input,
            outputs: vec![(Box::new(master), 1)],
        })
    } else {
        command.stdin(if interactive {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        if capture {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn Write + Send>);
        let output = if capture {
            vec![
                (
                    Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
                    1,
                ),
                (
                    Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
                    2,
                ),
            ]
        } else {
            Vec::new()
        };
        Ok(Spawned {
            child,
            input,
            outputs: output,
        })
    }
}

fn read_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    match reader.read(buffer) {
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
        result => result,
    }
}

fn pump(mut reader: impl Read, writer: Arc<Mutex<UnixStream>>, stream: u8) -> io::Result<()> {
    let mut buffer = [0; 16 * 1024];
    loop {
        let size = read_chunk(&mut reader, &mut buffer)?;
        if size == 0 {
            return Ok(());
        }
        write_frame(
            &mut *writer.lock().unwrap(),
            &Reply::Output {
                stream,
                bytes: buffer[..size].to_vec(),
            },
        )?;
    }
}

fn consume_input(stream: &mut UnixStream, output: &mut impl Write) -> io::Result<()> {
    loop {
        match read_frame::<StdinFrame>(stream)? {
            StdinFrame::Data { bytes } => {
                output.write_all(&bytes)?;
                output.flush()?;
            }
            StdinFrame::Eof {} => return Ok(()),
        }
    }
}

fn serve(
    mut stream: UnixStream,
    config: &GuestConfig,
    container: &ContainerConfig,
    pid: u32,
    input: &Input,
) -> io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let request: Request = read_frame(&mut stream)?;
    stream.set_read_timeout(None)?;
    match request {
        Request::Ping {} => write_frame(&mut stream, &Reply::Ready),
        Request::Input {} => {
            let mut input = input
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| io::Error::other("stdin is closed or already attached"))?;
            write_frame(&mut stream, &Reply::Ready)?;
            consume_input(&mut stream, &mut input)?;
            drop(input);
            write_frame(&mut stream, &Reply::Ready)
        }
        Request::Stop { signal } => {
            if !(1..=64).contains(&signal) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid signal",
                ));
            }
            check(unsafe { libc::kill(-(pid as i32), signal) })?;
            write_frame(&mut stream, &Reply::Ready)
        }
        Request::Connect { port } => {
            let mut tcp = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_secs(10),
            )?;
            write_frame(&mut stream, &Reply::Ready)?;
            let mut input = stream.try_clone()?;
            let mut output = tcp.try_clone()?;
            std::thread::spawn(move || {
                let _ = consume_input(&mut input, &mut output);
                let _ = output.shutdown(std::net::Shutdown::Write);
            });
            let mut bytes = [0; 16384];
            loop {
                let size = tcp.read(&mut bytes)?;
                if size == 0 {
                    break;
                }
                write_frame(
                    &mut stream,
                    &Reply::Output {
                        stream: 1,
                        bytes: bytes[..size].to_vec(),
                    },
                )?;
            }
            write_frame(&mut stream, &Reply::Exit { code: 0 })
        }
        Request::Exec {
            argv,
            env,
            unset_env,
            cwd,
            user,
            tty,
        } => {
            let mut config = config.clone();
            config.argv = argv;
            for key in unset_env {
                config.env.remove(&key);
            }
            config.env.extend(env);
            if let Some(cwd) = cwd {
                config.cwd = cwd;
            }
            let Spawned {
                mut child,
                input,
                outputs,
            } = match spawn(
                &config,
                user.as_deref().unwrap_or(&container.user),
                true,
                tty,
                true,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let code = match error.raw_os_error() {
                        Some(libc::ENOENT) => 127,
                        Some(libc::EACCES | libc::ENOEXEC) => 126,
                        _ => 125,
                    };
                    write_frame(
                        &mut stream,
                        &Reply::Error {
                            message: error.to_string(),
                        },
                    )?;
                    return write_frame(&mut stream, &Reply::Exit { code });
                }
            };
            write_frame(&mut stream, &Reply::Ready)?;
            let mut stdin = input.unwrap();
            let mut input = stream.try_clone()?;
            let exec_pid = child.id();
            std::thread::spawn(move || {
                if consume_input(&mut input, &mut stdin).is_err() {
                    unsafe {
                        libc::kill(-(exec_pid as i32), libc::SIGKILL);
                    }
                }
            });
            let output = Arc::new(Mutex::new(stream.try_clone()?));
            let pumps: Vec<_> = outputs
                .into_iter()
                .map(|(reader, channel)| {
                    let writer = output.clone();
                    std::thread::spawn(move || pump(reader, writer, channel))
                })
                .collect();
            let status = child.wait()?;
            unsafe {
                libc::kill(-(exec_pid as i32), libc::SIGKILL);
            }
            for pump in pumps {
                let _ = pump.join();
            }
            use std::os::unix::process::ExitStatusExt;
            write_frame(
                &mut *output.lock().unwrap(),
                &Reply::Exit {
                    code: status.code().unwrap_or(128 + status.signal().unwrap_or(9)),
                },
            )?;
            Ok(())
        }
    }
}

fn copy_volume(source: &std::path::Path, target: &std::path::Path, depth: usize) -> io::Result<()> {
    if depth > 256 {
        return Err(io::Error::other(
            "volume initialization exceeds directory depth limit",
        ));
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_volume(&entry.path(), &target.join(entry.file_name()), depth + 1)?;
        }
    } else if metadata.is_file() {
        fs::copy(source, target)?;
    } else if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume initialization of special files is unsupported",
        ));
    }
    let path = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())?;
    check(unsafe { libc::lchown(path.as_ptr(), metadata.uid(), metadata.gid()) })?;
    if !metadata.file_type().is_symlink() {
        fs::set_permissions(target, fs::Permissions::from_mode(metadata.mode()))?;
    }
    Ok(())
}

pub(super) fn run(config: &GuestConfig, container: &ContainerConfig) -> io::Result<i32> {
    for (index, share) in container.mounts.iter().enumerate() {
        if !share.target.is_absolute() || share.target == std::path::Path::new("/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid container mount target",
            ));
        }
        fs::create_dir_all(&share.target)?;
        if share.initialize {
            let temporary = std::path::PathBuf::from(format!("/run/pvisor-volume-{index}"));
            fs::create_dir_all(&temporary)?;
            mount(&share.tag, &temporary, "virtiofs", 0)?;
            if fs::read_dir(&temporary)?.next().is_none() {
                copy_volume(&share.target, &temporary, 0)?;
            }
            let path = std::ffi::CString::new(temporary.as_os_str().as_encoded_bytes())?;
            check(unsafe { libc::umount2(path.as_ptr(), 0) })?;
            fs::remove_dir(temporary)?;
        }
        mount(
            &share.tag,
            &share.target,
            "virtiofs",
            if share.read_only { libc::MS_RDONLY } else { 0 },
        )?;
    }
    fs::create_dir_all(&config.cwd)?;
    let listener = listener()?;
    let Spawned {
        mut child,
        input: stdin,
        outputs,
    } = match spawn(
        config,
        &container.user,
        container.interactive,
        container.tty,
        false,
    ) {
        Ok(child) => child,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(127),
        Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::ENOEXEC)) => {
            return Ok(126);
        }
        Err(error) => return Err(error),
    };
    let pid = child.id();
    let input: Input = Arc::new(Mutex::new(stdin));
    let pumps: Vec<_> = outputs
        .into_iter()
        .map(|(mut reader, _)| {
            std::thread::spawn(move || {
                let mut buffer = [0; 16384];
                while let Ok(size) = read_chunk(&mut reader, &mut buffer) {
                    if size == 0 {
                        break;
                    }
                    if io::stdout().write_all(&buffer[..size]).is_err() {
                        break;
                    }
                    let _ = io::stdout().flush();
                }
            })
        })
        .collect();
    let config = config.clone();
    let container = container.clone();
    std::thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        loop {
            // UnixListener::accept rejects AF_VSOCK peer addresses.
            let fd = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&address) as libc::socklen_t;
            if unsafe {
                libc::getpeername(
                    stream.as_raw_fd(),
                    (&mut address as *mut libc::sockaddr_vm).cast(),
                    &mut size,
                )
            } != 0
                || address.svm_cid != libc::VMADDR_CID_HOST
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= 32 {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = active.clone();
            let config = config.clone();
            let container = container.clone();
            let input = input.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                if let Err(error) = serve(
                    stream.try_clone().unwrap(),
                    &config,
                    &container,
                    pid,
                    &input,
                ) {
                    let _ = write_frame(
                        &mut stream,
                        &Reply::Error {
                            message: error.to_string(),
                        },
                    );
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
    let status = child.wait()?;
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    for pump in pumps {
        let _ = pump.join();
    }
    use std::os::unix::process::ExitStatusExt;
    Ok(status.code().unwrap_or(128 + status.signal().unwrap_or(9)))
}
