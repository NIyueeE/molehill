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
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import argparse  # noqa: E402
import contextlib  # noqa: E402
import json  # noqa: E402
import py_compile  # noqa: E402
import shutil  # noqa: E402
import subprocess  # noqa: E402

import analysis  # noqa: E402
import instruments as inst  # noqa: E402
import model  # noqa: E402
import runner  # noqa: E402
import topology  # noqa: E402


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
            f"  {name:<12} kind={arm.kind:<8} mode={arm.data_mode:<10} "
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
    for name in model.ARM_CATALOG:
        arm = model.arms_from_names([name], "/bin/true")[0]
        c.equal(f"arm {name} id", arm.id, name)
        for kind in ("control", "l4", "l3"):
            spec = f"id={kind},mode=direct,txqueuelen={model.DEEP_TXQUEUELEN}"
            parsed = model.parse_arm_spec(spec)
            c.equal(f"arm spec {kind} txqueuelen", parsed.txqueuelen, 10000)
    # `mode` and `carrier` are independent axes: the product accepts every
    # combination, so the catalog may state every combination.
    for mode in ("direct", "multiplex"):
        spec = f"id=l3,mode={mode},carrier=kcp"
        parsed = model.parse_arm_spec(spec)
        c.equal(f"arm spec carries kcp in {mode} mode", parsed.data_carrier, "kcp")
        c.equal(f"arm spec keeps the {mode} mode", parsed.data_mode, mode)
    c.check(
        "arm spec refuses an unknown carrier",
        _refused(model.parse_arm_spec, "id=l3,carrier=quic"),
        "carrier=quic was accepted",
    )
    c.check(
        "unknown arm key is refused",
        _refused(model.parse_arm_spec, "id=l3,typo=1"),
        "typo=1 was accepted",
    )


def _refused(fn, value) -> bool:
    """True when the call raises: the shape of every refusal check here."""
    try:
        fn(value)
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
    method = runner.method_record(model.PROFILES["smoke"], scenarios, Args(), "smoke")
    same = runner.method_record(model.PROFILES["smoke"], scenarios, Args(), "smoke")
    c.equal(
        "the same method fingerprints the same",
        runner.fingerprint(method),
        runner.fingerprint(same),
    )
    Args.tun_mtu = 8000
    changed = runner.method_record(model.PROFILES["smoke"], scenarios, Args(), "smoke")
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


def _check_probes(c: Checker) -> None:
    for probe in sorted((HERE / "probes").glob("*.py")):
        try:
            py_compile.compile(str(probe), doraise=True, cfile=str(probe) + "c")
            c.check(f"probe {probe.name} compiles", True)
        except py_compile.PyCompileError as exc:
            c.check(f"probe {probe.name} compiles", False, str(exc)[:200])
        finally:
            with contextlib.suppress(OSError):
                Path(str(probe) + "c").unlink()


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


def cmd_selfcheck(args) -> int:
    del args
    c = Checker()
    _check_registry(c)
    _check_arms(c)
    _check_analysis(c)
    _check_summary(c)
    _check_comparability(c)
    _check_fingerprint(c)
    _check_probes(c)
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
        help="an arm as data: id=l3,mode=direct,txqueuelen=10000 (repeatable)",
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
