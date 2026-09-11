#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Shared infrastructure for the benchmark matrix (schema v3).

Design principles (mapped to the test-engineering concepts):
- continue-on-error: every arm and every metric is individually guarded; a
  failure records an error/skipped entry and the matrix moves on
- real-time reporting: each completed arm is merged into the results file and
  the file is atomically rewritten (checkpoint) before the next arm starts
- resumability: `--merge` re-runs selected arms into an existing results file
- isolation: every arm gets fresh processes, its own port band, and per-cell
  backend instances
"""
import contextlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path

SCHEMA = 3


# --- stale-run pid ledger ----------------------------------------------------
def record_pid(work: Path, pid: int) -> None:
    """Append a pid to work/pids.json so a later run can reap a crashed run."""
    f = work / "pids.json"
    try:
        pids = json.loads(f.read_text()) if f.exists() else []
        pids.append(pid)
        f.write_text(json.dumps(pids))
    except (OSError, ValueError):
        pass


def reap_pids(pids: list) -> int:
    """SIGKILL leftover pids whose /proc cmdline still mentions a bench
    workdir. Two guards:
    - the cmdline check (rejects recycled pids);
    - a parent check: a process whose parent is a live `bench.py` belongs to
      a RUNNING run and is never reaped (concurrent runs used to kill each
      other's arms through this sweep; the global lock now also refuses
      concurrency, this is the second line of defense)."""
    probe = f"{tempfile.gettempdir()}/molehill-bench."
    killed = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as fh:
                cmd = fh.read().replace(b"\0", b" ")
        except OSError:
            continue
        if probe.encode() not in cmd:
            continue
        try:
            with open(f"/proc/{pid}/stat") as fh:
                stat = fh.read().split(") ", 1)[1]
            ppid = int(stat.split()[1])
            with open(f"/proc/{ppid}/cmdline", "rb") as fh:
                parent_cmd = fh.read()
        except (OSError, ValueError, IndexError):
            parent_cmd = b""  # orphaned (parent gone) -> reap
        if b"bench.py" in parent_cmd:
            continue  # a live run owns this process
        try:
            os.kill(pid, signal.SIGKILL)
            killed += 1
        except OSError:
            pass
    return killed


def sweep_stale(work: Path) -> int:
    """Reap pids recorded by previous bench runs (a crashed run leaks arms
    that would otherwise pollute the next run's measurements and ports).
    Live processes of a running run are never touched (see reap_pids)."""
    total = 0
    for f in Path(tempfile.gettempdir()).glob("molehill-bench.*/pids.json"):
        if f.parent == work:
            continue
        try:
            pids = json.loads(f.read_text())
        except (OSError, ValueError):
            continue
        total += reap_pids(pids if isinstance(pids, list) else [])
        f.unlink(missing_ok=True)
    return total


# --- single-run lock ----------------------------------------------------------
LOCK_PATH = Path(tempfile.gettempdir()) / "molehill-bench.lock"


def acquire_lock() -> None:
    """Refuse to start while another bench run is active. Concurrent runs
    use the same port bands and sweep_stale used to reap each other's live
    processes, so the second run must never proceed."""
    for _ in range(2):  # one retry after clearing a stale (crashed) lock
        try:
            fd = os.open(LOCK_PATH, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(fd, str(os.getpid()).encode())
            os.close(fd)
            return
        except FileExistsError:
            pid = 0
            with contextlib.suppress(OSError, ValueError):
                pid = int(LOCK_PATH.read_text().strip())
            if pid and Path(f"/proc/{pid}").exists():
                # verify it really is a bench run: a recycled pid must not
                # wedge the lock behind an unrelated process
                try:
                    cmdline = Path(f"/proc/{pid}/cmdline").read_bytes()
                    is_bench = b"bench.py" in cmdline
                except OSError:
                    is_bench = False
                if is_bench:
                    sys.exit(f"another bench run (pid {pid}) is active — "
                             "concurrent runs interfere with each other; "
                             "wait for it to finish")
            LOCK_PATH.unlink(missing_ok=True)  # stale lock from a crash
    sys.exit("could not acquire the bench lock")


def release_lock() -> None:
    LOCK_PATH.unlink(missing_ok=True)


# --- configuration -----------------------------------------------------------
@dataclass
class Knobs:
    molehill_bin: str
    peer_dir: str
    molehill_reps: int = 3
    peer_reps: int = 1
    molehill_secs: int = 8
    peer_secs: int = 5
    molehill_secs_weak: int = 10
    peer_secs_weak: int = 6
    hol_secs: float = 5.0
    hol_bulk_rate_tcp: float = 200.0
    hol_bulk_rate_udp: float = 50.0
    steady_ping_count: int = 100
    churn_secs: float = 3.0
    churn_concurrency: int = 16
    cooldown_load_factor: float = 0.7  # wait until loadavg < nproc * factor
    cooldown_max_wait_s: float = 30.0
    udp_capacity_count: int = 10000
    udp_capacity_pps: float = 20000.0
    scale_streams: int = 64  # below the yamux ceiling (count x 32)
    pool_size: int = 8
    allow_port_hi: int = 25999  # server-side allow_ports upper bound
    udp_count: int = 200
    udp_interval_ms: int = 20

    @classmethod
    def from_env(cls) -> "Knobs":
        def e(name, default):
            v = os.environ.get(name)
            return type(default)(v) if v else default
        return cls(
            molehill_bin=os.environ.get(
                "MOLEHILL_BIN",
                str(Path(__file__).parents[3] / "target/release/molehill")),
            peer_dir=os.environ.get(
                "PEER_DIR", str(Path.home() / "tmp" / "bench-peers")),
            molehill_reps=e("MOLEHILL_REPS", 3),
            peer_reps=e("PEER_REPS", 1),
            molehill_secs=e("MOLEHILL_SECS", 8),
            peer_secs=e("PEER_SECS", 5),
            molehill_secs_weak=e("MOLEHILL_SECS_WEAK", 10),
            peer_secs_weak=e("PEER_SECS_WEAK", 6),
            hol_secs=e("HOL_SECS", 5.0),
            hol_bulk_rate_tcp=e("HOL_BULK_RATE_TCP", 200.0),
            hol_bulk_rate_udp=e("HOL_BULK_RATE_UDP", 50.0),
            churn_secs=e("CHURN_SECS", 3.0),
            churn_concurrency=e("CHURN_CONCURRENCY", 16),
            cooldown_load_factor=e("COOLDOWN_LOAD_FACTOR", 0.7),
            cooldown_max_wait_s=e("COOLDOWN_MAX_WAIT_S", 30.0),
            pool_size=e("POOL_SIZE", 8),
            udp_count=e("UDP_COUNT", 200),
            udp_interval_ms=e("UDP_INTERVAL_MS", 20),
        )


@dataclass
class CellSpec:
    name: str
    loss: float = 0.0
    burst: float = 0.0
    rate: float = 0.0
    rtt: float = 0.0
    jitter: float = 0.0  # netem delay distribution width (ms)
    loss_model: str = ""  # what Netem actually applied (recorded in meta)

    @property
    def weak(self) -> bool:
        return self.loss or self.burst or self.rate or self.rtt or self.jitter


def parse_cell(cell: str) -> CellSpec:
    """Formats: "loss%/rtt", "loss%:burst%/rtt", "r<mbit>/<rtt>",
    "j<rtt>/<jitter>" (delay with a distribution width), "0/0".
    burst = the Gilbert-Elliot bad-state exit probability in % — mean burst
    length is 100/burst packets (see Netem.on)."""
    loss = burst = rate = rtt = jitter = 0.0
    if cell.startswith("j") and cell[1:2].isdigit():
        rtt = float(cell[1:].split("/", maxsplit=1)[0])
        jitter = float(cell.split("/")[1])
        name = f"jitter{rtt:g}_{jitter:g}"
    elif cell.startswith("r") and cell[1:2].isdigit():
        rate = float(cell[1:].split("/", maxsplit=1)[0])
        rtt = float(cell.split("/")[1])
        name = f"rate{rate:g}_rtt{rtt:g}"
    elif ":" in cell:
        lb, rtt_s = cell.split("/")
        loss, burst = (float(x) for x in lb.split(":"))
        rtt = float(rtt_s)
        name = f"loss{loss:g}b{burst:g}_rtt{rtt:g}"
    else:
        loss = float(cell.split("/", maxsplit=1)[0].rstrip("%"))
        rtt = float(cell.split("/")[1])
        if loss == 0 and rtt == 0:
            name = "loopback"
        elif loss == 0:
            name = f"rtt{rtt:g}"
        else:
            name = f"loss{loss:g}_rtt{rtt:g}"
    return CellSpec(name=name, loss=loss, burst=burst, rate=rate, rtt=rtt,
                    jitter=jitter)


def cell_sort_key(c: dict) -> tuple:
    """Canonical cell order for the results meta: the default --cells order
    (loopback, rtt10, rtt100, loss1, loss5, loss2b25, rate100, rate20,
    jitter), unknown cells last by name. Targeted re-runs merge into the
    meta in arbitrary order; sorting here keeps the charts and tables
    plotting cells in the same order on every run."""
    canonical = ("loopback", "rtt10", "rtt100", "loss1_rtt10",
                 "loss5_rtt100", "loss2b25_rtt10", "rate100_rtt20",
                 "rate20_rtt40", "jitter20_10")
    name = c.get("name", "")
    return (canonical.index(name) if name in canonical
            else len(canonical), name)


# --- netem -------------------------------------------------------------------
# netem rate-limit queue depth in packets (see Netem.on for the measurement
# behind the value). Recorded in the results meta as `netem_rate_limit`.
RATE_QUEUE_LIMIT = 2000


class Netem:
    def __init__(self):
        self.tc = shutil.which("tc") or ""
        self.ok = False
        self.active = False
        if self.tc and subprocess.run(["sudo", "-n", "true"], check=False,
                                         timeout=10).returncode == 0:
            probe = subprocess.run(
                ["sudo", self.tc, "qdisc", "replace", "dev", "lo", "root",
                 "netem", "loss", "0%", "delay", "0ms"],
                capture_output=True, check=False, timeout=10)
            self.ok = probe.returncode == 0
            if self.ok:
                # the probe left a no-op netem on lo — remove it
                self.active = True
                self.off()
        if not self.ok:
            print("NOTE: netem unavailable (no CAP_NET_ADMIN) -> rtt cells "
                  "run via userspace weakproxy; loss cells are skipped",
                  file=sys.stderr)

    def on(self, spec: CellSpec) -> bool:
        if not self.ok:
            return False
        args = ["sudo", self.tc, "qdisc", "replace", "dev", "lo", "root",
                "netem"]
        if spec.loss:
            if spec.burst:
                # Gilbert-Elliot burst model per tc-netem(8) LOSS grammar.
                # The plain correlated form 'loss X% Y%' silently drops
                # nothing at low rates on this kernel (measured 0/1500 at
                # 2%+25%), so the documented burst model is used: average
                # loss = spec.loss, mean burst length = 100/burst packets.
                p = spec.loss * spec.burst / (100 - spec.loss)
                args += ["loss", "gemodel", f"{p:.3g}%",
                         f"{spec.burst:g}%", "100%", "0%"]
                spec.loss_model = (f"gemodel {p:.3g}% {spec.burst:g}% "
                                   "100% 0%")
            else:
                args += ["loss", f"{spec.loss:g}%"]
                spec.loss_model = f"random {spec.loss:g}%"
        if spec.rtt or spec.jitter:
            args += ["delay", f"{spec.rtt:g}ms", f"{spec.jitter:g}ms"]
        if spec.rate:
            # Queue depth is a MEASUREMENT parameter, not a detail: netem's
            # token bucket needs a queue deep enough to absorb the sender's
            # bursts, otherwise the shaper tail-drops whole GSO super-segments
            # and even a healthy TCP sender collapses. Measured on this host
            # at `rate 100mbit delay 20ms` with a plain iperf3 pair (-P 8):
            #   limit 1    -> 18 Mbit/s   (81% of the shaped rate lost)
            #   limit 1000 -> 99.6 Mbit/s (honest)
            #   limit 20000-> 100 Mbit/s  (deep buffer, longer drain)
            # The old hardcoded `limit 1000` was recorded nowhere, so a
            # reader could not tell whether a weak rate cell was the tool or
            # the shaper. `limit 2000` keeps the burst tolerance of 1000 with
            # headroom for 8-stream tests; it is reported in the results meta
            # (`netem_rate_limit`) so the shaping model is auditable.
            args += ["rate", f"{spec.rate:g}mbit", "limit",
                     str(RATE_QUEUE_LIMIT)]
        applied = subprocess.run(args, capture_output=True,
                                 check=False, timeout=10).returncode == 0
        self.active = applied
        return applied

    def off(self) -> None:
        if self.tc and self.active:
            subprocess.run(["sudo", self.tc, "qdisc", "del", "dev", "lo",
                            "root"], capture_output=True, check=False,
                           timeout=10)
            self.active = False


# --- local backends ----------------------------------------------------------
class Backends:
    """iperf3 (TCP, external binary) + in-process TCP/UDP echo servers.

    start() raises RuntimeError when a backend cannot come up (e.g. a port
    squatted by a leaked process) — the matrix records that cell's arms as
    errors instead of aborting the whole run.
    """

    def __init__(self):
        self._stop = threading.Event()
        self._threads = []
        self._procs = []
        self._socks = []
        self._work = None
        self.iperf_port = 0
        self.tcp_port = 0
        self.udp_port = 0

    def start(self, iperf_port: int, tcp_port: int, udp_port: int,
              work: Path | None = None):
        self.iperf_port, self.tcp_port, self.udp_port = (
            iperf_port, tcp_port, udp_port)
        self._work = work
        # A previous arm's wedged server can hold the port briefly (or, if
        # it survived a kill, indefinitely): kill whatever listens on our
        # ports and retry a few times instead of failing the whole arm.
        last = None
        for attempt in range(5):
            try:
                self._start_once(iperf_port, tcp_port, udp_port, work)
                return
            except Exception as e:
                last = e
                if "Address already in use" not in str(e) \
                        and "died on startup" not in str(e):
                    break  # not a port conflict; retrying cannot help
                self._kill_port_holder(iperf_port)
                self._kill_port_holder(tcp_port)
                self._kill_port_holder(udp_port)
                time.sleep(1.0)
        raise RuntimeError(f"backends failed on ports "
                           f"{iperf_port}/{tcp_port}/{udp_port}: {last}") \
            from last

    @staticmethod
    def _kill_port_holder(port: int) -> None:
        """SIGKILL processes listening on `port` (ours: bench backend ports)."""
        try:
            out = subprocess.run(
                ["ss", "-ltnp", f"sport = :{port}"], capture_output=True,
                text=True, timeout=5, check=False).stdout
        except Exception:
            return
        for pid in re.findall(r"pid=(\d+)", out):
            with contextlib.suppress(OSError, ValueError):
                os.kill(int(pid), signal.SIGKILL)

    def _start_once(self, iperf_port: int, tcp_port: int, udp_port: int,
                    work: Path | None):
        try:
            # --logfile puts the (temp) workdir into the cmdline so a leaked
            # iperf3 from a crashed run is reaped by sweep_stale like the
            # other arm processes (its port would silently break the next
            # run's throughput arms)
            cmd = ["iperf3", "-s", "-B", "127.0.0.1", "-p", str(iperf_port)]
            if work is not None:
                cmd += ["--logfile", str(Path(work) / "iperf3.log")]
            proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL)
            self._procs.append(proc)
            if work is not None:
                record_pid(work, proc.pid)
            time.sleep(0.3)
            if proc.poll() is not None:
                raise RuntimeError(
                    f"iperf3 died on startup (port {iperf_port}, exit "
                    f"{proc.returncode}) — is another iperf3 or a leaked "
                    "bench process holding the port?")
            self._spawn_tcp(tcp_port)
            self._spawn_udp(udp_port)
            time.sleep(0.4)
        except Exception as e:
            for p in self._procs:
                with contextlib.suppress(OSError):
                    p.terminate()
            self._procs.clear()
            for s in self._socks:  # a partial bind must not leak into retries
                with contextlib.suppress(OSError):
                    s.close()
            self._socks.clear()
            raise RuntimeError(f"backends failed on ports "
                               f"{iperf_port}/{tcp_port}/{udp_port}: {e}") \
                from e

    def _spawn_tcp(self, port: int) -> None:
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            srv.bind(("127.0.0.1", port))
        except OSError as e:
            srv.close()
            raise RuntimeError(f"bind tcp backend {port}: {e}") from e
        srv.listen(512)
        self._socks.append(srv)

        def serve(conn):
            try:
                while True:
                    d = conn.recv(65536)
                    if not d:
                        break
                    conn.sendall(d)
            except OSError:
                pass
            finally:
                conn.close()

        def acceptor():
            srv.settimeout(0.5)
            while not self._stop.is_set():
                try:
                    conn, _ = srv.accept()
                    threading.Thread(target=serve, args=(conn,),
                                     daemon=True).start()
                except TimeoutError:
                    continue
                except OSError:
                    break  # socket closed by stop()

        t = threading.Thread(target=acceptor, daemon=True)
        t.start()
        self._threads.append(t)

    def _spawn_udp(self, port: int) -> None:
        srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            srv.bind(("127.0.0.1", port))
        except OSError as e:
            srv.close()
            raise RuntimeError(f"bind udp backend {port}: {e}") from e
        srv.settimeout(0.5)
        self._socks.append(srv)

        def serve():
            while not self._stop.is_set():
                try:
                    data, addr = srv.recvfrom(65535)
                    srv.sendto(data, addr)
                except TimeoutError:
                    continue
                except OSError:
                    break  # socket closed by stop()

        t = threading.Thread(target=serve, daemon=True)
        t.start()
        self._threads.append(t)

    def restart_iperf(self) -> None:
        """Spawn a FRESH iperf3 server on the same port.

        A stalled throughput test at a shaped (netem rate-limited) cell
        poisons the single-test iperf3 server's state ("unable to receive
        cookie" / "Bad file descriptor"): every later test on that server
        then hangs until the harness timeout, which is why the 8-stream
        value at the rate cells kept coming back None. A fresh process has
        clean state; the TCP/UDP echo servers are unaffected. Called by
        `run_throughput` after every failed rep; the old wedged process is
        killed and the new one re-recorded for crash reaping.
        """
        if not self._procs:
            raise RuntimeError("restart_iperf: no backend server running")
        proc = self._procs.pop(0)  # index 0 is always the iperf3 server
        with contextlib.suppress(OSError):
            proc.kill()
        time.sleep(0.3)
        # a wedged server can survive SIGKILL briefly; make sure the port
        # is really free before rebinding it
        self._kill_port_holder(self.iperf_port)
        cmd = ["iperf3", "-s", "-B", "127.0.0.1", "-p", str(self.iperf_port)]
        if self._work is not None:
            cmd += ["--logfile", str(Path(self._work) / "iperf3.log")]
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)
        self._procs.insert(0, proc)
        if self._work is not None:
            record_pid(self._work, proc.pid)
        time.sleep(0.4)
        if proc.poll() is not None:
            raise RuntimeError(
                f"iperf3 died on restart (port {self.iperf_port}, exit "
                f"{proc.returncode})")

    def run_throughput(self, exposed_port: int, reps: int, streams: int,
                       secs: int, tag: str = "", backend_port: int | None = None) -> dict:
        """Sample a `-P streams` throughput N times through one arm, isolated.

        Each rep gets a BOUNDED client (the bound scales with the test length,
        not with a fixed 20 s that a shaped-cell drain legitimately exceeds) and
        a fresh server when the previous rep wedged it. Returns the full
        measurement, not only a rate: per-rep records plus the median-rep
        headline and min/max spread, so `results-*.json` can answer *why* a
        number is missing or zero after the run (the shape of the failure is
        data: a 0-byte sum and a timeout are different findings).

        Why per-rep server hygiene matters: iperf3's server is single-test, and
        a stalled test leaves it in a state ("unable to receive cookie" / EBADF)
        where every FOLLOWING test hangs until the harness bound. With the
        harness bound at `secs + 20` and 3 reps, one wedge therefore turned a
        measurable 8-stream cell into `null` and killed the rest of the
        repetition budget — the observed rate-cell behaviour in v0.8.0.
        """
        # The client MUST dial the tool's EXPOSED port: `self.iperf_port` is
        # the iperf3 BACKEND (what the tool forwards to), so dialing it
        # measures the loopback iperf3 ceiling with the tool bypassed — the
        # pre-schema-v3 error. The assert below makes that impossible to
        # reintroduce silently; the entry also records both ports.
        if backend_port is not None and exposed_port == backend_port:
            raise RuntimeError(
                f"throughput endpoint {exposed_port} is the iperf3 backend, "
                "not the tunnel's exposed port — the tool would be bypassed")
        port = exposed_port
        # Expected wall time of one rep: `-O 2` warm-up + the measured window,
        # plus slack for slow-start and the results exchange. The margin is
        # generous because killing a still-transferring test turns real data
        # into a `null`; the whole-arm timeout in bench.py stays the hard stop.
        # Bound the client by the test length, but never tighter than the
        # historical `secs + 20`: measured through the tunnel (not the
        # backend), the weakest cells need that slack to finish the results
        # exchange, and the first version of this bound (secs*2+6 = 26 s for
        # a 10 s test) turned KCP's rtt100 samples into nulls.
        timeout = max(secs * 2.0 + 6.0, secs + 20.0)
        records = []
        for i in range(reps):
            art = None
            if self._work is not None:
                # per-ARM subdirectory: the work dir is per RUN, so without
                # the tag every arm overwrote the previous arm's raw
                # artifacts and only the last arm's evidence survived
                safe = re.sub(r"[^A-Za-z0-9._-]+", "_", tag) if tag else "arm"
                art = (Path(self._work) / "iperf-raw" / safe
                       / f"P{streams}-rep{i}")
            r = iperf_result(port, streams, secs, timeout, art)
            r["rep"] = i
            records.append(r)
            outcome = "ok" if r["ok"] else "fail"  # NOT `tag`: that is the
            # artifact-directory name and must survive the loop
            print(f"    rep{i} P{streams}: {outcome} "
                  f"{r.get('gbps_headline', '-')} Gbit/s, "
                  f"{r.get('gbps_received_own_window', '-')} recv-own, "
                  f"wall {r['wall_s']}s"
                  f"{'' if r['ok'] else ' — ' + str(r.get('reason'))[:90]}",
                  flush=True)
            if not r["ok"]:
                # the NEXT rep is what a wedged server would poison
                with contextlib.suppress(Exception):
                    self.restart_iperf()
        ok = [r for r in records if r["ok"]]
        out = {"reps_run": len(records), "reps_ok": len(ok), "records": records,
               "port": port}
        if not ok:
            last = records[-1]
            out["error"] = last.get("reason", "no result")
            return out
        by_rate = sorted(ok, key=lambda r: r["gbps_headline"])
        med = by_rate[len(by_rate) // 2]
        out |= {
            # headline = the delivered/ingress bytes over the measured window
            # (see iperf_result): the side whose accounting is complete
            "gbps_sent": round(med["gbps_headline"], 4),
            "gbps_sent_only": round(med["gbps_sent_only"], 4),
            "gbps_received": round(med["gbps_received_window"], 4),
            "gbps_received_own_window": round(med["gbps_received_own_window"], 4),
            "gbps_min": round(by_rate[0]["gbps_headline"], 4),
            "gbps_max": round(by_rate[-1]["gbps_headline"], 4),
            "degenerate_reps": sum(
                1 for r in ok if r.get("sender_accounting_degenerate")),
            "retransmits": med["retransmits"],
            "bytes_sent": med["bytes_sent"],
            "bytes_received": med["bytes_received"],
            "active_s": med["active_s"],
            "receiver_window_s": med["receiver_window_s"],
            "per_stream_bytes": med["per_stream_bytes"],
            "mean_rtt_us": med.get("mean_rtt_us"),
            "median_rep": med["rep"],
        }
        return out

    def stop(self) -> None:
        self._stop.set()
        for s in self._socks:
            with contextlib.suppress(OSError):
                s.close()
        for p in self._procs:
            with contextlib.suppress(OSError):
                # SIGKILL, not terminate: a wedged iperf3 server (stuck in a
                # stalled test) can survive SIGTERM and hold its port, which
                # would poison the next arm's backends with EADDRINUSE
                os.kill(p.pid, signal.SIGKILL)
        self._procs.clear()
        self._socks.clear()
        self._threads.clear()
        self._stop = threading.Event()


# --- load-aware cooldown ----------------------------------------------------
def wait_load_quiet(factor: float, max_wait_s: float) -> None:
    """Wait until the 1-minute load average is below `factor x nproc` (each
    arm then starts from a quiet machine: the measurement is cleaner, and a
    long matrix can never stack saturation into a system freeze). Gives up
    after `max_wait_s` and proceeds anyway; no-ops where /proc/loadavg is
    unavailable."""
    try:
        nproc = os.cpu_count() or 1
        target = nproc * factor
        deadline = time.time() + max_wait_s
        waited = 0.0
        while time.time() < deadline:
            with open("/proc/loadavg") as fh:
                load = float(fh.read().split()[0])
            if load < target:
                break
            time.sleep(1.0)
            waited += 1.0
        if waited > 1.0:
            print(f"cooldown: waited {waited:.0f}s for load {load:.1f} "
                  f"to drop below {target:.1f}", file=sys.stderr)
    except (OSError, ValueError, IndexError):
        pass


# --- wait for a TCP port -----------------------------------------------------
def wait_port(port: int, timeout_s: float = 25.0) -> bool:
    end = time.time() + timeout_s
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.15)
    return False


# --- metrics -----------------------------------------------------------------
def iperf_active_seconds(doc: dict, secs: int) -> float:
    """Length of the MEASURED window = the sender's non-omitted intervals.

    This is the honest denominator for the sender-side rate: total bytes sent
    include the `-O` warm-up, so `bytes / sum_sent.seconds` (the full test
    duration) is not the measured-window rate. The receiver keeps a SEPARATE
    window (`sum_received.seconds`), reported alongside: after the sender
    stops, the backend can still be draining (6.24 s of receive window for a
    6.00 s send window on a shaped cell), and collapsing the two into one
    denominator is what made `sent` and `received` look incomparable.
    Falls back to the configured duration when the interval list is absent."""
    intervals = doc.get("intervals") or []
    acc = 0.0
    for iv in intervals:
        s = iv.get("sum") or {}
        if s.get("omitted"):
            continue
        # ALWAYS the interval's own span. iperf3's per-interval `seconds`
        # field is not that span: on the interval that follows the `-O`
        # warm-up it reports the warm-up plus the interval (2.005 s for a
        # 0.005 s tail), which summed to 9.0 s for an 8 s test and silently
        # deflated the headline by ~12% on the loopback cell.
        acc += max(0.0, s.get("end", 0.0) - s.get("start", 0.0))
    return acc if acc > 0 else float(secs)


def iperf_result(port: int, streams: int, secs: int, timeout: float,
                 artifact: Path | None) -> dict:
    """One `-P streams` iperf3 client run through the tunnel; never raises.

    A raw artifact directory is captured on EVERY outcome (requested and
    actual timeout, exit status, stdout, stderr) so a later null in the
    results file can be re-diagnosed instead of guessed at."""
    if artifact is not None:
        artifact.mkdir(parents=True, exist_ok=True)
    cmd = ["iperf3", "-J", "-c", "127.0.0.1", "-p", str(port),
           "-t", str(secs), "-O", "2", "-P", str(streams)]
    t0 = time.perf_counter()
    timed_out = False
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout, check=False)
        rc, out, err = proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as e:
        timed_out = True
        rc, err = None, (e.stderr or "")
        out = e.stdout if isinstance(e.stdout, str) else (e.stdout or b"").decode(
            "utf-8", "replace")
    wall = round(time.perf_counter() - t0, 2)
    if artifact is not None:
        (artifact / "cmd.txt").write_text(" ".join(cmd) + "\n")
        (artifact / "stdout.json").write_text(out or "")
        (artifact / "stderr.txt").write_text(err or "")
        (artifact / "meta.json").write_text(json.dumps(
            {"streams": streams, "secs": secs, "timeout_s": timeout,
             "exit": rc, "timed_out": timed_out, "wall_s": wall}, indent=1))
    base = {"streams": streams, "secs": secs, "timeout_s": round(timeout, 1),
            "wall_s": wall, "timed_out": timed_out, "exit": rc,
            "artifact": str(artifact / "stdout.json") if artifact else None}
    doc = None
    with contextlib.suppress(ValueError):
        doc = json.loads(out) if out else None
    if doc and doc.get("error"):
        return base | {"ok": False, "reason": f"iperf3 error: {doc['error']}"}
    if timed_out:
        return base | {"ok": False,
                       "reason": f"client timed out after {timeout:.0f}s "
                                 "(harness bound, test may still have been "
                                 "transferring)"}
    if rc != 0:
        detail = (err or out or "").strip().replace("\n", " ")[:200]
        return base | {"ok": False,
                       "reason": f"iperf3 exit {rc}: {detail or 'no output'}"}
    if doc is None or "end" not in doc:
        return base | {"ok": False, "reason": "unparseable iperf3 output"}
    end = doc["end"]
    sent = end.get("sum_sent") or {}
    recv = end.get("sum_received") or {}
    active_s = iperf_active_seconds(doc, secs)
    sent_b = sent.get("bytes", 0)
    recv_b = recv.get("bytes", 0)
    # iperf3 3.18 nests each connection's own counters under
    # end.streams[i].sender (a stream entry's top level is empty for a
    # sender-side run): reading the top level yields all-zero per-stream byte
    # counts, which is how a perfectly healthy run can still be summarised as
    # "8 zero streams"
    streams = [(s.get("sender") or {}) for s in end.get("streams", [])]
    # `sum_sent` covers only the post-omit window, so its byte count is the
    # right numerator for the measured window — EXCEPT when the sender's
    # writes all completed inside the `-O` warm-up and backpressure then
    # blocked it for the whole measured window. That is exactly what a fast
    # sender into a slow shaper does: measured at rate20_rtt40, the warm-up
    # interval carried 153 MB at 1.22 Gbit/s and every measured interval
    # showed 0 bytes sent, while the receiver still logged 29 MB (the shaped
    # link rate). The receiver's count is then the only evidence of what the
    # path carried; both sides are recorded either way.
    sent_gbps = sent_b * 8 / active_s / 1e9 if active_s else 0.0
    recv_gbps = recv_b * 8 / active_s / 1e9 if active_s else 0.0
    degenerate = recv_b > 0 and sent_b * 2 < recv_b
    # Headline policy: the sender's post-omit bytes over the measured window
    # is the clean, consistent definition (ingress rate). max(sent, recv)
    # would inflate: the receiver's total can include warm-up backlog still
    # draining inside the window (64-stream loopback measured 59.1 Gbit/s
    # received vs 45.9 sent). Only when the sender's accounting is provably
    # defeated does the receiver's count become the evidence, and then it is
    # the ONLY evidence of what the path carried.
    headline_gbps = recv_gbps if degenerate else sent_gbps
    return base | {
        "ok": True,
        "bytes_sent": sent_b,
        "bytes_received": recv_b,
        "active_s": round(active_s, 3),
        "gbps_sent_only": round(sent_gbps, 4),
        "gbps_received_window": round(recv_gbps, 4),
        "gbps_headline": round(headline_gbps, 4),
        "sender_accounting_degenerate": degenerate,
        "gbps_received_own_window": round(
            recv.get("bits_per_second", 0.0) / 1e9, 4),
        "receiver_window_s": round(recv.get("seconds", 0.0), 3),
        "retransmits": sent.get("retransmits", 0),
        "per_stream_bytes": [s.get("bytes", 0) for s in streams],
        "per_stream_gbps": [round(s.get("bits_per_second", 0.0) / 1e9, 4)
                            for s in streams],
        "mean_rtt_us": streams[0].get("mean_rtt") if streams else None,
    }



def latency(exposed_port: int, samples: int = 300,
             max_wall_s: float = 20.0) -> dict:
    """Connection-path RTT: connect + 1-byte ping over fresh TCP connections.
    Every socket is bounded, five consecutive failures abort the probe, and
    the whole probe stops after `max_wall_s` — on a 100 ms cell each sample
    costs ~1 s, so a fixed sample count would stall the arm for minutes;
    ~20 samples on a fixed-delay path carry the same p50/p99."""
    xs = []

    def once() -> float | None:
        t0 = time.perf_counter()
        s = socket.socket()
        s.settimeout(3.0)
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        try:
            s.connect(("127.0.0.1", exposed_port))
            s.sendall(b"p")
            s.recv(1)
            return (time.perf_counter() - t0) * 1000.0
        except (OSError, TimeoutError):
            return None
        finally:
            s.close()

    failed = 0
    for _ in range(30):
        if once() is None:
            failed += 1
            if failed >= 5:
                raise RuntimeError("latency probe: path not answering")
    failed = 0
    deadline = time.time() + max_wall_s
    while len(xs) < samples and time.time() < deadline:
        v = once()
        if v is None:
            failed += 1
            if failed >= 5:
                raise RuntimeError("latency probe: path not answering")
            continue
        xs.append(v)
    if len(xs) < 10:
        raise RuntimeError("latency probe: too few samples")
    xs.sort()
    return {
        "p50": round(xs[len(xs) // 2], 3),
        "p95": round(xs[int(len(xs) * 0.95)], 3),
        "p99": round(xs[int(len(xs) * 0.99) - 1], 3),
        "mean": round(sum(xs) / len(xs), 3),
    }


def churn(exposed_port: int, secs: float, concurrency: int) -> dict:
    """Short-connection storm: `concurrency` parallel workers each loop
    connect -> 1-byte ping -> close as fast as they can for `secs` seconds.
    The real-world "connection churn" workload (HTTP, game lobbies): the
    mux-vs-direct and pool_size guidance data — per-connection setup cost
    and its success rate under weak cells."""
    stop = threading.Event()
    total, ok = 0, 0
    lats = []
    lock = threading.Lock()

    def worker():
        nonlocal total, ok
        while not stop.is_set():
            t0 = time.perf_counter()
            try:
                # Bounded: a wedged tunnel must fail the connection, not
                # hang the worker (and with it the whole arm) forever.
                s = socket.socket()
                s.settimeout(2.0)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                s.connect(("127.0.0.1", exposed_port))
                s.sendall(b"p")
                s.recv(1)
                dt = (time.perf_counter() - t0) * 1000.0
                s.close()
                with lock:
                    total += 1
                    ok += 1
                    lats.append(dt)
            except (OSError, TimeoutError):
                with lock:
                    total += 1

    threads = [threading.Thread(target=worker, daemon=True)
               for _ in range(concurrency)]
    for t in threads:
        t.start()
    stop.wait(secs)
    stop.set()
    for t in threads:
        t.join()
    if total == 0 or ok == 0:
        # nothing attempted, or every connection failed (e.g. a rate-limited
        # cell where the 2 s socket timeout beats the tunnel): percentiles
        # of an empty sample set are meaningless, report zeros instead of
        # crashing on xs[-1]
        return {"connects": total, "success_pct": round(100.0 * ok / total, 2)
                if total else 0.0,
                "connects_per_s": round(total / secs, 1),
                "setup_first_byte_ms_p50": 0.0, "setup_first_byte_ms_p95": 0.0,
                "setup_first_byte_ms_p99": 0.0, "setup_first_byte_ms_mean": 0.0}
    xs = sorted(lats)
    return {
        "connects": total,
        "success_pct": round(100.0 * ok / total, 2),
        "connects_per_s": round(total / secs, 1),
        "setup_first_byte_ms_p50": round(xs[len(xs) // 2], 3),
        "setup_first_byte_ms_p95": round(xs[int(len(xs) * 0.95)], 3),
        "setup_first_byte_ms_p99": round(xs[int(len(xs) * 0.99) - 1], 3),
        "setup_first_byte_ms_mean": round(sum(xs) / len(xs), 3),
    }


def udp_capacity_probe(exposed_port: int, count: int, pps: float,
                       rate_mbit: float = 0.0) -> dict:
    """Two-point UDP characterisation, because ONE burst cannot describe a
    capacity curve.

    The single 20k-pps burst this replaces read `loss_pct: 100.0` on every
    shaped cell — the KCP/UDP forwarder's knee sits near 5k pps on this host
    (measured: 2k/5k -> 0% loss, 10k -> 30% at ~9.5 Mbit/s delivered,
    20k -> 100%), so offering double the knee saturated it and the loss
    percentage said nothing about capacity. Reported instead:
      - `paced`: a fraction of the bursting rate, inside the sustainable
        regime — "does the path deliver when not overloaded", with loss.
      - `saturated`: the configured rate — "what the path actually carries
        under overload", judged by DELIVERED Mbit/s, not by loss.
    """
    # The paced point sits under the shaped link budget (60%), not at a fixed
    # fraction of the burst rate: on a 100 Mbit/s cell that is ~4.4k pps —
    # inside the sustainable regime measured above — while `pps` stays the
    # overload point.
    paced_pps = pps / 5.0
    if rate_mbit:
        paced_pps = min(paced_pps,
                        max(50.0, rate_mbit * 1e6 * 0.6 / ((128 + 28 + 14) * 8)))
    # The UDP flow is created lazily by the first datagram (the server dials
    # the backend, the client registers the session): a cold burst loses its
    # whole establishment window, which is why the identical offered load
    # reads 0 delivered in the matrix but 0% loss once a paced probe has
    # warmed the session. Prime with a handful of paced datagrams and keep
    # every attempt, so a persistent zero stays visible instead of being
    # hidden behind the retry.
    attempts = [udp_capacity(exposed_port, count, paced_pps,
                             rate_mbit=rate_mbit)]
    if attempts[0].get("datagrams_received", 0) == 0:
        with contextlib.suppress(Exception):
            udp_capacity(exposed_port, 32, 200.0, rate_mbit=rate_mbit)
        primed = udp_capacity(exposed_port, count, paced_pps,
                              rate_mbit=rate_mbit)
        primed["after_prime"] = True
        attempts.append(primed)
    paced = attempts[-1]
    sated = udp_capacity(exposed_port, count, pps, rate_mbit=rate_mbit)
    # Capacity = the delivered rate of the HIGHEST offered step that arrived
    # intact (<=1% loss), i.e. the last point before the path bends. When the
    # top (saturating) step is also lossless, the real ceiling is above the
    # probe's reach and the figure is a LOWER BOUND — that is the loopback
    # case (every tool delivers the full 27.2 Mbit/s offer), which is why the
    # old "capacity_mbit = paced delivered" was really the probe's own pace.
    steps = [paced, sated]
    lossless = [a for a in steps if (a.get("loss_pct") or 0.0) <= 1.0]
    cap = max(lossless, key=lambda a: a.get("delivered_mbit", 0.0)) \
        if lossless else None
    return {
        "paced": paced,
        "saturated": sated,
        "capacity_mbit": cap.get("delivered_mbit") if cap else None,
        "capacity_offered_mbit": cap.get("offered_mbit") if cap else None,
        "capacity_is_lower_bound": bool(cap is not None and cap is sated),
        "capacity_loss_pct": cap.get("loss_pct") if cap else None,
        "capacity_mbit_best_attempt": max(a.get("delivered_mbit", 0.0)
                                          for a in attempts),
        "attempts": attempts,
        "paced_loss_pct": paced.get("loss_pct"),
        "saturated_loss_pct": sated.get("loss_pct"),
    }


def udp_capacity(exposed_port: int, count: int, pps: float,
                 rate_mbit: float = 0.0) -> dict:
    """Sustained UDP forwarder capacity: send `count` datagrams at a paced
    `pps` rate and count the echoes — the "how much UDP can one client
    carry" number the probes alone never give. Sending and draining share
    ONE socket (replies land on the sender's ephemeral port), and the pace
    keeps the measurement in the sustainable regime instead of flooding
    the tunnel into drop territory.

    `rate_mbit` is the cell's shaped link rate (0 = unshaped). Without it a
    fixed 20k pps (~24 Mbit/s incl. headers) is offered even on a 20 Mbit/s
    cell, where 100% "loss" is just the shaper doing its job — a number
    that says nothing about the tunnel. On a shaped cell the offered load is
    therefore kept under the link budget, and both the offered and the
    delivered rate are recorded so `loss_pct` is interpretable."""
    stop = threading.Event()
    recv = [0]
    lock = threading.Lock()

    def drain(s):
        try:
            while not stop.is_set():
                try:
                    s.recvfrom(65535)
                    with lock:
                        recv[0] += 1
                except TimeoutError:
                    continue
        except OSError:
            pass

    payload = b"x" * 128
    on_wire_bits = (len(payload) + 28 + 14) * 8  # payload + UDP/IP + eth
    offered_bitrate = pps * on_wire_bits
    if rate_mbit:
        # 0.4 of the shaped rate: enough to expose where the tunnel bends,
        # well under the link rate so the shaper is not the only dropper
        budget = rate_mbit * 1e6 * 0.4
        if offered_bitrate > budget:
            pps = max(50.0, budget / on_wire_bits)
            offered_bitrate = pps * on_wire_bits
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(0.5)
    # The probe's own receive buffer is a measurement parameter: the kernel
    # default (~212 KiB) holds only ~1600 of the 128-byte reply datagrams, so
    # a burst that outruns the single drain thread used to lose datagrams in
    # THIS process and report them as tunnel loss. Size the buffer from the
    # offered load (half a second of replies, rounded up) so the reported
    # loss is the path's, not the probe's.
    want = int(max(on_wire_bits / 8 * pps * 0.5, 1 << 20))
    with contextlib.suppress(OSError):
        s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, want)
    rcvbuf = s.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)
    t = threading.Thread(target=drain, args=(s,), daemon=True)
    t.start()
    try:
        sent = 0
        interval = 1.0 / pps
        t0 = time.perf_counter()
        next_t = t0
        while sent < count:
            try:
                s.sendto(payload, ("127.0.0.1", exposed_port))
                sent += 1
            except OSError:
                break  # path saturated; report what we got
            next_t += interval
            delay = next_t - time.perf_counter()
            if delay > 0:
                time.sleep(delay)
        wall = time.perf_counter() - t0
        time.sleep(1.0)  # catch-up window for in-flight replies
    finally:
        s.close()
    stop.set()
    t.join()
    with lock:
        received = recv[0]
    loss = 100.0 * (sent - received) / sent if sent else 0.0
    return {
        "datagrams_sent": sent,
        "datagrams_received": received,
        "loss_pct": round(loss, 2),
        "pps": round(sent / wall, 1) if wall > 0 else 0.0,
        "offered_pps": round(pps, 1),
        "offered_mbit": round(offered_bitrate / 1e6, 2),
        "delivered_mbit": round(
            received * on_wire_bits / wall / 1e6, 2) if wall > 0 else 0.0,
        "shaped_rate_mbit": rate_mbit or None,
        # SO_RCVBUF doubles the requested value on Linux (bookkeeping);
        # recorded so a high loss_pct can be checked against the probe's own
        # buffer before it is blamed on the tunnel
        "probe_rcvbuf_bytes": rcvbuf,
    }


def mem_stats(samples: list) -> dict:
    """samples: list of (server_kb, client_kb) tuples."""
    if not samples:
        return {"server_avg_kb": 0, "client_avg_kb": 0, "total_avg_kb": 0,
                "total_peak_kb": 0, "samples": 0}
    totals = [s + c for s, c in samples]
    return {
        "server_avg_kb": round(sum(s for s, _ in samples) / len(samples)),
        "client_avg_kb": round(sum(c for _, c in samples) / len(samples)),
        "total_avg_kb": round(sum(totals) / len(totals)),
        "total_peak_kb": max(totals),
        "samples": len(samples),
    }


def run_rss_sampler(server_pid: int, client_pid: int, stop: threading.Event,
                    out: list, interval: float = 0.5) -> None:
    while not stop.is_set():
        try:
            with open(f"/proc/{server_pid}/statm") as fh:
                s = int(fh.read().split()[1]) * 4
            with open(f"/proc/{client_pid}/statm") as fh:
                c = int(fh.read().split()[1]) * 4
            out.append((s, c))
        except (OSError, ValueError, IndexError):
            pass
        stop.wait(interval)


def run_cpu_sampler(server_pid: int, client_pid: int, stop: threading.Event,
                    out: list, interval: float = 0.5) -> None:
    """Sample per-process CPU% of one core (utime+stime delta over wall
    time, HZ=100 ticks): the tradeoff cost for the noise (crypto) and KCP
    (ARQ) rows, which the guidance quotes but nothing measured. Values are
    percentages of ONE core and can exceed 100 (multi-core utilization)."""
    def ticks(pid: int) -> tuple | None:
        try:
            with open(f"/proc/{pid}/stat") as fh:
                f = fh.read().split(") ", 1)[1].split()
            return int(f[11]) + int(f[12]), time.monotonic()
        except (OSError, ValueError, IndexError):
            return None

    prev = [ticks(server_pid), ticks(client_pid)]
    while not stop.is_set():
        stop.wait(interval)
        cur = [ticks(server_pid), ticks(client_pid)]
        for i, (p, c) in enumerate(zip(prev, cur)):
            if p is None or c is None:
                continue
            wall = c[1] - p[1]
            if wall > 0:
                out.append((i, (c[0] - p[0]) / wall))  # % of one core
        prev = cur


def cpu_stats(samples: list) -> dict:
    """samples: list of (which_pid, cpu_pct_of_one_core)."""
    if not samples:
        return {"server_avg_pct": 0.0, "client_avg_pct": 0.0,
                "total_avg_pct": 0.0, "total_peak_pct": 0.0, "samples": 0}
    s = [v for i, v in samples if i == 0]
    c = [v for i, v in samples if i == 1]
    totals = [a + b for a, b in zip(s, c)]
    return {
        "server_avg_pct": round(sum(s) / len(s), 1),
        "client_avg_pct": round(sum(c) / len(c), 1),
        "total_avg_pct": round(sum(totals) / len(totals), 1),
        "total_peak_pct": round(max(totals), 1),
        "samples": len(samples),
    }


# --- results file: merge + atomic incremental dump ---------------------------
def load_results(path: Path) -> dict:
    if path.exists():
        try:
            return json.loads(path.read_text())
        except json.JSONDecodeError:
            print(f"WARNING: {path} is not valid JSON; starting fresh")
    return {"meta": {}, "results": {}}


def dump_results(data: dict, path: Path) -> None:
    """Atomic write: a crash mid-dump can never lose earlier arms."""
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2))
    os.replace(tmp, path)


def merge_arm(data: dict, tool: str, cell: str, entry: dict,
              path: Path) -> None:
    """Merge one completed arm into the results and checkpoint to disk.
    An error never clobbers a previously recorded ok entry (a failed
    re-run must not destroy the baseline it was trying to refresh)."""
    data.setdefault("meta", {})["schema"] = SCHEMA
    prev = data.setdefault("results", {}).setdefault(tool, {}).get(cell)
    if entry.get("status") == "error" and prev is not None \
            and prev.get("status") != "error":
        return
    data["results"][tool][cell] = entry
    dump_results(data, path)
