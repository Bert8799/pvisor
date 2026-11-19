from __future__ import annotations

import collections
import fcntl
import hashlib
import json
import os
import subprocess
import sys
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from scripts.images import merge_nydus as MERGE
from tests.fixtures import workspace
from tests.image_fixtures.image_node import TOOLS, NodeFixture, Origin, node, rpc


@unittest.skipUnless(
    all((os.environ.get(k) for k in TOOLS)) and sys.platform == "linux",
    "requires pinned Nydus tools, reader and Linux loopback access",
)
class NativeImageNodeTests(NodeFixture, unittest.TestCase):
    def test_partial_reads_cross_process_coalescing_and_persistent_cross_image_cache(self):
        with workspace() as (root, _):
            store, handles, blobs, payload = self.prepare(root, layers=True)
            with Origin(blobs) as origin:
                with node(root, store, origin) as (endpoint, _):
                    reply, _ = rpc(
                        endpoint, handles[0], dict(op="open", commit_device=0, commit_inode=1)
                    )
                    self.assertEqual(reply["status"], "error")
                    self.assertEqual(
                        origin.requests, [], "wrong physical commit must not initiate remote I/O"
                    )
                    for handle in handles:
                        self.opened(endpoint, store, handle)
                    self.assertLess(
                        sum((end - start + 1 for _, start, end in origin.requests)), len(payload)
                    )
                    before = len(origin.requests)
                    origin.delay = 0.1
                    program = "import hashlib,sys\nfrom tests.image_fixtures.image_node import rpc\nsys.stdin.read(1)\nreply, body = rpc(sys.argv[1],sys.argv[2],dict(op='read',path='payload',offset=0,length=64))\nassert reply['status']=='ok', reply\nprint(hashlib.sha256(body).hexdigest())\n"
                    children = [
                        subprocess.Popen(
                            [sys.executable, "-c", program, str(endpoint), handles[i % 2]],
                            stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                        )
                        for i in range(8)
                    ]
                    try:
                        with ThreadPoolExecutor(max_workers=8) as pool:
                            outputs = list(
                                pool.map(lambda p: p.communicate(b"x", timeout=20), children)
                            )
                        for process, (out, err) in zip(children, outputs):
                            self.assertEqual(process.returncode, 0, err.decode())
                            self.assertEqual(
                                out.strip().decode(), hashlib.sha256(payload[:64]).hexdigest()
                            )
                    finally:
                        for process in children:
                            if process.poll() is None:
                                process.kill()
                            process.wait()
                    requests = origin.requests[before:]
                    self.assertTrue(requests)
                    self.assertEqual(max(collections.Counter(requests).values()), 1, requests)
                    self.assertLess(
                        sum((end - start + 1 for _, start, end in requests)), len(payload)
                    )
                    warm = len(origin.requests)
                    for handle in handles:
                        self.assertEqual(
                            rpc(
                                endpoint,
                                handle,
                                dict(op="read", path="payload", offset=0, length=64),
                            )[1],
                            payload[:64],
                        )
                    self.assertEqual(len(origin.requests), warm)
                    for request, expected in [
                        (dict(op="stat", path="missing"), 2),
                        (dict(op="read", path="", offset=0, length=1), 21),
                    ]:
                        reply, data = rpc(endpoint, handles[0], request)
                        self.assertEqual(reply["errno"], expected)
                        self.assertEqual(data, b"")
                before = len(origin.requests)
                with node(root, store, origin) as (endpoint, _):
                    self.opened(endpoint, store, handles[0])
                    self.assertEqual(
                        rpc(
                            endpoint,
                            handles[0],
                            dict(op="read", path="payload", offset=0, length=64),
                        )[1],
                        payload[:64],
                    )
                self.assertEqual(
                    len(origin.requests),
                    before,
                    "restart must reuse validated disk data and metadata",
                )
                registry = json.loads((root / "cache/registry.json").read_text())
                self.assertEqual(set(registry["blobs"]), set(blobs))
                self.assertLessEqual(
                    sum((v["charged_bytes"] for v in registry["blobs"].values())), 64 * 1024 * 1024
                )

    def test_capacity_rejection_and_resident_image_eviction_release_only_idle_leases(self):
        with workspace() as (root, _):
            store, handles, blobs, _ = self.prepare(root, layers=True)
            with Origin(blobs) as origin:
                with node(root, store, origin, cache_name="tiny", max_cache_bytes=1) as (
                    endpoint,
                    _,
                ):
                    info = (store / "merged" / MERGE.revision(handles[0]) / "MERGED.json").stat()
                    reply, body = rpc(
                        endpoint,
                        handles[0],
                        dict(op="open", commit_device=info.st_dev, commit_inode=info.st_ino),
                    )
                    self.assertEqual(reply["status"], "error")
                    self.assertIn("capacity", reply["message"])
                    self.assertEqual(body, b"")
                    self.assertEqual(origin.requests, [])
                    registry = json.loads((root / "tiny/registry.json").read_text())
                    self.assertEqual(registry["blobs"], {})
                with node(root, store, origin, max_images=1) as (endpoint, _):
                    self.opened(endpoint, store, handles[0])
                    path = store / "merged" / MERGE.revision(handles[0]) / "MERGED.json"
                    with path.open("rb") as lease:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        self.opened(endpoint, store, handles[1])
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        fcntl.flock(lease, fcntl.LOCK_UN)

    def test_remote_corruption_rejected_without_prefix_and_retry_can_recover(self):
        chunk = 1024 * 1024
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            blob_id, original = next(iter(blobs.items()))
            for fault in ("payload", "payload-and-digest", "transient"):
                data = bytearray(original)
                pos = data.find(payload[chunk : chunk + 64])
                self.assertGreaterEqual(pos, 0)
                data[pos + 16] ^= 1
                if fault == "payload-and-digest":
                    digest = hashlib.sha256(payload[chunk : 2 * chunk]).digest()
                    self.assertEqual(data.count(digest), 1)
                    index = data.find(digest)
                    changed = bytearray(payload[chunk : 2 * chunk])
                    changed[16] ^= 1
                    data[index : index + 32] = hashlib.sha256(changed).digest()
                with (
                    self.subTest(fault=fault),
                    Origin({blob_id: original if fault == "transient" else bytes(data)}) as origin,
                ):
                    with node(root, store, origin, cache_name="cache-" + fault) as (endpoint, _):
                        if fault == "payload-and-digest":
                            info = (
                                store / "merged" / MERGE.revision(handle) / "MERGED.json"
                            ).stat()
                            reply, body = rpc(
                                endpoint,
                                handle,
                                dict(
                                    op="open", commit_device=info.st_dev, commit_inode=info.st_ino
                                ),
                            )
                        else:
                            self.opened(endpoint, store, handle)
                            if fault == "transient":
                                origin.fail_requests = True
                            reply, body = rpc(
                                endpoint,
                                handle,
                                dict(op="read", path="payload", offset=chunk - 32, length=64),
                            )
                        if fault == "payload-and-digest" and reply["status"] == "ok":
                            reply, body = rpc(
                                endpoint,
                                handle,
                                dict(op="read", path="payload", offset=chunk, length=64),
                            )
                        self.assertEqual(reply["status"], "error", reply)
                        self.assertEqual(
                            body, b"", "failed multi-chunk read must disclose no good prefix"
                        )
                        if fault == "transient":
                            origin.fail_requests = False
                            reply, body = rpc(
                                endpoint,
                                handle,
                                dict(op="read", path="payload", offset=chunk - 32, length=64),
                            )
                            self.assertEqual(reply["status"], "ok", reply)
                            self.assertEqual(body, payload[chunk - 32 : chunk + 32], reply)

    @unittest.skipUnless(os.environ.get("PVISOR_RAFS_DAEMON"), "requires product cache daemon")
    def test_product_cache_protocol_authentication_and_owner_shutdown(self):
        token = "fixture-node-token"
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            with Origin(blobs) as origin:
                with node(root, store, origin, daemon=True, token=token) as (endpoint, process):
                    refused, body = rpc(
                        endpoint, handle, dict(op="stat", path="payload"), cache=True, token="wrong"
                    )
                    self.assertEqual(refused["status"], "error")
                    self.assertEqual(refused["code"], "permission_denied")
                    self.assertEqual(body, b"")
                    self.assertEqual(origin.requests, [])
                    reply = self.opened(endpoint, store, handle, cache=True, token=token)
                    self.assertTrue(reply["private_roots"])
                    generation = reply["node_generation"]
                    refused, body = rpc(
                        endpoint,
                        handle,
                        dict(op="stat", path="payload"),
                        cache=True,
                        token=token,
                        generation="stale-service",
                    )
                    self.assertEqual(refused["status"], "error")
                    self.assertIn("generation", refused["message"])
                    self.assertEqual(body, b"")
                    self.assertEqual(
                        rpc(
                            endpoint,
                            handle,
                            dict(op="read", path="payload", offset=0, length=64),
                            cache=True,
                            token=token,
                            generation=generation,
                        )[1],
                        payload[:64],
                    )
                    receipt = json.loads((root / "cache/control.json").read_text())
                    private = Path(receipt["path"])
                    children = (
                        Path(f"/proc/{process.pid}/task/{process.pid}/children").read_text().split()
                    )
                    self.assertEqual(len(children), 1)
                self.assertEqual(process.returncode, 0)
                self.assertFalse(private.exists())
                self.assertFalse((root / "cache/control.json").exists())
                for pid in children:
                    self.assertFalse(Path("/proc", pid).exists(), "owned reader leaked")
                with node(root, store, origin, daemon=True, token=token) as (endpoint, _):
                    reopened = self.opened(endpoint, store, handle, cache=True, token=token)
                    self.assertNotEqual(reopened["node_generation"], generation)
                    stale, body = rpc(
                        endpoint,
                        handle,
                        dict(op="stat", path="payload"),
                        cache=True,
                        token=token,
                        generation=generation,
                    )
                    self.assertEqual(stale["status"], "error")
                    self.assertIn("generation", stale["message"])
                    self.assertEqual(body, b"")
                    generation = reopened["node_generation"]
                    before = len(origin.requests)
                    self.assertEqual(
                        rpc(
                            endpoint,
                            handle,
                            dict(op="read", path="payload", offset=0, length=64),
                            cache=True,
                            token=token,
                            generation=generation,
                        )[1],
                        payload[:64],
                    )
                self.assertEqual(len(origin.requests), before, origin.requests[before:])
