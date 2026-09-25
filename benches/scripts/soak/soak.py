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
import itertools
import json
import math
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time

import lib

WORKLOAD_VERSION = 1

# The SLO is a method constant (versioned into the meta): the interactive
# stream's p99 must stay under it, with zero errors, for a load level to
# count as sustainable.
SLO_RTT_P99_MS = 50.0

# An interactive stream silent for longer than this is a wedge: recorded
# as a flat segment with its duration, never a bare `None`.
WEDGE_SILENCE_S = 5.0

# Path classes: the stage schedule's vocabulary. The args are applied to
# each tool's own netem class in place; `clean` is the unshaped control.
# Positional argument lists for `tc qdisc ... netem`: this iproute2 spells
# jitter as the second positional after `delay` and has no `jitter` keyword.
PATH_CLASSES = {
    "clean": [],
    "rtt100": ["delay", "100ms"],
    "loss1": ["delay", "10ms", "loss", "1%"],
    "loss5": ["delay", "100ms", "loss", "5%"],
    "rate100": ["rate", "100mbit", "delay", "20ms", "limit", "2000"],
    "rate20": ["rate", "20mbit", "delay", "40ms", "limit", "2000"],
    "jitter": ["delay", "20ms", "10ms"],
}

DEFAULT_TIMELINE = [
    ("clean", 150), ("rtt100", 120), ("loss1", 120), ("loss5", 120),
    ("rate100", 120), ("rate20", 120), ("jitter", 120), ("clean", 150),
]
SOAK_TIMELINE = [("clean", 180), ("loss1", 180), ("rtt100", 180),
                 ("loss5", 180), ("clean", 180)]


# --- series statistics ------------------------------------------------------
def pct(values: list, q: float):
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, max(0, math.ceil(q * len(s)) - 1))]


def series_stats(rows: list, metric: str) -> dict:
    vals = [r["v"] for r in rows
            if r.get("metric") == metric and isinstance(r.get("v"), (int, float))]
    if not vals:
        return {"n": 0}
    return {"n": len(vals), "mean": round(sum(vals) / len(vals), 3),
            "p50": round(pct(vals, 0.5), 3), "p99": round(pct(vals, 0.99), 3),
            "max": round(max(vals), 3), "min": round(min(vals), 3)}


def slope_per_min(rows: list, metric: str):
    """Least-squares slope in units/minute: the drift axis (handles, RSS,
    CPU). A leak is a slope, not a level."""
    pts = [(r["t"], r["v"]) for r in rows
           if r.get("metric") == metric and isinstance(r.get("v"), (int, float))]
    if len(pts) < 10:
        return None
    t0, t1 = pts[0][0], pts[-1][0]
    if t1 - t0 < 60:
        return None
    n = len(pts)
    mx = sum(p[0] for p in pts) / n
    my = sum(p[1] for p in pts) / n
    num = sum((p[0] - mx) * (p[1] - my) for p in pts)
    den = sum((p[0] - mx) ** 2 for p in pts)
    if not den:
        return None
    return round(num / den * 60.0, 4)


def worst_window(rows: list, metric: str, window_s: float = 1.0):
    """The worst `window_s` slice's mean: the stability axis. A stage whose
    worst second sits far above its mean is not a stable configuration."""
    pts = [(r["t"], r["v"]) for r in rows
           if r.get("metric") == metric and isinstance(r.get("v"), (int, float))]
    if len(pts) < 2:
        return None
    best = None
    for i, (t0, _) in enumerate(pts):
        acc, n = 0.0, 0
        for t1, v in pts[i:]:
            if t1 - t0 > window_s:
                break
            acc += v
            n += 1
        if n:
            mean = acc / n
            if best is None or mean > best["mean"]:
                best = {"mean": round(mean, 3), "n": n}
    return best


def flat_segments(rows: list, metric: str = "rtt_interactive_ms",
                  gap_s: float = WEDGE_SILENCE_S) -> list:
    """Gaps in the interactive series beyond the wedge threshold: the
    shape of a silent stage, recorded instead of a null."""
    pts = [(r["t"], r["v"]) for r in rows if r.get("metric") == metric]
    return [{"start": round(t0, 3), "end": round(t1, 3),
             "duration_s": round(t1 - t0, 1)}
            for (t0, _), (t1, _) in itertools.pairwise(pts)
            if t1 - t0 > gap_s]


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

    def __init__(self, classes: list, log=print):
        self.classes = classes  # [(classid, band)]
        self.log = log

    def _tc(self, *args) -> None:
        r = subprocess.run(["tc", *args], capture_output=True, text=True,
                           check=False)
        if r.returncode != 0:
            raise RuntimeError(f"tc {' '.join(args)}: {r.stderr.strip()[:400]}")

    def _ports(self, band: dict) -> list:
        # The data-plane ports only: the TOOL's control channel stays in
        # the unshaped default class. A capacity measurement that shapes
        # the control plane kills the tool's heartbeat (measured: 40 s
        # timeout on a 100 mbit cell) and the run becomes a wedge study
        # instead of a capacity study.
        return [band[k] for k in ("iperf_exposed", "echo_exposed",
                                  "udp_exposed", "kcp_bind", "iperf_backend",
                                  "echo_backend", "udp_backend")]

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
            self._tc("filter", "del", "dev", "lo", "parent", "1:", "prio",
                     prio)
        for port in self._ports(band):
            for key in ("dport", "sport"):
                self._tc("filter", "add", "dev", "lo", "parent", "1:",
                         "prio", prio, "protocol", "ip", "u32",
                         "match", "ip", key, str(port), "0xffff",
                         "flowid", flowid)

    def build(self) -> None:
        # delete any existing root first: `qdisc replace` cannot CHANGE a
        # root qdisc into a different kind (a leftover netem root fails
        # with "Change operation not supported"), so a survivor from an
        # earlier experiment would abort the whole run.
        with contextlib.suppress(Exception):
            self._tc("qdisc", "delete", "dev", "lo", "root")
        self._tc("qdisc", "add", "dev", "lo", "root", "handle", "1:",
                 "htb", "default", "999")
        for cid, band in self.classes:
            minor = cid.split(":")[1]
            self._tc("class", "replace", "dev", "lo", "parent", "1:",
                     "classid", cid, "htb", "rate", "10gbit")
            self._tc("qdisc", "replace", "dev", "lo", "parent", cid,
                     "handle", f"{minor}0:", "netem")
            # start unclassified: a clean stage creates no HTB path
            self._filters(cid, band, self.DEFAULT)
        self.log(f"    shaper: {len(self.classes)} tool class(es) on lo "
                 f"(clean stages stay in the default class)")

    def apply(self, cid: str, stage: str) -> None:
        band = next(b for c, b in self.classes if c == cid)
        args = PATH_CLASSES.get(stage, [])
        minor = cid.split(":")[1]
        if not args:
            # an unshaped stage: no HTB path, no netem tax
            self._filters(cid, band, self.DEFAULT)
            self.log(f"    {cid} path={stage} (unshaped, default class)")
            return
        self._tc("qdisc", "replace", "dev", "lo", "parent", cid,
                 "handle", f"{minor}0:", "netem", *args)
        self._filters(cid, band, cid)
        self.log(f"    {cid} path={stage} ({' '.join(args)})")

    def teardown(self) -> None:
        with contextlib.suppress(Exception):
            self._tc("qdisc", "delete", "dev", "lo", "root")


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
        self.coverage = {"tcp_bulk": True, "tcp_interactive": True,
                         "tcp_churn": True, "udp_session": True}

    def start(self, binary: str = "") -> None:
        k, p = self.knobs, self.band
        setup = {"molehill": lambda: lib.setup_molehill(
                      self.variant, k, p, self.procs, self.work, binary),
                 "frp": lambda: lib.setup_frp(k, p, self.procs, self.work),
                 "rathole": lambda: lib.setup_rathole(k, p, self.procs,
                                                      self.work),
                 "nps": lambda: lib.setup_nps(k, p, self.procs, self.work),
                 }[self.name]
        setup()
        for port in (p["iperf_exposed"], p["echo_exposed"]):
            if not lib.wait_port(port, 30):
                raise TimeoutError(
                    f"{self.label}: exposed port {port} not ready")

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
            k = lib.Knobs(molehill_bin=binary or self.knobs.molehill_bin,
                          peer_dir=self.knobs.peer_dir)
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

    def __init__(self, band: dict, knobs, out: list, work, log=print):
        self.band, self.knobs, self.out, self.work, self.log = (
            band, knobs, out, work, log)
        self.procs: list = []
        self.readers: list = []
        self.stop = threading.Event()
        self.errors = 0
        self.attempts = 0
        self.churn_errors = 0

    def log_path(self, mode: str) -> str:
        return str(Path(self.work) / f"probe-{mode}-{self.band['echo_exposed']}.log")

    def _spawn(self, mode: str, port: int, interval: float,
               backend_port: int = 0, rate: float = 0.0) -> None:
        log_path = self.log_path(mode)
        with open(log_path, "w") as errlog:
            proc = subprocess.Popen(
                [sys.executable, "-c", PROBE_SRC, mode, str(port),
                 str(interval), str(backend_port), str(rate)],
                stdout=subprocess.PIPE, stderr=errlog, text=True, bufsize=1)
        self.procs.append(proc)

        def reader() -> None:
            for line in proc.stdout:
                parts = line.strip().split("\t")
                if len(parts) != 3:
                    continue
                try:
                    v = float(parts[2])
                except ValueError:
                    continue
                if parts[1] == "rtt_interactive_error":
                    self.errors += 1
                elif parts[1] == "churn_error":
                    self.churn_errors += 1
                else:
                    self.attempts += 1
                self.out.append({"t": float(parts[0]), "metric": parts[1],
                                 "v": v})

        th = threading.Thread(target=reader, daemon=True)
        th.start()
        self.readers.append(th)

    def start(self) -> None:
        """Bind the echo backends the TOOL forwards to, then dial the tool.

        The probe process is therefore both ends of the interactive path
        (a fresh TCP connection per ping) and of the UDP session, with the
        tool's tunnel in between and the harness out of the measured path.
        """
        self._spawn("interactive", self.band["echo_exposed"],
                    self.knobs.ping_interval_ms / 1000.0,
                    backend_port=self.band["echo_backend"])
        self._spawn("udp", self.band["udp_exposed"],
                    self.knobs.udp_interval_ms / 1000.0,
                    backend_port=self.band["udp_backend"])
        self._spawn("churn", self.band["echo_exposed"],
                    1.0 / max(1, self.knobs.churn_connects_s),
                    backend_port=self.band["echo_backend"],
                    rate=float(self.knobs.churn_connects_s))

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
                self.out.append({"t": round(now, 3),
                                 "metric": f"{label}_{fn.__name__}",
                                 "v": v})
            self.stop.wait(0.5)

    @staticmethod
    def rss_kb(pid: int):
        try:
            with open(f"/proc/{pid}/statm") as fh:
                return int(fh.read().split()[1]) * (os.sysconf("SC_PAGE_SIZE")
                                                    // 1024)
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
                threading.Thread(target=self._sampler, args=(fn,), daemon=True))
        # CPU as a delta of ticks over the wall interval
        self.threads.append(threading.Thread(target=self._cpu_loop,
                                             daemon=True))
        for t in self.threads:
            t.start()

    def _cpu_loop(self) -> None:
        prev = {p: (self.cpu_ticks(p), time.monotonic())
                for p in self.pids_of() if p}
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
                    self.out.append({"t": round(time.time(), 3),
                                     "metric": f"{label}_cpu_pct",
                                     "v": round((cur - p0) / wall / 100.0, 1)})
                prev[pid] = (cur, now)

    def stop_and_join(self) -> None:
        self.stop.set()
        for t in self.threads:
            t.join(timeout=3)


# --- test types -------------------------------------------------------------
def stage_spine(tool: Tool, streams: int, secs: float, entry: dict,
                backends, log) -> dict:
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
    cmd = ["iperf3", "-c", "127.0.0.1", "-p", str(tool.band["iperf_exposed"]),
           "-t", str(int(secs)), "-O", "2", "-P", str(streams), "-i", "1",
           "--json-stream"]
    t_end = time.time() + secs
    outcome = {}
    for attempt in (0, 1):
        outcome = _spine_once(cmd, secs, entry, t_end)
        if outcome["intervals"]:
            return outcome
        if attempt == 0:
            with contextlib.suppress(Exception):
                backends.restart_iperf()
    # the stage's full duration elapses regardless: a dead spine must not
    # cut the probes' and samplers' window short
    while time.time() < t_end:
        time.sleep(min(1.0, max(0.1, t_end - time.time())))
    return outcome


def _spine_once(cmd: list, secs: float, entry: dict, t_end: float) -> dict:
    """One iperf3 client attempt for a stage's bulk load."""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True,
                            bufsize=1)
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
                    entry["series"].append({
                        "t": now, "metric": "throughput_bulk_gbps",
                        "v": round(s.get("bits_per_second", 0) / 1e9, 4)})
                    entry["series"].append({
                        "t": now, "metric": "bulk_retransmits",
                        "v": s.get("retransmits", 0)})
    finally:
        proc.kill()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)
    return {"intervals": intervals, "exit": proc.returncode}


def record_stage(entry: dict, mark: int, log) -> None:
    """Derive one stage's statistics from the series since `mark`."""
    st = series_stats(entry["series"][mark:], "rtt_interactive_ms")
    udp = series_stats(entry["series"][mark:], "rtt_udp_ms")
    churn = series_stats(entry["series"][mark:], "churn_setup_ms")
    entry["stages"][-1].update(
        {"rtt_p99": st.get("p99"), "rtt_mean": st.get("mean"),
         "rtt_max": st.get("max"), "rtt_n": st.get("n"),
         "udp_p99": udp.get("p99"), "udp_mean": udp.get("mean"),
         "churn_p99": churn.get("p99"), "churn_per_s": churn.get("n")})
    flats = flat_segments(entry["series"][mark:])
    if flats:
        entry["stages"][-1]["flat_segments"] = flats
    log(f"    interactive p99={st.get('p99')} mean={st.get('mean')} "
        f"n={st.get('n', 0)}; udp p99={udp.get('p99')}; "
        f"churn/s={churn.get('n', 0)} p99={churn.get('p99')}")


def run_capacity(tool: Tool, args, knobs, timeline, shaper: Shaper,
                 pingers: Pingers, backends, entry: dict, log) -> None:
    """Ramp the bulk load until the interactive stream breaks the SLO."""
    ceiling = args.streams_max or knobs.streams_max
    sustainable = 0
    for streams in range(1, ceiling + 1):
        mark = len(entry["series"])
        r = backends.iperf_burst(tool.band["iperf_exposed"], streams,
                                 knobs.settle_s, tag=f"{tool.label} cap",
                                 backend_port=tool.band["iperf_backend"])
        st = series_stats(entry["series"][mark:], "rtt_interactive_ms")
        p99 = st.get("p99")
        broken = (p99 is not None and p99 > SLO_RTT_P99_MS) or not r["ok"]
        point = {"streams": streams, "gbps": r.get("gbps_headline"),
                 "rtt_p99": p99, "rtt_mean": st.get("mean"),
                 "rtt_n": st.get("n"), "slo_broken": bool(broken),
                 "reason": (r.get("reason") if not r["ok"] else
                            (f"interactive p99 {p99} > {SLO_RTT_P99_MS}"
                             if broken else None))}
        entry["metrics"].setdefault("curve", []).append(point)
        log(f"    load {streams}: {r.get('gbps_headline', '-')} Gbit/s, "
            f"interactive p99={p99} (n={st.get('n', 0)}) -> "
            f"{'BROKEN' if broken else 'ok'}")
        if broken:
            break
        sustainable = streams
    entry["metrics"]["max_sustainable_streams"] = sustainable
    entry["metrics"]["headroom"] = round(1 - sustainable / ceiling, 4)


def run_rrul(tool: Tool, args, knobs, timeline, shaper: Shaper,
             pingers: Pingers, backends, entry: dict, log) -> None:
    """Saturate the path and watch the interactive stream's RTT over time.

    N = cpu count (the canonical saturation), the interactive stream's RTT
    distribution over time through the stage schedule — the
    queueing-under-load detector, with the return-to-clean stage as the
    recovery axis.
    """
    streams = max(1, knobs.rrul_stream_factor * (os.cpu_count() or 1))
    entry["metrics"]["bulk_streams"] = streams
    for stage, secs in timeline:
        run_one_stage(tool, cid=tool.cid, stage=stage, secs=secs,
                      shaper=shaper, streams=streams, entry=entry,
                      backends=backends, log=log)


def run_staged(tool: Tool, args, knobs, timeline, shaper: Shaper,
               pingers: Pingers, backends, entry: dict, log) -> None:
    """soak / cost: drive the timeline under a fixed load, sample every stage.

    `soak` carries half the configured max load (the drift/leak axis is
    measured under load); `cost` carries the same load and additionally
    derives CPU-seconds per carried Gbit at that operating point.
    """
    streams = max(1, (args.streams_max or knobs.streams_max) // 2)
    entry["metrics"]["bulk_streams"] = streams
    for stage, secs in timeline:
        mark = run_one_stage(tool, cid=tool.cid, stage=stage, secs=secs,
                             shaper=shaper, streams=streams, entry=entry,
                             backends=backends, log=log)
        if args.test == "cost":
            record_cost(entry, mark, streams, log)


def run_one_stage(tool: Tool, cid: str, stage: str, secs: float,
                  shaper: Shaper, streams: int, entry: dict, backends,
                  log) -> int:
    """Shape one stage, run its bulk spine, record the stage's stats."""
    shaper.apply(cid, stage)
    mark = len(entry["series"])
    entry["stages"].append({"stage": stage, "secs": secs,
                            "t_start": round(time.time(), 3)})
    log(f"  stage {stage} ({secs}s)")
    outcome = stage_spine(tool, streams, secs, entry, backends, log)
    if not outcome["intervals"]:
        entry["stages"][-1]["bulk_error"] = (
            f"spine produced no intervals (exit {outcome['exit']})")
        log(f"    bulk spine produced nothing (exit {outcome['exit']})")
    record_stage(entry, mark, log)
    return mark


def record_cost(entry: dict, mark: int, streams: int, log) -> None:
    """CPU-seconds per carried Gbit over one stage's burst window."""
    stage = entry["stages"][-1]
    bulk = series_stats(entry["series"][mark:], "throughput_bulk_gbps")
    carried = (bulk.get("mean") or 0) * stage["secs"]
    if carried <= 0 or not bulk.get("n"):
        stage["cost_error"] = "no carried bytes in this stage"
        return
    cpu = (series_stats(entry["series"][mark:], "server_cpu_pct").get("mean")
           or 0.0) + (series_stats(entry["series"][mark:],
                                   "client_cpu_pct").get("mean") or 0.0)
    cost = (cpu / 100.0 * stage["secs"]) / carried
    stage["bulk_streams"] = streams
    stage["cost_cpu_per_gbit"] = round(cost, 4)
    total = entry["metrics"].get("cost_cpu_per_gbit_sum", 0.0) + cost
    stages = entry["metrics"].get("cost_stages", 0) + 1
    entry["metrics"]["cost_cpu_per_gbit"] = round(total / stages, 4)
    entry["metrics"]["cost_cpu_per_gbit_sum"] = total
    entry["metrics"]["cost_stages"] = stages
    log(f"    cost: {cost:.4f} CPU-s per carried Gbit "
        f"({streams} streams, {bulk.get('mean')} Gbit/s)")


def run_screen(tool: Tool, args, knobs, timeline, shaper: Shaper,
               pingers: Pingers, backends, entry: dict, log) -> None:
    """Fast A/B: two builds, interleaved inside every step of one test.

    The pair is spawned in the same batch (same epoch) and the tool's
    processes are swapped between the two builds at every load step, so
    both sample the same machine state — sequential before/after runs are
    defeated by epoch drift, which is the whole reason this exists.
    """
    build_a, build_b = args.ab
    entry["metrics"]["builds"] = {
        "A": build_a, "B": build_b,
        "A_version": tool.version(build_a), "B_version": tool.version(build_b)}
    rounds = []
    for step in range(1, (args.streams_max or knobs.streams_max) + 1):
        pair = []
        for label, binary in (("A", build_a), ("B", build_b)):
            tool.restart(binary)
            mark = len(entry["series"])
            r = backends.iperf_burst(tool.band["iperf_exposed"], step,
                                     knobs.settle_s, tag=f"{tool.label} {label}",
                                     backend_port=tool.band["iperf_backend"])
            st = series_stats(entry["series"][mark:], "rtt_interactive_ms")
            pair.append({"build": label, "gbps": r.get("gbps_headline"),
                         "rtt_p99": st.get("p99"), "rtt_n": st.get("n"),
                         "rtt_mean": st.get("mean")})
        rounds.append({"streams": step, "pair": pair})
        log(f"    step {step}: " + " | ".join(
            f"{p['build']} {p['gbps']} Gbit/s p99={p['rtt_p99']}" for p in pair))
    entry["metrics"]["rounds"] = rounds


# --- one tool's pass --------------------------------------------------------
def run_tool(tool: Tool, cid: str, args, knobs, timeline, shaper: Shaper,
             log) -> dict:
    tool.cid = cid
    entry = {"test": args.test, "path": args.path, "series": [],
             "stages": [], "metrics": {}}
    out = entry["series"]
    samplers = Samplers(tool.pids_of, out)
    pingers = Pingers(tool.band, knobs, out, tool.work, log=log)
    backends = lib.Backends()
    try:
        backends.start(tool.band["iperf_backend"], tool.band["echo_backend"],
                       tool.band["udp_backend"], tool.work,
                       echo_in_probe=True)
    except Exception as e:
        entry["error"] = f"Backends: {e}"
        return entry
    samplers.start()
    pingers.start()
    try:
        runner = {"capacity": run_capacity, "rrul": run_rrul,
                  "soak": run_staged, "cost": run_staged,
                  "screen": run_screen}[args.test]
        runner(tool, args, knobs, timeline, shaper, pingers, backends,
               entry, log)
    finally:
        pingers.stop_and_join()
        samplers.stop_and_join()
        backends.stop()
    # derived metrics: stability and drift
    for metric, label in (("rtt_interactive_ms", "interactive_rtt"),
                          ("rtt_udp_ms", "udp_rtt"),
                          ("throughput_bulk_gbps", "bulk_throughput")):
        entry["metrics"][f"{label}_stats"] = series_stats(out, metric)
        w = worst_window(out, metric)
        if w:
            entry["metrics"][f"{label}_worst_1s"] = w
    entry["metrics"]["flat_segments"] = flat_segments(out)
    # the interactive error rate from the probe's own series: errors over
    # attempts, not over the other probes' samples
    it_ok = series_stats(out, "rtt_interactive_ms").get("n", 0)
    it_err = series_stats(out, "rtt_interactive_error").get("n", 0)
    entry["metrics"]["interactive_error_rate"] = round(
        it_err / max(1, it_ok + it_err), 5)
    entry["metrics"]["churn_error_rate"] = round(
        series_stats(out, "churn_error").get("n", 0)
        / max(1, series_stats(out, "churn_setup_ms").get("n", 0)
              + series_stats(out, "churn_error").get("n", 0)), 5)
    # The drift axis skips the first stage: the connection-setup pool
    # allocation ramps RSS/fds once at startup, and warm-up is not a leak.
    drift_from = (entry["stages"][1]["t_start"] if len(entry["stages"]) > 1
                  else out[0]["t"] if out else 0)
    for metric in ("server_rss_kb", "client_rss_kb", "server_fds",
                   "client_fds", "server_threads", "client_threads",
                   "server_cpu_pct", "client_cpu_pct"):
        slope = slope_per_min([r for r in out if r["t"] >= drift_from], metric)
        if slope is not None:
            entry["metrics"][f"{metric}_slope_per_min"] = slope
    entry["metrics"]["drift_from_t"] = round(drift_from, 3)
    return entry


def checkpoint(path: Path, meta: dict, tests: list, log) -> None:
    """Atomically dump the tests completed so far.

    A run that is killed (SIGKILL, host wipe, an aborting test type) must
    still leave the finished tests on disk — the retired matrix's
    real-time checkpointing, which is what made a 3-hour run resumable.
    """
    payload = {"meta": meta | {"date": time.strftime("%Y-%m-%d %H:%M %z")},
               "tests": tests}
    tmp = path.with_suffix(path.suffix + ".tmp")
    with contextlib.suppress(OSError):
        tmp.write_text(json.dumps(payload))
        os.replace(tmp, path)


def main() -> None:
    ap = argparse.ArgumentParser(description="soak benchmark runner")
    ap.add_argument("--tools", default="molehill",
                    help="comma list: molehill,frp,rathole,nps")
    ap.add_argument("--variants", default="mux",
                    help="molehill variants: mux,noise,mux1,kcp4,mux-off")
    ap.add_argument("--test", default="capacity",
                    choices=["capacity", "rrul", "soak", "cost", "screen"])
    ap.add_argument("--path", default="clean", choices=sorted(PATH_CLASSES))
    ap.add_argument("--timeline", default="",
                    help="stage:secs,... (default: the test type's own)")
    ap.add_argument("--secs", type=float, default=0,
                    help="single-stage timeline of this length on --path")
    ap.add_argument("--streams-max", type=int, default=0)
    ap.add_argument("--batch", type=int, default=0)
    ap.add_argument("--ab", metavar="BIN_A,BIN_B",
                    help="screen: the two builds to interleave")
    ap.add_argument("--fresh", action="store_true")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    if args.test == "screen" and not args.ab:
        ap.error("--ab BIN_A,BIN_B is required for the screen test")

    knobs = lib.Knobs.from_env()
    log = lambda *a: print(*a, flush=True)
    lib.acquire_lock()
    # The run's working artifacts (tool logs, iperf-raw evidence, probe
    # logs) live in a temp dir: they are evidence for the session, not
    # repository content — and tool logs carry the config's key material.
    # The `molehill-bench.` prefix keeps one lock/ledger namespace with the
    # sweep below, which reaps a previously SIGKILLed run's leaked arms.
    work = Path(tempfile.mkdtemp(prefix="molehill-bench."))
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(130))
    reaped = lib.sweep_stale(work)
    if reaped:
        log(f"reaped {reaped} stale process(es) from crashed runs")

    out = Path(args.out) if args.out else (work / "results-soak-dev.json")
    with contextlib.suppress(OSError):
        out.parent.mkdir(parents=True, exist_ok=True)
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    variants = [v.strip() for v in args.variants.split(",") if v.strip()]
    timeline = ([(s.strip(), float(d)) for s, d in
                 (p.split(":") for p in args.timeline.split(",") if p)]
                if args.timeline else
                SOAK_TIMELINE if args.test == "soak" else
                DEFAULT_TIMELINE if args.test == "rrul" else
                [(args.path, args.secs or 60)])
    nproc = os.cpu_count() or 1
    budget = max(1, int(nproc / knobs.cores_per_pair))
    batch = args.batch or min(knobs.max_batch, budget)
    slots = [(t, v) for t in tools for v in
             (variants if t == "molehill" else [""])]

    log(f"soak: test={args.test} path={args.path} slots={slots} "
        f"timeline={timeline} batch={batch} (nproc={nproc})")
    args.ab = [b for b in args.ab.split(",")] if args.ab else None

    meta = {
        "workload_version": WORKLOAD_VERSION,
        "slo": {"rtt_p99_ms": SLO_RTT_P99_MS},
        "path_classes": PATH_CLASSES,
        "timeline": [{"stage": s, "secs": d} for s, d in timeline],
        "batch": batch, "nproc": nproc,
        "cores_per_pair": knobs.cores_per_pair,
        "settle_s": knobs.settle_s,
        "interactive_ping_interval_ms": knobs.ping_interval_ms,
        "udp_ping_interval_ms": knobs.udp_interval_ms,
        "wedge_silence_s": WEDGE_SILENCE_S,
        "hostname": socket.gethostname(),
        "kernel": subprocess.run(["uname", "-r"], capture_output=True,
                                 text=True, check=False).stdout.strip(),
    }
    tests: list = []
    shaper = None
    exit_code = 0
    try:
        for start in range(0, len(slots), batch):
            group = slots[start:start + batch]
            bands = [lib.tool_band(26000 + i * 100, 0)
                   for i in range(len(group))]
            classes = [(f"1:{20 + i}", bands[i]) for i in range(len(group))]
            shaper = Shaper(classes, log=log)
            shaper.build()
            log(f"== batch: {[f'{t} {v}'.strip() for t, v in group]}")
            for (tool_name, variant), (cid, band) in zip(group, classes):
                binary = ""
                if args.ab and tool_name == "molehill":
                    binary = args.ab[0]  # the screen swaps builds per step
                t = Tool(tool_name, variant, band, knobs, work)
                entry = {"test": args.test, "path": args.path, "series": [],
                         "stages": [], "metrics": {}}
                try:
                    t.start(binary)
                    entry = run_tool(t, cid, args, knobs, timeline, shaper,
                                     log)
                except Exception as e:  # a failed test is data
                    # keep whatever this tool measured: the series and the
                    # stages it completed are the evidence for the failure,
                    # and discarding them would lose the run
                    entry["error"] = f"{type(e).__name__}: {e}"
                    log(f"    {t.label} FAILED: {entry['error']}")
                finally:
                    t.stop()
                tests.append(entry | {
                    "tool": t.label, "variant": variant,
                    "version": t.version(binary),
                    "coverage": t.coverage})
                checkpoint(out, meta, tests, log)
            shaper.teardown()
            shaper = None
    except KeyboardInterrupt:
        log("interrupted — completed tests are kept")
        exit_code = 130
    except Exception as e:
        log(f"run failed: {type(e).__name__}: {e}")
        exit_code = 1
    finally:
        if shaper is not None:
            shaper.teardown()
        lib.release_lock()
        out = Path(args.out) if args.out else (
            work / "results-soak-dev.json")
        checkpoint(out, meta, tests, log)
        log(f"soak complete: {len(tests)} test(s) -> {out}")
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
