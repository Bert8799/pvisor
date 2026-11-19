from __future__ import annotations

import copy
import os
import tarfile
from contextlib import ExitStack
from pathlib import Path
from unittest import mock

from scripts.images import publish_nydus as PUB
from tests.image_fixtures.image_format import layer, layout

ROOT = Path(__file__).resolve().parents[2]

BLOB_ID = "1" * 64

TOC = "sha256:" + "2" * 64


class StorageFixture:
    """Fake converter only for storage faults; never used by native tests."""

    def __init__(self, root: Path, stack: ExitStack):
        self.root = root
        self.source = self.new_source("original", b"one")
        self.tools = []
        for name in ("converter", "daemon", "reader"):
            path = root / name
            path.write_bytes(name.encode())
            self.tools.append(path)
        self.report_failure = False
        self.description = {
            "id": BLOB_ID,
            "toc_sha256": TOC,
            "toc_size": 16,
            "inline_chunk_digests": True,
            "digester_sha256": True,
        }
        stack.enter_context(mock.patch.object(PUB.convert, "probe", side_effect=self.qualify))
        stack.enter_context(
            mock.patch.object(
                PUB, "describe_blobs", side_effect=lambda *_: [copy.deepcopy(self.description)]
            )
        )

    def new_source(self, name: str, content: bytes) -> Path:
        directory = self.root / name
        directory.mkdir()
        source = layer(directory / "layer.tar", [("file", tarfile.REGTYPE, content)])
        return layout(directory / "oci", [source])

    def qualify(
        self,
        source,
        platform,
        image,
        daemon,
        output,
        timeout,
        *,
        inventory_reader,
        raw_layer_index=None,
    ):
        identity, layers = PUB.convert.load_image(source, platform)
        output.mkdir()
        blobs = output / "blobs"
        blobs.mkdir()
        (blobs / BLOB_ID).write_bytes(b"fake storage payload long enough for a ToC")
        (output / "image.bootstrap").write_bytes(
            b"fake bootstrap:" + identity["manifest"]["digest"].encode()
        )
        paths = [path for _, path in layers]
        if raw_layer_index is not None:
            paths = [paths[raw_layer_index]]
        PUB.convert.save(
            output / "expected.json",
            PUB.convert.inventory(paths, whiteouts=raw_layer_index is None),
        )
        report = {
            "raw_layer_index": raw_layer_index,
            "source": identity,
            "passed": not self.report_failure,
            "error": None,
            "differences": [],
            "verification_mode": "direct-reader",
            "tools": {
                str(p): PUB.convert.digest_file(p) for p in (image, daemon, inventory_reader)
            },
            "artifacts": {
                name: PUB.convert.digest_file(output / name)
                for name in ("image.bootstrap", "blobs/" + BLOB_ID)
            },
        }
        PUB.convert.save(output / "report.json", report)
        return report

    def publish(self, name="work", source=None, store=None, **options):
        return PUB.publish(
            source or self.source,
            "linux/amd64",
            *self.tools,
            store or self.root / "store",
            self.root / name,
            **options,
        )


def killed_writer(root: Path):
    with ExitStack() as stack:
        fixture = StorageFixture(root, stack)
        with mock.patch.object(PUB.os, "rename", side_effect=lambda *_: os._exit(19)):
            fixture.publish()


def record_to_oci(value):
    return {"digest": value["sha256"], "size": value["size"]}
