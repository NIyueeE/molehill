#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Request/response probe: the round-trip rate and the latency distribution.

Runs inside the visitor's namespace and knows nothing about the tunnel: it dials
whatever address it is given, sends a deterministic payload, and compares what
comes back byte for byte. That comparison is not decoration — it is the
reordering/PMTU canary the acceptance harness uses, and a run whose bytes came
back wrong is a failed sample, not a slow one.

Two shapes:

- **reused** (default): one connection per thread, `--requests` round trips on
  it, one outstanding at a time. This is the shape an interactive protocol has,
  and its rate is a latency measurement in disguise.
- **`--fresh`**: a new connection per request, so the connect time is measured
  separately (`setup_us`) and the rate is a *setup* rate — the number that says
  what a visitor pays to arrive, and how much per-visitor state the architecture
  keeps.

The last line is a JSON object with the raw sample arrays; percentiles are
derived by the engine (`analysis.py`) so every metric in the results has exactly
one definition.
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import threading
import time

#: The failures worth naming. A refused/timed-out connection and a closed
#: connection are different findings — one is the path, the other is the peer.
CONNECT_TIMEOUT = 5.0
#: Bytes of sequence number at the front of every request payload.
SEQ_BYTES = 4


def payload_for(seq: int, size: int) -> bytes:
    """A deterministic payload that varies per request, so a stale reply is a
    mismatch rather than a lucky hit."""
    body = bytes(((seq * 7 + i) & 0xFF) for i in range(max(0, size - SEQ_BYTES)))
    return struct.pack("!I", seq) + body


def read_exactly(sock: socket.socket, size: int) -> bytes:
    """`size` bytes, or fewer if the peer closed: a short read is a failure."""
    chunks: list = []
    left = size
    while left > 0:
        data = sock.recv(left)
        if not data:
            break
        chunks.append(data)
        left -= len(data)
    return b"".join(chunks)


class Worker(threading.Thread):
    def __init__(self, args, index: int, out: dict, t0: float):
        super().__init__(daemon=True)
        self.args, self.index, self.out = args, index, out
        self.t0 = t0
        self.host, self.port = args.target.rsplit(":", 1)
        self.port = int(self.port)

    def _connect(self) -> socket.socket:
        sock = socket.create_connection((self.host, self.port), CONNECT_TIMEOUT)
        sock.settimeout(self.args.timeout)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        return sock

    def run(self) -> None:
        ok = failed = mismatched = 0
        latencies: list = []
        setups: list = []
        errors: list = []
        sock = None
        stopped_early = False
        try:
            for seq in range(self.args.requests):
                if self.args.max_s and time.perf_counter() - self.t0 > self.args.max_s:
                    stopped_early = True
                    break
                if sock is None:
                    t0 = time.perf_counter()
                    sock = self._connect()
                    setups.append((time.perf_counter() - t0) * 1e6)
                want = payload_for(
                    self.index * self.args.requests + seq, self.args.size
                )
                t0 = time.perf_counter()
                try:
                    sock.sendall(want)
                    got = read_exactly(sock, len(want))
                except OSError as exc:
                    failed += 1
                    errors.append(f"{type(exc).__name__}: {exc}")
                    sock = None
                    continue
                latencies.append((time.perf_counter() - t0) * 1e6)
                if got != want:
                    mismatched += 1
                ok += 1
                if self.args.fresh and sock is not None:
                    sock.close()
                    sock = None
        finally:
            if sock is not None:
                sock.close()
        self.out[self.index] = {
            "ok": ok,
            "failed": failed,
            "mismatched": mismatched,
            "latencies_us": latencies,
            "setup_us": setups,
            "errors": errors[:20],
            "stopped_early": stopped_early,
        }


def main() -> int:
    ap = argparse.ArgumentParser(description="request/response probe")
    ap.add_argument("--target", required=True, help="host:port the visitor dials")
    ap.add_argument("--connections", type=int, default=1)
    ap.add_argument("--requests", type=int, default=1000, help="per connection")
    ap.add_argument("--size", type=int, default=64, help="request payload bytes")
    ap.add_argument("--fresh", action="store_true", help="a new connection per request")
    ap.add_argument("--timeout", type=float, default=5.0)
    #: The wall-clock budget for issuing requests. A count is not a duration:
    #: the same 40 000 round trips take a second on a clean path and hours on a
    #: 100 ms one, so the model declares the budget and the probe stops on it —
    #: recording what it did rather than being killed by the harness.
    ap.add_argument("--max-s", type=float, default=0.0, help="0 = no budget")
    args = ap.parse_args()
    if args.connections < 1 or args.requests < 1 or args.size < SEQ_BYTES:
        ap.error(f"connections, requests >= 1 and size >= {SEQ_BYTES}")

    out: dict = {}
    t0 = time.perf_counter()
    workers = [Worker(args, i, out, t0) for i in range(args.connections)]
    for w in workers:
        w.start()
    for w in workers:
        w.join()
    wall = time.perf_counter() - t0

    merged = {
        "max_s": args.max_s,
        "stopped_early": any(r.get("stopped_early") for r in out.values() if r),
        "connections": args.connections,
        "requests_per_connection": args.requests,
        "size": args.size,
        "fresh": args.fresh,
        "wall_s": round(wall, 4),
        "ok": sum(r["ok"] for r in out.values()),
        "failed": sum(r["failed"] for r in out.values()),
        "mismatched": sum(r["mismatched"] for r in out.values()),
        "latencies_us": [v for r in out.values() for v in r["latencies_us"]],
        "setup_us": [v for r in out.values() for v in r["setup_us"]],
        "errors": [e for r in out.values() if r for e in r["errors"]][:20],
    }
    print(
        f"RR ok={merged['ok']} failed={merged['failed']} "
        f"mismatched={merged['mismatched']} wall={merged['wall_s']}s",
        flush=True,
    )
    print(json.dumps(merged), flush=True)
    if merged["ok"] == 0:
        print("RR FAILED: the path carried no round trip at all", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
