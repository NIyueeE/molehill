# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""When does a UDP worker queue actually overflow — D27's missing evidence.

`MOLEHILL_UDP_STATS` reports two drop counters, and the milestone table's M2c
row (D27: "a new peer goes to the *shortest* worker queue") has stayed unlanded
because both have read zero under every measured load. Zero under the soak
schedule is not evidence: that schedule's UDP probe is a handful of datagrams per
second, and the drain path is two data channels already busy carrying half of a
20-stream bulk run. This script asks the question those counters exist for, with
load that can actually reach the queues:

* **the server's worker channel** holds `DEFAULT_UDP_SENDQ_SIZE` (1024)
  datagrams. A new peer is assigned **round-robin** over the live workers and
  then pinned, so a queue overflows when arrivals into one worker outrun that
  worker's data channel long enough to fill 1024 datagrams.
* **one peer cannot be spread** (session affinity), so "one visitor alone
  overflows its worker" and "many visitors together overfill one worker because
  the assignment was unlucky" are different findings with different fixes. The
  first is a per-peer queue question, which D27 would not touch; the second is
  exactly what D27 proposes to fix.

Three things this instrument had to get right before its numbers meant anything,
each learned from a failed version of it:

* **the sink must not be the bottleneck.** A Python echo absorbed 0.76 Gbit/s of
  a 1.98 Gbit/s offer — with that in the path, "the queue overflowed" would have
  been a statement about the sink. Here the sink is a **separate process** that
  only receives (its own GIL, no reply path), it reports its running total every
  half second, and the table prints it beside the offered rate so a reader can
  see which side saturated first.
* **`iperf3 -u` cannot generate this load.** Its UDP tests still open a TCP
  control connection to the same port first, and a molehill UDP service carries
  UDP only, so the client is refused before a single datagram is sent.
* **a measurement that failed must not read as "no drops".** The rate ramp is
  reported whether or not the counters moved, and a step that never reached the
  sink is called out as inconclusive instead of folded into a clean verdict.
* **"the tunnel is the limit" needs a control.** Every run ends by blasting the
  same visitors *straight at the sink*, with no tunnel in between: that step's
  absorbed rate is the sink's own ceiling, and only if it is clearly above the
  tunnelled step can the drop be attributed to the tunnel. Without that control,
  a slow sink and a slow tunnel look the same from the counters.

The verdict rule, stated before the numbers: if a **single** visitor drops at a
rate the sink clearly keeps up with, the queue is too small for one visitor and
assignment (D27) is not the lever; if only the many-visitor step drops and the
`pinned` counts are even, the tunnel is the limit and round-robin is not the
lever either; if only the many-visitor step drops and one worker holds far more
peers than another, that is D27's evidence.
"""

import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

#: Run as a child: one thread receives datagrams and counts them, the main thread
#: answers a request on stdin with the total. Answering on request rather than
#: printing on a timer is what keeps the parent from blocking on a readline that
#: has no line coming (the first version did, and hung the whole run).
SINK = """
import socket, sys, threading
count = 0
lock = threading.Lock()
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.settimeout(0.1)


def receive():
    global count
    while True:
        try:
            s.recvfrom(65535)
        except TimeoutError:
            continue
        except OSError:
            return
        with lock:
            count += 1


threading.Thread(target=receive, daemon=True).start()
for _ in sys.stdin:
    with lock:
        print(count, flush=True)
"""

BIN = Path("/home/dsh/repo/molehill/target/release/molehill")
PACKET = 1400
#: The server's per-worker channel capacity (`DEFAULT_UDP_SENDQ_SIZE`), quoted so
#: a reader can hold the offered load against what one queue absorbs.
WORKER_QUEUE = 1024
STATS = re.compile(
    r"udp-stats:.*?affinity=(?P<affinity>\d+).*?evictions=(?P<evictions>\d+)"
    r".*?workers=(?P<workers>\d+).*?pinned=(?P<pinned>\S+)"
    r".*?queue_full=(?P<queue_full>\d+).*?no_worker=(?P<no_worker>\d+)"
)


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument(
        "--rates",
        default="10000,25000,50000,100000",
        help="per-visitor offered rates (datagrams/s), comma separated",
    )
    p.add_argument(
        "--visitors", type=int, default=16, help="visitors in the flat-out step"
    )
    p.add_argument("--secs", type=float, default=8.0, help="seconds per step")
    p.add_argument("--workers", type=int, default=2, help="the service's udp_workers")
    p.add_argument("--out", type=Path, help="write the numbers here as JSON")
    return p.parse_args()


def write_configs(dirpath: Path, workers: int) -> tuple[Path, Path, int, int]:
    control, exposed, backend = 26501, 26502, 26503
    server = dirpath / "server.toml"
    client = dirpath / "client.toml"
    server.write_text(
        "[server]\n"
        'default_token = "udp-stress"\n'
        f'allow_ports = ["{exposed}"]\n\n'
        "[server.control]\n"
        f'bind_addr = "127.0.0.1:{control}"\n'
    )
    client.write_text(
        "[client]\n"
        'default_token = "udp-stress"\n\n'
        "[client.control]\n"
        f'default_remote_addr = "127.0.0.1:{control}"\n\n'
        "[client.services.echo]\n"
        'protocol = "udp"\n'
        f'local_addr = "127.0.0.1:{backend}"\n'
        f'remote_bind_addr = "127.0.0.1:{exposed}"\n'
        f"udp_workers = {workers}\n"
    )
    return server, client, exposed, backend


class Sink:
    """The receive-only sink, in its own process, reporting its running total."""

    def __init__(self, port: int) -> None:
        self.proc = subprocess.Popen(
            [sys.executable, "-c", SINK, str(port)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        time.sleep(0.4)

    def count(self) -> int:
        """Ask the sink for its total. One request, one reply, no waiting."""
        if self.proc.stdin is None or self.proc.stdout is None:
            return 0
        try:
            self.proc.stdin.write("\n")
            self.proc.stdin.flush()
            return int(self.proc.stdout.readline().strip() or 0)
        except (ValueError, OSError):
            return 0


def read_stats(log: Path) -> dict | None:
    """The last `udp-stats` line, parsed. Its counters are cumulative."""
    last = None
    for line in log.read_text(errors="replace").splitlines():
        if "udp-stats:" in line:
            last = line
    if last is None:
        return None
    m = STATS.search(last)
    if m is None:
        return None
    per_worker = {}
    if m.group("pinned") != "-":
        for item in m.group("pinned").split(","):
            worker, _, count = item.partition(":")
            per_worker[worker] = int(count)
    return {
        "affinity": int(m.group("affinity")),
        "evictions": int(m.group("evictions")),
        "workers": int(m.group("workers")),
        "pinned": per_worker,
        "queue_full": int(m.group("queue_full")),
        "no_worker": int(m.group("no_worker")),
    }


def one_visitor(
    exposed: int, secs: float, pps: int | None, sent: list, index: int
) -> None:
    """One visitor socket. `pps=None` is flat out, otherwise paced to `pps`."""
    payload = bytes(PACKET)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.connect(("127.0.0.1", exposed))
    end = time.perf_counter() + secs
    if pps is None:
        while time.perf_counter() < end:
            try:
                s.send(payload)
            except OSError:
                break
            sent[index] += 1
    else:
        gap = 1.0 / pps
        next_send = time.perf_counter()
        while time.perf_counter() < end:
            try:
                s.send(payload)
            except OSError:
                break
            sent[index] += 1
            next_send += gap
            slack = next_send - time.perf_counter()
            if slack > 0:
                time.sleep(slack)
            else:
                next_send = time.perf_counter()
    s.close()


def step(label: str, visitors: int, pps: int | None, ctx: dict) -> dict:
    """One step: run the visitors, then read what the counters and sink saw."""
    server_log = ctx["server_log"]
    sink = ctx["sink"]
    before = read_stats(server_log) or {"queue_full": 0, "no_worker": 0}
    sink_before = sink.count()
    sent = [0] * visitors
    t0 = time.perf_counter()
    threads = [
        threading.Thread(
            target=one_visitor, args=(ctx["exposed"], ctx["secs"], pps, sent, i)
        )
        for i in range(visitors)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.perf_counter() - t0
    time.sleep(1.2)  # let the last stats tick land
    after = read_stats(server_log) or before
    absorbed = sink.count() - sink_before
    return {
        "step": label,
        "visitors": visitors,
        "target_pps": pps,
        "offered_pps": round(sum(sent) / elapsed),
        "absorbed_pps": round(absorbed / elapsed),
        "absorbed_gbit": round(absorbed * PACKET * 8 / elapsed / 1e9, 3),
        "drops_queue_full": after["queue_full"] - before["queue_full"],
        "drops_no_worker": after["no_worker"] - before["no_worker"],
        "pinned": after["pinned"],
        "workers": after["workers"],
        "secs": round(elapsed, 2),
    }


def verdict(ramp: list, steps: list, many: dict, control: dict) -> str:
    """Which finding this run produced. See the module docstring."""
    if any(s["absorbed_pps"] == 0 for s in steps):
        return (
            "inconclusive: nothing reached the sink, so no load entered the "
            "forwarding path and the counters say nothing"
        )
    if control["absorbed_pps"] <= many["absorbed_pps"] * 1.2:
        return (
            f"inconclusive: the sink itself absorbed only "
            f"{control['absorbed_pps']:,}/s when the same visitors blasted it "
            f"directly, against {many['absorbed_pps']:,}/s through the tunnel — the "
            "sink is the limit, not the forwarding path"
        )
    single = [s for s in ramp if s["drops_queue_full"] or s["drops_no_worker"]]
    if single:
        first = single[0]
        return (
            f"a single visitor already overflows its worker's queue at "
            f"{first['offered_pps']:,} datagrams/s offered "
            f"({first['drops_queue_full']:,} drops, sink keeping up at "
            f"{first['absorbed_pps']:,}/s): the queue is sized for a slower "
            "visitor, and assignment (D27) is not the lever"
        )
    if many["drops_queue_full"] or many["drops_no_worker"]:
        counts = list(many["pinned"].values())
        even = bool(counts) and (max(counts) - min(counts)) <= 1
        if even:
            return (
                "visitors are spread evenly and the aggregate still overflows: the "
                "tunnel is the limit, not the assignment"
            )
        return "the overflow coincides with an uneven visitor spread: D27's premise"
    return (
        "no drop at any offered rate ("
        + ", ".join(f"{s['offered_pps']:,}/s" for s in steps)
        + f") — the counters stay zero while the sink absorbs "
        f"{control['absorbed_pps']:,}/s directly, so the policy stays unlanded, "
        "now with a number instead of a shrug"
    )


def measure(args: argparse.Namespace) -> dict:
    """Start a real pair, run the ramp and the flat-out step, tear it down."""
    work = Path(tempfile.mkdtemp(prefix="udp-stress-"))
    server_cfg, client_cfg, exposed, backend_port = write_configs(work, args.workers)
    server_log = work / "server.log"
    sink = Sink(backend_port)
    env = {**os.environ, "MOLEHILL_UDP_STATS": "1"}
    server = subprocess.Popen(
        [BIN, str(server_cfg)],
        stdout=server_log.open("w"),
        stderr=subprocess.STDOUT,
        env=env,
    )
    client = subprocess.Popen(
        [BIN, str(client_cfg)], stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT
    )
    try:
        deadline = time.time() + 20
        while time.time() < deadline and read_stats(server_log) is None:
            time.sleep(0.2)
        if read_stats(server_log) is None:
            sys.exit("the service never registered (no udp-stats line on the server)")
        ctx = {
            "exposed": exposed,
            "server_log": server_log,
            "sink": sink,
            "secs": args.secs,
        }
        ramp = [
            step(f"1 visitor @ {int(rate):,}/s", 1, int(rate), ctx)
            for rate in args.rates.split(",")
        ]
        many = step(f"{args.visitors} visitors, flat out", args.visitors, None, ctx)
        # The control: the same load straight at the sink, no tunnel. Its absorbed
        # rate is the sink's ceiling, which is what makes "the tunnel is the limit"
        # a measurement instead of an assumption.
        control = step(
            f"{args.visitors} visitors -> sink, no tunnel",
            args.visitors,
            None,
            {**ctx, "exposed": backend_port, "server_log": server_log},
        )
    finally:
        for p in (client, server, sink.proc):
            p.terminate()
        for p in (client, server, sink.proc):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
    steps = [*ramp, many, control]
    return {
        "work": str(work),
        "packet_bytes": PACKET,
        "worker_queue": WORKER_QUEUE,
        "udp_workers": args.workers,
        "steps": steps,
        "verdict": verdict(ramp, steps, many, control),
    }


def report(record: dict) -> None:
    print(
        f"# per-worker queue = {record['worker_queue']} datagrams, "
        f"udp_workers = {record['udp_workers']}, {record['packet_bytes']} B payloads; "
        "the sink is a separate receive-only process"
    )
    print(
        f"{'step':24s}{'offered/s':>12s}{'absorbed/s':>12s}{'Gbit/s':>9s}"
        f"{'queue_full':>12s}{'no_worker':>11s}   pinned"
    )
    for s in record["steps"]:
        pinned = ", ".join(f"{w}:{n}" for w, n in sorted(s["pinned"].items())) or "-"
        print(
            f"{s['step']:24s}{s['offered_pps']:>12,d}{s['absorbed_pps']:>12,d}"
            f"{s['absorbed_gbit']:>9.2f}{s['drops_queue_full']:>12,d}"
            f"{s['drops_no_worker']:>11,d}   {pinned}"
        )
    print(f"\nVERDICT: {record['verdict']}")


def main() -> int:
    args = parse_args()
    if not BIN.exists():
        sys.exit(f"build the release binary first: {BIN} does not exist")
    record = measure(args)
    report(record)
    if args.out:
        args.out.write_text(json.dumps(record, indent=2) + "\n")
        print(f"-> {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
