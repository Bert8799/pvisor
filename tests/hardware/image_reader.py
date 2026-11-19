from __future__ import annotations

import base64
import errno
import hashlib
import os
import random
import shutil
import unittest

from scripts.images import merge_nydus as MERGE
from scripts.images import publish_nydus as PUB
from scripts.images.read_nydus import RafsReader
from tests.fixtures import workspace
from tests.image_fixtures.image_merge import D, F, S, cases
from tests.image_fixtures.image_reader import ReaderFixture


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in (
                "PVISOR_NYDUS_IMAGE",
                "PVISOR_NYDUSD",
                "PVISOR_RAFS_INVENTORY",
                "PVISOR_RAFS_READER",
            )
        )
    ),
    "requires pinned patched converter, inventory helper and demand reader",
)
class NativeImageReaderTests(ReaderFixture, unittest.TestCase):
    def test_native_filesystem_errno_survives_and_reader_remains_usable(self):
        with workspace() as (root, _):
            store, handle, engine, _ = self.prepare(
                root, [[(".", D, b""), ("file", F, b"data"), ("link", S, "file")]]
            )
            with RafsReader(store, handle, engine, root / "reader") as reader:
                for path, expected_errno in [
                    ("absent", errno.ENOENT),
                    ("absent/child", errno.ENOENT),
                    ("file/child", errno.ENOTDIR),
                    ("link/child", errno.ENOTDIR),
                ]:
                    with self.subTest(path=path), self.assertRaises(OSError) as raised:
                        reader.stat(path)
                    self.assertEqual(raised.exception.errno, expected_errno)
                for path, expected_errno in [("", errno.EISDIR), ("link", errno.ELOOP)]:
                    with self.subTest(path=path), self.assertRaises(OSError) as raised:
                        reader.read_at(path, 0, 1)
                    self.assertEqual(raised.exception.errno, expected_errno)
                self.assertEqual(reader.read_at("file", 0, 4), b"data")

    def test_paged_walk_and_reads_preserve_qualified_tree_and_inode_links(self):
        for case, entries in enumerate(cases()):
            with self.subTest(case=case), workspace() as (root, _):
                store, handle, engine, sources = self.prepare(root, entries)
                expected = PUB.convert.inventory(sources)
                tree, groups = ({}, {})
                with RafsReader(store, handle, engine, root / "reader") as reader:
                    process = reader.process
                    pending = [""]
                    while pending:
                        name = pending.pop()
                        node = reader.stat(name)
                        entry = {key: node[key] for key in ("kind", "mode", "uid", "gid")}
                        entry["mtime_ns"] = node["mtime"] * 1000000000 + node["mtime_nsec"]
                        entry["xattrs"] = {
                            key: base64.b64encode(bytes(value)).decode("ascii")
                            for key, value in node["xattrs"].items()
                        }
                        if node["kind"] == "directory":
                            offset = 0
                            while True:
                                page = reader.entries(name, offset, 1)
                                pending.extend(
                                    (
                                        (name + "/" if name else "") + child
                                        for child in page["names"]
                                    )
                                )
                                if page["next_offset"] is None:
                                    break
                                self.assertGreater(page["next_offset"], offset)
                                offset = page["next_offset"]
                        elif node["kind"] == "file":
                            body = reader.read_at(name, 0, 1024 * 1024)
                            entry.update(size=node["size"], sha256=PUB.sha256(body))
                            groups.setdefault(node["inode"], []).append((name, node["nlink"]))
                        else:
                            entry["target"] = node["target"]
                            with self.assertRaises(OSError):
                                reader.read_at(name, 0, 1)
                            with self.assertRaises(OSError):
                                reader.stat(name + "/child")
                        tree[name] = entry
                    for names in groups.values():
                        for name, nlink in names:
                            tree[name]["hardlinks"] = sorted((name for name, _ in names))
                            self.assertEqual(nlink, len(names))
                    self.assertEqual(PUB.convert.differences(expected, tree), [])
                    for invalid in ("../escape", "/absolute", "dir/../file", "dir//file", "./file"):
                        with self.subTest(path=invalid), self.assertRaises(OSError):
                            reader.stat(invalid)
                    self.assertEqual(reader.stat()["kind"], "directory")
                    with self.assertRaises(OSError):
                        reader._result({"op": "write", "path": "new", "data": "forbidden"})
                    self.assertEqual(reader.stat()["kind"], "directory")
                self.assertIsNotNone(process.poll())

    def test_open_does_not_hash_payload_and_corruption_never_returns_bytes(self):
        chunk = 1024 * 1024
        payload = random.Random(291).randbytes(3 * chunk)
        with workspace() as (root, _):
            store, handle, engine, _ = self.prepare(
                root, [[(".", D, b""), ("payload", F, payload)]]
            )
            bundle, record = MERGE.open_merged(store, handle)
            blob = bundle / "blobs" / next(iter(record["blobs"]))
            original = blob.read_bytes()
            for fault in ("payload", "payload-and-digest"):
                with self.subTest(fault=fault):
                    data = bytearray(original)
                    offset = data.find(payload[chunk : chunk + 64])
                    self.assertGreaterEqual(offset, 0)
                    data[offset + 16] ^= 1
                    if fault == "payload-and-digest":
                        digest = hashlib.sha256(payload[chunk : 2 * chunk]).digest()
                        self.assertEqual(data.count(digest), 1)
                        digest_offset = data.find(digest)
                        changed = bytearray(payload[chunk : 2 * chunk])
                        changed[16] ^= 1
                        data[digest_offset : digest_offset + 32] = hashlib.sha256(changed).digest()
                    blob.unlink()
                    blob.write_bytes(data)
                    if fault == "payload":
                        with RafsReader(store, handle, engine, root / fault) as reader:
                            self.assertEqual(reader.stat("payload")["size"], len(payload))
                            self.assertEqual(reader.read_at("payload", 0, 64), payload[:64])
                            self.assertEqual(reader.read_at("payload", len(payload), 64), b"")
                            with self.assertRaises(OSError):
                                reader.read_at("payload", chunk, 64)
                            with self.assertRaises(OSError):
                                reader.read_at("payload", chunk - 32, 64)
                    else:
                        with self.assertRaises((OSError, RuntimeError)):
                            with RafsReader(store, handle, engine, root / fault) as reader:
                                reader.read_at("payload", chunk, 64)

    def test_corrupt_bootstrap_and_symlinked_payload_are_rejected(self):
        with workspace() as (root, _):
            store, handle, engine, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            bundle, record = MERGE.open_merged(store, handle)
            bootstrap = bundle / "metadata/image.bootstrap"
            original = bootstrap.read_bytes()
            bootstrap.unlink()
            changed = bytearray(original)
            changed[-1] ^= 1
            bootstrap.write_bytes(changed)
            with self.assertRaises(RuntimeError):
                RafsReader(store, handle, engine, root / "bad-bootstrap")
            bootstrap.write_bytes(original)
            blob = bundle / "blobs" / next(iter(record["blobs"]))
            other = root / "payload"
            other.write_bytes(blob.read_bytes())
            blob.unlink()
            blob.symlink_to(other)
            with self.assertRaises(RuntimeError):
                RafsReader(store, handle, engine, root / "bad-link")

    def test_rebound_metadata_cannot_disable_filesystem_chunk_validation(self):
        with workspace() as (root, _):
            store, handle, engine, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            original, record = MERGE.open_merged(store, handle)
            data = bytearray((original / "metadata/image.bootstrap").read_bytes())
            offset, digest_flag = (1024 + 128, 256)
            flags = int.from_bytes(data[offset : offset + 8], "little")
            self.assertTrue(flags & digest_flag)
            data[offset : offset + 8] = (flags & ~digest_flag).to_bytes(8, "little")
            record["bootstrap"] = {"sha256": PUB.sha256(data), "size": len(data)}
            raw = PUB.encoded(record)
            forged = MERGE.PREFIX + hashlib.sha256(raw).hexdigest()
            bundle = store / "merged" / MERGE.revision(forged)
            shutil.copytree(original, bundle)
            (bundle / "MERGED.json").unlink()
            (bundle / "MERGED.json").write_bytes(raw)
            (bundle / "metadata/image.bootstrap").unlink()
            (bundle / "metadata/image.bootstrap").write_bytes(data)
            with self.assertRaises(RuntimeError):
                RafsReader(store, forged, engine, root / "disabled-validation")
            self.assertIn(
                "mandatory SHA-256", (root / "disabled-validation/reader.log").read_text()
            )
