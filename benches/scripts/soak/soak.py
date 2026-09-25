#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Soak: measure workloads under staged network conditions as time series.

The model (docs/release.md, "Benchmarks"): a *tool* is driven through a
scripted workload — one interactive stream (the SLO instrument), N bulk TCP
streams, C short connections per second and one UDP session — while the
network condition changes on a stage schedule, in place (`tc qdisc change`;
the session is never rebuilt). Everything measured is externally
observable, so peers are measured with exactly the same workload.

Test types
----------
capacity  ramp the bulk load until the interactive stream breaks the SLO;
          report the sustainable load plus the response-time curve
rrul      saturate (N = cpu count) and watch the interactive stream's RTT
          distribution *over time* — the queueing-under-load detector
soak      fixed load under a rotating path schedule, long: the drift, leak
          and recovery axis
cost      at a fixed operating point, CPU-seconds per carried Gbit/s
screen    fast A/B: one test type x one path class x one config pair, the
          two builds interleaved inside every step, sequential decision

Parallelism: each tool in a batch owns its port band and its own shaped
path (one HTB class + independent netem on `lo`), so tools never share a
shaper. The pair under comparison always runs in the same batch.
"""

# E402 is waived file-wide: the sys.path insert below is the bench-lib
# import pattern and must precede the third-party imports.
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import argparse
import contextlib
import json
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time
from dataclasses import asdict, dataclass

import lib

WORKLOAD_VERSION = 1


@dataclass(frozen=True)
class Stage:
    """One step of the schedule: the path class and how long it holds."""

    path: str
    secs: float


@dataclass(frozen=True)
class PathClass:
    """One condition a stage can impose on the path.

    `netem` holds positional arguments for `tc qdisc ... netem` (this iproute2
    spells jitter as the second positional after `delay` and has no `jitter`
    keyword); an empty list means "no netem": the stage is either the unshaped
    control or limited only by `mtu`.

    `mtu` is a different axis and a different mechanism: it is an *interface*
    property (`ip link set dev lo mtu`), not a qdisc, so it cannot be per-tool
    or per-port the way the netem classes are. A stage that sets it changes the
    path for every packet on `lo` during that stage — the peers', the harness's
    and the control plane's included — which is why it is a first-class field
    the results meta records rather than a hidden side effect, and why the
    shaper verifies the restore on teardown. It exists because IPv4
    fragmentation is the one real-network failure mode the netem classes
    cannot produce: `lo` is MTU 65536, so without it every datagram KCP emits
    fits in one fragment and the amplification a lost fragment causes is
    unmeasurable.
    """

    netem: list
    mtu: int | None = None


# The stage schedule's vocabulary. `clean` is the unshaped control; the two
# MTU classes carry the fragmentation axis (see `PathClass.mtu`).
PATH_CLASSES = {
    "clean": PathClass([]),
    "rtt100": PathClass(["delay", "100ms"]),
    "loss1": PathClass(["delay", "10ms", "loss", "1%"]),
    "loss5": PathClass(["delay", "100ms", "loss", "5%"]),
    "rate100": PathClass(["rate", "100mbit", "delay", "20ms", "limit", "2000"]),
    "rate20": PathClass(["rate", "20mbit", "delay", "40ms", "limit", "2000"]),
    "jitter": PathClass(["delay", "20ms", "10ms"]),
    # --- the fragmentation axis (interface-wide; see PathClass) -------------
    # Shrink the path MTU to the IPv6 minimum every real deployment tolerates.
    # `loss1_mtu1280` is the cell that matters: the same 1% *fragment* loss as
    # `loss1`, on a path where KCP's 1400-byte datagrams become two fragments,
    # so one lost fragment costs the whole datagram.
    "mtu1280": PathClass([], mtu=1280),
    "loss1_mtu1280": PathClass(["delay", "10ms", "loss", "1%"], mtu=1280),
}

DEFAULT_TIMELINE = [
    Stage("clean", 150),
    Stage("rtt100", 120),
    Stage("loss1", 120),
    Stage("loss5", 120),
    Stage("rate100", 120),
    Stage("rate20", 120),
    Stage("jitter", 120),
    Stage("clean", 150),
]
SOAK_TIMELINE = [
    Stage("clean", 180),
    Stage("loss1", 180),
    Stage("rtt100", 180),
    Stage("loss5", 180),
    Stage("clean", 180),
]


def log(*a) -> None:
    """Print a run-progress line immediately (a run is watched live)."""
    print(*a, flush=True)


# --- per-tool shaping -------------------------------------------------------
class Shaper:
    """Per-tool shaped paths on `lo`: one HTB root, one class per tool.

    A tool's traffic is classified into its own class only while the path
    is shaped: on a clean stage the filters point at the unshaped default
    class, so the tool's packets pay no HTB/netem tax at all. That matters:
    measured on this host, an always-classified clean path costs ~25% of
    throughput (12.5 -> 9.3 Gbit/s) and the netem child another ~20%, so a
    model that shaped the clean stages would measure the shaper, not the
    tool. Each tool keeps a distinct filter priority so one tool's filters
    can be rewritten without touching another's.

    Unmatched traffic (the harness's own control) stays in the default
    class. Every port the harness dials is classified, so a
    misclassification is impossible by construction.
    """

    DEFAULT = "1:999"
    MTU_PATH = "/sys/class/net/lo/mtu"

    def __init__(self, classes: list, log=print):
        self.classes = classes  # [(classid, band)]
        self.log = log
        self.orig_mtu = self._read_mtu()
        self.mtu_now = self.orig_mtu

    def _read_mtu(self) -> int:
        try:
            return int(Path(self.MTU_PATH).read_text().strip())
        except (OSError, ValueError) as e:
            raise RuntimeError(f"cannot read {self.MTU_PATH}: {e}") from e

    def _set_mtu(self, mtu: int) -> None:
        """Set the interface MTU, loudly.

        `ip` ships with `tc`, so a missing binary means the harness is
        incomplete, not that the stage silently runs unshaped — which would
        measure the wrong path and record it as if it were the right one.
        """
        r = subprocess.run(
            ["ip", "link", "set", "dev", "lo", "mtu", str(mtu)],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode != 0:
            raise RuntimeError(f"ip link set lo mtu {mtu}: {r.stderr.strip()[:200]}")
        self.mtu_now = mtu

    def _apply_mtu(self, want: int | None) -> None:
        """Move the interface to `want`, restoring the original when None."""
        target = self.orig_mtu if want is None else want
        if target != self.mtu_now:
            self._set_mtu(target)
            self.log(f"    lo mtu -> {target} (interface-wide for this stage)")

    def _tc(self, *args) -> None:
        r = subprocess.run(["tc", *args], capture_output=True, text=True, check=False)
        if r.returncode != 0:
            raise RuntimeError(f"tc {' '.join(args)}: {r.stderr.strip()[:400]}")

    def _ports(self, band: dict) -> list:
        # The data-plane ports only: the TOOL's control channel stays in
        # the unshaped default class. A capacity measurement that shapes
        # the control plane kills the tool's heartbeat (measured: 40 s
        # timeout on a 100 mbit cell) and the run becomes a wedge study
        # instead of a capacity study.
        return [
            band[k]
            for k in (
                "iperf_exposed",
                "echo_exposed",
                "udp_exposed",
                "kcp_bind",
                "iperf_backend",
                "echo_backend",
                "udp_backend",
            )
        ]

    def _prio(self, cid: str) -> str:
        return str(10 + int(cid.split(":")[1]))

    def _filters(self, cid: str, band: dict, flowid: str) -> None:
        """Point this tool's filters at `flowid`, replacing what was there.

        `tc filter replace` ADDS a second filter when the match differs
        instead of replacing — and the first match wins, so a stage change
        that only re-points the flowid silently leaves the traffic in the
        previous class (measured: an unshaped interactive stream on a
        "rtt100" stage). Delete the tool's priority group first, then add.
        """
        prio = self._prio(cid)
        with contextlib.suppress(Exception):
            self._tc("filter", "del", "dev", "lo", "parent", "1:", "prio", prio)
        for port in self._ports(band):
            for key in ("dport", "sport"):
                self._tc(
                    "filter",
                    "add",
                    "dev",
                    "lo",
                    "parent",
                    "1:",
                    "prio",
                    prio,
                    "protocol",
                    "ip",
                    "u32",
                    "match",
                    "ip",
                    key,
                    str(port),
                    "0xffff",
                    "flowid",
                    flowid,
                )

    def build(self) -> None:
        # delete any existing root first: `qdisc replace` cannot CHANGE a
        # root qdisc into a different kind (a leftover netem root fails
        # with "Change operation not supported"), so a survivor from an
        # earlier experiment would abort the whole run.
        with contextlib.suppress(Exception):
            self._tc("qdisc", "delete", "dev", "lo", "root")
        self._tc(
            "qdisc", "add", "dev", "lo", "root", "handle", "1:", "htb", "default", "999"
        )
        for cid, band in self.classes:
            minor = cid.split(":")[1]
            self._tc(
                "class",
                "replace",
                "dev",
                "lo",
                "parent",
                "1:",
                "classid",
                cid,
                "htb",
                "rate",
                "10gbit",
            )
            self._tc(
                "qdisc",
                "replace",
                "dev",
                "lo",
                "parent",
                cid,
                "handle",
                f"{minor}0:",
                "netem",
            )
            # start unclassified: a clean stage creates no HTB path
            self._filters(cid, band, self.DEFAULT)
        self.log(
            f"    shaper: {len(self.classes)} tool class(es) on lo "
            f"(clean stages stay in the default class)"
        )

    def apply(self, cid: str, stage: str) -> None:
        band = next(b for c, b in self.classes if c == cid)
        path = PATH_CLASSES.get(stage, PathClass([]))
        # MTU first: it is an interface property, so it applies to whatever the
        # qdisc does below, and a stage without it must restore the original.
        self._apply_mtu(path.mtu)
        args = path.netem
        minor = cid.split(":")[1]
        if not args:
            # an unshaped stage: no HTB path, no netem tax
            self._filters(cid, band, self.DEFAULT)
            self.log(f"    {cid} path={stage} (unshaped, default class)")
            return
        self._tc(
            "qdisc",
            "replace",
            "dev",
            "lo",
            "parent",
            cid,
            "handle",
            f"{minor}0:",
            "netem",
            *args,
        )
        self._filters(cid, band, cid)
        self.log(f"    {cid} path={stage} ({' '.join(args)})")

    def teardown(self) -> None:
        with contextlib.suppress(Exception):
            self._tc("qdisc", "delete", "dev", "lo", "root")
        # A run that leaves `lo` at 1280 poisons every later run on this host
        # (and every other tenant of it), so the restore is verified rather
        # than assumed: a mismatch is a hard failure with the fix in the
        # message.
        if self.mtu_now != self.orig_mtu:
            self._set_mtu(self.orig_mtu)
        got = self._read_mtu()
        if got != self.orig_mtu:
            raise RuntimeError(
                f"lo mtu is {got} after teardown, expected {self.orig_mtu} — "
                f"restore it with: ip link set dev lo mtu {self.orig_mtu}"
            )


# --- one tool's process pair ------------------------------------------------
class Tool:
    """A tool instance: its processes, its port band, its sampler targets."""

    def __init__(self, name: str, variant: str, band: dict, knobs, work: Path):
        self.name = name
        self.variant = variant
        self.band = band
        self.knobs = knobs
        self.work = work
        self.label = f"{name} ({variant})" if variant else name
        self.procs = lib.ArmProcs(work, f"{name} {variant}".strip())
        self.coverage = {
            "tcp_bulk": True,
            "tcp_interactive": True,
            "tcp_churn": True,
            "udp_session": True,
        }

    def start(self, binary: str = "") -> None:
        p = self.band
        # One setup value and one signature for every tool (lib.ToolSetup →
        # `setup(s, knobs)`), so the dispatch is a dict lookup rather than four
        # lambdas that can drift from the functions they call — they did, and
        # every molehill arm failed with a TypeError until a smoke run caught
        # it.
        s = lib.ToolSetup(
            band=p,
            procs=self.procs,
            work=self.work,
            variant=self.variant,
            binary=binary,
        )
        lib.TOOL_SETUPS[self.name](s, self.knobs)
        for port in (p["iperf_exposed"], p["echo_exposed"]):
            if not lib.wait_port(port, 30):
                raise TimeoutError(f"{self.label}: exposed port {port} not ready")

    def restart(self, binary: str = "") -> None:
        """Restart the tool's processes with another build: the screen's
        per-step swap. The cold start is paid once per step, which is the
        price of measuring a different binary."""
        self.stop()
        self.procs = lib.ArmProcs(self.work, f"{self.name} {self.variant}".strip())
        self.start(binary)

    def stop(self) -> None:
        self.procs.kill()

    @property
    def pids(self) -> tuple:
        p = self.procs.tool_pids
        return (p[0] if p else 0, p[1] if len(p) > 1 else 0)

    def pids_of(self) -> tuple:
        """The tool's current process pair, read fresh each tick: a screen's
        per-step restart replaces the processes, and the samplers must
        follow."""
        return self.pids

    def version(self, binary: str = "") -> str:
        if self.name == "molehill":
            k = lib.Knobs(
                molehill_bin=binary or self.knobs.molehill_bin,
                peer_dir=self.knobs.peer_dir,
            )
            return lib.tool_version(k)
        return lib.peer_version(self.name, self.knobs)


# --- the workload's client-side probes --------------------------------------
# The interactive stream and the UDP session run as their own PROCESS, not
# as threads of the harness: the echo backend would otherwise share the
# GIL with the samplers and the other probe, and the SLO instrument must
# never perturb what it measures (an in-process echo backend measured
# ~2 s interactive RTT under 20 bulk streams, where an isolated one is
# in the milliseconds — the difference was the instrument).
PROBE_SRC = """
import socket, sys, threading, time

mode, port, interval = (sys.argv[1], int(sys.argv[2]),
                        float(sys.argv[3]))
backend_port = int(sys.argv[4])
out = sys.stdout


def emit(metric, value):
    out.write(f"{time.time():.3f}\\t{metric}\\t{value}\\n")
    out.flush()


def echo_server(srv):
    while True:
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        threading.Thread(target=echo_conn, args=(conn,), daemon=True).start()


def echo_conn(conn):
    with conn:
        while True:
            data = conn.recv(4096)
            if not data:
                return
            conn.sendall(data)


def udp_server(srv):
    while True:
        try:
            data, addr = srv.recvfrom(2048)
        except OSError:
            return
        srv.sendto(data, addr)


if mode == "interactive":
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", backend_port))
    srv.listen(16)
    threading.Thread(target=echo_server, args=(srv,), daemon=True).start()
    payload = b"x" * 64
    while True:
        t0 = time.perf_counter()
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5.0)
            s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            s.sendall(payload)
            got = 0
            while got < len(payload):
                chunk = s.recv(len(payload) - got)
                if not chunk:
                    raise ConnectionError("closed")
                got += len(chunk)
            emit("rtt_interactive_ms", round((time.perf_counter() - t0) * 1000.0, 3))
            s.close()
        except Exception:
            emit("rtt_interactive_error", 1)
        time.sleep(interval)
elif mode == "udp":
    srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    srv.bind(("127.0.0.1", backend_port))
    threading.Thread(target=udp_server, args=(srv,), daemon=True).start()
    payload = b"u" * 64
    cli = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    cli.settimeout(max(0.5, interval * 4))
    while True:
        t0 = time.perf_counter()
        # One attempt line per ping: the loss rate needs its denominator
        # (the outcome line alone cannot tell loss from a slow ping).
        emit("udp_attempt", 1)
        try:
            cli.sendto(payload, ("127.0.0.1", port))
            data, _ = cli.recvfrom(2048)
            if data == payload:
                emit("rtt_udp_ms", round((time.perf_counter() - t0) * 1000.0, 3))
            else:
                emit("udp_loss", 1)
        except TimeoutError:
            emit("udp_loss", 1)
        time.sleep(interval)
elif mode == "churn":
    # The connection-establishment axis: a steady rate of fresh short
    # connections (the real-world HTTP/lobby pattern), each measured for
    # its connect+echo time. It reuses the interactive probe's echo
    # backend — the tool forwards both to the same service — so this
    # process binds nothing and only dials.
    payload = b"c" * 64
    rate = float(sys.argv[5])
    while True:
        t0 = time.perf_counter()
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5.0)
            s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            s.sendall(payload)
            got = 0
            while got < len(payload):
                chunk = s.recv(len(payload) - got)
                if not chunk:
                    raise ConnectionError("closed")
                got += len(chunk)
            emit("churn_setup_ms", round((time.perf_counter() - t0) * 1000.0, 3))
            s.close()
        except Exception:
            emit("churn_error", 1)
        time.sleep(max(0.0, 1.0 / rate - (time.perf_counter() - t0)))

"""


class Pingers:
    """The workload's client-side probes, each in its own process.

    A fresh connection per ping for the interactive stream (the
    connection-path RTT is what a new visitor experiences) and one UDP
    session for the UDP axis. The processes print `t\tmetric\tvalue`
    lines; the parent reads them into the shared series.
    """

    def __init__(self, band: dict, knobs: lib.Knobs, out: list, work: Path):
        self.band, self.knobs, self.out, self.work = band, knobs, out, work
        self.procs: list = []
        self.readers: list = []
        self.stop = threading.Event()

    def log_path(self, mode: str) -> str:
        return str(Path(self.work) / f"probe-{mode}-{self.band['echo_exposed']}.log")

    def _spawn(
        self,
        mode: str,
        port: int,
        interval: float,
        backend_port: int = 0,
        rate: float = 0.0,
    ) -> None:
        log_path = self.log_path(mode)
        with open(log_path, "w") as errlog:
            proc = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    PROBE_SRC,
                    mode,
                    str(port),
                    str(interval),
                    str(backend_port),
                    str(rate),
                ],
                stdout=subprocess.PIPE,
                stderr=errlog,
                text=True,
                bufsize=1,
            )
        self.procs.append(proc)

        def reader() -> None:
            for line in proc.stdout:
                parts = line.strip().split("\t")
                if len(parts) != lib.PROBE_FIELDS:
                    continue
                try:
                    t, v = float(parts[0]), float(parts[2])
                except ValueError:
                    continue
                self.out.append({"t": t, "metric": parts[1], "v": v})

        th = threading.Thread(target=reader, daemon=True)
        th.start()
        self.readers.append(th)

    def start(self) -> None:
        """Bind the echo backends the TOOL forwards to, then dial the tool.

        The probe process is therefore both ends of the interactive path
        (a fresh TCP connection per ping) and of the UDP session, with the
        tool's tunnel in between and the harness out of the measured path.
        """
        self._spawn(
            "interactive",
            self.band["echo_exposed"],
            self.knobs.ping_interval_ms / 1000.0,
            backend_port=self.band["echo_backend"],
        )
        self._spawn(
            "udp",
            self.band["udp_exposed"],
            self.knobs.udp_interval_ms / 1000.0,
            backend_port=self.band["udp_backend"],
        )
        self._spawn(
            "churn",
            self.band["echo_exposed"],
            1.0 / max(1, self.knobs.churn_connects_s),
            backend_port=self.band["echo_backend"],
            rate=float(self.knobs.churn_connects_s),
        )

    def stop_and_join(self) -> None:
        self.stop.set()
        for proc in self.procs:
            with contextlib.suppress(Exception):
                proc.kill()
        for th in self.readers:
            th.join(timeout=2)


class Samplers:
    """Per-process RSS / CPU / handle counts, written into the same series.

    The handle counts are the drift axis for black boxes too: a leak shows
    up as a slope in /proc/<pid>/fd regardless of what the tool is. The
    pids are read per tick through `pids_of`, so a tool restart (the
    screen's per-step build swap) is followed instead of silently
    sampling dead processes.
    """

    def __init__(self, pids_of, out: list):
        self.pids_of, self.out, self.stop = pids_of, out, threading.Event()
        self.threads: list = []

    def _sampler(self, fn) -> None:
        while not self.stop.is_set():
            now = time.time()
            for label, pid in zip(("server", "client"), self.pids_of()):
                v = fn(pid)
                if v is None:
                    continue
                self.out.append(
                    {"t": round(now, 3), "metric": f"{label}_{fn.__name__}", "v": v}
                )
            self.stop.wait(0.5)

    @staticmethod
    def rss_kb(pid: int):
        try:
            with open(f"/proc/{pid}/statm") as fh:
                return int(fh.read().split()[1]) * (os.sysconf("SC_PAGE_SIZE") // 1024)
        except (OSError, ValueError, IndexError):
            return None

    @staticmethod
    def fds(pid: int):
        try:
            return len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            return None

    @staticmethod
    def thread_count(pid: int):
        try:
            return len(os.listdir(f"/proc/{pid}/task"))
        except OSError:
            return None

    @staticmethod
    def cpu_ticks(pid: int):
        try:
            with open(f"/proc/{pid}/stat") as fh:
                f = fh.read().split(") ", 1)[1].split()
            return int(f[11]) + int(f[12])
        except (OSError, ValueError, IndexError):
            return None

    def start(self) -> None:
        for fn in (self.rss_kb, self.fds, self.thread_count):
            self.threads.append(
                threading.Thread(target=self._sampler, args=(fn,), daemon=True)
            )
        # CPU as a delta of ticks over the wall interval
        self.threads.append(threading.Thread(target=self._cpu_loop, daemon=True))
        for t in self.threads:
            t.start()

    def _cpu_loop(self) -> None:
        prev = {p: (self.cpu_ticks(p), time.monotonic()) for p in self.pids_of() if p}
        while not self.stop.is_set():
            self.stop.wait(0.5)
            now = time.monotonic()
            for label, pid in zip(("server", "client"), self.pids_of()):
                if not pid:
                    continue
                cur = self.cpu_ticks(pid)
                if cur is None:
                    continue
                p0, t0 = prev.get(pid, (cur, now))
                wall = now - t0
                if wall > 0:
                    self.out.append(
                        {
                            "t": round(time.time(), 3),
                            "metric": f"{label}_cpu_pct",
                            "v": round((cur - p0) / wall / 100.0, 1),
                        }
                    )
                prev[pid] = (cur, now)

    def stop_and_join(self) -> None:
        self.stop.set()
        for t in self.threads:
            t.join(timeout=3)


# --- test types -------------------------------------------------------------
@dataclass
class RunContext:
    """What every test type needs, in one value.

    The runners used to take eight positional arguments, most of them unused
    by any given test type; a runner that silently ignores a parameter is how
    the SLO knob and the load fractions drifted out of the measured path.

    `backends` and `load` are filled in per test — the backends once they are
    up, the load when the test type picks its operating point — so the stage
    runners take `(tool, ctx, entry)` and nothing else.
    """

    args: argparse.Namespace
    knobs: lib.Knobs
    timeline: list
    shaper: "Shaper"
    backends: lib.Backends | None = None
    load: int = 0

    def with_backends(self, backends: lib.Backends) -> "RunContext":
        self.backends = backends
        return self

    @property
    def ceiling(self) -> int:
        """The configured maximum load, in bulk streams."""
        return self.args.streams_max or self.knobs.streams_max

    def load_for(self, test: str) -> int:
        """The fixed operating point of a staged test, from the knobs.

        Neither `soak` nor `cost` measures capacity first, so the fraction is
        of the *configured* ceiling and the results meta says so; naming it
        "fraction of measured capacity" was a claim the runner never checked.
        """
        fraction = (
            self.knobs.cost_operating_point
            if test == "cost"
            else self.knobs.soak_load_fraction
        )
        return max(1, round(self.ceiling * fraction))


def stage_spine(tool: Tool, ctx: RunContext, entry: dict, stage: Stage) -> dict:
    """Run one stage's bulk load through the tool and record its intervals.

    The bulk is per stage ON PURPOSE: a spine that dies (a wedged tunnel, a
    shaped-path timeout) must not poison the remaining stages, and the
    stage where it comes back is exactly the recovery axis the model
    claims to measure. Either way the stage's full duration elapses — the
    probes and samplers keep measuring the tool whether or not bulk is
    running. A stage whose spine delivers nothing after the server-hygiene
    retry records the failure with its reason instead of a fake 0.

    The server is the iperf3 instance the backends started once: it is
    single-test, so a spine this stage killed leaves it wedged for the
    next dial ("unable to receive cookie"/exit 1). One restart-and-retry
    per stage keeps one bad sample from becoming a dead axis.
    """
    target = lib.ThroughputTarget.from_band(tool.band)
    cmd = [
        "iperf3",
        "-c",
        "127.0.0.1",
        "-p",
        str(target.exposed),
        "-t",
        str(int(stage.secs)),
        "-O",
        "2",
        "-P",
        str(ctx.load),
        "-i",
        "1",
        "--json-stream",
    ]
    t_end = time.time() + stage.secs
    outcome = {}
    for attempt in (0, 1):
        outcome = _spine_once(cmd, entry, t_end)
        if outcome["intervals"]:
            return outcome
        if attempt == 0:
            with contextlib.suppress(Exception):
                ctx.backends.restart_iperf()
    # the stage's full duration elapses regardless: a dead spine must not
    # cut the probes' and samplers' window short
    while time.time() < t_end:
        time.sleep(min(1.0, max(0.1, t_end - time.time())))
    return outcome


def _spine_once(cmd: list, entry: dict, t_end: float) -> dict:
    """One iperf3 client attempt for a stage's bulk load."""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, bufsize=1)
    intervals = 0
    try:
        while time.time() < t_end and proc.poll() is None:
            line = proc.stdout.readline()
            if not line:
                break
            with contextlib.suppress(ValueError):
                d = json.loads(line)
                if d.get("event") != "interval":
                    continue
                s = (d.get("data") or {}).get("sum") or {}
                if s and not s.get("omitted"):
                    now = round(time.time(), 3)
                    intervals += 1
                    # `span_s` is the interval's own length. It is recorded
                    # because a jammed tunnel makes iperf3 emit one wide
                    # catch-up interval whose average describes a much longer
                    # window than the nominal 1 s — a reader (and the chart)
                    # has to be able to tell it apart from a normal sample.
                    entry["series"].append(
                        {
                            "t": now,
                            "metric": "throughput_bulk_gbps",
                            "v": round(s.get("bits_per_second", 0) / 1e9, 4),
                            "span_s": round(
                                max(0.0, s.get("end", 0.0) - s.get("start", 0.0)),
                                3,
                            ),
                        }
                    )
                    entry["series"].append(
                        {
                            "t": now,
                            "metric": "bulk_retransmits",
                            "v": s.get("retransmits", 0),
                        }
                    )
    finally:
        proc.kill()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)
    return {"intervals": intervals, "exit": proc.returncode}


def record_stage(entry: dict, mark: int) -> None:
    """Derive one stage's statistics from the series since `mark`."""
    window = entry["series"][mark:]
    st = lib.series_stats(window, "rtt_interactive_ms")
    udp = lib.series_stats(window, "rtt_udp_ms")
    churn = lib.series_stats(window, "churn_setup_ms")
    it_err = lib.series_stats(window, "rtt_interactive_error").get("n", 0)
    attempts = st.get("n", 0) + it_err
    losses = lib.series_stats(window, "udp_loss").get("n", 0)
    udp_attempts = lib.series_stats(window, "udp_attempt").get("n", 0)
    worst = lib.worst_window(window, "rtt_interactive_ms")
    entry["stages"][-1].update(
        {
            "rtt_p99": st.get("p99"),
            "rtt_mean": st.get("mean"),
            "rtt_max": st.get("max"),
            "rtt_n": st.get("n"),
            "rtt_worst_1s": worst.get("mean") if worst else None,
            "rtt_error_n": it_err,
            "rtt_error_rate": round(it_err / attempts, 5) if attempts else None,
            "udp_p99": udp.get("p99"),
            "udp_mean": udp.get("mean"),
            "udp_loss_pct": (
                round(100.0 * losses / udp_attempts, 3) if udp_attempts else None
            ),
            "churn_p99": churn.get("p99"),
            "churn_per_s": churn.get("n"),
        }
    )
    flats = lib.flat_segments(window)
    if flats:
        entry["stages"][-1]["flat_segments"] = flats
    log(
        f"    interactive p99={st.get('p99')} mean={st.get('mean')} "
        f"n={st.get('n', 0)} err={it_err}; udp p99={udp.get('p99')} "
        f"loss={entry['stages'][-1]['udp_loss_pct']}%; "
        f"churn/s={churn.get('n', 0)} p99={churn.get('p99')}"
    )


def run_capacity(tool: Tool, ctx: RunContext, entry: dict) -> None:
    """Ramp the bulk load until the interactive stream breaks the SLO.

    The SLO is the knob's, not a module constant: the meta records the value
    the verdict was actually taken against, and the interactive error rate is
    part of it (the documented SLO is "under this p99 AND under this error
    rate"), not an afterthought.
    """
    knobs = ctx.knobs
    target = lib.ThroughputTarget.from_band(tool.band)
    sustainable = 0
    for streams in range(1, ctx.ceiling + 1):
        mark = len(entry["series"])
        r = ctx.backends.iperf_burst(
            target, streams, knobs.settle_s, tag=f"{tool.label} cap"
        )
        window = entry["series"][mark:]
        st = lib.series_stats(window, "rtt_interactive_ms")
        p99 = st.get("p99")
        errors = lib.series_stats(window, "rtt_interactive_error").get("n", 0)
        err_rate = errors / max(1, st.get("n", 0) + errors)
        reasons = []
        if not r["ok"]:
            reasons.append(str(r.get("reason")))
        if p99 is not None and p99 > knobs.slo_rtt_p99_ms:
            reasons.append(f"interactive p99 {p99} > {knobs.slo_rtt_p99_ms}")
        if errors and err_rate > knobs.slo_error_rate:
            reasons.append(
                f"interactive error rate {err_rate:.3f} > {knobs.slo_error_rate}"
            )
        broken = bool(reasons)
        entry["metrics"].setdefault("curve", []).append(
            {
                "streams": streams,
                "gbps": r.get("gbps_headline"),
                "rtt_p99": p99,
                "rtt_mean": st.get("mean"),
                "rtt_n": st.get("n"),
                "rtt_error_rate": round(err_rate, 5),
                "slo_broken": broken,
                "reason": "; ".join(x for x in reasons if x) or None,
            }
        )
        log(
            f"    load {streams}: {r.get('gbps_headline', '-')} Gbit/s, "
            f"interactive p99={p99} err={err_rate:.4f} (n={st.get('n', 0)}) -> "
            f"{'BROKEN' if broken else 'ok'}"
        )
        if broken:
            break
        sustainable = streams
    entry["metrics"]["max_sustainable_streams"] = sustainable
    entry["metrics"]["headroom"] = round(1 - sustainable / ctx.ceiling, 4)


def run_rrul(tool: Tool, ctx: RunContext, entry: dict) -> None:
    """Saturate the path and watch the interactive stream's RTT over time.

    N = cpu count x the factor (the canonical saturation), the interactive
    stream's RTT distribution over time through the stage schedule — the
    queueing-under-load detector, with the return-to-clean stage as the
    recovery axis.
    """
    ctx.load = max(1, ctx.knobs.rrul_stream_factor * (os.cpu_count() or 1))
    entry["metrics"]["bulk_streams"] = ctx.load
    for stage in ctx.timeline:
        run_one_stage(tool, ctx, entry, stage)


def run_staged(tool: Tool, ctx: RunContext, entry: dict) -> None:
    """soak / cost: drive the timeline under a fixed load, sample every stage.

    `soak` carries `soak_load_fraction` of the configured ceiling (the
    drift/leak axis is measured under load); `cost` carries
    `cost_operating_point` of it and additionally derives CPU-seconds per
    carried Gbit at that operating point. Both fractions are knobs, and both
    are recorded in the meta.
    """
    ctx.load = ctx.load_for(ctx.args.test)
    entry["metrics"]["bulk_streams"] = ctx.load
    for stage in ctx.timeline:
        mark = run_one_stage(tool, ctx, entry, stage)
        if ctx.args.test == "cost":
            record_cost(entry, mark, ctx.load)


def run_one_stage(tool: Tool, ctx: RunContext, entry: dict, stage: Stage) -> int:
    """Shape one stage, run its bulk spine, record the stage's stats."""
    ctx.shaper.apply(tool.cid, stage.path)
    mark = len(entry["series"])
    entry["stages"].append(
        {"stage": stage.path, "secs": stage.secs, "t_start": round(time.time(), 3)}
    )
    log(f"  stage {stage.path} ({stage.secs}s)")
    outcome = stage_spine(tool, ctx, entry, stage)
    if not outcome["intervals"]:
        entry["stages"][-1]["bulk_error"] = (
            f"spine produced no intervals (exit {outcome['exit']})"
        )
        log(f"    bulk spine produced nothing (exit {outcome['exit']})")
    record_stage(entry, mark)
    return mark


def record_cost(entry: dict, mark: int, streams: int) -> None:
    """CPU-seconds per carried Gbit over one stage's burst window.

    The denominator is the stage's duration, not the sum of the interval
    spans: the spine is cut off at the stage boundary, so a shorter sum
    would inflate the cost. Both the carried mean and the CPU mean come from
    the same window.
    """
    stage = entry["stages"][-1]
    bulk = lib.series_stats(entry["series"][mark:], "throughput_bulk_gbps")
    carried = (bulk.get("mean") or 0) * stage["secs"]
    if carried <= 0 or not bulk.get("n"):
        stage["cost_error"] = "no carried bytes in this stage"
        return
    cpu = (
        lib.series_stats(entry["series"][mark:], "server_cpu_pct").get("mean") or 0.0
    ) + (lib.series_stats(entry["series"][mark:], "client_cpu_pct").get("mean") or 0.0)
    cost = (cpu / 100.0 * stage["secs"]) / carried
    stage["bulk_streams"] = streams
    stage["cost_cpu_per_gbit"] = round(cost, 4)
    total = entry["metrics"].get("cost_cpu_per_gbit_sum", 0.0) + cost
    stages = entry["metrics"].get("cost_stages", 0) + 1
    entry["metrics"]["cost_cpu_per_gbit"] = round(total / stages, 4)
    entry["metrics"]["cost_cpu_per_gbit_sum"] = total
    entry["metrics"]["cost_stages"] = stages
    log(
        f"    cost: {cost:.4f} CPU-s per carried Gbit "
        f"({streams} streams, {bulk.get('mean')} Gbit/s)"
    )


def run_screen(tool: Tool, ctx: RunContext, entry: dict) -> None:
    """Fast A/B: two builds, interleaved inside every step of one test.

    The pair runs in the same batch (same epoch) and the tool's processes
    are swapped between the two builds at every load step, so both sample
    the same machine state — sequential before/after runs are defeated by
    epoch drift, which is the whole reason this exists.

    The path is constant for the whole comparison: `--path` is applied once,
    before the interleave, and never changes between the two builds — a shape
    differing between them would be a second variable, which is what makes
    this a single-variable test rather than two measurements. It used to be
    recorded without being applied at all, which made the results meta
    describe a path the run never had (a `screen` run labelled `loss1` was
    clean traffic); applying it once is also what lets a shaped cell — the
    MTU/fragmentation cell, for instance — be A/B-ed at all.
    """
    build_a, build_b = ctx.args.ab
    target = lib.ThroughputTarget.from_band(tool.band)
    entry["metrics"]["builds"] = {
        "A": build_a,
        "B": build_b,
        "A_version": tool.version(build_a),
        "B_version": tool.version(build_b),
    }
    if ctx.args.path:
        ctx.shaper.apply(tool.cid, ctx.args.path)
    rounds = []
    for step in range(1, ctx.ceiling + 1):
        pair = []
        for label, binary in (("A", build_a), ("B", build_b)):
            tool.restart(binary)
            mark = len(entry["series"])
            r = ctx.backends.iperf_burst(
                target, step, ctx.knobs.settle_s, tag=f"{tool.label} {label}"
            )
            st = lib.series_stats(entry["series"][mark:], "rtt_interactive_ms")
            pair.append(
                {
                    "build": label,
                    "gbps": r.get("gbps_headline"),
                    "rtt_p99": st.get("p99"),
                    "rtt_n": st.get("n"),
                    "rtt_mean": st.get("mean"),
                }
            )
        rounds.append({"streams": step, "pair": pair})
        log(
            f"    step {step}: "
            + " | ".join(
                f"{p['build']} {p['gbps']} Gbit/s p99={p['rtt_p99']}" for p in pair
            )
        )
    entry["metrics"]["rounds"] = rounds


#: Cold-start repetitions per build. Five is the smallest count that gives a
#: median and a spread worth quoting; the probe is cheap enough to afford it.
RECONNECT_REPS = 5


def run_reconnect(tool: Tool, ctx: RunContext, entry: dict) -> None:
    """Cold start: how long from a client start until every service answers?

    The measurement no other test type can make. Every probe in this harness
    dials a *running* tool, so the setup cost a client pays — registering its
    services, opening the control channel, authenticating — is invisible to
    all of them, and a change that only moves that cost (opening the data
    channels concurrently, a resumable handshake) had no metric to win on. The
    unit is seconds from `start` to the last service answering; per service, so
    a regression in one registration is visible, and repeated, because the
    number is a tail as much as a mean.

    Both builds are restarted in turn inside one run, like the screen: the
    comparison is against the same machine state, not against yesterday.
    """
    # `--ab` is a pair of paths, not labelled pairs: label them here.
    builds = (
        (("A", ctx.args.ab[0]), ("B", ctx.args.ab[1]))
        if ctx.args.ab
        else (("A", ctx.knobs.molehill_bin),)
    )
    if ctx.args.ab:
        entry["metrics"]["builds"] = {
            "A": ctx.args.ab[0],
            "B": ctx.args.ab[1],
            "A_version": tool.version(ctx.args.ab[0]),
            "B_version": tool.version(ctx.args.ab[1]),
        }
    if ctx.args.path:
        ctx.shaper.apply(tool.cid, ctx.args.path)

    # Every registered service is a port the client has to get answering; the
    # slowest one is the cold-start time a user experiences.
    services = [
        ("iperf", tool.band["iperf_exposed"]),
        ("echo", tool.band["echo_exposed"]),
    ]
    samples = []
    for rep in range(RECONNECT_REPS):
        for label, binary in builds:
            # The old listener must be gone before the clock starts: `stop`
            # kills the processes, but a socket that outlives its process (or a
            # port still in TIME_WAIT) would answer the first poll and report a
            # 0.0001 s cold start that measured the previous tool.
            tool.stop()
            for _ in range(200):
                if not any(lib.port_open(port) for _, port in services):
                    break
                time.sleep(0.05)
            tool.procs = lib.ArmProcs(tool.work, f"{tool.name} {tool.variant}".strip())
            t0 = time.monotonic()
            tool.start(binary)
            firsts = {}
            for name, port in services:
                if not lib.wait_port(port, RECONNECT_TIMEOUT_S):
                    firsts[name] = None
                else:
                    firsts[name] = round(time.monotonic() - t0, 4)
            total = max((v for v in firsts.values() if v is not None), default=None)
            samples.append(
                {"build": label, "rep": rep, "per_service": firsts, "total_s": total}
            )
            log(
                f"    {label} rep {rep}: "
                + " ".join(f"{k}={v}s" for k, v in firsts.items())
                + f" total={total}s"
            )
    entry["metrics"]["samples"] = samples


#: A cold start that has not answered in this long is a failure, not a slow
#: start: the client's own registration timeout is shorter.
RECONNECT_TIMEOUT_S = 30.0


TEST_TYPES = {
    "capacity": run_capacity,
    "rrul": run_rrul,
    "soak": run_staged,
    "cost": run_staged,
    "screen": run_screen,
    "reconnect": run_reconnect,
}


# --- one tool's pass --------------------------------------------------------
def run_tool(tool: Tool, cid: str, ctx: RunContext, entry: dict) -> dict:
    """Run one test type against one tool pair and derive the test's metrics.

    The entry is filled in place: a failure part-way through leaves the
    series and the completed stages as the evidence for that failure.
    """
    args, knobs = ctx.args, ctx.knobs
    tool.cid = cid
    entry.update(
        {
            "test": args.test,
            "path": args.path,
            # The endpoint record (§10): which port each probe dialed, and which
            # port the backend listens on. `soak_check` re-checks the pair, so a
            # sample that measured the backend instead of the tool cannot pass
            # the gate just because the numbers look plausible.
            "endpoints": {
                "throughput": asdict(lib.ThroughputTarget.from_band(tool.band)),
                "interactive": {
                    "exposed": tool.band["echo_exposed"],
                    "backend": tool.band["echo_backend"],
                },
                "udp": {
                    "exposed": tool.band["udp_exposed"],
                    "backend": tool.band["udp_backend"],
                },
            },
        }
    )
    out = entry["series"]
    samplers = Samplers(tool.pids_of, out)
    pingers = Pingers(tool.band, knobs, out, tool.work)
    backends = lib.Backends()
    try:
        backends.start(
            lib.BackendPorts.from_band(tool.band), tool.work, echo_in_probe=True
        )
    except Exception as e:  # noqa: BLE001 — a dead backend is that test's data
        entry["error"] = f"Backends: {e}"
        return entry
    ctx.with_backends(backends)
    samplers.start()
    pingers.start()
    try:
        TEST_TYPES[args.test](tool, ctx, entry)
    finally:
        pingers.stop_and_join()
        samplers.stop_and_join()
        backends.stop()
    # derived metrics: stability and drift
    for metric, label in (
        ("rtt_interactive_ms", "interactive_rtt"),
        ("rtt_udp_ms", "udp_rtt"),
        ("throughput_bulk_gbps", "bulk_throughput"),
    ):
        entry["metrics"][f"{label}_stats"] = lib.series_stats(out, metric)
        w = lib.worst_window(out, metric)
        if w:
            entry["metrics"][f"{label}_worst_1s"] = w
    entry["metrics"]["flat_segments"] = lib.flat_segments(out)
    # the interactive error rate from the probe's own series: errors over
    # attempts, not over the other probes' samples
    it_ok = lib.series_stats(out, "rtt_interactive_ms").get("n", 0)
    it_err = lib.series_stats(out, "rtt_interactive_error").get("n", 0)
    entry["metrics"]["interactive_error_rate"] = round(
        it_err / max(1, it_ok + it_err), 5
    )
    entry["metrics"]["churn_error_rate"] = round(
        lib.series_stats(out, "churn_error").get("n", 0)
        / max(
            1,
            lib.series_stats(out, "churn_setup_ms").get("n", 0)
            + lib.series_stats(out, "churn_error").get("n", 0),
        ),
        5,
    )
    # The derived UDP loss rate is a series of its own (the raw probe stream
    # only carries 1-per-loss markers, which have no contrast to plot).
    out.extend(lib.loss_rate_series(out))
    # The drift axis skips the first stage: the connection-setup pool
    # allocation ramps RSS/fds once at startup, and warm-up is not a leak.
    drift_from = (
        entry["stages"][1]["t_start"]
        if len(entry["stages"]) > 1
        else out[0]["t"]
        if out
        else 0
    )
    for metric in (
        "server_rss_kb",
        "client_rss_kb",
        "server_fds",
        "client_fds",
        "server_threads",
        "client_threads",
        "server_cpu_pct",
        "client_cpu_pct",
    ):
        slope = lib.slope_per_min([r for r in out if r["t"] >= drift_from], metric)
        if slope is not None:
            entry["metrics"][f"{metric}_slope_per_min"] = slope
    entry["metrics"]["drift_from_t"] = round(drift_from, 3)
    return entry


@dataclass
class Results:
    """The run's output file, its method record and the tests so far."""

    path: Path
    meta: dict
    tests: list

    def checkpoint(self) -> None:
        """Atomically dump the tests completed so far.

        A run that is killed (SIGKILL, host wipe, an aborting test type) must
        still leave the finished tests on disk — the real-time checkpointing
        that makes a multi-hour run resumable.
        """
        payload = {
            "meta": self.meta | {"date": time.strftime("%Y-%m-%d %H:%M %z")},
            "tests": self.tests,
        }
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        with contextlib.suppress(OSError):
            tmp.write_text(json.dumps(payload))
            os.replace(tmp, self.path)


def parse_args(argv: list | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        description='Soak benchmark runner (see docs/release.md, "Benchmarks")'
    )
    ap.add_argument(
        "--tools", default="molehill", help="comma list: molehill,frp,rathole,nps"
    )
    ap.add_argument(
        "--variants",
        default="mux",
        help="molehill variants: mux,noise,mux1,kcp4,noise-direct,mux-off",
    )
    ap.add_argument("--test", default="capacity", choices=sorted(TEST_TYPES))
    ap.add_argument(
        "--path",
        default="clean",
        choices=sorted(PATH_CLASSES),
        help="the stage class for the single-stage test types "
        "(rrul and soak use their own timelines)",
    )
    ap.add_argument(
        "--timeline", default="", help="stage:secs,... (default: the test type's own)"
    )
    ap.add_argument(
        "--secs",
        type=float,
        default=0,
        help="single-stage timeline of this length on --path",
    )
    ap.add_argument(
        "--streams-max",
        type=int,
        default=0,
        help="the configured maximum bulk load (default: the SOAK_STREAMS_MAX knob)",
    )
    ap.add_argument(
        "--batch",
        type=int,
        default=0,
        help="tools measured concurrently (default: the CPU "
        "budget from SOAK_CORES_PER_PAIR)",
    )
    ap.add_argument(
        "--ab", metavar="BIN_A,BIN_B", help="screen: the two builds to interleave"
    )
    ap.add_argument("--out", default="")
    args = ap.parse_args(argv)
    if args.test in ("screen", "reconnect") and not args.ab:
        ap.error(f"--ab BIN_A,BIN_B is required for the {args.test} test")
    if args.test not in ("screen", "reconnect") and args.ab:
        ap.error("--ab is only meaningful for the interleaved test types")
    if args.ab:
        args.ab = args.ab.split(",")
        if len(args.ab) != lib.AB_BUILDS:
            ap.error("--ab takes exactly two binaries: BIN_A,BIN_B")
    return args


def timeline_for(args: argparse.Namespace) -> list[Stage]:
    """The stage schedule: explicit, or the test type's own default."""
    if args.timeline:
        return [
            Stage(path=s.strip(), secs=float(d))
            for s, d in (p.split(":") for p in args.timeline.split(",") if p)
        ]
    if args.test == "soak":
        return SOAK_TIMELINE
    if args.test == "rrul":
        return DEFAULT_TIMELINE
    return [Stage(path=args.path, secs=args.secs or 60.0)]


# `lo`'s MTU is 65536 on every Linux host; the only thing that ever changes it
# is this harness's `mtu` path classes. That makes it safe to assert at startup
# rather than track: a run that was SIGKILLed between "shrink" and "restore"
# cannot clean up after itself, so the *next* run does it here.
LO_MTU_DEFAULT = 65536


def restore_stale_mtu(log) -> None:
    """Put `lo` back to 65536 if a previous run died holding it shrunk.

    A leftover 1280 poisons every later run on this host — including other
    tenants' — and would show up as a mysterious throughput loss rather than
    as a harness state. Cheap to check, so it is checked.
    """
    mtu = shaper_mtu()
    if mtu is None or mtu == LO_MTU_DEFAULT:
        return
    r = subprocess.run(
        ["ip", "link", "set", "dev", "lo", "mtu", str(LO_MTU_DEFAULT)],
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode == 0:
        log(f"restored lo mtu {mtu} -> {LO_MTU_DEFAULT} (a run died holding it)")
    else:
        raise RuntimeError(
            f"lo mtu is {mtu} and could not be restored: {r.stderr.strip()[:200]}"
        )


def shaper_mtu() -> int | None:
    """The interface MTU the run starts from (None when unreadable).

    Recorded so a stage that changes it is auditable against the value the
    host actually had, not against an assumption about the default.
    """
    try:
        return int(Path("/sys/class/net/lo/mtu").read_text().strip())
    except (OSError, ValueError):
        return None


def build_meta(
    args: argparse.Namespace, knobs: lib.Knobs, timeline: list, batch: int, nproc: int
) -> dict:
    """The run's method record: every knob that changes a number.

    A reader must be able to tell what was measured and against what, so the
    SLO the verdict used, the load fractions, the stage schedule and the
    instrumentation switches all travel with the results (§10).
    """
    return {
        "workload_version": WORKLOAD_VERSION,
        "slo": {"rtt_p99_ms": knobs.slo_rtt_p99_ms, "error_rate": knobs.slo_error_rate},
        "load_fractions": {
            "soak": knobs.soak_load_fraction,
            "cost": knobs.cost_operating_point,
            "rrul_stream_factor": knobs.rrul_stream_factor,
        },
        "path_classes": {name: asdict(pc) for name, pc in PATH_CLASSES.items()},
        # The MTU axis changes the whole interface, not one tool's class, and
        # the original value is what teardown restores — both belong in the
        # method record (§10).
        "mtu_restore_to": shaper_mtu(),
        "timeline": [asdict(t) for t in timeline],
        "batch": batch,
        "nproc": nproc,
        "cores_per_pair": knobs.cores_per_pair,
        "streams_max": args.streams_max or knobs.streams_max,
        "settle_s": knobs.settle_s,
        "interactive_ping_interval_ms": knobs.ping_interval_ms,
        "udp_ping_interval_ms": knobs.udp_interval_ms,
        "churn_connects_s": knobs.churn_connects_s,
        "wedge_silence_s": lib.WEDGE_SILENCE_S,
        "loss_window_s": lib.LOSS_WINDOW_S,
        # The opt-in molehill instrumentation the run inherited (empty for a
        # default run): an instrumented path is not the same path.
        "instrumentation": lib.diag_env(),
        # Provenance (§10): the run must correspond to a known revision of a
        # known binary. `revision` marks a dirty tree as such, because a
        # number produced by uncommitted code describes code that does not
        # exist anywhere else.
        "revision": lib.git_revision(),
        "molehill_bin": str(knobs.molehill_bin),
        "molehill_version": lib.tool_version(knobs),
        "hostname": socket.gethostname(),
        "kernel": subprocess.run(
            ["uname", "-r"], capture_output=True, text=True, check=False
        ).stdout.strip(),
    }


def run_one_tool(
    tool: Tool, cid: str, ctx: RunContext, binary: str, entry: dict
) -> dict:
    """Start one tool pair, run its test, and never let a failure lose data.

    A failing tool is data: the entry keeps whatever was measured before the
    failure (its series and completed stages) and gains a typed error.
    """
    try:
        tool.start(binary)
        return run_tool(tool, cid, ctx, entry)
    except Exception as e:  # noqa: BLE001 — a failed test is data, not an abort
        entry["error"] = f"{type(e).__name__}: {e}"
        log(f"    {tool.label} FAILED: {entry['error']}")
        return entry
    finally:
        tool.stop()


def measure_batch(ctx: RunContext, group: list, work: Path, results: Results) -> None:
    """Shape one batch's paths, measure every tool in it, checkpoint.

    The batch's shaper is torn down on the way out even when a tool raises:
    a leftover qdisc would shape the next batch's clean stages, which is the
    one thing the clean stages exist to rule out.
    """
    bands = [lib.tool_band(26000 + i * 100, 0) for i in range(len(group))]
    classes = [(f"1:{20 + i}", bands[i]) for i in range(len(group))]
    shaper = Shaper(classes, log=log)
    ctx.shaper = shaper
    shaper.build()
    try:
        log(f"== batch: {[f'{t} {v}'.strip() for t, v in group]}")
        for (tool_name, variant), (cid, band) in zip(group, classes):
            # The screen starts on build A and swaps per step; the other
            # test types start on the default binary.
            binary = ctx.args.ab[0] if ctx.args.ab and tool_name == "molehill" else ""
            tool = Tool(tool_name, variant, band, ctx.knobs, work)
            entry = run_one_tool(tool, cid, ctx, binary, new_entry(ctx.args))
            results.tests.append(
                entry
                | {
                    "tool": tool.label,
                    "variant": variant,
                    "version": tool.version(binary),
                    "coverage": tool.coverage,
                }
            )
            results.checkpoint()
    finally:
        shaper.teardown()


def new_entry(args: argparse.Namespace) -> dict:
    """An empty test record, filled in by the test type as it measures."""
    return {
        "test": args.test,
        "path": args.path,
        "series": [],
        "stages": [],
        "metrics": {},
    }


def main() -> None:
    args = parse_args()
    knobs = lib.Knobs.from_env()
    lib.acquire_lock()
    # The run's working artifacts (tool logs, iperf-raw evidence, probe
    # logs) are evidence for the session, not repository content — and tool
    # logs carry the config's key material. The default results path is NOT
    # the work dir: `soak-plot` and `soak-check` glob the script's own
    # directory, so a run whose output they cannot see is a run nobody can
    # read. Release runs still pass an explicit --out.
    # `SOAK_KEEP=1` keeps the working directory and prints its path: diagnosing
    # a cell (why did this arm carry nothing?) needs the tool logs, the
    # kcp-stats lines and the raw iperf3 output, and a run that deletes its own
    # evidence turns every such question into a re-run.
    keep_work = bool(os.environ.get("SOAK_KEEP"))
    if keep_work and os.environ.get("SOAK_WORK"):
        work = Path(os.environ["SOAK_WORK"])
        work.mkdir(parents=True, exist_ok=True)
    else:
        work = Path(tempfile.mkdtemp(prefix=lib.WORK_PREFIX))
    if keep_work:
        log(f"work dir kept: {work}")
    restore_stale_mtu(log)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(130))
    reaped = lib.sweep_stale(work)
    if reaped:
        log(f"reaped {reaped} stale process(es) from crashed runs")

    out = (
        Path(args.out) if args.out else Path(__file__).parent / "results-soak-dev.json"
    )
    with contextlib.suppress(OSError):
        out.parent.mkdir(parents=True, exist_ok=True)
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    variants = [v.strip() for v in args.variants.split(",") if v.strip()]
    timeline = timeline_for(args)
    nproc = os.cpu_count() or 1
    budget = max(1, int(nproc / knobs.cores_per_pair))
    batch = args.batch or min(knobs.max_batch, budget)
    slots = [(t, v) for t in tools for v in (variants if t == "molehill" else [""])]

    log(
        f"soak: test={args.test} path={args.path} slots={slots} "
        f"timeline={[(s.path, s.secs) for s in timeline]} batch={batch} "
        f"(nproc={nproc})"
    )
    if args.ab:
        log(f"      A/B: {args.ab[0]} vs {args.ab[1]}")
    results = Results(
        path=out, meta=build_meta(args, knobs, timeline, batch, nproc), tests=[]
    )
    ctx = RunContext(args=args, knobs=knobs, timeline=timeline, shaper=None)
    exit_code = 0
    try:
        for start in range(0, len(slots), batch):
            measure_batch(ctx, slots[start : start + batch], work, results)
    except KeyboardInterrupt:
        log("interrupted — completed tests are kept")
        exit_code = 130
    except Exception as e:  # noqa: BLE001 — record the failure, keep the tests
        log(f"run failed: {type(e).__name__}: {e}")
        exit_code = 1
    finally:
        lib.release_lock()
        results.checkpoint()
        log(f"soak complete: {len(results.tests)} test(s) -> {out}")
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
