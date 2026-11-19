from __future__ import annotations

import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import unittest
from unittest import mock

from tests.fixtures import workspace
from tests.image_fixtures.image_format import PROBE, layer, layout


class ImageFormatTests(unittest.TestCase):
    def test_unsupported_nodes_and_ambiguous_layer_paths_are_rejected(self):
        with workspace() as (root, _):
            for kind in (tarfile.CHRTYPE, tarfile.BLKTYPE, tarfile.FIFOTYPE):
                with self.subTest(kind=kind):
                    source = layer(root / "special.tar", [("special", kind, b"")])
                    with self.assertRaisesRegex(ValueError, "unsupported node"):
                        PROBE.inventory([source])
            source = layer(
                root / "duplicate.tar",
                [("file", tarfile.REGTYPE, b"old"), ("./file", tarfile.REGTYPE, b"new")],
            )
            with self.assertRaisesRegex(ValueError, "duplicate OCI layer path"):
                PROBE.inventory([source])
            for name in (".wh.", ".wh..", ".wh..."):
                with self.subTest(name=name):
                    source = layer(root / "whiteout.tar", [(name, tarfile.REGTYPE, b"")])
                    with self.assertRaisesRegex(ValueError, "unsafe whiteout target"):
                        PROBE.inventory([source])

    def test_conflicting_hardlink_xattrs_are_rejected(self):
        with workspace() as (root, _):
            source = root / "xattrs.tar"
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for name in ("file", "alias"):
                    item = tarfile.TarInfo(name)
                    item.pax_headers["SCHILY.xattr.user.test"] = name
                    if name == "alias":
                        item.type, item.linkname = (tarfile.LNKTYPE, "file")
                    archive.addfile(item, io.BytesIO() if name == "file" else None)
            with self.assertRaisesRegex(ValueError, "conflicting hardlink xattrs"):
                PROBE.inventory([source])

    def test_offline_merge_emits_decimal_pax_times_including_one_nanosecond(self):
        with workspace() as (root, _):
            source = root / "tiny.tar"
            times = ["0.000000001", "0.999999999", "42.1", "1700000000.123456789"]
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for index, timestamp in enumerate(times):
                    item = tarfile.TarInfo(str(index))
                    item.pax_headers["mtime"] = timestamp
                    archive.addfile(item, io.BytesIO())
            merged = root / "merged.tar"
            PROBE.flattened_tar([source], merged)
            with tarfile.open(merged) as archive:
                for index, timestamp in enumerate(times):
                    self.assertEqual(archive.getmember(str(index)).pax_headers["mtime"], timestamp)
            self.assertEqual(
                PROBE.differences(PROBE.inventory([source]), PROBE.inventory([merged])), []
            )

    def test_invalid_whiteout_does_not_silently_remove_source_content(self):
        with workspace() as (root, _):
            path = layer(root / "invalid.tar", [(".wh.file", tarfile.REGTYPE, b"not empty")])
            with self.assertRaisesRegex(ValueError, "empty regular"):
                PROBE.inventory([path])
            path = layer(root / "link.tar", [(".wh.file", tarfile.SYMTYPE, "/outside")])
            with self.assertRaisesRegex(ValueError, "empty regular"):
                PROBE.inventory([path])

    def test_valid_compressed_digest_does_not_replace_diff_id_validation(self):
        with workspace() as (root, _):
            source = layer(root / "layer.tar", [("file", tarfile.REGTYPE, b"data")])
            image = layout(root / "oci", [source])
            index = json.loads((image / "index.json").read_text())
            manifest = json.loads(PROBE.verified_blob(image, index["manifests"][0]).read_text())
            config = json.loads(PROBE.verified_blob(image, manifest["config"]).read_text())
            config["rootfs"]["diff_ids"] = ["sha256:" + "0" * 64]

            def replace(descriptor, value):
                data = json.dumps(value).encode()
                digest = hashlib.sha256(data).hexdigest()
                (image / "blobs/sha256" / digest).write_bytes(data)
                descriptor.update(digest="sha256:" + digest, size=len(data))

            replace(manifest["config"], config)
            replace(index["manifests"][0], manifest)
            (image / "index.json").write_text(json.dumps(index))
            with self.assertRaisesRegex(ValueError, "diff_id mismatch"):
                PROBE.load_image(image, "linux/amd64")

    def test_offline_merge_preserves_hardlinks_metadata_and_lower_only_whiteouts(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("dir", tarfile.DIRTYPE, b""),
                    ("dir/old", tarfile.REGTYPE, b"old"),
                    ("a", tarfile.REGTYPE, b"data"),
                    ("b", tarfile.LNKTYPE, "a"),
                ],
                ns=True,
            )
            upper = layer(
                root / "upper.tar",
                [
                    ("dir/new", tarfile.REGTYPE, b"new"),
                    ("dir/.wh..wh..opq", tarfile.REGTYPE, b""),
                    ("sym", tarfile.SYMTYPE, "a"),
                ],
            )
            destination = root / "merged.tar"
            PROBE.flattened_tar([lower, upper], destination)
            self.assertEqual(PROBE.inventory([lower, upper]), PROBE.inventory([destination]))
            with tarfile.open(destination) as archive:
                self.assertEqual(archive.extractfile("a").read(), b"data")
                self.assertEqual(archive.extractfile("dir/new").read(), b"new")
                self.assertTrue(archive.getmember("b").islnk())
                self.assertNotIn("dir/old", archive.getnames())

    def test_tool_timeout_terminates_and_reaps_its_process(self):
        with workspace() as (root, _):
            processes = []
            real_popen = subprocess.Popen

            def capture(*args, **kwargs):
                process = real_popen(*args, **kwargs)
                processes.append(process)
                return process

            with mock.patch.object(PROBE.subprocess, "Popen", side_effect=capture):
                with self.assertRaises(subprocess.TimeoutExpired):
                    PROBE.run_tool(
                        [sys.executable, "-c", "import time; time.sleep(60)"],
                        root / "timeout.log",
                        0.05,
                    )
            self.assertEqual(len(processes), 1)
            self.assertIsNotNone(processes[0].returncode)
            with self.assertRaises(ProcessLookupError):
                os.kill(processes[0].pid, 0)

    def test_whiteouts_remove_only_lower_entries_regardless_of_marker_order(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [
                    ("gone", tarfile.REGTYPE, b"old"),
                    ("dir", tarfile.DIRTYPE, b""),
                    ("dir/old", tarfile.REGTYPE, b"old"),
                ],
            )
            upper = layer(
                root / "upper.tar",
                [
                    ("dir/new", tarfile.REGTYPE, b"new"),
                    ("dir/.wh..wh..opq", tarfile.REGTYPE, b""),
                    ("gone", tarfile.REGTYPE, b"new"),
                    (".wh.gone", tarfile.REGTYPE, b""),
                ],
            )
            tree = PROBE.inventory([lower, upper])
            self.assertEqual(set(tree), {"", "dir", "dir/new", "gone"})
            self.assertEqual(tree["gone"]["sha256"], "sha256:" + hashlib.sha256(b"new").hexdigest())

    def test_replacement_splits_hardlinks_without_losing_the_old_alias(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [
                    ("a", tarfile.REGTYPE, b"old"),
                    ("b", tarfile.LNKTYPE, "a"),
                    ("same-bytes", tarfile.REGTYPE, b"old"),
                ],
            )
            upper = layer(root / "upper.tar", [("a", tarfile.REGTYPE, b"new")])
            before = PROBE.inventory([lower])
            self.assertEqual(before["a"]["hardlinks"], ["a", "b"])
            self.assertEqual(before["same-bytes"]["hardlinks"], ["same-bytes"])
            after = PROBE.inventory([lower, upper])
            self.assertEqual(after["a"]["hardlinks"], ["a"])
            self.assertEqual(after["b"]["hardlinks"], ["b"])
            self.assertNotEqual(after["a"]["sha256"], after["b"]["sha256"])

    def test_forward_hardlinks_resolve_and_cycles_are_rejected(self):
        with workspace() as (root, _):
            path = layer(
                root / "valid.tar",
                [("alias", tarfile.LNKTYPE, "file"), ("file", tarfile.REGTYPE, b"data")],
            )
            self.assertEqual(PROBE.inventory([path])["alias"]["hardlinks"], ["alias", "file"])
            path = layer(
                root / "cycle.tar", [("a", tarfile.LNKTYPE, "b"), ("b", tarfile.LNKTYPE, "a")]
            )
            with self.assertRaisesRegex(ValueError, "cyclic"):
                PROBE.inventory([path])

    def test_nanosecond_mtime_difference_is_not_normalized_away(self):
        with workspace() as (root, _):
            path = layer(root / "ns.tar", [("file", tarfile.REGTYPE, b"data")], ns=True)
            expected = PROBE.inventory([path])
            self.assertEqual(expected["file"]["mtime_ns"], 1700000000123456789)
            actual = {name: dict(entry) for name, entry in expected.items()}
            actual["file"]["mtime_ns"] = 1700000000000000000
            changes = PROBE.differences(expected, actual)
            self.assertEqual([(x["path"], x["field"]) for x in changes], [("file", "mtime_ns")])

    def test_symlink_targets_are_preserved_but_parent_traversal_is_rejected(self):
        with workspace() as (root, _):
            path = layer(root / "link.tar", [("sym", tarfile.SYMTYPE, "/outside")])
            self.assertEqual(PROBE.inventory([path])["sym"]["target"], "/outside")
            path = layer(
                root / "bad.tar",
                [("sym", tarfile.SYMTYPE, "/outside"), ("sym/file", tarfile.REGTYPE, b"bad")],
            )
            with self.assertRaisesRegex(ValueError, "non-directory"):
                PROBE.inventory([path])
            path = layer(root / "escape.tar", [("../escape", tarfile.REGTYPE, b"bad")])
            with self.assertRaisesRegex(ValueError, "unsafe"):
                PROBE.inventory([path])
            self.assertFalse((root.parent / "escape").exists())

    def test_directory_replacement_removes_descendants(self):
        with workspace() as (root, _):
            lower = layer(
                root / "lower.tar",
                [("dir", tarfile.DIRTYPE, b""), ("dir/old", tarfile.REGTYPE, b"old")],
            )
            upper = layer(root / "upper.tar", [("dir", tarfile.REGTYPE, b"file")])
            tree = PROBE.inventory([lower, upper])
            self.assertEqual(set(tree), {"", "dir"})
            self.assertEqual(tree["dir"]["kind"], "file")

    def test_verified_platform_and_corruption_rejection(self):
        with workspace() as (root, _):
            source = layer(root / "layer.tar", [("file", tarfile.REGTYPE, b"data")])
            image = layout(root / "oci", [source])
            identity, layers = PROBE.load_image(image, "linux/amd64")
            self.assertEqual(identity["platform"], "linux/amd64")
            self.assertEqual(len(layers), 1)
            with self.assertRaisesRegex(ValueError, "exactly one"):
                PROBE.load_image(image, "linux/arm64")
            path = layers[0][1]
            data = bytearray(path.read_bytes())
            data[0] ^= 1
            path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "digest mismatch"):
                PROBE.load_image(image, "linux/amd64")

    def test_attributes_xattrs_and_empty_files_are_checked(self):
        with workspace() as (root, _):
            path = root / "metadata.tar"
            with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as archive:
                item = tarfile.TarInfo("empty")
                item.mode, item.uid, item.gid = (2536, 2345, 3456)
                item.pax_headers["SCHILY.xattr.user.test"] = "value"
                archive.addfile(item, io.BytesIO())
            tree = PROBE.inventory([path])
            expected = tree["empty"]
            self.assertEqual(
                (expected["mode"], expected["uid"], expected["gid"]), (2536, 2345, 3456)
            )
            self.assertEqual(expected["xattrs"], {"user.test": "dmFsdWU="})
            self.assertEqual(expected["size"], 0)
            self.assertEqual(PROBE.differences(tree, tree), [])
            changed = {name: dict(entry) for name, entry in tree.items()}
            changed["empty"]["mode"] = 488
            changed["empty"]["uid"] = 0
            changed["empty"]["xattrs"] = {}
            self.assertEqual(
                {x["field"] for x in PROBE.differences(tree, changed)}, {"mode", "uid", "xattrs"}
            )
