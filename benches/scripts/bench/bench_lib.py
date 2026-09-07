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
    molehill_secs_weak: int = 15
    peer_secs_weak: int = 8
    hol_secs: float = 8.0
    hol_bulk_rate_tcp: float = 200.0
    hol_bulk_rate_udp: float = 50.0
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
            peer_dir=os.environ.get("PEER_DIR", "/tmp/bench-peers"),
            molehill_reps=e("MOLEHILL_REPS", 3),
            peer_reps=e("PEER_REPS", 1),
            molehill_secs=e("MOLEHILL_SECS", 8),
            peer_secs=e("PEER_SECS", 5),
            molehill_secs_weak=e("MOLEHILL_SECS_WEAK", 15),
            peer_secs_weak=e("PEER_SECS_WEAK", 8),
            hol_secs=e("HOL_SECS", 8.0),
            hol_bulk_rate_tcp=e("HOL_BULK_RATE_TCP", 200.0),
            hol_bulk_rate_udp=e("HOL_BULK_RATE_UDP", 50.0),
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
    loss_model: str = ""  # what Netem actually applied (recorded in meta)

    @property
    def weak(self) -> bool:
        return self.loss or self.burst or self.rate or self.rtt


def parse_cell(cell: str) -> CellSpec:
    """Formats: "loss%/rtt", "loss%:burst%/rtt", "r<mbit>/<rtt>", "0/0".
    burst = the Gilbert-Elliot bad-state exit probability in % — mean burst
    length is 100/burst packets (see Netem.on)."""
    loss = burst = rate = rtt = 0.0
    if cell.startswith("r") and cell[1:2].isdigit():
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
    return CellSpec(name=name, loss=loss, burst=burst, rate=rate, rtt=rtt)


# --- netem -------------------------------------------------------------------
class Netem:
    def __init__(self):
        self.tc = shutil.which("tc") or ""
        self.ok = False
        self.active = False
        if self.tc and subprocess.run(["sudo", "-n", "true"],
                                         check=False).returncode == 0:
            probe = subprocess.run(
                ["sudo", self.tc, "qdisc", "replace", "dev", "lo", "root",
                 "netem", "loss", "0%", "delay", "0ms"],
                capture_output=True, check=False)
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
        if spec.rtt:
            args += ["delay", f"{spec.rtt:g}ms"]
        if spec.rate:
            args += ["rate", f"{spec.rate:g}mbit"]
        applied = subprocess.run(args, capture_output=True,
                                 check=False).returncode == 0
        self.active = applied
        return applied

    def off(self) -> None:
        if self.tc and self.active:
            subprocess.run(["sudo", self.tc, "qdisc", "del", "dev", "lo",
                            "root"], capture_output=True, check=False)
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
        self.iperf_port = 0
        self.tcp_port = 0
        self.udp_port = 0

    def start(self, iperf_port: int, tcp_port: int, udp_port: int,
              work: Path | None = None):
        self.iperf_port, self.tcp_port, self.udp_port = (
            iperf_port, tcp_port, udp_port)
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
            raise RuntimeError(f"backends failed on ports "
                               f"{iperf_port}/{tcp_port}/{udp_port}: {e}") \
                from e

    def _spawn_tcp(self, port: int) -> None:
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", port))
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
        srv.bind(("127.0.0.1", port))
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

    def stop(self) -> None:
        self._stop.set()
        for s in self._socks:
            with contextlib.suppress(OSError):
                s.close()
        for p in self._procs:
            with contextlib.suppress(OSError):
                p.terminate()
        self._procs.clear()
        self._socks.clear()
        self._threads.clear()
        self._stop = threading.Event()


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
def throughput(reps: int, streams: int, secs: int, iperf_port: int) -> tuple | None:
    """iperf3 TCP throughput; returns (gbps, retransmits) of the MEDIAN rep
    (both values from the same rep, so the retransmit count belongs to the
    reported throughput), or None when every rep failed."""
    pairs = []
    for _ in range(reps):
        try:
            j = subprocess.run(
                ["iperf3", "-J", "-c", "127.0.0.1", "-p", str(iperf_port),
                 "-t", str(secs), "-O", "2", "-P", str(streams)],
                capture_output=True, text=True, timeout=secs + 20,
                check=False).stdout
            d = json.loads(j)
            pairs.append((d["end"]["sum_received"]["bits_per_second"] / 1e9,
                          d["end"]["sum_sent"].get("retransmits", 0)))
        except Exception:
            continue
    if not pairs:
        return None
    pairs.sort(key=lambda p: p[0])
    gbps, retr = pairs[len(pairs) // 2]
    return round(gbps, 3), retr


def latency(exposed_port: int, samples: int = 300) -> dict:
    """Connection-path RTT: connect + 1-byte ping over fresh TCP connections."""
    xs = []

    def once() -> float:
        t0 = time.perf_counter()
        s = socket.socket()
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.connect(("127.0.0.1", exposed_port))
        s.sendall(b"p")
        s.recv(1)
        dt = (time.perf_counter() - t0) * 1000.0
        s.close()
        return dt

    for _ in range(30):
        once()
    xs = sorted(once() for _ in range(samples))
    return {
        "p50": round(xs[len(xs) // 2], 3),
        "p95": round(xs[int(len(xs) * 0.95)], 3),
        "p99": round(xs[int(len(xs) * 0.99) - 1], 3),
        "mean": round(sum(xs) / len(xs), 3),
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
    """Merge one completed arm into the results and checkpoint to disk."""
    data.setdefault("meta", {})["schema"] = SCHEMA
    data.setdefault("results", {}).setdefault(tool, {})[cell] = entry
    dump_results(data, path)
