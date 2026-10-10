#!/usr/bin/env python3
"""The bench model's one topology.

Every arm — L4, L3 and the control — is measured on this topology, because
"comparable" starts with "the same path". Three namespaces, two veth pairs and
two TUN devices:

    visitor ns ──v-vis/v-srv── server ns ──v-cli/v-srv2── client ns
     10.10.0.2                  10.10.0.254              10.30.0.2
                                TUN mhbsrv0              TUN mhbcli0
                                                          owns 10.99.0.1

The backend (an echoing TCP service, a datagram echo and one iperf3 server per
measurement) runs inside the *client* namespace, which is where a service behind
the NAT sits. The arms reach it differently, and that difference is the
architecture under measurement:

* **L4**: the server binds `10.10.0.254:<port>`; the client dials
  `127.0.0.1:<port>` in its own namespace. The visitor's TCP connection is
  terminated by the server and its bytes are re-sent over the tunnel.
* **L3**: the client owns `10.99.0.1` on its TUN; the visitor's packets are
  carried whole and delivered by the *client's* kernel to the same process.
* **control**: no tool; the visitor dials the client namespace's own address and
  the routed path does the work. It is the ceiling of this topology and backend.

Why namespaces at all, rather than host loopback: the counters that make a byte
ratio trustworthy (`/proc/net/dev` per interface, and the TCP MIB of the stack
that owns each flow) only exist where the paths are *separate* interfaces, and
per-arm isolation means two arms cannot share a port, a socket or a qdisc by
accident. It costs root, which `bench.py doctor` checks and states.

Nothing here configures the daemons: they attach to the devices this creates,
exactly as an operator's would (`src/transparent/check.rs` verifies the parts
they depend on).
"""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import model

#: Namespaces and devices carry an `mhb` infix: the L3 acceptance harness's
#: (`l3vis`, …) and the retired comparison instrument's (`l3cmpvis`, …) must not
#: be disturbed by a bench run, and vice versa.
VIS_NS = "mhbvis"
SRV_NS = "mhbsrv"
CLI_NS = "mhbcli"
TUN_SRV = "mhbsrv0"
TUN_CLI = "mhbcli0"
NS_ALL = (VIS_NS, SRV_NS, CLI_NS)

#: Ports inside the topology. They are fixed, not ephemeral, because every one
#: of them is named in a config the run writes and in the evidence it keeps.
CONTROL_PORT = 2400
ECHO_PORT = 2401
IPERF_PORT = 2402
IPERF2_PORT = 2403
UDP_PORT = 2404
IPERF_UDP_PORT = 2405
SERVICE_PORTS = (ECHO_PORT, IPERF_PORT, IPERF2_PORT, UDP_PORT, IPERF_UDP_PORT)

#: The `/proc/net/dev` fields per interface: name, then 16 counters.
DEV_COLUMNS = 16
#: `/proc/net/snmp`'s Tcp line: algorithm parameters, then the counters read.
TCP_MIB_COLUMNS = 13
#: Ticks per second of the CPU counters, the denominator of every cost figure.
CLK_TCK = os.sysconf("SC_CLK_TCK")

#: The legs a condition can land on, as `(namespace, device)` pairs whose egress
#: is shaped. Both ends of a leg are shaped because netem is egress-only: one
#: end alone would shape one direction.
#:
#: The condition *vocabulary* lives in `model.CONDITIONS` — it is method, and a
#: run's fingerprint must move when a condition's meaning moves. This module
#: only knows how to impose one.
LEGS: dict[str, tuple] = {
    "visitor": ((VIS_NS, "v-vis"), (SRV_NS, "v-srv")),
    "tunnel": ((SRV_NS, "v-srv2"), (CLI_NS, "v-cli")),
}
#: The interfaces the model reads its byte ratios from: the visitor's egress is
#: the offered load, the tunnel link is what the path cost.
VISITOR_IFACE = f"{VIS_NS}/v-vis"
TUNNEL_IFACE = f"{CLI_NS}/v-cli"

#: The kernel's default device queue length. Every arm states it explicitly,
#: because an L3 arm's is a measured lever and a leftover from a previous arm
#: would be a silent second variable.
DEFAULT_TXQUEUELEN = 1000
#: The deeper queue an arm may ask for: the same architecture with one operator
#: setting changed.
DEEP_TXQUEUELEN = 10000


#: How many consecutive quiet polls end a drain. Two, because one reading of a
#: path that is still draining is not a predicate.
DRAIN_QUIET_POLLS = 2
#: The queue never reaches zero (the probes leave ~1 KB in it permanently), so
#: "quiet" is a tolerance: one `lo` MTU worth of bytes.
DRAIN_TOLERANCE_B = 65536

#: `tc`'s backlog unit suffixes. A reading that assumed bytes would read "1Mb"
#: as one byte; the parser exists because the drain predicate is a reading.
BACKLOG_UNITS = {"b": 1, "Kb": 1000, "Mb": 1000**2, "Gb": 1000**3}


def _bytes_of(raw: str) -> int:
    """`"18432b"`/`"1Mb"` -> bytes, or 0 for a shape this parser does not know."""
    for suffix, factor in sorted(BACKLOG_UNITS.items(), key=lambda kv: -len(kv[0])):
        if raw.endswith(suffix):
            with contextlib.suppress(ValueError):
                return int(float(raw[: -len(suffix)]) * factor)
            return 0
    with contextlib.suppress(ValueError):
        return int(raw)
    return 0


def run(argv: list, timeout: float = 30.0, check: bool = True):
    """One short host command; raises with its own stderr in the message."""
    r = subprocess.run(
        argv, capture_output=True, text=True, timeout=timeout, check=False
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"{' '.join(argv)}: exit {r.returncode}: {r.stderr.strip()[:300]}"
        )
    return r


class TopologyError(RuntimeError):
    """The topology could not be built or torn down; the run must not continue."""


class Topology:
    """The three namespaces, their links and the two TUN devices."""

    def __init__(self, tun_mtu: int = 1400, link_mtu: int = 1500):
        self.tun_mtu = tun_mtu
        self.link_mtu = link_mtu
        #: (namespace, device) -> the MTU it had before this run changed it.
        self._mtu_saved: dict = {}

    # --- lifecycle ---------------------------------------------------------
    def up(self) -> None:
        self.down()
        for ns in NS_ALL:
            run(["ip", "netns", "add", ns])
        run(["ip", "link", "add", "v-vis", "type", "veth", "peer", "name", "v-srv"])
        run(["ip", "link", "set", "v-vis", "netns", VIS_NS])
        run(["ip", "link", "set", "v-srv", "netns", SRV_NS])
        run(["ip", "link", "add", "v-cli", "type", "veth", "peer", "name", "v-srv2"])
        run(["ip", "link", "set", "v-srv2", "netns", SRV_NS])
        run(["ip", "link", "set", "v-cli", "netns", CLI_NS])

        self._ip("addr", "add", f"{model.TOPO_VIS_IP}/24", "dev", "v-vis", ns=VIS_NS)
        self._ip("addr", "add", f"{model.TOPO_SRV_IP}/24", "dev", "v-srv", ns=SRV_NS)
        self._ip(
            "addr", "add", f"{model.TOPO_SRV_CLI_IP}/24", "dev", "v-srv2", ns=SRV_NS
        )
        self._ip("addr", "add", f"{model.TOPO_CLI_IP}/24", "dev", "v-cli", ns=CLI_NS)

        for ns in NS_ALL:
            self._ip("link", "set", "lo", "up", ns=ns)
        for ns, dev in (
            (VIS_NS, "v-vis"),
            (SRV_NS, "v-srv"),
            (SRV_NS, "v-srv2"),
            (CLI_NS, "v-cli"),
        ):
            self._ip("link", "set", dev, "up", "mtu", str(self.link_mtu), ns=ns)
        self._ip("route", "add", "default", "via", model.TOPO_SRV_IP, ns=VIS_NS)

        # The TUN devices exist before the daemons start: they attach to the
        # name and refuse to create a device themselves.
        self._ip("tuntap", "add", "dev", TUN_SRV, "mode", "tun", ns=SRV_NS)
        self._ip("link", "set", TUN_SRV, "up", "mtu", str(self.tun_mtu), ns=SRV_NS)
        self._ip(
            "route", "add", f"{model.TOPO_PUBLIC_IP}/32", "dev", TUN_SRV, ns=SRV_NS
        )

        self._ip("tuntap", "add", "dev", TUN_CLI, "mode", "tun", ns=CLI_NS)
        self._ip("link", "set", TUN_CLI, "up", "mtu", str(self.tun_mtu), ns=CLI_NS)
        self._ip("addr", "add", f"{model.TOPO_PUBLIC_IP}/32", "dev", TUN_CLI, ns=CLI_NS)
        # Source policy, not a destination route: every reply from the owned
        # address goes back into the tunnel, whatever it is addressed to. The
        # rule's default preference (32765) stays ahead of main's (32766), so
        # only the owned address is diverted.
        self._ip(
            "rule", "add", "from", model.TOPO_PUBLIC_IP, "lookup", "100", ns=CLI_NS
        )
        self._ip("route", "add", "default", "dev", TUN_CLI, "table", "100", ns=CLI_NS)
        self._ip("route", "add", "default", "via", model.TOPO_SRV_CLI_IP, ns=CLI_NS)

        # Forwarding, and no reverse-path filtering: the tunnel injects packets
        # whose source is the visitor's, which a strict rp_filter would drop.
        for ns in NS_ALL:
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.ip_forward=1"])
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.conf.all.rp_filter=0"])
            self.ns_run(ns, ["sysctl", "-qw", "net.ipv4.conf.default.rp_filter=0"])
        self.ns_run(SRV_NS, ["sysctl", "-qw", f"net.ipv4.conf.{TUN_SRV}.rp_filter=0"])
        self.ns_run(CLI_NS, ["sysctl", "-qw", f"net.ipv4.conf.{TUN_CLI}.rp_filter=0"])

    def down(self) -> None:
        failures = self.restore_mtus()
        if failures:
            raise TopologyError(f"could not restore interface MTUs: {failures}")
        for ns in NS_ALL:
            pids = run(["ip", "netns", "pids", ns], check=False).stdout.split()
            for pid in pids:
                with contextlib.suppress(OSError, ValueError):
                    os.kill(int(pid), signal.SIGKILL)
            run(["ip", "netns", "del", ns], check=False)
        time.sleep(0.2)
        left = [ns for ns in NS_ALL if ns in self._list()]
        if left:
            raise TopologyError(f"namespaces survived teardown: {left}")

    @staticmethod
    def stale() -> list:
        """Namespaces from an interrupted run, so `doctor` can name them."""
        return [ns for ns in NS_ALL if ns in Topology._list()]

    @staticmethod
    def _list() -> set:
        return {
            line.split()[0]
            for line in run(["ip", "netns", "list"], check=False).stdout.splitlines()
            if line.strip()
        }

    # --- helpers -----------------------------------------------------------
    @staticmethod
    def ns_argv(ns: str, argv: list) -> list:
        return ["ip", "netns", "exec", ns, *argv]

    def ns_run(self, ns: str, argv: list, timeout: float = 30.0, check: bool = True):
        return run(self.ns_argv(ns, argv), timeout=timeout, check=check)

    @staticmethod
    def _ip(*argv: str, ns: str | None = None) -> None:
        run(["ip", *(["-n", ns] if ns else []), *argv])

    @staticmethod
    def _ip_check(*argv: str, ns: str | None = None):
        return run(["ip", *(["-n", ns] if ns else []), *argv], check=False)

    def set_condition(self, leg: str, condition) -> None:
        """Impose one condition on a leg, *in place*, both directions.

        In place is the point: a staged run changes the path while the tool
        keeps running, so what is measured is how the tool adapts rather than
        how it starts. `tc qdisc replace` swaps the discipline without touching
        the device, and `clean` removes it.
        """
        for ns, dev in LEGS[leg]:
            if condition.netem:
                self.ns_run(
                    ns,
                    [
                        "tc",
                        "qdisc",
                        "replace",
                        "dev",
                        dev,
                        "root",
                        "netem",
                        *condition.netem,
                    ],
                )
            else:
                self.ns_run(ns, ["tc", "qdisc", "del", "dev", dev, "root"], check=False)
            if condition.mtu is not None:
                self.set_link_mtu(ns, dev, condition.mtu)

    def set_link_mtu(self, ns: str, dev: str, mtu: int) -> None:
        """Change one interface's MTU, remembering the original to restore it.

        An MTU class is an *interface* property, not a qdisc: it changes the
        path for every packet on that device during the stage. A leftover 1280
        would poison every later run on the host, so the original is recorded
        and `down()` puts it back.
        """
        if (ns, dev) not in self._mtu_saved:
            r = self.ns_run(ns, ["cat", f"/sys/class/net/{dev}/mtu"], check=False)
            with contextlib.suppress(ValueError):
                self._mtu_saved[(ns, dev)] = int(r.stdout.strip())
        self._ip("link", "set", dev, "mtu", str(mtu), ns=ns)

    def restore_mtus(self) -> list:
        """Put every MTU this run changed back, and report the failures."""
        failures = []
        for (ns, dev), mtu in self._mtu_saved.items():
            r = self._ip_check("link", "set", dev, "mtu", str(mtu), ns=ns)
            if r.returncode != 0:
                failures.append(f"{ns}/{dev}: {r.stderr.strip()[:120]}")
        self._mtu_saved.clear()
        return failures

    def backlog(self, leg: str) -> int:
        """Bytes queued in the leg's qdiscs, both ends — a reading, not a guess.

        `tc` renders the backlog with a unit suffix (`b`, `Kb`, `Mb`, `Gb`), so
        the suffix is parsed rather than assumed: a boundary that waits on a
        number it misread waits on nothing.
        """
        total = 0
        for ns, dev in LEGS[leg]:
            text = self.ns_run(
                ns, ["tc", "-s", "qdisc", "show", "dev", dev], check=False
            ).stdout
            for line in text.splitlines():
                fields = line.split()
                if "backlog" in fields:
                    total += _bytes_of(fields[fields.index("backlog") + 1])
        return total

    def drain(
        self, leg: str, budget_s: float = 60.0, tolerance_b: int = DRAIN_TOLERANCE_B
    ) -> dict:
        """Wait for a leg to go quiet before the next stage is imposed.

        A killed bulk client keeps delivering what its kernel still holds, and
        reshaping at that instant puts the old stage's drain into the new
        stage's queue — a SYN behind it costs the next stage its first seconds.
        The predicate is the queue below a tolerance *and* no socket that can
        still send, held for two consecutive polls; the budget is a safety net,
        and a wait that ends on it is recorded rather than absorbed.
        """
        started = time.time()
        quiet_polls = 0
        while time.time() - started < budget_s:
            busy = self.send_capable_sockets()
            queued = self.backlog(leg)
            if queued <= tolerance_b and not busy:
                quiet_polls += 1
                if quiet_polls >= DRAIN_QUIET_POLLS:
                    break
            else:
                quiet_polls = 0
            time.sleep(0.25)
        queued = self.backlog(leg)
        busy = self.send_capable_sockets()
        return {
            "drain_s": round(time.time() - started, 3),
            "drain_expired": bool(queued > tolerance_b or busy),
            "drain_final_backlog": queued,
            "drain_busy_sockets": busy,
        }

    def send_capable_sockets(self) -> int:
        """Sockets on a service port that can still send (the drain's other half).

        `FIN-WAIT-2`/`CLOSING` linger for minutes carrying nothing, while
        `ESTAB`/`FIN-WAIT-1`/`CLOSE-WAIT`/`SYN-SENT`/`SYN-RECV` are exactly the
        states that retransmit the tens of MB a killed client's kernel holds.
        """
        total = 0
        for ns in NS_ALL:
            for state in ("estab", "fin-wait-1", "close-wait", "syn-sent", "syn-recv"):
                r = self.ns_run(ns, ["ss", "-Htn", "state", state], check=False)
                total += len([ln for ln in r.stdout.splitlines() if ln.strip()])
        return total

    def set_txqueuelen(self, length: int) -> None:
        """Set both TUN devices' queue length — the arm's stated setting."""
        for ns, dev in ((SRV_NS, TUN_SRV), (CLI_NS, TUN_CLI)):
            self._ip("link", "set", dev, "txqueuelen", str(length), ns=ns)

    # --- observation -------------------------------------------------------
    def wait_target(self, host: str, port: int, timeout: float = 30.0) -> bool:
        """Can a *visitor* connect to `host:port`? The readiness verdict.

        Nothing on the host can answer this: the path under test starts inside
        the visitor's namespace, so the probe runs there — and that makes it the
        honest check, because the arm is ready when the workload's own first
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

        These counters are the run's wire evidence: the visitor's egress is what
        was offered, the server-to-client link is what the path cost, and the
        differences between arms are byte differences no instrument's own
        accounting can manufacture.
        """
        out: dict = {}
        for ns in NS_ALL:
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

        Loss is its own evidence: a path can shed packets without the sender's
        protocol reporting it (measured on this topology: a visitor
        retransmitting ~98k segments while iperf3's `retransmits` read 0), so
        the retransmission counters are sampled beside the bytes, and the
        interfaces' dropped columns say *where* a packet went.
        """
        out: dict = {}
        for short, ns in (("visitor", VIS_NS), ("server", SRV_NS), ("client", CLI_NS)):
            text = run(self.ns_argv(ns, ["cat", "/proc/net/snmp"]), check=False).stdout
            for line in text.splitlines():
                if not line.startswith("Tcp: ") or line.startswith("Tcp: Rto"):
                    continue
                f = line.split()[1:]
                if len(f) >= TCP_MIB_COLUMNS:
                    out[short] = {
                        "in_segs": int(f[9]),
                        "out_segs": int(f[10]),
                        "retrans_segs": int(f[11]),
                        "in_errs": int(f[12]),
                    }
        return out

    def sockets(self, port: int, kind: str = "tcp") -> dict:
        """Sockets on a service port, per namespace — the state an arm keeps.

        This is the architectural contrast as a number: with L4 the server owns
        one accepted socket per visitor on the exposed port, with L3 it owns
        none (the client's namespace does), and the tunnel's own connections are
        excluded by the port filter, which is the point — they exist either way.
        """
        out: dict = {"server": 0, "client": 0, "error": ""}
        flag = "-Htn" if kind == "tcp" else "-Hun"
        state = ["state", "established"] if kind == "tcp" else []
        for short, ns in (("server", SRV_NS), ("client", CLI_NS)):
            r = run(
                self.ns_argv(ns, ["ss", flag, *state, f"sport = :{port}"]), check=False
            )
            if r.returncode != 0:
                # A filter `ss` cannot run prints nothing and says so on stderr;
                # reading the empty stdout as "no sockets" is how an instrument
                # reports a silent zero (this one did, for a whole campaign:
                # the flags were unpacked character by character).
                out["error"] = f"ss in {ns}: {r.stderr.strip()[:200]}"
                continue
            out[short] = len([ln for ln in r.stdout.splitlines() if ln.strip()])
        return out


def service_config(arm: model.Arm, work: Path) -> dict:
    """The server and client TOML for one arm, as an operator would write them.

    Both architectures share the port numbers and the backend address *inside
    the client namespace*: an L4 client forwards to `127.0.0.1:<port>` there, an
    L3 claim carries `10.99.0.1:<port>` to a kernel that delivers it locally.
    The only difference in the visitor's view is which address it dials — and
    that is the architecture.
    """
    d = work / f"arm-{arm.id}"
    d.mkdir(parents=True, exist_ok=True)
    ports = f'["{ECHO_PORT}-{IPERF_UDP_PORT}"]'
    if arm.kind == "l3":
        server = f"""[server]
default_token = "bench"
allow_ports = {ports}

[server.control]
bind_addr = "{model.TOPO_SRV_IP}:{CONTROL_PORT}"

[server.transparent]
tun = "{TUN_SRV}"
"""
        claims = "\n".join(
            "\n".join(
                [
                    f"[transparent.claims.s{port}]",
                    f'remote_bind_addr = "{model.TOPO_PUBLIC_IP}:{port}"',
                ]
            )
            + "\n"
            for port in SERVICE_PORTS
        )
        # The data plane is stated once, on the block every claim inherits:
        # both keys are written even when they are the product's defaults,
        # because an arm must record the configuration it measured and the
        # carrier is one of the axes this model compares.
        client = f"""[transparent]
default_token = "bench"
tun = "{TUN_CLI}"

[transparent.control]
default_remote_addr = "{model.TOPO_SRV_IP}:{CONTROL_PORT}"

[transparent.data]
default_mode = "{arm.data_mode}"
default_carrier = "{arm.data_carrier}"

{claims}"""
    else:
        cap = f"\n[client.data.tcp]\ntunnels = {arm.pool_cap}\n" if arm.pool_cap else ""
        services = []
        for port in SERVICE_PORTS:
            block = [f"[client.services.s{port}]"]
            if port in (UDP_PORT, IPERF_UDP_PORT):
                block.append('protocol = "udp"')
            block.append(f'local_addr = "127.0.0.1:{port}"')
            block.append(f'remote_bind_addr = "{model.TOPO_SRV_IP}:{port}"')
            if port in (UDP_PORT, IPERF_UDP_PORT):
                block.append("udp_workers = 2")
            services.append("\n".join(block) + "\n")
        # iperf3's UDP test still opens a TCP control connection to the same
        # port, so that port carries both protocols for an L4 arm. L3 needs no
        # such companion: a claim carries whatever the visitor sends.
        services.append(
            "\n".join(
                [
                    f"[client.services.s{IPERF_UDP_PORT}-ctrl]",
                    'protocol = "tcp"',
                    f'local_addr = "127.0.0.1:{IPERF_UDP_PORT}"',
                    f'remote_bind_addr = "{model.TOPO_SRV_IP}:{IPERF_UDP_PORT}"',
                ]
            )
            + "\n"
        )
        server = f"""[server]
default_token = "bench"
allow_ports = {ports}

[server.control]
bind_addr = "{model.TOPO_SRV_IP}:{CONTROL_PORT}"
"""
        client = f"""[client]
default_token = "bench"

[client.control]
default_remote_addr = "{model.TOPO_SRV_IP}:{CONTROL_PORT}"

[client.data]
default_mode = "{arm.data_mode}"
default_carrier = "{arm.data_carrier}"
{cap}
{"".join(services)}"""
    (d / "server.toml").write_text(server)
    (d / "client.toml").write_text(client)
    return {"server": d / "server.toml", "client": d / "client.toml"}
