"""nps: `nps` and `npc`, an ini pair, and a tree of the run's own.

nps resolves `conf/` and `web/` from the directory of `os.Args[0]` (and
`/etc/nps`, when that directory exists, wins over it), so `write()`
materializes the run's tree: hard links to the two binaries, a `conf/` holding
the config, and the shipped web assets by symlink. `npc` takes its own file by
`-config`, and its ini parser keeps the spaces around `=` (measured: with them
the client dials the wrong transport and dies on a UDP write to port 0), so its
keys are written tight while `nps.conf`'s parser accepts either. The dashboard
is mandatory — `web_port = 0` leaves nps blocked on a channel — so it takes the
first free port above the band, on loopback inside the server namespace.
"""

from __future__ import annotations

import os
import shutil
from collections.abc import Mapping
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
class NpsAdapter(PeerAdapter):
    tool: str = "nps"

    def binaries(self, arm: model.Arm | None, peer_dir: Path) -> BinarySet:
        del arm
        return BinarySet(
            {
                "server": peer_dir / "nps" / "nps",
                "client": peer_dir / "nps" / "npc",
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
        """Write the run's tree, then nps.conf and npc.conf inside it."""
        directory = config_dir(work, self.tool)
        bins = require(self.tool, self.binaries(None, self.peer_dir)).roles
        _nps_tree(directory, bins)
        forwarded = forwards(ports)
        low = min(port for _, _, port in forwarded)
        high = max(port for _, _, port in forwarded)
        server_src = (
            "appname = nps\n"
            "runmode = pro\n"
            "http_proxy_ip =\n"
            "http_proxy_port =\n"
            "https_proxy_port =\n"
            "bridge_type = tcp\n"
            f"bridge_ip = {SRV_IP}\n"
            f"bridge_port = {ports.control}\n"
            f"public_vkey = {TOKEN}\n"
            "log_level = 6\n"
            f"web_host = {LOOPBACK}\n"
            f"web_username = {TOKEN}\n"
            f"web_password = {TOKEN}\n"
            f"web_ip = {LOOPBACK}\n"
            f"web_port = {high + 1}\n"
            f"allow_ports = {low}-{high}\n"
            "allow_flow_limit = false\n"
            "allow_rate_limit = false\n"
            "allow_tunnel_num_limit = false\n"
            "allow_local_proxy = false\n"
            "allow_connection_num_limit = false\n"
            "allow_multi_ip = false\n"
            "system_info_display = false\n"
            "disconnect_timeout = 60\n"
        )
        tasks = "".join(
            f"\n[{protocol}_{name}]\nmode={protocol}\n"
            f"target_addr={LOOPBACK}:{port}\n"
            f"server_port={port}\n"
            for name, protocol, port in forwarded
        )
        client_src = (
            "[common]\n"
            f"server_addr={SRV_IP}:{ports.control}\n"
            "conn_type=tcp\n"
            f"vkey={TOKEN}\n"
            "auto_reconnection=true\n"
            "crypt=false\n"
            "compress=false\n"
            "max_conn=1000\n"
            "disconnect_timeout=60\n"
            f"{tasks}"
        )
        server, client = directory / "conf" / "nps.conf", directory / "npc.conf"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(server_ns, [str(directory / "nps")]),
            "client": topology.Topology.ns_argv(
                client_ns, [str(directory / "npc"), "-config", str(client)]
            ),
        }


def _link_or_copy(src: Path, target: Path) -> None:
    """Materialize `src` at `target` by hard link, falling back to a copy.

    A hard link keeps nps's ~24 MB of binaries on one inode, and `os.Args[0]`
    still names the linked path — which is where nps looks for its `conf/`. The
    copy is the fallback for a peer cache on another filesystem: measured as
    EXDEV from `/home` to `/tmp`, which failed a whole nps test while the
    binaries' own link had a fallback and quietly worked.
    """
    try:
        os.link(src, target)
    except OSError:
        shutil.copy2(src, target)


def _nps_tree(directory: Path, bins: Mapping[str, Path]) -> None:
    """Materialize the run's nps tree: binaries, shipped files, web assets.

    Idempotent, because a work directory may be written twice (a retried arm),
    and the shipped files are only linked when they are not there: nps reads
    its registries (`clients.json`, `hosts.json`, ...) from the same directory
    and panics when they are missing.
    """
    if Path("/etc/nps").exists():
        raise RuntimeError(
            "/etc/nps exists: nps would read its conf/ and web/ from there "
            "instead of this run's tree, so the config written here would be "
            "ignored"
        )
    for source in bins.values():
        target = directory / source.name
        if not target.exists():
            _link_or_copy(source, target)
        target.chmod(0o755)
    conf = directory / "conf"
    conf.mkdir(exist_ok=True)
    shipped = bins["server"].parent / "conf"
    if not shipped.is_dir():
        raise FileNotFoundError(f"nps: {shipped} is missing; re-fetch the peers")
    for src in sorted(shipped.iterdir()):
        if src.name != "nps.conf" and not (conf / src.name).exists():
            _link_or_copy(src, conf / src.name)
    web = directory / "web"
    if not web.exists():
        web.symlink_to(bins["server"].parent / "web", target_is_directory=True)
