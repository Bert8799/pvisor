//! Authenticated, bounded, demand-only RAFS v6 reader. Built against pinned Nydus.
//! Stdio is a private engineering protocol, not the pVisor cache service protocol.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use fuse_backend_rs::api::filesystem::ZeroCopyWriter;
use fuse_backend_rs::file_buf::FileVolatileSlice;
use fuse_backend_rs::file_traits::FileReadWriteVolatile;
use nydus_api::ConfigV2;
use nydus_rafs::metadata::{RafsInodeExt, RafsSuper};
use nydus_rafs::RafsIoReader;
use nydus_storage::device::{BlobChunkInfo, BlobDevice, BlobFeatures, BlobPrefetchRequest};
use nydus_utils::digest::{self, DigestHasher};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_READ: usize = 1024 * 1024;
const PREFIX: &str = "pvisor-rafs-merged-v1:";
#[path = "rafs_reader/node.rs"]
mod node;

fn private_directory(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "directory must be private, owned and non-symlink"
    );
    Ok(())
}

fn regular(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        f.metadata()?.is_file(),
        "expected a regular non-symlink file"
    );
    Ok(f)
}

fn control(path: &Path) -> Result<Vec<u8>> {
    let file = regular(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_FRAME as u64,
        "control object too large"
    );
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_FRAME, "control object grew");
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = digest::RafsDigest::hasher(digest::Algorithm::Sha256);
    hasher.digest_update(bytes);
    format!("sha256:{}", hasher.digest_finalize())
}

fn hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Blob {
    artifact: Descriptor,
    toc_sha256: String,
    toc_size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Commit {
    schema: String,
    algorithm: String,
    environment: String,
    platform: String,
    bootstrap: Descriptor,
    inventory: Descriptor,
    config: Descriptor,
    blobs: BTreeMap<String, Blob>,
    tools: BTreeMap<String, String>,
}

struct Image {
    node_cache: bool,
    rafs: RafsSuper,
    _bootstrap: std::sync::Mutex<RafsIoReader>,
    device: BlobDevice,
    _pin: File,
    _node_budget: Option<node::budget::ImageLease>,
    _commit: File,
}

impl Image {
    fn open(bundle: &Path, handle: &str, cache: &Path) -> Result<Self> {
        Self::open_with(bundle, handle, cache, None)
    }

    fn open_with(
        bundle: &Path,
        handle: &str,
        cache: &Path,
        shared: Option<&node::Shared>,
    ) -> Result<Self> {
        let revision = handle
            .strip_prefix(PREFIX)
            .context("expected a fixed merged-image handle")?;
        ensure!(hex_digest(revision), "invalid merged revision");
        private_directory(bundle)?;
        private_directory(&bundle.join("metadata"))?;
        private_directory(&bundle.join("blobs"))?;
        use std::os::fd::AsRawFd;
        let mut commit = regular(&bundle.join("MERGED.json"))?;
        ensure!(
            unsafe { libc::flock(commit.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0,
            io::Error::last_os_error()
        );
        ensure!(
            commit.metadata()?.len() <= MAX_FRAME as u64,
            "commit exceeds limit"
        );
        let mut raw = Vec::new();
        (&mut commit)
            .take(MAX_FRAME as u64 + 1)
            .read_to_end(&mut raw)?;
        ensure!(
            sha256(&raw) == format!("sha256:{revision}"),
            "merged identity mismatch"
        );
        let record: Commit = serde_json::from_slice(&raw)?;
        ensure!(
            record.schema == "pvisor-rafs-merged-v1"
                && record.algorithm == "oci-overlay-directory-inheritance-v1"
                && matches!(record.platform.as_str(), "linux/amd64" | "linux/arm64"),
            "unsupported merged profile"
        );
        ensure!(
            record.environment.starts_with("pvisor-rafs-env-v1:")
                && hex_digest(&record.environment["pvisor-rafs-env-v1:".len()..]),
            "invalid environment identity"
        );
        for descriptor in [&record.bootstrap, &record.inventory, &record.config] {
            ensure!(
                descriptor.sha256.starts_with("sha256:") && hex_digest(&descriptor.sha256[7..]),
                "invalid descriptor"
            );
        }
        ensure!(
            record.tools.len() == 2
                && record.tools.contains_key("converter")
                && record.tools.contains_key("reader"),
            "invalid tool identities"
        );
        // Inventory and retained source bundles are offline audit data. Runtime opens
        // authenticate the small control/config and bootstrap, never all payloads.
        let mut node_budget = shared
            .map(|s| {
                let charge = record
                    .bootstrap
                    .size
                    .checked_mul(2)
                    .and_then(|v| v.checked_add(2 * MAX_FRAME as u64))
                    .context("node bootstrap charge overflow")?;
                s.resources.image(charge)
            })
            .transpose()?;
        let config_bytes = control(&bundle.join("metadata/source.config.json"))?;
        ensure!(
            config_bytes.len() as u64 == record.config.size
                && sha256(&config_bytes) == record.config.sha256,
            "config mismatch"
        );
        let bootstrap_path = bundle.join("metadata/image.bootstrap");
        let mut pin = regular(&bootstrap_path)?;
        ensure!(
            pin.metadata()?.len() == record.bootstrap.size
                && record.bootstrap.size
                    <= if shared.is_some() {
                        32 * 1024 * 1024
                    } else {
                        256 * 1024 * 1024
                    },
            "bootstrap size mismatch"
        );
        let mut hasher = digest::RafsDigest::hasher(digest::Algorithm::Sha256);
        let mut buffer = [0; 128 * 1024];
        loop {
            let n = pin.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.digest_update(&buffer[..n]);
        }
        ensure!(
            format!("sha256:{}", hasher.digest_finalize()) == record.bootstrap.sha256,
            "bootstrap digest mismatch"
        );
        ensure!(record.blobs.len() <= 4096, "too many blobs");
        for (id, blob) in &record.blobs {
            ensure!(
                hex_digest(id)
                    && blob.artifact.sha256.starts_with("sha256:")
                    && hex_digest(&blob.artifact.sha256[7..])
                    && blob.toc_size > 0
                    && blob.toc_size <= blob.artifact.size,
                "invalid blob descriptor"
            );
            if shared.is_none() {
                ensure!(
                    regular(&bundle.join("blobs").join(id))?.metadata()?.len()
                        == blob.artifact.size,
                    "blob size mismatch"
                );
            }
        }
        let config = if let Some(shared) = shared {
            shared.config.clone()
        } else {
            fs::DirBuilder::new().mode(0o700).create(cache)?;
            Arc::new(serde_json::from_value::<ConfigV2>(json!({
            "version": 2, "backend": {"type": "localfs", "localfs": {"dir": bundle.join("blobs")}},
            "cache": {"type": "blobcache", "validate": true, "prefetch": {"enable": false}, "filecache": {"work_dir": cache}},
            "rafs": {"mode": "direct", "validate": false, "enable_xattr": true, "prefetch": {"enable": false}}
            }))?)
        };
        // Linux's descriptor path pins the exact authenticated metadata inode.
        // The engineering reader is Linux-only until other platforms can pin this
        // mapping without re-opening a mutable name.
        let pinned = format!("/proc/self/fd/{}", pin.as_raw_fd());
        let (rafs, reader) = RafsSuper::load_from_file(pinned, config.clone(), false)?;
        ensure!(
            rafs.meta.is_v6() && rafs.meta.chunk_size <= 1024 * 1024,
            "unsupported RAFS version or chunk size"
        );
        if shared.is_some() {
            ensure!(
                (4096..=1024 * 1024).contains(&rafs.meta.chunk_size)
                    && rafs.meta.chunk_size.is_power_of_two(),
                "unsupported node chunk geometry"
            );
        }
        let blobs = rafs.superblock.get_blob_infos();
        ensure!(
            blobs.is_empty()
                || (rafs.meta.has_inlined_chunk_digest()
                    && rafs.meta.get_digester() == digest::Algorithm::Sha256),
            "filesystem lacks mandatory SHA-256 chunk digests"
        );
        ensure!(
            blobs.len() == record.blobs.len() && blobs.len() <= 4096,
            "blob set mismatch"
        );
        for info in &blobs {
            ensure!(hex_digest(&info.blob_id()), "invalid native blob ID");
            let expected = record.blobs.get(&info.blob_id()).context("unbound blob")?;
            ensure!(
                info.has_feature(BlobFeatures::INLINED_CHUNK_DIGEST)
                    && info.digester() == digest::Algorithm::Sha256
                    && !info.features().intersects(
                        BlobFeatures::BATCH
                            | BlobFeatures::ENCRYPTED
                            | BlobFeatures::EXTERNAL
                            | BlobFeatures::ZRAN
                    ),
                "unsupported chunk features"
            );
            ensure!(
                expected.toc_size > 0
                    && expected.toc_size == u64::from(info.blob_toc_size())
                    && expected.toc_sha256
                        == format!("sha256:{}", hex::encode(info.blob_toc_digest()))
                    && expected.toc_sha256 != format!("sha256:{}", "0".repeat(64)),
                "ToC mismatch"
            );
        }
        if let Some(shared) = shared {
            let budget = node_budget.as_mut().unwrap();
            budget.blobs(&blobs)?;
            shared.reserve(&record.blobs, &blobs)?;
            // Admission precedes native allocation. One shared cell constructs
            // each blob cache synchronously, including its authenticated ToC.
            // Retain the cache until the admitted image releases its lease.
            for info in &blobs {
                budget.cache(info, &config)?;
            }
        }
        let device = BlobDevice::new(&config, &blobs, "/")?;
        Ok(Self {
            node_cache: shared.is_some(),
            rafs,
            _bootstrap: std::sync::Mutex::new(reader),
            device,
            _pin: pin,
            _node_budget: node_budget,
            _commit: commit,
        })
    }

    fn node(&self, path: &str) -> Result<Arc<dyn RafsInodeExt>> {
        ensure!(
            path.len() <= 4096 && !path.contains('\0') && !path.starts_with('/'),
            "invalid relative path"
        );
        let mut node = self
            .rafs
            .get_extended_inode(self.rafs.superblock.root_ino(), true)?;
        if !path.is_empty() {
            ensure!(path.split('/').count() <= 256, "path too deep");
            for part in path.split('/') {
                ensure!(
                    !part.is_empty() && part != "." && part != "..",
                    "noncanonical path"
                );
                if !node.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR).into());
                }
                node = node.get_child_by_name(OsStr::new(part))?;
                node.validate(self.rafs.get_max_ino(), self.rafs.meta.chunk_size as u64)?;
            }
        }
        Ok(node)
    }

    fn stat(&self, path: &str) -> Result<Value> {
        let node = self.node(path)?;
        let attr = node.get_attr();
        let kind = if node.is_dir() {
            "directory"
        } else if node.is_reg() {
            "file"
        } else {
            ensure!(node.is_symlink(), "unsupported node type");
            "symlink"
        };
        ensure!(attr.mtimensec < 1_000_000_000, "invalid timestamp");
        let mut xattrs = BTreeMap::new();
        for key in node.get_xattrs()? {
            let name = String::from_utf8(key)?;
            let value = node
                .get_xattr(OsStr::new(&name))?
                .context("missing xattr")?;
            xattrs.insert(name, value);
        }
        Ok(
            json!({"kind": kind, "inode": node.ino(), "mode": attr.mode & 0o7777,
            "uid": attr.uid, "gid": attr.gid, "size": attr.size, "nlink": attr.nlink,
            "mtime": attr.mtime, "mtime_nsec": attr.mtimensec, "xattrs": xattrs,
            "target": if node.is_symlink() { Some(node.get_symlink()?.into_string().map_err(|_| io::Error::from_raw_os_error(libc::EILSEQ))?) } else { None }}),
        )
    }

    fn list(&self, path: &str, offset: u32, limit: u32) -> Result<Value> {
        ensure!((1..=128).contains(&limit), "invalid directory page size");
        let node = self.node(path)?;
        if !node.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR).into());
        }
        let count = node.get_child_count();
        ensure!(
            count <= 200_000 && offset <= count,
            "invalid directory offset/count"
        );
        let end = count.min(offset.saturating_add(limit));
        let mut names = Vec::new();
        for index in offset..end {
            let child = node.get_child_by_index(index)?;
            let name = child
                .name()
                .into_string()
                .map_err(|_| io::Error::from_raw_os_error(libc::EILSEQ))?;
            ensure!(
                !name.is_empty()
                    && !matches!(name.as_str(), "." | "..")
                    && !name.contains(['/', '\0']),
                "invalid child name"
            );
            names.push(name);
        }
        Ok(json!({"names": names, "next_offset": if end < count { Some(end) } else { None }}))
    }

    fn read(
        &self,
        path: &str,
        offset: u64,
        length: u32,
        io_budget: Option<&mut node::budget::IoLease>,
    ) -> Result<Vec<u8>> {
        ensure!(
            length as usize <= MAX_READ && offset.checked_add(length as u64).is_some(),
            "invalid read range"
        );
        let node = self.node(path)?;
        if !node.is_reg() {
            return Err(io::Error::from_raw_os_error(if node.is_dir() {
                libc::EISDIR
            } else {
                libc::ELOOP
            })
            .into());
        }
        let count = (node.size().saturating_sub(offset)).min(length as u64) as usize;
        let mut vectors = if count == 0 {
            Vec::new()
        } else {
            node.alloc_bio_vecs(&self.device, offset, count, true)?
        };
        if let Some(budget) = io_budget {
            let mut bytes = (count as u64) * 2;
            for vector in &vectors {
                for index in 0..vector.len() {
                    let desc = vector
                        .blob_io_desc(index)
                        .context("missing chunk descriptor")?;
                    let c = &desc.chunkinfo;
                    ensure!(
                        c.uncompressed_size() <= 1024 * 1024
                            && c.compressed_size() <= c.uncompressed_size().saturating_add(65536),
                        "node chunk buffer exceeds supported geometry"
                    );
                    bytes = bytes
                        .checked_add(
                            u64::from(c.uncompressed_size()) * 4
                                + u64::from(c.compressed_size()) * 2
                                + 256,
                        )
                        .context("node read charge overflow")?;
                }
            }
            budget.grow(bytes)?;
        }
        let mut output = Buffer {
            bytes: Vec::with_capacity(count),
            capacity: count,
        };
        if count != 0 {
            for io in &mut vectors {
                self.device.read_to(&mut output, io)?;
                if self.node_cache {
                    // The node requires caller-side persistence. Confirm the
                    // same chunks reached READY before acknowledging the read;
                    // failed persistence must not masquerade as retained cache.
                    // This is not a power-loss durability guarantee.
                    for index in 0..io.len() {
                        let desc = io.blob_io_desc(index).context("missing chunk descriptor")?;
                        self.device.fetch_range_synchronous(&[BlobPrefetchRequest {
                            blob_id: desc.blob.blob_id(),
                            offset: desc.chunkinfo.uncompressed_offset(),
                            len: u64::from(desc.chunkinfo.uncompressed_size()),
                        }])?;
                    }
                }
            }
        }
        ensure!(output.bytes.len() == count, "short authenticated read");
        // Do not emit a prefix before all involved chunks passed authentication.
        Ok(output.bytes)
    }
}

struct Buffer {
    bytes: Vec<u8>,
    capacity: usize,
}
impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        ensure_capacity(self.bytes.len(), bytes.len(), self.capacity)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn ensure_capacity(used: usize, count: usize, capacity: usize) -> io::Result<()> {
    if count > capacity.saturating_sub(used) {
        Err(io::Error::from_raw_os_error(libc::EOVERFLOW))
    } else {
        Ok(())
    }
}
impl ZeroCopyWriter for Buffer {
    fn write_from(
        &mut self,
        file: &mut dyn FileReadWriteVolatile,
        count: usize,
        offset: u64,
    ) -> io::Result<usize> {
        ensure_capacity(self.bytes.len(), count, self.capacity)?;
        let mut temporary = vec![0; count];
        let slice =
            unsafe { FileVolatileSlice::from_raw_ptr(temporary.as_mut_ptr(), temporary.len()) };
        let n = file.read_at_volatile(slice, offset)?;
        if n == 0 && count != 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.bytes.extend_from_slice(&temporary[..n]);
        Ok(n)
    }
    fn available_bytes(&self) -> usize {
        self.capacity - self.bytes.len()
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Stat {
        path: String,
    },
    List {
        path: String,
        offset: u32,
        limit: u32,
    },
    Read {
        path: String,
        offset: u64,
        length: u32,
    },
}

fn execute(
    image: &Image,
    request: Request,
    mut budget: Option<&mut node::budget::IoLease>,
) -> Result<(Value, Vec<u8>)> {
    match request {
        Request::Stat { path } => Ok((json!({"status":"ok", "node":image.stat(&path)?}), vec![])),
        Request::List {
            path,
            offset,
            limit,
        } => {
            let page = image.list(&path, offset, limit)?;
            let mut result = json!({"status":"ok", "page":page});
            // Core already projects each listed child's attributes. Return that
            // metadata in the same native exchange, without reading file data.
            // Small I/O budgets preserve the old plain-list behavior.
            if image.node_cache
                && budget
                    .as_mut()
                    .is_some_and(|b| b.grow(node::budget::CONTROL_BYTES + 512 * 1024).is_ok())
            {
                let mut batch = Vec::new();
                let mut bytes = 0;
                for name in result["page"]["names"].as_array().unwrap() {
                    let name = name.as_str().unwrap();
                    let child = if path.is_empty() {
                        name.into()
                    } else {
                        format!("{path}/{name}")
                    };
                    let Ok(node) = image.stat(&child) else {
                        continue;
                    };
                    let row = json!({"name":name,"node":node});
                    let charge = serde_json::to_vec(&row)?.len();
                    if charge > 8192 || bytes + charge > 65536 {
                        continue;
                    }
                    bytes += charge;
                    batch.push(row);
                }
                result["metadata_batch"] = json!(batch);
            }
            Ok((result, vec![]))
        }
        Request::Read {
            path,
            offset,
            length,
        } => {
            let body = image.read(&path, offset, length, budget)?;
            Ok((
                json!({"status":"ok", "length":body.len(), "sha256":sha256(&body)}),
                body,
            ))
        }
    }
}

fn error_code(error: &anyhow::Error) -> i32 {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<io::Error>())
        .map(|e| {
            e.raw_os_error().unwrap_or(match e.kind() {
                io::ErrorKind::NotFound => libc::ENOENT,
                io::ErrorKind::PermissionDenied => libc::EACCES,
                io::ErrorKind::AlreadyExists => libc::EEXIST,
                io::ErrorKind::NotADirectory => libc::ENOTDIR,
                io::ErrorKind::IsADirectory => libc::EISDIR,
                io::ErrorKind::InvalidInput => libc::EINVAL,
                io::ErrorKind::Unsupported => libc::EOPNOTSUPP,
                io::ErrorKind::WouldBlock => libc::EAGAIN,
                io::ErrorKind::OutOfMemory => libc::ENOMEM,
                _ => libc::EIO,
            })
        })
        .unwrap_or(libc::EINVAL)
}

fn write_result(output: &mut impl Write, result: Result<(Value, Vec<u8>)>) -> Result<()> {
    match result {
        Ok((reply, body)) => {
            frame(output, &reply)?;
            output.write_all(&body)?;
        }
        Err(error) => {
            let errno = error_code(&error);
            frame(
                output,
                &json!({"status":"error","errno":errno,"message":error.to_string()}),
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

fn frame(output: &mut impl Write, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "response too large");
    output.write_all(&(bytes.len() as u32).to_be_bytes())?;
    output.write_all(&bytes)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 3 && args[0] == "--node" {
        return node::serve(Path::new(&args[1]), Path::new(&args[2]));
    }
    ensure!(
        args.len() == 3,
        "usage: rafs_reader MERGED_BUNDLE MERGED_HANDLE NEW_CACHE_DIRECTORY"
    );
    let handle = args[1].to_str().context("non-UTF-8 handle")?;
    let image = Image::open(Path::new(&args[0]), handle, Path::new(&args[2]))?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    frame(&mut output, &json!({"version": 1, "handle": handle}))?;
    output.flush()?;
    loop {
        let mut header = [0; 4];
        if input.read(&mut header[..1])? == 0 {
            break;
        }
        input.read_exact(&mut header[1..])?;
        let size = u32::from_be_bytes(header) as usize;
        ensure!(size > 0 && size <= MAX_FRAME, "invalid request frame");
        let mut bytes = vec![0; size];
        input.read_exact(&mut bytes)?;
        write_result(
            &mut output,
            serde_json::from_slice(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|request| execute(&image, request, None)),
        )?;
    }
    Ok(())
}
