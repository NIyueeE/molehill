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
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

# What a compressor could take off one packet: an IPv4+TCP header without
# options is 40 bytes, and a per-flow delta of the changing fields is ~5.
HEADER_SAVING_BYTES = 35

# /proc/net/dev columns per interface: 8 receive fields, then the transmit
# ones, so a short line is a malformed one rather than an interface.
MIN_DEV_COLUMNS = 16


def parse(path: Path) -> dict[str, dict[str, int]]:
    """`{interface: {rx_bytes, rx_pkts, tx_bytes, tx_pkts}}` from /proc/net/dev."""
    out: dict[str, dict[str, int]] = {}
    for line in path.read_text().splitlines():
        name, _, rest = line.partition(":")
        if not rest:
            continue
        cols = rest.split()
        if len(cols) < MIN_DEV_COLUMNS:
            continue
        out[name.strip()] = {
            "rx_bytes": int(cols[0]),
            "rx_pkts": int(cols[1]),
            "tx_bytes": int(cols[8]),
            "tx_pkts": int(cols[9]),
        }
    return out


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

    before, after = parse(args.before), parse(args.after)
    d = {name: delta(before[name], after[name]) for name in before if name in after}

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
    return 0


if __name__ == "__main__":
    sys.exit(main())
