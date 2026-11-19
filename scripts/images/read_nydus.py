#!/usr/bin/env python3
"""Own a persistent authenticated RAFS reader for a pinned local merged image."""

from __future__ import annotations

import argparse
import json
import math
import os
import select
import subprocess
import sys
import threading
import time
from pathlib import Path

if __package__:
    from . import merge_nydus as merged
    from . import publish_nydus as images
else:
    import merge_nydus as merged
    import publish_nydus as images

MAX_FRAME = 1024 * 1024


class RafsReader:
    """Single-owner engineering reader; serialize calls and close before work cleanup.

    Only metadata is authenticated at open; payload chunks are authenticated on
    demand by the native engine. The store must remain private and immutable for
    the owner's lifetime. No cache GC or VM handoff/lease is provided here.
    """

    def __init__(self, store: Path, handle: str, engine: Path, work: Path, timeout: float = 60):
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be positive and finite")
        revision = merged.revision(handle)
        root = images.store_root(store, create=False)
        images.directory(root / "merged", create=False)
        work = work.resolve()
        canonical = root.resolve()
        if canonical == work or canonical in work.parents or work in canonical.parents:
            raise ValueError("work directory and store must be disjoint")
        self.timeout, self.process, self.log = timeout, None, None
        self.lock = threading.Lock()
        self.handle = handle
        engine = engine.resolve()
        work.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.log = (work / "reader.log").open("wb")
        try:
            self.process = subprocess.Popen(
                [str(engine), str(root / "merged" / revision), handle, str(work / "cache")],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self.log,
                start_new_session=True,
            )
            for stream in (self.process.stdin, self.process.stdout):
                os.set_blocking(stream.fileno(), False)
            greeting = self._frame(time.monotonic() + timeout)
            if greeting != {"version": 1, "handle": handle}:
                raise ValueError("reader did not bind the requested immutable image")
        except BaseException:
            self.close()
            raise

    def _transfer(self, data: bytes | int, deadline: float) -> bytes:
        reading = isinstance(data, int)
        stream = self.process.stdout if reading else self.process.stdin
        count, result = 0, bytearray()
        length = data if reading else len(data)
        while count < length:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("RAFS reader deadline exceeded")
            ready = select.select(
                [stream] if reading else [], [] if reading else [stream], [], remaining
            )
            if not ready[0] and not ready[1]:
                raise TimeoutError("RAFS reader deadline exceeded")
            try:
                if reading:
                    chunk = os.read(stream.fileno(), min(length - count, 128 * 1024))
                    if not chunk:
                        raise RuntimeError("RAFS reader closed its output; see reader.log")
                    result.extend(chunk)
                    count += len(chunk)
                else:
                    count += os.write(stream.fileno(), data[count : count + 128 * 1024])
            except BlockingIOError:
                continue
        return bytes(result)

    def _frame(self, deadline: float) -> dict:
        size = int.from_bytes(self._transfer(4, deadline), "big")
        if not 0 < size <= MAX_FRAME:
            raise ValueError("invalid reader frame size")
        result = json.loads(self._transfer(size, deadline))
        if not isinstance(result, dict):
            raise ValueError("invalid reader response")
        return result

    def _request(self, request: dict, *, body=False):
        with self.lock:
            if self.process is None:
                raise RuntimeError("reader is closed")
            raw = images.encoded(request)
            if len(raw) > MAX_FRAME:
                raise ValueError("request too large")
            deadline = time.monotonic() + self.timeout
            try:
                self._transfer(len(raw).to_bytes(4, "big") + raw, deadline)
                response = self._frame(deadline)
                if response.get("status") == "error":
                    images.keys(response, ("status", "errno", "message"))
                    if (
                        type(response["errno"]) is not int
                        or not 0 < response["errno"] <= 4095
                        or not isinstance(response["message"], str)
                    ):
                        raise ValueError("invalid reader error")
                    # A filesystem error consumes one complete response. It is not
                    # a framing error or permission to switch to another image.
                    return OSError(response["errno"], response["message"])
                if response.get("status") != "ok":
                    raise ValueError("invalid reader status")
                if body:
                    images.keys(response, ("status", "length", "sha256"))
                    length = response["length"]
                    if type(length) is not int or not 0 <= length <= request["length"]:
                        raise ValueError("invalid read response length")
                    data = self._transfer(length, deadline)
                    if images.sha256(data) != response["sha256"]:
                        raise ValueError("read response checksum mismatch")
                    return data
                field = "node" if request["op"] == "stat" else "page"
                images.keys(response, ("status", field))
                if not isinstance(response[field], dict):
                    raise ValueError("invalid reader metadata response")
                return response
            except BaseException:
                self.close()
                raise

    def _result(self, request, *, body=False):
        result = self._request(request, body=body)
        if isinstance(result, OSError):
            raise result
        return result

    def stat(self, path: str = "") -> dict:
        return self._result({"op": "stat", "path": path})["node"]

    def entries(self, path: str = "", offset: int = 0, limit: int = 128) -> dict:
        return self._result({"op": "list", "path": path, "offset": offset, "limit": limit})["page"]

    def read_at(self, path: str, offset: int, length: int) -> bytes:
        if (
            type(offset) is not int
            or type(length) is not int
            or offset < 0
            or not 0 <= length <= MAX_FRAME
            or offset + length >= 2**64
        ):
            raise ValueError("invalid read range")
        return self._result(
            {"op": "read", "path": path, "offset": offset, "length": length}, body=True
        )

    def close(self) -> None:
        process, self.process = self.process, None
        try:
            if process is not None:
                images.convert.stop(process)
                for stream in (process.stdin, process.stdout):
                    stream.close()
        finally:
            if self.log is not None:
                self.log.close()
                self.log = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--store", required=True, type=Path)
    parser.add_argument("--handle", required=True)
    parser.add_argument("--reader", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--timeout", default=60, type=float)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("stat", "list", "read"):
        command = commands.add_parser(name)
        command.add_argument("path", nargs="?", default="")
        if name == "read":
            command.add_argument("--offset", default=0, type=int)
            command.add_argument("--length", default=64, type=int)
        if name == "list":
            command.add_argument("--offset", default=0, type=int)
            command.add_argument("--limit", default=128, type=int)
    args = parser.parse_args()
    try:
        with RafsReader(
            args.store, args.handle, args.reader, args.work_dir, args.timeout
        ) as reader:
            if args.command == "read":
                sys.stdout.buffer.write(reader.read_at(args.path, args.offset, args.length))
            else:
                result = (
                    reader.stat(args.path)
                    if args.command == "stat"
                    else reader.entries(args.path, args.offset, args.limit)
                )
                print(json.dumps(result))
    except (ValueError, OSError, RuntimeError) as error:
        parser.exit(1, f"{type(error).__name__}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
