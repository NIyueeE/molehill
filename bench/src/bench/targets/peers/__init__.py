"""The peer tools as a package: registry, deploy contract and fetcher."""

from __future__ import annotations

from bench.targets.peers.base import PeerAdapter
from bench.targets.peers.fetch import fetch, main
from bench.targets.peers.registry import PEERS

__all__ = ["PEERS", "PeerAdapter", "fetch", "main"]
