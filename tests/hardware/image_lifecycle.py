from __future__ import annotations

import fcntl
import json
import os
import sys
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from scripts.images import manage_node as MANAGE
from tests.fixtures import workspace
from tests.image_fixtures import image_node as N
from tests.image_fixtures.image_lifecycle import LifecycleFixture, MiB
from tests.image_fixtures.image_resources import TrackingOrigin, stats


@unittest.skipUnless(
    all((os.environ.get(k) for k in N.TOOLS)) and sys.platform == "linux",
    "requires pinned Nydus tools and local Linux sockets",
)
class NativeImageLifecycleTests(LifecycleFixture, unittest.TestCase):
    def test_durable_template_and_checkpoint_dependencies_survive_restart_and_share_blob_pins(self):
        with workspace() as (root, _):
            store, handles, blobs, payload = self.prepare(root, layers=True)
            combined, base = handles
            with TrackingOrigin(blobs) as origin:
                with N.node(root, store, origin) as (endpoint, process):
                    first = self.lease(endpoint, base, "template", "template-generation-1")
                    self.assertEqual(
                        first, self.lease(endpoint, base, "template", "template-generation-1")
                    )
                    self.lease(endpoint, base, "checkpoint", "checkpoint-1")
                    self.lease(endpoint, combined, "template", "template-generation-2")
                    self.assertEqual(self.commit(store, base).stat().st_nlink, 3)
                    for handle in handles:
                        reply, body = N.rpc(
                            endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                        )
                        self.assertEqual(reply["status"], "ok", reply)
                        self.assertEqual(body, payload[:64])
                    self.assertFalse(self.trim(endpoint, base)["target_reached"])
                    for handle in handles:
                        reply, _ = N.rpc(endpoint, handle, dict(op="remove"))
                        self.assertEqual(reply["errno"], 16, reply)
                    transferred = len(origin.requests)
                    process.kill()
                    self.assertNotEqual(process.wait(5), 0)
                with N.node(root, store, origin) as (endpoint, _):
                    for handle, kind, owner in [
                        (base, "template", "template-generation-1"),
                        (base, "checkpoint", "checkpoint-1"),
                        (combined, "template", "template-generation-2"),
                    ]:
                        self.assertTrue(
                            self.lease(endpoint, handle, kind, owner, "inspect")["retained"]
                        )
                    self.opened(endpoint, store, base)
                    self.assertEqual(
                        N.rpc(endpoint, base, dict(op="read", path="payload", offset=0, length=64))[
                            1
                        ],
                        payload[:64],
                    )
                    self.assertEqual(len(origin.requests), transferred)
                    self.lease(endpoint, combined, "template", "template-generation-2", "release")
                    trimmed = self.trim(endpoint, base)
                    self.assertFalse(trimmed["target_reached"])
                    self.assertEqual(trimmed["reclaimed_blobs"], 1)
                    self.call(endpoint, combined, "remove")
                    self.assertFalse(self.commit(store, combined).exists())
                    self.lease(endpoint, base, "template", "template-generation-1", "release")
                    self.assertFalse(self.trim(endpoint, base)["target_reached"])
                    self.lease(endpoint, base, "checkpoint", "checkpoint-1", "release")
                    self.assertTrue(self.trim(endpoint, base)["target_reached"])
                    self.assertFalse(
                        self.lease(endpoint, base, "checkpoint", "checkpoint-1", "inspect")[
                            "retained"
                        ]
                    )
                    self.call(endpoint, base, "remove")
                    self.assertEqual(list((store / "retired").iterdir()), [])
                    self.assertFalse(self.commit(store, base).exists())

    def test_active_commit_and_busy_native_backing_block_reclamation(self):
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            with TrackingOrigin(blobs) as origin, N.node(root, store, origin) as (endpoint, _):
                bad, _ = N.rpc(endpoint, handle, dict(op="remove", path="/arbitrary"))
                self.assertEqual(bad["errno"], 22, bad)
                self.assertTrue(self.commit(store, handle).exists())
                self.opened(endpoint, store, handle)
                origin.idle()
                origin.block = True
                with ThreadPoolExecutor(max_workers=1) as pool:
                    read = pool.submit(
                        N.rpc,
                        endpoint,
                        handle,
                        dict(op="read", path="payload", offset=0, length=64),
                    )
                    try:
                        self.assertTrue(origin.entered.wait(5))
                        self.assertFalse(self.trim(endpoint, handle)["target_reached"])
                        reply, _ = N.rpc(endpoint, handle, dict(op="remove"))
                        self.assertEqual(reply["errno"], 11, reply)
                    finally:
                        origin.release.set()
                    self.assertEqual(
                        read.result(timeout=10),
                        (
                            {
                                "status": "ok",
                                "length": 64,
                                "sha256": "sha256:" + N.hashlib.sha256(payload[:64]).hexdigest(),
                            },
                            payload[:64],
                        ),
                    )
                with self.commit(store, handle).open("rb") as lease:
                    fcntl.flock(lease, fcntl.LOCK_SH)
                    self.assertFalse(self.trim(endpoint, handle)["target_reached"])
                    reply, _ = N.rpc(endpoint, handle, dict(op="remove"))
                    self.assertEqual(reply["errno"], 11, reply)
                self.assertTrue(self.trim(endpoint, handle)["target_reached"])
                before = len(origin.requests)
                self.opened(endpoint, store, handle)
                reply, body = N.rpc(
                    endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                )
                self.assertEqual(reply["status"], "ok", reply)
                self.assertEqual(body, payload[:64])
                self.assertGreater(len(origin.requests), before)

    def test_capacity_pressure_reclaims_only_unpinned_blobs_and_reloads_them(self):
        with workspace() as (root, _):
            store = root / "node-store"
            first, a, pa = self.prepare_seed(root / "a", 271, store)
            second, b, pb = self.prepare_seed(root / "b", 272, store)
            with (
                TrackingOrigin(a | b) as origin,
                N.node(root, store, origin, max_cache_bytes=5 * MiB) as (endpoint, _),
            ):
                self.lease(endpoint, first, "checkpoint", "saved-1")
                rejected, _ = N.rpc(
                    endpoint, second, dict(op="read", path="payload", offset=0, length=32)
                )
                self.assertEqual(rejected["errno"], 11, rejected)
                self.assertTrue(
                    self.lease(endpoint, first, "checkpoint", "saved-1", "inspect")["retained"]
                )
                self.lease(endpoint, first, "checkpoint", "saved-1", "release")
                for handle, payload in [(second, pb), (first, pa), (second, pb)]:
                    reply, body = N.rpc(
                        endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                    )
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, payload[:64])
                    snap = stats(endpoint, handle)
                    self.assertLessEqual(snap["disk_reserved_bytes"], 5 * MiB)
                    self.assertEqual(
                        len(json.loads((root / "cache/registry.json").read_text())["blobs"]), 1
                    )

    def test_durable_eviction_intent_recovers_partial_unlink_without_early_credit(self):
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            with TrackingOrigin(blobs) as origin:
                with N.node(root, store, origin) as (endpoint, _):
                    self.assertEqual(
                        N.rpc(
                            endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                        )[1],
                        payload[:64],
                    )
                registry = root / "cache/registry.json"
                ledger = json.loads(registry.read_text())
                blob = next(iter(ledger["blobs"]))
                ledger["deleting"] = [blob]
                registry.write_text(json.dumps(ledger))
                (root / "cache" / (blob + ".blob.data")).unlink()
                with N.node(root, store, origin) as (endpoint, _):
                    self.assertEqual(stats(endpoint, handle)["disk_reserved_bytes"], 0)
                    self.assertFalse(
                        any((p.name.startswith(blob) for p in (root / "cache").iterdir()))
                    )
                    reply, body = N.rpc(
                        endpoint, handle, dict(op="read", path="payload", offset=0, length=64)
                    )
                    self.assertEqual(reply["status"], "ok", reply)
                    self.assertEqual(body, payload[:64])
                ledger = json.loads(registry.read_text())
                ledger["deleting"] = ["f" * 64]
                registry.write_text(json.dumps(ledger))
                data = root / "cache" / (blob + ".blob.data")
                before = data.stat()
                with self.assertRaisesRegex(RuntimeError, "unbound eviction intent"):
                    with N.node(root, store, origin):
                        self.fail("malformed intent must not become ready")
                self.assertEqual(
                    (data.stat().st_ino, data.stat().st_size), (before.st_ino, before.st_size)
                )

    def test_pending_pin_release_and_metadata_tombstone_recovery(self):
        with workspace() as (root, _):
            store, (handle,), blobs, _ = self.prepare(root)
            with TrackingOrigin(blobs) as origin:
                with N.node(root, store, origin) as (endpoint, _):
                    pin = self.lease(endpoint, handle, "template", "saved-template")
                    pins = store / "pins"
                    final = pins / pin["key"]
                    pending = pins / ("pending-" + pin["key"])
                    final.rename(pending)
                    reply, _ = N.rpc(
                        endpoint,
                        handle,
                        dict(op="lease", kind="template", owner="saved-template", action="inspect"),
                    )
                    self.assertEqual(reply["errno"], 11, reply)
                    self.assertTrue(pending.is_dir())
                    self.assertFalse(final.exists())
                with N.node(root, store, origin) as (endpoint, _):
                    self.assertTrue(
                        self.lease(endpoint, handle, "template", "saved-template", "inspect")[
                            "retained"
                        ]
                    )
                    self.assertTrue(final.is_dir())
                releasing = pins / ("releasing-" + pin["key"])
                final.rename(releasing)
                (releasing / "MERGED.json").unlink()
                with N.node(root, store, origin) as (endpoint, _):
                    self.assertFalse(
                        self.lease(endpoint, handle, "template", "saved-template", "inspect")[
                            "retained"
                        ]
                    )
                commit = self.commit(store, handle)
                info = commit.stat()
                retired = (
                    store / "retired" / f"{handle.split(':')[1]}-{info.st_dev:x}-{info.st_ino:x}"
                )
                commit.parent.rename(retired)
                (retired / "metadata/image.bootstrap").unlink()
                with N.node(root, store, origin):
                    self.assertFalse(retired.exists())
                    self.assertFalse(commit.exists())

    def test_concurrent_retain_trim_and_remove_never_collect_an_acknowledged_dependency(self):
        with workspace() as (root, _):
            store, (handle,), blobs, _ = self.prepare(root)
            sources = [
                p.parents[2]
                for p in root.glob("*/merged/" + handle.split(":")[1] + "/MERGED.json")
                if (p.parent / "environment").is_dir()
            ]
            self.assertEqual(len(sources), 1)
            with TrackingOrigin(blobs) as origin, N.node(root, store, origin) as (endpoint, _):
                for index in range(8):
                    if not self.commit(store, handle).exists():
                        N.STAGE.stage(sources[0], handle, store)
                    barrier = threading.Barrier(3)

                    def call(request):
                        barrier.wait(timeout=5)
                        return N.rpc(endpoint, handle, request)[0]

                    with ThreadPoolExecutor(max_workers=3) as pool:
                        futures = [
                            pool.submit(call, r)
                            for r in [
                                dict(
                                    op="lease",
                                    kind="checkpoint",
                                    owner=f"saved-{index}",
                                    action="retain",
                                ),
                                dict(op="trim", target_bytes=0),
                                dict(op="remove"),
                            ]
                        ]
                        results = [f.result(timeout=20) for f in futures]
                    for reply in results:
                        if reply["status"] == "error":
                            self.assertIn(reply["errno"], (2, 11, 16), reply)
                    if results[0]["status"] == "ok":
                        self.assertTrue(self.commit(store, handle).exists())
                        self.assertTrue(
                            self.lease(endpoint, handle, "checkpoint", f"saved-{index}", "inspect")[
                                "retained"
                            ]
                        )
                        self.assertFalse(self.trim(endpoint, handle)["target_reached"])
                        self.lease(endpoint, handle, "checkpoint", f"saved-{index}", "release")
                    self.assertTrue(self.trim(endpoint, handle)["target_reached"])
                    if self.commit(store, handle).exists():
                        self.call(endpoint, handle, "remove")

    def test_rebound_canonical_metadata_cannot_bypass_a_durable_pin(self):
        with workspace() as (root, _):
            store, (handle,), blobs, _ = self.prepare(root)
            with TrackingOrigin(blobs) as origin:
                with N.node(root, store, origin) as (endpoint, _):
                    pin = self.lease(endpoint, handle, "checkpoint", "saved-generation")
                commit = self.commit(store, handle)
                raw = commit.read_bytes()
                old_inode = commit.stat().st_ino
                commit.rename(commit.with_name("old-commit"))
                commit.write_bytes(raw)
                self.assertNotEqual(commit.stat().st_ino, old_inode)
                with self.assertRaisesRegex(
                    RuntimeError, "retained metadata disappeared or changed"
                ):
                    with N.node(root, store, origin):
                        self.fail("rebound canonical metadata must not become ready")
                self.assertEqual(
                    (store / "pins" / pin["key"] / "MERGED.json").stat().st_ino, old_inode
                )
                self.assertEqual(commit.read_bytes(), raw)

    def test_failed_multichunk_read_can_be_trimmed_without_open_deleted_backing(self):
        with workspace() as (root, _):
            store, (handle,), blobs, payload = self.prepare(root)
            blob, original = next(iter(blobs.items()))
            damaged = bytearray(original)
            position = damaged.find(payload[MiB : MiB + 64])
            self.assertGreaterEqual(position, 0)
            damaged[position + 16] ^= 1
            with (
                TrackingOrigin({blob: bytes(damaged)}) as origin,
                N.node(root, store, origin) as (endpoint, process),
            ):
                self.opened(endpoint, store, handle)
                reply, body = N.rpc(
                    endpoint, handle, dict(op="read", path="payload", offset=MiB - 32, length=64)
                )
                self.assertEqual(reply["status"], "error", reply)
                self.assertEqual(body, b"", "failed reads must not disclose a valid prefix")
                self.assertTrue(self.trim(endpoint, handle)["target_reached"])
                retained = []
                for fd in Path(f"/proc/{process.pid}/fd").iterdir():
                    try:
                        target = os.readlink(fd)
                    except FileNotFoundError:
                        continue
                    if str(root / "cache" / blob) in target:
                        retained.append(target)
                self.assertEqual(
                    retained, [], "reclaimed cache must have no deferred writer descriptor"
                )
                origin.blobs[blob] = original
                reply, body = N.rpc(
                    endpoint, handle, dict(op="read", path="payload", offset=MiB - 32, length=64)
                )
                self.assertEqual(reply["status"], "ok", reply)
                self.assertEqual(body, payload[MiB - 32 : MiB + 32])

    def test_raw_native_reader_holds_an_independent_commit_lock(self):
        from scripts.images.read_nydus import RafsReader

        with workspace() as (root, _):
            store, (handle,), _, payload = self.prepare(root)
            candidates = [
                p
                for p in root.glob("*/merged/" + handle.split(":")[1] + "/MERGED.json")
                if (p.parent / "environment").is_dir()
            ]
            self.assertEqual(len(candidates), 1)
            commit = candidates[0]
            with RafsReader(
                commit.parents[2],
                handle,
                Path(os.environ["PVISOR_RAFS_READER"]),
                root / "raw-reader",
            ) as image:
                with commit.open("rb") as lease:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertEqual(image.read_at("payload", 0, 64), payload[:64])
            with commit.open("rb") as lease:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)

    @unittest.skipUnless(os.environ.get("PVISOR_RAFS_DAEMON"), "requires product cache daemon")
    def test_authenticated_operator_generation_fencing_and_no_implicit_replay(self):
        with workspace() as (root, _):
            store, (handle,), blobs, _ = self.prepare(root)
            with TrackingOrigin(blobs) as origin:
                with N.node(root, store, origin, daemon=True, token="lifecycle-token") as (
                    endpoint,
                    _,
                ):
                    address = "unix://" + str(endpoint)
                    old = MANAGE.operate(address, handle, {"op": "stats"}, token="lifecycle-token")[
                        "node_generation"
                    ]
                    with self.assertRaisesRegex(RuntimeError, "permission_denied"):
                        MANAGE.operate(address, handle, {"op": "remove"}, token="wrong-token")
                    pin = MANAGE.operate(
                        address,
                        handle,
                        dict(op="lease", kind="checkpoint", owner="checkpoint-1", action="retain"),
                        token="lifecycle-token",
                    )
                    self.assertTrue(pin["dependency"]["retained"])
                with N.node(root, store, origin, daemon=True, token="lifecycle-token") as (
                    endpoint,
                    _,
                ):
                    address = "unix://" + str(endpoint)
                    stale, _ = N.rpc(
                        endpoint,
                        handle,
                        dict(op="lease", kind="checkpoint", owner="checkpoint-1", action="release"),
                        cache=True,
                        token="lifecycle-token",
                        generation=old,
                    )
                    self.assertEqual(stale["status"], "error")
                    state = MANAGE.operate(
                        address,
                        handle,
                        dict(op="lease", kind="checkpoint", owner="checkpoint-1", action="inspect"),
                        token="lifecycle-token",
                    )
                    self.assertTrue(state["dependency"]["retained"])
                    self.assertFalse(
                        MANAGE.operate(
                            address,
                            handle,
                            dict(op="trim", target_bytes=0),
                            token="lifecycle-token",
                        )["cache_trim"]["target_reached"]
                    )
