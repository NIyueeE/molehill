# pyright: strict
"""The control arm: no tool, no processes, the topology's own ceiling."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from bench import model, topology
from bench.targets.base import (
    BinarySet,
    Deployment,
    DeploymentContext,
    Endpoint,
)


@dataclass(frozen=True)
class ControlAdapter:
    """An arm with nothing in the path: the backend is the endpoint."""

    name: str = "control"

    def binaries(self, arm: model.Arm, peer_dir: Path) -> BinarySet:
        del arm, peer_dir
        return BinarySet({})

    def deploy(self, arm: model.Arm, ctx: DeploymentContext) -> Deployment:
        del ctx
        return Deployment(
            tool=self.name,
            launches=(),
            binaries=BinarySet({}),
            endpoint=Endpoint(arm.dial_host, topology.ECHO_PORT),
        )

    def describe_version(self, binary: Path) -> str:
        del binary
        return ""
