from __future__ import annotations

import io
import os
import tarfile
from pathlib import Path

from scripts.images import compose_nydus as ENV
from scripts.images import publish_nydus as PUB
from tests.image_fixtures.image_composition import component
from tests.image_fixtures.image_format import layout


def changeset(path, entries, *, mode=0o751, uid=1234, time="1700000000.123456789"):
    """Entries are name, tar type and bytes/link target; every attr is explicit."""
    with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as archive:
        for name, kind, data in entries:
            item = tarfile.TarInfo(name)
            item.type, item.uid, item.gid = kind, uid, uid + 1
            item.mode = mode if kind == tarfile.DIRTYPE else 0o644
            item.pax_headers = {"mtime": time, "SCHILY.xattr.user.test": "inherited"}
            stream = None
            if kind == tarfile.REGTYPE:
                item.size, stream = len(data), io.BytesIO(data)
            elif kind in (tarfile.LNKTYPE, tarfile.SYMTYPE):
                item.linkname = data
            archive.addfile(item, stream)
    return path


D, F, H, S = tarfile.DIRTYPE, tarfile.REGTYPE, tarfile.LNKTYPE, tarfile.SYMTYPE


def cases():
    return [
        (
            [(".", D, b""), ("dir", D, b""), ("dir/old", F, b"old")],
            [("dir/new", F, b"new"), ("new/nested/file", F, b"data")],
        ),
        (
            [
                (".", D, b""),
                ("dir", D, b""),
                ("dir/old", F, b"old"),
                ("gone", F, b"gone"),
                ("again", D, b""),
                ("again/old", F, b"old"),
            ],
            [
                ("dir/new", F, b"new"),
                ("dir/.wh..wh..opq", F, b""),
                (".wh.gone", F, b""),
                ("again/new", F, b"new"),
                (".wh.again", F, b""),
                (".wh.absent", F, b""),
            ],
        ),
        (
            [
                (".", D, b""),
                ("a", F, b"old"),
                ("alias", H, "a"),
                ("kept", H, "a"),
                ("dir", D, b""),
                ("dir/child", F, b"old"),
                ("file", F, b"old"),
                ("link", S, "dir"),
                ("to-link", D, b""),
                ("to-link/old", F, b"old"),
            ],
            [
                ("a", F, b"new"),
                ("a2", H, "a"),
                ("equal", F, b"new"),
                ("dir", F, b"replacement"),
                ("file", D, b""),
                ("file/child", F, b"new"),
                ("link", D, b""),
                ("link/child", F, b"new"),
                ("to-link", S, "file"),
            ],
        ),
        (
            [(".", D, b""), ("dir", D, b""), ("dir/keep", F, b"keep")],
            [(".", D, b""), ("dir", D, b"")],
        ),
        ([(".", D, b"")], [(".wh.absent", F, b"")]),
    ]


class MergeFixture:
    def prepare(self, root, entries):
        tools = [
            Path(os.environ[name]).resolve()
            for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
        ]
        store, handles, sources = (root / "store", [], [])
        for index, items in enumerate(entries):
            source = changeset(
                root / f"layer-{index}.tar", items, mode=489 - index, uid=1234 + index
            )
            sources.append(source)
            oci = layout(root / f"oci-{index}", [source])
            handles.append(
                PUB.publish(
                    oci,
                    "linux/amd64",
                    *tools,
                    store,
                    root / f"publish-{index}",
                    **{"layer_index": 0} if index else {},
                )
            )
        environment = ENV.compose(
            store,
            handles[0],
            [component(handle, name=f"layer-{index}") for index, handle in enumerate(handles[1:])],
        )
        return (store, environment, tools, sources)
