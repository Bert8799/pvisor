from __future__ import annotations

import fcntl
import os
import resource
import sys
import threading
import time
import unittest
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path

from tests.fixtures import workspace
from tests.image_fixtures import image_node as N
from tests.image_fixtures.image_resources import MiB, ResourceFixture, TrackingOrigin, stats


@unittest.skipUnless(
    all((os.environ.get(k) for k in N.TOOLS)) and sys.platform == "linux",
    "requires pinned node-admission Nydus tools and Linux loopback access",
)
class NativeImageResourceTests(ResourceFixture, unittest.TestCase):
    def test_shared_metadata_and_data_backing_are_charged_once_and_kernel_ceiling_is_real(self):
        with workspace() as (root, _):
            store, handles, blobs, payload = self.prepare(root, layers=True)
            with (
                TrackingOrigin(blobs) as origin,
                N.node(root, store, origin) as (endpoint, process),
            ):
                empty = stats(endpoint, handles[0])
                self.assertEqual(empty["resident_images"], 0)
                self.assertEqual(empty["resources"]["metadata_bytes"], 0)
                self.opened(endpoint, store, handles[1])
                first = stats(endpoint, handles[1])
                self.assertEqual(first["resources"]["shared_blobs"], 1)
                self.opened(endpoint, store, handles[1])
                self.assertEqual(
                    stats(endpoint, handles[1])["resources"]["metadata_bytes"],
                    first["resources"]["metadata_bytes"],
                )
                self.opened(endpoint, store, handles[0])
                second = stats(endpoint, handles[0])
                self.assertEqual(second["resident_images"], 2)
                self.assertEqual(second["resources"]["shared_blobs"], 2)
                self.assertEqual(second["resources"]["blob_references"], 3)
                self.assertGreater(
                    second["resources"]["blob_metadata_bytes"],
                    first["resources"]["blob_metadata_bytes"],
                )
                for handle in handles:
                    reply, body = N.rpc(
                        endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                    )
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, payload[:64])
                data_fds = []
                for fd in Path(f"/proc/{process.pid}/fd").iterdir():
                    try:
                        target = os.readlink(fd)
                    except FileNotFoundError:
                        continue
                    if target.endswith(".blob.data"):
                        data_fds.append(target)
                self.assertEqual(len(data_fds), 2, data_fds)
                self.assertEqual(len(set(data_fds)), 2, "one physical data backing per blob")
                ceiling = resource.prlimit(process.pid, resource.RLIMIT_AS)
                self.assertEqual(ceiling, (1024 * MiB, 1024 * MiB))
                self.assert_limits(second)

    def test_actual_range_concurrency_never_exceeds_the_shared_download_gate(self):
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            with (
                TrackingOrigin(blobs) as origin,
                N.node(root, store, origin, workers=6, max_downloads=2) as (endpoint, _),
            ):
                self.opened(endpoint, store, handle)
                origin.idle()
                origin.delay = 0.1
                barrier = threading.Barrier(6)

                def read(index):
                    barrier.wait(timeout=5)
                    return N.rpc(
                        endpoint,
                        handle,
                        dict(op="read", path="payload", offset=index * MiB, length=64),
                    )

                with ThreadPoolExecutor(max_workers=6) as pool:
                    results = list(pool.map(read, range(6)))
                for index, (reply, body) in enumerate(results):
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, payload[index * MiB : index * MiB + 64])
                origin.idle(reset=False)
                self.assertEqual(origin.peak, 2)
                snapshot = stats(endpoint, handle)
                self.assertEqual(snapshot["max_downloads"], 2)
                self.assert_limits(snapshot)
                self.assertGreaterEqual(snapshot["resources"]["io_active_peak"], 2)
                self.assertEqual(snapshot["resources"]["io_bytes"], 0)

    def test_work_byte_saturation_is_fail_fast_and_releases_after_slow_response(self):
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            with (
                TrackingOrigin(blobs) as origin,
                N.node(root, store, origin, max_io_bytes=12 * MiB) as (endpoint, _),
            ):
                self.opened(endpoint, store, handle)
                origin.idle()
                origin.block = True

                def read(index):
                    return N.rpc(
                        endpoint,
                        handle,
                        dict(op="read", path="payload", offset=index * MiB, length=64),
                    )

                with ThreadPoolExecutor(max_workers=4) as pool:
                    futures = [pool.submit(read, 0)]
                    try:
                        self.assertTrue(origin.entered.wait(5))
                        futures.extend((pool.submit(read, index) for index in range(1, 4)))
                        deadline = time.monotonic() + 4
                        while len([f for f in futures if f.done()]) < 3:
                            remaining = deadline - time.monotonic()
                            self.assertGreater(remaining, 0)
                            wait(
                                [f for f in futures if not f.done()],
                                timeout=remaining,
                                return_when=FIRST_COMPLETED,
                            )
                        rejected = [f.result() for f in futures if f.done()]
                        self.assertEqual(len(rejected), 3)
                        for reply, body in rejected:
                            self.assertEqual(reply["errno"], 11, reply)
                            self.assertIn("budget", reply["message"])
                            self.assertEqual(body, b"")
                        active = stats(endpoint, handle)
                        self.assertEqual(active["resources"]["io_active"], 1)
                        self.assertEqual(active["resources"]["io_rejections"], 3)
                        self.assert_limits(active)
                    finally:
                        origin.release.set()
                    outputs = [f.result(timeout=10) for f in futures]
                self.assertEqual(sum((reply["status"] == "ok" for reply, _ in outputs)), 1)
                after = stats(endpoint, handle)
                self.assertEqual(after["resources"]["io_bytes"], 0)
                self.assertEqual(after["resources"]["io_active"], 0)
                reply, body = N.rpc(
                    endpoint, handle, dict(op="read", path="payload", offset=7 * MiB, length=64)
                )
                self.assertEqual(reply["status"], "ok", reply)
                self.assertEqual(body, payload[7 * MiB : 7 * MiB + 64])

    def test_metadata_pressure_evicts_idle_images_but_preserves_busy_backing(self):
        with workspace() as (root, _):
            store, handles, blobs, _ = self.prepare(root, layers=True)
            base = handles[1]
            with (
                TrackingOrigin(blobs) as origin,
                N.node(root, store, origin, max_metadata_bytes=4 * MiB) as (endpoint, _),
            ):
                self.opened(endpoint, store, base)
                origin.idle()
                origin.block = True
                with ThreadPoolExecutor(max_workers=1) as pool:
                    future = pool.submit(
                        N.rpc, endpoint, base, dict(op="read", path="payload", offset=0, length=64)
                    )
                    try:
                        self.assertTrue(origin.entered.wait(5))
                        commit = store / "merged" / handles[0].split(":")[1] / "MERGED.json"
                        info = commit.stat()
                        reply, body = N.rpc(
                            endpoint,
                            handles[0],
                            dict(op="open", commit_device=info.st_dev, commit_inode=info.st_ino),
                        )
                        self.assertEqual(reply["errno"], 11, reply)
                        self.assertEqual(body, b"")
                        before = stats(endpoint, base)
                        self.assertEqual(before["resident_images"], 1)
                        with (store / "merged" / base.split(":")[1] / "MERGED.json").open(
                            "rb"
                        ) as lease:
                            with self.assertRaises(BlockingIOError):
                                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    finally:
                        origin.release.set()
                    self.assertEqual(future.result(timeout=10)[0]["status"], "ok")
                origin.block = False
                for index in range(12):
                    selected = handles[index % 2]
                    self.opened(endpoint, store, selected)
                    snapshot = stats(endpoint, selected)
                    self.assertEqual(snapshot["resident_images"], 1)
                    self.assertEqual(
                        snapshot["resources"]["shared_blobs"], 2 if selected == handles[0] else 1
                    )
                    self.assertEqual(
                        snapshot["resources"]["blob_references"], 2 if selected == handles[0] else 1
                    )
                    self.assert_limits(snapshot)
                self.assertGreater(snapshot["resources"]["metadata_rejections"], 0)

    @unittest.skipUnless(os.environ.get("PVISOR_RAFS_DAEMON"), "requires product cache daemon")
    def test_product_limits_and_authenticated_resource_discovery(self):
        with workspace() as (root, _):
            store, (handle,), blobs, _ = self.prepare(root)
            with (
                TrackingOrigin(blobs) as origin,
                N.node(
                    root,
                    store,
                    origin,
                    daemon=True,
                    token="resource-token",
                    client_metadata_entries=1,
                ) as (endpoint, _),
            ):
                denied, body = N.rpc(endpoint, handle, {"op": "stats"}, cache=True, token="wrong")
                self.assertEqual(denied["status"], "error")
                self.assertEqual(denied["code"], "permission_denied")
                self.assertEqual(body, b"")
                discovered = stats(endpoint, handle, cache=True, token="resource-token")
                self.assertEqual(discovered["resident_images"], 0)
                self.assertEqual(discovered["client_limits"]["metadata_entries"], 1)
                opened = self.opened(endpoint, store, handle, cache=True, token="resource-token")
                self.assertEqual(opened["client_limits"], discovered["client_limits"])
                snapshot = stats(
                    endpoint,
                    handle,
                    cache=True,
                    token="resource-token",
                    generation=opened["node_generation"],
                )
                self.assert_limits(snapshot)
