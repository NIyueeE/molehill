"""The peer registry: every reference tool the model can measure."""

from __future__ import annotations

from bench.targets.peers.base import PeerAdapter
from bench.targets.peers.frp import FrpAdapter
from bench.targets.peers.nps import NpsAdapter
from bench.targets.peers.rathole import RatholeAdapter

#: The adapters `model.Arm.tool` names. This lookup is the only wiring a peer
#: arm needs; `targets.adapter_for` dispatches through it.
PEERS: dict[str, PeerAdapter] = {
    "frp": FrpAdapter(),
    "rathole": RatholeAdapter(),
    "nps": NpsAdapter(),
}
