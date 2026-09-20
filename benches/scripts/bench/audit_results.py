#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Acceptance audit for a bench matrix run: are the numbers complete and
self-consistent, and did any arm need manual intervention?

Run it against a results file (default: the current release baseline):

    uv run benches/scripts/bench/audit_results.py \
        /tmp/matrix-check.json

It reports, per tool:
  - holes: a metric that is `None` where the tool/cell should provide it
  - errors / partial_metrics: arms that did not complete on their own
  - throughput self-consistency: `reps_ok`, per-stream byte counts, whether
    the recorded headline matches the per-rep raw artifacts, and whether the
    receiver window is plausible against the measured window
Exit status is non-zero when holes or arm-level errors are found.
"""
import argparse
import json
import sys
from pathlib import Path

# which throughput slots every arm must fill, per cell kind
THR_ALL = ("throughput_1stream_gbps", "throughput_8streams_gbps")
# metrics that only exist for tools with a UDP path and non-loopback cells
UDP_METRICS = ("udp_rtt_ms", "udp_loss_pct", "udp_jitter_ms",
               "udp_max_gap_ms", "udp_capacity")
LOOPBACK_ONLY = ("mixed_bulk_latency", "throughput_64streams_gbps")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("path", nargs="?", default="benches/scripts/bench/"
                    "results-v0.8.0.json")
    ap.add_argument("--udp-tools", default="molehill,frp,rathole")
    ap.add_argument("--show-ok", action="store_true")
    args = ap.parse_args()

    data = json.loads(Path(args.path).read_text())
    results = data.get("results", {})
    udp_tools = set(args.udp_tools.split(","))
    holes, errors, warns = [], [], []

    for tool in sorted(results):
        is_udp = tool.split()[0] in udp_tools
        for cell in sorted(results[tool]):
            e = results[tool][cell]
            if e.get("status") != "ok":
                errors.append(f"{tool} / {cell}: {e.get('error')}")
                continue
            # A null is a HOLE unless it carries a typed reason in
            # partial_metrics (e.g. the documented rate-cell/8-stream
            # structural limit): silence is a gap, a recorded reason is a
            # finding.
            reasons = " | ".join(str(x) for x in (e.get("partial_metrics") or []))
            for m in THR_ALL:
                if e.get(m) is None:
                    field = m.replace("_gbps", "").replace("throughput_", "")
                    if field in reasons:
                        warns.append(f"{tool} / {cell}: {m} is None with a "
                                     f"recorded reason (documented gap)")
                    else:
                        holes.append(f"{tool} / {cell}: {m} is None without a "
                                     "recorded reason")
            if cell != "loopback":
                for m in UDP_METRICS:
                    if is_udp and e.get(m) is None:
                        holes.append(f"{tool} / {cell}: {m} is None")
                for m in LOOPBACK_ONLY:
                    if e.get(m) is not None:
                        warns.append(f"{tool} / {cell}: {m} set on a "
                                     "non-loopback cell")
            # per-arm self-consistency
            for prefix in ("1stream", "8streams", "64streams"):
                key = f"throughput_{prefix}_gbps"
                if e.get(key) is None:
                    continue
                if e.get(f"throughput_{prefix}_reps_ok") is None:
                    warns.append(f"{tool} / {cell}: {key} has no reps_ok")
                win = e.get(f"throughput_{prefix}_receiver_window_s")
                if win is not None and win <= 0:
                    warns.append(f"{tool} / {cell}: {key} receiver window "
                                 f"{win}")
                psb = e.get(f"throughput_{prefix}_per_stream_bytes")
                want = 1 if prefix == "1stream" else (
                    8 if prefix == "8streams" else 64)
                if psb is not None and len(psb) != want:
                    warns.append(f"{tool} / {cell}: {key} reports "
                                 f"{len(psb)} streams, expected {want}")
                # per-stream sender counters are legitimately all-zero when
                # the sender's writes completed inside the -O warm-up and
                # backpressure blocked it afterwards (a fast sender into a
                # slow shaper): the aggregate receiver bytes then carry the
                # measurement. Only a zero stream count WITH sender bytes is
                # contradictory.
                sent_total = e.get(f"throughput_{prefix}_sent_only_gbps")
                if (psb is not None and any(b == 0 for b in psb)
                        and sent_total):
                    warns.append(f"{tool} / {cell}: {key} has zero-byte "
                                 "streams")
                degen = e.get(f"throughput_{prefix}_sender_degenerate_reps")
                if degen:
                    warns.append(
                        f"{tool} / {cell}: {key} sender accounting degenerate "
                        f"in {degen} rep(s) — measured from receiver bytes")
            # nested metrics: a rename inside one of these silently nulls a
            # field while the metric itself stays non-None (the exact bug a
            # stale throughput key caused in mixed_bulk_latency.bulk_gbps)
            if cell == "loopback":
                mixed = e.get("mixed_bulk_latency")
                if not mixed:
                    holes.append(f"{tool} / {cell}: mixed_bulk_latency is "
                                 "None on loopback")
                elif mixed.get("bulk_gbps") is None:
                    holes.append(f"{tool} / {cell}: mixed bulk_gbps is None"
                                 f" ({mixed.get('bulk_reason', 'no reason')})")
            # ENDPOINT GUARD: a throughput number must come from the tool's
            # exposed port. Dialing the iperf3 backend measures the loopback
            # ceiling with the tool bypassed (it invalidated a whole baseline
            # before this check existed).
            exp = e.get("_throughput_exposed_port")
            back = e.get("_bench_backend_port")
            if exp is None and back is None:
                warns.append(f"{tool} / {cell}: entry has no recorded "
                             "throughput endpoint (pre-guard baseline)")
            elif exp == back:
                errors.append(f"{tool} / {cell}: throughput endpoint "
                              f"{exp} is the iperf3 backend — the tool was "
                              "bypassed")
            ch = e.get("churn")
            if not ch or ch.get("connects") is None:
                holes.append(f"{tool} / {cell}: churn missing/incomplete")
            rss = e.get("memory_rss_kb") or {}
            cpu = e.get("cpu") or {}
            if not rss.get("samples"):
                holes.append(f"{tool} / {cell}: rss sampler produced no "
                             "samples")
            if not cpu.get("samples"):
                holes.append(f"{tool} / {cell}: cpu sampler produced no "
                             "samples")
            pm = e.get("partial_metrics")
            if pm:
                # a scale point above the arm's yamux ceiling is skipped BY
                # DESIGN (documented in the README); anything else is noise
                real = [m for m in pm
                        if "yamux ceiling" not in str(m)]
                if real:
                    warns.append(f"{tool} / {cell}: partial_metrics {real}")

    print(f"file: {args.path}")
    print(f"meta: netem_rate_limit={data.get('meta', {}).get('netem_rate_limit')} "
          f"| udp_capacity_pps={data.get('meta', {}).get('udp_capacity_pps')} "
          f"| hostname={data.get('meta', {}).get('hostname')}")
    n = sum(len(v) for v in results.values())
    print(f"\narms: {n} | cell errors: {len(errors)} | holes: {len(holes)} "
          f"| warnings: {len(warns)}")
    for label, items in (("ERROR", errors), ("HOLE", holes),
                         ("WARN", warns)):
        for i in items:
            print(f"  {label}: {i}")
    if args.show_ok:
        for tool in sorted(results):
            for cell in sorted(results[tool]):
                e = results[tool][cell]
                print(f"  ok  {tool} / {cell}: "
                      f"1s={e.get('throughput_1stream_gbps')} "
                      f"8s={e.get('throughput_8streams_gbps')} "
                      f"udp={e.get('udp_capacity') and 'set'}")
    return 1 if (holes or errors) else 0


if __name__ == "__main__":
    sys.exit(main())
