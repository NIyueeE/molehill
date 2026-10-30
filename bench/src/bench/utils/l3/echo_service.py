#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Echo service for the transparent-L3 acceptance harness.

Runs inside the client namespace and binds the public address the *client*
owns. The line it prints on accept (`PEER <ip>:<port>`) is the transparency
proof: it must show the visitor's address, never the server's or the client's.
The `echo:` prefix is sent once per connection, so the visitor's byte count
stays exact for the bulk check.
"""

from __future__ import annotations

import argparse
import socket
import sys
import threading

PREFIX = b"echo:"


def serve_one(conn: socket.socket, addr: tuple) -> None:
    peer = f"{addr[0]}:{addr[1]}"
    print(f"PEER {peer}", flush=True)
    reads = 0
    total = 0
    failure = ""
    # Every connection ends with one line that names it. The per-read lines
    # above cannot be attributed — one thread per connection interleaves them —
    # so "which flow reached the backend, and with how much" was a question this
    # log could not answer.
    try:
        with conn:
            conn.sendall(PREFIX)
            while True:
                data = conn.recv(65536)
                if not data:
                    break
                reads += 1
                total += len(data)
                print(f"RECV {len(data)}B", flush=True)
                conn.sendall(data)
    except OSError as exc:
        # A reset is an outcome, not a crash: the summary carries it, and one
        # failed flow must not take the harness down with a traceback.
        failure = f" error={type(exc).__name__}"
    print(f"CLOSED {peer} reads={reads} bytes={total}{failure}", flush=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bind", default="10.99.0.1:8443")
    args = ap.parse_args()
    host, port = args.bind.rsplit(":", 1)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((host, int(port)))
    srv.listen(8)
    print(f"ECHO READY {args.bind}", flush=True)
    while True:
        conn, addr = srv.accept()
        threading.Thread(target=serve_one, args=(conn, addr), daemon=True).start()


if __name__ == "__main__":
    sys.exit(main())
