from __future__ import annotations

import hashlib
import json
import os
import tarfile
from decimal import Decimal
from pathlib import Path

from scripts.images import merge_nydus as MERGE
from scripts.images import publish_nydus as PUB
from tests.image_fixtures import image_merge as merge_tests


class CoreFixture:
    def prepare(self, root, layers):
        store, environment, tools, _ = merge_tests.MergeFixture.prepare(self, root, layers)
        handle = MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
        engine = Path(os.environ["PVISOR_RAFS_READER"]).resolve()
        probe = Path(os.environ["PVISOR_RAFS_CORE_PROBE"]).resolve()
        return (store, handle, engine, probe)

    def execute(self, root, inputs, file="a", alias=None, output="probe"):
        store, handle, engine, probe = inputs
        command = [str(probe), str(store), handle, str(engine), str(root / output), file]
        if alias is not None:
            command.append(alias)
        try:
            PUB.convert.run_tool(command, root / f"{output}.log", 120)
        except RuntimeError as error:
            raise RuntimeError((root / f"{output}.log").read_text()) from error
        report = json.loads((root / f"{output}.log").read_text())
        expected_path = root / output / "image-export-expected.json"
        if expected_path.exists():
            expected = json.loads(expected_path.read_text())
            fingerprint = expected["fingerprint"]
            with tarfile.open(root / output / "image-export.tar") as archive:
                entry = archive.getmember(expected["path"])
                self.assertEqual(entry.uid, fingerprint["uid"])
                self.assertEqual(entry.gid, fingerprint["gid"])
                self.assertEqual(entry.mode, fingerprint.get("mode", 0o777) & 0o7777)
                seconds, nanos = expected["mtime"]
                self.assertEqual(
                    Decimal(entry.pax_headers["mtime"]), Decimal(seconds) + Decimal(nanos) / 10**9
                )
                attributes = []
                for key, value in entry.pax_headers.items():
                    if key.startswith("SCHILY.xattr."):
                        name = key.removeprefix("SCHILY.xattr.").encode()
                        digest = hashlib.sha256(
                            value.encode("utf-8", "surrogateescape")
                        ).hexdigest()
                        attributes.append([list(name), digest])
                self.assertEqual(sorted(attributes), fingerprint["xattrs"]["entries"])
                if fingerprint["kind"] == "file":
                    self.assertEqual(
                        hashlib.sha256(archive.extractfile(entry).read()).hexdigest(),
                        fingerprint["sha256"],
                    )
                else:
                    self.assertTrue(entry.issym())
                    self.assertEqual(list(entry.linkname.encode()), fingerprint["target"])
                for member in archive:
                    self.assertFalse(member.name.startswith(".wh."))
                    self.assertFalse(
                        any(k.startswith("SCHILY.xattr.user.pvisor.") for k in member.pax_headers)
                    )
            report["logical_image_export"] = True
        return report
