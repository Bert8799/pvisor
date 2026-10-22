"""Offline checks for convergence, observation gaps and startup timing."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import baseline_analysis as subject


def curve(fn, end=60):
    return [(i / 20, fn(i / 20)) for i in range(-20, end * 20 + 21)]


class SteadyStateTests(unittest.TestCase):
    def test_already_stable_keeps_startup_cost(self):
        result = subject.steady_state(curve(lambda t: 200), 0, 60, startup_origin=-3)
        self.assertEqual(result["status"], "reached")
        self.assertEqual(result["after_window_origin_seconds"], 0)
        self.assertEqual(result["after_group_start_seconds"], 3)
        self.assertEqual(result["median_mib"], 200)

    def test_late_rebound_invalidates_earlier_plateau(self):
        result = subject.steady_state(curve(lambda t: 200 if t < 45 else 250), 0, 60)
        self.assertEqual(result["status"], "reached")
        self.assertEqual(result["after_window_origin_seconds"], 45)
        self.assertEqual(result["median_mib"], 250)

    def test_end_of_window_spike_prevents_false_steady(self):
        result = subject.steady_state(curve(lambda t: 300 if t >= 59.95 else 200), 0, 60)
        self.assertEqual(result["status"], "not_reached")
        self.assertIsNone(result["median_mib"])

    def test_short_final_plateau_is_not_convergence(self):
        result = subject.steady_state(curve(lambda t: 400 - 5 * min(t, 55)), 0, 60)
        self.assertEqual(result["status"], "not_reached")

    def test_gap_is_missing_evidence_not_nonconvergence(self):
        points = [(t, y) for t, y in curve(lambda t: 200) if not 20 < t < 21]
        result = subject.steady_state(points, 0, 60)
        self.assertEqual(result["status"], "insufficient_evidence")

    def test_missing_endpoint_and_nonmonotonic_samples_are_rejected(self):
        points = curve(lambda t: 200)
        for broken in (points[40:], points[:-40], [*points, points[-1]]):
            self.assertEqual(subject.steady_state(broken, 0, 60)["status"], "insufficient_evidence")

    def test_absolute_and_relative_tolerances(self):
        for level, amplitude, reached in (
            (100, 3.9, True),
            (100, 4.1, False),
            (400, 7.9, True),
            (400, 8.2, False),
        ):
            with self.subTest(level=level, amplitude=amplitude):
                points = curve(lambda t: level + (amplitude if round(t * 20) % 2 else 0))
                state = subject.steady_state(points, 0, 60)
                self.assertEqual(state["status"] == "reached", reached)

    def test_post_write_has_its_own_time_origin(self):
        state = subject.steady_state(curve(lambda t: 300, end=100), 70, 100, startup_origin=-2)
        self.assertEqual(state["after_window_origin_seconds"], 0)
        self.assertEqual(state["after_group_start_seconds"], 72)

    def test_actual_timestamps_used_for_integral(self):
        self.assertEqual(subject.integral([(0, 100), (0.1, 200), (2, 200)]), 395)

    def test_boundary_interpolation_does_not_fill_gaps(self):
        self.assertEqual(
            subject.window([(-0.1, 100), (0.1, 200), (0.2, 200)], 0, 0.2, 0.25),
            [(0, 150), (0.1, 200), (0.2, 200)],
        )
        with self.assertRaisesRegex(ValueError, "gap"):
            subject.window([(-1, 100), (1, 200)], 0, 0.5, 0.25)


class ConditionTests(unittest.TestCase):
    def test_readback_rebound_and_correct_cpu_window(self):
        origin = 100_000_000_000

        def phase(name, seconds, memory, cpu):
            return {
                "name": name,
                "monotonic_ns": origin + int(seconds * 1e9),
                "counters": {
                    "memory.current": str(memory * subject.MIB),
                    "memory.peak": str(400 * subject.MIB),
                    "memory.stat": "anon 104857600\nfile 104857600\n",
                    "cpu.stat": f"usage_usec {cpu}\n",
                },
                "ksm": {"pid": 1, "starttime": 2, "cpu_seconds": cpu / 1e6},
            }

        worker = {
            "all_ready_ns": origin,
            "post_write_origin_ns": origin + 70_000_000_000,
            "before": phase("before", -2, 10, 0),
            "after": phase("after", 101, 10, 9_000_000),
            "first_full_read_ms": 123,
            "phases": [
                phase("idle-0", 0, 200, 2_000_000),
                phase("idle-60", 60, 200, 3_000_000),
                phase("readback", 61, 300, 4_000_000),
                phase("cow-100", 70, 300, 6_000_000),
                phase("post-write-30", 100, 300, 8_000_000),
            ],
        }
        points = [(i / 20, 200 if i / 20 < 61 else 300) for i in range(-60, 2021)]
        memory = [
            {"time_ns": origin + round(t * 1e9), "memory_current": y * subject.MIB}
            for t, y in points
        ]
        pss = [
            {"monotonic_ns": origin + round(t * 1e9), "pss_bytes": y * subject.MIB}
            for t, y in points[::10]
        ]
        row, _, _ = subject.analyze_condition(worker, memory, pss)
        self.assertTrue(row["evidence_sufficient"])
        self.assertEqual(row["idle_cpu_seconds"], 1)
        self.assertEqual(row["post_write_cpu_seconds"], 2)
        self.assertEqual(row["readback_change_mib"], 100)
        self.assertEqual(row["idle_steady"]["after_group_start_seconds"], 2)
        self.assertEqual(row["post_write_steady"]["median_mib"], 300)
        self.assertGreater(
            row["startup_through_post_write_mib_seconds"], row["idle_memory_mib_seconds"]
        )


if __name__ == "__main__":
    unittest.main()
