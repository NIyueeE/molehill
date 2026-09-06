#!/usr/bin/env bash
# Regression gate: compare the freshest benchmark results against the
# previous tag's results file. Run before tagging — a violation means the
# release must not go out until fixed or explicitly waived (HANDOFF.md).
#
# Usage: check_regression.sh [current.json] [baseline.json]
#   current  defaults to the highest-version results-v*.json here
#   baseline defaults to the next-lower version file
#
# Thresholds (percent, env-overridable): only molehill's default (mux) row is
# gated, and only cells present in BOTH files (older baselines without
# weak-network cells degrade to loopback-only with a warning).
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)

REG_RTT_P50_PCT=${REG_RTT_P50_PCT:-15}
REG_RTT_P99_PCT=${REG_RTT_P99_PCT:-20}
REG_THR_PCT=${REG_THR_PCT:-5}
REG_RSS_PCT=${REG_RSS_PCT:-20}

cur=${1:-}
base=${2:-}
if [ -z "$cur" ]; then
    cur=$(ls "$DIR"/results-v*.json 2>/dev/null | sort -V | tail -1) || true
    [ -n "$cur" ] || { echo "no results-v*.json found" >&2; exit 1; }
fi
if [ -z "$base" ]; then
    base=$(ls "$DIR"/results-v*.json 2>/dev/null | sort -V | grep -B1 -F "$(basename "$cur")" | head -1) || true
    [ -n "$base" ] || { echo "no baseline found older than $(basename "$cur")" >&2; exit 1; }
fi
echo "current : $(basename "$cur")"
echo "baseline: $(basename "$base")"

python3 - "$cur" "$base" <<'PYREG'
import json, os, sys

cur_path, base_path = sys.argv[1], sys.argv[2]
cur = json.load(open(cur_path))
base = json.load(open(base_path))

def molehill_key(results):
    cands = [t for t in results if t.startswith("molehill")
             and "mux=off" not in t]
    return cands[0] if cands else None

def env_pct(name, default):
    try:
        return float(os.environ.get(name, default))
    except ValueError:
        return default

thr_pct = env_pct("REG_THR_PCT", 5)
p50_pct = env_pct("REG_RTT_P50_PCT", 15)
p99_pct = env_pct("REG_RTT_P99_PCT", 20)
rss_pct = env_pct("REG_RSS_PCT", 20)

ck, bk = molehill_key(cur["results"]), molehill_key(base["results"])
if not ck or not bk:
    print("FAIL: no molehill (mux) row in one of the files")
    sys.exit(1)
print(f"gated row: {ck!r} vs {bk!r}")

ccur, cbase = cur["results"][ck], base["results"][bk]
# v1 baselines (pre-matrix, e.g. results-v0.7.0.json) store the molehill row
# flat, without a cell layer; treat it as the loopback cell
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

# metric: (label, getter, threshold_pct, direction)  direction: -1 lower-is-worse
METRICS = [
    ("thr 1-stream", lambda c: c.get("throughput_1stream_gbps"), thr_pct, -1),
    ("thr 8-stream", lambda c: c.get("throughput_8streams_gbps"), thr_pct, -1),
    ("rtt p50", lambda c: c.get("echo_rtt_ms", {}).get("p50"), p50_pct, +1),
    ("rtt p99", lambda c: c.get("echo_rtt_ms", {}).get("p99"), p99_pct, +1),
    ("rss avg", lambda c: c.get("memory_rss_kb", {}).get("total_avg_kb"), rss_pct, +1),
]

violations = 0
print(f"\n{'cell':<14}{'metric':<14}{'baseline':>12}{'current':>12}"
      f"{'delta':>9}{'limit':>8}  verdict")
for cell in common:
    bcell, ccell = cbase[cell], ccur[cell]
    for label, get, pct, sign in METRICS:
        b, c = get(bcell), get(ccell)
        if b is None or c is None or b == 0:
            continue
        delta = (c - b) / b * 100.0
        bad = delta > pct if sign > 0 else delta < -pct
        verdict = "REGRESSION" if bad else "ok"
        if bad:
            violations += 1
        print(f"{cell:<14}{label:<14}{b:>12.3f}{c:>12.3f}"
              f"{delta:>+8.1f}%{pct:>7.0f}%  {verdict}")

print()
if violations:
    print(f"FAIL: {violations} regression(s) — fix or waive explicitly (HANDOFF.md)")
    sys.exit(1)
print("OK: no regression against the previous tag")
PYREG
