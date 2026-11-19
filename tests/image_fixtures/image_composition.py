from __future__ import annotations

from tests.image_fixtures.image_publication import StorageFixture


def component(handle, role="toolchain", name="python"):
    return {"role": role, "name": name, "handle": handle}


class ImageCompositionFixture:
    def make(self, root, stack):
        fixture = StorageFixture(root, stack)
        base = fixture.publish("base-work")
        tooling = fixture.publish("tool-work", layer_index=0)
        second = fixture.new_source("workspace-source", b"workspace")
        workspace_layer = fixture.publish("workspace-work", source=second, layer_index=0)
        return (
            fixture,
            base,
            [component(tooling), component(workspace_layer, "workspace", "initial")],
        )
