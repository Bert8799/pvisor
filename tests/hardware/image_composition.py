from __future__ import annotations

import json
import os
import shutil
import tarfile
import unittest
from pathlib import Path

from scripts.images import compose_nydus as ENV
from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_composition import component
from tests.image_fixtures.image_format import layer, layout


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
        )
    ),
    "requires explicit native Nydus conversion and inventory tools",
)
class NativeImageCompositionTests(unittest.TestCase):
    def test_pinned_raw_layers_retain_deletions_and_order_in_a_native_merge(self):
        with workspace() as (root, _):
            specifications = [
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("dir", tarfile.DIRTYPE, b""),
                    ("dir/old", tarfile.REGTYPE, b"old"),
                    ("gone", tarfile.REGTYPE, b"gone"),
                    ("tool", tarfile.REGTYPE, b"base"),
                    ("alias", tarfile.LNKTYPE, "tool"),
                ],
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("dir", tarfile.DIRTYPE, b""),
                    (".wh.gone", tarfile.REGTYPE, b""),
                    ("tool", tarfile.REGTYPE, b"python"),
                ],
                [
                    (".", tarfile.DIRTYPE, b""),
                    ("dir", tarfile.DIRTYPE, b""),
                    ("dir/.wh..wh..opq", tarfile.REGTYPE, b""),
                    ("dir/new", tarfile.REGTYPE, b"new"),
                    ("tool", tarfile.REGTYPE, b"workspace"),
                ],
            ]
            tools = [
                Path(os.environ[name])
                for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
            ]
            source_layers, handles = ([], [])
            for index, entries in enumerate(specifications):
                source_layer = layer(root / f"layer-{index}.tar", entries, ns=True)
                source_layers.append(source_layer)
                source = layout(root / f"oci-{index}", [source_layer])
                handles.append(
                    PUB.publish(
                        source,
                        "linux/amd64",
                        *tools,
                        root / "store",
                        root / f"work-{index}",
                        **{"layer_index": 0} if index else {},
                    )
                )
            overlays = [component(handles[1]), component(handles[2], "workspace", "initial")]
            env = ENV.compose(root / "store", handles[0], overlays)
            reverse = ENV.compose(root / "store", handles[0], list(reversed(overlays)))
            self.assertNotEqual(env, reverse)
            shutil.rmtree(root / "store/revisions")
            (root / "store/revisions").mkdir(mode=448)
            for index, handle in enumerate((env, reverse)):
                bundle, record = ENV.open_environment(root / "store", handle)
                output = root / f"merge-{index}"
                output.mkdir()
                blobs = output / "blobs"
                blobs.mkdir()
                bootstraps = []
                for name, dependency in ENV.references(record):
                    path = bundle / "components" / name
                    bootstraps.append(path / "metadata/image.bootstrap")
                    details = PUB.verify_bundle(path, dependency)
                    if dependency.startswith(PUB.LAYER_PREFIX):
                        inventory = json.loads((path / "metadata/layer.inventory.json").read_text())
                        self.assertTrue(any((".wh." in name for name in inventory)))
                    for blob_id in details["blobs"]:
                        target = blobs / blob_id
                        if not target.exists():
                            os.link(path / "blobs" / blob_id, target)
                before = {p.name: PUB.convert.digest_file(p) for p in blobs.iterdir()}
                config = output / "config.json"
                PUB.convert.save(
                    config,
                    {
                        "version": 2,
                        "backend": {"type": "localfs", "localfs": {"dir": str(blobs)}},
                        "cache": {
                            "type": "blobcache",
                            "validate": True,
                            "filecache": {"work_dir": str(output / "cache")},
                        },
                        "rafs": {"mode": "direct", "validate": False, "enable_xattr": True},
                    },
                )
                bootstrap = output / "merged.bootstrap"
                PUB.convert.run_tool(
                    [
                        str(tools[0]),
                        "merge",
                        "--bootstrap",
                        str(bootstrap),
                        "--config",
                        str(config),
                        *map(str, bootstraps),
                    ],
                    output / "merge.log",
                    30,
                )
                actual = PUB.convert.scan_direct(tools[2], bootstrap, blobs, output, 30)
                order = (
                    source_layers
                    if not index
                    else [source_layers[0], source_layers[2], source_layers[1]]
                )
                self.assertEqual(PUB.convert.differences(PUB.convert.inventory(order), actual), [])
                self.assertEqual(
                    before, {p.name: PUB.convert.digest_file(p) for p in blobs.iterdir()}
                )
