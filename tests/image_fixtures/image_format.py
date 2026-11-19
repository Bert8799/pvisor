from __future__ import annotations

import gzip
import hashlib
import importlib.util
import io
import json
import os
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

SPEC = importlib.util.spec_from_file_location("image_probe", ROOT / "scripts/images/probe_nydus.py")

PROBE = importlib.util.module_from_spec(SPEC)

SPEC.loader.exec_module(PROBE)


def layer(path: Path, entries: list[tuple[str, bytes, bytes | str]], *, ns: bool = False) -> Path:
    with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as archive:
        for name, kind, value in entries:
            member = tarfile.TarInfo(name)
            member.type = kind
            member.uid = 1234
            member.gid = 2345
            member.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
            member.mtime = 1700000000
            if ns:
                member.pax_headers["mtime"] = "1700000000.123456789"
            if kind == tarfile.REGTYPE:
                member.size = len(value)
                archive.addfile(member, io.BytesIO(value))
            else:
                if kind in (tarfile.LNKTYPE, tarfile.SYMTYPE):
                    member.linkname = value
                archive.addfile(member)
    return path


def layout(root: Path, layers: list[Path]) -> Path:
    root.mkdir()
    blobs = root / "blobs/sha256"
    blobs.mkdir(parents=True)

    def store(data, media_type):
        digest = hashlib.sha256(data).hexdigest()
        (blobs / digest).write_bytes(data)
        return {"mediaType": media_type, "digest": "sha256:" + digest, "size": len(data)}

    config = store(
        json.dumps(
            {
                "os": "linux",
                "architecture": "amd64",
                "rootfs": {"type": "layers", "diff_ids": [PROBE.digest_file(p) for p in layers]},
            }
        ).encode(),
        "application/vnd.oci.image.config.v1+json",
    )
    descriptors = []
    for index, path in enumerate(layers):
        data = path.read_bytes()
        media = "application/vnd.oci.image.layer.v1.tar"
        if index % 2:
            data = gzip.compress(data, mtime=0)
            media += "+gzip"
        descriptors.append(store(data, media))
    manifest = store(
        json.dumps({"schemaVersion": 2, "config": config, "layers": descriptors}).encode(),
        "application/vnd.oci.image.manifest.v1+json",
    )
    manifest["platform"] = {"os": "linux", "architecture": "amd64"}
    (root / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
    (root / "index.json").write_text(json.dumps({"schemaVersion": 2, "manifests": [manifest]}))
    return root


class FormatFixture:
    def run_probe(self, root: Path, source: Path, name: str) -> dict:
        output = root / name
        report = PROBE.probe(
            source,
            "linux/amd64",
            Path(os.environ["PVISOR_NYDUS_IMAGE"]),
            Path(os.environ["PVISOR_NYDUSD"]),
            output,
            timeout=10,
        )
        self.assertIsNone(report["error"], report)
        cleanup = json.loads((output / "cleanup.json").read_text())
        self.assertTrue(cleanup["unmounted"], cleanup)
        self.assertEqual(cleanup["errors"], [])
        with self.assertRaises(ProcessLookupError):
            os.kill(cleanup["pid"], 0)
        if report["error"]:
            report["daemon_log"] = (output / "nydusd.log").read_text()
        return report
