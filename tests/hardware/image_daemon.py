from __future__ import annotations

import json
import os
import sys
import time
import unittest
import uuid
from pathlib import Path

from tests.fixtures import workspace
from tests.image_fixtures.image_daemon import REQUIRED, DaemonFixture


@unittest.skipUnless(
    sys.platform == "linux" and all((os.environ.get(name) for name in REQUIRED)),
    "requires qualified Python RAFS fixture, native binaries, KVM and delegated cgroup v2",
)
class NativeImageDaemonTests(DaemonFixture, unittest.TestCase):
    def test_native_image_and_inert_template_lifecycle(self):
        inputs = json.loads(Path(os.environ["PVISOR_RAFS_CASE_INPUTS"]).read_text())
        parent = Path(os.environ["PVISOR_RAFS_CGROUP_ROOT"]).resolve()
        group = parent / ("image-case-" + uuid.uuid4().hex)
        group.mkdir(mode=448)

        def stop_native():
            (group / "cgroup.kill").write_text("1")
            deadline = time.monotonic() + 10
            while "populated 1" in (group / "cgroup.events").read_text():
                self.assertLess(time.monotonic(), deadline, "native test cgroup stayed populated")
                time.sleep(0.02)

        try:
            (group / "cgroup.subtree_control").write_text("+cpu +memory +pids")
            with workspace() as (root, resources):
                resources.callback(stop_native)
                self.exercise(root, group, inputs)
        finally:
            stop_native()
            for path in sorted(group.rglob("*"), reverse=True):
                if path.is_dir():
                    path.rmdir()
            group.rmdir()
