//! The daemon owns this worker's readiness, lifetime and exit status.
use anyhow::{Context, bail};
use std::{io::Write, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, ChildStdin, Command},
};

pub(super) struct RunningCache {
    child: Child,
    // Child::wait closes its own stdin; keep the owner lease outside Child.
    owner: Option<ChildStdin>,
}

pub(super) fn worker(
    listen: String,
    image_store: Option<PathBuf>,
    rafs_config: Option<PathBuf>,
) -> anyhow::Result<()> {
    // EOF on the daemon-owned pipe returns through normal resource cleanup.
    pvisor::cache::serve_managed(
        listen,
        image_store,
        std::env::var("PVISOR_CACHE_TOKEN").ok(),
        rafs_config,
        Some(0),
        || {
            println!("ready");
            std::io::stdout().flush()?;
            Ok(())
        },
    )
}

pub(super) async fn start(
    listen: Option<String>,
    image_store: Option<PathBuf>,
    rafs_config: Option<PathBuf>,
) -> anyhow::Result<RunningCache> {
    let config = pvisor::cache::CacheConfig::from_options(
        Some(pvisor::cache::CacheBackend::Server),
        listen,
        Some(false),
        image_store,
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .envs(worker_environment(std::env::vars_os()));
    command.args(["cache-worker", "--listen", &config.location]);
    if let Some(store) = config.image_store {
        command.arg("--image-store").arg(store);
    }
    if let Some(path) = rafs_config {
        command.arg("--rafs-config").arg(path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let mut output = BufReader::new(child.stdout.take().context("missing readiness pipe")?);
    let mut line = String::new();
    let ready = tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line)).await;
    if !matches!(ready, Ok(Ok(_))) || line != "ready\n" {
        child.kill().await?;
        bail!("cache component failed to become ready; see component error above");
    }
    let owner = child.stdin.take().context("missing owner pipe")?;
    Ok(RunningCache {
        child,
        owner: Some(owner),
    })
}

pub(super) async fn wait(child: &mut Option<RunningCache>) -> anyhow::Result<()> {
    if let Some(child) = child {
        let status = child.child.wait().await?;
        bail!("cache component exited unexpectedly: {status}");
    }
    std::future::pending().await
}

fn worker_environment(
    env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "DOCKER_CONFIG",
        "RUST_LOG",
        "PVISOR_CACHE_TOKEN",
    ];
    env.into_iter()
        .filter(|(key, _)| ALLOWED.iter().any(|allowed| key == *allowed))
        .collect()
}

pub(super) async fn stop(service: &mut RunningCache) -> anyhow::Result<()> {
    drop(service.owner.take());
    let child = &mut service.child;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            child.kill().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_worker_does_not_inherit_lifecycle_or_unrelated_secrets() {
        let input = [
            "OPEN_SANDBOX_API_KEY",
            "E2B_API_KEY",
            "UNRELATED_SECRET",
            "PVISOR_CACHE_TOKEN",
            "PATH",
            "HTTPS_PROXY",
            "SSL_CERT_FILE",
        ];
        let env =
            super::worker_environment(input.into_iter().map(|key| (key.into(), "value".into())));
        let keys: Vec<_> = env.iter().map(|(key, _)| key.to_str().unwrap()).collect();
        assert_eq!(
            keys,
            ["PVISOR_CACHE_TOKEN", "PATH", "HTTPS_PROXY", "SSL_CERT_FILE"]
        );
    }
}
