#!/usr/bin/env python3
"""Atomically stage authenticated metadata, without payloads, for a RAFS node."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import tempfile
from pathlib import Path

if __package__:
    from . import merge_nydus as merged
    from . import publish_nydus as images
else:
    import merge_nydus as merged
    import publish_nydus as images


def verify(bundle: Path, handle: str) -> dict:
    """Verify the staged subset; full source qualification occurs before staging."""
    images.directory(bundle, create=False)
    for name in ("metadata", "blobs"):
        images.directory(bundle / name, create=False)
    raw = images.read_control(bundle / "MERGED.json")
    if images.sha256(raw) != "sha256:" + merged.revision(handle):
        raise ValueError("node metadata identity mismatch")
    record = json.loads(raw)
    if images.encoded(record) != raw or record["schema"] != merged.PREFIX[:-1]:
        raise ValueError("unsupported node metadata")
    if {p.name for p in bundle.iterdir()} != {"MERGED.json", "metadata", "blobs"}:
        raise ValueError("unexpected node objects")
    if list((bundle / "blobs").iterdir()):
        raise ValueError("node metadata must not contain local payloads")
    if {p.name for p in (bundle / "metadata").iterdir()} != {
        "image.bootstrap",
        "source.config.json",
    }:
        raise ValueError("unexpected node metadata objects")
    for key, name in (("bootstrap", "image.bootstrap"), ("config", "source.config.json")):
        images.consume(bundle / "metadata" / name, record[key])
    return record


def stage(source: Path, handle: str, destination: Path) -> str:
    """Full offline audit, then copy and atomically commit only runtime metadata.

    Destination parents must exist. Existing matching revisions are verified and
    reused; inconsistent ones fail. A post-rename sync error retains the fixed
    handle in PublicationUncertain, as in ordinary publication. This does not
    upload blobs: serve the qualified source blob set separately at the origin.
    """
    value = merged.revision(handle)
    source = images.store_root(source, create=False)
    destination = Path(os.path.abspath(destination))
    a, b = source.resolve(), destination.resolve()
    if a == b or a in b.parents or b in a.parents:
        raise ValueError("source and node stores must be disjoint")
    bundle = source / "merged" / value
    with images.regular(bundle / "MERGED.json") as lease:
        fcntl.flock(lease, fcntl.LOCK_SH | fcntl.LOCK_NB)
        record = merged.verify_merged(bundle, handle)
        raw = images.read_control(bundle / "MERGED.json")
        root = images.store_root(destination, create=True)
        images.directory(root / "merged", create=True)
        with tempfile.TemporaryDirectory(prefix="node-", dir=root / "staging") as temporary:
            staged = Path(temporary) / "bundle"
            for path in (staged, staged / "metadata", staged / "blobs"):
                path.mkdir(mode=0o700)
            for key, name in (("bootstrap", "image.bootstrap"), ("config", "source.config.json")):
                images.copy_verified(
                    bundle / "metadata" / name, staged / "metadata" / name, record[key]
                )
            images.copy_verified(
                bundle / "MERGED.json",
                staged / "MERGED.json",
                {"sha256": "sha256:" + hashlib.sha256(raw).hexdigest(), "size": len(raw)},
            )
            for path in (staged / "metadata", staged / "blobs"):
                images.sync_directory(path)
            images.install_revision(root, staged, root / "merged" / value, handle, verify)
    return handle


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-store", type=Path, required=True)
    parser.add_argument("--handle", required=True)
    parser.add_argument("--node-store", type=Path, required=True)
    args = parser.parse_args()
    print(stage(args.source_store, args.handle, args.node_store))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
