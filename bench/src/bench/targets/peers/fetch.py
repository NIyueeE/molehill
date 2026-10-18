"""Install the peer reference tools: newest prebuilt GitHub release, never source.

A peer is cached together with a marker naming the release that was resolved;
the marker is the only name a tool that prints no version can be recorded by,
and `describe_version`'s fallback. A tool already at the newest release is left
alone, so a run normally needs no network.
"""

from __future__ import annotations

import argparse
import json
import platform
import shutil
import tarfile
import urllib.request
import zipfile
from pathlib import Path

from bench.paths import DEFAULT_PEER_DIR
from bench.targets.peers.registry import PEERS

#: The GitHub API and asset requests identify themselves.
UA = {
    "User-Agent": "molehill-bench-peer-fetch",
    "Accept": "application/vnd.github+json",
}

#: The vendors' architecture spellings: frp and nps use Go's `amd64`/`arm64`,
#: rathole's Rust target uses `x86_64`/`aarch64`.
GO_ARCH = {"x86_64": "amd64", "aarch64": "arm64"}
RUST_ARCH = {"x86_64": "x86_64", "aarch64": "aarch64"}


def _host_arch(tool: str) -> str:
    """The vendor's name for this host's architecture, or a refusal.

    One lookup per family, checked once per tool, so a new machine fails loudly
    for every peer instead of silently for the one whose map was not checked.
    """
    machine = platform.machine()
    table = RUST_ARCH if tool == "rathole" else GO_ARCH
    if machine not in table:
        raise SystemExit(f"unsupported arch for {tool}: {machine}")
    return table[machine]


def _http_get(url: str) -> bytes:
    """One GET of a release asset or of the GitHub API.

    The URL always comes from this file's own repo and asset constants, never
    from user input, which is what the scheme audit is asking about.
    """
    req = urllib.request.Request(url, headers=UA)  # noqa: S310 — an https URL we built
    with urllib.request.urlopen(req, timeout=180) as r:  # noqa: S310 — same
        return r.read()


def _latest_release(repo: str) -> tuple[str, list[str]]:
    """`(version without its 'v', asset names)` of a repo's latest release."""
    data = json.loads(_http_get(f"https://api.github.com/repos/{repo}/releases/latest"))
    return data["tag_name"].removeprefix("v"), [a["name"] for a in data["assets"]]


def _pick_asset(assets: list[str], patterns: list[str]) -> str:
    """The first asset whose name exactly matches one of the patterns."""
    for name in patterns:
        if name in assets:
            return name
    raise SystemExit(f"no asset matching {patterns}; available: {assets}")


def _cached(tool: str, version: str, peer_dir: Path) -> bool:
    """Is the tool's resolved latest release already installed?"""
    roles = PEERS[tool].binaries(None, peer_dir).roles
    if not all(path.exists() for path in roles.values()):
        return False
    try:
        have = (peer_dir / f".{tool}-release-version").read_text().strip()
    except OSError:
        return False
    return have == version


def _fetch_frp(peer_dir: Path, arch: str) -> str:
    """frp's tarball: a versioned directory plus a stable `frp` symlink."""
    repo = "fatedier/frp"
    version, assets = _latest_release(repo)
    if _cached("frp", version, peer_dir):
        print(f"frp: cached {version}")
        return version
    name = _pick_asset(assets, [f"frp_{version}_linux_{arch}.tar.gz"])
    print(f"frp: downloading {name}")
    tarball = peer_dir / "frp.tar.gz"
    tarball.write_bytes(
        _http_get(f"https://github.com/{repo}/releases/download/v{version}/{name}")
    )
    with tarfile.open(tarball) as t:
        # `filter="data"` refuses absolute paths, links and device nodes, so
        # the extraction is safe by construction (ruff's S202 fires only on an
        # unfiltered extract).
        t.extractall(filter="data", path=peer_dir)
    tarball.unlink()
    link = peer_dir / "frp"
    link.unlink(missing_ok=True)
    link.symlink_to(f"frp_{version}_linux_{arch}")
    (peer_dir / ".frp-release-version").write_text(version)
    return version


def _fetch_rathole(peer_dir: Path, arch: str) -> str:
    """rathole's zip: one binary at the archive root, moved into place."""
    repo = "rathole-org/rathole"
    version, assets = _latest_release(repo)
    if _cached("rathole", version, peer_dir):
        print(f"rathole: cached {version}")
        return version
    name = _pick_asset(
        assets,
        [
            f"rathole-{arch}-unknown-linux-gnu.zip",
            f"rathole-{arch}-unknown-linux-musl.zip",
        ],
    )
    print(f"rathole: downloading {name}")
    archive = peer_dir / "rathole.zip"
    archive.write_bytes(
        _http_get(f"https://github.com/{repo}/releases/download/v{version}/{name}")
    )
    unpacked = peer_dir / "rathole-zip"
    with zipfile.ZipFile(archive) as z:
        # A trusted release asset; zipfile has no tarfile-style filter.
        z.extractall(path=unpacked)  # noqa: S202 — a trusted release asset
    archive.unlink()
    candidates = list(unpacked.rglob("rathole"))
    if not candidates:
        raise SystemExit("rathole: the release archive holds no rathole binary")
    shutil.move(str(candidates[0]), peer_dir / "rathole")
    (peer_dir / "rathole").chmod(0o755)
    shutil.rmtree(unpacked, ignore_errors=True)
    (peer_dir / ".rathole-release-version").write_text(version)
    return version


def _fetch_nps(peer_dir: Path, arch: str) -> str:
    """nps's two tarballs: the server's `nps`+`conf/`+`web/`, the client's `npc`.

    Both land in one directory because nps resolves its configuration relative
    to the executable, and the server needs the shipped `conf/` files it reads
    its registries from.
    """
    version, assets = _latest_release("ehang-io/nps")
    if _cached("nps", version, peer_dir):
        print(f"nps: cached {version}")
        return version
    directory = peer_dir / "nps"
    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True, exist_ok=True)
    for role in ("server", "client"):
        name = _pick_asset(assets, [f"linux_{arch}_{role}.tar.gz"])
        print(f"nps: downloading {name}")
        tarball = peer_dir / f"nps-{role}.tar.gz"
        tarball.write_bytes(
            _http_get(
                f"https://github.com/ehang-io/nps/releases/download/v{version}/{name}"
            )
        )
        with tarfile.open(tarball) as t:
            t.extractall(filter="data", path=directory)
        tarball.unlink()
    for binary in ("nps", "npc"):
        path = directory / binary
        if not path.exists():
            raise SystemExit(f"nps: {binary} is not in the release archives")
        path.chmod(0o755)
    (peer_dir / ".nps-release-version").write_text(version)
    return version


#: Tool -> its fetcher, all sharing `(peer_dir, arch) -> version`, so the
#: dispatch is a dict lookup rather than per-tool lambdas that can drift from
#: the signature they call (they did once: every setup failed with a TypeError
#: until a smoke run caught it).
_FETCHERS = {"frp": _fetch_frp, "rathole": _fetch_rathole, "nps": _fetch_nps}


def fetch(peer_dir: Path, tools: list[str] | None = None) -> list[str]:
    """Install the newest GitHub release of each requested tool in `peer_dir`.

    The policy is the soak fetcher's: a peer is the latest **prebuilt** release
    and never a source build, and the resolved version is written to a marker
    next to the binaries — the only name a tool that prints no version can be
    recorded by, and `describe_version`'s fallback. A tool already at the
    newest release is left alone.

    Returns one `"<tool> <version>"` line per tool, in the order fetched.
    """
    wanted = list(tools) if tools else list(PEERS)
    unknown = sorted(set(wanted) - set(PEERS))
    if unknown:
        raise SystemExit(f"unknown peer tool(s): {unknown}; known: {sorted(PEERS)}")
    peer_dir.mkdir(parents=True, exist_ok=True)
    done = [f"{tool} {_FETCHERS[tool](peer_dir, _host_arch(tool))}" for tool in wanted]
    print(f"peers ready in {peer_dir}: {', '.join(done)}")
    return done


def main(argv: list[str] | None = None) -> int:
    """`bench peers fetch [--peer-dir DIR] [tool ...]`."""
    parser = argparse.ArgumentParser(
        prog="bench peers",
        description="the reference-tool peers: install their newest releases",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    fetch_cmd = sub.add_parser("fetch", help="download the newest release binaries")
    fetch_cmd.add_argument(
        "tools", nargs="*", help=f"any of {', '.join(PEERS)} (default: all)"
    )
    fetch_cmd.add_argument(
        "--peer-dir", type=Path, default=DEFAULT_PEER_DIR, help="where to install"
    )
    args = parser.parse_args(argv)
    fetch(args.peer_dir, args.tools or None)
    return 0
