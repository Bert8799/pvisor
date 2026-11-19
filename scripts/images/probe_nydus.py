#!/usr/bin/env python3
"""Qualify OCI -> RAFS v6 conversion against a real, owned read-only mount."""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import json
import math
import os
import posixpath
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import time
from contextlib import contextmanager
from decimal import Decimal, InvalidOperation
from pathlib import Path

MAX_ENTRIES = 200_000
LAYER_TYPES = {
    "application/vnd.oci.image.layer.v1.tar": "tar-rafs",
    "application/vnd.oci.image.layer.v1.tar+gzip": "targz-rafs",
    "application/vnd.docker.image.rootfs.diff.tar.gzip": "targz-rafs",
}


def digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while data := stream.read(128 * 1024):
            digest.update(data)
    return "sha256:" + digest.hexdigest()


def save(path: Path, value) -> None:
    path.write_text(json.dumps(value, sort_keys=True, indent=2, ensure_ascii=True) + "\n")


def image_path(name: str) -> str:
    if name.startswith("/") or ".." in name.split("/") or "\0" in name:
        raise ValueError(f"unsafe image path: {name!r}")
    result = posixpath.normpath(name)
    return "" if result == "." else result


def verified_blob(layout: Path, descriptor: dict) -> Path:
    digest = descriptor["digest"]
    if not isinstance(digest, str) or not digest.startswith("sha256:"):
        raise ValueError("only sha256 OCI descriptors are supported")
    hex_digest = digest[7:]
    if len(hex_digest) != 64 or any(c not in "0123456789abcdef" for c in hex_digest):
        raise ValueError("invalid OCI descriptor digest")
    path = layout / "blobs" / "sha256" / hex_digest
    if path.is_symlink() or not path.is_file():
        raise ValueError("OCI blob must be a regular, non-symlink file")
    if path.stat().st_size != descriptor["size"] or digest_file(path) != digest:
        raise ValueError(f"OCI blob size/digest mismatch: {digest}")
    return path


def load_image(layout: Path, platform: str) -> tuple[dict, list[tuple[dict, Path]]]:
    if json.loads((layout / "oci-layout").read_text())["imageLayoutVersion"] != "1.0.0":
        raise ValueError("unsupported OCI layout version")
    index = json.loads((layout / "index.json").read_text())
    os_name, architecture = platform.split("/")
    selected = [
        descriptor
        for descriptor in index["manifests"]
        if descriptor.get("platform", {}).get("os") == os_name
        and descriptor.get("platform", {}).get("architecture") == architecture
    ]
    if len(selected) != 1:
        raise ValueError("select exactly one platform manifest in the OCI layout")
    descriptor = selected[0]
    manifest_path = verified_blob(layout, descriptor)
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("schemaVersion") != 2:
        raise ValueError("unsupported OCI manifest version")
    config_path = verified_blob(layout, manifest["config"])
    config = json.loads(config_path.read_text())
    if (config.get("os"), config.get("architecture")) != (os_name, architecture):
        raise ValueError("OCI configuration platform mismatch")
    layers = []
    for layer in manifest["layers"]:
        if layer["mediaType"] not in LAYER_TYPES:
            raise ValueError(f"unsupported OCI layer media type: {layer['mediaType']}")
        layers.append((layer, verified_blob(layout, layer)))
    if not layers:
        raise ValueError("empty OCI images are not qualified by this probe")
    rootfs = config.get("rootfs", {})
    if rootfs.get("type") != "layers" or len(rootfs.get("diff_ids", [])) != len(layers):
        raise ValueError("invalid OCI rootfs diff_ids")
    for expected, (layer, path) in zip(rootfs["diff_ids"], layers):
        opener = gzip.open if layer["mediaType"].endswith("gzip") else Path.open
        digest = hashlib.sha256()
        with opener(path, "rb") as stream:
            while data := stream.read(128 * 1024):
                digest.update(data)
        if expected != "sha256:" + digest.hexdigest():
            raise ValueError("OCI uncompressed layer diff_id mismatch")
    return {"manifest": descriptor, "config": manifest["config"], "platform": platform}, layers


def tar_entry(member: tarfile.TarInfo, archive: tarfile.TarFile) -> dict:
    kinds = {
        tarfile.REGTYPE: "file",
        tarfile.AREGTYPE: "file",
        tarfile.DIRTYPE: "directory",
        tarfile.SYMTYPE: "symlink",
        tarfile.LNKTYPE: "hardlink",
    }
    if member.type not in kinds:
        raise ValueError(f"unsupported node type at {member.name!r}")
    xattrs = {}
    for key, value in member.pax_headers.items():
        if key.startswith("SCHILY.xattr."):
            xattrs[key[len("SCHILY.xattr.") :]] = base64.b64encode(
                value.encode("utf-8", "surrogateescape")
            ).decode("ascii")
        elif key.startswith("LIBARCHIVE.xattr."):
            raise ValueError("LIBARCHIVE xattr encoding is not yet qualified")
        elif key.startswith("GNU.sparse"):
            raise ValueError("sparse tar encodings are not yet qualified")
    try:
        mtime_ns = Decimal(member.pax_headers.get("mtime", str(member.mtime))) * 1_000_000_000
    except InvalidOperation as error:
        raise ValueError("invalid mtime") from error
    if not mtime_ns.is_finite() or mtime_ns != mtime_ns.to_integral_value():
        raise ValueError("mtime must have at most nanosecond precision")
    entry = {
        "kind": kinds[member.type],
        "mode": member.mode & 0o7777,
        "uid": member.uid,
        "gid": member.gid,
        "mtime_ns": int(mtime_ns),
        "xattrs": xattrs,
    }
    if member.isfile():
        stream = archive.extractfile(member)
        if stream is None:
            raise ValueError("missing tar file body")
        digest = hashlib.sha256()
        count = 0
        with stream:
            while data := stream.read(128 * 1024):
                digest.update(data)
                count += len(data)
        if count != member.size:
            raise ValueError("short tar file body")
        entry.update(size=count, sha256="sha256:" + digest.hexdigest())
    elif member.issym():
        entry["target"] = member.linkname
    elif member.islnk():
        entry["target"] = image_path(member.linkname)
    return entry


def remove_tree(tree: dict, name: str, *, children_only: bool = False) -> None:
    prefix = name + "/" if name else ""
    for path in list(tree):
        if (not children_only and path == name) or (path != name and path.startswith(prefix)):
            del tree[path]


def apply_layer(tree: dict[str, dict], path: Path, *, whiteouts: bool = True) -> None:
    """Apply lower-only OCI whiteouts before adding this layer's entries."""
    entries = []
    names = set()
    with tarfile.open(path, mode="r|*") as archive:
        for ordinal, member in enumerate(archive):
            name = image_path(member.name)
            if name in names:
                raise ValueError(f"duplicate OCI layer path: {name!r}")
            names.add(name)
            entry = tar_entry(member, archive)
            entry["_origin"] = (str(path), ordinal)
            entries.append((name, entry))
            if len(entries) > MAX_ENTRIES:
                raise ValueError("layer exceeds entry limit")
    for name, entry in entries:
        parent, base = posixpath.split(name)
        if not name and entry["kind"] != "directory":
            raise ValueError("image root must be a directory")
        if base.startswith(".wh.") and (entry["kind"] != "file" or entry["size"] != 0):
            raise ValueError("OCI whiteout must be an empty regular file")
        if base == ".wh..wh..opq":
            if whiteouts:
                remove_tree(tree, parent, children_only=True)
        elif base.startswith(".wh."):
            if base[4:] in ("", ".", ".."):
                raise ValueError("unsafe whiteout target")
            if whiteouts:
                remove_tree(tree, posixpath.join(parent, base[4:]))
    pending = {}
    for name, entry in entries:
        if whiteouts and posixpath.basename(name).startswith(".wh."):
            continue
        parent = posixpath.dirname(name)
        parents = []
        while parent:
            parents.append(parent)
            parent = posixpath.dirname(parent)
        for parent in reversed(parents):
            if parent in tree and tree[parent]["kind"] != "directory":
                raise ValueError("image entry traverses a non-directory parent")
            tree.setdefault(parent, {"kind": "directory", "implicit": True})
        if name in tree and not (tree[name]["kind"] == entry["kind"] == "directory"):
            remove_tree(tree, name)
        pending.pop(name, None)
        if entry["kind"] == "hardlink":
            pending[name] = entry
        else:
            tree[name] = entry
        if len(tree) + len(pending) > MAX_ENTRIES:
            raise ValueError("image exceeds entry limit")
    while pending:
        progressed = False
        for name, entry in list(pending.items()):
            target = tree.get(entry["target"])
            if target is None or entry["target"] in pending:
                continue
            if target["kind"] != "file":
                raise ValueError("only regular-file hardlinks are qualified")
            for key in ("mode", "uid", "gid", "mtime_ns"):
                if entry[key] != target[key]:
                    raise ValueError(f"conflicting hardlink attributes: {name!r} {key}")
            if entry["xattrs"] and entry["xattrs"] != target["xattrs"]:
                raise ValueError(f"conflicting hardlink xattrs: {name!r}")
            tree[name] = target
            del pending[name]
            progressed = True
        if not progressed:
            raise ValueError("unresolved or cyclic hardlink")
    tree.setdefault("", {"kind": "directory", "implicit": True})


def overlay_tree(layers: list[Path], *, whiteouts: bool = True) -> dict[str, dict]:
    tree: dict[str, dict] = {}
    for path in layers:
        apply_layer(tree, path, whiteouts=whiteouts)
    return tree


def inventory(layers: list[Path], *, whiteouts: bool = True) -> dict[str, dict]:
    tree = overlay_tree(layers, whiteouts=whiteouts)
    groups: dict[int, list[str]] = {}
    for name, entry in tree.items():
        if entry["kind"] == "file":
            groups.setdefault(id(entry), []).append(name)
    return {
        name: {
            **{key: value for key, value in entry.items() if not key.startswith("_")},
            **({"hardlinks": sorted(groups[id(entry)])} if entry["kind"] == "file" else {}),
        }
        for name, entry in sorted(tree.items())
    }


def flattened_tar(layers: list[Path], destination: Path, *, whiteouts: bool = True) -> None:
    """Collapse OCI history offline, retaining numeric ownership without chown."""
    tree = overlay_tree(layers, whiteouts=whiteouts)
    groups: dict[int, list[str]] = {}
    for name, entry in sorted(tree.items()):
        if entry["kind"] == "file":
            groups.setdefault(id(entry), []).append(name)

    def header(name: str, entry: dict) -> tarfile.TarInfo:
        item = tarfile.TarInfo(name or ".")
        item.uid, item.gid = entry.get("uid", 0), entry.get("gid", 0)
        item.mode = entry.get("mode", 0o755)
        ns = entry.get("mtime_ns", 0)
        item.mtime = ns // 1_000_000_000
        item.pax_headers["mtime"] = format(Decimal(ns) / 1_000_000_000, "f")
        for key, value in entry.get("xattrs", {}).items():
            item.pax_headers["SCHILY.xattr." + key] = base64.b64decode(value).decode(
                "utf-8", "surrogateescape"
            )
        return item

    needed = {
        entry["_origin"]: (groups[id(entry)][0], entry)
        for entry in tree.values()
        if entry["kind"] == "file"
    }
    with tarfile.open(destination, "w", format=tarfile.PAX_FORMAT) as target:
        for name, entry in sorted(tree.items()):
            if entry["kind"] == "directory":
                item = header(name, entry)
                item.type = tarfile.DIRTYPE
                target.addfile(item)
        for layer_path in layers:
            with tarfile.open(layer_path, "r|*") as source:
                for ordinal, original in enumerate(source):
                    record = needed.pop((str(layer_path), ordinal), None)
                    if record is None:
                        continue
                    name, entry = record
                    item = header(name, entry)
                    item.size = entry["size"]
                    stream = source.extractfile(original)
                    if stream is None:
                        raise ValueError("missing live file body during offline merge")
                    with stream:
                        target.addfile(item, stream)
        for name, entry in sorted(tree.items()):
            if entry["kind"] == "symlink":
                item = header(name, entry)
                item.type = tarfile.SYMTYPE
                item.linkname = entry["target"]
                target.addfile(item)
            elif entry["kind"] == "file" and name != groups[id(entry)][0]:
                item = header(name, entry)
                item.type = tarfile.LNKTYPE
                item.linkname = groups[id(entry)][0]
                target.addfile(item)
    if needed:
        raise ValueError("source layer changed during offline merge")
    # This independently rereads all emitted bytes and verifies the merge before
    # handing data to the converter; it does not substitute a different oracle.
    changes = differences(
        inventory(layers, whiteouts=whiteouts), inventory([destination], whiteouts=whiteouts)
    )
    if changes:
        raise ValueError(f"offline merge changed OCI semantics: {changes[:3]}")


def scan_mount(root: Path) -> dict[str, dict]:
    tree = {}
    groups: dict[tuple[int, int], list[str]] = {}
    pending = [root]
    while pending:
        path = pending.pop()
        s = path.lstat()
        name = path.relative_to(root).as_posix()
        name = "" if name == "." else name
        kind = (
            "directory"
            if stat.S_ISDIR(s.st_mode)
            else "file"
            if stat.S_ISREG(s.st_mode)
            else "symlink"
            if stat.S_ISLNK(s.st_mode)
            else "unsupported"
        )
        entry = {
            "kind": kind,
            "mode": stat.S_IMODE(s.st_mode),
            "uid": s.st_uid,
            "gid": s.st_gid,
            "mtime_ns": s.st_mtime_ns,
            "xattrs": {},
        }
        for key in os.listxattr(path, follow_symlinks=False):
            entry["xattrs"][key] = base64.b64encode(
                os.getxattr(path, key, follow_symlinks=False)
            ).decode("ascii")
        if kind == "directory":
            pending.extend(sorted(path.iterdir(), reverse=True))
        elif kind == "file":
            entry.update(size=s.st_size, sha256=digest_file(path))
            groups.setdefault((s.st_dev, s.st_ino), []).append(name)
        elif kind == "symlink":
            entry["target"] = os.readlink(path)
        tree[name] = entry
        if len(tree) > MAX_ENTRIES:
            raise ValueError("mounted image exceeds entry limit")
    for names in groups.values():
        for name in names:
            tree[name]["hardlinks"] = sorted(names)
    return dict(sorted(tree.items()))


def scan_direct(reader: Path, bootstrap: Path, blobs: Path, output: Path, timeout: float) -> dict:
    """Read every inode and file body without applying host mount permissions."""
    result = output / "reader.json"
    run_tool(
        [str(reader), str(bootstrap), str(blobs), str(output / "reader-cache")], result, timeout
    )
    tree = json.loads(result.read_text())
    for entry in tree.values():
        entry["xattrs"] = {
            key: base64.b64encode(bytes(value)).decode("ascii")
            for key, value in entry["xattrs"].items()
        }
    return tree


def differences(expected: dict, actual: dict) -> list[dict]:
    result = []
    for path in sorted(expected.keys() | actual.keys()):
        if path not in expected or path not in actual:
            result.append(
                {
                    "path": path,
                    "field": "presence",
                    "expected": path in expected,
                    "actual": path in actual,
                }
            )
            continue
        fields = {"kind"} if expected[path].get("implicit") else expected[path].keys()
        for field in sorted(fields):
            if expected[path][field] != actual[path].get(field):
                result.append(
                    {
                        "path": path,
                        "field": field,
                        "expected": expected[path][field],
                        "actual": actual[path].get(field),
                    }
                )
    return result


def stop(process: subprocess.Popen) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    process.wait()


def run_tool(command: list[str], output: Path, timeout: float) -> None:
    with output.open("wb") as log:
        process = subprocess.Popen(
            command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True
        )
        try:
            if process.wait(timeout=timeout):
                raise RuntimeError(f"tool failed; see {output}")
        finally:
            stop(process)


@contextmanager
def mounted(nydusd: Path, bootstrap: Path, blobs: Path, output: Path, timeout: float):
    mountpoint = output / "mount"
    mountpoint.mkdir()
    cache = output / "cache"
    cache.mkdir()
    config = output / "nydusd.json"
    save(
        config,
        {
            "version": 2,
            "backend": {"type": "localfs", "localfs": {"dir": str(blobs)}},
            "cache": {
                "type": "blobcache",
                "validate": True,
                "filecache": {"work_dir": str(cache)},
                "prefetch": {"enable": False},
            },
            "rafs": {
                "mode": "direct",
                # v6 verifies data through cache.validate and inline chunk
                # digests authenticated by the pinned bootstrap's blob ToC.
                # rafs.validate is a different, v5-only filesystem check.
                "validate": False,
                "enable_xattr": True,
                "batch_size": 0,
                "prefetch": {"enable": False},
            },
        },
    )
    # The upstream daemon unconditionally requests allow_other. Restrict this
    # probe's mount to its owner instead of modifying /etc/fuse.conf. Preserve
    # the FUSE communication descriptor while executing the real mount helper.
    fusermount = shutil.which("fusermount3")
    if not fusermount:
        raise RuntimeError("fusermount3 is required")
    helpers = output / "helpers"
    helpers.mkdir(mode=0o700)
    helper = helpers / "fusermount3"
    helper.write_text(
        f"#!{sys.executable}\nimport os, sys\nargs = sys.argv[1:]\n"
        "if '-o' in args:\n"
        "    i = args.index('-o') + 1\n"
        "    args[i] = ','.join(x for x in args[i].split(',') "
        "if x not in ('allow_other', 'allow_root'))\n"
        f"os.execv({fusermount!r}, ['fusermount3', *args])\n"
    )
    helper.chmod(0o700)
    environment = dict(os.environ, PATH=str(helpers) + os.pathsep + os.environ.get("PATH", ""))
    with (output / "nydusd.log").open("wb") as log:
        process = subprocess.Popen(
            [
                str(nydusd),
                "fuse",
                "--config",
                str(config),
                "--bootstrap",
                str(bootstrap),
                "--mountpoint",
                str(mountpoint),
                "--fuse-threads",
                "2",
                "--rlimit-nofile",
                "0",
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            env=environment,
        )
        cleanup_errors = []
        try:
            deadline = time.monotonic() + timeout
            while not mountpoint.is_mount():
                if process.poll() is not None:
                    raise RuntimeError(
                        f"nydusd exited before mounting; see {output / 'nydusd.log'}"
                    )
                if time.monotonic() >= deadline:
                    raise TimeoutError("nydusd mount timed out")
                time.sleep(0.02)
            if not os.statvfs(mountpoint).f_flag & os.ST_RDONLY:
                raise RuntimeError("qualification requires a read-only FUSE mount")
            yield mountpoint
        finally:
            if mountpoint.is_mount():
                try:
                    run_tool(
                        [shutil.which("fusermount3") or "fusermount3", "-u", str(mountpoint)],
                        output / "unmount.log",
                        5,
                    )
                except Exception as error:
                    cleanup_errors.append(str(error))
            stop(process)
            if mountpoint.is_mount():
                try:
                    run_tool(
                        [shutil.which("fusermount3") or "fusermount3", "-uz", str(mountpoint)],
                        output / "unmount-fallback.log",
                        5,
                    )
                except Exception as error:
                    cleanup_errors.append(str(error))
            if mountpoint.is_mount():
                cleanup_errors.append("owned FUSE mount is still present")
            save(
                output / "cleanup.json",
                {
                    "pid": process.pid,
                    "returncode": process.returncode,
                    "unmounted": not mountpoint.is_mount(),
                    "errors": cleanup_errors,
                },
            )
            if cleanup_errors:
                raise RuntimeError("owned mount cleanup failed: " + "; ".join(cleanup_errors))


def probe(
    layout: Path,
    platform: str,
    image_tool: Path,
    daemon: Path,
    output: Path,
    timeout: float = 60,
    *,
    inventory_reader: Path | None = None,
    raw_layer_index: int | None = None,
) -> dict:
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be positive and finite")
    layout, image_tool, daemon, output = [p.resolve() for p in (layout, image_tool, daemon, output)]
    if inventory_reader is not None:
        inventory_reader = inventory_reader.resolve()
    if output == layout or layout in output.parents or output in layout.parents:
        raise ValueError("source layout and output must be disjoint")
    identity, layers = load_image(layout, platform)
    paths = [path for _, path in layers]
    if raw_layer_index is not None:
        if type(raw_layer_index) is not int or not 0 <= raw_layer_index < len(paths):
            raise ValueError("raw layer index is out of range")
        if inventory_reader is None:
            raise ValueError("raw layers require explicit direct-reader qualification")
        paths = [paths[raw_layer_index]]
    expected = inventory(paths, whiteouts=raw_layer_index is None)
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    blobs = output / "blobs"
    blobs.mkdir()
    report = {
        "passed": False,
        "source": identity,
        "raw_layer_index": raw_layer_index,
        "format": "rafs-v6",
        "tools": {str(p): digest_file(p) for p in (image_tool, daemon)},
        "differences": [],
        "error": None,
        "runtime_integrated": False,
        "runtime_qualified": False,
        "verification_mode": "direct-reader" if inventory_reader else "fuse-mount",
        "mount_checked": False,
        "verification": (
            "offline full-blob SHA-256 and complete OCI file/metadata comparison; "
            "native cache chunk validation with blob-toc"
        ),
        "runtime_gaps": [
            "runtime publication, trusted bootstrap pinning and shared cache are not integrated"
        ],
    }
    if inventory_reader:
        report["tools"][str(inventory_reader)] = digest_file(inventory_reader)
        report["verification"] = (
            "offline full-blob SHA-256 and complete OCI file/metadata comparison "
            "via direct RAFS reader; no FUSE permission bypass"
        )
    save(output / "expected.json", expected)
    try:
        run_tool([str(image_tool), "--version"], output / "tool-version.log", timeout)
        merged = output / "merged.tar"
        flattened_tar(paths, merged, whiteouts=raw_layer_index is None)
        parent = output / "image.bootstrap"
        command = [
            str(image_tool),
            "create",
            "--type",
            "tar-rafs",
            "--fs-version",
            "6",
            "--compressor",
            "lz4_block",
            "--digester",
            "sha256",
            "--features",
            "blob-toc",
            "--whiteout-spec",
            "oci",
            "--bootstrap",
            str(parent),
            "--blob-dir",
            str(blobs),
            "--output-json",
            str(output / "create.json"),
            str(merged),
        ]
        save(output / "create-command.json", command)
        run_tool(command, output / "create.log", timeout)
        run_tool(
            [
                str(image_tool),
                "check",
                str(parent),
                "--blob-dir",
                str(blobs),
                "--output-json",
                str(output / "check.json"),
            ],
            output / "check.log",
            timeout,
        )
        artifacts = {
            str(p.relative_to(output)): digest_file(p)
            for p in [parent, *sorted(blobs.iterdir())]
            if p.is_file()
        }
        for path in blobs.iterdir():
            if (
                path.is_symlink()
                or not path.is_file()
                or len(path.name) != 64
                or any(c not in "0123456789abcdef" for c in path.name)
            ):
                raise ValueError("RAFS data blob must be regular with a valid blob ID")
        # Nydus's blob-toc ID excludes some trailing metadata; it is not the
        # entire file's SHA-256. Bind the ID to the full artifact digest above,
        # retain native chunk/ToC checks, then rehash all artifacts after use.
        report["artifacts"] = artifacts
        if inventory_reader:
            actual = scan_direct(inventory_reader, parent, blobs, output, timeout)
        else:
            with mounted(daemon, parent, blobs, output, timeout) as root:
                actual = scan_mount(root)
            report["mount_checked"] = True
        save(output / "actual.json", actual)
        report["differences"] = differences(expected, actual)
        after_identity, after_layers = load_image(layout, platform)
        if after_identity != identity or after_layers != layers:
            raise ValueError("source OCI identity changed during qualification")
        for relative, expected_digest in artifacts.items():
            if digest_file(output / relative) != expected_digest:
                raise ValueError("RAFS artifact changed during qualification")
        report["passed"] = not report["differences"]
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    save(output / "report.json", report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oci-layout", required=True, type=Path)
    parser.add_argument("--platform", default="linux/amd64", choices=["linux/amd64", "linux/arm64"])
    parser.add_argument("--nydus-image", required=True, type=Path)
    parser.add_argument("--nydusd", required=True, type=Path)
    parser.add_argument(
        "--inventory-reader",
        type=Path,
        help="explicit direct RAFS qualification instead of a FUSE mount; preserves image ownership",
    )
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout", default=60.0, type=float)
    args = parser.parse_args()
    try:
        report = probe(
            args.oci_layout,
            args.platform,
            args.nydus_image,
            args.nydusd,
            args.output,
            args.timeout,
            inventory_reader=args.inventory_reader,
        )
    except (ValueError, OSError, KeyError, tarfile.TarError) as error:
        parser.exit(1, f"{type(error).__name__}: {error}\n")
    print(
        json.dumps(
            {
                "passed": report["passed"],
                "differences": len(report["differences"]),
                "error": report["error"],
                "report": str(args.output / "report.json"),
            }
        )
    )
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
