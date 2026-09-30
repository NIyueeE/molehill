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
import re
import signal
import subprocess
import tempfile
import threading
import time
from dataclasses import asdict, dataclass
from dataclasses import field as dataclass_field

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


@dataclass(frozen=True)
class DrainReport:
    """What one stage transition's drain did, as evidence rather than a float.

    The drain is not part of any stage's window, so its cost and its outcome
    were invisible in the results: the run logged a duration and nothing else.
    A drain that spends its whole budget is the transition saying the path
    never went quiet, and that is exactly the state the next stage's spine is
    measured in -- so it is recorded beside the stage it precedes.
    """

    #: Seconds actually spent draining.
    seconds: float
    #: True when the budget expired with the path still busy.
    expired: bool
    #: Bytes still queued at the last poll; `None` when the stage is unshaped
    #: (no qdisc to read, so there is nothing to drain and nothing to report).
    final_backlog: int | None
    #: Sockets in a state that can still retransmit, on the port the drain
    #: watches, at the last poll.
    busy_sockets: int


#: `tc`'s size suffixes, as bytes. Binary multiples: verified against
#: `tc -s -j` on this host, where `backlog 28447Kb` read 29129713 bytes
#: (ratio 1024.06). `_backlog` parses the suffix because getting it wrong is
#: not a rounding error — an unparsed suffix reads as "no qdisc at all".
BACKLOG_UNITS = {"b": 1, "Kb": 1024, "Mb": 1024**2, "Gb": 1024**3}


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
    #: Consecutive empty-qdisc polls that count as "the senders drained". The
    #: poll interval is 0.25 s, so this is ~0.5 s of a genuinely quiet queue
    #: rather than one sample taken between two bursts.
    DRAIN_QUIET_POLLS = 2

    def __init__(self, classes: list, log=print, legs: str = "visitor"):
        self.classes = classes  # [(classid, band)]
        self.log = log
        # An unknown value falls back to the default rather than shaping the
        # wrong legs silently: the knob's own reader validates the spelling,
        # and this is the second line of defence for a direct API call.
        self.legs = legs if legs in self.LEGS else "visitor"
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

    def _tc_out(self, *args) -> str:
        r = subprocess.run(["tc", *args], capture_output=True, text=True, check=False)
        return r.stdout if r.returncode == 0 else ""

    def _backlog(self, handle: str) -> int | None:
        """The queued bytes on one tool's netem qdisc, or None if unknown.

        `tc -s qdisc show` renders each qdisc as a header line ("qdisc netem
        20: parent 1:20 ...") followed by an indented stats line and a
        `backlog <size> <packets>p` line. The handle is the tool's own
        (`<minor>0:`), so a batch's tools never read each other's queue.

        **The size carries a unit suffix and it must be parsed.** `tc` picks
        between `b` and `Kb` on its own, and the choice is not magnitude alone:
        measured on this host, a 43 MB backlog on a `delay`-only netem printed
        as `43260000b` while a 29 MB backlog on a `rate` netem printed as
        `28447Kb` (the same instant's `tc -s -j` says 29129713 bytes, i.e.
        `Kb` = KiB). A regex that accepts only the bare `b` therefore does not
        report a large backlog *wrongly* — it reports it as **no qdisc at
        all**, and `settle` reads that as "nothing to drain" and returns
        immediately. The drain is then a silent no-op at exactly the shaped
        rate transitions whose queued bulk it exists to wait out, which is
        where every dead spine of the 2026-09-28 sweeps was.
        """
        lines = self._tc_out("-s", "qdisc", "show", "dev", "lo").splitlines()
        for i, line in enumerate(lines):
            if not line.startswith("qdisc netem ") or f" {handle} " not in line + " ":
                continue
            for follow in lines[i + 1 : i + 4]:
                m = re.search(r"backlog\s+(\d+)([KMG]?b)\s+(\d+)p", follow)
                if m:
                    return int(m.group(1)) * BACKLOG_UNITS[m.group(2)]
        return None

    #: TCP states that can still put bytes on the path. The drain waits on
    #: these and no others, and the split is measured rather than read off a
    #: state diagram: after a `rate20` stage's bulk client is killed, its
    #: sockets sit in `FIN-WAIT-1`/`CLOSE-WAIT` while they retransmit the tens
    #: of MB their kernel still holds (measured: the dominant carrier of the
    #: whole tail), and then in `FIN-WAIT-2`/`CLOSING` for *minutes* while
    #: carrying nothing at all. Waiting on every state never ends; waiting on
    #: `established` alone misses the tail that does the damage.
    SEND_CAPABLE = ("ESTAB", "FIN-WAIT-1", "CLOSE-WAIT", "SYN-SENT", "SYN-RECV")

    #: The queue half of the drain predicate is a tolerance, not zero. The
    #: probes share the tool's class, so a few hundred bytes are always queued
    #: and `backlog == 0` is unreachable: measured with a 400 s budget, the
    #: queue settled at ~1.2 KB and *never* reached zero, so a drain waiting
    #: for zero can only ever end by burning its budget — which makes the whole
    #: transition timer-driven rather than condition-driven. One frame at
    #: `lo`'s MTU separates that floor from the tens of MB a killed bulk client
    #: leaves behind by three orders of magnitude.
    DRAIN_BACKLOG_TOLERANCE = 65536

    def _busy_sockets(self, band: dict) -> int:
        """Sockets on the throughput port that can still put bytes on the path.

        The qdisc can be empty *between* retransmissions of a killed client's
        FIN, so an empty queue is not the same as a quiet stage: the teardown
        of the previous stage's bulk lands on whatever the next stage dials.
        This is the second half of the drain predicate.

        It counts `SEND_CAPABLE` states, and the two ways of getting that wrong
        were both measured. `established` alone (what the frozen-commit sweep
        ran with) reads **0** from ~t+20 s while the killed client's
        `FIN-WAIT-1` sockets are still retransmitting megabytes — the drain
        then declares the path quiet while it is anything but. Every non-LISTEN
        state makes the predicate unsatisfiable instead, because
        `FIN-WAIT-2`/`CLOSING` linger for minutes after a kill and carry
        nothing. Neither extreme is a predicate; this is the subset that can
        still send.
        """
        port = band["iperf_exposed"]
        counts = lib.tcp_state_counts({"p": port}).get("p", {})
        return sum(n for s, n in counts.items() if s in self.SEND_CAPABLE)

    def settle(self, cid: str, budget: float) -> float:
        """Wait out a stage's in-flight bulk before the next stage reshapes it.

        The stage boundary kills the bulk client, and a killed TCP socket does
        not discard what its kernel side still holds: `close()` leaves the
        remaining bytes to be delivered in the background. Switching the qdisc
        to a slower shaper at that instant puts that drain and the *next*
        stage's handshake into the same queue, and a SYN dropped behind
        megabytes of retransmitted bulk costs the next stage its first tens of
        seconds. Measured with no proxy in the path at all (htb + netem on
        `lo`, exactly this harness's shape, 20 bulk streams killed as the
        qdisc changed from `rate100` to `rate20`): a fresh connect timed out
        after 10.5 s and the next round trip took 3.5-6.7 s, settling at the
        steady-state 161 ms only ~15 s in. Draining *at the old shaper* for
        10 s before the switch makes the very first round trip 164 ms.

        That artifact is why the `rate20` cell read "spine produced no
        intervals": the stall is the shaper's, not the tool's. Waiting here
        (the previous stage's qdisc is still installed until `apply`) removes
        it from the measurement instead of attributing it to the tool.

        The predicate is two-part, because an empty queue is not a quiet
        stage: the qdisc drains *between* retransmissions of a killed client's
        FIN, so the previous stage's bulk connection is still tearing down
        ~10 s later. Measured on `rate100:120,rate20:120`: with only the
        queue check the drain finished in 1.0-1.8 s and the bulk spine still
        died reporting `control socket has closed unexpectedly` — the server
        dropped the new visitor when the dying channel it had been paired with
        ended. Waiting for the bulk port's own connections to reach zero as
        well (`_busy_sockets`) is what makes the next stage start from a quiet
        path.

        Both halves of that predicate were re-derived on 2026-09-28 and both
        went back to what the frozen-commit sweep ran with: see
        `_busy_sockets`, which records the three variants and which of them
        measurably failed.

        Returns a `DrainReport`: the seconds spent (so the cost is visible in
        the results log) plus what the path looked like when the wait ended.
        `budget` is a **safety net, not the mechanism**: the wait ends when the
        path is quiet by the predicate below, and a budget that fires is
        reported rather than silently absorbed.
        """
        minor = cid.split(":")[1]
        handle = f"{minor}0:"
        band = next(b for c, b in self.classes if c == cid)
        started = time.time()
        deadline = started + budget
        quiet = 0
        backlog = None
        live = 0
        polled = False
        while time.time() < deadline:
            backlog = self._backlog(handle)
            live = self._busy_sockets(band)
            polled = True
            # Quiet means "no more than the probes' own share is queued AND
            # nothing on the throughput port can still send". Both halves are
            # measurements of the same question -- is the previous stage still
            # putting bytes on this path -- and both have to be decidable, or
            # the transition is timer-driven and the next stage's start state
            # is whatever the clock happened to allow.
            queued = backlog is not None and backlog > self.DRAIN_BACKLOG_TOLERANCE
            if queued or live:
                quiet = 0
            else:
                quiet += 1
                if quiet >= self.DRAIN_QUIET_POLLS:
                    return DrainReport(time.time() - started, False, backlog, live)
            time.sleep(0.25)
        # A budget of zero leaves the loop unentered, so the report would
        # otherwise claim an unshaped path (`final_backlog: None`) for a
        # shaped one. Read once so the two cases stay distinguishable.
        if not polled:
            backlog = self._backlog(handle)
            live = self._busy_sockets(band)
        self.log(
            f"    {cid} drain budget of {budget:.0f}s expired with the path "
            f"still busy (backlog={backlog}, bulk sockets={live}); "
            f"the next stage starts anyway"
        )
        return DrainReport(time.time() - started, True, backlog, live)

    #: The two ports the shaped set carries that a transition can leave
    #: sockets on: the exposed port the drain already watches, and the backend
    #: leg it does not. The echo/udp ports are deliberately absent -- the
    #: interactive and churn probes dial them continuously, so a snapshot of
    #: them is never quiet and says nothing about the transition.
    SNAPSHOT_PORTS = ("iperf_exposed", "iperf_backend")

    #: The ports the *path* is applied to, in two groups. `visitor` is the
    #: workload's own access link — the exposed side the probes and the bulk
    #: client dial — plus `kcp_bind`, the one tunnel the harness can name on
    #: the wire (a plain multiplexed tunnel's ports are ephemeral, so it stays
    #: unshaped whatever this knob says). `backend` is the tool's own LAN side,
    #: the leg between the client process and the backend it forwards to.
    #:
    #: Shaping both is what the model did until now, and it is not the same
    #: measurement as shaping one: the two legs share a single HTB class, so a
    #: 100 Mbit `rate100` class carried 100 Mbit *in total* — about 42 Mbit
    #: end to end, measured on all four tools — and every injected delay was
    #: paid twice (a `rtt100` stage's interactive floor is 802 ms, exactly the
    #: four 100 ms delays of a fresh connection whose handshake and request
    #: both cross both legs). `visitor` applies the class once, which is what
    #: the stage table in docs/benchmarks.md says it does.
    PATH_PORTS = ("iperf_exposed", "echo_exposed", "udp_exposed", "kcp_bind")
    BACKEND_PORTS = ("iperf_backend", "echo_backend", "udp_backend")
    #: The accepted `SOAK_SHAPE_LEGS` values. `visitor` is the default since
    #: the 2026-09-29 A/B (HANDOFF.md, "Shaping scope"): measured on one host,
    #: one method, two configurations of the same build, it halves every
    #: injected delay, raises the rate cells from ~42% of nominal to the
    #: nominal rate, and cuts the `rate20 -> jitter` transition from a 159 s
    #: flush to seconds. `both` stays selectable because every stored result
    #: before that date was measured with it.
    LEGS = lib.Knobs.SHAPE_LEGS

    def socket_snapshot(self, band: dict) -> dict:
        """Per-port TCP socket state counts, for the transition diagnostics.

        `_busy_sockets` answers one yes/no question about one port. This
        answers the shape question the drain predicate cannot: what is left
        standing on *both* throughput legs when the next stage starts. It
        returns `{label: {state: count}}`, or `{}` when `ss` is unavailable or
        sees nothing -- it never raises, because it records evidence beside a
        measurement rather than gating one.
        """
        return lib.tcp_state_counts({k: band[k] for k in self.SNAPSHOT_PORTS})

    def _ports(self, band: dict) -> list:
        # The data-plane ports only: the TOOL's control channel stays in
        # the unshaped default class. A capacity measurement that shapes
        # the control plane kills the tool's heartbeat (measured: 40 s
        # timeout on a 100 mbit cell) and the run becomes a wedge study
        # instead of a capacity study.
        keys = self.PATH_PORTS
        if self.legs == "both":
            keys += self.BACKEND_PORTS
        return [band[k] for k in keys]

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
        #: What each test type this tool can run actually records. The claim is
        #: per test, not per tool: a capacity ramp drives its load through
        #: `iperf_burst`, which reports one sample per load level
        #: (`metrics.curve`) instead of the staged run's per-interval
        #: `throughput_bulk_gbps` series, so a capacity entry that claimed
        #: `tcp_bulk` was describing a series it never writes — and the
        #: completeness gate failed a run that was complete.
        self.coverage_by_test = {
            "capacity": {
                "tcp_interactive": True,
                "tcp_churn": True,
                "udp_session": True,
            },
        }
        self.coverage = {
            "tcp_bulk": True,
            "tcp_interactive": True,
            "tcp_churn": True,
            "udp_session": True,
        }

    def coverage_for(self, test: str) -> dict:
        """The axes one test type's entry claims (the staged set by default)."""
        return self.coverage_by_test.get(test, self.coverage)

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

    def use_variant(self, variant: str) -> None:
        """Point the tool at another configuration variant.

        The screen's variant A/B (`--ab-variants`) changes `self.variant`
        *before* the restart, because the restart is what rewrites the
        config from it. Label and variant move together so the two arms'
        logs and tags cannot land in each other's files.
        """
        self.variant = variant
        self.label = f"{self.name} ({variant})" if variant else self.name

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
elif mode == "slow":
    # The M7 slow visitor: ONE connection to the tool's echo service, held
    # open for `interval` seconds and read back at `rate` bit/s. A writer
    # thread offers the request body; the main thread consumes the echoed
    # response no faster than the knob allows. Reading slowly IS the
    # instrument -- the return path's buffer fills, the tool has to stop
    # draining this one stream, and whether that costs the streams sharing
    # its pool is what the stage's interactive p99 measures. The probe runs
    # in its own process for the reason documented above PROBE_SRC: a
    # throttled reader sharing the harness's GIL would put the instrument
    # into the path it is measuring.
    budget = interval
    rate = float(sys.argv[5])
    chunk = 4096
    pace_s = chunk * 8.0 / rate
    done = threading.Event()

    def offer(sock):
        payload = b"v" * 65536
        while not done.is_set():
            try:
                sock.sendall(payload)
            except OSError:
                return

    total = 0
    try:
        # The connect is inside the guard on purpose: a refused or timed-out
        # connect is this probe's likeliest failure, and it has to leave the
        # same typed reason as a failed read -- the harness records that line
        # in the stage instead of a bare null.
        s = socket.create_connection(("127.0.0.1", port), timeout=5.0)
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.settimeout(max(5.0, pace_s * 4.0))
        threading.Thread(target=offer, args=(s,), daemon=True).start()
        t_start = time.perf_counter()
        while time.perf_counter() - t_start < budget:
            t_cycle = time.perf_counter()
            data = s.recv(chunk)
            if not data:
                raise ConnectionError("closed by the tool")
            total += len(data)
            # One sample per paced read: the denominator is the whole cycle
            # (read + deliberate delay), so the rate sits at the knob's
            # unless the path stalled the visitor -- which is the one thing
            # this series exists to show.
            time.sleep(max(0.0, pace_s - (time.perf_counter() - t_cycle)))
            paced_s = time.perf_counter() - t_cycle
            # kbit/s, not Gbit/s: a throttled visitor is orders of magnitude
            # below the bulk axis, and Gbit/s with three decimals rounds the
            # whole reading to 0.0.
            emit("slow_visitor_read_kbps",
                 round(len(data) * 8 / max(paced_s, 1e-9) / 1e3, 3))
            emit("slow_visitor_bytes", total)
        emit("slow_visitor_done", 1)
    except Exception as e:
        sys.stderr.write(f"slow-visitor: {type(e).__name__}: {e}\\n")
        sys.stderr.flush()
        emit("slow_visitor_error", 1)
        sys.exit(1)
    finally:
        done.set()

"""


@dataclass
class SlowVisitor:
    """One stage's slow-visitor probe and what the stage must record.

    `t0` bounds the samples that belong to this visitor: the series is
    shared with the long-lived probes, and a stage's stats are derived from
    a slice of it.
    """

    proc: subprocess.Popen
    reader: threading.Thread
    log: Path
    t0: float


def _probe_reason(path: Path) -> str:
    """The probe's own last typed line, for the stage line's failure reason.

    The child writes `slow-visitor: <ExceptionType>: <message>` on the way
    out. Recording that line is what keeps a failed stage diagnosable
    instead of a bare null (AGENTS.md §10, "every failure leaves evidence").
    """
    with contextlib.suppress(OSError):
        lines = [ln.strip() for ln in path.read_text().splitlines() if ln.strip()]
        if lines:
            return lines[-1].removeprefix("slow-visitor: ")[:200]
    return ""


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
        # The M7 slow visitor is restarted per stage, so it is tracked apart
        # from the three long-lived probes; `slow_error` carries a spawn
        # failure to the stage line that would otherwise claim it ran.
        self.slow: SlowVisitor | None = None
        self.slow_error = ""

    def log_path(self, mode: str) -> Path:
        return Path(self.work) / f"probe-{mode}-{self.band['echo_exposed']}.log"

    def _spawn(
        self,
        mode: str,
        port: int,
        interval: float,
        backend_port: int = 0,
        rate: float = 0.0,
    ) -> tuple:
        """Start one probe process; returns `(proc, reader, log_path)`.

        `interval` is the mode's rate knob (ping interval, or the slow
        visitor's observation budget in seconds), and `rate` its rate knob
        (the churn connector's connects/s, or the slow visitor's bit/s).
        """
        log_path = self.log_path(mode)
        with log_path.open("w") as errlog:
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
        return proc, th, log_path

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

    # --- the M7 slow visitor (opt-in; one fresh process per stage/step) -----
    def start_slow_visitor(self, secs: float) -> None:
        """Offer one slow-reading visitor for the next `secs` seconds.

        A no-op unless `SOAK_SLOW_VISITOR_BPS` is set. One process per
        stage, like every other external tool here: a wedged visitor must
        not become the next stage's sample (AGENTS.md §10), and the stage
        boundary is where its outcome is recorded. The visitor dials the
        tool's exposed echo port — the path under test — and its backend is
        the one the interactive probe already serves, so no new service or
        backend binary appears in the tool's configuration.
        """
        self.finish_slow_visitor(None)
        if not self.knobs.slow_visitor_bps:
            return
        try:
            proc, th, log_path = self._spawn(
                "slow",
                self.band["echo_exposed"],
                secs,
                backend_port=self.band["echo_backend"],
                rate=float(self.knobs.slow_visitor_bps),
            )
        except OSError as e:
            self.slow_error = f"probe failed to start: {type(e).__name__}: {e}"
            return
        self.slow = SlowVisitor(proc=proc, reader=th, log=log_path, t0=time.time())
        self.slow_error = ""

    def finish_slow_visitor(self, target: dict | None) -> None:
        """Record the slow visitor's outcome for the stage that just ended.

        `target` is the stage (or screen arm) dictionary the rate, the byte
        count and the state are written into; `None` on the pre-restart call
        where the previous visitor is only reaped. Every exit path records a
        state and a typed reason — a stage that had a visitor and no line
        about it would be a bare null.
        """
        sv, self.slow = self.slow, None
        if sv is None:
            if target is not None and self.slow_error:
                target["slow_visitor_state"] = "failed"
                target["slow_visitor_reason"] = self.slow_error
            self.slow_error = ""
            return
        killed = False
        try:
            code = sv.proc.wait(timeout=lib.SLOW_VISITOR_JOIN_S)
        except subprocess.TimeoutExpired:
            killed = True
            with contextlib.suppress(OSError):
                sv.proc.kill()
            with contextlib.suppress(subprocess.TimeoutExpired):
                sv.proc.wait(timeout=5)
            code = sv.proc.returncode
        sv.reader.join(timeout=2)
        window = [
            r
            for r in self.out
            if r["t"] >= sv.t0 and str(r.get("metric", "")).startswith("slow_visitor_")
        ]
        rates = lib.series_stats(window, "slow_visitor_read_kbps")
        errors = lib.series_stats(window, "slow_visitor_error").get("n", 0)
        done = lib.series_stats(window, "slow_visitor_done").get("n", 0)
        last_bytes = [r["v"] for r in window if r["metric"] == "slow_visitor_bytes"]
        if errors:
            state = "failed"
            reason = _probe_reason(sv.log) or "probe reported an error without a line"
        elif killed:
            state = "killed"
            reason = f"still running {lib.SLOW_VISITOR_JOIN_S:.0f}s after the stage"
        elif code == 0 and done:
            state, reason = "completed", ""
        elif code == 0:
            state, reason = "failed", "exited 0 without a completion sample"
        else:
            state = "failed"
            reason = _probe_reason(sv.log) or f"exited {code} without a typed error"
        if target is not None:
            target["slow_visitor_state"] = state
            target["slow_visitor_read_kbps"] = rates.get("mean")
            target["slow_visitor_samples"] = rates.get("n", 0)
            target["slow_visitor_bytes"] = last_bytes[-1] if last_bytes else 0
            if reason:
                target["slow_visitor_reason"] = reason
        log(
            f"    slow visitor: {state}"
            + (f" ({reason})" if reason else "")
            + f", read {rates.get('mean') or 0} kbit/s over "
            f"{rates.get('n', 0)} sample(s), "
            f"{last_bytes[-1] if last_bytes else 0} bytes"
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
            for label, pid in zip(("server", "client"), self.pids_of(), strict=True):
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
            with Path(f"/proc/{pid}/statm").open() as fh:
                return int(fh.read().split()[1]) * (os.sysconf("SC_PAGE_SIZE") // 1024)
        except (OSError, ValueError, IndexError):
            return None

    @staticmethod
    def fds(pid: int):
        try:
            return len(list(Path(f"/proc/{pid}/fd").iterdir()))
        except OSError:
            return None

    @staticmethod
    def thread_count(pid: int):
        try:
            return len(list(Path(f"/proc/{pid}/task").iterdir()))
        except OSError:
            return None

    @staticmethod
    def cpu_ticks(pid: int):
        try:
            with Path(f"/proc/{pid}/stat").open() as fh:
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
            for label, pid in zip(("server", "client"), self.pids_of(), strict=True):
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
    runners take `(tool, ctx, entry)` and nothing else. `pingers` follows the
    same rule: it owns the M7 slow visitor's per-stage lifecycle, and a test
    type that cannot see it could not restart that visitor per stage.
    """

    args: argparse.Namespace
    knobs: lib.Knobs
    timeline: list
    shaper: "Shaper"
    #: The test type in flight. One run can carry several (a release sweep runs
    #: `rrul,capacity`), so a runner that read `args.test` would measure the
    #: first type's load for every type after it.
    test: str = ""
    backends: lib.Backends | None = None
    pingers: "Pingers | None" = None
    load: int = 0

    def with_backends(self, backends: lib.Backends) -> "RunContext":
        self.backends = backends
        return self

    def with_pingers(self, pingers: "Pingers") -> "RunContext":
        self.pingers = pingers
        return self

    def backend(self) -> lib.Backends:
        """The live backend set.

        Every runner is built through `with_backends`, so the contract is
        checked here once instead of dereferencing an Optional at each call
        site (the fields are Optional only so the builder can be chained, and
        a missing one is a harness bug, not a measurement).
        """
        if self.backends is None:
            raise RuntimeError("RunContext used before with_backends")
        return self.backends

    def pinger(self) -> "Pingers":
        """The live pinger set, on the same terms as [`backend`]."""
        if self.pingers is None:
            raise RuntimeError("RunContext used before with_pingers")
        return self.pingers

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


#: The shape of an `iperf3 -w` value: a byte count or a K/M/G suffix. Checked
#: so a typo is refused at startup rather than aborting every rate stage.
_SOCKET_WINDOW_RE = re.compile(r"^\d+[KMG]?$")

#: The shortest bulk dial worth starting: below this a stage cannot carry the
#: floor the completeness gate asks for (`min_bulk_intervals`), so a retry
#: that could not leave this much of the stage is not attempted at all.
MIN_SPINE_SECS = 5

#: How long past the stage boundary the spine keeps reading for the client's
#: own summary. `-t` is sized to the stage, so a dial that is making progress
#: ends right at the boundary and emits its `end` event a moment later — and
#: that event is the *only* place the receiver's own window is reported. The
#: sender's interval accounting is provably defeated on a shaped stage (its
#: writes all complete inside the `-O` warm-up and the measured intervals then
#: read zero while the receiver keeps draining), so this is the half of §10's
#: "state one convention, with the receiver's own window beside it" that a
#: streaming client cannot otherwise provide. A dial that is *stuck* does not
#: get the grace: it is killed at the deadline and the stage records that its
#: reading has no receiver half.
SPINE_SUMMARY_GRACE_S = 5.0


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

    A *fresh* server is started for every stage, before the first attempt
    and not only after a failed one. Measured on the `rate100:120,rate20:120`
    reproducer: the previous stage's killed 20-stream client finishes tearing
    down ~10 s into the next stage (its FINs are retransmitted through the
    newly-lowered shaper), and the single-test server answers a connection
    aborted in that window with `Bad file descriptor` — after which every
    later dial hangs. Restarting first means the teardown lands on a process
    nobody will dial again, which is AGENTS.md §10's "one failure must not
    poison the next sample" applied to a single-test external tool. The retry
    below stays as the second line of defence.

    Everything the transition left standing is recorded rather than judged:
    `spine_sockets` (both throughput legs, immediately after the restart and
    before the first dial), `backend_restart` (what the backend port held as
    the fresh server was bound) and `first_interval_s` (how far into the
    stage bulk actually started). Which of those correlates with a dead spine
    is what the diagnostics are for.

    The dial is retried at `Knobs.spine_retry_s` because the drain cannot
    always win: measured on the `rate20 -> jitter` transition, the previous
    stage's killed client leaves ~33 MB that retransmits for ~106 s at that
    stage's own 20 Mbit/s, so the first dial lands on a busy path and dies
    with `control socket has closed unexpectedly` — and a later dial, into the
    same stage, carries its intervals (37 measured, at t+50 s). A retry is
    **not** free of meaning and is not hidden: the stage record carries
    `spine_attempts` and `first_interval_s`, so a recovered stage reads as one
    that needed two dials rather than as a clean one.
    """
    target = lib.ThroughputTarget.from_band(tool.band)
    t_start = time.time()
    t_end = t_start + stage.secs
    backend = ctx.backend()
    with contextlib.suppress(Exception):
        backend.restart_iperf()
    # Immediately after the restart and before the first dial: this is the
    # state the next visitor's connection is about to be opened into.
    spine_sockets = ctx.shaper.socket_snapshot(tool.band)
    gaps = tuple(ctx.knobs.spine_retry_s)
    outcome = {}
    attempts = 0
    for i, gap in enumerate(gaps):
        if t_start + gap + MIN_SPINE_SECS > t_end:
            break  # no room left for a dial that could carry its floor
        if i:
            # Restart before every retry too: the attempt that just failed may
            # have left the single-test server wedged.
            with contextlib.suppress(Exception):
                backend.restart_iperf()
        if time.time() < t_start + gap:
            time.sleep(t_start + gap - time.time())
        attempts += 1
        remaining = max(MIN_SPINE_SECS, int(t_end - time.time()))
        cmd = ["iperf3", "-c", "127.0.0.1", "-p", str(target.exposed)]
        # A rate class is the one place the sender cannot account for what the
        # path carried, so it is the one place the instrument bounds the
        # sender's buffer (`rate_socket_window`); every other class is measured
        # with the client's own window, as it always was.
        if ctx.knobs.rate_socket_window and stage_is_rate_limited(stage.path):
            cmd += ["-w", ctx.knobs.rate_socket_window]
        cmd += [
            "-t",
            str(remaining),
            "-O",
            "2",
            "-P",
            str(ctx.load),
            "-i",
            "1",
            "--json-stream",
        ]
        # A dial that has carried nothing by the next gap is stuck, and a
        # stuck dial must not eat the retry it exists to leave room for. The
        # last dial is bounded by the stage instead.
        give_up_after = t_start + gaps[i + 1] if i + 1 < len(gaps) else None
        outcome = _spine_once(cmd, entry, t_end, give_up_after)
        if outcome["intervals"]:
            break
    first = outcome.get("first_interval_at")
    outcome["attempts"] = attempts
    outcome["first_interval_s"] = (
        round(first - t_start, 3) if first is not None else None
    )
    outcome["spine_sockets"] = spine_sockets
    # The *last* restart's report: when the first attempt failed, the second
    # restart is the one that ran against the dying connection.
    outcome["backend_restart"] = backend.restart_report
    if outcome["intervals"]:
        return outcome
    # the stage's full duration elapses regardless: a dead spine must not
    # cut the probes' and samplers' window short
    while time.time() < t_end:
        time.sleep(min(1.0, max(0.1, t_end - time.time())))
    return outcome


def _kill_when_stalled(
    proc, got_interval: threading.Event, give_up_after: float, stop: threading.Event
) -> None:
    """Watchdog body: end a dial that has carried nothing by `give_up_after`.

    Split out of `_spine_once` (which reads one blocking line at a time, so it
    cannot poll the clock itself) and out of its complexity budget.
    """
    while not stop.wait(0.5):
        if not got_interval.is_set() and time.time() > give_up_after:
            with contextlib.suppress(OSError):
                proc.kill()
            return


def _spine_once(
    cmd: list,
    entry: dict,
    t_end: float,
    give_up_after: float | None = None,
    summary_grace: float = SPINE_SUMMARY_GRACE_S,
) -> dict:
    """One iperf3 client attempt for a stage's bulk load.

    The client's own diagnosis is returned as `client_error`. iperf3 with
    `--json-stream` reports a failure as an `error` event on stdout, and the
    interval loop below skips everything that is not an interval — so before
    this the harness recorded "exit 1" and threw away the reason, which is
    exactly the bare failure AGENTS.md §10 forbids.

    `first_interval_at` is when this attempt's first counted interval landed,
    or `None` when it carried none. Whether a spine that dies was ever alive
    is the difference between "the dial never completed" and "the transfer
    started and was cut", and the two have different causes.

    `give_up_after` abandons a dial that has carried nothing by then. Reading
    the client's stream *blocks*, so an attempt that never answers would hold
    the stage to its end (measured: a dead spine recorded `exit -9`, the
    harness killing it at `t_end`) and the retry the stage still had time for
    would never happen. A watchdog thread ends it instead; a dial that has
    already carried an interval is never abandoned, because it is a live
    sample and may still recover.

    Past the stage boundary the loop keeps reading for `summary_grace` seconds
    for the client's `end` event, which carries `sum_sent` and `sum_received`
    — the receiver's own window. **No interval is recorded in that window**:
    the stage's measurement is closed at `t_end`, and the grace exists only to
    collect the summary. `summary` says which of the two outcomes happened
    (`"end"`, or `"truncated"` when the client had to be killed without one),
    because "the receiver carried this" and "nobody asked the receiver" must
    not read the same in a results file.
    """
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, bufsize=1)
    seen = _SpineRead()
    got_interval = threading.Event()
    stop = threading.Event()
    if give_up_after is not None:
        threading.Thread(
            target=_kill_when_stalled,
            args=(proc, got_interval, give_up_after, stop),
            daemon=True,
        ).start()
    try:
        _read_spine_window(proc, entry, t_end, seen, got_interval)
        # The stage window is closed; read on for the summary only.
        _read_spine_summary(proc, seen, summary_grace)
    finally:
        stop.set()
        proc.kill()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)
    return {
        "intervals": seen.intervals,
        "exit": proc.returncode,
        "client_error": seen.client_error,
        "first_interval_at": seen.first_interval_at,
        "summary": _spine_summary(seen.summary),
    }


@dataclass
class _SpineRead:
    """What one dial's stdout produced, so the two read phases share it.

    Split from `_spine_once` because the function had grown past the
    complexity budget: the stage-window loop and the summary loop both parse
    the same stream and both must record the client's own error event.
    """

    intervals: int = 0
    client_error: str = ""
    first_interval_at: float | None = None
    summary: dict = dataclass_field(default_factory=dict)


def _read_spine_window(
    proc, entry: dict, t_end: float, seen: "_SpineRead", got
) -> None:
    """Read the dial's intervals until the stage's measurement window closes.

    `span_s` is the interval's own length. It is recorded because a jammed
    tunnel makes iperf3 emit one wide catch-up interval whose average describes
    a much longer window than the nominal 1 s — a reader (and the chart) has to
    be able to tell it apart from a normal sample.
    """
    while time.time() < t_end and proc.poll() is None:
        line = proc.stdout.readline()
        if not line:
            break
        with contextlib.suppress(ValueError):
            d = json.loads(line)
            if d.get("event") != "interval":
                if d.get("event") == "error" or d.get("error"):
                    seen.client_error = str(d.get("data") or d.get("error"))[:200]
                continue
            s = (d.get("data") or {}).get("sum") or {}
            if not s or s.get("omitted"):
                continue
            now = round(time.time(), 3)
            seen.intervals += 1
            if seen.first_interval_at is None:
                seen.first_interval_at = now
                got.set()
            entry["series"].append(
                {
                    "t": now,
                    "metric": "throughput_bulk_gbps",
                    "v": round(s.get("bits_per_second", 0) / 1e9, 4),
                    "span_s": round(
                        max(0.0, s.get("end", 0.0) - s.get("start", 0.0)), 3
                    ),
                }
            )
            entry["series"].append(
                {"t": now, "metric": "bulk_retransmits", "v": s.get("retransmits", 0)}
            )


def _read_spine_summary(proc, seen: "_SpineRead", grace: float) -> None:
    """Read past the stage for the `end` event, recording nothing else.

    The stage's window is closed at its boundary, so an interval that arrives
    in the grace period is deliberately *not* appended: it belongs to no stage,
    and the drain that follows would otherwise let it land inside the next
    stage's window.

    **The loop does not test `proc.poll()`.** A client that exits exactly at the
    boundary — which is what `-t` is sized for — leaves its whole summary in the
    pipe, and a `poll()` guard skips reading it: that is measured, not
    theoretical, and it made the receiver's window appear in some runs of a cell
    and not others (the same rate stage reported `0.0995` or `0.0902` Gbit/s
    depending on which side of the race the harness landed on). Reading to EOF
    instead is always safe: an exited process's pipe yields what it wrote and
    then an empty read.
    """
    deadline = time.time() + grace
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            break
        with contextlib.suppress(ValueError):
            d = json.loads(line)
            if d.get("event") == "end":
                seen.summary = d.get("data") or {}
                break
            if d.get("event") == "error" or d.get("error"):
                seen.client_error = str(d.get("data") or d.get("error"))[:200]


def _spine_summary(end: dict) -> dict:
    """The receiver's own window out of one dial's `end` event, typed.

    `iperf_result` established the two conventions this reuses rather than
    reinvents (AGENTS.md §10): the measured window is the side's own
    `seconds`, never the configured duration; and a sender whose warm-up
    swallowed every write is `sender_accounting_degenerate`, at which point
    the receiver's count is the only evidence of what the path carried.

    An `end` event without `sum_received` (a killed client, an old server) is
    recorded as exactly that: `{"state": "no receiver summary"}`, never as a
    zero, which would read as "the receiver got nothing".
    """
    if not end:
        return {"state": "truncated"}
    sent = end.get("sum_sent") or {}
    recv = end.get("sum_received") or {}
    if not recv:
        return {"state": "no receiver summary"}
    recv_bytes = recv.get("bytes", 0)
    sent_bytes = sent.get("bytes", 0)
    return {
        "state": "end",
        "sender_gbps": round(sent.get("bits_per_second", 0.0) / 1e9, 4),
        "sender_window_s": round(sent.get("seconds", 0.0), 3),
        "sender_bytes": sent_bytes,
        "receiver_gbps": round(recv.get("bits_per_second", 0.0) / 1e9, 4),
        "receiver_window_s": round(recv.get("seconds", 0.0), 3),
        "receiver_bytes": recv_bytes,
        "sender_accounting_degenerate": bool(
            recv_bytes and sent_bytes * 2 < recv_bytes
        ),
    }


def bulk_window_stats(rows: list) -> dict:
    """A stage's bulk load over its whole window, not its best second.

    The peak 1 s interval was the model's headline and it is the wrong
    statistic for a shaped stage: netem holds a burst's bytes past the
    interval that produced them, so the peak is whichever interval the shaper
    happened to release into, and the median interval is zero. The
    span-weighted mean over the stage is the *longer accounting window* the
    shaped cells need — every interval counts for the time it actually
    covered, including the zero-byte ones, so a stage that carried nothing
    reads as nothing instead of as its one lucky burst.

    Pure: reads the series rows, returns numbers. The caller decides which of
    them a cell is allowed to print.
    """
    rates, spans = [], []
    for r in rows:
        if r.get("metric") != "throughput_bulk_gbps":
            continue
        rates.append(float(r.get("v", 0.0)))
        spans.append(max(0.0, float(r.get("span_s", 1.0) or 1.0)))
    if not rates:
        return {}
    total_span = sum(spans)
    weighted = sum(v * s for v, s in zip(rates, spans, strict=True))
    return {
        "bulk_intervals": len(rates),
        "bulk_span_s": round(total_span, 3),
        "bulk_peak_gbps": round(max(rates), 4),
        "bulk_window_gbps": round(weighted / total_span, 4) if total_span else None,
        "bulk_zero_share": round(
            sum(1 for v in rates if v == 0) / len(rates),
            4,
        ),
    }


def stage_is_rate_limited(stage: str) -> bool:
    """Whether a stage class puts a rate shaper in front of the sender.

    Read off the class's own netem arguments, so the classification cannot
    drift from what the shaper applies. A rate shaper is what defeats the
    sender's interval accounting: the client's writes complete into a socket
    buffer far larger than the shaped path can drain, and every measured
    interval then reads zero bytes while the path keeps carrying them — at
    `rate20` the client was still blocked **30 s past the stage boundary**, so
    its own summary never arrives at all.
    """
    return "rate" in PATH_CLASSES.get(stage, PathClass([])).netem


#: At or above this share of zero-byte intervals the sender's own accounting is
#: defeated: the writes completed into a socket buffer the shaped or congested
#: path cannot drain, and the measured intervals read zero while the path keeps
#: carrying them. It is a *withholding* threshold, never a selector — a cell at
#: or above it is a cell the sender cannot speak for.
BULK_ZERO_SHARE_DEGENERATE = 0.5


def bulk_reading(st: dict) -> tuple:
    """A stage's published bulk reading: `(gbps, source)`, or `(None, why not)`.

    One convention, applied to every stage (AGENTS.md §10): the sender's bytes
    over the measured window, with the receiver's own window beside it — and
    the receiver's count becomes the *reading* only when the sender's
    accounting is provably defeated, which a shaped stage does by design (all
    of the client's writes complete inside the `-O` warm-up, then the measured
    intervals read zero while the path keeps draining).

    The reading is the span-weighted window mean and **not** the peak
    interval. The peak is the statistic that made a shaped cell unquotable:
    netem releases a burst into whichever second it likes, so the peak is a
    property of the shaper's schedule, while the mean over the stage is what
    the path carried.

    A stage with no reading says why. The three reasons are different facts
    and are not collapsed: a dead spine carried nothing, a truncated dial has
    no receiver half, and a stage whose sender accounting is degenerate but
    whose receiver never reported has no evidence at all.
    """
    if not st.get("bulk_intervals"):
        return None, "no bulk intervals"
    zero_share = st.get("bulk_zero_share") or 0.0
    # The *measurement* decides which side speaks, never the class: the sender's
    # accounting is defeated when its writes did not track the path, and that is
    # what the zero-byte share reads (with the `end` event's own flag as
    # corroboration). A rate class used to be treated as defeated by
    # construction — true for a client whose socket buffer absorbs the whole
    # stage, which is what a rate shaper does to an unbounded sender — but the
    # rate classes now bound the client's window (`rate_socket_window`), and
    # with it their zero-share reads 0-14 % instead of 72-100 %. A class rule
    # would then withhold a reading the instrument can now take; the threshold
    # stays as the rule that *withholds* one (at or over it, the sender wrote
    # nothing it could account for, and the cell says so).
    degenerate = bool(st.get("sender_accounting_degenerate")) or (
        zero_share >= BULK_ZERO_SHARE_DEGENERATE
    )
    if degenerate:
        recv = st.get("receiver_gbps")
        if recv is None:
            return None, (
                f"the sender's interval accounting is defeated "
                f"({100 * zero_share:.0f}% zero-byte intervals) and the dial "
                f"produced no receiver summary "
                f"({st.get('spine_summary') or 'no summary'})"
            )
        return recv, "receiver's own window"
    return st.get("bulk_window_gbps"), "sender's window mean"


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
    entry["stages"][-1] |= bulk_window_stats(window)
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
        # One visitor per ramp step: the load level is this test's stage, so
        # the slow visitor's line belongs beside that level's p99.
        ctx.pinger().start_slow_visitor(knobs.settle_s)
        r = ctx.backend().iperf_burst(
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
        point = {
            "streams": streams,
            "gbps": r.get("gbps_headline"),
            "rtt_p99": p99,
            "rtt_mean": st.get("mean"),
            "rtt_n": st.get("n"),
            "rtt_error_rate": round(err_rate, 5),
            "slo_broken": broken,
            "reason": "; ".join(x for x in reasons if x) or None,
        }
        ctx.pinger().finish_slow_visitor(point)
        entry["metrics"].setdefault("curve", []).append(point)
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
    ctx.load = ctx.load_for(ctx.test)
    entry["metrics"]["bulk_streams"] = ctx.load
    for stage in ctx.timeline:
        mark = run_one_stage(tool, ctx, entry, stage)
        if ctx.test == "cost":
            record_cost(entry, mark, ctx.load)


def run_one_stage(tool: Tool, ctx: RunContext, entry: dict, stage: Stage) -> int:
    """Shape one stage, run its bulk spine, record the stage's stats.

    The M7 slow visitor is started with the stage and closed with it, so
    each stage's interactive p99 is measured against a visitor that is
    alive for exactly that window, and a visitor that fails is one stage's
    recorded state rather than a poisoned axis (AGENTS.md §10).

    A stage after the first drains the previous stage's in-flight bulk
    *before* the qdisc changes (`Shaper.settle`): the stage boundary kills
    the bulk client, and reshaping under a still-draining socket puts the
    next stage's handshake behind the old stage's bytes. The drain is not
    part of the stage's window — `mark` is taken after it, so its probe
    samples belong to no stage. What the drain cost and whether it actually
    reached a quiet path is written into the stage it precedes (`drain_*`),
    because a transition that never went quiet is the state the spine was
    then measured in.

    The spine's own transition evidence (`spine_*`, `backend_restart`) is
    recorded whether or not it carried intervals: a dead stage and a stage
    that needed two dials are both facts about the transition, and the
    failure path already carries `bulk_error` beside them.
    """
    stage_rec = {"stage": stage.path, "secs": stage.secs}
    if entry["stages"]:
        drain = ctx.shaper.settle(tool.cid, ctx.knobs.stage_drain_budget)
        log(f"    drained the previous stage in {drain.seconds:.1f}s")
        stage_rec |= {
            "drain_s": round(drain.seconds, 3),
            "drain_expired": drain.expired,
            "drain_final_backlog": drain.final_backlog,
            "drain_busy_sockets": drain.busy_sockets,
        }
    ctx.shaper.apply(tool.cid, stage.path)
    mark = len(entry["series"])
    stage_rec["t_start"] = round(time.time(), 3)
    entry["stages"].append(stage_rec)
    log(f"  stage {stage.path} ({stage.secs}s)")
    ctx.pinger().start_slow_visitor(stage.secs)
    outcome = stage_spine(tool, ctx, entry, stage)
    ctx.pinger().finish_slow_visitor(entry["stages"][-1])
    entry["stages"][-1] |= {
        "spine_attempts": outcome.get("attempts"),
        "spine_first_interval_s": outcome.get("first_interval_s"),
        "spine_sockets": outcome.get("spine_sockets"),
        "backend_restart": outcome.get("backend_restart"),
        "spine_summary": (outcome.get("summary") or {}).get("state"),
    }
    summary = outcome.get("summary") or {}
    if summary.get("state") == "end":
        entry["stages"][-1] |= summary
    if not outcome["intervals"]:
        entry["stages"][-1]["bulk_error"] = (
            f"spine produced no intervals (exit {outcome['exit']})"
        )
        # The client's own reason, when it gave one: "exit 1" alone is the
        # bare failure §10 forbids.
        if outcome.get("client_error"):
            entry["stages"][-1]["bulk_client_error"] = outcome["client_error"]
            log(f"    bulk client said: {outcome['client_error']}")
        log(f"    bulk spine produced nothing (exit {outcome['exit']})")
    record_stage(entry, mark)
    stage = entry["stages"][-1]
    # Logged, not stored: the reading is a derivation of the stage's recorded
    # evidence (`bulk_window_gbps`, `bulk_zero_share`, `receiver_gbps`,
    # `spine_summary`), and the plot and the gate derive it from that same
    # function — so a rule fix re-renders every stored file instead of needing
    # the sweep to be run again, and there is one rule rather than three.
    reading, source = bulk_reading(stage)
    log(
        f"    bulk: {reading if reading is not None else '-'} Gbit/s "
        f"({source}; peak {stage.get('bulk_peak_gbps')}, "
        f"{(stage.get('bulk_zero_share') or 0) * 100:.0f}% of "
        f"{stage.get('bulk_intervals')} intervals zero)"
    )
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
    """Fast A/B: two builds — or two variants of one build — interleaved.

    The pair runs in the same batch (same epoch) and the tool's processes
    are swapped between the two arms at every load step, so both sample
    the same machine state — sequential before/after runs are defeated by
    epoch drift, which is the whole reason this exists.

    `--ab` interleaves two binaries; `--ab-variants` interleaves two
    configurations of the *same* binary instead (M7's shared-pool vs
    `direct` question), which is the axis a build A/B cannot isolate. The
    axis is recorded in `metrics.builds` and the arm's rate/p99 in
    `metrics.rounds`, so a reader can tell which question a file answers.

    The path is constant for the whole comparison: `--path` is applied once,
    before the interleave, and never changes between the two arms — a shape
    differing between them would be a second variable, which is what makes
    this a single-variable test rather than two measurements. It used to be
    recorded without being applied at all, which made the results meta
    describe a path the run never had (a `screen` run labelled `loss1` was
    clean traffic); applying it once is also what lets a shaped cell — the
    MTU/fragmentation cell, for instance — be A/B-ed at all.
    """
    variant_axis = bool(ctx.args.ab_variants)
    if variant_axis:
        arm_a, arm_b = ctx.args.ab_variants
        # Both arms are the one binary this run measured: a variant A/B
        # must not smuggle a build difference in beside the config change.
        binary_a = binary_b = ctx.knobs.molehill_bin
    else:
        binary_a, binary_b = ctx.args.ab
        arm_a = arm_b = ""
    target = lib.ThroughputTarget.from_band(tool.band)
    entry["metrics"]["builds"] = {
        "axis": "variant" if variant_axis else "build",
        "A": binary_a,
        "B": binary_b,
        "A_version": tool.version(binary_a),
        "B_version": tool.version(binary_b),
    }
    if variant_axis:
        entry["metrics"]["builds"] |= {"A_variant": arm_a, "B_variant": arm_b}
        log(f"    variant A/B on one binary: A={arm_a} vs B={arm_b} ({binary_a})")
    if ctx.args.path:
        ctx.shaper.apply(tool.cid, ctx.args.path)
    rounds = []
    for step in range(1, ctx.ceiling + 1):
        pair = []
        for label, variant, binary in (
            ("A", arm_a, binary_a),
            ("B", arm_b, binary_b),
        ):
            if variant_axis:
                # The config is rewritten by the restart, so the variant
                # must be set before it — that ordering is the whole
                # mechanism of this arm swap.
                tool.use_variant(variant)
            tool.restart(binary)
            ctx.pinger().start_slow_visitor(ctx.knobs.settle_s)
            mark = len(entry["series"])
            r = ctx.backend().iperf_burst(
                target, step, ctx.knobs.settle_s, tag=f"{tool.label} {label}"
            )
            st = lib.series_stats(entry["series"][mark:], "rtt_interactive_ms")
            arm = {
                "build": label,
                "gbps": r.get("gbps_headline"),
                "rtt_p99": st.get("p99"),
                "rtt_n": st.get("n"),
                "rtt_mean": st.get("mean"),
            }
            ctx.pinger().finish_slow_visitor(arm)
            pair.append(arm)
        rounds.append({"streams": step, "pair": pair})
        log(
            f"    step {step}: "
            + " | ".join(
                f"{p['build']} {p['gbps']} Gbit/s p99={p['rtt_p99']}" for p in pair
            )
        )
    entry["metrics"]["rounds"] = rounds
    if variant_axis:
        # The entry's `tool`/`variant` record which configuration the run
        # ended on; leave it on A so the file describes the reference arm
        # (the axis itself lives in `metrics.builds`).
        tool.use_variant(arm_a)


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

    The type comes from the entry, which `new_entry` stamped: a run can carry
    several test types, the file keys each entry by (tool, type), and a second
    parameter carrying the same value would be one more thing that can drift
    from the record it describes.
    """
    args, knobs = ctx.args, ctx.knobs
    test = entry["test"]
    ctx.test = test
    tool.cid = cid
    endpoints = {
        "throughput": asdict(lib.ThroughputTarget.from_band(tool.band)),
        "interactive": {
            "exposed": tool.band["echo_exposed"],
            "backend": tool.band["echo_backend"],
        },
        "udp": {
            "exposed": tool.band["udp_exposed"],
            "backend": tool.band["udp_backend"],
        },
    }
    if knobs.slow_visitor_bps:
        # The slow visitor dials the tool's exposed echo port too (its reads
        # must traverse the path under test, not the backend); it is recorded
        # only when it ran, so the file never claims a probe that was off.
        endpoints["slow_visitor"] = {
            "exposed": tool.band["echo_exposed"],
            "backend": tool.band["echo_backend"],
        }
    entry.update(
        {
            "test": test,
            "path": args.path,
            # The endpoint record (§10): which port each probe dialed, and which
            # port the backend listens on. `soak_check` re-checks the pair, so a
            # sample that measured the backend instead of the tool cannot pass
            # the gate just because the numbers look plausible.
            "endpoints": endpoints,
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
    ctx.with_pingers(pingers)
    samplers.start()
    pingers.start()
    try:
        TEST_TYPES[test](tool, ctx, entry)
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
            tmp.replace(self.path)


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
        help="molehill variants (comma list): " + ",".join(lib.MOLEHILL_VARIANTS),
    )
    ap.add_argument(
        "--test",
        default="capacity",
        help="comma list of test types: " + ",".join(sorted(TEST_TYPES)) + " "
        "(the release sweep runs `rrul,capacity`: the staged schedule and the "
        "load ramp, in one artifact)",
    )
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
        "--ab",
        metavar="BIN_A,BIN_B",
        help="screen/reconnect: the two builds to interleave",
    )
    ap.add_argument(
        "--ab-variants",
        metavar="VAR_A,VAR_B",
        help="screen: two variants of the same binary to interleave "
        "(the config is rewritten per arm; the axis replaces --variants and "
        "is mutually exclusive with --ab)",
    )
    ap.add_argument("--out", default="")
    args = ap.parse_args(argv)
    args.tests = parse_test_types(ap, args.test)
    if args.ab and args.ab_variants:
        ap.error(
            "--ab and --ab-variants are mutually exclusive: pick one axis "
            "(two builds, or two variants of one binary)"
        )
    if args.ab_variants:
        if args.tests != ["screen"]:
            # Not supported rather than silently ignored: a variant swap
            # would need its own cold-start metric in `reconnect` and would
            # be a second variable in the single-configuration test types.
            ap.error(
                f"--ab-variants is not supported for the {args.test} test: "
                "it interleaves two configurations of one binary, which only "
                "the screen measures (reconnect's --ab is a build pair, and "
                "the staged test types run one configuration throughout)"
            )
        args.ab_variants = [v.strip() for v in args.ab_variants.split(",") if v.strip()]
        if len(args.ab_variants) != lib.AB_BUILDS:
            ap.error("--ab-variants takes exactly two variants: VAR_A,VAR_B")
        unknown = [v for v in args.ab_variants if v not in lib.MOLEHILL_VARIANTS]
        if unknown:
            # An unknown variant silently means the default control, so a
            # typo would measure mux against mux and report "no effect".
            ap.error(
                f"unknown variant(s) {','.join(unknown)}: known variants are "
                + ",".join(lib.MOLEHILL_VARIANTS)
            )
    interleaved = set(args.tests) & {"screen", "reconnect"}
    if interleaved and not (args.ab or args.ab_variants):
        ap.error(f"--ab BIN_A,BIN_B is required for the {args.tests[0]} test")
    if not interleaved and args.ab:
        ap.error("--ab is only meaningful for the interleaved test types")
    if args.ab:
        args.ab = args.ab.split(",")
        if len(args.ab) != lib.AB_BUILDS:
            ap.error("--ab takes exactly two binaries: BIN_A,BIN_B")
    return args


def parse_test_types(ap: argparse.ArgumentParser, raw: str) -> list:
    """The run's test types, validated as a *set* per run.

    One artifact carries one entry per (tool, test type), which is what lets
    the release sweep publish the staged schedule and the load ramp together
    (docs/release.md). The refusals are here rather than in `parse_args` so
    each of them can say what it protects.
    """
    tests = [t.strip() for t in raw.split(",") if t.strip()]
    if not tests:
        ap.error("--test needs at least one test type")
    unknown = [t for t in tests if t not in TEST_TYPES]
    if unknown:
        ap.error(
            f"unknown test type(s) {','.join(unknown)}: known types are "
            + ",".join(sorted(TEST_TYPES))
        )
    if len(set(tests)) != len(tests):
        # Two entries of one type would overwrite each other's meaning: the
        # file keys a test by (tool, type), and the gate would compare the
        # second against the baseline's first.
        ap.error("--test lists a type twice: each type runs once per tool")
    if len(tests) > 1 and set(tests) & {"screen", "reconnect"}:
        ap.error(
            "screen and reconnect cannot be combined with another test type: "
            "they interleave two arms inside their own steps, so a second type "
            "in the same run would share neither the arm nor the epoch"
        )
    return tests


def timeline_for(args: argparse.Namespace) -> list[Stage]:
    """The stage schedule: explicit, or the test type's own default."""
    if args.timeline:
        return [
            Stage(path=s.strip(), secs=float(d))
            for s, d in (p.split(":") for p in args.timeline.split(",") if p)
        ]
    if "soak" in args.tests:
        return SOAK_TIMELINE
    if "rrul" in args.tests:
        return DEFAULT_TIMELINE
    # The single-stage types (cost, screen, reconnect) and a bare `capacity`
    # ramp: `--path` at `--secs`. The load ramp walks no stage schedule at all,
    # so a run that carries one *and* the staged schedule records the staged
    # one — the ramp's own record is its `curve`, per load level.
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


def results_path(args: argparse.Namespace) -> Path:
    """Where this run writes its results.

    One definition, because two callers need it and they must agree: the run
    writes there, and the provenance check excludes that same path from the
    clean-tree verdict.
    """
    return (
        Path(args.out) if args.out else Path(__file__).parent / "results-soak-dev.json"
    )


def build_meta(
    args: argparse.Namespace, knobs: lib.Knobs, timeline: list, batch: int, nproc: int
) -> dict:
    """The run's method record: every knob that changes a number.

    A reader must be able to tell what was measured and against what, so the
    SLO the verdict used, the load fractions, the stage schedule and the
    instrumentation switches all travel with the results (§10).
    """
    revision, tree_clean = lib.git_revision(exclude=results_path(args))
    return {
        # The slow visitor is an extra connection in the measured path, so a
        # run that has one is a different workload and must not be compared
        # across the version boundary (soak_check.comparability refuses it);
        # a default run keeps the version it always had.
        "workload_version": WORKLOAD_VERSION + (1 if knobs.slow_visitor_bps else 0),
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
        # The test types this artifact carries, in the order they ran. The
        # gate keys each entry by (tool, test) and compares the types both
        # files have; this says what the file was asked for.
        "tests": list(args.tests),
        "batch": batch,
        "nproc": nproc,
        "cores_per_pair": knobs.cores_per_pair,
        "streams_max": args.streams_max or knobs.streams_max,
        "settle_s": knobs.settle_s,
        # Instrument parameters are part of the method (§10): this bound is
        # what keeps a stage's killed bulk from being measured as the next
        # stage's handshake, so the value the verdict was taken against is
        # recorded rather than implied.
        "stage_drain_budget_s": knobs.stage_drain_budget,
        # The drain's queue half is a tolerance, not zero, because the probes
        # share the tool's class and leave a floor that never clears. Recorded
        # because it decides when a stage is allowed to start.
        "drain_backlog_tolerance_b": Shaper.DRAIN_BACKLOG_TOLERANCE,
        # The states the drain treats as "can still send". A file whose drain
        # waited on a different set is a different method.
        "drain_send_states": list(Shaper.SEND_CAPABLE),
        # The stage offsets at which the bulk spine is dialed. A schedule with
        # more than one entry can carry a stage on a later dial, so which
        # schedule produced a sample is part of what the sample means.
        "spine_retry_s": list(knobs.spine_retry_s),
        # How long a spine waits past its stage for the receiver's summary, and
        # which legs the stage classes are applied to. Both decide what a
        # stage's number means, so both are method (§10).
        "spine_summary_grace_s": knobs.spine_summary_grace_s,
        "shape_legs": knobs.shape_legs,
        # The rate classes' bulk-client socket window: empty means the
        # client's own default, which is what every sweep before this key
        # measured. Part of the method — it decides whether those cells have a
        # readable window at all.
        "rate_socket_window": knobs.rate_socket_window,
        "interactive_ping_interval_ms": knobs.ping_interval_ms,
        "udp_ping_interval_ms": knobs.udp_interval_ms,
        "churn_connects_s": knobs.churn_connects_s,
        # 0 = the slow visitor is off (the default): the knob is part of the
        # method whenever it is not, because it decides how much a visitor
        # holds its stream and therefore what the interactive p99 sees.
        "slow_visitor_bps": knobs.slow_visitor_bps,
        "wedge_silence_s": lib.WEDGE_SILENCE_S,
        "loss_window_s": lib.LOSS_WINDOW_S,
        # The opt-in molehill instrumentation the run inherited (empty for a
        # default run): an instrumented path is not the same path.
        "instrumentation": lib.diag_env(),
        # Provenance (§10): the run must correspond to a known revision of a
        # known binary. `revision` names the commit and `tree_clean` says
        # whether the numbers describe exactly it, because a number produced by
        # uncommitted code describes code that does not exist anywhere else.
        "revision": revision,
        # False means an uncommitted change outside this run's own output: the
        # numbers describe a tree nobody can check out (AGENTS.md §10). The
        # results file this run writes is excluded — otherwise every artifact
        # would be marked dirty by the act of producing it.
        "tree_clean": tree_clean,
        "molehill_bin": str(knobs.molehill_bin),
        # The binary's own bytes, not just its version string: two builds of
        # the same release are indistinguishable by version, and a bench-profile
        # build that outlives the revert of a change describes code that no
        # longer exists. `stale` says the binary predates the newest source file.
        "molehill_bin_fingerprint": lib.binary_fingerprint(knobs.molehill_bin),
        "molehill_version": lib.tool_version(knobs),
        # The host, as a *stable* identity rather than a name: a container
        # hostname changes on every restart while the hardware does not, and a
        # comparison keyed on it refused same-machine runs (and would have
        # accepted a different machine that happens to reuse a hostname).
        # `host_id` is machine-id + CPU model + core count, hashed.
        **lib.host_identity(),
        # ...and the *state* of that machine at the time of the run, which the
        # identity cannot carry: on a host with no machine id the id reduces to
        # `cpu_model | nproc`, so two different machines can name the same host.
        # A fixed, tool-free workload is the measurement that closes it — same
        # id *and* same calibration, or the gate refuses to compare.
        "host_calibration": lib.host_calibration(),
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
    shaper = Shaper(classes, log=log, legs=ctx.knobs.shape_legs)
    ctx.shaper = shaper
    shaper.build()
    try:
        log(f"== batch: {[f'{t} {v}'.strip() for t, v in group]}")
        for (tool_name, variant), (cid, band) in zip(group, classes, strict=True):
            # The screen starts on build A and swaps per step; the other
            # test types start on the default binary.
            binary = ctx.args.ab[0] if ctx.args.ab and tool_name == "molehill" else ""
            tool = Tool(tool_name, variant, band, ctx.knobs, work)
            # One tool pair per test type, started fresh for each: a ramp that
            # followed a 20-minute staged schedule would measure a warmed
            # process, and the pair's own state is part of what a cold start
            # means here.
            for test in ctx.args.tests:
                entry = run_one_tool(tool, cid, ctx, binary, new_entry(ctx.args, test))
                results.tests.append(
                    entry
                    | {
                        "tool": tool.label,
                        "variant": variant,
                        "version": tool.version(binary),
                        "coverage": tool.coverage_for(test),
                    }
                )
                results.checkpoint()
    finally:
        shaper.teardown()


def new_entry(args: argparse.Namespace, test: str) -> dict:
    """An empty test record, filled in by the test type as it measures."""
    return {
        "test": test,
        "path": args.path,
        "series": [],
        "stages": [],
        "metrics": {},
    }


def refuse_inapplicable_knobs(args: argparse.Namespace, knobs: lib.Knobs) -> None:
    """Refuse a knob the chosen test type does not apply — never ignore it.

    `reconnect` restarts the tool in a tight loop to time its cold start, so
    a visitor restarted per stage would be measuring the harness's own
    restart cadence. A knob that is accepted but not applied is a method
    claim the run cannot back (`lib.Knobs`).
    """
    if knobs.rate_socket_window and not _SOCKET_WINDOW_RE.match(
        knobs.rate_socket_window
    ):
        # Refused, not ignored: a malformed window would either abort every
        # rate stage (an iperf3 usage error) or silently measure the default,
        # and both would be recorded as the window the run asked for.
        sys.exit(
            f"SOAK_RATE_SOCKET_WINDOW={knobs.rate_socket_window!r} is not a "
            "size iperf3 accepts: write bytes or a K/M/G suffix, e.g. 256K"
        )
    if knobs.slow_visitor_bps and "reconnect" in args.tests:
        sys.exit(
            f"SOAK_SLOW_VISITOR_BPS={knobs.slow_visitor_bps} is not applied to "
            "--test=reconnect: the visitor is restarted per stage, and that "
            "test's unit is a cold start (use screen, capacity, rrul, soak or "
            "cost, or unset the knob)"
        )


def run_slots(args: argparse.Namespace) -> list:
    """The run's `(tool, variant)` slots.

    A variant A/B is ONE slot on purpose: the two configurations are
    interleaved inside the screen, so listing both through `--variants`
    would run each as its own tool and hide the comparison the axis exists
    for. `--variants` is therefore replaced by the axis' A side, and the
    run header logs the slots that actually ran.
    """
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    if args.ab_variants:
        variants = [args.ab_variants[0]]
    else:
        variants = [v.strip() for v in args.variants.split(",") if v.strip()]
    return [(t, v) for t in tools for v in (variants if t == "molehill" else [""])]


def announce_run(
    args: argparse.Namespace, knobs: lib.Knobs, slots: list, timeline: list, batch: int
) -> None:
    """The run header: what is measured, and on which axis."""
    log(
        f"soak: test={','.join(args.tests)} path={args.path} slots={slots} "
        f"timeline={[(s.path, s.secs) for s in timeline]} batch={batch} "
        f"(nproc={os.cpu_count() or 1})"
    )
    if args.ab:
        log(f"      A/B builds: {args.ab[0]} vs {args.ab[1]}")
    if args.ab_variants:
        log(
            f"      A/B variants (same binary): {args.ab_variants[0]} vs "
            f"{args.ab_variants[1]} — {knobs.molehill_bin}"
        )
    if knobs.slow_visitor_bps:
        log(
            f"      slow visitor: one connection at "
            f"{knobs.slow_visitor_bps} bit/s per stage (workload version "
            f"{WORKLOAD_VERSION + 1})"
        )


def main() -> None:
    args = parse_args()
    knobs = lib.Knobs.from_env()
    refuse_inapplicable_knobs(args, knobs)
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
    # The provenance check runs whatever the work dir is: `SOAK_KEEP` decides
    # where the artifacts go, not whether the binary is current.
    fp = lib.binary_fingerprint(knobs.molehill_bin)
    if fp.get("stale"):
        # Loud, but not fatal: a stale peer binary is a legitimate
        # reproduction, and refusing to run would be worse than recording it.
        # The release ritual is where it must stop a run (docs/release.md).
        log(
            f"WARNING: {knobs.molehill_bin} predates the newest source file — "
            f"these numbers describe an older build"
        )
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

    out = results_path(args)
    with contextlib.suppress(OSError):
        out.parent.mkdir(parents=True, exist_ok=True)
    timeline = timeline_for(args)
    nproc = os.cpu_count() or 1
    budget = max(1, int(nproc / knobs.cores_per_pair))
    batch = args.batch or min(knobs.max_batch, budget)
    slots = run_slots(args)

    announce_run(args, knobs, slots, timeline, batch)
    results = Results(
        path=out,
        meta=build_meta(args, knobs, timeline, batch, nproc),
        tests=[],
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
