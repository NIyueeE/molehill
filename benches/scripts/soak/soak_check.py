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
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

import lib
import soak

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
#: The verdict a *shaped* stage class can still carry: a blow-up bound, not a
#: difference test. A netem stage's interactive distribution is dominated by the
#: queue the harness itself installed, and one run of that class does not
#: repeat: measured on this host, three runs of one unchanged method moved the
#: shaped cells by 24-86 % (HANDOFF.md, "Shaped-cell resolution") against a
#: per-stage limit of 25 %. Gating those cells at the limit is gating noise, so
#: the gate reports them as context and keeps only a safety net — a shaped stage
#: that triples is a regression whatever the noise does.
SHAPED_BLOWUP_PCT = 200.0
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
#: The axes a test type records when it is not a staged walk. A capacity ramp
#: drives its load through `iperf_burst` (one sample per load level, in
#: `metrics.curve`) instead of the staged `throughput_bulk_gbps` series, so the
#: claim to check is this set — and an entry that claims more than its type can
#: record is reported, because that is a record describing a series it never
#: wrote.
TEST_COVERAGE = {
    "capacity": ("tcp_interactive", "tcp_churn", "udp_session"),
}
# A stage's bulk intervals are the ones timestamped inside its window. The
# runner marks `t_start` *before* the spine starts, so no interval of a stage
# can precede it — and allowing a tolerance below `t_start` would credit a dead
# stage with the previous stage's dying spine (measured: the frozen-commit
# sweep's `jitter` stage read "2 intervals" that way, and both samples were the
# `rate20` tail at `t_start - 1.8 s` and `-0.8 s`; the jitter spine itself
# recorded none). The window is inclusive at the right edge because a stage's
# last interval can be emitted a fraction of a second after its nominal end.


def min_bulk_intervals(secs: float) -> int:
    """The fewest bulk intervals a stage of this length may carry and count.

    The spine emits one interval per second, so a stage that ran its bulk load
    carries roughly `secs` minus a warmup of them; the floor is deliberately
    far below that so ordinary variance never trips it, and only a spine that
    did not run at all is caught. One interval per 30 s, minimum one, separates
    a dead stage (zero) from a loaded one (91-294 on the frozen-commit sweep),
    and the healthy stages of all four tools sit 17-37x above it.
    """
    return max(1, int(secs // 30))


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


#: The `meta` keys that carry the **method** rather than the machine or the
#: result: what was measured, over which schedule, with which instrument
#: settings. Two files that disagree on any of them were produced by different
#: instruments, and comparing them would print verdicts about a method nobody
#: ran. `workload_version` is not enough on its own — it is a single integer
#: for the whole model, and this cycle changed five of these keys while it
#: stayed `1` (the drain's introduction, the backlog suffix, the spine retry,
#: the drain budget, and the drain predicate), so the gate would have called
#: two different instruments comparable.
#:
#: `slow_visitor_bps` is deliberately absent: it has its own check below, with
#: a better message, because a missing key there means "no visitor" rather than
#: "unknown method".
METHOD_KEYS = (
    "timeline",
    "path_classes",
    "streams_max",
    "settle_s",
    "stage_drain_budget_s",
    "drain_backlog_tolerance_b",
    "drain_send_states",
    "spine_retry_s",
    # The legs a stage's class is applied to, and how long a spine waits for
    # the receiver's summary: the first decides what a rate class measures
    # (one access link or two sharing it), the second whether a stage has a
    # receiver half at all.
    "shape_legs",
    "spine_summary_grace_s",
    # The bulk client's socket window on a rate class: empty for the client's
    # own default. It decides whether those cells have a sender the path can
    # keep up with, so a run measured with a window is a different instrument.
    "rate_socket_window",
    "interactive_ping_interval_ms",
    "udp_ping_interval_ms",
    "churn_connects_s",
    "slo",
    "load_fractions",
    "mtu_restore_to",
    "cores_per_pair",
    "batch",
)


#: How many differing key names to spell out before eliding the rest: enough to
#: diagnose the usual one-or-two, short enough to stay one line.
_FMT_KEYS_MAX = 6

#: A stage class has to appear at least this often in the schedule to be a
#: replicate of itself. Two is the schedule's `clean` at both ends.
_REPLICATE_MIN = 2


def _fmt_keys(keys: list) -> str:
    more = " …" if len(keys) > _FMT_KEYS_MAX else ""
    return ", ".join(f"`{k}`" for k in keys[:_FMT_KEYS_MAX]) + more


def comparability(base: dict, cur: dict) -> str | None:
    """Why these two runs may not be compared, or `None` when they may.

    docs/release.md and docs/benchmarks.md both state the boundary: only
    same-schema, same-method, same-host runs are comparable. This is that
    sentence as a function, so the gate refuses an invalid comparison instead
    of printing verdicts nobody may act on.

    **Every** blocking reason is reported, not just the first. A baseline can
    fail more than one — the release path's `results-soak-v0.9.0.json` differs
    in host *and* in method — and naming only the first would tell a reader
    that clearing it would make the pair comparable, which would be false and
    would cost them a run to discover.

    The host key is `meta.host_id` — machine id + CPU model + core count,
    because the path, the CPU budget and the loopback ceiling are properties of
    the *machine*, and a container hostname (what the records carried before)
    changes on every restart while the hardware does not: keying on it refused
    same-host runs and would admit a different host that happened to reuse the
    name. Runs that predate the field fall back to the recorded hostname, which
    is conservative in the safe direction (refusing to compare) and recorded in
    HANDOFF.md as the method fix — a stable host identity is a calibration
    measurement, not a name.
    """
    reasons = []
    if base["meta"].get("workload_version") != cur["meta"].get("workload_version"):
        reasons.append(
            "the runs have different workload versions "
            f"({base['meta'].get('workload_version')} vs "
            f"{cur['meta'].get('workload_version')}): different method"
        )
    bh, ch = base["meta"].get("host_id"), cur["meta"].get("host_id")
    if bh and ch:
        if bh != ch:
            reasons.append(
                f"the runs were made on different hosts (host_id {bh} vs {ch}, "
                f"{base['meta'].get('hostname')} vs {cur['meta'].get('hostname')}): "
                "the path, the CPU budget and the loopback ceiling are properties "
                "of the machine, and the peers' clean-tool spread shows it"
            )
    else:
        # One of the runs predates `host_id`: compare the recorded hostnames,
        # which is what the older files have, and say which key was used.
        bh, ch = base["meta"].get("hostname"), cur["meta"].get("hostname")
        if bh and ch and bh != ch:
            reasons.append(
                f"the runs were made on different hosts ({bh} vs {ch}): the path, "
                "the CPU budget and the loopback ceiling are properties of where a "
                "run happens, and the peers' clean-tool spread shows it"
            )
    reasons.extend(
        r
        for r in (
            _comparability_visitor(base, cur),
            _comparability_method(base, cur),
            _comparability_calibration(base, cur),
        )
        if r
    )
    return " Also, ".join(reasons) if reasons else None


#: How far apart two runs' host calibrations may sit before the gate refuses to
#: compare them, in percent. Sized from both ends: the probe repeats to ~1-2 %
#: on an idle host (measured here; `host_calibration.spread_pct` records each
#: run's own figure), while a machine that is merely *busy* — a neighbour
#: stealing cores, a thermal-throttled clock — moves a fixed CPU workload by
#: far more than this, and those are exactly the runs whose throughput numbers
#: must not be compared with an idle run's. A quarter is deliberately loose:
#: refusing a comparable pair costs one run, accepting an incomparable one
#: publishes a verdict about the machine as if it were about the tool.
HOST_CALIBRATION_TOLERANCE_PCT = 25.0


def _comparability_calibration(base: dict, cur: dict) -> str | None:
    """Refuse two runs whose host was not in the same measured state.

    The identity check above answers "same machine?" by a *name*
    (`machine_id | cpu_model | nproc`, and on a host without a machine id that
    is the whole key). This answers the question the name cannot: was the
    machine in the same state? Both halves are needed — two machines can share
    every identity field, and one machine can be a different machine's worth of
    busy between two runs.

    A file that predates the probe is not read as agreement: it is reported as
    unverifiable, and the comparison proceeds on the identity check alone,
    which is the behaviour every stored file gets today.
    """
    bcal = base["meta"].get("host_calibration") or {}
    ccal = cur["meta"].get("host_calibration") or {}
    if not (bcal.get("ok") and ccal.get("ok")):
        return None  # reported by the caller's own note; never read as equal
    b, c = bcal.get("median"), ccal.get("median")
    if not b or not c:
        return None
    delta = abs(c - b) / b * 100.0
    if delta <= HOST_CALIBRATION_TOLERANCE_PCT:
        return None
    return (
        f"the hosts measure differently at the fixed calibration workload "
        f"({b} vs {c} MiB/s, {delta:.1f}% apart, over the "
        f"{HOST_CALIBRATION_TOLERANCE_PCT:.0f}% the gate allows): the runs did "
        "not see the same machine state (a different machine sharing the "
        f"identity, or one of them busy/throttled), and {bcal.get('probe')} is "
        "the measurement a host key alone cannot make"
    )


def _comparability_method(base: dict, cur: dict) -> str | None:
    """Refuse to compare two runs whose *method record* differs.

    The host check above answers "same machine?"; this answers "same
    instrument?", which is the half a single `workload_version` integer cannot
    carry. It is the check that matters most on one host — the case where
    everything else looks comparable.

    A key missing from either file is not treated as a default: unlike
    `slow_visitor_bps` (where absence means "off"), an absent
    `drain_backlog_tolerance_b` means the file predates that instrument, and
    reading it as "the default" would invent a method rather than refuse one.
    """
    bm, cm = base["meta"], cur["meta"]
    b_missing = [k for k in METHOD_KEYS if k not in bm]
    c_missing = [k for k in METHOD_KEYS if k not in cm]
    if b_missing or c_missing:
        who = []
        if b_missing:
            who.append(f"the baseline records no {_fmt_keys(b_missing)}")
        if c_missing:
            who.append(f"this run records no {_fmt_keys(c_missing)}")
        return (
            "the method record is incomplete — " + "; ".join(who) + ": a key "
            "absent from a file is an instrument that file cannot describe, so "
            "the two cannot be shown to have measured the same way"
        )
    diff = [k for k in METHOD_KEYS if bm[k] != cm[k]]
    if diff:
        detail = "; ".join(f"{k}: {bm[k]!r} vs {cm[k]!r}" for k in diff[:3])
        return (
            f"the runs used different methods ({_fmt_keys(diff)} differ — "
            f"{detail}): same host or not, a comparison would be a verdict "
            "about an instrument neither run used (AGENTS.md §10)"
        )
    return None


def _comparability_visitor(base: dict, cur: dict) -> str | None:
    # The injected slow visitor is part of the workload, not a viewer of it:
    # two runs that throttled it differently measured different paths. The
    # workload-version check above already separates "off" from "on" (an
    # enabled visitor bumps the version); this catches two *different* rates,
    # which both carry the same bumped version. Missing keys mean an older
    # file, i.e. no visitor, which is what 0 reads as.
    bv, cv = (
        base["meta"].get("slow_visitor_bps") or 0,
        (cur["meta"].get("slow_visitor_bps") or 0),
    )
    if bv != cv:
        return (
            f"the runs injected different slow visitors ({bv} vs {cv} bit/s): "
            "the visitor shares the path it measures, so its rate is a method "
            "parameter (AGENTS.md §10)"
        )
    return None


def calibration_note(base: dict, cur: dict) -> str | None:
    """What the host calibration could *not* verify about a comparison.

    Printed when the pair is compared anyway, because a check that silently
    does not run is the failure mode this whole function family exists to
    prevent: a reader has to know whether "same host" was verified by a
    measurement or only by a name. `None` when both files carried a probe —
    the pair was checked, and `comparability` already refused it if the two
    readings were too far apart to compare.
    """
    braw = base["meta"].get("host_calibration")
    craw = cur["meta"].get("host_calibration")
    if braw is not None and craw is not None:
        if braw.get("ok") and craw.get("ok"):
            return None
        who = "the baseline" if not braw.get("ok") else "this run"
        return (
            f"the host calibration probe did not run on {who}: 'same host' "
            "rests on the identity key alone, which on a host without a "
            "machine id is `cpu_model | nproc`"
        )
    who = [
        label
        for label, raw in (("the baseline", braw), ("this run", craw))
        if raw is None
    ]
    return (
        f"{' and '.join(who)} carr{'ies' if len(who) == 1 else 'y'} no host "
        "calibration: 'same host' rests on the identity key alone, which on a "
        "host without a machine id is `cpu_model | nproc`"
    )


def metric_count(test: dict, metric: str) -> int:
    return sum(1 for r in test.get("series", []) if r.get("metric") == metric)


def check_run(cur: dict, rep: Report) -> None:
    """The run's self-check: completeness, endpoints, absolute SLO.

    This is the part that makes a baseline-less release gate mean something,
    and it is the mechanical half of §10's "re-check every consumer after
    reshaping data" rule: a renamed or missing series, or a throughput sample
    that dialed the backend, must fail the gate rather than plot as a hole.
    """
    slo = SloContract.from_meta(cur.get("meta") or {})
    tests = cur.get("tests", [])
    if not tests:
        rep.fail("the results file contains no tests")
        return
    for t in tests:
        name = t.get("tool", "?")
        # One subject per test, set here: `check_completeness` and
        # `check_endpoints` gate through `rep.subject`, and `check_slo` used to
        # recompute it locally — so a peer's missing series failed the release
        # gate while its SLO did not.
        rep.set_subject(name)
        if t.get("error"):
            rep.fail(f"{name}: the test failed ({t['error']})")
            continue
        check_completeness(name, t, rep)
        check_endpoints(name, t, rep)
        if t.get("stages"):
            check_slo(name, t, rep, slo)
            check_bulk_readings(name, t, rep)
            check_replicate(name, t, rep)
        else:
            # A test that walks no schedule (the capacity ramp) is judged by
            # what it does record, not by staging checks that cannot apply.
            check_capacity_run(name, t, rep)


def check_capacity_run(name: str, t: dict, rep: Report) -> None:
    """A capacity ramp must carry the curve and the verdict it claims."""
    if t.get("test") != "capacity":
        return
    m = t.get("metrics") or {}
    curve = m.get("curve") or []
    if not curve:
        rep.fail(f"{name}: the capacity ramp recorded no load level")
    elif "max_sustainable_streams" not in m:
        rep.fail(f"{name}: the capacity ramp has no sustainable-load verdict")
    else:
        broken = next((p for p in curve if p.get("slo_broken")), None)
        why = (
            f", broken at {broken['streams']} ({broken.get('reason')})"
            if broken
            else ", never broke the SLO"
        )
        rep.ok(
            f"{name}: capacity ramp carried {len(curve)} load level(s), "
            f"{m['max_sustainable_streams']} sustainable{why}"
        )


def check_bulk_per_stage(name: str, t: dict, rep: Report) -> None:
    """Every stage must carry the bulk spine the coverage claims.

    A `throughput_bulk_gbps` count over the whole run hides a hole the size of
    one stage: the frozen-commit sweep's `jitter` stage recorded **no**
    intervals at all (the bulk client could not dial the exposed port and the
    retry timed out) and the run still read "complete (95660 samples, 8
    stage(s))", because the global count was 849 and every other stage carried
    its spine. A stage is where the bulk load either ran or did not, so the
    check is per stage against a floor that scales with the stage length, and a
    stage that recorded why it failed reports that reason rather than a bare
    zero.

    The intervals are collected once and then counted per stage: the series
    holds tens of thousands of samples and the naive form rescanned all of them
    for every stage.
    """
    if not (t.get("coverage") or {}).get("tcp_bulk"):
        return
    stamps = [
        r.get("t", 0)
        for r in t.get("series", [])
        if r.get("metric") == "throughput_bulk_gbps"
    ]
    for st in t.get("stages", []):
        t0 = st.get("t_start")
        if t0 is None:
            continue  # no `t_start`: the stage cannot be attributed to
        t1 = t0 + float(st.get("secs", 0))
        n = sum(1 for ts in stamps if t0 <= ts <= t1)
        floor = min_bulk_intervals(float(st.get("secs", 0)))
        if n >= floor:
            continue
        reason = st.get("bulk_error") or f"{n} interval(s) in {st.get('secs')}s"
        detail = st.get("bulk_client_error")
        rep.fail(
            f"{name} {st.get('stage')}: the bulk spine carried {n} interval(s), "
            f"below the {floor} a {st.get('secs')}s stage needs "
            f"({reason}{'; ' + detail if detail else ''})"
        )


def check_completeness(name: str, t: dict, rep: Report) -> None:
    """Every coverage axis the test claims must have carried samples."""
    if not t.get("coverage"):
        rep.fail(f"{name}: no coverage record")
        return
    claimed = t["coverage"]
    kind = t.get("test")
    if kind in TEST_COVERAGE:
        # Check what this test type records, and report a claim it cannot back.
        extra = [a for a in claimed if a not in TEST_COVERAGE[kind] and claimed[a]]
        if extra:
            rep.note(
                f"{name}: a {kind} entry claims {', '.join(sorted(extra))} — that "
                "test type records one sample per load level, not a series"
            )
        claimed = {a: claimed.get(a) for a in TEST_COVERAGE[kind]}
    missing = [
        axis
        for axis, metric in COVERAGE_SERIES.items()
        if claimed.get(axis) and metric_count(t, metric) == 0
    ]
    if missing:
        rep.fail(f"{name}: claimed coverage with no series for {', '.join(missing)}")
    else:
        rep.ok(
            f"{name}: complete ({len(t['series'])} samples, "
            f"{len(t.get('stages', []))} stage(s))"
        )
    check_bulk_per_stage(name, t, rep)


def _stage_bulk_reading(st: dict):
    """The stage's bulk reading, derived from its recorded evidence.

    Derived rather than read from a stored value: the rule needs the interval
    distribution, the class and the client's `end` event, all of which the
    stage records, and the same function serves the plot and the runner's log —
    one rule with three readers instead of three rules. A stored reading would
    also be a number frozen under whatever rule was current the day it ran.
    """
    return soak.bulk_reading(st)[0]


def check_bulk_readings(name: str, t: dict, rep: Report) -> None:
    """Every stage that carried a spine must state what it carried.

    A stage with intervals but no reading is not a failure — it is a stage
    whose *sender* accounting was defeated (the shape of a shaped stage) and
    whose dial produced no receiver summary, so the data says what happened
    but not how much the path carried. It is reported rather than passed in
    silence, because a table with a hole in it and no explanation is how a
    degenerate cell gets read as a zero.
    """
    for st in t.get("stages", []):
        gbps, why = soak.bulk_reading(st)
        if st.get("bulk_intervals") and gbps is None:
            rep.note(
                f"{name} {st.get('stage')}: {st['bulk_intervals']} bulk "
                f"interval(s) but no reading — {why}"
            )


def check_replicate(name: str, t: dict, rep: Report) -> None:
    """Report what the schedule's repeated stage classes say about noise.

    Every stage is one sample, so a run cannot normally state its own
    repeatability — but the schedule measures `clean` at both ends of every
    timeline. Those two readings are two samples of the same condition taken
    about an hour apart, and they are the closest thing this instrument has to
    a control. Their agreement is the scale a reader has to hold every other
    cell against: a between-tool difference smaller than the same condition's
    spread an hour apart is not a difference this run can see.

    Reported, never judged. Variance is data (AGENTS.md §10): a threshold here
    would be invented, and the point is to publish the number the other claims
    have to clear, not to fail a run for being noisy.
    """
    by_class: dict = {}
    for st in t.get("stages", []):
        by_class.setdefault(st.get("stage"), []).append(st)
    for cls, sts in by_class.items():
        if len(sts) < _REPLICATE_MIN:
            continue
        peaks = [_stage_bulk_reading(st) for st in sts]
        p99s = [st.get("rtt_p99") for st in sts]
        if any(p is None for p in peaks) or any(p is None for p in p99s):
            continue
        blo, bhi = min(peaks), max(peaks)
        rlo, rhi = min(p99s), max(p99s)
        # A peak of zero makes the relative spread meaningless; report the
        # absolute readings instead of inventing a percentage.
        spread = f"{(bhi - blo) / bhi * 100:.1f}%" if bhi else "n/a (both 0)"
        rep.note(
            f"{name}: the schedule measures `{cls}` {len(sts)} times, so those are "
            f"the run's own replicate — bulk {blo:.3f}-{bhi:.3f} Gbit/s "
            f"({spread} apart), interactive p99 {rlo:.1f}-{rhi:.1f} ms; a "
            "between-tool difference smaller than this is not one this run can see"
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


@dataclass(frozen=True)
class SloContract:
    """The SLO the run was taken against, plus the classes it applies to.

    One value rather than three parameters: the p99, the error rate and the
    set of *unshaped* classes are all "what this run's numbers may be judged
    against", and a caller that got two of the three would judge a shaped
    stage by a clean-path contract — the failure this grouping makes
    unrepresentable.
    """

    rtt_p99_ms: float | None
    error_rate: float | None
    #: Stage classes that apply no netem: the control stages, where the SLO is
    #: a contract and a single run can compare numbers.
    comparable: frozenset

    @classmethod
    def from_meta(cls, meta: dict) -> "SloContract":
        slo = meta.get("slo") or {}
        classes = meta.get("path_classes") or {}
        return cls(
            rtt_p99_ms=slo.get("rtt_p99_ms"),
            error_rate=slo.get("error_rate"),
            comparable=frozenset(
                name for name, cls in classes.items() if not (cls or {}).get("netem")
            ),
        )

    def is_shaped(self, stage: str) -> bool:
        """Whether a stage class imposes a netem queue (see `SHAPED_BLOWUP_PCT`)."""
        return stage not in self.comparable


def check_slo(name: str, t: dict, rep: Report, slo: SloContract) -> None:
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
    # The SLO gates the tool this repository releases; a peer that misses it is
    # a finding about the peer (reported, with its number) and not a block.
    # `rep.subject` is set per test by `check_run`, so this agrees with the
    # completeness and endpoint checks instead of deciding on its own.
    subject = rep.subject
    over = rep.fail if subject else rep.note
    peer_note = "" if subject else " (reference peer — reported, not gated)"
    for stage in clean:
        p99, err = stage.get("rtt_p99"), stage.get("rtt_error_rate")
        if p99 is None:
            rep.note(f"{name} clean: no interactive p99 to judge")
        elif slo.rtt_p99_ms is not None and p99 > slo.rtt_p99_ms:
            over(
                f"{name} clean: interactive p99 {p99} ms is over the SLO "
                f"({slo.rtt_p99_ms} ms){peer_note}"
            )
        else:
            rep.ok(
                f"{name} clean: interactive p99 {p99} ms is inside the SLO "
                f"({slo.rtt_p99_ms} ms)"
            )
        if err is not None and slo.error_rate is not None and err > slo.error_rate:
            over(
                f"{name} clean: interactive error rate {err} is over the SLO "
                f"({slo.error_rate}){peer_note}"
            )
    shaped = [
        s.get("rtt_p99")
        for s in t.get("stages", [])
        if slo.is_shaped(s.get("stage")) and s.get("rtt_p99") is not None
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


def check_stages(tool: str, rep: Report, c: dict, b: dict, slo: SloContract) -> None:
    """Per-stage verdicts: the interactive distribution and the worst second.

    Stages are matched by **occurrence**, not by name. The default schedule
    opens and closes with the same `clean` condition (the recovery axis), so a
    name-keyed lookup pairs the *return* clean stage with the baseline's
    *initial* clean stage — comparing a recovered tool against a fresh one and
    calling the recovery axis silent. Both walks are the same schedule, so the
    k-th stage of a name in one run is the k-th of that name in the other; a
    name that appears a different number of times is reported, not guessed at.
    """
    p99_lim = env_pct("rrul_rtt_p99_pct")
    worst_lim = env_pct("rrul_worst_1s_pct")

    # Bucket both runs' stages by name, in order: [clean, rtt100, ..., clean].
    def by_name(t: dict) -> dict:
        buckets: dict = {}
        for s in t.get("stages", []):
            buckets.setdefault(s.get("stage"), []).append(s)
        return buckets

    base_stages = by_name(b)
    cur_seen: dict = {}
    for stage_c in c.get("stages", []):
        name = stage_c.get("stage")
        index = cur_seen.get(name, 0)
        cur_seen[name] = index + 1
        candidates = base_stages.get(name, [])
        if index >= len(candidates):
            rep.note(
                f"{tool} {name}: the baseline has no stage #{index + 1} of that "
                "name — the schedules differ, so that stage is not compared"
            )
            continue
        stage_b = candidates[index]
        # A shaped class is context: its p99 is dominated by the queue the
        # harness installed, and one run does not repeat it (see
        # `SHAPED_BLOWUP_PCT`). It is reported, and only a blow-up fails.
        shaped = slo.is_shaped(name)
        if shaped:
            rep.note(
                f"{tool} {name} p99: {stage_b.get('rtt_p99')} -> "
                f"{stage_c.get('rtt_p99')} ms (shaped class — context, not a "
                "verdict: a same-code re-run moves this cell by far more than "
                "the limit)"
            )
        else:
            rep.limit(
                stage_c.get("rtt_p99"),
                stage_b.get("rtt_p99"),
                p99_lim,
                f"{tool} {stage_c['stage']} p99: "
                f"{stage_b.get('rtt_p99')} -> {stage_c.get('rtt_p99')} ms",
            )
        # The worst second is the stability axis: a stage may hold its median
        # while a single second blows up, which is what a queue does. It is a
        # shaped cell too, so it gets the same blow-up bound rather than the
        # difference test (a queue that spikes is the *measurement* at a shaped
        # stage, not a regression).
        worst_c = stage_c.get("rtt_worst_1s")
        worst_b = stage_b.get("rtt_worst_1s")
        if worst_c is not None and worst_b is not None:
            rep.limit(
                worst_c,
                worst_b,
                SHAPED_BLOWUP_PCT if shaped else worst_lim,
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
            # Keyed by (tool, test): one file may carry several test types per
            # tool — the release sweep runs the staged schedule *and* the load
            # ramp into the same artifact — and keying by tool alone silently
            # kept whichever came last and dropped the other.
            cur_tests = {
                (t["tool"], t.get("test")): t
                for t in cur["tests"]
                if not t.get("error")
            }
            base_tests = {
                (t["tool"], t.get("test")): t
                for t in base["tests"]
                if not t.get("error")
            }
            per_tool = Counter(tool for tool, _ in cur_tests)
            print("\n# Comparison against the baseline")
            cal = calibration_note(base, cur)
            if cal is not None:
                rep.note(cal)
            for (tool, test), c in sorted(cur_tests.items()):
                b = base_tests.get((tool, test))
                # Name the test type only where a tool has more than one: a
                # single-test file keeps the short label its history uses.
                label = f"{tool} [{test}]" if per_tool[tool] > 1 else tool
                if b is None:
                    rep.note(f"{label}: not in the baseline (new test?)")
                    continue
                rep.set_subject(tool)
                check_capacity(label, rep, c, b)
                check_stages(label, rep, c, b, SloContract.from_meta(cur["meta"]))
                check_cost_and_drift(label, rep, c, b)
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


def screen_slow_visitor(rounds: list) -> None:
    """The slow visitor's state beside the verdict it shaped.

    The interactive p99 the table judges was measured with this visitor in
    the path, so a stage where it failed (or never completed) has to be
    visible here and not only in the series.
    """
    states = [p.get("slow_visitor_state") for r in rounds for p in r["pair"]]
    if not any(states):
        return
    # One pass, not `states.count(s)` per distinct state: the list holds two
    # entries per round, so the old form was quadratic in the round count for
    # no reason (a screen run is short today, but this reads a per-round table
    # and had no bound).
    counts = {s: n for s, n in sorted(Counter(states).items()) if s}
    print(f"        slow visitor (SOAK_SLOW_VISITOR_BPS): {counts}")
    for reason in sorted(
        {
            p.get("slow_visitor_reason")
            for r in rounds
            for p in r["pair"]
            if p.get("slow_visitor_reason")
        }
    ):
        print(f"        slow visitor failure: {reason}")


def screen(data: dict) -> int:
    """The A/B verdict for a `--test=screen` run.

    Read in one direction only: every delta is `head - base` where A is the
    first arm on the command line. A step that favours B is reported as
    such, and a run where every step favours B is a claim for B — the
    sequential decision the runner's interleave exists to support.

    Two axes reach this file: two builds (`--ab`) and two variants of one
    binary (`--ab-variants`). The header says which one was measured —
    printing a version and a path on both sides of a variant run would
    present a config comparison as a build comparison.

    Every metric the run carries gets its own table and its own verdict, in
    `SCREEN_METRICS` order. One metric used to be chosen for the whole run —
    throughput whenever any step had it — and that decision can hide the
    answer: a run whose throughput was pure noise reported "not a claim" while
    the interactive p99 favoured one arm on every one of its twenty steps
    (measured: the shared-pool screen, HANDOFF "The pool-size question,
    re-opened"), and a run whose bulk probe died on the first step flipped to
    response time without the columns saying so.

    Four steps, one function each: what was compared, the slow visitor, the
    tables, then a verdict per table. The split is what keeps each piece
    readable (the whole thing measured C901 12 as one function).
    """
    t = next((t for t in data["tests"] if t["test"] == "screen"), None)
    if t is None:
        sys.exit("no screen test in that results file")
    rounds = t["metrics"].get("rounds") or []
    screen_header(t["metrics"].get("builds") or {})
    screen_slow_visitor(rounds)
    if not rounds:
        sys.exit("no rounds recorded")
    metrics = screen_metrics(rounds)
    if not metrics:
        sys.exit("no step carried a comparable metric")
    for key, better, unit in metrics:
        steps, a_ahead, b_ahead = screen_table(rounds, key, better, unit)
        print()
        screen_verdict(steps, a_ahead, b_ahead, unit)
    return 0


def screen_header(builds: dict) -> None:
    """Say which two things were compared, and on which axis."""
    if builds.get("axis") == "variant":
        print(
            f"screen: axis=variant  A={builds.get('A_variant')} "
            f"(bin {builds.get('A_version')} {builds.get('A')})\n"
            f"        B={builds.get('B_variant')} (same binary)"
        )
    else:
        print(
            f"screen: A={builds.get('A_version')} ({builds.get('A')})\n"
            f"        B={builds.get('B_version')} ({builds.get('B')})"
        )


#: The metrics a screen reports, in order: `(key, sign that favours A, unit)`.
#: A higher throughput is better, a lower response time is. Both are reported
#: when the run carries both, because "the effect is in the metric nobody
#: printed" is a trap this instrument has already fallen into.
SCREEN_METRICS = (
    ("gbps", 1.0, "Gbit/s"),
    ("rtt_p99", -1.0, "ms (p99)"),
)


def screen_metrics(rounds: list) -> list:
    """The metrics this run produced, with the sign that favours A.

    Decided by what the run actually carried — not by its first step, and not
    by a preference: a cell hostile enough to kill the bulk probe (the
    MTU/fragmentation cell does that) simply has no throughput column, and the
    response-time table stands alone with its units in the header.
    """
    present = [
        (key, better, unit)
        for key, better, unit in SCREEN_METRICS
        if any(p.get(key) is not None for r in rounds for p in r["pair"])
    ]
    if not any(key == "gbps" for key, _, _ in present) and present:
        print(
            "\nnote: no step produced a throughput sample (the bulk probe "
            "failed on every step); response time is the only table"
        )
    return present


def screen_table(rounds: list, key: str, better: float, unit: str) -> tuple:
    """Print one table for one metric; return `(usable, a_ahead, b_ahead)`."""
    steps = 0
    a_ahead = b_ahead = 0
    print(f"\n--- {unit} ---")
    print(f"{'streams':>8}{'A':>10}{'B':>10}{'delta':>9}   reading  [{unit}]")
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
    return steps, a_ahead, b_ahead


def screen_verdict(steps: int, a_ahead: int, b_ahead: int, unit: str) -> None:
    """One metric's verdict line. The exit code is `screen`'s, always 0."""
    if steps < SCREEN_MIN_STEPS:
        print(
            f"NO CLAIM [{unit}]: {steps} usable step(s) — a verdict needs "
            f"{SCREEN_MIN_STEPS} at least"
        )
        return
    for label, wins in (("A", a_ahead), ("B", b_ahead)):
        if wins == steps:
            other = "B" if label == "A" else "A"
            print(
                f"CLAIM [{unit}]: {label} ahead on every step by >= "
                f"{SCREEN_CLAIM_PCT:.0f}% ({steps} steps) — pursue the "
                f"direction (the metric favours {label} over {other})"
            )
            return
    # The steps neither arm won were inside the threshold. Naming them keeps a
    # lopsided result readable: "A on 19/20, B on 0/20, 1 inside" is not a
    # claim by the rule, but it is plainly not noise either, and a reader who
    # sees only the word DIRECTIONAL would miss that.
    inside = steps - a_ahead - b_ahead
    tail = f", {inside} inside the threshold" if inside else ""
    print(
        f"DIRECTIONAL [{unit}]: A ahead on {a_ahead}/{steps} steps and B on "
        f"{b_ahead}/{steps}{tail}; not a claim at the "
        f"{SCREEN_CLAIM_PCT:.0f}% threshold — the effect is inside the "
        "noise or the host is noisy today"
    )


def resolve_paths(args: list) -> tuple:
    """The (current, baseline) files to compare, from argv or by convention.

    Only published files take part in the convention: a `dev` or screen output
    is scratch, and it must never be picked as the newest run or as a baseline.
    The ordering is `lib.release_files` (semantic version), because the lexical
    one answers this wrongly from v0.10.0 on — `results-soak-v0.10.0.json` sorts
    before `results-soak-v0.9.0.json`, so the gate would compare the wrong pair.
    """
    here = Path(__file__).parent
    if args:
        cur = Path(args[0])
    else:
        cur = lib.newest_results(here)
        if cur is None:
            sys.exit("no results-soak-*.json found")
    if len(args) > 1:
        return cur, Path(args[1])
    cur_version = lib.release_version(cur)
    older = [
        p
        for p in lib.release_files(here)
        if cur_version is not None and lib.release_version(p) < cur_version
    ]
    return cur, (older[-1] if older else None)


def load(path: Path) -> dict:
    """Read one results file, or exit with its path and the parse error.

    Every other user error in this gate exits with a message; a bare
    traceback here would be the one failure an operator is most likely to
    hit (a truncated run, a wrong path).
    """
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        sys.exit(f"cannot read {path}: {e}")


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] == "--screen":
        if len(args) < SCREEN_ARGS:
            sys.exit("usage: soak_check.py --screen <results.json>")
        sys.exit(screen(load(Path(args[1]))))
    cur, base = resolve_paths(args)
    print(f"current : {cur.name}")
    if base is not None:
        print(f"baseline: {base.name}")
    sys.exit(gate(load(cur), load(base) if base else None))


if __name__ == "__main__":
    main()
