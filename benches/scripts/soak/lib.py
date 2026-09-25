#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Shared infrastructure for the Soak benchmark model.

The model measures *workloads under staged network conditions* as time
series — not one average per tool per network condition. This module carries only the reusable
primitives (process bookkeeping, the lock, the local backends, the
samplers, the individual probes); the model itself — test types, the
timeline, the claim rules, per-tool shaping — lives in `soak.py`.

Design principles:
- every metric is externally observable (peers are black boxes): the
  load is offered client-side, quality is measured on a client-side
  interactive stream, resources come from /proc
- continue-on-error: a failed probe records the failure with its typed
  reason instead of a fake 0
- isolation: every tool owns its port band and, in a batch, its own
  shaped path (one HTB class + independent netem per tool)
"""

import contextlib
import functools
import itertools
import json
import math
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

# The runner that owns the lock ledger and the process groups. Identified by
# its own name so the liveness guards below can never drift from the file
# that actually runs: a guard that greps for a renamed/retired runner is a
# guard that silently never fires (it did, for `bench.py` after the Soak
# model replaced the matrix — concurrent runs stopped being refused and the
# stale sweep could kill a live run's processes).
RUNNER_NAME = "soak.py"
WORK_PREFIX = "molehill-bench."


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


def _proc_cmdline(pid: int) -> bytes:
    """The NUL-joined cmdline of `pid`, or b"" when it is gone/unreadable."""
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return b""


def reap_pids(pids: list) -> int:
    """SIGKILL leftover pids whose /proc cmdline still mentions a bench
    workdir. Two guards:
    - the cmdline check (rejects recycled pids);
    - a parent check: a process whose parent is a live runner belongs to a
      RUNNING run and is never reaped (concurrent runs used to kill each
      other's processes through this sweep; the global lock now also refuses
      concurrency, this is the second line of defense)."""
    probe = f"{tempfile.gettempdir()}/{WORK_PREFIX}".encode()
    killed = 0
    for pid in pids:
        cmd = _proc_cmdline(pid)
        if probe not in cmd:
            continue
        try:
            with open(f"/proc/{pid}/stat") as fh:
                stat = fh.read().split(") ", 1)[1]
            ppid = int(stat.split()[1])
            parent_cmd = _proc_cmdline(ppid)
        except (OSError, ValueError, IndexError):
            parent_cmd = b""  # orphaned (parent gone) -> reap
        if RUNNER_NAME.encode() in parent_cmd:
            continue  # a live run owns this process
        with contextlib.suppress(OSError):
            os.kill(pid, signal.SIGKILL)
            killed += 1
    return killed


def sweep_stale(work: Path) -> int:
    """Reap pids recorded by previous bench runs (a crashed run leaks
    that would otherwise pollute the next run's measurements and ports).
    Live processes of a running run are never touched (see reap_pids)."""
    total = 0
    for f in Path(tempfile.gettempdir()).glob(f"{WORK_PREFIX}*/pids.json"):
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


def _live_runner_pid() -> int | None:
    """The pid recorded in the lock file, if that process is still a runner.

    A recycled pid must not wedge the lock behind an unrelated process, so
    the cmdline is checked, not just /proc presence.
    """
    with contextlib.suppress(OSError, ValueError):
        pid = int(LOCK_PATH.read_text().strip())
        if pid and RUNNER_NAME.encode() in _proc_cmdline(pid):
            return pid
    return None


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
            pid = _live_runner_pid()
            if pid is not None:
                # A caller that redirected stderr (a shell loop does)
                # would otherwise see only a non-zero exit and no reason.
                # Leave the reason on disk next to the lock so the blocked
                # run can be diagnosed after the fact.
                with contextlib.suppress(OSError):
                    LOCK_PATH.with_suffix(".blocked").write_text(
                        f"{time.strftime('%H:%M:%S')} pid {os.getpid()} "
                        f"blocked by pid {pid}\n"
                    )
                sys.exit(
                    f"another bench run (pid {pid}) is active — "
                    "concurrent runs interfere with each other; "
                    "wait for it to finish"
                )
            LOCK_PATH.unlink(missing_ok=True)  # stale lock from a crash
    sys.exit("could not acquire the bench lock")


def release_lock() -> None:
    LOCK_PATH.unlink(missing_ok=True)
    LOCK_PATH.with_suffix(".blocked").unlink(missing_ok=True)


# --- configuration -----------------------------------------------------------
@dataclass
class Knobs:
    """Run parameters. Everything that changes a number is recorded in the
    results meta — a constant buried in code is a method nobody can audit.

    Every field here is read by the runner or by a probe: a knob that is
    accepted but ignored is a method claim the run cannot back, so dead
    knobs are deleted rather than kept "for later". The env names that
    override each field carry the same value in the results meta.
    """

    molehill_bin: str
    peer_dir: str
    # --- workload (versioned constants of the model) -----------------------
    # bulk TCP streams offered; the ramp's load knob
    streams_max: int = 8
    # short connections per second offered by the churn connector
    churn_connects_s: int = 16
    # the UDP session's ping interval
    udp_interval_ms: int = 20
    # --- SLO (method constant) --------------------------------------------
    # the interactive stream must stay under this p99 AND under this error
    # rate for a load level to count as sustainable (`capacity`)
    slo_rtt_p99_ms: float = 50.0
    slo_error_rate: float = 0.0
    # --- test-type parameters ---------------------------------------------
    # per load-step settle window and the interactive-stream sample rate
    settle_s: float = 6.0
    ping_interval_ms: int = 50
    # the operating point for `cost`: fraction of the configured max load
    cost_operating_point: float = 0.8
    # rrul saturation: streams = this x cpu count
    rrul_stream_factor: int = 1
    # soak: fraction of the configured max load carried through the schedule
    soak_load_fraction: float = 0.5
    # --- batching / parallelism -------------------------------------------
    # CPU budget per concurrent tool pair, used to size a batch
    cores_per_pair: float = 7.0
    max_batch: int = 8
    # --- process / misc ----------------------------------------------------
    pool_size: int = 8
    allow_port_hi: int = 26999  # server-side allow_ports upper bound

    @classmethod
    def from_env(cls) -> "Knobs":
        """Build the run's parameters from the environment.

        The env names are the method's public interface (docs/release.md);
        each one is echoed into the results meta by the runner.
        """
        return cls(
            molehill_bin=os.environ.get(
                "MOLEHILL_BIN",
                str(Path(__file__).parents[3] / "target/release/molehill"),
            ),
            peer_dir=os.environ.get(
                "PEER_DIR", str(Path.home() / "tmp" / "bench-peers")
            ),
            streams_max=_env_int("SOAK_STREAMS_MAX", 8),
            churn_connects_s=_env_int("SOAK_CHURN_CONNECTS_S", 16),
            udp_interval_ms=_env_int("SOAK_UDP_INTERVAL_MS", 20),
            slo_rtt_p99_ms=_env_float("SOAK_SLO_RTT_P99_MS", 50.0),
            slo_error_rate=_env_float("SOAK_SLO_ERROR_RATE", 0.0),
            settle_s=_env_float("SOAK_SETTLE_S", 6.0),
            ping_interval_ms=_env_int("SOAK_PING_INTERVAL_MS", 50),
            cost_operating_point=_env_float("SOAK_COST_OPERATING_POINT", 0.8),
            rrul_stream_factor=_env_int("SOAK_RRUL_STREAM_FACTOR", 1),
            soak_load_fraction=_env_float("SOAK_SOAK_LOAD_FRACTION", 0.5),
            cores_per_pair=_env_float("SOAK_CORES_PER_PAIR", 7.0),
            max_batch=_env_int("SOAK_MAX_BATCH", 8),
            pool_size=_env_int("POOL_SIZE", 8),
        )


def _env_int(name: str, default: int) -> int:
    """Environment override of an integer knob; unset/blank keeps the default."""
    v = os.environ.get(name)
    try:
        return int(v) if v else default
    except ValueError:
        return default


def _env_float(name: str, default: float) -> float:
    """Environment override of a float knob; unset/blank keeps the default."""
    v = os.environ.get(name)
    try:
        return float(v) if v else default
    except ValueError:
        return default


# --- local backends ----------------------------------------------------------
@dataclass(frozen=True)
class BackendPorts:
    """The three backend listeners a test needs, taken from its port band."""

    iperf: int
    echo: int
    udp: int

    @classmethod
    def from_band(cls, band: dict) -> "BackendPorts":
        return cls(
            iperf=band["iperf_backend"],
            echo=band["echo_backend"],
            udp=band["udp_backend"],
        )

    def as_tuple(self) -> tuple[int, int, int]:
        return (self.iperf, self.echo, self.udp)

    def __str__(self) -> str:
        return f"{self.iperf}/{self.echo}/{self.udp}"


class Backends:
    """iperf3 (TCP, external binary) + in-process TCP/UDP echo servers.

    start() raises RuntimeError when a backend cannot come up (e.g. a port
    squatted by a leaked process); the runner records that test's failure and
    moves on instead of aborting the whole run.
    """

    def __init__(self):
        self.echo_in_probe = False
        self._stop = threading.Event()
        self._threads = []
        self._procs = []
        self._socks = []
        self._work = None
        self.ports = BackendPorts(0, 0, 0)

    @property
    def iperf_port(self) -> int:
        return self.ports.iperf

    def start(
        self, ports: BackendPorts, work: Path | None = None, echo_in_probe: bool = False
    ) -> None:
        """`echo_in_probe`: the interactive/UDP echo backends live in the
        probe's own process (so the harness is not in the measured path),
        and only the iperf3 backend is started here."""
        self.ports = ports
        self.echo_in_probe = echo_in_probe
        self._work = work
        # A previous run's wedged server can hold the port briefly (or, if
        # it survived a kill, indefinitely): kill whatever listens on our
        # ports and retry a few times instead of failing the whole test.
        last: Exception | None = None
        for _ in range(5):
            try:
                self._start_once(work)
                return
            except Exception as e:  # noqa: BLE001 — retry reads the message
                last = e
                if "Address already in use" not in str(
                    e
                ) and "died on startup" not in str(e):
                    break  # not a port conflict; retrying cannot help
                for port in ports.as_tuple():
                    self._kill_port_holder(port)
                time.sleep(1.0)
        raise RuntimeError(f"backends failed on ports {ports}: {last}") from last

    @staticmethod
    def _kill_port_holder(port: int) -> None:
        """SIGKILL processes listening on `port` (ours: bench backend ports)."""
        try:
            out = subprocess.run(
                ["ss", "-ltnp", f"sport = :{port}"],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            ).stdout
        except Exception:  # noqa: BLE001 — `ss` missing means no holder to kill
            return
        for pid in re.findall(r"pid=(\d+)", out):
            with contextlib.suppress(OSError, ValueError):
                os.kill(int(pid), signal.SIGKILL)

    def _start_once(self, work: Path | None) -> None:
        ports = self.ports
        try:
            # --logfile puts the (temp) workdir into the cmdline so a leaked
            # iperf3 from a crashed run is reaped by sweep_stale like the
            # other test processes (its port would silently break the next
            # run's throughput measurements)
            cmd = ["iperf3", "-s", "-B", "127.0.0.1", "-p", str(ports.iperf)]
            if work is not None:
                cmd += ["--logfile", str(Path(work) / "iperf3.log")]
            proc = subprocess.Popen(
                cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
            self._procs.append(proc)
            if work is not None:
                record_pid(work, proc.pid)
            time.sleep(0.3)
            if proc.poll() is not None:
                raise RuntimeError(
                    f"iperf3 died on startup (port {ports.iperf}, exit "
                    f"{proc.returncode}) — is another iperf3 or a leaked "
                    "bench process holding the port?"
                )
            if not self.echo_in_probe:
                self._spawn_tcp(ports.echo)
                self._spawn_udp(ports.udp)
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
            raise RuntimeError(f"backends failed on ports {ports}: {e}") from e

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
                    threading.Thread(target=serve, args=(conn,), daemon=True).start()
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

        A stalled throughput test at a shaped (netem rate-limited) stage
        poisons the single-test iperf3 server's state ("unable to receive
        cookie" / "Bad file descriptor"): every later test on that server
        then hangs until the harness timeout, which is why the 8-stream
        value at the rate stages kept coming back None. A fresh process has
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
        proc = subprocess.Popen(
            cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        self._procs.insert(0, proc)
        if self._work is not None:
            record_pid(self._work, proc.pid)
        time.sleep(0.4)
        if proc.poll() is not None:
            raise RuntimeError(
                f"iperf3 died on restart (port {self.iperf_port}, exit "
                f"{proc.returncode})"
            )

    def iperf_burst(
        self, target: "ThroughputTarget", streams: int, secs: int, tag: str = ""
    ) -> dict:
        """One `-P streams` throughput measurement through the tunnel.

        Single-run by design: the model's load knob is offered for a settle
        window and the sample is the window, not a median of reps. The
        single-test iperf3 server hygiene is kept — a "server is busy" answer
        is server state, not path state, so the server is restarted and the
        run retried once with both attempts' evidence kept.
        """
        # Bound the client by the test length, never tighter than the
        # historical `secs + 20`: through the tunnel, the weakest stages need
        # that slack to finish the results exchange (the tighter secs*2+6
        # bound turned the KCP rtt100 samples into nulls).
        timeout = max(secs * 2.0 + 6.0, secs + 20.0)
        art = None
        if self._work is not None:
            safe = re.sub(r"[^A-Za-z0-9._-]+", "_", tag) if tag else "test"
            art = Path(self._work) / "iperf-raw" / safe / f"P{streams}"
        r = iperf_result(target.exposed, streams, secs, timeout, art)
        if not r["ok"] and "server is busy" in str(r.get("reason", "")):
            with contextlib.suppress(Exception):
                self.restart_iperf()
            retry_art = None
            if art is not None:
                retry_art = art.with_name(art.name + "-retry")
            r2 = iperf_result(target.exposed, streams, secs, timeout, retry_art)
            r2["retried_after_busy"] = True
            if r2["ok"] or not r2.get("timed_out"):
                r = r2
        print(
            f"    P{streams}: {'ok' if r['ok'] else 'fail'} "
            f"{r.get('gbps_headline', '-')} Gbit/s, "
            f"{r.get('gbps_received_own_window', '-')} recv-own, "
            f"wall {r['wall_s']}s"
            f"{'' if r['ok'] else ' — ' + str(r.get('reason'))[:90]}",
            flush=True,
        )
        if not r["ok"]:
            with contextlib.suppress(Exception):
                self.restart_iperf()
        return r

    def stop(self) -> None:
        self._stop.set()
        for s in self._socks:
            with contextlib.suppress(OSError):
                s.close()
        for p in self._procs:
            with contextlib.suppress(OSError):
                # SIGKILL, not terminate: a wedged iperf3 server (stuck in a
                # stalled test) can survive SIGTERM and hold its port, which
                # would poison the next test's backends with EADDRINUSE
                os.kill(p.pid, signal.SIGKILL)
        self._procs.clear()
        self._socks.clear()
        self._threads.clear()
        self._stop = threading.Event()


@dataclass(frozen=True)
class ThroughputTarget:
    """Where a throughput sample is dialed, and the invariant that guards it.

    `exposed` is the port the tool listens on — the path under test;
    `backend` is the iperf3 server behind it. Dialing the backend measures the
    loopback ceiling and silently bypasses the tool, which is the schema-v3
    incident of 2026-09-10 (every tool reported the same ~46 Gbit/s). Carrying
    the two ports in one value and refusing equality makes that mistake
    unrepresentable instead of merely documented.
    """

    exposed: int
    backend: int

    def __post_init__(self) -> None:
        if self.exposed == self.backend:
            raise ValueError(
                f"throughput target {self.exposed} is the iperf3 backend, not "
                "the tool's exposed port — the tool would be bypassed"
            )

    @classmethod
    def from_band(cls, band: dict) -> "ThroughputTarget":
        return cls(exposed=band["iperf_exposed"], backend=band["iperf_backend"])


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


def iperf_active_seconds(doc: dict, secs: int) -> float:
    """Length of the MEASURED window = the sender's non-omitted intervals.

    This is the honest denominator for the sender-side rate: total bytes sent
    include the `-O` warm-up, so `bytes / sum_sent.seconds` (the full test
    duration) is not the measured-window rate. The receiver keeps a SEPARATE
    window (`sum_received.seconds`), reported alongside: after the sender
    stops, the backend can still be draining (6.24 s of receive window for a
    6.00 s send window on a shaped stage), and collapsing the two into one
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
        # deflated the headline by ~12% on the loopback stage.
        acc += max(0.0, s.get("end", 0.0) - s.get("start", 0.0))
    return acc if acc > 0 else float(secs)


def iperf_result(
    port: int, streams: int, secs: int, timeout: float, artifact: Path | None
) -> dict:
    """One `-P streams` iperf3 client run through the tunnel; never raises.

    A raw artifact directory is captured on EVERY outcome (requested and
    actual timeout, exit status, stdout, stderr) so a later null in the
    results file can be re-diagnosed instead of guessed at."""
    if artifact is not None:
        artifact.mkdir(parents=True, exist_ok=True)
    cmd = [
        "iperf3",
        "-J",
        "-c",
        "127.0.0.1",
        "-p",
        str(port),
        "-t",
        str(secs),
        "-O",
        "2",
        "-P",
        str(streams),
    ]
    t0 = time.perf_counter()
    timed_out = False
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, check=False
        )
        rc, out, err = proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as e:
        timed_out = True
        rc, err = None, (e.stderr or "")
        out = (
            e.stdout
            if isinstance(e.stdout, str)
            else (e.stdout or b"").decode("utf-8", "replace")
        )
    wall = round(time.perf_counter() - t0, 2)
    if artifact is not None:
        (artifact / "cmd.txt").write_text(" ".join(cmd) + "\n")
        (artifact / "stdout.json").write_text(out or "")
        (artifact / "stderr.txt").write_text(err or "")
        (artifact / "meta.json").write_text(
            json.dumps(
                {
                    "streams": streams,
                    "secs": secs,
                    "timeout_s": timeout,
                    "exit": rc,
                    "timed_out": timed_out,
                    "wall_s": wall,
                },
                indent=1,
            )
        )
    base = {
        "streams": streams,
        "secs": secs,
        "timeout_s": round(timeout, 1),
        "wall_s": wall,
        "timed_out": timed_out,
        "exit": rc,
        "artifact": str(artifact / "stdout.json") if artifact else None,
    }
    doc = None
    with contextlib.suppress(ValueError):
        doc = json.loads(out) if out else None
    if doc and doc.get("error"):
        return base | {"ok": False, "reason": f"iperf3 error: {doc['error']}"}
    if timed_out:
        return base | {
            "ok": False,
            "reason": f"client timed out after {timeout:.0f}s "
            "(harness bound, test may still have been "
            "transferring)",
        }
    if rc != 0:
        detail = (err or out or "").strip().replace("\n", " ")[:200]
        return base | {
            "ok": False,
            "reason": f"iperf3 exit {rc}: {detail or 'no output'}",
        }
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
        "gbps_received_own_window": round(recv.get("bits_per_second", 0.0) / 1e9, 4),
        "receiver_window_s": round(recv.get("seconds", 0.0), 3),
        "retransmits": sent.get("retransmits", 0),
        "per_stream_bytes": [s.get("bytes", 0) for s in streams],
        "per_stream_gbps": [
            round(s.get("bits_per_second", 0.0) / 1e9, 4) for s in streams
        ],
        "mean_rtt_us": streams[0].get("mean_rtt") if streams else None,
    }


# Method constants of the derived statistics. They are named because they
# decide whether a slope or a rate is reported at all, i.e. they are part of
# the method and not incidental guards. They live here, with the statistics
# themselves, so the runner, the gate and the charts cannot disagree about
# what a slope or a loss rate means.
MIN_SLOPE_POINTS = 10  # fewer samples than this cannot show a drift
MIN_SLOPE_SPAN_S = 60.0  # nor can a window shorter than a minute
MIN_WINDOW_POINTS = 2  # a "worst window mean" needs two samples
MIN_LOSS_SAMPLES = 3  # a UDP loss rate needs a denominator worth having
PROBE_FIELDS = 3  # a probe line is `t \t metric \t value`
AB_BUILDS = 2  # --ab takes exactly one binary per side
# An interactive stream silent for longer than this is a wedge: recorded as a
# flat segment with its duration, never a bare `None`.
WEDGE_SILENCE_S = 5.0
# The window the derived UDP loss rate is computed over (the probe's own
# attempt stream is the denominator, so a shaped stage's loss is a rate and
# not a count of 1s).
LOSS_WINDOW_S = 5.0


# --- series statistics ------------------------------------------------------
def pct(values: list, q: float):
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, max(0, math.ceil(q * len(s)) - 1))]


def points(rows: list, metric: str) -> list[tuple[float, float]]:
    """The `(t, v)` samples of one metric, in series order."""
    return [
        (r["t"], r["v"])
        for r in rows
        if r.get("metric") == metric and isinstance(r.get("v"), (int, float))
    ]


def series_stats(rows: list, metric: str) -> dict:
    vals = [v for _, v in points(rows, metric)]
    if not vals:
        return {"n": 0}
    return {
        "n": len(vals),
        "mean": round(sum(vals) / len(vals), 3),
        "p50": round(pct(vals, 0.5), 3),
        "p99": round(pct(vals, 0.99), 3),
        "max": round(max(vals), 3),
        "min": round(min(vals), 3),
    }


def slope_per_min(rows: list, metric: str):
    """Least-squares slope in units/minute: the drift axis (handles, RSS,
    CPU). A leak is a slope, not a level."""
    pts = points(rows, metric)
    if len(pts) < MIN_SLOPE_POINTS:
        return None
    t0, t1 = pts[0][0], pts[-1][0]
    if t1 - t0 < MIN_SLOPE_SPAN_S:
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
    pts = points(rows, metric)
    if len(pts) < MIN_WINDOW_POINTS:
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


def flat_segments(
    rows: list, metric: str = "rtt_interactive_ms", gap_s: float = WEDGE_SILENCE_S
) -> list:
    """Gaps in the interactive series beyond the wedge threshold: the
    shape of a silent stage, recorded instead of a null."""
    pts = [(r["t"], r["v"]) for r in rows if r.get("metric") == metric]
    return [
        {"start": round(t0, 3), "end": round(t1, 3), "duration_s": round(t1 - t0, 1)}
        for (t0, _), (t1, _) in itertools.pairwise(pts)
        if t1 - t0 > gap_s
    ]


def loss_rate_series(rows: list, window_s: float = LOSS_WINDOW_S) -> list:
    """A sliding-window UDP loss rate, in percent, from the probe's stream.

    The probe emits one `udp_attempt` per ping and then either `rtt_udp_ms`
    (the echo came back) or `udp_loss` (it did not), so the attempt stream is
    the denominator and a shaped stage's loss becomes a rate over time rather
    than a row of 1s. A count of loss events has no contrast to plot — which
    is exactly the degenerate panel the old chart drew.
    """
    events = sorted(
        [(r["t"], bool(r["v"])) for r in rows if r.get("metric") == "udp_attempt"],
        key=lambda p: p[0],
    )
    if not events:
        return []
    out = []
    head = 0
    for i, (t, _) in enumerate(events):
        while events[head][0] < t - window_s:
            head += 1
        window = events[head : i + 1]
        if len(window) < MIN_LOSS_SAMPLES:  # too few attempts for a rate
            continue
        lost = sum(1 for _, is_loss in window if is_loss)
        out.append(
            {
                "t": t,
                "metric": "udp_loss_pct",
                "v": round(100.0 * lost / len(window), 2),
            }
        )
    return out


# --- tool processes -------------------------------------------------------
PEER_BINS = {"frp": "frps", "rathole": "rathole", "nps": "nps"}


class ArmProcs:
    """Process group for one tool pair: spawn with per-pair logs, kill as a group."""

    def __init__(self, work: Path, label: str):
        self.work = work
        self.label = label
        self.pids: list[int] = []
        self.tool_pids: list[int] = []  # the tool's own pids (sampler input)
        self.logs: list[Path] = []

    def spawn(
        self,
        cmd: list,
        tool: bool = True,
        cwd: Path | None = None,
        role: str = "",
        env: dict | None = None,
    ) -> None:
        # `role` keeps a multi-process tool's streams apart: without it every
        # process of one tool appends to the same file, and a parser cannot
        # tell their lines (or their counters) apart.
        name = self.label.replace(" ", "_").replace("/", "_")
        if role:
            name = f"{name}.{role}"
        log = self.work / f"{name}.log"
        with open(log, "ab") as f:
            pid = subprocess.Popen(cmd, stdout=f, stderr=f, cwd=cwd, env=env).pid
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


@functools.cache
def noise_keys(binary: str) -> tuple[str, str]:
    """Generate once per run and cache a Noise keypair via `--genkey`.

    Cached per binary path (the screen swaps builds between steps, and a
    build swap must not silently reuse another build's keypair).
    """
    out = subprocess.run(
        [binary, "--genkey"], capture_output=True, text=True, timeout=30, check=False
    ).stdout
    priv = pub = ""
    lines = out.splitlines()
    for i, line in enumerate(lines):
        if line.strip() == "Private Key:" and i + 1 < len(lines):
            priv = lines[i + 1].strip()
        elif line.strip() == "Public Key:" and i + 1 < len(lines):
            pub = lines[i + 1].strip()
    if not priv or not pub:
        raise RuntimeError(f"noise keygen failed for {binary}: {out[:200]}")
    return (priv, pub)


def molehill_config(
    work: Path, variant: str, knobs: Knobs, p: dict, binary: str = ""
) -> Path:
    """Write server/client tomls for one molehill variant; returns config dir."""
    d = work / "molehill"
    d.mkdir(exist_ok=True)
    mode = "direct" if variant in ("mux-off", "noise-direct") else "multiplex"
    # Single-variable variants. `mux` is the default control (plain transport,
    # multiplex, count = 4): `noise` changes only the transport, `mux1` only
    # the tunnel count, `kcp4` only the data-plane carrier (on top of the
    # noise variant). `noise-direct` is the noise transport in direct mode: a
    # new point on the transport x mode grid, and the variant that isolates the
    # record-stream cost with no mux framing in the way (the mux frame
    # reader asks for less than a record, so a record staged in the wrapper
    # is structural there — compare `mux-off` for the transport axis and
    # `noise` for the mode axis).
    data_c = data_s = ""
    if variant == "noise":
        transport = "noise"
    elif variant == "mux1":  # count axis: plain, one tunnel
        transport = "plain"
        data_c = "default_count = 1\n"
    elif variant == "kcp4":  # carrier axis: noise + KCP-over-UDP
        transport = "noise"
        data_c = (
            f'default_carrier = "kcp"\n'
            f'default_data_addr = "127.0.0.1:{p["kcp_bind"]}"\n'
            f"default_count = 4\n"
        )
        data_s = f'bind_addr = "127.0.0.1:{p["kcp_bind"]}"\n'
    elif variant == "noise-direct":  # transport axis, no mux layer
        transport = "noise"
    else:
        transport = "plain"
    noise_s = noise_c = ""
    if transport == "noise":
        priv, pub = noise_keys(binary or knobs.molehill_bin)
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
bind_addr = "127.0.0.1:{p["control"]}"

{data_s_block}{server_transport}""")
    (d / "client.toml").write_text(f"""[client]
default_token = "bench"

[client.control]
default_remote_addr = "127.0.0.1:{p["client_dial"]}"

[client.data]
default_mode = "{mode}"
{data_c}[client.transport]
type = "{transport}"
{noise_c}
[client.services.iperf]
local_addr = "127.0.0.1:{p["iperf_backend"]}"
remote_bind_addr = "127.0.0.1:{p["iperf_exposed"]}"
pool_size = {knobs.pool_size}

[client.services.echo]
local_addr = "127.0.0.1:{p["echo_backend"]}"
remote_bind_addr = "127.0.0.1:{p["echo_exposed"]}"
pool_size = {knobs.pool_size}

[client.services.udpecho]
protocol = "udp"
local_addr = "127.0.0.1:{p["udp_backend"]}"
remote_bind_addr = "127.0.0.1:{p["udp_exposed"]}"
pool_size = 2
udp_buffer_size = 2048
udp_idle_timeout = 60
udp_send_queue_size = 1024
""")
    return d


def start_molehill(procs: ArmProcs, d: Path, binary: str) -> None:
    procs.spawn([binary, "--server", str(d / "server.toml")], role="server")
    procs.spawn([binary, "--client", str(d / "client.toml")], role="client")


def diag_env() -> dict:
    """The opt-in instrumentation the run was started with.

    Everything a tool spawns inherits the parent environment, so an explicit
    `MOLEHILL_MUX_STATS=1` (or `MOLEHILL_KCP_STATS`, `MOLEHILL_STRIPE_COUNT`,
    `MOLEHILL_TCP_BUFFER_BYTES`) reaches the tool unchanged. The runner used
    to force the mux framing counters ON for every molehill spawn: that is
    molehill-only work inside the measured path, the peers have no equivalent,
    and nothing in the Soak model parses the lines — an asymmetric instrument
    is a method bug. The runner records what was set instead of deciding.
    """
    return {
        k: v
        for k, v in os.environ.items()
        if k.startswith(("MOLEHILL_", "BENCH_", "SOAK_")) and k != "MOLEHILL_BIN"
    }


@dataclass(frozen=True)
class ToolSetup:
    """Everything a tool-specific setup function needs.

    One value instead of five positional arguments, and a uniform signature
    (`setup(s, knobs)`) so the runner's dispatch is a plain dict lookup with
    no per-tool lambdas to drift out of sync.
    """

    band: dict
    procs: ArmProcs
    work: Path
    variant: str = ""
    binary: str = ""

    def path(self, tool: str) -> Path:
        """The tool's config directory inside the run's work dir."""
        d = self.work / tool
        d.mkdir(exist_ok=True)
        return d


def setup_molehill(s: ToolSetup, knobs: Knobs) -> None:
    d = molehill_config(s.work, s.variant, knobs, s.band, s.binary)
    start_molehill(s.procs, d, s.binary or knobs.molehill_bin)


def setup_frp(s: ToolSetup, knobs: Knobs) -> None:
    p = s.band
    d = s.path("frp")
    peer = Path(knobs.peer_dir) / "frp"
    (d / "frps.toml").write_text(
        f'bindAddr = "127.0.0.1"\nbindPort = {p["control"]}\nauth.token = "bench"\n'
    )
    (d / "frpc.toml").write_text(f"""serverAddr = "127.0.0.1"
serverPort = {p["client_dial"]}
auth.token = "bench"
loginFailExit = false

[[proxies]]
name = "iperf"
type = "tcp"
localIP = "127.0.0.1"
localPort = {p["iperf_backend"]}
remotePort = {p["iperf_exposed"]}

[[proxies]]
name = "echo"
type = "tcp"
localIP = "127.0.0.1"
localPort = {p["echo_backend"]}
remotePort = {p["echo_exposed"]}

[[proxies]]
name = "udpecho"
type = "udp"
localIP = "127.0.0.1"
localPort = {p["udp_backend"]}
remotePort = {p["udp_exposed"]}
""")
    s.procs.spawn([str(peer / "frps"), "-c", str(d / "frps.toml")])
    s.procs.spawn([str(peer / "frpc"), "-c", str(d / "frpc.toml")])


def setup_rathole(s: ToolSetup, knobs: Knobs) -> None:
    p = s.band
    d = s.path("rathole")
    peer = Path(knobs.peer_dir) / "rathole"
    (d / "server.toml").write_text(f"""[server]
bind_addr = "127.0.0.1:{p["control"]}"
[server.transport]
type = "tcp"

[server.services.iperf]
bind_addr = "127.0.0.1:{p["iperf_exposed"]}"
token = "bench"

[server.services.echo]
bind_addr = "127.0.0.1:{p["echo_exposed"]}"
token = "bench"

[server.services.udpecho]
type = "udp"
bind_addr = "127.0.0.1:{p["udp_exposed"]}"
token = "bench"
""")
    (d / "client.toml").write_text(f"""[client]
remote_addr = "127.0.0.1:{p["client_dial"]}"
[client.transport]
type = "tcp"

[client.services.iperf]
local_addr = "127.0.0.1:{p["iperf_backend"]}"
token = "bench"

[client.services.echo]
local_addr = "127.0.0.1:{p["echo_backend"]}"
token = "bench"

[client.services.udpecho]
type = "udp"
local_addr = "127.0.0.1:{p["udp_backend"]}"
token = "bench"
""")
    s.procs.spawn([str(peer), "--server", str(d / "server.toml")])
    s.procs.spawn([str(peer), "--client", str(d / "client.toml")])


def setup_nps(s: ToolSetup, knobs: Knobs) -> None:
    """nps: one server (`nps`) plus one client (`npc`) holding all proxies.

    nps multiplexes every proxy over its bridge connection — the same
    shape as molehill's mux variant and frp — so it is a like-for-like peer.
    Crypt and compress are OFF (the example npc.conf ships them on): the
    plain-TCP peers are the unencrypted, uncompressed reference points.

    nps resolves its config relative to the EXECUTABLE's directory — a
    config in the CWD is silently ignored (verified: it bound the shipped
    defaults instead) — so each test builds its own tree next to the work
    dir: hard links to the ~24 MB of binaries (same inode, and
    /proc/self/exe resolves to this path), a real `conf/` to customise,
    and the shipped web assets by symlink. The server also writes an
    sqlite db, so it runs with this directory as its CWD.
    """
    p = s.band
    d = s.path("nps")
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
    # test's port band, and the http/https proxy ports stay empty so
    # nothing privileged is opened.
    (conf / "nps.conf").write_text(f"""appname = nps
runmode = pro
http_proxy_ip =
http_proxy_port =
https_proxy_port =
bridge_type = tcp
bridge_ip = 127.0.0.1
bridge_port = {p["control"]}
public_vkey = bench
log_level = 6
web_host = 127.0.0.1
web_username = bench
web_password = bench
web_ip = 127.0.0.1
web_port = {p["nps_web"]}
allow_ports = {p["iperf_exposed"]}-{p["udp_exposed"]}
allow_flow_limit = false
allow_rate_limit = false
allow_tunnel_num_limit = false
allow_local_proxy = false
allow_connection_num_limit = false
allow_multi_ip = false
system_info_display = false
disconnect_timeout = 60
""")
    # Idempotent: the peer's tree is shared by every test in a run, and a
    # second test would otherwise fail on the symlink that already
    # exists (the first test created it).
    web = d / "web"
    if not web.exists():
        web.symlink_to(peer / "web", target_is_directory=True)
    # No spaces around `=`: npc's ini parser keeps them (verified — the
    # client then dials the wrong transport and dies on a UDP write to
    # port 0), while nps.conf's parser accepts either.
    (d / "npc.conf").write_text(f"""[common]
server_addr=127.0.0.1:{p["client_dial"]}
conn_type=tcp
vkey=bench
auto_reconnection=true
crypt=false
compress=false
max_conn=1000
disconnect_timeout=60

[tcp_iperf]
mode=tcp
target_addr=127.0.0.1:{p["iperf_backend"]}
server_port={p["iperf_exposed"]}

[tcp_echo]
mode=tcp
target_addr=127.0.0.1:{p["echo_backend"]}
server_port={p["echo_exposed"]}

[udp_udpecho]
mode=udp
target_addr=127.0.0.1:{p["udp_backend"]}
server_port={p["udp_exposed"]}
""")
    s.procs.spawn([str(d / "nps")], cwd=d)
    if not wait_port(p["control"], 10):
        raise TimeoutError("nps bridge port did not open")
    s.procs.spawn([str(d / "npc"), "-config", str(d / "npc.conf")], cwd=d)
    # The TCP tunnels only: a UDP task has no listening TCP port to wait
    # for (the bench's own UDP probe is what exercises it).
    for port in (p["iperf_exposed"], p["echo_exposed"]):
        if not wait_port(port, 10):
            raise TimeoutError(f"nps tunnel {port} not ready")


def tool_band(base: int, off: int) -> dict:
    """Per-tool port band. `base` is the tool's slot in the batch (a
    parallel run's tools never share a band), `off` the variant offset."""
    return {
        "control": base + off + 1,
        "client_dial": base + off + 1,
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
    }


def tool_version(knobs: Knobs) -> str:
    try:
        out = subprocess.run(
            [knobs.molehill_bin, "--version"],
            capture_output=True,
            text=True,
            check=False,
            timeout=15,
        ).stdout
        return next(
            (l.split()[2] for l in out.splitlines() if l.startswith("Build Version:")),
            "dev",
        )
    except OSError:
        return "dev"


def peer_version(tool: str, knobs: Knobs) -> str:
    """Best-effort version string: peers print it in different places
    (frp on stderr, rathole as a 'Build Version:' line, nps as a
    'Version:' line);
    fall back to the release-version marker written by fetch_peers.py,
    then to the upstream release version constant."""
    binname = PEER_BINS[tool]
    for cand in (
        Path(knobs.peer_dir) / tool / binname,  # frp's subdir layout
        Path(knobs.peer_dir) / binname,
    ):  # flat layouts
        if cand.exists():
            break
    else:
        return "0.5.0"
    try:
        r = subprocess.run(
            [str(cand), "--version"],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
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


#: Tool name → its setup function, all sharing `setup(s: ToolSetup, knobs)`.
#: Defined once, after the four functions, so the runner's dispatch is a dict
#: lookup instead of four lambdas that can drift from the signatures they call
#: (they did: every molehill test failed with a TypeError until a smoke run
#: caught it).
TOOL_SETUPS = {
    "molehill": setup_molehill,
    "frp": setup_frp,
    "rathole": setup_rathole,
    "nps": setup_nps,
}


def git_revision() -> str:
    """The repository revision this run's harness came from (`--dirty` marked).

    §10's provenance rule: a number must describe a revision someone can
    check out. The harness's own revision is recorded beside the tool's
    version because a harness change moves numbers too.
    """
    try:
        r = subprocess.run(
            ["git", "describe", "--always", "--dirty", "--broken"],
            capture_output=True,
            text=True,
            check=False,
            timeout=10,
            cwd=Path(__file__).parent,
        )
        return r.stdout.strip() or "unknown"
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
