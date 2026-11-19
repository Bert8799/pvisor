from __future__ import annotations

import copy
import tarfile
import unittest

from scripts.images import compose_nydus as ENV
from scripts.images import merge_nydus as MERGE
from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_merge import D, F, S, cases, changeset
from tests.image_fixtures.image_publication import StorageFixture


class ImageMergeModelTests(unittest.TestCase):
    def test_work_and_store_remain_disjoint_through_a_symlinked_parent(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            base = fixture.publish()
            store = root / "store"
            environment = ENV.compose(store, base, [])
            alias = root / "alias"
            alias.symlink_to(root, target_is_directory=True)
            for work in (store, store / "staging/work", root):
                with self.subTest(work=work), self.assertRaisesRegex(ValueError, "disjoint"):
                    MERGE.merge(
                        alias / "store", environment, fixture.tools[0], fixture.tools[2], work
                    )
            self.assertEqual(list((store / "staging").iterdir()), [])

    def test_overlay_matches_original_oci_for_all_cases(self):
        with workspace() as (root, _):
            for index, (lower, upper) in enumerate(cases()):
                with self.subTest(case=index):
                    base = changeset(root / f"base-{index}.tar", lower)
                    raw = changeset(root / f"upper-{index}.tar", upper, mode=456, uid=4321)
                    declared = PUB.convert.inventory([raw], whiteouts=False)
                    concrete = copy.deepcopy(declared)
                    for entry in concrete.values():
                        if entry.pop("implicit", False):
                            entry.update(mode=493, uid=0, gid=0, mtime_ns=0, xattrs={})
                    result = MERGE.overlay_inventories(
                        PUB.convert.inventory([base]), [(concrete, declared)]
                    )
                    self.assertEqual(
                        PUB.convert.differences(PUB.convert.inventory([base, raw]), result), []
                    )

    def test_implicit_parent_cannot_traverse_a_lower_symlink_or_file(self):
        with workspace() as (root, _):
            raw = changeset(root / "upper.tar", [("parent/child", F, b"data")])
            declared = PUB.convert.inventory([raw], whiteouts=False)
            concrete = copy.deepcopy(declared)
            for entry in concrete.values():
                if entry.pop("implicit", False):
                    entry.update(mode=493, uid=0, gid=0, mtime_ns=0, xattrs={})
            for kind, data in [(S, "/outside"), (F, b"file")]:
                base = changeset(root / "base.tar", [(".", D, b""), ("parent", kind, data)])
                with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "non-directory"):
                    MERGE.overlay_inventories(PUB.convert.inventory([base]), [(concrete, declared)])

    def test_changed_raw_content_or_metadata_is_rejected(self):
        with workspace() as (root, _):
            source = changeset(root / "layer.tar", [(".", D, b""), ("file", F, b"data")])
            raw = PUB.convert.inventory([source], whiteouts=False)
            for key, value in [("sha256", "sha256:" + "0" * 64), ("mtime_ns", 1)]:
                changed = copy.deepcopy(raw)
                changed["file"][key] = value
                with self.subTest(key=key), self.assertRaisesRegex(ValueError, "inventory changed"):
                    MERGE.overlay_inventories(raw, [(changed, raw)])

    def test_correction_layer_contains_only_exact_directory_metadata(self):
        with workspace() as (root, _):
            source = changeset(
                root / "source.tar",
                [(".", D, b""), ("dir", D, b""), ("file", F, b"data")],
                time="0.000000001",
            )
            tree = PUB.convert.inventory([source])
            correction = root / "directories.tar"
            MERGE.directory_tar(tree, correction)
            expected = {name: entry for name, entry in tree.items() if entry["kind"] == "directory"}
            self.assertEqual(PUB.convert.inventory([correction]), expected)
            with tarfile.open(correction) as archive:
                self.assertTrue(all((item.isdir() and item.size == 0 for item in archive)))
