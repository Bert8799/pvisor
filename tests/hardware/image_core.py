from __future__ import annotations

import io
import os
import random
import tarfile
import unittest
from pathlib import Path

from scripts.images import compose_nydus as ENV
from scripts.images import merge_nydus as MERGE
from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_core import CoreFixture
from tests.image_fixtures.image_format import layout
from tests.image_fixtures.image_merge import D, F, H, S


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in (
                "PVISOR_NYDUS_IMAGE",
                "PVISOR_NYDUSD",
                "PVISOR_RAFS_INVENTORY",
                "PVISOR_RAFS_READER",
                "PVISOR_RAFS_CORE_PROBE",
            )
        )
    ),
    "requires patched Nydus and the compiled Rust Core probe",
)
class NativeImageCoreTests(CoreFixture, unittest.TestCase):
    def test_host_representable_symlink_survives_owned_export(self):
        with workspace() as (root, _):
            source = root / "layer.tar"
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for name, kind, data in [(".", D, b""), ("a", F, b"payload"), ("link", S, "a")]:
                    item = tarfile.TarInfo(name)
                    item.uid, item.gid, item.type = (os.getuid(), os.getgid(), kind)
                    item.mode = 511 if kind == S else 493 if kind == D else 420
                    item.pax_headers["mtime"] = "1700000000.123456789"
                    if kind == F:
                        item.size = len(data)
                    if kind == S:
                        item.linkname = data
                    archive.addfile(item, io.BytesIO(data) if kind == F else None)
            tools = [
                Path(os.environ[name]).resolve()
                for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
            ]
            store = root / "store"
            base = PUB.publish(
                layout(root / "oci", [source]), "linux/amd64", *tools, store, root / "publish"
            )
            env = ENV.compose(store, base, [])
            handle = MERGE.merge(store, env, tools[0], tools[2], root / "merge")
            inputs = (
                store,
                handle,
                Path(os.environ["PVISOR_RAFS_READER"]).resolve(),
                Path(os.environ["PVISOR_RAFS_CORE_PROBE"]).resolve(),
            )
            result = self.execute(root, inputs)
            link = Path(result["owned_export"]) / "link"
            self.assertTrue(link.is_symlink())
            self.assertEqual(os.readlink(link), "a")
            self.assertEqual(link.lstat().st_uid, os.getuid())

    def test_copy_up_apply_conflicts_and_self_contained_export_use_real_runtime(self):
        payload = random.Random(772).randbytes(1024 * 1024 + 97)
        with workspace() as (root, _):
            inputs = self.prepare(
                root,
                [
                    [
                        (".", D, b""),
                        ("a", F, b"old"),
                        ("old-alias", H, "a"),
                        ("nested", D, b""),
                        ("nested/unvisited", F, b"untouched"),
                    ],
                    [("a", F, payload), ("new-alias", H, "a")],
                ],
            )
            store, handle, _, _ = inputs
            bundle, record = MERGE.open_merged(store, handle)
            before = {
                name: PUB.convert.digest_file(bundle / "blobs" / name) for name in record["blobs"]
            }
            result = self.execute(root, inputs, alias="new-alias")
            for name in (
                "same_process_core",
                "service_read",
                "copy_up",
                "stage_apply",
                "logical_image_export",
                "conflict_rejected",
                "reader_reaped",
            ):
                self.assertIs(result[name], True)
            self.assertEqual(result["shared_source_views"], 2)
            exported = Path(result["owned_export"])
            self.assertEqual((exported / "a").read_bytes(), payload)
            self.assertEqual((exported / "a").stat().st_ino, (exported / "new-alias").stat().st_ino)
            self.assertEqual((exported / "nested/unvisited").read_bytes(), b"untouched")
            self.assertEqual((exported / "old-alias").read_bytes(), b"old")
            self.assertEqual(os.getxattr(exported / "a", "user.test"), b"inherited")
            after = {
                name: PUB.convert.digest_file(bundle / "blobs" / name) for name in record["blobs"]
            }
            self.assertEqual(before, after)
            self.assertEqual(list((root / "probe/readers").iterdir()), [])

    def test_corrupt_data_stops_core_and_reaps_the_reader(self):
        payload = random.Random(773).randbytes(2 * 1024 * 1024)
        with workspace() as (root, _):
            inputs = self.prepare(root, [[(".", D, b""), ("a", F, payload)]])
            store, handle, _, _ = inputs
            bundle, record = MERGE.open_merged(store, handle)
            blob = bundle / "blobs" / next(iter(record["blobs"]))
            data = bytearray(blob.read_bytes())
            offset = data.find(payload[:64])
            self.assertGreaterEqual(offset, 0)
            data[offset + 16] ^= 1
            blob.unlink()
            blob.write_bytes(data)
            with self.assertRaises(RuntimeError):
                self.execute(root, inputs)
            self.assertFalse((root / "probe/export").exists())
            self.assertEqual(list((root / "probe/readers").iterdir()), [])

    def test_foreign_symlink_preserves_export_apply_conflicts_and_new_process_reopen(self):
        with workspace() as (root, _):
            inputs = self.prepare(root, [[(".", D, b""), ("a", F, b"data"), ("link", S, "a")]])
            result = self.execute(root, inputs, "link", "--inspect-symlink", "readonly")
            for name in (
                "readonly_symlink",
                "copy_up",
                "rename",
                "reopen_without_source",
                "reopen_in_new_process",
                "owned_export",
                "base_roundtrip",
                "stage_apply",
                "logical_image_export",
                "conflict_rejected",
                "reader_reaped",
            ):
                self.assertIs(result[name], True)
            self.assertEqual((result["uid"], result["gid"], result["mode"]), (1234, 1235, 420))
            self.assertEqual(result["target"], list(b"a"))
            self.assertEqual(result["xattrs"], {"user.test": list(b"inherited")})
            self.assertEqual(list((root / "readonly/readers").iterdir()), [])
            result = self.execute(root, inputs)
            self.assertIs(result["stage_apply"], True)
            self.assertIs(result["conflict_rejected"], True)
            self.assertTrue((root / "probe/export/link").is_symlink())
            self.assertEqual(list((root / "probe/readers").iterdir()), [])
