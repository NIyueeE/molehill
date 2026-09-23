#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Benchmark matrix orchestrator (schema v3) — the uv/python test entry.

Test-flow properties:
- continue-on-error: an arm or metric failure records an error/skipped entry
  and never aborts the matrix (Ctrl-C included: completed arms are saved)
- real-time reporting: every completed arm is printed AND checkpointed to the
  results file immediately (atomic write)
- test selection: --tools / --cells / --variants pick the arms to run; results
  are merged into the existing file unless --fresh is given
- crash hygiene: every spawned pid is recorded in <workdir>/pids.json; a new
  run reaps leftovers of crashed runs (cmdline-verified, so recycled pids are
  never killed)

Machine notes (moved from the retired run_bench.sh):
- throughput dials the tool's EXPOSED port — everything is measured through
  the tunnel; pre-v3 baselines dialed the backend directly and are invalid
- weak cells prefer netem on `lo` (needs CAP_NET_ADMIN). netem applies to ALL
  loopback traffic, so every leg of visitor->server->client->backend is
  delayed/lossy (the loopback RTT amplifies by leg count — the same for every
  tool). Without CAP_NET_ADMIN, rtt cells fall back to the userspace
  weakproxy (delays only the client<->server leg) and loss cells are skipped
- netem random drops land mostly on the saturating iperf flow (shared qdisc
  dilution): UDP-loss metrics in loss cells measure the residual share a
  game-like session sees, not the configured loss rate
- a leftover netem qdisc from a killed run is replaced and removed by the
  startup capability probe, so a crashed run cannot keep delaying loopback
- knobs are env-tunable (see bench_lib.Knobs): molehill runs at full rigor
  (3 reps), peers at reduced rigor (1 rep) — the regression gate only gates
  molehill's default (mux) row
- peers are reference points: they run the loopback / rtt10 / loss1 cells
  plus the rate-limited cells (the pure-delay, high-loss and jitter
  stories are told by the molehill rows), which keeps the peer axis lean
- rate cells (r<mbit>/<rtt>) need netem with `rate` support (modern
  iproute2); without it the cell degrades to the weakproxy fallback
"""
import argparse
import contextlib
import hashlib
import json
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from bench_lib import (
    RATE_QUEUE_LIMIT,
    SCHEMA,
    Backends,
    Knobs,
    Netem,
    acquire_lock,
    cell_sort_key,
    churn,
    cpu_stats,
    dump_results,
    framing_stats,
    latency,
    load_results,
    mem_stats,
    merge_arm,
    parse_cell,
    record_pid,
    release_lock,
    run_cpu_sampler,
    run_rss_sampler,
    sweep_stale,
    udp_capacity_probe,
    wait_load_quiet,
    wait_port,
)
from hol_probe import run_hol_probe
from tcp_steady_ping import run_tcp_steady_ping
from udp_ping import run_udp_ping


def _handle_sigterm(signum, frame):
    """SIGTERM takes the Ctrl-C path: arms are killed, the netem qdisc is
    removed and the full meta is checkpointed before exit."""
    raise KeyboardInterrupt


PEER_BINS = {"frp": "frps", "rathole": "rathole", "nps": "nps"}

# yamux's default max concurrent streams per tunnel
# (DEFAULT_MUX_MAX_STREAMS in src/common/constants.rs). An arm's *usable*
# data-stream ceiling is its tunnel count x this, minus the channels the
# bench itself holds open on those tunnels (see
# `variant_stream_ceiling`), so a scale test above the usable ceiling is
# skipped deliberately instead of failing an over-limit dial.
MUX_MAX_STREAMS = 64

# The stripe experiment's arm: the default mux shape (multiplex, count = 4)
# with every visitor connection spread over STRIPE_ARMS parallel data
# channels. The stripe count rides the environment (the server-side
# `stripe_count` knob's measurement-only override), so the config on the
# wire stays identical to the `mux` arm and a binary that predates the
# striped data-channel command ignores the variable entirely.
STRIPE_ARMS = {"mux-stripe": 4}


def stripe_env(variant: str) -> dict | None:
    """Per-arm environment for the stripe experiment, or None."""
    stripes = STRIPE_ARMS.get(variant)
    if not stripes:
        return None
    return {**MUX_STATS_ENV, "MOLEHILL_STRIPE_COUNT": str(stripes)}


def variant_stream_ceiling(variant: str, pool_size: int) -> int | None:
    """Concurrent data-stream ceiling of a molehill arm, or None when the
    arm has no mux layer (direct mode). `mux1` runs one tunnel -> 64; the
    count = 4 arms -> 256.

    The cap counts *streams*, and a tunnel also carries the channels the
    bench itself holds open: the server pre-opens `pool_size` data
    channels per registered service (iperf and echo here, plus the UDP
    echo's own 2) at registration, and the measurement client's control
    stream is one more. The usable data-stream ceiling is what remains.
    Probed exactly on one host as cap - pools - control: a count = 1
    tunnel with pool_size = 16 failed at the 16th data stream with
    cap = 32 and at the 48th with cap = 64, i.e. 15 and 47 usable."""
    count = {"mux1": 1, "mux-off": None, "noise-direct": None}.get(variant, 4)
    if count is None:
        return None
    reserved = 2 * pool_size + 2  # iperf + echo pools, plus the UDP echo pool
    # A striped visitor connection holds `stripes` streams on the tunnels,
    # so the number of concurrent visitors the arm can carry is the stream
    # budget divided by the stripe count (`mux-stripe` sets
    # MOLEHILL_STRIPE_COUNT, below).
    stripes = STRIPE_ARMS.get(variant, 1)
    return (count * MUX_MAX_STREAMS - reserved - 1) // stripes


class ArmProcs:
    """Process group for one arm: spawn with per-arm logs, kill as a group."""

    def __init__(self, work: Path, label: str):
        self.work = work
        self.label = label
        self.pids = []
        self.tool_pids = []  # the proxied tool's own pids (RSS sampler input)
        self.logs: list[Path] = []

    def spawn(self, cmd: list, tool: bool = True,
              cwd: Path | None = None, role: str = "", env: dict | None = None) -> None:
        # `role` keeps a multi-process arm's streams apart: without it every
        # process of one arm appends to the same file, and a parser cannot
        # tell their lines (or their counters) apart.
        name = self.label.replace(" ", "_").replace("/", "_")
        if role:
            name = f"{name}.{role}"
        log = self.work / f"{name}.log"
        with open(log, "ab") as f:
            pid = subprocess.Popen(cmd, stdout=f, stderr=f, cwd=cwd,
                                   env=env or MUX_STATS_ENV).pid
        record_pid(self.work, pid)
        self.pids.append(pid)
        if tool:
            self.tool_pids.append(pid)
        self.logs.append(log)

    def kill(self) -> None:
        for pid in self.pids:
            with contextlib.suppress(OSError):
                os.kill(pid, signal.SIGKILL)
        self.pids.clear()
        time.sleep(0.3)


_NOISE_KEYS = None


def noise_keys(binary: str) -> tuple:
    """Generate once per run and cache a Noise keypair via `--genkey`."""
    global _NOISE_KEYS
    if _NOISE_KEYS is None:
        out = subprocess.run([binary, "--genkey"], capture_output=True,
                             text=True, timeout=30, check=False).stdout
        priv = pub = ""
        lines = out.splitlines()
        for i, line in enumerate(lines):
            if line.strip() == "Private Key:" and i + 1 < len(lines):
                priv = lines[i + 1].strip()
            elif line.strip() == "Public Key:" and i + 1 < len(lines):
                pub = lines[i + 1].strip()
        if not priv or not pub:
            raise RuntimeError(f"noise keygen failed: {out[:200]}")
        _NOISE_KEYS = (priv, pub)
    return _NOISE_KEYS


def ab_suffixes(bins: list) -> dict:
    """{binary path: label suffix} — unique for every given binary.

    The suffix distinguishes the two sides of an `--ab` interleave:
    `merge_arm` keys results by (tool, cell), so two arms sharing a
    label overwrite each other, and `ab_compare.py` pairs rounds by
    parsing the suffix — a collision there reads as "not an A/B pair"
    and the whole interleave is silently dropped. The short basename
    keeps the chart legend readable, but two binaries built from
    different worktrees usually share it (`target/release/molehill`),
    so a collision falls back to a short hash of the full path: still
    distinct, still stable across the run's three rounds.
    """
    by_name: dict = {}
    for b in bins:
        by_name.setdefault(Path(b).name, []).append(b)
    out = {}
    for name, same in by_name.items():
        for b in same:
            label = name
            if len(same) > 1:
                digest = hashlib.sha256(str(Path(b).resolve()).encode()).hexdigest()
                label = f"{name}-{digest[:8]}"
            out[b] = label
    return out


def molehill_config(work: Path, variant: str, knobs: Knobs, p: dict) -> Path:
    """Write server/client tomls for one molehill variant; returns config dir."""
    d = work / "molehill"; d.mkdir(exist_ok=True)
    mode = "direct" if variant in ("mux-off", "noise-direct") else "multiplex"
    # Single-variable arms. `mux` is the default control (plain transport,
    # multiplex, count = 4): `noise` changes only the transport, `mux1` only
    # the tunnel count, `kcp4` only the data-plane carrier (on top of the
    # noise arm). `noise-direct` is the noise transport in direct mode: a
    # new point on the transport x mode grid, and the arm that isolates the
    # record-stream cost with no mux framing in the way (the mux frame
    # reader asks for less than a record, so a record staged in the wrapper
    # is structural there — compare `mux-off` for the transport axis and
    # `noise` for the mode axis).
    data_c = data_s = ""
    if variant == "noise":
        transport = "noise"
    elif variant == "mux1":            # count axis: plain, one tunnel
        transport = "plain"
        data_c = "default_count = 1\n"
    elif variant == "kcp4":            # carrier axis: noise + KCP-over-UDP
        transport = "noise"
        data_c = (f'default_carrier = "kcp"\n'
                  f'default_data_addr = "127.0.0.1:{p["kcp_bind"]}"\n'
                  f"default_count = 4\n")
        data_s = (f'bind_addr = "127.0.0.1:{p["kcp_bind"]}"\n')
    elif variant == "noise-direct":    # transport axis, no mux layer
        transport = "noise"
    else:
        transport = "plain"
    noise_s = noise_c = ""
    if transport == "noise":
        priv, pub = noise_keys(knobs.molehill_bin)
        noise_s = f'[server.transport.noise]\nlocal_private_key = "{priv}"\n'
        noise_c = f'[client.transport.noise]\nremote_public_key = "{pub}"\n'
    data_s_block = f"[server.data]\n{data_s}" if data_s else ""
    # Client-first server transport: keys only, no `type` — whether a
    # connection is encrypted is the client's call (v3 selector byte).
    server_transport = noise_s or ""
    (d / "server.toml").write_text(f"""[server]
default_token = "bench"
allow_ports = ["25100-{knobs.allow_port_hi}"]

[server.control]
bind_addr = "127.0.0.1:{p['control']}"

{data_s_block}{server_transport}""")
    (d / "client.toml").write_text(f"""[client]
default_token = "bench"

[client.control]
default_remote_addr = "127.0.0.1:{p['client_dial']}"

[client.data]
default_mode = "{mode}"
{data_c}[client.transport]
type = "{transport}"
{noise_c}
[client.services.iperf]
local_addr = "127.0.0.1:{p['iperf_backend']}"
remote_bind_addr = "127.0.0.1:{p['iperf_exposed']}"
pool_size = {knobs.pool_size}

[client.services.echo]
local_addr = "127.0.0.1:{p['echo_backend']}"
remote_bind_addr = "127.0.0.1:{p['echo_exposed']}"
pool_size = {knobs.pool_size}

[client.services.udpecho]
protocol = "udp"
local_addr = "127.0.0.1:{p['udp_backend']}"
remote_bind_addr = "127.0.0.1:{p['udp_exposed']}"
pool_size = 2
udp_buffer_size = 2048
udp_idle_timeout = 60
udp_send_queue_size = 1024
""")
    return d


def start_molehill(procs: ArmProcs, d: Path, variant: str = "",
                   binary: str = "") -> None:
    env = stripe_env(variant)
    procs.spawn([binary, "--server", str(d / "server.toml")],
                role="server", env=env)
    procs.spawn([binary, "--client", str(d / "client.toml")],
                role="client", env=env)


# The framing counters are the bench's attribution tool (frames/s and
# CPU/frame per arm), so every molehill arm runs with them on. Setting it
# here rather than relying on the caller's shell keeps a forgotten variable
# from silently turning the metric into an absent field.
MUX_STATS_ENV = {**os.environ, "MOLEHILL_MUX_STATS": "1"}
# A second, opt-in instrumentation switch: an explicit data-socket buffer
# size. The rtt100 cell is bounded outside the engine (see HANDOFF), and
# this is the variable that decides whether the kernel's auto-tuned window
# is the binding constraint there. Unset leaves the kernel alone.
_maybe_buf = os.environ.get("BENCH_TCP_BUFFER_BYTES")
MUX_STATS_ENV = {**MUX_STATS_ENV,
                 "MOLEHILL_TCP_BUFFER_BYTES": _maybe_buf} if _maybe_buf else MUX_STATS_ENV



def setup_molehill(variant: str, knobs: Knobs, p: dict, procs: ArmProcs,
                   work: Path) -> None:
    d = molehill_config(work, variant, knobs, p)
    start_molehill(procs, d, variant, knobs.molehill_bin)


def setup_frp(knobs: Knobs, p: dict, procs: ArmProcs, work: Path) -> None:
    d = work / "frp"; d.mkdir(exist_ok=True)
    peer = Path(knobs.peer_dir) / "frp"
    (d / "frps.toml").write_text(
        f'bindAddr = "127.0.0.1"\nbindPort = {p["control"]}\n'
        f'auth.token = "bench"\n')
    (d / "frpc.toml").write_text(f"""serverAddr = "127.0.0.1"
serverPort = {p['client_dial']}
auth.token = "bench"
loginFailExit = false

[[proxies]]
name = "iperf"
type = "tcp"
localIP = "127.0.0.1"
localPort = {p['iperf_backend']}
remotePort = {p['iperf_exposed']}

[[proxies]]
name = "echo"
type = "tcp"
localIP = "127.0.0.1"
localPort = {p['echo_backend']}
remotePort = {p['echo_exposed']}

[[proxies]]
name = "udpecho"
type = "udp"
localIP = "127.0.0.1"
localPort = {p['udp_backend']}
remotePort = {p['udp_exposed']}
""")
    procs.spawn([str(peer / "frps"), "-c", str(d / "frps.toml")])
    procs.spawn([str(peer / "frpc"), "-c", str(d / "frpc.toml")])


def setup_rathole(knobs: Knobs, p: dict, procs: ArmProcs, work: Path) -> None:
    d = work / "rathole"; d.mkdir(exist_ok=True)
    peer = Path(knobs.peer_dir) / "rathole"
    (d / "server.toml").write_text(f"""[server]
bind_addr = "127.0.0.1:{p['control']}"
[server.transport]
type = "tcp"

[server.services.iperf]
bind_addr = "127.0.0.1:{p['iperf_exposed']}"
token = "bench"

[server.services.echo]
bind_addr = "127.0.0.1:{p['echo_exposed']}"
token = "bench"

[server.services.udpecho]
type = "udp"
bind_addr = "127.0.0.1:{p['udp_exposed']}"
token = "bench"
""")
    (d / "client.toml").write_text(f"""[client]
remote_addr = "127.0.0.1:{p['client_dial']}"
[client.transport]
type = "tcp"

[client.services.iperf]
local_addr = "127.0.0.1:{p['iperf_backend']}"
token = "bench"

[client.services.echo]
local_addr = "127.0.0.1:{p['echo_backend']}"
token = "bench"

[client.services.udpecho]
type = "udp"
local_addr = "127.0.0.1:{p['udp_backend']}"
token = "bench"
""")
    procs.spawn([str(peer), "--server", str(d / "server.toml")])
    procs.spawn([str(peer), "--client", str(d / "client.toml")])


def setup_nps(knobs: Knobs, p: dict, procs: ArmProcs, work: Path) -> None:
    """nps: one server (`nps`) plus one client (`npc`) holding all proxies.

    nps multiplexes every proxy over its bridge connection — the same
    shape as molehill's mux arm and frp — so it is a like-for-like peer.
    Crypt and compress are OFF (the example npc.conf ships them on): the
    plain-TCP peers are the unencrypted, uncompressed reference points.

    nps resolves its config relative to the EXECUTABLE's directory — a
    config in the CWD is silently ignored (verified: it bound the shipped
    defaults instead) — so each arm builds its own tree next to the work
    dir: hard links to the ~24 MB of binaries (same inode, and
    /proc/self/exe resolves to this path), a real `conf/` to customise,
    and the shipped web assets by symlink. The server also writes an
    sqlite db, so it runs with this directory as its CWD.
    """
    d = work / "nps"
    d.mkdir(exist_ok=True)
    peer = Path(knobs.peer_dir) / "nps"
    for binary in ("nps", "npc"):
        target = d / binary
        if not target.exists():
            try:
                os.link(peer / binary, target)
            except OSError:  # different filesystem: fall back to a copy
                shutil.copy2(peer / binary, target)
        target.chmod(0o755)
    conf = d / "conf"
    conf.mkdir(exist_ok=True)
    # nps also reads its registries (clients.json, hosts.json, ...) from
    # the same directory and panics when they are missing, so link every
    # shipped file that is not the config itself.
    for shipped in (peer / "conf").iterdir():
        if shipped.name != "nps.conf" and not (conf / shipped.name).exists():
            os.link(shipped, conf / shipped.name)
    # The web UI is mandatory in nps.conf; it takes a free slot in the
    # arm's port band, and the http/https proxy ports stay empty so
    # nothing privileged is opened.
    (conf / "nps.conf").write_text(f"""appname = nps
runmode = pro
http_proxy_ip =
http_proxy_port =
https_proxy_port =
bridge_type = tcp
bridge_ip = 127.0.0.1
bridge_port = {p['control']}
public_vkey = bench
log_level = 6
web_host = 127.0.0.1
web_username = bench
web_password = bench
web_ip = 127.0.0.1
web_port = {p['nps_web']}
allow_ports = {p['iperf_exposed']}-{p['udp_exposed']}
allow_flow_limit = false
allow_rate_limit = false
allow_tunnel_num_limit = false
allow_local_proxy = false
allow_connection_num_limit = false
allow_multi_ip = false
system_info_display = false
disconnect_timeout = 60
""")
    (d / "web").symlink_to(peer / "web", target_is_directory=True)
    # No spaces around `=`: npc's ini parser keeps them (verified — the
    # client then dials the wrong transport and dies on a UDP write to
    # port 0), while nps.conf's parser accepts either.
    (d / "npc.conf").write_text(f"""[common]
server_addr=127.0.0.1:{p['client_dial']}
conn_type=tcp
vkey=bench
auto_reconnection=true
crypt=false
compress=false
max_conn=1000
disconnect_timeout=60

[tcp_iperf]
mode=tcp
target_addr=127.0.0.1:{p['iperf_backend']}
server_port={p['iperf_exposed']}

[tcp_echo]
mode=tcp
target_addr=127.0.0.1:{p['echo_backend']}
server_port={p['echo_exposed']}

[udp_udpecho]
mode=udp
target_addr=127.0.0.1:{p['udp_backend']}
server_port={p['udp_exposed']}
""")
    procs.spawn([str(d / "nps")], cwd=d)
    if not wait_port(p["control"], 10):
        raise TimeoutError("nps bridge port did not open")
    procs.spawn([str(d / "npc"), "-config", str(d / "npc.conf")], cwd=d)
    # The TCP tunnels only: a UDP task has no listening TCP port to wait
    # for (the bench's own UDP probe is what exercises it).
    for port in (p["iperf_exposed"], p["echo_exposed"]):
        if not wait_port(port, 10):
            raise TimeoutError(f"nps tunnel {port} not ready")


def _start_weakproxy(procs: ArmProcs, p: dict) -> None:
    """Spawn the per-arm userspace delay proxy (fallback cells only)."""
    cmd = p.get("weakproxy_cmd")
    if cmd:
        procs.spawn(list(cmd), tool=False)
        time.sleep(0.3)


def cell_port_map(base: int, off: int, mech: str) -> dict:
    """Per-tool port band inside one cell."""
    return {
        "control": base + off + 1,
        "client_dial": base + off + 19 if mech == "weakproxy"
        else base + off + 1,
        "iperf_exposed": base + off + 2,
        "echo_exposed": base + off + 3,
        "udp_exposed": base + off + 4,
        # KCP tunnel listener (UDP): distinct within the tool band and below
        # the backend block at base+90
        "kcp_bind": base + off + 6,
        # nps's web UI (nps.conf requires one): a free slot in the band
        "nps_web": base + off + 8,
        "iperf_backend": base + 90,
        "echo_backend": base + 91,
        "udp_backend": base + 92,
        "mech": mech,
    }


def build_arms(tool: str, spec, variants: list, knobs: Knobs, p: dict,
               work: Path) -> list:
    """Return [(label, start_fn, has_udp, full_rigor, stream_ceiling)] for
    one tool in a cell."""
    arms = []
    if tool == "molehill":
        loop = list(variants)
        if spec.name == "loopback" and "mux-off" not in loop:
            loop.append("mux-off")
        for variant in loop:
            def start(v=variant):
                procs = ArmProcs(work, f"molehill {v} {spec.name}")
                try:
                    _start_weakproxy(procs, p)
                    setup_molehill(v, knobs, p, procs, work)
                except Exception:
                    # a half-started arm must not leak its processes
                    procs.kill()
                    raise
                return procs
            arms.append((f"molehill {tool_version(knobs)} ({variant})",
                         start, True, True,
                         variant_stream_ceiling(variant, knobs.pool_size)))
    else:
        setup = {"frp": setup_frp, "rathole": setup_rathole,
                 "nps": setup_nps}[tool]

        def start():
            procs = ArmProcs(work, f"{tool} {spec.name}")
            try:
                _start_weakproxy(procs, p)
                setup(knobs, p, procs, work)
            except Exception:
                procs.kill()
                raise
            return procs
        arms.append((f"{tool} {peer_version(tool, knobs)}", start, True,
                     False, None))
    return arms


def peer_cell(spec) -> bool:
    """Peers are reference points: the low-loss / low-delay cells plus the
    rate-limited ones (overhead at a bottleneck is a peer-comparison
    question); the rtt100 / loss5 / burst / jitter stories are told by the
    molehill rows."""
    return (spec.loss <= 1.0 and spec.rtt <= 10.0 and spec.jitter == 0.0) \
        or spec.rate > 0


def default_out() -> Path:
    """Results file for the current Cargo.toml version (the next tag), so a
    plain `just bench` never merges into the previous release's baseline."""
    try:
        with open(Path(__file__).parents[3] / "Cargo.toml", "rb") as fh:
            ver = tomllib.load(fh)["package"]["version"]
    except (OSError, KeyError):
        ver = "dev"
    return Path(__file__).parent / f"results-v{ver}.json"


def mixed_bulk_latency(backends, iperf_exposed: int, echo_port: int,
                       secs: int) -> dict:
    """Bulk transfer (iperf, 1 stream) and interactive latency (fresh
    connections to the echo service) CONCURRENTLY through the same client —
    the per-service mix story: does a bulk service starve an interactive
    one? (loopback cell only).

    Uses the same isolated sampler as the matrix throughput metrics, so its
    bulk number has the same window/accounting convention and cannot be
    silently nulled by a renamed field (the bug that produced a
    `KeyError: gbps_sent_window` here on every loopback arm)."""
    res: dict = {}

    def bulk():
        try:
            out = backends.run_throughput(iperf_exposed, 1, 1, secs,
                                          tag="mixed-bulk")
            res["bulk_gbps"] = out.get("gbps_sent")
            if res["bulk_gbps"] is None:
                # run_throughput reports failure in its return value, not by
                # raising: keep the reason so a null is never silent (the
                # audit rejects an unexplained null)
                res["bulk_reason"] = out.get("error", "no valid rep")
                last = (out.get("records") or [{}])[-1]
                if last.get("reason"):
                    res["bulk_reason"] += f"; last rep: {last['reason']}"
        except Exception as e:
            # keep the echo half of the metric; record why the bulk failed
            res["bulk_gbps"] = None
            res["bulk_reason"] = f"{type(e).__name__}: {e}"

    t = threading.Thread(target=bulk, daemon=True)
    t.start()
    xs = []
    failed = 0
    deadline = time.time() + secs
    while time.time() < deadline:
        t0 = time.perf_counter()
        try:
            s = socket.socket()
            s.settimeout(3.0)
            s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            s.connect(("127.0.0.1", echo_port))
            s.sendall(b"p")
            s.recv(1)
            s.close()
            xs.append((time.perf_counter() - t0) * 1000.0)
            failed = 0
        except (OSError, TimeoutError):
            failed += 1
            if failed >= 5:
                break  # path wedged; keep the bulk half of the metric
    t.join()
    if xs:
        xs.sort()
        res.update({
            "echo_p50_ms": round(xs[len(xs) // 2], 3),
            "echo_p99_ms": round(xs[int(len(xs) * 0.99) - 1], 3),
            "echo_mean_ms": round(sum(xs) / len(xs), 3),
        })
    return res


def tool_version(knobs: Knobs) -> str:
    try:
        out = subprocess.run([knobs.molehill_bin, "--version"],
                             capture_output=True, text=True,
                             check=False, timeout=15).stdout
        return next((l.split()[2] for l in out.splitlines()
                     if l.startswith("Build Version:")), "dev")
    except OSError:
        return "dev"


def peer_version(tool: str, knobs: Knobs) -> str:
    """Best-effort version string: peers print it in different places
    (frp on stderr, rathole as a 'Build Version:' line, nps as a
    'Version:' line);
    fall back to the release-version marker written by fetch_peers.py,
    then to the upstream release version constant."""
    binname = PEER_BINS[tool]
    for cand in (Path(knobs.peer_dir) / tool / binname,   # frp's subdir layout
                 Path(knobs.peer_dir) / binname):         # flat layouts
        if cand.exists():
            break
    else:
        return "0.5.0"
    try:
        r = subprocess.run([str(cand), "--version"],
                           capture_output=True, text=True, timeout=15,
                           check=False)
        out = r.stdout + r.stderr
        for line in out.splitlines():
            if line.startswith("Build Version:"):
                tok = line.split()[-1]
                if re.search(r"\d+\.\d+\.\d+", tok):
                    return tok
        m = re.search(r"\d+\.\d+\.\d+", out)
        if m:
            return m.group(0)
    except (OSError, subprocess.TimeoutExpired):
        pass
    try:  # marker written by fetch_peers.py (tools without --version)
        marker = Path(knobs.peer_dir) / f".{tool}-release-version"
        return marker.read_text().strip() or "0.5.0"
    except OSError:
        return "0.5.0"


# === arm execution ===

def _thr_fields(prefix: str, thr) -> dict:
    """Flatten one throughput sample into the results entry.

    `thr` is `Backends.run_throughput`'s dict (the isolated path) or the
    legacy tuple from `throughput()` (the mixed probe's shared server).
    The headline rate is the bytes sent over the MEASURED window — that is
    the number all tools share; the receiver's own window is reported next to
    it because at a shaped cell the backend keeps draining after the client
    stops and `sum_received.seconds` runs 50%+ longer, which alone made the
    received figure look like a different measurement.
    """
    if isinstance(thr, tuple):  # legacy (gbps, retr, min, max)
        return {f"throughput_{prefix}_gbps": thr[0],
                f"retransmits_{prefix}": thr[1],
                f"throughput_{prefix}_min_gbps": thr[2],
                f"throughput_{prefix}_max_gbps": thr[3]}
    if thr is None:  # probe not applicable to this cell (not a failure)
        return {f"throughput_{prefix}_gbps": None}
    out = {
        f"throughput_{prefix}_gbps": thr.get("gbps_sent"),
        f"retransmits_{prefix}": thr.get("retransmits"),
        f"throughput_{prefix}_min_gbps": thr.get("gbps_min"),
        f"throughput_{prefix}_max_gbps": thr.get("gbps_max"),
    }
    if thr.get("gbps_received") is not None:
        out[f"throughput_{prefix}_received_gbps"] = thr["gbps_received"]
        out[f"throughput_{prefix}_received_own_window_gbps"] = \
            thr.get("gbps_received_own_window")
        out[f"throughput_{prefix}_receiver_window_s"] = \
            thr.get("receiver_window_s")
    if thr.get("gbps_sent_only") is not None:
        out[f"throughput_{prefix}_sent_only_gbps"] = thr["gbps_sent_only"]
    if thr.get("degenerate_reps"):
        out[f"throughput_{prefix}_sender_degenerate_reps"] = \
            thr["degenerate_reps"]
    if thr.get("median_rep") is not None:
        out[f"throughput_{prefix}_median_rep"] = thr["median_rep"]
    if thr.get("per_stream_bytes"):
        out[f"throughput_{prefix}_per_stream_bytes"] = thr["per_stream_bytes"]
    if thr.get("per_stream_gbps"):
        out[f"throughput_{prefix}_per_stream_gbps"] = thr["per_stream_gbps"]
    if thr.get("reps_ok") is not None:
        out[f"throughput_{prefix}_reps_ok"] = thr["reps_ok"]
    return out


class ArmTimeout(Exception):
    """One arm exceeded its wall-clock budget (see `arm_watchdog`)."""


def arm_watchdog(seconds: float):
    """Arm a wall-clock alarm for one arm, returning its previous handler.

    A hung arm is the failure mode this guards: a wedged iperf3 client or a
    tunnel that stopped forwarding used to leave the run sitting on a
    blocking read for as long as the OS allowed, which looks identical to a
    slow cell from outside and (worse) holds the bench lock, so every later
    invocation exits immediately without producing results. The alarm makes
    the hang a recorded error instead.

    The previous handler is returned so the caller can restore it, and the
    alarm is one-shot: an arm that finishes in time never sees it.
    """
    def _fire(signum, frame):
        raise ArmTimeout(f"arm exceeded {seconds:g}s")

    previous = signal.signal(signal.SIGALRM, _fire)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    return previous


def run_arm(label: str, spec, start_fn, has_udp: bool, full_rigor: bool,
            stream_ceiling: int | None, knobs: Knobs, p: dict, mech: str,
            data: dict, out_path: Path, base: int, work: Path) -> None:
    """One arm, fully guarded: a failure records an error entry and the
    matrix moves on. Completed metrics are checkpointed immediately."""
    # Load-aware cooldown: start every arm from a quiet machine.
    wait_load_quiet(knobs.cooldown_load_factor, knobs.cooldown_max_wait_s)
    # Backends (iperf3 server + echo/udp servers) are per-ARM, not per-cell:
    # the iperf3 server is single-test and can wedge on a stalled test (rate-
    # limited cells with parallel streams — GSO-sized segments times netem's
    # packet limit buffer seconds of data) and then die with EBADF. A shared
    # server would poison every later arm of the cell with refused dials.
    backends = Backends()
    try:
        backends.start(base + 90, base + 91, base + 92, work)
    except Exception as e:  # backends failed (ports held, leaked process, ...)
        entry = {"status": "error", "error": f"Backends: {e}"}
        merge_arm(data, label, spec.name, entry, out_path)
        print(f"RESULT [{spec.name}] {label}: error: {entry['error']}",
              flush=True)
        return
    try:
        procs = start_fn()
    except Exception as e:  # setup failed (ports held, tool crashed, ...)
        entry = {"status": "error", "error": f"{type(e).__name__}: {e}"}
        merge_arm(data, label, spec.name, entry, out_path)
        print(f"RESULT [{spec.name}] {label}: error: {entry['error']}",
              flush=True)
        return
    # The arm's wall-clock span: the denominator for the framing rates, and
    # the window the CPU average covers (the sampler runs for the same span).
    arm_t0 = time.monotonic()
    # A shaped cell's probes are far slower than a loopback one, so the
    # budget scales with the test length rather than being a flat constant:
    # the sum of the per-probe timeouts plus headroom for setup and teardown.
    budget = knobs.arm_timeout(spec)
    previous_handler = arm_watchdog(budget)
    try:
        if not wait_port(p["iperf_exposed"], 25) or \
                not wait_port(p["echo_exposed"], 25):
            raise TimeoutError("exposed ports not ready")
        time.sleep(0.7)
        # molehill arms run at full rigor (the regression gate gates them);
        # peers are reference points at reduced reps/duration
        if full_rigor:
            reps = knobs.molehill_reps
            secs = knobs.molehill_secs if mech == "direct" else knobs.molehill_secs_weak
        else:
            reps = knobs.peer_reps
            secs = knobs.peer_secs if mech == "direct" else knobs.peer_secs_weak

        samples: list = []
        cpu_samples: list = []
        stop = threading.Event()
        sampler = threading.Thread(
            target=run_rss_sampler,
            args=(procs.tool_pids[0] if procs.tool_pids else 0,
                  procs.tool_pids[1] if len(procs.tool_pids) > 1 else 0,
                  stop, samples), daemon=True)
        cpu_sampler = threading.Thread(
            target=run_cpu_sampler,
            args=(procs.tool_pids[0] if procs.tool_pids else 0,
                  procs.tool_pids[1] if len(procs.tool_pids) > 1 else 0,
                  stop, cpu_samples), daemon=True)
        sampler.start()
        cpu_sampler.start()

        # Each metric is guarded independently: a failure records None and a
        # note in `partial_metrics` instead of faking a 0 or discarding the
        # other metrics of the arm.
        partial: list = []

        def metric(name: str, fn):
            try:
                v = fn()
            except Exception as e:
                partial.append(f"{name}: {type(e).__name__}: {e}")
                return None
            if v is None:  # e.g. throughput: every rep failed
                partial.append(f"{name}: no valid result")
            return v

        # Every throughput sample goes through Backends.run_throughput: it
        # bounds each iperf3 client by the test length (not a fixed 20 s a
        # shaped-cell drain can legitimately exceed) and replaces the
        # single-test server whenever a rep wedges it, so one stalled rep no
        # longer poisons the rest of the repetition budget. Raw per-rep
        # artifacts land in <work>/iperf-raw/.
        thr1 = metric("throughput_1stream",
                      lambda: backends.run_throughput(
                         p["iperf_exposed"], reps, 1, secs,
                         tag=f"{label} {spec.name}",
                         backend_port=p["iperf_backend"]))
        # The cheap probes run BEFORE the 8-stream test: on rate-limited
        # cells (netem rate + GSO-sized segments + the packet limit) eight
        # parallel iperf streams can leave the tunnel saturated —
        # echo/steady/udp/hol/churn must not inherit that state.
        lat = metric("echo_rtt",
                     lambda: latency(p["echo_exposed"]))
        steady = metric("tcp_steady_rtt", lambda: run_tcp_steady_ping(
            "127.0.0.1", p["echo_exposed"], knobs.steady_ping_count, 20))
        udp = hol_udp = udpcap = None
        if has_udp:
            udp = metric("udp_ping", lambda: run_udp_ping(
                "127.0.0.1", p["udp_exposed"], knobs.udp_count,
                knobs.udp_interval_ms, secs + 3))
            # Capacity runs BEFORE the bulk-UDP HoL probe, not after it.
            # Measured 2026-09-10 (kcp4, rate100): a 10k-datagram capacity
            # sample on a fresh UDP session delivers 100% (0% loss) at the
            # very same offered pace, while the SAME sample taken after the
            # 50 Mbit/s hol_udp blast receives 0 datagrams — and the
            # visitor's UDP path then stays dead for the rest of the arm
            # (hol_udp's own paced pinger records 100% loss too). That is a
            # finding about the forwarder's recovery after overload, and it
            # must not be reported as `udp_capacity: 0` for every arm.
            udpcap = metric("udp_capacity", lambda: udp_capacity_probe(
                p["udp_exposed"], knobs.udp_capacity_count,
                knobs.udp_capacity_pps, rate_mbit=spec.rate,
                configured_loss=spec.loss))
            hol_udp = metric("hol_udp", lambda: run_hol_probe(
                "udp", "127.0.0.1", p["udp_exposed"], knobs.hol_secs,
                bulk_rate_mbps=knobs.hol_bulk_rate_udp))
        hol = metric("hol_tcp", lambda: run_hol_probe(
            "tcp", "127.0.0.1", p["echo_exposed"], knobs.hol_secs,
            bulk_rate_mbps=knobs.hol_bulk_rate_tcp))
        # Scale + per-service mix stories only make sense unthrottled, and
        # they run BEFORE the UDP capacity blast: a saturated UDP path can
        # wedge the shared mux tunnel, and the TCP metrics must not inherit
        # that state. The scale stream count stays below the yamux ceiling
        # (count x MUX_MAX_STREAMS): an arm whose ceiling is lower than the
        # scale point is skipped for an arm whose ceiling is below it
        # (deliberately: attempting it wedges the per-arm iperf3 server and
        # poisons the mixed-workload probe that follows). The ceiling
        # model subtracts the bench's own pooled channels and the client's
        # control stream, so a count = 1 arm's ceiling (45 with the default
        # pool_size = 8) sits below the 64-stream scale point and the cell
        # is skipped for it; the count = 4 arms (237) run it. Attempting it
        # anyway does not merely fail the dial: exceeding the cap closes
        # the tunnel connection, and the 8-stream cell that runs after
        # this one then hangs against the dead tunnel (observed: every
        # iperf3 rep timing out at the harness bound).
        if spec.name == "loopback" and (stream_ceiling is None
                                        or knobs.scale_streams
                                        <= stream_ceiling):
            thr128 = metric("throughput_64streams",
                            lambda: backends.run_throughput(
                                p["iperf_exposed"], 1, knobs.scale_streams,
                                secs, tag=f"{label} {spec.name}",
                                backend_port=p["iperf_backend"]))
        else:
            thr128 = None
            if spec.name == "loopback":
                partial.append(
                    f"throughput_64streams: skipped ({knobs.scale_streams} "
                    f"streams exceed this arm's {stream_ceiling}-stream "
                    "yamux ceiling)")
        if spec.name == "loopback":
            mixed = metric("mixed_bulk_latency", lambda: mixed_bulk_latency(
                backends, p["iperf_exposed"], p["echo_exposed"], secs))
        else:
            mixed = None
        churn_ = metric("churn", lambda: churn(
            p["echo_exposed"], knobs.churn_secs, knobs.churn_concurrency))
        # 8-stream throughput is attempted on every cell, rate-limited ones
        # included. With per-rep server hygiene + a test-length-bound client
        # this is measurable even at the shaped cells (the v0.8.0 nulls came
        # from a wedged server starving every later rep, not from the path).
        thr8 = metric("throughput_8streams",
                      lambda: backends.run_throughput(
                         p["iperf_exposed"], reps, 8, secs,
                         tag=f"{label} {spec.name}",
                         backend_port=p["iperf_backend"]))
        stop.set()
        sampler.join(timeout=2)
        cpu_sampler.join(timeout=2)

        # A sampler that produced no rate must record WHY (per-rep reasons are
        # in its records); otherwise a null reaches the results file with no
        # evidence and the audit has to treat it as an unexplained hole.
        for name, thr in (("throughput_1stream", thr1),
                          ("throughput_8streams", thr8),
                          ("throughput_64streams", thr128)):
            if isinstance(thr, dict) and thr.get("gbps_sent") is None:
                detail = thr.get("error") or "no valid rep"
                partial.append(f"{name}: {detail}")
                reps = thr.get("records") or []
                last = reps[-1] if reps else {}
                if last.get("reason"):
                    partial.append(f"{name}: last rep: {last['reason']}")
        entry = {
            "status": "ok",
            # measurement endpoints, so an audit can prove the throughput
            # numbers came from the tunnel (exposed) and not the backend
            "_throughput_endpoint": "exposed",
            "_throughput_exposed_port": p["iperf_exposed"],
            "_bench_backend_port": p["iperf_backend"],
            **_thr_fields("1stream", thr1),
            **_thr_fields("8streams", thr8),
            **_thr_fields("64streams", thr128),
            "echo_rtt_ms": lat,
            "tcp_steady_rtt_ms": steady,
            "udp_rtt_ms": (udp or {}).get("rtt_ms"),
            "udp_loss_pct": (udp or {}).get("loss_pct"),
            "udp_jitter_ms": (udp or {}).get("jitter_ms"),
            "udp_max_gap_ms": (udp or {}).get("max_gap_ms"),
            "churn": churn_,
            "udp_capacity": udpcap,
            "mixed_bulk_latency": mixed,
            "hol": hol, "hol_udp": hol_udp,
            "memory_rss_kb": mem_stats(samples),
            "cpu": cpu_stats(cpu_samples),
        }
        if partial:
            entry["partial_metrics"] = partial
        # Read molehill's framing counters while the processes are still
        # alive: their logs are the source, and the delta across the arm's
        # stats lines is what turns counters into a rate.
        framing = framing_stats(procs.logs)
        if framing:
            entry["framing"] = framing
            # The attribution metric: throughput alone cannot tell "too much
            # work per frame" from "too many frames", and the engine's whole
            # cost is their product. Note the directions are counted
            # separately: a frame is written once and read once (on opposite
            # processes), so the wire rate is the written total while the
            # per-frame cost divides by the work actually done, which is the
            # written + read total. Absent for mux-off, which has no engine.
            cpu = entry.get("cpu") or {}
            cpu_pct = cpu.get("total_avg_pct") or 0.0
            written = framing.get("frames_written_delta", 0)
            read = framing.get("frames_read_delta", 0)
            nbytes = framing.get("frame_bytes_delta", 0)
            elapsed = max(1.0, time.monotonic() - arm_t0)
            if (written + read) and cpu_pct:
                entry["framing_cpu"] = {
                    "frames_per_s": round(written / elapsed, 1),
                    # Both directions contribute to the byte total, so the
                    # average body size divides by the frames *processed*
                    # (written + read), not by the written count alone.
                    "avg_frame_bytes": round(nbytes / (written + read), 1),
                    "cpu_pct_per_kframe": round(cpu_pct * 1000 / (written + read), 4),
                    "arm_seconds": round(elapsed, 1),
                }
        elif any(f"({v})" in label for v in ("mux", "mux1", "noise", "kcp4")):
            # A framed arm with no mux-stats lines at all is an INSTRUMENT
            # failure, not a metric that happens to be zero: the bench sets
            # MOLEHILL_MUX_STATS=1 for every molehill spawn, so the engine's
            # counters were either not emitted or the logs were lost. A whole
            # matrix once ran with this silently absent (the host's
            # environment dropped the env mid-session) and only the missing
            # framing_cpu column hinted at it — record the reason instead
            # (AGENTS.md §10: every failure leaves evidence). Re-assign
            # because `partial_metrics` was already attached to the entry
            # above; the list is shared, but the assignment keeps the intent
            # local to this branch.
            partial.append(
                "framing: no mux-stats lines in this arm's logs "
                "(MOLEHILL_MUX_STATS=1 is set for every spawn; the engine "
                "counters were not emitted — attribution metrics absent)")
            entry["partial_metrics"] = partial
    except ArmTimeout as e:
        # A hung arm is recorded like any other failure so the matrix moves
        # on; the partial probes it did finish are discarded because their
        # windows are not comparable to a completed arm's.
        entry = {"status": "error", "error": f"ArmTimeout: {e}"}
    except Exception as e:  # continue-on-error: record and move on
        entry = {"status": "error", "error": f"{type(e).__name__}: {e}"}
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)
        procs.kill()
        backends.stop()
    merge_arm(data, label, spec.name, entry, out_path)
    summary = json.dumps(entry)
    print(f"RESULT [{spec.name}] {label}: "
          f"{summary[:120]}{'...' if len(summary) > 120 else ''}", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--tools", default="molehill,frp,rathole,nps",
                    help="comma list: molehill,frp,rathole,nps")
    ap.add_argument("--cells",
                    default="0/0,0/10,0/100,1%/10,5%/100,2:25/10,"
                            "r100/20,r20/40,j20/10",
                    help="comma list: loss[/burst]/rtt, r<mbit>/rtt, "
                         "j<rtt>/<jitter>")
    ap.add_argument("--variants", default="mux,noise,mux1,kcp4",
                    help="molehill arms: mux (plain control), noise "
                         "(encryption), mux1 (count=1), kcp4 (carrier), "
                         "mux-off (loopback only), noise-direct (noise in "
                         "direct mode); each varies one knob")
    ap.add_argument("--out", default=str(default_out()))
    ap.add_argument("--fresh", action="store_true",
                    help="discard existing results instead of merging")
    ap.add_argument("--pool-size", type=int, default=8)
    ap.add_argument("--ab", metavar="BIN_A,BIN_B",
                    help="interleave two molehill binaries inside every cell "
                         "(round-robin over rounds) so the epoch drift that "
                         "defeats sequential before/after runs is shared by "
                         "both; comma-free paths only, and each run writes "
                         "its own --out. See AGENTS.md section 10")
    args = ap.parse_args()

    # Yield to interactive/system tasks: the matrix saturates every core it
    # can reach, and a raised nice value keeps the host responsive without
    # touching the measurements.
    with contextlib.suppress(OSError):
        os.nice(10)

    knobs = Knobs.from_env()
    knobs.pool_size = args.pool_size
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    cells = [parse_cell(c.strip()) for c in args.cells.split(",") if c.strip()]
    variants = [v.strip() for v in args.variants.split(",") if v.strip()]
    knobs.allow_port_hi = 25100 + (len(cells) - 1) * 100 + 92

    out_path = Path(args.out)
    data = {"meta": {}, "results": {}} if args.fresh else load_results(out_path)
    if args.fresh and out_path.exists():
        # a killed `--fresh` run must never be able to destroy the previous
        # results file beyond recovery
        bak = out_path.with_name(out_path.name + ".bak")
        shutil.copy2(out_path, bak)
        print(f"--fresh: previous results backed up to {bak.name}",
              flush=True)
    # cells recorded by earlier runs but not selected this time survive a
    # merge so a subset rerun never shrinks the file's cell metadata
    prior_cells = {c.get("name"): c
                   for c in data["meta"].get("cells", []) if c.get("name")}

    acquire_lock()  # refuse to run concurrently with another bench run
    work = Path(tempfile.mkdtemp(prefix="molehill-bench."))
    signal.signal(signal.SIGTERM, _handle_sigterm)
    reaped = sweep_stale(work)
    if reaped:
        print(f"reaped {reaped} stale process(es) from crashed runs",
              flush=True)
    netem = Netem()
    print(f"schema v{SCHEMA} | tools: {tools} | "
          f"cells: {[c.name for c in cells]} | netem: {netem.ok} | "
          f"out: {out_path} | work: {work}", flush=True)

    # static meta written into every checkpoint so a killed run leaves a
    # complete (not just schema) meta behind; tool_versions are only known
    # at the end and are filled in the final dump
    statics = {
        "schema": SCHEMA,
        "date": time.strftime("%Y-%m-%d"),
        "topology": "loopback visitor->server->client->backend",
        "transport": "plain tcp + noise (molehill variants)",
        "reps": knobs.molehill_reps, "peer_reps": knobs.peer_reps,
        "secs_per_rep_loopback": knobs.molehill_secs,
        "secs_per_rep_weak": knobs.molehill_secs_weak,
        "peer_secs_per_rep_loopback": knobs.peer_secs,
        "peer_secs_per_rep_weak": knobs.peer_secs_weak,
        "hol_probe_seconds": knobs.hol_secs,
        "churn_seconds": knobs.churn_secs,
        "churn_concurrency": knobs.churn_concurrency,
        "udp_capacity_datagrams": knobs.udp_capacity_count,
        "udp_capacity_pps": knobs.udp_capacity_pps,
        # the steady UDP ping's own shape: count x interval sets how long
        # the session is observed and what one "lost datagram" is worth
        # (a method parameter — AGENTS.md §10 — recorded so the loss_pct
        # denominator is auditable)
        "udp_ping_datagrams": knobs.udp_count,
        "udp_ping_interval_ms": knobs.udp_interval_ms,
        # netem rate-cell queue depth: a measurement parameter that changes
        # the result (see bench_lib.RATE_QUEUE_LIMIT), recorded so a weak
        # rate cell can be attributed to the tool rather than the shaper
        "netem_rate_limit": RATE_QUEUE_LIMIT,
        "latency_samples": 300,
        "memory_samples_interval_s": 0.5,
        "hostname": socket.gethostname(),
        "kernel": platform.release(),
        "cpu": next((l.split(":", 1)[1].strip()
                     for l in Path("/proc/cpuinfo").read_text().splitlines()
                     if l.startswith("model name")), "?"),
    }

    meta_cells = []
    exit_code = 0
    try:
        for ci, spec in enumerate(cells):
            base = 25100 + ci * 100
            mech = "direct"
            if spec.weak:
                if netem.on(spec):
                    mech = "netem"
                elif spec.loss or spec.burst:
                    print(f"skip cell {spec.name}: loss simulation needs "
                          "netem (CAP_NET_ADMIN)", file=sys.stderr)
                    continue
                else:
                    mech = "weakproxy"
            meta_cells.append({"name": spec.name, "loss_pct": spec.loss,
                               "burst_pct": spec.burst, "rate_mbit": spec.rate,
                               "rtt_ms": spec.rtt, "jitter_ms": spec.jitter,
                               "mech": mech, "loss_model": spec.loss_model})
            # keep mid-run checkpoints informative: the cells seen so far and
            # the static meta are part of every merge_arm dump
            cur_names = {c["name"] for c in meta_cells}
            data.setdefault("meta", {}).update(statics)
            # canonical cell order (loopback first, then by shaping params):
            # targeted re-runs must not shuffle the order the charts plot
            data["meta"]["cells"] = sorted(
                [c for n, c in prior_cells.items() if n not in cur_names]
                + meta_cells, key=cell_sort_key)
            try:
                for tool in tools:
                    if tool != "molehill" and not peer_cell(spec):
                        continue
                    off = {"molehill": 0, "frp": 20, "rathole": 40,
                           "nps": 80}[tool]
                    p = cell_port_map(base, off, mech)
                    if mech == "weakproxy":
                        p["weakproxy_cmd"] = [
                            sys.executable,
                            str(Path(__file__).parent / "weakproxy.py"),
                            str(p["client_dial"]),
                            f"127.0.0.1:{p['control']}", f"{spec.rtt:g}"]
                    # --ab: run this cell's molehill arms once per binary,
                    # then repeat and average per round; the two binaries
                    # alternate within each round so both sample the same
                    # epochs (sequential before/after runs are defeated by
                    # ~12% epoch drift on the shaped cells — see HANDOFF).
                    # Peers never take part: they are reference points.
                    ab_bins = ([b for b in args.ab.split(",") if b]
                               if args.ab and tool == "molehill" else [None])
                    if len(ab_bins) > 2:
                        raise SystemExit("--ab takes exactly two binaries")
                    # Unique label per binary (see `ab_suffixes`): two
                    # worktree builds share the basename `molehill`, and a
                    # colliding suffix would make the two sides overwrite
                    # each other and `ab_compare` see no pair at all.
                    ab_label = ab_suffixes(ab_bins)
                    # Record which label is which path in the results meta:
                    # `ab_compare` sorts the pair by label, so a reader who
                    # assumes "first printed = A = the left --ab entry" can
                    # silently read a verdict backwards (it happened: a
                    # winning change was read as a regression and reverted).
                    data["meta"]["ab_bin_paths"] = {
                        ab_label[b]: str(Path(b).resolve()) for b in ab_bins
                    }
                    for ab_round in range(1, knobs.molehill_reps + 1):
                        for ab_bin in ab_bins:
                            if ab_bin is not None:
                                # `knobs.molehill_bin` is what every spawn
                                # site reads, so this alone swaps the
                                # interleave's binary.
                                knobs.molehill_bin = ab_bin
                            # The label must distinguish the two binaries:
                            # merge_arm keys results by (tool, cell), so two
                            # arms sharing a label would overwrite each
                            # other in the same file. The short basename
                            # keeps the chart's legend readable.
                            suffix = ("" if ab_bin is None
                                      else f" (ab{ab_round}:"
                                           f"{ab_label[ab_bin]})")
                            for (label, start_fn, has_udp, full_rigor,
                                 stream_ceiling) in build_arms(
                                    tool, spec, variants, knobs, p, work):
                                run_arm(label + suffix, spec, start_fn,
                                        has_udp, full_rigor, stream_ceiling,
                                        knobs, p, mech, data, out_path, base,
                                        work)
            finally:
                netem.off()
    except KeyboardInterrupt:
        print("interrupted — completed arms are saved", flush=True)
        exit_code = 130
    finally:
        release_lock()
        cur_cells = {c["name"] for c in meta_cells}
        merged_cells = sorted(
            [c for n, c in prior_cells.items() if n not in cur_cells]
            + meta_cells, key=cell_sort_key)
        # same merge rule for tool_versions: keep tools from earlier runs
        tv = dict(data["meta"].get("tool_versions") or {})
        tv.update({t: tool_version(knobs) if t == "molehill"
                   else peer_version(t, knobs) for t in tools})
        data["meta"].update(statics)
        data["meta"].update({
            "cells": merged_cells,
            "tool_versions": tv,
        })
        dump_results(data, out_path)
        arms = sum(1 for t in data["results"].values() for _ in t.values())
        print(f"matrix complete: {arms} arm results -> {out_path}", flush=True)
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
