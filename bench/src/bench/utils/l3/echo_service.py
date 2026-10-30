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
import struct
import sys
import threading

PREFIX = b"echo:"


#: `struct tcp_info` up to and including `tcpi_total_retrans`, which sits at
#: offset 100.
TCP_INFO_RETRANS_END = 104

#: Offset of `tcpi_total_retrans` in `struct tcp_info`.
TCP_INFO_TOTAL_RETRANS = 100


def retransmits(conn: socket.socket) -> int:
    """How many times this connection's kernel had to resend, or -1 if unknown.

    The one number that separates "the reply was lost on the way" from "the reply
    was never sent": a reply the peer acknowledged costs no retransmission, so a
    stalled flow whose backend reports retransmissions had its replies going
    missing between here and the visitor — and one reporting zero had them
    accepted, which would mean the loss is on the visitor's side of the wire.
    """
    level = getattr(socket, "TCP_INFO", None)
    if level is None:
        return -1
    try:
        info = conn.getsockopt(socket.IPPROTO_TCP, level, TCP_INFO_RETRANS_END)
    except OSError:
        return -1
    if len(info) < TCP_INFO_RETRANS_END:
        return -1
    return struct.unpack("I", info[TCP_INFO_TOTAL_RETRANS:TCP_INFO_RETRANS_END])[0]


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
    sent_again = -1
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
            # Still inside the `with`: the count is gone once the socket closes.
            sent_again = retransmits(conn)
    except OSError as exc:
        # A reset is an outcome, not a crash: the summary carries it, and one
        # failed flow must not take the harness down with a traceback.
        failure = f" error={type(exc).__name__}"
    print(
        f"CLOSED {peer} reads={reads} bytes={total}{failure} retrans={sent_again}",
        flush=True,
    )


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
