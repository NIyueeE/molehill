#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""The long-running probes: one sample per line, for a whole staged run.

A staged run is a *time series*, not one number per stage: the tool is driven
while the path changes underneath it, and the question is what its interactive
stream, its datagram session and its connection churn do as that happens. Each
mode here owns one of those streams and writes NDJSON:

    {"t": 1760000000.123, "metric": "rtt_ms", "v": 0.41}

The engine brackets the run with counter snapshots and slices the lines by
stage window, so a per-stage number and a drift slope over the whole run come
from the same samples. Three modes:

* **interactive** — a fresh TCP connection per ping (the SLO instrument: a
  visitor arriving, not a session already open), `--interval-ms` apart;
* **udp** — a datagram echoed back per attempt, so the line is either an RTT or
  a loss;
* **churn** — `--rate` short-lived connections per second, each with one
  request, recording the setup time: the cost of arriving, under load.

A failure is a line, never silence: `metric: "error"` carries a typed reason in
`why`, because a stream that stops producing lines is indistinguishable from a
tool that stopped answering.
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time

#: The ping's payload: 64 B, one cache line, the SLO's own size.
PING_BYTES = 64
#: How long an attempt may take before it is a failure. A ping that is still
#: waiting is not a measurement.
ATTEMPT_TIMEOUT_S = 5.0
#: Sockets that cannot be created are retried rather than recorded: a refused
#: connection during a restart is the tool's business, a closed socket in this
#: process is not.
UDP_SEQ_BYTES = 4


def emit(metric: str, value, **extra) -> None:
    line = {"t": time.time(), "metric": metric, "v": value} | extra
    sys.stdout.write(json.dumps(line) + "\n")
    sys.stdout.flush()


def emit_error(why: str) -> None:
    emit("error", 1, why=why[:120])


def ping(target: tuple, size: int) -> None:
    """One fresh TCP connection, one echo, timed end to end."""
    started = time.perf_counter()
    try:
        with socket.create_connection(target, ATTEMPT_TIMEOUT_S) as sock:
            sock.settimeout(ATTEMPT_TIMEOUT_S)
            payload = bytes(size)
            sock.sendall(payload)
            got = 0
            while got < size:
                chunk = sock.recv(size - got)
                if not chunk:
                    break
                got += len(chunk)
    except OSError as exc:
        emit_error(f"{type(exc).__name__}: {exc}")
        return
    if got != size:
        emit_error(f"short echo: {got} of {size} bytes")
        return
    emit("rtt_ms", round((time.perf_counter() - started) * 1000, 4))


def _deadline(args) -> float:
    """When to stop: a timestamp, or infinity for "until killed"."""
    return time.time() + args.duration_s if args.duration_s else float("inf")


def run_interactive(target: tuple, args) -> None:
    period = args.interval_ms / 1000.0
    end = _deadline(args)
    while time.time() < end:
        started = time.perf_counter()
        ping(target, args.size)
        left = period - (time.perf_counter() - started)
        if left > 0:
            time.sleep(left)


def run_udp(target: tuple, args) -> None:
    """A datagram echoed back per attempt: an RTT, or a loss."""
    period = args.interval_ms / 1000.0
    end = _deadline(args)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
    seq = 0
    try:
        while time.time() < end:
            started = time.perf_counter()
            seq += 1
            payload = struct.pack("!I", seq) + bytes(max(0, args.size - UDP_SEQ_BYTES))
            try:
                sock.sendto(payload, target)
                sock.settimeout(ATTEMPT_TIMEOUT_S)
                data, _ = sock.recvfrom(65535)
                got = struct.unpack("!I", data[:UDP_SEQ_BYTES])[0]
                if got != seq:
                    emit("loss", 1, why="a reply arrived for another datagram")
                elif len(data) != args.size:
                    emit("loss", 1, why=f"reply was {len(data)} of {args.size} bytes")
                else:
                    emit(
                        "rtt_ms",
                        round((time.perf_counter() - started) * 1000, 4),
                    )
            except (TimeoutError, OSError) as exc:
                emit("loss", 1, why=f"{type(exc).__name__}: {exc}"[:80])
            left = period - (time.perf_counter() - started)
            if left > 0:
                time.sleep(left)
    finally:
        sock.close()


def run_churn(target: tuple, args) -> None:
    """`--rate` fresh connections per second, each with one request."""
    period = 1.0 / args.rate
    end = _deadline(args)
    next_at = time.perf_counter()
    while time.time() < end:
        started = time.perf_counter()
        ping(target, args.size)
        emit("setup_ms", round((time.perf_counter() - started) * 1000, 4))
        next_at += period
        left = next_at - time.perf_counter()
        if left > 0:
            time.sleep(left)
        else:
            # Fell behind: re-anchor rather than accumulate a backlog, so the
            # offered rate is the rate this probe actually offers.
            next_at = time.perf_counter()


MODES = {
    "interactive": run_interactive,
    "udp": run_udp,
    "churn": run_churn,
}


def main() -> int:
    ap = argparse.ArgumentParser(description="staged-run series probe")
    ap.add_argument("--mode", required=True, choices=sorted(MODES))
    ap.add_argument("--target", required=True, help="host:port the visitor dials")
    #: Seconds to run. `0` means "until killed": a staged run's stages are
    #: separated by drains whose cost is not known in advance, so a duration
    #: computed before the run can end before its last stage does - measured,
    #: as stages that carried no interactive samples at all. The workload owns
    #: the lifetime and kills the probe in its own teardown.
    ap.add_argument("--duration-s", type=float, required=True)
    ap.add_argument("--interval-ms", type=float, default=50.0)
    ap.add_argument("--rate", type=float, default=16.0, help="churn connections/s")
    ap.add_argument("--size", type=int, default=PING_BYTES)
    args = ap.parse_args()
    if args.duration_s < 0:
        ap.error("--duration-s must be >= 0 (0 = until killed)")

    host, port = args.target.rsplit(":", 1)
    target = (host, int(port))
    emit("start", 1, mode=args.mode, target=args.target)
    MODES[args.mode](target, args)
    emit("end", 1, mode=args.mode)
    return 0


if __name__ == "__main__":
    sys.exit(main())
