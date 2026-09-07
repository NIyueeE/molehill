#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Head-of-line (HoL) probe: saturating bulk flow + game-like pinger running
CONCURRENTLY through the SAME forwarded service (and therefore the same
tunnel). This is the metric that separates single-tunnel TCP (one loss domain,
all flows stall together) from multi-tunnel and stream-isolated transports.

- bulk:   `--bulk-conns` connections blasting 256 KiB chunks as fast as the
          service echoes them back (drained continuously to keep the pressure
          sustained for the whole duration)
- pinger: one small-packet connection/socket doing ping-pong at `--ping-hz`;
          reports RTT percentiles and the largest stall between consecutive
          replies (`ping_max_gap_ms`) — the player-perceived stutter.

Usage:
  hol_probe.py --mode tcp --host H --port P --duration S [--ping-hz 30] [--bulk-conns 2]
  hol_probe.py --mode udp --host H --port P --duration S [...]
Output: one JSON object.
"""
import argparse
import contextlib
import json
import select
import socket
import struct
import threading
import time

CHUNK = 256 * 1024


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(len(xs) * p))], 3)


def bulk_tcp(stop, host, port, stats, idx, rate):
    try:
        s = socket.socket()
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.connect((host, port))
        chunk = b"x" * CHUNK
        chunk_secs = (CHUNK * 8) / (rate * 1e6) if rate > 0 else 0.0
        sent = 0
        while not stop.is_set():
            t0 = time.perf_counter()
            s.sendall(chunk)
            got = 0
            while got < len(chunk) and not stop.is_set():
                got += len(s.recv(min(65536, len(chunk) - got)))
            sent += len(chunk)
            if chunk_secs:
                elapsed = time.perf_counter() - t0
                if elapsed < chunk_secs:
                    time.sleep(chunk_secs - elapsed)
        stats["bytes"] += sent
    except OSError:
        pass
    finally:
        with contextlib.suppress(Exception):
            s.close()


def bulk_udp(stop, host, port, stats, idx, rate):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect((host, port))
        s.setblocking(False)
        pkt = b"b" * 1200
        pkt_secs = (len(pkt) * 8) / (rate * 1e6) if rate > 0 else 0.0
        sent = 0
        next_t = time.perf_counter()
        while not stop.is_set():
            try:
                s.send(pkt)
                sent += len(pkt)
            except BlockingIOError:
                pass
            try:
                while True:
                    s.recv(65535)
            except BlockingIOError:
                pass
            if pkt_secs:
                next_t += pkt_secs
                now = time.perf_counter()
                if next_t > now:
                    time.sleep(next_t - now)
        stats["bytes"] += sent
    except OSError:
        pass
    finally:
        s.close()


def ping_tcp(stop, host, port, hz, stats):
    try:
        s = socket.socket()
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.connect((host, port))
    except OSError:
        return
    interval = 1.0 / hz
    rtts, gaps = [], []
    last = None
    next_t = time.perf_counter()
    while not stop.is_set():
        now = time.perf_counter()
        if now < next_t:
            time.sleep(min(next_t - now, 0.001))
            continue
        t0 = time.perf_counter()
        try:
            s.sendall(b"p")
            while s.recv(1) != b"p":
                pass
        except OSError:
            break
        rtt = (time.perf_counter() - t0) * 1000.0
        rtts.append(rtt)
        if last is not None:
            gaps.append((t0 - last) * 1000.0)
        last = t0
        next_t += interval
    stats["ping_rtt_ms"] = {
        "p50": pct(rtts, 0.50), "p95": pct(rtts, 0.95),
        "p99": pct(rtts, 0.99),
        "mean": round(sum(rtts) / len(rtts), 3) if rtts else 0.0,
    }
    stats["ping_max_gap_ms"] = round(max(gaps), 3) if gaps else 0.0


def ping_udp(stop, host, port, hz, stats):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect((host, port))
        s.setblocking(False)
    except OSError:
        return
    PKT = struct.Struct("<Id")
    interval = 1.0 / hz
    rtts, gaps, sent, received = [], [], 0, 0
    last = None
    next_t = time.perf_counter()
    while not stop.is_set():
        now = time.perf_counter()
        wait = max(0.0, min(next_t - now, 0.001))
        r, _, _ = select.select([s], [], [], wait)
        if r:
            try:
                data = s.recv(65535)
            except OSError:
                # connected UDP sockets surface ICMP errors (e.g. a flow that
                # expired behind a lossy path) — count as lost and go on
                continue
            if len(data) >= PKT.size:
                _, t0 = PKT.unpack_from(data)
                received += 1
                rtts.append((time.perf_counter() - t0) * 1000.0)
                if last is not None:
                    gaps.append((time.perf_counter() - last) * 1000.0)
                last = time.perf_counter()
            continue
        now = time.perf_counter()
        if now >= next_t:
            try:
                s.send(PKT.pack(sent, now))
                sent += 1
            except BlockingIOError:
                pass
            next_t = now + interval
    stats["ping_sent"] = sent
    stats["ping_received"] = received
    stats["ping_rtt_ms"] = {
        "p50": pct(rtts, 0.50), "p95": pct(rtts, 0.95),
        "p99": pct(rtts, 0.99),
        "mean": round(sum(rtts) / len(rtts), 3) if rtts else 0.0,
    }
    stats["ping_max_gap_ms"] = round(max(gaps), 3) if gaps else 0.0
    stats["ping_loss_pct"] = round((sent - received) / sent * 100.0, 2) if sent else 0.0


def run_hol_probe(mode: str, host: str, port: int, duration: float = 8.0,
                  ping_hz: float = 30.0, bulk_conns: int = 2,
                  bulk_rate_mbps: float = 0.0) -> dict:
    """Bulk flow + game-like pinger through the same service; returns the
    HoL dict (bulk_gbps, ping_rtt_ms, ping_max_gap_ms, [ping_loss_pct])."""
    stop = threading.Event()
    stats = {"bytes": 0}
    bulk = bulk_udp if mode == "udp" else bulk_tcp
    ping = ping_udp if mode == "udp" else ping_tcp

    threads = [threading.Thread(target=ping,
                                args=(stop, host, port, ping_hz, stats),
                                daemon=True)]
    for i in range(max(1, bulk_conns)):
        threads.append(threading.Thread(target=bulk,
                                        args=(stop, host, port, stats, i,
                                              bulk_rate_mbps),
                                        daemon=True))
    for t in threads:
        t.start()
    time.sleep(duration)
    stop.set()
    for t in threads:
        t.join(timeout=5)

    out = {
        "mode": mode,
        "bulk_gbps": round(stats["bytes"] * 8 / duration / 1e9, 3),
        "ping_rtt_ms": stats.get("ping_rtt_ms"),
        "ping_max_gap_ms": stats.get("ping_max_gap_ms", 0.0),
    }
    if mode == "udp":
        out["ping_sent"] = stats.get("ping_sent", 0)
        out["ping_received"] = stats.get("ping_received", 0)
        out["ping_loss_pct"] = stats.get("ping_loss_pct", 0.0)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["tcp", "udp"], required=True)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--duration", type=float, default=8.0)
    ap.add_argument("--ping-hz", type=float, default=30.0)
    ap.add_argument("--bulk-conns", type=int, default=2)
    # 0 = saturate (stress mode); a paced rate keeps the pinger alive so the
    # metric stays comparable across arms
    ap.add_argument("--bulk-rate-mbps", type=float, default=0.0)
    a = ap.parse_args()
    print(json.dumps(run_hol_probe(a.mode, a.host, a.port, a.duration,
                                   a.ping_hz, a.bulk_conns, a.bulk_rate_mbps)))


if __name__ == "__main__":
    main()
