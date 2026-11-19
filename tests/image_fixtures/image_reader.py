from __future__ import annotations

import os
import sys
from pathlib import Path

from scripts.images import merge_nydus as MERGE
from tests.image_fixtures import image_merge as merge_tests
from tests.image_fixtures.image_publication import StorageFixture


class ReaderProcessFixture:
    def fixture(self, root, stack, program):
        fixture = StorageFixture(root, stack)
        fixture.publish("publication-work")
        (root / "store/merged").mkdir(mode=448)
        engine = root / "engine"
        engine.write_text(f"#!{sys.executable}\n" + program)
        engine.chmod(448)
        handle = MERGE.PREFIX + "a" * 64
        return (root / "store", handle, engine)


class ReaderFixture:
    def prepare(self, root, entries):
        store, environment, tools, sources = merge_tests.MergeFixture.prepare(self, root, entries)
        handle = MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
        engine = Path(os.environ["PVISOR_RAFS_READER"]).resolve()
        return (store, handle, engine, sources)
