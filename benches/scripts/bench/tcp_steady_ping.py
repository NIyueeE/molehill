#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Data-path RTT for a TCP service: ping-pong over ONE established connection.

The matrix's `echo_rtt_ms` opens a fresh connection per sample (connection-path
latency: pool handoff + connect). This helper measures the other half of the
story — the RTT a long-lived connection sees once established (what an SSH
session or a WebSocket feels like).

Usage: tcp_steady_ping.py <host> <port> <count> <interval_ms>
Output: one JSON object.
"""
import json
import socket
import sys
import time


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(len(xs) * p))], 3)


def run_tcp_steady_ping(host: str, port: int, count: int,
                        interval_ms: int, max_wall_s: float = 20.0) -> dict:
    """Ping-pong over one established TCP connection; returns RTT percentiles.
    Wall-bounded like the latency probe: on a 100 ms cell each ping costs
    ~1 s, and ~20 samples on a fixed-delay path carry the same p50/p99.

    A stalled connection (recv timeout) or a closed one (EOF) counts as a
    stall and the probe reconnects — under burst loss a wedged session must
    not nuke the whole metric, and reconnecting is what a real long-lived
    client does anyway."""
    interval = interval_ms / 1000.0
    rtts = []
    deadline = time.time() + max_wall_s

    def new_conn():
        s = socket.socket()
        s.settimeout(3.0)  # a wedged tunnel must fail the probe, not hang
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.connect((host, port))
        return s

    s = new_conn()
    try:
        for _ in range(count):
            if time.time() > deadline:
                break
            t0 = time.perf_counter()
            try:
                s.sendall(b"p")
                # exactly one reply byte per ping on this connection; EOF
                # means the peer closed it — reconnect instead of spinning
                if s.recv(1) != b"p":
                    s.close()
                    s = new_conn()
                    continue
                rtts.append((time.perf_counter() - t0) * 1000.0)
            except (TimeoutError, OSError):
                s.close()
                s = new_conn()
            time.sleep(interval)
    finally:
        s.close()
    if len(rtts) < 10:
        raise RuntimeError("steady ping: too few samples")

    return {
        "p50": pct(rtts, 0.50),
        "p95": pct(rtts, 0.95),
        "p99": pct(rtts, 0.99),
        "mean": round(sum(rtts) / len(rtts), 3),
    }


if __name__ == "__main__":
    host, port = sys.argv[1], int(sys.argv[2])
    count, interval = int(sys.argv[3]), int(sys.argv[4])
    print(json.dumps(run_tcp_steady_ping(host, port, count, interval)))
