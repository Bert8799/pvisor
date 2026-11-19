#!/usr/bin/env python3
"""Convert, qualify and atomically publish local RAFS v6 image bundles."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import stat
import tempfile
from contextlib import contextmanager
from pathlib import Path

if __package__:
    from . import probe_nydus as convert
else:
    import probe_nydus as convert

PREFIX = "pvisor-rafs-v1:"
LAYER_PREFIX = "pvisor-rafs-layer-v1:"
MAX_CONTROL = 1024 * 1024
MAX_INVENTORY = 256 * 1024 * 1024
HEX = re.compile(r"[0-9a-f]{64}\Z")
METADATA = {"image.bootstrap", "source.manifest.json", "source.config.json"}


class PublicationUncertain(RuntimeError):
    """A complete revision is visible, but its directory fsync failed."""

    def __init__(self, handle: str):
        self.handle = handle
        super().__init__(f"revision is visible but durability is uncertain; inspect {handle}")


def encoded(value) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n"
    ).encode()


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def check_digest(value: str) -> None:
    if (
        not isinstance(value, str)
        or not value.startswith("sha256:")
        or not HEX.fullmatch(value[7:])
    ):
        raise ValueError("expected a lowercase SHA-256 digest")


def keys(value, expected) -> None:
    if not isinstance(value, dict) or value.keys() != set(expected):
        raise ValueError("unsupported record fields")


def descriptor(value) -> None:
    keys(value, ("sha256", "size"))
    check_digest(value["sha256"])
    if type(value["size"]) is not int or not 0 <= value["size"] < 2**63:
        raise ValueError("invalid object size")


def handle_revision(handle: str) -> str:
    if not isinstance(handle, str) or not handle.startswith((PREFIX, LAYER_PREFIX)):
        raise ValueError("expected an immutable RAFS image or layer handle")
    revision = handle.split(":", 1)[1]
    if not HEX.fullmatch(revision):
        raise ValueError("invalid revision")
    return revision


def sync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def directory(path: Path, *, create: bool) -> None:
    if create:
        try:
            path.mkdir(mode=0o700)
        except FileExistsError:
            pass
        else:
            sync_directory(path.parent)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError(f"store directory must be private, owned and non-symlink: {path}")


def store_root(path: Path, *, create: bool) -> Path:
    # Do not resolve away a symlink before checking the store itself.
    path = Path(os.path.abspath(path))
    for relative in ("", "blobs", "blobs/sha256", "revisions", "staging", "locks"):
        directory(path / relative, create=create)
    return path


@contextmanager
def regular(path: Path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError(f"expected a regular non-symlink file: {path}")
        stream = os.fdopen(fd, "rb")
        fd = -1
        with stream:
            yield stream
    finally:
        if fd != -1:
            os.close(fd)


def consume(path: Path, expected: dict, destination=None) -> None:
    descriptor(expected)
    with regular(path) as source:
        before = os.fstat(source.fileno())
        if before.st_size != expected["size"]:
            raise ValueError(f"object size mismatch: {path}")
        digest = hashlib.sha256()
        count = 0
        while data := source.read(128 * 1024):
            count += len(data)
            if count > expected["size"]:
                raise ValueError(f"object grew while reading: {path}")
            digest.update(data)
            if destination is not None:
                destination.write(data)
        after = os.fstat(source.fileno())

        def identity(info):
            # Concurrent bundle hardlinks legitimately change ctime/nlink.
            # The pinned full-file digest authenticates the bytes being copied.
            return (info.st_size, info.st_mtime_ns)

        if (
            count != expected["size"]
            or "sha256:" + digest.hexdigest() != expected["sha256"]
            or identity(before) != identity(after)
        ):
            raise ValueError(f"object digest or identity mismatch: {path}")


def read_control(path: Path, expected: dict | None = None) -> bytes:
    with regular(path) as source:
        if os.fstat(source.fileno()).st_size > MAX_CONTROL:
            raise ValueError("control object exceeds size limit")
        data = source.read(MAX_CONTROL + 1)
    if len(data) > MAX_CONTROL:
        raise ValueError("control object exceeds size limit")
    if expected is not None and (
        sha256(data) != expected["sha256"] or len(data) != expected["size"]
    ):
        raise ValueError(f"control object mismatch: {path}")
    return data


def copy_verified(source: Path, target: Path, expected: dict) -> None:
    with target.open("xb") as output:
        consume(source, expected, output)
        output.flush()
        os.fchmod(output.fileno(), 0o444)
        os.fsync(output.fileno())


def validate_record(record: dict) -> None:
    keys(record, ("schema", "source", "bootstrap", "blobs", "qualification"))
    if record["schema"] not in ("pvisor-rafs-v1", "pvisor-rafs-layer-v1"):
        raise ValueError("unsupported bundle schema")
    is_layer = record["schema"] == "pvisor-rafs-layer-v1"
    source = record["source"]
    keys(source, ("platform", "manifest", "config", *(["layer_index"] if is_layer else [])))
    if is_layer and (type(source["layer_index"]) is not int or source["layer_index"] < 0):
        raise ValueError("invalid source layer index")
    if source["platform"] not in ("linux/amd64", "linux/arm64"):
        raise ValueError("unsupported platform")
    for value in (record["bootstrap"], source["manifest"], source["config"]):
        descriptor(value)
    if max(source["manifest"]["size"], source["config"]["size"]) > MAX_CONTROL:
        raise ValueError("source control object exceeds size limit")
    blobs = record["blobs"]
    if not isinstance(blobs, dict) or len(blobs) > 4096:
        raise ValueError("invalid blob inventory")
    for blob_id, value in blobs.items():
        if not HEX.fullmatch(blob_id):
            raise ValueError("invalid native blob ID")
        keys(value, ("artifact", "toc_sha256", "toc_size"))
        descriptor(value["artifact"])
        check_digest(value["toc_sha256"])
        if value["toc_sha256"] == "sha256:" + "0" * 64:
            raise ValueError("missing authenticated blob ToC")
        if (
            type(value["toc_size"]) is not int
            or not 0 < value["toc_size"] <= value["artifact"]["size"]
        ):
            raise ValueError("invalid blob ToC size")
    qualification = record["qualification"]
    keys(
        qualification,
        ("method", "inventory_sha256", "tools", *(["inventory_size"] if is_layer else [])),
    )
    if qualification["method"] != ("raw-oci-layer-v1" if is_layer else "full-oci-inventory-v1"):
        raise ValueError("unsupported qualification method")
    if is_layer and (
        type(qualification["inventory_size"]) is not int
        or not 0 < qualification["inventory_size"] <= MAX_INVENTORY
    ):
        raise ValueError("layer inventory exceeds size limit")
    check_digest(qualification["inventory_sha256"])
    keys(qualification["tools"], ("converter", "daemon", "reader"))
    for value in qualification["tools"].values():
        check_digest(value)


def verify_bundle(bundle: Path, handle: str) -> dict:
    revision = handle_revision(handle)
    directory(bundle, create=False)
    for child in ("metadata", "blobs"):
        directory(bundle / child, create=False)
    raw = read_control(bundle / "COMMIT.json")
    if sha256(raw) != "sha256:" + revision:
        raise ValueError("revision identity mismatch")
    record = json.loads(raw)
    validate_record(record)
    if record["schema"] != handle.split(":", 1)[0]:
        raise ValueError("handle kind does not match the bundle schema")
    if encoded(record) != raw:
        raise ValueError("commit is not canonically encoded")
    if {p.name for p in bundle.iterdir()} != {"COMMIT.json", "metadata", "blobs"}:
        raise ValueError("unexpected bundle objects")
    is_layer = record["schema"] == "pvisor-rafs-layer-v1"
    expected_metadata = METADATA | ({"layer.inventory.json"} if is_layer else set())
    if {p.name for p in (bundle / "metadata").iterdir()} != expected_metadata:
        raise ValueError("incomplete metadata set")
    if {p.name for p in (bundle / "blobs").iterdir()} != record["blobs"].keys():
        raise ValueError("incomplete blob set")
    consume(bundle / "metadata/image.bootstrap", record["bootstrap"])
    source = record["source"]
    manifest = json.loads(
        read_control(bundle / "metadata/source.manifest.json", source["manifest"])
    )
    config = json.loads(read_control(bundle / "metadata/source.config.json", source["config"]))
    if (
        manifest.get("schemaVersion") != 2
        or manifest.get("config", {}).get("digest") != source["config"]["sha256"]
        or manifest.get("config", {}).get("size") != source["config"]["size"]
        or source["platform"] != f"{config.get('os')}/{config.get('architecture')}"
    ):
        raise ValueError("source identity or platform mismatch")
    if is_layer:
        if source["layer_index"] >= len(manifest.get("layers", [])):
            raise ValueError("source layer index is out of range")
        consume(
            bundle / "metadata/layer.inventory.json",
            {
                "sha256": record["qualification"]["inventory_sha256"],
                "size": record["qualification"]["inventory_size"],
            },
        )
    for blob_id, value in record["blobs"].items():
        consume(bundle / "blobs" / blob_id, value["artifact"])
    return record


def open_bundle(store: Path, handle: str) -> tuple[Path, dict]:
    """Offline full verification, not a lazy VM startup operation or a lease."""
    revision = handle_revision(handle)
    root = store_root(store, create=False)
    bundle = root / "revisions" / revision
    return bundle, verify_bundle(bundle, handle)


def describe_blobs(reader: Path, conversion: Path, timeout: float) -> list[dict]:
    output = conversion / "blob-descriptions.json"
    convert.run_tool(
        [
            str(reader),
            "--describe",
            str(conversion / "image.bootstrap"),
            str(conversion / "blobs"),
            str(conversion / "description-cache"),
        ],
        output,
        timeout,
    )
    result = json.loads(output.read_text())
    if not isinstance(result, list) or len(result) > 4096:
        raise ValueError("invalid native blob descriptions")
    return result


def install_blob(source: Path, expected: dict, root: Path, staging: Path) -> Path:
    temporary = staging / "object"
    copy_verified(source, temporary, expected)
    target = root / "blobs/sha256" / expected["sha256"][7:]
    try:
        os.link(temporary, target)
    except FileExistsError:
        consume(target, expected)
    else:
        sync_directory(target.parent)
    temporary.unlink()
    return target


@contextmanager
def publication_lock(root: Path):
    fd = os.open(
        root / "locks/publish.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600
    )
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("publication lock is not regular")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def install_revision(root: Path, bundle: Path, final: Path, handle: str, verify) -> None:
    """Commit a verified directory; callers sync nested files/directories first."""
    verify(bundle, handle)
    sync_directory(bundle)
    with publication_lock(root):
        if final.exists() or final.is_symlink():
            verify(final, handle)
        else:
            os.rename(bundle, final)
        try:
            sync_directory(final.parent)
        except OSError as error:
            raise PublicationUncertain(handle) from error


def publish(
    layout: Path,
    platform: str,
    image_tool: Path,
    daemon: Path,
    reader: Path,
    store: Path,
    work: Path,
    timeout: float = 60,
    *,
    layer_index: int | None = None,
) -> str:
    """Qualify before publishing; return only a durable immutable revision handle.

    Normal pre-commit failure removes owned staging. A killed publisher can leave
    unreachable staging/CAS objects, never a partial visible revision. Post-rename
    directory-sync failure raises PublicationUncertain with a verifiable handle.
    Store parents must exist; the store and all internal directories are private.
    """
    layout, work = layout.resolve(), work.resolve()
    store = Path(os.path.abspath(store))
    paths = (layout, work, store.resolve())
    if any(
        a == b or a in b.parents or b in a.parents
        for i, a in enumerate(paths)
        for b in paths[i + 1 :]
    ):
        raise ValueError("source layout, work directory and store must be disjoint")
    image_tool, daemon, reader = (p.resolve() for p in (image_tool, daemon, reader))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    conversion = work / "conversion"
    options = {"inventory_reader": reader}
    if layer_index is not None:
        options["raw_layer_index"] = layer_index
    report = convert.probe(layout, platform, image_tool, daemon, conversion, timeout, **options)
    if (
        not report["passed"]
        or report["error"]
        or report["differences"]
        or report["verification_mode"] != "direct-reader"
    ):
        raise ValueError(f"format qualification failed; see {conversion / 'report.json'}")
    identity, _layers = convert.load_image(layout, platform)
    if identity != report["source"]:
        raise ValueError("source identity changed after qualification")
    tools = {"converter": image_tool, "daemon": daemon, "reader": reader}
    tool_digests = {role: convert.digest_file(path) for role, path in tools.items()}
    if any(report["tools"].get(str(path)) != tool_digests[role] for role, path in tools.items()):
        raise ValueError("qualification tool changed")
    expected_artifacts = report["artifacts"]
    bootstrap = conversion / "image.bootstrap"
    record = {
        "schema": "pvisor-rafs-v1",
        "source": {
            "platform": platform,
            "manifest": {
                "sha256": identity["manifest"]["digest"],
                "size": identity["manifest"]["size"],
            },
            "config": {"sha256": identity["config"]["digest"], "size": identity["config"]["size"]},
        },
        "bootstrap": {
            "sha256": expected_artifacts["image.bootstrap"],
            "size": bootstrap.stat().st_size,
        },
        "blobs": {},
        "qualification": {
            "method": "full-oci-inventory-v1",
            "inventory_sha256": convert.digest_file(conversion / "expected.json"),
            "tools": tool_digests,
        },
    }
    if layer_index is not None:
        if report.get("raw_layer_index") != layer_index:
            raise ValueError("qualification did not retain the selected raw layer")
        record["schema"] = "pvisor-rafs-layer-v1"
        record["source"]["layer_index"] = layer_index
        record["qualification"]["method"] = "raw-oci-layer-v1"
        record["qualification"]["inventory_size"] = (conversion / "expected.json").stat().st_size
    for info in describe_blobs(reader, conversion, timeout):
        keys(info, ("id", "toc_sha256", "toc_size", "inline_chunk_digests", "digester_sha256"))
        blob_id = info["id"]
        if not isinstance(blob_id, str) or not HEX.fullmatch(blob_id) or blob_id in record["blobs"]:
            raise ValueError("invalid or duplicate native blob ID")
        if info["inline_chunk_digests"] is not True or info["digester_sha256"] is not True:
            raise ValueError("native blob lacks mandatory SHA-256 chunk validation")
        record["blobs"][blob_id] = {
            "artifact": {
                "sha256": expected_artifacts["blobs/" + blob_id],
                "size": (conversion / "blobs" / blob_id).stat().st_size,
            },
            "toc_sha256": info["toc_sha256"],
            "toc_size": info["toc_size"],
        }
    if set(expected_artifacts) != {
        "image.bootstrap",
        *("blobs/" + name for name in record["blobs"]),
    }:
        raise ValueError("bootstrap blob references do not match qualified artifacts")
    validate_record(record)
    raw = encoded(record)
    if len(raw) > MAX_CONTROL:
        raise ValueError("commit exceeds size limit")
    handle = record["schema"] + ":" + hashlib.sha256(raw).hexdigest()
    root = store_root(store, create=True)
    with tempfile.TemporaryDirectory(prefix="publish-", dir=root / "staging") as temporary:
        staging = Path(temporary)
        bundle = staging / "bundle"
        for path in (bundle, bundle / "metadata", bundle / "blobs"):
            path.mkdir(mode=0o700)
        copy_verified(bootstrap, bundle / "metadata/image.bootstrap", record["bootstrap"])
        for kind in ("manifest", "config"):
            value = record["source"][kind]
            copy_verified(
                layout / "blobs/sha256" / value["sha256"][7:],
                bundle / f"metadata/source.{kind}.json",
                value,
            )
        if layer_index is not None:
            copy_verified(
                conversion / "expected.json",
                bundle / "metadata/layer.inventory.json",
                {
                    "sha256": record["qualification"]["inventory_sha256"],
                    "size": record["qualification"]["inventory_size"],
                },
            )
        for blob_id, value in record["blobs"].items():
            target = install_blob(conversion / "blobs" / blob_id, value["artifact"], root, staging)
            os.link(target, bundle / "blobs" / blob_id)
        with (bundle / "COMMIT.json").open("xb") as output:
            output.write(raw)
            output.flush()
            os.fchmod(output.fileno(), 0o444)
            os.fsync(output.fileno())
        for path in (bundle / "metadata", bundle / "blobs"):
            sync_directory(path)
        install_revision(
            root, bundle, root / "revisions" / handle_revision(handle), handle, verify_bundle
        )
    return handle


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    publication = commands.add_parser("publish", help="convert, qualify and publish an OCI layout")
    publication.add_argument("--oci-layout", required=True, type=Path)
    publication.add_argument(
        "--platform", default="linux/amd64", choices=["linux/amd64", "linux/arm64"]
    )
    publication.add_argument("--nydus-image", required=True, type=Path)
    publication.add_argument("--nydusd", required=True, type=Path)
    publication.add_argument("--inventory-reader", required=True, type=Path)
    publication.add_argument("--work-dir", required=True, type=Path)
    publication.add_argument("--timeout", default=60.0, type=float)
    publication.add_argument(
        "--layer-index", type=int, help="publish one raw OCI changeset, retaining whiteouts"
    )
    inspection = commands.add_parser(
        "inspect", help="verify a complete revision, including every data blob"
    )
    inspection.add_argument("--handle", required=True)
    for command in (publication, inspection):
        command.add_argument("--store", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "publish":
            handle = publish(
                args.oci_layout,
                args.platform,
                args.nydus_image,
                args.nydusd,
                args.inventory_reader,
                args.store,
                args.work_dir,
                args.timeout,
                layer_index=args.layer_index,
            )
            print(json.dumps({"handle": handle}))
        else:
            bundle, record = open_bundle(args.store, args.handle)
            print(json.dumps({"handle": args.handle, "bundle": str(bundle), "commit": record}))
    except (ValueError, OSError, RuntimeError, KeyError, TypeError) as error:
        parser.exit(1, f"{type(error).__name__}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
