"""CLI template benchmark contracts; synthetic fixtures are not VM measurements."""

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from tests.fixtures import workspace

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "startup"))
import template_cli as BENCH  # noqa: E402


def fixture(directory, mode, token, identity="instance-new", attempt="attempt-new"):
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "workspace").mkdir()
    (directory / "workspace/startup-canary").write_bytes(b"unchanged startup input\n")
    (directory / "workspace/startup-token").write_text(token + "\n")
    (directory / "stage/upper").mkdir(parents=True)
    instance = identity if mode == "template" else "cold"
    (directory / "stdout").write_text(
        f"REFERENCE_TOKEN {token}\nREFERENCE_INSTANCE {instance}\nREFERENCE_READY\n"
        'REFERENCE_RESULT {"mode":"ready","correctness":"passed"}\n'
    )
    (directory / "stderr").touch()
    BENCH.save(
        directory / "stage/run-bundle.json",
        dict(
            run=dict(
                command=["/bin/sh", "-c", BENCH.PAYLOAD],
                attempt_id=attempt,
                state="completed",
                exit_code=0,
                executor=dict(kind="virtual_machine", isolation="virtual_machine"),
            ),
            safety=dict(filesystem_changes_staged=True),
        ),
    )


class TemplateCliContracts(unittest.TestCase):
    def test_preparation_uses_the_product_cli_and_checks_build_and_guest(self):
        for mutation in (None, "build", "guest_sha256", "workspace_device", "failed"):
            with self.subTest(mutation=mutation), workspace() as (root, _):
                binary = root / "pvisor"
                binary.write_bytes(b"same CLI artifact")
                image = root / "sealed"
                image.mkdir()
                (image / "init.krun").write_bytes(b"exact embedded guest")
                prepared = dict(
                    rootfs=str(image),
                    catalog=str(root / "catalog"),
                    version="a" * 64,
                    recipe=dict(
                        cpus=2,
                        memory_mib=128,
                        workspace_device=True,
                        compatibility=dict(build=BENCH.timing.COMMON.digest(binary)),
                        guest_sha256=BENCH.timing.COMMON.digest(image / "init.krun"),
                    ),
                )
                if mutation == "build":
                    prepared["recipe"]["compatibility"]["build"] = "b" * 64
                elif mutation == "guest_sha256":
                    prepared["recipe"]["guest_sha256"] = "b" * 64
                elif mutation == "workspace_device":
                    prepared["recipe"]["workspace_device"] = False
                child = Mock(pid=987654321)
                child.wait.return_value = 1 if mutation == "failed" else 0

                def spawn(argv, **kwargs):
                    self.assertEqual(argv[:3], [str(binary), "cache", "prepare-template"])
                    self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
                    self.assertTrue(kwargs["start_new_session"])
                    kwargs["stdout"].write(json.dumps(prepared).encode())
                    return child

                with (
                    patch.object(BENCH.subprocess, "Popen", side_effect=spawn),
                    patch.object(BENCH.os, "killpg") as kill,
                ):
                    if mutation is None:
                        path, version, elapsed = BENCH.prepare_template(
                            binary, root, root / "source", root / "catalog"
                        )
                        self.assertEqual((path, version), (image, "a" * 64))
                        self.assertGreaterEqual(elapsed, 0)
                        self.assertTrue((root / "preparation.json").is_file())
                    else:
                        with self.assertRaises(ValueError):
                            BENCH.prepare_template(binary, root, root / "source", root / "catalog")
                        self.assertFalse((root / "preparation.json").exists())
                    kill.assert_called_once_with(child.pid, BENCH.signal.SIGKILL)
                    self.assertGreaterEqual(child.wait.call_count, 2)

    def test_preflight_does_not_produce_formal_samples_or_comparison(self):
        with workspace() as (root, _):

            def trial(path, mode, phase, index, env):
                self.assertEqual((phase, index), ("preflight", 0))
                return dict(ready_ns=1, exit_ns=2), dict(attempt_id=mode, instance_id="fresh")

            with (
                patch.object(BENCH, "trial", side_effect=trial) as run,
                patch.object(BENCH, "compare") as compare,
            ):
                self.assertIsNone(BENCH.cohort(root, {}, preflight_only=True))
                self.assertEqual(run.call_count, 2)
                compare.assert_not_called()
            self.assertFalse((root / "samples.json").exists())

    def test_shared_shell_probe_reports_the_fresh_input_before_ready(self):
        with workspace() as (root, _):
            (root / "startup-token").write_text("fresh-token\n")
            for identity in (None, "instance-new"):
                env = BENCH.environment()
                if identity is not None:
                    env["PVISOR_INSTANCE_ID"] = identity
                result = subprocess.run(
                    ["/bin/sh", "-c", BENCH.PAYLOAD],
                    cwd=root,
                    env=env,
                    capture_output=True,
                    text=True,
                    check=True,
                )
                BENCH.hooks.validate_output(result.stdout, result.stderr)
                self.assertEqual(
                    result.stdout.splitlines()[:3],
                    [
                        "REFERENCE_TOKEN fresh-token",
                        "REFERENCE_INSTANCE " + (identity or "cold"),
                        "REFERENCE_READY",
                    ],
                )

    def test_actual_cli_task_and_budget_are_identical_except_selector(self):
        root, image, binary = Path("/batch/trial"), Path("/image"), Path("/frozen/pvisor")
        cold = BENCH.command(binary, root, image, Path("/cold.toml"))
        restored = BENCH.command(binary, root, image, Path("/template.toml"))
        self.assertEqual(cold[:2], [str(binary), "run"])
        config = cold.index("--config") + 1
        restored[config] = cold[config]
        self.assertEqual(cold, restored)
        self.assertEqual(cold[cold.index("--overlaynet") + 1], "auto")
        self.assertEqual(cold[cold.index("--cpu") + 1], "2")
        self.assertEqual(cold[cold.index("--memory") + 1], "128MiB")
        self.assertIn("--stage", cold)
        self.assertEqual(cold[cold.index("--") + 1 :], ["/bin/sh", "-c", BENCH.PAYLOAD])

    def test_handoff_and_fresh_workspace_are_required_before_ready(self):
        mutations = [
            (
                "fallback",
                lambda p: (p / "stdout").write_text(
                    (p / "stdout").read_text().replace("instance-new", "cold")
                ),
            ),
            ("old workspace", lambda p: (p / "workspace/startup-token").write_text("old\n")),
            (
                "wrong token",
                lambda p: (p / "stdout").write_text(
                    (p / "stdout").read_text().replace("nonce", "other")
                ),
            ),
            (
                "duplicate Ready",
                lambda p: (p / "stdout").write_text(
                    (p / "stdout").read_text() + "REFERENCE_READY\n"
                ),
            ),
            ("writes", lambda p: (p / "stage/upper/unexpected").touch()),
        ]
        for name, mutate in mutations:
            with self.subTest(name=name), workspace() as (root, _):
                trial = root / "trial"
                fixture(trial, "template", "nonce")
                self.assertEqual(
                    BENCH.validate_trial(trial, "template", "nonce")["instance_id"], "instance-new"
                )
                mutate(trial)
                with self.assertRaises(ValueError):
                    BENCH.validate_trial(trial, "template", "nonce")

    def test_trial_preserves_archived_workspace_after_the_next_reset(self):
        with workspace() as (root, _):
            BENCH.save(
                root / "protocol.json",
                dict(
                    backends=["pvisor-vm"],
                    assets=None,
                    subject=BENCH.command(Path("/frozen/pvisor"), root, Path("/image")),
                ),
            )
            counter = 0

            def capture(argv, cwd, env, log):
                nonlocal counter
                counter += 1
                token = (cwd / "startup-token").read_text().strip()
                log.write_text(
                    f"REFERENCE_TOKEN {token}\nREFERENCE_INSTANCE cold\nREFERENCE_READY\n"
                    'REFERENCE_RESULT {"mode":"ready","correctness":"passed"}\n'
                )
                BENCH.save(
                    log.parent / "stage/run-bundle.json",
                    dict(
                        run=dict(
                            command=["/bin/sh", "-c", BENCH.PAYLOAD],
                            attempt_id=f"attempt-{counter}",
                            state="completed",
                            exit_code=0,
                            executor=dict(kind="virtual_machine", isolation="virtual_machine"),
                        ),
                        safety=dict(filesystem_changes_staged=True),
                    ),
                )
                log.with_suffix(".timing.json").write_text("{}")
                return dict(
                    command=argv,
                    start_ns=1,
                    ready_ns=2,
                    end_ns=3,
                    exit_code=0,
                    ready_duration_ns=1,
                    exit_duration_ns=2,
                )

            before = os.environ.copy()
            with patch.object(BENCH.timing, "capture", side_effect=capture):
                _, first = BENCH.trial(root, "cold", "warmup", 0, BENCH.environment())
                BENCH.trial(root, "cold", "warmup", 1, BENCH.environment())
            self.assertEqual(os.environ.copy(), before)
            self.assertFalse((root / "trials/00000/workspace").is_symlink())
            self.assertEqual(
                BENCH.validate_trial(root / "trials/00000", "cold", first["token"])["token"],
                first["token"],
            )

    def test_statistics_require_all_sixty_pairs_and_keep_slow_samples(self):
        samples = {
            mode: [dict(ready_ns=1_000_000 + i, exit_ns=2_000_000 + i) for i in range(60)]
            for mode in BENCH.MODES
        }
        samples["template"][-1] = dict(ready_ns=100_000_000, exit_ns=110_000_000)
        result = BENCH.compare(samples)
        self.assertEqual(result["template"]["Ready max ms"], 100)
        self.assertGreater(result["template"]["Ready mean ms"], 2)
        self.assertEqual(result["comparison"]["conclusion"], "no detected difference")
        self.assertEqual(result["comparison"]["excluded_samples"], 0)
        samples["cold"].pop()
        with self.assertRaisesRegex(ValueError, "complete"):
            BENCH.compare(samples)

    def prepared(self, root, resources):
        image, catalog = root / "image", root / "catalog"
        image.mkdir()
        catalog.mkdir()
        (root / "frozen").mkdir()
        for mode in BENCH.MODES:
            BENCH.save(root / mode / "protocol.json", dict(subject=["/frozen/pvisor", "run", mode]))
        config = dict(
            rootfs=str(image),
            catalog=str(catalog),
            subjects={
                mode: BENCH.read(root / mode / "protocol.json")["subject"] for mode in BENCH.MODES
            },
        )
        BENCH.save(root / "configuration.json", config)
        BENCH.save(root / "frozen/configuration.json", config)
        BENCH.save(
            root / "inputs.json",
            dict(
                frozen={
                    "configuration.json": BENCH.timing.COMMON.digest(
                        root / "frozen/configuration.json"
                    )
                },
                trees={"rootfs": BENCH.inventory(image), "catalog": BENCH.inventory(catalog)},
            ),
        )
        BENCH.save(
            root / "process-cleanup.json",
            dict(errors=[], after=[], temporary_executable_removed=True),
        )
        BENCH.save(root / "cpu-quiet-gate.json", dict(passed=True))
        counter = 0

        def trial(path, mode, phase, index, env):
            nonlocal counter
            counter += 1
            base = path / "trials"
            number = len(list(base.iterdir())) if base.exists() else 0
            directory = base / f"{number:05d}"
            token = f"token-{counter}"
            fixture(
                directory, mode, token, identity=f"instance-{counter}", attempt=f"attempt-{counter}"
            )
            duration = (3 if mode == "cold" else 2) * 1_000_000 + index
            start = counter * 10_000_000
            BENCH.save(
                directory / "timing.json",
                dict(
                    command=config["subjects"][mode],
                    phase=phase,
                    index=index,
                    start_ns=start,
                    ready_ns=start + duration,
                    end_ns=start + duration + 1_000_000,
                    exit_code=0,
                    ready_duration_ns=duration,
                    exit_duration_ns=duration + 1_000_000,
                ),
            )
            proof = BENCH.validate_trial(directory, mode, token)
            BENCH.save(directory / "startup-validation.json", dict(passed=True, **proof))
            return dict(ready_ns=duration, exit_ns=duration + 1_000_000), proof

        resources.enter_context(patch.object(BENCH, "trial", side_effect=trial))
        resources.enter_context(patch("builtins.print"))
        return BENCH.cohort(root, {})

    def test_cohort_preserves_all_trials_and_audit_rejects_changed_raw_evidence(self):
        with workspace() as (root, resources):
            result = self.prepared(root, resources)
            self.assertEqual(BENCH.audit(root), result)
            self.assertEqual(len(list((root / "cold/trials").iterdir())), 64)
            self.assertEqual(len(list((root / "template/trials").iterdir())), 64)
            self.assertEqual(BENCH.order().count(["cold", "template"]), 30)
            self.assertLess(result["comparison"]["paired_bootstrap_95_ci_ms"][1], 0)
            path = root / "cold/trials/00004/timing.json"
            record = BENCH.read(path)
            record["ready_duration_ns"] += 1
            BENCH.save(path, record)
            with self.assertRaisesRegex(ValueError, "original CLI timing"):
                BENCH.audit(root)

    def test_failure_never_reaches_a_comparison(self):
        with workspace() as (root, _):
            with (
                patch.object(BENCH, "trial", side_effect=ValueError("CLI selector unsupported")),
                patch.object(BENCH, "compare") as compare,
            ):
                with self.assertRaisesRegex(ValueError, "unsupported"):
                    BENCH.cohort(root, {})
                compare.assert_not_called()
            self.assertFalse((root / "summary.json").exists())

    def test_audit_rejects_missing_trials_frozen_changes_and_cleanup_failure(self):
        for change, message in [
            ("missing", "missing or extra"),
            ("frozen", "frozen artifact"),
            ("cleanup", "cleanup failed"),
        ]:
            with self.subTest(change=change), workspace() as (root, resources):
                self.prepared(root, resources)
                if change == "missing":
                    import shutil

                    shutil.rmtree(root / "template/trials/00001")
                elif change == "frozen":
                    (root / "frozen/configuration.json").write_text("{}")
                else:
                    BENCH.save(root / "process-cleanup.json", dict(errors=["process remains"]))
                with self.assertRaisesRegex(ValueError, message):
                    BENCH.audit(root)


if __name__ == "__main__":
    unittest.main()
