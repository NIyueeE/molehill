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
import itertools
import json
import statistics
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

from bench import instruments as inst
from bench import iperf, model, topology

HERE = Path(__file__).resolve().parent
RR_PROBE = HERE / "probes" / "rr.py"
SERIES_PROBE = HERE / "probes" / "series.py"
UDP_PROBE = HERE / "probes" / "udp.py"

#: The backend inside the client namespace. It announces every accepted peer
#: (the transparency evidence: on L3 the backend must see the *visitor*), and
#: echoes both protocols so one process serves every arm and every workload —
#: the same backend, by three addresses, is what makes the arms comparable.
#:
#: **One thread alternating `recv`/`sendall` deadlocks on a full-duplex bulk
#: echo**, and it did: both directions' socket buffers fill, so the peer stops
#: reading because it is writing and this loop stops reading because it is
#: writing, and the transfer stops a few megabytes in with every instrument on
#: the carrier reading clean — because the source of the bytes had simply
#: stopped delivering. A reader thread and a writer thread joined by a bounded
#: queue cannot do that: the queue is the backpressure, each direction has its
#: own thread, and a stall in one cannot park the other. The end of the stream
#: travels through the same queue, so the writer cannot finish ahead of the
#: bytes that preceded it.
ECHO_SRC = """
import queue, socket, sys, threading
from contextlib import suppress

port = int(sys.argv[1])
udp_port = int(sys.argv[2])

#: How much the writer may fall behind the reader before the reader parks. Deep
#: enough to keep a bulk echo streaming, shallow enough that a stalled writer
#: cannot let this process hold tens of megabytes per connection.
QUEUE_DEPTH = 64

#: A connection that carried more than this has its totals announced, so a
#: stalled transfer can be attributed to the backend rather than guessed at.
#: Small connections stay silent: at four thousand arrivals a second a line per
#: connection would make the log the load.
ANNOUNCE_OVER = 64 << 10

EOF = object()


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
    q = queue.Queue(maxsize=QUEUE_DEPTH)
    done = threading.Event()
    taken = [0]
    given = [0]

    def read_direction():
        try:
            while True:
                data = conn.recv(65536)
                if not data:
                    return
                taken[0] += len(data)
                while not done.is_set():
                    try:
                        q.put(data, timeout=0.2)
                        break
                    except queue.Full:
                        continue
                if done.is_set():
                    return
        except OSError:
            pass
        finally:
            # The end rides the queue, so it cannot overtake the data.
            while not done.is_set():
                try:
                    q.put(EOF, timeout=0.2)
                    return
                except queue.Full:
                    continue

    def write_direction():
        while True:
            item = q.get()
            if item is EOF:
                return
            try:
                conn.sendall(item)
                given[0] += len(item)
            except OSError:
                done.set()
                # Unblock the reader: it may be parked in recv on a peer that
                # is already gone, and a leaked thread per dead visitor would
                # eventually be the load this process cannot carry.
                with suppress(OSError):
                    conn.shutdown(socket.SHUT_RD)
                return

    reader = threading.Thread(target=read_direction, daemon=True)
    writer = threading.Thread(target=write_direction, daemon=True)
    reader.start()
    writer.start()
    reader.join()
    writer.join()
    done.set()
    with suppress(OSError):
        conn.close()
    if taken[0] > ANNOUNCE_OVER or given[0] > ANNOUNCE_OVER:
        # Never starts with `PEER `: that prefix is the transparency evidence
        # and nothing else may be mistaken for it.
        sys.stdout.write(f"ECHO DONE read={taken[0]} wrote={given[0]}\\n")
        sys.stdout.flush()


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


@dataclass(frozen=True)
class Span:
    """The seconds a stage was measured over, and what it was asked to offer.

    Its own value rather than four parameters: the window's start, its length
    and the cadences the probes were told to keep travel together, and a stage
    metric that is computed from a window without knowing which window it was
    is the mistake that made a ninety-second median look like a property of the
    carrier.
    """

    since: float
    seconds: float
    params: dict


#: The running arm's pid map, supplied by the runner.
PidsOf = Callable[[], dict[str, int]]
#: The backend's peers announced since the previous cell, or no reader.
PeersOf = Callable[[], list[str]] | None
#: Stop and restart the arm, returning (seconds to answer, failure reason).
RestartArm = Callable[[], tuple[float | None, str]] | None


@dataclass
class Ctx:
    """Everything a workload needs, so no workload takes a long argument list."""

    topo: topology.Topology
    pids_of: PidsOf
    work: Path
    arm: model.Arm
    probe_startup_s: float = 0.0
    #: The backend's announced peers, read after each cell: on an L3 arm the
    #: backend must see the *visitor*, and on L4 the client. It is the
    #: transparency evidence, recorded beside the numbers rather than assumed.
    peers_of: PeersOf = None
    #: The campaign's condition and the leg it is imposed on: a staged scenario
    #: changes conditions *within* a run and must put this one back.
    leg: str = "visitor"
    condition: model.Condition | None = None
    #: Restart the arm's daemons and return the seconds until the service
    #: answers — the reconnect scenario's instrument (the runner owns the
    #: processes, the workload owns the measurement).
    restart_arm: RestartArm = None


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

    It serves its whole lifetime rather than one test, because a readiness probe
    has to be able to reach the tool's port without consuming the sample: with
    `-1` a single probe connection made every iperf3 client fail with `control
    socket has closed unexpectedly` (measured: 0 of 14 samples). The sink is
    killed after every sample either way, so a wedged one cannot outlive it. The
    bind is the arm's `backend_bind`, which is not cosmetic — an iperf3 UDP server
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
                    # Line-buffered, or its banner and its errors stay in a
                    # block buffer and the log is empty exactly when a failure
                    # is being diagnosed.
                    "stdbuf",
                    "-oL",
                    "iperf3",
                    "-s",
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
        yield Sink(proc, log, port)
    finally:
        with contextlib.suppress(OSError):
            proc.kill()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)
        time.sleep(0.2)


#: What the boundary probe prints: an outcome and a duration.
PROBE_FIELDS = 2


def echo_round_trip(ctx: Ctx, timeout_s: float = 3.0) -> dict:
    """One visitor's whole life through the tool, asked at a stage boundary.

    A stage's carrier counters can read clean while its bulk transfer carries nothing,
    so "was the tool serving anyone now" has to be asked from outside it: connect to
    the **echo** service, send a byte, read it back, close. Cheap, harmless, and it
    separates "the tool stopped forwarding" from "this stage's iperf client never got
    going" — two failures a summary cannot tell apart.
    """
    host, port = ctx.arm.dial_host, topology.ECHO_PORT
    script = (
        "import socket,time\n"
        "t=time.time()\n"
        "try:\n"
        f"    s=socket.create_connection(('{host}', {port}), {timeout_s})\n"
        "    s.sendall(b'x')\n"
        "    print('ok' if s.recv(1) else 'eof', round((time.time()-t)*1000, 2))\n"
        "except Exception as e:\n"
        "    print('fail', type(e).__name__, round((time.time()-t)*1000, 2))\n"
    )
    done = ctx.topo.ns_run(
        topology.VIS_NS, ["python3", "-c", script], timeout=timeout_s + 5, check=False
    )
    parts = (done.stdout or "").split()
    # The probe prints exactly `<outcome> <milliseconds>`; anything else is no answer.
    if len(parts) == PROBE_FIELDS:
        return {"outcome": parts[0], "ms": float(parts[-1])}
    return {"outcome": "no-answer", "ms": None}


def reap_iperf(ctx: Ctx) -> None:
    """Kill anything still running `iperf3` on either side of the path.

    Both the sink's namespace and the client's: a leftover from the previous stage
    either holds the port or reconnects into the fresh sink, and either way this
    stage's measurements are lost to a process nobody is measuring.
    """
    for ns in (topology.CLI_NS, topology.VIS_NS):
        ctx.topo.ns_run(ns, ["pkill", "-9", "-x", "iperf3"], check=False)


def wait_dial_port(ctx: Ctx, port: int, timeout: float = 30.0) -> bool:
    """Is the port this workload is about to dial actually open yet?

    The run's readiness step probes the arm's declared endpoint, which is the
    **echo** port — and an arm exposes several services, registered one after
    another, so the echo port can be up while the iperf one is still joining. A
    sample that dials the later port then gets a reset in twenty milliseconds
    from a tool that is a hair from ready (measured: one sample of fourteen, with
    the backend listening and never accepting).

    The model's own rule is that an arm is ready when the workload's first packet
    would succeed, so the probe belongs on the port the workload dials — which is
    only safe because the sink now serves more than one connection.
    """
    return ctx.topo.wait_target(ctx.arm.dial_host, port, timeout=timeout)


@dataclass
class Sink:
    """One iperf3 server the harness started, and its own account of itself."""

    proc: subprocess.Popen
    log: Path
    port: int

    def ready(self, timeout: float = 15.0) -> bool:
        """Is this sink both alive and listening?

        `wait_listener` alone asks the kernel whether *something* listens on the
        port; a sink that exited at startup leaves it free, and a sample taken
        against it reports `Connection refused` from the visitor, which reads as
        a failure of the tool under test. Asking whether our own process is
        still running closes that hole.

        The sink's *log* is deliberately not part of this: iperf3's stdout is
        block-buffered when it is a file, so a healthy sink writes nothing until
        enough output accumulates, and a zero-byte log says nothing about
        whether it started. That misreading cost a whole A/B run — every sample
        reported "never started" while the sinks were fine. The sink is started
        under `stdbuf -oL` so its log *is* readable when a run is being diagnosed.
        """
        end = time.time() + timeout
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            if wait_listener(self.port, timeout=0.2):
                return True
            time.sleep(0.02)
        return False


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
            window["socket_polls"] = sock.polls
            window["socket_error"] = sock.error


def _evidence(ctx: Ctx, window: dict, extra: dict | None = None) -> dict:
    """The counters and commands a reader needs to audit one cell."""
    sample = window.get("sample", {})
    wire = sample.get("wire", {})
    peaks = window.get("peaks", {})
    sockets = window.get("sockets", {})
    out = {
        "dial_host": ctx.arm.dial_host,
        "backend_bind": ctx.arm.backend_bind,
        "arm_kind": ctx.arm.kind,
        "arm_data_carrier": ctx.arm.data_carrier,
        "window_s": sample.get("elapsed_s"),
        "cpu_s": sample.get("cpu_s"),
        "cpu_s_total": sample.get("cpu_s_total"),
        # The split, because the sum cannot say what to fix: kernel time is
        # syscalls and boundary copies, user time is copies and locks.
        "cpu_user_s": sample.get("cpu_user_s"),
        "cpu_sys_s": sample.get("cpu_sys_s"),
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
        # The shaper's own reading, kept beside the devices' drops rather than
        # folded into them: `drops` says *how much* an interface shed, this says
        # whether the queue it shed from was the binding constraint (a standing
        # backlog at the limit) or merely passing a burst.
        "qdisc": sample.get("qdisc"),
        "tcp": sample.get("tcp"),
        "peaks": peaks,
        "sockets": sockets,
        "socket_polls": window.get("socket_polls"),
        "socket_error": window.get("socket_error") or None,
        "probe_startup_s": ctx.probe_startup_s,
    }
    if extra:
        out |= extra
    peers_of = ctx.peers_of
    if peers_of is not None:
        out["backend_peers"] = sorted(set(peers_of()))
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

#: The metrics that describe **device** I/O — the L3 path's shape. Only an arm
#: that moves its payload through a character device can produce them:
#: `/proc/<pid>/io` accounts file and pipe traffic, not socket payload
#: (measured: 200 MB through a socketpair moves `rchar` by 105 KB, the same
#: bytes through a pipe by 209 MB), so for a forwarding arm the reading would
#: describe the runtime's plumbing rather than the path. Reported as a typed
#: absence instead, never as a number.
DEVICE_IO_METRICS = ("bytes_per_syscall", "syscalls_per_s")


def _counter_cell(spot: Spot, metrics: dict, extra: dict | None = None) -> Cell:
    merged = dict(metrics)
    window = spot.window
    merged |= inst.counter_metrics(
        window["sample"], window.get("peaks", {}), device_io=spot.ctx.arm.uses_device_io
    )
    if window.get("sockets"):
        merged["service_sockets_peak"] = window["sockets"].get("server")
    unavailable: dict = {}
    if not spot.ctx.arm.is_tool:
        reason = "the control arm runs no tool processes"
        for metric in TOOL_ONLY_METRICS:
            merged.pop(metric, None)
            unavailable[metric] = {"reason": reason}
    elif not spot.ctx.arm.uses_device_io:
        reason = (
            "this arm's payload never crosses a device, and `/proc/<pid>/io` "
            "does not account socket payload"
        )
        for metric in DEVICE_IO_METRICS:
            merged.pop(metric, None)
            unavailable[metric] = {"reason": reason}
    return Cell(
        cell=spot.cell,
        ok=True,
        metrics=merged,
        unavailable=unavailable,
        evidence=_evidence(spot.ctx, window, extra),
    )


def _dial(ctx: Ctx, params: dict) -> iperf.IperfDial:
    return iperf.IperfDial(
        params["port"],
        host=ctx.arm.dial_host,
        argv_prefix=("ip", "netns", "exec", topology.VIS_NS),
        omit=params.get("omit", 2),
        udp=params.get("udp", False),
        bitrate=params.get("bitrate", ""),
        length=params.get("size", 0) if params.get("udp") else 0,
    )


def _iperf_cell(spot: Spot, result: dict, metrics: dict, lenient: bool = False) -> Cell:
    """One bulk measurement's cell.

    `lenient` is for a **staged** cell, where the bulk spine is one of several
    measurements the same stage carried. A dead `iperf3` there must not delete
    the arrivals that were measured beside it: the failure becomes a typed
    absence on `throughput_gbps`, the stage stays readable, and the runner
    retries the stage for the number it lost. A standalone bulk scenario keeps
    the strict shape, because there the bulk measurement *is* the cell.
    """
    if result.get("ok") is False:
        reason = result.get("reason", "unknown iperf3 failure")
        if not lenient:
            extra = {"cmd": result.get("artifact"), "exit": result.get("exit")}
            return Cell(
                cell=spot.cell,
                ok=False,
                reason=reason,
                evidence=_evidence(spot.ctx, spot.window, extra),
            )
        built = _counter_cell(
            spot,
            metrics,
            {"artifact": result.get("artifact"), "exit": result.get("exit")},
        )
        built.evidence["iperf_failure"] = reason[:200]
        return built
    if not result:
        # No bulk spine at all (`streams: 0`): the throughput metric is a typed
        # absence rather than a zero, because a run that carried nothing must
        # not read as one that carried nothing *well*.
        return Cell(
            cell=spot.cell,
            ok=True,
            metrics=metrics,
            unavailable={"throughput_gbps": {"reason": "no bulk spine: streams = 0"}},
            evidence=_evidence(spot.ctx, spot.window, {}),
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
        iperf_sink(ctx, tag, port) as sink,
    ):
        if not sink.ready():
            return [Cell("", False, reason="the iperf3 sink never started")]
        if not wait_dial_port(ctx, port):
            return [Cell("", False, reason="the tool never listened on the iperf port")]
        result = iperf.iperf_result(
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
            started = [m.__enter__() for m in sinks]
            if not all(sink.ready() for sink in started):
                return [Cell("", False, reason="an iperf3 sink never started")]
            if not all(wait_dial_port(ctx, p) for p in ports):
                return [
                    Cell("", False, reason="the tool never listened on a pair port")
                ]

            def worker(index: int, port: int) -> None:
                results[index] = iperf.iperf_result(
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
    # A round that ran out of visitor ports is not a reading of the tool: the
    # probe backs off 50 ms per refused connect, so `wall_s` grows by the wait
    # and `rate_per_s` falls by however much of the round starved. It happened on
    # the A/A twin as readily as on the arm under test, which is what made it look
    # like an intermittent 16x collapse instead of an instrument limit. The
    # namespace's own settings now prevent it (`Topology.up` sets
    # `tcp_tw_reuse` and widens the port range); this guard is what stops a
    # starved round being read as a slow tool if it ever happens again.
    connect_errors = result.get("connect_errors", 0)
    attempts = result.get("ok", 0) + result.get("failed", 0) + connect_errors
    if attempts and connect_errors * 20 > attempts:
        return [
            Cell(
                cell,
                False,
                reason=(
                    f"the visitor ran out of ephemeral ports: {connect_errors} of "
                    f"{attempts} connects were refused, so this round measured the "
                    "probe's backoff rather than the tool"
                ),
            )
        ]
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
            iperf_sink(ctx, f"udp{rate_mbit:g}", topology.IPERF_UDP_PORT) as sink,
        ):
            if not sink.ready():
                return [Cell(cell, False, reason="the UDP sink never started")]
            result = iperf.iperf_result(
                iperf.IperfDial(
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


# --- the staged sweep -------------------------------------------------------
@dataclass(frozen=True)
class SeriesSpec:
    """What to start: the mode, who it dials, for how long, and where it writes."""

    mode: str
    target: str
    seconds: float
    log: Path | None = None


def _series_probe(ctx: Ctx, spec: SeriesSpec, params: dict):
    """Start one series probe; return `(process, log path)`.

    The caller may name the log (a capacity ramp starts one probe per level and
    must read each level's own file); when it does not, the mode names it.
    """
    mode = spec.mode
    log = spec.log or ctx.work / f"series-{ctx.arm.id}-{mode}.ndjson"
    argv = ctx.topo.ns_argv(
        topology.VIS_NS,
        [
            sys.executable,
            str(SERIES_PROBE),
            "--mode",
            mode,
            "--target",
            spec.target,
            "--duration-s",
            str(round(spec.seconds, 2)),
            "--interval-ms",
            str(
                params.get("ping_interval_ms", 50)
                if mode != "udp"
                else params.get("udp_interval_ms", 20)
            ),
            "--rate",
            str(params.get("churn_per_s", 16)),
        ],
    )
    fh = log.open("wb")
    proc = subprocess.Popen(argv, stdout=fh, stderr=subprocess.STDOUT)
    fh.close()
    return proc, log


def read_series(log: Path, since: float, until: float) -> list:
    """The samples of one series log inside a window `[since, until]`."""
    points: list = []
    with contextlib.suppress(OSError):
        for line in log.read_text(errors="replace").splitlines():
            if not line.startswith("{"):
                continue
            with contextlib.suppress(ValueError):
                row = json.loads(line)
                if since <= row.get("t", 0.0) <= until:
                    points.append(row)
    return points


def _stage_absences(metrics: dict, interaction: list, datagrams: list) -> dict:
    """Why a stage's interactive or datagram numbers are missing, typed.

    A stage where every attempt failed has no percentile to report — and the
    difference between "the tool answered slowly" and "the tool never answered"
    is the finding, so it is written down rather than left as an absent key.
    """
    unavailable: dict = {}
    failed = [p for p in interaction if p["metric"] == "error"]
    if failed and metrics.get("rtt_p99_ms") is None:
        reason = f"all {len(failed)} interactive attempts failed in this stage"
        for metric in ("rtt_p50_ms", "rtt_p99_ms", "rtt_max_ms", "rtt_worst_1s_ms"):
            if metrics.get(metric) is None:
                unavailable[metric] = {"reason": reason}
    losses = [p for p in datagrams if p["metric"] in ("loss", "error")]
    if losses and metrics.get("udp_rtt_p99_ms") is None:
        unavailable["udp_rtt_p99_ms"] = {
            "reason": f"no datagram came back: {len(losses)} attempt(s) lost"
        }
    return unavailable


def _stage_metrics(
    interaction: list,
    datagrams: list,
    churn: list,
    span: Span,
) -> tuple:
    """One stage's interactive/datagram/churn numbers, from its own samples."""
    window_s = span.seconds
    rtts = [p["v"] for p in interaction if p["metric"] == "rtt_ms"]
    errors = [p for p in interaction if p["metric"] == "error"]
    churn_setup = [p["v"] for p in churn if p["metric"] == "setup_ms"]
    datagram_rtts = [p["v"] for p in datagrams if p["metric"] == "rtt_ms"]
    losses = [p for p in datagrams if p["metric"] == "loss"]
    losses += [p for p in datagrams if p["metric"] == "error"]
    attempts = len(datagram_rtts) + len(losses)
    metrics = {
        "rtt_samples": len(rtts) or None,
        "rtt_p50_ms": inst.pct(rtts, 0.50),
        "rtt_p99_ms": inst.pct(rtts, 0.99),
        "rtt_max_ms": max(rtts) if rtts else None,
        "rtt_worst_1s_ms": _worst_second(interaction),
        "rtt_error_rate_pct": (
            round(100.0 * len(errors) / (len(rtts) + len(errors)), 4)
            if (rtts or errors)
            else None
        ),
        "churn_per_s": (
            round(len(churn_setup) / window_s, 2) if window_s > 0 else None
        ),
        "udp_rtt_p99_ms": inst.pct(datagram_rtts, 0.99),
        "udp_loss_pct": (
            round(100.0 * len(losses) / attempts, 4) if attempts else None
        ),
        "udp_gap_p99_ms": _gap_p99(datagrams),
    }
    evidence = {
        "series": {
            "interactive": len(interaction),
            "datagram": len(datagrams),
            "churn": len(churn),
        },
        # The probe's own log paths are the caller's to add: this function is
        # handed the *points*, not the files, and a list of the points rendered
        # as text (which is what stood here) is not a log — it doubled every
        # staged results file and named nothing.
        # The stage's shape, not only its summary: a median over ninety seconds
        # is the same number for a stage that was flat and for one that began
        # fast and slowed, and the difference between those two is the whole
        # first-stage question.
        "interactive_per_s": _per_second(interaction, span.since),
        "interactive_cadence": _probe_cadence(
            interaction, span.params.get("ping_interval_ms", 50.0) / 1000.0
        ),
        # The offered rate beside the achieved one (`churn_per_s` is measured):
        # a churn probe that fell behind offers less than the timeline asked
        # for, and its cells are then a statement about the probe.
        "churn_offered_per_s": span.params.get("churn_per_s", 16.0),
    }
    return metrics, evidence, _stage_absences(metrics, interaction, datagrams)


def _per_second(points: list, since: float) -> list:
    """The interactive stream's own second-by-second shape inside one stage.

    Each row is one second that produced attempts. A second with **none is
    absent**, which is what keeps "the probe stopped" from reading as "the
    stream was fast": a probe that dies leaves no row, and one that is merely
    slow leaves a row with a large gap to the next (see `_probe_cadence`).
    """
    buckets: dict = {}
    for point in points:
        kind = point["metric"]
        if kind not in ("rtt_ms", "error"):
            continue
        second = int(point["t"] - since)
        row = buckets.setdefault(second, {"rtt": [], "errors": 0})
        if kind == "rtt_ms":
            row["rtt"].append(point["v"])
        else:
            row["errors"] += 1
    return [
        {
            "s": second,
            "n": len(row["rtt"]),
            "errors": row["errors"],
            "p50": inst.pct(row["rtt"], 0.50),
            "p99": inst.pct(row["rtt"], 0.99),
            "max": max(row["rtt"]) if row["rtt"] else None,
        }
        for second, row in sorted(buckets.items())
    ]


def _probe_cadence(points: list, interval_s: float) -> dict:
    """Whether the probe kept its own cadence: the instrument's health.

    A probe slower than its interval reports the *probe's* load as the path's.
    The gap between attempts is the part of the measurement no carrier can
    influence, so it is recorded beside the number it decided, and `overruns`
    counts the intervals the probe missed by more than a factor of two.
    """
    times = [p["t"] for p in points if p["metric"] in ("rtt_ms", "error")]
    if len(times) < inst.MIN_WEDGE_POINTS:
        return {"samples": len(times)}
    gaps = [(b - a) * 1000.0 for a, b in itertools.pairwise(times)]
    limit_ms = 2.0 * interval_s * 1000.0
    return {
        "samples": len(times),
        "interval_ms": round(interval_s * 1000.0, 3),
        "gap_p50_ms": inst.pct(gaps, 0.50),
        "gap_p99_ms": inst.pct(gaps, 0.99),
        "overruns": sum(1 for gap in gaps if gap > limit_ms),
    }


def _worst_second(points: list):
    """The worst one-second mean of the interactive stream's round trips."""
    rtts = [(p["t"], p["v"]) for p in points if p["metric"] == "rtt_ms"]
    if len(rtts) < inst.MIN_WEDGE_POINTS:
        return None
    worst = None
    start = 0
    for end, (t, _) in enumerate(rtts):
        while t - rtts[start][0] > 1.0:
            start += 1
        window = [v for _, v in rtts[start : end + 1]]
        mean = sum(window) / len(window)
        worst = mean if worst is None else max(worst, mean)
    return round(worst, 4) if worst is not None else None


def _gap_p99(points: list):
    """The 99th percentile of the gaps between arriving datagram replies."""
    times = [p["t"] for p in points if p["metric"] in ("rtt_ms", "loss")]
    if len(times) < inst.MIN_WEDGE_POINTS:
        return None
    gaps = [(b - a) * 1000.0 for a, b in itertools.pairwise(times)]
    return inst.pct(gaps, 0.99)


def _stages_of(params: dict) -> list:
    """The timeline with its holds scaled, floored, and numbered per condition."""
    holds = model.timeline_for(params["timeline"], params.get("scale", 1.0))
    floor = params.get("min_hold_s", 0.0)
    seen: dict = {}
    stages = []
    for condition, secs in holds:
        seen[condition] = seen.get(condition, 0) + 1
        occurrence = seen[condition]
        label = condition if occurrence == 1 else f"{condition}#{occurrence}"
        stages.append((label, condition, max(secs, floor)))
    return stages


def run_staged(ctx: Ctx, params: dict) -> list:
    """The scripted schedule: the tool runs while the path changes underneath.

    The tool's processes start once and the condition changes in place, so what
    is measured is how the tool *adapts* rather than how it starts. The three
    series probes run for the whole timeline and are sliced per stage, which is
    what makes a per-stage number and a whole-run drift slope come from the same
    samples; the bulk spine is re-dialed per stage after the leg has gone quiet,
    because a killed client keeps delivering what its kernel still holds.
    """
    stages = _stages_of(params)
    cells: list = []
    probes = [
        # 0 = run until this workload kills them: a stage's drain cost is not
        # known before the run, so a precomputed duration can end before the
        # last stage begins.
        _series_probe(ctx, SeriesSpec(mode, target, 0.0), params)
        for mode, target in (
            ("interactive", f"{ctx.arm.dial_host}:{topology.ECHO_PORT}"),
            ("udp", f"{ctx.arm.dial_host}:{topology.UDP_PORT}"),
            ("churn", f"{ctx.arm.dial_host}:{topology.ECHO_PORT}"),
        )
    ]
    logs = {
        mode: probe[1]
        for mode, probe in zip(("interactive", "udp", "churn"), probes, strict=True)
    }
    drift_points: list = []
    try:
        with inst.DriftSampler(ctx.pids_of, interval=inst.DRIFT_POLL_S) as drift:
            cells.extend(_one_stage(ctx, stage, params, logs) for stage in stages)
            drift_points = list(drift.points)
    finally:
        for proc, _ in probes:
            with contextlib.suppress(OSError):
                proc.kill()
            with contextlib.suppress(Exception):
                proc.wait(timeout=5)
        if ctx.condition is not None:
            ctx.topo.set_condition(ctx.leg, ctx.condition)
    cells.append(_run_cell(drift_points, logs))
    return cells


class _RestoresCondition:
    """Put the leg back to clean however the stage ends.

    `_one_stage` imposes a condition and, until this guard existed, never took it
    off: `run_staged`'s own `finally` restores `ctx.condition`, which is `None` for
    a staged run, so it restored nothing. The last stage of the timeline is
    `clean#2` and therefore benign — but a stage that *dies* early used to leave
    whatever it had installed in place, and every stage and every arm after it ran
    on that path.

    Measured, and it cost a whole five-arm sweep: one arm lost `loss5`, four of its
    stages followed, the next arm lost two of its own plus all eight capacity
    levels, and the two arms after that carried nothing at all — the gate reported
    69 failures and the reference tools were the ones with zero streams. **One bad
    stage must not cost every measurement after it**, which is AGENTS §10's rule
    about a failure poisoning the next sample, applied to a condition instead of a
    process.
    """

    def __init__(self, topo, leg: str) -> None:
        self.topo, self.leg = topo, leg

    def __enter__(self) -> None:
        return None

    def __exit__(self, *_: object) -> None:
        with contextlib.suppress(Exception):
            self.topo.set_condition(self.leg, model.CONDITIONS["clean"])


#: Failure reasons that belong to the instrument rather than to the arm.
#:
#: `iperf3`'s client exits with this line when its own control socket closes, and
#: its sink then reports the streams established with zero bytes for the whole
#: stage — indistinguishable, in a summary, from a carrier that forwards a
#: connection and drops its payload. Measured on the release sweep: the sender's
#: window grew to 13.5 MB with the flight at zero through the stages that lost
#: their cells, so the carrier was idle rather than broken.
INSTRUMENT_FAILURES = (
    "control socket has closed unexpectedly",
    "the server is busy running",
    "exit -9: no output",
)


def _instrument_failure(reason: str | None) -> bool:
    """Whether a stage's failure is the instrument's own."""
    return bool(reason) and any(marker in reason for marker in INSTRUMENT_FAILURES)


def _one_stage(ctx: Ctx, stage: tuple, params: dict, logs: dict) -> Cell:
    """One stage: impose the condition, let the leg go quiet, then measure.

    A stage whose `iperf3` client died for the instrument's own reason is measured
    a second time, in place, so the schedule keeps its shape and one dead client
    does not leave a hole in the coverage the gate checks. The second attempt is
    what the sample carries; the first one's reason travels in its evidence.
    """
    condition = stage[1]
    ctx.topo.set_condition(ctx.leg, model.CONDITIONS[condition])
    with _RestoresCondition(ctx.topo, ctx.leg):
        cell = _measure_stage(ctx, stage, params, logs)
        # What the instrument said, whether it took the whole cell down with it or
        # only `throughput_gbps`: a stage measured leniently still deserves the
        # number it lost, and the reason to retry is the instrument's, not the
        # cell's shape.
        failed = cell.evidence.get("iperf_failure") or cell.reason
        if not _instrument_failure(failed):
            return cell
        first = failed
        # **What the path looks like at the moment it failed**, before the retry
        # changes it: whether the tool still answers on the port it just failed to
        # carry on. A stage that fails and is still reachable is a carrier that
        # stopped forwardting; one that is unreachable is a tool that died or a
        # listener that went away, and the two want different investigations.
        cell.evidence["reachable_after_failure"] = wait_dial_port(
            ctx, topology.IPERF_PORT, 5.0
        )
        ctx.topo.drain(ctx.leg)
        again = _measure_stage(ctx, stage, params, logs)
        again.evidence["retried_after"] = (first or "")[:200]
        # The first attempt's own evidence is the one that describes the failure;
        # the retried cell replaces it, so carry the facts across or they are lost
        # with the cell they were recorded on.
        if "reachable_after_failure" in cell.evidence:
            again.evidence["reachable_after_failure"] = cell.evidence[
                "reachable_after_failure"
            ]
        return again


def _measure_stage(ctx: Ctx, stage: tuple, params: dict, logs: dict) -> Cell:
    """The body of one stage, with its condition already imposed."""
    label, condition, secs = stage
    drain = ctx.topo.drain(ctx.leg)
    # Asked before the condition and before this stage's own traffic: was the tool
    # serving anyone at this boundary at all?
    boundary = echo_round_trip(ctx)
    port = topology.IPERF_PORT
    # A stage must not inherit the previous one's traffic crew. `iperf3` leaves a
    # client behind when it is killed, and one that reconnects makes the fresh sink
    # answer `the server is busy running` to every client of *this* stage — a
    # failure attributed to the arm that is entirely the instrument's (measured
    # 2026-11-03: two stages lost that way, and one run died outright).
    reap_iperf(ctx)
    # `streams: 0` is the bulk spine switched off: the staged timeline still
    # measures its interactive, datagram and churn series for every stage, and
    # the throughput cell reads as a typed absence rather than as zero. A tool
    # cannot be asked to carry nothing and then be judged on what it carried, so
    # the run says so instead of dialling iperf3 with `-P 0`.
    streams = int(params.get("streams", 4))
    result: dict = {}
    # A bulk instrument that fails must not delete the stage. The arrivals were
    # measured whether or not `iperf3` survived, and returning a failed cell here
    # is how four stages of a 2026-11-04 release run lost the very number the SLO
    # is stated on — including the last clean stage, whose p99 nobody could quote
    # afterwards. So the failure becomes a typed absence on one metric, the stage
    # still runs for its own hold, and the runner still retries it (see
    # `instrument_failed`).
    bulk_failure = ""
    # Bound up front so every path below is total: each branch overwrites them,
    # and a reader can see that the window is one value rather than four.
    t0 = t1 = 0.0
    if streams > 0:
        with (
            measured(ctx, topology.ECHO_PORT) as window,
            iperf_sink(ctx, f"stage-{label}", port) as sink,
        ):
            if not sink.ready():
                bulk_failure = "the iperf3 sink never started"
            elif not wait_dial_port(ctx, port):
                bulk_failure = "the tool never listened on the iperf port"
            else:
                t0 = time.time()
                result = iperf.iperf_result(
                    iperf.IperfDial(
                        port,
                        host=ctx.arm.dial_host,
                        argv_prefix=("ip", "netns", "exec", topology.VIS_NS),
                        omit=min(2, max(0, int(secs) // 4)),
                    ),
                    streams,
                    secs,
                    iperf_timeout(secs + 20),
                    ctx.work / "iperf-raw" / f"{ctx.arm.id}-stage-{label}",
                )
                t1 = time.time()
    else:
        with measured(ctx, topology.ECHO_PORT) as window:
            t0 = time.time()
            time.sleep(max(0.0, secs))
            t1 = time.time()
    if bulk_failure:
        # The spine could not even be dialed: hold the stage for the length the
        # schedule asked for, so its arrivals are measured over that window
        # rather than over an empty one, and take a fresh counter window.
        with measured(ctx, topology.ECHO_PORT) as again:
            t0 = time.time()
            time.sleep(max(0.0, secs))
            t1 = time.time()
        window = again
    interaction = read_series(logs["interactive"], t0, t1)
    datagrams = read_series(logs["udp"], t0, t1)
    churn = read_series(logs["churn"], t0, t1)
    window_s = max(t1 - t0, 1e-9)
    stage_metrics, evidence, unavailable = _stage_metrics(
        interaction, datagrams, churn, Span(since=t0, seconds=window_s, params=params)
    )
    stage_metrics |= {"slo_broken": _breaks_slo(stage_metrics)}
    # Only the interactive metrics carry the stage's verdict; throughput is the
    # bulk spine's and a run without one records its absence, not a zero.
    if result:
        stage_metrics["throughput_gbps"] = result.get("gbps_received_own_window")
    # `lenient`: a failed bulk instrument leaves one metric absent instead of
    # discarding the stage (see the note where `bulk_failure` is set).
    built = _iperf_cell(Spot(ctx, window, label), result, stage_metrics, lenient=True)
    built.unavailable |= unavailable
    if bulk_failure:
        built.absent("throughput_gbps", bulk_failure)
        built.evidence["iperf_failure"] = bulk_failure
    # Everything the stage's own samples said about themselves — the counts, the
    # within-window shape (`interactive_per_s`), the probe's own cadence, and the
    # rate the churn probe was asked for beside the one it achieved. A stage is
    # one number per metric, and the first-stage question is about a stage that
    # changes *inside* itself, so the shape is evidence and not a chart.
    built.evidence |= evidence
    # The three logs this stage was read out of, by path: the numbers above are
    # slices of them, and a reader who wants the shape at another resolution
    # needs the file rather than a second summary.
    built.evidence["series_logs"] = [
        str(logs[mode]) for mode in ("interactive", "udp", "churn") if mode in logs
    ]
    # The boundary's own answer, on every stage: a visitor's whole life through the
    # tool, asked before this stage's traffic started.
    built.evidence["echo_probe"] = boundary
    built.evidence |= {
        "condition": condition,
        "hold_s": secs,
        "series_window_s": round(window_s, 3),
        # The window's own edges in unix time, so a reader can line these rows up
        # with the host's load average and the tool's counters instead of
        # re-deriving the window from a duration.
        "window_start": round(t0, 3),
        "window_end": round(t1, 3),
    }
    built.evidence |= drain
    return built


def _breaks_slo(metrics: dict) -> int | None:
    """1 when a level or stage broke the SLO, 0 when it met it, None if unmeasured."""
    p99 = metrics.get("rtt_p99_ms")
    errors = metrics.get("rtt_error_rate_pct")
    if p99 is None and errors is None:
        return None
    broken = (p99 is not None and p99 > model.SLO_RTT_P99_MS) or (
        errors is not None and errors > model.SLO_ERROR_RATE_PCT
    )
    return int(broken)


def _run_cell(drift_points: list, logs: dict) -> Cell:
    """The whole run: the drift axis and the wedge axis, from its own series."""
    interaction = read_series(logs["interactive"], 0.0, time.time() + 1)
    wedged = inst.wedges(interaction)
    metrics = {
        "drift_rss_mib_per_min": _per_min(inst.slope_per_min(drift_points, "rss_kib")),
        "drift_fds_per_min": inst.slope_per_min(drift_points, "fds"),
        "drift_threads_per_min": inst.slope_per_min(drift_points, "threads"),
        "wedge_count": wedged["count"],
        "wedge_max_s": wedged["max_s"],
        "rtt_samples": len([p for p in interaction if p["metric"] == "rtt_ms"]) or None,
    }
    return Cell(
        "run",
        ok=True,
        metrics=metrics,
        unavailable={},
        evidence={
            "drift_points": len(drift_points),
            "drift_series": _thinned(drift_points),
            "series": {"interactive": len(interaction)},
            "series_logs": [str(p) for p in logs.values()],
            "wedge_silence_s": inst.WEDGE_SILENCE_S,
        },
    )


def _per_min(slope):
    """KiB/min -> MiB/min, or `None` when the series could not be fitted."""
    return None if slope is None else round(slope / 1024.0, 4)


def _thinned(points: list, every: int = 5) -> list:
    """Every n-th drift sample, for the results file: the fit uses them all."""
    return points[::every]


def run_capacity(ctx: Ctx, params: dict) -> list:
    """Ramp the bulk load until the interactive stream breaks the SLO.

    The user's question is "how much can it carry and still be usable", and the
    instrument is a fresh interactive connection per ping *during* the load:
    the slow visitor's own rate is not the number, the arriving visitor's tail
    latency is.
    """
    ctx.topo.set_condition(ctx.leg, model.CONDITIONS[params["condition"]])
    settle = params["settle_s"]
    cells: list = []
    sustainable = 0
    level = 0
    for level in range(1, params["streams_max"] + 1):
        cell = _capacity_level(ctx, level, settle, params)
        cells.append(cell)
        if not cell.ok or cell.metrics.get("slo_broken"):
            break
        sustainable = level
    cells.append(
        Cell(
            "ramp",
            ok=True,
            metrics={
                "capacity_streams": sustainable,
                "capacity_headroom_pct": round(
                    100.0 * (1 - sustainable / params["streams_max"]), 2
                ),
            },
            evidence={
                "streams_max": params["streams_max"],
                "levels_run": level,
                "condition": params["condition"],
                "slo": {
                    "rtt_p99_ms": model.SLO_RTT_P99_MS,
                    "error_rate_pct": model.SLO_ERROR_RATE_PCT,
                },
            },
        )
    )
    return cells


def _capacity_level(ctx: Ctx, level: int, settle: float, params: dict) -> Cell:
    """One ramp level: N bulk streams, with a fresh-connection ping beside them."""
    port = topology.IPERF_PORT
    log = ctx.work / f"series-{ctx.arm.id}-ramp{level}.ndjson"
    target = f"{ctx.arm.dial_host}:{topology.ECHO_PORT}"
    probe, log = _series_probe(ctx, SeriesSpec("interactive", target, 0.0, log), params)
    try:
        with (
            measured(ctx, topology.ECHO_PORT) as window,
            iperf_sink(ctx, f"ramp-{level}", port) as sink,
        ):
            if not sink.ready():
                return Cell(f"L{level}", False, reason="the iperf3 sink never started")
            t0 = time.time()
            result = iperf.iperf_result(
                iperf.IperfDial(
                    port,
                    host=ctx.arm.dial_host,
                    argv_prefix=("ip", "netns", "exec", topology.VIS_NS),
                    omit=1,
                ),
                level,
                settle,
                iperf_timeout(settle),
                ctx.work / "iperf-raw" / f"{ctx.arm.id}-ramp-{level}",
            )
            t1 = time.time()
    finally:
        with contextlib.suppress(OSError):
            probe.kill()
        with contextlib.suppress(Exception):
            probe.wait(timeout=5)
    interaction = read_series(log, t0, t1)
    rtts = [p["v"] for p in interaction if p["metric"] == "rtt_ms"]
    errors = [p for p in interaction if p["metric"] == "error"]
    metrics = {
        "throughput_gbps": result.get("gbps_received_own_window"),
        "rtt_samples": len(rtts) or None,
        "rtt_p99_ms": inst.pct(rtts, 0.99),
        "rtt_p50_ms": inst.pct(rtts, 0.50),
        "rtt_error_rate_pct": (
            round(100.0 * len(errors) / (len(rtts) + len(errors)), 4)
            if (rtts or errors)
            else None
        ),
    }
    metrics["slo_broken"] = _breaks_slo(metrics)
    built = _iperf_cell(Spot(ctx, window, f"L{level}"), result, metrics)
    built.evidence["series_log"] = str(log)
    built.evidence["streams"] = level
    return built


def run_reconnect(ctx: Ctx, params: dict) -> list:
    """Cold start: how long after a restart a visitor's first connection works.

    The runner owns the processes (it is the thing that can restart an arm), so
    the workload asks it to: five restarts, each timed from spawn to the first
    answered connection, which is what a visitor actually waits for.
    """
    restart_arm = ctx.restart_arm
    if restart_arm is None:
        return [Cell("", False, reason="this arm cannot be restarted")]
    starts: list = []
    failures: list = []
    for _ in range(max(1, params["reps"])):
        seconds, why = restart_arm()
        if seconds is None:
            failures.append(why)
        else:
            starts.append(seconds)
    if not starts:
        return [Cell("", False, reason=f"no cold start succeeded: {failures[:2]}")]
    return [
        Cell(
            "cold",
            ok=True,
            metrics={
                "startup_s": round(statistics.median(starts), 4),
                "startup_max_s": round(max(starts), 4),
            },
            evidence={
                "starts_s": starts,
                "failures": failures[:5],
                "reps": params["reps"],
            },
        )
    ]


RUNNERS = {
    model.Kind.BULK: run_bulk,
    model.Kind.BULK_PAIR: run_bulk_pair,
    model.Kind.RR: run_rr,
    model.Kind.UDP: run_udp,
    model.Kind.UDP_LADDER: run_udp_ladder,
    model.Kind.STAGED: run_staged,
    model.Kind.CAPACITY: run_capacity,
    model.Kind.RECONNECT: run_reconnect,
}


def run(kind: str, ctx: Ctx, params: dict) -> list:
    return RUNNERS[kind](ctx, params)
