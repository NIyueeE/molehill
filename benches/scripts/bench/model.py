#!/usr/bin/env python3
"""The bench model's vocabulary: metrics, arms, scenarios, profiles.

This module is the standard, written down once. Everything else in
`benches/scripts/bench/` executes it, and nothing may add a metric, an arm or a
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
  the seconds it expects to spend; `bench.py doctor` and the run report print
  the budget beside the actual cost, so "fast" is a measurement too.
"""

from __future__ import annotations

from dataclasses import dataclass, field
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
        "needs": ("bulk", "bulk-pair"),
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
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 10.0),
        "precision": 3,
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
        "needs": ("rr",),
        "materiality": ("rel", 15.0),
        "precision": 3,
    },
    {
        "id": "rtt_p99_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "nearest-rank 99th percentile of the probe's round trips",
        "denominator": "one request/response pair",
        "needs": ("rr",),
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
        "needs": ("rr",),
        "materiality": ("rel", 25.0),
        "precision": 3,
    },
    {
        "id": "rtt_samples",
        "unit": "count",
        "direction": "none",
        "definition": "round trips the percentile is taken over",
        "denominator": "none (sample count)",
        "needs": ("rr", "udp"),
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
        "needs": ("udp", "udp-ladder"),
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
        "needs": ("udp", "udp-ladder"),
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
        "needs": ("udp", "udp-ladder"),
        "materiality": ("rel", 20.0),
        "precision": 3,
    },
    {
        "id": "udp_rtt_p99_ms",
        "unit": "ms",
        "direction": "lower",
        "definition": "99th percentile of the probe's datagram echo round trips",
        "denominator": "one echoed datagram",
        "needs": ("udp",),
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
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
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
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 15.0),
        "precision": 3,
    },
    {
        "id": "bytes_per_syscall",
        "unit": "B",
        "direction": "higher",
        "definition": "tool process rchar+wchar / syscr+syscw (the I/O granularity)",
        "denominator": "one read/write-family syscall",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 15.0),
        "precision": 1,
    },
    {
        "id": "syscalls_per_s",
        "unit": "1/s",
        "direction": "lower",
        "definition": "tool process syscr+syscw / measured window",
        "denominator": "the workload's measured window",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 15.0),
        "precision": 0,
    },
    {
        "id": "wire_per_visitor_byte",
        "unit": "ratio",
        "direction": "lower",
        "definition": "tunnel-link bytes (both directions) / visitor-link egress bytes",
        "denominator": "bytes the visitor offered",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
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
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
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
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 10.0),
        "precision": 1,
    },
    {
        "id": "fds_peak",
        "unit": "count",
        "direction": "lower",
        "definition": "peak open file descriptors summed over the tool's daemons",
        "denominator": "one process set",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        "materiality": ("rel", 15.0),
        "precision": 0,
    },
    {
        "id": "threads_peak",
        "unit": "count",
        "direction": "lower",
        "definition": "peak thread count summed over the tool's daemons",
        "denominator": "one process set",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
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
        "needs": ("bulk", "rr"),
        "materiality": ("abs", 1.0),
        "precision": 0,
    },
    {
        "id": "retrans_segments",
        "unit": "count",
        "direction": "lower",
        "definition": "TCP segments retransmitted by the visitor and server namespaces",
        "denominator": "one TCP segment",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
        # Counts are judged in packets, not in percent: a percentage of a small
        # retransmit count is noise, and "a thousand packets" is a finding.
        "materiality": ("abs", 1000.0),
        "precision": 0,
    },
    {
        "id": "dropped_packets",
        "unit": "count",
        "direction": "lower",
        "definition": "packets the topology's interfaces dropped over the window",
        "denominator": "one packet",
        "needs": ("bulk", "bulk-pair", "rr", "udp", "udp-ladder"),
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
    axis. `pool_cap` and `txqueuelen` are the two settings this model prices
    separately, because both are architectural choices rather than tuning: a
    capped pool separates the multiplexer's framing cost from its pool's, and
    the TUN queue length decides whether an L3 arm measures the architecture or
    the kernel's default queue.
    """

    id: str
    kind: str
    mode: str = "multiplex"
    pool_cap: int = 0
    txqueuelen: int = 1000
    binary: str = ""
    #: Free-form label recorded in the results (e.g. "A"/"B" for a build pair).
    side: str = ""

    @property
    def is_tool(self) -> bool:
        return self.kind != "control"

    @property
    def mode_flag(self) -> str:
        return "--transparent" if self.kind == "l3" else "--client"

    @property
    def data_mode(self) -> str:
        if self.kind == "l3":
            return self.mode if self.mode in ("direct", "multiplex") else "direct"
        return self.mode

    @property
    def dial_host(self) -> str:
        """The address the visitor dials: what the architecture exposes.

        It is the path under test, so it is never the backend's address: an L4
        server exposes its own listener, an L3 client exposes the address its
        TUN owns, and the control arm exposes the backend directly.
        """
        return {"control": TOPO_CONTROL_IP, "l4": TOPO_SRV_IP, "l3": TOPO_PUBLIC_IP}[
            self.kind
        ]

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
            "l3": TOPO_PUBLIC_IP,
        }[self.kind]


#: The deeper TUN queue an arm may ask for: the same architecture with one
#: operator setting changed (`ip link set <tun> txqueuelen N`).
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
    "l4-mux1": Arm("l4-mux1", "l4", pool_cap=1),
    "l4-mux2": Arm("l4-mux2", "l4", pool_cap=2),
    "l4-mux8": Arm("l4-mux8", "l4", pool_cap=8),
    "l4-direct": Arm("l4-direct", "l4", mode="direct"),
    "l3": Arm("l3", "l3", mode="direct"),
    "l3-mux": Arm("l3-mux", "l3", mode="multiplex"),
    "l3-deep": Arm("l3-deep", "l3", mode="direct", txqueuelen=DEEP_TXQUEUELEN),
}


def arms_from_names(names: list, binary: str) -> list:
    """Resolve `--arms control,l4,l3` against the catalog, or refuse."""
    unknown = [n for n in names if n not in ARM_CATALOG]
    if unknown:
        raise SystemExit(f"unknown arm(s): {unknown}; known: {', '.join(ARM_CATALOG)}")
    return [
        Arm(
            id=a.id,
            kind=a.kind,
            mode=a.mode,
            pool_cap=a.pool_cap,
            txqueuelen=a.txqueuelen,
            binary=binary,
        )
        for a in (ARM_CATALOG[n] for n in names)
    ]


def default_arms() -> list:
    """The arms a profile runs when the command line names none."""
    return [
        Arm("control", "control"),
        Arm("l4", "l4"),
        Arm("l3", "l3"),
    ]


def parse_arm_spec(spec: str) -> Arm:
    """`id=l3,mode=direct,txqueuelen=10000,side=B` — an arm as data.

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
    known = {"id", "kind", "mode", "pool_cap", "txqueuelen", "binary", "side"}
    unknown = sorted(set(fields) - known)
    if unknown:
        raise ValueError(f"unknown arm key(s) {unknown}; known: {sorted(known)}")
    for numeric in ("pool_cap", "txqueuelen"):
        if numeric in fields:
            fields[numeric] = int(fields[numeric])
    kind = fields.pop("kind", None) or fields.get("id", "")
    if kind not in ("control", "l4", "l3"):
        raise ValueError(f"unknown arm kind {kind!r}; known: control, l4, l3")
    return Arm(kind=kind, **fields)


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

DEFAULT_PROFILE = "smoke"


def profile_of(name: str, args) -> dict:
    """The profile with the command line's explicit overrides applied."""
    if name not in PROFILES:
        raise SystemExit(
            f"unknown profile {name!r}; known: {', '.join(PROFILES)} "
            "(see `bench.py list`)"
        )
    profile = dict(PROFILES[name])
    profile = {**profile, "params": {k: dict(v) for k, v in profile["params"].items()}}
    if getattr(args, "rounds", None):
        profile["rounds"] = args.rounds
    if getattr(args, "warmup_rounds", None) is not None:
        profile["warmup_rounds"] = args.warmup_rounds
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

    ALL: ClassVar[tuple] = (BULK, BULK_PAIR, RR, UDP, UDP_LADDER)


def validate_scenarios(names: list) -> list:
    """Refuse an unknown scenario name instead of measuring a default."""
    unknown = [n for n in names if n not in SCENARIOS]
    if unknown:
        raise SystemExit(
            f"unknown scenario(s): {unknown}; known: {list(SCENARIOS)} "
            "(see `bench.py list`)"
        )
    return names
