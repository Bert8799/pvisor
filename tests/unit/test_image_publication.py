from __future__ import annotations

import errno
import os
import subprocess
import sys
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock

from scripts.images import publish_nydus as PUB
from tests.fixtures import workspace
from tests.image_fixtures.image_publication import (
    BLOB_ID,
    ROOT,
    StorageFixture,
    record_to_oci,
)


class ImagePublicationTests(unittest.TestCase):
    def test_verified_bytes_survive_concurrent_hardlink_metadata_changes(self):
        with workspace() as (root, _):
            source = root / "data"
            content = b"immutable content"
            source.write_bytes(content)

            class LinkOnWrite:
                def write(self, _data):
                    os.link(source, root / "another-revision")

            PUB.consume(
                source, {"sha256": PUB.sha256(content), "size": len(content)}, LinkOnWrite()
            )
            self.assertEqual(source.stat().st_nlink, 2)

    def test_immutable_identity_deduplication_and_source_config_survive(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            handle = fixture.publish("first")
            self.assertEqual(handle, fixture.publish("repeat"))
            bundle, record = PUB.open_bundle(root / "store", handle)
            self.assertNotEqual(BLOB_ID, record["blobs"][BLOB_ID]["artifact"]["sha256"][7:])
            original_config = PUB.convert.verified_blob(
                fixture.source, record_to_oci(record["source"]["config"])
            )
            self.assertEqual(
                (bundle / "metadata/source.config.json").read_bytes(), original_config.read_bytes()
            )
            second = fixture.new_source("second", b"two")
            next_handle = fixture.publish("next", source=second)
            self.assertNotEqual(handle, next_handle)
            next_bundle, _ = PUB.open_bundle(root / "store", next_handle)
            self.assertEqual(
                (bundle / "blobs" / BLOB_ID).stat().st_ino,
                (next_bundle / "blobs" / BLOB_ID).stat().st_ino,
            )
            self.assertEqual(len(list((root / "store/blobs/sha256").iterdir())), 1)
            self.assertEqual(list((root / "store/staging").iterdir()), [])
            PUB.open_bundle(root / "store", handle)

    def test_failed_qualification_or_missing_authentication_never_publishes(self):
        for problem in ("qualification", "digests", "toc", "toc-size"):
            with self.subTest(problem=problem), workspace() as (root, stack):
                fixture = StorageFixture(root, stack)
                if problem == "qualification":
                    fixture.report_failure = True
                elif problem == "digests":
                    fixture.description["inline_chunk_digests"] = False
                elif problem == "toc":
                    fixture.description["toc_sha256"] = "sha256:" + "0" * 64
                else:
                    fixture.description["toc_size"] = 0
                with self.assertRaises(ValueError):
                    fixture.publish()
                self.assertFalse((root / "store").exists())

    def test_changed_qualified_artifact_is_rejected_before_visibility(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)

            def change(_reader, conversion, _timeout):
                (conversion / "image.bootstrap").write_bytes(b"changed after qualification")
                return [fixture.description]

            with mock.patch.object(PUB, "describe_blobs", side_effect=change):
                with self.assertRaisesRegex(ValueError, "mismatch"):
                    fixture.publish()
            self.assertEqual(list((root / "store/revisions").iterdir()), [])
            self.assertEqual(list((root / "store/staging").iterdir()), [])

    def test_concurrent_publishers_reuse_the_same_complete_revision(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            with ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(fixture.publish, name) for name in ("one", "two")]
                handles = [future.result(timeout=30) for future in futures]
            self.assertEqual(handles[0], handles[1])
            PUB.open_bundle(root / "store", handles[0])
            self.assertEqual(len(list((root / "store/revisions").iterdir())), 1)
            self.assertEqual(list((root / "store/staging").iterdir()), [])

    def test_precommit_error_preserves_the_old_revision(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            old = fixture.publish("first")
            second = fixture.new_source("second", b"different")
            with mock.patch.object(
                PUB.os, "rename", side_effect=OSError(errno.EIO, "injected rename failure")
            ):
                with self.assertRaises(OSError):
                    fixture.publish("failed", source=second)
            self.assertEqual(
                [p.name for p in (root / "store/revisions").iterdir()], [PUB.handle_revision(old)]
            )
            PUB.open_bundle(root / "store", old)
            self.assertEqual(list((root / "store/staging").iterdir()), [])

    def test_postcommit_sync_error_reports_a_complete_handle_and_retry_resyncs(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            real_sync = PUB.sync_directory

            def fail(path):
                if path == root / "store/revisions":
                    raise OSError(errno.EIO, "injected directory sync failure")
                real_sync(path)

            with mock.patch.object(PUB, "sync_directory", side_effect=fail):
                with self.assertRaises(PUB.PublicationUncertain) as caught:
                    fixture.publish("uncertain")
            handle = caught.exception.handle
            PUB.open_bundle(root / "store", handle)
            with mock.patch.object(PUB, "sync_directory", wraps=real_sync) as syncing:
                self.assertEqual(fixture.publish("retry"), handle)
                self.assertIn(mock.call(root / "store/revisions"), syncing.call_args_list)

    def test_abrupt_exit_leaves_no_revision_and_a_later_publish_succeeds(self):
        with workspace() as (root, stack):
            crash = root / "crash"
            crash.mkdir()
            code = "from pathlib import Path; from tests.image_fixtures.image_publication import killed_writer; import sys; killed_writer(Path(sys.argv[1]))"
            child = subprocess.run(
                [sys.executable, "-c", code, str(crash)], cwd=ROOT, capture_output=True, timeout=30
            )
            self.assertEqual(child.returncode, 19, child.stderr.decode())
            self.assertEqual(list((crash / "store/revisions").iterdir()), [])
            retry = root / "retry"
            retry.mkdir()
            fixture = StorageFixture(retry, stack)
            handle = fixture.publish(store=crash / "store")
            PUB.open_bundle(crash / "store", handle)

    def test_readers_reject_tampering_missing_objects_and_symlinks(self):
        for name, operation in (
            ("COMMIT.json", "modify"),
            ("metadata/image.bootstrap", "modify"),
            ("metadata/source.config.json", "modify"),
            ("metadata/source.manifest.json", "delete"),
            ("blobs/" + BLOB_ID, "modify"),
            ("blobs/" + BLOB_ID, "delete"),
            ("blobs/" + BLOB_ID, "symlink"),
        ):
            with self.subTest(name=name, operation=operation), workspace() as (root, stack):
                fixture = StorageFixture(root, stack)
                handle = fixture.publish()
                bundle, _ = PUB.open_bundle(root / "store", handle)
                target = bundle / name
                original = target.read_bytes()
                target.unlink()
                if operation == "modify":
                    target.write_bytes(bytes([original[0] ^ 1]) + original[1:])
                elif operation == "symlink":
                    outside = root / "same-content"
                    outside.write_bytes(original)
                    target.symlink_to(outside)
                with self.assertRaises((OSError, ValueError)):
                    PUB.open_bundle(root / "store", handle)

    def test_existing_corrupt_cas_is_not_silently_repaired(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            handle = fixture.publish("first")
            blob = next((root / "store/blobs/sha256").iterdir())
            blob.unlink()
            blob.write_bytes(b"corrupt")
            with self.assertRaisesRegex(ValueError, "mismatch"):
                fixture.publish("second")
            PUB.open_bundle(root / "store", handle)
            self.assertEqual(blob.read_bytes(), b"corrupt")

    def test_store_symlinks_public_permissions_and_escaping_handles_are_rejected(self):
        with workspace() as (root, stack):
            fixture = StorageFixture(root, stack)
            public = root / "public"
            public.mkdir(mode=493)
            public.chmod(493)
            with self.assertRaisesRegex(ValueError, "private"):
                fixture.publish("public-work", store=public)
            destination = root / "destination"
            destination.mkdir(mode=448)
            link = root / "link"
            link.symlink_to(destination, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "non-symlink"):
                fixture.publish("symlink-work", store=link)
            for handle in (PUB.PREFIX + "../escape", PUB.PREFIX + "A" * 64, "pvisor-v1:other"):
                with self.subTest(handle=handle), self.assertRaises(ValueError):
                    PUB.open_bundle(root / "missing", handle)
