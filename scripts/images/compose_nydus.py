#!/usr/bin/env python3
"""Publish immutable ordered environment recipes with retained RAFS components."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import tempfile
from pathlib import Path

if __package__:
    from . import publish_nydus as images
else:
    import publish_nydus as images

PREFIX = "pvisor-rafs-env-v1:"
NAME = re.compile(r"[a-z][a-z0-9_-]{0,63}\Z")
MAX_LAYERS = 64


def revision(handle: str) -> str:
    if not isinstance(handle, str) or not handle.startswith(PREFIX):
        raise ValueError("expected an immutable environment handle")
    value = handle[len(PREFIX) :]
    if not images.HEX.fullmatch(value):
        raise ValueError("invalid environment revision")
    return value


def validate(record: dict) -> None:
    images.keys(record, ("schema", "platform", "base", "layers"))
    if record["schema"] != PREFIX[:-1] or record["platform"] not in ("linux/amd64", "linux/arm64"):
        raise ValueError("unsupported environment schema or platform")
    base = record["base"]
    images.handle_revision(base)
    if not base.startswith(images.PREFIX):
        raise ValueError("base must be a complete image, not an incremental layer")
    if not isinstance(record["layers"], list) or len(record["layers"]) > MAX_LAYERS:
        raise ValueError("invalid layer count")
    names, handles = set(), set()
    workspace = False
    for layer in record["layers"]:
        images.keys(layer, ("role", "name", "handle"))
        role, name, handle = layer["role"], layer["name"], layer["handle"]
        if (
            role not in ("toolchain", "workspace")
            or not isinstance(name, str)
            or not NAME.fullmatch(name)
        ):
            raise ValueError("invalid component role or name")
        images.handle_revision(handle)
        if not handle.startswith(images.LAYER_PREFIX):
            raise ValueError("overlays must be raw incremental layers, not complete images")
        if (role, name) in names or handle in handles or role == "workspace" and workspace:
            raise ValueError("duplicate component or workspace")
        names.add((role, name))
        handles.add(handle)
        workspace |= role == "workspace"


def references(record: dict):
    yield "base", record["base"]
    for index, layer in enumerate(record["layers"]):
        yield f"{index:04d}", layer["handle"]


def verify_environment(bundle: Path, handle: str) -> dict:
    images.directory(bundle, create=False)
    images.directory(bundle / "components", create=False)
    raw = images.read_control(bundle / "ENVIRONMENT.json")
    if images.sha256(raw) != "sha256:" + revision(handle):
        raise ValueError("environment identity mismatch")
    record = json.loads(raw)
    validate(record)
    if images.encoded(record) != raw:
        raise ValueError("non-canonical environment record")
    if {p.name for p in bundle.iterdir()} != {"ENVIRONMENT.json", "components"}:
        raise ValueError("unexpected environment objects")
    refs = list(references(record))
    if {p.name for p in (bundle / "components").iterdir()} != {name for name, _ in refs}:
        raise ValueError("incomplete environment components")
    for name, component in refs:
        details = images.verify_bundle(bundle / "components" / name, component)
        if details["source"]["platform"] != record["platform"]:
            raise ValueError("environment component platform mismatch")
    return record


def open_environment(store: Path, handle: str) -> tuple[Path, dict]:
    value = revision(handle)
    root = images.store_root(store, create=False)
    images.directory(root / "environments", create=False)
    bundle = root / "environments" / value
    return bundle, verify_environment(bundle, handle)


def link_component(source: Path, destination: Path, handle: str) -> None:
    images.verify_bundle(source, handle)
    destination.mkdir(mode=0o700)
    os.link(source / "COMMIT.json", destination / "COMMIT.json", follow_symlinks=False)
    for name in ("metadata", "blobs"):
        target = destination / name
        target.mkdir(mode=0o700)
        for path in (source / name).iterdir():
            os.link(path, target / path.name, follow_symlinks=False)
        images.sync_directory(target)
    images.sync_directory(destination)


def compose(store: Path, base: str, layers: list[dict]) -> str:
    """Pin component identity and order without merging or copying their data.

    Layers are lower-to-upper OCI changesets. Base launch defaults remain the
    launch contract; overlay config files do not silently change it. Retained
    component hardlinks survive removal of the original revision directories.
    This publishes a recipe, not a merged filesystem or an active VM lease.
    """
    root = images.store_root(store, create=False)
    base_bundle, base_record = images.open_bundle(root, base)
    record = {
        "schema": PREFIX[:-1],
        "platform": base_record["source"]["platform"],
        "base": base,
        "layers": layers,
    }
    validate(record)
    sources = {"base": base_bundle}
    for name, handle in list(references(record))[1:]:
        source, details = images.open_bundle(root, handle)
        if details["source"]["platform"] != record["platform"]:
            raise ValueError("environment component platform mismatch")
        sources[name] = source
    raw = images.encoded(record)
    if len(raw) > images.MAX_CONTROL:
        raise ValueError("environment record exceeds size limit")
    handle = PREFIX + hashlib.sha256(raw).hexdigest()
    images.directory(root / "environments", create=True)
    with tempfile.TemporaryDirectory(prefix="environment-", dir=root / "staging") as temporary:
        bundle = Path(temporary) / "bundle"
        bundle.mkdir(mode=0o700)
        (bundle / "components").mkdir(mode=0o700)
        for name, component in references(record):
            link_component(sources[name], bundle / "components" / name, component)
        with (bundle / "ENVIRONMENT.json").open("xb") as stream:
            stream.write(raw)
            stream.flush()
            os.fchmod(stream.fileno(), 0o444)
            os.fsync(stream.fileno())
        images.sync_directory(bundle / "components")
        images.install_revision(
            root, bundle, root / "environments" / revision(handle), handle, verify_environment
        )
    return handle


def parse_layer(value: str) -> dict:
    label, separator, handle = value.partition("=")
    role, colon, name = label.partition(":")
    if not separator or not colon:
        raise argparse.ArgumentTypeError("expected ROLE:NAME=IMMUTABLE_LAYER_HANDLE")
    return {"role": role, "name": name, "handle": handle}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    publication = commands.add_parser("publish")
    publication.add_argument("--base", required=True)
    publication.add_argument("--layer", action="append", type=parse_layer, default=[])
    inspection = commands.add_parser("inspect")
    inspection.add_argument("--handle", required=True)
    for command in (publication, inspection):
        command.add_argument("--store", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "publish":
            print(json.dumps({"handle": compose(args.store, args.base, args.layer)}))
        else:
            bundle, record = open_environment(args.store, args.handle)
            print(json.dumps({"handle": args.handle, "bundle": str(bundle), "environment": record}))
    except (ValueError, OSError, RuntimeError, KeyError, TypeError) as error:
        parser.exit(1, f"{type(error).__name__}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
