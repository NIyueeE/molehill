#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""UDP probe for the L3-vs-L4 comparison.

Runs inside the visitor namespace and knows nothing about the tunnel: it sends
datagrams to whatever address it is given and waits for them back. Two shapes,
because a datagram path has two different questions:

- **paced** (`--rate`): one datagram outstanding at a time, so the loss rate and
  the round-trip distribution are properties of this path at this offered rate,
  not of a queue that was run over. This is the shape a request/response
  protocol (DNS, QUIC's handshake, a game's state) actually uses.
- **blast** (`--rate 0`): as fast as the socket takes them, then drain. This is
  the shape that finds the drop point, and its loss rate is the path's, not the
  probe's, only if the probe's own receive buffer is large enough to hold the
  replies — hence the socket-buffer sizing below.

The last line is a JSON object so the harness parses numbers rather than prose.
`--size` is the datagram's payload; the sequence number lives in the first four
bytes of it, so a reply is matched to its request and a duplicate is not
mistaken for a new one.
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time

#: Big enough that a blast's replies are not the probe's own loss. The path is
#: what is under test; a 4 MiB receive buffer holds ~2 s of a gigabit of
#: 64-byte replies, far more than the drain window.
RCVBUF = 4 << 20


#: Bytes of sequence number at the front of every datagram; the rest is padding.
SEQ_BYTES = 4


def header(seq: int, size: int) -> bytes:
    return struct.pack("!I", seq) + bytes(size - SEQ_BYTES)


def seq_of(data: bytes) -> int | None:
    """The sequence number of a reply, or None for a runt."""
    if len(data) < SEQ_BYTES:
        return None
    return struct.unpack("!I", data[:SEQ_BYTES])[0]


def stats(sent: int, received: set, rtts: list, wall: float, send_s: float) -> dict:
    rtts.sort()

    def pct(q: float):
        if not rtts:
            return None
        return round(rtts[min(len(rtts) - 1, int(q * len(rtts)))], 1)

    return {
        "sent": sent,
        "received": len(received),
        "lost": sent - len(received),
        "loss_rate": round((sent - len(received)) / sent, 5) if sent else None,
        "wall_s": round(wall, 3),
        "rate_per_s": round(sent / wall, 1) if wall > 0 else None,
        # The send phase on its own: a blast's `wall_s` includes the drain, so
        # the offered rate is the send window's, not the whole run's.
        "send_s": round(send_s, 3),
        "send_rate_per_s": round(sent / send_s, 1) if send_s > 0 else None,
        "rtt_p50_us": pct(0.50),
        "rtt_p99_us": pct(0.99),
        "rtt_max_us": round(rtts[-1], 1) if rtts else None,
        "rtt_samples": len(rtts),
    }


def run_paced(sock, target: tuple, args, payload_size: int) -> tuple:
    """One datagram outstanding at a time: the request/response shape."""
    period = 1.0 / args.rate
    rtts: list[float] = []
    received: set = set()
    sent = 0
    t_first_send = time.perf_counter()
    for seq in range(args.datagrams):
        cycle = time.perf_counter()
        sock.sendto(header(seq, payload_size), target)
        sent += 1
        deadline = time.perf_counter() + args.timeout
        while True:
            left = deadline - time.perf_counter()
            if left <= 0:
                break
            sock.settimeout(left)
            try:
                data, _ = sock.recvfrom(65535)
            except TimeoutError:
                break
            if seq_of(data) == seq:
                received.add(seq)
                rtts.append((time.perf_counter() - cycle) * 1e6)
                break
        left = period - (time.perf_counter() - cycle)
        if left > 0:
            time.sleep(left)
    return sent, received, rtts, time.perf_counter() - t_first_send


def run_blast(sock, target: tuple, args, payload_size: int) -> tuple:
    """As fast as the socket takes them, then drain: the drop-point shape.

    Each send is timestamped, so a reply still yields an RTT — which under a
    blast is the *queueing* delay, the number that says whether the path is
    dropping because it is full rather than because it is slow.
    """
    received: set = set()
    rtts: list[float] = []
    sent_at: dict = {}
    t_send0 = time.perf_counter()
    for seq in range(args.datagrams):
        sock.sendto(header(seq, payload_size), target)
        sent_at[seq] = time.perf_counter()
    send_s = time.perf_counter() - t_send0
    deadline = time.perf_counter() + args.drain
    while time.perf_counter() < deadline:
        sock.settimeout(max(0.01, deadline - time.perf_counter()))
        try:
            data, _ = sock.recvfrom(65535)
        except TimeoutError:
            break
        got = seq_of(data)
        if got is not None and got not in received:
            received.add(got)
            if got in sent_at:
                rtts.append((time.perf_counter() - sent_at[got]) * 1e6)
    return args.datagrams, received, rtts, send_s


def run(args) -> dict:
    host, port = args.target.rsplit(":", 1)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, RCVBUF)
    sock.settimeout(args.timeout)
    payload_size = max(SEQ_BYTES, args.size)
    t0 = time.perf_counter()
    if args.rate > 0:
        sent, received, rtts, send_s = run_paced(
            sock, (host, int(port)), args, payload_size
        )
    else:
        sent, received, rtts, send_s = run_blast(
            sock, (host, int(port)), args, payload_size
        )
    return stats(sent, received, rtts, time.perf_counter() - t0, send_s)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", default="10.99.0.1:9004")
    ap.add_argument("--datagrams", type=int, default=5000)
    ap.add_argument("--size", type=int, default=64)
    # 0 = blast (as fast as the socket takes them); anything else is a paced
    # one-outstanding stream in datagrams per second.
    ap.add_argument("--rate", type=float, default=2000.0)
    ap.add_argument("--timeout", type=float, default=0.5)
    ap.add_argument("--drain", type=float, default=1.0)
    args = ap.parse_args()
    try:
        result = run(args)
    except OSError as exc:
        print(json.dumps({"error": f"{type(exc).__name__}: {exc}"}), flush=True)
        return 1
    print(
        f"UDP sent={result['sent']} received={result['received']} "
        f"loss={result['lost']} p50={result['rtt_p50_us']}us "
        f"p99={result['rtt_p99_us']}us",
        flush=True,
    )
    print(json.dumps(result), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
