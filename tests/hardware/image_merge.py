from __future__ import annotations

import errno
import json
import os
import shutil
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest import mock

from scripts.images import compose_nydus as ENV
from scripts.images import merge_nydus as MERGE
from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_merge import D, F, MergeFixture, cases


@unittest.skipUnless(
    all(
        (
            os.environ.get(name)
            for name in ("PVISOR_NYDUS_IMAGE", "PVISOR_NYDUSD", "PVISOR_RAFS_INVENTORY")
        )
    ),
    "requires pinned, patched native Nydus tools",
)
class NativeImageMergeTests(MergeFixture, unittest.TestCase):
    def test_full_merge_semantics_retain_attrs_and_original_data_inodes(self):
        for index, entries in enumerate(cases()):
            with self.subTest(case=index), workspace() as (root, _):
                store, environment, tools, sources = self.prepare(root, entries)
                env_path, recipe = ENV.open_environment(store, environment)
                originals = {}
                for name, handle in ENV.references(recipe):
                    path, record = PUB.open_bundle(store, handle)
                    for blob_id, descriptor in record["blobs"].items():
                        originals[blob_id] = ((path / "blobs" / blob_id).stat().st_ino, descriptor)
                handle = MERGE.merge(store, environment, tools[0], tools[2], root / "merge")
                bundle, record = MERGE.open_merged(store, handle)
                self.assertEqual(json.loads((root / "merge/differences.json").read_text()), [])
                expected = PUB.convert.inventory(sources)
                stored = json.loads((bundle / "metadata/inventory.json").read_text())
                self.assertEqual(PUB.convert.differences(expected, stored), [])
                for blob_id, descriptor in record["blobs"].items():
                    inode, original = originals[blob_id]
                    self.assertEqual(descriptor, original)
                    self.assertEqual((bundle / "blobs" / blob_id).stat().st_ino, inode)
                self.assertEqual(list((root / "merge/directory-blobs").iterdir()), [])
                self.assertEqual(
                    MERGE.merge(store, environment, tools[0], tools[2], root / "repeat"), handle
                )
                shutil.rmtree(store / "revisions")
                (store / "revisions").mkdir(mode=448)
                shutil.rmtree(env_path)
                shutil.rmtree(root / "merge")
                shutil.rmtree(root / "repeat")
                for source_index, source in enumerate(sources):
                    source.unlink()
                    shutil.rmtree(root / f"oci-{source_index}")
                    shutil.rmtree(root / f"publish-{source_index}")
                MERGE.open_merged(store, handle)
                output = root / "readback"
                output.mkdir()
                actual = PUB.convert.scan_direct(
                    tools[2], bundle / "metadata/image.bootstrap", bundle / "blobs", output, 60
                )
                self.assertEqual(PUB.convert.differences(stored, actual), [])
                self.assertEqual(list((store / "staging").iterdir()), [])

    def test_base_only_concurrent_merge_is_idempotent(self):
        with workspace() as (root, _):
            store, env, tools, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            with ThreadPoolExecutor(max_workers=2) as pool:
                jobs = [
                    pool.submit(MERGE.merge, store, env, tools[0], tools[2], root / name)
                    for name in ("one", "two")
                ]
                handles = [job.result(timeout=120) for job in jobs]
            self.assertEqual(handles[0], handles[1])
            MERGE.open_merged(store, handles[0])
            self.assertEqual(len(list((store / "merged").iterdir())), 1)

    def test_failed_readback_or_commit_never_publishes_a_partial_revision(self):
        with workspace() as (root, _):
            store, env, tools, _ = self.prepare(root, [cases()[0][0], cases()[0][1]])
            real = PUB.convert.scan_direct

            def changed(reader, bootstrap, blobs, output, timeout):
                tree = real(reader, bootstrap, blobs, output, timeout)
                if output.name == "readback":
                    tree["dir/new"]["sha256"] = "sha256:" + "0" * 64
                return tree

            with mock.patch.object(PUB.convert, "scan_direct", side_effect=changed):
                with self.assertRaisesRegex(ValueError, "differs"):
                    MERGE.merge(store, env, tools[0], tools[2], root / "bad-readback")
            self.assertEqual(list((store / "merged").iterdir()), [])
            with mock.patch.object(PUB.os, "rename", side_effect=OSError(errno.EIO, "injected")):
                with self.assertRaises(OSError):
                    MERGE.merge(store, env, tools[0], tools[2], root / "bad-rename")
            self.assertEqual(list((store / "merged").iterdir()), [])
            self.assertEqual(list((store / "staging").iterdir()), [])
            ENV.open_environment(store, env)

    def test_tampering_is_detected_without_fallback(self):
        with workspace() as (root, _):
            store, env, tools, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            handle = MERGE.merge(store, env, tools[0], tools[2], root / "merge")
            bundle, record = MERGE.open_merged(store, handle)
            paths = [
                "MERGED.json",
                "metadata/image.bootstrap",
                "metadata/inventory.json",
                "metadata/source.config.json",
                "environment/ENVIRONMENT.json",
                "blobs/" + next(iter(record["blobs"])),
            ]
            for filename in paths:
                with self.subTest(filename=filename):
                    path = bundle / filename
                    original = path.read_bytes()
                    path.unlink()
                    path.write_bytes(b"changed")
                    with self.assertRaises((ValueError, json.JSONDecodeError)):
                        MERGE.open_merged(store, handle)
                    path.unlink()
                    path.write_bytes(original)
            MERGE.open_merged(store, handle)

    def test_changed_bootstrap_or_missing_chunk_authentication_is_rejected(self):
        with workspace() as (root, _):
            store, env, tools, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            real_run = PUB.convert.run_tool

            def change_bootstrap(command, output, timeout):
                real_run(command, output, timeout)
                if "--describe" in command:
                    with Path(command[2]).open("ab") as stream:
                        stream.write(b"changed after qualification")

            def omit_digests(command, output, timeout):
                real_run(command, output, timeout)
                if "--describe" in command:
                    descriptions = json.loads(output.read_text())
                    descriptions[0]["inline_chunk_digests"] = False
                    PUB.convert.save(output, descriptions)

            for name, target, replacement in [
                ("bootstrap", "run_tool", change_bootstrap),
                ("digests", "run_tool", omit_digests),
            ]:
                with (
                    self.subTest(fault=name),
                    mock.patch.object(PUB.convert, target, side_effect=replacement),
                ):
                    with self.assertRaises(ValueError):
                        MERGE.merge(store, env, tools[0], tools[2], root / name)
                self.assertEqual(list((store / "merged").iterdir()), [])
                self.assertEqual(list((store / "staging").iterdir()), [])

    def test_postcommit_sync_failure_returns_a_verifiable_handle_and_retry_resyncs(self):
        with workspace() as (root, _):
            store, env, tools, _ = self.prepare(root, [[(".", D, b""), ("file", F, b"data")]])
            real_sync = PUB.sync_directory

            def fail_visible(path):
                if path == store / "merged" and list(path.iterdir()):
                    raise OSError(errno.EIO, "injected after rename")
                real_sync(path)

            with mock.patch.object(PUB, "sync_directory", side_effect=fail_visible):
                with self.assertRaises(PUB.PublicationUncertain) as raised:
                    MERGE.merge(store, env, tools[0], tools[2], root / "uncertain")
            handle = raised.exception.handle
            MERGE.open_merged(store, handle)
            self.assertEqual(MERGE.merge(store, env, tools[0], tools[2], root / "retry"), handle)
