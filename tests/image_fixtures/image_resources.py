from __future__ import annotations

import threading
import time

from tests.image_fixtures import image_node as N

MiB = 1024 * 1024


class TrackingOrigin(N.Origin):
    def __init__(self, blobs):
        super().__init__(blobs)
        self.condition = threading.Condition()
        self.active = self.peak = 0
        self.block = False
        self.entered = threading.Event()
        self.release = threading.Event()
        original = self.server.RequestHandlerClass.do_GET
        owner = self

        def traced(handler):
            with owner.condition:
                owner.active += 1
                owner.peak = max(owner.peak, owner.active)
                owner.condition.notify_all()
            try:
                owner.entered.set()
                if owner.block and not owner.release.wait(10):
                    raise TimeoutError("controlled Range gate was not released")
                original(handler)
            finally:
                with owner.condition:
                    owner.active -= 1
                    owner.condition.notify_all()

        self.server.RequestHandlerClass.do_GET = traced

    def idle(self, *, reset=True):
        deadline = time.monotonic() + 5
        with self.condition:
            while self.active:
                if not self.condition.wait(deadline - time.monotonic()):
                    raise TimeoutError("origin did not become idle")
            if reset:
                self.peak = 0
                self.entered.clear()

    def __exit__(self, *args):
        self.release.set()
        super().__exit__(*args)


def stats(endpoint, handle, **kwargs):
    reply, body = N.rpc(endpoint, handle, {"op": "stats"}, **kwargs)
    if reply["status"] != "ok" or body:
        raise AssertionError(reply)
    return reply["stats"]


class ResourceFixture:
    def prepare(self, root, *, layers=False):
        return N.NodeFixture.prepare(self, root, layers=layers)

    def opened(self, *args, **kwargs):
        return N.NodeFixture.opened(self, *args, **kwargs)

    def assert_limits(self, snapshot):
        state = snapshot["resources"]
        self.assertLessEqual(state["metadata_bytes"], state["max_metadata_bytes"])
        self.assertLessEqual(state["metadata_peak_bytes"], state["max_metadata_bytes"])
        self.assertLessEqual(state["io_bytes"], state["max_io_bytes"])
        self.assertLessEqual(state["io_peak_bytes"], state["max_io_bytes"])
        self.assertLessEqual(
            snapshot["process"]["virtual_bytes"], snapshot["max_address_space_bytes"]
        )
