# pyright: strict
"""The tool-adapter contract: everything the runner needs to start a subject.

The runner knows one topology and one measurement loop; each tool under test
supplies the binaries, the launch argvs and the endpoint a visitor dials. A
peer adapts to the same interface as molehill, so a new reference tool is a new
adapter plus a registry row — never a branch in the runner.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol

from bench import model, topology


@dataclass(frozen=True)
class BinarySet:
    """One tool's role -> executable; empty for an arm with no binary."""

    roles: Mapping[str, Path]

    @property
    def primary(self) -> Path | None:
        """The executable that names the tool's version, if it has one."""
        return next(iter(self.roles.values()), None)


@dataclass(frozen=True)
class Ports:
    """The one port band every deployment exposes."""

    control: int
    echo: int
    iperf: int
    iperf2: int
    udp: int
    udp_sink: int

    @classmethod
    def from_topology(cls) -> Ports:
        return cls(
            control=topology.CONTROL_PORT,
            echo=topology.ECHO_PORT,
            iperf=topology.IPERF_PORT,
            iperf2=topology.IPERF2_PORT,
            udp=topology.UDP_PORT,
            udp_sink=topology.IPERF_UDP_PORT,
        )


@dataclass(frozen=True)
class Launch:
    """One process a deployment starts, under the role its log is named for."""

    role: str
    argv: tuple[str, ...]
    config: Path | None = None


@dataclass(frozen=True)
class Endpoint:
    """The address the visitor dials: the path under test."""

    host: str
    port: int


@dataclass(frozen=True)
class Deployment:
    """A tool's whole contribution to one arm turn."""

    tool: str
    launches: tuple[Launch, ...]
    binaries: BinarySet
    endpoint: Endpoint
    configs: Mapping[str, str] = field(default_factory=dict[str, str])


@dataclass(frozen=True)
class DeploymentContext:
    """What an adapter may read about the run it is being deployed into."""

    topology: topology.Topology
    work: Path
    ports: Ports
    peer_dir: Path


class ToolAdapter(Protocol):
    """A tool the model can measure, adapted to the model's one topology."""

    @property
    def name(self) -> str:
        """The name the adapter registry and the results call this tool."""
        ...

    def binaries(self, arm: model.Arm, peer_dir: Path) -> BinarySet:
        """The tool's executables for this arm."""
        ...

    def deploy(self, arm: model.Arm, ctx: DeploymentContext) -> Deployment:
        """Prepare this arm's configs and the processes that start it."""
        ...

    def describe_version(self, binary: Path) -> str:
        """The version the tool itself reports, or `""` when unreadable."""
        ...
