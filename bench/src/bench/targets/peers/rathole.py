"""rathole: one binary, TOML, a plain-TCP transport on both sides.

The transport is stated because it *is* the compared configuration: the
plain-TCP shape is what the other peers run. A UDP service is a service like
any other here (`type = "udp"`), so the `udp_sink` pair is two services on one
address — the same shape rathole's own feature request for a combined `tcp,udp`
type describes as the working alternative.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from bench import model, topology
from bench.targets.base import BinarySet, Ports
from bench.targets.peers.base import (
    LOOPBACK,
    SRV_IP,
    TOKEN,
    PeerAdapter,
    config_dir,
    forwards,
    require,
)


@dataclass(frozen=True)
class RatholeAdapter(PeerAdapter):
    tool: str = "rathole"

    def binaries(self, arm: model.Arm | None, peer_dir: Path) -> BinarySet:
        del arm
        return BinarySet(
            {"server": peer_dir / "rathole", "client": peer_dir / "rathole"}
        )

    def write(
        self,
        work: Path,
        ports: Ports,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list[str]]:
        """Write both TOMLs, one service per forwarded port and protocol."""
        directory = config_dir(work, self.tool)
        bins = require(self.tool, self.binaries(None, self.peer_dir)).roles
        server_src = (
            f'[server]\nbind_addr = "{SRV_IP}:{ports.control}"\n'
            '[server.transport]\ntype = "tcp"\n'
            + "".join(
                f"\n[server.services.{name}]\n"
                f'type = "{protocol}"\n'
                f'bind_addr = "{SRV_IP}:{port}"\n'
                f'token = "{TOKEN}"\n'
                for name, protocol, port in forwards(ports)
            )
        )
        client_src = (
            f'[client]\nremote_addr = "{SRV_IP}:{ports.control}"\n'
            '[client.transport]\ntype = "tcp"\n'
            + "".join(
                f"\n[client.services.{name}]\n"
                f'type = "{protocol}"\n'
                f'local_addr = "{LOOPBACK}:{port}"\n'
                f'token = "{TOKEN}"\n'
                for name, protocol, port in forwards(ports)
            )
        )
        server, client = directory / "server.toml", directory / "client.toml"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(
                server_ns, [str(bins["server"]), "--server", str(server)]
            ),
            "client": topology.Topology.ns_argv(
                client_ns, [str(bins["client"]), "--client", str(client)]
            ),
        }
