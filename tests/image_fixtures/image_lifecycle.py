from __future__ import annotations

import random

from tests.image_fixtures import image_node as N

MiB = 1024 * 1024


class LifecycleFixture:
    prepare = N.NodeFixture.prepare
    opened = N.NodeFixture.opened

    def call(self, endpoint, handle, op, **fields):
        reply, body = N.rpc(endpoint, handle, dict(op=op, **fields))
        self.assertEqual(reply["status"], "ok", reply)
        self.assertEqual(body, b"")
        return reply

    def lease(self, endpoint, handle, kind, owner, action="retain"):
        return self.call(endpoint, handle, "lease", kind=kind, owner=owner, action=action)[
            "dependency"
        ]

    def trim(self, endpoint, handle):
        return self.call(endpoint, handle, "trim", target_bytes=0)["cache_trim"]

    def commit(self, store, handle):
        return store / "merged" / handle.split(":")[1] / "MERGED.json"

    def prepare_seed(self, root, seed, destination):
        root.mkdir(mode=448)
        payload = random.Random(seed).randbytes(3 * MiB)
        store, environment, tools, _ = N.merge_tests.MergeFixture.prepare(
            self, root, [[(".", N.D, b""), ("payload", N.F, payload)]]
        )
        handle = N.MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
        N.STAGE.stage(store, handle, destination)
        bundle, record = N.MERGE.open_merged(store, handle)
        return (handle, {k: (bundle / "blobs" / k).read_bytes() for k in record["blobs"]}, payload)
