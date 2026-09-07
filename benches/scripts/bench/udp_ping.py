#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Steady-state UDP session quality: ping over ONE established UDP session.

Sends `count` datagrams spaced `interval_ms` apart from a single socket (the
"player"), payload carries seq + send timestamp; a UDP echo backend returns
them untouched. This models stateful game traffic on an already-registered
session — the data-path metrics a player feels, as opposed to connection
setup latency.

Metrics:
  - rtt_ms percentiles over received replies
  - loss_pct: (sent - received) / sent — end-to-end datagram loss
  - jitter_ms: mean |rtt[i] - rtt[i-1]| across consecutive replies
  - max_gap_ms: largest stall between consecutive replies (perceived stutter)

Usage: udp_ping.py <host> <port> <count> <interval_ms> <timeout_s>
Output: one JSON object.
"""
import json
import select
import socket
import struct
import sys
import time

PKT = struct.Struct("<Id")  # seq, send timestamp


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(len(xs) * p))], 3)


def _reset_udp_socket(s, host: str, port: int) -> None:
    """Clear a poisoned connected-UDP error state (ECONNREFUSED lingers)."""
    try:
        s.connect((host, port))
    except OSError:
        pass


def run_udp_ping(host: str, port: int, count: int, interval_ms: int,
                 timeout_s: float) -> dict:
    """Steady same-socket UDP ping; returns the session-quality dict."""
    interval = interval_ms / 1000.0
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.connect((host, port))

    sent = received = 0
    rtts, gaps = [], []
    last_recv = None
    start = time.perf_counter()
    next_send = start
    end_by = start + timeout_s

    while received < count and time.perf_counter() < end_by:
        now = time.perf_counter()
        wait = max(0.0, min(next_send - now, 0.002))
        r, _, _ = select.select([s], [], [], wait)
        if r:
            try:
                data = s.recv(65535)
            except OSError:
                # a connected UDP socket surfaces ICMP errors (an early
                # datagram dropped before the tunnel warmed up arrives as
                # ConnectionRefusedError) and stays poisoned until reset
                _reset_udp_socket(s, host, port)
                continue
            if len(data) >= PKT.size:
                seq, t0 = PKT.unpack_from(data)
                now2 = time.perf_counter()
                if seq in range(count):
                    received += 1
                    rtts.append((now2 - t0) * 1000.0)
                    if last_recv is not None:
                        gaps.append((now2 - last_recv) * 1000.0)
                    last_recv = now2
            continue
        now = time.perf_counter()
        if sent < count and now >= next_send:
            try:
                s.send(PKT.pack(sent, now))
            except OSError:
                _reset_udp_socket(s, host, port)
            sent += 1  # a refused send counts as session loss
            next_send = now + interval

    loss_pct = round((sent - received) / sent * 100.0, 2) if sent else 100.0
    return {
        "sent": sent,
        "received": received,
        "loss_pct": loss_pct,
        "rtt_ms": {
            "p50": pct(rtts, 0.50),
            "p95": pct(rtts, 0.95),
            "p99": pct(rtts, 0.99),
            "mean": round(sum(rtts) / len(rtts), 3) if rtts else 0.0,
        },
        "jitter_ms": round(sum(abs(b - a) for a, b in zip(rtts, rtts[1:]))
                           / max(len(rtts) - 1, 1), 3) if len(rtts) > 1 else 0.0,
        "max_gap_ms": round(max(gaps), 3) if gaps else 0.0,
    }


if __name__ == "__main__":
    host, port = sys.argv[1], int(sys.argv[2])
    count, interval = int(sys.argv[3]), int(sys.argv[4])
    timeout = float(sys.argv[5])
    print(json.dumps(run_udp_ping(host, port, count, interval, timeout)))
