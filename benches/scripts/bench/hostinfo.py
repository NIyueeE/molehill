#!/usr/bin/env python3
"""What a run is: its binary, its host, and the host's two probes.

A number is only comparable with another number from the same *kind* of machine
in the same *state*, and neither half of that is a name:

* `binary_fingerprint` identifies the bytes that ran, and whether they are older
  than the sources they were built from (a run whose binary was two commits
  behind HEAD describes code that no longer exists);
* `host_identity` is a stable key (machine id + CPU model + core count) rather
  than the hostname, which a container changes on every restart;
* `host_calibration` (a fixed CPU workload) and `host_loopback` (one socket
  pair, pinned to a single CPU **in a child process**) are the two measurements
  that certify the machine was in the same state - the loopback one pins in a
  child because `sched_setaffinity` is inherited by everything the harness
  spawns afterwards.

Both probes never raise: a host that cannot run one records the typed failure,
and a comparison reports it as unchecked rather than as agreement.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import socket
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path


def binary_fingerprint(path) -> dict:
    """Identify the binary a run measured, and whether it is stale.

    §10 spent two incidents on this: a run whose binary was two commits behind
    HEAD described code that no longer existed, and a `--profile bench` build
    that survived a revert did the same thing here. A version string cannot
    tell two builds of the same release apart, so the fingerprint is the
    binary's own bytes; `stale` compares its mtime against the newest source
    file, which is what catches "I edited, rebuilt nothing, and measured the
    old binary".

    Returns `{}` when the file cannot be read (a peer binary may not exist for
    a tool that is not in the run), and never raises.
    """
    path = Path(path)
    try:
        data = path.read_bytes()
        mtime = path.stat().st_mtime
    except OSError:
        return {}
    newest_src = 0.0
    root = Path(__file__).resolve().parents[3]
    for rel in ("src", "build.rs"):
        target = root / rel
        if target.is_file():
            newest_src = max(newest_src, target.stat().st_mtime)
            continue
        for f in target.rglob("*.rs"):
            with contextlib.suppress(OSError):
                newest_src = max(newest_src, f.stat().st_mtime)
    for rel in ("Cargo.toml", "Cargo.lock"):
        with contextlib.suppress(OSError):
            newest_src = max(newest_src, (root / rel).stat().st_mtime)
    return {
        "sha256": hashlib.sha256(data).hexdigest()[:16],
        "bytes": len(data),
        "mtime": round(mtime, 3),
        "stale": bool(newest_src and mtime < newest_src),
    }


#: Identity files, most authoritative first. A container hostname changes on
#: every container restart, which is what made the drift gate unusable here:
#: stored runs on the *same hardware* carried different hostnames, so
#: `analysis.comparability` refused to compare them, so that half of the gate
#: had never once run. `/etc/machine-id` is stable for the machine (it
#: survives container recreation, because it is the host's).
_HOST_ID_FILES = ("/etc/machine-id", "/var/lib/dbus/machine-id")


def _read_first(paths: tuple) -> str:
    for p in paths:
        try:
            return Path(p).read_text().strip()
        except OSError:
            continue
    return ""


def host_identity() -> dict:
    """The host this run happened on: a *stable* identity, not a name.

    `hostname` is kept (it is what a reader recognises), but it is not the
    comparison key: in a containerized bench host it changes on every restart
    while the hardware does not, so comparing by hostname compares two
    differently-named runs of the same machine and — the actual failure —
    refuses two runs of the same machine because the container was recreated
    between them.

    `host_id` is the calibration-grade key: the machine id (survives container
    recreation) plus the CPU model and the core count (what actually bounds the
    loopback ceiling), hashed so the file stays readable and two hosts cannot
    be confused by a shared component. `None` when nothing can be read, which
    is the honest answer rather than a guess: the gate then falls back to the
    hostname and says so.

    The CPU budget and the loopback ceiling are properties of the *machine*,
    so this is the field two runs must agree on before any cross-run claim is
    made (analysis.comparability; AGENTS.md §10, "same model, same host").
    """
    machine_id = _read_first(_HOST_ID_FILES)
    cpu_model = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    nproc = str(os.cpu_count() or 0)
    if not (machine_id or cpu_model):
        return {"hostname": socket.gethostname(), "host_id": None}
    basis = f"{machine_id}|{cpu_model}|{nproc}"
    return {
        "hostname": socket.gethostname(),
        "host_id": hashlib.sha256(basis.encode()).hexdigest()[:16],
        "host_id_basis": {
            "machine_id": bool(machine_id),
            "cpu_model": cpu_model,
            "cpu_count": nproc,
        },
    }


#: The host calibration probe: a fixed CPU-bound workload (SHA-256 over a fixed
#: buffer), reported as MiB/s.
#:
#: `host_id` answers "which machine is this?", and on a host *without*
#: `/etc/machine-id` — this one, see HANDOFF.md — it reduces to
#: `cpu_model | nproc`, so two different machines can hash to the same id.
#: A name cannot close that hole; a measurement can: two runs are comparable
#: only if the machine was in the same *state*, and the state (turbo, thermal,
#: steal, a neighbour's load) is exactly what moves a fixed workload's
#: throughput. The probe is deliberately CPU-bound rather than a loopback
#: throughput measurement: measured on this host, a 128 MiB socketpair probe
#: drifts 18.7 % across median-of-five readings (it follows the CPU's power
#: state), while this one repeats to 2.2 % — a calibration key has to be
#: stabler than the differences it exists to detect.
#: The tool label the SLO gates: this repository's own binary. A reference peer
#: that misses the SLO is a finding about the peer — reported with its number,
#: never a block — so anything that *aborts* on an SLO violation has to make the
#: same distinction (measured: a driver that did not, aborted a healthy sweep at
#: rathole's first clean stage, whose p99 is ~100 ms by its own nature). Shared
#: with `analysis.SUBJECT` so the two cannot drift apart.
SUBJECT = "molehill"

#: The loopback probe's transfer, per reading: MiB pushed through one socket
#: pair, and how many readings the median is taken over. 512 MiB is ~0.2 s at
#: this host's ceiling — long enough that the socket and the copies dominate
#: the timer, short enough that the whole probe (one discarded warm-up plus
#: the reps) stays under two seconds.
LOOPBACK_MIB = 512
LOOPBACK_REPS = 5
LOOPBACK_CHUNK = 1 << 20
#: Per-socket timeout for the probe. The whole transfer is ~0.2 s at this host's
#: ceiling, so this is three orders of magnitude of slack — it exists to fail a
#: stalled probe, not to time a slow one.
LOOPBACK_TIMEOUT_S = 30.0
#: The whole child (interpreter start, two warm-ups, five transfers), with room
#: for a machine that is busy rather than broken.
LOOPBACK_CHILD_TIMEOUT_S = 120.0

CALIBRATION_MIB = 192
#: Median of this many readings. Three is the smallest count with a median at
#: all, and the probe is ~0.5 s per reading.
CALIBRATION_REPS = 3
#: The buffer hashed per iteration. 1 MiB keeps `hashlib` on its bulk path (it
#: releases the GIL above ~2 KiB), so the number is a CPU measurement rather
#: than a Python-loop measurement.
CALIBRATION_CHUNK = 1 << 20


def _calibration_once(buffer: bytes) -> float:
    """One reading: MiB/s of SHA-256 over `buffer`, repeated to `CALIBRATION_MIB`."""
    digest = hashlib.sha256()
    iterations = CALIBRATION_MIB
    started = time.perf_counter()
    for _ in range(iterations):
        digest.update(buffer)
    elapsed = time.perf_counter() - started
    digest.digest()
    return iterations / elapsed if elapsed > 0 else 0.0


#: Pre-touched and reused for every transfer. A freshly allocated `bytearray`
#: faults its pages *inside* the timed region, and where those pages land is a
#: property of the process — one of the two sources of the variance that made
#: the first formulation unusable (see `host_loopback`).
_LOOPBACK_BUF = bytearray(LOOPBACK_CHUNK)
for _i in range(0, LOOPBACK_CHUNK, 4096):
    _LOOPBACK_BUF[_i] = 1


def _one_cpu() -> int | None:
    """The CPU the probe pins itself to, or `None` where that is not available
    (not Linux, or a sandbox without `sched_setaffinity`)."""
    try:
        allowed = sorted(os.sched_getaffinity(0))
    except (AttributeError, OSError):
        return None
    return allowed[0] if allowed else None


def _affinity() -> frozenset:
    """This process's CPU set, or an empty one where it cannot be read."""
    try:
        return frozenset(os.sched_getaffinity(0))
    except (AttributeError, OSError):
        return frozenset()


def _loopback_once(total_bytes: int) -> float:
    """One reading: MiB/s through a loopback socket pair, sender and reader live.

    The reader runs in its own thread so the pair is pipelined the way the
    benchmark's own streams are; the clock covers the whole transfer, because a
    sender-only clock would miss the tail still queued when the last write
    returns.
    """
    sender, reader = socket.socketpair()
    received = []
    try:
        # A writable buffer for `recv_into` — a `bytes` chunk raises inside the
        # reader thread and leaves the sender blocked on a full socket — and a
        # timeout on both ends, so a stalled probe is the typed failure
        # `host_loopback` records rather than a hang.
        buf = _LOOPBACK_BUF
        sender.settimeout(LOOPBACK_TIMEOUT_S)
        reader.settimeout(LOOPBACK_TIMEOUT_S)
        started = time.perf_counter()

        def drain() -> None:
            got = 0
            try:
                while got < total_bytes:
                    n = reader.recv_into(buf)
                    if not n:
                        break
                    got += n
            except OSError:
                pass  # recorded as a short read by the caller
            received.append(got)

        thread = threading.Thread(target=drain)
        thread.start()
        sent = 0
        try:
            while sent < total_bytes:
                sent += sender.send(buf[: min(LOOPBACK_CHUNK, total_bytes - sent)])
        except OSError:
            pass  # the reader's own error is the one worth reporting
        thread.join()
        elapsed = time.perf_counter() - started
    finally:
        sender.close()
        reader.close()
    if received != [total_bytes] or elapsed <= 0:
        raise OSError(f"loopback probe moved {received} of {total_bytes} bytes")
    return total_bytes / elapsed / (1 << 20)


def _loopback_child(cpu: int | None, total: int, reps: int) -> dict:
    """The probe as a **child process**: pin, warm up, measure, report.

    It runs in a child because `sched_setaffinity` is per-*process* and
    inherited by every child that process spawns. An earlier version pinned
    itself in the runner's own process and never restored it, so the whole
    harness — the tools under test, their iperf3 clients and servers, the
    pingers, everything — ran on one core for the rest of the run. Measured on
    the resulting sweep: clean bulk 16.4 -> 5.2 Gbit/s, interactive p99
    9.6 -> 163 ms, while a tool that is single-threaded by nature (nps) and the
    two calibration probes were untouched, because each of them needs only one
    CPU. A child cannot leak that into the run.
    """
    try:
        if cpu is not None:
            os.sched_setaffinity(0, {cpu})
        for _ in range(2):  # cold: the socket buffers have not grown yet
            _loopback_once(total)
        values = [_loopback_once(total) for _ in range(max(1, reps))]
    except (OSError, MemoryError, ValueError, AttributeError) as e:
        return {"error": str(e)[:200]}
    return {"cpu": cpu, "values": values}


#: The child's whole program: import this module by path and print its result.
#: Spawned with `sys.executable`, so a fresh interpreter does the pinning and
#: the interpreter's own threads are none of its business.
_LOOPBACK_CHILD = (
    "import json, sys; sys.path.insert(0, {libdir!r}); import hostinfo; "
    "print(json.dumps(hostinfo._loopback_child({cpu!r}, {total!r}, {reps!r})))"
)


def host_loopback(reps: int = LOOPBACK_REPS) -> dict:
    """What this host's **loopback path** measures, with no tool in it.

    `host_calibration` above certifies CPU state, and that is all it certifies.
    Measured: two container instances of the same `host_id` — one hostname
    apart, identity fields identical — differed by **25-39 %** on the clean bulk
    cells of the two arms that reach the loopback ceiling, while the CPU probe
    read 414.0 against 421.2 MiB/s, 1.7 % apart, inside the 25 % the gate
    allows. It waved that pair through, because a CPU workload is not what those
    cells are bounded by: they are bounded by this path — the copies, the
    syscalls, the socket buffers, the memory behind them.

    **Pinned to one CPU, and that is not a detail.** Unpinned, this probe is
    bimodal *across processes* — five runs at 29.4-29.7 Gbit/s and two at
    34.2-34.4 on an idle machine, the two threads' cores deciding which copy
    path they get — a 17 % spread that would refuse comparable pairs, since the
    cells it is meant to certify did not move with it (two sweeps an hour apart:
    16.3-16.8 Gbit/s both times, CPU probe 414.0 against 424.4). Pinned, ten
    runs on an idle machine span 20.99-22.57 Gbit/s (worst case 7 %, typically
    3 %), and four busy loops elsewhere on the host drop it to 20.4 — which is
    the sensitivity the key exists for. Its level also brackets the cells
    correctly: 12.9-16.7 Gbit/s of tool throughput under a ~22 Gbit/s per-core
    ceiling, with the unpinned 29-34 Gbit/s ceiling above both.

    Never raises, like `host_calibration`: a host that cannot run it records the
    typed failure and the gate reports the comparison as unchecked.
    """
    total = LOOPBACK_MIB * (1 << 20)
    before = _affinity()
    try:
        script = _LOOPBACK_CHILD.format(
            libdir=str(Path(__file__).parent), cpu=_one_cpu(), total=total, reps=reps
        )
        proc = subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            text=True,
            timeout=LOOPBACK_CHILD_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as e:
        return {"probe": "loopback_socketpair", "ok": False, "reason": str(e)[:200]}
    # The child may not have changed *this* process's affinity — that is the
    # whole reason it exists — and a run whose harness is pinned measures one
    # core forever. Checked rather than assumed, because the failure is silent
    # everywhere else: it was found only after a sweep came back with every fast
    # number a third of its usual size.
    if _affinity() != before:
        raise RuntimeError(
            "the loopback probe changed the harness's own CPU affinity "
            f"({before} -> {_affinity()}): it must run in a child process, or "
            "every tool and client this run spawns inherits the pin"
        )
    try:
        payload = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {
            "probe": "loopback_socketpair",
            "ok": False,
            "reason": f"probe child said nothing usable: {proc.stderr.strip()[:200]}",
        }
    if "error" in payload:
        return {"probe": "loopback_socketpair", "ok": False, "reason": payload["error"]}
    values = payload["values"]
    pinned = payload.get("cpu")
    lo, hi = min(values), max(values)
    return {
        "probe": "loopback_socketpair",
        "ok": True,
        "unit": "Gbit/s",
        "mib": LOOPBACK_MIB,
        "reps": len(values),
        # The CPU the probe pinned itself to, when it could: a reader comparing
        # two runs of one host wants to know it was the same one.
        "cpu": pinned,
        # Gbit/s, the unit the bench's own cells are read in, so a reader can
        # hold the two side by side without converting.
        "median": round(statistics.median(values) * 8.0 / 1024.0, 2),
        "min": round(lo * 8.0 / 1024.0, 2),
        "max": round(hi * 8.0 / 1024.0, 2),
        "spread_pct": round((hi - lo) / hi * 100.0, 1) if hi else None,
    }


def host_calibration(reps: int = CALIBRATION_REPS) -> dict:
    """What this host measures at a fixed, tool-free workload — the *state* key.

    `host_identity`'s `host_id` is stable but it is a name: it cannot tell two
    machines with the same CPU model and core count apart, and on a host with
    no machine id that is all it has. This is the measurement the original note
    asked for instead of more identity fields, and it is recorded beside the
    identity rather than hashed into it — a noisy reading folded into a key
    would make the key as noisy as the reading and impossible to diagnose.

    Never raises: a host that cannot run the probe (a sandbox without the
    cycles, a platform where it is meaningless) records the typed failure, and
    the gate then reports that the comparison could not be checked rather than
    reading a missing value as agreement.
    """
    try:
        buffer = bytes(CALIBRATION_CHUNK)  # zeroes: hashing does not care
        values = [_calibration_once(buffer) for _ in range(max(1, reps))]
    except (OSError, MemoryError, ValueError) as e:
        return {"probe": "sha256_fixed_buffer", "ok": False, "reason": str(e)[:200]}
    lo, hi = min(values), max(values)
    return {
        "probe": "sha256_fixed_buffer",
        "ok": True,
        "unit": "MiB/s",
        "mib": CALIBRATION_MIB,
        "reps": len(values),
        "median": round(statistics.median(values), 1),
        "min": round(lo, 1),
        "max": round(hi, 1),
        # The probe's own spread, recorded so a reader can hold a between-run
        # difference against the instrument's repeatability instead of guessing.
        "spread_pct": round((hi - lo) / hi * 100.0, 1) if hi else None,
    }


def git_revision(exclude: Path | None = None) -> tuple:
    """The commit this run's harness came from, and whether the tree is clean.

    §10's provenance rule: a number must describe a revision someone can check
    out. Two facts, deliberately separate:

    * `git describe --always --broken` names the commit — without `--dirty`,
      whose suffix conflates two very different situations;
    * the clean/dirty verdict comes from `git status --porcelain` **minus the
      results file this run is writing**. A run always makes its own tracked
      output dirty, so a naive `--dirty` marks every release artifact dirty and
      the mark stops carrying information. An uncommitted *source* change still
      shows up, which is the case the rule exists for.

    Returns `(revision, tree_clean)`; `tree_clean` is `None` when git cannot
    answer (a tarball checkout), which is honest rather than assumed-clean.
    """
    here = Path(__file__).parent
    rev = "unknown"
    with contextlib.suppress(OSError, subprocess.SubprocessError):
        r = subprocess.run(
            # No `--broken`: in git 2.47 it appends `-dirty` to the *name*,
            # which is the conflation this function exists to undo (the
            # clean/dirty verdict is the second return value, and it excludes
            # the results file this run is writing).
            ["git", "describe", "--always"],
            capture_output=True,
            text=True,
            check=False,
            timeout=10,
            cwd=here,
        )
        if r.returncode == 0 and r.stdout.strip():
            rev = r.stdout.strip()
    clean = None
    with contextlib.suppress(OSError, subprocess.SubprocessError):
        args = ["git", "status", "--porcelain"]
        # `:(exclude)` pathspec: everything except the run's own output. Two
        # facts make this subtle, both measured rather than assumed:
        #
        # * the pathspec is interpreted from `here`, the cwd of this call, so a
        #   repo-relative path (or an absolute one) never matches and the run's
        #   own results file would mark every artifact dirty;
        # * a path *outside* the repository cannot be excluded at all — git
        #   exits 128 with "outside repository", which would turn the whole
        #   verdict into `None` (an honest "cannot answer", but a worse one than
        #   the truth).
        #
        # An output file outside the tree cannot make the tree dirty either, so
        # the exclusion is simply not needed there and the plain verdict is the
        # right answer.
        exclude_in_tree = False
        if exclude is not None:
            try:
                root = subprocess.run(
                    ["git", "rev-parse", "--show-toplevel"],
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=10,
                    cwd=here,
                ).stdout.strip()
                resolved = Path(exclude).resolve()
                if root and resolved.is_relative_to(Path(root)):
                    exclude_in_tree = True
                    spec = Path(os.path.relpath(resolved, here.resolve()))
                    args += ["--", ".", f":(exclude){spec}"]
            except (OSError, ValueError, subprocess.SubprocessError):
                exclude_in_tree = False
        if exclude is not None and not exclude_in_tree:
            # Keep the `-- .` scope (repo files only): an outside output path
            # needs no exclusion, and the caller's intent is still "the tree".
            args += ["--", "."]
        r = subprocess.run(
            args, capture_output=True, text=True, check=False, timeout=20, cwd=here
        )
        if r.returncode == 0:
            clean = not r.stdout.strip()
        if r.returncode == 0:
            clean = not r.stdout.strip()
    return rev, clean
