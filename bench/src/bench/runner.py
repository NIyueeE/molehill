#!/usr/bin/env python3
"""The campaign runner: one topology, one rotation, one results file.

A run is a sequence of rounds; in each round every arm takes its turn, and the
turn order rotates, so an arm's position in the round cannot become a finding.
Within a turn the arm's daemons start once and every scenario runs back to back
— starting a process pair per workload would price process startup, not the
path — and the daemons stop at the end of the turn so each round is a replicate
of the same cold-to-warm path.

What the runner guarantees, because a measurement that cannot be reproduced is
not a measurement:

* **the whole method is recorded before the first sample.** The fingerprint is
  a hash over the topology, the scenarios and their resolved parameters, the
  instrument cadences and the engine's own code; two results files may only be
  compared when it matches (see `analysis.comparability`).
* **every artifact is kept.** Each cell names the commands it ran, the counters
  it read and the raw log it left behind, under the work directory.
* **the results file is written as the run goes.** An interrupted campaign is
  still analysable, and the samples that did happen are on disk.
* **teardown is unconditional.** The namespaces, the TUN devices and every
  daemon go away in a `finally`, including on an exception or a signal.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import time
import traceback
from dataclasses import asdict, dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path

from bench import analysis, hostinfo, model, paths, topology, workloads
from bench import instruments as inst
from bench.targets import adapter_for, peer_version
from bench.targets.base import DeploymentContext, Ports

#: The results file's schema version. It changes when the *shape* of a file
#: changes — the analysis version (`method.analysis_version`) covers the
#: statistics instead, and the fingerprint covers the meaning.
SCHEMA = 3
HERE = Path(__file__).resolve().parent

#: The modules that decide what a number means. `engine_hash` covers exactly
#: these: a change to the launch/configuration path (`targets/`) or a probe's
#: measurement (`probes/`) is a method change; a change to the CLI, the
#: plotter, the path helpers or a diagnostic utility is not. `githooks/pre-tag`
#: stays conservative and watches the whole package.
_METHOD_MODULES = (
    "model.py",
    "analysis.py",
    "runner.py",
    "topology.py",
    "workloads.py",
    "instruments.py",
    "iperf.py",
    "hostinfo.py",
)
_METHOD_DIRS = ("targets", "probes")


class PreflightError(RuntimeError):
    """The host cannot run this model; the message says what to do about it."""


def preflight(binary: str, need_iperf: bool = True) -> dict:
    """Everything the model depends on, checked with its own command.

    A model that silently measures less than it claims is worse than one that
    refuses: each check either passes or is a `PreflightError` naming the
    remedy, and the passed set is recorded in the results.
    """
    missing: list = []
    if os.geteuid() != 0:
        missing.append(
            "root: the model's one topology is three network namespaces and two "
            "TUN devices (run it with sudo)"
        )
    missing.extend(
        f"{tool}: not on PATH (install iproute2)"
        for tool in ("ip", "ss", "tc")
        if shutil.which(tool) is None
    )
    if not Path("/dev/net/tun").exists():
        missing.append("/dev/net/tun: the transparent feature needs the tun module")
    if need_iperf and shutil.which("iperf3") is None:
        missing.append("iperf3: not on PATH (install it, or run --scenarios rr-1)")
    if not Path(binary).exists():
        missing.append(f"{binary}: no such binary (build it, or pass --binary)")
    if missing:
        raise PreflightError(
            "the model cannot run here:\n  - " + "\n  - ".join(missing)
        )
    stale = topology.Topology.stale()
    if stale:
        # Not fatal: `up()` starts by removing them. Recorded because a stale
        # namespace means a previous run died without its teardown.
        pass
    return {
        "euid": os.geteuid(),
        "tools": {t: shutil.which(t) for t in ("ip", "ss", "tc", "iperf3")},
        "tun": str(Path("/dev/net/tun")),
        "stale_namespaces": stale,
    }


def engine_paths() -> list[Path]:
    """The files whose change is a method change, relative to the package."""
    paths = [HERE / name for name in _METHOD_MODULES]
    for directory in _METHOD_DIRS:
        paths.extend((HERE / directory).rglob("*.py"))
    return sorted(path.relative_to(HERE) for path in paths)


def engine_hash() -> str:
    """A hash over the engine's own code: the method is the code that ran it."""
    digest = hashlib.sha256()
    for relative in engine_paths():
        digest.update(relative.as_posix().encode())
        digest.update((HERE / relative).read_bytes())
    return digest.hexdigest()[:16]


def metrics_hash() -> str:
    """A hash over the metric definitions: a redefinition is a new method."""
    return hashlib.sha256(
        json.dumps(model.METRIC_SPECS, sort_keys=True).encode()
    ).hexdigest()[:16]


def method_record(
    profile: dict,
    scenarios: list,
    args,
    profile_name: str,
    arms: list,
) -> dict:
    """Everything that decides *what a number means* — and nothing else.

    The primary endpoints are part of the method, deliberately: a comparison
    designated before the run is a different *design* from the same comparison
    picked out afterwards, and the fingerprint is what makes the distinction
    checkable rather than a matter of trust. Two files whose endpoints differ
    are refused by `comparability`, which is the point — choosing the winner
    after seeing the numbers must change the file's identity, not just its
    story. The iperf3 version travels with it for the same reason: a version
    can change the summary fields the model reads (measured across a
    reinstall), so a number from the other side of that boundary is not
    comparable with one from this side.
    """
    return {
        "profile": profile_name,
        "engine_hash": engine_hash(),
        "metrics_hash": metrics_hash(),
        "analysis_version": analysis.ANALYSIS_VERSION,
        "rounds": profile["rounds"],
        "warmup_rounds": profile["warmup_rounds"],
        "tun_mtu": args.tun_mtu,
        "link_mtu": args.link_mtu,
        "condition": args.condition,
        "condition_leg": args.condition_leg,
        "primary": primary_endpoints(getattr(args, "primary", None), scenarios),
        "normalize": getattr(args, "normalize", "") or "",
        "slo": {
            "rtt_p99_ms": model.SLO_RTT_P99_MS,
            "error_rate_pct": model.SLO_ERROR_RATE_PCT,
        },
        "instruments": {
            "socket_poll_s": inst.SOCKET_POLL_S,
            "proc_poll_s": inst.PROC_POLL_S,
            "drift_poll_s": inst.DRIFT_POLL_S,
            "wedge_silence_s": inst.WEDGE_SILENCE_S,
            "drain_tolerance_b": topology.DRAIN_TOLERANCE_B,
            "drain_quiet_polls": topology.DRAIN_QUIET_POLLS,
            "clk_tck": topology.CLK_TCK,
            "iperf_omit_default": 2,
            "iperf_version": iperf_version(),
        },
        # The peer tools' resolved versions, beside the numbers they produced:
        # a reference-tool version is as much a part of what a peer number
        # means as the iperf3 version is for the samples it instruments (the
        # fetcher resolves it from the tool's own output, falling back to its
        # release marker; an unreadable version is an empty string rather
        # than a guess). Molehill's own version is the binary's provenance
        # line, not the method's, so it is not repeated here.
        "peer_versions": {
            arm.tool: peer_version(
                arm.tool, Path(getattr(args, "peer_dir", paths.DEFAULT_PEER_DIR))
            )
            for arm in arms
            if getattr(arm, "tool", None)
        },
        "scenarios": [
            {"id": s.id, "kind": s.kind, "params": s.params_for(profile)}
            for s in scenarios
        ],
        "udp_probe_rcvbuf": 4 << 20,
    }


def primary_endpoints(specs, scenarios: list) -> list:
    """The run's pre-registered comparisons, as data, validated against the run.

    `--primary metric=throughput_gbps,scenario=bulk-n[,arm=l4][,cell=P1]` may
    be repeated. Every name is checked against what this run will actually
    measure — a typo would otherwise pre-register a comparison that never
    happens and silence the honest ones. Empty (the default): the scenarios'
    headline metrics are the primary set, which is the model's standing
    declaration and needs no command line to say it.
    """
    if not specs:
        return []
    known_metrics = set(model.metric_ids())
    known_scenarios = {s.id for s in scenarios}
    out: list = []
    for spec in specs:
        fields: dict = {}
        for part in str(spec).split(","):
            if "=" not in part:
                raise SystemExit(
                    f"--primary {spec!r}: every field is key=value "
                    "(metric=, scenario=, arm=, cell=)"
                )
            key, value = (s.strip() for s in part.split("=", 1))
            fields[key] = value
        metric = fields.get("metric", "")
        if metric not in known_metrics:
            raise SystemExit(
                f"--primary {spec!r}: unknown metric {metric!r}; "
                f"known: {', '.join(sorted(known_metrics))}"
            )
        scenario = fields.get("scenario", "")
        if scenario not in known_scenarios:
            raise SystemExit(
                f"--primary {spec!r}: scenario {scenario!r} is not in this run; "
                f"known: {', '.join(sorted(known_scenarios))}"
            )
        out.append(
            {
                "metric": metric,
                "scenario": scenario,
                "arm": fields.get("arm", ""),
                "cell": fields.get("cell", ""),
                "direction": fields.get("direction", ""),
            }
        )
    return out


def iperf_version() -> str:
    """What iperf3 this run would use, or `""` when none is installed.

    Recorded in the method: iperf3 changed the UDP summary fields the model
    reads across a version boundary (measured, HANDOFF's environment notes),
    so the version is as much a part of what a UDP number means as the
    workload's parameters are.
    """
    iperf3 = shutil.which("iperf3")
    if iperf3 is None:
        return ""
    try:
        r = subprocess.run(
            [iperf3, "--version"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        first = (r.stdout or r.stderr).strip().splitlines()
        return first[0][:60] if first else ""
    except (OSError, subprocess.SubprocessError):
        return ""


def fingerprint(method: dict) -> str:
    return hashlib.sha256(
        json.dumps(method, sort_keys=True, default=str).encode()
    ).hexdigest()[:16]


def resolve_scenarios(args, profile: dict) -> list:
    wanted = (
        model.validate_scenarios([s.strip() for s in args.scenarios.split(",")])
        if args.scenarios
        else list(profile["scenarios"])
    )
    return [model.SCENARIOS[s] for s in wanted]


def resolve_arms(args, profile: dict | None = None) -> list:
    """The arms a run measures: catalog names, explicit specs, or the default.

    `--arms` names the catalog; `--arm` states an arm as data, so a knob the
    catalog does not carry is still declared (and therefore recorded) rather
    than patched into the source.
    """
    if args.arm:
        arms = [model.parse_arm_spec(spec) for spec in args.arm]
    elif getattr(args, "arms", ""):
        names = [n.strip() for n in args.arms.split(",") if n.strip()]
        arms = model.arms_from_names(names, args.binary)
    elif profile and model.profile_arms(profile):
        arms = model.arms_from_names(model.profile_arms(profile), args.binary)
    else:
        arms = model.default_arms()
    arms = [a if a.binary else _with_binary(a, args.binary) for a in arms]
    if args.ab_arm:
        if not args.binary_b:
            raise SystemExit("--ab-arm needs --binary-b: an A/B is two builds")
        base = next((a for a in arms if a.id == args.ab_arm), None)
        if base is None:
            raise SystemExit(
                f"--ab-arm {args.ab_arm!r} is not in the arm list "
                f"({[a.id for a in arms]})"
            )
        arms.append(_with_binary(base, args.binary_b, arm_id=f"{base.id}~b", side="B"))
        arms = [_set_side(a, "A" if a.id == base.id else a.side) for a in arms]
    ids = [a.id for a in arms]
    if len(set(ids)) != len(ids):
        raise SystemExit(f"duplicate arm ids: {ids}")
    return arms


def _with_binary(arm: model.Arm, binary: str, arm_id: str = "", side: str = ""):
    """The same arm on another build.

    `replace` rather than a field-by-field constructor: every field of the arm
    is part of what it measures, so a new axis must never be able to go missing
    here (the carrier axis was added by exactly this rule).
    """
    return replace(arm, id=arm_id or arm.id, binary=binary, side=side or arm.side)


def _set_side(arm: model.Arm, side: str):
    return replace(arm, side=side or arm.side)


def with_aa(arms: list, enabled: bool) -> list:
    """Add the A/A twin: the same arm, measured twice, under two names.

    It is the run's own noise floor, and it is the reason a verdict may say
    "indistinguishable" instead of "1.4 % better": without a repeated
    measurement of the *same* configuration, a difference inside the
    instrument's own scatter is indistinguishable from a change.
    """
    if not enabled:
        return arms
    base = next((a for a in arms if a.is_tool), None)
    if base is None:
        return arms
    return [*arms, replace(base, id=f"{base.id}~aa")]


@dataclass
class Campaign:
    args: argparse.Namespace
    profile: dict
    profile_name: str
    arms: list
    scenarios: list
    work: Path
    out: Path
    method: dict
    results: dict = field(default_factory=dict)
    topo: topology.Topology | None = None
    backend: workloads.Backend | None = None

    def save(self) -> None:
        self.results["meta"]["updated"] = _now()
        _write_json(self.out, self.results)


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


def _write_json(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=1, default=str))
    tmp.replace(path)


def probe_startup(reps: int = 3) -> float:
    """What starting a probe costs, measured rather than subtracted.

    The round-trip rate's window is the probe's own (it starts its clock after
    the interpreter is up), so this is not a correction — it is recorded so a
    reader can see how much of a *short* arm the interpreter would have been.
    """
    values: list = []
    for _ in range(reps):
        t0 = time.perf_counter()
        subprocess.run(
            topology.Topology.ns_argv(topology.VIS_NS, [sys.executable, "-c", "pass"]),
            check=False,
            capture_output=True,
        )
        values.append(time.perf_counter() - t0)
    return round(statistics.median(values), 4)


class ArmTurn:
    """One arm's turn: its daemons, its configs, and its logs."""

    def __init__(self, camp: Campaign, arm: model.Arm, rnd: int):
        self.camp, self.arm, self.rnd = camp, arm, rnd
        self.dir = camp.work / f"round{rnd}" / f"arm-{arm.id}"
        self.dir.mkdir(parents=True, exist_ok=True)
        self.procs: dict = {}
        self.logs: dict = {}
        self.config: dict = {}

    def start(self) -> tuple:
        """Start the daemons and wait for the *visitor's* first packet.

        Readiness is the workload's own first connection, not a process being
        up: a client can run perfectly while failing to register, and a control
        arm has no daemons at all.
        """
        topo = self._topology()
        topo.set_txqueuelen(self.arm.txqueuelen)
        deployment = adapter_for(self.arm).deploy(self.arm, self._context())
        self.config = dict(deployment.configs)
        for launch in deployment.launches:
            self._spawn(launch.role, list(launch.argv))
        ready = topo.wait_target(
            deployment.endpoint.host, deployment.endpoint.port, timeout=30.0
        )
        return self.pids(), ready

    def _topology(self) -> topology.Topology:
        """The campaign's topology, which exists before any turn starts."""
        topo = self.camp.topo
        if topo is None:
            raise RuntimeError("a turn starts after the topology is up")
        return topo

    def _context(self) -> DeploymentContext:
        return DeploymentContext(
            topology=self._topology(),
            work=self.camp.work,
            ports=Ports.from_topology(),
            peer_dir=Path(self.camp.args.peer_dir),
        )

    def restart(self) -> tuple:
        """Stop the arm and start it again, timing the visitor's first success.

        The reconnect scenario's instrument: what a visitor waits after a
        restart is not "the process is up" but "a connection is answered", so
        the clock runs to `wait_target`'s first success.
        """
        self.stop(grace=0.0)
        started = time.perf_counter()
        self.procs.clear()
        self.logs.clear()
        _, ready = self.start()
        elapsed = round(time.perf_counter() - started, 4)
        if not ready:
            return None, f"not ready within {elapsed}s"
        return elapsed, ""

    def _spawn(self, role: str, argv: list) -> None:
        path = self.dir / f"{role}.log"
        fh = path.open("ab")
        # The daemons inherit this process's environment, which is where the
        # diagnostic switches live: a run that forces a carrier
        # (`MOLEHILL_DATA_CARRIER=mhd`) inherited it from the shell that started
        # the bench, and the results record inherited `MOLEHILL_*` switches as
        # instrumentation, so the forcing is auditable rather than implied.
        env = dict(os.environ)
        self.procs[role] = subprocess.Popen(argv, stdout=fh, stderr=fh, env=env)
        self.logs[role] = path

    def pids(self) -> dict:
        return {r: p.pid for r, p in self.procs.items() if p.poll() is None}

    def tail(self, lines: int = 8) -> dict:
        out = {}
        for role, path in self.logs.items():
            with contextlib.suppress(OSError):
                text = path.read_text(errors="replace").splitlines()[-lines:]
                out[role] = "\n".join(text)[:2000]
        return out

    def stop(self, grace: float = 0.3) -> None:
        for p in self.procs.values():
            with contextlib.suppress(OSError):
                p.kill()
        for p in self.procs.values():
            with contextlib.suppress(Exception):
                p.wait(timeout=5)
        # A killed daemon can hold a port briefly; the next arm must be able to
        # bind it, and an EADDRINUSE would look like a tool failure. A restart
        # that is being timed passes `grace=0`: the wait is exactly what it
        # measures.
        time.sleep(grace)


def arm_turn(camp: Campaign, arm: model.Arm, rnd: int, warmup: bool) -> list:
    """One arm's turn: every scenario, once, with its counters around it."""
    topo = camp.topo
    if topo is None:
        raise RuntimeError("a scenario runs after the topology is up")
    turn = ArmTurn(camp, arm, rnd)
    samples: list = []
    _, ready = turn.start()
    try:
        if not ready:
            reason = (
                f"not ready: no visitor could reach {arm.dial_host}:"
                f"{topology.ECHO_PORT}"
            )
            samples += [
                _sample(
                    _who(arm, rnd, warmup, scenario),
                    workloads.Cell("", False, reason=reason),
                )
                for scenario in camp.scenarios
            ]
            _log(f"    [{arm.id}] FAIL {reason}")
            _log(f"    logs: {json.dumps(turn.tail(3))[:400]}")
            return samples
        for scenario in camp.scenarios:
            if not arm.is_tool and not scenario.control:
                # A diagnostic scenario has no tool-free path to measure: the
                # control arm would be measuring a different workload.
                continue
            params = scenario.params_for(camp.profile)
            who = _who(arm, rnd, warmup, scenario)
            ctx = workloads.Ctx(
                topo=topo,
                pids_of=turn.pids,
                work=turn.dir,
                arm=arm,
                probe_startup_s=camp.results["meta"]["probe_startup_s"],
                peers_of=_cell_peers(camp.backend),
                leg=camp.args.condition_leg,
                condition=model.CONDITIONS[camp.args.condition],
                restart_arm=turn.restart,
            )
            t0 = time.perf_counter()
            try:
                cells = workloads.run(scenario.kind, ctx, params)
            except Exception as exc:  # noqa: BLE001 — one cell must not kill a run
                reason = f"{type(exc).__name__}: {exc}"[:400]
                cells = [workloads.Cell("", False, reason=reason)]
            cost = round(time.perf_counter() - t0, 3)
            for cell in cells:
                cell.evidence["scenario_cost_s"] = cost
                samples.append(_sample(who, cell))
            headline = _headline(cells, scenario)
            _log(f"    [{arm.id}] {scenario.id} {cost:6.2f}s  {headline}")
    finally:
        turn.stop()
    return samples


def _headline(cells: list, scenario: model.Scenario) -> str:
    """One line per scenario in the run log: the metric that answers it."""
    parts = []
    for cell in cells:
        value = cell.metrics.get(scenario.headline)
        metric = model.METRICS.get(scenario.headline)
        shown = metric.format(value) if metric else str(value)
        suffix = f" {metric.unit}" if metric and value is not None else ""
        if not cell.ok:
            shown, suffix = f"FAILED ({cell.reason[:60]})", ""
        label = f"{cell.cell}: " if cell.cell else ""
        parts.append(f"{label}{shown}{suffix}")
    return "; ".join(parts)


def _cell_peers(backend):
    """A reader of the peers the backend announces *from now on*.

    The backend logs one `PEER ip:port` per accepted connection, and it is the
    transparency evidence: on an L3 arm the backend must see the visitor. Read
    cumulatively it would carry every earlier cell's connections; read this way
    a cell records its own, which is what makes the evidence readable.
    """
    if backend is None:
        return None
    before = set(backend.peers())
    return lambda: sorted(set(backend.peers()) - before)


def _who(arm: model.Arm, rnd: int, warmup: bool, scenario: model.Scenario) -> dict:
    """Where a sample came from: the five fields that identify it."""
    return {
        "arm": arm.id,
        "round": rnd,
        "warmup": warmup,
        "scenario": scenario.id,
        "kind": scenario.kind,
    }


def _sample(who: dict, cell: workloads.Cell) -> dict:
    return who | {
        "cell": cell.cell,
        "ok": bool(cell.ok),
        "reason": cell.reason,
        "metrics": cell.metrics,
        "unavailable": cell.unavailable,
        "evidence": cell.evidence,
    }


def _log(*a) -> None:
    print(*a, flush=True)


def run_campaign(camp: Campaign) -> dict:
    args = camp.args
    _log(f"bench: profile {camp.profile_name} — {camp.profile['what']}")
    _log(
        f"  arms: {', '.join(a.id for a in camp.arms)}"
        f"   scenarios: {', '.join(s.id for s in camp.scenarios)}"
    )
    _log(
        f"  {camp.profile['rounds']} measured rounds "
        f"(+{camp.profile['warmup_rounds']} warm-up), fingerprint "
        f"{camp.results['meta']['fingerprint']}"
    )
    if args.hypothesis:
        _log(f"  hypothesis: {args.hypothesis}")
    total = camp.profile["rounds"] + camp.profile["warmup_rounds"]
    for rnd in range(total):
        warmup = rnd < camp.profile["warmup_rounds"]
        order = camp.arms[rnd % len(camp.arms) :] + camp.arms[: rnd % len(camp.arms)]
        _log(
            f"  round {rnd}{' (warm-up)' if warmup else ''}: "
            f"{' -> '.join(a.id for a in order)}"
        )
        for arm in order:
            samples = arm_turn(camp, arm, rnd, warmup)
            camp.results["samples"].extend(samples)
            camp.save()
    return camp.results


def build_results(camp: Campaign, pre: dict, provenance: dict, started: str) -> dict:
    return {
        "meta": {
            "model": "bench",
            "schema": SCHEMA,
            "runner": "bench/src/bench/cli.py",
            "fingerprint": fingerprint(camp.method),
            "method": camp.method,
            "profile": camp.profile_name,
            "profile_what": camp.profile["what"],
            "rounds": camp.profile["rounds"],
            "warmup_rounds": camp.profile["warmup_rounds"],
            "budget_s": camp.profile["budget_s"],
            "hypothesis": camp.args.hypothesis,
            # The diagnostic switches this run inherited (`MOLEHILL_*`): a
            # forced carrier or a scheduled switch is part of what the numbers
            # *mean*, so it is recorded beside the method — deliberately not
            # inside it, because a declared treatment (like `--ab-arm`'s two
            # binaries) is exactly what a between-file comparison contrasts,
            # while a method key difference names files that measured
            # different things. The reader decides; the file has to say.
            "molehill_switches": {
                name: os.environ[name]
                for name in sorted(
                    {
                        "MOLEHILL_DATA_CARRIER",
                        "MOLEHILL_CARRIER_SWITCH_AT",
                        "MOLEHILL_TUNNEL_SWITCH_AT",
                        "MOLEHILL_L3_STATS",
                        "MOLEHILL_MHD_STATS",
                        "MOLEHILL_TUNNEL_STATS",
                        "MOLEHILL_POOL_STATS",
                        "MOLEHILL_STRIPE_COUNT",
                    }
                    & set(os.environ)
                )
            },
            "started": started,
            "finished": "",
            "elapsed_s": 0.0,
            "probe_startup_s": 0.0,
            "work_dir": str(camp.work),
            "preflight": pre,
            "provenance": provenance,
            "arms": [asdict(a) for a in camp.arms],
            "scenarios": [
                {
                    "id": s.id,
                    "kind": s.kind,
                    "claim": s.claim,
                    "control": s.control,
                    "headline": s.headline,
                    "params": s.params_for(camp.profile),
                }
                for s in camp.scenarios
            ],
        },
        "samples": [],
    }


def _prepare(args) -> Campaign:
    """Everything a run needs before the topology exists: method, paths, arms."""
    profile_name = args.profile
    profile = model.profile_of(profile_name, args)
    aa = profile.get("aa", False) if args.aa is None else args.aa
    arms = with_aa(resolve_arms(args, profile), aa)
    scenarios = resolve_scenarios(args, profile)
    pre = preflight(
        args.binary, need_iperf=any(s.kind != model.Kind.RR for s in scenarios)
    )
    if args.out and Path(args.out).exists() and not args.force:
        raise SystemExit(
            f"{args.out} exists; a results file is evidence, not a scratch pad "
            "(delete it, pick another --out, or pass --force)"
        )
    stamp = time.strftime("%Y%m%d-%H%M%S")
    home = paths.user_home()
    work = Path(args.work or (home / "tmp" / f"bench-{stamp}"))
    out = Path(args.out or (home / "tmp" / f"bench-{stamp}.json"))
    work.mkdir(parents=True, exist_ok=True)
    method = method_record(profile, scenarios, args, profile_name, arms)
    camp = Campaign(
        args=args,
        profile=profile,
        profile_name=profile_name,
        arms=arms,
        scenarios=scenarios,
        work=work,
        out=out,
        method=method,
    )
    _log(f"bench: work {work}")
    _log(f"bench: results {out}")
    _log("bench: host probes (the machine's own baseline)...")
    rev, clean, dirty = hostinfo.git_revision(exclude=out)
    provenance = {
        # `[revision, tree_clean]`, the shape `githooks/pre-tag` reads, plus the
        # paths themselves: a boolean nobody can check is a claim, and the list
        # of what was uncommitted is evidence.
        "revision": [rev, clean],
        "dirty_paths": dirty,
        "host": inst.host_provenance(with_calibration=not args.no_host_probes),
        "binaries": _binaries(arms),
        "engine_hash": method["engine_hash"],
    }
    camp.results = build_results(camp, pre, provenance, _now())
    return camp


def _open_topology(camp: Campaign, args) -> topology.Topology:
    """The one topology, shaped, with its backend — or a refusal."""
    topo = topology.Topology(tun_mtu=args.tun_mtu, link_mtu=args.link_mtu)
    t0 = time.perf_counter()
    topo.up()
    topo.set_condition(args.condition_leg, model.CONDITIONS[args.condition])
    camp.topo = topo
    camp.backend = workloads.Backend(topo, camp.work)
    camp.backend.start()
    if not camp.backend.wait_ready():
        raise PreflightError("the backend never announced itself")
    if not topo.wait_target(model.TOPO_CONTROL_IP, topology.ECHO_PORT):
        raise PreflightError("the control path never answered")
    _log(
        f"bench: topology up ({time.perf_counter() - t0:.1f}s), "
        f"condition {args.condition} on the {args.condition_leg} leg"
    )
    return topo


def _sigint(signum, frame):  # noqa: ARG001 — signal handlers take both
    raise KeyboardInterrupt


def run(args) -> int:
    try:
        model.refuse_misplaced_condition(
            model.CONDITIONS[args.condition], args.condition_leg
        )
    except ValueError as e:
        # A condition that cannot mean anything on the leg it was given is an
        # argument error, not a campaign failure: say so before any namespace
        # is built, and without a traceback.
        raise SystemExit(str(e)) from e
    camp = _prepare(args)
    topo = topology.Topology(tun_mtu=args.tun_mtu, link_mtu=args.link_mtu)
    t0 = time.perf_counter()
    old = signal.signal(signal.SIGINT, _sigint)
    try:
        topo = _open_topology(camp, args)
        camp.results["meta"]["probe_startup_s"] = probe_startup()
        run_campaign(camp)
        camp.results["summary"] = analysis.summarize(camp.results)
        _log("")
        _log(analysis.render(camp.results, markdown=False))
        _log("")
        _log(analysis.verdicts_text(camp.results))
    except (PreflightError, topology.TopologyError) as exc:
        _log(f"bench: FAILED - {exc}")
        camp.results["meta"]["error"] = str(exc)
        return 1
    except KeyboardInterrupt:
        _log("bench: interrupted; the samples so far are in the results file")
        camp.results["meta"]["error"] = "interrupted"
        return 130
    except Exception as exc:  # noqa: BLE001 - a crash must be evidence, not a trace
        detail = traceback.format_exc()
        _log(f"bench: FAILED - {type(exc).__name__}: {exc}")
        _log(detail)
        camp.results["meta"]["error"] = f"{type(exc).__name__}: {exc}"
        camp.results["meta"]["traceback"] = detail[-4000:]
        return 1
    finally:
        signal.signal(signal.SIGINT, old)
        _finish(camp, topo, t0)
    meta = camp.results["meta"]
    _log(
        f"bench: {meta['elapsed_s']}s for {len(camp.results['samples'])} samples "
        f"({meta['cost_s']}s per arm-round; the {camp.profile_name} profile "
        f"budgets {camp.profile['budget_s']:.0f}s)"
    )
    _log(f"bench: results in {camp.out}")
    return 0


def _finish(camp: Campaign, topo: topology.Topology, t0: float) -> None:
    """Teardown and the final write, whatever happened above."""
    if camp.backend is not None:
        camp.backend.stop()
    with contextlib.suppress(Exception):
        topo.down()
    meta = camp.results["meta"]
    meta["finished"] = _now()
    meta["elapsed_s"] = round(time.perf_counter() - t0, 1)
    meta["cost_s"] = round(
        meta["elapsed_s"] / max(1, len(camp.arms)) / max(1, camp.profile["rounds"]), 2
    )
    camp.save()


def _binaries(arms: list) -> list:
    seen: dict = {}
    for arm in arms:
        if arm.binary and arm.binary not in seen:
            seen[arm.binary] = inst.binary_provenance(arm.binary)
    return list(seen.values())
