# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fetch the peer tools for the benchmark matrix into PEER_DIR.

Policy: every peer is the **latest GitHub release**, downloaded as a prebuilt
binary — nothing is ever compiled from source. The resolved version of each
run is recorded in the results meta (`tool_versions`) and the chart footer,
so the README always states what was actually measured.

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

PEER_DIR = Path(sys.argv[1] if len(sys.argv) > 1
                else Path.home() / "tmp" / "bench-peers")
UA = {"User-Agent": "molehill-bench-peer-fetch",
      "Accept": "application/vnd.github+json"}

# tool -> binary path relative to PEER_DIR once installed
BIN = {"frp": "frp/frps", "bore": "bore", "rathole": "rathole"}


def http_get(url: str) -> bytes:
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=180) as r:
        return r.read()


def latest_release(repo: str) -> tuple:
    """(version without 'v', asset names) of the repo's latest release."""
    data = json.loads(http_get(
        f"https://api.github.com/repos/{repo}/releases/latest"))
    return data["tag_name"].removeprefix("v"), \
        [a["name"] for a in data["assets"]]


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


def main() -> None:
    PEER_DIR.mkdir(parents=True, exist_ok=True)
    arch = platform.machine()
    frp_arch = {"x86_64": "amd64", "aarch64": "arm64"}.get(arch)
    rust_arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(arch)
    if frp_arch is None:
        sys.exit(f"unsupported arch: {arch}")

    # ---- frp: frp_<ver>_linux_amd64.tar.gz (frps+frpc directory) ----------
    def install_frp(ver: str, blob: bytes) -> None:
        tarball = PEER_DIR / "frp.tar.gz"
        tarball.write_bytes(blob)
        with tarfile.open(tarball) as t:
            t.extractall(filter="data", path=PEER_DIR)
        tarball.unlink()
        link = PEER_DIR / "frp"
        link.unlink(missing_ok=True)
        link.symlink_to(f"frp_{ver}_linux_{frp_arch}")

    fetch_release(
        "frp", "fatedier/frp",
        lambda v: [f"frp_{v}_linux_{frp_arch}.tar.gz"], install_frp)

    # ---- bore: bore-v<ver>-x86_64-unknown-linux-musl.tar.gz ---------------
    def install_bore(ver: str, blob: bytes) -> None:
        tarball = PEER_DIR / "bore.tar.gz"
        tarball.write_bytes(blob)
        with tarfile.open(tarball) as t:
            t.extractall(filter="data", path=PEER_DIR)
        tarball.unlink()
        if not (PEER_DIR / "bore").exists():
            for cand in PEER_DIR.glob("bore*/bore"):
                shutil.move(str(cand), PEER_DIR / "bore")
                break
        (PEER_DIR / "bore").chmod(0o755)

    fetch_release(
        "bore", "ekzhang/bore",
        lambda v: [f"bore-v{v}-{rust_arch}-unknown-linux-musl.tar.gz",
                   f"bore-v{v}-{rust_arch}-unknown-linux-gnu.tar.gz"],
        install_bore)

    # ---- rathole (upstream): rathole-x86_64-unknown-linux-{gnu,musl}.zip --
    def install_rathole(ver: str, blob: bytes) -> None:
        zp = PEER_DIR / "rathole.zip"
        zp.write_bytes(blob)
        ex = PEER_DIR / "rathole-zip"
        with zipfile.ZipFile(zp) as z:
            z.extractall(path=ex)  # zipfile has no tarfile-style filter
        zp.unlink()
        candidates = list(ex.rglob("rathole"))
        if not candidates:
            sys.exit("rathole binary not found in the release archive")
        shutil.move(str(candidates[0]), PEER_DIR / "rathole")
        (PEER_DIR / "rathole").chmod(0o755)
        shutil.rmtree(ex, ignore_errors=True)

    fetch_release(
        "rathole", "rathole-org/rathole",
        lambda v: [f"rathole-{rust_arch}-unknown-linux-gnu.zip",
                   f"rathole-{rust_arch}-unknown-linux-musl.zip"],
        install_rathole)

    print("== peer versions ==")
    for tool, rel in BIN.items():
        r = subprocess.run([str(PEER_DIR / rel), "--version"],
                           capture_output=True, text=True, check=False,
                           timeout=15)
        out = r.stdout + r.stderr
        m = re.search(r"\d+\.\d+\.\d+", out)
        print(f"  {tool}: {m.group(0) if m else '?'}")
    print(f"peers ready in {PEER_DIR}")


if __name__ == "__main__":
    main()
