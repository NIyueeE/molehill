#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Soak gate: decide whether a run is trustworthy, and whether it regressed.

Two modes, both refusing to reduce a run to a single averaged number:

1. `soak_check.py [current.json [baseline.json]]` — the release gate. First
   the run is checked *against itself*: every test must have the series its
   coverage claims, every throughput sample must have dialed the tool's
   exposed port rather than its backend, and every test must meet the
   absolute SLO. Then, when a baseline exists, each test type is compared to
   it under per-type thresholds. A violation exits non-zero.
2. `soak_check.py --screen <screen.json>` — the development A/B verdict:
   per-step medians, the effect size and a CLAIM / directional / no-claim
   decision for the two builds a `--test=screen` run interleaved.

The self-check is what makes the first release gate meaningful: with no
baseline the absolute SLO is the only verdict available, and it can only be
believed if the run is complete and was measured on the right endpoint.
"""

import json
import os
import sys
from pathlib import Path

# Per-type gate thresholds (percent unless noted). They are method
# constants — the env var of the same name overrides one for an experiment.
THRESHOLDS = {
    "capacity_streams_pct": 10.0,  # sustainable-load drop
    "capacity_rtt_p99_pct": 25.0,  # response-time curve at matched load
    "rrul_rtt_p99_pct": 25.0,  # per-stage interactive p99
    "rrul_worst_1s_pct": 30.0,  # per-stage worst second
    "cost_cpu_per_gbit_pct": 15.0,  # the fixed operating point
    "drift_fds_per_min": 1.0,  # soak leak axis (absolute)
    "drift_rss_mb_per_min": 50.0,  # soak leak axis (absolute)
}
# The screen's claim threshold: same direction on every step AND this much.
SCREEN_CLAIM_PCT = 15.0
# A screen needs at least this many steps to carry a claim at all.
SCREEN_MIN_STEPS = 2
# `--screen <file>` takes the flag plus one path.
SCREEN_ARGS = 2
# The interactive error rate may rise by at most this much (percentage
# points) before it counts as a regression.
ERROR_RATE_RISE_PP = 5.0
# The tool this repository releases. The SLO is *its* contract: the peer
# tools are measured under the same workload for context, and a peer that
# misses the SLO is a finding about the peer, not a block on this release.
SUBJECT = "molehill"
# Every coverage axis a test claims, and the series that has to carry it.
COVERAGE_SERIES = {
    "tcp_bulk": "throughput_bulk_gbps",
    "tcp_interactive": "rtt_interactive_ms",
    "tcp_churn": "churn_setup_ms",
    "udp_session": "rtt_udp_ms",
}


def env_pct(name: str) -> float:
    try:
        return float(os.environ.get(name, THRESHOLDS[name]))
    except ValueError:
        return THRESHOLDS[name]


def pct_change(base, cur) -> float | None:
    if base is None or cur is None or base == 0:
        return None
    return (cur - base) / base * 100.0


class Report:
    """Gate output: every line is a verdict, and violations are counted."""

    def __init__(self) -> None:
        self.violations = 0
        self.legacy_checks = 0
        # Whether the tool currently being checked is the one this repository
        # releases. The self-checks (`check_run`) are always about it; the
        # comparison loop sets this per tool.
        self.subject = True

    def set_subject(self, tool: str) -> None:
        """Decide who a failure belongs to.

        The SLO gates the tool this repository releases. The peers are measured
        under the same workload for context, and third-party behaviour swings
        between runs — rathole's `rate100` p99 moved 161 ms -> 7064 ms here
        while molehill's stages stayed within 15% — so a peer's violation is
        reported with its numbers and does not block a molehill release
        (docs/release.md). Counting them made the gate exit non-zero on
        somebody else's noise.
        """
        self.subject = tool.startswith(SUBJECT)

    def ok(self, fmt: str, *a) -> None:
        print(f"  ok    {fmt.format(*a)}")

    def fail(self, fmt: str, *a) -> None:
        if self.subject:
            print(f"  FAIL  {fmt.format(*a)}")
            self.violations += 1
        else:
            print(f"  NOTE  {fmt.format(*a)} (reference peer — reported, not gated)")

    def note(self, fmt: str, *a) -> None:
        print(f"  NOTE  {fmt.format(*a)}")

    def legacy(self, fmt: str, *a) -> None:
        """A check this data cannot answer because it predates the field.

        Neither a violation (the run was made before the field existed) nor an
        `ok`: the gate states exactly what it could not verify, so nobody
        reads the summary as a clean bill of health.
        """
        print(f"  LEGACY {fmt.format(*a)}")
        self.legacy_checks += 1

    def limit(self, value, base, limit: float, fmt: str, *a) -> None:
        """A threshold verdict: `value` must not exceed `base` by `limit`%."""
        if value is None or base is None:
            self.note(fmt + " (incomplete)", *a)
            return
        d = pct_change(base, value)
        if d <= limit:
            self.ok(fmt + f" ({d:+.1f}%, limit +{limit:.0f}%)", *a)
        else:
            self.fail(fmt + f" ({d:+.1f}%, limit +{limit:.0f}%)", *a)


def comparability(base: dict, cur: dict) -> str | None:
    """Why these two runs may not be compared, or `None` when they may.

    docs/release.md and docs/benchmarks.md both state the boundary: only
    same-schema, same-host runs are comparable. This is that sentence as a
    function, so the gate refuses an invalid comparison instead of printing
    verdicts nobody may act on. The host key is the recorded hostname, which is
    what the results carry today; a containerized bench host changes it on
    every container restart, which is conservative in the safe direction
    (refusing to compare) and is recorded in HANDOFF.md as the next method
    fix — a stable host identity is a calibration measurement, not a name.
    """
    if base["meta"].get("workload_version") != cur["meta"].get("workload_version"):
        return (
            "the runs have different workload versions "
            f"({base['meta'].get('workload_version')} vs "
            f"{cur['meta'].get('workload_version')}): different method"
        )
    bh, ch = base["meta"].get("hostname"), cur["meta"].get("hostname")
    if bh and ch and bh != ch:
        return (
            f"the runs were made on different hosts ({bh} vs {ch}): the path, "
            "the CPU budget and the loopback ceiling are properties of where a "
            "run happens, and the peers' clean-tool spread shows it"
        )
    return None


def metric_count(test: dict, metric: str) -> int:
    return sum(1 for r in test.get("series", []) if r.get("metric") == metric)


def check_run(cur: dict, rep: Report) -> None:
    """The run's self-check: completeness, endpoints, absolute SLO.

    This is the part that makes a baseline-less release gate mean something,
    and it is the mechanical half of §10's "re-check every consumer after
    reshaping data" rule: a renamed or missing series, or a throughput sample
    that dialed the backend, must fail the gate rather than plot as a hole.
    """
    slo = cur.get("meta", {}).get("slo") or {}
    slo_p99 = slo.get("rtt_p99_ms")
    slo_err = slo.get("error_rate")
    tests = cur.get("tests", [])
    if not tests:
        rep.fail("the results file contains no tests")
        return
    for t in tests:
        name = t.get("tool", "?")
        if t.get("error"):
            rep.fail(f"{name}: the test failed ({t['error']})")
            continue
        check_completeness(name, t, rep)
        check_endpoints(name, t, rep)
        check_slo(name, t, rep, slo_p99, slo_err)


def check_completeness(name: str, t: dict, rep: Report) -> None:
    """Every coverage axis the test claims must have carried samples."""
    if not t.get("coverage"):
        rep.fail(f"{name}: no coverage record")
        return
    missing = [
        axis
        for axis, metric in COVERAGE_SERIES.items()
        if t["coverage"].get(axis) and metric_count(t, metric) == 0
    ]
    if missing:
        rep.fail(f"{name}: claimed coverage with no series for {', '.join(missing)}")
    else:
        rep.ok(
            f"{name}: complete ({len(t['series'])} samples, "
            f"{len(t.get('stages', []))} stage(s))"
        )


def check_endpoints(name: str, t: dict, rep: Report) -> None:
    """The throughput sample must have dialed the tool, not its backend."""
    ep = (t.get("endpoints") or {}).get("throughput") or {}
    exposed, backend = ep.get("exposed"), ep.get("backend")
    if exposed is None:
        rep.legacy(
            f"{name}: no throughput endpoint recorded — the run predates the "
            "provenance record, so the endpoint invariant cannot be checked "
            "from it (re-run with the current harness, or record the waiver "
            "in HANDOFF.md)"
        )
    elif exposed == backend:
        rep.fail(
            f"{name}: throughput dialed the backend ({backend}), not "
            "the tool's exposed port"
        )
    else:
        rep.ok(
            f"{name}: throughput endpoint is the exposed port {exposed} "
            f"(backend {backend})"
        )


def check_slo(name: str, t: dict, rep: Report, slo_p99, slo_err) -> None:
    """The absolute SLO, applied where the model defines it to apply.

    The SLO is a *clean-path* contract: the compliant path is the unshaped
    control stage, and a saturated `rrul`/`soak` stage under a shaped class is
    expected to sit far above it — that degradation is the measurement, not a
    violation. So the verdict is per **clean** stage, and what the shaped
    stages did is reported beside it rather than judged by a line that was
    never meant to hold there.
    """
    clean = [s for s in t.get("stages", []) if s.get("stage") == "clean"]
    if not clean:
        rep.note(
            f"{name}: no clean stage in the schedule — the SLO cannot be "
            "judged for this test"
        )
        return
    subject = name.startswith(SUBJECT)
    # The SLO gates the tool this repository releases; a peer that misses it is
    # a finding about the peer (reported, with its number) and not a block.
    over = rep.fail if subject else rep.note
    peer_note = "" if subject else " (reference peer — reported, not gated)"
    for stage in clean:
        p99, err = stage.get("rtt_p99"), stage.get("rtt_error_rate")
        if p99 is None:
            rep.note(f"{name} clean: no interactive p99 to judge")
        elif slo_p99 is not None and p99 > slo_p99:
            over(
                f"{name} clean: interactive p99 {p99} ms is over the SLO "
                f"({slo_p99} ms){peer_note}"
            )
        else:
            rep.ok(
                f"{name} clean: interactive p99 {p99} ms is inside the SLO "
                f"({slo_p99} ms)"
            )
        if err is not None and slo_err is not None and err > slo_err:
            over(
                f"{name} clean: interactive error rate {err} is over the SLO "
                f"({slo_err}){peer_note}"
            )
    shaped = [
        s.get("rtt_p99")
        for s in t.get("stages", [])
        if s.get("stage") != "clean" and s.get("rtt_p99") is not None
    ]
    if shaped:
        rep.note(
            f"{name}: {len(shaped)} shaped stage(s) sit above the SLO by "
            f"design, p99 up to {max(shaped)} ms — that is the degradation "
            "curve, not a verdict"
        )


def check_capacity(tool: str, rep: Report, c: dict, b: dict) -> None:
    cm, bm = c["metrics"], b["metrics"]
    if "max_sustainable_streams" not in cm or "max_sustainable_streams" not in bm:
        return
    lim = env_pct("capacity_streams_pct")
    cur_s, base_s = cm["max_sustainable_streams"], bm["max_sustainable_streams"]
    if base_s:
        d = (cur_s - base_s) / base_s * 100.0
        (rep.ok if d >= -lim else rep.fail)(
            f"{tool} capacity: {base_s} -> {cur_s} streams "
            f"({d:+.1f}%, limit -{lim:.0f}%)"
        )
    else:
        rep.note(f"{tool} capacity: baseline measured no sustainable load")
    lim = env_pct("capacity_rtt_p99_pct")
    for point in cm.get("curve", []):
        bp = {p["streams"]: p for p in bm.get("curve", [])}.get(point["streams"])
        if not bp or bp.get("rtt_p99") is None or point.get("rtt_p99") is None:
            continue
        rep.limit(
            point["rtt_p99"],
            bp["rtt_p99"],
            lim,
            f"{tool} capacity p99 @ {point['streams']} streams: "
            f"{bp['rtt_p99']} -> {point['rtt_p99']} ms",
        )


def check_stages(tool: str, rep: Report, c: dict, b: dict) -> None:
    """Per-stage verdicts: the interactive distribution and the worst second."""
    p99_lim = env_pct("rrul_rtt_p99_pct")
    worst_lim = env_pct("rrul_worst_1s_pct")
    for stage_c in c.get("stages", []):
        stage_b = next(
            (s for s in b.get("stages", []) if s.get("stage") == stage_c.get("stage")),
            None,
        )
        if stage_b is None:
            continue
        rep.limit(
            stage_c.get("rtt_p99"),
            stage_b.get("rtt_p99"),
            p99_lim,
            f"{tool} {stage_c['stage']} p99: "
            f"{stage_b.get('rtt_p99')} -> {stage_c.get('rtt_p99')} ms",
        )
        # The worst second is the stability axis: a stage may hold its median
        # while a single second blows up, which is what a queue does.
        worst_c = stage_c.get("rtt_worst_1s")
        worst_b = stage_b.get("rtt_worst_1s")
        if worst_c is not None and worst_b is not None:
            rep.limit(
                worst_c,
                worst_b,
                worst_lim,
                f"{tool} {stage_c['stage']} worst 1s: {worst_b} -> {worst_c} ms",
            )
        # a stage that wedges only in the current run is a regression even
        # when the numbers that did land look fine
        if stage_c.get("flat_segments") and not stage_b.get("flat_segments"):
            rep.fail(
                f"{tool} {stage_c['stage']}: wedge appeared "
                f"({len(stage_c['flat_segments'])} flat segment(s))"
            )


def check_cost_and_drift(tool: str, rep: Report, c: dict, b: dict) -> None:
    cm, bm = c["metrics"], b["metrics"]
    cg, bg = cm.get("cost_cpu_per_gbit"), bm.get("cost_cpu_per_gbit")
    if cg is not None and bg is not None:
        rep.limit(
            cg,
            bg,
            env_pct("cost_cpu_per_gbit_pct"),
            f"{tool} cost: {bg} -> {cg} CPU-s/Gbit",
        )
    leak_axis = c.get("test") == "soak"
    for metric, lim, unit in (
        ("server_fds_slope_per_min", env_pct("drift_fds_per_min"), "fds/min"),
        (
            "server_rss_kb_slope_per_min",
            env_pct("drift_rss_mb_per_min") * 1024,
            "KiB/min",
        ),
    ):
        v = cm.get(metric)
        if v is None:
            continue
        label = metric.replace("_slope_per_min", "")
        base_v = bm.get(metric)
        if leak_axis or base_v is None:
            # The absolute limits are the *soak* leak axis, which is what they
            # are calibrated for: over a long run, one fd per minute is hundreds
            # of fds. Every other test type is compared against its baseline
            # instead, because a short churn-heavy run grows server fds by
            # warm-up — identically in both runs (molehill 31 -> 165 at v0.9.0,
            # 31 -> 169 here), so an absolute limit would report a property of
            # the workload as a regression.
            (rep.ok if abs(v) <= lim else rep.fail)(
                f"{tool} drift {label}: {v:+} {unit} (limit ±{lim:.0f})"
            )
        else:
            (rep.ok if v <= base_v + lim else rep.fail)(
                f"{tool} drift {label}: {base_v:+} -> {v:+} {unit} "
                f"(rise limit +{lim:.0f})"
            )
    # The stored rates are fractions (0.00248 is 0.248%) and the limit is in
    # percentage points, so the comparison is a *difference*, not a ratio: a
    # ratio turns a 0.03pp wobble on a 0.25% rate into "+13%", which reads as a
    # violation and is not one.
    base_err, cur_err = (
        bm.get("interactive_error_rate"),
        cm.get("interactive_error_rate"),
    )
    if base_err is not None and cur_err is not None:
        rise_pp = (cur_err - base_err) * 100
        (rep.ok if rise_pp <= ERROR_RATE_RISE_PP else rep.fail)(
            f"{tool} interactive error rate: {base_err} -> {cur_err} "
            f"({rise_pp:+.2f}pp, limit +{ERROR_RATE_RISE_PP:.1f}pp)"
        )


def gate(cur: dict, base: dict | None) -> int:
    """The release verdict: self-check, then per-type comparison."""
    rep = Report()
    print(
        f"current : {cur['meta'].get('date')} "
        f"revision {cur['meta'].get('revision', 'unrecorded')}"
    )
    if not cur["meta"].get("revision"):
        rep.legacy(
            "the results meta has no revision — the run cannot be tied to a "
            "checkout (AGENTS.md §10, 'prove provenance')"
        )
    print("\n# Run self-check (completeness, endpoints, absolute SLO)")
    check_run(cur, rep)
    if base is None:
        print(
            "\nno baseline: the first soak run is gated by the absolute SLO "
            "above (see docs/release.md, 'Benchmarks')"
        )
    else:
        print(
            f"\nbaseline: {base['meta'].get('date')} "
            f"revision {base['meta'].get('revision', 'unrecorded')}"
        )
        # The comparability boundary, enforced instead of assumed: a number
        # from another host is not a gate input (docs/release.md,
        # docs/benchmarks.md). Two runs whose high-throughput tools differ by
        # a third while ones that never reach that ceiling do not are
        # describing two environments, not two builds — measured here: the
        # container was recreated between the runs, molehill and rathole both
        # lost ~33% of clean bulk and frp/nps were flat, which is a property
        # of where the run happened.
        why = comparability(base, cur)
        if why is not None:
            print("\n# Comparison against the baseline: skipped")
            print(f"  NOTE  baseline is not a gate input: {why}")
            print(
                "  NOTE  the run above is gated by its own checks: "
                "completeness, the endpoint invariant and the absolute SLO"
            )
        else:
            cur_tools = {t["tool"]: t for t in cur["tests"] if not t.get("error")}
            base_tools = {t["tool"]: t for t in base["tests"] if not t.get("error")}
            print("\n# Comparison against the baseline")
            for tool, c in sorted(cur_tools.items()):
                b = base_tools.get(tool)
                if b is None:
                    rep.note(f"{tool}: not in the baseline (new tool?)")
                    continue
                rep.set_subject(tool)
                check_capacity(tool, rep, c, b)
                check_stages(tool, rep, c, b)
                check_cost_and_drift(tool, rep, c, b)
    print()
    if rep.legacy_checks:
        print(
            f"{rep.legacy_checks} check(s) could not be answered by this data "
            "(LEGACY above): the run predates the provenance record"
        )
    if rep.violations:
        print(
            f"FAIL: {rep.violations} violation(s) — fix, or waive "
            "explicitly (HANDOFF.md)"
        )
        return 1
    print("OK: no gate violation")
    return 0


def screen(data: dict) -> int:
    """The A/B verdict for a `--test=screen` run.

    Read in one direction only: every delta is `head - base` where A is the
    first build on the command line. A step that favours B is reported as
    such, and a run where every step favours B is a claim for B — the
    sequential decision the runner's interleave exists to support.
    """
    t = next((t for t in data["tests"] if t["test"] == "screen"), None)
    if t is None:
        sys.exit("no screen test in that results file")
    rounds = t["metrics"].get("rounds") or []
    builds = t["metrics"].get("builds") or {}
    print(
        f"screen: A={builds.get('A_version')} ({builds.get('A')})\n"
        f"        B={builds.get('B_version')} ({builds.get('B')})"
    )
    if not rounds:
        sys.exit("no rounds recorded")
    # Which metric the table shows, decided by what the run actually produced —
    # not by its first step. A cell hostile enough to kill the bulk probe on
    # step 1 (the MTU/fragmentation cell does exactly that) used to flip the
    # whole verdict to response time while the columns still read like
    # throughput, so a "claim B" could be about milliseconds and look like
    # Gbit/s. Throughput is the primary metric: use it whenever any step has
    # it, and label the table so the units are never in doubt.
    have_gbps = any(p.get("gbps") is not None for r in rounds for p in r["pair"])
    key = "gbps" if have_gbps else "rtt_p99"
    unit = "Gbit/s" if key == "gbps" else "ms (p99)"
    if not have_gbps:
        print(
            "\nnote: no step produced a throughput sample (the bulk probe "
            "failed on every step); comparing response time instead"
        )
    # For throughput a higher A is better; for a response time a lower A is.
    better = 1.0 if key == "gbps" else -1.0
    print(f"\n{'streams':>8}{'A':>10}{'B':>10}{'delta':>9}   reading  [{unit}]")
    steps = 0
    a_ahead = b_ahead = 0
    for r in rounds:
        a = next((p for p in r["pair"] if p["build"] == "A"), {}).get(key)
        b = next((p for p in r["pair"] if p["build"] == "B"), {}).get(key)
        if a is None or b is None or b == 0:
            print(f"{r['streams']:>8}{'—':>10}{'—':>10}{'—':>9}   no data")
            continue
        steps += 1
        d = (a - b) / b * 100.0
        strong = abs(d) >= SCREEN_CLAIM_PCT
        a_wins = d * better > 0
        if strong and a_wins:
            a_ahead += 1
        elif strong:
            b_ahead += 1
        winner = "A" if a_wins else "B"
        print(
            f"{r['streams']:>8}{a:>10.3f}{b:>10.3f}{d:>+8.1f}%   "
            f"{'claim ' + winner if strong else 'directional ' + winner}"
        )
    print()
    if steps < SCREEN_MIN_STEPS:
        print(
            f"NO CLAIM: {steps} usable step(s) — a verdict needs "
            f"{SCREEN_MIN_STEPS} at least"
        )
        return 0
    for label, wins in (("A", a_ahead), ("B", b_ahead)):
        if wins == steps:
            other = "B" if label == "A" else "A"
            print(
                f"CLAIM: {label} ahead on every step by >= "
                f"{SCREEN_CLAIM_PCT:.0f}% ({steps} steps) — pursue the "
                f"direction (the metric favours {label} over {other})"
            )
            return 0
    print(
        f"DIRECTIONAL: A ahead on {a_ahead}/{steps} steps and B on "
        f"{b_ahead}/{steps}; not a claim at the "
        f"{SCREEN_CLAIM_PCT:.0f}% threshold — the effect is inside the "
        "noise or the host is noisy today"
    )
    return 0


def resolve_paths(args: list) -> tuple:
    """The (current, baseline) files to compare, from argv or by convention."""
    here = Path(__file__).parent
    found = sorted(here.glob("results-soak-*.json"), key=lambda p: p.name)
    cur = Path(args[0]) if args else (found[-1] if found else None)
    if cur is None:
        sys.exit("no results-soak-*.json found")
    if len(args) > 1:
        return cur, Path(args[1])
    older = [p for p in found if p.name < cur.name]
    return cur, (older[-1] if older else None)


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] == "--screen":
        if len(args) < SCREEN_ARGS:
            sys.exit("usage: soak_check.py --screen <results.json>")
        sys.exit(screen(json.loads(Path(args[1]).read_text())))
    cur, base = resolve_paths(args)
    print(f"current : {cur.name}")
    if base is not None:
        print(f"baseline: {base.name}")
    sys.exit(
        gate(
            json.loads(cur.read_text()), json.loads(base.read_text()) if base else None
        )
    )


if __name__ == "__main__":
    main()
