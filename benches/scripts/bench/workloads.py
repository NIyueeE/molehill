#!/usr/bin/env python3
"""The model's workloads: what is offered, and what it is offered to.

One function per workload kind, each returning **cells** — a scenario may
measure more than one thing (a UDP ladder measures one cell per offered rate),
and a cell is what the analysis aggregates and the verdict compares.

The discipline every kind keeps, in the same shape:

* **the counters bracket the payload.** `measured()` snapshots the system
  before and after exactly the process that carried the workload, so a byte
  ratio's numerator and denominator cover the same seconds.
* **the rate's denominator is the instrument's own window, and it is stated.**
  A bulk rate is the receiver's post-warm-up window (iperf3's), a round-trip
  rate is the probe's own measured window (not the interpreter's start), and a
  datagram rate is the probe's receive window (which for a blast includes the
  drain). Counter-derived ratios use the counters' window. Two conventions
  never share a denominator silently.
* **a failure is typed and an absence is typed.** A cell that could not be
  measured carries `ok=False` and the instrument's own reason; a metric the
  instrument cannot produce is listed in `unavailable` with a reason, never
  written as zero.
"""

from __future__ import annotations

import contextlib
import json
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

import instruments as inst
import lib
import model
import topology

HERE = Path(__file__).resolve().parent
RR_PROBE = HERE / "probes" / "rr.py"
UDP_PROBE = HERE / "probes" / "udp.py"

#: The backend inside the client namespace. It announces every accepted peer
#: (the transparency evidence: on L3 the backend must see the *visitor*), and
#: echoes both protocols so one process serves every arm and every workload —
#: the same backend, by three addresses, is what makes the arms comparable.
ECHO_SRC = """
import socket, sys, threading

port = int(sys.argv[1])
udp_port = int(sys.argv[2])


def udp_echo():
    srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # The sink must not be the loss: a blast's drops have to be the path's.
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4 << 20)
    srv.bind(("0.0.0.0", udp_port))
    while True:
        data, addr = srv.recvfrom(65535)
        srv.sendto(data, addr)


threading.Thread(target=udp_echo, daemon=True).start()


def serve(conn, addr):
    sys.stdout.write(f"PEER {addr[0]}:{addr[1]}\\n")
    sys.stdout.flush()
    with conn:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
# 0.0.0.0 because the three arms reach this one process by three addresses.
srv.bind(("0.0.0.0", port))
srv.listen(128)
print(f"ECHO READY {port}", flush=True)
while True:
    conn, addr = srv.accept()
    threading.Thread(target=serve, args=(conn, addr), daemon=True).start()
"""


#: The iperf3 client's own bound: the soak model's formula, so a shared
#: instrument cannot time out on one model and not the other.
def iperf_timeout(secs: float) -> float:
    return max(secs * 2.0 + 6.0, secs + 20.0)


@dataclass
class Cell:
    """One measured thing: its metrics, its evidence, and what is missing."""

    cell: str
    ok: bool
    metrics: dict = field(default_factory=dict)
    #: metric id -> {"reason": ...}: a typed absence, never a zero.
    unavailable: dict = field(default_factory=dict)
    evidence: dict = field(default_factory=dict)
    reason: str = ""

    def absent(self, metric: str, reason: str) -> None:
        """Record that this metric could not be measured, and why."""
        self.unavailable[metric] = {"reason": reason}


@dataclass
class Spot:
    """Where one cell was measured: its context, its counter window, its cell."""

    ctx: Ctx
    window: dict
    cell: str


@dataclass
class Ctx:
    """Everything a workload needs, so no workload takes a long argument list."""

    topo: topology.Topology
    pids_of: object
    work: Path
    arm: model.Arm
    probe_startup_s: float = 0.0
    #: The backend's announced peers, read after each cell: on an L3 arm the
    #: backend must see the *visitor*, and on L4 the client. It is the
    #: transparency evidence, recorded beside the numbers rather than assumed.
    peers_of: object = None


class Backend:
    """The one echoing backend, inside the client namespace."""

    def __init__(self, topo: topology.Topology, work: Path):
        self.topo, self.work = topo, work
        self.log = work / "backend.log"
        self.proc: subprocess.Popen | None = None

    def start(self) -> None:
        argv = self.topo.ns_argv(
            topology.CLI_NS,
            [
                sys.executable,
                "-c",
                ECHO_SRC,
                str(topology.ECHO_PORT),
                str(topology.UDP_PORT),
            ],
        )
        with self.log.open("ab") as fh:
            self.proc = subprocess.Popen(argv, stdout=fh, stderr=fh)

    def wait_ready(self, timeout: float = 20.0) -> bool:
        end = time.time() + timeout
        while time.time() < end:
            with contextlib.suppress(OSError):
                if "ECHO READY" in self.log.read_text(errors="replace"):
                    return True
            if self.proc is not None and self.proc.poll() is not None:
                return False
            time.sleep(0.1)
        return False

    def peers(self) -> list:
        """Every `PEER ip:port` the backend announced so far."""
        with contextlib.suppress(OSError):
            return [
                ln.split(" ", 1)[1].strip()
                for ln in self.log.read_text(errors="replace").splitlines()
                if ln.startswith("PEER ")
            ]
        return []

    def stop(self) -> None:
        if self.proc is None:
            return
        with contextlib.suppress(OSError):
            self.proc.kill()
        with contextlib.suppress(Exception):
            self.proc.wait(timeout=5)


@contextlib.contextmanager
def iperf_sink(ctx: Ctx, tag: str, port: int):
    """A one-off iperf3 server inside the client namespace.

    `-1` handles exactly one test and exits: iperf3's server is single-test by
    design, and a wedged one turns every later sample into a timeout. The bind
    is the arm's `backend_bind`, which is not cosmetic — an iperf3 UDP server
    left on the wildcard learns the visitor as its peer, `connect()`s outbound
    and then answers nothing addressed to the owned address (measured: the run
    ends on an ICMP port-unreachable with `OutDatagrams 1 / NoPorts 1`).
    """
    log = ctx.work / f"iperf3-server-{ctx.arm.id}-{tag}.log"
    with log.open("ab") as fh:
        proc = subprocess.Popen(
            ctx.topo.ns_argv(
                topology.CLI_NS,
                [
                    "iperf3",
                    "-s",
                    "-1",
                    "-B",
                    ctx.arm.backend_bind,
                    "-p",
                    str(port),
                ],
            ),
            stdout=fh,
            stderr=fh,
        )
    try:
        yield proc
    finally:
        with contextlib.suppress(OSError):
            proc.kill()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)
        time.sleep(0.2)


def wait_listener(port: int, timeout: float = 15.0) -> bool:
    """Is the backend listening inside the client namespace?

    Always the *TCP* listener, even for a UDP test: iperf3's server opens its
    TCP control socket at startup and the datagram socket only when a test
    begins, so waiting on a UDP socket waits for something that does not exist
    yet - measured, as a ladder that reported "the UDP sink never listened" on
    every arm while the sink was there.
    """
    end = time.time() + timeout
    while time.time() < end:
        r = topology.run(
            topology.Topology.ns_argv(
                topology.CLI_NS, ["ss", "-Htln", f"sport = :{port}"]
            ),
            check=False,
        )
        if r.stdout.strip():
            return True
        time.sleep(0.1)
    return False


@contextlib.contextmanager
def measured(ctx: Ctx, port: int, kind: str = "tcp"):
    """Counters around exactly one workload: the window every ratio is over."""
    before = inst.snapshot(ctx.topo, ctx.pids_of())
    with (
        inst.ProcWatch(ctx.pids_of) as proc,
        inst.SocketWatch(ctx.topo, port, kind) as sock,
    ):
        window: dict = {"started": time.time()}
        try:
            yield window
        finally:
            after = inst.snapshot(ctx.topo, ctx.pids_of())
            window["sample"] = inst.delta(before, after)
            window["peaks"] = dict(proc.peak)
            window["sockets"] = dict(sock.peak)


def _evidence(ctx: Ctx, window: dict, extra: dict | None = None) -> dict:
    """The counters and commands a reader needs to audit one cell."""
    sample = window.get("sample", {})
    wire = sample.get("wire", {})
    peaks = window.get("peaks", {})
    sockets = window.get("sockets", {})
    out = {
        "window_s": sample.get("elapsed_s"),
        "cpu_s": sample.get("cpu_s"),
        "cpu_s_total": sample.get("cpu_s_total"),
        "syscalls": sample.get("syscalls"),
        "wire": {
            key: wire.get(key)
            for key in (
                topology.VISITOR_IFACE,
                f"{topology.SRV_NS}/v-srv",
                topology.TUNNEL_IFACE,
            )
        },
        "drops": sample.get("drops"),
        "tcp": sample.get("tcp"),
        "peaks": peaks,
        "sockets": sockets,
        "probe_startup_s": ctx.probe_startup_s,
    }
    if extra:
        out |= extra
    if ctx.peers_of is not None:
        out["backend_peers"] = sorted(set(ctx.peers_of()))
    return out


#: The metrics that describe the *tool's* processes. The control arm has none,
#: so they are recorded as unavailable with a reason rather than as zero - a
#: zero would read as "free", which is the one thing the control cannot be.
TOOL_ONLY_METRICS = (
    "cpu_s_per_gbit",
    "cpu_cores",
    "bytes_per_syscall",
    "syscalls_per_s",
    "rss_peak_mib",
    "fds_peak",
    "threads_peak",
    "service_sockets_peak",
)


def _counter_cell(spot: Spot, metrics: dict, extra: dict | None = None) -> Cell:
    merged = dict(metrics)
    window = spot.window
    merged |= inst.counter_metrics(window["sample"], window.get("peaks", {}))
    if window.get("sockets"):
        merged["service_sockets_peak"] = window["sockets"].get("server")
    unavailable: dict = {}
    if not spot.ctx.arm.is_tool:
        reason = "the control arm runs no tool processes"
        for metric in TOOL_ONLY_METRICS:
            merged.pop(metric, None)
            unavailable[metric] = {"reason": reason}
    return Cell(
        cell=spot.cell,
        ok=True,
        metrics=merged,
        unavailable=unavailable,
        evidence=_evidence(spot.ctx, window, extra),
    )


def _dial(ctx: Ctx, params: dict) -> lib.IperfDial:
    return lib.IperfDial(
        params["port"],
        host=ctx.arm.dial_host,
        argv_prefix=("ip", "netns", "exec", topology.VIS_NS),
        omit=params.get("omit", 2),
        udp=params.get("udp", False),
        bitrate=params.get("bitrate", ""),
        length=params.get("size", 0) if params.get("udp") else 0,
    )


def _iperf_cell(spot: Spot, result: dict, metrics: dict) -> Cell:
    if not result.get("ok"):
        extra = {"cmd": result.get("artifact"), "exit": result.get("exit")}
        return Cell(
            cell=spot.cell,
            ok=False,
            reason=result.get("reason", "unknown iperf3 failure"),
            evidence=_evidence(spot.ctx, spot.window, extra),
        )
    extra = {
        "bytes_sent": result.get("bytes_sent"),
        "bytes_received": result.get("bytes_received"),
        "sender_accounting_degenerate": result.get("sender_accounting_degenerate"),
        "receiver_window_s": result.get("receiver_window_s"),
        "retransmits_iperf": result.get("retransmits"),
        "per_stream_gbps": result.get("per_stream_gbps"),
        "udp": result.get("udp"),
        "artifact": result.get("artifact"),
    }
    built = _counter_cell(spot, metrics, extra)
    built.evidence["iperf_wall_s"] = result.get("wall_s")
    return built


def run_bulk(ctx: Ctx, params: dict) -> list:
    """`-P streams` iperf3 TCP through the arm, one cell."""
    port = topology.IPERF_PORT
    tag = f"P{params['streams']}"
    with (
        measured(ctx, topology.ECHO_PORT) as window,
        iperf_sink(ctx, tag, port),
    ):
        if not wait_listener(port):
            return [Cell("", False, reason="the iperf3 sink never listened")]
        result = lib.iperf_result(
            _dial(ctx, {**params, "port": port}),
            params["streams"],
            params["secs"],
            iperf_timeout(params["secs"]),
            ctx.work / "iperf-raw" / f"{ctx.arm.id}-{tag}",
        )
    metrics = {"throughput_gbps": result.get("gbps_received_own_window")}
    return [_iperf_cell(Spot(ctx, window, tag), result, metrics)]


def run_bulk_pair(ctx: Ctx, params: dict) -> list:
    """Two single-stream bulk runs at once, one per service (or L3 claim).

    This separates "the architecture costs this much per packet" from "one
    forwarding unit carries this much": if two services together carry twice
    what one carries, the ceiling is per unit and parallelism is the lever; if
    they do not, it is the host's per-packet cost and no shape of the
    configuration moves it.
    """
    ports = (topology.IPERF_PORT, topology.IPERF2_PORT)
    results: list = [None, None]
    with measured(ctx, topology.ECHO_PORT) as window:
        sinks = [iperf_sink(ctx, f"pair-{p}", p) for p in ports]
        try:
            for m in sinks:
                m.__enter__()
            if not all(wait_listener(p) for p in ports):
                return [Cell("", False, reason="an iperf3 sink never listened")]

            def worker(index: int, port: int) -> None:
                results[index] = lib.iperf_result(
                    _dial(ctx, {**params, "port": port}),
                    params["streams"],
                    params["secs"],
                    iperf_timeout(params["secs"]),
                    ctx.work / "iperf-raw" / f"{ctx.arm.id}-pair-{port}",
                )

            threads = [
                threading.Thread(target=worker, args=(i, p))
                for i, p in enumerate(ports)
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
        finally:
            for m in sinks:
                m.__exit__(None, None, None)
    ok = all(r and r.get("ok") for r in results)
    per_run = [
        {
            "port": port,
            "gbps": (r or {}).get("gbps_received_own_window"),
            "ok": bool(r and r.get("ok")),
            "reason": (r or {}).get("reason"),
        }
        for port, r in zip(ports, results, strict=True)
    ]
    total = sum(r["gbps"] or 0 for r in per_run)
    cell = _counter_cell(
        Spot(ctx, window, "pair"),
        {"throughput_gbps": round(total, 4) if ok else None},
        {"runs": per_run},
    )
    cell.ok = ok
    if not ok:
        cell.reason = "; ".join(
            f"port {r['port']}: {r['reason']}" for r in per_run if not r["ok"]
        )
    return [cell]


def _probe_cell(spot: Spot, result: dict, metrics: dict, log: Path) -> Cell:
    built = _counter_cell(spot, metrics, {"probe_log": str(log)})
    raw = ("latencies_us", "rtt_us", "gaps_us")
    built.evidence["probe"] = {k: v for k, v in result.items() if k not in raw}
    return built


def run_rr(ctx: Ctx, params: dict) -> list:
    """Strict request/response round trips from the visitor namespace."""
    scale = params["connections"] * params["requests"]
    log = ctx.work / f"rr-{ctx.arm.id}-c{params['connections']}.log"
    argv = ctx.topo.ns_argv(
        topology.VIS_NS,
        [
            sys.executable,
            str(RR_PROBE),
            "--target",
            f"{ctx.arm.dial_host}:{topology.ECHO_PORT}",
            "--connections",
            str(params["connections"]),
            "--requests",
            str(params["requests"]),
            "--size",
            str(params["size"]),
            "--max-s",
            str(params.get("max_s", 0)),
            *(["--fresh"] if params.get("fresh") else []),
        ],
    )
    with measured(ctx, topology.ECHO_PORT) as window:
        result, rc, wall = _spawn_probe(argv, log, timeout=300)
    cell = f"c{params['connections']}"
    if not result:
        return [Cell(cell, False, reason=f"the probe produced no result (exit {rc})")]
    ok = result.get("ok", 0)
    metrics = {
        "rate_per_s": round(ok / result["wall_s"], 1) if result.get("wall_s") else None,
        "rtt_samples": len(result.get("latencies_us", [])),
        "rtt_p50_ms": _ms(inst.pct(result.get("latencies_us", []), 0.50)),
        "rtt_p99_ms": _ms(inst.pct(result.get("latencies_us", []), 0.99)),
        "rtt_max_ms": (
            _ms(max(result["latencies_us"])) if result.get("latencies_us") else None
        ),
        "setup_p50_ms": _ms(inst.pct(result.get("setup_us", []), 0.50)),
    }
    built = _probe_cell(Spot(ctx, window, cell), result, metrics, log)
    built.evidence["probe_wall_s"] = round(wall, 4)
    if result.get("stopped_early"):
        # Not a failure: the reading is over the window the probe did measure,
        # and the sample count says how much of the workload it covers.
        built.evidence["probe_stopped_early"] = True
    if result.get("mismatched"):
        built.ok = False
        built.reason = (
            f"{result['mismatched']} of {ok} replies did not match what was sent"
        )
    elif ok < scale:
        # A partial run is a reading, not a failure: the loss is in `failed`.
        built.evidence["expected_round_trips"] = scale
    return [built]


def run_udp(ctx: Ctx, params: dict) -> list:
    """One paced or blast datagram arm from the visitor namespace."""
    rate = params.get("datagrams_per_s", 0.0)
    log = ctx.work / f"udp-{ctx.arm.id}-{params['datagrams']}.log"
    argv = ctx.topo.ns_argv(
        topology.VIS_NS,
        [
            sys.executable,
            str(UDP_PROBE),
            "--target",
            f"{ctx.arm.dial_host}:{topology.UDP_PORT}",
            "--datagrams",
            str(params["datagrams"]),
            "--size",
            str(params["size"]),
            "--rate",
            str(rate),
            "--max-s",
            str(params.get("max_s", 0)),
        ],
    )
    with measured(ctx, topology.UDP_PORT, kind="udp") as window:
        result, rc, wall = _spawn_probe(argv, log, timeout=300)
    cell = "blast" if not rate else f"{rate:g}/s"
    if not result:
        return [Cell(cell, False, reason=f"the probe produced no result (exit {rc})")]
    sent, received = result.get("sent", 0), result.get("received", 0)
    window_s = max(result.get("wall_s", 0.0), 1e-9)
    metrics = {
        "udp_loss_pct": (round(100.0 * (sent - received) / sent, 4) if sent else None),
        "udp_recv_mbit": round(received * params["size"] * 8 / window_s / 1e6, 2),
        "udp_gap_p99_ms": _ms(inst.pct(result.get("gaps_us", []), 0.99)),
        "udp_rtt_p99_ms": _ms(inst.pct(result.get("rtt_us", []), 0.99)),
        "rtt_samples": len(result.get("rtt_us", [])),
    }
    built = _probe_cell(Spot(ctx, window, cell), result, metrics, log)
    built.evidence["probe_wall_s"] = round(wall, 4)
    if result.get("stopped_early"):
        built.evidence["probe_stopped_early"] = True
    if result.get("mismatched"):
        built.ok = False
        built.reason = f"{result['mismatched']} replies did not match what was sent"
    return [built]


def run_udp_ladder(ctx: Ctx, params: dict) -> list:
    """One iperf3 UDP cell per offered rate: where a datagram path sheds.

    The sink has to be one that can absorb a gigabit — the python echo is not
    (it drops half of a 200k datagram/s blast on the control arm too, so its
    loss would be the probe's, not the path's) — and iperf3's datagram mode is
    that sink. Its accounting is the receiver's own, which is the side this
    model quotes rates from anyway.
    """
    cells: list = []
    for rate_mbit in params["rates_mbit"]:
        cell = f"{rate_mbit:g}M"
        with (
            measured(ctx, topology.IPERF_UDP_PORT, kind="udp") as window,
            iperf_sink(ctx, f"udp{rate_mbit:g}", topology.IPERF_UDP_PORT),
        ):
            if not wait_listener(topology.IPERF_UDP_PORT):
                return [Cell(cell, False, reason="the UDP sink never listened")]
            result = lib.iperf_result(
                lib.IperfDial(
                    topology.IPERF_UDP_PORT,
                    host=ctx.arm.dial_host,
                    argv_prefix=("ip", "netns", "exec", topology.VIS_NS),
                    omit=params.get("omit", 0),
                    udp=True,
                    bitrate=f"{rate_mbit:g}M",
                    length=params["size"],
                ),
                1,
                params["secs"],
                iperf_timeout(params["secs"]),
                ctx.work / "iperf-raw" / f"{ctx.arm.id}-udp-{rate_mbit:g}M",
            )
        udp = (result or {}).get("udp") or {}
        metrics = {
            "udp_recv_mbit": (
                round((result.get("gbps_received_own_window") or 0) * 1000, 2)
                if result.get("ok")
                else None
            ),
            "udp_loss_pct": udp.get("lost_percent"),
        }
        built = _iperf_cell(Spot(ctx, window, cell), result, metrics)
        if built.ok:
            built.absent(
                "udp_gap_p99_ms",
                "the iperf3 sink reports jitter, not arrival gaps; the paced "
                "probe measures gaps",
            )
            built.absent(
                "udp_rtt_p99_ms",
                "the iperf3 sink echoes nothing, so the ladder has no round trip",
            )
        cells.append(built)
    return cells


def _ms(microseconds):
    return round(microseconds / 1000.0, 3) if microseconds is not None else None


def _spawn_probe(argv: list, log: Path, timeout: float) -> tuple:
    """Run one probe process; return its JSON line, exit status and wall time."""
    t0 = time.perf_counter()
    with log.open("wb") as fh:
        proc = subprocess.Popen(argv, stdout=fh, stderr=fh)
        try:
            rc = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            rc = None
    wall = time.perf_counter() - t0
    result: dict = {}
    with contextlib.suppress(OSError, ValueError):
        for line in reversed(log.read_text(errors="replace").splitlines()):
            if line.startswith("{"):
                result = json.loads(line)
                break
    return result, rc, wall


RUNNERS = {
    model.Kind.BULK: run_bulk,
    model.Kind.BULK_PAIR: run_bulk_pair,
    model.Kind.RR: run_rr,
    model.Kind.UDP: run_udp,
    model.Kind.UDP_LADDER: run_udp_ladder,
}


def run(kind: str, ctx: Ctx, params: dict) -> list:
    return RUNNERS[kind](ctx, params)
