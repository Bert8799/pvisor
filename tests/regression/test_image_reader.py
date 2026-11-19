from __future__ import annotations

import subprocess
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock

from scripts.images.read_nydus import RafsReader
from tests.fixtures import workspace
from tests.image_fixtures.image_reader import ReaderProcessFixture


class ReaderProcessTests(ReaderProcessFixture, unittest.TestCase):
    """Fake engines test framing/cleanup only; no claims about format integrity."""

    def test_bad_handshake_and_timeout_stop_and_reap_the_engine(self):
        for program in [
            "import time; time.sleep(60)",
            "import sys; sys.stdout.buffer.write((1048577).to_bytes(4, 'big'))",
            "import sys; sys.stdout.buffer.write((2).to_bytes(4, 'big') + b'{}')",
            "import sys; sys.stdout.buffer.write((8).to_bytes(4, 'big') + b'partial')",
        ]:
            with self.subTest(program=program), workspace() as (root, stack):
                store, handle, engine = self.fixture(root, stack, program)
                processes, popen = ([], subprocess.Popen)

                def capture(*args, **kwargs):
                    process = popen(*args, **kwargs)
                    processes.append(process)
                    return process

                with mock.patch.object(subprocess, "Popen", side_effect=capture):
                    with self.assertRaises((ValueError, RuntimeError, TimeoutError)):
                        RafsReader(store, handle, engine, root / "work", timeout=0.2)
                self.assertEqual(len(processes), 1)
                self.assertIsNotNone(processes[0].poll())
                self.assertTrue(processes[0].stdin.closed and processes[0].stdout.closed)

    def test_serialized_requests_preserve_framing_and_filesystem_errors_are_recoverable(self):
        program = "\nimport sys, json, hashlib\ninp, out = sys.stdin.buffer, sys.stdout.buffer\ndef frame(value):\n    raw = json.dumps(value).encode()\n    out.write(len(raw).to_bytes(4, 'big') + raw); out.flush()\nframe({'version': 1, 'handle': sys.argv[2]})\nwhile head := inp.read(4):\n    request = json.loads(inp.read(int.from_bytes(head, 'big')))\n    if request['path'] == 'missing':\n        frame({'status': 'error', 'errno': 2, 'message': 'missing'})\n    elif request['op'] == 'read':\n        body = b'data'\n        frame({'status': 'ok', 'length': 4, 'sha256': 'sha256:' + hashlib.sha256(body).hexdigest()})\n        out.write(body); out.flush()\n    else:\n        frame({'status': 'ok', 'node': {'path': request['path']}})\n"
        with workspace() as (root, stack):
            store, handle, engine = self.fixture(root, stack, program)
            with RafsReader(store, handle, engine, root / "work") as reader:
                process = reader.process
                with self.assertRaises(FileNotFoundError):
                    reader.stat("missing")
                with ThreadPoolExecutor(max_workers=4) as pool:
                    results = list(pool.map(lambda index: reader.stat(str(index)), range(12)))
                self.assertEqual(results, [{"path": str(index)} for index in range(12)])
                self.assertEqual(reader.read_at("file", 0, 4), b"data")
                for offset, length in [(-1, 1), (0, 1048577), (2**64 - 1, 1), (0, True)]:
                    with self.assertRaises(ValueError):
                        reader.read_at("file", offset, length)
            self.assertIsNotNone(process.poll())
            with self.assertRaisesRegex(RuntimeError, "closed"):
                reader.stat()

    def test_work_overlap_is_rejected_before_starting_an_engine(self):
        with workspace() as (root, stack):
            store, handle, engine = self.fixture(root, stack, "raise AssertionError('started')")
            with mock.patch.object(subprocess, "Popen") as popen:
                with self.assertRaisesRegex(ValueError, "disjoint"):
                    RafsReader(store, handle, engine, store / "reader-work")
                popen.assert_not_called()

    def test_request_timeout_and_corrupt_response_retire_the_engine(self):
        for fault in ("timeout", "checksum", "oversized"):
            program = "\nimport sys, json, time\nout, inp = sys.stdout.buffer, sys.stdin.buffer\ndef frame(value):\n    raw = json.dumps(value).encode(); out.write(len(raw).to_bytes(4, 'big') + raw); out.flush()\nframe({'version': 1, 'handle': sys.argv[2]})\nsize = int.from_bytes(inp.read(4), 'big'); inp.read(size)\n"
            program += {
                "timeout": "time.sleep(60)",
                "checksum": "frame({'status': 'ok', 'length': 4, 'sha256': 'sha256:' + '0' * 64}); out.write(b'data'); out.flush()",
                "oversized": "out.write((1048577).to_bytes(4, 'big')); out.flush()",
            }[fault]
            with self.subTest(fault=fault), workspace() as (root, stack):
                store, handle, engine = self.fixture(root, stack, program)
                with RafsReader(store, handle, engine, root / "work") as reader:
                    process = reader.process
                    reader.timeout = 0.2
                    with self.assertRaises((ValueError, TimeoutError)):
                        reader.read_at("file", 0, 4)
                    self.assertIsNotNone(process.poll())
                    with self.assertRaisesRegex(RuntimeError, "closed"):
                        reader.stat()
