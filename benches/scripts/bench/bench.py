#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""bench: the model that measures molehill's performance, one way.

This is the entry point. The model it runs is declared in `model.py` (metrics,
arms, scenarios, profiles) and executed by `runner.py`; the rules it enforces
are in `analysis.py`. Read `docs/benchmarks.md`, "The bench model", first.

    sudo -n just bench                  # the smoke profile: every headline
                                        # metric, about a minute
    sudo -n just bench --profile dev    # the default for an optimization
    sudo -n just bench --arm id=l3,txqueuelen=10000
    sudo -n just bench --profile smoke --ab-arm l3 --binary-b /path/to/other
    just bench report ~/tmp/bench-*.json
    just bench compare a.json b.json
    just bench selfcheck                # the model's own checks, no root

Every run writes its results and its raw artifacts outside the tree (under
`~/tmp/` unless `--out`/`--work` say otherwise): a results file is evidence, and
evidence does not belong in the repository.
"""

# E402 is waived file-wide: the sys.path insert below is the bench-lib import
# pattern and must precede the sibling imports.
import os
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import argparse  # noqa: E402
import contextlib  # noqa: E402
import importlib.util  # noqa: E402
import json  # noqa: E402
import py_compile  # noqa: E402
import shutil  # noqa: E402
import subprocess  # noqa: E402

import analysis  # noqa: E402
import instruments as inst  # noqa: E402
import model  # noqa: E402
import runner  # noqa: E402
import topology  # noqa: E402
import workloads  # noqa: E402


def cmd_list(args) -> int:
    del args
    print("metrics (every one defined once, in model.METRICS):")
    print(f"  {'id':<26} {'unit':<8} {'direction':<26} definition")
    for metric in model.METRICS.values():
        print(
            f"  {metric.id:<26} {metric.unit:<8} {metric.better:<26} "
            f"{metric.definition}"
        )
    print("\nscenarios (each declares the claim it supports and its control):")
    for scenario in model.SCENARIOS.values():
        control = "control" if scenario.control else "DIAGNOSTIC (no verdict)"
        print(f"  {scenario.id:<12} {scenario.kind:<11} {control}")
        print(f"      claim: {scenario.claim}")
        print(f"      params: {json.dumps(scenario.params, default=str)}")
    print("\narms (the catalog; `--arm id=...` may state any of them):")
    for name, arm in model.ARM_CATALOG.items():
        print(
            f"  {name:<12} kind={arm.kind:<8} "
            f"carrier={arm.data_carrier or '-':<5} "
            f"pool_cap={arm.pool_cap or 'default':<8} txqueuelen={arm.txqueuelen}"
        )
    print("\nprofiles (a profile is a time budget with a method attached):")
    for name, profile in model.PROFILES.items():
        print(
            f"  {name:<8} {profile['rounds']} rounds "
            f"(+{profile['warmup_rounds']} warm-up), ~{profile['budget_s']:.0f}s "
            f"per arm, A/A {'on' if profile.get('aa') else 'off'}"
        )
        print(f"      {profile['what']}")
        print(f"      scenarios: {', '.join(profile['scenarios'])}")
    return 0


def cmd_doctor(args) -> int:
    print(f"binary: {args.binary}")
    try:
        pre = runner.preflight(args.binary)
    except runner.PreflightError as exc:
        print(f"NOT READY\n{exc}")
        return 1
    print(f"root: euid {pre['euid']}")
    for tool, path in pre["tools"].items():
        print(f"tool: {tool:<8} {path or 'MISSING'}")
    print(f"tun:  {pre['tun']}")
    if pre["stale_namespaces"]:
        print(
            f"stale namespaces: {', '.join(pre['stale_namespaces'])} "
            "(a previous run died; `bench run` removes them itself)"
        )
    provenance = inst.binary_provenance(args.binary)
    print(
        f"build: {provenance['version']} sha256 {provenance['sha256']} "
        f"{provenance['bytes']} bytes stale={provenance['stale']}"
    )
    if provenance.get("stale"):
        print("WARNING: the binary is older than the sources it was built from")
    host = inst.host_provenance(with_calibration=not args.no_host_probes)
    identity = host["identity"]
    print(
        f"host: {identity.get('hostname')} id {identity.get('host_id')} "
        f"kernel {host['kernel']}"
    )
    for probe in ("calibration", "loopback"):
        read = host.get(probe) or {}
        if read.get("ok"):
            print(
                f"probe {probe}: {read.get('median')} {read.get('unit')} "
                f"(spread {read.get('spread_pct')} %)"
            )
        else:
            print(f"probe {probe}: UNAVAILABLE ({read.get('reason', 'not run')})")
    return 0


def cmd_run(args) -> int:
    return runner.run(args)


def cmd_report(args) -> int:
    results = analysis.load(args.file)
    meta = results.get("meta", {})
    print(
        f"{meta.get('profile')} run, fingerprint {meta.get('fingerprint')}, "
        f"{len(results.get('samples', []))} samples, "
        f"{meta.get('elapsed_s')}s"
    )
    print()
    print(analysis.render(results, markdown=args.markdown))
    if not args.markdown:
        print()
        print(analysis.verdicts_text(results))
    return 0


def cmd_gate(args) -> int:
    """What a run must satisfy before its numbers are published."""
    results = analysis.load(args.file)
    baseline = analysis.load(args.baseline) if args.baseline else None
    lines, failures = analysis.gate(results, baseline)
    print("\n".join(lines))
    if failures:
        print(f"gate: {len(failures)} FAILURE(S)")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    return 0


def cmd_compare(args) -> int:
    text, refused, _claims = analysis.compare_files(
        analysis.load(args.a), analysis.load(args.b)
    )
    print(text)
    return 2 if refused else 0


# --- selfcheck --------------------------------------------------------------
#: A leg is shaped on both of its ends, because netem is egress-only. The
#: selfcheck asserts it so a future leg cannot silently shape one direction.
SHAPED_ENDS_PER_LEG = 2

#: How long the selfcheck gives one binary to load one config. Loading is
#: milliseconds; the ceiling only exists so a wedged subprocess cannot hang a
#: commit gate.
CONFIG_LOAD_TIMEOUT_S = 15


def _check_config_loads(c: "Checker", arm: model.Arm, cfg: dict) -> None:
    """Whether the product loads this arm's config, per end.

    A peer arm's config belongs to another tool (frp's `frps.toml`, nps's
    `npc.conf`), so this binary cannot read it and that is not a finding.
    """
    if arm.kind == "peer":
        c.check(f"arm {arm.id} config load not applicable", True, "peer arm")
        return
    binary = _selfcheck_binary()
    if not binary:
        c.check(
            f"arm {arm.id} config load skipped",
            True,
            "no built binary here (cargo build --release)",
        )
        return
    for role in ("server", "client"):
        flag = _role_flag(arm, role)
        ok, why = _config_loads(binary, cfg[role], flag)
        c.check(f"arm {arm.id} {role} config loads", ok, why)


def _selfcheck_binary() -> str:
    """The built molehill the selfcheck may ask to load a config, or `""`.

    `target/release/molehill` is the default `--binary` a run uses, so it is
    the one host a config has to work on. Missing (a fresh clone, or a tree
    nobody has released-built), the load check is skipped rather than failed:
    the selfcheck runs in the pre-commit gate, where a missing build is not a
    defect.
    """
    binary = Path(HERE).parents[2] / "target/release/molehill"
    return str(binary) if binary.is_file() and os.access(binary, os.X_OK) else ""


def _config_loads(binary: str, path: Path, flag: str) -> tuple:
    """Whether the product loads this config, and the reason when it does not.

    The daemon refuses a config the schema accepts (a control channel without
    Noise keys, say) at a point no TOML grammar can see, so an arm whose
    config is merely well-formed can still be one that never starts. A refused
    load is the finding; a *successful* load is not, so this stops the process
    as soon as it reports a mode rather than waiting for it to serve.

    Only the load is judged. The mode flag names the end, and anything past
    the load — a bind that needs the topology, a transparent prerequisite that
    needs the operator's routes — belongs to the run, not to this check.
    """
    # The process is read until it reports a mode and then stopped: a client
    # that loads *stays* (it retries the server forever, which is the
    # behaviour a run depends on), so `run(timeout=…)` would call a healthy
    # load a failure. What is judged is the first verdict it prints — either
    # `Running as …` or an `Error` — and nothing after it.
    try:
        proc = subprocess.Popen(
            [binary, flag, str(path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
    except (OSError, subprocess.SubprocessError) as e:
        return False, f"{type(e).__name__}: {e}"
    deadline = time.monotonic() + CONFIG_LOAD_TIMEOUT_S
    out: list = []
    loaded = False
    refused = ""
    try:
        for line in proc.stdout:
            out.append(line)
            if line.startswith("Running as"):
                loaded = True
                break
            # `Failed to load <path>` is the daemon's own marker for a config
            # it will not accept — the shape a mandatory key that was never
            # written takes. An `Error` that is *not* that is a runtime fault
            # (a bind address that needs the topology, a transparent
            # prerequisite that needs the operator's routes), which belongs to
            # the run and not to this check.
            if line.startswith("Error: Failed to load"):
                refused = line.strip()
                break
            if time.monotonic() > deadline:
                break
    finally:
        proc.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            proc.wait(timeout=5)
        with contextlib.suppress(Exception):
            proc.stdout.close()

    if loaded or not refused:
        return True, ""
    return False, refused


def _role_flag(arm: model.Arm, role: str) -> str:
    """The mode flag this arm's `role` end runs under.

    A server is always `--server`; a client is `--client` except for an L3
    arm, whose file carries a `[transparent]` block and runs as
    `--transparent`.
    """
    if role == "server":
        return "--server"
    return "--transparent" if arm.kind == "l3" else "--client"


class Checker:
    """The model's own checks: the pure layer, without root or a topology."""

    def __init__(self):
        self.failures: list = []
        self.checks = 0

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        self.checks += 1
        if not condition:
            self.failures.append(f"{name}: {detail}" if detail else name)

    def equal(self, name: str, got, want) -> None:
        self.check(name, got == want, f"got {got!r}, want {want!r}")

    def report(self) -> int:
        if self.failures:
            print(f"selfcheck FAILED ({len(self.failures)} of {self.checks}):")
            for failure in self.failures:
                print(f"  - {failure}")
            return 1
        print(f"selfcheck OK ({self.checks} checks)")
        return 0


def _check_registry(c: Checker) -> None:
    ids = [m.id for m in model.METRICS.values()]
    c.equal("metric ids are unique", len(ids), len(set(ids)))
    for metric in model.METRICS.values():
        c.check(
            f"metric {metric.id} direction",
            metric.direction in ("higher", "lower", "none"),
            metric.direction,
        )
        c.check(
            f"metric {metric.id} materiality",
            metric.materiality[0] in ("rel", "abs"),
            str(metric.materiality),
        )
        c.check(
            f"metric {metric.id} denominator",
            bool(metric.denominator),
            "a ratio without a stated denominator cannot be compared",
        )
        for kind in metric.needs:
            c.check(
                f"metric {metric.id} kind {kind}",
                kind in model.Kind.ALL,
                f"unknown workload kind {kind}",
            )
    for scenario in model.SCENARIOS.values():
        c.check(
            f"scenario {scenario.id} kind",
            scenario.kind in model.Kind.ALL,
            scenario.kind,
        )
        headline = scenario.headline
        c.check(
            f"scenario {scenario.id} headline exists",
            headline in model.METRICS,
            headline,
        )
        c.check(
            f"scenario {scenario.id} headline is supported",
            headline in model.metrics_for(scenario.kind),
            f"{headline} is not declared for kind {scenario.kind}",
        )
    diagnostics = [s.id for s in model.SCENARIOS.values() if s.diagnostic]
    c.check(
        "diagnostic scenarios are declared",
        all(model.SCENARIOS[s].control is False for s in diagnostics),
        str(diagnostics),
    )
    for name, profile in model.PROFILES.items():
        for scenario_id in profile["scenarios"]:
            c.check(
                f"profile {name} scenario {scenario_id}",
                scenario_id in model.SCENARIOS,
                "unknown scenario in a profile",
            )
        c.check(
            f"profile {name} budget",
            profile["budget_s"] > 0 and profile["rounds"] >= 1,
            "a profile needs a positive budget and at least one round",
        )
        for scenario in model.SCENARIOS.values():
            params = scenario.params_for(profile)
            c.check(
                f"profile {name} params {scenario.id}",
                bool(params) == bool(scenario.params),
                "profile parameters may not add keys a scenario does not have",
            )


def _check_arms(c: Checker) -> None:
    for name, arm in model.ARM_CATALOG.items():
        c.equal(f"arm catalog key {name} is the arm's id", arm.id, name)
    # An L3 arm's `pool_cap` is the *carrier's* lane budget: the product divides
    # it among every claim of the topology and refuses one below the claim count,
    # so an arm that names such a budget would measure a client that never
    # started. A budget that is not a multiple of the claim count is legal but
    # leaves its remainder unallocated, which would make the arm quietly measure
    # fewer lanes than its name says.
    claims = len(topology.SERVICE_PORTS)
    for name, arm in model.ARM_CATALOG.items():
        if arm.kind != "l3" or not arm.pool_cap:
            continue
        c.check(
            f"arm {name} lane budget covers every claim",
            arm.pool_cap >= claims,
            f"{arm.pool_cap} lanes for {claims} claims is refused by the client",
        )
        c.equal(
            f"arm {name} lane budget divides evenly",
            arm.pool_cap % claims,
            0,
        )
    for name in model.ARM_CATALOG:
        arm = model.arms_from_names([name], "/bin/true")[0]
        c.equal(f"arm {name} id", arm.id, name)
        # Every arm's config is *built* here, not described: an arm whose
        # topology writer reaches for a knob the model no longer has fails the
        # campaign cell by cell (`AttributeError: 'Arm' object has no attribute
        # ...`) after the round's setup, which is a whole run spent to learn
        # something this check answers in milliseconds.
        try:
            cfg = topology.service_config(arm, Path(tempfile.mkdtemp(prefix="armcfg-")))
            c.check(
                f"arm {name} builds a config",
                bool(cfg.get("server")) and bool(cfg.get("client")),
                f"the topology writer returned {sorted(cfg)}",
            )
            # Built *and parsed*: a written config can be invalid TOML (a
            # duplicate table header, say) or carry a key the product no longer
            # knows, and either way the arm fails every cell of a run that has
            # already paid for its namespaces. tomllib is the same grammar the
            # product's loader uses, so a duplicate header is caught here.
            for role in ("server", "client"):
                text = (cfg[role]).read_text()
                try:
                    tomllib.loads(text)
                    ok, why = True, ""
                except tomllib.TOMLDecodeError as e:
                    ok, why = False, str(e)
                c.check(f"arm {name} {role} config parses", ok, why)
            # Parsing is not starting, and this is the half a TOML grammar
            # cannot see: an arm whose config the product *refuses* (a
            # mandatory key the topology writer never wrote, a value the
            # schema accepts and the daemon does not) fails every cell of a
            # run that has already paid for its namespaces — and, measured,
            # it did exactly that to every arm of this model while all 620
            # selfcheck assertions stayed green. The binary is asked to load
            # each end's config, which is the check a run performs for real.
            _check_config_loads(c, arm, cfg)
        except Exception as e:  # noqa: BLE001 - any failure is the finding
            c.check(f"arm {name} builds a config", False, f"{type(e).__name__}: {e}")
        for kind in ("control", "l4", "l3"):
            spec = f"id={kind},txqueuelen={model.DEEP_TXQUEUELEN}"
            parsed = model.parse_arm_spec(spec)
            c.equal(f"arm spec {kind} txqueuelen", parsed.txqueuelen, 10000)
    # A carrier is *not* an arm key (the product chooses one at runtime, so the
    # contrast is the `udp_blocked` condition); the *shape* is derived from the
    # service type (a forwarding service multiplexes, a claim never does), so an
    # arm spec that still names a mode is refused rather than silently measuring
    # the default — and `pool_cap` means the pool's width for an L4 arm and the
    # claim's lane budget for an L3 one.
    parsed = model.parse_arm_spec("id=l3,pool_cap=4")
    c.equal("an arm claims no carrier", parsed.data_carrier, "auto")
    c.check(
        "the udp-blocked condition blocks UDP",
        model.CONDITIONS["udp_blocked"].block_udp,
        "udp_blocked does not block UDP",
    )
    c.check(
        "the udp-blocked condition is refused off the tunnel leg",
        _refused(
            model.refuse_misplaced_condition,
            model.CONDITIONS["udp_blocked"],
            "visitor",
        ),
        "udp_blocked was accepted on the visitor leg",
    )
    c.check(
        "a netem condition is accepted on any leg",
        not _refused(
            model.refuse_misplaced_condition, model.CONDITIONS["loss1"], "visitor"
        ),
        "loss1 was refused on the visitor leg",
    )
    c.equal("arm spec keeps the l3 lane budget", parsed.pool_cap, 4)
    c.check(
        "arm spec refuses the removed mode key",
        _refused(model.parse_arm_spec, "id=l3,mode=direct"),
        "mode=direct was accepted",
    )
    c.check(
        "arm spec refuses a carrier key",
        _refused(model.parse_arm_spec, "id=l3,force_carrier=kcp"),
        "force_carrier=kcp was accepted (the carrier is chosen at runtime)",
    )
    c.check(
        "unknown arm key is refused",
        _refused(model.parse_arm_spec, "id=l3,typo=1"),
        "typo=1 was accepted",
    )


def _check_device_io(c: Checker) -> None:
    """The device-I/O metrics belong to the arms that have a device.

    `/proc/<pid>/io` does not account socket payload (measured in this
    container: 200 MB through a socketpair moves `rchar` by 105 KB, the same
    bytes through a pipe by 209 MB), so a forwarding arm would report its
    runtime's plumbing as if it were the path. The reader of a result sees a
    typed absence for those arms instead of a number that means nothing.
    """
    l3 = model.arms_from_names(["l3"], "/bin/true")[0]
    l4 = model.arms_from_names(["l4"], "/bin/true")[0]
    c.check("an L3 arm has device I/O", l3.uses_device_io, "l3 reported no device")
    c.check("an L4 arm has no device I/O", not l4.uses_device_io, "l4 reported one")
    sample = {
        "elapsed_s": 2.0,
        "io": {"server": {"rchar": 4096, "wchar": 2048, "syscr": 2, "syscw": 1}},
        "syscalls": 3,
        "cpu_s_total": 1.0,
        "wire": {},
    }
    with_device = inst.counter_metrics(sample, {}, device_io=True)
    without = inst.counter_metrics(sample, {}, device_io=False)
    c.equal(
        "a device arm reports bytes per syscall",
        with_device["bytes_per_syscall"],
        2048.0,
    )
    c.equal("a socket arm reports none", without["bytes_per_syscall"], None)
    c.check(
        "the two device metrics share one rule",
        workloads.DEVICE_IO_METRICS == ("bytes_per_syscall", "syscalls_per_s"),
        "the rule and the metric list disagree",
    )


def _refused(fn, *args) -> bool:
    """True when the call raises: the shape of every refusal check here."""
    try:
        fn(*args)
    except ValueError:
        return True
    return False


def _check_analysis(c: Checker) -> None:
    c.equal("percentile of one value", inst.pct([5.0], 0.99), 5.0)
    c.equal(
        "percentile is nearest-rank",
        inst.pct([1.0, 2.0, 3.0, 4.0], 0.5),
        3.0,
    )
    c.equal("percentile of nothing", inst.pct([], 0.99), None)

    claim = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[10.0, 10.1],
            values_b=[5.0, 5.0],
            label_a="a",
            label_b="control",
            noise={"pct": 1.0},
        )
    )
    c.equal("a 100 % difference is a claim", claim["verdict"], "claim")
    c.equal("the better side is named", claim["better"], "a")

    inside = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[10.0, 10.05],
            values_b=[10.0, 10.0],
            noise={"pct": 3.0},
        )
    )
    c.equal(
        "inside the noise floor is not a claim", inside["verdict"], "indistinguishable"
    )

    disagreeing = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[13.0, 9.0],
            values_b=[10.0, 10.0],
            noise={"pct": 1.0},
        )
    )
    c.equal("a sign disagreement is directional", disagreeing["verdict"], "directional")

    single = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[20.0],
            values_b=[1.0],
            noise={"pct": 0.0},
        )
    )
    c.equal("one round is not a verdict", single["verdict"], "single-round")

    missing = analysis.verdict(
        analysis.Comparison(metric="throughput_gbps", values_a=[], values_b=[1.0])
    )
    c.equal("no samples is unavailable", missing["verdict"], "unavailable")

    loss = analysis.verdict(
        analysis.Comparison(
            metric="udp_loss_pct",
            values_a=[0.2, 0.3],
            values_b=[2.0, 2.1],
            noise={"abs": 0.5},
        )
    )
    c.equal(
        "an absolute floor is applied in the metric's unit", loss["verdict"], "claim"
    )


def _check_summary(c: Checker) -> None:
    def sample(arm, rnd, value, warmup=False, ok=True):
        return {
            "arm": arm,
            "round": rnd,
            "warmup": warmup,
            "scenario": "bulk-1",
            "kind": "bulk",
            "cell": "P1",
            "ok": ok,
            "reason": "" if ok else "typed",
            "metrics": {"throughput_gbps": value} if ok else {},
            "unavailable": {},
            "evidence": {},
        }

    results = {
        "meta": {"arms": [{"id": "control"}, {"id": "l3"}, {"id": "l3~aa"}]},
        "samples": [
            sample("l3", 0, 99.0, warmup=True),
            sample("l3", 1, 4.0),
            sample("l3", 2, 4.4),
            sample("l3", 3, 0.0, ok=False),
            sample("l3~aa", 1, 4.1),
            sample("l3~aa", 2, 4.3),
            sample("control", 1, 40.0),
            sample("control", 2, 41.0),
        ],
    }
    results["summary"] = analysis.summarize(results)
    cell = results["summary"]["cells"][0]
    l3 = cell["arms"]["l3"]
    c.equal("warm-up rounds are excluded", l3["rounds"], 2)
    c.equal("a failed round is counted, not averaged", l3["failed"], 1)
    c.equal(
        "the cell's median is over the measured rounds",
        l3["metrics"]["throughput_gbps"]["median"],
        4.2,
    )
    c.equal(
        "failures are listed with their reason", len(results["summary"]["failures"]), 1
    )
    noise = results["summary"]["noise"]["metrics"]["throughput_gbps"]
    c.check(
        "the A/A twin gives the throughput floor",
        noise["floor_pct"] is not None and noise["floor_pct"] > 0,
        str(noise),
    )
    verdicts = analysis.verdicts(results)
    claims = [v for v in verdicts if v["verdict"] == "claim" and not v["aa"]]
    c.check(
        "a 10x throughput difference against the control is a claim",
        any(v["metric"] == "throughput_gbps" for v in claims),
        str([(v["metric"], v["verdict"]) for v in verdicts if not v["aa"]]),
    )
    aa = [v for v in verdicts if v["aa"]]
    c.check("the A/A pair produces verdicts", bool(aa), "no A/A verdicts")
    c.check(
        "the A/A pair is not a claim",
        all(v["verdict"] != "claim" for v in aa),
        str([(v["metric"], v["verdict"]) for v in aa]),
    )
    text = analysis.verdicts_text(results)
    c.check("the verdict text renders", "CLAIM" in text, text[:200])


def _cell_sample(arm, rnd, value, warmup=False, ok=True):
    """One sample row for the statistics fixtures (same shape as a run's)."""
    return {
        "arm": arm,
        "round": rnd,
        "warmup": warmup,
        "scenario": "bulk-1",
        "kind": "bulk",
        "cell": "P1",
        "ok": ok,
        "reason": "" if ok else "typed",
        "metrics": {"throughput_gbps": value} if ok else {},
        "unavailable": {},
        "evidence": {},
    }


def _check_statistics(c: Checker) -> None:
    """Every new verdict rule, on data with a known answer."""
    # Sign test: perfectly consistent readings at n rounds are labelled with
    # the error rate they carry, not silently treated as 5 % evidence.
    c.equal("sign test of nothing", analysis.sign_test_p([]), 1.0)
    c.equal("3/3 same sign is p 0.25", analysis.sign_test_p([1, 1, 1]), 0.25)
    c.equal(
        "2/3 same sign is p 1.0",
        analysis.sign_test_p([1, 1, -1]),
        1.0,
    )
    c.equal("min attainable p at 3 rounds", analysis.min_attainable_p(3), 0.25)
    c.equal(
        "six rounds reach the nominal level",
        analysis.alpha_eff(6),
        analysis.ALPHA,
    )
    c.equal(
        "three rounds degrade to what they can test",
        analysis.alpha_eff(3),
        0.25,
    )
    c.equal(
        "the planning estimate scales with the scatter",
        analysis.rounds_needed(20.0, 5.0),
        62,
    )
    c.equal(
        "a tighter scatter needs fewer rounds", analysis.rounds_needed(10.0, 5.0), 16
    )
    c.equal("no scatter needs no rounds", analysis.rounds_needed(0.0, 5.0), None)

    # Interval: resampling a stored run must reproduce the same interval.
    pairs = [(4.0, 4.1), (4.4, 4.3), (4.2, 4.15)]
    first = analysis.bootstrap_ci(pairs, seed=7)
    c.equal(
        "the bootstrap is reproducible",
        analysis.bootstrap_ci(pairs, seed=7),
        first,
    )
    c.equal(
        "too few pairs say so instead of guessing",
        analysis.bootstrap_ci(pairs[:2], seed=7),
        (None, None),
    )

    # BH: one pre-registered endpoint is unadjusted; two consistent ones both
    # survive; a straggler is named.
    c.equal(
        "a single-endpoint family adjusts nothing",
        analysis.benjamini_hochberg([0.04]),
        [0.04],
    )
    c.equal(
        "two endpoints at the same p survive together",
        analysis.benjamini_hochberg([0.04, 0.04]),
        [0.04, 0.04],
    )
    c.check(
        "the weaker endpoint is corrected upward",
        analysis.benjamini_hochberg([0.04, 0.04, 0.30])[2]
        > analysis.benjamini_hochberg([0.04, 0.04, 0.30])[0],
        str(analysis.benjamini_hochberg([0.04, 0.04, 0.30])),
    )


def _check_verdict_shapes(c: Checker) -> None:
    """The verdict states on data with a known answer, at a real length."""

    # them (5 rounds: p 0.0625 vs the degraded level 0.0625 — exactly at it).
    def five(base, treated):
        return [base + d for d in treated]

    # A 20 % effect with a 1 % floor: entirely beyond, consistent, stable.
    beyond = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=five(10.0, [2.0, 2.2, 1.9, 2.1, 2.0]),
            values_b=[10.0] * 5,
            noise={"pct": 1.0},
        )
    )
    c.equal(
        "a consistent effect beyond the floor is a claim",
        beyond["verdict"],
        "claim",
    )
    c.equal(
        "the claim's interval lies entirely beyond the threshold",
        beyond["ci"],
        "beyond",
    )
    c.equal("the claim is not fragile", beyond["fragile"], False)

    # A boundary case worth naming: the median exactly at the floor. The
    # interval straddles it (half the resamples clear, half do not), so the
    # verdict is directional — "at the threshold" is not "beyond it", and the
    # verdict text is where a reader finds out.
    at_floor = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=five(10.0, [1.0, 1.2, 0.9, 1.1, 1.0]),
            values_b=[10.0] * 5,
            noise={"pct": 1.0},
        )
    )
    c.equal(
        "a median exactly at the floor straddles it",
        at_floor["ci"],
        "straddling",
    )

    straddling = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[12.0, 9.0, 13.0, 11.8, 9.4],
            values_b=[10.0] * 5,
            noise={"pct": 1.0},
        )
    )
    c.equal(
        "an inconsistent effect is directional with a straddling interval",
        (straddling["verdict"], straddling["ci"]),
        ("directional", "straddling"),
    )
    c.check(
        "the straddling verdict is fragile",
        straddling["fragile"],
        str(straddling["ci"]),
    )

    within = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[10.02, 9.98, 10.01, 9.99, 10.0],
            values_b=[10.0] * 5,
            noise={"pct": 1.0},
        )
    )
    c.equal(
        "an effect inside the threshold reads indistinguishable",
        within["verdict"],
        "indistinguishable",
    )
    c.equal(
        "the indistinguishable interval sits within the threshold",
        within["ci"],
        "within",
    )

    # Pairing by round index, not position: a failed round on one side must
    # not pair round 1 with round 2 and call it a replicate.
    shifted = analysis.verdict(
        analysis.Comparison(
            metric="throughput_gbps",
            values_a=[10.0, 20.0, 30.0],
            values_b=[15.0, 25.0],
            rounds_a=(1, 2, 3),
            rounds_b=(1, 3),
            noise={"pct": 1.0},
        )
    )
    c.equal(
        "round-index pairing drops the unmatched round",
        shifted["n_pairs"],
        2,
    )


def _check_run_rules(c: Checker) -> None:
    """The rules that read a whole run: families, drowning, normalization."""
    sample = _cell_sample
    results_exploratory = {
        "meta": {"method": {"primary": []}, "arms": [{"id": "control"}, {"id": "l3"}]},
        "samples": [
            sample("l3", 1, 4.0),
            sample("l3", 2, 4.4),
            sample("control", 1, 40.0),
            sample("control", 2, 41.0),
        ],
    }
    results_exploratory["summary"] = analysis.summarize(results_exploratory)
    exploratory = analysis.verdicts(results_exploratory)
    headline = [
        v for v in exploratory if v["metric"] == "throughput_gbps" and not v.get("twin")
    ]
    c.check(
        "the headline stays primary when nothing was declared",
        headline and all(v["primary"] for v in headline),
        str(headline),
    )
    for entry in exploratory:
        if not entry["primary"]:
            c.check(
                "an exploratory comparison is never a claim",
                entry["verdict"] != "claim",
                f"{entry['metric']}: {entry['verdict']}",
            )

    # A declared endpoint survives the family gate; the correction applies to
    # it and not to the exploratory rest.
    results_declared = {
        "meta": {
            "method": {
                "primary": [
                    {"metric": "throughput_gbps", "scenario": "bulk-1"},
                    {"metric": "rss_peak_mib", "scenario": "bulk-1"},
                ]
            },
            "arms": [{"id": "control"}, {"id": "l3"}],
        },
        "samples": [
            sample("l3", 1, 4.0),
            sample("l3", 2, 4.4),
            sample("control", 1, 40.0),
            sample("control", 2, 41.0),
        ],
    }
    results_declared["summary"] = analysis.summarize(results_declared)
    declared_verdicts = analysis.verdicts(results_declared)
    throughput = next(
        v
        for v in declared_verdicts
        if v["metric"] == "throughput_gbps" and not v.get("twin")
    )
    c.check(
        "the declared metric is primary",
        throughput["primary"],
        str(throughput.get("primary")),
    )
    c.check(
        "the declared family is corrected (both endpoints read the same p)",
        throughput["q"] == throughput["p"],
        f"p {throughput['p']} q {throughput['q']}",
    )
    c.check(
        "the undeclared metrics are exploratory",
        all(
            not v["primary"]
            for v in declared_verdicts
            if v["metric"] not in ("throughput_gbps", "rss_peak_mib")
        ),
        "an undeclared metric was primary",
    )

    # Drowned cells: past half the rounds failed, the median is not quoted.
    results_drowned = {
        "meta": {"arms": [{"id": "control"}, {"id": "l3"}]},
        "samples": [
            sample("l3", 1, 4.0),
            sample("l3", 2, 0.0, ok=False),
            sample("l3", 3, 0.0, ok=False),
            sample("l3", 4, 0.0, ok=False),
            sample("control", 1, 40.0),
            sample("control", 2, 41.0),
        ],
    }
    results_drowned["summary"] = analysis.summarize(results_drowned)
    l3_drowned = results_drowned["summary"]["cells"][0]["arms"]["l3"]
    c.check(
        "a cell that lost most rounds is marked drowned",
        l3_drowned["drowned"],
        str(l3_drowned),
    )
    verdicts_drowned = analysis.verdicts(results_drowned)
    c.check(
        "a drowned cell's comparison is unavailable, never a median",
        all(
            v["verdict"] == "unavailable"
            for v in verdicts_drowned
            if v["metric"] == "throughput_gbps"
        ),
        str([(v["verdict"], v.get("reason", "")) for v in verdicts_drowned]),
    )
    _gate_lines, gate_failures = analysis.gate(results_drowned)
    c.check(
        "the gate refuses a drowned cell",
        any("drowned" in f for f in gate_failures),
        str(gate_failures),
    )

    # Normalization: opt-in per run (`--normalize control`), and only honest
    # between two tool arms — each divided by its own round's control. An
    # arm against the control itself is circular (it would divide by itself),
    # so that pair always stays raw.
    def nsample(arm, rnd, value):
        row = sample(arm, rnd, value)
        row["metrics"]["throughput_gbps"] = value
        return row

    samples_normalized = [
        # The host halves between rounds; both tool arms track it exactly, so
        # raw their difference is the drift, normalized it cancels.
        nsample("control", 1, 10.0),
        nsample("control", 2, 20.0),
        nsample("a", 1, 5.0),
        nsample("a", 2, 10.0),
        nsample("b", 1, 6.0),
        nsample("b", 2, 12.0),
    ]
    results_normalized = {
        "meta": {
            "method": {"normalize": "control", "primary": []},
            "arms": [
                {"id": "control"},
                {"id": "a"},
                {"id": "b", "against": "a"},
            ],
        },
        "samples": list(samples_normalized),
    }
    results_normalized["summary"] = analysis.summarize(results_normalized)
    verdicts_normalized = analysis.verdicts(results_normalized)
    pair = next(
        v
        for v in verdicts_normalized
        if v["metric"] == "throughput_gbps" and v["a"] == "a" and v["b"] == "b"
    )
    c.check(
        "a declared A/B pair is normalized by round",
        pair.get("normalized"),
        str(pair),
    )
    c.check(
        "the drift cancels: the ratio is what the verdict reads",
        pair["delta_pct"] is not None
        and round(pair["delta_pct"], 3) == round(-100.0 / 6.0, 3),
        f"delta_pct {pair['delta_pct']} (expected the true -16.67 %, not the "
        "host's 2x swing)",
    )
    # The default run does none of that: raw readings, both because the run
    # did not ask and because an arm-against-control pair would be circular.
    results_raw = {
        "meta": {"arms": [{"id": "control"}, {"id": "a"}, {"id": "b", "against": "a"}]},
        "samples": list(samples_normalized),
    }
    results_raw["summary"] = analysis.summarize(results_raw)
    raw_verdicts = analysis.verdicts(results_raw)
    pair_raw = next(
        v for v in raw_verdicts if v["metric"] == "throughput_gbps" and v["a"] == "a"
    )
    c.check(
        "the default comparison stays raw",
        not pair_raw.get("normalized"),
        str(pair_raw.get("normalized")),
    )
    c.equal(
        "an arm-against-control pair reads the true gap (2x), raw",
        pair_raw["delta_abs"],
        -7.5,
    )


def _check_statistics_registry(c: Checker) -> None:
    """The statistics layer's own invariants, independent of any run."""
    c.check(
        "the analysis version is declared",
        isinstance(analysis.ANALYSIS_VERSION, int),
        "not an int",
    )
    c.check(
        "the bootstrap level is a probability",
        0 < analysis.BOOTSTRAP_LEVEL < 1,
        str(analysis.BOOTSTRAP_LEVEL),
    )
    c.check(
        "the nominal level is a probability",
        0 < analysis.ALPHA < 1,
        str(analysis.ALPHA),
    )
    c.check(
        "the drowned threshold admits a majority of rounds",
        0 < analysis.MAX_FAILED_FRACTION < 1,
        str(analysis.MAX_FAILED_FRACTION),
    )


def _check_comparability(c: Checker) -> None:
    def meta(fingerprint="abc", host_id="h1", calibration=100.0, loopback=20.0):
        return {
            "model": "bench",
            "fingerprint": fingerprint,
            "method": {"engine_hash": "e1", "rounds": 2, "tune": 1},
            "provenance": {
                "host": {
                    "identity": {"host_id": host_id},
                    "calibration": {"ok": True, "median": calibration},
                    "loopback": {"ok": True, "median": loopback},
                }
            },
        }

    c.equal("identical methods compare", analysis.comparability(meta(), meta()), [])
    blockers = analysis.comparability(meta(), meta(fingerprint="xyz"))
    c.check(
        "a different fingerprint is refused by key",
        any("tune" in b for b in blockers) or any("fingerprint" in b for b in blockers),
        str(blockers),
    )
    c.equal(
        "a different host is refused",
        len(analysis.comparability(meta(), meta(host_id="h2"))),
        1,
    )
    c.check(
        "a calibration drift beyond the tolerance is refused",
        bool(analysis.comparability(meta(calibration=100.0), meta(calibration=50.0))),
        "25 % limit was not applied",
    )
    c.check(
        "a loopback drift beyond the tolerance is refused",
        bool(analysis.comparability(meta(loopback=20.0), meta(loopback=10.0))),
        "15 % limit was not applied",
    )
    empty = meta()
    empty["provenance"]["host"].pop("calibration")
    c.check(
        "a missing probe is unverifiable, not agreement",
        bool(analysis.comparability(meta(), empty)),
        "a file without the probe was compared anyway",
    )
    c.check(
        "a non-bench file is refused",
        bool(analysis.comparability({"model": "soak"}, meta())),
        "a soak file was accepted",
    )


def _check_fingerprint(c: Checker) -> None:
    class Args:
        tun_mtu = 1400
        link_mtu = 1500
        condition = "clean"
        condition_leg = "visitor"

    scenarios = [model.SCENARIOS["bulk-1"]]
    no_arms: list = []  # a model-only run: the peer-version block is empty
    method = runner.method_record(
        model.PROFILES["smoke"], scenarios, Args(), "smoke", no_arms
    )
    same = runner.method_record(
        model.PROFILES["smoke"], scenarios, Args(), "smoke", no_arms
    )
    c.equal(
        "the same method fingerprints the same",
        runner.fingerprint(method),
        runner.fingerprint(same),
    )
    Args.tun_mtu = 8000
    changed = runner.method_record(
        model.PROFILES["smoke"], scenarios, Args(), "smoke", no_arms
    )
    c.check(
        "a topology change fingerprints differently",
        runner.fingerprint(method) != runner.fingerprint(changed),
        "an MTU change did not change the fingerprint",
    )
    Args.tun_mtu = 1400
    c.check(
        "the metric definitions are part of the method",
        method["metrics_hash"] == runner.metrics_hash(),
        "metrics_hash is not the registry's hash",
    )


def _load_probe_module(name: str):
    """A probe module, loaded by path so its constants can be read.

    The probes are scripts with a PEP 723 header, not a package, and they carry
    no import-time side effects (their `main` is behind a `__name__` guard), so
    loading one is how a check in this file can assert something *about* the
    numbers the probe runs with instead of restating them.
    """
    path = HERE / "probes" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"bench_probe_{name}", path)
    if spec is None or spec.loader is None:
        return None
    module = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(module)
    except Exception:  # noqa: BLE001 — an unimportable probe is the finding
        return None
    return module


def _check_probes(c: Checker) -> None:
    """The probes compile, and the silence threshold is stated against theirs.

    The wedge threshold and the interactive probe's attempt budget are two
    numbers in two files that have to stay ordered. With the threshold at or
    below the budget, **one** unanswered attempt is a "silence" by
    construction: a lost SYN on a 5 %-loss path costs 1 s and then 3 s, every
    tool pays it, and a gate that fails a subject arm on any wedge becomes a
    coin flip (measured 2026-10-15: the same L4 arm read 0 wedges in one
    release sweep and 2 in the next). Both were 5.0 when that happened, and no
    check noticed — this is the check.
    """
    for probe in sorted((HERE / "probes").glob("*.py")):
        try:
            py_compile.compile(str(probe), doraise=True, cfile=str(probe) + "c")
            c.check(f"probe {probe.name} compiles", True)
        except py_compile.PyCompileError as exc:
            c.check(f"probe {probe.name} compiles", False, str(exc)[:200])
        finally:
            with contextlib.suppress(OSError):
                Path(str(probe) + "c").unlink()

    series = _load_probe_module("series")
    if series is None:
        c.check(
            "the interactive probe's attempt budget is readable",
            False,
            "probes/series.py did not import",
        )
        return
    c.check(
        "the wedge threshold exceeds the probe's own attempt budget",
        inst.WEDGE_SILENCE_S > series.ATTEMPT_TIMEOUT_S,
        f"wedge {inst.WEDGE_SILENCE_S}s vs attempt {series.ATTEMPT_TIMEOUT_S}s: "
        "a threshold at or below the budget makes one unanswered attempt a wedge",
    )


#: What [`_check_wedges`]'s silent sample must report: two unanswered attempts,
#: the first starting at 105.0 and the last ending at 115.6 — so a silence is
#: measured from the first failure's start, not from the gap between the
#: samples around it (which would be 5.1 or 10.5 and would read as a different
#: metric's answer).
WEDGE_SAMPLE_SPAN_S = 10.6


def _check_wedges(c: Checker) -> None:
    """A slow answer is not a silence, and a run of them is.

    The distinction is the whole metric, and it was wrong: the threshold sat at
    the probe's own attempt budget, and the gaps it measured were the *next
    attempt's duration*, so one slow-but-successful answer counted as a wedge.
    Both samples are written the way the probe now emits them — an attempt has a
    start and an end — because the metric reads both.
    """
    slow = [
        {"t": 100.0, "started_at": 95.0, "metric": "rtt_ms", "v": 4980.0},
        {"t": 105.1, "started_at": 100.05, "metric": "rtt_ms", "v": 4980.0},
    ]
    silent = [
        *slow,
        {"t": 110.5, "started_at": 105.0, "metric": "error", "v": 1, "why": "timeout"},
        {"t": 115.6, "started_at": 110.5, "metric": "error", "v": 1, "why": "timeout"},
    ]
    answered = inst.wedges(slow)
    c.check(
        "a slow but successful answer is not a wedge",
        answered["count"] == 0,
        f"count {answered['count']} over {len(slow)} answered attempts",
    )
    wedged = inst.wedges(silent)
    c.check(
        "a run of unanswered attempts is a wedge, measured end to end",
        wedged["count"] == 1 and wedged["max_s"] == WEDGE_SAMPLE_SPAN_S,
        f"count {wedged['count']}, max {wedged['max_s']}s "
        f"(want 1 and {WEDGE_SAMPLE_SPAN_S})",
    )
    c.check(
        "a silence is reported even when the attempts around it answered",
        inst.wedges(silent[1:])["count"] == 1,
        "the run starts at the first failure, not at the first attempt",
    )


def _check_socket_query(c: Checker) -> None:
    """The socket filter must be a command `ss` accepts.

    This is the cheap mechanical guard for a real incident: the flags were
    unpacked character by character, `ss` failed on stderr, and the watcher
    read the empty stdout as "no sockets" for a whole campaign.
    """
    if not shutil.which("ss"):
        return
    query = ["ss", "-Htn", "state", "established", "sport = :1"]
    r = subprocess.run(query, capture_output=True, text=True, check=False)
    c.check(
        "the socket filter is a valid ss invocation",
        r.returncode == 0,
        f"exit {r.returncode}: {r.stderr.strip()[:200]}",
    )
    udp = subprocess.run(
        ["ss", "-Hun", "sport = :1"], capture_output=True, text=True, check=False
    )
    c.check(
        "the datagram socket filter is a valid ss invocation",
        udp.returncode == 0,
        f"exit {udp.returncode}: {udp.stderr.strip()[:200]}",
    )


def _check_topology(c: Checker) -> None:
    c.check(
        "every shape is a netem argument list",
        all(isinstance(c.netem, tuple) for c in model.CONDITIONS.values()),
        "a condition's netem arguments are not a tuple",
    )
    c.check(
        "both legs are shaped on both ends",
        all(len(ends) == SHAPED_ENDS_PER_LEG for ends in topology.LEGS.values()),
        "a leg without both ends would shape one direction only",
    )
    c.check(
        "the control arm dials the backend's own address",
        model.ARM_CATALOG["control"].dial_host
        == model.ARM_CATALOG["control"].backend_bind,
        "a control arm that dials a tool address is not a control",
    )
    c.check(
        "the model's namespaces are its own",
        all(ns.startswith("mhb") for ns in topology.NS_ALL),
        str(topology.NS_ALL),
    )
    _check_qdisc_parse(c)


#: A real `tc -s qdisc show` transcript, with the three shapes the parser has to
#: tell apart: a `noqueue` device, a shaped one whose queue is pinned at its
#: limit and dropping, and a TUN on `fq`. Copied from a run, not written by
#: hand: the first parser read the drop count one token out and answered "no
#: drops" for a leg dropping 28 000 packets/s, which no hand-written sample
#: would have caught because the sample would have been written to match it.
QDISC_SAMPLE = """
qdisc noqueue 0: dev lo root refcnt 2
 Sent 0 bytes 0 pkt (dropped 0, overlimits 0 requeues 0)
 backlog 0b 0p requeues 0
qdisc netem 8059: dev v-srv root refcnt 21 limit 1000 delay 100ms
 Sent 43083438 bytes 631739 pkt (dropped 2445474, overlimits 0 requeues 0)
 backlog 66024b 1000p requeues 0
qdisc fq 8056: dev mhbsrv0 root refcnt 2 limit 1000p
 Sent 119476087498 bytes 85445027 pkt (dropped 0, overlimits 0 requeues 0)
 backlog 0b 0p requeues 0
"""

#: What [`QDISC_SAMPLE`] must parse to: the shaped device's drop count and the
#: queue depth it was pinned at. Named, because a bare number in a check is one
#: nobody can tell from a typo.
QDISC_SAMPLE_DROPPED = 2_445_474
QDISC_SAMPLE_BACKLOG_P = 1_000


def _check_qdisc_parse(c: Checker) -> None:
    """The shaper's own counters are read the way `tc` writes them.

    This is the instrument the whole L3 wedge was invisible to, so its shape is
    pinned: a parser that silently matches nothing reports a clean path, and a
    clean path is exactly the answer that was wrong.
    """
    parsed = topology.parse_qdisc(QDISC_SAMPLE)
    c.check(
        "the qdisc parser reads every device it is given",
        set(parsed) == {"lo", "v-srv", "mhbsrv0"},
        f"parsed {sorted(parsed)}",
    )
    c.check(
        "the qdisc parser reads a drop count, not the token before it",
        parsed.get("v-srv", {}).get("dropped") == QDISC_SAMPLE_DROPPED,
        f"v-srv dropped {parsed.get('v-srv', {}).get('dropped')}",
    )
    c.check(
        "the qdisc parser separates a full queue from an idle one",
        (
            parsed.get("v-srv", {}).get("backlog_p"),
            parsed.get("lo", {}).get("backlog_p"),
        )
        == (QDISC_SAMPLE_BACKLOG_P, 0),
        f"v-srv backlog {parsed.get('v-srv', {}).get('backlog_p')}",
    )
    c.check(
        "every shaped class states its queue depth",
        all(
            "limit" in condition.netem
            for condition in model.CONDITIONS.values()
            if condition.netem
        ),
        "a class left its buffer to netem's default",
    )


def cmd_selfcheck(args) -> int:
    del args
    c = Checker()
    _check_registry(c)
    _check_arms(c)
    _check_device_io(c)
    _check_analysis(c)
    _check_summary(c)
    _check_statistics(c)
    _check_verdict_shapes(c)
    _check_run_rules(c)
    _check_statistics_registry(c)
    _check_comparability(c)
    _check_fingerprint(c)
    _check_probes(c)
    _check_wedges(c)
    _check_socket_query(c)
    _check_topology(c)
    return c.report()


# --- command line -----------------------------------------------------------
def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        prog="bench.py", description="the molehill performance model"
    )
    sub = ap.add_subparsers(dest="command", required=True)

    listing = sub.add_parser("list", help="metrics, scenarios, arms and profiles")
    listing.set_defaults(func=cmd_list)

    doctor = sub.add_parser("doctor", help="what this host can and cannot measure")
    doctor.add_argument(
        "--binary", default=str(HERE.parents[2] / "target/release/molehill")
    )
    doctor.add_argument("--no-host-probes", action="store_true")
    doctor.set_defaults(func=cmd_doctor)

    run = sub.add_parser("run", help="run a profile (the model's only entry)")
    run.add_argument("--profile", default=model.DEFAULT_PROFILE)
    run.add_argument(
        "--arms",
        default="",
        help="comma list from the catalog (default: the profile's own)",
    )
    run.add_argument(
        "--arm",
        action="append",
        default=[],
        help="an arm as data: id=l3,pool_cap=10,txqueuelen=10000 (repeatable)",
    )
    run.add_argument("--scenarios", default="", help="comma list of scenario ids")
    run.add_argument("--rounds", type=int, default=0, help="measured rounds per arm")
    run.add_argument(
        "--scale",
        type=float,
        default=0.0,
        help="scale every staged timeline's holds (a full sweep in minutes)",
    )
    run.add_argument("--warmup-rounds", type=int, default=None)
    run.add_argument(
        "--binary", default=str(HERE.parents[2] / "target/release/molehill")
    )
    run.add_argument("--binary-b", default="", help="the second build of an A/B")
    run.add_argument("--ab-arm", default="", help="the arm id to duplicate for the A/B")
    run.add_argument(
        "--aa",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="measure one arm twice as the run's own noise floor",
    )
    run.add_argument(
        "--condition",
        default="clean",
        choices=sorted(model.CONDITIONS),
        help="the path condition the whole campaign runs under",
    )
    run.add_argument(
        "--condition-leg", default="visitor", choices=sorted(topology.LEGS)
    )
    run.add_argument("--tun-mtu", type=int, default=1400)
    run.add_argument("--link-mtu", type=int, default=1500)
    run.add_argument(
        "--out", default="", help="results file (default ~/tmp/bench-*.json)"
    )
    run.add_argument("--work", default="", help="artifact directory")
    run.add_argument("--force", action="store_true", help="overwrite an existing --out")
    run.add_argument("--hypothesis", default="", help="what you expect, stated first")
    run.add_argument(
        "--primary",
        action="append",
        default=[],
        help=(
            "a pre-registered endpoint as data: "
            "metric=throughput_gbps,scenario=bulk-n[,arm=l4][,cell=P1] "
            "(repeatable; default: every scenario's headline metric)"
        ),
    )
    run.add_argument(
        "--normalize",
        default="",
        choices=("", "control"),
        help=(
            "divide each arm's per-round reading by the control's reading of "
            "the same round (host-bound cells only: it cancels the host's "
            "drift, and on a ceiling-bound cell it would cancel real signal)"
        ),
    )
    run.add_argument("--no-host-probes", action="store_true")
    run.set_defaults(func=cmd_run)

    report = sub.add_parser("report", help="render a stored results file")
    report.add_argument("file")
    report.add_argument("--markdown", action="store_true")
    report.set_defaults(func=cmd_report)

    gate = sub.add_parser(
        "gate", help="what a run must satisfy before its numbers are published"
    )
    gate.add_argument("file")
    gate.add_argument("--baseline", default="", help="a comparable earlier run")
    gate.set_defaults(func=cmd_gate)

    compare = sub.add_parser("compare", help="A/B two results files, or refuse")
    compare.add_argument("a")
    compare.add_argument("b")
    compare.set_defaults(func=cmd_compare)

    selfcheck = sub.add_parser("selfcheck", help="the model's own checks")
    selfcheck.set_defaults(func=cmd_selfcheck)
    return ap


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
