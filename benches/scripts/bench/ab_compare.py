# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""A/B verdict for two interleaved benchmark runs (AGENTS.md section 10).

Where `check_regression.py` gates a release against thresholds, this tool
answers the question an A/B actually asks: given two binaries measured in
interleaved rounds, which cells moved and which of those movements can be
*claimed*. A movement inside the rep spread is not a claim — that rule is
what the 2026-09-21 measurement revision exists for (a -31% "regression"
was a bimodal cell landing on an outlier, and a real -11.8% regression hid
inside the same noise for a whole session).

It reads a `--ab BIN_A,BIN_B` run's output, pairs the arms by round, and
reports per cell: both medians, the delta, whether any round's rep ranges
are disjoint, and the verdict. Paired rounds matter because the interleave
is what cancels epoch drift; comparing pooled medians without the pairing
would reintroduce it.

Usage:
  # one interleaved run (bench.py --ab BIN_A,BIN_B)
  uv run ab_compare.py results.json [--label-a A] [--label-b B]
  # two independent runs (the older hand-interleaved form)
  uv run ab_compare.py new.json --baseline old.json

The first form matches arms by the `(abN:<basename>)` label suffix `--ab`
writes, so each pair shares the rounds that cancel epoch drift. The second
compares two files run-to-run, which carries whatever drift separated them
and is therefore reported with that caveat. `--label-*` narrows to one arm
(e.g. only `mux1`); the default covers every molehill arm present.
"""
import argparse
import json
import re
import statistics
import sys
from pathlib import Path

# The metrics worth a verdict, with how to read them: higher-is-better
# ("up") or lower-is-better ("down"). Anything not listed is reported but
# never given a verdict, because a direction would be a guess.
METRICS = {
    "throughput_1stream_gbps": ("up", "1-stream"),
    "throughput_8streams_gbps": ("up", "8-stream"),
    "throughput_64streams_gbps": ("up", "64-stream"),
    # connects/s is a rate: MORE is better (the churn arm measures how fast
    # the path sets up short-lived connections), unlike the latency metrics
    # below it where lower is better
    "churn_connects_per_s": ("up", "churn/s"),
    "echo_rtt_ms_p50": ("down", "echo p50"),
    "udp_rtt_ms_p50": ("down", "udp p50"),
    "udp_loss_pct": ("down", "udp loss"),
    "udp_jitter_ms": ("down", "jitter"),
    "hol_ping_max_gap_ms": ("down", "HoL gap"),
    "memory_total_avg_kb": ("down", "RSS"),
    "cpu_total_avg_pct": ("down", "CPU"),
    "framing_cpu_pct_per_kframe": ("down", "cpu/kframe"),
}

# AB label suffix written by bench.py --ab: "molehill 0.8.1 (mux) (ab1:bin)"
AB_MARK = " (ab"


def arms_of(results: dict, label_filter: str | None) -> list:
    """Every arm key that carries an --ab round suffix."""
    out = []
    for key in results.get("results", {}):
        if AB_MARK not in key:
            continue
        if label_filter and label_filter not in key:
            continue
        out.append(key)
    return sorted(out)


def rounds_of(results: dict, key: str) -> dict:
    """{round: entry} for one arm's label (without the ab suffix)."""
    return results["results"][key]


def parse_metric(entry: dict, metric: str):
    """Value for a metric, or None when absent/not applicable."""
    if metric.startswith("churn_"):
        return (entry.get("churn") or {}).get("connects_per_s")
    if metric.startswith("echo_rtt_ms_"):
        return (entry.get("echo_rtt_ms") or {}).get(metric.rsplit("_", 1)[1])
    if metric.startswith("udp_rtt_ms_"):
        return (entry.get("udp_rtt_ms") or {}).get(metric.rsplit("_", 1)[1])
    if metric.startswith("hol_ping_max_gap_ms"):
        return (entry.get("hol") or {}).get("ping_max_gap_ms")
    if metric.startswith("memory_total"):
        # the schema's key is total_avg_kb (mem_stats). The pre-fix name
        # looked up total_kb, which does not exist, so RSS silently never
        # reached a verdict — the whole memory axis was missing from every
        # A/B report
        return (entry.get("memory_rss_kb") or {}).get("total_avg_kb")
    if metric.startswith("cpu_total_"):
        return (entry.get("cpu") or {}).get("total_avg_pct")
    if metric.startswith("framing_cpu_pct_per_kframe"):
        return (entry.get("framing_cpu") or {}).get("cpu_pct_per_kframe")
    return entry.get(metric)


def rep_range(entry: dict, metric: str):
    """(min, max) rep range for a throughput metric, or None.

    **Only the throughput metrics carry an explicit per-rep min/max in the
    schema.** Treating a single-valued metric's value as its own range
    would make every non-zero difference look non-overlapping, which is
    exactly the false-claim failure this tool exists to prevent — a 0.0%
    median difference must never be reported as a regression. Metrics
    without a recorded spread therefore get a median-only verdict.
    """
    base = metric.removesuffix("_gbps")
    lo = entry.get(f"{base}_min_gbps")
    hi = entry.get(f"{base}_max_gbps")
    if lo is not None and hi is not None:
        return (lo, hi)
    return None


def pair_keys(results: dict, label_filter: str | None) -> list:
    """[(arm_label_without_suffix, cell, {bin_name: {round: entry}})]."""
    # "molehill 0.8.1 (mux) (ab3:molehill-pool)" -> arm, round, binary
    ab_re = re.compile(r"^(?P<arm>.*) \(ab(?P<round>\d+):(?P<bin>[^)]+)\)$")
    groups: dict = {}
    for key in arms_of(results, label_filter):
        m = ab_re.match(key)
        if not m:
            continue
        for cell, entry in results["results"][key].items():
            groups.setdefault((m.group("arm"), cell), {}).setdefault(
                m.group("bin"), {})[int(m.group("round"))] = entry
    out = []
    for (arm, cell), per_bin in sorted(groups.items()):
        if len(per_bin) < 2:
            continue  # not an A/B pair for this arm/cell
        out.append((arm, cell, per_bin))
    return out


def pair_across_files(cur: dict, base: dict) -> list:
    """[(arm, cell, {bin: {round: entry}})] from two independent files.

    Each file contributes one "round" per arm/cell, so the pairing is
    run-to-run and the drift between the runs is not cancelled.
    """
    groups: dict = {}
    for name, src in (("current", cur), ("baseline", base)):
        for key, cells in src.get("results", {}).items():
            for cell, entry in cells.items():
                groups.setdefault((key, cell), {}).setdefault(
                    name, {})[0] = entry
    return [(arm, cell, per) for (arm, cell), per in sorted(groups.items())
            if len(per) == 2]


def verdict(metric: str, a_vals: list, b_vals: list,
            a_ranges: list, b_ranges: list) -> str:
    """The section-10 rule: claim only where no round's ranges overlap.

    A metric without recorded rep ranges can never reach a CLAIM: the spread
    is unknown, so the difference is reported as a median movement only.
    """
    if not a_vals or not b_vals:
        return "no data"
    direction, _ = METRICS.get(metric, (None, None))
    ma, mb = statistics.median(a_vals), statistics.median(b_vals)
    if ma == mb:
        return "no change"
    pct = abs(mb - ma) / ma * 100
    moved = "up" if mb > ma else "down"
    if not (a_ranges and b_ranges and len(a_ranges) == len(b_ranges)):
        return f"median {moved} {pct:.1f}% (no spread recorded)"
    disjoint = any(
        alo > bhi or blo > ahi
        for (alo, ahi), (blo, bhi) in zip(a_ranges, b_ranges)
    )
    if direction is None:
        return f"{moved} {pct:.1f}% ({'non-overlapping' if disjoint else 'inside spread'})"
    favourable = (direction == "up" and moved == "up") or (
        direction == "down" and moved == "down")
    if disjoint:
        return (f"CLAIM {'favourable' if favourable else 'REGRESSION'} "
                f"{pct:.1f}% (non-overlapping)")
    return f"inside spread {pct:.1f}% {moved}"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("results", type=Path)
    ap.add_argument("--label-a", help="only arms whose label contains this")
    ap.add_argument("--label-b", help="only arms whose label contains this")
    ap.add_argument("--metric", action="append",
                    help="restrict to this metric (repeatable)")
    ap.add_argument("--baseline", type=Path,
                    help="compare against a second results file instead of "
                         "reading --ab pairs from one (no shared rounds, so "
                         "epoch drift is not cancelled)")
    ap.add_argument("--json", action="store_true",
                    help="machine-readable output")
    args = ap.parse_args()

    data = json.loads(args.results.read_text())
    if args.baseline:
        base = json.loads(args.baseline.read_text())
        pairs = pair_across_files(data, base)
    else:
        pairs = pair_keys(data, args.label_a or args.label_b)
    if not pairs:
        sys.exit(f"no --ab arm pairs found in {args.results} "
                 f"(expected labels containing '{AB_MARK}')")

    report = []
    for arm, cell, per_bin in pairs:
        bins = sorted(per_bin)
        a_name, b_name = bins[0], bins[1]
        a_rounds, b_rounds = per_bin[a_name], per_bin[b_name]
        shared = sorted(set(a_rounds) & set(b_rounds))
        if not shared:
            continue
        for metric in (args.metric or list(METRICS)):
            a_vals, b_vals, a_rng, b_rng = [], [], [], []
            for r in shared:
                ea, eb = a_rounds[r], b_rounds[r]
                va, vb = parse_metric(ea, metric), parse_metric(eb, metric)
                if va is None or vb is None:
                    continue
                a_vals.append(va)
                b_vals.append(vb)
                ra, rb = rep_range(ea, metric), rep_range(eb, metric)
                if ra and rb:
                    a_rng.append(ra)
                    b_rng.append(rb)
            if not a_vals:
                continue
            entry = {
                "arm": arm, "cell": cell, "metric": metric,
                "a": a_name, "b": b_name,
                "a_median": statistics.median(a_vals),
                "b_median": statistics.median(b_vals),
                "rounds": len(a_vals),
                "verdict": verdict(metric, a_vals, b_vals, a_rng, b_rng),
            }
            report.append(entry)

    if args.json:
        print(json.dumps(report, indent=2))
        return

    cross = bool(args.baseline)
    if cross:
        print("NOTE: two independent files — no shared rounds, so epoch "
              "drift is NOT cancelled;\n      only a non-overlapping "
              "throughput difference is a claim.")
    width = max((len(e["arm"]) for e in report), default=20)
    cur = None
    for e in report:
        key = (e["arm"], e["cell"])
        if key != cur:
            cur = key
            print(f"\n{e['arm']:{width}s}  [{e['cell']}]")
        name = METRICS.get(e["metric"], ("", e["metric"]))[1]
        print(f"  {name:12s} {e['a']}={e['a_median']:>10.3f} "
              f"{e['b']}={e['b_median']:>10.3f}  {e['verdict']}")
    claims = [e for e in report if e["verdict"].startswith("CLAIM")]
    regressions = [e for e in claims if "REGRESSION" in e["verdict"]]
    unverifiable = [e for e in report if "no spread recorded" in e["verdict"]]
    print(f"\n{len(report)} metrics over {len(pairs)} arm/cell pairs; "
          f"{len(claims)} claimable ({len(regressions)} regressions), "
          f"{len(unverifiable)} median-only (no spread recorded)")
    if unverifiable:
        print("  note: only throughput metrics record a per-rep range in the "
              r"schema, so\. they are the only ones that can reach a CLAIM")
    if regressions:
        sys.exit(2)


if __name__ == "__main__":
    main()
