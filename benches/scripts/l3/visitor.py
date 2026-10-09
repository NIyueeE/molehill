#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Visitor for the transparent-L3 acceptance harness.

Runs inside the visitor namespace and knows nothing about the tunnel: it opens
a TCP connection to the public address and checks what comes back. `--size N`
sends a deterministic N-byte payload and compares the echoed bytes exactly, so
a dropped or reordered segment fails the run. `--requests N` runs N strict
request/response round trips instead, which is the small-packet workload: every
round trip is its own segment in both directions, so the tunnel carries many
small packets rather than a few full ones. `--connections N` runs that workload
on N connections at once, which is the *multi-flow* arm: several flows
interleaved through one claim, where packets queue and batches can form.
`--hold S` keeps the connection open after the check, so the harness can
snapshot live socket and conntrack state. A one-line `VISITOR OK` (and exit 0)
is the pass signal.
"""

from __future__ import annotations

import argparse
import socket
import sys
import threading
import time

PREFIX = b"echo:"


def recv_exactly(sock: socket.socket, want: int) -> bytes:
    """Read up to `want` bytes, stopping early only at end of stream."""
    got = bytearray()
    while len(got) < want:
        chunk = sock.recv(65536)
        if not chunk:
            break
        got += chunk
    return bytes(got)


def pattern(size: int) -> bytes:
    """A deterministic payload of exactly `size` bytes."""
    return (bytes(range(256)) * (size // 256 + 1))[:size]


def round_trips(sock: socket.socket, count: int, size: int) -> bool:
    """N strict request/response cycles: nothing is sent until the reply is in.

    One outstanding request at a time is the point — it keeps each round trip
    its own small segment, which is the workload whose packet sizes decide what
    header compression could save.
    """
    blob = pattern(size)
    for i in range(count):
        sock.sendall(blob)
        got = recv_exactly(sock, len(blob))
        if got != blob:
            print(f"REQUESTS FAIL at {i}: got {len(got)} bytes", flush=True)
            return False
    print(f"REQUESTS n={count} size={size} ok", flush=True)
    return True


def one_flow(
    target: tuple, index: int, requests: int, size: int, timeout: float
) -> str:
    """One connection's worth of strict round trips; returns "" when it passed."""
    try:
        with socket.create_connection(target, timeout=timeout) as sock:
            banner = recv_exactly(sock, len(PREFIX))
            if banner != PREFIX:
                return f"flow {index}: expected {PREFIX!r}, got {banner!r}"
            if not round_trips(sock, requests, size):
                return f"flow {index}: round trips did not match"
    except OSError as exc:
        return f"flow {index}: {exc}"
    return ""


def many_flows(
    target: tuple, conns: int, requests: int, size: int, timeout: float
) -> bool:
    """`conns` flows at once: what a claim looks like with several visitors.

    Threads, not processes: a socket read releases the GIL, so the flows
    interleave in the tunnel the way separate visitors would, while the probe
    itself costs one process.
    """
    failures: list[str] = []
    lock = threading.Lock()

    def worker(index: int) -> None:
        failure = one_flow(target, index, requests, size, timeout)
        if failure:
            with lock:
                failures.append(failure)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(conns)]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = time.perf_counter() - started

    for failure in failures:
        print(f"CONCURRENT FAIL {failure}", flush=True)
    print(
        f"CONCURRENT conns={conns} requests={requests} size={size} "
        f"ok={conns - len(failures)}/{conns} in {elapsed:.3f}s",
        flush=True,
    )
    return not failures


def run(args: argparse.Namespace) -> bool:
    host, port = args.target.rsplit(":", 1)
    if args.connections:
        ok = many_flows(
            (host, int(port)), args.connections, args.requests, args.size, args.timeout
        )
        print("VISITOR OK" if ok else "VISITOR FAIL concurrent flows", flush=True)
        return ok

    with socket.create_connection((host, int(port)), timeout=args.timeout) as sock:
        local = sock.getsockname()
        print(f"CONNECTED {local[0]}:{local[1]}", flush=True)

        if args.requests:
            # The service announces itself once per connection; take it off the
            # stream so the round trips below are symmetric.
            banner = recv_exactly(sock, len(PREFIX))
            if banner != PREFIX:
                print(f"VISITOR FAIL expected {PREFIX!r}, got {banner!r}", flush=True)
                return False
            ok = round_trips(sock, args.requests, args.size)
            verdict = "VISITOR OK" if ok else "VISITOR FAIL requests"
        elif args.size:
            blob = pattern(args.size)
            sock.sendall(blob)
            expected = PREFIX + blob
            got = recv_exactly(sock, len(expected))
            print(f"BULK sent={len(blob)} received={len(got)}", flush=True)
            ok = got == expected
            verdict = "VISITOR OK" if ok else "VISITOR FAIL bulk mismatch"
        else:
            payload = args.payload.encode()
            sock.sendall(payload)
            expected = PREFIX + payload
            got = recv_exactly(sock, len(expected))
            print(f"REPLY {got!r}", flush=True)
            ok = got == expected
            verdict = "VISITOR OK" if ok else f"VISITOR FAIL expected {expected!r}"
        print(verdict, flush=True)

        # Sleep inside the `with` so the connection stays open for the
        # harness's snapshots.
        if args.hold:
            time.sleep(args.hold)
        return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", default="10.99.0.1:8443")
    ap.add_argument("--payload", default="hello-transparent")
    # --size replaces --payload: multi-segment traffic, which is also the MTU
    # black-hole check. With --requests it is the per-request payload instead.
    ap.add_argument("--size", type=int, default=0)
    # --requests N: N strict round trips of --size bytes (default 64).
    ap.add_argument("--requests", type=int, default=0)
    # --connections N: run that workload on N connections at once (multi-flow).
    ap.add_argument("--connections", type=int, default=0)
    ap.add_argument("--hold", type=float, default=0.0)
    # The readiness probe retries with a short timeout; the real visitor can
    # afford the default.
    ap.add_argument("--timeout", type=float, default=5.0)
    args = ap.parse_args()
    if (args.requests or args.connections) and not args.size:
        args.size = 64

    try:
        ok = run(args)
    except OSError as exc:
        print(f"VISITOR FAIL {exc}", flush=True)
        return 1
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
