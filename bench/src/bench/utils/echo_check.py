"""Does the campaign's echo backend behave like a load generator?

`ECHO_SRC` in `bench/src/bench/workloads.py` is the process every arm forwards
to: it echoes TCP, echoes UDP, and announces each accepted peer as the
transparency evidence. If *it* stalls, the carrier's counters read clean while
the transfer stops — and that is not hypothetical. Three carrier-side
instruments were built to chase a stall that the harness could have produced,
and the round that finally measured the backend found it could not have (see
`HANDOFF.md`, "The echo backend is decoupled").

This is the check that keeps that honest, and it is deliberately **outside the
gate**: it is not part of `just bench selfcheck`, the pre-commit chain or the
release ritual. It exists so that a future round asking "is the load generator
the stall?" can answer with a run instead of an inference::

    uv run --project bench --locked python src/bench/utils/echo_check.py

Three questions, in the order a stalled transfer raises them:

* **does a full-duplex bulk echo complete, byte for byte** — 8 MiB out and 8 MiB
  back at the same time, with small socket buffers so the two directions really
  contend;
* **how much does the backend absorb while its peer reads nothing** — the
  coupling bound. An echo that stops accepting bytes because it is blocked
  writing turns every slow reader into a stalled transfer;
* **does it say so** — a transfer over `ANNOUNCE_OVER` must leave an
  `ECHO DONE read=… wrote=…` line, which is what makes the next stall
  attributable by the backend's own counters rather than by inference.

Runs in seconds, needs no root and no network namespace, binds two ports on the
loopback. It is not a benchmark: it asserts behaviour and *prints* the coupling
number, because the number belongs to the host.
"""

from __future__ import annotations

import socket
import subprocess
import sys
import threading
import time
from contextlib import suppress

from bench import workloads

#: The bulk transfer this check echoes: far past any socket buffer, so neither
#: direction can be satisfied by kernel memory alone.
BULK = 8 << 20
#: `ECHO DONE` is announced above this; a smaller connection must stay silent or
#: the log becomes the load at four thousand arrivals a second.
ANNOUNCE_OVER = 64 << 10
#: How long the peer refuses to read in the coupling experiment.
STALL_S = 3.0
#: What a healthy backend can absorb with its peer unread. Well past `BULK`, so
#: a transfer of `BULK` cannot be blocked by this bound.
MIN_ABSORBED = 16 << 20
#: An attempt that has not finished by now is a stall, not a slow measurement.
DEADLINE_S = 30.0


def backend_source() -> str:
    """The script the campaign hands its child interpreter, byte for byte.

    Read as the *parsed* constant, never by slicing the file between its
    quotes: the literal is escaped for Python, so a raw slice hands the child
    `\\\\n` instead of `\\n` — and the first version of this check did exactly
    that, then reported a log-formatting defect that did not exist. A test that
    measures a copy of the thing is the mistake this whole file exists to
    prevent.
    """
    return workloads.ECHO_SRC


def free_port() -> int:
    """A port the kernel just handed out and nothing is using."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def start(port: int) -> subprocess.Popen:
    """Run the backend on `port` and wait for it to announce itself."""
    udp = free_port()
    proc = subprocess.Popen(
        [sys.executable, "-c", backend_source(), str(port), str(udp)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.time() + DEADLINE_S
    while time.time() < deadline:
        line = proc.stdout.readline() if proc.stdout else ""
        if not line:
            proc.kill()
            raise SystemExit("the backend died before it was ready")
        if "ECHO READY" in line:
            return proc
    proc.kill()
    raise SystemExit("the backend never announced itself")


def round_trip(port: int, total: int, bufsize: int) -> str:
    """Send `total` bytes while reading `total` back; answer what happened."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    # Small buffers on purpose: an echo that cannot keep both directions moving
    # deadlocks in a few hundred kilobytes instead of a few megabytes.
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, bufsize)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, bufsize)
    sock.settimeout(DEADLINE_S)
    sock.connect(("127.0.0.1", port))
    payload = bytes(range(256)) * (total // 256)
    total = len(payload)
    got = bytearray()
    stopped: list[str] = []

    def read_side() -> None:
        try:
            while len(got) < total:
                chunk = sock.recv(1 << 16)
                if not chunk:
                    stopped.append(f"eof at {len(got)}")
                    return
                got.extend(chunk)
        except OSError as e:
            stopped.append(f"{type(e).__name__} at {len(got)}")

    started = time.perf_counter()
    reader = threading.Thread(target=read_side, daemon=True)
    reader.start()
    try:
        sock.sendall(payload)
    except OSError as e:
        stopped.append(f"send {type(e).__name__} at {len(got)}")
    reader.join(DEADLINE_S)
    elapsed = time.perf_counter() - started
    with suppress(OSError):
        sock.close()
    if len(got) != total:
        why = stopped[0] if stopped else "no progress"
        return f"STALLED after {elapsed:.1f}s: {len(got)} of {total} bytes ({why})"
    if bytes(got) != payload:
        return "CORRUPT: the echo did not return the bytes it was given"
    return f"ok {total} bytes both ways in {elapsed:.2f}s"


def absorbed(port: int, limit: int) -> int:
    """Bytes the peer can write while it refuses to read, up to `limit`."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(STALL_S)
    sock.connect(("127.0.0.1", port))
    written = [0]
    blocked = threading.Event()

    def write_side() -> None:
        chunk = b"z" * (1 << 20)
        try:
            while written[0] < limit:
                sock.sendall(chunk)
                written[0] += len(chunk)
        except OSError:
            pass
        blocked.set()

    writer = threading.Thread(target=write_side, daemon=True)
    writer.start()
    # Reading nothing for the stall *is* the experiment.
    blocked.wait(STALL_S)
    at_stall = written[0]
    with suppress(OSError):
        sock.shutdown(socket.SHUT_RDWR)
    with suppress(OSError):
        sock.close()
    return at_stall


def announced(log: str) -> bool:
    """Whether the backend reported the totals of a connection it finished."""
    return any(line.startswith("ECHO DONE read=") for line in log.splitlines())


def main() -> int:
    port = free_port()
    proc = start(port)
    failures: list[str] = []
    try:
        for bufsize in (32 << 10, 4 << 20):
            verdict = round_trip(port, BULK, bufsize)
            print(
                f"full-duplex {BULK >> 20} MiB, buf {bufsize >> 10:>4d} KiB: {verdict}"
            )
            if not verdict.startswith("ok"):
                failures.append(verdict)
        at_stall = absorbed(port, 64 << 20)
        print(f"accepted while unread for {STALL_S:.0f}s: {at_stall >> 20} MiB")
        if at_stall < MIN_ABSORBED:
            failures.append(
                f"the backend absorbed {at_stall >> 20} MiB with its peer unread, "
                f"below the {MIN_ABSORBED >> 20} MiB a {BULK >> 20} MiB echo needs"
            )
    finally:
        proc.kill()
        proc.wait(timeout=5)
        log = proc.stdout.read() if proc.stdout else ""
    if not announced(log):
        failures.append("no `ECHO DONE` line: a stalled transfer is unattributable")
    for failure in failures:
        print(f"FAIL {failure}")
    print("PASS" if not failures else f"{len(failures)} FAILED")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
