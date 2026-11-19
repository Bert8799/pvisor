from __future__ import annotations

import contextlib
import hashlib
import http.server
import json
import os
import random
import select
import shutil
import socket
import subprocess
import threading
import time

from scripts.images import compose_nydus as ENV
from scripts.images import merge_nydus as MERGE
from scripts.images import stage_node as STAGE
from tests.image_fixtures import image_merge as merge_tests
from tests.image_fixtures.image_merge import D, F

TOOLS = ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY", "PVISOR_RAFS_READER")


def receive(stream, count):
    data = bytearray()
    while len(data) < count:
        part = stream.recv(count - len(data))
        if not part:
            raise EOFError("node closed the response")
        data.extend(part)
    return bytes(data)


def rpc(endpoint, handle, request, *, cache=False, token=None, generation=None):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(60)
        stream.connect(str(endpoint))
        call = {"handle": handle, "request": request}
        if cache:
            call = {
                "version": 2,
                "token": token,
                "request": {"op": "rafs", "generation": generation, **call},
            }
        raw = json.dumps(call).encode()
        stream.sendall(len(raw).to_bytes(4, "big") + raw)
        count = int.from_bytes(receive(stream, 4), "big")
        if not 0 < count <= 1024 * 1024:
            raise ValueError("invalid node frame")
        reply = json.loads(receive(stream, count))
        if cache and reply["status"] == "rafs":
            data = receive(stream, reply["length"])
            if reply["sha256"] != "sha256:" + hashlib.sha256(data).hexdigest():
                raise ValueError("bad cache body")
            reply = reply["frame"]
        else:
            data = receive(stream, reply.get("length", 0))
        if "sha256" in reply and reply["sha256"] != "sha256:" + hashlib.sha256(data).hexdigest():
            raise ValueError("bad native body")
        return reply, data


class Origin:
    """Controlled real Range server; records actual transmitted bytes."""

    def __init__(self, blobs):
        self.blobs = blobs
        self.requests = []
        self.fail_requests = False
        self.delay = 0
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_HEAD(self):
                data = owner.blobs.get(self.path[1:])
                if data is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()

            def do_GET(self):
                data = owner.blobs.get(self.path[1:])
                header = self.headers.get("Range", "")
                if data is None or not header.startswith("bytes="):
                    self.send_error(400)
                    return
                start, end = map(int, header[6:].split("-"))
                if not 0 <= start <= end < len(data):
                    self.send_error(416)
                    return
                if owner.fail_requests:
                    self.send_error(503)
                    return
                if owner.delay:
                    time.sleep(owner.delay)
                owner.requests.append((self.path, start, end))
                self.send_response(206)
                self.send_header("Content-Length", str(end - start + 1))
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
                self.end_headers()
                self.wfile.write(data[start : end + 1])

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)
        if self.thread.is_alive():
            raise RuntimeError("Range server leaked")

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"


@contextlib.contextmanager
def node(root, store, origin, *, cache_name="cache", daemon=False, token=None, **limits):
    directory = root / ("control-" + str(time.time_ns()))
    directory.mkdir(mode=0o700)
    endpoint = directory / "node.sock"
    config = directory / "node.json"
    options = dict(
        store=str(store),
        cache=str(root / cache_name),
        origin=origin.url,
        max_images=4,
        max_cache_bytes=64 * 1024 * 1024,
        workers=4,
    )
    options.update(limits)
    config.write_text(json.dumps(options))
    config.chmod(0o600)
    if daemon:
        options = json.loads(config.read_text())
        engine = directory / "reader"
        shutil.copyfile(os.environ["PVISOR_RAFS_READER"], engine)
        engine.chmod(0o700)
        options["reader"] = str(engine)
        config.write_text(json.dumps(options))
        command = [
            os.environ["PVISOR_RAFS_DAEMON"],
            "cache-worker",
            "--listen",
            "unix://" + str(endpoint),
            "--image-store",
            str(root / "oci-cache"),
            "--rafs-config",
            str(config),
        ]
    else:
        (root / cache_name).mkdir(mode=0o700, exist_ok=True)
        command = [os.environ["PVISOR_RAFS_READER"], "--node", str(config), str(endpoint)]
    with (directory / "stderr").open("wb") as log:
        env = dict(os.environ)
        env["MALLOC_ARENA_MAX"] = "2"
        env.pop("PVISOR_CACHE_TOKEN", None)
        if token is not None:
            env["PVISOR_CACHE_TOKEN"] = token
        process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, env=env
        )
        try:
            if not select.select([process.stdout], [], [], 15)[0]:
                raise RuntimeError("node readiness timed out")
            if daemon:
                ready = process.stdout.readline() == b"ready\n"
            else:
                raw = process.stdout.read(4)
                ready = bool(raw) and json.loads(
                    process.stdout.read(int.from_bytes(raw, "big"))
                ) == {"version": 1, "node": True}
            if not ready:
                raise RuntimeError((directory / "stderr").read_text())
            yield endpoint, process
        finally:
            process.stdin.close()
            if not daemon:
                process.terminate()
            try:
                process.wait(15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
            process.stdout.close()


class NodeFixture:
    def prepare(self, root, *, layers=False):
        payload = random.Random(184).randbytes(8 * 1024 * 1024)
        entries = [[(".", D, b""), ("payload", F, payload)]]
        if layers:
            entries.append([("extra", F, b"independent-tool")])
        store, environment, tools, _ = merge_tests.MergeFixture.prepare(self, root, entries)
        handle = MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
        handles = [handle]
        if layers:
            _, recipe = ENV.open_environment(store, environment)
            base = ENV.compose(store, recipe["base"], [])
            handles.append(MERGE.merge(store, base, tools[0], tools[2], root / "base-merge"))
        blobs = {}
        for handle in handles:
            STAGE.stage(store, handle, root / "node-store")
            bundle, record = MERGE.open_merged(store, handle)
            blobs.update({k: (bundle / "blobs" / k).read_bytes() for k in record["blobs"]})
        self.assertEqual(STAGE.stage(store, handles[0], root / "node-store"), handles[0])
        return (root / "node-store", handles, blobs, payload)

    def opened(self, endpoint, store, handle, **kwargs):
        commit = store / "merged" / MERGE.revision(handle) / "MERGED.json"
        info = commit.stat()
        reply, data = rpc(
            endpoint,
            handle,
            dict(op="open", commit_device=info.st_dev, commit_inode=info.st_ino),
            **kwargs,
        )
        self.assertEqual(reply["status"], "ok", reply)
        self.assertEqual(data, b"")
        return reply
