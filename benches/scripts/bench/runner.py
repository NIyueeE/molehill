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

import contextlib
import hashlib
import json
import os
import pwd
import shutil
import signal
import statistics
import subprocess
import sys
import time
import traceback
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path

import analysis
import hostinfo
import instruments as inst
import model
import peers
import topology
import workloads

#: The engine's schema version. It changes when the *shape* of a results file
#: changes; the fingerprint covers the meaning (see `method_record`).
SCHEMA = 2
HERE = Path(__file__).resolve().parent


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


def engine_hash() -> str:
    """A hash over the engine's own code: the method is the code that ran it."""
    digest = hashlib.sha256()
    for path in sorted(HERE.rglob("*.py")):
        digest.update(path.name.encode())
        digest.update(path.read_bytes())
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
) -> dict:
    """Everything that decides *what a number means* — and nothing else."""
    return {
        "profile": profile_name,
        "engine_hash": engine_hash(),
        "metrics_hash": metrics_hash(),
        "rounds": profile["rounds"],
        "warmup_rounds": profile["warmup_rounds"],
        "tun_mtu": args.tun_mtu,
        "link_mtu": args.link_mtu,
        "condition": args.condition,
        "condition_leg": args.condition_leg,
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
        },
        "scenarios": [
            {"id": s.id, "kind": s.kind, "params": s.params_for(profile)}
            for s in scenarios
        ],
        "udp_probe_rcvbuf": 4 << 20,
    }


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
    return model.Arm(
        id=arm_id or arm.id,
        kind=arm.kind,
        mode=arm.mode,
        pool_cap=arm.pool_cap,
        txqueuelen=arm.txqueuelen,
        binary=binary,
        side=side or arm.side,
    )


def _set_side(arm: model.Arm, side: str):
    return model.Arm(
        id=arm.id,
        kind=arm.kind,
        mode=arm.mode,
        pool_cap=arm.pool_cap,
        txqueuelen=arm.txqueuelen,
        binary=arm.binary,
        side=side or arm.side,
    )


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
    twin = _with_binary(base, base.binary, arm_id=f"{base.id}~aa")
    twin = model.Arm(
        id=twin.id,
        kind=twin.kind,
        mode=twin.mode,
        pool_cap=twin.pool_cap,
        txqueuelen=twin.txqueuelen,
        binary=twin.binary,
        side=twin.side,
    )
    return [*arms, twin]


@dataclass
class Campaign:
    args: object
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


def user_home() -> Path:
    """Where a run's artifacts go: the *invoking* user's home.

    `sudo` sets `HOME=/root`, so a `sudo just bench` would drop its results in
    a directory the person who ran it cannot read. The results are theirs, so
    they land in their home.
    """
    user = os.environ.get("SUDO_USER")
    if user:
        with contextlib.suppress(KeyError, OSError):
            return Path(pwd.getpwnam(user).pw_dir)
    return Path.home()


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
        self.camp.topo.set_txqueuelen(self.arm.txqueuelen)
        if self.arm.kind == "peer":
            self._start_peer()
        elif self.arm.is_tool:
            cfg = topology.service_config(self.arm, self.camp.work)
            self.config = {
                "server": cfg["server"].read_text(),
                "client": cfg["client"].read_text(),
            }
            self._spawn(
                "server",
                self.camp.topo.ns_argv(
                    topology.SRV_NS, [self.arm.binary, "--server", str(cfg["server"])]
                ),
            )
            self._spawn(
                "client",
                self.camp.topo.ns_argv(
                    topology.CLI_NS,
                    [self.arm.binary, self.arm.mode_flag, str(cfg["client"])],
                ),
            )
        ready = self.camp.topo.wait_target(
            self.arm.dial_host, topology.ECHO_PORT, timeout=30.0
        )
        return self.pids(), ready

    def _start_peer(self) -> None:
        """Start a reference tool from its own adapter (`peers.py`).

        The adapter owns the tool's configuration language; the model owns the
        topology, the ports and the measurement. That split is what lets a peer
        be an arm like any other.
        """
        adapter = _peer_adapter(self.arm.tool)
        ports = {
            "control": topology.CONTROL_PORT,
            "echo": topology.ECHO_PORT,
            "iperf": topology.IPERF_PORT,
            "iperf2": topology.IPERF2_PORT,
            "udp": topology.UDP_PORT,
            "udp_sink": topology.IPERF_UDP_PORT,
        }
        for role, argv in adapter.write(self.dir, ports).items():
            self._spawn(role, argv)

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
        self.procs[role] = subprocess.Popen(argv, stdout=fh, stderr=fh)
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
                topo=camp.topo,
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


def _peer_adapter(tool: str):
    """The reference tool's adapter, or a refusal naming the tools that exist."""
    if tool not in peers.PEERS:
        raise PreflightError(
            f"no adapter for peer tool {tool!r}; known: {', '.join(peers.PEERS)}"
        )
    return peers.PEERS[tool]


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
            "runner": "benches/scripts/bench/bench.py",
            "fingerprint": fingerprint(camp.method),
            "method": camp.method,
            "profile": camp.profile_name,
            "profile_what": camp.profile["what"],
            "rounds": camp.profile["rounds"],
            "warmup_rounds": camp.profile["warmup_rounds"],
            "budget_s": camp.profile["budget_s"],
            "hypothesis": camp.args.hypothesis,
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
    home = user_home()
    work = Path(args.work or (home / "tmp" / f"bench-{stamp}"))
    out = Path(args.out or (home / "tmp" / f"bench-{stamp}.json"))
    work.mkdir(parents=True, exist_ok=True)
    method = method_record(profile, scenarios, args, profile_name)
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
    provenance = {
        "revision": hostinfo.git_revision(exclude=out),
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
