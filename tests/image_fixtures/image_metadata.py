from __future__ import annotations

from tests.image_fixtures import image_node as N
from tests.image_fixtures.image_merge import D, F, H, S


class MetadataFixture:
    def prepare(self, root):
        entries = [
            (".", D, b""),
            ("dir", D, b""),
            ("dir/file", F, b"payload"),
            ("dir/alias", H, "dir/file"),
            ("dir/link", S, "file"),
        ]
        store, environment, tools, _ = N.merge_tests.MergeFixture.prepare(self, root, [entries])
        handle = N.MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
        N.STAGE.stage(store, handle, root / "node-store")
        bundle, record = N.MERGE.open_merged(store, handle)
        return (
            root / "node-store",
            handle,
            {k: (bundle / "blobs" / k).read_bytes() for k in record["blobs"]},
        )
