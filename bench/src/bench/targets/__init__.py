# pyright: strict
"""The adapter registry: one dispatch for every tool the model can measure."""

from __future__ import annotations

from pathlib import Path

from bench import model
from bench.paths import DEFAULT_PEER_DIR
from bench.targets.base import ToolAdapter
from bench.targets.control import ControlAdapter
from bench.targets.molehill import MolehillAdapter
from bench.targets.peers import PEERS

_MOLEHILL: ToolAdapter = MolehillAdapter()
_CONTROL: ToolAdapter = ControlAdapter()
_PEERS: dict[str, ToolAdapter] = dict(PEERS)

#: Everything the model can start, by the name the arm dispatch uses.
ADAPTERS: dict[str, ToolAdapter] = {
    "molehill": _MOLEHILL,
    "control": _CONTROL,
    **_PEERS,
}


def adapter_for(arm: model.Arm) -> ToolAdapter:
    """The adapter an arm runs through, or a refusal naming the known tools."""
    if arm.kind == "control":
        name = "control"
    elif arm.kind == "peer":
        name = arm.tool
    else:
        name = "molehill"
    try:
        return ADAPTERS[name]
    except KeyError:
        known = ", ".join(sorted(ADAPTERS))
        raise SystemExit(
            f"no adapter for arm {arm.id!r} (kind {arm.kind!r}, tool {name!r}); "
            f"known: {known}"
        ) from None


def peer_version(tool: str, peer_dir: Path = DEFAULT_PEER_DIR) -> str:
    """One peer tool's resolved version, through its own adapter."""
    adapter = PEERS.get(tool)
    if adapter is None:
        return ""
    primary = adapter.binaries(None, peer_dir).primary
    if primary is None:
        return ""
    return adapter.describe_version(primary)
