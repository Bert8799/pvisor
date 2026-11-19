//! Offline qualification helper, built as an example of the pinned Nydus source.
//! Reads all supported nodes without applying host DAC or extracting image paths.
//! Whole-file hashes are compared against the independent OCI inventory; this is
//! not the production authenticated Range reader or a replacement for its tests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use nydus_api::ConfigV2;
use nydus_rafs::metadata::RafsSuper;
use nydus_rafs::RafsIoReader;
use nydus_storage::device::BlobFeatures;
use nydus_utils::digest::DigestHasher;
use nydus_utils::{compress, digest};
use serde_json::{json, Value};

fn load(bootstrap: &Path, blobs: &Path, cache: &Path) -> Result<(RafsSuper, RafsIoReader)> {
    fs::create_dir(cache)?;
    let config: ConfigV2 = serde_json::from_value(json!({
        "version": 2,
        "backend": {"type": "localfs", "localfs": {"dir": blobs}},
        "cache": {"type": "blobcache", "validate": true, "filecache": {"work_dir": cache}},
        "rafs": {"mode": "direct", "validate": false, "enable_xattr": true}
    }))?;
    let loaded = RafsSuper::load_from_file(bootstrap, Arc::new(config), false)?;
    ensure!(loaded.0.meta.is_v6(), "only RAFS v6 is qualified");
    Ok(loaded)
}

fn describe(bootstrap: &Path, blobs: &Path, cache: &Path) -> Result<Value> {
    let (rafs, _reader) = load(bootstrap, blobs, cache)?;
    let descriptions: Vec<_> = rafs
        .superblock
        .get_blob_infos()
        .iter()
        .map(|info| {
            json!({
                "id": info.blob_id(),
                "toc_sha256": format!("sha256:{}", hex::encode(info.blob_toc_digest())),
                "toc_size": info.blob_toc_size(),
                "inline_chunk_digests": info.has_feature(BlobFeatures::INLINED_CHUNK_DIGEST),
                "digester_sha256": info.digester() == digest::Algorithm::Sha256,
            })
        })
        .collect();
    Ok(json!(descriptions))
}

fn inventory(bootstrap: &Path, blobs: &Path, cache: &Path) -> Result<Value> {
    let (rafs, _reader) = load(bootstrap, blobs, cache)?;
    ensure!(rafs.meta.is_v6(), "only RAFS v6 is qualified");
    let mut readers = HashMap::new();
    for info in rafs.superblock.get_blob_infos() {
        let id = info.blob_id();
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid blob ID"
        );
        let path = blobs.join(id);
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "blob is not regular"
        );
        readers.insert(info.blob_index(), (File::open(path)?, info.compressor()));
    }
    let root = rafs.get_extended_inode(rafs.superblock.root_ino(), false)?;
    let mut pending = vec![(root, String::new())];
    let mut directories = HashSet::new();
    let mut groups: HashMap<u64, Vec<String>> = HashMap::new();
    let mut tree = BTreeMap::new();
    while let Some((inode, path)) = pending.pop() {
        ensure!(tree.len() < 200_000, "too many nodes");
        ensure!(!tree.contains_key(&path), "duplicate path");
        let attr = inode.get_attr();
        let kind = if inode.is_dir() {
            "directory"
        } else if inode.is_reg() {
            "file"
        } else {
            ensure!(inode.is_symlink(), "unsupported node at {path:?}");
            "symlink"
        };
        let nanos = attr
            .mtime
            .checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(attr.mtimensec as u64))
            .context("mtime exceeds the qualification reader's range")?;
        ensure!(attr.mtimensec < 1_000_000_000, "invalid mtime nanoseconds");
        let mut xattrs = BTreeMap::new();
        for key in inode.get_xattrs()? {
            let value = inode
                .get_xattr(OsStr::new(std::str::from_utf8(&key)?))?
                .context("missing xattr")?;
            xattrs.insert(String::from_utf8(key)?, value);
        }
        let mut entry = json!({
            "kind": kind, "mode": attr.mode & 0o7777,
            "uid": attr.uid, "gid": attr.gid, "mtime_ns": nanos, "xattrs": xattrs
        });
        if inode.is_dir() {
            ensure!(directories.insert(inode.ino()), "directory alias or cycle");
            ensure!(
                pending.len() + inode.get_child_count() as usize <= 200_000,
                "too many children"
            );
            for index in 0..inode.get_child_count() {
                // Do not use RafsIterator: it logs and skips failed children.
                let child = inode.get_child_by_index(index)?;
                let name = child.name();
                let name = name
                    .to_str()
                    .context("non-UTF-8 names are not yet qualified")?;
                ensure!(
                    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']),
                    "invalid child name"
                );
                pending.push((
                    child,
                    if path.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{path}/{name}")
                    },
                ));
            }
        } else if inode.is_reg() {
            let mut hasher = digest::RafsDigest::hasher(digest::Algorithm::Sha256);
            let mut length = 0u64;
            for index in 0..inode.get_chunk_count() {
                let chunk = inode.get_chunk_info(index)?;
                ensure!(
                    !chunk.is_encrypted() && !chunk.is_batch(),
                    "unsupported chunk encoding"
                );
                let (file, compressor) =
                    readers.get(&chunk.blob_index()).context("missing blob")?;
                let compressed_size = chunk.compressed_size() as usize;
                let size = chunk.uncompressed_size() as usize;
                ensure!(
                    compressed_size <= 16 * 1024 * 1024 && size <= 16 * 1024 * 1024,
                    "chunk too large"
                );
                let end = chunk
                    .compressed_offset()
                    .checked_add(compressed_size as u64)
                    .context("offset overflow")?;
                ensure!(end <= file.metadata()?.len(), "chunk outside blob");
                let mut bytes = vec![0; compressed_size];
                file.read_exact_at(&mut bytes, chunk.compressed_offset())?;
                if chunk.is_compressed() {
                    let mut decoded = vec![0; size];
                    ensure!(
                        compress::decompress(&bytes, &mut decoded, *compressor)? == size,
                        "short decode"
                    );
                    bytes = decoded;
                }
                ensure!(bytes.len() == size, "invalid uncompressed chunk size");
                length = length
                    .checked_add(size as u64)
                    .context("file size overflow")?;
                ensure!(length <= inode.size(), "chunk data exceeds file size");
                hasher.digest_update(&bytes);
            }
            ensure!(length == inode.size(), "missing file data");
            entry["size"] = json!(length);
            entry["sha256"] = json!(format!("sha256:{}", hasher.digest_finalize()));
            groups.entry(inode.ino()).or_default().push(path.clone());
        } else {
            entry["target"] = json!(inode
                .get_symlink()?
                .to_str()
                .context("non-UTF-8 symlink target")?);
        }
        tree.insert(path, entry);
    }
    for names in groups.values_mut() {
        names.sort();
        for name in names.iter() {
            tree.get_mut(name).unwrap()["hardlinks"] = json!(names);
        }
    }
    Ok(serde_json::to_value(tree)?)
}

fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let description = args.first().is_some_and(|arg| arg == "--describe");
    if description {
        args.remove(0);
    }
    ensure!(
        args.len() == 3,
        "usage: rafs_inventory [--describe] BOOTSTRAP BLOBS NEW_CACHE_DIRECTORY"
    );
    println!(
        "{}",
        (if description { describe } else { inventory })(
            Path::new(&args[0]),
            Path::new(&args[1]),
            Path::new(&args[2])
        )?
    );
    Ok(())
}
