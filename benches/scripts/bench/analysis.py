#!/usr/bin/env python3
"""What the samples mean: aggregation, the noise floor, and verdicts.

Three rules, and they are the reason a number from this model can be quoted:

* **Variance is data.** Every cell is a median over rounds *with its range*, and
  a difference is only a claim when it clears both the metric's own materiality
  floor and the run's measured noise.
* **Two runs are comparable only if their methods are.** `comparability()`
  checks the fingerprint, the engine hash, the host identity and both host
  calibration probes, and refuses with a list of every blocker - named, not
  summarized.
* **An absence is reported, not zero.** A metric no arm could produce is listed
  with the reason the instrument gave; a comparison with no evidence says so
  instead of printing a difference between two nothings.
"""

from __future__ import annotations

import json
import statistics
from dataclasses import dataclass, field
from pathlib import Path

import instruments as inst
import model

#: Tolerances for the two host probes, carried over from the soak model's
#: incidents: a CPU probe that repeats to ~2 % and is blind to the loopback
#: ceiling, and a loopback probe that repeats to ~1.5 % and drops ~5 % under
#: load, against swings of 25-39 % that they exist to catch.
HOST_CALIBRATION_TOLERANCE_PCT = 25.0
HOST_LOOPBACK_TOLERANCE_PCT = 15.0
#: The fewest measured rounds a claim may rest on: one round is an anecdote,
#: and the model says so instead of reading a direction into it.
MIN_ROUNDS_FOR_CLAIM = 2
#: Values are rounded to this many decimals in the results file, which is more
#: than any of these instruments resolves and keeps a file diffable.
ROUND_TO = 6
#: How much of a method value the refusal message quotes before eliding.
METHOD_VALUE_CHARS = 60


@dataclass(frozen=True)
class Comparison:
    """The two measured sets one verdict compares.

    `noise` is the run's own resolution for this metric in *both* units - a
    relative one and the metric's own - because a verdict must compare like
    with like: a loss floor is a fraction of a percent, a throughput floor is a
    fraction of a rate, and mixing them would let one metric's scale decide
    another's claim.
    """

    metric: str
    values_a: list
    values_b: list
    label_a: str = "A"
    label_b: str = "B"
    noise: dict | None = None


@dataclass
class FilePair:
    """Two results files, loaded, with the pieces a comparison needs."""

    a: dict
    b: dict
    shared: list = field(default_factory=list)
    floors_a: dict = field(default_factory=dict)
    floors_b: dict = field(default_factory=dict)
    aa_a: bool = False
    aa_b: bool = False


# --- aggregation ------------------------------------------------------------
def summarize(results: dict) -> dict:
    """Every cell's per-arm statistics, plus the run's own noise floor."""
    buckets: dict = {}
    failures: list = []
    for sample in results.get("samples", []):
        if sample.get("warmup"):
            continue
        key = (sample["scenario"], sample["cell"])
        bucket = buckets.setdefault(
            key, {"scenario": key[0], "cell": key[1], "arms": {}}
        )
        arm = bucket["arms"].setdefault(sample["arm"], {"rounds": [], "failed": 0})
        if not sample.get("ok"):
            arm["failed"] += 1
            failures.append(
                {
                    "arm": sample["arm"],
                    "scenario": key[0],
                    "cell": key[1],
                    "round": sample["round"],
                    "reason": sample.get("reason", ""),
                }
            )
            continue
        arm["rounds"].append(sample)
    return {
        "cells": [_stats_of(b) for b in buckets.values()],
        "failures": failures,
        "noise": noise_floor(results),
    }


def _stats_of(bucket: dict) -> dict:
    arms: dict = {}
    for arm, data in bucket["arms"].items():
        rows = data["rounds"]
        metrics: dict = {}
        for metric in model.metric_ids():
            values = [
                row["metrics"][metric]
                for row in rows
                if row.get("metrics", {}).get(metric) is not None
            ]
            if values:
                metrics[metric] = _stat(values)
        arms[arm] = {
            "rounds": len(rows),
            "failed": data["failed"],
            "metrics": metrics,
            "unavailable": _unavailable_of(rows),
        }
    return {"scenario": bucket["scenario"], "cell": bucket["cell"], "arms": arms}


def _stat(values: list) -> dict:
    median = statistics.median(values)
    return {
        "n": len(values),
        "median": round(median, ROUND_TO),
        "min": round(min(values), ROUND_TO),
        "max": round(max(values), ROUND_TO),
        "values": [round(v, ROUND_TO) for v in values],
        "spread_pct": (
            round((max(values) - min(values)) / abs(median) * 100, 3)
            if median
            else None
        ),
    }


def _unavailable_of(rows: list) -> dict:
    """The typed absences of a cell: metric -> reason, first one wins.

    A bare string is accepted as well as the `{"reason": ...}` record: a cell
    that says why in one field must not crash the analysis, and the analysis is
    where a reader finds out what was not measured.
    """
    out: dict = {}
    for row in rows:
        for metric, info in (row.get("unavailable") or {}).items():
            reason = info if isinstance(info, str) else info.get("reason", "")
            out.setdefault(metric, reason)
    return out


# --- the noise floor --------------------------------------------------------
#: An arm id ending in one of these is a *twin* of the arm before the suffix:
#: the same configuration measured again. `~aa` is the run's own noise floor
#: (the A/A pair), `~b` is the second build of an A/B.
TWIN_SUFFIXES = ("~aa", "~b")


#: The suffix that marks the *noise floor's* pair. Only this one measures the
#: run against itself: `~b` is the second build of an A/B, i.e. the thing under
#: test, and using it as a floor would let a real difference become the
#: resolution.
AA_SUFFIX = "~aa"


def _aa_pair(results: dict) -> tuple:
    """The A/A twin of an arm, if the run carried one."""
    return _twin_pair(results, AA_SUFFIX)


def _twin_pair(results: dict, suffix: str) -> tuple:
    ids = {a["id"] for a in results["meta"]["arms"]}
    for arm_id in sorted(ids):
        if arm_id.endswith(suffix) and arm_id[: -len(suffix)] in ids:
            return arm_id[: -len(suffix)], arm_id
    return "", ""


def _twin_pairs(results: dict) -> list:
    """Every (base, twin) pair the run carried, A/A first."""
    pairs = []
    for suffix in TWIN_SUFFIXES:
        pair = _twin_pair(results, suffix)
        if pair[0]:
            pairs.append(pair)
    return pairs


def round_values(
    results: dict, arm: str, scenario: str, cell: str, metric: str
) -> list:
    """Every measured round's value of one metric, for one arm and cell."""
    values = []
    for sample in results.get("samples", []):
        if sample.get("warmup") or not sample.get("ok"):
            continue
        if sample["arm"] != arm or sample["scenario"] != scenario:
            continue
        if sample["cell"] != cell:
            continue
        value = sample.get("metrics", {}).get(metric)
        if value is not None:
            values.append(value)
    return values


def _cell_keys(results: dict) -> list:
    """Every (scenario, cell) the run actually measured, warm-ups excluded.

    Derived from the samples rather than from an existing summary: the noise
    floor is computed *while* the summary is being built, and reading the
    summary it is part of would see an empty run.
    """
    keys: list = []
    for sample in results.get("samples", []):
        if sample.get("warmup"):
            continue
        key = (sample["scenario"], sample["cell"])
        if key not in keys:
            keys.append(key)
    return keys


def noise_floor(results: dict) -> dict:
    """What this run can resolve: the A/A difference and the control's drift.

    Three readings, all of them data rather than a threshold:

    * the **A/A difference** - the two halves of the same arm, paired round by
      round, so the host's drift between rounds cancels and what is left is
      what "two arms measured in this run" can differ by when nothing differs.
      This is the honest floor for a claim, and it is why the model insists on
      the twin.
    * the **A/A scatter** - the within-arm half-range of the twin, which is the
      same quantity for a run whose arms are not paired round by round.
    * the **control's half-range** - the same path with no tool in it, i.e. the
      host's own drift over the run.

    The floor is the largest of them, per cell and per metric, because a quiet
    instrument must not license a claim the host then makes untrue. It is kept
    per cell: a run can resolve a bulk difference and not a round-trip one, and
    one number for the whole run would hide exactly that.
    """
    base, twin = _aa_pair(results)
    by_cell: dict = {}
    for scenario, cell in _cell_keys(results):
        entries = {}
        spot = FloorSpot(
            results=results, base=base, twin=twin, scenario=scenario, cell=cell
        )
        for metric in model.metric_ids():
            entry = _floor_entry(spot, metric)
            readings = [
                entry.get(f"{name}_{unit}")
                for name in FLOOR_READINGS
                for unit in ("pct", "abs")
            ]
            if any(value is not None for value in readings):
                entries[metric] = entry
        if entries:
            by_cell[_cell_id(scenario, cell)] = entries
    return {
        "aa_arm": twin,
        "base_arm": base,
        "metrics": _aggregate_floors(by_cell),
        "by_cell": by_cell,
    }


#: The readings a floor is the maximum of, in both units: a percentage of the
#: reference and the metric's own unit. A verdict picks by the metric's
#: materiality kind, so a loss floor never decides a throughput claim.
FLOOR_READINGS = ("aa_diff", "aa_scatter", "control")


@dataclass(frozen=True)
class FloorSpot:
    """Where a floor is measured: the run, the A/A pair, and the cell."""

    results: dict
    base: str
    twin: str
    scenario: str
    cell: str


def _floor_entry(spot: FloorSpot, metric: str) -> dict:
    readings = {
        "aa_diff_pct": None,
        "aa_diff_abs": None,
        "aa_scatter_pct": None,
        "aa_scatter_abs": None,
        "control_pct": None,
        "control_abs": None,
    }
    base_values = round_values(
        spot.results, spot.base, spot.scenario, spot.cell, metric
    )
    twin_values = round_values(
        spot.results, spot.twin, spot.scenario, spot.cell, metric
    )
    control_values = round_values(
        spot.results, "control", spot.scenario, spot.cell, metric
    )
    if spot.twin:
        readings |= _paired_readings(base_values, twin_values)
        readings |= _scatter_readings(twin_values, "aa_scatter")
    readings |= _scatter_readings(control_values, "control")
    entry = dict(readings) | {"metric": metric}
    entry["floor_pct"] = _floor_of(readings, "pct")
    entry["floor_abs"] = _floor_of(readings, "abs")
    return entry


def _paired_readings(base_values: list, twin_values: list) -> dict:
    """The paired difference between the two halves of one arm.

    Paired by round so the host's drift cancels; the median of the paired
    differences rather than the difference of the medians, so one bad round
    cannot manufacture a floor for the whole run.
    """
    pairs = [
        (a, b) for a, b in zip(base_values, twin_values, strict=False) if b is not None
    ]
    if not pairs:
        return {"aa_diff_pct": None, "aa_diff_abs": None}
    abs_delta = statistics.median([a - b for a, b in pairs])
    ratios = [(a - b) / abs(b) * 100.0 for a, b in pairs if b]
    return {
        "aa_diff_pct": round(abs(statistics.median(ratios)), 3) if ratios else None,
        "aa_diff_abs": round(abs(abs_delta), 3),
    }


def _scatter_readings(values: list, prefix: str) -> dict:
    """The within-arm half-range, in both units."""
    both = _half_range(values)
    return {
        f"{prefix}_pct": None if both is None else round(both[0], 3),
        f"{prefix}_abs": None if both is None else round(both[1], 3),
    }


def _floor_of(readings: dict, unit: str) -> float | None:
    """The largest reading in one unit: a claim must clear all of them."""
    keys = (f"{name}_{unit}" for name in FLOOR_READINGS)
    values = [readings[key] for key in keys if readings.get(key) is not None]
    return round(max(values), 3) if values else None


def _cell_id(scenario: str, cell: str) -> str:
    return f"{scenario}|{cell}"


def _aggregate_floors(by_cell: dict) -> dict:
    """One floor per metric: the worst cell's, with the cell named."""
    metrics: dict = {}
    for cell_id, entries in by_cell.items():
        for metric, entry in entries.items():
            current = metrics.get(metric)
            if current is None or (entry["floor_pct"] or 0) > (
                current["floor_pct"] or 0
            ):
                metrics[metric] = dict(entry) | {"worst_cell": cell_id}
    return metrics


def floor_for(noise: dict, scenario: str, cell: str, metric: str) -> dict:
    """The floor that applies to one cell's metric, or the run's worst."""
    entry = ((noise.get("by_cell") or {}).get(_cell_id(scenario, cell)) or {}).get(
        metric
    )
    if entry:
        return entry
    return (noise.get("metrics") or {}).get(metric) or {}


def _half_range(values: list):
    """(max - min) / 2 as a percentage of the median and in the metric's unit."""
    if len(values) < MIN_ROUNDS_FOR_CLAIM:
        return None
    span = (max(values) - min(values)) / 2.0
    median = statistics.median(values)
    if not median:
        return None
    return (span / abs(median) * 100.0, span)


def _sign_agreement(values_a: list, values_b: list):
    """How many paired rounds moved the same way, as a fraction."""
    diffs = [a - b for a, b in zip(values_a, values_b, strict=False)]
    if not diffs:
        return None
    positive = sum(1 for d in diffs if d > 0)
    return round(max(positive, len(diffs) - positive) / len(diffs), 3)


# --- verdicts ---------------------------------------------------------------
def verdict(cmp: Comparison) -> dict:
    """One metric's verdict between two measured sets of rounds.

    `claim` requires every paired round to agree in sign *and* the median
    difference to clear both the metric's materiality floor and the run's noise
    floor. Everything else is `directional` (a direction, but a disagreement or
    a floor is in the way), `single-round`, or `indistinguishable` - and
    `unavailable` when one side has no reading at all.
    """
    metric = model.METRICS[cmp.metric]
    if not cmp.values_a or not cmp.values_b:
        return {
            "metric": cmp.metric,
            "unit": metric.unit,
            "a": cmp.label_a,
            "b": cmp.label_b,
            "verdict": "unavailable",
            "delta_pct": None,
            "delta_abs": None,
            "median_a": None,
            "median_b": None,
            "threshold": None,
            "sign_agreement": None,
            "rounds": [len(cmp.values_a), len(cmp.values_b)],
            "better": None,
            "reason": (
                f"{cmp.label_a}: {len(cmp.values_a)} samples, "
                f"{cmp.label_b}: {len(cmp.values_b)}"
            ),
        }
    base = _verdict_base(cmp, metric)
    base["verdict"] = _verdict_state(cmp, metric, base)
    if base["verdict"] in ("claim", "directional") and metric.direction != "none":
        improved = (base["delta_abs"] > 0) == (metric.direction == "higher")
        base["better"] = cmp.label_a if improved else cmp.label_b
    return base


def _verdict_base(cmp: Comparison, metric: model.Metric) -> dict:
    med_a, med_b = statistics.median(cmp.values_a), statistics.median(cmp.values_b)
    delta_abs = med_a - med_b
    delta_pct = (delta_abs / abs(med_b) * 100.0) if med_b else None
    noise = cmp.noise or {}
    relative = metric.materiality[0] == "rel"
    floor = (noise.get("pct") if relative else noise.get("abs")) or 0.0
    threshold = max(metric.materiality[1], floor)
    measured = (delta_pct or 0.0) if relative else delta_abs
    return {
        "metric": cmp.metric,
        "unit": "pct" if relative else metric.unit,
        "a": cmp.label_a,
        "b": cmp.label_b,
        "median_a": round(med_a, ROUND_TO),
        "median_b": round(med_b, ROUND_TO),
        "delta_abs": round(delta_abs, ROUND_TO),
        "delta_pct": None if delta_pct is None else round(delta_pct, 3),
        "sign_agreement": _sign_agreement(cmp.values_a, cmp.values_b),
        "threshold": round(threshold, 3),
        "noise": {k: (None if v is None else round(v, 3)) for k, v in noise.items()},
        "material": abs(measured) >= threshold,
        "rounds": [len(cmp.values_a), len(cmp.values_b)],
        "better": None,
    }


def _verdict_state(cmp: Comparison, metric: model.Metric, base: dict) -> str:
    del cmp  # the state is a function of the metric and the computed base
    if metric.direction == "none":
        return "context"
    if min(base["rounds"]) < MIN_ROUNDS_FOR_CLAIM:
        return "single-round"
    if not base["material"]:
        return "indistinguishable"
    if base["sign_agreement"] == 1.0:
        return "claim"
    return "directional"


def verdicts(results: dict) -> list:
    """Every verdict this run supports: each tool arm against the control.

    The control is the reference, not a competitor: it is the same topology and
    backend with no tool in the path, so "the arm differs from the control" is a
    statement about the architecture, while "one arm differs from another" is a
    statement about a choice.
    """
    out: list = []
    noise = (results.get("summary", {}).get("noise", {}) or {}).get("metrics", {})
    twins = _twin_pairs(results)
    for summary_cell in results.get("summary", {}).get("cells", []):
        scenario, cell = summary_cell["scenario"], summary_cell["cell"]
        for label_a, label_b in _pairs_of(summary_cell, twins):
            for metric_id in model.metric_ids():
                values_a = round_values(results, label_a, scenario, cell, metric_id)
                values_b = round_values(results, label_b, scenario, cell, metric_id)
                if not values_a and not values_b:
                    continue
                entry = verdict(
                    Comparison(
                        metric=metric_id,
                        values_a=values_a,
                        values_b=values_b,
                        label_a=label_a,
                        label_b=label_b,
                        noise=floor_for(noise, scenario, cell, metric_id),
                    )
                )
                entry |= {
                    "scenario": scenario,
                    "cell": cell,
                    "aa": label_b.endswith(AA_SUFFIX),
                    "twin": label_b.endswith(TWIN_SUFFIXES),
                }
                out.append(entry)
    return out


def _pairs_of(summary_cell: dict, twins: list) -> list:
    """The comparisons one cell supports: each arm against the control, then
    every twin against its base (the A/A floor and the A/B pair)."""
    arms = summary_cell["arms"]
    pairs = []
    if "control" in arms:
        pairs = [
            (a, "control")
            for a in sorted(arms)
            if a != "control" and not a.endswith(TWIN_SUFFIXES)
        ]
    pairs += [(base, twin) for base, twin in twins if base in arms and twin in arms]
    return pairs


# --- comparability ----------------------------------------------------------
def comparability(meta_a: dict, meta_b: dict) -> list:
    """Every reason these two results files may not be compared, named."""
    blockers: list = []
    for tag, meta in (("A", meta_a), ("B", meta_b)):
        if meta.get("model") != "bench":
            blockers.append(f"{tag} is not a bench results file")
    if blockers:
        return blockers
    return _method_blockers(meta_a, meta_b) + _host_blockers(meta_a, meta_b)


def _method_blockers(meta_a: dict, meta_b: dict) -> list:
    if meta_a.get("fingerprint") == meta_b.get("fingerprint"):
        return []
    method_a = meta_a.get("method", {})
    method_b = meta_b.get("method", {})
    blockers = [
        f"method key {key!r} differs: {_short(method_a.get(key))} vs "
        f"{_short(method_b.get(key))}"
        for key in sorted(set(method_a) | set(method_b))
        if method_a.get(key) != method_b.get(key)
    ]
    return blockers or ["method fingerprints differ"]


def _host_blockers(meta_a: dict, meta_b: dict) -> list:
    host_a = (meta_a.get("provenance") or {}).get("host") or {}
    host_b = (meta_b.get("provenance") or {}).get("host") or {}
    blockers: list = []
    for probe, tolerance in (
        ("calibration", HOST_CALIBRATION_TOLERANCE_PCT),
        ("loopback", HOST_LOOPBACK_TOLERANCE_PCT),
    ):
        blockers += _probe_blocker(host_a, host_b, probe, tolerance)
    identity_a = (host_a.get("identity") or {}).get("host_id")
    identity_b = (host_b.get("identity") or {}).get("host_id")
    if identity_a and identity_b and identity_a != identity_b:
        blockers.append(f"different hosts: {identity_a} vs {identity_b}")
    return blockers


def _probe_blocker(host_a: dict, host_b: dict, probe: str, tolerance: float) -> list:
    read_a, read_b = _probe(host_a, probe), _probe(host_b, probe)
    if read_a is None or read_b is None:
        missing = "A" if read_a is None else "B"
        message = (
            f"{probe} probe missing in {missing} (a file predating the probe "
            "is unverifiable, not comparable)"
        )
        return [message]
    if not read_a or not read_b:
        return [f"{probe} probe reads zero in one file"]
    drift = abs(read_a - read_b) / max(read_a, read_b) * 100.0
    if drift > tolerance:
        message = (
            f"{probe} probes differ by {drift:.1f} % (limit {tolerance:.0f} %): "
            f"{read_a} vs {read_b}"
        )
        return [message]
    return []


def _probe(host: dict, name: str):
    probe = host.get(name) or {}
    if not probe:
        return None
    if not probe.get("ok"):
        return 0.0
    return probe.get("median")


def _short(value) -> str:
    text = json.dumps(value, default=str)
    if len(text) <= METHOD_VALUE_CHARS:
        return text
    return text[: METHOD_VALUE_CHARS - 3] + "..."


# --- rendering --------------------------------------------------------------
def _fmt(metric: model.Metric, value) -> str:
    return "-" if value is None else metric.format(value)


#: The columns a cell's table shows, per workload kind: the headline first,
#: then the numbers that say what the headline cost. A metric the kind cannot
#: produce is not a column, so no cell shows a row of dashes.
KIND_COLUMNS: dict = {
    "bulk": ("throughput_gbps", "cpu_s_per_gbit", "wire_per_visitor_byte"),
    "bulk-pair": ("throughput_gbps", "cpu_s_per_gbit", "wire_per_visitor_byte"),
    "rr": ("rate_per_s", "rtt_p99_ms", "setup_p50_ms", "wire_per_visitor_byte"),
    "udp": ("udp_loss_pct", "udp_recv_mbit", "udp_gap_p99_ms", "udp_rtt_p99_ms"),
    "udp-ladder": ("udp_loss_pct", "udp_recv_mbit", "cpu_s_per_gbit"),
}


def _row_metrics(scenario: str) -> list:
    spec = model.SCENARIOS.get(scenario)
    if spec is None:
        return []
    wanted = KIND_COLUMNS.get(spec.kind, ())
    if spec.headline and spec.headline not in wanted:
        wanted = (spec.headline, *wanted)
    return [m for m in wanted if m in model.METRICS]


def _cell_table(summary_cell: dict, markdown: bool) -> list:
    scenario, cell = summary_cell["scenario"], summary_cell["cell"]
    metrics = _row_metrics(scenario)
    header = (
        "| arm | "
        + " | ".join(f"{m} ({model.METRICS[m].unit})" for m in metrics)
        + " |"
    )
    lines = [f"### {scenario}{f' [{cell}]' if cell else ''}", "", header]
    lines.append("|---" * (len(metrics) + 1) + "|")
    lines.extend(
        _cell_row(summary_cell["arms"][arm_id], arm_id, metrics, markdown)
        for arm_id in sorted(summary_cell["arms"])
    )
    lines.append("")
    return lines


def _cell_row(arm: dict, arm_id: str, metrics: list, markdown: bool) -> str:
    row = [_metric_cell(arm, metric_id, markdown) for metric_id in metrics]
    label = arm_id + (f" ({arm['failed']} failed)" if arm["failed"] else "")
    return f"| {label} | " + " | ".join(row) + " |"


def _metric_cell(arm: dict, metric_id: str, markdown: bool) -> str:
    stat = arm["metrics"].get(metric_id)
    if not stat:
        return "-"
    metric = model.METRICS[metric_id]
    if not markdown:
        return (
            f"{_fmt(metric, stat['median'])} "
            f"[{_fmt(metric, stat['min'])}..{_fmt(metric, stat['max'])}]"
        )
    return (
        f"{_fmt(metric, stat['median'])} "
        f"({_fmt(metric, stat['min'])} to {_fmt(metric, stat['max'])})"
    )


def render(results: dict, markdown: bool = True) -> str:
    """The run's own table: each cell, each arm, each headline metric.

    The floors are part of the table in both shapes, because a number without
    the resolution it was measured at is a number a reader will over-read.
    """
    lines: list = []
    for summary_cell in results.get("summary", {}).get("cells", []):
        lines += _cell_table(summary_cell, markdown)
    lines += _noise_table(results, markdown)
    return "\n".join(lines)


def _noise_table(results: dict, markdown: bool = True) -> list:
    noise = results.get("summary", {}).get("noise", {}) or {}
    metrics = noise.get("metrics", {})
    base, twin = noise.get("base_arm"), noise.get("aa_arm")
    header = (
        f"Floors this run measured, per metric (A/A pair: {base or '-'} vs "
        f"{twin or '-'}; the largest reading is what a claim must clear):"
    )
    if not markdown:
        lines = [header]
        for metric_id, entry in sorted(metrics.items()):
            lines.append(
                f"  {metric_id:<26} floor {_floor_cell(entry):>14}  "
                f"(A/A diff {_pct_cell(entry.get('aa_diff_pct'))}, "
                f"A/A scatter {_pct_cell(entry.get('aa_scatter_pct'))}, "
                f"control drift {_pct_cell(entry.get('control_pct'))})"
            )
        return [*lines, ""]
    columns = (
        "| metric | A/A difference | A/A scatter | control drift | floor | worst cell |"
    )
    lines = [header, "", columns, "|---|---|---|---|---|---|"]
    for metric_id, entry in sorted(metrics.items()):
        lines.append(
            f"| {metric_id} | {_pct_cell(entry.get('aa_diff_pct'))} | "
            f"{_pct_cell(entry.get('aa_scatter_pct'))} | "
            f"{_pct_cell(entry.get('control_pct'))} | "
            f"{_floor_cell(entry)} | {entry.get('worst_cell', '-')} |"
        )
    lines.append("")
    return lines


def _pct_cell(value) -> str:
    return "-" if value is None else f"{value:.2f} %"


def _floor_cell(entry: dict) -> str:
    """A floor in the unit its metric is judged in."""
    metric = model.METRICS.get(entry.get("metric", ""))
    if metric is not None and metric.materiality[0] == "abs":
        return _num_cell(entry.get("floor_abs"), metric.unit)
    return _pct_cell(entry.get("floor_pct"))


def _num_cell(value, unit: str) -> str:
    return "-" if value is None else f"{value:g} {unit}"


def _threshold_cell(entry: dict) -> str:
    value = entry.get("threshold")
    if value is None:
        return "-"
    unit = entry.get("unit") or ""
    return f"{value:g} {unit}".strip()


def verdicts_text(results: dict) -> str:
    """The verdict lines: what this run claims, and what it refuses to.

    A claim or a direction is printed in full; the metrics that merely agreed
    with the control are counted, not listed - a page of "indistinguishable"
    lines hides the one line that matters.
    """
    header = (
        "verdicts (each tool arm against the control; a claim needs every round "
        "to agree in sign and both the materiality and noise floors cleared):"
    )
    lines = [header]
    claims = 0
    quiet = 0
    for (scenario, cell), entries in _grouped_verdicts(results).items():
        where = scenario + (f"[{cell}]" if cell else "")
        shown: list = []
        for entry in entries:
            if entry["verdict"] in ("claim", "directional"):
                shown.append(_verdict_text_line(entry))
                claims += entry["verdict"] == "claim"
            elif entry["verdict"] == "unavailable" and entry.get("headline"):
                shown.append(_verdict_text_line(entry))
            else:
                quiet += 1
        if shown:
            lines.append(f"  {where}:")
            lines += [f"  {line}" for line in shown]
    if not claims:
        lines.append(
            "  no claim cleared the floors: every difference this run saw is "
            "inside its own scatter or below its metric's materiality"
        )
    lines.append(f"  ({quiet} further metric comparisons were indistinguishable)")
    lines += _aa_lines(results)
    return "\n".join(lines)


def _aa_lines(results: dict) -> list:
    """The twins' own verdicts: the run's resolution, stated.

    A twin is the same arm measured twice - the A/A pair, or the two builds of
    an A/B. Any metric on which it claims a difference is a metric this run
    cannot resolve, and saying so is the point of carrying it.
    """
    twin_verdicts = [v for v in verdicts(results) if v.get("twin")]
    if not twin_verdicts:
        return []
    lines: list = []
    for base, twin in _twin_pairs(results):
        pair = [v for v in twin_verdicts if v["a"] == base and v["b"] == twin]
        if not pair:
            continue
        unresolved = [v for v in pair if v["verdict"] == "claim"]
        lines.append(
            f"  {twin}: {len(pair)} metric comparisons of {base} against a "
            f"second copy of itself, {len(unresolved)} of them claims (each is "
            "a metric this run cannot resolve)"
        )
        lines += [
            f"    {v['scenario']}[{v['cell']}] {v['metric']}: {_delta_cell(v)}"
            for v in unresolved
        ]
    return lines


def _grouped_verdicts(results: dict) -> dict:
    """The run's verdicts, grouped by cell, headline first, A/A twins last."""
    grouped: dict = {}
    for entry in verdicts(results):
        if entry.get("aa"):
            continue
        key = (entry["scenario"], entry["cell"])
        spec = model.SCENARIOS.get(entry["scenario"])
        entry["headline"] = bool(spec and entry["metric"] == spec.headline)
        grouped.setdefault(key, []).append(entry)
    for entries in grouped.values():
        entries.sort(key=lambda e: (not e["headline"], e["metric"]))
    return grouped


def _verdict_text_line(entry: dict) -> str:
    head = _verdict_head(entry)
    if entry["verdict"] == "unavailable":
        return f"{head}: unavailable ({entry.get('reason', '')})"
    detail = (
        f"{_delta_cell(entry)} (threshold {_threshold_cell(entry)}, "
        f"sign agreement {entry['sign_agreement']}, n={entry['rounds']})"
    )
    state = entry["verdict"]
    if state == "claim":
        return f"{head}: CLAIM {entry['better']} better, {detail}"
    if state == "directional":
        return f"{head}: directional ({entry['better'] or '?'} better), {detail}"
    if state == "single-round":
        return f"{head}: single-round, not a verdict ({detail})"
    if state == "context":
        return f"{head}: context only, {detail}"
    return f"{head}: indistinguishable, {detail}"


def _verdict_head(entry: dict) -> str:
    return f"{entry['a']} vs {entry['b']} {entry['metric']}"


def _delta_cell(entry: dict) -> str:
    """A difference in the unit a reader can act on.

    A ratio metric's difference is a percentage of the reference; an absolute
    metric's (loss, descriptors, packets) is a difference in its own unit - a
    percentage *of* a percentage hides the number that matters.
    """
    metric = model.METRICS[entry["metric"]]
    if metric.materiality[0] == "abs" or entry["delta_pct"] is None:
        return f"{entry['delta_abs']:+.{metric.precision}f} {metric.unit}"
    return f"{entry['delta_pct']:+.2f} %"


# --- cross-file comparison --------------------------------------------------
def compare_files(results_a: dict, results_b: dict) -> tuple:
    """A/B across two results files: (text, refused, claims).

    The files must describe the same method - `comparability` names every
    reason they do not - and the comparison is made *per arm id*, so a build
    pair reads as "the same arm, two binaries" and a whole-run pair reads as
    "every arm, the same change". The control arm's own delta is reported first:
    it is the host's drift over the two runs, and a change smaller than it is
    not resolvable.
    """
    blockers = comparability(results_a.get("meta", {}), results_b.get("meta", {}))
    if blockers:
        lines = ["refused: these files are not comparable"]
        lines += [f"  - {b}" for b in blockers]
        lines.append(
            "(re-run both sides with the same profile, topology and host, or "
            "compare each against its own control)"
        )
        return "\n".join(lines), True, []
    pair = _file_pair(results_a, results_b)
    lines = _compare_header(pair)
    if not (pair.aa_a and pair.aa_b):
        lines.append(_no_aa_warning(pair))
    arms = _shared_arms(pair)
    lines += _drift_lines(pair, arms)
    claims = _claim_lines(pair, arms, lines)
    return "\n".join(lines), False, claims


def _no_aa_warning(pair: FilePair) -> str:
    """Say what a between-file claim needs, and what it therefore is not.

    The runs' own scatter is what an A/A twin measures; without one in both
    files a difference between two runs of the *same* code cannot be told from
    a difference the change made. Every claim below is downgraded to
    `directional` for that reason.
    """
    missing = [tag for tag, has in (("A", pair.aa_a), ("B", pair.aa_b)) if not has]
    return (
        f"note: {' and '.join(missing)} carried no A/A twin, so this comparison "
        "has no measurement of its own run-to-run scatter: every difference "
        "below is reported as directional, never as a claim (re-run with --aa)"
    )


def _file_pair(results_a: dict, results_b: dict) -> FilePair:
    cells_a = _cells_by_key(results_a)
    shared = [k for k in cells_a if k in _cells_by_key(results_b)]
    return FilePair(
        a=results_a,
        b=results_b,
        shared=shared,
        floors_a=_floors(results_a),
        floors_b=_floors(results_b),
        aa_a=bool(_aa_pair(results_a)[0]),
        aa_b=bool(_aa_pair(results_b)[0]),
    )


def _compare_header(pair: FilePair) -> list:
    meta_a, meta_b = pair.a["meta"], pair.b["meta"]
    return [
        f"A: {_file_label(meta_a)}",
        f"B: {_file_label(meta_b)}",
        f"fingerprint {meta_a['fingerprint']} (identical)",
        "",
    ]


def _file_label(meta: dict) -> str:
    binaries = (meta.get("provenance") or {}).get("binaries") or [{}]
    shown = " | ".join(
        f"{b.get('version', '?')} {b.get('sha256', '?')}" for b in binaries if b
    )
    return (
        f"{meta.get('profile')} {str(meta.get('started', ''))[:19]} "
        f"{shown or '(control only)'}"
    )


def _drift_lines(pair: FilePair, arms: list) -> list:
    if "control" not in arms:
        return []
    lines = ["host drift (the control arm against itself, across the runs):"]
    lines.extend(
        f"  {_verdict_line(entry)}"
        for entry in _pair_verdicts(pair, "control")
        if entry["verdict"] != "indistinguishable"
    )
    return [*lines, ""]


def _claim_lines(pair: FilePair, arms: list, lines: list) -> list:
    claims: list = []
    for arm in arms:
        if arm == "control":
            continue
        for entry in _pair_verdicts(pair, arm):
            if entry["verdict"] == "claim":
                claims.append(entry)
            elif entry["verdict"] in ("directional", "single-round"):
                lines.append(_verdict_line(entry))
    if claims:
        lines.append("claims (every paired round agreed, and both floors cleared):")
        lines.extend(f"  {_verdict_line(entry)}" for entry in claims)
    else:
        lines.append(
            "no claim: every between-file difference is inside the floors or "
            "below its metric's materiality"
        )
    return claims


def _shared_arms(pair: FilePair) -> list:
    if not pair.shared:
        return []
    key = pair.shared[0]
    arms_a = set(_cells_by_key(pair.a)[key]["arms"])
    arms_b = set(_cells_by_key(pair.b)[key]["arms"])
    return [a for a in sorted(arms_a & arms_b) if not a.endswith("~aa")]


def _cells_by_key(results: dict) -> dict:
    cells = results.get("summary", {}).get("cells", [])
    return {(c["scenario"], c["cell"]): c for c in cells}


def _floors(results: dict) -> dict:
    noise = (results.get("summary", {}).get("noise", {}) or {}).get("metrics", {})
    return {
        k: v
        for k, v in noise.items()
        if v.get("floor_pct") is not None or v.get("floor_abs") is not None
    }


def _pair_verdicts(pair: FilePair, arm: str) -> list:
    out: list = []
    cells_a, cells_b = _cells_by_key(pair.a), _cells_by_key(pair.b)
    for scenario, cell in pair.shared:
        for metric_id in model.metric_ids():
            values_a = _values_of(cells_a[(scenario, cell)], arm, metric_id)
            values_b = _values_of(cells_b[(scenario, cell)], arm, metric_id)
            if not values_a and not values_b:
                continue
            entry = verdict(
                Comparison(
                    metric=metric_id,
                    values_a=values_a,
                    values_b=values_b,
                    label_a=f"A.{arm}",
                    label_b=f"B.{arm}",
                    noise=_worst_floor(pair, metric_id),
                )
            )
            entry |= {"scenario": scenario, "cell": cell, "arm": arm, "aa": False}
            out.append(_without_floor(entry, pair))
    return out


def _without_floor(entry: dict, pair: FilePair) -> dict:
    """A between-file difference with no A/A twin is directional, not a claim.

    The files' own resolution is what licenses a claim; with no twin in one of
    them, this comparison cannot say whether the difference came from the change
    or from the runs.
    """
    if entry["verdict"] == "claim" and not (pair.aa_a and pair.aa_b):
        entry = dict(entry) | {
            "verdict": "directional",
            "downgraded": "no A/A twin in both files",
        }
    return entry


def _worst_floor(pair: FilePair, metric_id: str) -> dict:
    """Both files' floors for one metric, the worse reading of each unit."""
    out: dict = {}
    for unit in ("pct", "abs"):
        values = [
            (source.get(metric_id) or {}).get(f"floor_{unit}")
            for source in (pair.floors_a, pair.floors_b)
        ]
        values = [v for v in values if v is not None]
        out[unit] = max(values) if values else None
    return out


def _values_of(cell: dict, arm: str, metric_id: str) -> list:
    stats = (cell.get("arms", {}).get(arm, {}).get("metrics", {}) or {}).get(metric_id)
    return list(stats.get("values", [])) if stats else []


def _verdict_line(entry: dict) -> str:
    metric = model.METRICS[entry["metric"]]
    where = entry["scenario"] + (f"[{entry['cell']}]" if entry["cell"] else "")
    limit = _threshold_cell(entry)
    return (
        f"{where} {entry['metric']} ({metric.unit}): {_delta_cell(entry)} "
        f"[{entry['median_a']} vs {entry['median_b']}] {entry['verdict']} "
        f"(threshold {limit}, n={entry['rounds']}) "
        f"{entry['better'] or ''}".rstrip()
    )


def _ramp(results: dict, lines: list) -> list:
    """A capacity ramp that carried nothing is not a reading."""
    failures: list = []
    for summary_cell in results.get("summary", {}).get("cells", []):
        if summary_cell["cell"] != "ramp":
            continue
        for arm, stats in summary_cell["arms"].items():
            streams = (stats["metrics"].get("capacity_streams") or {}).get("median")
            lines.append(f"capacity: {arm} sustained {streams} stream(s)")
            if streams == 0 and _arm_kind(results, arm) in SUBJECT_KINDS:
                failures.append(f"capacity: {arm} carried no level that met the SLO")
    return failures


def _baseline(results: dict, baseline: dict | None, lines: list) -> list:
    """The regression half: this run against a comparable earlier one."""
    if baseline is None:
        lines.append("baseline: none given (--baseline for the regression half)")
        return []
    text, refused, _claims = compare_files(baseline, results)
    if refused:
        lines.append("baseline: refused")
        lines += [f"  {line}" for line in text.splitlines() if line.strip()]
        return ["baseline: the two files are not comparable (see the gate output)"]
    lines.append("baseline: compared")
    lines += [f"  {line}" for line in text.splitlines() if line.strip()]
    return []


def load(path) -> dict:
    """A results file, with its summary recomputed from the samples.

    The samples are the evidence and the summary is derived, so a report is
    always produced by the analysis the reader is running - and a stored file
    from an older engine still renders. Two files may only be *compared* when
    their engine hashes match (`comparability`), which is what keeps that
    recomputation from comparing two different methods.
    """
    results = json.loads(Path(path).read_text())
    if results.get("samples"):
        results["summary"] = summarize(results)
    return results


# --- the release gate -------------------------------------------------------
#: What a published run must not do. The thresholds are the soak model's, kept
#: because they were set against measured spread, and they are absolute where
#: the quantity is a leak (a slope, not a ratio).
DRIFT_RSS_MIB_PER_MIN = 50.0
DRIFT_FDS_PER_MIN = 1.0
DRIFT_THREADS_PER_MIN = 1.0
#: The shortest run a drift gate may judge. The thresholds above were set
#: against the fifteen-minute `soak` profile, and a shorter run's slope is a
#: slope through the pool's own growth (measured: a scaled 8-stage timeline read
#: 2.7 fds/min on an unchanged build) — reported as context, never failed.
MIN_DRIFT_SPAN_S = 900.0
#: The arms whose numbers are gated. A reference peer that misses the SLO is a
#: finding about the peer: reported with its number, never a block.
SUBJECT_KINDS = ("l4", "l3")


def gate(results: dict, baseline: dict | None = None) -> tuple:
    """What a run must satisfy before its numbers may be published.

    A single check per question, each one reporting what it saw rather than
    only whether it passed:

    * **coverage** — every cell the scenario declared, for every arm;
    * **the endpoint invariant** — no sample dialed the backend it forwards to
      (the mistake that made an early model report the loopback ceiling for
      every tool);
    * **the SLO on the clean stages** — for the product's own arms only;
    * **the drift and wedge axes** — a leak is a slope, a wedge is a silence;
    * **the capacity ramp** — a ramp that carried nothing is not a reading.

    Returns `(lines, failures)`: the failures are the strings a reader must act
    on, and an empty list is a pass.
    """
    lines: list = []
    failures: list = []
    meta = results.get("meta", {})
    lines.append(
        f"gate: profile {meta.get('profile')} fingerprint {meta.get('fingerprint')} "
        f"revision {(meta.get('provenance') or {}).get('revision')}"
    )
    failures += _coverage(results, lines)
    failures += _endpoints(results, lines)
    failures += _slo(results, lines)
    failures += _drift(results, lines)
    failures += _ramp(results, lines)
    failures += _baseline(results, baseline, lines)
    if not failures:
        lines.append("gate: PASS")
    return lines, failures


def _coverage(results: dict, lines: list) -> list:
    """Every declared cell, for every arm, with at least one measured round."""
    failures: list = []
    params_of = {
        s["id"]: s["params"] for s in results.get("meta", {}).get("scenarios", [])
    }
    measured = {
        (sample["arm"], sample["scenario"], sample["cell"])
        for sample in results.get("samples", [])
        if sample.get("ok") and not sample.get("warmup")
    }
    declared = results.get("meta", {}).get("scenarios", [])
    expected_kinds = {s["id"]: s["kind"] for s in declared}
    # A diagnostic scenario has no tool-free path, so the control arm is not
    # expected to have measured it at all.
    diagnostic = {s["id"] for s in declared if not s.get("control", True)}
    for arm in results.get("meta", {}).get("arms", []):
        for scenario_id, params in params_of.items():
            if arm["kind"] == "control" and scenario_id in diagnostic:
                continue
            for cell in model.expected_cells(scenario_id, params):
                if (arm["id"], scenario_id, cell) in measured:
                    continue
                failures.append(
                    f"coverage: {arm['id']} / {scenario_id}"
                    f"{f' / {cell}' if cell else ''} produced no measured round "
                    f"({expected_kinds.get(scenario_id, '?')})"
                )
    lines.append(f"coverage: {len(measured)} measured arm/scenario/cell triples")
    return failures


def _endpoints(results: dict, lines: list) -> list:
    """A tool arm's probe must dial what the tool exposes, never its backend."""
    failures: list = []
    checked = 0
    for sample in results.get("samples", []):
        evidence = sample.get("evidence") or {}
        dial, backend, kind = (
            evidence.get("dial_host"),
            evidence.get("backend_bind"),
            evidence.get("arm_kind"),
        )
        # A transparent arm is the exception that proves the rule: the visitor
        # dials the address the *client* owns, and the backend binds that same
        # address inside the client namespace, because the client's kernel
        # delivers the packet. The visitor cannot reach it without the tunnel
        # (the acceptance harness proves that); the direct-dial mistake this
        # check exists for is an L4/peer arm dialing its own backend.
        if not dial or not backend or kind in ("control", "l3"):
            continue
        checked += 1
        if dial == backend:
            failures.append(
                f"endpoint: {sample.get('arm')} / {sample.get('scenario')} dialed "
                f"{dial}, which is the backend it forwards to"
            )
    lines.append(f"endpoint invariant: {checked} samples carry a dialed endpoint")
    return failures


def _slo(results: dict, lines: list) -> list:
    """The clean stages must meet the SLO — for the product's own arms."""
    failures: list = []
    clean = 0
    for summary_cell in results.get("summary", {}).get("cells", []):
        if not _is_clean(results, summary_cell):
            continue
        for arm, stats in summary_cell["arms"].items():
            if _arm_kind(results, arm) not in SUBJECT_KINDS:
                continue
            clean += 1
            p99 = (stats["metrics"].get("rtt_p99_ms") or {}).get("median")
            errors = (stats["metrics"].get("rtt_error_rate_pct") or {}).get("median")
            if p99 is not None and p99 > model.SLO_RTT_P99_MS:
                failures.append(
                    f"SLO: {arm} / {summary_cell['scenario']}"
                    f"[{summary_cell['cell']}] p99 {p99} ms > "
                    f"{model.SLO_RTT_P99_MS} ms on a clean stage"
                )
            if errors is not None and errors > model.SLO_ERROR_RATE_PCT:
                failures.append(
                    f"SLO: {arm} / {summary_cell['scenario']}"
                    f"[{summary_cell['cell']}] error rate {errors} % > "
                    f"{model.SLO_ERROR_RATE_PCT} % on a clean stage"
                )
    lines.append(f"SLO: {clean} clean-stage cells gated")
    return failures


def _is_clean(results: dict, summary_cell: dict) -> bool:
    scenario = summary_cell["scenario"]
    cell = summary_cell["cell"]
    for sample in results.get("samples", []):
        if sample["scenario"] != scenario or sample["cell"] != cell:
            continue
        evidence = sample.get("evidence") or {}
        if "condition" in evidence:
            return evidence["condition"] == "clean"
    return False


def _series_span(results: dict, scenario: str) -> float:
    """How long the drift series of one scenario is, in seconds."""
    for sample in results.get("samples", []):
        if sample["scenario"] != scenario or sample["cell"] != "run":
            continue
        series = (sample.get("evidence") or {}).get("drift_series") or []
        if len(series) >= inst.MIN_WEDGE_POINTS:
            return float(series[-1]["t"] - series[0]["t"])
    return 0.0


def _arm_kind(results: dict, arm_id: str) -> str:
    for arm in results.get("meta", {}).get("arms", []):
        if arm["id"] == arm_id:
            return arm["kind"]
    return ""


def _drift(results: dict, lines: list) -> list:
    """A leak is a slope and a wedge is a silence; both are failures.

    The wedge half is judged at any run length — an interactive stream that
    stopped answering for five seconds is not a slope question — while the
    slope half needs a run long enough to carry one: the thresholds were set
    against the fifteen-minute `soak` profile, and a shorter run's slope is a
    slope through the pool's own growth (measured: a scaled eight-stage
    timeline read 2.7 fds/min on an unchanged build).
    """
    failures: list = []
    thresholds = {
        "drift_rss_mib_per_min": DRIFT_RSS_MIB_PER_MIN,
        "drift_fds_per_min": DRIFT_FDS_PER_MIN,
        "drift_threads_per_min": DRIFT_THREADS_PER_MIN,
    }
    for summary_cell in results.get("summary", {}).get("cells", []):
        if summary_cell["cell"] != "run":
            continue
        span = _series_span(results, summary_cell["scenario"])
        long_enough = span >= MIN_DRIFT_SPAN_S
        if not long_enough:
            lines.append(
                f"drift: {summary_cell['scenario']} ran {span:.0f}s, under the "
                f"{MIN_DRIFT_SPAN_S:.0f}s a slope needs: slopes reported, wedges "
                "still gated"
            )
        for arm, stats in summary_cell["arms"].items():
            if _arm_kind(results, arm) not in SUBJECT_KINDS:
                continue
            wedges = (stats["metrics"].get("wedge_count") or {}).get("median")
            if wedges:
                worst = (stats["metrics"].get("wedge_max_s") or {}).get("median")
                failures.append(
                    f"wedge: {arm} went silent {wedges:.0f} time(s), longest {worst} s"
                )
            if not long_enough:
                continue
            for metric, limit in thresholds.items():
                value = (stats["metrics"].get(metric) or {}).get("median")
                if value is not None and abs(value) > limit:
                    failures.append(f"drift: {arm} {metric} {value} exceeds {limit}")
    lines.append("drift: checked the run cells' slopes and wedges")
    return failures
