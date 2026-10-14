#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Visitor for the transparent-L3 acceptance harness.

Runs inside the visitor namespace and knows nothing about the tunnel: it opens
a TCP connection to the public address and checks what comes back. `--size N`
sends a deterministic N-byte payload and compares the echoed bytes exactly, so
a dropped or reordered segment fails the run. `--hold S` keeps the connection
open after the check, so the harness can snapshot live socket and conntrack
state. A one-line `VISITOR OK` (and exit 0) is the pass signal.
"""

from __future__ import annotations

import argparse
import socket
import sys
import time


def recv_exactly(sock: socket.socket, want: int) -> bytes:
    """Read up to `want` bytes, stopping early only at end of stream."""
    got = bytearray()
    while len(got) < want:
        chunk = sock.recv(65536)
        if not chunk:
            break
        got += chunk
    return bytes(got)


def run(args: argparse.Namespace) -> bool:
    host, port = args.target.rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout=args.timeout) as sock:
        local = sock.getsockname()
        print(f"CONNECTED {local[0]}:{local[1]}", flush=True)

        if args.size:
            blob = (bytes(range(256)) * (args.size // 256 + 1))[: args.size]
            sock.sendall(blob)
            expected = b"echo:" + blob
            got = recv_exactly(sock, len(expected))
            print(f"BULK sent={len(blob)} received={len(got)}", flush=True)
            ok = got == expected
            verdict = "VISITOR OK" if ok else "VISITOR FAIL bulk mismatch"
        else:
            payload = args.payload.encode()
            sock.sendall(payload)
            expected = b"echo:" + payload
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
    # black-hole check.
    ap.add_argument("--size", type=int, default=0)
    ap.add_argument("--hold", type=float, default=0.0)
    # The readiness probe retries with a short timeout; the real visitor can
    # afford the default.
    ap.add_argument("--timeout", type=float, default=5.0)
    args = ap.parse_args()

    try:
        ok = run(args)
    except OSError as exc:
        print(f"VISITOR FAIL {exc}", flush=True)
        return 1
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
