from __future__ import annotations

import hashlib
import json
import socket
import sys
import threading
import unittest
import uuid

from scripts.images import manage_node as MANAGE
from tests.fixtures import workspace
from tests.image_fixtures import image_node as N


@unittest.skipUnless(sys.platform == "linux", "same-UID Linux Unix peer credentials")
class ManagementTransportTests(unittest.TestCase):
    def test_disconnect_after_effect_does_not_replay_management(self):
        with workspace() as (root, _):
            address = root / "control.sock"
            seen = []
            errors = []
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(address))
                address.chmod(384)
                server.listen()
                server.settimeout(5)

                def peer():
                    try:
                        for index in range(2):
                            stream, _ = server.accept()
                            with stream:
                                stream.settimeout(5)
                                size = int.from_bytes(N.receive(stream, 4), "big")
                                seen.append(json.loads(N.receive(stream, size)))
                                if index == 0:
                                    frame = dict(
                                        status="rafs",
                                        length=0,
                                        sha256="sha256:" + hashlib.sha256(b"").hexdigest(),
                                        frame=dict(status="ok", node_generation=str(uuid.uuid4())),
                                    )
                                    raw = json.dumps(frame).encode()
                                    stream.sendall(len(raw).to_bytes(4, "big") + raw)
                    except BaseException as error:
                        errors.append(error)

                thread = threading.Thread(target=peer)
                thread.start()
                try:
                    with self.assertRaises(EOFError):
                        MANAGE.operate(
                            "unix://" + str(address),
                            "pvisor-rafs-merged-v1:" + "a" * 64,
                            dict(op="lease", kind="template", owner="version-1", action="retain"),
                            token="test-token",
                        )
                finally:
                    thread.join(6)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual([c["request"]["request"]["op"] for c in seen], ["stats", "lease"])
                self.assertEqual([c["version"] for c in seen], [2, 2])
                self.assertEqual(seen[0]["token"], "test-token")
                self.assertIsNotNone(seen[1]["request"]["generation"])

    def test_shared_socket_is_rejected_before_connecting(self):
        with workspace() as (root, _), socket.socket(socket.AF_UNIX) as server:
            address = root / "control.sock"
            server.bind(str(address))
            address.chmod(438)
            with self.assertRaises(PermissionError):
                MANAGE.operate(
                    "unix://" + str(address), "pvisor-rafs-merged-v1:" + "a" * 64, {"op": "remove"}
                )
