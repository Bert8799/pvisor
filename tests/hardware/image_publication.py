from __future__ import annotations

import json
import os
import shutil
import tarfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_format import layer, layout


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
        )
    ),
    "requires explicit Nydus converter and inventory tools",
)
class NativeImagePublicationTests(unittest.TestCase):
    def test_real_bootstrap_without_inline_digests_is_rejected(self):
        with workspace() as (root, _):
            source = layout(
                root / "oci", [layer(root / "layer.tar", [("file", tarfile.REGTYPE, b"data")])]
            )
            tools = [
                Path(os.environ[name])
                for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
            ]
            run_tool = PUB.convert.run_tool

            def omit_digests(command, output, timeout):
                if command[1] == "create":
                    index = command.index("--features")
                    self.assertEqual(command[index + 1], "blob-toc")
                    command = command[:index] + command[index + 2 :]
                run_tool(command, output, timeout)

            with mock.patch.object(PUB.convert, "run_tool", side_effect=omit_digests):
                with self.assertRaisesRegex(ValueError, "mandatory"):
                    PUB.publish(source, "linux/amd64", *tools, root / "store", root / "work")
            report = json.loads((root / "work/conversion/report.json").read_text())
            self.assertTrue(report["passed"])
            self.assertFalse((root / "store").exists())

    def test_published_bundle_reads_after_source_and_work_are_removed(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("old", tarfile.REGTYPE, b"old"),
                    ("tool", tarfile.REGTYPE, b"data"),
                    ("alias", tarfile.LNKTYPE, "tool"),
                ],
                ns=True,
            )
            upper = layer(
                root / "upper.tar",
                [(".wh.old", tarfile.REGTYPE, b""), ("new", tarfile.REGTYPE, b"new")],
            )
            source = layout(root / "oci", [lower, upper])
            expected = PUB.convert.inventory([lower, upper])
            tools = [
                Path(os.environ[name])
                for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
            ]
            first = PUB.publish(source, "linux/amd64", *tools, root / "store", root / "one")
            second = PUB.publish(source, "linux/amd64", *tools, root / "store", root / "two")
            self.assertEqual(first, second)
            for path in (source, root / "one", root / "two"):
                shutil.rmtree(path)
            bundle, record = PUB.open_bundle(root / "store", first)
            output = root / "readback"
            output.mkdir()
            actual = PUB.convert.scan_direct(
                tools[2], bundle / "metadata/image.bootstrap", bundle / "blobs", output, 30
            )
            self.assertEqual(PUB.convert.differences(expected, actual), [])
            self.assertTrue(
                all(
                    (
                        value["toc_sha256"] != "sha256:" + "0" * 64
                        for value in record["blobs"].values()
                    )
                )
            )
            self.assertEqual(list((root / "store/staging").iterdir()), [])
