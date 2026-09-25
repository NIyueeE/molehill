# Benchmarks

How the numbers in the [README](../README.md#benchmarks) are produced, what
they can and cannot answer, and how to reproduce them on your own hardware.

Chinese mirror: [benchmarks.zh.md](benchmarks.zh.md).

## What is measured

A **workload over time**, not one average per network condition. Each tool is
driven through the identical client-side workload — one interactive stream (a
fresh TCP connection per ping: the instrument the SLO is stated on), N bulk TCP
streams, a steady rate of short-lived connections, and one UDP session — while
the path follows a scripted schedule of network conditions. The tool's
processes start once and the shaping changes in place, so the session is never
rebuilt: how a tool *adapts* to a degrading and then recovering path is part of
the measurement, not a warm-up cost hidden before it.

Everything is observed from outside the tool (throughput and per-interval
retransmits from iperf3, RTT/loss/jitter from the probes, RSS / CPU / open fds
/ threads from `/proc`). That is what lets the peer tools be measured by the
same workload and charted in the same panels — and it is why molehill's own
internal counters never appear in the comparison.

Measurements are single-machine: visitor, server, client and backend all share
one host, with the network condition applied to the loopback path. Absolute
throughput therefore describes *this* host; the shapes, the ordering and the
SLO behaviour are what travel.

## How to read the charts

- **The SLO line (dashed).** An interactive stream must keep p99 at or under
  50 ms, with zero errors, for a path to be considered usable. It is the one
  line every chart shares.
- **The vertical axis of a response time is logarithmic.** A degraded path
  costs three orders of magnitude; on a linear axis the healthy stages would be
  invisible.
- **The shaded bands are the path classes**, named along the top; green bands
  are the unshaped control stages, grey ones are deliberately degraded. The
  last band is the same clean condition as the first: the **recovery axis**.
- **A red bar on the bottom edge is a wedge** — the interactive stream went
  silent (no response at all) for more than five seconds. Its duration is
  printed in the corner of the panel. A tool that stays wedged is a finding;
  one that recovers is the more useful answer.
- **Solids and dashes are the per-stage distribution**: the solid step line is
  each stage's median, the dashed one its p99. They exist because a scatter of
  tens of thousands of samples hides the summary, and the summary hides the
  outliers — you want both.
- **The small-multiples figure** (`soak-<version>-stages.png`) answers the
  comparison question directly: one panel per condition, one lollipop per tool
  (dot = median, bar = p99, tick = worst single second). An `x` means that
  stage produced no samples for that tool.
- **The drift figure** prints the fitted slope of every line, because a leak is
  a slope over time, not a level: an RSS line that is high but flat is not a
  leak, and one that climbs slowly is.

## The stage schedule

This is the default schedule — what the `capacity` and `rrul` runs walk
through. Three other schedules exist, and which one a run uses is recorded in
its results file (`meta.timeline`, beside the path classes):

- **`soak`** rotates a shorter one for a longer time: `clean` → `loss1` →
  `rtt100` → `loss5` → `clean`, 180 s each;
- **`cost`** and **`screen`** run a single stage — `--path` at `--secs`
  (60 s by default);
- **any test** accepts an explicit `--timeline clean:60,loss1:120,...`.

| Stage | What it emulates | Applied to the path | Duration |
|---|---|---|---|
| `clean` | a healthy network (the control) | nothing | 150 s |
| `rtt100` | a long-haul or satellite link | 100 ms delay | 120 s |
| `loss1` | a lossy wifi or mobile link | 10 ms delay, 1 % loss | 120 s |
| `loss5` | a badly congested path | 100 ms delay, 5 % loss | 120 s |
| `rate100` | a 100 Mbit uplink | 100 Mbit/s, 20 ms delay | 120 s |
| `rate20` | a 20 Mbit/s uplink | 20 Mbit/s, 40 ms delay | 120 s |
| `jitter` | a bufferbloated access link | 20 ms delay ± 10 ms | 120 s |
| `clean` | recovery — is the tool still the tool it was? | nothing | 150 s |

Two further classes carry the **fragmentation** axis and are deliberately not in
any default timeline:

| Class | What it emulates | Applied to the path |
|---|---|---|
| `mtu1280` | a tunnel or an IPv6-minimum path | interface MTU 1280 |
| `loss1_mtu1280` | a lossy link that also fragments | 10 ms delay, 1 % loss, interface MTU 1280 |

MTU is an *interface* property, not a qdisc: a stage that uses one of these
classes changes the path for **every packet on `lo`** during that stage — the
peers', the harness's and the tool's control plane included — which is why they
stay out of the default schedules and are run as focused single-stage cells
(`--test=cost --path=loss1_mtu1280`, or `--test=screen --path=…` to A/B two
builds on it). `meta.mtu_restore_to` records the interface's MTU at the start of
the run and the harness restores it on teardown, failing loudly if it cannot —
a leftover 1280 would poison every later run on the host. The reason the axis
exists at all: `lo` is MTU 65536, so without it every datagram fits in one
fragment and the cost of a lost fragment is unmeasurable. Read the two classes
together with the `carrier = "kcp"` row below.

The schedule, the durations, the sample rates and the SLO are recorded in every
results file (`meta`), so a chart can always be traced back to the method that
produced it. So is the shaping applied to each class — including the rate
stages' queue depth (`rate`/`limit 2000`), which bounds how much traffic the
shaper may hold and therefore what a burst through it can do.

Only the data plane is shaped. The tool's control channel stays on the
unshaped path: shaping it kills the heartbeat and turns a capacity measurement
into a wedge study.

## The SLO

An interactive stream's p99 at or under **50 ms**, with a zero error rate. It is
the break condition of the `capacity` test, the dashed line in every chart, and
what the release gate checks on the clean stages. Degraded stages are *expected*
to sit far above it — that is the measurement, not a failure.

## Test types

| Type | The question it answers |
|---|---|
| `capacity` | how much bulk load can the tool carry while a fresh interactive connection still meets the SLO? (sustainable load + the full response-time curve) |
| `rrul` | under saturation, what happens to a new visitor's latency as the path changes over time? (the figure in the README) |
| `soak` | over a long run on a rotating path: does anything leak, drift or degrade? |
| `cost` | at a fixed operating point, how many CPU-seconds does one carried Gbit/s cost? |
| `screen` | for a development change: is the difference between two builds a claim or noise? |

## What each configuration choice costs (per-decision measurements)

These figures are from the **retired per-cell model** (the v0.8.x method: one
cold-started average per tool per network condition, reported as a median over
repetitions). They are kept because they are still the only measured basis for
a few configuration decisions, and they are **not comparable** with the
workload-over-time figures above — the v0.9.0 run covers the default
configuration only. Treat them as directional, and re-measure your own case.

| Decision | Option | Measured basis (retired per-cell model) |
|---|---|---|
| `mode` | `"multiplex"` (default) | 1-stream 10.0 Gbit/s on loopback vs 19.2 for `direct`; at 8 streams 19.5 vs 23.3; multiplex absorbs per-connection setup (churn ~4.8k connects/s) and saves FDs / ports / NAT mappings |
| `mode` | `"direct"` | raw single-stream throughput; one physical tunnel per stream (FD / port / NAT cost scales with stream count) |
| `count` | `1` | one tunnel for everything: no aggregation and one retransmit domain shared by every stream (loopback 8-stream aggregate 9.2 vs 19.5 Gbit/s at count = 4; loss5 head-of-line max 2.5 s vs 1.6 s) |
| test type | what it answers |
|---|---|
| `rrul` | the mixed workload over the full stage schedule (the release run) |
| `soak` | the same workload, longer, for drift |
| `cost` | one stage, one path: what a configuration choice costs |
| `capacity` | the ceiling probe: how many streams until it stops scaling |
| `screen` | interleaved A/B of two builds on one path |
| `reconnect` | cold start: client start → every registered service answering, five repetitions per build (interleaved when `--ab` is given) |

| `count` | `4` (default) | aggregates beyond one flow (loss1 8-str 12.3 vs 4.5 Gbit/s) and isolates head-of-line blocking (rtt10 max gap 80.6 vs 100.1 ms at count = 1); yamux ceiling `count × 64` concurrent connections |
| `count` | `8+` | ~512 concurrent connections (8 tunnels × 64 yamux streams); 8 physical tunnels per service (NAT mappings ×8) |
| `carrier` | `"tcp"` (default) | ahead of the KCP carrier in every unflagged measurement (loopback 1-stream 5.8 vs 3.7 Gbit/s against the kcp4 arm on the noise transport), and far cheaper in memory (RSS 26 vs 85 MiB). One 8-stream loopback cell (14.9 vs 1.1 Gbit/s) is excluded here: it was bimodal across repetitions on both builds, so it is not evidence of anything |
| `carrier` | `"kcp"` | only when TCP data tunnels are blocked or throttled, or to A/B a UDP game on a high-latency path: its one measured win is UDP session quality at rtt100 (0 % loss, 20 ms maximum inter-packet gap vs 100+ ms for the TCP arms) |
| transport | `"plain"` | 10.0 / 19.5 Gbit/s (1 / 8 streams) on loopback |
| transport | `"noise"` | 5.8 / 14.9 Gbit/s; sub-millisecond RTT cost; CPU parity under full load |
| `pool_size` | 8 TCP / 2 UDP (defaults) | setup-to-first-byte p99 ~3.5 ms at 16-way churn; UDP shards distinct visitors across channels and never splits one session (session affinity) |
| `[server.data].stripe_count` | `K = 4` (experimental) | a single long-lived connection stops being bounded by one tunnel flow: 1-stream throughput +48.7 % on loopback, at the cost of a reorder buffer, +8.7 % RSS and +40.8 % CPU (per-frame CPU is halved, because the frames spread over four driver tasks) |

Which setting to pick, and why: [configuration.md](configuration.md#choosing-your-configuration-decision-tree).

## Comparability

- **Same model, same host.** Every results file records the method version, the
  host and the harness revision; a number from another host or another model is
  context, not a baseline.
- **Older releases are a different instrument.** Releases up to v0.8.x measured
  one average per tool per network condition, in a cold-started process, and
  reported a median over repetitions. Those tables cannot be compared with these
  figures — an average per cold cell cannot see a wedge, and several of this
  model's findings are wedges. Historical numbers stay in the release notes of
  their own version.
- **Variance is stated, not smoothed.** If a difference sits inside the spread
  of the runs being compared, it is reported as directional and no claim is
  made from it.

## Reproduce it yourself

On a Linux host with `iperf3` and `tc` (see `just bench-deps`):

```bash
just soak-peers    # download the peer tools' latest release binaries
just soak          # one tool (or a batch) through the stage schedule
just soak-plot     # render the charts and print the markdown tables
just soak-check    # verdict: completeness, endpoints, SLO, drift
```

`just soak --help` lists the test types, the variants (`mux`, `noise`, `mux1`,
`kcp4`, `mux-off`), the stage schedule and the batching controls; the load,
SLO and sample-rate knobs are environment variables (`SOAK_*`) and every one of
them is echoed into the results meta.

To compare **two of your own builds** without a full run:

```bash
just soak --test=screen --path=loss1 --streams-max=8 \
     --ab /path/to/bin-a,/path/to/bin-b --out results-screen.json
just soak-check --screen results-screen.json
```

The two builds are interleaved step by step in one run, so both see the same
machine state; the verdict claims a difference only when every step agrees in
sign and exceeds the threshold, and calls everything else directional.

The release gate — what a published number must satisfy before a tag can carry
it — is documented in [release.md](release.md).
