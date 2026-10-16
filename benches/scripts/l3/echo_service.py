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
    print(f"PEER {addr[0]}:{addr[1]}", flush=True)
    with conn:
        conn.sendall(PREFIX)
        while True:
            data = conn.recv(65536)
            if not data:
                break
            print(f"RECV {len(data)}B", flush=True)
            conn.sendall(data)
    print("CLOSED", flush=True)


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
