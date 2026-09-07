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
                        interval_ms: int) -> dict:
    """Ping-pong over one established TCP connection; returns RTT percentiles."""
    interval = interval_ms / 1000.0
    s = socket.socket()
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.connect((host, port))

    rtts = []
    for _ in range(count):
        t0 = time.perf_counter()
        s.sendall(b"p")
        while s.recv(1) != b"p":
            pass
        rtts.append((time.perf_counter() - t0) * 1000.0)
        time.sleep(interval)

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
