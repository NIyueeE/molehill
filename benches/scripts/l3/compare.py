#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""L3 (transparent) against L4 (terminated): what each architecture costs.

The bench's stage-schedule model (`soak.soak`) cannot express an L3 arm: it runs
every arm on host loopback, where a transparent client has no visitor stack to
own an address in, and its per-tool shaping is an HTB class on `lo`. This runner
is the L3 arm's home, and it is built to the same rules (AGENTS.md §10):

* **the path under test is what is dialed.** Every probe dials the address the
  *tool* exposes — the server's listener for L4, the address the *client* owns
  on its TUN for L3 — never the backend;
* **a control beside every number.** The `control` arm runs the same workload
  and the same port with no tool in the path, so a slow probe and a slow tunnel
  are distinguishable;
* **same host, same run, interleaved.** The arms take turns inside one campaign
  (the order rotates per round), because sequential before/after runs are
  defeated by epoch drift;
* **one variable per pair.** `l4`, `l4-direct` and `l3` differ in the mode and
  in the address the visitor dials; the backend process, the ports, the visitor
  probe and the binary are the same ones throughout;
* **variance is data.** Every workload repeats, and the summary quotes
  min/median/max rather than one number per cell.

Topology (root-only, Linux-only; the same three namespaces the acceptance
harness proves, with its own names so the two cannot collide):

    visitor ns ──v-vis/v-srv── server ns ──v-cli/v-srv2── client ns
     10.10.0.2                  10.10.0.254              10.30.0.2
                                TUN l3cmpsrv0            TUN l3cmpcli0
                                                          owns 10.99.0.1

The backend (an echoing TCP service and a one-off iperf3 server per bulk
measurement) runs inside the *client* namespace on `0.0.0.0`, which is where a
service behind the NAT sits. The two modes reach it differently, and that
difference is the architecture under measurement:

* **L4**: the server binds `10.10.0.254:<port>`; the client dials
  `127.0.0.1:<port>` inside its own namespace. The visitor's TCP connection is
  terminated by the server and its bytes are re-sent over the tunnel.
* **L3**: the client owns `10.99.0.1` on its TUN; the server has a route for it
  on its own TUN. The visitor's packets are carried whole and delivered by the
  *client's* kernel to the same backend process. Same port, same backend.

Run it as root, after building the binary the numbers should describe:

    sudo -n uv run benches/scripts/l3/compare.py --rounds 4

It writes its results file outside the tree by default (`~/tmp/l3-vs-l4-*.json`)
and prints the per-arm table; nothing is committed, because a comparison is
analyzed before it is published.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Self

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent.parent
# The measurement primitives are the Soak model's (`iperf_result` and its
# measured-window convention, the host identity, the revision verdict). An
# out-of-model arm parsed by a second implementation of them would drift from
# the numbers the gate and the charts are built on.
sys.path.insert(0, str(HERE.parent / "soak"))
import lib  # noqa: E402

VISITOR = HERE / "visitor.py"

# --- the topology's constants -----------------------------------------------
# Names carry a `cmp` infix: the acceptance harness's namespaces (`l3vis`,
# `l3srv`, `l3cli`) must not be disturbed by a comparison run, and vice versa.
VIS_NS = "l3cmpvis"
SRV_NS = "l3cmpsrv"
CLI_NS = "l3cmpcli"
TUN_SRV = "l3cmpsrv0"
TUN_CLI = "l3cmpcli0"

VIS_IP = "10.10.0.2"
SRV_VIS_IP = "10.10.0.254"
SRV_CLI_IP = "10.30.0.1"
CLI_IP = "10.30.0.2"
PUBLIC_IP = "10.99.0.1"

CONTROL_PORT = 2333
ECHO_PORT = 9001
IPERF_PORT = 9002
ROUTE_TABLE = 100
# The address a visitor dials on the `control` arm: the client namespace's own
# veth address, i.e. the service with no tool in front of it.
CONTROL_IP = CLI_IP

#: The deeper queue the `l3-deep` arm gives both TUN devices: the same
#: architecture with the kernel's packet queue at 10 000 instead of 1000. It is
#: an operator setting (`ip link set <tun> txqueuelen N`), so an architecture
#: comparison should price both: the default's drops and the deeper queue's
#: buffering.
DEEP_TXQUEUELEN = 10000

#: The fields `/proc/net/dev` prints per interface (name, then 16 counters).
DEV_COLUMNS = 16

#: `/proc/net/snmp`'s Tcp line: the algorithm parameters, then the counters
#: this instrument reads (InSegs, OutSegs, RetransSegs, InErrs).
TCP_MIB_COLUMNS = 13

#: Ticks per second of the CPU counters, the denominator of every cost figure.
CLK_TCK = os.sysconf("SC_CLK_TCK")

#: The echo backend. It runs inside the client namespace, announces itself once
#: per connection (the visitor probe expects the banner) and echoes. `PEER` is
#: printed per accept because it is the transparency evidence: on the L3 arm it
#: must be the visitor's address, on L4 it is the client's (the server
#: terminated the visitor's connection, which is the architectural difference
#: this file exists to price).
ECHO_SRC = """
import socket, sys, threading

port = int(sys.argv[1])


def serve(conn, addr):
    sys.stdout.write(f"PEER {addr[0]}:{addr[1]}\\n")
    sys.stdout.flush()
    with conn:
        conn.sendall(b"echo:")
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
# 0.0.0.0 because the three arms reach this one process by three addresses.
srv.bind(("0.0.0.0", port))
srv.listen(128)
print(f"ECHO READY {port}", flush=True)
while True:
    conn, addr = srv.accept()
    threading.Thread(target=serve, args=(conn, addr), daemon=True).start()
"""


def log(*a) -> None:
    print(*a, flush=True)


def run(argv: list, timeout: float = 30.0, check: bool = True):
    """One short-lived host command; raises with its own stderr in the message."""
    r = subprocess.run(
        argv, capture_output=True, text=True, timeout=timeout, check=False
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"{' '.join(argv)}: exit {r.returncode}: {r.stderr.strip()[:300]}"
        )
    return r


class Topology:
    """The three namespaces, their links and the two TUN devices.

    Everything here is the *operator's* side of the contract: the daemons
    configure no network themselves, they attach to the devices this creates
    (src/transparent/check.rs verifies the parts they depend on).
    """

    def __init__(self, tun_mtu: int, link_mtu: int):
        self.tun_mtu = tun_mtu
        self.link_mtu = link_mtu

    # --- helpers -----------------------------------------------------------
    @staticmethod
    def ns_argv(ns: str, argv: list) -> list:
        return ["ip", "netns", "exec", ns, *argv]

    def ns_run(self, ns: str, argv: list, timeout: float = 30.0, check: bool = True):
        return run(self.ns_argv(ns, argv), timeout=timeout, check=check)

    @staticmethod
    def _ip(*argv: str, ns: str | None = None) -> None:
        run(["ip", *(["-n", ns] if ns else []), *argv])

    def up(self) -> None:
        self.down()
        for ns in (VIS_NS, SRV_NS, CLI_NS):
            run(["ip", "netns", "add", ns])
        # visitor <-> server, and server <-> client (the client dials out).
        run(["ip", "link", "add", "v-vis", "type", "veth", "peer", "name", "v-srv"])
        run(["ip", "link", "set", "v-vis", "netns", VIS_NS])
        run(["ip", "link", "set", "v-srv", "netns", SRV_NS])
        run(["ip", "link", "add", "v-cli", "type", "veth", "peer", "name", "v-srv2"])
        run(["ip", "link", "set", "v-srv2", "netns", SRV_NS])
        run(["ip", "link", "set", "v-cli", "netns", CLI_NS])

        self._ip("addr", "add", f"{VIS_IP}/24", "dev", "v-vis", ns=VIS_NS)
        self._ip("addr", "add", f"{SRV_VIS_IP}/24", "dev", "v-srv", ns=SRV_NS)
        self._ip("addr", "add", f"{SRV_CLI_IP}/24", "dev", "v-srv2", ns=SRV_NS)
        self._ip("addr", "add", f"{CLI_IP}/24", "dev", "v-cli", ns=CLI_NS)

        for ns in (VIS_NS, SRV_NS, CLI_NS):
            self._ip("link", "set", "lo", "up", ns=ns)
        self._ip("link", "set", "v-vis", "up", "mtu", str(self.link_mtu), ns=VIS_NS)
        self._ip("link", "set", "v-srv", "up", "mtu", str(self.link_mtu), ns=SRV_NS)
        self._ip("link", "set", "v-srv2", "up", "mtu", str(self.link_mtu), ns=SRV_NS)
        self._ip("link", "set", "v-cli", "up", "mtu", str(self.link_mtu), ns=CLI_NS)
        self._ip("route", "add", "default", "via", SRV_VIS_IP, ns=VIS_NS)

        # The TUN devices exist before the daemons start: they attach to the
        # name and refuse to create a device themselves.
        self._ip("tuntap", "add", "dev", TUN_SRV, "mode", "tun", ns=SRV_NS)
        self._ip("link", "set", TUN_SRV, "up", "mtu", str(self.tun_mtu), ns=SRV_NS)
        self._ip("route", "add", f"{PUBLIC_IP}/32", "dev", TUN_SRV, ns=SRV_NS)

        self._ip("tuntap", "add", "dev", TUN_CLI, "mode", "tun", ns=CLI_NS)
        self._ip("link", "set", TUN_CLI, "up", "mtu", str(self.tun_mtu), ns=CLI_NS)
        self._ip("addr", "add", f"{PUBLIC_IP}/32", "dev", TUN_CLI, ns=CLI_NS)
        # Source policy, not a destination route: every reply from the owned
        # address goes back into the tunnel, whatever it is addressed to. The
        # rule's default preference (32765) stays ahead of main's (32766), so
        # only the owned address is diverted.
        self._ip(
            "rule", "add", "from", PUBLIC_IP, "lookup", str(ROUTE_TABLE), ns=CLI_NS
        )
        self._ip(
            "route",
            "add",
            "default",
            "dev",
            TUN_CLI,
            "table",
            str(ROUTE_TABLE),
            ns=CLI_NS,
        )
        self._ip("route", "add", "default", "via", SRV_CLI_IP, ns=CLI_NS)

        # Forwarding, and no reverse-path filtering: the tunnel injects packets
        # whose source is the visitor's, which a strict rp_filter would drop.
        for ns in (VIS_NS, SRV_NS, CLI_NS):
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.ip_forward=1"])
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.conf.all.rp_filter=0"])
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.conf.default.rp_filter=0"])
        self.ns_run(SRV_NS, ["sysctl", "-qw", f"net.ipv4.conf.{TUN_SRV}.rp_filter=0"])
        self.ns_run(CLI_NS, ["sysctl", "-qw", f"net.ipv4.conf.{TUN_CLI}.rp_filter=0"])

    def down(self) -> None:
        for ns in (VIS_NS, SRV_NS, CLI_NS):
            pids = run(["ip", "netns", "pids", ns], check=False).stdout.split()
            for pid in pids:
                with contextlib.suppress(OSError, ValueError):
                    os.kill(int(pid), signal.SIGKILL)
            run(["ip", "netns", "del", ns], check=False)
        time.sleep(0.2)
        left = [ns for ns in (VIS_NS, SRV_NS, CLI_NS) if ns in self._list()]
        if left:
            raise RuntimeError(f"namespaces survived teardown: {left}")

    def set_txqueuelen(self, length: int) -> None:
        """Set both TUN devices' queue length — the arm's stated setting."""
        for ns, dev in ((SRV_NS, TUN_SRV), (CLI_NS, TUN_CLI)):
            self._ip("link", "set", dev, "txqueuelen", str(length), ns=ns)

    @staticmethod
    def _list() -> set:
        return {
            line.split()[0]
            for line in run(["ip", "netns", "list"], check=False).stdout.splitlines()
            if line.strip()
        }

    # --- observation -------------------------------------------------------
    def wait_target(self, host: str, port: int, timeout: float = 30.0) -> bool:
        """Can a *visitor* connect to `host:port`? The readiness verdict.

        Nothing on the host can answer this: the path under test starts inside
        the visitor's namespace, so the probe runs there. That also makes it the
        honest readiness check — the arm is ready when the workload's own first
        packet would succeed, not when a process is up.
        """
        code = (
            "import socket, sys;"
            "s = socket.create_connection((sys.argv[1], int(sys.argv[2])), 0.5);"
            "s.close()"
        )
        end = time.time() + timeout
        while time.time() < end:
            r = run(
                self.ns_argv(VIS_NS, [sys.executable, "-c", code, host, str(port)]),
                timeout=5,
                check=False,
            )
            if r.returncode == 0:
                return True
            time.sleep(0.2)
        return False

    def dev(self) -> dict:
        """`/proc/net/dev` of every namespace, keyed `ns/iface`.

        The counters are the run's wire evidence: the veth a client dials on is
        the tunnel's real cost (both directions, the carrier's own headers
        included) and the TUN devices are the packets the L3 path carries.
        """
        out: dict = {}
        for ns in (VIS_NS, SRV_NS, CLI_NS):
            text = run(self.ns_argv(ns, ["cat", "/proc/net/dev"]), check=False).stdout
            for line in text.splitlines():
                if ":" not in line:
                    continue
                name, rest = line.split(":", 1)
                f = rest.split()
                if len(f) < DEV_COLUMNS:
                    continue
                out[f"{ns}/{name.strip()}"] = {
                    "rx_bytes": int(f[0]),
                    "rx_packets": int(f[1]),
                    "rx_dropped": int(f[3]),
                    "tx_bytes": int(f[8]),
                    "tx_packets": int(f[9]),
                    "tx_dropped": int(f[11]),
                }
        return out

    def tcp_mib(self) -> dict:
        """The namespaces' own TCP counters, per namespace.

        The path can shed packets without the sender's protocol reporting it:
        measured on this topology, one L3 bulk run had the visitor's TCP
        retransmit ~98k segments while iperf3's `retransmits` field read 0, and
        the wire carried 15 % more bytes than the payload that arrived. A
        comparison whose numbers are byte ratios cannot afford to be blind to
        that, so the retransmission and segment counters are sampled from
        `/proc/net/snmp` beside the byte counters, in the namespaces that own
        the flows. `/proc/net/dev`'s dropped columns are the other half of the
        same evidence: they say *where* the packet was lost.
        """
        out: dict = {}
        for short, ns in (("visitor", VIS_NS), ("server", SRV_NS), ("client", CLI_NS)):
            text = run(self.ns_argv(ns, ["cat", "/proc/net/snmp"]), check=False).stdout
            for line in text.splitlines():
                if not line.startswith("Tcp: ") or line.startswith("Tcp: Rto"):
                    continue
                f = line.split()[1:]
                # Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens
                #      PassiveOpens AttemptFails EstabResets CurrEstab InSegs
                #      OutSegs RetransSegs InErrs OutRsts InCsumErrors
                if len(f) >= TCP_MIB_COLUMNS:
                    out[short] = {
                        "in_segs": int(f[9]),
                        "out_segs": int(f[10]),
                        "retrans_segs": int(f[11]),
                        "in_errs": int(f[12]),
                    }
        return out

    def established(self, port: int) -> dict:
        """Established sockets *on a service port*, per namespace.

        This is the architectural contrast as a number: with L4 the server owns
        one accepted socket per visitor on the exposed port, with L3 it owns
        none — the client's namespace does. The tunnel's own connections are
        excluded by the port filter, which is the point (they exist in both).
        """
        out: dict = {}
        for short, ns in (("server", SRV_NS), ("client", CLI_NS)):
            text = run(
                self.ns_argv(
                    ns, ["ss", "-Htn", "state", "established", f"sport = :{port}"]
                ),
                check=False,
            ).stdout
            out[short] = len([ln for ln in text.splitlines() if ln.strip()])
        return out


#: The kernel's default queue length for a device. Every arm states it
#: explicitly, because the L3 arms' is a measured lever and a leftover from a
#: previous arm would be a silent second variable.
DEFAULT_TXQUEUELEN = 1000


@dataclass(frozen=True)
class Arm:
    """One architecture (or the control) and the address its visitor dials.

    `txqueuelen` is the TUN devices' queue length for this arm. The L3 arms are
    the only ones that can shed a packet at the kernel/userspace boundary (the
    server's and client's TUN queues), so how deep those queues are decides
    whether the arm measures the architecture or the default queue: with the
    kernel's 1000, one 8-stream bulk run dropped 98k packets and the visitor's
    TCP retransmitted 108k segments to recover. The deeper-queue arm is the same
    architecture with that one operator setting changed.
    """

    name: str
    mode: str  # control | l4 | l4-direct | l3 | l3-deep
    host: str
    txqueuelen: int = DEFAULT_TXQUEUELEN

    @property
    def client_flag(self) -> str:
        return "--transparent" if self.mode.startswith("l3") else "--client"

    @property
    def data_mode(self) -> str:
        return "direct" if self.mode in ("l3", "l3-deep", "l4-direct") else "multiplex"


ALL_ARMS = [
    Arm("control", "control", CONTROL_IP),
    Arm("l4", "l4", SRV_VIS_IP),
    Arm("l4-direct", "l4-direct", SRV_VIS_IP),
    Arm("l3", "l3", PUBLIC_IP),
    Arm("l3-deep", "l3-deep", PUBLIC_IP, txqueuelen=DEEP_TXQUEUELEN),
]


def arms_for(wanted: str) -> list:
    if not wanted:
        return list(ALL_ARMS)
    known = {a.name: a for a in ALL_ARMS}
    names = [a.strip() for a in wanted.split(",") if a.strip()]
    unknown = [n for n in names if n not in known]
    if unknown:
        raise SystemExit(f"unknown arm(s): {unknown}; known: {list(known)}")
    return [known[n] for n in names]


# --- configuration ----------------------------------------------------------
def write_configs(work: Path, arm: Arm) -> dict:
    """The server and client tomls for one arm, as the operator would write them.

    The two modes share the port numbers and the backend address *inside the
    client namespace*: an L4 client forwards to `127.0.0.1:<port>` there, an L3
    claim carries `10.99.0.1:<port>` to a kernel that delivers it locally. The
    only difference in the visitor's view is which address it dials.
    """
    d = work / f"arm-{arm.name}"
    d.mkdir(parents=True, exist_ok=True)
    ports = f'["{ECHO_PORT}-{IPERF_PORT}"]'
    if arm.mode.startswith("l3"):
        server = f"""[server]
default_token = "bench"
allow_ports = {ports}

[server.control]
bind_addr = "{SRV_VIS_IP}:{CONTROL_PORT}"

[server.transparent]
tun = "{TUN_SRV}"
"""
        client = f"""[transparent]
default_token = "bench"
tun = "{TUN_CLI}"

[transparent.control]
default_remote_addr = "{SRV_VIS_IP}:{CONTROL_PORT}"

[transparent.claims.echo]
remote_bind_addr = "{PUBLIC_IP}:{ECHO_PORT}"

[transparent.claims.iperf]
remote_bind_addr = "{PUBLIC_IP}:{IPERF_PORT}"
"""
    else:
        server = f"""[server]
default_token = "bench"
allow_ports = {ports}

[server.control]
bind_addr = "{SRV_VIS_IP}:{CONTROL_PORT}"
"""
        client = f"""[client]
default_token = "bench"

[client.control]
default_remote_addr = "{SRV_VIS_IP}:{CONTROL_PORT}"

[client.data]
default_mode = "{arm.data_mode}"

[client.services.echo]
local_addr = "127.0.0.1:{ECHO_PORT}"
remote_bind_addr = "{SRV_VIS_IP}:{ECHO_PORT}"

[client.services.iperf]
local_addr = "127.0.0.1:{IPERF_PORT}"
remote_bind_addr = "{SRV_VIS_IP}:{IPERF_PORT}"
"""
    (d / "server.toml").write_text(server)
    (d / "client.toml").write_text(client)
    return {"server": d / "server.toml", "client": d / "client.toml"}


# --- processes --------------------------------------------------------------
class Procs:
    """The daemons of one arm, with their logs and their pids."""

    def __init__(self, work: Path, arm: Arm):
        self.work = work / f"arm-{arm.name}"
        self.work.mkdir(parents=True, exist_ok=True)
        self.procs: dict = {}
        self.logs: dict = {}
        self.config: dict = {}

    def spawn(self, role: str, argv: list) -> None:
        path = self.work / f"{role}.log"
        fh = path.open("ab")
        self.procs[role] = subprocess.Popen(argv, stdout=fh, stderr=fh)
        self.logs[role] = path

    def pids(self) -> dict:
        return {role: p.pid for role, p in self.procs.items() if p.poll() is None}

    def tail(self, lines: int = 12) -> dict:
        out = {}
        for role, path in self.logs.items():
            with contextlib.suppress(OSError):
                out[role] = "\n".join(path.read_text().splitlines()[-lines:])[:2000]
        return out

    def stop(self) -> None:
        for p in self.procs.values():
            with contextlib.suppress(OSError):
                p.kill()
        for p in self.procs.values():
            with contextlib.suppress(Exception):
                p.wait(timeout=5)
        # A killed daemon can hold a port briefly; the next L4 arm must be able
        # to bind it (EADDRINUSE would look like a tool failure).
        time.sleep(0.3)


def cpu_ticks(pids: dict) -> dict:
    out = {}
    for role, pid in pids.items():
        with contextlib.suppress(OSError, ValueError, IndexError):
            fields = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()
            out[role] = int(fields[11]) + int(fields[12])
    return out


def snapshot(topo: Topology, pids: dict, peers_of, with_sockets: bool = True) -> dict:
    return {
        "t": time.time(),
        "cpu": cpu_ticks(pids),
        "dev": topo.dev(),
        "tcp": topo.tcp_mib(),
        "established": topo.established(ECHO_PORT) if with_sockets else {},
        "peer_lines": peers_of(),
    }


def delta(before: dict, after: dict) -> dict:
    cpu = {
        role: round((after["cpu"].get(role, 0) - ticks) / CLK_TCK, 3)
        for role, ticks in before["cpu"].items()
    }
    wire = {}
    for key, a in after["dev"].items():
        b = before["dev"].get(key)
        if not b:
            continue
        wire[key] = {k: a[k] - b[k] for k in a}
    tcp = {
        ns: {k: v - before["tcp"].get(ns, {}).get(k, 0) for k, v in counters.items()}
        for ns, counters in after["tcp"].items()
    }
    drops = {
        key: {"rx_dropped": a["rx_dropped"], "tx_dropped": a["tx_dropped"]}
        for key, a in wire.items()
        if a["rx_dropped"] or a["tx_dropped"]
    }
    return {
        "elapsed_s": round(after["t"] - before["t"], 3),
        "cpu_s": cpu,
        "cpu_s_total": round(sum(cpu.values()), 3),
        "wire": wire,
        "drops": drops,
        "tcp": tcp,
        "established": after["established"],
        "peers": sorted(set(after["peer_lines"]) - set(before["peer_lines"])),
    }


class PeerLog:
    """The echo backend's log, read as new `PEER ip:port` lines per window."""

    def __init__(self, path: Path):
        self.path = path
        self.seen: set = set()

    def new(self) -> list:
        with contextlib.suppress(OSError):
            found = {
                ln.split(" ", 1)[1].strip()
                for ln in self.path.read_text().splitlines()
                if ln.startswith("PEER ") and " " in ln
            }
            fresh = sorted(found - self.seen)
            self.seen |= found
            return fresh
        return []


# --- workloads --------------------------------------------------------------
@dataclass(frozen=True)
class Workload:
    name: str
    kind: str  # bulk | requests
    streams: int = 0
    requests: int = 0
    connections: int = 0
    size: int = 0


@dataclass
class Campaign:
    """What an arm and its workloads need, in one value.

    The Soak runner's `RunContext` idiom, for its reason: a runner that takes
    seven positional arguments is a runner whose parameters drift out of the
    measured path.
    """

    topo: Topology
    args: argparse.Namespace
    work: Path
    peers: PeerLog

    @property
    def binary(self) -> str:
        return self.args.binary


def workloads_for(args) -> list:
    return [
        Workload("bulk-P1", "bulk", streams=1),
        Workload("bulk-P8", "bulk", streams=8),
        Workload("bulk-P16", "bulk", streams=16),
        Workload(
            "small", "requests", requests=args.small_requests, size=args.small_bytes
        ),
        Workload(
            "many",
            "requests",
            requests=args.many_requests,
            connections=args.many_connections,
            size=args.small_bytes,
        ),
    ]


def start_iperf_server(camp: Campaign, tag: str) -> subprocess.Popen:
    """A one-off iperf3 server inside the client namespace.

    `-1` handles exactly one test and exits: iperf3's server is single-test by
    design, and a wedged one would turn every later sample into a timeout
    (AGENTS.md §10, "one failure must not poison the next sample"). The bind is
    `0.0.0.0` because the three arms reach the same process by three different
    addresses (the client's loopback, the claimed public address, and the
    client's veth address for the control).
    """
    log_path = camp.work / f"iperf3-server-{tag}.log"
    with log_path.open("ab") as fh:
        return subprocess.Popen(
            # The wildcard bind is deliberate and confined to the test's client
            # namespace, where this one process must answer on the loopback,
            # the claimed public address and the veth address alike.
            camp.topo.ns_argv(
                CLI_NS,
                ["iperf3", "-s", "-1", "-B", "0.0.0.0", "-p", str(IPERF_PORT)],  # noqa: S104
            ),
            stdout=fh,
            stderr=fh,
        )


def wait_listener(topo: Topology, ns: str, port: int, timeout: float = 15.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        r = topo.ns_run(ns, ["ss", "-Htln", f"sport = :{port}"], check=False)
        if r.stdout.strip():
            return True
        time.sleep(0.1)
    return False


def run_bulk(camp: Campaign, arm: Arm, wl: Workload, pids: dict) -> dict:
    """One `-P streams` iperf3 measurement, dialed at the tool's own address.

    The counters are sampled around the *client process* and nowhere else: the
    payload denominator is the whole test's bytes (iperf3 reports the omitted
    warm-up intervals too, byte counts and all), so the wire and the payload
    windows are the same window. That matters because the warm-up is not a
    fixed share of a run — on the L3 arm its first two seconds run at about a
    third of the steady rate — and a ratio whose numerator and denominator
    cover different windows describes neither. The reported *rate* still comes
    from the post-warm-up window, which is the Soak model's convention; a
    bytes-per-byte ratio does not care what the rate was.
    """
    srv = start_iperf_server(camp, f"{arm.name}-P{wl.streams}")
    try:
        if not wait_listener(camp.topo, CLI_NS, IPERF_PORT):
            return {"ok": False, "reason": "the iperf3 backend never listened"}
        secs = camp.args.bulk_secs
        before = snapshot(camp.topo, pids, camp.peers.new, with_sockets=False)
        result = lib.iperf_result(
            lib.IperfDial(
                IPERF_PORT,
                host=arm.host,
                argv_prefix=("ip", "netns", "exec", VIS_NS),
                omit=camp.args.bulk_omit,
            ),
            wl.streams,
            secs,
            max(secs * 2.0 + 6.0, secs + 20.0),
            camp.work / "iperf-raw" / f"{arm.name}-P{wl.streams}",
        )
        result["sample"] = delta(
            before, snapshot(camp.topo, pids, camp.peers.new, with_sockets=False)
        )
        result |= derived(result["sample"])
        return result
    finally:
        with contextlib.suppress(OSError):
            srv.kill()
        with contextlib.suppress(Exception):
            srv.wait(timeout=5)
        time.sleep(0.2)


class SocketWatch:
    """Peak established sockets on a service port, polled *during* a workload.

    Sampling once at the end would read zero on every arm — a round-trip arm
    closes its connections before it returns — which would hide exactly the
    contrast this file exists to price (the server owning one accepted socket
    per visitor on L4, none on L3). The peak is what the arm held while it was
    loaded; a poll interval of 150 ms is short against every workload here.
    """

    def __init__(self, topo: Topology, port: int, interval: float = 0.15):
        self.topo, self.port, self.interval = topo, port, interval
        self.peak = {"server": 0, "client": 0}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            counts = self.topo.established(self.port)
            for role in self.peak:
                self.peak[role] = max(self.peak[role], counts.get(role, 0))
            self._stop.wait(self.interval)

    def __enter__(self) -> Self:
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self._stop.set()
        self._thread.join(timeout=3)


def run_requests(camp: Campaign, arm: Arm, wl: Workload) -> dict:
    """Strict request/response round trips from the visitor namespace.

    `visitor.py` is the acceptance harness's probe, unchanged: one fresh
    connection with `--requests`, `--connections` of them at once otherwise.
    Output is parsed rather than re-implemented so the comparison and the
    acceptance test cannot disagree about what a round trip is.

    The timed window is the probe *process*, so its interpreter start is inside
    it; the counts are set well above the tens of milliseconds that costs (the
    measured start is recorded in the results meta as `probe_startup_s`), and
    the raw wall time is reported beside the rate rather than corrected.
    """
    log_path = camp.work / f"visitor-{arm.name}-{wl.name}.log"
    argv = camp.topo.ns_argv(
        VIS_NS,
        [
            sys.executable,
            str(VISITOR),
            "--target",
            f"{arm.host}:{ECHO_PORT}",
            "--requests",
            str(wl.requests),
            "--size",
            str(wl.size),
        ],
    )
    if wl.connections:
        argv = [*argv, "--connections", str(wl.connections)]
    t0 = time.perf_counter()
    with log_path.open("wb") as fh:
        proc = subprocess.Popen(argv, stdout=fh, stderr=fh)
        try:
            rc = proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            rc = None
    elapsed = time.perf_counter() - t0
    text = log_path.read_text(errors="replace")
    total = wl.requests * max(1, wl.connections)
    return {
        "ok": rc == 0 and "VISITOR OK" in text,
        "exit": rc,
        "round_trips": total,
        "wall_s": round(elapsed, 3),
        "rt_per_s": round(total / elapsed, 1) if elapsed > 0 else None,
        "verdict": next(
            (ln for ln in reversed(text.splitlines()) if ln.startswith("VISITOR")),
            "",
        ),
    }


def wire_of(sample: dict, iface: str = f"{CLI_NS}/v-cli") -> int:
    """One link's bytes in both directions over a sampled window.

    The link between the server's namespace and the client's is the path every
    arm's traffic crosses: the tunnel for L4 and L3, and the routed path for the
    control. Its two directions are summed because a wire costs what it carries,
    acknowledgements included — which is part of why the L3 numbers differ (it
    carries the visitor's own ACKs as packets).
    """
    w = sample.get("wire", {}).get(iface, {})
    return w.get("rx_bytes", 0) + w.get("tx_bytes", 0)


def derived(sample: dict) -> dict:
    """The cost ratios that need only counters — no iperf3 accounting.

    The denominator is what the *visitor* offered on its own link, and the
    numerator is what the server<->client link carried, both over the same
    window (the workload's own process). That is deliberate: iperf3's interval
    list is not a safe denominator. With `-O 2` it mangles the interval after
    the warm-up — a measured 1 ms interval carrying hundreds of MB, and
    sometimes a whole measured interval missing from the list — which made its
    whole-run byte total under-report by 12 % and turned a 1.00 ratio into
    1.14 on arms that had carried nothing extra. A byte ratio whose numerator
    and denominator both come from interface counters cannot be mangled that
    way. The reported *rate* still uses iperf3's own receiver window, which is
    correct in both cases.
    """
    visitor = sample["wire"].get(f"{VIS_NS}/v-vis", {}).get("tx_bytes", 0)
    if not visitor:
        return {}
    return {
        "visitor_bytes": visitor,
        "link_bytes": wire_of(sample),
        "wire_per_visitor_byte": round(wire_of(sample) / visitor, 4),
        "cpu_cores": round(sample["cpu_s_total"] / max(sample["elapsed_s"], 1e-9), 2),
        "cpu_s_per_visitor_gbit": round(sample["cpu_s_total"] / (visitor * 8 / 1e9), 4),
    }


def run_workload(camp: Campaign, arm: Arm, wl: Workload, pids: dict) -> dict:
    """One workload: sample, run, sample, and the wire/CPU in between.

    A bulk run owns its own window (see `run_bulk`) because its payload
    denominator has to cover exactly what the counters covered; a round-trip arm
    has no warm-up and one window around the probe process.
    """
    if wl.kind == "bulk":
        with SocketWatch(camp.topo, ECHO_PORT) as watch:
            result = run_bulk(camp, arm, wl, pids)
        if "sample" in result:
            # The peak seen *while* the arm ran, not the after-sample: see
            # SocketWatch.
            result["sample"]["established_peak"] = watch.peak
            result |= derived(result["sample"])
    else:
        before = snapshot(camp.topo, pids, camp.peers.new)
        with SocketWatch(camp.topo, ECHO_PORT) as watch:
            result = run_requests(camp, arm, wl)
        result["sample"] = delta(before, snapshot(camp.topo, pids, camp.peers.new))
        result["sample"]["established_peak"] = watch.peak
        result |= derived(result["sample"])
    sample = result["sample"]
    # The receiver's own window is the rate this runner quotes: iperf3's
    # interval-derived headline runs high on the arms where its interval list
    # is mangled (see `derived`), and the receiver is the side the path
    # delivers to. The interval-derived number stays in the record beside it.
    headline = (
        f"{result.get('gbps_received_own_window')} Gbit/s"
        if wl.kind == "bulk"
        else f"{result.get('rt_per_s')} round trips/s"
    )
    extra = ""
    if result.get("wire_per_visitor_byte") is not None:
        extra = (
            f"  wire/visitor x{result['wire_per_visitor_byte']}"
            f"  cpu {result['cpu_s_per_visitor_gbit']}s/Gbit-vis"
            f"  cores {result['cpu_cores']}"
        )
    loss = sample["tcp"].get("visitor", {}).get("retrans_segs", 0)
    dropped = sum(
        d["tx_dropped"] + d["rx_dropped"] for key, d in sample["drops"].items()
    )
    extra += f"  retrans {loss}  drops {dropped}"
    log(
        f"    {wl.name:<10} {'ok ' if result.get('ok') else 'FAIL'} {headline}"
        f"  cpu {sample['cpu_s_total']}s"
        f"  srv-sockets {sample['established_peak']['server']}"
        f"  cli-sockets {sample['established_peak']['client']}"
        + extra
        + (f"  peers {sample['peers']}" if sample["peers"] else "")
    )
    return result


# --- one arm ----------------------------------------------------------------
def start_arm(camp: Campaign, arm: Arm) -> tuple:
    """Start one arm's daemons (or nothing, for the control) and wait for them.

    Readiness is the visitor's own first connection, so a control arm is ready
    when the backend answers and an L3 arm is ready when the claim is live — a
    process being up is not the same claim (the client could be failing to
    register while running perfectly).
    """
    camp.topo.set_txqueuelen(arm.txqueuelen)
    procs = Procs(camp.work, arm)
    if arm.mode != "control":
        cfg = write_configs(camp.work, arm)
        procs.config = {
            "server": cfg["server"].read_text(),
            "client": cfg["client"].read_text(),
        }
        procs.spawn(
            "server",
            camp.topo.ns_argv(SRV_NS, [camp.binary, "--server", str(cfg["server"])]),
        )
        procs.spawn(
            "client",
            camp.topo.ns_argv(
                CLI_NS, [camp.binary, arm.client_flag, str(cfg["client"])]
            ),
        )
    ready = camp.topo.wait_target(arm.host, ECHO_PORT, timeout=30.0)
    return procs, ready


def arm_round(camp: Campaign, arm: Arm, rnd: int) -> dict:
    """One arm's turn: every workload, once, with the daemons' cost around each."""
    log(f"  [{arm.name}] round {rnd} — {arm.mode}, visitor dials {arm.host}")
    procs, ready = start_arm(camp, arm)
    rec = {
        "arm": arm.name,
        "mode": arm.mode,
        "txqueuelen": arm.txqueuelen,
        "round": rnd,
        "workloads": {},
        "config": procs.config,
    }
    try:
        if not ready:
            rec["error"] = f"not ready: no visitor could reach {arm.host}:{ECHO_PORT}"
            rec["logs"] = procs.tail()
            log(f"    FAIL {rec['error']}")
            return rec
        pids = procs.pids()
        for wl in workloads_for(camp.args):
            rec["workloads"][wl.name] = run_workload(camp, arm, wl, pids)
        rec["logs"] = procs.tail(lines=4)
    finally:
        procs.stop()
        if arm.mode in ("l4", "l4-direct"):
            # The server's listeners must be gone before the next L4 round
            # binds them.
            end = time.time() + 5
            while (
                time.time() < end
                and camp.topo.ns_run(
                    SRV_NS, ["ss", "-Htln", f"sport = :{ECHO_PORT}"], check=False
                ).stdout.strip()
            ):
                time.sleep(0.1)
    return rec


# --- results ----------------------------------------------------------------
#: Written into the results file *before* the campaign, and not edited after it:
#: a criterion that moves once the numbers are known is not a criterion
#: (AGENTS.md §10, and the header-compression question that followed it).
CRITERIA = [
    (
        "Every probe dials the address the tool exposes; the control arm dials "
        "the backend with no tool in the path, so each number has its ceiling "
        "beside it."
    ),
    (
        "The arms are interleaved inside one campaign (order rotates per round); "
        "a difference inside the observed spread is not a difference."
    ),
    (
        "One variable per pair: the backend process, the port numbers, the "
        "visitor probe and the release binary are identical across arms; L3 and "
        "L4 differ in the mode and in the address the visitor dials."
    ),
    (
        "The L3 arm is validated before its numbers are read: the same probe "
        "must reproduce the acceptance harness's ballpark on this topology "
        "(bulk ~2 Gbit/s at a 1400-byte TUN MTU, ~10k single-flow and ~22k "
        "multi-flow round trips/s)."
    ),
    (
        "Costs are reported per carried byte and per carried packet as well as "
        "per second, and the build is named (release)."
    ),
    (
        "Loss is its own evidence, never inferred from a throughput number: the "
        "namespaces' TCP segment counters and every interface's dropped columns "
        "are sampled per workload. iperf3's own `retransmits` field is recorded "
        "but not trusted alone — measured before this campaign, it read 0 on a "
        "run whose visitor namespace retransmitted 98k segments."
    ),
    (
        "The L3 arms are measured at the kernel's default TUN queue (1000) and "
        "at a deeper one (10000), because the architecture's bulk ceiling is "
        "queue-limited: a comparison that reported one of them would be "
        "reporting that queue, not the architecture."
    ),
]

#: The falsifiable expectations, written before the campaign and kept beside
#: the criteria. A prediction that the numbers refute is a result; one that is
#: written afterwards is a story.
PREDICTIONS = [
    (
        "P1 — L4 terminates the visitor's TCP and re-sends the bytes over its "
        "own reliable tunnel, so it cannot shed packets the way L3's shared TUN "
        "queue can: L4's bulk throughput leads at the default L3 queue."
    ),
    (
        "P2 — L3's wire cost per payload byte is higher than L4's, because it "
        "carries the visitor's headers and ACKs as packets; the gap is largest "
        "on the small-packet arms."
    ),
    (
        "P3 — the server holds one accepted socket per visitor on L4 and none on "
        "L3, which is the capability difference the architecture is chosen for."
    ),
    (
        "P4 — with a deeper TUN queue, L3's bulk throughput rises and its drops "
        "fall to zero; what remains is per-packet cost, not loss."
    ),
    (
        "P5 — on the many-flow round-trip arm, L3 and L4 land close together "
        "with both bounded by the probe's own ceiling."
    ),
]


def summarize(results: dict) -> str:
    """The per-arm table, over the non-warm-up rounds, as min/median/max."""
    lines = []
    rows: dict = {}
    warm = results["meta"]["instrument"]["warmup_rounds"]
    for rec in results["records"]:
        if rec["round"] < warm or "error" in rec:
            continue
        for name, wl in rec["workloads"].items():
            rows.setdefault((rec["arm"], name), []).append(wl)
    for (arm, name), samples in rows.items():
        if name.startswith("bulk"):
            vals = [s.get("gbps_received_own_window") for s in samples if s.get("ok")]
            unit = "Gbit/s"
        else:
            vals = [s.get("rt_per_s") for s in samples if s.get("ok")]
            unit = "rt/s"
        vals = [v for v in vals if v is not None]
        cpu = [s["sample"]["cpu_s_total"] for s in samples]
        srv_sock = [s["sample"]["established_peak"]["server"] for s in samples]
        cli_sock = [s["sample"]["established_peak"]["client"] for s in samples]
        if not vals:
            lines.append(f"{arm:<10} {name:<8} no ok sample ({len(samples)} runs)")
            continue
        lo, hi = min(vals), max(vals)
        mid = sorted(vals)[len(vals) // 2]
        spread = (hi - lo) / mid * 100 if mid else 0.0
        loss = ""
        mib = [
            s["sample"]["tcp"].get("visitor", {}).get("retrans_segs", 0)
            for s in samples
        ]
        drops = [
            sum(
                d["tx_dropped"] + d["rx_dropped"] for d in s["sample"]["drops"].values()
            )
            for s in samples
        ]
        if any(mib) or any(drops):
            loss = f"  retrans {min(mib)}-{max(mib)}  drops {min(drops)}-{max(drops)}"
        lines.append(
            f"{arm:<10} {name:<8} {mid:>10.1f} {unit:<7} [{lo:.1f}-{hi:.1f}] "
            f"({spread:.1f}% spread)  cpu {min(cpu):.2f}-{max(cpu):.2f}s{loss}  "
            f"sockets srv {max(srv_sock)} / cli {max(cli_sock)}  n={len(vals)}"
        )
    return "\n".join(lines)


def parse_args(argv=None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "molehill"))
    ap.add_argument("--rounds", type=int, default=4)
    ap.add_argument("--warmup-rounds", type=int, default=1)
    ap.add_argument(
        "--arms",
        default="",
        help="comma-separated subset of control,l4,l4-direct,l3 (default: all)",
    )
    ap.add_argument("--bulk-secs", type=int, default=6)
    ap.add_argument("--bulk-omit", type=int, default=2)
    ap.add_argument("--small-requests", type=int, default=30000)
    ap.add_argument("--small-bytes", type=int, default=64)
    ap.add_argument("--many-connections", type=int, default=16)
    ap.add_argument("--many-requests", type=int, default=3000)
    ap.add_argument("--tun-mtu", type=int, default=1400)
    ap.add_argument("--link-mtu", type=int, default=1500)
    ap.add_argument("--work", default="")
    ap.add_argument("--out", default="")
    return ap.parse_args(argv)


def check_environment(args) -> int:
    """The preconditions, checked before a namespace is created."""
    if os.geteuid() != 0:
        print(
            "this runner needs root: namespaces, TUN devices and routes",
            file=sys.stderr,
        )
        return 77
    if not Path("/dev/net/tun").exists():
        print("/dev/net/tun is missing", file=sys.stderr)
        return 1
    for tool in ("ip", "ss", "iperf3"):
        if not shutil.which(tool):
            print(f"missing required tool: {tool}", file=sys.stderr)
            return 1
    if not os.access(args.binary, os.X_OK):
        print(f"no executable at {args.binary} — build it first", file=sys.stderr)
        return 1
    return 0


def probe_startup(topo: Topology, reps: int = 3) -> float:
    """The median cost of starting the visitor probe inside its namespace.

    The round-trip arms time the probe as a process, so its interpreter start is
    inside the measured window. It is measured and recorded rather than
    subtracted: at this campaign's request counts it is under a percent, and a
    corrected number whose correction is invisible is harder to audit than the
    raw one beside its own overhead.
    """
    times = []
    for _ in range(reps):
        t0 = time.perf_counter()
        run(
            topo.ns_argv(VIS_NS, [sys.executable, "-c", "pass"]),
            timeout=20,
            check=False,
        )
        times.append(time.perf_counter() - t0)
    return round(sorted(times)[len(times) // 2], 4)


def build_meta(args, arms: list, work: Path) -> dict:
    rev, clean = lib.git_revision()
    version = run([args.binary, "--version"], check=False).stdout.strip()
    return {
        "runner": "benches/scripts/l3/compare.py",
        "revision": rev,
        "tree_clean": clean,
        "binary": {
            "path": args.binary,
            "version": version,
            "fingerprint": lib.binary_fingerprint(args.binary),
        },
        "host": lib.host_identity(),
        "instrument": {
            "rounds": args.rounds,
            "warmup_rounds": args.warmup_rounds,
            "arms": [a.name for a in arms],
            "bulk_streams": [1, 8, 16],
            "bulk_secs": args.bulk_secs,
            "bulk_omit": args.bulk_omit,
            "small_requests": args.small_requests,
            "small_bytes": args.small_bytes,
            "many_connections": args.many_connections,
            "many_requests": args.many_requests,
            "tun_mtu": args.tun_mtu,
            "link_mtu": args.link_mtu,
            "echo_port": ECHO_PORT,
            "iperf_port": IPERF_PORT,
            "control_port": CONTROL_PORT,
            "probe_startup_s": None,
        },
        "criteria": CRITERIA,
        "predictions": PREDICTIONS,
        "work_dir": str(work),
        "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def run_campaign(camp: Campaign, results: dict, arms: list, out: Path) -> None:
    """The interleaved rounds: every arm takes every turn, order rotating."""
    started = time.time()
    topo = camp.topo
    topo.up()
    log("topology up: three namespaces, two TUN devices, routes and sysctls")
    results["meta"]["instrument"]["probe_startup_s"] = probe_startup(topo)
    echo_log = camp.peers.path
    with echo_log.open("ab") as fh:
        backend = subprocess.Popen(
            topo.ns_argv(CLI_NS, [sys.executable, "-c", ECHO_SRC, str(ECHO_PORT)]),
            stdout=fh,
            stderr=fh,
        )
    try:
        if not wait_listener(topo, CLI_NS, ECHO_PORT):
            raise RuntimeError(f"the echo backend never listened on {ECHO_PORT}")
        log(
            f"echo backend ready in {CLI_NS} on 0.0.0.0:{ECHO_PORT} (pid {backend.pid})"
        )
        for rnd in range(camp.args.rounds):
            order = arms[rnd % len(arms) :] + arms[: rnd % len(arms)]
            for arm in order:
                results["records"].append(arm_round(camp, arm, rnd))
                results["meta"]["elapsed_s"] = round(time.time() - started, 1)
                out.write_text(json.dumps(results, indent=1) + "\n")
    finally:
        with contextlib.suppress(OSError):
            backend.kill()
        with contextlib.suppress(Exception):
            backend.wait(timeout=5)
        with contextlib.suppress(Exception):
            topo.down()
        results["meta"]["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        results["meta"]["elapsed_s"] = round(time.time() - started, 1)


def main() -> int:
    args = parse_args()
    args.binary = str(Path(args.binary).resolve())
    status = check_environment(args)
    if status:
        return status

    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    work = Path(args.work or Path.home() / "tmp" / f"l3-vs-l4-{stamp}")
    out = Path(args.out or Path.home() / "tmp" / f"l3-vs-l4-{stamp}.json")
    work.mkdir(parents=True, exist_ok=True)
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        print(f"refusing to overwrite {out}", file=sys.stderr)
        return 2

    arms = arms_for(args.arms)
    results = {"meta": build_meta(args, arms, work), "records": []}
    log(f"binary : {args.binary} ({results['meta']['binary']['version']})")
    rev, clean = results["meta"]["revision"], results["meta"]["tree_clean"]
    log(f"rev    : {rev} (clean={clean})")
    log(f"work   : {work}")
    log(f"out    : {out}")
    log(f"arms   : {', '.join(a.name for a in arms)}")

    camp = Campaign(
        topo=Topology(args.tun_mtu, args.link_mtu),
        args=args,
        work=work,
        peers=PeerLog(work / "echo-backend.log"),
    )
    run_campaign(camp, results, arms, out)

    log("\n=== per arm (non-warm-up rounds; median [min-max]) ===")
    log(summarize(results))
    log(f"\nresults: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
