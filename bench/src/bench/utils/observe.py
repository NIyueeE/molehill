"""A process set's memory, sampled from `/proc` while something else runs.

The model keeps a run's *peak* resident set (`rss_peak_mib`), which is the right
shape for a gate and the wrong shape for a diagnosis: a plateau and a leak have
the same peak. This keeps the curve instead — one NDJSON row per sample, per
process — so "grew 150 MB/s for forty seconds" and "settled at 120 MiB after
two" can be told apart. It found the carrier's unbounded send buffer, which the
peak alone had reported as a number nobody could interpret.

    just bench observe --match molehill --duration 60
    just bench observe --match molehill --out ~/tmp/rss.ndjson --interval 0.2

Nothing here is a measurement of the product: it is a window on a process, and
the model's numbers are what a claim is made with (AGENTS.md §10).
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path

#: The kernel's page size in KiB, which is what `/proc/<pid>/statm` counts in.
PAGE_KIB = os.sysconf("SC_PAGE_SIZE") // 1024

#: A sample every this many seconds unless `--interval` says otherwise.
DEFAULT_INTERVAL_S = 0.5

#: How long to watch unless `--duration` says otherwise.
DEFAULT_DURATION_S = 30.0

#: The process name to follow unless `--match` says otherwise.
DEFAULT_MATCH = "molehill"

#: Where `/proc/<pid>/statm` puts the resident page count: size, resident, ...
STATM_RESIDENT = 1


@dataclass
class Sample:
    """One process at one instant."""

    at: float
    pid: int
    name: str
    rss_kib: int
    fds: int
    threads: int

    def row(self) -> dict:
        return {
            "t": round(self.at, 3),
            "pid": self.pid,
            "name": self.name,
            "rss_kib": self.rss_kib,
            "fds": self.fds,
            "threads": self.threads,
        }


def resident_kib(pid: int) -> int | None:
    """The process's resident set, or None when it is gone."""
    try:
        fields = Path(f"/proc/{pid}/statm").read_text().split()
    except OSError:
        return None
    if len(fields) <= STATM_RESIDENT:
        return None
    try:
        return int(fields[STATM_RESIDENT]) * PAGE_KIB
    except ValueError:
        return None


def count_fds(pid: int) -> int:
    """Open descriptors, or 0 when the process is not ours to read."""
    try:
        return len(list(Path(f"/proc/{pid}/fd").iterdir()))
    except OSError:
        return 0


def count_threads(pid: int) -> int:
    """Threads from `/proc/<pid>/status`, or 0 when it is gone."""
    try:
        text = Path(f"/proc/{pid}/status").read_text()
    except OSError:
        return 0
    for line in text.splitlines():
        if line.startswith("Threads:"):
            with contextlib.suppress(ValueError):
                return int(line.split()[1])
    return 0


def process_name(pid: int) -> str:
    """The basename of the process's `argv[0]`, empty when it is gone.

    The *binary* rather than the whole command line, because a tool that
    watches `molehill` would otherwise match the shell, `uv` and this very
    command — every one of which names it in its arguments.
    """
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return ""
    first = raw.split(b"\0", 1)[0].decode(errors="replace")
    return Path(first).name


def matching(match: str) -> list[tuple[int, str]]:
    """Every live process whose binary name contains `match`."""
    out = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        name = process_name(pid)
        if name and match in name:
            out.append((pid, name))
    return out


def sample(match: str, at: float) -> list[Sample]:
    """One pass over the process table."""
    out = []
    for pid, name in matching(match):
        rss = resident_kib(pid)
        if rss is None:
            continue
        out.append(
            Sample(
                at=at,
                pid=pid,
                name=name,
                rss_kib=rss,
                fds=count_fds(pid),
                threads=count_threads(pid),
            )
        )
    return out


def summarize(rows: list[Sample]) -> None:
    """Peak, mean and last, per process — the three numbers that decide a shape."""
    by_pid: dict[int, list[Sample]] = {}
    for row in rows:
        by_pid.setdefault(row.pid, []).append(row)
    print(
        f"{'pid':>7} {'process':<12} {'samples':>7} "
        f"{'peak MiB':>9} {'mean MiB':>9} {'last MiB':>9}"
    )
    for pid, samples in sorted(by_pid.items()):
        peak = max(sample.rss_kib for sample in samples)
        mean = sum(sample.rss_kib for sample in samples) / len(samples)
        last = samples[-1].rss_kib
        print(
            f"{pid:>7} {samples[0].name:<12} {len(samples):>7} "
            f"{peak / 1024:>9.0f} {mean / 1024:>9.0f} {last / 1024:>9.0f}"
        )


#: A `/proc/<pid>/statm` body for `--self-check`: seven fields, 300 pages resident.
CANNED_STATM = ["4096", "300", "100", "1", "0", "200", "0"]

#: The thread count inside `CANNED_STATUS`, which the parser must find.
CANNED_THREADS = 14

#: A `/proc/<pid>/status` body with the one line the sampler reads.
CANNED_STATUS = f"Name:\tmolehill\nThreads:\t{CANNED_THREADS}\nVmRSS:\t 1024 kB\n"


def self_check() -> int:
    """The arithmetic, on canned `/proc` shapes, without a process."""
    failures = []
    # statm's second field is resident pages, not bytes.
    rss = int(CANNED_STATM[STATM_RESIDENT]) * PAGE_KIB
    if rss <= 0:
        failures.append(f"resident {rss} is not positive")
    if PAGE_KIB not in (4, 16, 64):
        failures.append(f"implausible page size {PAGE_KIB} KiB")
    # A status block with Threads must yield the count, not the label.
    threads = 0
    for line in CANNED_STATUS.splitlines():
        if line.startswith("Threads:"):
            threads = int(line.split()[1])
    if threads != CANNED_THREADS:
        failures.append(f"threads {threads} is not {CANNED_THREADS}")
    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        return 1
    print("observe self-check ok")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="bench observe",
        description="sample a process set's resident set, descriptors and threads",
    )
    parser.add_argument("--match", default=DEFAULT_MATCH)
    parser.add_argument("--interval", type=float, default=DEFAULT_INTERVAL_S)
    parser.add_argument("--duration", type=float, default=DEFAULT_DURATION_S)
    parser.add_argument("--out", default="", help="NDJSON path (default: ~/tmp)")
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args(argv)

    if args.self_check:
        return self_check()
    if args.interval <= 0 or args.duration <= 0:
        parser.error("--interval and --duration must be positive")

    out = (
        Path(args.out)
        if args.out
        else Path.home() / "tmp" / f"observe-{int(time.time())}.ndjson"
    )
    out.parent.mkdir(parents=True, exist_ok=True)

    rows: list[Sample] = []
    started = time.monotonic()
    deadline = started + args.duration
    seen: set[int] = set()
    with out.open("w") as handle:
        while time.monotonic() < deadline:
            now = time.time()
            for one in sample(args.match, now):
                handle.write(json.dumps(one.row()) + "\n")
                rows.append(one)
                seen.add(one.pid)
            handle.flush()
            time.sleep(args.interval)
    if not rows:
        print(
            f"no process matched {args.match!r} in {args.duration:.0f}s",
            file=sys.stderr,
        )
        return 1
    summarize(rows)
    print(f"\n{len(rows)} samples from {len(seen)} processes in {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
