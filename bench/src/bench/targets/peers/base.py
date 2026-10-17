# pyright: strict
"""The reference tools: one adapter contract, one shape for every peer arm.

Every adapter satisfies the model's `ToolAdapter` through `deploy()`, and the
rest of this package only adds the tool's own configuration language:

* the tool's **server** runs in the server namespace and exposes the model's
  ports on `model.TOPO_SRV_IP`, so the visitor dials the path under test and
  never the backend;
* the tool's **client** runs in the client namespace and forwards to
  `127.0.0.1:<port>` there, so one backend, one iperf3 sink and one probe serve
  every arm;
* the **control port** is the tool's own channel between the two daemons, and
  it is forwarded nowhere;
* every configuration file is written under `work` — the caller's evidence
  directory — and every returned argv is already wrapped in
  `ip netns exec <namespace>`.

Each tool carries UDP its own way, and each adapter writes its tool's own
shape, never a TCP-only config that would quietly drop the datagram scenarios.
The `udp_sink` port is forwarded as *both* protocols: iperf3's UDP test opens a
TCP control connection to the same number it blasts datagrams at, exactly as
the L4 arms' extra `-ctrl` service does, and a datagram-only forward would fail
the ladder before its first sample.
"""

from __future__ import annotations

import contextlib
import re
import subprocess
from abc import ABC, abstractmethod
from dataclasses import dataclass, replace
from pathlib import Path

from bench import model, topology
from bench.paths import DEFAULT_PEER_DIR
from bench.targets.base import (
    BinarySet,
    Deployment,
    DeploymentContext,
    Endpoint,
    Launch,
    Ports,
)

#: The token every peer's own handshake uses. It is not a secret: it is a
#: bench-local value written into every config the way the L4 arms write
#: `default_token = "bench"`, so no peer is accidentally left open to whatever
#: else shares the namespace.
TOKEN = "bench"  # noqa: S105 — a bench-local constant, not a credential

#: The exposed address — the *server* namespace's own — and the loopback every
#: daemon sees as its own. The visitor dials the first (that is the path under
#: test); the peers' clients forward to the second inside the *client*
#: namespace, where the backend listens.
SRV_IP = model.TOPO_SRV_IP
LOOPBACK = "127.0.0.1"

#: How a refusal tells a reader to install what is missing.
FETCH_CMD = "just bench peers fetch"


def forwards(ports: Ports) -> list[tuple[str, str, int]]:
    """What every adapter forwards, as `(service name, protocol, port)`.

    One statement for all three tools, so a change cannot land in one tool's
    config and miss another's. `udp_sink` appears twice on purpose (see the
    module docstring); the control port never appears, because it is the peer's
    own channel and forwarding it would push the bench's management traffic
    through the path under test.
    """
    return [
        ("echo", "tcp", ports.echo),
        ("iperf", "tcp", ports.iperf),
        ("iperf2", "tcp", ports.iperf2),
        ("udp", "udp", ports.udp),
        ("udpsink", "udp", ports.udp_sink),
        ("udpsink_ctrl", "tcp", ports.udp_sink),
    ]


def config_dir(work: Path, tool: str) -> Path:
    """The tool's own directory under the run's work tree, created on demand."""
    directory = work / tool
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def require(tool: str, binaries: BinarySet) -> BinarySet:
    """The tool's executables, or a refusal that names how to install them."""
    missing = sorted(
        f"{role} ({path})" for role, path in binaries.roles.items() if not path.exists()
    )
    if missing:
        raise FileNotFoundError(
            f"{tool}: missing {', '.join(missing)}; run `{FETCH_CMD}`"
        )
    return binaries


@dataclass(frozen=True)
class PeerAdapter(ABC):
    """One reference tool, adapted to the model's one topology.

    `PEERS` is the registry the adapter lookup uses. `write()` embeds the
    executables' paths in the argv it returns, so an adapter knows where its
    own peer cache is: a run that keeps the cache somewhere else than the
    fetcher's default states it in `DeploymentContext.peer_dir`, and `deploy()`
    binds it onto a copy of the adapter before writing.
    """

    #: The name `PEERS` and `model.Arm.tool` call this tool.
    tool: str
    #: Where the tool's binaries are installed; the fetcher's own directory.
    peer_dir: Path = DEFAULT_PEER_DIR

    @property
    def name(self) -> str:
        return self.tool

    @abstractmethod
    def binaries(self, arm: model.Arm | None, peer_dir: Path) -> BinarySet:
        """The tool's server and client executables under `peer_dir`.

        `arm` is part of the common adapter contract; a peer's binaries do not
        depend on it, and it is accepted (and ignored) so one dispatch serves
        the subject and every reference tool.

        The layout is `fetch()`'s, so an adapter and the fetcher cannot drift
        apart; `write()` refuses when one of them is missing, because half a
        pair starts and then answers nothing.
        """
        raise NotImplementedError

    @abstractmethod
    def write(
        self,
        work: Path,
        ports: Ports,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list[str]]:
        """Write this tool's configuration under `work`; return the argvs.

        The result is keyed `"server"` and `"client"`, each argv already
        wrapped in `ip netns exec <namespace>`. The namespaces default to the
        model's only topology; they are parameters because the adapter must not
        decide where the model puts its client.
        """
        raise NotImplementedError

    def deploy(self, arm: model.Arm, ctx: DeploymentContext) -> Deployment:
        """Write the configs and wrap the argvs in the model's deployment."""
        configured = replace(self, peer_dir=ctx.peer_dir)
        argvs = configured.write(ctx.work, ctx.ports)
        launches = tuple(Launch(role, tuple(argv)) for role, argv in argvs.items())
        return Deployment(
            tool=self.tool,
            launches=launches,
            binaries=configured.binaries(arm, ctx.peer_dir),
            endpoint=Endpoint(model.TOPO_SRV_IP, topology.ECHO_PORT),
        )

    def describe_version(self, binary: Path) -> str:
        """The version the tool prints, or `""` when it cannot be read.

        The three tools disagree about where a version goes — frp prints it to
        stderr, rathole as a `Build Version:` line, nps as `Version:` — so one
        tolerant parse serves all of them. The fetcher's marker is the fallback
        for a binary that prints nothing, and an unreadable version stays an
        empty string: a guessed release number is worse than none, because it
        would be recorded beside the numbers as provenance.
        """
        out = ""
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            result = topology.run([str(binary), "--version"], timeout=15, check=False)
            out = result.stdout + result.stderr
        match = _SEMVER.search(out)
        if match:
            return match.group(0)
        for marker in (
            binary.parent / f".{self.tool}-release-version",
            binary.parent.parent / f".{self.tool}-release-version",
        ):
            with contextlib.suppress(OSError):
                return marker.read_text().strip()
        return ""


_SEMVER = re.compile(r"\d+\.\d+\.\d+")
