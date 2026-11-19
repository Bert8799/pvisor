from __future__ import annotations

import copy
import http.server
import json
import os
import threading
from unittest import mock

from tests.image_fixtures.image_format import PROBE


class IntegrityFixture:
    def read_range(self, daemon, bootstrap, blobs, blob_name, data, output, validate):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_HEAD(self):
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()

            def do_GET(self):
                value = self.headers.get("Range", "")
                if self.path != "/blobs/" + blob_name or not value.startswith("bytes="):
                    self.send_error(400)
                    return
                start, end = map(int, value[6:].split("-"))
                if not 0 <= start <= end < len(data):
                    self.send_error(416)
                    return
                body = data[start : end + 1]
                requests.append(len(body))
                self.send_response(206)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
                self.end_headers()
                self.wfile.write(body)

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        real_save = PROBE.save

        def save(path, value):
            if path.name == "nydusd.json":
                value = copy.deepcopy(value)
                value["backend"] = {
                    "type": "http-proxy",
                    "http-proxy": {
                        "addr": f"http://127.0.0.1:{server.server_port}",
                        "path": "/blobs",
                        "timeout": 5,
                    },
                }
                value["cache"]["validate"] = validate
            real_save(path, value)

        body, error = (None, None)
        try:
            with mock.patch.object(PROBE, "save", side_effect=save):
                with PROBE.mounted(daemon, bootstrap, blobs, output, 10) as mount:
                    try:
                        with (mount / "payload").open("rb") as stream:
                            body = stream.read(64)
                    except OSError as caught:
                        error = caught
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())
        cleanup = json.loads((output / "cleanup.json").read_text())
        self.assertTrue(cleanup["unmounted"], cleanup)
        self.assertEqual(cleanup["errors"], [])
        with self.assertRaises(ProcessLookupError):
            os.kill(cleanup["pid"], 0)
        return (body, error, requests)
