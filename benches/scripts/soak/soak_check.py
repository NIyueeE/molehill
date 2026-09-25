#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Soak gate: decide whether a change moved what it claimed to move.

Two verdicts, one rule each — never a single averaged number:

1. `soak_check.py [current.json [baseline.json]]` — the release gate.
   Compares the current run against the previous release's run, per tool,
   per test type, with per-type thresholds. A violation exits non-zero.
2. `soak_check.py --screen <screen.json>` — the development A/B verdict:
   per-step medians, the effect size and a CLAIM / directional / no-claim
   decision for the two builds a `--test=screen` run interleaved.

The screen's decision is deliberately conservative: with the per-step
aggregates the runner records, a CLAIM needs every step to agree in sign
and to exceed the documented threshold; anything else is reported as
directional with its effect size, which is what "should I pursue this
direction?" actually needs.
"""
import json
import os
import sys
from pathlib import Path

# Per-type gate thresholds (percent unless noted). They are method
# constants — recorded in the results meta, overridable for experiments.
THRESHOLDS = {
    "capacity_streams_pct": 10.0,   # sustainable-load drop
    "capacity_rtt_p99_pct": 25.0,   # response-time curve at matched load
    "rrul_rtt_p99_pct": 25.0,       # per-stage interactive p99
    "rrul_worst_1s_pct": 30.0,      # per-stage worst second
    "cost_cpu_per_gbit_pct": 15.0,  # the fixed operating point
    "drift_fds_per_min": 1.0,       # soak leak axis (absolute)
    "drift_rss_mb_per_min": 50.0,   # soak leak axis (absolute)
}
# The screen's claim threshold: same direction on every step AND this much.
SCREEN_CLAIM_PCT = 15.0


def env_pct(name: str) -> float:
    try:
        return float(os.environ.get(name, THRESHOLDS[name]))
    except ValueError:
        return THRESHOLDS[name]


def pct_change(base, cur) -> float | None:
    if base is None or cur is None or base == 0:
        return None
    return (cur - base) / base * 100.0


def gate(cur: dict, base: dict) -> int:
    cur_tools = {t["tool"]: t for t in cur["tests"] if not t.get("error")}
    base_tools = {t["tool"]: t for t in base["tests"] if not t.get("error")}
    violations = 0
    print(f"current : {cur['meta'].get('date')} "
          f"({', '.join(cur_tools) or 'no tools'})")
    print(f"baseline: {base['meta'].get('date')} "
          f"({', '.join(base_tools) or 'no tools'})")
    for tool, c in sorted(cur_tools.items()):
        b = base_tools.get(tool)
        if b is None:
            print(f"  NOTE  {tool}: not in the baseline (new tool?)")
            continue
        cm, bm = c["metrics"], b["metrics"]
        # --- capacity: the sustainable load and its curve ------------------
        if "max_sustainable_streams" in cm and "max_sustainable_streams" in bm:
            d = pct_change(bm["max_sustainable_streams"],
                           cm["max_sustainable_streams"])
            lim = env_pct("capacity_streams_pct")
            ok = d is None or d >= -lim
            print(f"  {'ok  ' if ok else 'FAIL'}  {tool} capacity: "
                  f"{bm['max_sustainable_streams']} -> "
                  f"{cm['max_sustainable_streams']} streams "
                  f"({d:+.1f}%, limit -{lim:.0f}%)" if d is not None else
                  f"  ok    {tool} capacity: incomplete")
            violations += 0 if ok else 1
            bcurve = {p["streams"]: p for p in bm.get("curve", [])}
            for p in cm.get("curve", []):
                bp = bcurve.get(p["streams"])
                if not bp or bp.get("rtt_p99") is None or p.get("rtt_p99") is None:
                    continue
                d = pct_change(bp["rtt_p99"], p["rtt_p99"])
                lim = env_pct("capacity_rtt_p99_pct")
                ok = d is None or d <= lim
                print(f"  {'ok  ' if ok else 'FAIL'}  {tool} capacity "
                      f"p99 @ {p['streams']} streams: "
                      f"{bp['rtt_p99']} -> {p['rtt_p99']} ms "
                      f"({d:+.1f}%, limit +{lim:.0f}%)")
                violations += 0 if ok else 1
        # --- rrul / soak: the per-stage interactive distribution ------------
        for stage_c, stage_b in zip(c.get("stages", []), b.get("stages", [])):
            if stage_c.get("stage") != stage_b.get("stage"):
                continue
            for key, lim_name in (("rtt_p99", "rrul_rtt_p99_pct"),):
                d = pct_change(stage_b.get(key), stage_c.get(key))
                lim = env_pct(lim_name)
                ok = d is None or d <= lim
                print(f"  {'ok  ' if ok else 'FAIL'}  {tool} {stage_c['stage']} "
                      f"{key}: {stage_b.get(key)} -> {stage_c.get(key)} ms "
                      f"({d:+.1f}%, limit +{lim:.0f}%)" if d is not None else
                      f"  ok    {tool} {stage_c['stage']} {key}: incomplete")
                violations += 0 if ok else 1
            # a stage that wedges only in the current run is a regression
            # even when the numbers that did land look fine
            if stage_c.get("flat_segments") and not stage_b.get("flat_segments"):
                print(f"  FAIL  {tool} {stage_c['stage']}: wedge appeared "
                      f"({len(stage_c['flat_segments'])} flat segment(s))")
                violations += 1
        # --- interactive error rate -----------------------------------------
        d = pct_change(bm.get("interactive_error_rate") or 0,
                       cm.get("interactive_error_rate") or 0)
        if d is not None and d > 5.0:
            print(f"  FAIL  {tool} interactive error rate: "
                  f"{bm.get('interactive_error_rate')} -> "
                  f"{cm.get('interactive_error_rate')} ({d:+.1f}pp)")
            violations += 1
        # --- cost ------------------------------------------------------------
        cg = cm.get("cost_cpu_per_gbit")
        bg = bm.get("cost_cpu_per_gbit")
        if cg is not None and bg is not None:
            d = pct_change(bg, cg)
            lim = env_pct("cost_cpu_per_gbit_pct")
            ok = d is None or d <= lim
            print(f"  {'ok  ' if ok else 'FAIL'}  {tool} cost: "
                  f"{bg} -> {cg} CPU-s/Gbit ({d:+.1f}%, limit +{lim:.0f}%)")
            violations += 0 if ok else 1
        # --- drift -----------------------------------------------------------
        for metric, lim, unit in (
                ("server_fds_slope_per_min", env_pct("drift_fds_per_min"),
                 "fds/min"),
                ("server_rss_kb_slope_per_min",
                 env_pct("drift_rss_mb_per_min") * 1024, "KiB/min")):
            v = cm.get(metric)
            if v is None:
                continue
            ok = abs(v) <= lim
            print(f"  {'ok  ' if ok else 'FAIL'}  {tool} drift "
                  f"{metric.replace('_slope_per_min', '')}: "
                  f"{v:+} {unit} (limit ±{lim:.0f})")
            violations += 0 if ok else 1
    print()
    if violations:
        print(f"FAIL: {violations} violation(s) — fix, or waive explicitly "
              "(HANDOFF.md)")
        return 1
    print("OK: no gate violation against the baseline")
    return 0


def screen(data: dict) -> int:
    """The A/B verdict for a `--test=screen` run."""
    t = next((t for t in data["tests"] if t["test"] == "screen"), None)
    if t is None:
        sys.exit("no screen test in that results file")
    rounds = t["metrics"].get("rounds") or []
    builds = t["metrics"].get("builds") or {}
    print(f"screen: A={builds.get('A_version')} B={builds.get('B_version')}")
    if not rounds:
        sys.exit("no rounds recorded")
    key = "gbps" if rounds[0]["pair"][0].get("gbps") is not None else "rtt_p99"
    claims = 0
    print(f"\n{'streams':>8}{'A':>10}{'B':>10}{'delta':>9}   reading")
    for r in rounds:
        a = next(p for p in r["pair"] if p["build"] == "A")[key]
        b = next(p for p in r["pair"] if p["build"] == "B")[key]
        if a is None or b is None or b == 0:
            continue
        d = (a - b) / b * 100.0
        sign = "A" if d > 0 else "B"
        strong = abs(d) >= SCREEN_CLAIM_PCT
        claims += 1 if (strong and d > 0) else 0
        print(f"{r['streams']:>8}{a:>10.3f}{b:>10.3f}{d:>+8.1f}%   "
              f"{'claim ' + sign if strong else 'directional ' + sign}")
    print()
    steps = len(rounds)
    if claims == steps and steps >= 2:
        print(f"CLAIM: A ahead on every step by >= {SCREEN_CLAIM_PCT:.0f}% "
              f"({steps} steps) — pursue the direction")
        return 0
    print(f"DIRECTIONAL: A ahead on {claims}/{steps} steps; not a claim at "
          f"the {SCREEN_CLAIM_PCT:.0f}% threshold — the effect is inside "
          "the noise or the host is noisy today")
    return 0


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] == "--screen":
        if len(args) < 2:
            sys.exit("usage: soak_check.py --screen <results.json>")
        data = json.loads(Path(args[1]).read_text())
        sys.exit(screen(data))
    here = Path(__file__).parent
    found = sorted(here.glob("results-soak-*.json"), key=lambda p: p.name)
    cur = Path(args[0]) if args else (found[-1] if found else None)
    if cur is None:
        sys.exit("no results-soak-*.json found")
    base = Path(args[1]) if len(args) > 1 else None
    if base is None:
        older = [p for p in here.glob("results-soak-*.json")
                 if p.name < cur.name]
        if not older:
            print(f"current : {cur.name}")
            print("no baseline: the first soak run is gated by the absolute "
                  "SLO only (see docs/release.md)")
            sys.exit(0)
        base = older[-1]
    print(f"current : {cur.name}\nbaseline: {base.name}")
    data = json.loads(cur.read_text())
    base_data = json.loads(base.read_text())
    sys.exit(gate(data, base_data))


if __name__ == "__main__":
    main()
