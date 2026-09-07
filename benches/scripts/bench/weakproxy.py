#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Userspace weak-network proxy: adds fixed one-way delay to a TCP stream, or
delay + optional loss to a UDP datagram flow.

No root required (unlike tc netem). Sits between the benchmark's client and
server: client -> PROXY_PORT -> [delay rtt/2] -> server. Every client-originated
connection gets the delay in both directions, so the client observes ~rtt extra
latency on the whole server<->client leg. Visitor-side connections are not
proxied (they model a LAN visitor).

TCP mode (default): delay-only. Packet LOSS needs kernel netem
(CAP_NET_ADMIN) — the bench skips TCP loss cells when netem is unavailable
rather than faking it here, because a userspace proxy cannot drop packets
before the kernel ACKs them.

UDP mode (--udp): delay AND loss are both possible — UDP datagrams are not
retransmitted by the kernel, so a userspace proxy can drop them before they
reach the target. Loss applies independently per direction. Every client
address gets its own target socket, so distinct visitors stay distinct peers
from the forwarded service's point of view (session affinity stays observable).

Usage:
  weakproxy.py <listen_port> <target_host:port> <rtt_ms>
  weakproxy.py --udp <listen_port> <target_host:port> <rtt_ms> [loss_pct]
"""
import asyncio
import contextlib
import random
import sys

args = sys.argv[1:]
UDP = bool(args) and args[0] == "--udp"
if UDP:
    args = args[1:]
LISTEN = int(args[0])
TARGET_HOST, TARGET_PORT = args[1].rsplit(":", 1)
TARGET_PORT = int(TARGET_PORT)
ONE_WAY = float(args[2]) / 2000.0  # rtt_ms -> one-way seconds
LOSS_PCT = float(args[3]) if UDP and len(args) > 3 else 0.0

_TASKS: list = []  # keep references so pending relays are not GC-collected


def drop():
    return LOSS_PCT > 0 and random.random() * 100.0 < LOSS_PCT


async def delayed_send(data: bytes, writer):
    if ONE_WAY > 0:
        # tasks are created in arrival order and sleep the same duration,
        # so the event loop wakes them in order — byte order is preserved
        await asyncio.sleep(ONE_WAY)
    writer.write(data)


async def tcp_pipe(reader: asyncio.StreamReader,
                   writer: asyncio.StreamWriter):
    pending = []
    try:
        while True:
            data = await reader.read(262144)
            if not data:
                break
            pending.append(asyncio.create_task(delayed_send(data, writer)))
    finally:
        if pending:
            await asyncio.gather(*pending)
        writer.close()


async def handle(client_reader, client_writer):
    try:
        server_reader, server_writer = await asyncio.open_connection(
            TARGET_HOST, TARGET_PORT)
    except OSError:
        client_writer.close()
        return
    # client leg gets the delay towards the server, and the server leg gets
    # it back towards the client: full rtt added on the client<->server path
    t1 = asyncio.create_task(tcp_pipe(client_reader, server_writer))
    t2 = asyncio.create_task(tcp_pipe(server_reader, client_writer))
    await asyncio.gather(t1, t2)
    client_writer.close()


async def tcp_main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    async with server:
        await server.serve_forever()


async def udp_main():
    loop = asyncio.get_running_loop()
    target = (TARGET_HOST, TARGET_PORT)

    # Single-socket relay: client datagrams and target replies both arrive on
    # the same socket and are told apart by source address. One client at a
    # time (the bench pings with one pinger per arm) — documented limitation.

    class Proto(asyncio.DatagramProtocol):
        client_addr = None
        transport = None

        def connection_made(self, transport):
            Proto.transport = transport

        def datagram_received(self, data, addr):
            if addr == target:
                ca = Proto.client_addr
                if ca is None:
                    return
                _TASKS.append(loop.create_task(relay(data, ca)))
            else:
                Proto.client_addr = addr
                _TASKS.append(loop.create_task(relay(data, target)))

    async def relay(data: bytes, dest):
        if ONE_WAY > 0:
            await asyncio.sleep(ONE_WAY)
        if drop():
            return
        with contextlib.suppress(OSError):
            Proto.transport.sendto(data, dest)

    transport, _ = await loop.create_datagram_endpoint(
        Proto, local_addr=("127.0.0.1", LISTEN))
    Proto.transport = transport
    await asyncio.Event().wait()


if __name__ == "__main__":
    with contextlib.suppress(KeyboardInterrupt):
        asyncio.run(udp_main() if UDP else tcp_main())
