//! Versioned requests, responses, and length-prefixed wire frames.
use super::ImageTotals;
use anyhow::ensure;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub(super) const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_READ: u32 = 1024 * 1024;

/// All paths are Unix bytes, relative to image root. Envelope version 2 permits
/// sequential framed exchanges; other versions are rejected.
/// A `Read` or `Metadata` response frame is followed by exactly `Data.length` raw bytes before
/// the next request/response frame. There is no pipelining or protocol-level replay;
/// private pinned-read clients may retry a transport disconnect once per hop.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    /// Open a published immutable revision without reading mutable HEAD.
    Open {
        handle: String,
        architecture: String,
    },
    Prepare {
        image: String,
        architecture: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        refresh: bool,
    },
    List {
        digest: String,
        path: Vec<u8>,
        offset: usize,
    },
    Stat {
        digest: String,
        path: Vec<u8>,
    },
    /// Read immutable portable metadata, never file content or arbitrary keys.
    /// Supported when Prepared advertises metadata_pages.
    /// Length must be 1..=MAX_READ; offset + length must not overflow. Offsets
    /// beyond the object are rejected, and reads crossing EOF are shortened.
    /// The Data frame is followed by its length of SHA-256 authenticated bytes.
    Metadata {
        /// Immutable revision handle; mutable image references are not accepted.
        handle: String,
        /// Exact whitelisted metadata basename, not a storage key or path.
        object_name: String,
        /// Byte offset within the named object, including its EOF boundary.
        offset: u64,
        /// Maximum number of bytes returned in the following Data body.
        length: u32,
    },
    Read {
        digest: String,
        path: Vec<u8>,
        offset: u64,
        length: u32,
    },
    /// Node-owned RAFS reads and explicit lifecycle management. Requires a configured RAFS service;
    /// daemonless filesystem/S3 backends reject it. The handle is immutable and
    /// the operation cannot select tools, origins or arbitrary host paths.
    Rafs {
        /// Exact published merged-image identity.
        handle: String,
        /// Bounded typed operation; authorization precedes reads and lifecycle effects.
        request: RafsRequest,
        /// Omitted for initial Open or resource Stats. Subsequent operations bind the exact
        /// service generation returned by Open or Stats; a restart requires new admission.
        #[serde(default)]
        generation: Option<String>,
    },
}

/// Image reads and explicit lifecycle management for a node-local RAFS generation.
/// Paths are UTF-8, relative, at most 4096 bytes/256 components with no dot or
/// parent components. Directory pages contain at most 128 names; data at most
/// MAX_READ bytes. Invalid requests have no authority to open other host files.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum RafsRequest {
    /// Node resource snapshot, with no image load or data body. The fixed handle
    /// remains syntactically required. Generation may be omitted for discovery;
    /// supplied generations are fenced as for reads. Counters describe declared
    /// backing/work buffers separately from the native process address space/RSS.
    Stats,
    /// Retain, release or inspect a durable dependency on this exact image.
    /// Retain must precede publishing a dependent owner; release must follow its
    /// durable deletion. No TTL or automatic owner-death release applies. Owner
    /// IDs must never be reused for unrelated owners. Calls are idempotent by
    /// kind/owner/handle; lost replies may follow committed effects. Requires the
    /// current service generation; does not enable image-backed template capture.
    Lease {
        /// Dependency type: `template` or `checkpoint`.
        kind: String,
        /// Immutable owner ID: 1..=128 ASCII letters, digits, `_` or `-`.
        owner: String,
        /// `retain`, `release` or `inspect`; no path or executable authority.
        action: String,
    },
    /// Best-effort whole-blob disk reclamation. Active commits, durable dependencies
    /// and native backing references are protected. May finish above the target;
    /// the reply reports whether it was reached. Requires the service generation.
    Trim {
        /// Desired remaining reserved cache bytes, at most the configured limit.
        /// Zero requests all currently reclaimable objects, without changing capacity.
        target_bytes: u64,
    },
    /// Remove this node's metadata-only revision if unleased and unretained.
    /// Busy/retained revisions fail; full publication bundles are unsupported.
    /// Interrupted tombstones are recovered before new service readiness. A lost
    /// reply may follow deletion; inspect metadata before retrying. Requires the
    /// service generation. Does not delete origin objects or full source stores.
    Remove {},
    /// Verify that client and node lease the same physical metadata commit.
    Open {
        /// Device of the caller's independently opened MERGED.json.
        commit_device: u64,
        /// Inode of that leased commit; zero is invalid.
        commit_inode: u64,
    },
    /// Read complete logical metadata, including links and xattrs.
    Stat {
        /// Relative image path; the empty string denotes the root.
        path: String,
    },
    /// Read one stable directory-name page without allocating child inodes.
    List {
        /// Relative directory path.
        path: String,
        /// Native directory cursor, initially zero.
        offset: u32,
        /// Requested page size in 1..=128.
        limit: u32,
    },
    /// Read authenticated file bytes, returning no prefix on chunk failure.
    Read {
        /// Relative regular-file path.
        path: String,
        /// Byte offset, with checked offset+length arithmetic.
        offset: u64,
        /// Maximum response bytes, at most MAX_READ; EOF may shorten it.
        length: u32,
    },
}
impl RafsRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let path = match self {
            Self::Stats | Self::Trim { .. } | Self::Remove {} => return Ok(()),
            Self::Lease {
                kind,
                owner,
                action,
            } => {
                ensure!(
                    matches!(kind.as_str(), "template" | "checkpoint")
                        && matches!(action.as_str(), "retain" | "release" | "inspect")
                        && !owner.is_empty()
                        && owner.len() <= 128
                        && owner
                            .bytes()
                            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-')),
                    "invalid RAFS dependency request"
                );
                return Ok(());
            }
            Self::Open { commit_inode, .. } => {
                ensure!(*commit_inode != 0, "invalid RAFS commit identity");
                return Ok(());
            }
            Self::Stat { path } => path,
            Self::List { path, limit, .. } => {
                ensure!((1..=128).contains(limit), "invalid RAFS directory page");
                path
            }
            Self::Read {
                path,
                offset,
                length,
            } => {
                ensure!(
                    *length <= MAX_READ && offset.checked_add(u64::from(*length)).is_some(),
                    "invalid RAFS read range"
                );
                path
            }
        };
        ensure!(
            path.len() <= 4096 && !path.starts_with('/') && !path.contains('\0'),
            "invalid RAFS path"
        );
        ensure!(
            path.is_empty()
                || (path.split('/').count() <= 256
                    && path.split('/').all(|c| !matches!(c, "" | "." | ".."))),
            "invalid RAFS path components"
        );
        Ok(())
    }
}

/// Version 2 retains the connection for
/// sequential exchanges until EOF, idle timeout, or a transport/framing failure.
/// Clients negotiate with one version-2 `Ping` on a fresh connection
/// before sending any actual request. Authorization is
/// checked on every envelope, and the version cannot change within a connection.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    /// Must be 2; unsupported values fail before request execution.
    pub(super) version: u32,
    pub(super) token: Option<String>,
    pub(super) request: Request,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ready,
    Prepared {
        /// Immutable image/platform/revision handle used by every reader.
        image_handle: String,
        metadata_generation: String,
        /// This immutable handle supports Metadata reads.
        /// An absent capability selects the bounded RPC metadata path.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        metadata_pages: bool,
        #[serde(default)]
        totals: Option<ImageTotals>,
        digest: String,
        architecture: String,
        env: std::collections::BTreeMap<String, String>,
        entrypoint: Vec<String>,
        cmd: Vec<String>,
    },
    Entries {
        names: Vec<Vec<u8>>,
        /// Attributes aligned with names, required on every directory page.
        metadata: Vec<Response>,
        next_offset: Option<usize>,
    },
    Metadata {
        kind: String,
        size: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        inode: u64,
        nlink: u64,
        mtime: i64,
        mtime_nsec: i64,
        target: Option<Vec<u8>>,
    },
    Data {
        length: u32,
        sha256: String,
    },
    /// Validated native RAFS reply and an optional following binary body.
    /// The native frame retains lossless metadata/errno semantics; clients also
    /// verify the bounded body length and SHA-256 before decoding the reply.
    Rafs {
        /// Native metadata, names, open/data declaration or lifecycle acknowledgement.
        frame: serde_json::Value,
        /// Exact binary bytes following this response frame, at most MAX_READ.
        length: u32,
        /// Lowercase SHA-256 identity of those bytes, including an empty body.
        sha256: String,
    },
    Error {
        code: String,
        message: String,
    },
}

pub(super) fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> anyhow::Result<T> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_FRAME,
        "invalid cache frame length"
    );
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_FRAME,
        "cache response exceeds frame limit"
    );
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

pub(super) fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::util::encode_hex(&Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rafs_operations_reject_unsafe_paths_ranges_and_unknown_authority() {
        for op in [
            serde_json::json!({"op":"write","path":"file"}),
            serde_json::json!({"op":"open","commit_device":1,"commit_inode":2,"origin":"https://other"}),
            serde_json::json!({"op":"stat","path":"file","reader":"/bin/sh"}),
        ] {
            assert!(serde_json::from_value::<RafsRequest>(op).is_err());
        }
        for path in [
            "/absolute",
            "../escape",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "a\0b",
        ] {
            assert!(RafsRequest::Stat { path: path.into() }.validate().is_err());
        }
        assert!(RafsRequest::Stat { path: "".into() }.validate().is_ok());
        assert!(
            RafsRequest::Open {
                commit_device: 0,
                commit_inode: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            RafsRequest::Read {
                path: "file".into(),
                offset: u64::MAX,
                length: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            RafsRequest::Read {
                path: "file".into(),
                offset: 0,
                length: MAX_READ + 1
            }
            .validate()
            .is_err()
        );
        for limit in [0, 129] {
            assert!(
                RafsRequest::List {
                    path: "".into(),
                    offset: 0,
                    limit
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn lifecycle_requests_are_strict_and_never_use_immutable_read_replay() {
        for (kind, owner, action) in [
            ("other", "owner", "retain"),
            ("template", "../owner", "retain"),
            ("checkpoint", "owner", "expire"),
            ("template", "", "inspect"),
        ] {
            assert!(
                RafsRequest::Lease {
                    kind: kind.into(),
                    owner: owner.into(),
                    action: action.into()
                }
                .validate()
                .is_err()
            );
        }
        for operation in [
            RafsRequest::Lease {
                kind: "template".into(),
                owner: "generation-1".into(),
                action: "retain".into(),
            },
            RafsRequest::Trim { target_bytes: 0 },
            RafsRequest::Remove {},
        ] {
            operation.validate().unwrap();
            let request = Request::Rafs {
                handle: "pvisor-rafs-merged-v1:fixed".into(),
                request: operation,
                generation: Some("generation".into()),
            };
            assert!(!super::super::client::pinned_read(
                "pvisor-rafs-merged-v1:fixed",
                &request
            ));
        }
        assert!(
            serde_json::from_value::<RafsRequest>(
                serde_json::json!({"op":"remove","path":"/arbitrary"})
            )
            .is_err()
        );
    }

    #[test]
    fn metadata_pages_capability_defaults_false_and_is_optional_on_the_wire() {
        let old = serde_json::json!({
            "status": "prepared", "image_handle": "pvisor-v2:revision",
            "metadata_generation": "sha256:revision", "digest": "sha256:manifest",
            "architecture": "amd64", "env": {}, "entrypoint": [], "cmd": []
        });
        let response: Response = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(
            &response,
            Response::Prepared {
                metadata_pages: false,
                ..
            }
        ));
        assert!(
            serde_json::to_value(response)
                .unwrap()
                .get("metadata_pages")
                .is_none()
        );
        let mut capable = old;
        capable["metadata_pages"] = serde_json::json!(true);
        let response: Response = serde_json::from_value(capable).unwrap();
        assert!(matches!(
            &response,
            Response::Prepared {
                metadata_pages: true,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(response).unwrap()["metadata_pages"],
            true
        );
    }

    #[test]
    fn metadata_frames_and_data_bodies_preserve_v2_frame_boundaries() {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &Envelope {
                version: 2,
                token: Some("secret".into()),
                request: Request::Metadata {
                    handle: "pvisor-v2:revision".into(),
                    object_name: "index.bin".into(),
                    offset: 65536,
                    length: 65536,
                },
            },
        )
        .unwrap();
        let envelope: Envelope = read_frame(&mut wire.as_slice()).unwrap();
        assert_eq!(envelope.version, 2);
        assert!(matches!(
            envelope.request,
            Request::Metadata {
                offset: 65536,
                length: 65536,
                ..
            }
        ));

        let body = b"metadata";
        wire.clear();
        write_frame(
            &mut wire,
            &Response::Data {
                length: body.len() as u32,
                sha256: hash(body),
            },
        )
        .unwrap();
        wire.extend_from_slice(body);
        write_frame(&mut wire, &Response::Ready).unwrap();
        let mut stream = wire.as_slice();
        let response: Response = read_frame(&mut stream).unwrap();
        assert!(matches!(response, Response::Data { length: 8, sha256 } if sha256 == hash(body)));
        let mut received = [0; 8];
        stream.read_exact(&mut received).unwrap();
        assert_eq!(&received, body);
        assert!(matches!(
            read_frame::<Response>(&mut stream).unwrap(),
            Response::Ready
        ));
    }

    #[test]
    fn prepared_responses_require_an_immutable_handle_and_generation() {
        let complete = serde_json::json!({
            "status": "prepared", "image_handle": "pvisor-v2:revision",
            "metadata_generation": "sha256:revision", "digest": "sha256:manifest",
            "architecture": "amd64", "env": {}, "entrypoint": [], "cmd": []
        });
        serde_json::from_value::<Response>(complete.clone()).unwrap();
        for field in ["image_handle", "metadata_generation"] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Response>(missing).is_err());
            let mut null = complete.clone();
            null[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<Response>(null).is_err());
        }
    }
}
