from __future__ import annotations

import errno
import shutil
import tarfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock

from scripts.images import compose_nydus as ENV
from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_composition import ImageCompositionFixture, component
from tests.image_fixtures.image_format import layer


class ImageCompositionTests(ImageCompositionFixture, unittest.TestCase):
    def test_order_and_independent_component_update_change_only_the_recipe(self):
        with workspace() as (root, stack):
            fixture, base, layers = self.make(root, stack)
            store = root / "store"
            first = ENV.compose(store, base, layers)
            self.assertEqual(first, ENV.compose(store, base, layers))
            reversed_order = ENV.compose(store, base, list(reversed(layers)))
            self.assertNotEqual(first, reversed_order)
            changed = fixture.publish(
                "updated-work", source=fixture.new_source("updated", b"python-v2"), layer_index=0
            )
            updated = ENV.compose(store, base, [component(changed), layers[1]])
            self.assertNotEqual(first, updated)
            old_bundle, old_record = ENV.open_environment(store, first)
            new_bundle, new_record = ENV.open_environment(store, updated)
            self.assertEqual(old_record["base"], new_record["base"])
            self.assertEqual(old_record["layers"][1], new_record["layers"][1])
            old = old_bundle / "components/base/metadata/image.bootstrap"
            new = new_bundle / "components/base/metadata/image.bootstrap"
            self.assertEqual(old.stat().st_ino, new.stat().st_ino)
            shutil.rmtree(store / "revisions")
            (store / "revisions").mkdir(mode=448)
            ENV.open_environment(store, first)
            ENV.open_environment(store, updated)

    def test_raw_layer_inventory_and_handle_domain_are_authenticated(self):
        with workspace() as (root, stack):
            _, _, layers = self.make(root, stack)
            handle = layers[0]["handle"]
            bundle, record = PUB.open_bundle(root / "store", handle)
            self.assertEqual(record["source"]["layer_index"], 0)
            with self.assertRaisesRegex(ValueError, "kind"):
                PUB.verify_bundle(bundle, PUB.PREFIX + PUB.handle_revision(handle))
            path = bundle / "metadata/layer.inventory.json"
            path.unlink()
            path.write_text("{}")
            with self.assertRaisesRegex(ValueError, "mismatch"):
                PUB.open_bundle(root / "store", handle)

    def test_wrong_component_kinds_names_and_duplicate_workspaces_are_rejected(self):
        with workspace() as (root, stack):
            _, base, layers = self.make(root, stack)
            for bad_base, bad_layers in [
                (layers[0]["handle"], []),
                (base, [component(base)]),
                (base, [component(layers[0]["handle"], name="../escape")]),
                (base, layers + [layers[0]]),
                (
                    base,
                    [
                        component(layers[0]["handle"], "workspace", "one"),
                        component(layers[1]["handle"], "workspace", "two"),
                    ],
                ),
            ]:
                with self.subTest(base=bad_base, layers=bad_layers), self.assertRaises(ValueError):
                    ENV.compose(root / "store", bad_base, bad_layers)
            self.assertFalse((root / "store/environments").exists())

    def test_missing_or_wrong_platform_component_prevents_publication(self):
        with workspace() as (root, stack):
            _, base, layers = self.make(root, stack)
            missing = PUB.LAYER_PREFIX + "f" * 64
            with self.assertRaises(FileNotFoundError):
                ENV.compose(root / "store", base, [component(missing)])
            real = PUB.open_bundle

            def wrong_platform(store, handle):
                bundle, record = real(store, handle)
                if handle == layers[0]["handle"]:
                    record["source"]["platform"] = "linux/arm64"
                return (bundle, record)

            with mock.patch.object(PUB, "open_bundle", side_effect=wrong_platform):
                with self.assertRaisesRegex(ValueError, "platform"):
                    ENV.compose(root / "store", base, layers)
            self.assertFalse((root / "store/environments").exists())

    def test_concurrent_composition_and_precommit_failure_preserve_old_versions(self):
        with workspace() as (root, stack):
            _, base, layers = self.make(root, stack)
            store = root / "store"
            with ThreadPoolExecutor(max_workers=2) as pool:
                handles = list(pool.map(lambda _: ENV.compose(store, base, layers), range(2)))
            self.assertEqual(handles[0], handles[1])
            with mock.patch.object(PUB.os, "rename", side_effect=OSError(errno.EIO, "injected")):
                with self.assertRaises(OSError):
                    ENV.compose(store, base, list(reversed(layers)))
            ENV.open_environment(store, handles[0])
            self.assertEqual(len(list((store / "environments").iterdir())), 1)
            self.assertEqual(list((store / "staging").iterdir()), [])

    def test_mutated_environment_or_dependency_is_rejected(self):
        for filename in ("ENVIRONMENT.json", "components/0000/metadata/layer.inventory.json"):
            with self.subTest(filename=filename), workspace() as (root, stack):
                _, base, layers = self.make(root, stack)
                handle = ENV.compose(root / "store", base, layers)
                bundle, _ = ENV.open_environment(root / "store", handle)
                path = bundle / filename
                path.unlink()
                path.write_text("{}")
                with self.assertRaises(ValueError):
                    ENV.open_environment(root / "store", handle)


class RawLayerInventoryTests(unittest.TestCase):
    def test_raw_inventory_retains_both_whiteout_kinds_and_invalid_markers_fail(self):
        with workspace() as (root, _):
            source = layer(
                root / "layer.tar",
                [
                    (".wh.old", tarfile.REGTYPE, b""),
                    ("dir/.wh..wh..opq", tarfile.REGTYPE, b""),
                    ("dir/new", tarfile.REGTYPE, b"new"),
                ],
            )
            raw = PUB.convert.inventory([source], whiteouts=False)
            self.assertIn(".wh.old", raw)
            self.assertIn("dir/.wh..wh..opq", raw)
            destination = root / "normalized.tar"
            PUB.convert.flattened_tar([source], destination, whiteouts=False)
            self.assertEqual(
                PUB.convert.differences(raw, PUB.convert.inventory([destination], whiteouts=False)),
                [],
            )
            bad = layer(root / "bad.tar", [(".wh.old", tarfile.REGTYPE, b"bad")])
            with self.assertRaisesRegex(ValueError, "empty regular"):
                PUB.convert.inventory([bad], whiteouts=False)
