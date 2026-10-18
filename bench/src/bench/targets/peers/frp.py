"""frp: `frps` and `frpc`, TOML, one `[[proxies]]` entry per service.

`bindAddr` is the control listener and `proxyBindAddr` the exposed one; frp
would default the latter to the former, and both are stated so the config says
which address the visitor's path starts at. frps keeps a TCP and a UDP port
manager apart, so the `udp_sink` pair may share one number. The client keeps
trying after a refused login (`loginFailExit = false`), because the bench starts
both daemons together and the server may not be listening yet.
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
class FrpAdapter(PeerAdapter):
    tool: str = "frp"

    def binaries(self, arm: model.Arm | None, peer_dir: Path) -> BinarySet:
        del arm
        return BinarySet(
            {
                "server": peer_dir / "frp" / "frps",
                "client": peer_dir / "frp" / "frpc",
            }
        )

    def write(
        self,
        work: Path,
        ports: Ports,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list[str]]:
        """Write both TOMLs, one proxy per forwarded service."""
        directory = config_dir(work, self.tool)
        bins = require(self.tool, self.binaries(None, self.peer_dir)).roles
        server_src = (
            f'bindAddr = "{SRV_IP}"\n'
            f"bindPort = {ports.control}\n"
            f'proxyBindAddr = "{SRV_IP}"\n'
            f'auth.token = "{TOKEN}"\n'
        )
        proxies = "".join(
            "[[proxies]]\n"
            f'name = "{name}"\n'
            f'type = "{protocol}"\n'
            f'localIP = "{LOOPBACK}"\n'
            f"localPort = {port}\n"
            f"remotePort = {port}\n\n"
            for name, protocol, port in forwards(ports)
        )
        client_src = (
            f'serverAddr = "{SRV_IP}"\n'
            f"serverPort = {ports.control}\n"
            f'auth.token = "{TOKEN}"\n'
            "loginFailExit = false\n\n"
            f"{proxies}"
        )
        server, client = directory / "frps.toml", directory / "frpc.toml"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(
                server_ns, [str(bins["server"]), "-c", str(server)]
            ),
            "client": topology.Topology.ns_argv(
                client_ns, [str(bins["client"]), "-c", str(client)]
            ),
        }
