# pyright: strict
"""The subject under test: molehill's own server and client."""

from __future__ import annotations

import contextlib
import subprocess
from dataclasses import dataclass
from pathlib import Path

from bench import model, topology
from bench.targets.base import (
    BinarySet,
    Deployment,
    DeploymentContext,
    Endpoint,
    Launch,
)


@dataclass(frozen=True)
class MolehillAdapter:
    """Molehill's two ends, configured by the model's topology writer."""

    name: str = "molehill"

    def binaries(self, arm: model.Arm, peer_dir: Path) -> BinarySet:
        del peer_dir
        binary = Path(arm.binary)
        return BinarySet({"server": binary, "client": binary})

    def deploy(self, arm: model.Arm, ctx: DeploymentContext) -> Deployment:
        cfg = topology.service_config(arm, ctx.work)
        server_cfg, client_cfg = cfg["server"], cfg["client"]
        server = Launch(
            "server",
            tuple(
                ctx.topology.ns_argv(
                    topology.SRV_NS,
                    [arm.binary, "--server", str(server_cfg)],
                )
            ),
            config=server_cfg,
        )
        client = Launch(
            "client",
            tuple(
                ctx.topology.ns_argv(
                    topology.CLI_NS,
                    [arm.binary, arm.mode_flag, str(client_cfg)],
                )
            ),
            config=client_cfg,
        )
        return Deployment(
            tool=self.name,
            launches=(server, client),
            binaries=self.binaries(arm, ctx.peer_dir),
            endpoint=Endpoint(arm.dial_host, topology.ECHO_PORT),
            configs={
                "server": server_cfg.read_text(),
                "client": client_cfg.read_text(),
            },
        )

    def describe_version(self, binary: Path) -> str:
        line = ""
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            result = subprocess.run(
                [str(binary), "--version"],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            first = (result.stdout or result.stderr).strip().splitlines()
            line = first[0][:200] if first else ""
        return line
