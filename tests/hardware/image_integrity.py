from __future__ import annotations

import hashlib
import io
import os
import random
import tarfile
import unittest
from pathlib import Path

from tests.fixtures import workspace
from tests.image_fixtures.image_format import PROBE
from tests.image_fixtures.image_integrity import IntegrityFixture


@unittest.skipUnless(
    os.environ.get("PVISOR_NYDUS_IMAGE") and os.environ.get("PVISOR_NYDUSD"),
    "requires explicit Nydus tools, Linux FUSE and loopback HTTP access",
)
class NativeImageIntegrityTests(IntegrityFixture, unittest.TestCase):
    def test_range_validation_rejects_payload_and_digest_tampering(self):
        chunk_size = 1024 * 1024
        payload = random.Random(492).randbytes(8 * chunk_size)
        image_tool = Path(os.environ["PVISOR_NYDUS_IMAGE"]).resolve()
        daemon = Path(os.environ["PVISOR_NYDUSD"]).resolve()
        with workspace() as (root, _):
            source = root / "source.tar"
            with tarfile.open(source, "w", format=tarfile.PAX_FORMAT) as archive:
                for name in (".", "payload"):
                    item = tarfile.TarInfo(name)
                    item.type = tarfile.DIRTYPE if name == "." else tarfile.REGTYPE
                    item.mode = 493 if name == "." else 420
                    item.mtime = 1700000000
                    item.size = 0 if name == "." else len(payload)
                    archive.addfile(item, None if name == "." else io.BytesIO(payload))
            images = {}
            for toc in (False, True):
                directory = root / str(toc)
                directory.mkdir()
                blobs = directory / "blobs"
                blobs.mkdir()
                bootstrap = directory / "bootstrap"
                command = [
                    str(image_tool),
                    "create",
                    "--type",
                    "tar-rafs",
                    "--fs-version",
                    "6",
                    "--compressor",
                    "none",
                    "--digester",
                    "sha256",
                    "--chunk-size",
                    str(chunk_size),
                    "--bootstrap",
                    str(bootstrap),
                    "--blob-dir",
                    str(blobs),
                ]
                if toc:
                    command += ["--features", "blob-toc"]
                PROBE.run_tool(command + [str(source)], directory / "build.log", 30)
                blob = next(blobs.iterdir())
                images[toc] = (bootstrap, blobs, blob.name, blob.read_bytes())
            cases = [
                ("valid", True, True, None),
                ("missing-digests", False, True, "payload"),
                ("disabled-validation", True, False, "payload"),
                ("bad-payload", True, True, "payload"),
                ("bad-payload-and-digest", True, True, "both"),
            ]
            for name, toc, validate, corruption in cases:
                with self.subTest(name=name):
                    bootstrap, blobs, blob_name, original = images[toc]
                    pinned_digest = PROBE.digest_file(bootstrap)
                    data = bytearray(original)
                    if corruption:
                        offset = data.find(payload[:64])
                        self.assertGreaterEqual(offset, 0)
                        data[offset + 16] ^= 1
                    if corruption == "both":
                        digest = hashlib.sha256(payload[:chunk_size]).digest()
                        self.assertEqual(data.count(digest), 1)
                        offset = data.find(digest)
                        changed = bytearray(payload[:chunk_size])
                        changed[16] ^= 1
                        data[offset : offset + 32] = hashlib.sha256(changed).digest()
                    output = root / name
                    output.mkdir()
                    body, error, requests = self.read_range(
                        daemon, bootstrap, blobs, blob_name, bytes(data), output, validate
                    )
                    if name == "valid":
                        self.assertIsNone(error)
                        self.assertEqual(body, payload[:64])
                        self.assertLess(sum(requests), len(data))
                    elif name in ("missing-digests", "disabled-validation"):
                        self.assertIsNone(error)
                        self.assertIsNotNone(body)
                        self.assertNotEqual(body, payload[:64])
                    else:
                        self.assertIsNotNone(error, "corrupt data was accepted")
                        self.assertIsNone(body, "reader delivered corrupt bytes")
                    self.assertTrue(requests)
                    self.assertEqual(PROBE.digest_file(bootstrap), pinned_digest)
