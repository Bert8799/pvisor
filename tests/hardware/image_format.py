from __future__ import annotations

import io
import os
import random
import tarfile
import unittest
from pathlib import Path

from tests.fixtures import workspace
from tests.image_fixtures.image_format import PROBE, FormatFixture, layer, layout


@unittest.skipUnless(
    os.environ.get("PVISOR_NYDUS_IMAGE") and os.environ.get("PVISOR_NYDUSD"),
    "requires explicit Nydus tools and Linux FUSE access",
)
class NativeImageFormatTests(FormatFixture, unittest.TestCase):
    def test_real_layer_conversion_and_cleanup(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("dir", tarfile.DIRTYPE, b""),
                    ("dir/old", tarfile.REGTYPE, b"old"),
                    ("tool", tarfile.REGTYPE, b"#!/bin/sh\necho READY\n"),
                    ("alias", tarfile.LNKTYPE, "tool"),
                    ("sym", tarfile.SYMTYPE, "tool"),
                    ("gone", tarfile.REGTYPE, b"old"),
                    ("empty", tarfile.REGTYPE, b""),
                    ("space name", tarfile.REGTYPE, b"space"),
                ],
            )
            upper = layer(
                root / "upper.tar",
                [
                    (".wh.gone", tarfile.REGTYPE, b""),
                    ("dir/.wh..wh..opq", tarfile.REGTYPE, b""),
                    ("dir/new", tarfile.REGTYPE, b"new"),
                ],
            )
            source = layout(root / "oci", [lower, upper])
            report = self.run_probe(root, source, "roundtrip")
            self.assertTrue(report["passed"], report)

    def test_fractional_oci_timestamps_are_preserved_exactly(self):
        with workspace() as (root, _):
            original = layer(
                root / "layer.tar",
                [(".", tarfile.DIRTYPE, b""), ("file", tarfile.REGTYPE, b"data")],
                ns=True,
            )
            source = layout(root / "oci", [original])
            report = self.run_probe(root, source, "fractional")
            self.assertIsNone(report["error"], report)
            self.assertTrue(report["passed"], report)
            self.assertEqual(report["differences"], [])

    def test_one_nanosecond_and_subsecond_boundaries_survive_native_conversion(self):
        with workspace() as (root, _):
            source = root / "times.tar"
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for name, timestamp in [
                    (".", "0.000000001"),
                    ("tiny", "0.000000001"),
                    ("last", "0.999999999"),
                    ("epoch", "0"),
                    ("integer", "42"),
                ]:
                    item = tarfile.TarInfo(name)
                    item.type = tarfile.DIRTYPE if name == "." else tarfile.REGTYPE
                    item.mode = 493 if name == "." else 420
                    item.pax_headers["mtime"] = timestamp
                    archive.addfile(item, None if name == "." else io.BytesIO())
            report = self.run_probe(root, layout(root / "oci", [source]), "times")
            self.assertTrue(report["passed"], report)


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
        )
    ),
    "requires explicit Nydus tools and the direct inventory reader",
)
class NativeDirectImageFormatTests(unittest.TestCase):
    def test_private_foreign_owned_nodes_retain_all_attributes_and_contents(self):
        with workspace() as (root, _):
            source = root / "private.tar"
            payload = random.Random(919).randbytes(1024)
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for name, kind, mode in [
                    (".", tarfile.DIRTYPE, 488),
                    ("private", tarfile.DIRTYPE, 448),
                    ("private/file", tarfile.REGTYPE, 256),
                    ("private/alias", tarfile.LNKTYPE, 256),
                    ("same-bytes", tarfile.REGTYPE, 256),
                    ("sym", tarfile.SYMTYPE, 511),
                ]:
                    item = tarfile.TarInfo(name)
                    item.type, item.mode = (kind, mode)
                    item.uid, item.gid = (1234, 2345)
                    item.pax_headers["mtime"] = "0.000000001"
                    if kind in (tarfile.REGTYPE, tarfile.LNKTYPE):
                        item.pax_headers["SCHILY.xattr.user.test"] = "value"
                    if kind == tarfile.REGTYPE:
                        item.size = len(payload)
                    if kind in (tarfile.LNKTYPE, tarfile.SYMTYPE):
                        item.linkname = "private/file"
                    archive.addfile(item, io.BytesIO(payload) if kind == tarfile.REGTYPE else None)
            output = root / "qualified"
            reader = Path(os.environ["PVISOR_RAFS_INVENTORY"])
            report = PROBE.probe(
                layout(root / "oci", [source]),
                "linux/amd64",
                Path(os.environ["PVISOR_NYDUS_IMAGE"]),
                Path(os.environ["PVISOR_NYDUSD"]),
                output,
                inventory_reader=reader,
            )
            self.assertTrue(report["passed"], report)
            self.assertFalse(report["mount_checked"])
            self.assertEqual(report["verification_mode"], "direct-reader")
            self.assertFalse((output / "mount").exists())
            expected = PROBE.inventory([source])
            blob = next((output / "blobs").iterdir())
            content = bytearray(blob.read_bytes())
            offset = content.find(payload)
            self.assertGreaterEqual(offset, 0)
            content[offset + 10] ^= 1
            blob.write_bytes(content)
            corrupted = root / "corrupted"
            corrupted.mkdir()
            actual = PROBE.scan_direct(
                reader, output / "image.bootstrap", output / "blobs", corrupted, 10
            )
            changed = PROBE.differences(expected, actual)
            self.assertTrue(any((change["field"] == "sha256" for change in changed)), changed)
