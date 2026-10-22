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

import hashlib
import json
import math
import random
import statistics
from dataclasses import dataclass, field
from pathlib import Path

from bench import instruments as inst
from bench import model

#: Tolerances for the two host probes, carried over from the soak model's
#: incidents: a CPU probe that repeats to ~2 % and is blind to the loopback
#: ceiling, and a loopback probe that repeats to ~1.5 % and drops ~5 % under
#: load, against swings of 25-39 % that they exist to catch.
HOST_CALIBRATION_TOLERANCE_PCT = 25.0
HOST_LOOPBACK_TOLERANCE_PCT = 15.0
#: The fewest measured rounds a claim may rest on: one round is an anecdote,
#: and the model says so instead of reading a direction into it.
MIN_ROUNDS_FOR_CLAIM = 2
#: The fewest paired rounds a bootstrap confidence interval may be estimated
#: over. Below this the interval is not printed at all (`ci_low`/`ci_high`
#: stay `None`) rather than printed with a width that would be a guess:
#: resampling two points cannot say how wide the effect's distribution is.
MIN_PAIRS_FOR_CI = 3
#: The nominal significance level of a claim's sign test, and the level the
#: Benjamini-Hochberg step-up controls the primary family at. A run whose
#: rounds cannot reach it is not silent about that: the verdict carries
#: `min_attainable_p`, and the effective level degrades to it (see
#: `alpha_eff`), so a three-round claim is *labelled* a 25 %-error statement
#: instead of passing silently as a 5 % one.
ALPHA = 0.05
#: The bootstrap: resamples per interval, and the percentile interval's
#: coverage. Seeded from the results file's fingerprint, so re-analysing a
#: stored run reproduces the same interval byte for byte.
BOOTSTRAP_REPS = 2000
BOOTSTRAP_LEVEL = 0.95
#: The largest fraction of a cell's rounds that may fail and still leave a
#: median worth reading. Above it the arm's cell reads `unavailable` with the
#: count, never a median over the survivors — a 1-of-4 median is an anecdote
#: promoted by dropping the rounds that disagreed with it.
MAX_FAILED_FRACTION = 0.5
#: The planning cutoff for `resolution_lines`: at or below this many rounds
#: the scatter is not what stands between the run and its floor (doubling
#: would not reach it), so the advice changes to "longer workloads".
MIN_ROUNDS_FOR_PLAN = 2
#: The analysis layer's own version, recorded in every results file's method:
#: the statistics are part of what a number means, and two files reduced by
#: different versions are not *compared* (either may be re-rendered by the
#: current code — `load` recomputes the summary from the stored samples — but
#: a comparison needs one reduction).
ANALYSIS_VERSION = 3
#: Values are rounded to this many decimals in the results file, which is more
#: than any of these instruments resolves and keeps a file diffable.
ROUND_TO = 6
#: How much of a method value the refusal message quotes before eliding.
METHOD_VALUE_CHARS = 60


@dataclass(frozen=True)
class Comparison:
    """The two measured sets one verdict compares.

    `values_a`/`values_b` are the rounds' readings in round order, and
    `rounds_a`/`rounds_b` are the round indexes those readings came from —
    paired by index, never by position, because a failed round on one side
    must shift neither side's pairing (the positional zip silently paired
    round 1 against round 2 and called the difference a replicate).

    `noise` is the run's own resolution for this metric in *both* units - a
    relative one and the metric's own - because a verdict must compare like
    with like: a loss floor is a fraction of a percent, a throughput floor is a
    fraction of a rate, and mixing them would let one metric's scale decide
    another's claim. `seed` makes the verdict's bootstrap reproducible: the
    same stored run analysed twice produces the same interval.
    """

    metric: str
    values_a: list
    values_b: list
    label_a: str = "A"
    label_b: str = "B"
    noise: dict | None = None
    rounds_a: tuple = ()
    rounds_b: tuple = ()
    seed: int = 0


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
        # A median over the survivors is a median over the rounds that
        # happened to agree with it. Past half the rounds failed, the cell
        # says why instead of quoting what is left: the gate and the verdicts
        # both read this flag.
        total = len(rows) + data["failed"]
        drowned = bool(rows) and data["failed"] / max(total, 1) > MAX_FAILED_FRACTION
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
            "drowned": drowned,
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
    entry: dict[str, object] = {}
    entry.update(readings)
    entry["metric"] = metric
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


# --- the statistics a verdict is made of ------------------------------------
def round_readings(
    results: dict, arm: str, scenario: str, cell: str, metric: str
) -> dict:
    """One metric's readings by **round index**, for one arm and cell.

    The round index is the pairing key, not position: a failed round on one
    side must shift neither side's pairing. The positional zip this replaces
    paired round 1 against round 2 and called the difference a replicate.
    """
    readings: dict = {}
    for sample in results.get("samples", []):
        if sample.get("warmup") or not sample.get("ok"):
            continue
        if sample["arm"] != arm or sample["scenario"] != scenario:
            continue
        if sample["cell"] != cell:
            continue
        value = sample.get("metrics", {}).get(metric)
        if value is not None:
            readings[sample["round"]] = value
    return readings


def _aligned(values_a: list, rounds_a: tuple, values_b: list, rounds_b: tuple):
    """Two sides' readings paired by round index.

    Falls back to positional pairing when round indexes were not supplied (the
    verdict's direct callers in the selfcheck pass bare lists), which is exact
    whenever both sides measured every round - and the caller says so.
    """
    if rounds_a and rounds_b:
        common = sorted(set(rounds_a) & set(rounds_b))
        index_a = dict(zip(rounds_a, values_a, strict=True))
        index_b = dict(zip(rounds_b, values_b, strict=True))
        return [(index_a[r], index_b[r]) for r in common]
    return list(zip(values_a, values_b, strict=False))


def sign_test_p(diffs: list) -> float:
    """The two-sided **exact sign test** p-value of the paired differences.

    The null is "a round is as likely to move either way": under it the number
    of positive differences is Binomial(n, 1/2), and the p-value doubles the
    tail at or beyond the observed count. Computed from `math.comb`, so the
    model carries no dependency for it.

    This replaces the silent `sign agreement == 1.0` the verdict used before:
    same information, but now the test's own error rate is *stated* — at three
    rounds a perfectly consistent reading has p = 0.25, which is the false
    positive rate that verdict had all along and never printed.
    """
    n = len(diffs)
    if n == 0:
        return 1.0
    positives = sum(1 for d in diffs if d > 0)
    tails = sum(math.comb(n, k) for k in range(max(positives, n - positives), n + 1))
    return min(1.0, 2.0 * tails / 2**n)


def min_attainable_p(rounds: int) -> float:
    """The smallest p-value `rounds` paired rounds can produce: `2 / 2**n`.

    The exact sign test cannot go below perfect consistency, so this is the
    strongest evidence a run of this length can offer - and it is the reason a
    claim at n < 6 is a statement with a 25 %, 12.5 %, ... error rate rather
    than a 5 % one. `alpha_eff` degrades to it so the verdict says what its
    own design permits instead of pretending a threshold was met.
    """
    if rounds < 1:
        return 1.0
    return 2.0 / 2**rounds


def alpha_eff(rounds: int) -> float:
    """The significance level a run of this length can honestly test at."""
    return max(ALPHA, min_attainable_p(rounds))


def _stable_seed(*parts) -> int:
    """A deterministic seed from the comparison's identity.

    The bootstrap must be reproducible: re-analysing a stored run produces the
    same interval, so a published CI is a property of the run rather than of
    the afternoon it was recomputed in. No global random state is touched.
    """
    digest = hashlib.sha256("|".join(str(p) for p in parts).encode())
    return int.from_bytes(digest.digest()[:8], "big")


def bootstrap_ci(pairs: list, seed: int) -> tuple:
    """A percentile bootstrap CI over the **difference of medians**.

    Resamples the paired rounds with replacement — both sides of a pair
    together, so the pairing survives the resample — and recomputes
    `median(a) - median(b)` per resample: the same statistic the verdict
    reports, so the interval brackets the number the reader is shown. Returns
    `(None, None)` below [`MIN_PAIRS_FOR_CI`]: resampling one or two points
    cannot say how wide the effect's distribution is, and a guessed width is
    worse than an honest absence.
    """
    if len(pairs) < MIN_PAIRS_FOR_CI:
        return (None, None)
    # Not cryptographic: the seed exists so re-analysing a stored run
    # reproduces the same interval, and the generator's quality bar is
    # "resamples a median without a pattern a reader could guess".
    rng = random.Random(seed)  # noqa: S311 - reproducible bootstrap, not security
    samples: list = []
    for _ in range(BOOTSTRAP_REPS):
        picks = [rng.randrange(len(pairs)) for _ in range(len(pairs))]
        a = [pairs[i][0] for i in picks]
        b = [pairs[i][1] for i in picks]
        samples.append(statistics.median(a) - statistics.median(b))
    samples.sort()
    alpha = 1.0 - BOOTSTRAP_LEVEL
    low = inst.pct(samples, alpha / 2.0)
    high = inst.pct(samples, 1.0 - alpha / 2.0)
    return (low, high)


def benjamini_hochberg(p_values: list, q: float = ALPHA) -> list:
    """BH step-up adjusted q-values, in the input's order.

    Controls the false discovery rate across the family of primary endpoints:
    with several pre-registered comparisons, some will clear their floors by
    chance, and this says which of the claims survive the correction. The
    adjusted value is the smallest family-wise level at which that comparison
    still reads significant; a claim survives when `p <= q_value`.
    """
    m = len(p_values)
    if m == 0:
        return []
    order = sorted(range(m), key=lambda i: p_values[i])
    adjusted: list = [1.0] * m
    running = 1.0
    for rank in range(m, 0, -1):
        index = order[rank - 1]
        running = min(running, p_values[index] * m / rank)
        adjusted[index] = min(1.0, running)
    return [round(v, 4) if v * q < 1.0 else 1.0 for v in adjusted]


def rounds_needed(sd_pct: float, effect_pct: float) -> int | None:
    """The paired rounds that resolve `effect_pct` at this scatter, stated.

    A normal approximation over the paired differences: `n = (1.96 s / d)²`.
    A *planning* number, printed beside the exact one the sign test gives
    (`min_attainable_p` — six rounds reach p ≤ 0.05 whatever the scatter, so
    consistency is cheap and magnitude is what rounds buy). `None` when either
    side is zero or the effect is already inside one round's resolution.
    """
    if sd_pct <= 0 or effect_pct <= 0:
        return None
    n = (1.96 * sd_pct / effect_pct) ** 2
    return max(1, math.ceil(n))


def resolution_lines(results: dict) -> list:
    """What this run could resolve, and what it would take to resolve more.

    One line per cell where the run's resolution is *worse* than the metric's
    floor — the cells where a claim is currently unreachable, with the round
    count that would reach it (`rounds_needed` from the run's own A/A
    scatter). A run that already resolves its metrics' floors prints nothing:
    there is nothing to plan for, and a page of "1 rounds would reach it"
    would just be noise.
    """
    noise = (results.get("summary", {}).get("noise", {}) or {}).get("by_cell", {})
    lines: list = []
    for cell_id, entries in sorted(noise.items()):
        for metric_id, entry in entries.items():
            metric = model.METRICS.get(metric_id)
            if metric is None or metric.materiality[0] != "rel":
                continue
            floor = entry.get("floor_pct")
            if floor is None or floor <= metric.materiality[1]:
                continue
            # The A/A scatter is the run's own spread for this cell: it is
            # what more rounds would narrow.
            scatter = entry.get("aa_scatter_pct") or floor
            need = rounds_needed(scatter, metric.materiality[1])
            if need is None or need <= MIN_ROUNDS_FOR_PLAN:
                # Two or fewer rounds means the scatter is not the thing
                # standing between the run and its floor — say what is.
                lines.append(
                    f"  {cell_id} {metric_id}: the run resolves {floor:.1f} % "
                    f"against a {metric.materiality[1]:g} % floor — "
                    "the cell is too noisy for rounds to fix (longer workloads, "
                    "not more of them)"
                )
            else:
                lines.append(
                    f"  {cell_id} {metric_id}: the run resolves {floor:.1f} % "
                    f"against a {metric.materiality[1]:g} % floor — about "
                    f"{need} rounds would reach it"
                )
    return lines[:12]


# --- verdicts ---------------------------------------------------------------
def verdict(cmp: Comparison) -> dict:
    """One metric's verdict between two measured sets of rounds.

    The state is decided by three legs, each reported rather than hidden:

    * **materiality** — the median difference clears both the metric's own
      floor and the run's noise floor (as before);
    * **consistency** — the paired rounds are tested with the *exact sign
      test* at the level the run's length can honestly support
      (`alpha_eff`): at three rounds that is 0.25, and the verdict says so,
      instead of a silent "every round agreed" that was a 25 %-error rule;
    * **the interval** — a bootstrap CI over the paired difference says
      whether the effect is *entirely beyond* the threshold (a claim's
      shape), *entirely inside* it (bounded evidence of no difference), or
      *straddling* it (a direction, and nothing more).

    Only a **primary** endpoint may read `claim`: the scenario's headline
    metric, or an explicit hypothesis declaration. Everything else is
    exploratory and is reported with its numbers, never as a claim.
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
    pairs = _aligned(cmp.values_a, cmp.rounds_a, cmp.values_b, cmp.rounds_b)
    base = _verdict_base(pairs, metric, cmp.noise, cmp.seed)
    base |= {"metric": cmp.metric, "a": cmp.label_a, "b": cmp.label_b}
    base["verdict"] = _verdict_state(base)
    if base["verdict"] in ("claim", "directional") and metric.direction != "none":
        improved = (base["delta_abs"] > 0) == (metric.direction == "higher")
        base["better"] = cmp.label_a if improved else cmp.label_b
    base["fragile"] = _fragile(pairs, metric, cmp.noise, cmp.seed, base)
    return base


def _verdict_base(
    pairs: list, metric: model.Metric, noise: dict | None, seed: int
) -> dict:
    """The verdict's measured legs, from one set of paired rounds.

    A pure function of the pairs: leave-one-out fragility calls it again on
    each subset, so the reported verdict and the fragility check are the same
    computation rather than two that could drift apart.
    """
    values_a = [a for a, _ in pairs]
    values_b = [b for _, b in pairs]
    med_a, med_b = statistics.median(values_a), statistics.median(values_b)
    delta_abs = med_a - med_b
    delta_pct = (delta_abs / abs(med_b) * 100.0) if med_b else None
    noise = noise or {}
    relative = metric.materiality[0] == "rel"
    floor = (noise.get("pct") if relative else noise.get("abs")) or 0.0
    threshold = max(metric.materiality[1], floor)
    measured = (delta_pct or 0.0) if relative else delta_abs
    diffs = [a - b for a, b in pairs]
    p = sign_test_p(diffs)
    level = alpha_eff(len(diffs))
    ci_low, ci_high = bootstrap_ci(pairs, seed)
    # The threshold in the difference's own unit, so the interval and the
    # floor are compared like with like: a relative floor is a percentage of
    # the reference, which is exactly what delta_pct divides by.
    threshold_abs = threshold / 100.0 * abs(med_b) if relative and med_b else threshold
    # The interval read on the improvement scale: positive means a is better.
    a_better = metric.direction == "higher"
    imp = (
        (ci_low, ci_high)
        if a_better
        else ((None, None) if ci_low is None else (-ci_high, -ci_low))
    )
    return {
        "metric": metric.id,
        "unit": "pct" if relative else metric.unit,
        "median_a": round(med_a, ROUND_TO),
        "median_b": round(med_b, ROUND_TO),
        "delta_abs": round(delta_abs, ROUND_TO),
        "delta_pct": None if delta_pct is None else round(delta_pct, 3),
        "sign_agreement": _sign_agreement(values_a, values_b),
        "threshold": round(threshold, 3),
        "noise": {k: (None if v is None else round(v, 3)) for k, v in noise.items()},
        "material": abs(measured) >= threshold,
        "rounds": [len(values_a), len(values_b)],
        "p": round(p, 4),
        "alpha_eff": round(level, 4),
        "min_attainable_p": round(min_attainable_p(len(diffs)), 4),
        "n_pairs": len(diffs),
        "ci_low": None if ci_low is None else round(ci_low, ROUND_TO),
        "ci_high": None if ci_high is None else round(ci_high, ROUND_TO),
        "ci_level": BOOTSTRAP_LEVEL if ci_low is not None else None,
        "ci": _ci_state(imp[0], imp[1], threshold_abs),
        "equivalent": False,
        "fragile": False,
        "better": None,
    }


def _ci_state(imp_low, imp_high, threshold_abs) -> str:
    """What the improvement interval says, or `""` when none was estimable."""
    if imp_low is None:
        return ""
    if imp_low > threshold_abs:
        return "beyond"
    if imp_high < threshold_abs and imp_low > -threshold_abs:
        return "within"
    return "straddling"


def _decide(material: bool, p: float, level: float, ci: str) -> str:
    """The verdict state from its three legs, on one set of paired rounds.

    `ci == ""` (no interval estimable) keeps the pre-interval behaviour: the
    materiality and consistency legs decide, and the verdict text carries the
    `min_attainable_p` caveat that says how strong that is.
    """
    significant = p <= level
    if material and significant and ci in ("", "beyond"):
        return "claim"
    if ci == "within":
        return "indistinguishable"
    if material:
        return "directional"
    return "indistinguishable"


def _verdict_state(base: dict) -> str:
    if base["metric"] in model.METRICS and model.METRICS[base["metric"]].direction == (
        "none"
    ):
        return "context"
    if min(base["rounds"]) < MIN_ROUNDS_FOR_CLAIM:
        return "single-round"
    return _decide(base["material"], base["p"], base["alpha_eff"], base["ci"])


def _fragile(pairs, metric, noise: dict | None, seed, base) -> bool:
    """Whether the verdict survives dropping any single paired round.

    A leave-one-out check, reported and never applied: a reading that flips
    when one round is removed is exactly the shape the P3 re-test hit (the
    baseline's own first round sat above the candidate's median). Not computed
    below three pairs — every two-pair verdict is fragile by construction, and
    the `single-round` state already says what one round is worth.
    """
    if base["n_pairs"] < MIN_PAIRS_FOR_CI:
        return False
    for i in range(len(pairs)):
        subset = pairs[:i] + pairs[i + 1 :]
        sub = _verdict_base(subset, metric, noise, seed)
        if _decide(sub["material"], sub["p"], sub["alpha_eff"], sub["ci"]) != (
            _decide(base["material"], base["p"], base["alpha_eff"], base["ci"])
        ):
            return True
    return False


def verdicts(results: dict) -> list:
    """Every verdict this run supports: each tool arm against the control.

    The control is the reference, not a competitor: it is the same topology and
    backend with no tool in the path, so "the arm differs from the control" is a
    statement about the architecture, while "one arm differs from another" is a
    statement about a choice.

    Two rules come with the scale this loop runs at — a small run emits well
    over a hundred verdicts — and both are reported per entry:

    * **primary** marks the family the false-discovery correction is applied
      over: the run's declared hypothesis endpoints (`--hypothesis`), or the
      scenario's headline metric when nothing was declared. Every other
      comparison is exploratory: it is reported with its numbers and never
      reads `claim`, because a claim picked out of a hundred comparisons is
      not a finding, it is a selection.
    * **q** is the Benjamini-Hochberg adjusted value over the primary family
      (per comparison pair, per cell): a primary claim must also survive it.
    """
    out: list = []
    noise = (results.get("summary", {}).get("noise", {}) or {}).get("metrics", {})
    twins = _twin_pairs(results)
    # An arm may declare the arm it is the *decision* against (a configuration
    # axis rather than a build axis); the results file carries the declaration.
    declared = {
        a["id"]: a.get("against", "")
        for a in results.get("meta", {}).get("arms", [])
        if a.get("against")
    }
    fingerprint = (results.get("meta", {}) or {}).get("fingerprint", "")
    for summary_cell in results.get("summary", {}).get("cells", []):
        scenario, cell = summary_cell["scenario"], summary_cell["cell"]
        for label_a, label_b in _pairs_of(summary_cell, twins, declared):
            family: list = []
            for metric_id in model.metric_ids():
                readings_a = round_readings(results, label_a, scenario, cell, metric_id)
                readings_b = round_readings(results, label_b, scenario, cell, metric_id)
                if not readings_a and not readings_b:
                    continue
                if any(
                    summary_cell["arms"].get(a, {}).get("drowned")
                    for a in (label_a, label_b)
                ):
                    out.append(
                        {
                            "metric": metric_id,
                            "unit": model.METRICS[metric_id].unit,
                            "a": label_a,
                            "b": label_b,
                            "verdict": "unavailable",
                            "reason": (
                                "more than half of this cell's rounds failed; "
                                "the surviving median is not quoted"
                            ),
                            "rounds": [
                                summary_cell["arms"].get(label_a, {}).get("rounds", 0),
                                summary_cell["arms"].get(label_b, {}).get("rounds", 0),
                            ],
                            "scenario": scenario,
                            "cell": cell,
                            "aa": label_b.endswith(AA_SUFFIX),
                            "twin": label_b.endswith(TWIN_SUFFIXES),
                            "primary": False,
                            "fragile": False,
                        }
                    )
                    continue
                control_readings = round_readings(
                    results, "control", scenario, cell, metric_id
                )
                rounds_a = tuple(sorted(readings_a))
                rounds_b = tuple(sorted(readings_b))
                # Normalize only where the run asked for it (`--normalize
                # control`): dividing by the control is right when the cell is
                # host-bound and the control shares that bound, and circular
                # when the tool is what limits the path — a judgement the run
                # states, not one the analysis guesses at.
                normalize = (results.get("meta", {}).get("method", {}) or {}).get(
                    "normalize", ""
                )
                pair = _Pair(
                    a=readings_a,
                    b=readings_b,
                    rounds_a=rounds_a,
                    rounds_b=rounds_b,
                    control=control_readings,
                )
                norm_a, norm_b, normalized = _normalized(pair, normalize == "control")
                entry = verdict(
                    Comparison(
                        metric=metric_id,
                        values_a=norm_a,
                        values_b=norm_b,
                        label_a=label_a,
                        label_b=label_b,
                        noise=floor_for(noise, scenario, cell, metric_id),
                        rounds_a=rounds_a,
                        rounds_b=rounds_b,
                        seed=_stable_seed(
                            fingerprint, label_a, label_b, scenario, cell, metric_id
                        ),
                    )
                )
                entry |= {
                    "scenario": scenario,
                    "cell": cell,
                    "aa": label_b.endswith(AA_SUFFIX),
                    "twin": label_b.endswith(TWIN_SUFFIXES),
                    "normalized": normalized,
                }
                family.append(entry)
                out.append(entry)
            declared = (results.get("meta", {}).get("method", {}) or {}).get(
                "primary", []
            )
            _apply_family(family, scenario, declared)
    return out


@dataclass(frozen=True)
class _Pair:
    """One cell's two sides and the control that may normalize them."""

    a: dict
    b: dict
    rounds_a: tuple
    rounds_b: tuple
    control: dict


def _normalized(pair: _Pair, wanted: bool):
    """Both sides' readings as ratios to the control's reading of their round.

    Opt-in (`--normalize control`), because it is only honest where the cell
    is host-bound: the ratio cancels the host's round-to-round drift, but it
    also cancels any real difference that moves the *control* the same way —
    so on a loopback-ceiling cell the division would manufacture invariance
    the path does not have. A metric whose readings are zero-or-negative is
    left raw (the ratio is meaningless there), and so is a comparison where
    either side has no control reading to divide by; the fallback is recorded,
    not silent, by the `normalized` flag the verdict carries.
    """
    raw = (
        [pair.a[r] for r in pair.rounds_a],
        [pair.b[r] for r in pair.rounds_b],
        False,
    )
    if not wanted:
        return raw
    common_a = [r for r in pair.rounds_a if r in pair.control]
    common_b = [r for r in pair.rounds_b if r in pair.control]
    usable = (
        all(pair.a[r] > 0 for r in pair.rounds_a)
        and all(pair.b[r] > 0 for r in pair.rounds_b)
        and all(pair.control[r] > 0 for r in common_a + common_b)
        and len(common_a) == len(pair.rounds_a)
        and len(common_b) == len(pair.rounds_b)
    )
    if not usable:
        return raw
    return (
        [pair.a[r] / pair.control[r] for r in pair.rounds_a],
        [pair.b[r] / pair.control[r] for r in pair.rounds_b],
        True,
    )


def _apply_family(family: list, scenario: str, declared: list) -> None:
    """Mark primary endpoints and BH-adjust the family's p-values.

    Primary is the run's pre-registered set: the endpoints `--primary`
    declared for this scenario (metric, and arm/cell when they name one), or
    — when nothing was declared — the scenario's headline metric, which is
    the model's standing declaration. The correction covers that set only:
    adjusting the hundred-odd exploratory comparisons too would drive every
    q to 1 and make the correction a way of never seeing anything, which is
    not control, it is refusal. Exploratory entries are capped at
    `directional` below, before they reach the text.
    """
    if not family:
        return
    headline = (model.SCENARIOS.get(scenario) or model.Scenario("", "", "")).headline
    here = [d for d in declared if not d.get("scenario") or d["scenario"] == scenario]
    for entry in family:
        if here:
            entry["primary"] = any(
                d["metric"] == entry["metric"]
                and (not d.get("arm") or d["arm"] == entry["a"])
                and (not d.get("cell") or d["cell"] == entry["cell"])
                for d in here
            )
        else:
            entry["primary"] = bool(headline) and entry["metric"] == headline
    # A drowned cell has no statistics to correct — it is already
    # `unavailable`, and the family math passes it by.
    measurable = [e for e in family if "p" in e]
    primaries = [e for e in measurable if e["primary"]]
    adjusted = benjamini_hochberg([e["p"] for e in primaries])
    for entry, q in zip(primaries, adjusted, strict=True):
        entry["q"] = q
    for entry in measurable:
        entry.setdefault("q", entry["p"])
    # The two claims-gates the family carries: exploratory comparisons never
    # read claim (a claim picked from a hundred is a selection, not a
    # finding), and a primary claim must survive the family correction — at
    # the level the run's own design can test at, the same `alpha_eff` the
    # consistency leg used. Gate at the nominal level when the design can
    # reach it, and at `min_attainable_p` when it cannot, so a three-round
    # claim is labelled 25 % rather than silently impossible.
    for entry in family:
        if not entry["primary"] and entry["verdict"] == "claim":
            entry["verdict"] = "directional"
            entry["downgraded"] = (
                "exploratory: not a pre-registered endpoint of this run"
            )
        elif (
            entry["primary"]
            and entry["verdict"] == "claim"
            and entry["q"] > (entry["alpha_eff"])
        ):
            entry["verdict"] = "directional"
            entry["downgraded"] = (
                f"q {entry['q']} above the family level {entry['alpha_eff']}"
            )


def _pairs_of(summary_cell: dict, twins: list, declared: dict) -> list:
    """The comparisons one cell supports: each arm against the control, every
    twin against its base (the A/A floor and the A/B pair), and every arm that
    declared a baseline against it (a configuration's own A/B)."""
    arms = summary_cell["arms"]
    pairs = []
    if "control" in arms:
        pairs = [
            (a, "control")
            for a in sorted(arms)
            if a != "control" and not a.endswith(TWIN_SUFFIXES)
        ]
    pairs += [(base, twin) for base, twin in twins if base in arms and twin in arms]
    pairs += [
        (declared[a], a) for a in sorted(arms) if a in declared and declared[a] in arms
    ]
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
    return (
        _switch_blockers(meta_a, meta_b)
        + _method_blockers(meta_a, meta_b)
        + _host_blockers(meta_a, meta_b)
    )


def _switch_blockers(meta_a: dict, meta_b: dict) -> list:
    """Different inherited switches are different subjects, not a method detail.

    `molehill_switches` sits *beside* `method` on purpose — a declared treatment
    is exactly what a between-file comparison contrasts, and the file has to say
    it rather than hide it in a hash. But this function is the reader the gate
    uses to decide whether one file may stand in as another's baseline, and a
    forced carrier or a scheduled switch changes what the numbers *mean*, so two
    runs whose switches differ are refused here even when their fingerprints
    match (the fingerprint hashes `method`, which deliberately omits them).
    """
    switches_a = meta_a.get("molehill_switches") or {}
    switches_b = meta_b.get("molehill_switches") or {}
    if switches_a == switches_b:
        return []
    differing = [
        f"{name}={switches_a.get(name, '-')} vs {switches_b.get(name, '-')}"
        for name in sorted(set(switches_a) | set(switches_b))
        if switches_a.get(name) != switches_b.get(name)
    ]
    return [f"different inherited switches: {', '.join(differing)}"]


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
    # The statistics are part of the method now (`analysis_version` travels in
    # it), so name the actionable remedy when that is the only difference:
    # both files can be re-rendered by one code path, but the comparison of
    # two reductions is refused rather than assumed.
    if blockers and all(
        key in ("analysis_version", "engine_hash")
        for key in (k.split("'")[1] for k in blockers if k.startswith("method key"))
    ):
        blockers.append(
            "only the analysis version differs: re-analyse both files with the "
            "current code (`just bench report`) before comparing them"
        )
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
    lines hides the one line that matters. Under it, the resolution statement:
    what this run could resolve and what more rounds would buy, so a no-claim
    carries its own diagnosis.
    """
    header = (
        "verdicts (a claim needs the effect beyond its floors, the paired "
        "rounds consistent at the level this run's length can test, and a "
        "pre-registered endpoint; exploratory comparisons are directional at "
        "best):"
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
    lines += resolution_lines(results)
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
        f"{_ci_cell(entry)}p {entry.get('p')} vs alpha_eff "
        f"{entry.get('alpha_eff')}, n={entry['rounds']}"
    )
    if entry.get("downgraded"):
        detail += f"; {entry['downgraded']}"
    if entry.get("fragile"):
        detail += "; FRAGILE: dropping any one round flips this"
    detail += ")"
    state = entry["verdict"]
    if state == "claim":
        return f"{head}: CLAIM {entry['better']} better, {detail}"
    if state in ("directional", "single-round"):
        suffix = (
            f"({entry['better'] or '?'} better), {detail}"
            if state == "directional"
            else f"not a verdict ({detail})"
        )
        return f"{head}: {state}, {suffix}"
    if state == "context":
        return f"{head}: context only, {detail}"
    if state == "indistinguishable" and entry.get("ci") == "within":
        return (
            f"{head}: indistinguishable — the interval sits inside the "
            f"threshold ({detail})"
        )
    return f"{head}: indistinguishable, {detail}"


def _ci_cell(entry: dict) -> str:
    """The interval in the verdict line, or `""` when none was estimable."""
    low, high = entry.get("ci_low"), entry.get("ci_high")
    if low is None or high is None:
        return ""
    return f"CI[{low:.3g}..{high:.3g}] "


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
    from an older engine still renders. The reduction that produced what a
    reader sees is stamped on (`reduced_by`), so a re-analysis is never
    mistaken for the file's own; two files may only be *compared* when their
    methods match (`comparability`), which is what keeps a recomputation from
    quietly comparing two different reductions of two different methods.
    """
    results = json.loads(Path(path).read_text())
    if results.get("samples"):
        results["summary"] = summarize(results)
        results["reduced_by"] = ANALYSIS_VERSION
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
    failures += _drowned(results, lines)
    failures += _drift(results, lines)
    failures += _ramp(results, lines)
    failures += _baseline(results, baseline, lines)
    if not failures:
        lines.append("gate: PASS")
    return lines, failures


def ramp_reached(results: dict) -> dict:
    """How far each arm's ramp got: the highest level it measured.

    The capacity ramp climbs until a tool breaks the SLO, so the level it
    stopped *at* is the measurement and the levels above it are ones the run
    never attempted. Keyed `(arm, scenario)`; absent where a scenario is not
    a ramp.
    """
    reached: dict = {}
    for sample in results.get("samples", []):
        cell = sample.get("cell") or ""
        if not sample.get("ok") or not cell.startswith("L"):
            continue
        try:
            level = int(cell[1:])
        except ValueError:
            continue
        key = (sample.get("arm"), sample.get("scenario"))
        if key not in reached or level > reached[key]:
            reached[key] = level
    return reached


def _above_ceiling(cell, ceiling: int) -> bool:
    """Whether `cell` is a ramp level above the one the arm reached."""
    if not cell.startswith("L"):
        return False
    try:
        return int(cell[1:]) > ceiling
    except ValueError:
        return False


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
    # A capacity ramp stops at the level where a tool breaks the SLO: the
    # levels above it were never *meant* to run, and demanding them would
    # report "nps produced no L2" for a tool that was shown to carry L1 and
    # nothing more — which is the finding, not a gap in the run. The ramp's
    # own gate (`_ramp`) is what judges how far a tool got, so coverage here
    # stops at the highest level the arm actually reached.
    reached = ramp_reached(results)
    for arm in results.get("meta", {}).get("arms", []):
        for scenario_id, params in params_of.items():
            if arm["kind"] == "control" and scenario_id in diagnostic:
                continue
            ceiling = reached.get((arm["id"], scenario_id))
            for cell in model.expected_cells(scenario_id, params):
                if (arm["id"], scenario_id, cell) in measured:
                    continue
                if ceiling is not None and _above_ceiling(cell, ceiling):
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


def _drowned(results: dict, lines: list) -> list:
    """A cell that lost more than half its rounds is not quoted, it is refused.

    The verdicts already read the flag and report `unavailable`; the gate
    fails the run on it, because a published table with a median that half the
    rounds disagreed with is worse than a hole — it looks like agreement.
    """
    failures: list = []
    drowned = 0
    for summary_cell in results.get("summary", {}).get("cells", []):
        for arm, stats in summary_cell["arms"].items():
            if not stats.get("drowned"):
                continue
            drowned += 1
            failures.append(
                f"drowned: {arm} / {summary_cell['scenario']}"
                f"[{summary_cell['cell']}] lost {stats['failed']} of "
                f"{stats['failed'] + stats['rounds']} rounds — not quoted"
            )
    lines.append(f"drowned cells: {drowned} refused")
    return failures


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
