# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fetch the **previous molehill release** binary, for interop testing.

Policy: the interop matrix compares this build's client and server against the
binary of the release before it — both directions, plus "an old peer must reject
a new dialect cleanly". Nothing is compiled from source: the asset is the same
one users download, which is the point (a binary built from today's tree cannot
test yesterday's protocol).

Usage:
    uv run fetch_old.py [tag] [cache_dir]

`tag` defaults to the newest release that is not this build's own version, so a
run for v0.9.1 fetches v0.9.0 without being told. The resolved tag and the
binary's own `--version` output are printed; the interop test reads the path
from `MOLEHILL_OLD_BIN` (see `tests/interop_test.rs`).
"""

import json
import os
import platform
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

#: argv is [script, tag?, cache_dir?]; ruff's magic-value rule applies to the
#: index, which reads better named than inlined.
CACHE_ARG = 2
CACHE_DIR = Path(
    sys.argv[CACHE_ARG]
    if len(sys.argv) > CACHE_ARG
    else Path.home() / "tmp" / "interop"
)
REPO = "NIyueeE/molehill"
UA = {"User-Agent": "molehill-interop-fetch", "Accept": "application/vnd.github+json"}


def http_get(url: str) -> bytes:
    # URLs come from this file's own constants and the GitHub API response for
    # our own repository, never from user input.
    req = urllib.request.Request(url, headers=UA)  # noqa: S310
    with urllib.request.urlopen(req, timeout=180) as r:  # noqa: S310
        return r.read()


def releases() -> list:
    """Every release, newest first, as (tag, [asset names]).

    `gh` first: it is authenticated where a developer works, and the public API
    allows 60 requests an hour — a limit this script hit on its first run.
    urllib is the fallback for an environment without `gh`.
    """
    try:
        out = subprocess.run(
            ["gh", "api", f"repos/{REPO}/releases?per_page=20"],
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        ).stdout
        data = json.loads(out)
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError) as e:
        print(f"note: `gh` unavailable ({e}); falling back to the public API")
        try:
            data = json.loads(
                http_get(f"https://api.github.com/repos/{REPO}/releases?per_page=20")
            )
        except OSError as api_err:
            sys.exit(
                f"cannot list releases ({api_err}). Pass an explicit tag "
                f"(uv run fetch_old.py v0.9.0) or set MOLEHILL_OLD_BIN yourself."
            )
    return [(d["tag_name"], [a["name"] for a in d["assets"]]) for d in data]


def assets_for(tag: str) -> list:
    """Asset names of one release, without listing every release.

    An explicit tag skips the listing entirely — which is also the way around a
    rate-limited API (`--tag` on the command line, or `MOLEHILL_OLD_TAG`).
    """
    try:
        out = subprocess.run(
            ["gh", "release", "view", tag, "--json", "assets"],
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        ).stdout
        return [a["name"] for a in json.loads(out)["assets"]]
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, KeyError) as e:
        print(f"note: `gh release view` unavailable ({e}); trying the public API")
        data = json.loads(
            http_get(f"https://api.github.com/repos/{REPO}/releases/tags/{tag}")
        )
        return [a["name"] for a in data["assets"]]


def own_version() -> str:
    """The version in this checkout's Cargo.toml (`0.9.1` → `v0.9.1`)."""
    for line in (
        (Path(__file__).resolve().parents[3] / "Cargo.toml").read_text().split("\n")
    ):
        if line.startswith("version = "):
            return "v" + line.split('"')[1]
    sys.exit("cannot read the version from Cargo.toml")


def host_asset(version: str, assets: list) -> str:
    """The asset for this host, or a precise list of what exists instead.

    The interop test runs a real client and a real server, so it needs a native
    binary; a missing asset is a skip-worthy condition, not a hard failure, and
    the message says exactly which host it wanted.
    """
    machine = platform.machine()
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "armv7l": "armv7"}.get(machine)
    if arch is None:
        sys.exit(f"unsupported host architecture {machine!r} for interop")
    if sys.platform == "darwin":
        want = f"molehill-{version}-{arch}-apple-darwin.tar.gz"
    elif sys.platform.startswith("linux"):
        libc = "musl" if "musl" in (platform.libc_ver()[0] or "") else "gnu"
        want = f"molehill-{version}-{arch}-unknown-linux-{libc}.tar.gz"
    else:
        sys.exit(f"unsupported host platform {sys.platform!r} for interop")
    if want not in assets:
        sys.exit(f"{version} has no {want}; available: {assets}")
    return want


def main() -> None:
    # `MOLEHILL_OLD_TAG` or an explicit tag skips release listing entirely —
    # cheaper, and the way around a rate-limited API.
    want_tag = os.environ.get("MOLEHILL_OLD_TAG") or (
        sys.argv[1] if len(sys.argv) > 1 and sys.argv[1].startswith("v") else None
    )
    if want_tag is not None:
        tag, assets = want_tag, assets_for(want_tag)
    else:
        found = releases()
        if not found:
            sys.exit("no releases to fetch; pass an explicit tag")
        # The newest release, always: while a cycle is in development the tree's
        # version *is* the last release (the bump happens at release time), so
        # "newest release that is not ours" would skip one release back.
        tag, assets = found[0]
    if tag == own_version():
        print(
            f"note: {tag} is also the version in this tree, so this comparison "
            f"is same-release; it is a smoke check, not an old-peer test"
        )

    target = CACHE_DIR / tag
    binary = target / "molehill"
    if binary.exists():
        print(f"cached {tag} -> {binary}")
    else:
        name = host_asset(tag, assets)
        print(f"{tag}: downloading {name}")
        blob = http_get(f"https://github.com/{REPO}/releases/download/{tag}/{name}")
        target.mkdir(parents=True, exist_ok=True)
        tarball = target / name
        tarball.write_bytes(blob)
        with tarfile.open(tarball) as t:
            # `filter="data"` refuses absolute paths, links and device nodes.
            t.extractall(filter="data", path=target)
        tarball.unlink()
        # The archives extract a `molehill` binary beside (or below) the tarball.
        if not binary.exists():
            candidates = [p for p in target.rglob("molehill") if p.is_file()]
            if not candidates:
                sys.exit(f"no molehill binary inside {name}")
            candidates[0].chmod(0o755)
            candidates[0].rename(binary)
    binary.chmod(0o755)
    ver = subprocess.run(
        [str(binary), "--version"], capture_output=True, text=True, check=False
    )
    print(f"MOLEHILL_OLD_BIN={binary}")
    print(f"reports: {(ver.stdout or ver.stderr).strip()}")


if __name__ == "__main__":
    main()
