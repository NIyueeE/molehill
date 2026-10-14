#!/usr/bin/env python3
"""The model's iperf3 conventions: one definition of what a run measured.

`iperf_result` is the only place an iperf3 run is parsed, because the
conventions in it are what several numbers mean:

* the **window** is the test's own active seconds (`iperf_active_seconds`),
  which is not `-t`: a dropped or mangled interval would otherwise move it;
* the **rate** is the receiver's window, and the sender's accounting is
  recorded beside it rather than preferred (a fast sender into a slow shaper
  writes into its socket buffer and reports a full path as an empty one);
* a **failure is typed**: `ok`/`reason` with the raw artifact directory written
  on every outcome, so a later null can be diagnosed instead of guessed at.

The module has no dependencies outside the standard library and no knowledge of
any arm: it was the soak sweep's, and it is the model's now that the sweep is a
profile of the model.
"""

from __future__ import annotations

import contextlib
import json
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path


def iperf_active_seconds(doc: dict, secs: int) -> float:
    """Length of the MEASURED window = the sender's non-omitted intervals.

    This is the honest denominator for the sender-side rate: total bytes sent
    include the `-O` warm-up, so `bytes / sum_sent.seconds` (the full test
    duration) is not the measured-window rate. The receiver keeps a SEPARATE
    window (`sum_received.seconds`), reported alongside: after the sender
    stops, the backend can still be draining (6.24 s of receive window for a
    6.00 s send window on a shaped stage), and collapsing the two into one
    denominator is what made `sent` and `received` look incomparable.
    Falls back to the configured duration when the interval list is absent."""
    intervals = doc.get("intervals") or []
    acc = 0.0
    for iv in intervals:
        s = iv.get("sum") or {}
        if s.get("omitted"):
            continue
        # ALWAYS the interval's own span. iperf3's per-interval `seconds`
        # field is not that span: on the interval that follows the `-O`
        # warm-up it reports the warm-up plus the interval (2.005 s for a
        # 0.005 s tail), which summed to 9.0 s for an 8 s test and silently
        # deflated the headline by ~12% on the loopback stage.
        acc += max(0.0, s.get("end", 0.0) - s.get("start", 0.0))
    return acc if acc > 0 else float(secs)


@dataclass(frozen=True)
class IperfDial:
    """Where an iperf3 client dials, and how it gets there.

    The host-loopback model dials `127.0.0.1:<port>` from this process, which is
    what passing a plain port to `iperf_result` still means. An arm that does
    not run on host loopback — the bench model, whose visitor dials a
    tool-exposed address from inside a network namespace — needs a target other
    than the loopback and an `ip netns exec <ns>` prefix. Carrying both in one
    value keeps the parsing (the `-O` warm-up, the measured-window convention,
    the degenerate-sender fallback) shared: an out-of-model arm is summarised by
    the same code the gate's numbers come from, not by a second implementation
    that can drift from it.
    """

    port: int
    host: str = "127.0.0.1"
    argv_prefix: tuple = ()
    #: Run the UDP test shape instead of TCP: `-u`, optionally rate-limited
    #: with `bitrate` and with fixed-size datagrams (`length`). The L3-vs-L4
    #: comparison needs it because the two architectures carry datagrams in
    #: different *kinds* (a packet either way, versus a per-peer channel), and
    #: iperf3's UDP mode is the sink that can keep up with a rate ladder.
    udp: bool = False
    bitrate: str = ""
    length: int = 0
    #: Seconds iperf3 omits from its accounting (`-O`): the Soak model's
    #: convention on a shaped path whose slow start is seconds long. The bench
    #: model's bulk scenarios carry it too, and they do *not* take their byte
    #: ratios from this run's payload counters — around the `-O` boundary
    #: iperf3's interval list can lose a whole measured interval, so those
    #: ratios come from interface counters instead
    #: (`benches/scripts/bench/workloads.py`).
    omit: int = 2


def iperf_result(
    target: int | IperfDial,
    streams: int,
    secs: int,
    timeout: float,
    artifact: Path | None,
) -> dict:
    """One `-P streams` iperf3 client run through the tunnel; never raises.

    `target` is the port to dial (host loopback) or an [`IperfDial`] for every
    other case. A raw artifact directory is captured on EVERY outcome
    (requested and actual timeout, exit status, stdout, stderr) so a later null
    in the results file can be re-diagnosed instead of guessed at."""
    dial = target if isinstance(target, IperfDial) else IperfDial(target)
    if artifact is not None:
        artifact.mkdir(parents=True, exist_ok=True)
    cmd = [
        *dial.argv_prefix,
        "iperf3",
        "-J",
        "-c",
        dial.host,
        "-p",
        str(dial.port),
        "-t",
        str(secs),
        *(["-O", str(dial.omit)] if dial.omit else []),
        *(["-u"] if dial.udp else []),
        *(["-b", dial.bitrate] if dial.bitrate else []),
        *(["-l", str(dial.length)] if dial.length else []),
        "-P",
        str(streams),
    ]
    t0 = time.perf_counter()
    timed_out = False
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, check=False
        )
        rc, out, err = proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as e:
        timed_out = True
        rc = None
        # Both streams need the same coercion: `text=True` does not guarantee
        # str on this path (a known CPython quirk), and the artifact writer
        # below refuses bytes — which is how a timed-out UDP run turned into a
        # TypeError instead of a recorded timeout.
        err = (
            e.stderr
            if isinstance(e.stderr, str)
            else (e.stderr or b"").decode("utf-8", "replace")
        )
        out = (
            e.stdout
            if isinstance(e.stdout, str)
            else (e.stdout or b"").decode("utf-8", "replace")
        )
    wall = round(time.perf_counter() - t0, 2)
    if artifact is not None:
        (artifact / "cmd.txt").write_text(" ".join(cmd) + "\n")
        (artifact / "stdout.json").write_text(out or "")
        (artifact / "stderr.txt").write_text(err or "")
        (artifact / "meta.json").write_text(
            json.dumps(
                {
                    "streams": streams,
                    "secs": secs,
                    "timeout_s": timeout,
                    "exit": rc,
                    "timed_out": timed_out,
                    "wall_s": wall,
                },
                indent=1,
            )
        )
    base = {
        "streams": streams,
        "secs": secs,
        "timeout_s": round(timeout, 1),
        "wall_s": wall,
        "timed_out": timed_out,
        "exit": rc,
        "artifact": str(artifact / "stdout.json") if artifact else None,
    }
    doc = None
    with contextlib.suppress(ValueError):
        doc = json.loads(out) if out else None
    if doc and doc.get("error"):
        return base | {"ok": False, "reason": f"iperf3 error: {doc['error']}"}
    if timed_out:
        return base | {
            "ok": False,
            "reason": f"client timed out after {timeout:.0f}s "
            "(harness bound, test may still have been "
            "transferring)",
        }
    if rc != 0:
        detail = (err or out or "").strip().replace("\n", " ")[:200]
        return base | {
            "ok": False,
            "reason": f"iperf3 exit {rc}: {detail or 'no output'}",
        }
    if doc is None or "end" not in doc:
        return base | {"ok": False, "reason": "unparseable iperf3 output"}
    end = doc["end"]
    sent = end.get("sum_sent") or {}
    recv = end.get("sum_received") or {}
    # The whole test's bytes, warm-up included (the omitted intervals carry
    # their own byte counts). Recorded for provenance: around the `-O` boundary
    # iperf3 can also mangle the list itself — a measured 1 ms interval carrying
    # hundreds of MB, and sometimes a whole measured interval missing — which
    # under-reported one run's total by 12 %, so this is not a safe denominator
    # for a byte ratio. `sum_sent`/`sum_received` cover only the post-omit
    # window, which is the right numerator for the rate.
    total_b = sum(
        (iv.get("sum") or {}).get("bytes", 0) for iv in doc.get("intervals") or []
    )
    active_s = iperf_active_seconds(doc, secs)
    sent_b = sent.get("bytes", 0)
    recv_b = recv.get("bytes", 0)
    # iperf3 3.18 nests each connection's own counters under
    # end.streams[i].sender (a stream entry's top level is empty for a
    # sender-side run): reading the top level yields all-zero per-stream byte
    # counts, which is how a perfectly healthy run can still be summarised as
    # "8 zero streams"
    streams = [(s.get("sender") or {}) for s in end.get("streams", [])]
    # `sum_sent` covers only the post-omit window, so its byte count is the
    # right numerator for the measured window — EXCEPT when the sender's
    # writes all completed inside the `-O` warm-up and backpressure then
    # blocked it for the whole measured window. That is exactly what a fast
    # sender into a slow shaper does: measured at rate20_rtt40, the warm-up
    # interval carried 153 MB at 1.22 Gbit/s and every measured interval
    # showed 0 bytes sent, while the receiver still logged 29 MB (the shaped
    # link rate). The receiver's count is then the only evidence of what the
    # path carried; both sides are recorded either way.
    sent_gbps = sent_b * 8 / active_s / 1e9 if active_s else 0.0
    recv_gbps = recv_b * 8 / active_s / 1e9 if active_s else 0.0
    degenerate = recv_b > 0 and sent_b * 2 < recv_b
    # Headline policy: the sender's post-omit bytes over the measured window
    # is the clean, consistent definition (ingress rate). max(sent, recv)
    # would inflate: the receiver's total can include warm-up backlog still
    # draining inside the window (64-stream loopback measured 59.1 Gbit/s
    # received vs 45.9 sent). Only when the sender's accounting is provably
    # defeated does the receiver's count become the evidence, and then it is
    # the ONLY evidence of what the path carried.
    headline_gbps = recv_gbps if degenerate else sent_gbps
    # A UDP run reports a different summary: datagrams, and the loss and
    # jitter either side saw. It is recorded beside the byte accounting (which
    # iperf3 also fills for UDP) rather than replacing it.
    udp_stats = None
    if dial.udp:
        total = end.get("sum") or {}
        udp_stats = {
            "packets": total.get("packets"),
            "lost_packets": total.get("lost_packets"),
            "lost_percent": total.get("lost_percent"),
            "jitter_ms": total.get("jitter_ms"),
            "seconds": total.get("seconds"),
        }
    return base | {
        "ok": True,
        "bytes_sent": sent_b,
        "bytes_received": recv_b,
        "udp": udp_stats,
        "bytes_interval_total": total_b,
        # What the probe itself cost on both ends, as iperf3 measured it: the
        # tool's CPU is sampled from /proc, and a path cannot be called cheap
        # when the instrument is the busy side.
        "cpu_utilization_percent": end.get("cpu_utilization_percent"),
        "active_s": round(active_s, 3),
        "gbps_sent_only": round(sent_gbps, 4),
        "gbps_received_window": round(recv_gbps, 4),
        "gbps_headline": round(headline_gbps, 4),
        "sender_accounting_degenerate": degenerate,
        "gbps_received_own_window": round(recv.get("bits_per_second", 0.0) / 1e9, 4),
        "receiver_window_s": round(recv.get("seconds", 0.0), 3),
        "retransmits": sent.get("retransmits", 0),
        "per_stream_bytes": [s.get("bytes", 0) for s in streams],
        "per_stream_gbps": [
            round(s.get("bits_per_second", 0.0) / 1e9, 4) for s in streams
        ],
        "mean_rtt_us": streams[0].get("mean_rtt") if streams else None,
    }


# Method constants of the derived statistics. They are named because they
# decide whether a slope or a rate is reported at all, i.e. they are part of
# the method and not incidental guards. They live here, with the statistics
# themselves, so the runner, the gate and the charts cannot disagree about
