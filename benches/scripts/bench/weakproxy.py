#!/usr/bin/env python3
"""Userspace weak-network proxy: adds fixed one-way delay to a TCP stream.

No root required (unlike tc netem). Sits between the benchmark's client and
server: client -> PROXY_PORT -> [delay rtt/2] -> server. Every client-originated
connection gets the delay in both directions, so the client observes ~rtt extra
latency on the whole server<->client leg. Visitor-side connections are not
proxied (they model a LAN visitor).

Usage: weakproxy.py <listen_port> <target_host:port> <rtt_ms>

Note: delay-only. Packet LOSS needs kernel netem (CAP_NET_ADMIN) — the bench
skips loss cells when netem is unavailable rather than faking it here, because
a userspace proxy cannot drop packets before the kernel ACKs them.
"""
import asyncio
import sys

LISTEN = int(sys.argv[1])
TARGET_HOST, TARGET_PORT = sys.argv[2].rsplit(":", 1)
TARGET_PORT = int(TARGET_PORT)
ONE_WAY = float(sys.argv[3]) / 2000.0  # rtt_ms -> one-way seconds


async def pipe(reader: asyncio.StreamReader,
               writer: asyncio.StreamWriter,
               peer: asyncio.StreamWriter):
    pending = []
    try:
        while True:
            data = await reader.read(262144)
            if not data:
                break
            pending.append(asyncio.create_task(send(data, peer)))
    finally:
        if pending:
            await asyncio.gather(*pending)
        peer.close()


async def send(data: bytes, writer: asyncio.StreamWriter):
    if ONE_WAY > 0:
        # tasks are created in arrival order and sleep the same duration,
        # so the event loop wakes them in order — byte order is preserved
        await asyncio.sleep(ONE_WAY)
    writer.write(data)


async def handle(client_reader, client_writer):
    try:
        server_reader, server_writer = await asyncio.open_connection(
            TARGET_HOST, TARGET_PORT)
    except OSError:
        client_writer.close()
        return
    # client leg gets the delay towards the server, and the server leg gets
    # it back towards the client: full rtt added on the client<->server path
    t1 = asyncio.create_task(pipe(client_reader, server_writer, server_writer))
    t2 = asyncio.create_task(pipe(server_reader, client_writer, client_writer))
    await asyncio.gather(t1, t2)
    client_writer.close()


async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
