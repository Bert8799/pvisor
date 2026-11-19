from __future__ import annotations

import os
import sys
import unittest
from concurrent.futures import ThreadPoolExecutor

from tests.fixtures import workspace
from tests.image_fixtures import image_lifecycle as L
from tests.image_fixtures import image_node as N
from tests.image_fixtures import image_resources as R
from tests.image_fixtures.image_metadata import MetadataFixture


@unittest.skipUnless(
    sys.platform == "linux" and all((os.environ.get(k) for k in (*N.TOOLS, "PVISOR_RAFS_DAEMON"))),
    "requires native reader tools and cache daemon on Linux",
)
class NativeImageMetadataTests(MetadataFixture, unittest.TestCase):
    def test_switching_images_releases_hints_before_automatic_capacity_reclamation(self):
        with workspace() as (root, _):
            store = root / "node-store"
            first, blobs_a, data_a = L.LifecycleFixture.prepare_seed(self, root / "a", 1, store)
            second, blobs_b, data_b = L.LifecycleFixture.prepare_seed(self, root / "b", 2, store)
            with (
                N.Origin(blobs_a | blobs_b) as origin,
                N.node(
                    root, store, origin, daemon=True, max_images=1, max_cache_bytes=4 * 1024**2
                ) as (endpoint, _),
            ):
                generation = None
                for handle, data in ((first, data_a), (second, data_b), (first, data_a)):
                    if generation is not None:
                        reply, body = N.rpc(
                            endpoint,
                            handle,
                            dict(op="read", path="payload", offset=0, length=64),
                            cache=True,
                            generation=generation,
                        )
                        self.assertEqual(reply["status"], "ok", reply)
                        self.assertEqual(body, data[:64])
                    opened = N.NodeFixture.opened(self, endpoint, store, handle, cache=True)
                    auth = dict(cache=True, generation=opened["node_generation"])
                    generation = opened["node_generation"]
                    reply, _ = N.rpc(
                        endpoint, handle, dict(op="list", path="", offset=0, limit=128), **auth
                    )
                    self.assertEqual(reply["status"], "ok", reply)
                    reply, body = N.rpc(
                        endpoint,
                        handle,
                        dict(op="read", path="payload", offset=0, length=64),
                        **auth,
                    )
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, data[:64])
                    snapshot = R.stats(endpoint, handle, **auth)
                    self.assertEqual(snapshot["resident_images"], 1)
                    self.assertEqual(snapshot["shared_metadata_cache"]["image_leases"], 1)

    def test_list_batches_share_exact_attributes_without_payload_and_clear_before_reclamation(self):
        with workspace() as (root, _):
            store, handle, blobs = self.prepare(root)
            with (
                N.Origin(blobs) as origin,
                N.node(root, store, origin, daemon=True, token="metadata-test") as (endpoint, _),
            ):
                opened = N.NodeFixture.opened(
                    self, endpoint, store, handle, cache=True, token="metadata-test"
                )
                auth = dict(cache=True, token="metadata-test", generation=opened["node_generation"])

                def call(op, **fields):
                    reply, body = N.rpc(endpoint, handle, dict(op=op, **fields), **auth)
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, b"")
                    return reply

                before = R.stats(endpoint, handle, **auth)["shared_metadata_cache"]
                ranges = list(origin.requests)
                page = call("list", path="dir", offset=0, limit=128)
                self.assertEqual(
                    set(page), {"status", "page"}, "private batches leaked onto public frames"
                )
                paths = ["dir/" + name for name in page["page"]["names"]]
                with ThreadPoolExecutor(max_workers=6) as pool:
                    values = list(pool.map(lambda path: call("stat", path=path)["node"], paths * 3))
                cached = R.stats(endpoint, handle, **auth)["shared_metadata_cache"]
                self.assertGreaterEqual(cached["hits"] - before["hits"], len(paths) * 3)
                self.assertEqual(cached["image_leases"], 1)
                self.assertLessEqual(cached["bytes"], cached["max_bytes"])
                self.assertEqual(origin.requests, ranges, "metadata batching read file data")
                by_path = dict(zip(paths, values))
                self.assertEqual(by_path["dir/file"]["inode"], by_path["dir/alias"]["inode"])
                self.assertEqual(by_path["dir/link"]["target"], "file")
                self.assertEqual(by_path["dir/file"]["mtime_nsec"], 123456789)
                self.assertEqual(by_path["dir/file"]["xattrs"]["user.test"], list(b"inherited"))
                denied, _ = N.rpc(
                    endpoint,
                    handle,
                    dict(op="stat", path=paths[0]),
                    cache=True,
                    token="wrong",
                    generation=opened["node_generation"],
                )
                self.assertEqual(denied["status"], "error")
                denied, _ = N.rpc(
                    endpoint,
                    handle,
                    dict(op="stat", path=paths[0]),
                    **auth | dict(generation="wrong-generation"),
                )
                self.assertEqual(denied["status"], "error")
                call("trim", target_bytes=0)
                self.assertEqual(
                    R.stats(endpoint, handle, **auth)["shared_metadata_cache"]["image_leases"], 0
                )
                call("remove")
                missing, _ = N.rpc(endpoint, handle, dict(op="stat", path=paths[0]), **auth)
                self.assertEqual(missing["status"], "error")

    def test_minimum_work_budget_keeps_name_only_list_and_exact_uncached_metadata(self):
        with workspace() as (root, _):
            store, handle, blobs = self.prepare(root)
            with (
                N.Origin(blobs) as origin,
                N.node(root, store, origin, daemon=True, max_io_bytes=4 * 1024**2) as (endpoint, _),
            ):
                opened = N.NodeFixture.opened(self, endpoint, store, handle, cache=True)
                auth = dict(cache=True, generation=opened["node_generation"])
                reply, _ = N.rpc(
                    endpoint, handle, dict(op="list", path="dir", offset=0, limit=128), **auth
                )
                self.assertEqual(reply["status"], "ok", reply)
                snapshot = R.stats(endpoint, handle, **auth)["shared_metadata_cache"]
                self.assertEqual(snapshot["entries"], 0)
                first, _ = N.rpc(endpoint, handle, dict(op="stat", path="dir/file"), **auth)
                second, _ = N.rpc(endpoint, handle, dict(op="stat", path="dir/file"), **auth)
                self.assertEqual(first, second)
                self.assertEqual(first["status"], "ok", first)
                R.ResourceFixture.assert_limits(self, R.stats(endpoint, handle, **auth))
