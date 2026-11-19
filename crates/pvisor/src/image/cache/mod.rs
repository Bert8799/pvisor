//! Shared OCI cache with server, filesystem, S3, static HTTP/HTTPS and IPFS storage.
//! See docs/src/zh/reference/shared-image-cache.md for storage layout and lifecycle.

mod backend;
mod client;
mod config;
mod direct;
mod http;
mod mirrors;
mod network;
mod pool;
mod portable;
pub mod progress;
mod protocol;
mod s3_runtime;
mod server;
mod source;
pub(crate) mod storage;
pub(crate) use config::scrub_guest_environment;
mod transport;

pub use client::CacheClient;
pub(crate) use client::ClientBinding;
pub use config::{BACKEND_ENV, CacheBackend, CacheConfig, LOCATION_ENV, READ_ONLY_ENV};
pub use progress::ImageTotals;
use protocol::hash;
pub use protocol::{MAX_READ, RafsRequest, Request, Response};
pub use transport::default_endpoint;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod lazy;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use lazy::prepare_vm_image;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use lazy::{LazyImage, open_image_handle_for_host, open_image_handle_for_vm};

pub use crate::env::cache::SERVER_ENV;

fn architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use direct::DirectImage;
pub(crate) use direct::{attach_runner_lowers, private_owner as direct_image_owner};
pub(crate) use network::run_internal_if_requested as run_image_access_internal;
pub(crate) use pool::{open_local_image, publish_local_image, published_local_image};
pub(crate) use portable::export_pool;
pub(crate) use repository::{
    publish_reference, resolve_repository, validate_name as validate_registry_name,
    validate_registries,
};
pub(crate) use repository::{publish_repository, read_repository_template, validate_destination};
mod repository;

/// Serve until process termination, notifying the owner after binding and worker startup.
/// The callback runs once before accepting connections. Its failure stops startup.
/// The caller owns process supervision; in-flight requests are not drained on termination.
pub fn serve(
    location: String,
    image_store: Option<std::path::PathBuf>,
    token: Option<String>,
    ready: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    server::serve(
        location,
        crate::image::store::LocalStore::discover(image_store)?,
        token,
        ready,
    )
}

/// Serve the existing cache protocol with optional node-owned RAFS supply and
/// cooperative shutdown. `rafs_config` names an explicit protected local JSON
/// configuration; it cannot be supplied by requests. RAFS requires Linux and an
/// explicitly trusted native reader. A negative/closed shutdown descriptor is
/// invalid; the caller retains a live descriptor through this blocking call.
/// Readability or hangup ends admission, stops/reaps the native reader and keeps
/// shared cached data. In-flight requests are not drained. `ready` runs only
/// after the socket, bounded workers and optional native reader are ready.
pub fn serve_managed(
    location: String,
    image_store: Option<std::path::PathBuf>,
    token: Option<String>,
    rafs_config: Option<std::path::PathBuf>,
    shutdown_fd: Option<std::os::fd::RawFd>,
    ready: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    server::serve_managed(
        location,
        crate::image::store::LocalStore::discover(image_store)?,
        token,
        rafs_config,
        shutdown_fd,
        ready,
    )
}
