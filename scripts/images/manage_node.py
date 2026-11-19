#!/usr/bin/env python3
"""Manage explicit RAFS node dependencies, revisions and whole-blob cache eviction."""

from __future__ import annotations

import argparse
import errno
import hashlib
import json
import os
import socket
import stat
import struct
import sys
import uuid
from pathlib import Path

MAX_FRAME = 1024 * 1024


def receive(stream, count):
    result = bytearray()
    while len(result) < count:
        block = stream.recv(count - len(result))
        if not block:
            raise EOFError("cache disconnected; an operation may already have committed")
        result.extend(block)
    return bytes(result)


def exchange(endpoint, handle, request, *, generation=None, token=None):
    """One bounded same-UID exchange. Mutations are never automatically replayed."""
    if sys.platform != "linux":
        raise OSError(errno.EOPNOTSUPP, "RAFS node management requires Linux peer credentials")
    if not endpoint.startswith("unix:///"):
        raise ValueError("management requires an absolute local Unix endpoint")
    path = Path(endpoint.removeprefix("unix://"))
    parent = path.parent.lstat()
    info = path.lstat()
    if (
        not stat.S_ISDIR(parent.st_mode)
        or parent.st_uid != os.geteuid()
        or parent.st_mode & 0o077
        or not stat.S_ISSOCK(info.st_mode)
        or info.st_uid != os.geteuid()
        or info.st_mode & 0o077
    ):
        raise PermissionError("cache socket and parent must be private and same-UID")
    call = dict(
        version=2,
        token=token,
        request=dict(op="rafs", handle=handle, request=request, generation=generation),
    )
    wire = json.dumps(call, separators=(",", ":")).encode()
    if len(wire) > 32768:
        raise ValueError("management request exceeds frame bound")
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(30)
        stream.connect(str(path))
        _, uid, _ = struct.unpack(
            "3i", stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
        )
        after = path.lstat()
        if uid != os.geteuid() or (info.st_dev, info.st_ino) != (after.st_dev, after.st_ino):
            raise PermissionError("cache peer or socket identity changed")
        stream.sendall(len(wire).to_bytes(4, "big") + wire)
        length = int.from_bytes(receive(stream, 4), "big")
        if not 0 < length <= MAX_FRAME:
            raise ValueError("invalid cache response frame")
        response = json.loads(receive(stream, length))
        if response.get("status") == "error":
            raise RuntimeError(f"cache refused request: {response.get('code')}")
        if (
            response.get("status") != "rafs"
            or response.get("length") != 0
            or response.get("sha256") != "sha256:" + hashlib.sha256(b"").hexdigest()
        ):
            raise ValueError("expected empty authenticated RAFS management body")
        frame = response["frame"]
        if frame.get("status") == "error":
            raise OSError(frame["errno"], frame.get("message", "node operation failed"))
        if frame.get("status") != "ok":
            raise ValueError("invalid node acknowledgement")
        return frame


def operate(endpoint, handle, request, *, token=None):
    """Discover once, then fence the requested effect to that service generation."""
    snapshot = exchange(endpoint, handle, {"op": "stats"}, token=token)
    if request["op"] == "stats":
        return snapshot
    generation = snapshot.get("node_generation")
    if not isinstance(generation, str):
        raise ValueError("service does not advertise lifecycle generation fencing")
    uuid.UUID(generation)
    return exchange(endpoint, handle, request, generation=generation, token=token)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--handle", required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("stats")
    commands.add_parser("remove")
    trim = commands.add_parser("trim")
    trim.add_argument("--target-bytes", required=True, type=int)
    for name in ("retain", "release", "inspect"):
        operation = commands.add_parser(name)
        operation.add_argument("--kind", required=True, choices=("template", "checkpoint"))
        operation.add_argument("--owner", required=True)
    args = parser.parse_args(argv)
    if args.command in ("retain", "release", "inspect"):
        request = dict(op="lease", kind=args.kind, owner=args.owner, action=args.command)
    elif args.command == "trim":
        if not 0 <= args.target_bytes < 2**64:
            parser.error("target bytes must fit an unsigned 64-bit integer")
        request = dict(op="trim", target_bytes=args.target_bytes)
    else:
        request = dict(op=args.command)
    print(
        json.dumps(
            operate(
                args.endpoint, args.handle, request, token=os.environ.get("PVISOR_CACHE_TOKEN")
            ),
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
