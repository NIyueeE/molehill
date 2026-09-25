# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fetch the peer tools the Soak model compares against, into PEER_DIR.

Policy: every peer is the **latest GitHub release**, downloaded as a prebuilt
binary — nothing is ever compiled from source. Each resolved version is
written to a per-tool marker next to the binary; the runner reads it back
(`lib.peer_version`) and records it beside the measured numbers, so a chart
always states which peer build produced it.

Downloads hit api.github.com (unauthenticated: 60 req/h is plenty for four
lookups); binaries come from the release assets. A per-tool version marker
re-downloads automatically when a newer release appears.

Usage: uv run fetch_peers.py [peer_dir]   (default ~/tmp/bench-peers)
"""

import json
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
import zipfile
from pathlib import Path

PEER_DIR = Path(
    sys.argv[1] if len(sys.argv) > 1 else Path.home() / "tmp" / "bench-peers"
)
UA = {
    "User-Agent": "molehill-bench-peer-fetch",
    "Accept": "application/vnd.github+json",
}

# tool -> binary path relative to PEER_DIR once installed
BIN = {"frp": "frp/frps", "rathole": "rathole", "nps": "nps/nps"}


def http_get(url: str) -> bytes:
    # The URL always comes from this file's own repo/asset constants, never
    # from user input, so the scheme audit is satisfied here.
    req = urllib.request.Request(url, headers=UA)  # noqa: S310
    with urllib.request.urlopen(req, timeout=180) as r:  # noqa: S310
        return r.read()


def latest_release(repo: str) -> tuple:
    """(version without 'v', asset names) of the repo's latest release."""
    data = json.loads(http_get(f"https://api.github.com/repos/{repo}/releases/latest"))
    return data["tag_name"].removeprefix("v"), [a["name"] for a in data["assets"]]


def pick_asset(assets: list, patterns: list) -> str:
    """First asset whose name exactly matches one of the given patterns."""
    for name in patterns:
        if name in assets:
            return name
    raise SystemExit(f"no asset matching {patterns}; available: {assets}")


def cached(tool: str, ver: str) -> bool:
    """True if this tool's resolved latest release is already installed."""
    if not (PEER_DIR / BIN[tool]).exists():
        return False
    try:
        have = (PEER_DIR / f".{tool}-release-version").read_text().strip()
    except OSError:
        return False
    return have == ver


def fetch_release(tool: str, repo: str, mk_patterns, install) -> None:
    """Download the latest release matching mk_patterns(ver) and install."""
    ver, assets = latest_release(repo)
    if cached(tool, ver):
        print(f"{tool}: cached {ver}")
        return
    name = pick_asset(assets, mk_patterns(ver))
    print(f"{tool}: downloading {name}")
    url = f"https://github.com/{repo}/releases/download/v{ver}/{name}"
    install(ver, http_get(url))
    (PEER_DIR / f".{tool}-release-version").write_text(ver)


def install_frp(ver: str, blob: bytes, frp_arch: str) -> None:
    """`frp_<ver>_linux_<arch>.tar.gz`: a directory plus a stable symlink."""
    tarball = PEER_DIR / "frp.tar.gz"
    tarball.write_bytes(blob)
    with tarfile.open(tarball) as t:
        # `filter="data"` refuses absolute paths, links and device nodes, so
        # the extraction is safe by construction (ruff agrees: its S202 fires
        # only on an unfiltered extract).
        t.extractall(filter="data", path=PEER_DIR)
    tarball.unlink()
    link = PEER_DIR / "frp"
    link.unlink(missing_ok=True)
    link.symlink_to(f"frp_{ver}_linux_{frp_arch}")


def install_rathole(ver: str, blob: bytes) -> None:
    """`rathole-<arch>-unknown-linux-{gnu,musl}.zip`: one binary at the root."""
    zp = PEER_DIR / "rathole.zip"
    zp.write_bytes(blob)
    ex = PEER_DIR / "rathole-zip"
    with zipfile.ZipFile(zp) as z:
        # A trusted release asset; zipfile has no tarfile-style filter.
        z.extractall(path=ex)  # noqa: S202
    zp.unlink()
    candidates = list(ex.rglob("rathole"))
    if not candidates:
        sys.exit("rathole binary not found in the release archive")
    shutil.move(str(candidates[0]), PEER_DIR / "rathole")
    (PEER_DIR / "rathole").chmod(0o755)
    shutil.rmtree(ex, ignore_errors=True)


def fetch_nps(nps_arch: str) -> None:
    """nps ships separate server and client tarballs, so it is its own fetch.

    The server tarball carries `nps`, `conf/` and `web/`; the client tarball
    carries `npc`. Both land in one directory because nps resolves its config
    relative to the executable.
    """
    ver, assets = latest_release("ehang-io/nps")
    if cached("nps", ver):
        print(f"nps: cached {ver}")
        return
    nps_dir = PEER_DIR / "nps"
    shutil.rmtree(nps_dir, ignore_errors=True)
    nps_dir.mkdir(parents=True, exist_ok=True)
    for role in ("server", "client"):
        name = pick_asset(assets, [f"linux_{nps_arch}_{role}.tar.gz"])
        print(f"nps: downloading {name}")
        tarball = PEER_DIR / f"nps-{role}.tar.gz"
        tarball.write_bytes(
            http_get(f"https://github.com/ehang-io/nps/releases/download/v{ver}/{name}")
        )
        with tarfile.open(tarball) as t:
            t.extractall(filter="data", path=nps_dir)
        tarball.unlink()
    for binary in ("nps", "npc"):
        path = nps_dir / binary
        if not path.exists():
            sys.exit(f"nps: {binary} not found in the release archive")
        path.chmod(0o755)
    (PEER_DIR / ".nps-release-version").write_text(ver)


def print_versions() -> None:
    """What actually got installed — the versions the results will name."""
    print("== peer versions ==")
    for tool, rel in BIN.items():
        r = subprocess.run(
            [str(PEER_DIR / rel), "--version"],
            capture_output=True,
            text=True,
            check=False,
            timeout=15,
        )
        m = re.search(r"\d+\.\d+\.\d+", r.stdout + r.stderr)
        print(f"  {tool}: {m.group(0) if m else '?'}")
    print(f"peers ready in {PEER_DIR}")


def main() -> None:
    PEER_DIR.mkdir(parents=True, exist_ok=True)
    arch = platform.machine()
    frp_arch = {"x86_64": "amd64", "aarch64": "arm64"}.get(arch)
    rust_arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(arch)
    nps_arch = {"x86_64": "amd64", "aarch64": "arm64"}.get(arch)
    if frp_arch is None:
        sys.exit(f"unsupported arch: {arch}")
    if nps_arch is None:
        sys.exit(f"unsupported arch for nps: {arch}")

    fetch_release(
        "frp",
        "fatedier/frp",
        lambda v: [f"frp_{v}_linux_{frp_arch}.tar.gz"],
        lambda ver, blob: install_frp(ver, blob, frp_arch),
    )
    fetch_nps(nps_arch)
    fetch_release(
        "rathole",
        "rathole-org/rathole",
        lambda v: [
            f"rathole-{rust_arch}-unknown-linux-gnu.zip",
            f"rathole-{rust_arch}-unknown-linux-musl.zip",
        ],
        install_rathole,
    )
    print_versions()


if __name__ == "__main__":
    main()
