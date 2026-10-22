#!/usr/bin/env python3
"""Offline B-MEMORY-SCALE startup/steady-state analysis; never launches VMs."""

import argparse
import bisect
import json
import math
import statistics
from pathlib import Path

MIB = 2**20
STEADY = {
    "schema": "pvisor-observed-steady/v1",
    "minimum_seconds": 10,
    "absolute_range_mib": 4,
    "relative_range": 0.02,
    "memory_max_gap_seconds": 0.25,
    "pss_max_gap_seconds": 1.5,
    "scope": "earliest sampled suffix stable through the fixed observation window",
}


def window(curve, start, end, max_gap):
    """Clip at interpolated boundaries; retain every interior raw observation.

    Bracketing samples must cover both boundaries. Interpolation is only for
    integration/boundary values, never for filling an observation gap.
    """
    if not curve or not all(math.isfinite(x) and math.isfinite(y) and y >= 0 for x, y in curve):
        raise ValueError("missing or invalid samples")
    times = [x for x, _ in curve]
    if any(b <= a for a, b in zip(times, times[1:])):
        raise ValueError("nonmonotonic or duplicate sample timestamps")
    if times[0] > start or times[-1] < end:
        raise ValueError("samples do not cover the observation window")
    left = max(0, bisect.bisect_right(times, start) - 1)
    right = bisect.bisect_left(times, end)
    if any(times[i + 1] - times[i] > max_gap for i in range(left, right)):
        raise ValueError("sampling gap exceeds frozen tolerance")

    def boundary(t):
        i = bisect.bisect_left(times, t)
        if times[i] == t:
            return (t, curve[i][1])
        a, b = curve[i - 1], curve[i]
        return (t, a[1] + (b[1] - a[1]) * (t - a[0]) / (b[0] - a[0]))

    return [boundary(start), *[(x, y) for x, y in curve if start < x < end], boundary(end)]


def steady_state(curve, start, end, *, startup_origin=0):
    """Apply the frozen suffix rule; missing evidence and nonconvergence differ."""
    result = {
        "status": "not_reached",
        "window_seconds": end - start,
        "after_window_origin_seconds": None,
        "after_group_start_seconds": None,
        "median_mib": None,
        "min_mib": None,
        "max_mib": None,
    }
    try:
        points = window(curve, start, end, STEADY["memory_max_gap_seconds"])
    except ValueError as error:
        return {**result, "status": "insufficient_evidence", "reason": str(error)}
    # Include bracketing raw values in the range check so an interpolated
    # endpoint cannot conceal a spike straddling the window boundary.
    bracket_end = next(y for x, y in curve if x >= end)
    for i, (t, _) in enumerate(points):
        if end - t < STEADY["minimum_seconds"]:
            break
        values = [y for _, y in points[i:]]
        median = statistics.median(values)
        low, high = min(values + [bracket_end]), max(values + [bracket_end])
        threshold = max(STEADY["absolute_range_mib"], median * STEADY["relative_range"])
        if high - low <= threshold:
            return {
                **result,
                "status": "reached",
                "after_window_origin_seconds": t - start,
                "after_group_start_seconds": t - startup_origin,
                "median_mib": median,
                "min_mib": low,
                "max_mib": high,
                "range_mib": high - low,
                "allowed_range_mib": threshold,
                "observed_suffix_seconds": end - t,
            }
    return result


def integral(points):
    """Trapezoidal MiB-seconds using actual observation intervals."""
    return sum((b[0] - a[0]) * (a[1] + b[1]) / 2 for a, b in zip(points, points[1:]))


def analyze_condition(worker, memory, pss):
    from pool_compare import values

    origin = worker["all_ready_ns"]
    start = (worker["before"]["monotonic_ns"] - origin) / 1e9
    if start >= 0:
        raise ValueError("group startup must precede all-ready")
    charged = [((p["time_ns"] - origin) / 1e9, p["memory_current"] / MIB) for p in memory]
    resident = [((p["monotonic_ns"] - origin) / 1e9, p["pss_bytes"] / MIB) for p in pss]
    phases = {p["name"]: p for p in worker["phases"]}

    def phase_memory(name):
        return int(phases[name]["counters"]["memory.current"]) / MIB

    def cpu(name):
        return values(phases[name]["counters"]["cpu.stat"])["usage_usec"]

    result = {
        "startup_seconds_until_all_ready": -start,
        "ready_mib": phase_memory("idle-0"),
        "idle60_mib": phase_memory("idle-60"),
        "idle_steady": steady_state(charged, 0, 60, startup_origin=start),
        "whole_condition_peak_mib": int(worker["after"]["counters"]["memory.peak"]) / MIB,
        "first_full_read_ms": worker["first_full_read_ms"],
        "readback_mib": phase_memory("readback"),
        "readback_change_mib": phase_memory("readback") - phase_memory("idle-60"),
        "cow100_mib": phase_memory("cow-100"),
        "post_write30_mib": phase_memory("post-write-30"),
        "idle_cpu_seconds": (cpu("idle-60") - cpu("idle-0")) / 1e6,
        "post_write_cpu_seconds": (cpu("post-write-30") - cpu("cow-100")) / 1e6,
        "pool_storage": phases["idle-60"].get("pool_storage"),
        "memory_breakdown60_mib": {
            k: v / MIB
            for k, v in values(phases["idle-60"]["counters"]["memory.stat"]).items()
            if k in ("anon", "file", "kernel", "pagetables", "slab")
        },
    }
    offset = (worker["post_write_origin_ns"] - origin) / 1e9
    result["post_write_offset_seconds"] = offset
    result["post_write_steady"] = steady_state(charged, offset, offset + 30, startup_origin=start)
    try:
        full_window = window(charged, start, offset + 30, STEADY["memory_max_gap_seconds"])
        result["startup_through_post_write_mib_seconds"] = integral(full_window)
    except ValueError as error:
        result["lifecycle_integral_error"] = str(error)
    try:
        idle = window(charged, 0, 60, STEADY["memory_max_gap_seconds"])
        result["idle_memory_mib_seconds"] = integral(idle)
        result["idle_observed_peak_mib"] = max(y for _, y in idle)
        result["idle_at_seconds_mib"] = {
            str(t): window(charged, 0, t, STEADY["memory_max_gap_seconds"])[-1][1]
            for t in (2, 5, 10, 30, 60)
        }
    except ValueError as error:
        result["idle_metrics_error"] = str(error)
    for metric, curve, gap in (
        ("charged", charged, STEADY["memory_max_gap_seconds"]),
        ("pss", resident, STEADY["pss_max_gap_seconds"]),
    ):
        try:
            points = window(curve, start, 0, gap)
            result[f"startup_{metric}_peak_mib"] = max(y for _, y in points)
            result[f"startup_{metric}_mib_seconds"] = integral(points)
        except ValueError as error:
            result[f"startup_{metric}_error"] = str(error)
    first, last = phases["idle-0"]["ksm"], phases["idle-60"]["ksm"]
    if (first["pid"], first["starttime"]) != (last["pid"], last["starttime"]):
        raise ValueError("ksmd identity changed")
    result["global_ksmd_cpu_seconds"] = last["cpu_seconds"] - first["cpu_seconds"]
    result["evidence_sufficient"] = not any(k.endswith("_error") for k in result) and all(
        result[k]["status"] != "insufficient_evidence" for k in ("idle_steady", "post_write_steady")
    )
    return result, charged, resident


def render(rows, curves, output):
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    patterns = list(dict.fromkeys(r["pattern"] for r in rows))
    arms = list(dict.fromkeys(r["arm"] for r in rows))
    colors = {arm: plt.get_cmap("tab10")(i) for i, arm in enumerate(arms)}
    for view in ("startup-to-steady", "idle", "readback-and-write"):
        fig, axes = plt.subplots(len(patterns), 2, figsize=(14, 4 * len(patterns)), squeeze=False)
        for row, (charged, resident, worker) in zip(rows, curves):
            panel = axes[patterns.index(row["pattern"])]
            for ax, curve, metric in zip(panel, (charged, resident), ("charged", "pss")):
                startup = row["startup_seconds_until_all_ready"]
                finish = (worker["phases"][-1]["monotonic_ns"] - worker["all_ready_ns"]) / 1e9
                low, high, shift = (
                    (-startup, 60, startup)
                    if view == "startup-to-steady"
                    else ((0, 60, 0) if view == "idle" else (60, finish, 0))
                )
                points = [(x + shift, y) for x, y in curve if low <= x <= high]
                color = colors[row["arm"]]
                ax.plot(
                    *zip(*points),
                    ".-",
                    markersize=1.5,
                    linewidth=0.7,
                    label=row["arm"],
                    color=color,
                )
                if view != "readback-and-write":
                    ready = (
                        row["ready_mib"]
                        if metric == "charged"
                        else sum(p["pss_bytes"] for p in worker["phases"][0]["processes"]) / MIB
                    )
                    ax.scatter([shift], [ready], facecolors="none", edgecolors=[color], s=30)
                if metric == "charged":
                    state = (
                        row["post_write_steady"]
                        if view == "readback-and-write"
                        else row["idle_steady"]
                    )
                    base = row["post_write_offset_seconds"] if view == "readback-and-write" else 0
                    if state["status"] == "reached":
                        t = base + state["after_window_origin_seconds"] + shift
                        ax.hlines(
                            [state["min_mib"], state["max_mib"]],
                            t,
                            high + shift,
                            colors=[color],
                            linestyles="dotted",
                            linewidth=0.8,
                        )
                        ax.scatter([t], [state["median_mib"]], marker="x", color=color)
                if view == "readback-and-write":
                    for name in ("readback", "cow-25", "cow-100"):
                        phase = next(p for p in worker["phases"] if p["name"] == name)
                        t = (phase["monotonic_ns"] - worker["all_ready_ns"]) / 1e9
                        ax.axvline(t, color=color, linewidth=0.5, alpha=0.4)
        for pattern, panel in zip(patterns, axes):
            for ax, metric in zip(panel, ("Whole-group charge", "Sum of process PSS")):
                ax.set(
                    title=f"{pattern}: {metric}",
                    ylabel="MiB",
                    xlabel="Seconds after group startup"
                    if view == "startup-to-steady"
                    else "Seconds after all-ready",
                )
                ax.grid(alpha=0.2)
                ax.legend(fontsize=7)
        fig.suptitle(
            f"pVisor baseline: {view} (N=1 per condition)\n"
            "Open circle: Ready; x/dotted range: observed charged-memory steady suffix"
        )
        fig.tight_layout(rect=(0, 0, 1, 0.94))
        fig.savefig(output / f"{view}.svg")
        fig.savefig(output / f"{view}.png", dpi=120)
        plt.close(fig)


def analyze(cohort, output):
    """Audit retained evidence before producing a new, separate analysis."""
    from plot_pool import audit
    from scanner_compare import digest

    report, _ = audit(cohort)
    if any(not a.startswith("pvisor-") for a in report["arms"]):
        raise ValueError("baseline analysis accepts pvisor-* only")
    # Strengthen preflight auditing to retain all supporting hashes as well.
    for name, checksum in report["harness_files"].items():
        if digest(cohort / "harness" / Path(name).name) != checksum:
            raise ValueError("supporting harness changed")
    inputs = Path(json.loads((cohort / "0/config.json").read_text())["inputs"])
    receipt = report["receipt"]
    for name, checksum in {
        **receipt["hashes"],
        "source.tar.gz": receipt["source_archive_sha256"],
        "source-files.json": receipt["source_manifest_sha256"],
    }.items():
        if digest(inputs / name) != checksum:
            raise ValueError(f"frozen source/input changed: {name}")
    rows, curves = [], []
    for i, attempt in enumerate(report["attempts"]):
        folder = cohort / str(i)
        hashes = attempt["observation_hashes"]
        if set(hashes) != {"monitor.json", "pss.json", "guard.json"} or any(
            digest(folder / n) != h for n, h in hashes.items()
        ):
            raise ValueError("observation evidence changed or missing")
        worker = json.loads((folder / "worker.json").read_text())
        memory = json.loads((folder / "monitor.json").read_text())
        pss = json.loads((folder / "pss.json").read_text())
        if worker["condition"].get("baseline_state"):
            extra = attempt.get("baseline_evidence_hashes", {})
            if set(extra) != {"startup-observation.json", "baseline-guard.json"} or any(
                digest(folder / n) != h for n, h in extra.items()
            ):
                raise ValueError("baseline startup/guard evidence changed or missing")
            checks = json.loads((folder / "baseline-guard.json").read_text())
            if not checks or any(not p.get("valid") for p in checks):
                raise ValueError("baseline environment/source guard failed")
            initial = json.loads((folder / "startup-observation.json").read_text())
            memory = [initial, *[p for p in memory if p["time_ns"] > initial["time_ns"]]]
            pss = [
                initial["pss"],
                *[p for p in pss if p["monotonic_ns"] > initial["pss"]["monotonic_ns"]],
            ]
            ready_events = worker.get("ready_events", [])
            if len(ready_events) != 4 or {p["instance"] for p in ready_events} != {1, 2, 3, 4}:
                raise ValueError("individual VM Ready events missing")
        result, charged, resident = analyze_condition(worker, memory, pss)
        rows.append({"index": i, "arm": attempt["arm"], "pattern": attempt["pattern"], **result})
        curves.append((charged, resident, worker))
    output.mkdir()
    summary = {
        "schema": "pvisor-baseline-analysis/v1",
        "steady_rule": STEADY,
        "report_sha256": digest(cohort / "report.json"),
        "analyzer_sha256": digest(Path(__file__)),
        "conditions": rows,
        "evidence_sufficient": all(r["evidence_sufficient"] for r in rows),
    }
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    render(rows, curves, output)
    lines = [
        "# Local pVisor memory baseline",
        "",
        "N=1 per condition; exploratory observations.",
        "",
        "| Mode | Content | Startup→steady s | Ready→steady s | Steady MiB | Range MiB | 60s MiB | CPU s |",
        "|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        state = row["idle_steady"]

        def fmt(key):
            value = state.get(key)
            return f"{value:.2f}" if value is not None else state["status"]

        lines.append(
            f"| {row['arm']} | {row['pattern']} | {fmt('after_group_start_seconds')} | "
            f"{fmt('after_window_origin_seconds')} | {fmt('median_mib')} | "
            f"{fmt('range_mib')} | {row['idle60_mib']:.2f} | {row['idle_cpu_seconds']:.2f} |"
        )
    for name in ("startup-to-steady", "idle", "readback-and-write"):
        lines.extend(["", f"![{name}]({name}.svg)"])
    (output / "README.md").write_text("\n".join(lines) + "\n")
    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cohort", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    analyze(args.cohort, args.output)
