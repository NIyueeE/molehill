#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""The reference tools as model arms: frp, rathole and nps.

A *peer arm* is one of the tools this project is compared against, driven
through the model's own topology and measured by the model's own workloads. It
exists so a published comparison stays reproducible with one runner: the same
three namespaces, the same echoing backend, the same iperf3 sinks and the same
probes produce a peer's row and molehill's row, so neither can be a number
carried over from another harness. The retiring soak sweep measured the same
tools over a scripted degradation timeline; its peer configurations move here
rather than being lost with that tree, together with the pitfalls its comments
recorded.

Every adapter satisfies one contract, and that contract is what makes a peer an
arm like any other:

* the tool's **server** runs in the server namespace and exposes the model's
  ports on `model.TOPO_SRV_IP`, so the visitor dials the path under test and
  never the backend;
* the tool's **client** runs in the client namespace and forwards to
  `127.0.0.1:<port>` there, so one backend, one iperf3 sink and one probe serve
  every arm;
* the **control port** is the tool's own channel between the two daemons, and
  it is forwarded nowhere;
* every configuration file is written under `work` — the caller's evidence
  directory — and every returned argv is already wrapped in
  `ip netns exec <namespace>`.

Each tool carries UDP its own way — frp a `[[proxies]]` entry with
`type = "udp"`, rathole a service with `type = "udp"`, nps a client task with
`mode=udp` — and each adapter writes its tool's own shape, never a TCP-only
config that would quietly drop the datagram scenarios. The `udp_sink` port is
forwarded as *both* protocols: iperf3's UDP test opens a TCP control connection
to the same number it blasts datagrams at, exactly as the L4 arms' extra
`-ctrl` service does, and a datagram-only forward would fail the ladder before
its first sample.

`fetch()` installs the peers: the newest GitHub release, prebuilt and never
built from source, with the resolved version recorded in a per-tool marker.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import platform
import pwd
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
import zipfile
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path

import model
import topology

#: The token every peer's own handshake uses. It is not a secret: it is a
#: bench-local value written into every config the way the L4 arms write
#: `default_token = "bench"`, so no peer is accidentally left open to whatever
#: else shares the namespace.
TOKEN = "bench"  # noqa: S105 — a bench-local constant, not a credential

#: The exposed address — the *server* namespace's own — and the loopback every
#: daemon sees as its own. The visitor dials the first (that is the path under
#: test); the peers' clients forward to the second inside the *client*
#: namespace, where the backend listens.
SRV_IP = model.TOPO_SRV_IP
LOOPBACK = "127.0.0.1"

#: The port keys every adapter needs. A missing one is refused rather than
#: defaulted: a config written with port 0 is a peer that silently never
#: answers, which an analysis later reads as a tool failure.
REQUIRED_PORTS = ("control", "echo", "iperf", "iperf2", "udp", "udp_sink")

#: What every adapter forwards, as `(service name, protocol, port key)`. One
#: statement for all three tools, so a change cannot land in one tool's config
#: and miss another's. `udp_sink` appears twice on purpose (see the module
#: docstring); the control port never appears, because it is the peer's own
#: channel and forwarding it would push the bench's management traffic through
#: the path under test.
FORWARDS = (
    ("echo", "tcp", "echo"),
    ("iperf", "tcp", "iperf"),
    ("iperf2", "tcp", "iperf2"),
    ("udp", "udp", "udp"),
    ("udpsink", "udp", "udp_sink"),
    ("udpsink_ctrl", "tcp", "udp_sink"),
)


# --------------------------------------------------------------------------
# Paths and port helpers
# --------------------------------------------------------------------------
def _home() -> Path:
    """The *invoking* user's home directory.

    `sudo` sets `HOME=/root`, so a fetch run under `sudo` would install ~24 MB
    of peers into a directory the person who ran it cannot read, and the next
    unprivileged run would not find them. This is `runner.user_home`'s reason;
    it is duplicated rather than imported because `runner.py` imports this
    module and a cycle would be the price of a helper.
    """
    user = os.environ.get("SUDO_USER")
    if user:
        with contextlib.suppress(KeyError, OSError):
            return Path(pwd.getpwnam(user).pw_dir)
    return Path.home()


#: Where `fetch()` installs the peers: the soak fetcher's directory, so a host
#: that ran the sweep already has them.
DEFAULT_PEER_DIR = _home() / "tmp" / "bench-peers"

#: How a refusal tells a reader to install what is missing.
FETCH_CMD = f"python3 {Path(__file__).resolve()} fetch"


def _resolved(ports: dict) -> dict:
    """The caller's port band as ints, with every required key present."""
    missing = sorted(key for key in REQUIRED_PORTS if key not in ports)
    if missing:
        raise ValueError(
            f"ports is missing {missing}; every peer adapter needs "
            f"{list(REQUIRED_PORTS)}"
        )
    return {key: int(value) for key, value in ports.items()}


def _forwards(ports: dict) -> list[tuple[str, str, int]]:
    """`FORWARDS` with the caller's numbers: `(name, protocol, port)`."""
    return [(name, proto, ports[key]) for name, proto, key in FORWARDS]


def _config_dir(work: Path, tool: str) -> Path:
    """The tool's own directory under the run's work tree, created on demand."""
    d = work / tool
    d.mkdir(parents=True, exist_ok=True)
    return d


def _require(tool: str, binaries: dict) -> dict:
    """The tool's executables, or a refusal that names how to install them."""
    missing = sorted(
        f"{role} ({path})" for role, path in binaries.items() if not path.exists()
    )
    if missing:
        raise FileNotFoundError(
            f"{tool}: missing {', '.join(missing)}; run `{FETCH_CMD}`"
        )
    return binaries


def _link_or_copy(src: Path, target: Path) -> None:
    """Materialize `src` at `target` by hard link, falling back to a copy.

    A hard link keeps nps's ~24 MB of binaries on one inode, and `os.Args[0]`
    still names the linked path — which is where nps looks for its `conf/`. The
    copy is the fallback for a peer cache on another filesystem: measured as
    EXDEV from `/home` to `/tmp`, which failed a whole nps test while the
    binaries' own link had a fallback and quietly worked.
    """
    try:
        os.link(src, target)
    except OSError:
        shutil.copy2(src, target)


# --------------------------------------------------------------------------
# The adapters
# --------------------------------------------------------------------------
_SEMVER = re.compile(r"\d+\.\d+\.\d+")


@dataclass(frozen=True)
class PeerAdapter(ABC):
    """One reference tool, adapted to the model's one topology.

    `PEERS` is the registry the runner looks an arm's `model.Arm.tool` up in.
    `write()` embeds the executables' paths in the argv it returns, so an
    adapter knows where its own peer cache is: a run that keeps the cache
    somewhere else than the fetcher's default states it on the adapter it uses
    (`dataclasses.replace(PEERS[tool], peer_dir=...)`).
    """

    #: The name `PEERS` and `model.Arm.tool` call this tool.
    tool: str
    #: Where the tool's binaries are installed; the fetcher's own directory.
    peer_dir: Path = DEFAULT_PEER_DIR

    @abstractmethod
    def binaries(self, peer_dir: Path) -> dict[str, Path]:
        """The tool's server and client executables under `peer_dir`.

        The layout is `fetch()`'s, so an adapter and the fetcher cannot drift
        apart; `write()` refuses when one of them is missing, because half a
        pair starts and then answers nothing.
        """
        raise NotImplementedError

    @abstractmethod
    def write(
        self,
        work: Path,
        ports: dict,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list]:
        """Write this tool's configuration under `work`; return the argvs.

        The result is keyed `"server"` and `"client"`, each argv already
        wrapped in `ip netns exec <namespace>`. The namespaces default to the
        model's only topology; they are parameters because the adapter must not
        decide where the model puts its client.
        """
        raise NotImplementedError

    def describe_version(self, binary: Path) -> str:
        """The version the tool prints, or `""` when it cannot be read.

        The three tools disagree about where a version goes — frp prints it to
        stderr, rathole as a `Build Version:` line, nps as `Version:` — so one
        tolerant parse serves all of them. The fetcher's marker is the fallback
        for a binary that prints nothing, and an unreadable version stays an
        empty string: a guessed release number is worse than none, because it
        would be recorded beside the numbers as provenance.
        """
        out = ""
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            r = topology.run([str(binary), "--version"], timeout=15, check=False)
            out = r.stdout + r.stderr
        match = _SEMVER.search(out)
        if match:
            return match.group(0)
        for marker in (
            binary.parent / f".{self.tool}-release-version",
            binary.parent.parent / f".{self.tool}-release-version",
        ):
            with contextlib.suppress(OSError):
                return marker.read_text().strip()
        return ""


@dataclass(frozen=True)
class _Frp(PeerAdapter):
    """frp: `frps` and `frpc`, TOML, one `[[proxies]]` entry per service.

    `bindAddr` is the control listener and `proxyBindAddr` the exposed one;
    frp would default the latter to the former, and both are stated so the
    config says which address the visitor's path starts at. frps keeps a TCP
    and a UDP port manager apart, so the `udp_sink` pair may share one number.
    The client keeps trying after a refused login (`loginFailExit = false`),
    because the bench starts both daemons together and the server may not be
    listening yet.
    """

    tool: str = "frp"

    def binaries(self, peer_dir: Path) -> dict[str, Path]:
        return {
            "server": peer_dir / "frp" / "frps",
            "client": peer_dir / "frp" / "frpc",
        }

    def write(
        self,
        work: Path,
        ports: dict,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list]:
        """Write both TOMLs, one proxy per forwarded service."""
        ports = _resolved(ports)
        d = _config_dir(work, self.tool)
        bins = _require(self.tool, self.binaries(self.peer_dir))
        server_src = (
            f'bindAddr = "{SRV_IP}"\n'
            f"bindPort = {ports['control']}\n"
            f'proxyBindAddr = "{SRV_IP}"\n'
            f'auth.token = "{TOKEN}"\n'
        )
        proxies = "".join(
            "[[proxies]]\n"
            f'name = "{name}"\n'
            f'type = "{protocol}"\n'
            f'localIP = "{LOOPBACK}"\n'
            f"localPort = {port}\n"
            f"remotePort = {port}\n\n"
            for name, protocol, port in _forwards(ports)
        )
        client_src = (
            f'serverAddr = "{SRV_IP}"\n'
            f"serverPort = {ports['control']}\n"
            f'auth.token = "{TOKEN}"\n'
            "loginFailExit = false\n\n"
            f"{proxies}"
        )
        server, client = d / "frps.toml", d / "frpc.toml"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(
                server_ns, [str(bins["server"]), "-c", str(server)]
            ),
            "client": topology.Topology.ns_argv(
                client_ns, [str(bins["client"]), "-c", str(client)]
            ),
        }


@dataclass(frozen=True)
class _Rathole(PeerAdapter):
    """rathole: one binary, TOML, a plain-TCP transport on both sides.

    The transport is stated because it *is* the compared configuration: the
    plain-TCP shape is what the other peers run. A UDP service is a service
    like any other here (`type = "udp"`), so the `udp_sink` pair is two
    services on one address — the same shape rathole's own feature request for
    a combined `tcp,udp` type describes as the working alternative.
    """

    tool: str = "rathole"

    def binaries(self, peer_dir: Path) -> dict[str, Path]:
        return {"server": peer_dir / "rathole", "client": peer_dir / "rathole"}

    def write(
        self,
        work: Path,
        ports: dict,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list]:
        """Write both TOMLs, one service per forwarded port and protocol."""
        ports = _resolved(ports)
        d = _config_dir(work, self.tool)
        bins = _require(self.tool, self.binaries(self.peer_dir))
        server_src = (
            f'[server]\nbind_addr = "{SRV_IP}:{ports["control"]}"\n'
            '[server.transport]\ntype = "tcp"\n'
            + "".join(
                f"\n[server.services.{name}]\n"
                f'type = "{protocol}"\n'
                f'bind_addr = "{SRV_IP}:{port}"\n'
                f'token = "{TOKEN}"\n'
                for name, protocol, port in _forwards(ports)
            )
        )
        client_src = (
            f'[client]\nremote_addr = "{SRV_IP}:{ports["control"]}"\n'
            '[client.transport]\ntype = "tcp"\n'
            + "".join(
                f"\n[client.services.{name}]\n"
                f'type = "{protocol}"\n'
                f'local_addr = "{LOOPBACK}:{port}"\n'
                f'token = "{TOKEN}"\n'
                for name, protocol, port in _forwards(ports)
            )
        )
        server, client = d / "server.toml", d / "client.toml"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(
                server_ns, [str(bins["server"]), "--server", str(server)]
            ),
            "client": topology.Topology.ns_argv(
                client_ns, [str(bins["client"]), "--client", str(client)]
            ),
        }


@dataclass(frozen=True)
class _Nps(PeerAdapter):
    """nps: `nps` and `npc`, an ini pair, and a tree of the run's own.

    nps resolves `conf/` and `web/` from the directory of `os.Args[0]` (and
    `/etc/nps`, when that directory exists, wins over it), so `write()`
    materializes the run's tree: hard links to the two binaries, a `conf/`
    holding the config, and the shipped web assets by symlink. `npc` takes its
    own file by `-config`, and its ini parser keeps the spaces around `=`
    (measured: with them the client dials the wrong transport and dies on a
    UDP write to port 0), so its keys are written tight while `nps.conf`'s
    parser accepts either. The dashboard is mandatory — `web_port = 0` leaves
    nps blocked on a channel — so it takes `ports["web"]` when the caller
    states one and the first free port above the band otherwise, on loopback
    inside the server namespace.
    """

    tool: str = "nps"

    def binaries(self, peer_dir: Path) -> dict[str, Path]:
        return {
            "server": peer_dir / "nps" / "nps",
            "client": peer_dir / "nps" / "npc",
        }

    def write(
        self,
        work: Path,
        ports: dict,
        *,
        server_ns: str = topology.SRV_NS,
        client_ns: str = topology.CLI_NS,
    ) -> dict[str, list]:
        """Write the run's tree, then nps.conf and npc.conf inside it."""
        ports = _resolved(ports)
        d = _config_dir(work, self.tool)
        bins = _require(self.tool, self.binaries(self.peer_dir))
        _nps_tree(d, bins)
        forwards = _forwards(ports)
        low = min(port for _, _, port in forwards)
        high = max(port for _, _, port in forwards)
        server_src = (
            "appname = nps\n"
            "runmode = pro\n"
            "http_proxy_ip =\n"
            "http_proxy_port =\n"
            "https_proxy_port =\n"
            "bridge_type = tcp\n"
            f"bridge_ip = {SRV_IP}\n"
            f"bridge_port = {ports['control']}\n"
            f"public_vkey = {TOKEN}\n"
            "log_level = 6\n"
            f"web_host = {LOOPBACK}\n"
            f"web_username = {TOKEN}\n"
            f"web_password = {TOKEN}\n"
            f"web_ip = {LOOPBACK}\n"
            f"web_port = {ports.get('web', high + 1)}\n"
            f"allow_ports = {low}-{high}\n"
            "allow_flow_limit = false\n"
            "allow_rate_limit = false\n"
            "allow_tunnel_num_limit = false\n"
            "allow_local_proxy = false\n"
            "allow_connection_num_limit = false\n"
            "allow_multi_ip = false\n"
            "system_info_display = false\n"
            "disconnect_timeout = 60\n"
        )
        tasks = "".join(
            f"\n[{protocol}_{name}]\nmode={protocol}\n"
            f"target_addr={LOOPBACK}:{port}\n"
            f"server_port={port}\n"
            for name, protocol, port in forwards
        )
        client_src = (
            "[common]\n"
            f"server_addr={SRV_IP}:{ports['control']}\n"
            "conn_type=tcp\n"
            f"vkey={TOKEN}\n"
            "auto_reconnection=true\n"
            "crypt=false\n"
            "compress=false\n"
            "max_conn=1000\n"
            "disconnect_timeout=60\n"
            f"{tasks}"
        )
        server, client = d / "conf" / "nps.conf", d / "npc.conf"
        server.write_text(server_src)
        client.write_text(client_src)
        return {
            "server": topology.Topology.ns_argv(server_ns, [str(d / "nps")]),
            "client": topology.Topology.ns_argv(
                client_ns, [str(d / "npc"), "-config", str(client)]
            ),
        }


def _nps_tree(d: Path, bins: dict) -> None:
    """Materialize the run's nps tree: binaries, shipped files, web assets.

    Idempotent, because a work directory may be written twice (a retried arm),
    and the shipped files are only linked when they are not there: nps reads
    its registries (`clients.json`, `hosts.json`, ...) from the same directory
    and panics when they are missing.
    """
    if Path("/etc/nps").exists():
        raise RuntimeError(
            "/etc/nps exists: nps would read its conf/ and web/ from there "
            "instead of this run's tree, so the config written here would be "
            "ignored"
        )
    for source in bins.values():
        target = d / source.name
        if not target.exists():
            _link_or_copy(source, target)
        target.chmod(0o755)
    conf = d / "conf"
    conf.mkdir(exist_ok=True)
    shipped = bins["server"].parent / "conf"
    if not shipped.is_dir():
        raise FileNotFoundError(f"nps: {shipped} is missing; re-fetch the peers")
    for src in sorted(shipped.iterdir()):
        if src.name != "nps.conf" and not (conf / src.name).exists():
            _link_or_copy(src, conf / src.name)
    web = d / "web"
    if not web.exists():
        web.symlink_to(bins["server"].parent / "web", target_is_directory=True)


#: The adapters `model.Arm.tool` names. Public because this lookup is the only
#: wiring a peer arm needs.
PEERS: dict[str, PeerAdapter] = {
    "frp": _Frp(),
    "rathole": _Rathole(),
    "nps": _Nps(),
}


# --------------------------------------------------------------------------
# Fetching the peers
# --------------------------------------------------------------------------
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


def _pick_asset(assets: list, patterns: list) -> str:
    """The first asset whose name exactly matches one of the patterns."""
    for name in patterns:
        if name in assets:
            return name
    raise SystemExit(f"no asset matching {patterns}; available: {assets}")


def _cached(tool: str, version: str, peer_dir: Path) -> bool:
    """Is the tool's resolved latest release already installed?"""
    if not all(path.exists() for path in PEERS[tool].binaries(peer_dir).values()):
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
    d = peer_dir / "nps"
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True, exist_ok=True)
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
            t.extractall(filter="data", path=d)
        tarball.unlink()
    for binary in ("nps", "npc"):
        path = d / binary
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
    """`python3 peers.py fetch [--peer-dir DIR] [tool ...]`."""
    parser = argparse.ArgumentParser(
        prog="peers.py",
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


if __name__ == "__main__":
    sys.exit(main())
