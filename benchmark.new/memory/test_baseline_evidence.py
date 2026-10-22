"""Source and environment evidence checks without KVM or privileged writes."""

import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import baseline_evidence as subject


class EvidenceTests(unittest.TestCase):
    def test_untracked_bytes_modes_and_deletions_are_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            tracked = repo / "tracked"
            tracked.write_text("original")
            subprocess.run(["git", "-C", str(repo), "add", "tracked"], check=True)
            tracked.unlink()
            (repo / "new.py").write_text("print('untracked')")
            (repo / "link").symlink_to("new.py")
            output = Path(directory) / "evidence"
            output.mkdir()
            rows = subject.freeze_source(output, repo)
            self.assertTrue(rows["tracked"]["absent"])
            self.assertIn("sha256", rows["new.py"])
            with tarfile.open(output / "source.tar.gz") as archive:
                self.assertEqual(archive.extractfile("new.py").read(), b"print('untracked')")
                self.assertTrue(archive.getmember("link").issym())
            self.assertEqual(json.loads((output / "source-files.json").read_text()), rows)
            (repo / "new.py").write_text("print('modified')")
            self.assertNotEqual(subject.inventory(repo), rows)

    def test_cpu_governor_swap_is_detected_but_frequency_variation_allowed(self):
        before = {
            "cpus": {
                "0": {"scaling_governor": "schedutil", "scaling_cur_freq": "100"},
                "1": {"scaling_governor": "performance", "scaling_cur_freq": "200"},
            },
            "boost": "1",
        }
        after = json.loads(json.dumps(before))
        after["cpus"]["0"]["scaling_cur_freq"] = "999"
        self.assertEqual(subject.frequency_config(before), subject.frequency_config(after))
        after["cpus"]["0"]["scaling_governor"] = "performance"
        after["cpus"]["1"]["scaling_governor"] = "schedutil"
        self.assertNotEqual(subject.frequency_config(before), subject.frequency_config(after))

    def test_watch_records_failure_and_targets_only_owned_scope(self):
        import threading

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "state.json"
            state.write_text("{}")
            with (
                patch.object(subject, "verify", side_effect=ValueError("source changed")),
                patch.object(subject.subprocess, "run") as run,
            ):
                subject.watch("owned.scope", threading.Event(), root, state)
            self.assertIn("source changed", (root / "baseline-error.json").read_text())
            self.assertEqual(run.call_args.args[0][-1], "owned.scope")
            self.assertTrue(json.loads((root / "baseline-guard.json").read_text())[0]["error"])


if __name__ == "__main__":
    unittest.main()
