#!/usr/bin/env python3
"""The bench model's vocabulary: metrics, arms, scenarios, profiles.

This module is the standard, written down once. Everything else in
`bench/src/bench/` executes it, and nothing may add a metric, an arm or a
scenario anywhere else — a number that is not defined here has no definition,
and a metric without a definition is not comparable with anything.

The rules the model enforces, in code:

* **Every metric declares its own meaning.** `unit`, whether higher or lower is
  better, the denominator it is a ratio over, and where the number comes from.
  The report and the verdict are generated from that declaration, so a metric
  cannot be described one way and computed another.
* **Every scenario declares the claim it supports and the arm that controls
  it.** A scenario that cannot run on the `control` arm is `diagnostic`: it
  produces numbers and can never produce a verdict.
* **Every arm states the whole configuration it measures.** Nothing is left to a
  default that a previous arm could have changed; two arms differ only in the
  keys the arm itself sets (one variable per pair).
* **Every profile is time-budgeted.** A profile names the scenarios it runs and
  the seconds it expects to spend; `just bench doctor` and the run report print
  the budget beside the actual cost, so "fast" is a measurement too.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from typing import ClassVar

# --------------------------------------------------------------------------
# Metrics
# --------------------------------------------------------------------------
#: A metric needs a definition before it may appear in a result. `needs` lists
#: the workload kinds that can produce it; a metric requested by a scenario
#: whose kind is not listed is a modelling error and the run refuses it.
#:
#: `materiality` is the smallest difference worth calling a claim, and it is
#: the *floor*: the verdict raises it to the run's own measured noise (see
#: `analysis.verdict`). It is stated per metric because a 10 % throughput
#: difference and a 10 % latency difference do not mean the same thing.
METRIC_SPECS: tuple[dict, ...] = (
    {
        "id": "throughput_gbps",
        "unit": "Gbit/s",
        "direction": "higher",
        "definition": "receiver-window payload bytes x 8 / measured window",
        "denominator": "the workload's measured window",
        "needs": (
            "bulk",
            "bulk-pair",
            "staged",
        ),
        "materiality": ("rel", 10.0),
        "precision": 3,
        "headline": True,
    },
    {
        "id": "offered_gbps",
        "unit": "Gbit/s",
        "direction": "none",
        "definition": "visitor link egress bytes x 8 / measured window",
        "denominator": "the workload's measured window",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 10.0),
        "precision": 3,
    },
    {
        "id": "rtt_worst_1s_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": (
            "the worst one-second mean of the interactive stream's round trips: "
            "a burst that a percentile over the whole stage would average away"
        ),
        "denominator": "one second of the interactive stream",
        "needs": ("staged",),
        "materiality": ("rel", 30.0),
        "precision": 3,
    },
    {
        "id": "rtt_error_rate_pct",
        "unit": "%",
        "direction": "lower",
        "definition": "failed interactive pings / (successful + failed)",
        "denominator": "one interactive ping",
        "needs": ("staged", "capacity"),
        "materiality": ("abs", 0.5),
        "precision": 4,
    },
    {
        "id": "churn_per_s",
        "unit": "1/s",
        "direction": "higher",
        "definition": "short-lived connections established per second",
        "denominator": "the stage's measured window",
        "needs": ("staged",),
        "materiality": ("rel", 10.0),
        "precision": 1,
    },
    {
        "id": "drift_rss_mib_per_min",
        "unit": "MiB/min",
        "direction": "lower",
        "definition": (
            "least-squares slope of the arm's summed RSS over the run: a leak is "
            "a slope over time, not a level"
        ),
        "denominator": "one minute of run time",
        "needs": ("staged",),
        "materiality": ("abs", 50.0),
        "precision": 3,
    },
    {
        "id": "drift_fds_per_min",
        "unit": "count/min",
        "direction": "lower",
        "definition": "least-squares slope of the arm's open descriptors",
        "denominator": "one minute of run time",
        "needs": ("staged",),
        "materiality": ("abs", 1.0),
        "precision": 3,
    },
    {
        "id": "drift_threads_per_min",
        "unit": "count/min",
        "direction": "lower",
        "definition": "least-squares slope of the arm's thread count",
        "denominator": "one minute of run time",
        "needs": ("staged",),
        "materiality": ("abs", 1.0),
        "precision": 3,
    },
    {
        "id": "wedge_count",
        "unit": "count",
        "direction": "lower",
        "definition": (
            "stretches longer than 10 s in which the interactive probe asked "
            "and heard nothing: a tool that stops answering under degradation, "
            "which an average cannot show"
        ),
        "denominator": "one silent stretch",
        "needs": ("staged",),
        "materiality": ("abs", 1.0),
        "precision": 0,
    },
    {
        "id": "wedge_max_s",
        "unit": "s",
        "direction": "lower",
        "definition": "the longest interactive silence in the run",
        "denominator": "one silent stretch",
        "needs": ("staged",),
        "materiality": ("abs", 1.0),
        "precision": 3,
    },
    {
        "id": "capacity_streams",
        "unit": "count",
        "direction": "higher",
        "definition": (
            "the highest bulk-stream level whose interactive stream still met "
            "the SLO, from the capacity ramp"
        ),
        "denominator": "one bulk stream",
        "needs": ("capacity",),
        "materiality": ("abs", 1.0),
        "precision": 0,
        "headline": True,
    },
    {
        "id": "capacity_headroom_pct",
        "unit": "%",
        "direction": "higher",
        "definition": "1 - max sustainable streams / the ramp's ceiling",
        "denominator": "the ramp's ceiling",
        "needs": ("capacity",),
        "materiality": ("abs", 5.0),
        "precision": 1,
    },
    {
        "id": "startup_s",
        "unit": "s",
        "direction": "lower",
        "definition": (
            "median time from process start to the service answering a fresh "
            "connection: what a visitor pays after a restart"
        ),
        "denominator": "one cold start",
        "needs": ("reconnect",),
        "materiality": ("rel", 20.0),
        "precision": 3,
        "headline": True,
    },
    {
        "id": "startup_max_s",
        "unit": "s",
        "direction": "lower",
        "definition": "the worst cold start the run saw",
        "denominator": "one cold start",
        "needs": ("reconnect",),
        "materiality": ("rel", 25.0),
        "precision": 3,
    },
    {
        "id": "slo_broken",
        "unit": "bool",
        "direction": "none",
        "definition": (
            "1 when the cell broke the SLO (p99 above 50 ms or error rate above "
            "0.5 %), so a level's verdict travels with its numbers"
        ),
        "denominator": "one measured level or stage",
        "needs": ("staged", "capacity"),
        "materiality": ("abs", 1.0),
        "precision": 0,
    },
    {
        "id": "rate_per_s",
        "unit": "1/s",
        "direction": "higher",
        "definition": "completed request/response pairs / measured window",
        "denominator": "the workload's measured window",
        "needs": ("rr",),
        "materiality": ("rel", 10.0),
        "precision": 1,
        "headline": True,
    },
    {
        "id": "rtt_p50_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "nearest-rank median of the probe's per-request round trips",
        "denominator": "one request/response pair",
        "needs": (
            "rr",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 3,
    },
    {
        "id": "rtt_p99_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "nearest-rank 99th percentile of the probe's round trips",
        "denominator": "one request/response pair",
        "needs": (
            "rr",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 3,
        "headline": True,
    },
    {
        "id": "rtt_max_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "worst single round trip the probe observed",
        "denominator": "one request/response pair",
        "needs": (
            "rr",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 25.0),
        "precision": 3,
    },
    {
        "id": "rtt_samples",
        "unit": "count",
        "direction": "none",
        "definition": "round trips the percentile is taken over",
        "denominator": "none (sample count)",
        "needs": (
            "rr",
            "udp",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 0.0),
        "precision": 0,
    },
    {
        "id": "udp_recv_mbit",
        "unit": "Mbit/s",
        "direction": "higher",
        "definition": (
            "datagrams the probe received x payload / the probe's own measured "
            "window (which for a blast includes the drain)"
        ),
        "denominator": "the probe's receive window",
        "needs": (
            "udp",
            "udp-ladder",
        ),
        "materiality": ("rel", 10.0),
        "precision": 1,
        "headline": True,
    },
    {
        "id": "udp_loss_pct",
        "unit": "%",
        "direction": "lower",
        "definition": "(datagrams sent - datagrams received) / datagrams sent",
        "denominator": "datagrams the probe sent",
        "needs": (
            "udp",
            "udp-ladder",
            "staged",
        ),
        "materiality": ("abs", 1.0),
        "precision": 3,
        "headline": True,
    },
    {
        "id": "udp_gap_p99_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "99th percentile of the gaps between replies the probe received",
        "denominator": "one received datagram",
        "needs": (
            "udp",
            "udp-ladder",
            "staged",
        ),
        "materiality": ("rel", 20.0),
        "precision": 3,
    },
    {
        "id": "udp_rtt_p99_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "99th percentile of the probe's datagram echo round trips",
        "denominator": "one echoed datagram",
        "needs": (
            "udp",
            "staged",
        ),
        "materiality": ("rel", 20.0),
        "precision": 3,
    },
    {
        "id": "setup_p50_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": (
            "median time to establish one connection, when the workload opens a "
            "fresh one per request (the setup a visitor pays to arrive)"
        ),
        "denominator": "one connection",
        "needs": ("rr",),
        "materiality": ("rel", 15.0),
        "precision": 3,
    },
    {
        "id": "cpu_s_per_gbit",
        "unit": "s/Gbit",
        "direction": "lower",
        "definition": (
            "tool CPU-seconds (user+sys, both daemons) / Gbit the visitor offered"
        ),
        "denominator": "Gbit on the visitor's link egress",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 4,
        "headline": True,
    },
    {
        "id": "cpu_cores",
        "unit": "cores",
        "direction": "lower",
        "definition": "tool CPU-seconds / measured window",
        "denominator": "the workload's measured window",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 3,
    },
    {
        "id": "bytes_per_syscall",
        "unit": "B",
        "direction": "higher",
        "definition": (
            "device I/O per call: the tool process's rchar+wchar / syscr+syscw. "
            "Only an L3 arm can produce it — `/proc/<pid>/io` does not account "
            "socket payload — so a forwarding arm records a typed absence"
        ),
        "denominator": "one read/write-family syscall",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 1,
    },
    {
        "id": "syscalls_per_s",
        "unit": "1/s",
        "direction": "lower",
        "definition": (
            "device I/O calls per second: the tool process's syscr+syscw over "
            "the measured window. The L3 path's shape, and only its: see "
            "`bytes_per_syscall`"
        ),
        "denominator": "the workload's measured window",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 0,
    },
    {
        "id": "wire_per_visitor_byte",
        "unit": "ratio",
        "direction": "lower",
        "definition": "tunnel-link bytes (both directions) / visitor-link egress bytes",
        "denominator": "bytes the visitor offered",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 10.0),
        "precision": 4,
        "headline": True,
    },
    {
        "id": "mean_carried_packet_b",
        "unit": "B",
        "direction": "none",
        "definition": "tunnel-link bytes / tunnel-link packets, both directions",
        "denominator": "one packet on the tunnel link",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 5.0),
        "precision": 1,
    },
    {
        "id": "rss_peak_mib",
        "unit": "MiB",
        "direction": "lower",
        "definition": (
            "peak RSS summed over the tool's daemons, sampled during the workload"
        ),
        "denominator": "one process set",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 10.0),
        "precision": 1,
    },
    {
        "id": "fds_peak",
        "unit": "count",
        "direction": "lower",
        "definition": "peak open file descriptors summed over the tool's daemons",
        "denominator": "one process set",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 0,
    },
    {
        "id": "threads_peak",
        "unit": "count",
        "direction": "lower",
        "definition": "peak thread count summed over the tool's daemons",
        "denominator": "one process set",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("rel", 15.0),
        "precision": 0,
    },
    {
        "id": "service_sockets_peak",
        "unit": "count",
        "direction": "lower",
        "definition": (
            "peak sockets on the exposed service port in the server's own "
            "namespace: the per-visitor state the architecture keeps"
        ),
        "denominator": "one visitor",
        "needs": (
            "bulk",
            "rr",
            "staged",
        ),
        "materiality": ("abs", 1.0),
        "precision": 0,
    },
    {
        "id": "retrans_segments",
        "unit": "count",
        "direction": "lower",
        "definition": "TCP segments retransmitted by the visitor and server namespaces",
        "denominator": "one TCP segment",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        # Counts are judged in packets, not in percent: a percentage of a small
        # retransmit count is noise, and "a thousand packets" is a finding.
        "materiality": ("abs", 1000.0),
        "precision": 0,
    },
    {
        "id": "dropped_packets",
        "unit": "count",
        "direction": "lower",
        "definition": (
            "packets the topology's devices and qdiscs dropped over the window"
        ),
        "denominator": "one packet",
        "needs": (
            "bulk",
            "bulk-pair",
            "rr",
            "udp",
            "udp-ladder",
            "staged",
            "capacity",
        ),
        "materiality": ("abs", 1000.0),
        "precision": 0,
    },
)


@dataclass(frozen=True)
class Metric:
    """One measurable quantity: its meaning, its unit and its claim floor."""

    id: str
    unit: str
    direction: str
    definition: str
    denominator: str
    needs: tuple
    materiality: tuple
    precision: int
    headline: bool = False

    @property
    def better(self) -> str:
        return {"higher": "higher is better", "lower": "lower is better"}.get(
            self.direction, "context, not a verdict"
        )

    def format(self, value) -> str:
        if value is None:
            return "-"
        if self.unit == "count" and self.precision == 0 and float(value).is_integer():
            return f"{value:.0f}"
        return f"{value:.{self.precision}f}"


METRICS: dict[str, Metric] = {m["id"]: Metric(**m) for m in METRIC_SPECS}


def metric_ids() -> list:
    return list(METRICS)


def metrics_for(kind: str) -> list:
    """Every metric whose declared evidence the workload kind can produce."""
    return [m.id for m in METRICS.values() if kind in m.needs]


# --------------------------------------------------------------------------
# Conditions and timelines
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class Condition:
    """One path condition: what it emulates, and how it is imposed.

    `netem` is the argument list of a netem qdisc (`[]` is the clean path) and
    `mtu` is an *interface* property, not a qdisc: a condition that names one
    changes the path for every packet during that stage, which is why the MTU
    classes are never in a default timeline. `block_udp` is a firewall rule
    rather than a qdisc — it drops one *protocol* on the leg, which is how a
    path that refuses the data plane's first carrier is emulated without asking
    the product for a setting it does not have. The vocabulary lives here
    because it is method: a run's fingerprint must move when a condition's
    meaning moves, and a report must be able to say what a stage emulated.
    """

    name: str
    what: str
    netem: tuple = ()
    mtu: int | None = None
    block_udp: bool = False
    #: Packets per second of UDP the leg admits, when the condition polices the
    #: protocol rather than refusing it (`None` = not policed). `block_udp` is
    #: the hard case — the carrier cannot be established at all and the product
    #: falls back; a *limit* is the soft one, where the carrier still works but
    #: only at the rate the policer allows, which is the only shape in which a
    #: carrier can be losing traffic while looking healthy to its own sender.
    udp_limit: int | None = None


#: The buffer a shaped leg carries unless its condition says otherwise, in
#: packets — netem's `limit`, which is what its queue is counted in.
#:
#: A qdisc holds `offered rate x delay` packets in flight, so this number is a
#: *capacity* as much as a depth: at the 100 ms `rtt100` names, 10 000 packets
#: is 100 000 packets/s. The workloads offer up to ~38 000 packets/s on the
#: return leg, so the buffer is not the binding constraint and the delay the
#: condition names is the delay the path has. `loss1_buf` arrived at the same
#: value from the other direction ("tail drops out of the way without the
#: hundred-megabyte standing queue that inflates RTT"), and a condition that
#: means to tail-drop states its own smaller limit instead.
SHAPED_QUEUE_LIMIT_PACKETS = 10_000

#: The buffer a class carries when tail-dropping is part of what it *means*:
#: netem's own default, stated rather than inherited, so that "this class drops
#: when it is full" is a decision on the page instead of a value nobody chose.
TAIL_DROP_QUEUE_LIMIT_PACKETS = 1_000

CONDITIONS: dict[str, Condition] = {
    # A shaped leg's buffer is method and every shaped class states it rather
    # than leaving it to netem's default of 1 000 packets — see
    # `SHAPED_QUEUE_LIMIT_PACKETS` for what that default cost and why this
    # value. A class that *means* to tail-drop states the smaller limit it
    # means (`loss1`).
    "clean": Condition("clean", "a healthy network (the control)"),
    "rtt100": Condition(
        "rtt100",
        "a long-haul or satellite link",
        ("delay", "100ms", "limit", str(SHAPED_QUEUE_LIMIT_PACKETS)),
    ),
    "loss1": Condition(
        "loss1",
        "a lossy wifi or mobile link",
        ("delay", "10ms", "loss", "1%", "limit", str(TAIL_DROP_QUEUE_LIMIT_PACKETS)),
    ),
    "loss5": Condition(
        "loss5",
        "a badly congested path",
        ("delay", "100ms", "loss", "5%", "limit", str(SHAPED_QUEUE_LIMIT_PACKETS)),
    ),
    "rate100": Condition(
        "rate100",
        "a 100 Mbit uplink",
        ("rate", "100mbit", "delay", "20ms", "limit", "2000"),
    ),
    "rate20": Condition(
        "rate20",
        "a 20 Mbit uplink",
        ("rate", "20mbit", "delay", "40ms", "limit", "2000"),
    ),
    "loss1_rate100": Condition(
        "loss1_rate100",
        "a 100 Mbit uplink that also loses packets (the lossy-WAN case)",
        ("rate", "100mbit", "delay", "20ms", "loss", "1%", "limit", "2000"),
    ),
    "jitter": Condition(
        "jitter",
        "a bufferbloated access link",
        ("delay", "20ms", "10ms", "limit", str(SHAPED_QUEUE_LIMIT_PACKETS)),
    ),
    "loss1_deep": Condition(
        "loss1_deep",
        "the same 1 % loss with a queue deep enough not to tail-drop: it tells "
        "a carrier's loss recovery apart from the shaper's own drops",
        ("delay", "10ms", "loss", "1%", "limit", "100000"),
    ),
    "loss1_buf": Condition(
        "loss1_buf",
        "a lossy path with a large but finite buffer (10 000 packets ≈ 14.7 MB, "
        "≈ 27 ms of standing queue at 545 Mbit/s): tail drops are out of the "
        "way without the hundred-megabyte standing queue that inflates RTT",
        ("delay", "10ms", "loss", "1%", "limit", "10000"),
    ),
    "udp_throttled": Condition(
        "udp_throttled",
        "a path that polices UDP to a fraction of its capacity: the carrier "
        "the data plane prefers still establishes, but only carries what the "
        "policer admits (and pays its ARQ for what the policer drops)",
        # 10 000 packets/s is the `limit` module's own ceiling (~117 Mbit/s of
        # 1472-byte datagrams), which is the point: the policer has to bite
        # hard enough that the carrier riding UDP is starved while the one
        # that does not is untouched.
        udp_limit=10_000,
    ),
    "udp_blocked": Condition(
        "udp_blocked",
        "a path that drops UDP: the data plane's first carrier cannot be "
        "established, so it must fall back to TCP",
        block_udp=True,
    ),
    "mtu1280": Condition("mtu1280", "a tunnel or an IPv6-minimum path", mtu=1280),
    "loss1_mtu1280": Condition(
        "loss1_mtu1280",
        "a lossy link that also fragments",
        ("delay", "10ms", "loss", "1%", "limit", str(TAIL_DROP_QUEUE_LIMIT_PACKETS)),
        mtu=1280,
    ),
}


#: The stage schedules. A timeline is a *list* rather than a dict because the
#: recovery axis is a repeat: the clean stage at both ends is the run's own
#: replicate, and its position is part of what it means.
TIMELINES: dict[str, tuple] = {
    # The sweep: how a tool behaves as the path degrades, and whether it is
    # still the tool it was when the path recovers.
    "sweep": (
        ("clean", 150.0),
        ("rtt100", 120.0),
        ("loss1", 120.0),
        ("loss5", 120.0),
        ("rate100", 120.0),
        ("rate20", 120.0),
        ("jitter", 120.0),
        ("clean", 150.0),
    ),
    # The drift axis: fewer conditions, longer holds.
    "soak": (
        ("clean", 180.0),
        ("loss1", 180.0),
        ("rtt100", 180.0),
        ("loss5", 180.0),
        ("clean", 180.0),
    ),
    # One stage: the fixed operating point a cost or screen run measures.
    "single": (("clean", 60.0),),
    # The L3 wedge's own timeline: the one condition that produces it, held
    # long enough to catch it, bracketed by the clean stages that show it is
    # not permanent. A diagnostic, not the sweep: the wedge appears in the
    # *tail* of an `rtt100` hold (measured 2026-10-15: nothing in the first
    # 60 s of a stage, a collapse from t≈110 s of a 100 s hold), so a stage
    # short enough to be convenient is a stage that mostly misses it.
    "wedge": (
        ("clean", 30.0),
        ("rtt100", 180.0),
        ("clean", 30.0),
    ),
}


#: The service-level objective: the line every chart shares and the one the
#: capacity ramp breaks on. An *error rate* rather than zero, because zero is
#: not a property of a tool on a shaped path (measured across four tools: 0 to
#: 0.31 % on clean stages); half a percent is above every arm measured so far.
SLO_RTT_P99_MS = 50.0
SLO_ERROR_RATE_PCT = 0.5


def timeline_for(name: str, scale: float = 1.0) -> tuple:
    """A timeline with every stage's hold scaled, never the order or the set."""
    if name not in TIMELINES:
        raise SystemExit(f"unknown timeline {name!r}; known: {', '.join(TIMELINES)}")
    return tuple((cond, round(secs * scale, 2)) for cond, secs in TIMELINES[name])


# --------------------------------------------------------------------------
# Arms
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class Arm:
    """One thing under test, with its whole configuration stated.

    `id` is what the results call it. `kind` selects the architecture:

    * `control` — no tool in the path; the topology's own ceiling, and the arm
      every other arm is normalized against.
    * `l4` — terminated TCP forwarding, `mode` = `multiplex` (default pool) or
      `direct` (one physical connection per stream).
    * `l3` — the transparent client: whole IP packets over a TUN device.

    `binary` is the build under test; two arms differing only in it are the A/B
    axis. `pool_cap` (the configured `[client.data].tunnels`) and
    `txqueuelen` are the two settings this model prices separately, because both
    are architectural choices rather than tuning: the pool's width separates the
    multiplexer's framing cost from its pool's — the pool is pinned at that
    count, so an arm's width is a method parameter and not a result — and the
    TUN queue length decides whether an L3 arm measures the architecture or the
    kernel's default queue — and it is set as the **qdisc's** limit, not only as
    the interface attribute. A TUN device comes up on the host's default qdisc
    (`fq` here), which does not derive its limit from `txqueuelen`: setting the
    attribute alone left every arm on the host's default (measured 2026-10-15,
    `txqueuelen` 60/1000/10000 all read back `limit 10000p`), so the arm's
    stated length never reached the queue it was supposed to describe.

    `pool_cap` is the third such setting, and it means one thing per kind: for an
    L4 arm it is the pool's `[client.data].tunnels`; for an L3 arm it is the
    claim's carrier **lane budget** (`[transparent.data].tunnels`), which is one
    connection per lane because a claim never multiplexes. `0` leaves the key
    out of the generated config — the product's own default, four tunnels for a
    forwarding pool and four lanes per claim for a claim — so an arm states the
    width it measured instead of inheriting whatever the default is this week,
    and an arm that wants the pre-0.11 shape writes `pool_cap = <claims>`.

    `lend` states whether the L3 client may move a lane between claims, and only
    an L3 arm may say so (a forwarding service has no lanes to move).

    The data plane's *carrier* is not an arm parameter: the product chooses it
    at runtime (KCP, falling back to TCP, re-probing), so a run that needs the
    paths apart says so with a **condition** — `udp_blocked` is the fallback, a
    clean path carries KCP — or, to isolate one carrier, with the inherited
    `MOLEHILL_DATA_CARRIER` switch, which the results record as instrumentation.
    The data plane's *shape* is not an arm parameter either: a forwarding
    service always multiplexes and a claim never does, which is what the product
    derives from the service type.
    """

    id: str
    kind: str
    pool_cap: int = 0
    #: Whether an L3 client may lend an idle lane of one claim to another
    #: (`[transparent.data].lend`, on by default). Off is the equal split held
    #: static, which is the control an arm needs to price the mechanism: the two
    #: arms are then the same binary and the same topology, differing only in the
    #: decision under test.
    lend: bool = True
    txqueuelen: int = 1000
    binary: str = ""
    #: Free-form label recorded in the results (e.g. "A"/"B" for a build pair).
    side: str = ""
    #: The peer tool this arm runs, when `kind` is "peer" (frp, rathole, nps).
    tool: str = ""
    #: What a table calls this arm; the id is what a result file calls it.
    label: str = ""
    #: The arm this one is the *decision* against: the pair the analysis reads,
    #: for an axis that is a configuration field rather than a second binary.
    #: `--ab-arm` pairs two builds of one arm; this pairs two configurations of
    #: one build, which is what a knob's own A/B is.
    against: str = ""

    @property
    def name(self) -> str:
        return self.label or self.id

    @property
    def is_tool(self) -> bool:
        return self.kind != "control"

    @property
    def mode_flag(self) -> str:
        if self.kind == "peer":
            raise ValueError(f"a peer arm ({self.tool}) has no mode flag")
        return "--transparent" if self.kind == "l3" else "--client"

    @property
    def data_carrier(self) -> str:
        """The carrier an arm can claim, which is none: the product chooses.

        `auto` for a tool arm — the data plane tries KCP and falls back to TCP on
        its own, so an arm cannot name a carrier without describing a setting the
        product no longer has. A run that needs the paths apart uses the
        `udp_blocked` condition (the fallback) or the inherited
        `MOLEHILL_DATA_CARRIER` switch (one carrier alone, recorded as
        instrumentation). Empty for a control arm and for a peer, whose stacks
        are their own.
        """
        return "" if self.kind in ("peer", "control") else "auto"

    @property
    def states_kcp(self) -> bool:
        return self.data_carrier == "kcp"

    @property
    def uses_device_io(self) -> bool:
        """Whether this arm's payload crosses a character device.

        Only the L3 path does: a claim reads and writes whole packets on its TUN
        device. Everything else moves payload through sockets, and
        `/proc/<pid>/io` does not account socket payload (measured: 200 MB
        through a socketpair moves `rchar` by 105 KB, the same bytes through a
        pipe by 209 MB), so the device-I/O metrics can only be produced here.
        """
        return self.kind == "l3"

    @property
    def dial_host(self) -> str:
        """The address the visitor dials: what the architecture exposes.

        It is the path under test, so it is never the backend's address: an L4
        server exposes its own listener, an L3 client exposes the address its
        TUN owns, and the control arm exposes the backend directly.
        """
        return {
            "control": TOPO_CONTROL_IP,
            "l4": TOPO_SRV_IP,
            "peer": TOPO_SRV_IP,
            "l3": TOPO_PUBLIC_IP,
        }[self.kind]

    @property
    def backend_bind(self) -> str:
        """Where the backend listens: the address this architecture delivers to.

        Measured the hard way (docs/benchmarks.md): an iperf3 UDP server left on
        the wildcard learns the visitor as its peer and `connect()`s outbound,
        after which datagrams addressed to the owned address match no socket and
        the run ends on an ICMP port-unreachable.
        """
        return {
            "control": TOPO_CONTROL_IP,
            "l4": "127.0.0.1",
            "peer": "127.0.0.1",
            "l3": TOPO_PUBLIC_IP,
        }[self.kind]


#: The deeper TUN queue an arm may ask for: the same architecture with one
#: operator setting changed (`ip link set <tun> txqueuelen N`, plus the qdisc
#: limit that actually holds the packets — see `topology.set_txqueuelen`).
DEEP_TXQUEUELEN = 10000

# The topology's addresses live here because an arm's identity is defined by
# them; `topology.py` owns the devices and routes that make them true.
TOPO_VIS_IP = "10.10.0.2"
TOPO_SRV_IP = "10.10.0.254"
TOPO_SRV_CLI_IP = "10.30.0.1"
TOPO_CLI_IP = "10.30.0.2"
TOPO_PUBLIC_IP = "10.99.0.1"
TOPO_CONTROL_IP = TOPO_CLI_IP


#: The arms a profile or a command line may name, with the whole configuration
#: each one states. `l3`'s mode is written into every claim, so the arm records
#: the mode it measured rather than inheriting whatever the binary's default is
#: this week.
ARM_CATALOG: dict = {
    "control": Arm("control", "control"),
    "l4": Arm("l4", "l4"),
    "l4-mux2": Arm("l4-mux2", "l4", pool_cap=2),
    "l4-mux8": Arm("l4-mux8", "l4", pool_cap=8),
    # `pool_cap` writes `[client.data].tunnels`, so these arms are the pool
    # *pinned* at 2 / 8 connections — the arm's width, not a cap it may grow to.
    # There is no `l4-mux1`: the topology carries a UDP service with two workers,
    # so `tunnels = 1` is below the UDP-derived floor and the client refuses to
    # start (the degenerate L4 cell docs/benchmarks.md used to record without a
    # cause). One stream on a multiplexed session is measured with `l4-mux2` in a
    # single-flow scenario — the same shape, one stream on one session, with a
    # legal pool.
    "l3": Arm("l3", "l3"),
    # The lane axis: the carrier's budget, so a claim's inner flows can be spread
    # over several connections instead of one. A claim never multiplexes, so the
    # budget *is* the number of connections the claims hold — and it is the
    # *carrier's* total, divided equally among every claim of the topology, not
    # one claim's count. The L3 topology carries five claims, so a budget that is
    # not a multiple of five leaves its remainder unallocated (the allocator
    # redistributes the shares; it does not spend the remainder) and one below
    # five is refused outright: the self-check enforces both, because a config
    # the product refuses measures nothing.
    "l3-budget10": Arm("l3-budget10", "l3", pool_cap=10),
    # The same arm with the mechanism switched off: the equal split held static,
    # which is what prices lending itself (same binary, same topology, one
    # decision apart).
    "l3-budget10-nolend": Arm("l3-budget10-nolend", "l3", pool_cap=10, lend=False),
    # The same two claims, stated as the pair they are: `l3-budget10` declares
    # the static arm as its baseline, so a run carrying both compares them with
    # the run's own floors rather than leaving the reader to subtract medians.
    "l3-budget10-lending": Arm(
        "l3-budget10-lending", "l3", pool_cap=10, against="l3-budget10-nolend"
    ),
    "l3-deep": Arm("l3-deep", "l3", txqueuelen=DEEP_TXQUEUELEN),
    # There is no carrier axis: the product chooses its carrier at runtime
    # (KCP, falling back to TCP), so the contrast lives in the *path* — a run
    # against `--condition udp_blocked` is the fallback, a clean run is KCP —
    # while forcing one carrier for isolation is an experiment-wide environment
    # switch rather than an arm.
    # The reference tools. Their adapters live in `targets/peers/`; the arm is the
    # same shape as any other, which is what makes the comparison one run.
    "frp": Arm("frp", "peer", tool="frp", label="frp"),
    "rathole": Arm("rathole", "peer", tool="rathole", label="rathole"),
    "nps": Arm("nps", "peer", tool="nps", label="nps"),
    # The sweep's own row name: the product in its default configuration.
    "molehill": Arm("molehill", "l4", label="molehill"),
}


def arms_from_names(names: list, binary: str) -> list:
    """Resolve `--arms control,l4,l3` against the catalog, or refuse."""
    unknown = [n for n in names if n not in ARM_CATALOG]
    if unknown:
        raise SystemExit(f"unknown arm(s): {unknown}; known: {', '.join(ARM_CATALOG)}")
    arms = []
    for name in names:
        arm = replace(ARM_CATALOG[name], binary=binary)
        try:
            refuse_carrier_arm(arm)
            refuse_lend_outside_l3(arm)
            refuse_dangling_baseline(arm)
        except ValueError as e:
            raise SystemExit(str(e)) from e
        arms.append(arm)
    return arms


def default_arms() -> list:
    """The arms a profile runs when the command line names none."""
    return [
        Arm("control", "control"),
        Arm("l4", "l4"),
        Arm("l3", "l3"),
    ]


def parse_arm_spec(spec: str) -> Arm:
    """`id=l3,pool_cap=10,txqueuelen=10000,side=B` — an arm as data.

    The syntax exists so that an experiment with a knob the catalog does not
    name is still *declared* (and therefore recorded and comparable) rather than
    patched into the source. Unknown keys are refused, because a typo that
    silently measured the default is how a run stops describing itself.
    """
    parts = [p.strip() for p in spec.split(",") if p.strip()]
    if not parts or "=" not in parts[0]:
        raise ValueError(f"arm spec must start with id=<kind>: {spec!r}")
    fields: dict = {}
    for part in parts:
        if "=" not in part:
            raise ValueError(f"arm spec segment needs key=value: {part!r}")
        key, value = (s.strip() for s in part.split("=", 1))
        fields[key] = value
    known = {
        "id",
        "kind",
        "pool_cap",
        "lend",
        "against",
        "txqueuelen",
        "binary",
        "side",
        "tool",
        "label",
    }
    unknown = sorted(set(fields) - known)
    if unknown:
        raise ValueError(f"unknown arm key(s) {unknown}; known: {sorted(known)}")
    for numeric in ("pool_cap", "txqueuelen"):
        if numeric in fields:
            fields[numeric] = int(fields[numeric])
    if "lend" in fields:
        fields["lend"] = _parse_bool("lend", fields["lend"])
    kind = fields.pop("kind", None) or fields.get("id", "")
    if kind not in ("control", "l4", "l3", "peer"):
        raise ValueError(f"unknown arm kind {kind!r}; known: control, l4, l3, peer")
    arm = Arm(kind=kind, **fields)
    refuse_carrier_arm(arm)
    refuse_lend_outside_l3(arm)
    refuse_dangling_baseline(arm)
    return arm


def _parse_bool(key: str, value: str) -> bool:
    """A strict `true`/`false`.

    `bool("false")` is `True`, and an arm that silently measured the opposite of
    what it says is the failure the arm parser exists to prevent.
    """
    if value not in ("true", "false"):
        raise ValueError(f"{key} must be true or false, got {value!r}")
    return value == "true"


def refuse_dangling_baseline(arm: Arm) -> None:
    """Refuse an arm whose declared baseline is not in the catalog.

    A pair the catalog cannot resolve is a comparison nobody will make: the
    analysis reads the ids in the results file, and an arm naming a stranger
    would quietly measure alone.
    """
    if not arm.against:
        return
    if arm.against not in ARM_CATALOG:
        raise ValueError(
            f"arm {arm.id!r}: `against` names {arm.against!r}, "
            "which is not in the catalog"
        )
    if ARM_CATALOG[arm.against].kind != arm.kind:
        raise ValueError(
            f"arm {arm.id!r}: `against` names a {ARM_CATALOG[arm.against].kind} arm, "
            f"and this one is {arm.kind}"
        )


def refuse_lend_outside_l3(arm: Arm) -> None:
    """Refuse a `lend` statement on an arm that has no lanes.

    A forwarding service's channels come from a pool and are not allotted, so
    `lend` would be a setting the topology writer drops on the floor — and an arm
    that states a knob nothing reads is an arm whose name describes a method it
    did not use.
    """
    if not arm.lend and arm.kind != "l3":
        raise ValueError(
            f"arm {arm.id!r}: `lend` is an L3 claim's lane policy, and a "
            f"{arm.kind} arm has no lanes to lend"
        )


def refuse_misplaced_condition(condition: Condition, leg: str) -> None:
    """Refuse a condition imposed on a leg where it cannot mean anything.

    `udp_blocked` is how a run asks for the data plane's *fallback*: it drops
    UDP on the leg the carrier crosses. Imposed on the visitor's leg it would
    block the payload instead and the carrier would come up as usual — a run
    whose method says "the fallback" while every packet rode KCP. The refusal
    names the leg to use rather than letting the number lie.
    """
    if (condition.block_udp or condition.udp_limit is not None) and leg != "tunnel":
        raise ValueError(
            f"the {condition.name} condition drops UDP on the leg the carrier crosses; "
            f"impose it on the `tunnel` leg (got {leg!r}), or it blocks the payload "
            "instead of the carrier"
        )


def refuse_carrier_arm(arm: Arm) -> None:
    """Refuse an arm that names a carrier.

    The data plane's carrier is chosen at runtime (KCP, falling back to TCP), so
    there is no key to write and no arm field to set: a run that means "the
    fallback" asks for the `udp_blocked` condition, and one that means "KCP
    alone" sets `MOLEHILL_DATA_CARRIER=kcp` in the environment it launches the
    bench from (the results record it as instrumentation). An arm that named a
    carrier anyway would measure the product's own choice while its name said
    otherwise — the failure the arm parser exists to prevent.
    """
    if arm.data_carrier not in ("", "auto"):
        raise ValueError(
            f"arm {arm.id!r}: the carrier is chosen at runtime; use the `udp_blocked` "
            "condition for the fallback, not an arm field"
        )


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class Scenario:
    """One question, the workload that answers it, and its budget.

    `control` says whether the scenario is meaningful with no tool in the path.
    The engine runs every control-capable scenario on the control arm as well
    and refuses to call a difference a claim without it: without that arm a slow
    probe and a slow tunnel are the same reading. A scenario whose `control` is
    False is `diagnostic` — it reports numbers and never a verdict.
    """

    id: str
    kind: str
    claim: str
    params: dict = field(default_factory=dict)
    control: bool = True
    headline: str = ""

    @property
    def diagnostic(self) -> bool:
        return not self.control

    def params_for(self, profile: dict) -> dict:
        """The scenario's parameters with the profile's overrides applied.

        A profile overrides by *name*, and an override that names a parameter
        the scenario does not have is refused: a typo must not silently measure
        the default (the run would then describe a method it did not use).
        """
        merged = dict(self.params)
        overrides = profile.get("params", {}).get(self.id, {})
        unknown = [k for k in overrides if k not in merged]
        if unknown:
            raise ValueError(
                f"profile overrides {unknown} on scenario {self.id!r}, "
                f"which has {sorted(merged)}"
            )
        merged.update(overrides)
        return merged


def profile_arms(profile: dict) -> list:
    """The arms a profile runs when the command line names none."""
    names = profile.get("arms") or ()
    return list(names)


def expected_cells(scenario_id: str, params: dict) -> list:
    """The cells a scenario must produce for one arm, in order.

    The gate checks this list, so "the run covered what it claimed" is a
    declared expectation rather than a guess from whatever arrived. A staged
    scenario expects one cell per stage *occurrence* (`clean`, `clean#2`), a
    capacity ramp expects one per level plus its summary, and the scenarios
    that name their operating point (`bulk-n`'s `P8`, `rr-16`'s `c16`, the
    ladder's `1000M`) expect exactly the cell their own parameters name —
    the workload builds the cell name from the same parameters, so
    expectation and sample cannot disagree unless this rule is wrong, which
    is what the gate exists to catch.
    """
    scenario = SCENARIOS[scenario_id]
    if scenario.kind == Kind.STAGED:
        holds = timeline_for(params["timeline"], params.get("scale", 1.0))
        seen: dict = {}
        cells = []
        for condition, _ in holds:
            seen[condition] = seen.get(condition, 0) + 1
            cells.append(
                condition if seen[condition] == 1 else f"{condition}#{seen[condition]}"
            )
        return [*cells, "run"]
    if scenario.kind == Kind.CAPACITY:
        return [f"L{n}" for n in range(1, params["streams_max"] + 1)] + ["ramp"]
    if scenario.id == "udp-ladder":
        return [f"{rate:g}M" for rate in params["rates_mbit"]]
    if named := CELL_NAMING.get(scenario.kind):
        return [named(params)]
    return [""]


def scenario_catalog() -> list:
    """Every scenario the model runs, smallest question first."""
    return [
        Scenario(
            id="bulk-1",
            kind="bulk",
            claim="what one bulk TCP stream carries, and what it costs per byte",
            params={"streams": 1, "secs": 6, "omit": 2},
            headline="throughput_gbps",
        ),
        Scenario(
            id="bulk-n",
            kind="bulk",
            claim="whether N streams aggregate, or share one ceiling",
            params={"streams": 8, "secs": 6, "omit": 2},
            headline="throughput_gbps",
        ),
        Scenario(
            id="bulk-pair",
            kind="bulk-pair",
            claim=(
                "whether two services (or two L3 claims) each carry a full "
                "stream, which separates a per-flow ceiling from a per-host one"
            ),
            params={"streams": 1, "secs": 6, "omit": 2},
            headline="throughput_gbps",
        ),
        Scenario(
            id="rr-1",
            kind="rr",
            claim="the round-trip rate and latency of one strict request/response flow",
            params={
                "connections": 1,
                "requests": 20000,
                "size": 64,
                "fresh": False,
                "max_s": 60,
            },
            headline="rtt_p99_ms",
        ),
        Scenario(
            id="rr-16",
            kind="rr",
            claim="the same, 16 flows at once: aggregate rate and tail latency",
            params={
                "connections": 16,
                "requests": 3000,
                "size": 64,
                "fresh": False,
                "max_s": 120,
            },
            headline="rate_per_s",
        ),
        Scenario(
            id="churn-16",
            kind="rr",
            claim=(
                "a fresh connection per request: the setup cost a visitor pays, "
                "and the per-visitor state the architecture keeps"
            ),
            # Enough fresh connections to price them: measured at 0.1 s for a
            # single round of setups, the CPU counters (100 Hz) resolve nothing,
            # and the A/A twin said so - "-100 %" on cpu_cores.
            params={
                "connections": 16,
                "requests": 400,
                "size": 64,
                "fresh": True,
                "max_s": 120,
            },
            headline="rate_per_s",
        ),
        Scenario(
            id="udp-pace",
            kind="udp",
            claim="a paced UDP session: what fraction arrives, and how it is spaced",
            # One datagram outstanding at a time, so the number is a rate the
            # request/response shape can actually offer - the high-rate regime
            # is the ladder's, where the sink is iperf3.
            params={
                "datagrams_per_s": 2000,
                "datagrams": 6000,
                "size": 1200,
                "max_s": 60,
            },
            headline="udp_loss_pct",
        ),
        Scenario(
            id="udp-ladder",
            kind="udp-ladder",
            claim="where a UDP path starts shedding, per offered rate",
            params={"rates_mbit": (200, 1000, 2000, 5000), "secs": 3, "size": 1200},
            headline="udp_loss_pct",
        ),
        Scenario(
            id="timeline",
            kind="staged",
            claim=(
                "how the tool behaves as the path degrades and recovers: one "
                "interactive stream, N bulk streams, a churn stream and a UDP "
                "session, over a scripted schedule of conditions, in place"
            ),
            params={
                "timeline": "sweep",
                "scale": 1.0,
                "streams": 4,
                "ping_interval_ms": 50,
                "udp_interval_ms": 20,
                "churn_per_s": 16,
                "min_hold_s": 20.0,
            },
            headline="rtt_p99_ms",
        ),
        Scenario(
            id="soak",
            kind="staged",
            claim=(
                "over a long run on a rotating path: does anything leak, drift "
                "or degrade"
            ),
            params={
                "timeline": "soak",
                "scale": 1.0,
                "streams": 4,
                "ping_interval_ms": 50,
                "udp_interval_ms": 20,
                "churn_per_s": 16,
                "min_hold_s": 20.0,
            },
            headline="rtt_p99_ms",
        ),
        Scenario(
            id="cost",
            kind="staged",
            claim=(
                "at one fixed operating point, how many CPU-seconds one carried "
                "Gbit/s costs"
            ),
            params={
                "timeline": "single",
                "scale": 1.0,
                "streams": 6,
                "ping_interval_ms": 50,
                "udp_interval_ms": 20,
                "churn_per_s": 16,
                "min_hold_s": 20.0,
            },
            headline="cpu_s_per_gbit",
        ),
        Scenario(
            id="capacity",
            kind="capacity",
            claim=(
                "how much bulk load the tool carries while a fresh interactive "
                "connection still meets the SLO"
            ),
            params={
                "streams_max": 8,
                "settle_s": 6.0,
                "ping_interval_ms": 50,
                "condition": "clean",
            },
            headline="capacity_streams",
        ),
        Scenario(
            id="reconnect",
            kind="reconnect",
            claim=(
                "what a visitor pays after a restart: time from process start to "
                "answering"
            ),
            params={"reps": 5, "timeout_s": 30.0},
            headline="startup_s",
        ),
        Scenario(
            id="udp-blast",
            kind="udp",
            claim="the datagram ceiling when the probe offers as fast as it can",
            params={
                "datagrams_per_s": 0,
                "datagrams": 40000,
                "size": 1200,
                "max_s": 60,
            },
            headline="udp_recv_mbit",
        ),
    ]


SCENARIOS: dict[str, Scenario] = {s.id: s for s in scenario_catalog()}


# --------------------------------------------------------------------------
# Profiles
# --------------------------------------------------------------------------
#: A profile is a time budget with a method attached: which scenarios run, how
#: many rounds each arm is measured for, and the workload sizes the scenarios
#: take. `budget_s` is the profile's own estimate for one arm across every
#: scenario it names, measured on the host the model was built on — the run
#: prints the estimate beside the actual cost, so a profile that has drifted is
#: visible instead of assumed.
PROFILES: dict[str, dict] = {
    "smoke": {
        "what": "the fast loop: every headline metric, seconds not minutes",
        "rounds": 2,
        "warmup_rounds": 1,
        "scenarios": ("bulk-1", "rr-1", "rr-16", "udp-pace"),
        "budget_s": 45.0,
        "aa": False,
        "params": {
            "bulk-1": {"secs": 3, "omit": 1},
            "rr-1": {"requests": 3000},
            "rr-16": {"connections": 16, "requests": 300},
            "udp-pace": {"datagrams": 3000, "datagrams_per_s": 1000},
        },
    },
    "dev": {
        "what": "the default for an optimization: every scenario, minutes",
        "rounds": 4,
        "warmup_rounds": 1,
        "scenarios": (
            "bulk-1",
            "bulk-n",
            "bulk-pair",
            "rr-1",
            "rr-16",
            "churn-16",
            "udp-pace",
            "udp-ladder",
        ),
        "budget_s": 150.0,
        "aa": True,
        "params": {
            "bulk-1": {"secs": 5, "omit": 2},
            "bulk-n": {"streams": 8, "secs": 5, "omit": 2},
            "bulk-pair": {"secs": 5, "omit": 2},
            "rr-1": {"requests": 60000},
            "rr-16": {"connections": 16, "requests": 3000},
            "churn-16": {"connections": 16, "requests": 400},
            "udp-ladder": {"secs": 3},
        },
    },
    "full": {
        "what": "the release-grade sweep: everything, for as long as it takes",
        "rounds": 6,
        "warmup_rounds": 1,
        "scenarios": tuple(s.id for s in SCENARIOS.values()),
        "budget_s": 420.0,
        "aa": True,
        "params": {
            "bulk-1": {"secs": 10, "omit": 2},
            "bulk-n": {"streams": 8, "secs": 10, "omit": 2},
            "bulk-pair": {"secs": 10, "omit": 2},
            "rr-1": {"requests": 120000},
            "rr-16": {"connections": 16, "requests": 5000},
            "churn-16": {"connections": 32, "requests": 1000},
            "udp-pace": {"datagrams": 12000},
            "udp-ladder": {"rates_mbit": (200, 1000, 2000, 5000, 8000), "secs": 4},
            "udp-blast": {"datagrams": 60000},
        },
    },
}

#: The rest of the profiles are the sweep's own shapes. Each one is a budget:
#: `stage` answers a shape question in two minutes, `soak` is the drift axis,
#: `screen` is the interleaved A/B, and `sweep` is what a release publishes.
PROFILES |= {
    "stage": {
        "what": "one condition, the whole workload: a shape question, minutes",
        "rounds": 2,
        "warmup_rounds": 1,
        "scenarios": ("cost",),
        "budget_s": 150.0,
        "aa": True,
        "arms": ("control", "molehill", "l3"),
        "params": {"cost": {"scale": 1.0, "streams": 4}},
    },
    "soak": {
        "what": "the drift axis: a rotating path held for a long time",
        "rounds": 1,
        "warmup_rounds": 0,
        "scenarios": ("soak",),
        "budget_s": 1200.0,
        "aa": False,
        "arms": ("molehill",),
        "params": {"soak": {"scale": 1.0, "streams": 4}},
    },
    "screen": {
        "what": "a development A/B: one condition, the two builds interleaved",
        "rounds": 3,
        "warmup_rounds": 1,
        "scenarios": ("cost",),
        "budget_s": 240.0,
        "aa": False,
        "arms": (),
        "params": {"cost": {"scale": 0.5, "streams": 4}},
    },
    # A diagnostic, not a gate: the same staged timeline with the bulk spine
    # removed. It exists to answer one question — whether an L3 wedge is
    # contention with a bulk claim sharing the carrier — and the answer is read
    # by comparing its interactive stream against the sweep's own. The two runs
    # are otherwise identical, so anything that differs is the bulk workload.
    # `streams: 0` is the whole point and the spine handles it by dialing
    # nothing: the staged runner still measures the interactive, UDP and churn
    # series for every stage.
    "nobulk": {
        "what": "the same staged timeline with the bulk spine removed",
        "rounds": 2,
        "warmup_rounds": 1,
        "aa": False,
        "arms": ("l3",),
        "scenarios": ("timeline",),
        "budget_s": 300.0,
        "params": {"timeline": {"scale": 0.3, "streams": 0}},
    },
    # A diagnostic, not a gate: one condition, held long, with the bulk spine
    # off. It exists to answer one question — whether the L3 wedge is the
    # carrier's own behaviour at that condition or contention with a bulk claim
    # sharing it — and the answer is read from the interactive series beside the
    # product's own KCP and L3 stats lines. `streams: 0` removes the contention;
    # the `wedge` timeline keeps the delay and gives it time.
    "carrier": {
        "what": (
            "the L3 wedge's own timeline with the bulk spine off: "
            "is the carrier alone enough?"
        ),
        "rounds": 3,
        "warmup_rounds": 0,
        "aa": False,
        "arms": ("l3",),
        "scenarios": ("timeline",),
        "budget_s": 240.0,
        "params": {"timeline": {"scale": 1.0, "streams": 0, "timeline": "wedge"}},
    },
    "sweep": {
        # The product's own two architectures first, then the reference tools.
        # `l3` has no peer: frp, rathole and nps are forwarding proxies with no
        # TUN path, so that arm's comparison is against `molehill` (L4) and the
        # control — the same workload, the same topology, one architecture apart.
        # It earns its place because L3 is what this release is *for*, and a
        # release artifact that omits it cannot support the release's own claim.
        "what": "the release sweep: the staged schedule and the load ramp",
        "rounds": 1,
        "warmup_rounds": 0,
        "scenarios": ("timeline", "capacity"),
        "budget_s": 3000.0,
        "aa": False,
        "arms": ("molehill", "l3", "frp", "rathole", "nps"),
        "params": {},
    },
}

DEFAULT_PROFILE = "smoke"


def profile_of(name: str, args) -> dict:
    """The profile with the command line's explicit overrides applied."""
    if name not in PROFILES:
        raise SystemExit(
            f"unknown profile {name!r}; known: {', '.join(PROFILES)} "
            "(see `just bench list`)"
        )
    profile = dict(PROFILES[name])
    profile = {**profile, "params": {k: dict(v) for k, v in profile["params"].items()}}
    if getattr(args, "rounds", None):
        profile["rounds"] = args.rounds
    if getattr(args, "warmup_rounds", None) is not None:
        profile["warmup_rounds"] = args.warmup_rounds
    scale = getattr(args, "scale", None)
    if scale:
        # A staged scenario's budget is its timeline's holds: scaling them is
        # how a full sweep becomes a five-minute look without changing the
        # *shape* of the schedule, and it is recorded in the fingerprint like
        # every other method parameter.
        for scenario in SCENARIOS.values():
            if scenario.kind == Kind.STAGED:
                profile["params"].setdefault(scenario.id, {})
                profile["params"][scenario.id]["scale"] = scale
    budget = getattr(args, "budget_s", None)
    del budget  # consumed by the caller, kept out of the fingerprint
    return profile


class Kind:
    """The workload kinds, as a namespace the runner dispatches on."""

    BULK: ClassVar[str] = "bulk"
    BULK_PAIR: ClassVar[str] = "bulk-pair"
    RR: ClassVar[str] = "rr"
    UDP: ClassVar[str] = "udp"
    UDP_LADDER: ClassVar[str] = "udp-ladder"
    STAGED: ClassVar[str] = "staged"
    CAPACITY: ClassVar[str] = "capacity"
    RECONNECT: ClassVar[str] = "reconnect"

    ALL: ClassVar[tuple] = (
        BULK,
        BULK_PAIR,
        RR,
        UDP,
        UDP_LADDER,
        STAGED,
        CAPACITY,
        RECONNECT,
    )


#: How the parameterized *kinds* name the cells they produce — by kind, not
#: by scenario id, because several scenarios share a runner (bulk-1 and
#: bulk-n are the same workload at different widths, rr-1 and rr-16 likewise)
#: and the runner names the cell from its own parameters. The workload builds
#: the cell from these same parameters, so the gate's expectation and the
#: sample cannot disagree unless this table is wrong — which is the failure
#: the coverage check exists to catch.
CELL_NAMING: dict = {
    Kind.BULK: lambda p: f"P{p['streams']}",
    Kind.BULK_PAIR: lambda _p: "pair",
    Kind.RR: lambda p: f"c{p['connections']}",
    Kind.UDP: lambda p: (
        "blast" if not p.get("datagrams_per_s") else f"{p['datagrams_per_s']:g}/s"
    ),
}


def validate_scenarios(names: list) -> list:
    """Refuse an unknown scenario name instead of measuring a default."""
    unknown = [n for n in names if n not in SCENARIOS]
    if unknown:
        raise SystemExit(
            f"unknown scenario(s): {unknown}; known: {list(SCENARIOS)} "
            "(see `just bench list`)"
        )
    return names
