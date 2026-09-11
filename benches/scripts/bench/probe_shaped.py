#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Shaped-link probe: is the rate-cell 8-stream stall a real property of the
tool under test, or an artifact of the qdisc/harness?

Motivation (2026-09-10): at `rate100_rtt20` / `rate20_rtt40` every
8-parallel-stream iperf3 test returned zero throughput or timed out, and the
harness recorded `null`. Before "fixing" the harness, separate three
candidate causes:

1. the netem rate queue is too shallow for a GSO/TSO-shaped loopback: an
   enqueued super-segment larger than the queue tail-drops the whole burst,
   so even a well-behaved TCP sender stalls (`limit 1` is what the harness
   uses today);
2. iperf3's single-process `-P 8` coupling (one control connection, one
   results exchange, a shared omit window) is what wedges, not the path;
3. the path really cannot carry 8 concurrent streams (a genuine finding).

The probe answers this WITHOUT the tunnel: a plain iperf3 server and client
across a netem-shaped `lo`. A stall that reproduces here is tool-independent
— hence a harness/qdisc artifact; only a stall that survives on a working
qdisc is attributable to the tool under test.

**Verdict of the first run (kept for the record):** `-P 8` never stalls on
the raw path; the depth of the netem rate queue is what collapses it
(`limit 1` -> 18 Mbit/s, `limit 1000` -> 99.6 Mbit/s at a 100 Mbit/s rate).
The harness now uses a `limit 2000` queue (`bench_lib.RATE_QUEUE_LIMIT`).

Usage (needs CAP_NET_ADMIN for netem; `sudo -n`):

    uv run benches/scripts/bench/probe_shaped.py \
        --rate 100 --rtt 20 --limits 1,1000,20000 --secs 6

Prints a table and writes the raw JSON under `<work>/probe-shape.json`.
"""
import argparse
import contextlib
import json
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

TIMEOUT_SLACK = 1.6  # kill a shaped test well before an hour-long stall


def tc(args: list) -> int:
    return subprocess.run(["sudo", "tc", *args], capture_output=True,
                          check=False, timeout=10).returncode


def netem_on(rate: float, rtt: float, limit: int) -> None:
    """Shaped loopback exactly like bench_lib.Netem.on, but with an explicit
    queue limit so the probe can sweep it."""
    cmd = ["qdisc", "replace", "dev", "lo", "root", "netem"]
    if rtt:
        cmd += ["delay", f"{rtt:g}ms"]
    cmd += ["rate", f"{rate:g}mbit", "limit", str(limit)]
    if tc(cmd) != 0:
        raise SystemExit(f"tc failed: {' '.join(cmd)}")


def netem_off() -> None:
    tc(["qdisc", "del", "dev", "lo", "root"])


class IperfServer:
    """Persistent single-test iperf3 server with a health check.

    A stalled test can wedge the server's state ("unable to receive cookie" /
    EBADF) so every later test hangs — this wrapper therefore verifies the
    server with a 1 s probe before each measurement and replaces the process
    when the probe fails. That restart is what the harness needs; the probe
    measures whether it is *sufficient*.
    """

    def __init__(self, port: int, work: Path, tag: str):
        self.port = port
        self.work = work
        self.tag = tag
        self.log = work / f"iperf3-server-{tag}.log"
        self.proc = None
        self.restarts = 0

    def start(self) -> None:
        self.stop()
        with open(self.log, "ab") as f:
            self.proc = subprocess.Popen(
                ["iperf3", "-s", "-B", "127.0.0.1", "-p", str(self.port)],
                stdout=f, stderr=f)
        for _ in range(60):
            if self.proc.poll() is not None:
                raise SystemExit(f"iperf3 server exited on start "
                                 f"({self.proc.returncode}); see {self.log}")
            try:
                with socket.create_connection(("127.0.0.1", self.port),
                                              timeout=0.3):
                    return
            except OSError:
                time.sleep(0.1)
        raise SystemExit(f"iperf3 server did not listen on {self.port}")

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            with contextlib.suppress(OSError):
                self.proc.kill()
            with contextlib.suppress(Exception):
                self.proc.wait(timeout=5)

    def healthy(self) -> bool:
        """Side-effect-free liveness check: does the server complete a 1 s
        single-stream test with a per-stream figure? A wedged server answers
        the control connection but never finishes, which is exactly the state
        that poisons following arms in the harness."""
        try:
            res = subprocess.run(
                ["iperf3", "-J", "-c", "127.0.0.1", "-p", str(self.port),
                 "-t", "1"], capture_output=True, text=True, timeout=12,
                check=False)
        except subprocess.TimeoutExpired:
            return False
        if res.returncode != 0:
            return False
        try:
            # per-stream entries live under end["streams"]; end["sum_sent"]
            # holds no "streams" key at all (iperf 3.18)
            return bool(json.loads(res.stdout)["end"]["streams"])
        except (ValueError, KeyError):
            return False

    def ensure(self) -> str:
        """Return "" when healthy; restart and return a note otherwise."""
        if self.healthy():
            return ""
        self.restarts += 1
        self.start()
        if not self.healthy():
            raise SystemExit(f"iperf3 server on {self.port} unhealthy after "
                             f"{self.restarts} restart(s); see {self.log}")
        return f"server restarted (restart #{self.restarts})"


def client_once(port: int, secs: int, streams: int, timeout: float) -> str:
    """One parallel iperf3 client process running `streams` streams; returns
    its JSON stdout or raises with the reason."""
    try:
        res = subprocess.run(
            ["iperf3", "-J", "-c", "127.0.0.1", "-p", str(port),
             "-t", str(secs), "-O", "2", "-P", str(streams)],
            capture_output=True, text=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"client timeout after {timeout:.0f}s") from None
    if res.returncode != 0:
        detail = (res.stderr or res.stdout or "").strip().replace("\n", " ")
        raise RuntimeError(f"exit {res.returncode}: {detail[:160]}")
    return res.stdout


def measure(port: int, secs: int, streams: int, procs: int) -> dict:
    """`procs` independent iperf3 clients x `streams` each, started together;
    the aggregate is what the tunnel has to carry. procs=1 reproduces the
    current harness call (`-P streams`); procs=N decouples the clients."""
    timeout = secs * TIMEOUT_SLACK + 10
    t0 = time.perf_counter()
    children = [subprocess.Popen(
        ["iperf3", "-J", "-c", "127.0.0.1", "-p", str(port),
         "-t", str(secs), "-O", "2", "-P", str(streams)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        for _ in range(procs)]
    docs, failed = [], []
    for i, ch in enumerate(children):
        try:
            out, err = ch.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            ch.kill()
            out, err = ch.communicate()
            failed.append(f"c{i}: timeout after {timeout:.0f}s")
            continue
        if ch.returncode != 0:
            detail = (err or out or "").strip().replace("\n", " ")[:120]
            failed.append(f"c{i}: exit {ch.returncode}: {detail}")
            continue
        try:
            docs.append(json.loads(out)["end"])
        except (ValueError, KeyError):
            failed.append(f"c{i}: unparseable")
    wall = time.perf_counter() - t0

    sent_bps = recv_bps = byte_sum = retr = 0
    zero = total = 0
    for d in docs:
        sent, recv = d["sum_sent"], d.get("sum_received", {})
        sent_bps += sent.get("bits_per_second", 0)
        recv_bps += recv.get("bits_per_second", 0)
        byte_sum += sent.get("bytes", 0)
        retr += sent.get("retransmits", 0)
        per = [s.get("bits_per_second", 0) for s in d.get("streams", [])]
        zero += sum(1 for v in per if v <= 0.0)
        total += len(per)
    if total == 0:
        return {"ok": False, "reason": "; ".join(failed), "wall_s":
                round(wall, 2)}
    out = {"ok": True, "gbps_sent": round(sent_bps / 1e9, 3),
           "gbps_received": round(recv_bps / 1e9, 3),
           "bytes_sent": byte_sum, "retransmits": retr,
           "streams_zero": zero, "streams_total": total,
           "clients_ok": len(docs), "clients_total": procs,
           "wall_s": round(wall, 2)}
    if failed:
        out["clients_failed"] = failed
    return out


def probe(srv: IperfServer, secs: int, streams: int, procs: int) -> dict:
    """One measurement against a healthy server (restarted when wedged)."""
    note = srv.ensure()
    if note:  # the previous test wedged the server: record, do not hide
        pass
    try:
        r = measure(srv.port, secs, streams, procs)
        r["server_note"] = note
        return r
    except Exception as e:
        return {"ok": False, "reason": f"{type(e).__name__}: {e}",
                "server_note": note}


def free_port(hint: int) -> int:
    """Bind-probe a port above the ephemeral range; a wedged server from a
    previous run must not decide the next run's result."""
    for port in range(hint, hint + 40):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", port))
            return port
        except OSError:
            continue
        finally:
            s.close()
    raise SystemExit(f"no free port in {hint}..{hint + 40}")


CONFIGS = ((1, 1), (8, 1), (1, 8), (8, 8))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--rate", type=float, default=100.0,
                    help="netem rate in mbit/s (0 = no rate limit)")
    ap.add_argument("--rtt", type=float, default=20.0, help="netem delay ms")
    ap.add_argument("--limits", default="1,1000,20000",
                    help="comma list of netem queue limits to sweep")
    ap.add_argument("--secs", type=int, default=6)
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--port", type=int, default=25941)
    ap.add_argument("--work", default="/tmp/probe-shape")
    args = ap.parse_args()

    if shutil.which("iperf3") is None:
        raise SystemExit("iperf3 not on PATH")
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    limits = [int(x) for x in args.limits.split(",") if x.strip()]
    results = {"rate_mbit": args.rate, "rtt_ms": args.rtt, "secs": args.secs,
               "reps": args.reps, "runs": []}

    netem_off()  # a leftover qdisc from a killed run must not shape this one
    try:
        for limit in limits:
            port = free_port(args.port)
            srv = IperfServer(port, work, f"l{limit}")
            srv.start()
            if args.rate:
                netem_on(args.rate, args.rtt, limit)
            try:
                for rep in range(args.reps):
                    for streams, procs in CONFIGS:
                        r = probe(srv, args.secs, streams, procs)
                        r.update(limit=limit, rep=rep,
                                 streams_per_client=streams, clients=procs,
                                 port=port)
                        results["runs"].append(r)
                        shown = {k: v for k, v in r.items()
                                 if k not in ("rep", "port")}
                        print(f"limit={limit:<6} {procs}x-P{streams:<2} -> "
                              f"{json.dumps(shown)}", flush=True)
            finally:
                srv.stop()
                netem_off()
    finally:
        netem_off()
    out = work / "probe-shape.json"
    out.write_text(json.dumps(results, indent=1))
    print(f"\nraw: {out}")

    print("\n=== summary (aggregate Gbit/s received, median rep) ===")
    print(f"{'limit':>6} {'cfg':>7} {'Gbit/s':>8}  notes")
    for limit in limits:
        for streams, procs in CONFIGS:
            runs = [r for r in results["runs"]
                    if r["limit"] == limit and r["streams_per_client"] == streams
                    and r["clients"] == procs]
            if not runs:
                continue
            ok = [r for r in runs if r.get("ok")]
            cfg = f"{procs}x-P{streams}"
            if ok:
                med = sorted(r["gbps_received"] for r in ok)[len(ok) // 2]
                zeros = sum(r.get("streams_zero", 0) for r in ok)
                tots = sum(r["streams_total"] for r in ok)
                note = f"zero-streams={zeros}/{tots}"
                if any(r.get("server_note") for r in runs):
                    note += "; server restarted"
                print(f"{limit:>6} {cfg:>7} {med:>7.3f}G  {note}")
            else:
                reasons = "; ".join(r.get("reason", "?") for r in runs)[:80]
                print(f"{limit:>6} {cfg:>7} {'FAIL':>8}  {reasons}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
