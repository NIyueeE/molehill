#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Turn two interface-counter snapshots into what header compression could save.

The harness samples `/proc/net/dev` inside the namespaces before and after one
workload arm. Two interfaces matter:

- the **veth** the client dials the server on (`v-cli`) — the tunnel's wire, in
  both directions, including the carrier's own TCP/IP headers;
- the two **TUN** devices — the IP packets the L3 path actually carries. For a
  tun device the counters are the host's: `rx` is what userspace wrote into the
  kernel (the client injecting the visitor's replies) and `tx` is what the
  kernel routed out to userspace (the server handing the visitor's packets to
  the daemon).

Two numbers come out of that. The first is measured: mean carried packet size
per direction, which is what decides whether a header compressor has anything
to work with — the smaller the packets, the larger the share of the wire that
is header. The second is an **upper bound**, not a measurement: an IPv4+TCP
header with no options is 40 bytes and a VJ-style per-flow delta carries about
5, so `35 x packets` is the most any header compressor could take off, and the
first packet of each flow, ICMP and fragments would cost some of it back. It is
printed as a ceiling so the decision to write (or drop) a compressor can be
taken against a number rather than an intuition.

The denominator is stated with every number: `wire` is the veth's bytes, which
is the link's real cost and the convention the keep/remove criteria use;
`carried` is the same packets plus this protocol's own 2-byte length prefix.

The same snapshots carry the two daemons' CPU time, because a wire figure alone
cannot say whether a path is cheap or merely slow: cost per packet and the
share of one core it takes are what decide whether the next lever is fewer
bytes per packet (framing, headers) or more cores on the same bytes
(queues).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

# What a compressor could take off one packet: an IPv4+TCP header without
# options is 40 bytes, and a per-flow delta of the changing fields is ~5.
HEADER_SAVING_BYTES = 35

# A snapshot line's own arity: `iface <ns> <name> <rx B> <rx pkt> <tx B>
# <tx pkt>` and `cpu <who> <ticks>`. A shorter line is malformed, not an
# interface or a process.
IFACE_COLUMNS = 6
CPU_COLUMNS = 2


def parse(path: Path) -> tuple[dict[str, dict[str, int]], dict[str, int]]:
    """The snapshot: per-interface counters, and per-process CPU ticks.

    The harness writes both into one file, in its own two line shapes —
    `iface <namespace> <name> <rx bytes> <rx pkts> <tx bytes> <tx pkts>` and
    `cpu <who> <ticks>` — so that a report never has to know how either was
    read.
    """
    out: dict[str, dict[str, int]] = {}
    cpu: dict[str, int] = {}
    for line in path.read_text().splitlines():
        kind, _, rest = line.partition(" ")
        cols = rest.split()
        if kind == "iface" and len(cols) >= IFACE_COLUMNS:
            out[cols[1]] = {
                "rx_bytes": int(cols[2]),
                "rx_pkts": int(cols[3]),
                "tx_bytes": int(cols[4]),
                "tx_pkts": int(cols[5]),
            }
        elif kind == "cpu" and len(cols) >= CPU_COLUMNS:
            cpu[cols[0]] = int(cols[1])
    return out, cpu


def delta(before: dict, after: dict) -> dict[str, int]:
    return {k: after[k] - before[k] for k in before}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--label", required=True)
    ap.add_argument("--before", type=Path, required=True)
    ap.add_argument("--after", type=Path, required=True)
    ap.add_argument("--client-tun", default="l3cli0")
    ap.add_argument("--server-tun", default="l3srv0")
    ap.add_argument("--tunnel", default="v-cli")
    args = ap.parse_args()

    iface_before, cpu_before = parse(args.before)
    iface_after, cpu_after = parse(args.after)
    d = {
        name: delta(iface_before[name], iface_after[name])
        for name in iface_before
        if name in iface_after
    }

    def mean(counters: dict[str, int], byte_key: str, pkt_key: str) -> float:
        pkts = counters[pkt_key]
        return counters[byte_key] / pkts if pkts else 0.0

    tunnel = d.get(args.tunnel)
    if tunnel is None:
        print(f"no counters for {args.tunnel} in the snapshots", file=sys.stderr)
        return 2
    wire = tunnel["rx_bytes"] + tunnel["tx_bytes"]

    # Direction of the visitor's own traffic, and of the replies to it.
    to_client = d.get(args.server_tun, {})
    to_visitor = d.get(args.client_tun, {})
    fwd = {
        "pkts": to_client.get("tx_pkts", 0),
        "bytes": to_client.get("tx_bytes", 0),
    }
    back = {
        "pkts": to_visitor.get("rx_pkts", 0),
        "bytes": to_visitor.get("rx_bytes", 0),
    }
    carried_pkts = fwd["pkts"] + back["pkts"]
    carried_bytes = fwd["bytes"] + back["bytes"]
    framing = 2 * carried_pkts
    saving = HEADER_SAVING_BYTES * carried_pkts

    print(f"--- {args.label} ---")
    print(
        f"  carried: {carried_pkts} IP packets, {carried_bytes} bytes "
        f"(+{framing} B of framing)"
    )
    print(
        f"    visitor->client: {fwd['pkts']} pkts, {fwd['bytes']} B, "
        f"mean {mean(to_client, 'tx_bytes', 'tx_pkts'):.0f} B/pkt"
    )
    print(
        f"    client->visitor: {back['pkts']} pkts, {back['bytes']} B, "
        f"mean {mean(to_visitor, 'rx_bytes', 'rx_pkts'):.0f} B/pkt"
    )
    if not wire or not carried_pkts:
        print(
            f"  wire ({args.tunnel}, both directions): {wire} B — no traffic to judge"
        )
        return 0

    print(
        f"  wire ({args.tunnel}, both directions): {wire} B, "
        f"{wire / carried_pkts:.0f} B per carried packet"
    )
    print(
        f"  header-compression CEILING: {saving} B of {wire} B wire "
        f"= {100 * saving / wire:.1f}% "
        f"({100 * saving / (carried_bytes + framing):.1f}% of the carried bytes)"
    )
    print(
        "  method: 35 B saved per carried packet (40 B IPv4+TCP header -> ~5 B "
        "delta), every packet assumed compressible; first-packet, ICMP and "
        "fragment exemptions would lower it. Diagnostic, not a benchmark."
    )

    # CPU: the daemons' own time, ticks at the kernel's 100 Hz.
    cpu = {
        who: (cpu_after.get(who, 0) - cpu_before.get(who, 0)) * 10_000
        for who in ("client", "server")
        if who in cpu_after and who in cpu_before
    }
    if cpu and carried_pkts:
        total_us = sum(cpu.values())
        client_us = cpu.get("client", 0)
        server_us = cpu.get("server", 0)
        print(
            f"  cpu: client {client_us} us + server {server_us} us = "
            f"{total_us / carried_pkts:.2f} us per carried packet"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
