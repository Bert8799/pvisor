#!/usr/bin/env python3
"""Qualify and publish an offline metadata merge of a pinned RAFS environment."""

from __future__ import annotations

import argparse
import base64
import copy
import hashlib
import json
import math
import os
import posixpath
import subprocess
import tarfile
import tempfile
from collections.abc import Iterable
from pathlib import Path

if __package__:
    from . import compose_nydus as environments
    from . import publish_nydus as images
else:
    import compose_nydus as environments
    import publish_nydus as images

PREFIX = "pvisor-rafs-merged-v1:"
ALGORITHM = "oci-overlay-directory-inheritance-v1"


def revision(handle: str) -> str:
    if not isinstance(handle, str) or not handle.startswith(PREFIX):
        raise ValueError("expected an immutable merged-image handle")
    value = handle[len(PREFIX) :]
    if not images.HEX.fullmatch(value):
        raise ValueError("invalid merged-image revision")
    return value


def inventory_file(path: Path, descriptor: dict | None = None) -> dict:
    if descriptor is not None:
        images.consume(path, descriptor)
    with images.regular(path) as stream:
        if os.fstat(stream.fileno()).st_size > images.MAX_INVENTORY:
            raise ValueError("inventory exceeds size limit")
        raw = stream.read(images.MAX_INVENTORY + 1)
    if len(raw) > images.MAX_INVENTORY:
        raise ValueError("inventory exceeds size limit")
    tree = json.loads(raw)
    if (
        not isinstance(tree, dict)
        or len(tree) > images.convert.MAX_ENTRIES
        or not isinstance(tree.get(""), dict)
        or tree[""].get("kind") != "directory"
    ):
        raise ValueError("invalid inventory tree")
    for name, entry in tree.items():
        if not isinstance(name, str) or images.convert.image_path(name) != name:
            raise ValueError("noncanonical inventory path")
        if not isinstance(entry, dict):
            raise ValueError("invalid inventory entry")
        if entry.get("implicit"):
            images.keys(entry, ("kind", "implicit"))
            if entry["implicit"] is not True or entry["kind"] != "directory":
                raise ValueError("invalid implicit directory")
    return tree


def overlay_inventories(base: dict, uppers: Iterable[tuple[dict, dict]]) -> dict:
    """Independent OCI overlay model; uppers pair concrete bytes with raw declarations."""
    tree = copy.deepcopy(base)
    for name, entry in tree.items():
        if entry["kind"] == "file":
            entry["_object"] = (0, entry["hardlinks"][0])
    for ordinal, (actual, declared) in enumerate(uppers, 1):
        differences = images.convert.differences(declared, actual)
        if differences:
            raise ValueError(f"raw component inventory changed: {differences[:3]}")
        for name, entry in declared.items():
            parent, basename = posixpath.split(name)
            if basename.startswith(".wh."):
                if entry["kind"] != "file" or entry["size"] != 0 or basename[4:] in ("", ".", ".."):
                    raise ValueError("invalid whiteout declaration")
                if basename == ".wh..wh..opq":
                    images.convert.remove_tree(tree, parent, children_only=True)
                else:
                    images.convert.remove_tree(tree, posixpath.join(parent, basename[4:]))
        for name in sorted(actual, key=lambda name: (name.count("/") + bool(name), name)):
            if posixpath.basename(name).startswith(".wh."):
                continue
            entry = copy.deepcopy(actual[name])
            old = tree.get(name)
            if declared[name].get("implicit") and old is not None:
                if old["kind"] != "directory":
                    raise ValueError("implicit directory traverses a non-directory lower entry")
                continue
            parent = posixpath.dirname(name)
            if name and tree.get(parent, {}).get("kind") != "directory":
                raise ValueError("component traverses a non-directory parent")
            if old is not None and not (old["kind"] == entry["kind"] == "directory"):
                images.convert.remove_tree(tree, name)
            if entry["kind"] == "file":
                entry["_object"] = (ordinal, entry["hardlinks"][0])
            tree[name] = entry
            if len(tree) > images.convert.MAX_ENTRIES:
                raise ValueError("merged image exceeds node limit")
    groups = {}
    for name, entry in tree.items():
        if entry["kind"] == "file":
            groups.setdefault(entry["_object"], []).append(name)
    for entry in tree.values():
        if entry["kind"] == "file":
            entry["hardlinks"] = sorted(groups[entry.pop("_object")])
    return dict(sorted(tree.items()))


def directory_tar(tree: dict, path: Path) -> None:
    """Generate metadata only; no regular file data enters the correction layer."""
    with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as archive:
        for name, entry in sorted(tree.items()):
            if entry["kind"] != "directory":
                continue
            item = tarfile.TarInfo(name or ".")
            item.type, item.mode, item.uid, item.gid = (
                tarfile.DIRTYPE,
                entry["mode"],
                entry["uid"],
                entry["gid"],
            )
            item.mtime, nanoseconds = divmod(entry["mtime_ns"], 1_000_000_000)
            item.pax_headers["mtime"] = f"{item.mtime}.{nanoseconds:09d}"
            for key, value in entry["xattrs"].items():
                item.pax_headers["SCHILY.xattr." + key] = base64.b64decode(
                    value, validate=True
                ).decode("utf-8", "surrogateescape")
            archive.addfile(item)


def retain_environment(source: Path, destination: Path, handle: str) -> dict:
    record = environments.verify_environment(source, handle)
    destination.mkdir(mode=0o700)
    (destination / "components").mkdir(mode=0o700)
    os.link(source / "ENVIRONMENT.json", destination / "ENVIRONMENT.json", follow_symlinks=False)
    for name, component in environments.references(record):
        environments.link_component(
            source / "components" / name, destination / "components" / name, component
        )
    images.sync_directory(destination / "components")
    images.sync_directory(destination)
    return environments.verify_environment(destination, handle)


def object_descriptor(path: Path) -> dict:
    return {"sha256": images.convert.digest_file(path), "size": path.stat().st_size}


def verify_merged(bundle: Path, handle: str) -> dict:
    images.directory(bundle, create=False)
    for name in ("metadata", "blobs"):
        images.directory(bundle / name, create=False)
    raw = images.read_control(bundle / "MERGED.json")
    if images.sha256(raw) != "sha256:" + revision(handle):
        raise ValueError("merged-image identity mismatch")
    record = json.loads(raw)
    images.keys(
        record,
        (
            "schema",
            "algorithm",
            "environment",
            "platform",
            "bootstrap",
            "inventory",
            "config",
            "blobs",
            "tools",
        ),
    )
    if (
        record["schema"] != PREFIX[:-1]
        or record["algorithm"] != ALGORITHM
        or images.encoded(record) != raw
    ):
        raise ValueError("unsupported merged-image encoding")
    if {p.name for p in bundle.iterdir()} != {"MERGED.json", "metadata", "blobs", "environment"}:
        raise ValueError("unexpected merged-image objects")
    if {p.name for p in (bundle / "metadata").iterdir()} != {
        "image.bootstrap",
        "inventory.json",
        "source.config.json",
    }:
        raise ValueError("incomplete merged metadata")
    objects = (
        ("bootstrap", "image.bootstrap"),
        ("inventory", "inventory.json"),
        ("config", "source.config.json"),
    )
    for field, _name in objects:
        images.descriptor(record[field])
    if (
        record["inventory"]["size"] > images.MAX_INVENTORY
        or record["config"]["size"] > images.MAX_CONTROL
    ):
        raise ValueError("merged control data exceeds size limit")
    for field, name in objects:
        images.consume(bundle / "metadata" / name, record[field])
    source = environments.verify_environment(bundle / "environment", record["environment"])
    if source["platform"] != record["platform"]:
        raise ValueError("merged platform mismatch")
    base = images.verify_bundle(bundle / "environment/components/base", source["base"])
    if base["source"]["config"] != record["config"]:
        raise ValueError("merged launch config differs from the base")
    if not isinstance(record["blobs"], dict) or len(record["blobs"]) > 4096:
        raise ValueError("invalid merged blob table")
    if {p.name for p in (bundle / "blobs").iterdir()} != record["blobs"].keys():
        raise ValueError("incomplete merged blob set")
    available = {}
    for name, component in environments.references(source):
        details = images.verify_bundle(bundle / "environment/components" / name, component)
        for blob_id, descriptor in details["blobs"].items():
            if blob_id in available and available[blob_id] != descriptor:
                raise ValueError("native blob ID collision across components")
            available[blob_id] = descriptor
    for blob_id, descriptor in record["blobs"].items():
        if available.get(blob_id) != descriptor:
            raise ValueError("merged data does not match its pinned source")
        images.consume(bundle / "blobs" / blob_id, descriptor["artifact"])
    images.keys(record["tools"], ("converter", "reader"))
    for digest in record["tools"].values():
        images.check_digest(digest)
    return record


def open_merged(store: Path, handle: str) -> tuple[Path, dict]:
    value = revision(handle)
    root = images.store_root(store, create=False)
    images.directory(root / "merged", create=False)
    bundle = root / "merged" / value
    return bundle, verify_merged(bundle, handle)


def merge(
    store: Path, environment: str, converter: Path, reader: Path, work: Path, timeout: float = 60
) -> str:
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be positive and finite")
    root = images.store_root(store, create=False)
    source, _record = environments.open_environment(root, environment)
    work = work.resolve()
    canonical_store = root.resolve()
    if (
        canonical_store == work
        or canonical_store in work.parents
        or work in canonical_store.parents
    ):
        raise ValueError("work directory and store must be disjoint")
    converter, reader = converter.resolve(), reader.resolve()
    tool_digests = {
        "converter": images.convert.digest_file(converter),
        "reader": images.convert.digest_file(reader),
    }
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    images.directory(root / "merged", create=True)
    with tempfile.TemporaryDirectory(prefix="merge-", dir=root / "staging") as temporary:
        bundle = Path(temporary) / "bundle"
        bundle.mkdir(mode=0o700)
        for name in ("metadata", "blobs"):
            (bundle / name).mkdir(mode=0o700)
        recipe = retain_environment(source, bundle / "environment", environment)
        bootstraps, available = [], {}

        def component_inventories():
            for name, component in environments.references(recipe):
                path = bundle / "environment/components" / name
                details = images.verify_bundle(path, component)
                if len(details["blobs"]) > 1:
                    raise ValueError(
                        "this merge profile accepts at most one data blob per component"
                    )
                for blob_id, descriptor in details["blobs"].items():
                    if blob_id in available and available[blob_id] != descriptor:
                        raise ValueError("native blob ID collision across components")
                    if blob_id not in available:
                        os.link(
                            path / "blobs" / blob_id,
                            bundle / "blobs" / blob_id,
                            follow_symlinks=False,
                        )
                        available[blob_id] = descriptor
                bootstraps.append(path / "metadata/image.bootstrap")
                output = work / ("input-" + name)
                output.mkdir()
                actual = images.convert.scan_direct(
                    reader, bootstraps[-1], path / "blobs", output, timeout
                )
                declared = None
                if name != "base":
                    qualification = details["qualification"]
                    declared = inventory_file(
                        path / "metadata/layer.inventory.json",
                        {
                            "sha256": qualification["inventory_sha256"],
                            "size": qualification["inventory_size"],
                        },
                    )
                yield actual, declared

        inventories = component_inventories()
        expected = overlay_inventories(next(inventories)[0], inventories)
        base = images.verify_bundle(bundle / "environment/components/base", recipe["base"])
        config_descriptor = base["source"]["config"]
        images.convert.save(work / "expected.json", expected)
        correction = work / "directories.tar"
        directory_tar(expected, correction)
        correction_blobs = work / "directory-blobs"
        correction_blobs.mkdir()
        correction_bootstrap = work / "directories.bootstrap"
        images.convert.run_tool(
            [
                str(converter),
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
                str(correction_bootstrap),
                "--blob-dir",
                str(correction_blobs),
                str(correction),
            ],
            work / "directory-build.log",
            timeout,
        )
        if list(correction_blobs.iterdir()):
            raise ValueError("directory correction unexpectedly generated data blobs")
        config = work / "config.json"
        images.convert.save(
            config,
            {
                "version": 2,
                "backend": {"type": "localfs", "localfs": {"dir": str(bundle / "blobs")}},
                "cache": {
                    "type": "blobcache",
                    "validate": True,
                    "filecache": {"work_dir": str(work / "cache")},
                },
                "rafs": {"mode": "direct", "validate": False, "enable_xattr": True},
            },
        )
        bootstrap = work / "image.bootstrap"
        command = [
            str(converter),
            "merge",
            "--bootstrap",
            str(bootstrap),
            "--config",
            str(config),
            *map(str, bootstraps),
            str(correction_bootstrap),
        ]
        images.convert.save(work / "merge-command.json", command)
        images.convert.run_tool(command, work / "merge.log", timeout)
        bootstrap_descriptor = object_descriptor(bootstrap)
        output = work / "readback"
        output.mkdir()
        actual = images.convert.scan_direct(reader, bootstrap, bundle / "blobs", output, timeout)
        differences = images.convert.differences(expected, actual)
        images.convert.save(work / "differences.json", differences)
        if differences:
            raise ValueError(f"merged filesystem differs from OCI overlay model: {differences[:3]}")
        descriptions = work / "blob-descriptions.json"
        images.convert.run_tool(
            [
                str(reader),
                "--describe",
                str(bootstrap),
                str(bundle / "blobs"),
                str(work / "description-cache"),
            ],
            descriptions,
            timeout,
        )
        used = {}
        blob_descriptions = json.loads(images.read_control(descriptions))
        if not isinstance(blob_descriptions, list) or len(blob_descriptions) > 4096:
            raise ValueError("invalid merged blob descriptions")
        for info in blob_descriptions:
            images.keys(
                info, ("id", "toc_sha256", "toc_size", "inline_chunk_digests", "digester_sha256")
            )
            original = available.get(info["id"])
            if (
                not original
                or info["inline_chunk_digests"] is not True
                or info["digester_sha256"] is not True
                or original["toc_sha256"] != info["toc_sha256"]
                or original["toc_size"] != info["toc_size"]
                or info["id"] in used
            ):
                raise ValueError("merge changed authenticated blob identity or features")
            used[info["id"]] = original
        for blob_id, descriptor in available.items():
            images.consume(bundle / "blobs" / blob_id, descriptor["artifact"])
            if blob_id not in used:
                (bundle / "blobs" / blob_id).unlink()
        if tool_digests != {
            "converter": images.convert.digest_file(converter),
            "reader": images.convert.digest_file(reader),
        }:
            raise ValueError("merge tool changed during qualification")
        record = {
            "schema": PREFIX[:-1],
            "algorithm": ALGORITHM,
            "environment": environment,
            "platform": recipe["platform"],
            "bootstrap": bootstrap_descriptor,
            "inventory": object_descriptor(work / "expected.json"),
            "config": config_descriptor,
            "blobs": used,
            "tools": tool_digests,
        }
        if record["inventory"]["size"] > images.MAX_INVENTORY:
            raise ValueError("merged inventory exceeds size limit")
        for path, destination, descriptor in [
            (bootstrap, "image.bootstrap", record["bootstrap"]),
            (work / "expected.json", "inventory.json", record["inventory"]),
            (
                bundle / "environment/components/base/metadata/source.config.json",
                "source.config.json",
                record["config"],
            ),
        ]:
            images.copy_verified(path, bundle / "metadata" / destination, descriptor)
        raw = images.encoded(record)
        if len(raw) > images.MAX_CONTROL:
            raise ValueError("merged commit exceeds size limit")
        handle = PREFIX + hashlib.sha256(raw).hexdigest()
        with (bundle / "MERGED.json").open("xb") as stream:
            stream.write(raw)
            stream.flush()
            os.fchmod(stream.fileno(), 0o444)
            os.fsync(stream.fileno())
        for path in (bundle / "metadata", bundle / "blobs"):
            images.sync_directory(path)
        images.install_revision(
            root, bundle, root / "merged" / revision(handle), handle, verify_merged
        )
    return handle


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    publication = commands.add_parser("merge")
    publication.add_argument("--environment", required=True)
    publication.add_argument("--nydus-image", type=Path, required=True)
    publication.add_argument("--inventory-reader", type=Path, required=True)
    publication.add_argument("--work-dir", type=Path, required=True)
    publication.add_argument("--timeout", type=float, default=60)
    inspection = commands.add_parser("inspect")
    inspection.add_argument("--handle", required=True)
    for command in (publication, inspection):
        command.add_argument("--store", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "merge":
            print(
                json.dumps(
                    {
                        "handle": merge(
                            args.store,
                            args.environment,
                            args.nydus_image,
                            args.inventory_reader,
                            args.work_dir,
                            args.timeout,
                        )
                    }
                )
            )
        else:
            bundle, record = open_merged(args.store, args.handle)
            print(json.dumps({"handle": args.handle, "bundle": str(bundle), "commit": record}))
    except (
        ValueError,
        OSError,
        RuntimeError,
        KeyError,
        TypeError,
        subprocess.TimeoutExpired,
    ) as error:
        parser.exit(1, f"{type(error).__name__}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
