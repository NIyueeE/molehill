#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Focused single-arm experiment: raw iperf3 + molehill tunnel, shaped link.

Purpose: produce the ground truth behind a surprising metric. The matrix
harness records a failure's reason string in `partial_metrics`, which is
often not enough to diagnose it, so this script runs ONE arm in a specific
cell, keeps every raw artifact (iperf3 JSON per run, molehill server and
client logs, the applied qdisc) and prints a compact verdict. The matrix now
also keeps per-rep iperf3 JSON under its work directory (`iperf-raw/`), so
this script is for cases that need a different cell/variant/queue depth.

It reuses the matrix harness itself — `bench.py`'s config generator, port
map, arm spawner and `bench_lib.Backends` — so a discrepancy found here is a
property of the real arm, not of a paraphrased setup.

Usage:

    uv run benches/scripts/bench/focused_run.py \
        --variant kcp4 --cell r100/20 --streams 1,8 --secs 10

Artifacts land in `<work>/focused-<variant>-<cell>/`.
"""
import argparse
import json
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(BENCH_DIR))

import bench as matrix  # noqa: E402  (sets matrix._KNOBS via main() normally)
from bench_lib import (  # noqa: E402
    Backends,
    CellSpec,
    Knobs,
    Netem,
    parse_cell,
    wait_port,
)
from hol_probe import run_hol_probe  # noqa: E402


def free_band(base: int, width: int = 200) -> int:
    """First port band whose ports all bind (the matrix uses fixed bands)."""
    for b in range(base, min(base + 4000, 25900), width):
        socks = []
        try:
            for off in (1, 2, 3, 4, 6, 90, 91, 92):
                s = socket.socket()
                s.bind(("127.0.0.1", b + off))
                socks.append(s)
            return b
        except OSError:
            continue
        finally:
            for s in socks:
                s.close()
    raise SystemExit("no free port band")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--variant", default="kcp4",
                    help="molehill arm: mux, noise, mux1, kcp4, mux-off")
    ap.add_argument("--cell", default="r100/20")
    ap.add_argument("--streams", default="1,8")
    ap.add_argument("--secs", type=int, default=10)
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--base", type=int, default=25200,
                    help="port band start (scanned upward; must sit inside the server's allow_ports range 25100-25999)")
    ap.add_argument("--work", default="/tmp/focused")
    ap.add_argument("--limit", type=int, default=1,
                    help="netem queue limit (bench_lib.Netem uses 1000 for a "
                         "plain rate cell; swept here)")
    ap.add_argument("--keep-qdisc", action="store_true")
    ap.add_argument("--hol-udp", type=float, default=0.0,
                    help="after the arm is up, run the harness's HoL UDP probe "
                         "for N seconds (bulk + paced pinger) and report the "
                         "pinger's loss — the UDP-under-load discriminator")
    ap.add_argument("--bulk-mbps", type=float, default=None,
                    help="override the HoL bulk rate (default: knobs value)")
    args = ap.parse_args()

    knobs = Knobs.from_env()
    matrix._KNOBS["bin"] = knobs.molehill_bin
    if not Path(knobs.molehill_bin).is_file():
        raise SystemExit(f"molehill binary missing: {knobs.molehill_bin} "
                         "(build it first: just build / cargo build "
                         "--release --features kcp)")

    spec: CellSpec = parse_cell(args.cell)
    base = free_band(args.base)
    p = matrix.cell_port_map(base, 0, "netem")
    work = Path(args.work) / f"focused-{args.variant}-{spec.name}"
    work.mkdir(parents=True, exist_ok=True)
    report = {"variant": args.variant, "cell": spec.name, "ports": base,
              "secs": args.secs, "limit": args.limit, "runs": []}

    netem = Netem()
    if spec.weak and not netem.ok:
        raise SystemExit("netem unavailable; focused runs need CAP_NET_ADMIN")
    # A qdisc is applied ONLY for a shaped cell. Applying a parameterless
    # `netem` to an unshaped cell (loopback) puts a 1000-packet queue in the
    # path and throttles it — that artefact once made this helper report
    # ~2.7-4.7 Gbit/s where the matrix measures ~50. `netem.off()` first
    # removes anything a previous run left behind.
    netem.off()
    subprocess.run(["sudo", netem.tc, "qdisc", "del", "dev", "lo", "root"],
                   capture_output=True, check=False)
    if spec.weak:
        # bench_lib.Netem.on hardcodes its own queue depth; the sweep needs an
        # explicit one, so the qdisc is applied here from the same grammar.
        qdisc = ["sudo", netem.tc, "qdisc", "replace", "dev", "lo", "root",
                 "netem"]
        if spec.loss:
            qdisc += ["loss", f"{spec.loss:g}%"]
        if spec.rtt or spec.jitter:
            qdisc += ["delay", f"{spec.rtt:g}ms", f"{spec.jitter:g}ms"]
        if spec.rate:
            qdisc += ["rate", f"{spec.rate:g}mbit", "limit", str(args.limit)]
        if subprocess.run(qdisc, capture_output=True,
                          check=False).returncode != 0:
            raise SystemExit(f"tc failed: {' '.join(qdisc)}")
        print(f"    shaped lo: {' '.join(qdisc[4:])}", flush=True)

    backends = Backends()
    procs = None
    try:
        backends.start(p["iperf_backend"], p["echo_backend"],
                       p["udp_backend"], work)
        procs = matrix.ArmProcs(work, f"{args.variant} {spec.name}")
        matrix.setup_molehill(args.variant, knobs, p, procs, work)
        if not wait_port(p["iperf_exposed"], 25):
            raise SystemExit("iperf exposed port never came up")
        time.sleep(0.7)
        if args.hol_udp:
            rate = args.bulk_mbps if args.bulk_mbps is not None \
                else knobs.hol_bulk_rate_udp
            hol = run_hol_probe("udp", "127.0.0.1", p["udp_exposed"],
                                args.hol_udp, bulk_rate_mbps=rate)
            report["hol_udp"] = hol
            print(f"    hol_udp({args.hol_udp:g}s, bulk {rate} Mbit/s) -> "
                  f"{json.dumps(hol)}", flush=True)

        for streams in (int(x) for x in args.streams.split(",")):
            for rep in range(args.reps):
                raw = work / f"iperf-{streams}s-rep{rep}.json"
                err = work / f"iperf-{streams}s-rep{rep}.err"
                t0 = time.perf_counter()
                cmd = ["iperf3", "-J", "-c", "127.0.0.1",
                       "-p", str(p["iperf_exposed"]), "-t", str(args.secs),
                       "-O", "2", "-P", str(streams)]
                with open(raw, "w") as fo, open(err, "w") as fe:
                    res = subprocess.run(cmd, stdout=fo, stderr=fe,
                                         timeout=args.secs * 2 + 30,
                                         check=False)
                wall = time.perf_counter() - t0
                run = {"streams": streams, "rep": rep, "exit": res.returncode,
                       "wall_s": round(wall, 2), "cmd": " ".join(cmd),
                       "raw_json": str(raw)}
                try:
                    d = json.loads(raw.read_text())
                    end = d.get("end", {})
                    sent = end.get("sum_sent", {})
                    recv = end.get("sum_received", {})
                    per = [(s.get("bits_per_second", 0),
                            s.get("bytes", 0)) for s in end.get("streams", [])]
                    run.update({
                        "gbps_sent": round(sent.get("bits_per_second", 0)
                                           / 1e9, 4),
                        "gbps_received": round(recv.get("bits_per_second", 0)
                                               / 1e9, 4),
                        "bytes_sent": sent.get("bytes", 0),
                        "bytes_received": recv.get("bytes", 0),
                        "retransmits": sent.get("retransmits", 0),
                        "streams_per_stream_bytes": [b for _, b in per],
                        "sum_seconds_sent": sent.get("seconds"),
                        "sum_seconds_received": recv.get("seconds"),
                    })
                    if "error" in d:
                        run["iperf_error"] = d["error"]
                except Exception as e:
                    run["parse_error"] = f"{type(e).__name__}: {e}"
                    run["stderr"] = err.read_text()[:300]
                report["runs"].append(run)
                print(f"[{args.variant} {spec.name}] -P{streams} rep{rep}: "
                      f"{json.dumps({k: v for k, v in run.items() if k not in ('cmd', 'raw_json')})}",
                      flush=True)
    except subprocess.TimeoutExpired:
        report["runs"].append({"error": f"-P{streams} harness timeout"})
        print(f"[{args.variant} {spec.name}] -P{streams}: HARNESS TIMEOUT")
    finally:
        if procs is not None:
            procs.kill()
        backends.stop()
        if not args.keep_qdisc:
            netem.off()
            subprocess.run(["sudo", netem.tc, "qdisc", "del", "dev", "lo",
                            "root"], capture_output=True, check=False)

    out = work / "report.json"
    out.write_text(json.dumps(report, indent=1))
    print(f"\nreport: {out}")
    print(f"logs:   {work}/*.log")
    # The qdisc state is part of the evidence: a surprise here invalidates it.
    q = subprocess.run(["tc", "qdisc", "show", "dev", "lo"],
                       capture_output=True, text=True, check=False)
    print(f"qdisc after: {q.stdout.strip()}")
    if not shutil.which("iperf3"):
        print("WARNING: iperf3 disappeared mid-run")
    return 0


if __name__ == "__main__":
    sys.exit(main())
