from __future__ import annotations

import concurrent.futures
import contextlib
import json
import os
import shutil
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

from scripts.images import manage_node as MANAGE
from scripts.images import merge_nydus as MERGE
from scripts.images import stage_node as STAGE
from tests.image_fixtures import image_node as N

REQUIRED = (
    "PVISOR_RAFS_DAEMON",
    "PVISOR_RAFS_READER",
    "PVISOR_RAFS_GUEST",
    "PVISOR_RAFS_CASE_INPUTS",
    "PVISOR_RAFS_CGROUP_ROOT",
)

BOOTSTRAP = r"""
import socket, threading, json, os, sys, time
from pathlib import Path
assert not Path('/instance-marker').exists(), 'private root write leaked'
finished = threading.Event()
info = dict(argv=sys.argv[1:], secret=os.environ['TASK_SECRET'], cwd=os.getcwd(),
            instance=os.environ.get('PVISOR_INSTANCE_ID'), entropy=os.urandom(32).hex(),
            cache_credentials_leaked='PVISOR_CACHE_TOKEN' in os.environ,
            host_source_visible=Path(os.environ['HOST_PROBE']).exists())
Path('/instance-marker').write_text(info['secret'])
def serve(port):
    listener = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    listener.bind((socket.VMADDR_CID_ANY, port)); listener.listen(16)
    while True:
        stream, _ = listener.accept()
        with stream:
            stream.settimeout(10); request = b''
            while b'\r\n\r\n' not in request:
                part = stream.recv(8192)
                if not part: break
                request += part
            path = request.split(b' ', 2)[1]
            body = json.dumps(info if path == b'/info' else {'initialized': True}).encode()
            stream.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: '
                           + str(len(body)).encode() + b'\r\nConnection: close\r\n\r\n' + body)
        if path == b'/finish': finished.set()
for port in (44772, 18080):
    threading.Thread(target=serve, args=(port,), daemon=True).start()
finished.wait()
raise SystemExit(23)
"""


class DaemonFixture:
    def exercise(self, root, group, inputs):
        runtime = root / "runtime"
        runtime.mkdir(mode=448)
        for source, name in [
            (os.environ["PVISOR_RAFS_DAEMON"], "pvisor-daemon"),
            (os.environ["PVISOR_RAFS_GUEST"], "pvisor-guest"),
            (inputs["firmware"], "libkrunfw.so.5"),
        ]:
            shutil.copyfile(source, runtime / name)
            (runtime / name).chmod(448)
        store = root / "node-store"
        handle = inputs["rafs_handle"]
        STAGE.stage(Path(inputs["rafs_store"]), handle, store)
        bundle, record = MERGE.open_merged(Path(inputs["rafs_store"]), handle)
        blobs = {key: (bundle / "blobs" / key).read_bytes() for key in record["blobs"]}
        commit = store / "merged" / MERGE.revision(handle) / "MERGED.json"
        work = runtime / "work"
        work.mkdir(mode=448)
        catalog = root / "catalog"
        selector = root / "selector.json"
        selector.write_text(
            json.dumps(dict(store=str(store), handle=handle, endpoint="unix:///not-contacted.sock"))
        )
        result = subprocess.run(
            [
                str(runtime / "pvisor-daemon"),
                "template-prepare",
                "--rafs-node",
                str(selector),
                "--guest",
                str(runtime / "pvisor-guest"),
                "--catalog",
                str(catalog),
                "--name",
                "fixture",
                "--cpus",
                "2",
                "--memory-mib",
                "256",
                "--library-dir",
                str(runtime),
                "--work-dir",
                str(work),
            ],
            env=dict(os.environ, PVISOR_KRUN_ENOMEM_WORKAROUND="0"),
            capture_output=True,
            text=True,
            timeout=100,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        version = json.loads(result.stdout)
        self.assertEqual(version["spec"]["environment"]["handle"], handle)
        self.assertEqual(commit.stat().st_nlink, 2)
        self.assertEqual(list(work.iterdir()), [])
        images = runtime / "images"
        images.mkdir(mode=448)
        key, token = (uuid.uuid4().hex, uuid.uuid4().hex)
        host_probe = root / "host-only-marker"
        host_probe.write_text(uuid.uuid4().hex)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            address = f"127.0.0.1:{sock.getsockname()[1]}"
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def api(path="", method="GET", body=None, expected=200):
            request = urllib.request.Request(
                f"http://{address}/v1/sandboxes{path}",
                data=None if body is None else json.dumps(body).encode(),
                method=method,
                headers={"OPEN-SANDBOX-API-KEY": key, "Content-Type": "application/json"},
            )
            try:
                with opener.open(request, timeout=110) as response:
                    code, raw = (response.status, response.read())
            except urllib.error.HTTPError as error:
                code, raw = (error.code, error.read())
            self.assertEqual(code, expected, raw)
            return json.loads(raw) if raw else None

        @contextlib.contextmanager
        def frontend():
            with (root / "daemon.log").open("ab") as log:
                process = subprocess.Popen(
                    [
                        str(runtime / "pvisor-daemon"),
                        "serve",
                        "--sandbox",
                        "--images-dir",
                        str(images),
                        "--cgroup-root",
                        str(group),
                        "--state",
                        str(runtime / "state"),
                        "--listen",
                        address,
                        "--cpu-millis",
                        "8000",
                    ],
                    env=dict(os.environ, OPEN_SANDBOX_API_KEY=key, PVISOR_CACHE_TOKEN=token),
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=log,
                )
                try:
                    deadline = time.monotonic() + 20
                    while True:
                        self.assertIsNone(process.poll(), (root / "daemon.log").read_text())
                        try:
                            api()
                            break
                        except (ConnectionError, urllib.error.URLError):
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(0.02)
                    yield
                finally:
                    if process.poll() is None:
                        process.send_signal(signal.SIGTERM)
                    try:
                        process.wait(20)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                        raise

        def create(image, secret):
            reply = api(
                method="POST",
                body=dict(
                    image=dict(uri=image),
                    entrypoint=[secret],
                    env=dict(TASK_SECRET=secret, HOST_PROBE=str(host_probe)),
                    resourceLimits=dict(cpu="2", memory="256Mi"),
                ),
                expected=202,
            )
            return reply["id"]

        def info(identity, secret):
            result = api(f"/{identity}/proxy/44772/info")
            self.assertEqual(result["secret"], secret)
            self.assertEqual(result["argv"], [secret])
            self.assertEqual(result["cwd"], "/tmp")
            self.assertFalse(result["cache_credentials_leaked"])
            self.assertFalse(result["host_source_visible"])
            return result

        with N.Origin(blobs) as origin:
            for iteration in range(2):
                with N.node(
                    root, store, origin, daemon=True, token=token, max_cache_bytes=256 * 1024 * 1024
                ) as (endpoint, _):
                    source = dict(
                        store=str(store), handle=handle, endpoint="unix://" + str(endpoint)
                    )
                    for name in ["cold", "template"]:
                        manifest = dict(
                            rafs_node=source,
                            entrypoint=["/usr/bin/python3", "-I", "-B", "-c", BOOTSTRAP],
                            cwd="/tmp",
                            library_dir=str(runtime),
                        )
                        if name == "template":
                            manifest["template"] = dict(
                                catalog=str(catalog),
                                name="fixture",
                                image_sha256=version["spec"]["image_sha256"],
                            )
                        path = images / (name + ".json")
                        path.write_text(json.dumps(manifest))
                        path.chmod(384)
                    with frontend():
                        if iteration == 0:
                            cold = create("cold", "cold")
                            self.assertIsNone(info(cold, "cold")["instance"])
                            api("/" + cold, method="DELETE", expected=204)
                        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                            ids = list(
                                pool.map(lambda s: create("template", s), ["first", "second"])
                            )
                        first, second = [info(i, s) for i, s in zip(ids, ["first", "second"])]
                        self.assertTrue(first["instance"] and second["instance"])
                        self.assertNotEqual(first["instance"], second["instance"])
                        self.assertNotEqual(first["entropy"], second["entropy"])
                        for identity in ids:
                            api(f"/{identity}/pause", method="POST", expected=202)
                            api(f"/{identity}/resume", method="POST", expected=202)
                    with frontend():
                        for identity, secret in zip(ids, ["first", "second"]):
                            info(identity, secret)
                            if iteration == 0 and secret == "first":
                                api(f"/{identity}/proxy/44772/finish")
                                deadline = time.monotonic() + 20
                                while True:
                                    records = []
                                    for path in (runtime / "state" / identity / "run").rglob(
                                        "run-bundle.json"
                                    ):
                                        try:
                                            records.append(json.loads(path.read_text())["run"])
                                        except (FileNotFoundError, json.JSONDecodeError):
                                            pass
                                    terminal = [
                                        r for r in records if r["state"] in ("completed", "failed")
                                    ]
                                    if terminal:
                                        self.assertEqual(terminal[0]["exit_code"], 23, terminal[0])
                                        break
                                    self.assertLess(
                                        time.monotonic(), deadline, "guest did not publish its exit"
                                    )
                                    time.sleep(0.02)
                            api("/" + identity, method="DELETE", expected=204)
                    self.assertFalse([p for p in group.iterdir() if p.is_dir()])
                    trimmed = MANAGE.operate(
                        source["endpoint"], handle, dict(op="trim", target_bytes=0), token=token
                    )
                    self.assertFalse(trimmed["cache_trim"]["target_reached"])
                    with self.assertRaises(OSError):
                        MANAGE.operate(source["endpoint"], handle, dict(op="remove"), token=token)
                    self.assertEqual(commit.stat().st_nlink, 2)
        self.assertGreater(len(origin.requests), 0)
