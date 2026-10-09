#!/usr/bin/env python3
"""The model's instruments: what is observed, and from where.

Everything here reads the system the way an operator could — `/proc`, `ss`,
`ip`, and the tool's own version string. No arm's internal counters ever enter a
comparison, which is what lets the same instrument describe a shared library, a
peer binary or a control arm that has no internals at all.

Two rules the code enforces rather than documents:

* **A counter window must cover exactly what it is a ratio of.** Every sample is
  a before/after pair around one workload process, so the payload bytes and the
  wire bytes describe the same seconds.
* **A missing reading is typed, never zero.** `Unavailable` carries a reason
  with every metric the instruments could not produce; a consumer that reads
  `0` where nothing was measured is a consumer that reports a stall as a
  property of the path.
"""

from __future__ import annotations

import contextlib
import os
import platform
import subprocess
import threading
import time
from pathlib import Path

import lib
import topology

#: The `/proc/<pid>/io` fields this model reads. `rchar`/`wchar` count bytes at
#: the syscall boundary, so their ratio to `syscr`/`syscw` is the I/O *shape* —
#: how much each read and write carried. That is the number that separates "this
#: arm costs more userspace framing" (same syscalls, more CPU) from "this arm
#: has a different I/O shape" (more syscalls per byte).
IO_FIELDS = ("rchar", "wchar", "syscr", "syscw")
#: How often the peak samplers poll while a workload runs. Short against every
#: workload here, and recorded in the results meta because it decides what a
#: "peak" means.
SOCKET_POLL_S = 0.15
PROC_POLL_S = 0.20
#: The kernel's page size, for `statm`.
PAGE_KIB = os.sysconf("SC_PAGE_SIZE") // 1024


def pct(values: list, q: float):
    """Nearest-rank percentile: the model's one definition.

    Nearest-rank, not interpolated: with the sample counts these workloads
    produce (thousands), interpolation would imply a resolution the measurement
    does not have, and a p99 that no request actually experienced.
    """
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def mean(values: list):
    return sum(values) / len(values) if values else None


def cpu_ticks(pids: dict) -> dict:
    """CPU ticks (user+sys) per role, from `/proc/<pid>/stat`."""
    out: dict = {}
    for role, pid in pids.items():
        with contextlib.suppress(OSError, ValueError, IndexError):
            fields = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()
            out[role] = int(fields[11]) + int(fields[12])
    return out


def proc_io(pids: dict) -> dict:
    out: dict = {}
    for role, pid in pids.items():
        with contextlib.suppress(OSError, ValueError):
            fields = dict(
                line.split(": ", 1)
                for line in Path(f"/proc/{pid}/io").read_text().splitlines()
                if ": " in line
            )
            out[role] = {k: int(fields[k]) for k in IO_FIELDS if k in fields}
    return out


def proc_mem(pids: dict) -> dict:
    """RSS, open descriptors and threads per role — the footprint of the arm."""
    out: dict = {}
    for role, pid in pids.items():
        row: dict = {}
        with contextlib.suppress(OSError, ValueError, IndexError):
            row["rss_kib"] = int(Path(f"/proc/{pid}/statm").read_text().split()[1])
            row["rss_kib"] *= PAGE_KIB
        with contextlib.suppress(OSError):
            row["fds"] = len(list(Path(f"/proc/{pid}/fd").iterdir()))
        with contextlib.suppress(OSError, ValueError, IndexError):
            fields = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()
            row["threads"] = int(fields[17])
        out[role] = row
    return out


def snapshot(topo: topology.Topology, pids: dict) -> dict:
    """One instant of every counter the model reads."""
    return {
        "t": time.time(),
        "cpu": cpu_ticks(pids),
        "io": proc_io(pids),
        "dev": topo.dev(),
        "tcp": topo.tcp_mib(),
    }


def delta(before: dict, after: dict) -> dict:
    """The counters' difference over one window, in the units they are read in."""
    cpu = {
        role: round((after["cpu"].get(role, 0) - ticks) / topology.CLK_TCK, 4)
        for role, ticks in before["cpu"].items()
    }
    wire: dict = {}
    for key, a in after["dev"].items():
        b = before["dev"].get(key)
        if not b:
            continue
        wire[key] = {k: a[k] - b[k] for k in a}
    tcp = {
        ns: {k: v - before["tcp"].get(ns, {}).get(k, 0) for k, v in counters.items()}
        for ns, counters in after["tcp"].items()
    }
    io = {
        role: {k: v - before["io"].get(role, {}).get(k, 0) for k, v in counters.items()}
        for role, counters in after["io"].items()
    }
    drops = {
        key: {
            "rx_dropped": a["rx_dropped"]
            - before.get("dev", {}).get(key, {}).get("rx_dropped", 0),
            "tx_dropped": a["tx_dropped"]
            - before.get("dev", {}).get(key, {}).get("tx_dropped", 0),
        }
        for key, a in wire.items()
        if a["rx_dropped"] or a["tx_dropped"]
    }
    return {
        "elapsed_s": round(after["t"] - before["t"], 4),
        "cpu_s": cpu,
        "cpu_s_total": round(sum(cpu.values()), 4),
        "io": io,
        "syscalls": sum(c["syscr"] + c["syscw"] for c in io.values()),
        "wire": wire,
        "drops": drops,
        "tcp": tcp,
    }


class SocketWatch:
    """Peak sockets on a service port, polled *during* a workload.

    Sampling once at the end would read zero on every arm — a round-trip arm
    closes its connections before it returns — which would hide exactly the
    contrast this model exists to price: the server owning one accepted socket
    per visitor on L4 and none on L3. The peak is what the arm held while it was
    loaded.
    """

    def __init__(
        self,
        topo: topology.Topology,
        port: int,
        kind: str = "tcp",
        interval: float = SOCKET_POLL_S,
    ):
        self.topo, self.port, self.kind, self.interval = topo, port, kind, interval
        self.peak = {"server": 0, "client": 0}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            counts = self.topo.sockets(self.port, self.kind)
            for role in self.peak:
                self.peak[role] = max(self.peak[role], counts.get(role, 0))
            self._stop.wait(self.interval)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self._stop.set()
        self._thread.join(timeout=3)


class ProcWatch:
    """Peak RSS / fds / threads of the arm's daemons, polled during a workload.

    A level, not a series: the drift question ("is it leaking?") needs a long
    run and belongs to the soak model, while "what does this architecture cost
    in memory and descriptors at this operating point" is a level, and a peak
    because the interesting number is what it needed, not what it happened to
    hold when the workload ended.
    """

    def __init__(self, pids_of, interval: float = PROC_POLL_S):
        self.pids_of, self.interval = pids_of, interval
        self.peak = {"rss_kib": 0, "fds": 0, "threads": 0}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            mem = proc_mem(self.pids_of())
            for key in self.peak:
                total = sum(r.get(key, 0) for r in mem.values())
                self.peak[key] = max(self.peak[key], total)
            self._stop.wait(self.interval)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self._stop.set()
        self._thread.join(timeout=3)


def wire_of(sample: dict, iface: str = topology.TUNNEL_IFACE) -> int:
    """One link's bytes in both directions over the sampled window.

    The server-to-client link is the path every arm's traffic crosses: the
    tunnel for L4 and L3, and the routed path for the control. Its two
    directions are summed because a wire costs what it carries, acknowledgements
    included.
    """
    w = sample.get("wire", {}).get(iface, {})
    return w.get("rx_bytes", 0) + w.get("tx_bytes", 0)


def packets_of(sample: dict, iface: str = topology.TUNNEL_IFACE) -> int:
    w = sample.get("wire", {}).get(iface, {})
    return w.get("rx_packets", 0) + w.get("tx_packets", 0)


def visitor_bytes(sample: dict) -> int:
    """What the visitor offered: the denominator every cost ratio is over."""
    return sample.get("wire", {}).get(topology.VISITOR_IFACE, {}).get("tx_bytes", 0)


def counter_metrics(sample: dict, peaks: dict) -> dict:
    """Every metric that needs only counters, over the sample's own window."""
    window = max(sample.get("elapsed_s", 0.0), 1e-9)
    offered = visitor_bytes(sample)
    link = wire_of(sample)
    packets = packets_of(sample)
    calls = sample.get("syscalls", 0)
    moved = sum(c["rchar"] + c["wchar"] for c in sample.get("io", {}).values())
    retrans = sum(c.get("retrans_segs", 0) for c in sample.get("tcp", {}).values())
    dropped = sum(
        d["rx_dropped"] + d["tx_dropped"] for d in sample.get("drops", {}).values()
    )
    return {
        "offered_gbps": round(offered * 8 / window / 1e9, 4) if offered else None,
        "cpu_s_per_gbit": (
            round(sample["cpu_s_total"] / (offered * 8 / 1e9), 4) if offered else None
        ),
        "cpu_cores": round(sample["cpu_s_total"] / window, 3),
        "bytes_per_syscall": round(moved / calls, 1) if calls else None,
        "syscalls_per_s": round(calls / window, 1) if calls else None,
        "wire_per_visitor_byte": round(link / offered, 4) if offered else None,
        "mean_carried_packet_b": round(link / packets, 1) if packets else None,
        "rss_peak_mib": (
            round(peaks["rss_kib"] / 1024, 1) if peaks.get("rss_kib") else None
        ),
        "fds_peak": peaks.get("fds"),
        "threads_peak": peaks.get("threads"),
        "retrans_segments": retrans,
        "dropped_packets": dropped,
    }


def binary_provenance(binary: str) -> dict:
    """The build under test: path, hash, size, mtime and its own version line.

    A run must correspond to a revision and a freshly built binary; a number
    from a binary that was replaced underneath it describes code that no longer
    exists. The version string comes from the binary itself, not from
    `Cargo.toml`.
    """
    info = lib.binary_fingerprint(binary)
    version = ""
    with contextlib.suppress(OSError, subprocess.SubprocessError):
        r = subprocess.run(
            [binary, "--version"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        version = (r.stdout or r.stderr).strip().splitlines()[0][:200]
    return {
        "path": binary,
        "sha256": info.get("sha256"),
        "bytes": info.get("bytes"),
        "mtime": info.get("mtime"),
        "stale": info.get("stale"),
        "version": version,
    }


def host_provenance(with_calibration: bool = True) -> dict:
    """The machine: identity, kernel, and the two tool-free probes.

    The probes are why a comparison across days can be *checked* rather than
    assumed: a CPU workload and the loopback path are measured the same way in
    every run, and two runs whose probes disagree are refused (see
    `analysis.comparability`).
    """
    info = {"identity": lib.host_identity(), "kernel": platform.release()}
    if with_calibration:
        info["calibration"] = lib.host_calibration()
        info["loopback"] = lib.host_loopback()
    return info


def unavailable(reason: str) -> dict:
    """One typed absence, in the shape the results file stores."""
    return {"reason": reason}
