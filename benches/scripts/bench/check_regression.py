# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Regression gate: compare the freshest benchmark results against the
previous tag's results file. Run before tagging — a violation means the
release must not go out until fixed or explicitly waived (HANDOFF.md).

Usage:
  uv run check_regression.py [current.json] [baseline.json]
  current  defaults to the highest-version results-v*.json here
  baseline defaults to the next-lower version file

Only molehill's default (mux) row is gated; v3 metrics are skipped when the
baseline predates them. Thresholds are percent unless noted, env-overridable.
"""
import json
import os
import sys
from pathlib import Path

DIR = Path(__file__).parent

DEFAULTS = {
    "REG_RTT_P50_PCT": 15, "REG_RTT_P99_PCT": 20, "REG_THR_PCT": 5,
    "REG_RSS_PCT": 20, "REG_STEADY_RTT_PCT": 25, "REG_UDP_RTT_PCT": 25,
    "REG_HOL_GAP_PCT": 30, "REG_UDP_LOSS_PP": 1.0,  # absolute pp
}


def env_pct(name: str) -> float:
    try:
        return float(os.environ.get(name, DEFAULTS[name]))
    except ValueError:
        return DEFAULTS[name]


def molehill_key(results: dict):
    cands = [t for t in results if t.startswith("molehill")
             and "mux=off" not in t and "mux-off" not in t]
    # the gated row is the default mux arm — deterministic even though arm
    # completion order can vary
    cands.sort(key=lambda t: (0 if "(mux)" in t else 1, t))
    return cands[0] if cands else None


def pick_files():
    cur = sys.argv[1] if len(sys.argv) > 1 else ""
    base = sys.argv[2] if len(sys.argv) > 2 else ""
    files = sorted(DIR.glob("results-v*.json"), key=lambda p: p.name)
    if not cur:
        cur = str(files[-1]) if files else ""
        if not cur:
            print("no results-v*.json found" , file=sys.stderr)
            sys.exit(1)
    if not base:
        older = [f for f in files
                 if f.name < Path(cur).name]  # lexical = version order here
        base = str(older[-1]) if older else ""
        if not base:
            print(f"no baseline found older than {Path(cur).name}",
                  file=sys.stderr)
            sys.exit(1)
    return cur, base


def main():
    cur_path, base_path = pick_files()
    print(f"current : {Path(cur_path).name}")
    print(f"baseline: {Path(base_path).name}")
    cur = json.load(open(cur_path))
    base = json.load(open(base_path))

    ck, bk = molehill_key(cur["results"]), molehill_key(base["results"])
    if not ck or not bk:
        print("FAIL: no molehill (mux) row in one of the files")
        sys.exit(1)
    print(f"gated row: {ck!r} vs {bk!r}")

    ccur, cbase = cur["results"][ck], base["results"][bk]
    # v1 baselines (pre-matrix) store the molehill row flat: treat as loopback
    if "throughput_1stream_gbps" in cbase:
        cbase = {"loopback": cbase}
        print("NOTE: flat v1 baseline detected; gating the loopback cell only")
    common = sorted(set(ccur) & set(cbase))
    missing = sorted(set(ccur) - set(cbase))
    if missing:
        print(f"NOTE: cells absent from baseline (not gated): {', '.join(missing)}")
    if not common:
        print("FAIL: no comparable cells between current and baseline")
        sys.exit(1)

    hcur = cur["meta"].get("hostname", "?")
    hbase = base["meta"].get("hostname", "?")
    if hcur != hbase:
        print(f"WARNING: different hosts (current={hcur}, baseline={hbase}) "
              "- numbers may not be comparable")

    pct = {k: env_pct(k) for k in DEFAULTS}
    # getters must be None-safe: an arm records `null` for a metric that
    # failed instead of a fake 0 (the gate skips nulls on either side)
    METRICS = [
        ("thr 1-stream", lambda c: c.get("throughput_1stream_gbps"),
         pct["REG_THR_PCT"], -1, False),
        ("thr 8-stream", lambda c: c.get("throughput_8streams_gbps"),
         pct["REG_THR_PCT"], -1, False),
        ("rtt p50", lambda c: (c.get("echo_rtt_ms") or {}).get("p50"),
         pct["REG_RTT_P50_PCT"], +1, False),
        ("rtt p99", lambda c: (c.get("echo_rtt_ms") or {}).get("p99"),
         pct["REG_RTT_P99_PCT"], +1, False),
        ("rss avg", lambda c: (c.get("memory_rss_kb") or {}).get("total_avg_kb"),
         pct["REG_RSS_PCT"], +1, False),
        ("steady rtt p99", lambda c: (c.get("tcp_steady_rtt_ms") or {}).get("p99"),
         pct["REG_STEADY_RTT_PCT"], +1, False),
        ("udp rtt p99", lambda c: (c.get("udp_rtt_ms") or {}).get("p99"),
         pct["REG_UDP_RTT_PCT"], +1, False),
        ("hol max gap", lambda c: (c.get("hol") or {}).get("ping_max_gap_ms"),
         pct["REG_HOL_GAP_PCT"], +1, False),
        ("udp loss pp", lambda c: c.get("udp_loss_pct"),
         pct["REG_UDP_LOSS_PP"], +1, True),
    ]

    violations = 0
    print(f"\n{'cell':<16}{'metric':<16}{'baseline':>12}{'current':>12}"
          f"{'delta':>10}{'limit':>9}  verdict")
    for cell in common:
        bcell, ccell = cbase[cell], ccur[cell]
        for label, get, limit, sign, absolute in METRICS:
            b, c = get(bcell), get(ccell)
            if b is None or c is None:
                continue
            if absolute:
                delta = c - b
                bad = delta > limit
                shown, limit_s = f"{delta:+.2f}pp", f"{limit:.1f}pp"
                base_s, cur_s = f"{b:.2f}", f"{c:.2f}"
            else:
                delta = (c - b) / b * 100.0 if b else 0.0
                bad = delta > limit if sign > 0 else delta < -limit
                shown, limit_s = f"{delta:+.1f}%", f"{limit:.0f}%"
                base_s, cur_s = f"{b:.3f}", f"{c:.3f}"
            verdict = "REGRESSION" if bad else "ok"
            if bad:
                violations += 1
            print(f"{cell:<16}{label:<16}{base_s:>12}{cur_s:>12}"
                  f"{shown:>10}{limit_s:>9}  {verdict}")

    print()
    if violations:
        print(f"FAIL: {violations} regression(s) — fix or waive explicitly "
              "(HANDOFF.md)")
        sys.exit(1)
    print("OK: no regression against the previous tag")


if __name__ == "__main__":
    main()
