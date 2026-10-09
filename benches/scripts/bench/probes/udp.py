#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""UDP probe: loss, spacing and round-trip shape of a datagram path.

Runs inside the visitor's namespace and knows nothing about the tunnel. Two
shapes, because a datagram path has two different questions:

- **paced** (`--rate` datagrams/s): one datagram outstanding at a time, so the
  loss and the round-trip distribution are properties of this path at this
  offered rate, not of a queue that was run over. This is the shape a
  request/response protocol (DNS, QUIC's handshake, a game's state) uses.
- **blast** (`--rate 0`): as fast as the socket takes them, then drain. This
  finds the drop point, and its round trips are *queueing* delay — the number
  that says whether a path is dropping because it is full rather than because it
  is slow. Its loss is the path's only if the probe's own receive buffer can
  hold the replies, hence `RCVBUF` below.

Every datagram carries a four-byte sequence number, so a duplicate is not
mistaken for a new arrival and a reply is matched to its request. The last line
is JSON with the raw sample arrays; percentiles come from the engine, so every
derived metric has one definition.
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time

#: Big enough that a blast's replies are not the probe's own loss: 4 MiB holds
#: ~2 s of a gigabit of 64-byte replies, far more than the drain window.
RCVBUF = 4 << 20
#: Bytes of sequence number at the front of every datagram.
SEQ_BYTES = 4
#: The reply's arrival gaps are recorded for the spacing metric; a gap above
#: this is not a sample, it is the probe waiting for a drain that never came.
MAX_GAP_US = 5_000_000.0


def header(seq: int, size: int) -> bytes:
    return struct.pack("!I", seq) + bytes(max(0, size - SEQ_BYTES))


def seq_of(data: bytes) -> int | None:
    if len(data) < SEQ_BYTES:
        return None
    return struct.unpack("!I", data[:SEQ_BYTES])[0]


def payload_ok(data: bytes, size: int) -> bool:
    """The reply must be the datagram that was sent: sequence plus zero fill.

    A truncated or padded reply is a path finding (fragmentation, a mangling
    middlebox), not a slow sample, so it is counted rather than ignored.
    """
    return len(data) >= size and data[SEQ_BYTES:size] == bytes(size - SEQ_BYTES)


def _exchange(sock: socket.socket, target: tuple, seq: int, args) -> tuple:
    """Send one datagram and wait for that datagram's own reply.

    Returns `(rtt_us, mismatched, replied)`. One outstanding datagram is the
    request/response shape: the loss and the round-trip distribution are then
    properties of this path at this offered rate, not of a queue that was run
    over.
    """
    cycle = time.perf_counter()
    sock.sendto(header(seq, args.size), target)
    deadline = time.perf_counter() + args.timeout
    while True:
        left = deadline - time.perf_counter()
        if left <= 0:
            return None, False, False
        sock.settimeout(left)
        try:
            data, _ = sock.recvfrom(65535)
        except TimeoutError:
            return None, False, False
        if seq_of(data) == seq:
            return (
                (time.perf_counter() - cycle) * 1e6,
                not payload_ok(data, args.size),
                True,
            )


def run_paced(sock: socket.socket, target: tuple, args) -> dict:
    """One datagram outstanding at a time, at `--rate` datagrams per second."""
    period = 1.0 / args.rate
    rtts: list = []
    gaps: list = []
    received: set = set()
    mismatched = 0
    stopped_early = False
    sent = 0
    last_arrival = None
    t_first = time.perf_counter()
    while sent < args.datagrams:
        if args.max_s and time.perf_counter() - t_first > args.max_s:
            stopped_early = True
            break
        cycle = time.perf_counter()
        rtt, bad, replied = _exchange(sock, target, sent, args)
        sent += 1
        if replied:
            now = time.perf_counter()
            received.add(sent - 1)
            rtts.append(rtt)
            mismatched += bad
            if last_arrival is not None:
                gaps.append((now - last_arrival) * 1e6)
            last_arrival = now
        left = period - (time.perf_counter() - cycle)
        if left > 0:
            time.sleep(left)
    return {
        "sent": sent,
        "received": len(received),
        "dups": 0,
        "mismatched": mismatched,
        "rtt_us": rtts,
        "gaps_us": [g for g in gaps if g <= MAX_GAP_US],
        "send_s": time.perf_counter() - t_first,
        "wall_s": time.perf_counter() - t_first,
        "stopped_early": stopped_early,
    }


def run_blast(sock: socket.socket, target: tuple, args) -> dict:
    """As fast as the socket takes them, then drain: the drop-point shape."""
    received: set = set()
    dups = 0
    mismatched = 0
    rtts: list = []
    gaps: list = []
    sent_at: dict = {}
    t_send0 = time.perf_counter()
    for seq in range(args.datagrams):
        sock.sendto(header(seq, args.size), target)
        sent_at[seq] = time.perf_counter()
    send_s = time.perf_counter() - t_send0
    last_arrival = None
    deadline = time.perf_counter() + args.drain
    while time.perf_counter() < deadline:
        sock.settimeout(max(0.01, deadline - time.perf_counter()))
        try:
            data, _ = sock.recvfrom(65535)
        except TimeoutError:
            break
        got = seq_of(data)
        if got is None:
            continue
        now = time.perf_counter()
        if got in received:
            dups += 1
            continue
        received.add(got)
        if not payload_ok(data, args.size):
            mismatched += 1
        if got in sent_at:
            rtts.append((now - sent_at[got]) * 1e6)
        if last_arrival is not None:
            gap = (now - last_arrival) * 1e6
            if gap <= MAX_GAP_US:
                gaps.append(gap)
        last_arrival = now
    return {
        "sent": args.datagrams,
        "received": len(received),
        "dups": dups,
        "mismatched": mismatched,
        "rtt_us": rtts,
        "gaps_us": gaps,
        "send_s": send_s,
        "wall_s": time.perf_counter() - t_send0,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="UDP probe (paced or blast)")
    ap.add_argument("--target", required=True)
    ap.add_argument("--datagrams", type=int, default=5000)
    ap.add_argument("--size", type=int, default=1200)
    ap.add_argument("--rate", type=float, default=2000.0, help="datagrams/s; 0 = blast")
    ap.add_argument("--timeout", type=float, default=0.5)
    ap.add_argument("--drain", type=float, default=2.0)
    #: The wall-clock budget for offering datagrams: a paced run on a lossy
    #: path pays a timeout per lost datagram, so a count is not a duration.
    ap.add_argument("--max-s", type=float, default=0.0, help="0 = no budget")
    args = ap.parse_args()
    if args.size < SEQ_BYTES or args.datagrams < 1:
        ap.error("size >= 4 and datagrams >= 1")

    host, port = args.target.rsplit(":", 1)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, RCVBUF)
    sock.settimeout(args.timeout)
    try:
        result = (
            run_paced(sock, (host, int(port)), args)
            if args.rate > 0
            else run_blast(sock, (host, int(port)), args)
        )
    except OSError as exc:
        print(json.dumps({"error": f"{type(exc).__name__}: {exc}"}), flush=True)
        return 1
    finally:
        sock.close()

    result |= {
        "size": args.size,
        "max_s": args.max_s,
        "rate_per_s": args.rate,
        "offered_per_s": round(result["sent"] / max(result["send_s"], 1e-9), 1),
    }
    print(
        f"UDP sent={result['sent']} received={result['received']} "
        f"dups={result['dups']} send={result['send_s']:.3f}s",
        flush=True,
    )
    print(json.dumps(result), flush=True)
    if result["received"] == 0:
        print("UDP FAILED: no datagram came back at all", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
