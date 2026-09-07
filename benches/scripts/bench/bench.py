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
- bore's `--to` takes a bare host (port 7835 implied): in weakproxy cells its
  control port moves to 127.0.0.2 so the proxy can hold 127.0.0.1:7835
- knobs are env-tunable (see bench_lib.Knobs): molehill runs at full rigor
  (3 reps), peers at reduced rigor (1 rep) — the regression gate only gates
  molehill's default (mux) row
"""
import argparse
import contextlib
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
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from bench_lib import (
    SCHEMA,
    Backends,
    Knobs,
    Netem,
    acquire_lock,
    dump_results,
    latency,
    load_results,
    mem_stats,
    merge_arm,
    parse_cell,
    record_pid,
    release_lock,
    run_rss_sampler,
    sweep_stale,
    throughput,
    wait_port,
)
from hol_probe import run_hol_probe
from tcp_steady_ping import run_tcp_steady_ping
from udp_ping import run_udp_ping


def _handle_sigterm(signum, frame):
    """SIGTERM takes the Ctrl-C path: arms are killed, the netem qdisc is
    removed and the full meta is checkpointed before exit."""
    raise KeyboardInterrupt


TCP_ONLY = {"bore"}  # peers without UDP forwarding (UDP metrics omitted)
PEER_BINS = {"frp": "frps", "bore": "bore", "rathole": "rathole"}


class ArmProcs:
    """Process group for one arm: spawn with per-arm logs, kill as a group."""

    def __init__(self, work: Path, label: str):
        self.work = work
        self.label = label
        self.pids = []
        self.tool_pids = []  # the proxied tool's own pids (RSS sampler input)

    def spawn(self, cmd: list, tool: bool = True) -> None:
        log = self.work / f"{self.label.replace(' ', '_').replace('/', '_')}.log"
        with open(log, "ab") as f:
            pid = subprocess.Popen(cmd, stdout=f, stderr=f).pid
        record_pid(self.work, pid)
        self.pids.append(pid)
        if tool:
            self.tool_pids.append(pid)

    def kill(self) -> None:
        for pid in self.pids:
            with contextlib.suppress(OSError):
                os.kill(pid, signal.SIGKILL)
        self.pids.clear()
        time.sleep(0.3)


_NOISE_KEYS = None


def noise_keys() -> tuple:
    """Generate once per run and cache a Noise keypair via `--genkey`."""
    global _NOISE_KEYS
    if _NOISE_KEYS is None:
        out = subprocess.run([knobs_bin(), "--genkey"], capture_output=True,
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


def molehill_config(work: Path, variant: str, knobs: Knobs, p: dict) -> Path:
    """Write server/client tomls for one molehill variant; returns config dir."""
    d = work / "molehill"; d.mkdir(exist_ok=True)
    mux = "true" if variant != "mux-off" else "false"
    if variant == "noise":
        transport = "noise"
    elif variant == "tls":
        transport = "tls"
    else:
        transport = "tcp"
    noise_s = noise_c = ""
    if transport == "noise":
        priv, pub = noise_keys()
        noise_s = f'[server.transport.noise]\nlocal_private_key = "{priv}"\n'
        noise_c = f'[client.transport.noise]\nremote_public_key = "{pub}"\n'
    tls_s = tls_c = ""
    if transport == "tls":
        # repo-owned self-signed PKI (examples/tls); hostname check uses the
        # configured name, so dialing 127.0.0.1 is fine
        tls_dir = Path(__file__).parents[3] / "examples" / "tls"
        tls_s = (f'[server.transport.tls]\npkcs12 = "{tls_dir / "identity.pfx"}"'
                 f'\npkcs12_password = "1234"\n')
        tls_c = (f'[client.transport.tls]\ntrusted_root = "{tls_dir / "rootCA.crt"}"'
                 f'\nhostname = "localhost"\n')
    (d / "server.toml").write_text(f"""[server]
bind_addr = "127.0.0.1:{p['control']}"
default_token = "bench"
allow_ports = ["25100-{knobs.allow_port_hi}"]
[server.transport]
type = "{transport}"
{noise_s}{tls_s}""")
    (d / "client.toml").write_text(f"""[client]
remote_addr = "127.0.0.1:{p['client_dial']}"
default_token = "bench"
mux = {mux}
[client.transport]
type = "{transport}"
{noise_c}{tls_c}
[client.services.iperf]
local_addr = "127.0.0.1:{p['iperf_backend']}"
remote_bind_addr = "127.0.0.1:{p['iperf_exposed']}"
pool_size = {knobs.pool_size}

[client.services.echo]
local_addr = "127.0.0.1:{p['echo_backend']}"
remote_bind_addr = "127.0.0.1:{p['echo_exposed']}"
pool_size = {knobs.pool_size}

[client.services.udpecho]
type = "udp"
local_addr = "127.0.0.1:{p['udp_backend']}"
remote_bind_addr = "127.0.0.1:{p['udp_exposed']}"
pool_size = 2
udp_buffer_size = 2048
udp_idle_timeout = 60
udp_sendq_size = 1024
""")
    return d


def start_molehill(procs: ArmProcs, d: Path) -> None:
    procs.spawn([knobs_bin(), "--server", str(d / "server.toml")])
    procs.spawn([knobs_bin(), "--client", str(d / "client.toml")])


_KNOBS = {"bin": None}


def knobs_bin() -> str:
    return _KNOBS["bin"]


def setup_molehill(variant: str, knobs: Knobs, p: dict, procs: ArmProcs,
                   work: Path) -> None:
    d = molehill_config(work, variant, knobs, p)
    start_molehill(procs, d)


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


def setup_bore(knobs: Knobs, p: dict, procs: ArmProcs, work: Path) -> None:
    """TCP-only: two `bore local` instances (iperf + echo), no UDP arm."""
    peer = Path(knobs.peer_dir) / "bore"
    if p["mech"] == "weakproxy":
        procs.spawn([str(peer), "server", "--bind-addr", "127.0.0.2",
                     "--bind-tunnels", "127.0.0.1",
                     "--min-port", str(p["iperf_exposed"] - 2),
                     "--max-port", str(p["echo_exposed"] + 2)])
    else:
        procs.spawn([str(peer), "server", "--bind-addr", "127.0.0.1",
                     "--min-port", str(p["iperf_exposed"] - 2),
                     "--max-port", str(p["echo_exposed"] + 2)])
    for _ in range(50):
        if wait_port(7835, 0.5):
            break
    for exposed, backend in ((p["iperf_exposed"], p["iperf_backend"]),
                             (p["echo_exposed"], p["echo_backend"])):
        procs.spawn([str(peer), "local", str(backend), "--to", "127.0.0.1",
                     "--port", str(exposed)])
        if not wait_port(exposed, 3):
            raise TimeoutError(f"bore tunnel {exposed} not ready")


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
        "iperf_backend": base + 90,
        "echo_backend": base + 91,
        "udp_backend": base + 92,
        "mech": mech,
    }


def build_arms(tool: str, spec, variants: list, knobs: Knobs, p: dict,
               work: Path) -> list:
    """Return [(label, start_fn, has_udp, full_rigor)] for one tool in a cell."""
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
                         start, True, True))
    else:
        setup = {"frp": setup_frp, "rathole": setup_rathole,
                 "bore": setup_bore}[tool]
        has_udp = tool not in TCP_ONLY

        def start():
            procs = ArmProcs(work, f"{tool} {spec.name}")
            try:
                _start_weakproxy(procs, p)
                setup(knobs, p, procs, work)
            except Exception:
                procs.kill()
                raise
            return procs
        arms.append((f"{tool} {peer_version(tool, knobs)}", start, has_udp,
                     False))
    return arms


def tool_version(knobs: Knobs) -> str:
    try:
        out = subprocess.run([knobs_bin(), "--version"],
                             capture_output=True, text=True,
                             check=False).stdout
        return next((l.split()[2] for l in out.splitlines()
                     if l.startswith("Build Version:")), "dev")
    except OSError:
        return "dev"


def peer_version(tool: str, knobs: Knobs) -> str:
    """Best-effort version string: peers print it in different places
    (frp on stderr, rathole as a 'Build Version:' line, bore on stdout);
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

def run_arm(label: str, spec, start_fn, has_udp: bool, full_rigor: bool,
            knobs: Knobs, p: dict, mech: str, data: dict,
            out_path: Path) -> None:
    """One arm, fully guarded: a failure records an error entry and the
    matrix moves on. Completed metrics are checkpointed immediately."""
    try:
        procs = start_fn()
    except Exception as e:  # setup failed (ports held, tool crashed, ...)
        entry = {"status": "error", "error": f"{type(e).__name__}: {e}"}
        merge_arm(data, label, spec.name, entry, out_path)
        print(f"RESULT [{spec.name}] {label}: error: {entry['error']}",
              flush=True)
        return
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
        stop = threading.Event()
        sampler = threading.Thread(
            target=run_rss_sampler,
            args=(procs.tool_pids[0] if procs.tool_pids else 0,
                  procs.tool_pids[1] if len(procs.tool_pids) > 1 else 0,
                  stop, samples), daemon=True)
        sampler.start()

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

        thr1 = metric("throughput_1stream", lambda: throughput(
            reps, 1, secs, p["iperf_exposed"]))
        thr8 = metric("throughput_8streams", lambda: throughput(
            reps, 8, secs, p["iperf_exposed"]))
        lat = metric("echo_rtt",
                     lambda: latency(p["echo_exposed"]))
        steady = metric("tcp_steady_rtt", lambda: run_tcp_steady_ping(
            "127.0.0.1", p["echo_exposed"], 200, 20))
        udp = hol_udp = None
        if has_udp:
            udp = metric("udp_ping", lambda: run_udp_ping(
                "127.0.0.1", p["udp_exposed"], knobs.udp_count,
                knobs.udp_interval_ms, secs + 3))
            hol_udp = metric("hol_udp", lambda: run_hol_probe(
                "udp", "127.0.0.1", p["udp_exposed"], knobs.hol_secs,
                bulk_rate_mbps=knobs.hol_bulk_rate_udp))
        hol = metric("hol_tcp", lambda: run_hol_probe(
            "tcp", "127.0.0.1", p["echo_exposed"], knobs.hol_secs,
            bulk_rate_mbps=knobs.hol_bulk_rate_tcp))
        stop.set()
        sampler.join(timeout=2)

        entry = {
            "status": "ok",
            "throughput_1stream_gbps": thr1[0] if thr1 else None,
            "retransmits_1stream": thr1[1] if thr1 else None,
            "throughput_8streams_gbps": thr8[0] if thr8 else None,
            "retransmits_8streams": thr8[1] if thr8 else None,
            "echo_rtt_ms": lat,
            "tcp_steady_rtt_ms": steady,
            "udp_rtt_ms": (udp or {}).get("rtt_ms"),
            "udp_loss_pct": (udp or {}).get("loss_pct"),
            "udp_jitter_ms": (udp or {}).get("jitter_ms"),
            "udp_max_gap_ms": (udp or {}).get("max_gap_ms"),
            "hol": hol, "hol_udp": hol_udp,
            "memory_rss_kb": mem_stats(samples),
        }
        if partial:
            entry["partial_metrics"] = partial
    except Exception as e:  # continue-on-error: record and move on
        entry = {"status": "error", "error": f"{type(e).__name__}: {e}"}
    finally:
        procs.kill()
    merge_arm(data, label, spec.name, entry, out_path)
    summary = json.dumps(entry)
    print(f"RESULT [{spec.name}] {label}: "
          f"{summary[:120]}{'...' if len(summary) > 120 else ''}", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--tools", default="molehill,frp,rathole,bore",
                    help="comma list: molehill,frp,rathole,bore")
    ap.add_argument("--cells",
                    default="0/0,0/10,0/100,1%/10,5%/100,2:25/10",
                    help="comma list: loss[/burst]/rtt or r<mbit>/rtt")
    ap.add_argument("--variants", default="mux,noise,tls",
                    help="molehill arms: mux,noise,tls,mux-off "
                         "(mux-off: loopback only)")
    ap.add_argument("--out",
                    default=str(Path(__file__).parent / "results-v0.7.2.json"))
    ap.add_argument("--fresh", action="store_true",
                    help="discard existing results instead of merging")
    ap.add_argument("--pool-size", type=int, default=8)
    args = ap.parse_args()

    knobs = Knobs.from_env()
    knobs.pool_size = args.pool_size
    _KNOBS["bin"] = knobs.molehill_bin
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
                               "rtt_ms": spec.rtt, "mech": mech,
                               "loss_model": spec.loss_model})
            # keep mid-run checkpoints informative: the cells seen so far and
            # the static meta are part of every merge_arm dump
            cur_names = {c["name"] for c in meta_cells}
            data.setdefault("meta", {}).update(statics)
            data["meta"]["cells"] = (
                [c for n, c in prior_cells.items() if n not in cur_names]
                + meta_cells)
            try:
                backends = Backends()
                backends.start(base + 90, base + 91, base + 92, work)
            except Exception as e:
                # a broken backend cell must not abort the whole matrix:
                # record every arm of the cell as an error and move on
                print(f"cell {spec.name}: backends unavailable ({e}); "
                      "recording arms as errors", file=sys.stderr)
                for tool in tools:
                    off = {"molehill": 0, "frp": 20, "rathole": 40,
                           "bore": 60}[tool]
                    p = cell_port_map(base, off, mech)
                    for label, _, _, _ in build_arms(tool, spec, variants,
                                                     knobs, p, work):
                        merge_arm(data, label, spec.name,
                                  {"status": "error",
                                   "error": f"Backends: {e}"}, out_path)
                netem.off()
                continue
            try:
                for tool in tools:
                    off = {"molehill": 0, "frp": 20, "rathole": 40,
                           "bore": 60}[tool]
                    p = cell_port_map(base, off, mech)
                    if tool == "bore" and mech == "weakproxy":
                        # bore's control port is fixed at 7835; the weakproxy
                        # holds 127.0.0.1:7835 so the server moves to .2
                        p["weakproxy_cmd"] = [
                            sys.executable,
                            str(Path(__file__).parent / "weakproxy.py"),
                            "7835", "127.0.0.2:7835", f"{spec.rtt:g}"]
                        p["client_dial"] = 7835
                    elif mech == "weakproxy":
                        p["weakproxy_cmd"] = [
                            sys.executable,
                            str(Path(__file__).parent / "weakproxy.py"),
                            str(p["client_dial"]),
                            f"127.0.0.1:{p['control']}", f"{spec.rtt:g}"]
                    for label, start_fn, has_udp, full_rigor in build_arms(
                            tool, spec, variants, knobs, p, work):
                        run_arm(label, spec, start_fn, has_udp, full_rigor,
                                knobs, p, mech, data, out_path)
            finally:
                backends.stop()
                netem.off()
    except KeyboardInterrupt:
        print("interrupted — completed arms are saved", flush=True)
        exit_code = 130
    finally:
        release_lock()
        cur_cells = {c["name"] for c in meta_cells}
        merged_cells = ([c for n, c in prior_cells.items() if n not in cur_cells]
                        + meta_cells)
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
