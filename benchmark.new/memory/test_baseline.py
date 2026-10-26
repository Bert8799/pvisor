"""Contract checks for the fixed pVisor memory baseline wrapper."""

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("baseline", Path(__file__).with_name("baseline.py"))
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)


class BaselineContractTests(unittest.TestCase):
    def manifest(self):
        return {
            "profile": "full",
            "created_utc": "20261022T084801.569969270Z",
            "source": {"head": "0f04d9f0" + "a" * 32, "dirty": True},
            "source_manifest_sha256": "7" * 64,
            "inputs": {"receipt_sha256": "b" * 64},
        }

    def test_real_directories_have_independent_profile_sequences(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            stamp = self.manifest()["created_utc"]
            paths = [
                subject.create_run_directory(profile, parent=parent, stamp=stamp)
                for profile in ("full", "quick", "full")
            ]
            self.assertEqual(
                [p.name for p in paths], ["full-20261022-1", "quick-20261022-1", "full-20261022-2"]
            )
            self.assertTrue(all(p.is_dir() and not p.is_symlink() for p in paths))

    def test_sequence_preserves_existing_records(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            occupied = parent / "full-20261022-3"
            occupied.write_text("existing record")
            output = subject.create_run_directory(
                "full", parent=parent, stamp=self.manifest()["created_utc"]
            )
            self.assertEqual(output.name, "full-20261022-4")
            self.assertEqual(occupied.read_text(), "existing record")

    def test_profiles_have_fixed_counts_and_include_ksm(self):
        expected = {"quick": 6, "full": 15}
        for profile, (arms, patterns) in subject.PROFILES.items():
            with self.subTest(profile=profile):
                self.assertEqual(len(arms) * len(patterns), expected[profile])
                self.assertIn("pvisor-ksm", arms)
                self.assertTrue(set(arms) <= set(subject.PVISOR_ARMS))
                self.assertTrue(set(patterns) <= set(subject.PATTERNS))
        self.assertEqual(
            subject.PROFILES["quick"][0], ("pvisor-off", "pvisor-physical", "pvisor-ksm")
        )

    def test_timestamp_is_utc_nanoseconds(self):
        stamp = subject.utc_stamp_ns()
        self.assertRegex(stamp, subject.STAMP_RE)
        self.assertEqual(len(stamp), len("20261021T120000.123456789Z"))

    def test_command_keeps_fixed_profile_and_preflight(self):
        command = subject.build_command("quick", Path("/tmp/inputs"), Path("/tmp/output/cohort"))
        self.assertIn("--preflight", command)
        self.assertIn("pvisor-off,pvisor-physical,pvisor-ksm", command)
        self.assertIn("repeated,random-shared", command)
        self.assertEqual(command[command.index("--output") + 1], "/tmp/output/cohort")

    def test_engineering_arm_subset_is_explicit_and_cannot_expand_the_profile(self):
        arms = subject.selected_arms("quick", "pvisor-physical")
        command = subject.build_command(
            "quick", Path("/tmp/inputs"), Path("/tmp/cohort"), arms=arms
        )
        self.assertEqual(command[command.index("--arms") + 1], "pvisor-physical")
        self.assertEqual(command[command.index("--patterns") + 1], "repeated,random-shared")
        for selection in ("", "pvisor-local", "pvisor-physical,pvisor-physical", "firecracker"):
            with self.subTest(selection=selection), self.assertRaises(subject.BaselineError):
                subject.selected_arms("quick", selection)

    def test_fixed_budget_is_the_registered_four_cpu_layout(self):
        self.assertEqual(subject.FIXED["vms"], 4)
        self.assertEqual(subject.FIXED["memory_mib"], 256)
        self.assertEqual(subject.FIXED["memory_max"], 2**31)
        self.assertEqual(subject.FIXED["memory_swap_max"], 0)
        self.assertEqual(subject.FIXED["cpu_quota"], "400000 100000")
        self.assertEqual(subject.FIXED["idle_seconds"], 60)
        self.assertEqual(subject.FIXED["post_write_seconds"], 30)

    def test_pattern_subset_is_explicit(self):
        patterns = subject.selected_patterns("quick", "random-shared")
        command = subject.build_command(
            "quick", Path("/tmp/inputs"), Path("/tmp/cohort"), patterns=patterns
        )
        self.assertEqual(command[command.index("--patterns") + 1], "random-shared")
        for selection in ("", "random-unique", "repeated,repeated"):
            with self.subTest(selection=selection), self.assertRaises(subject.BaselineError):
                subject.selected_patterns("quick", selection)

    def test_final_validation_failure_never_publishes_success(self):
        import baseline_evidence

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            argv = [
                "baseline.py",
                "--inputs",
                str(Path(directory) / "inputs"),
                "--output",
                str(output),
            ]
            with (
                patch.object(sys, "argv", argv),
                patch.object(subject, "collect_host", return_value={}),
                patch.object(subject, "check_host"),
                patch.object(subject, "inputs_info", return_value={}),
                patch.object(subject, "source_info", return_value={}),
                patch.object(subject, "harness_info", return_value={}),
                patch.object(subject, "digest", return_value="hash"),
                patch.object(subject.subprocess, "check_output", return_value=b"patch"),
                patch.object(subject.subprocess, "run", return_value=SimpleNamespace(returncode=0)),
                patch.object(subject, "write_summary", return_value={"evidence_sufficient": True}),
                patch.object(baseline_evidence, "freeze_source", return_value={}),
                patch.object(baseline_evidence, "inventory", return_value={}),
                patch.object(baseline_evidence, "input_files", return_value={}),
                patch.object(baseline_evidence, "host_config", return_value={}),
                patch.object(
                    baseline_evidence,
                    "verify",
                    side_effect=[None, None, ValueError("CPU policy changed")],
                ),
            ):
                self.assertEqual(subject.main(), 1)
            result = json.loads((output / "baseline.json").read_text())
            self.assertFalse(result["complete"])
            self.assertFalse(result["evidence_valid"])
            self.assertIn("CPU policy changed", result["error"])


if __name__ == "__main__":
    unittest.main()
