"""The carrier's own phase budget, read back from the logs it already writes.

`MOLEHILL_MHD_STATS=1` makes each daemon log one cumulative line per second
(the switch and every field it carries are owned by `docs/configuration.md`).
This tool turns those lines into the table the carrier's cost is argued with:
seconds per carried Gbit, and nanoseconds per event, for every phase the
engine times.

Why it exists as a command rather than a notebook: the phase meters price a
*path*, not a product, and every optimisation question about the carrier starts
by asking which phase is worth attacking. The first time it was needed the
numbers were extracted by hand from a work directory, and the arithmetic that
matters — a sampled phase stands for one datagram in sixteen, and a run carries
about forty of them — was done in a shell one-liner.

    just bench phases target/evidence/work-phases-mhd
    just bench phases ~/tmp/bench-20261031-120000/results.json
    just bench phases WORK --carried-gbit 272 --json

The numbers are context, not evidence: the model's `cpu_s_per_gbit` is what a
claim is made with (AGENTS.md §10). This says *where* that number went.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path

#: Phases the engine times, and the event pool a sampled phase stands for.
#:
#: A phase timed on every event counts itself; one sampled one in sixteen is
#: scaled back up by its pool, which is what makes the column a cost per
#: carried Gbit instead of a cost per sampled event.
PHASES: tuple[tuple[str, str], ...] = (
    ("rx_total", "rx_datagrams"),
    ("decode", "rx_datagrams"),
    ("ack", "rx_datagrams"),
    ("append", "rx_datagrams"),
    ("tx_build", "tx_runs"),
    ("tx_send", "tx_runs"),
    ("tx_call", "tx_datagrams"),
    ("tx_pick", "tx_datagrams"),
    ("tx_encode", "tx_datagrams"),
    ("tx_book", "tx_datagrams"),
    ("io_read", ""),
    ("io_write", ""),
    ("seal", ""),
    ("open", ""),
)

#: What each phase measures, in one line, for the table's last column.
WHAT: dict[str, str] = {
    "rx_total": "one datagram, entry to return",
    "decode": "header parse and validation",
    "ack": "acknowledgement application",
    "append": "payload copy into the stream buffer",
    "tx_build": "one pass filling its run",
    "tx_send": "one pass posting its run",
    "tx_call": "one packet's turn under the lock",
    "tx_pick": "stream selection",
    "tx_encode": "framing and the payload copy",
    "tx_book": "flight table and cursor",
    "io_read": "copy out to the reader",
    "io_write": "copy in from the writer",
    "seal": "one record sealed",
    "open": "one record opened",
}

MARKER = "mhd-stats:"


@dataclass
class Phase:
    """One phase of one process in one round."""

    name: str
    seconds: float
    events: float
    pool: float

    @property
    def per_event(self) -> float:
        return self.seconds / self.events if self.events else 0.0

    @property
    def total(self) -> float:
        """Seconds the phase cost in the whole round, samples scaled up."""
        return self.per_event * self.pool if self.pool else self.seconds


@dataclass
class Role:
    """One daemon's last line: its counters and every phase it timed."""

    name: str
    counters: dict[str, float] = field(default_factory=dict)
    phases: list[Phase] = field(default_factory=list)

    @property
    def carried_gbit(self) -> float:
        return (
            max(
                self.counters.get("tx_bytes", 0.0),
                self.counters.get("rx_bytes", 0.0),
            )
            * 8
            / 1e9
        )


@dataclass
class Round:
    """One measured round: its two daemons and what the round carried."""

    name: str
    roles: list[Role] = field(default_factory=list)

    @property
    def carried_gbit(self) -> float:
        return max((role.carried_gbit for role in self.roles), default=0.0)


def parse(line: str) -> dict[str, float]:
    """One `mhd-stats:` line as `{field: value}`."""
    fields: dict[str, float] = {}
    for token in line.split():
        key, sep, value = token.partition("=")
        if not sep:
            continue
        try:
            fields[key] = float(value)
        except ValueError:
            continue
    return fields


def last_stats(path: Path) -> dict[str, float] | None:
    """The last cumulative line of one daemon's log, or None."""
    found = None
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return None
    for line in text.splitlines():
        if MARKER in line:
            found = parse(line)
    return found


def read_round(round_dir: Path) -> Round:
    """Every arm directory of one round, with both daemons' budgets."""
    out = Round(name=round_dir.name)
    for arm_dir in sorted(p for p in round_dir.iterdir() if p.is_dir()):
        for role in ("server", "client"):
            fields = last_stats(arm_dir / f"{role}.log")
            if fields is None:
                continue
            counters = {
                key: value
                for key, value in fields.items()
                if not key.endswith(("_us", "_n"))
            }
            phases = []
            for name, pool_field in PHASES:
                seconds = fields.get(f"{name}_us", 0.0) / 1e6
                events = fields.get(f"{name}_n", 0.0)
                if not events:
                    continue
                pool = fields.get(pool_field, 0.0) if pool_field else events
                phases.append(
                    Phase(
                        name=name,
                        seconds=seconds,
                        events=events,
                        pool=pool or events,
                    )
                )
            out.roles.append(
                Role(name=f"{arm_dir.name}/{role}", counters=counters, phases=phases)
            )
    return out


def read_work(work: Path) -> list[Round]:
    """Every round directory under a work directory, in order."""
    rounds = [read_round(path) for path in sorted(work.glob("round*")) if path.is_dir()]
    return [round_ for round_ in rounds if round_.roles]


def work_of(target: Path) -> Path:
    """The work directory a results file names, or the directory itself."""
    if target.is_dir():
        return target
    try:
        meta = json.loads(target.read_text()).get("meta", {})
    except (OSError, json.JSONDecodeError):
        meta = {}
    named = meta.get("work_dir")
    if not named:
        raise SystemExit(f"{target}: no meta.work_dir; pass the work directory instead")
    return Path(named)


def render(rounds: list[Round], carried: float | None) -> None:
    """Print the budget, one table per daemon."""
    for round_ in rounds:
        gbit = carried or round_.carried_gbit
        header = f"### {round_.name}"
        if gbit:
            header += f" — {gbit:.1f} Gbit carried"
        print(header)
        print()
        if not gbit:
            print(
                "No carrier counters in this round (the record layer's phases "
                "are still measured): pass --carried-gbit to price them."
            )
            print()
        for role in round_.roles:
            print(f"**{role.name}**")
            print()
            print("| phase | s/Gbit | per event | events | what |")
            print("|---|---|---|---|---|")
            for phase in role.phases:
                per_gbit = phase.total / gbit if gbit else 0.0
                print(
                    f"| `{phase.name}` | {per_gbit:.4f} | "
                    f"{phase.per_event * 1e6:.2f} µs | "
                    f"{phase.events:,.0f} | {WHAT.get(phase.name, '')} |"
                )
            print()


def emit_json(rounds: list[Round], carried: float | None) -> None:
    """The same budget as data, for a script or a spreadsheet."""
    payload = []
    for round_ in rounds:
        gbit = carried or round_.carried_gbit
        payload.append(
            {
                "round": round_.name,
                "carried_gbit": gbit,
                "roles": [
                    {
                        "role": role.name,
                        "counters": role.counters,
                        "phases": [
                            {
                                "phase": phase.name,
                                "seconds": phase.seconds,
                                "events": phase.events,
                                "pool": phase.pool,
                                "s_per_gbit": phase.total / gbit if gbit else None,
                                "ns_per_event": phase.per_event * 1e9,
                            }
                            for phase in role.phases
                        ],
                    }
                    for role in round_.roles
                ],
            }
        )
    print(json.dumps(payload, indent=1))


#: A line with every kind of phase, for `--self-check`. The numbers are round
#: on purpose: the assertions below are about the arithmetic, not the engine.
CANNED = (
    "2026-10-31T02:57:05Z INFO  mhd-stats: cumulative phase timings "
    "rx_runs=100 rx_datagrams=1600 rx_bytes=1000 tx_runs=10 "
    "tx_datagrams=320 tx_bytes=125000000 rx_total_us=1600 rx_total_n=100 "
    "decode_us=160 decode_n=100 ack_us=1600 ack_n=100 append_us=800 "
    "append_n=100 tx_build_us=1000 tx_build_n=10 tx_send_us=500 "
    "tx_send_n=10 tx_call_us=100 tx_call_n=20 tx_pick_us=10 tx_pick_n=20 "
    "tx_encode_us=40 tx_encode_n=20 tx_book_us=20 tx_book_n=20 "
    "io_read_us=100 io_read_n=50 io_write_us=100 io_write_n=50 "
    "seal_us=1000 seal_n=5 open_us=0 open_n=0"
)


def self_check() -> int:
    """Assert the scaling arithmetic on a canned line, without a run."""
    fields = parse(CANNED)
    failures = []

    def near(left: float, right: float) -> bool:
        """Equal to the precision a float sum of microsecond parts reaches."""
        return math.isclose(left, right, rel_tol=1e-9, abs_tol=1e-12)

    # One sampled datagram stands for sixteen: 1600 µs over 100 samples is
    # 16 µs each, and 1600 datagrams carry that cost in the round.
    phase = Phase(
        name="rx_total",
        seconds=fields["rx_total_us"] / 1e6,
        events=fields["rx_total_n"],
        pool=fields["rx_datagrams"],
    )
    if not near(phase.per_event, 16e-6):
        failures.append(f"per_event {phase.per_event!r} is not 16 µs")
    if not near(phase.total, 0.0256):
        failures.append(f"total {phase.total!r} is not 25.6 ms")

    # A run-timed phase is not scaled per datagram.
    build = Phase(
        name="tx_build",
        seconds=fields["tx_build_us"] / 1e6,
        events=fields["tx_build_n"],
        pool=fields["tx_runs"],
    )
    if not near(build.total, 0.001):
        failures.append(f"tx_build total {build.total!r} is not 1 ms")

    # And the carried total comes from the counters, not from a wall clock.
    role = Role(name="self", counters=fields)
    if not near(role.carried_gbit, 1.0):
        failures.append(f"carried {role.carried_gbit!r} is not 1 Gbit")

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        return 1
    print("phases self-check ok")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="bench phases",
        description=(
            "the carrier's phase budget, from the logs `MOLEHILL_MHD_STATS=1` "
            "writes (see docs/benchmarks.md)"
        ),
    )
    parser.add_argument("target", nargs="?", help="a work directory or results file")
    parser.add_argument("--carried-gbit", type=float, default=0.0)
    parser.add_argument("--json", action="store_true", help="the budget as data")
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args(argv)

    if args.self_check:
        return self_check()
    if not args.target:
        parser.error("a work directory or results file is required")
    work = work_of(Path(args.target))
    rounds = read_work(work)
    if not rounds:
        print(
            f"{work}: no `{MARKER}` line. Run the campaign with "
            "MOLEHILL_MHD_STATS=1 in its environment.",
            file=sys.stderr,
        )
        return 1
    if args.json:
        emit_json(rounds, args.carried_gbit)
    else:
        render(rounds, args.carried_gbit)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
