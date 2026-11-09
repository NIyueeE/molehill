# Benchmarks

One model produces every performance number in this repository, and this page
owns its method: what is measured, how a metric is defined, how to read a
chart, what the numbers may be compared with, and how to reproduce a run. The
published numbers themselves are in the [README](../README.md#benchmarks).

Chinese mirror: [benchmarks.zh.md](benchmarks.zh.md).

<!-- TOC -->

- [What is measured](#what-is-measured)
- [The bench model (the measurement standard)](#the-bench-model-the-measurement-standard)
- [The conditions and the stage schedule](#the-conditions-and-the-stage-schedule)
- [How to read the charts](#how-to-read-the-charts)
- [Running it](#running-it)
- [Reading a result](#reading-a-result)
- [The gate](#the-gate)
- [What the model cannot answer](#what-the-model-cannot-answer)
- [What each configuration choice costs (per-decision measurements)](#what-each-configuration-choice-costs-per-decision-measurements)
- [The transparent-L3 wire question (the acceptance harness)](#the-transparent-l3-wire-question-the-acceptance-harness)
- [The UDP queue question (a molehill-only diagnostic)](#the-udp-queue-question-a-molehill-only-diagnostic)
- [Comparability](#comparability)
- [Reproduce it yourself](#reproduce-it-yourself)

<!-- /TOC -->

## What is measured

A **workload under a path**, observed from outside the tool. Every arm — the
product in its configurations, a reference tool, or a control with no tool in
the path at all — is driven through the same workloads, on the same
three-namespace topology, by the same probes, in the same phase of one
campaign:

- one **interactive stream**: a fresh TCP connection per ping, the instrument
  the SLO is stated on;
- **bulk TCP**: one to N streams (iperf3), the throughput axis;
- **short-lived connections**: a churn of fresh connections, the cost of
  arriving;
- **UDP**: a paced datagram session, and a rate ladder up to the drop point;
- and, for the release sweep, a **scripted schedule** of network conditions
  that changes in place while the tool keeps running, so how a tool *adapts* to
  a degrading and recovering path is part of the measurement rather than a
  warm-up cost hidden before it.

Everything is observed from outside (`/proc`, `ss`, the interfaces' counters,
the namespaces' TCP MIB, iperf3's own receiver summary), which is what lets a
peer binary, a shipped configuration and a control arm be described by one set
of instruments — and why no internal counter of any tool ever appears in a
comparison.

Measurements are single-machine: visitor, server, client and backend all share
one host, with the path conditions applied to a veth leg of the topology.
Absolute throughput therefore describes *this* host; the shapes, the ordering
and the SLO behaviour are what travel. The topology exists so that an L4 arm
(terminated TCP), an L3 arm (whole packets over a TUN) and the control arm are
measured on **the same path**: they differ in the address the visitor dials and
in the mode written into their configuration, and in nothing else.

An interactive stream's p99 at or under **50 ms**, with an error rate at or
under **0.5 %**, is the SLO: the break condition of the capacity ramp, the
dashed line in every chart, and what the gate checks on the clean stages.
Degraded stages are *expected* to sit far above it — that is the measurement,
not a failure.

## The bench model (the measurement standard)

`bench/src/bench/` is the standard and the only runner. The sweep the
release publishes is not a second instrument: it is this model's `sweep`
profile — the `timeline` and `capacity` scenarios over the product and the
reference tools — which is why the staged schedule and the load ramp travel in
one results file, under one schema, behind one gate.

```bash
sudo -n just bench                     # the smoke profile, about a minute
sudo -n just bench --profile dev --aa  # the default loop for an optimization
sudo -n just bench doctor              # what this host can and cannot measure
just bench list                        # every metric, condition and profile
just bench report ~/tmp/bench-*.json   # a stored run: tables, floors, verdicts
just bench plot ~/tmp/bench-*.json     # its charts and markdown tables
just bench gate RESULTS.json           # may this run be published?
just bench-selfcheck                   # the model's own checks (a fast gate)
```

> **The two syscall metrics are device-I/O readings, and only an L3 arm can
> produce them.** `/proc/<pid>/io` accounts file and pipe traffic, not socket
> payload: measured in this container, 200 MB through a socketpair moves
> `rchar` by 105 KB while the same bytes through a pipe move it by 209 MB. A
> forwarding arm's bytes never cross that boundary, so the reading would
> describe the runtime's plumbing instead of the path — the model records a
> **typed absence** for those arms and `just bench-selfcheck` fails if the rule
> and the metric list ever disagree. For an L3 arm the reading is exactly what
> it claims to be: the packets read from and written to the TUN device.

### The rules the model enforces

- **An arm states the width it measured, and the data plane's shape is not an
  arm parameter at all.** `pool_cap` is the pool's
  `[client.data].tunnels` for a forwarding arm and a transparent claim's
  **lane budget** for an L3 one — one connection per lane, because a claim never
  multiplexes. The data plane's **carrier is not an arm axis either**: the
  product chooses it at runtime (mhd, falling back to TCP), so there is no key
  to write and no arm field to set. The contrast lives in the *path* —
  `--condition udp_blocked --condition-leg tunnel` drops UDP on the leg the
  carrier crosses, so the mhd attempt cannot be established and the run measures
  the fallback, with the fallback line and `carrier="tcp"` in the pool's own
  output as its evidence — while a clean run carries mhd. To isolate one carrier
  instead (an mhd-only run under loss, say), set `MOLEHILL_DATA_CARRIER=mhd` in
  the environment the bench is launched from: a run records the `MOLEHILL_*`
  switches it inherited as instrumentation, so the forcing is auditable. The shape itself is
  derived from the service type (a forwarding service multiplexes, a claim never
  does), so an arm spec that still names a `mode` is refused by
  `just bench-selfcheck`. Records below name the arms the catalog had when each
  run was made: where one says `l3-mux` (one stream of a pool), `l3-mux4` (four
  streams) or `l3-raw4` (four connections), today's catalog has `l3` (the
  product's default lane count) and `l3-budget10` (a carrier budget the
  claims share — the L3 topology carries five claims, so ten lanes start at two
  each and the allocator concentrates the spare ones on whichever claim is
  carrying flows), and `l4-mux1` is gone because
  the topology's UDP floor refuses a one-tunnel pool — one stream on a
  two-tunnel pool is `l4-mux2` in a single-flow scenario
  ([HANDOFF.md](../HANDOFF.md), "The mux×1 gap, measured before touching it").

- **A metric is defined once, in code.** Unit, direction, the denominator it is
  a ratio over, the workload kinds that can produce it and the smallest
  difference worth calling a claim all live in `model.METRICS`; the table below
  is generated from that registry and `just bench list` prints it. A metric
  described one way and computed another fails `just bench-selfcheck`.
- **A control arm is mandatory.** Every scenario also runs on the same
  topology, ports and backend with no tool in the path. Without it a slow probe
  and a slow tunnel are the same reading, which is why the model always reports
  a tool arm *against its own control* rather than on its own.
- **A run states its whole method before its first sample.** The results file
  carries a fingerprint over the topology, the conditions and timelines, the
  scenarios and their *resolved* parameters, the instrument cadences and the
  model's own code. Two files may only be compared when it matches, and
  `just bench compare` names every key that differs instead of printing a
  difference between two methods.
- **The run measures its own resolution.** One arm is measured twice under two
  names (`--aa`; on by default in `dev`, `full` and `stage`), and the paired
  difference between those two halves is the smallest difference the run can
  believe. Every verdict raises the metric's own materiality floor to it, and
  the run prints the floors beside its tables — and, under the verdicts, a
  planning line for every cell the run *cannot* resolve: how many rounds
  would reach the metric's floor, or that the cell is too noisy for rounds to
  fix. A resolution of 16 % is a statement about the run, not an excuse: it
  now comes with the number that acts on it.
- **A claim is pre-registered, tested, and interval-checked.** Three legs,
  each reported in the verdict line instead of hidden:
  - *materiality* — the median difference clears the metric's floor and the
    run's own, as before;
  - *consistency* — the paired rounds go through an **exact sign test** at
    the level the run's length can honestly support: at three rounds that is
    `p = 0.25`, and the verdict prints `p` beside `alpha_eff`, so a
    short-run claim is *labelled* as the 25 %-error statement it is instead
    of passing silently as a 5 % one (six rounds reach p ≤ 0.05; the
    smallest p a run of n rounds can produce is `2/2^n`);
  - *the interval* — a **bootstrap CI** over the paired difference
    (reproducible: seeded from the run's fingerprint) says whether the
    effect lies *entirely beyond* the threshold (a claim's shape), *entirely
    inside* it (bounded evidence there is nothing worth calling), or
    *straddling* it (a direction, and nothing more).
- **Only a pre-registered endpoint can claim.** By default that is each
  scenario's headline metric; `--primary metric=…,scenario=…[,arm=…][,cell=…]`
  declares more (or narrower) ones *before the run*, and the declaration is
  part of the fingerprint — picking the winner after seeing the numbers
  changes the file's identity, not just its story. Every other comparison is
  **exploratory**: reported with its numbers, never a claim. The primary
  family's p-values go through a Benjamini-Hochberg correction and a claim
  must also survive it.
- **A verdict has five states, and "no claim" is now three of them.**
  `claim`; `directional` (a direction the interval straddles or the design
  cannot confirm); `indistinguishable` — which now means one of two *stated*
  things: the interval sits **inside** the threshold (bounded evidence of no
  difference worth calling) or it **straddles** it (under-resolved, and the
  line says which); `single-round` refuses to read a direction into one
  measurement; `unavailable` names what was not measured. A verdict that
  flips when any single paired round is dropped is marked **FRAGILE** in the
  line — reported, never silently repaired. A cell that lost more than half
  its rounds is refused outright (`drowned`), by the verdicts and by the
  gate.
- **Two more method keys travel in the fingerprint.** `primary` (the
  pre-registered endpoints above) and `normalize` — `--normalize control`
  divides each tool arm's per-round reading by the control's reading of the
  *same round*, which cancels the host's round-to-round drift where the cell
  is host-bound, and would manufacture invariance where the tool is what
  limits the path, so it is a stated design choice, not a default. The
  iperf3 version is recorded beside the instrument cadences: a version can
  change the summary fields the model reads, and a number from the other
  side of that boundary is not comparable with one from this side.
- **Evidence stays with the number.** Each cell records the commands it ran,
  the counters it read, the endpoint the probe dialed, the peers the backend
  saw, the raw per-round sample arrays and the log its probe left behind. A
  staged cell also carries its **shape within the window** — the interactive
  stream's per-second median, p99 and failure count, the intervals between its
  own attempts (a probe slower than its cadence reports the probe's load as the
  path's), the window's own edges in unix time, and the churn rate the stage
  asked for beside the one it achieved. A ninety-second median is the same
  number for a flat stage and for one that began fast and slowed, and that
  difference is what a first stage is. The results file is written as the run
  goes, so an interrupted campaign is still analysable.
- **Results live outside the tree** unless the run *is* the release sweep: a
  run writes `~/tmp/bench-<stamp>.json` and its artifacts beside it (the
  invoking user's home, even under `sudo`); `--out`/`--work` move them, and an
  existing `--out` is refused unless `--force`.

### The metrics

Generated from `model.METRICS`; `just bench list` prints the same registry, and
the definitions are what the report and the verdict render.
| metric | unit | direction | what it is | divided by |
|---|---|---|---|---|
| `throughput_gbps` | Gbit/s | higher is better | receiver-window payload bytes x 8 / measured window | the workload's measured window |
| `offered_gbps` | Gbit/s | context, not a verdict | visitor link egress bytes x 8 / measured window | the workload's measured window |
| `rtt_worst_1s_ms` | ms | lower is better | the worst one-second mean of the interactive stream's round trips: a burst that a percentile over the whole stage would average away | one second of the interactive stream |
| `rtt_error_rate_pct` | % | lower is better | failed interactive pings / (successful + failed) | one interactive ping |
| `churn_per_s` | 1/s | higher is better | short-lived connections established per second | the stage's measured window |
| `drift_rss_mib_per_min` | MiB/min | lower is better | least-squares slope of the arm's summed RSS over the run: a leak is a slope over time, not a level | one minute of run time |
| `drift_fds_per_min` | count/min | lower is better | least-squares slope of the arm's open descriptors | one minute of run time |
| `drift_threads_per_min` | count/min | lower is better | least-squares slope of the arm's thread count | one minute of run time |
| `wedge_count` | count | lower is better | stretches longer than 10 s in which the interactive probe asked and heard nothing: a tool that stops answering under degradation, which an average cannot show | one silent stretch |
| `wedge_max_s` | s | lower is better | the longest such stretch in the run | one silent stretch |
| `capacity_streams` | count | higher is better | the highest bulk-stream level whose interactive stream still met the SLO, from the capacity ramp | one bulk stream |
| `capacity_headroom_pct` | % | higher is better | 1 - max sustainable streams / the ramp's ceiling | the ramp's ceiling |
| `startup_s` | s | lower is better | median time from process start to the service answering a fresh connection: what a visitor pays after a restart | one cold start |
| `startup_max_s` | s | lower is better | the worst cold start the run saw | one cold start |
| `slo_broken` | bool | context, not a verdict | 1 when the cell broke the SLO (p99 above 50 ms or error rate above 0.5 %), so a level's verdict travels with its numbers | one measured level or stage |
| `rate_per_s` | 1/s | higher is better | completed request/response pairs / measured window | the workload's measured window |
| `rtt_p50_ms` | ms | lower is better | nearest-rank median of the probe's per-request round trips | one request/response pair |
| `rtt_p99_ms` | ms | lower is better | nearest-rank 99th percentile of the probe's round trips | one request/response pair |
| `rtt_max_ms` | ms | lower is better | worst single round trip the probe observed | one request/response pair |
| `rtt_samples` | count | context, not a verdict | round trips the percentile is taken over | none (sample count) |
| `udp_recv_mbit` | Mbit/s | higher is better | datagrams the probe received x payload / the probe's own measured window (which for a blast includes the drain) | the probe's receive window |
| `udp_loss_pct` | % | lower is better | (datagrams sent - datagrams received) / datagrams sent | datagrams the probe sent |
| `udp_gap_p99_ms` | ms | lower is better | 99th percentile of the gaps between replies the probe received | one received datagram |
| `transport_open_p99_us` | us | lower is better | 99th percentile of a fresh stream's open: an mhd stream on a tool arm, a TCP connect on the control arm. A reuse cell reports it absent, not zero | one stream open |
| `transport_first_byte_p99_us` | us | lower is better | 99th percentile of the wait from the request being written to the first response byte: the carrier's own round trip with no proxy above it | one request |
| `transport_recovery_p99_us` | us | lower is better | 99th percentile of `max(0, complete_us - first_us)`: the wait **after** the first response byte, where a repaired or stalled stream shows. Not the whole round trip | one request |
| `transport_stream_error_rate_pct` | % | lower is better | failed attempts / (successful + failed), as the harness counts them | one request |
| `udp_rtt_p99_ms` | ms | lower is better | 99th percentile of the probe's datagram echo round trips | one echoed datagram |
| `setup_p50_ms` | ms | lower is better | median time to establish one connection, when the workload opens a fresh one per request. It measures the visitor-to-server handshake only: the server accepts before it pairs the visitor with a channel, so the tunnel's arrival cost is in `rtt_*`, not here | one connection |
| `cpu_s_per_gbit` | s/Gbit | lower is better | tool CPU-seconds (user+sys, both daemons) / Gbit the visitor offered | Gbit on the visitor's link egress |
| `cpu_cores` | cores | lower is better | tool CPU-seconds / measured window | the workload's measured window |
| `bytes_per_syscall` | B | higher is better | device I/O per call: the tool process's rchar+wchar / syscr+syscw. **L3 arms only** — see the note under the table | one read/write-family syscall |
| `syscalls_per_s` | 1/s | lower is better | device I/O calls per second: the tool process's syscr+syscw over the measured window. **L3 arms only** | the workload's measured window |
| `wire_per_visitor_byte` | ratio | lower is better | tunnel-link bytes (both directions) / visitor-link egress bytes | bytes the visitor offered |
| `mean_carried_packet_b` | B | context, not a verdict | tunnel-link bytes / tunnel-link packets, both directions | one packet on the tunnel link |
| `rss_peak_mib` | MiB | lower is better | peak RSS summed over the tool's daemons, sampled during the workload | one process set |
| `fds_peak` | count | lower is better | peak open file descriptors summed over the tool's daemons | one process set |
| `threads_peak` | count | lower is better | peak thread count summed over the tool's daemons | one process set |
| `service_sockets_peak` | count | lower is better | peak sockets on the exposed service port in the server's own namespace: the per-visitor state the architecture keeps | one visitor |
| `retrans_segments` | count | lower is better | TCP segments retransmitted by the visitor and server namespaces | one TCP segment |
| `dropped_packets` | count | lower is better | packets the topology's devices and qdiscs dropped over the window | one packet |


Two conventions the table cannot state, because they belong to the workload:

- **A rate's window is the instrument's own, and it is named.** A bulk rate is
  the receiver's post-warm-up window (iperf3's); a round-trip rate is the
  probe's measured window, not the interpreter's start (the start cost is
  recorded in `meta.probe_startup_s`); a datagram rate is the probe's receive
  window, which for a blast includes the drain. Counter-derived ratios
  (`cpu_s_per_gbit`, `wire_per_visitor_byte`, `syscalls_per_s`) use the
  counters' window, which brackets exactly the workload's process — so a
  ratio's numerator and denominator cover the same seconds.
- **`wire_per_visitor_byte` has a floor of 1 for one-way traffic and 2 for a
  strict request/response workload**, because it counts both directions of the
  tunnel link and a round trip is carried there twice. Read it against the
  control's own reading, never against 1.

### The scenarios

| scenario | workload | the claim it supports | headline |
|---|---|---|---|
| `bulk-1` | `bulk` | what one bulk TCP stream carries, and what it costs per byte | `throughput_gbps` |
| `bulk-n` | `bulk` | whether N streams aggregate, or share one ceiling | `throughput_gbps` |
| `bulk-pair` | `bulk-pair` | whether two services (or two L3 claims) each carry a full stream, which separates a per-flow ceiling from a per-host one | `throughput_gbps` |
| `rr-1` | `rr` | the round-trip rate and latency of one strict request/response flow | `rtt_p99_ms` |
| `rr-16` | `rr` | the same, 16 flows at once: aggregate rate and tail latency | `rate_per_s` |
| `churn-16` | `rr` | a fresh connection per request: what a visitor pays to arrive, as the first request's round trip on each new connection | `rtt_p99_ms` |
| `udp-pace` | `udp` | a paced UDP session: what fraction arrives, and how it is spaced | `udp_loss_pct` |

| `udp-ladder` | `udp-ladder` | where a UDP path starts shedding, per offered rate | `udp_loss_pct` |
| `timeline` | `staged` | how the tool behaves as the path degrades and recovers: one interactive stream, N bulk streams, a churn stream and a UDP session, over a scripted schedule of conditions, in place | `rtt_p99_ms` |
| `soak` | `staged` | over a long run on a rotating path: does anything leak, drift or degrade | `rtt_p99_ms` |
| `cost` | `staged` | at one fixed operating point, how many CPU-seconds one carried Gbit/s costs | `cpu_s_per_gbit` |
| `capacity` | `capacity` | how much bulk load the tool carries while a fresh interactive connection still meets the SLO | `capacity_streams` |
| `reconnect` | `reconnect` | what a visitor pays after a restart: time from process start to answering | `startup_s` |
| `udp-blast` | `udp` | the datagram ceiling when the probe offers as fast as it can | `udp_recv_mbit` |
| `transport` | `transport` | what the carrier itself costs with nothing above it: one request/response over a raw TCP socket (the control arm) against one over an mhd stream, on the same shaped path | `transport_first_byte_p99_us` |

A cell's **floor is a property of the run, not of the cell.** Four runs of
`bulk-n` on the same host, same binary pair, same arm count, gave A/A twin
spreads of 5.4 %, 3.9 %, 14.9 % and 52 % — and the 52 % run is the one whose
numbers look like a product difference (its baseline read 3.1-3.2 Gbit/s against
its usual 11.5). Nothing about the cell changed between them: the same
twenty-four samples sit either side of the swing. So a bulk comparison quotes the
twin's own floor from *its own run*, which the tables do, and a run whose floor
exceeds the threshold is reported as unable to answer rather than as a small
difference — the model prints exactly that ("the run resolves 27 % against a 10 %
floor — about 28 rounds would reach it"). A hypothesis that lengthening the
transfer would lower the floor was measured and **refuted**: at four seconds of
measurement the twin spans 5.4 % and at eighteen seconds 14.9 %, so the window is
not what moves it, and the cell kept its window.

### The profiles

A profile is a time budget with a method attached: which scenarios run, how
many rounds each arm is measured for, and the sizes the scenarios take.
`dev` is the loop an optimization should use and `sweep` is what a release
publishes; `--scale` shortens or lengthens every staged timeline's holds
without changing the schedule's *shape*, and because the scale lands in the
fingerprint, a shortened run can never be mistaken for a published one.

| profile | rounds | budget per arm | what it is for |
|---|---|---|---|
| `smoke` | 2 (+1 warm-up) | ~45 s | the fast loop: every headline metric, seconds not minutes |
| `dev` | 4 (+1 warm-up) | ~150 s | the default for an optimization: every scenario, minutes |
| `full` | 6 (+1 warm-up) | ~420 s | the release-grade sweep: everything, for as long as it takes |
| `stage` | 2 (+1 warm-up) | ~150 s | one condition, the whole workload: a shape question, minutes |
| `soak` | 1 (+0 warm-up) | ~1200 s | the drift axis: a rotating path held for a long time |
| `screen` | 3 (+1 warm-up) | ~240 s | a development A/B: one condition, the two builds interleaved |
| `sweep` | 1 (+0 warm-up) | ~3000 s | the release sweep: the staged schedule and the load ramp |
| `nobulk` | 2 (+1 warm-up) | ~300 s | the same staged timeline with the bulk spine removed: a diagnostic, not a gate |
| `carrier` | 3 (+0 warm-up) | ~240 s | one condition held long, bulk spine off: is the L3 wedge the carrier alone? |

## How to read the charts
Six figures, each answering one question, each drawn only when the run carried
that data and each carrying its method in the footer:

- **the timeline master** — per arm, the interactive stream's per-stage p50
  (solid step line) and p99 (dashed) on a log axis, the SLO as a dashed line,
  the stage bands shaded (clean versus degraded), wedges marked, and the bulk
  throughput per stage beneath it. It answers "what happens as the path
  changes";
- **the small multiples** — one panel per stage, one lollipop per arm (dot =
  p50, bar = p99, tick = worst single round trip): the comparison the scatter
  hides;
- **the capacity curve** — throughput and p99 against offered load, with the
  SLO line and each arm's max sustainable level marked: sustainable load is
  where a curve crosses the line;
- **the UDP ladder** — received rate and loss against offered rate, with the
  lossless reference;
- **the drift panels** — RSS, open descriptors and threads over the run, with
  the fitted slope per minute printed: a leak is a slope, not a level;
- **the cost bars** — CPU-seconds per carried Gbit per arm, the number the
  CPU-cost metric is for.

A cell that carried no reading is drawn as `x` and printed as `- (reason)`,
never as `0`: "the path carried nothing" and "nobody could measure what it
carried" are different findings.

### How to read a cell

- **A stage's bulk cell is the load over its whole window, and the side that
  measured it is named.** The reading is the sender's bytes over the stage's
  measured window, span-weighted — not its best second, because netem releases a
  shaped burst into whichever interval it likes, so the peak is a property of the
  shaper's schedule. The **measurement**, not the class, decides which side
  speaks: when the sender's accounting is defeated — the client's `end` event
  says so, or at least half the stage's intervals read zero bytes, which is what
  a socket buffer absorbing a stage looks like — the reading is the
  **receiver's own window**, which the plot and README mark (`*`); when there is
  no receiver summary either, the cell carries **no** reading and says why,
  rather than reporting the defeated side. Which side a cell used travels with
  it (`bulk_gbps_source`), and the fallback remains for the cells that still
  need it (`jitter`).
- **A shaped stage's interactive cell is context, not a verdict.** The netem
  queue the harness installed dominates it, and a single run does not repeat it:
  three runs of one unchanged method on this host moved the shaped p99 cells by
  5-24 % (and the two-leg scope's by up to 48 %) against the 25 % limit the gate
  applies to a per-stage p99. So the gate *reports* a shaped stage's number and
  fails only a blow-up (3× or more), and the README marks those columns as
  context instead of picking a winner in them. **The queue's depth is stated
  per class and its drops are recorded**, so "the shaper dominated this cell" is
  a reading rather than an excuse: a stage whose qdisc dropped what it was
  offered, or sat at its limit, is measuring the shaper, and its interactive
  number says nothing about the tool.
- **A stage that carried no reading says so, with the reason.** `— (reason)` in
  the plot's table, and `bulk_gbps_source` in the results file, distinguish "the
  path carried nothing" from "nobody could measure what it carried". A bare
  `0` cannot: it is the number a defeated sender reports for a path that was
  full.
- **A stage that asked for bulk and carried nothing is a failure, not a fast
  stage.** Its latency is a measurement of an idle path, so a cell with
  `streams > 0` and exactly zero carried bytes is `ok=False` with that reason, and
  the gate refuses it as an unmeasured round rather than gating its (excellent)
  latency. Measured: five of six last-stage cells in one campaign carried nothing
  while reporting a pass, because a tool that stops forwarding scores its best
  number on its emptiest stage. The test is exactly zero and never a threshold — a
  **rate-shaped** stage legitimately reads small (`rate20` carrying 19 Mbit/s
  against its 20 Mbit/s shaper), so the smallest honest value in a staged run is a
  shaper's own rate, orders of magnitude above zero.
- **The per-stage numbers carry their sample count.** A stage's `rtt_p99` is a
  nearest-rank percentile of that stage's interactive samples, so with fewer
  than a hundred samples the "p99" *is* the worst observation — a stage that
  wedged for most of its window (the shaped stages of a saturated run often
  carry tens of samples) reports its worst single measurement, not a tail
  estimate. The charts mark it with the worst-second tick; the results file
  carries `rtt_n` per stage so a reader can tell which kind of number a cell is.

## The conditions and the stage schedule

### The load generator's own limits

The visitor is a client under load, and a client has limits that are not the
tool's. One of them changes a reading enough to be worth stating: the churn
cells open a **fresh connection per request**, about 2 900 a second, so the
visitor namespace accumulates roughly 174 000 sockets in `TIME_WAIT` against a
28 231-port default range. When it runs dry the probe's connect is refused, it
backs off 50 ms, and those waits land inside the measured window — a round that
posts 2 900 arrivals a second becomes one that posts 183, and the raw sample says
so: `connect_errors=3773` of 6 400 attempts. It happened to the A/A twin as
readily as to the arm under test, which is exactly what made it look like an
intermittent collapse of the tool.

The topology therefore sets two switches **in the visitor namespace only**:
`net.ipv4.tcp_tw_reuse=1` (a `TIME_WAIT` 4-tuple may be reused for a new
*outbound* connection; the kernel default of 2 is loopback-only and the visitor
dials over the veth) and `net.ipv4.ip_local_port_range=1024 65535`. Both are the
load generator's hygiene and neither changes what is measured. As a second line,
a round in which more than one connect in twenty was refused is reported as a
failed cell naming port exhaustion rather than as a throughput number, so a
starved round can never be read as a slow tool — the failure and the count live in
the raw per-sample artifacts.

### The timeline

The **staged scenarios** (`timeline`, `soak`, `cost`) walk a *timeline*: an
ordered list of stages, each a condition held for a number of seconds. Which
timeline a run used is recorded in its results file, together with the scale
applied to its holds.

- **`sweep`** is the release schedule: `clean` → `rtt100` → `loss1` → `loss5` →
  `rate100` → `rate20` → `jitter` → `clean`, and it ends where it started,
  because whether a tool is still the tool it was after the path recovers is
  part of the question;
- **`soak`** rotates a shorter one for a longer time: `clean` → `loss1` →
  `rtt100` → `loss5` → `clean`;
- **`single`** is one stage — the fixed operating point a `cost` or a `screen`
  run measures.

`--scale` multiplies every hold without changing the order or the set, so the
same shape answers a question in two minutes or in forty; the scale travels in
the fingerprint, because a run that used it measured a different method.

**The staged p99 is one series, not a sum.** A stage runs three probes at once —
an interactive one (a fresh TCP connection carrying one 64-byte echo every
50 ms), a churn one (sixteen fresh connections a second) and a datagram one — and
each feeds a different metric: `rtt_p99_ms` comes from the interactive series
alone, `churn_per_s` from the churn series, `udp_rtt_p99_ms` and `udp_loss_pct`
from the datagram one. They are three measurements of one network at one instant
and are never pooled into one percentile; a stage's results record carries both
how many samples each series produced and which series ran.

The probe set is a method parameter. `--series interactive` starts that probe
alone, `--series interactive,churn` starts two. A subset is a **diagnostic** —
"does a second probe on this host move the first one's tail?" — and never a
release configuration, because a run without the interactive series has no staged
`rtt_p99_ms` at all and the gate refuses a clean stage it cannot judge.

A **single-condition** run (`--condition NAME`, on every other scenario) is the
same vocabulary without the schedule: one cell, one condition, one number.

| Stage | What it emulates | Applied to the path | `sweep` hold |
|---|---|---|---|
| `clean` | a healthy network (the control) | nothing | 150 s |
| `rtt100` | a long-haul or satellite link | 100 ms delay, 10 000-packet queue | 120 s |
| `loss1` | a lossy wifi or mobile link | 10 ms delay, 1 % loss, 1 000-packet queue (this class means to tail-drop) | 120 s |
| `loss5` | a badly congested path | 100 ms delay, 5 % loss, 10 000-packet queue | 120 s |
| `rate100` | a 100 Mbit uplink | 100 Mbit/s, 20 ms delay, 2000-packet queue | 120 s |
| `rate1000` | a gigabit metro link, short enough that a window ceiling cannot hide behind a long round trip | 1 Gbit/s, 2 ms delay, 4000-packet queue | focused cell only |
| `rate100_shallow` | a 100 Mbit uplink with an ~11 ms queue: shallow enough that a sender without a loss response overruns it, so the drops are its own rate | 100 Mbit/s, 20 ms delay, 100-packet queue | focused cell only |
| `rate20` | a 20 Mbit/s uplink | 20 Mbit/s, 40 ms delay, 2000-packet queue | 120 s |
| `loss1_rate100` | a 100 Mbit uplink that also loses packets — the lossy WAN the carriers are chosen for | 100 Mbit/s, 20 ms delay, 1 % loss | focused cell only |
| `jitter` | a bufferbloated access link | 20 ms delay ± 10 ms, 10 000-packet queue | 120 s |
| `loss1_buf` | a lossy path with a large but finite buffer: deep enough that tail drops are not the story, shallow enough not to inflate RTT (10 000 packets ≈ 14.7 MB, ≈ 27 ms of standing queue at 545 Mbit/s) | 10 ms delay, 1 % loss, 10 000-packet queue | focused cell only |
| `loss1_deep` | the same 1 % loss with a queue deep enough not to tail-drop — it tells a carrier's loss recovery apart from the shaper's own drops | 10 ms delay, 1 % loss, 100 000-packet queue | focused cell only |
| `udp_throttled` | a path that **polices** UDP instead of refusing it: the carrier riding UDP still establishes, but only carries what the policer admits (10 000 packets/s ≈ 117 Mbit/s of 1472-byte datagrams) while a carrier that does not ride UDP is untouched | `iptables -m limit` accept-then-drop for UDP on the leg (the `tunnel` leg only — the refusal says so) | focused cell only |
| `udp_blocked` | a path that drops UDP: the data plane's first carrier cannot be established, so the run measures the **TCP fallback** | `iptables -j DROP` for UDP on the leg (the `tunnel` leg only — the refusal says so) | focused cell only |
| `clean` | recovery — is the tool still the tool it was? | nothing | 150 s |

The schedule, the durations, the sample rates and the SLO are recorded in every
results file (`meta`), so a chart can always be traced back to the method that
produced it. So is the shaping applied to each class — including each class's
queue depth, which bounds how much traffic the shaper may hold and therefore
what a burst through it can do.

**A shaped class's queue is stated, and it is not netem's default.** That
default is 1 000 packets, and it is not neutral: a qdisc holds
`offered rate x delay` packets in flight, so at `rtt100`'s 100 ms a stream of
37 600 packets/s needs a 3 760-packet queue, and a 1 000-packet one **drops
73 % of it**. That is what happened to the transparent-L3 row — the leg shed
28 180 packets/s against 9 400 sent while every device counter in the evidence
read zero, because `/proc/net/dev` cannot see a qdisc's drops — and the loss was
read as a property of the tool for a whole cycle. The classes that shape a delay
now carry 10 000 packets (100 000 packets/s at 100 ms), the same value
`loss1_buf` established for "tail drops out of the way without a standing
queue"; a class that *means* to tail-drop states its own smaller limit, and
`loss1` does. The reading is checkable rather than promised: each stage's qdisc
drops and standing backlog travel in its evidence beside the devices'
counters.

### The MTU classes

Two further classes carry the **fragmentation** axis and are deliberately not in
any default timeline:

| Class | What it emulates | Applied to the path |
|---|---|---|
| `mtu1280` | a tunnel or an IPv6-minimum path | interface MTU 1280 |
| `loss1_mtu1280` | a lossy link that also fragments | 10 ms delay, 1 % loss, interface MTU 1280 |

MTU is an *interface* property, not a qdisc: a stage that uses one of these
classes changes the path for **every packet on `lo`** during that stage — the
peers', the harness's and the tool's control plane included — which is why they
stay out of the default timelines and are run as focused cells
(`--condition loss1_mtu1280`, or `--profile screen` on it to A/B two builds). `meta.mtu_restore_to` records the interface's MTU at the start of
the run and the harness restores it on teardown, failing loudly if it cannot —
a leftover 1280 would poison every later run on the host. The reason the axis
exists at all: `lo` is MTU 65536, so without it every datagram fits in one
fragment and the cost of a lost fragment is unmeasurable. Read the two classes
together with the `carrier` rows below.

### A stage owns its condition

Each stage of a timeline **imposes its own condition and puts the leg back to clean
when the stage ends, whether it succeeded or failed.** That is not tidiness: a stage
that dies mid-way (a tool that stops answering, a sink that never starts) used to
leave its condition installed, and every stage and every *arm* after it then ran on a
path nobody had asked for. Measured, and it cost a whole five-arm sweep — one arm lost
`loss5`, four of its stages followed, the next arm lost two of its own plus all eight
capacity levels, and the two arms after it carried nothing at all, so the gate reported
69 failures in which the *reference tools* were the ones with zero streams.

So if a cell is missing or unreadable, read it against the stage before it before
concluding anything about the tool: a run in which the peers also fail is a statement
about the run. This is [AGENTS](../AGENTS.md) §10's rule about one failure not
poisoning the next sample, applied to a condition instead of a process.

### The shaping scope

**A condition is applied to one leg of the topology**: `--condition-leg visitor`
(the default) shapes the link between the visitor and the server, and
`--condition-leg tunnel` shapes the link the tool's carrier crosses. Both ends
of the chosen leg are shaped, because netem is egress-only — shaping one end
would shape one direction. An injected delay is therefore paid once, on the
visitor's side, where a real deployment's WAN is; the backend leg (the client
process to the backend it forwards to) stays unshaped, because the backend is
next to the tool. The leg a run used travels in its results meta.

Only the data plane is shaped. The tool's control channel stays on the unshaped
path: shaping it kills the heartbeat and turns a capacity measurement into a
wedge study.

**A rate class is read from the receiver.** An unbounded sender defeats its own
accounting against a rate shaper — its writes complete into a socket buffer far
larger than the shaped path can drain, so the sender's own intervals read zero
bytes while the path keeps carrying them. The model does not bound the client's
socket window; it quotes the **receiver's** window for every rate (the table's
first convention), and the interface counters carry what the path actually
moved beside it. That is why a `rate20` cell is a reading rather than a
property of the client's buffer.

### Stage transitions

A stage does not start until the previous one has gone quiet. The boundary
kills the stage's bulk client, and a killed TCP socket keeps delivering what its
kernel still holds — and keeps retransmitting its FIN through whatever qdisc is
installed. Reshaping at that instant would put the old stage's drain into the
same queue as the new stage's handshake, and because a tool's data-plane ports
share one netem class, a SYN dropped behind that drain costs the next stage its
first seconds. So the model waits, at the *old* condition, until the path is
quiet by two measurements — no more than a frame's worth queued
(`topology.DRAIN_TOLERANCE_B`, 64 KiB) **and** no socket in a state that can
still send (`ESTAB`, `FIN-WAIT-1`, `CLOSE-WAIT`, `SYN-SENT`, `SYN-RECV`) — both
holding for two consecutive polls (`topology.DRAIN_QUIET_POLLS`), and only then
imposes the next stage's condition.

Both halves are tolerances. The queue never reaches zero — the interactive,
churn and UDP probes share the tool's class and leave ~1.2 KB in it permanently
— so an exact-empty-queue rule could only ever end by burning its budget, which
would make the transition timer-driven, its start state depending on the clock
rather than on the path. And "no socket" cannot mean "no non-LISTEN socket"
either: `FIN-WAIT-2`/`CLOSING` linger for *minutes* carrying nothing, while the
states above are exactly the ones that retransmit the tens of MB a killed bulk
client's kernel still holds. The queue half is a *reading*, not an assumption:
`tc` renders the backlog with a unit suffix (`b`, `Kb`, `Mb`, `Gb`), so the
drain parses the suffix into bytes before it decides anything.

The wait's budget (60 s) is a **safety net, not the mechanism**: the wait ends
on the predicate, and a budget that fires is recorded rather than absorbed. What
the wait cost and what it left behind travel with the stage it precedes
(`drain_s`, `drain_expired`, `drain_final_backlog`, `drain_busy_sockets`), so a
stage that began in a busy state says so.

Each stage then dials its own bulk spine, with the stage's hold as its window
and its warm-up scaled to it (`-O` of a quarter of the hold, at most two
seconds). A stage whose `iperf3` client fails is a typed failure for that cell
and the run continues: one bad stage cannot poison the next, and the cell
carries the instrument's own reason.

**A failure of the instrument is retried once, and only that kind.** `iperf3`'s
client exits with `control socket has closed unexpectedly` when its own control
connection drops, and its sink then reports the streams established with zero
bytes for the whole stage — indistinguishable, in the summary, from a carrier
that forwards a connection and drops its payload. Measured on the release sweep,
the sender's window grew to 13.5 MB with the flight at zero through the stages
that lost their cells, so the carrier was idle rather than broken.

A **stage** that failed that way is measured a second time in place, after the leg
is drained again; a **scenario** whose every cell failed that way is measured a
second time after a restart, which is what covers the single-cell scenarios where
a scenario is one cell. Either way the sample carries `retried_after` with the
first attempt's reason, so the gate still sees that the cell was lost once, and a
second failure stands as a failure. Every other reason is reported as it happens,
with no second attempt.

## Running it

```bash
# the fast loop, and the loop an optimization should use
sudo -n just bench
sudo -n just bench --profile dev --aa

# the release sweep: the staged schedule and the load ramp, five arms
just bench peers fetch
sudo -n just bench --profile sweep --out benches/records/results-bench-vX.Y.Z.json

# one condition, the whole workload (a shape question in minutes)
sudo -n just bench --profile stage --scenarios cost --arms molehill,frp

# an A/B: the same arm twice, two builds, interleaved, verdicts against the
# run's own noise floor
sudo -n just bench --profile screen --ab-arm molehill --binary-b /path/to/build

# an experiment with a knob the catalog does not carry: declare it
sudo -n just bench --arm id=l3-deep,txqueuelen=20000
```

`just bench doctor` states what this host can and cannot measure before a run
has to find out (root, `/dev/net/tun`, iperf3, the peer binaries). The peer
binaries come from `just bench peers fetch`: the latest GitHub release assets,
never built from source, with the resolved versions recorded in the results
meta.

## Reading a result

- **A profile is a resolution, and the smoke profile's is wide.** Repeated runs
  of one unchanged binary at `smoke` (`--aa`) have claimed differences of
  13-20 % on the round-trip cells and 23-29 % on the shortest ones: at
  seconds-long arms the model's own scatter is that large, and it says so
  rather than hiding it. Treat a smoke claim as a hypothesis, and re-measure it
  with `dev` — which is what `dev` exists for.
- **The cell table is a median with its range** over the measured rounds, per
  arm. Warm-up rounds are excluded; failed rounds are counted and listed with
  the instrument's typed reason, never averaged in.
- **The floors table is the run's resolution** per metric: the A/A difference,
  the A/A scatter and the control's drift over the run, and the largest of the
  three, which is what a claim must clear. The cell that produced the worst
  reading is named, because that is where to spend more time.
- **The verdicts are per scenario and cell**, each tool arm against the
  control, plus every twin's own verdicts — a metric on which the A/A pair
  claims a difference is a metric that run cannot resolve, and it says so.
- **A results file is `meta`, `samples` and `summary`.** `meta` carries the
  method (the fingerprint, the resolved scenario parameters, the topology, the
  conditions, the instrument cadences, the SLO), the provenance (revision, the
  binary's sha256 and its own `--version` line, host identity, both calibration
  probes, and the load average the run started under) and the arm list; `samples` is one record per arm, round, scenario
  and cell, each with its metrics, its typed absences and its evidence;
  `summary` is what the analysis derived from those samples — and
  `just bench report` recomputes it, because the samples are the evidence and
  the summary is a rendering of it. The load average is the host's, not the
  container's: on a shared bench host it is the one field that says whether the
  machine was in a state to be measured at all, and a run taken under a crowd is
  refused rather than compared.
- **`just bench plot` renders exactly the figures this page describes** from
  any results file: the timeline master, the small multiples, the capacity
  curve, the UDP ladder, the drift panels and the cost bars.
- **A CPU number can be split into the carrier's own phases.** When the campaign
  ran with `MOLEHILL_MHD_STATS=1`, `just bench phases <work directory or results
  file>` reads the daemons' cumulative logs and prints seconds per carried Gbit
  and nanoseconds per event for every phase the engine times: the record layer's
  seal and open, the sender's run build and its syscall, the receiver's
  per-datagram path, and the copies the two stream handles pay. Sampled phases
  are scaled back to the round's real event count, because the meters time one
  datagram in sixteen. It is an attribution, not a measurement — the claim is
  still `cpu_s_per_gbit`, and this only says where that number went.

## The gate

`just bench gate` is what a run must satisfy before its numbers may be
published. It reports what it saw for every question and fails the ones a human
must act on:

- **coverage** — every cell the scenarios declared, for every arm;
- **the endpoint invariant** — no L4 or peer arm dialed the backend it forwards
  to (a transparent arm is the documented exception: the visitor dials the
  address the client owns, and the client's kernel delivers it);
- **the SLO on the clean stages** — for the product's own arms; a reference
  peer that misses it is reported with its number and never blocks a tag. A
  clean stage that was measured but carries no round trip **fails** here: it is
  counted, so it cannot be one of the cells a green gate silently skipped;
- **the drift and wedge axes** — a leak is a slope over time and a wedge is an
  interactive silence; a slope is only judged when the run is long enough to
  carry one (fifteen minutes), and a shorter run's slopes are reported as
  context. **A silence is a run of unanswered attempts, measured from the first
  attempt's start to the last one's end — not a gap between samples.** The
  interactive probe is serial, so a gap is the *next attempt's own duration*,
  and reading gaps made a single slow-but-successful answer look like a wedge:
  two 5.1–5.5 s answers in a 5 %-loss stage were reported as L4 "going silent",
  and every tool pays those, because a fresh TCP connection's SYN ladder on a
  lossy path is 1 s and then 3 s. The threshold is **two attempt budgets**
  (2 × 5 s) for the same reason: at one budget, a single unanswered attempt is
  a silence by construction, and the axis becomes a coin flip;
- **the capacity ramp** — the level *where* a tool breaks the SLO is the
  measurement, so a ramp level is not judged by the stage SLO above. The
  ceiling is judged instead: a subject arm that stopped **short of the declared
  `streams_max`** fails, because a ramp that broke at three streams is a
  capacity regression and not a host reading. A reference peer's ceiling is
  reported and never blocks;
- and, with `--baseline`, the **regression half**: the same comparison
  `just bench compare` makes, refused unless the two files' method records
  match.

## What the model cannot answer

Stated so a reader does not ask a chart for something it never measured:

- **A churn comparison, until its instrument is fixed.** `churn-16` opens a
  fresh connection per request and the reading is not yet trustworthy: in three
  runs on 2026-10-10 the L4 arms produced every degenerate cell (four rounds
  reporting no samples at all, three rounds where the readiness probe could not
  reach the exposed port for its full 30 s), and never the L3 arms. Whether that
  is the architecture (per-connection state, or the ephemeral ports one visitor
  connection costs L4 and not L3) or the harness (a probe that reports zero
  without saying why) is exactly what has to be established before the scenario
  can order two architectures — the probe must record *why* a connection failed
  before its rate is quoted. The pattern is recorded in HANDOFF.md as an open
  thread, not as a result.

  *(Update 2026-10-12: the cause is named — see the history note below. The
  probe now counts failed connects (`connect_errors` in every churn cell's
  evidence), so new runs distinguish "slow" from "port-starved"; every churn
  number measured before that fix stays unquoted, and the degenerate cells
  above are instrument artefacts, not architecture findings.)*

- **How much the encryption costs, before 2026-11-08.** Every L4 arm writes
  `data_enable = true`, which **is** the client default for a forwarding service:
  unset resolves to `true` (`src/core/client.rs`), and each data channel then pays
  a Noise handshake. The other data plane — one Noise handshake per control
  session and none per data stream — is what a **transparent claim** runs by
  default (`[transparent.noise].data_enable` defaults to `false`), so the L4
  tables here describe the plane an L4 deployment actually runs and say nothing
  about a claim's. `l4-plain` is one knob apart from `l4`, so the pair prices the
  encryption and the opening shape that comes with it; no comparison against it
  has been published yet.

- **A receiver that is slow at *many small datagrams*.** Every bulk cell sends
  coalesced runs, so one `recvmsg` returns a whole run and the cost of taking one
  datagram per call never appears. A path that writes 1 349-byte datagrams one at
  a time has no coalescing, and on 2026-11-08 that shape lost 97 % of one
  direction while `bulk-n` read 15.0 Gbit/s before *and* after the fix
  (throughput −0.4 % against a 4.9 % A/A scatter) — see
  [HANDOFF.md](../HANDOFF.md) §4.117. So a bulk cell cannot price this class of
  defect, and no cell that writes like that relay exists yet.

- **A reorder-window (RACK) change.** RACK needs a *later* packet's
  acknowledgement to anchor its window on, and no cell keeps two or three packets
  in flight per stream: the many-stream cells go deeper than the three-packet
  threshold, which declares the loss first, and the one-stream cell loses the last
  packet of its flight, which has nothing after it to anchor on. Both were measured
  null on 2026-11-08 (see [HANDOFF.md](../HANDOFF.md) §4.125) and the change was
  reverted, so a reorder-window claim needs a cell the model does not have yet.

- **A carrier that beats a TCP-based tool on this host.** On a clean loopback
  cell the mhd carrier's round trip is about **45 µs against 20 µs for a plain TCP
  socket**, and a proxied request about **113 µs against rathole's 61** — because
  TCP's loopback path is kernel to kernel while a userspace datagram carrier needs
  a task hop the kernel does not, measured at 9.4 µs per direction. Removing it
  would take one socket per connection and was declined as too large a change to
  the configuration surface, so no cell on this page shows mhd ahead of frp or
  rathole. What the carrier buys — per-stream recovery over an unreliable
  datagram, no head-of-line blocking across the pool's streams, optional datagram
  semantics — is **not measured by any scenario here**: the loss cells tie because
  the shaper dominates, and the reorder-window cell does not exist. See
  [HANDOFF.md](../HANDOFF.md) §4.134 and §4.145.

- **Whether a loss is random or caused by the carrier's own rate.** On a carrier path that drops 5 % of
  datagrams at random, mhd carries about a quarter of what the same cell carries clean, and the counters say
  why: its window reaches 1.47 MB against a 16 MiB ceiling, which over a 200 ms round trip is 236 Mbit/s — the
  measured 290 — while retransmissions stay at 4.3 %, the loss rate. The window grows one packet per window
  after the first loss, so on a long path it recovers slowly. **No cell here makes loss depend on the offered
  rate**, so nothing can say whether a window that responded to loss would help or hurt: `loss1_rate100` shapes
  a rate and adds loss, but its queue is deep enough that the drops are the shaper's, not the sender's.

- **A number from another host.** The host identity and the two calibration
  probes travel in every file; two runs on different machines are refused, and
  two runs on one machine whose probes drifted beyond tolerance are refused
  too.
- **Two arms at the loopback ceiling.** On one host the top of the range is the
  path itself, not the tool, so the arms that reach it cannot be ordered by a
  run: across three sweeps of identical code the two fastest read 15.9-22.2 and
  12.8-20.4 Gbit/s — swings wide enough to *reverse* their order — while a
  reference tool an order of magnitude below moved by under 3 %. Read those
  rows as one reading of the host's state, and never carry their order as a
  standing claim.
- **One sample per condition.** A staged cell is one walk of that schedule, so
  a single run cannot state its own repeatability for a condition it visits
  once. The schedule visits `clean` twice (the run's own replicate) and the
  A/A pair measures the rest; a difference smaller than the floors is not
  resolvable by that run.
- **The control plane under degradation.** Only the data leg is shaped: the
  tool's own control channel stays on the unshaped path, because shaping it
  turns a capacity measurement into a wedge study. Anything the control channel
  does *under* loss is outside this model's reach.
- **A tool's internals.** Everything measured is externally observable, so a
  peer binary and molehill are measured identically — and no internal counter
  of either appears in a comparison. The opt-in `MOLEHILL_*` switches add
  molehill-only diagnostics to a run; a run that inherits one records it in its
  results meta.
- **Anything no tool-free path can run.** A scenario that cannot run on the
  control arm is `diagnostic` by declaration: it produces numbers and never a
  verdict.



Stated so a reader does not ask a chart for something it never measured:

## What each configuration choice costs (per-decision measurements)

The pool's own thresholds — refuse placement at 56 streams of the engine's 64,
repair a dead tunnel, reap a forward that moves nothing for 5 minutes — are
internal constants, not settings, and they change only with a measurement
behind them. The pool's width is not one of them: it is `tunnels`, and the
measurements behind its guidance are the `count` rows below (re-measured with
the pinned pool: one tunnel 12.85 Gbit/s against eight tunnels' 22.81 on a
clean jumbo path, 8 flows; 3.34 against 5.92 Gbit/s on `loss1` from one tunnel
to two).

| Decision | Option | Measured basis (retired per-cell model) |
|---|---|---|
| `mode` | `"multiplex"` (default) | 1-stream 10.0 Gbit/s on loopback vs 19.2 for `direct`; at 8 streams 19.5 vs 23.3; multiplex absorbs per-connection setup (churn ~4.8k connects/s) and saves FDs / ports / NAT mappings |
| `mode` | `"direct"` | raw single-stream throughput; one physical tunnel per stream (FD / port / NAT cost scales with stream count) |
| `tunnels` | `1` | one tunnel for everything: no aggregation and one retransmit domain shared by every stream (loopback 8-stream aggregate 9.2 vs 19.5 Gbit/s at four tunnels; loss5 head-of-line max 2.5 s vs 1.6 s; and on the pinned pool's own cells 8 flows read 3.34 Gbit/s on `loss1` against 5.92 at two) |
| `tunnels` | `4` (default) | aggregates beyond one flow (loss1 8-str 12.3 vs 4.5 Gbit/s) and isolates head-of-line blocking (rtt10 max gap 80.6 vs 100.1 ms at one tunnel); yamux ceiling `count × 64` concurrent connections, `count × 56` after placement's own ceiling |
| `tunnels` | `8+` | ~512 concurrent connections (8 tunnels × 64 yamux streams); 8 physical tunnels per service (NAT mappings ×8), and on a clean jumbo path 22.81 Gbit/s against 12.85 at one |
| `carrier` (forced) | `mhd` (default) | the carrier the product chooses for itself on a path that carries UDP, and the one the `cost` metric is quoted for: `cpu_s_per_gbit` 0.65 against TCP's 0.48 on the same cell. `MOLEHILL_DATA_CARRIER=mhd` pins it, which also makes the pool's carrier rule inert |
| `carrier` (forced) | `tcp` | the fallback when UDP is blocked or throttled: `MOLEHILL_DATA_CARRIER=tcp` pins it for an arm that needs the fallback's own numbers rather than the path that produces it |
| transport | `"plain"` | 10.0 / 19.5 Gbit/s (1 / 8 streams) on loopback |
| transport | `"noise"` | 5.8 / 14.9 Gbit/s; sub-millisecond RTT cost; CPU parity under full load |
| `pool_size` | 8 TCP / 2 UDP (defaults) | setup-to-first-byte p99 ~3.5 ms at 16-way churn; UDP shards distinct visitors across channels and never splits one session (session affinity) |
| striping | a group of four channels (the measured arm set `stripe_count = 4`; the key is gone — a group starts at one, and its width is not decided by another visitor's history, see [internals.md](internals.md#data-channel-striping)) | a single long-lived connection stops being bounded by one tunnel flow: 1-stream throughput +48.7 % on loopback, at the cost of a reorder buffer, +8.7 % RSS and +40.8 % CPU (per-frame CPU is halved, because the frames spread over four driver tasks) |

### The L3 data path: what limits it (2026-10-10, this model)

The transparent path carries **packets**, where the forwarding path carries
bytes: a claim hands every IP packet to the device, so the cost is per packet
rather than per byte, and the model measures what that costs. All four rows are
one host, `--profile smoke --condition clean`, two measured rounds, the A/A twin
in the run. `bulk-1` is one TCP flow through the tunnel, `bulk-n` is eight.

| Arm | `bulk-1`, 1400-byte TUN MTU | `bulk-n` (8 flows), 1400 | `bulk-1`, 8000-byte TUN MTU | `bulk-n` (8 flows), 8000 |
|---|---|---|---|---|
| control (no tool) | 40.640 Gbit/s | 49.159 | 39.951 | 50.853 |
| `l3` | 4.335 Gbit/s | 3.934 | **8.000** | 5.772 |
| `l3~aa` (same arm twice) | 4.214 | 3.847 | 7.214 | 5.961 |
| `l4` (forwarding, same topology) | 8.991 | 22.333 | 7.996 | 22.629 |

Two things are worth reading off it:

* **The L3 path does not scale with flows.** Eight flows move *less* than one
  (3.9 against 4.3 Gbit/s), while the forwarding path nearly triples
  (9.0 → 22.3). The ceiling is a serialized per-packet path — one claim is one
  device reader and one injector — not a per-flow window, so opening more
  connections through a claim cannot raise it.
* **Packet size is the lever that does move it.** Raising the TUN MTU from 1400
  to 8000 (with the link MTU to match) took the same bulk flow from 4.335 to
  **8.000 Gbit/s** — 1.84× — and CPU per Gbit from 0.852 to 0.313 s, a third of
  what it was. At that packet size a single L3 flow matches the forwarding path
  on the same host (8.00 against 8.00) and its CPU per byte is within 35 % of
  it (0.313 against 0.231).

The same effect shows up in the wire accounting: at 1400 bytes, 7.5 % of the
bytes on the visitor's leg were tunnel overhead (`wire_per_visitor_byte` 1.0755);
at 8000 it is 1.5 % (1.0152), because the carrier's own header and
acknowledgements are paid once per 8000-byte packet instead of once per 1400.

**Postscript (2026-10-16, the offload fix; re-swept 2026-10-17 and
2026-10-18): the two readings above are dated.** This section's binary segmented every packet at the
TUN boundary; the shipped one declares its segmentation offloads and receives
GSO super-frames — see the retraction-turned-correction below. The paired
evidence for the fix is a single-flow smoke A/B: 2.0 → 3.4 Gbit/s. The staged
L3 clean cell cannot carry an absolute comparison against this section's 4.335:
it has moved by more than a third between this release's sweeps on this host
(the published one reads 5.859 Gbit/s) while one run's own two clean readings
agree to under 1 % — the cell is the row's least stable, and the pair inside a
run is what it supports. "Does not scale with flows" and "packet size is the
lever" both survive — but the packet size inside the device is no longer the
operator's alone to set, and the serialized per-packet reader is no longer the
whole of the ceiling.

**What the ceiling then is.** The same topology, with the TUN MTU at 8000 and
the link MTU at 9000, against the forwarding path with its pool capped — the
`l4-mux1` arm is one tunnel, so it prices a single carrier connection:

| Arm | `bulk-1` (1 flow) | `bulk-n` (8 flows) |
|---|---|---|
| control | 38.672 Gbit/s | 44.805 |
| `l3` | 7.136 | 5.847 |
| `l4` (pool capped at 4 tunnels) | 8.286 | 21.604 |
| `l4-mux1` (one tunnel) | 9.819 | 12.646 |
| `l4-mux8` | 9.058 | 21.955 |

One carrier connection tops out near 8–10 Gbit/s on this host — the one-tunnel
arm says so directly, at one flow and at eight streams over the same tunnel —
and the forwarding path scales past it only by spreading streams over *several*
tunnels. A claim carried by **one** channel therefore sits at that per-connection
ceiling by construction, which is why its `bulk-n` is *below* its `bulk-1`:
eight flows through one channel is the same connection.

**The measurement that introduced a claim's member set** is what finally moved
the ceiling. The model called the knobs `members` / `[transparent.data].default_members`
at the time (default 1, so every arm above keeps its meaning); the current model
calls the connection budget `[transparent.data].tunnels` and the members lanes.
With four members, inner flows hashed across them by a canonical 5-tuple, jumbo
path, four measured rounds an arm and the A/A twin in the run:

| `bulk-n` (8 flows) | `l3` (direct, 1) | `l3-mux` (mux, 1) | `l3-mux4` (4 members) | twin (same config) | `l4` (forwarding pool) |
|---|---|---|---|---|---|
| throughput | 5.446 Gbit/s | 6.989 | **13.924** [11.528..15.091] | 13.598 | 29.166 |
| CPU s/Gbit | 0.270 | 0.560 | 0.653 | 0.655 | 0.412 |
| FDs | 39 | 69 | 69 | 69 | 99 |
| RSS | 18.35 MiB | 21.45 | 25.45 | 24.35 | 25.45 |

The claim **nearly doubles** — 13.924 against 6.989, with the twin at 13.598, on
a run whose own A/A floors were 3.34 % (throughput) and 0.17 % on the twin pair,
so this clears the noise by an order of magnitude. The rest of the cells hold:
`bulk-1` indistinguishable (6.201 / 6.244 / 6.299), `churn-16` **+22 %**
(6837/s against 5586/s), `rr-16` −1.8 % (inside the twin's 0.1 % pair spread),
`fds_peak` identical at 69 (members in `multiplex` mode are streams on tunnels
that already exist), `wire_per_visitor_byte` unchanged, and the one-member
claim's carrier retransmits fall from 518 to **0**.

Two findings ride with it. The fan-out is visible per member — every measured
round spread its flows over 3 or 4 of 4 slots, and **both ends agreed on the
split to within a point** (23/52/24/0 server-side against 23/53/24/0 client-side),
which is the canonical key verified in production rather than in a unit test.
And the first run of this slice read 11.282/13.142 instead of the table above:
FNV-1a's low bits are weak, the byte that varies between eight `iperf3` streams
is its ephemeral port, and every one of those ports was even — so modulo read a
collapsed distribution and put eight flows on two members. That is why the hash
ends with a splitmix64 finalizer, and why the regression test pins the measured
port set and every stride.

The operator's copy of the packet-size finding, with its recipe, is
[deployment.md](deployment.md#transparent-services).

**And the lever on an ordinary path — no jumbo frames and no wire change — is
the lane budget.** The ceiling is a serialized per-packet path (one device
reader and one injector per claim), so a claim's throughput follows its *lanes*
and the budget decides how many it may hold. One host, one binary, five claims,
eight inner flows: a claim at one lane carries 2.351 Gbit/s, at two 3.283, at
four 5.191. (Pre-v0.11.0 binary — the offload it takes raises the absolute
values; the paired smoke A/B in the postscript above measures it, and the lane
trade itself is unchanged.) It is not free — the lanes that spread a busy claim's flows are also
pressure on the one reader every claim shares, so four lanes spends the
interactive stream's SLO headroom (three streams sustained on the capacity ramp,
against two lanes' eight) — which is why the shipping default is two and four is
a configuration to ask for. What a lane *costs*, and the upgrade path for a
deployment that wants the old default, are
[configuration.md](configuration.md#transparent-l3-services)'s to state; this
page owns the measurement.

One earlier reading at the end of this section is **retracted as wrong** (the
2026-10-16 offload fix is its correction): the "device path that can carry more
than one packet per syscall" — GSO/TSO on the TUN — was ruled out on the
reasoning that a 1500-byte path cannot have it (the TUN may not hand userspace
packets larger than the path) and that enabling TSO would carry checksum and
GSO state per packet, which was called a wire change. Both halves fail: the
kernel hands the reader **GSO super-frames** — one up-to-64 KiB unit naming a
`gso_size`, whose packets the far end re-segments — so the path's MTU never
bounds the read, and the state already had a wire shape (the 10-byte
virtio-net header, relayed whole, `NEEDS_CSUM` intact). What the path lacked
was the declaration: `IFF_VNET_HDR` plus `TUNSETOFFLOAD`. Measured: 3.85 GB of
bulk TCP arrived as 576 071 reads of 1 500 bytes without it and 59 160 reads
averaging 65 212 bytes with it. That fix is in the shipping binary; the probe
is [`bench/src/bench/utils/l3/tcp_tso_census.py`](../bench/src/bench/utils/l3/tcp_tso_census.py),
and "What is left is per-packet syscall cost" below now reads differently.

### The carrier axis is a path condition now

The product chooses its carrier at runtime — mhd, falling back to TCP — so there
is no build to compare and no key to set. The contrast lives in the *condition*:
a clean path carries mhd, `--condition udp_blocked` carries the TCP fallback, and
the same binary measured once per path is the comparison. Two switches isolate a
single carrier; both are diagnostics, both are recorded as instrumentation, and
neither is a deployment setting:

- `MOLEHILL_DATA_CARRIER=mhd` / `=tcp` pins one carrier for the whole run. It
  also makes the pool's own carrier rule inert — a forced carrier is the run's
  answer, not a preference to be argued with, and the pool refuses to move a
  tunnel under it.
- `MOLEHILL_CARRIER_SWITCH_AT=<seconds>` moves every claim to the other carrier
  after that many seconds, so the cost of a live move can be measured before any
  policy that decides one exists.

The figures that shaped a TCP-versus-KCP axis are published history in
`benches/records/`: they measure a carrier this release deletes, their arms can no
longer be built, and they are not a baseline. What compares today is the table
each scenario prints, tool against the control arm of the same run.

### What a fresh connection costs, per carrier (2026-11-02)

The one axis where the two carriers are not close. Measured with
`MOLEHILL_DATA_CARRIER` pinning one per run, the same binary both times, over the
`dev` profile's `churn-16` (a fresh connection per request, sixteen at once, six
rounds, each run's own A/A twin) and its `rr-16` (sixteen connections held open
across many requests):

| | `churn-16` `mhd` | `churn-16` TCP | `rr-16` `mhd` | `rr-16` TCP |
|---|---|---|---|---|
| round trip, p50 | 2.47 ms | 2.25 ms | 0.46 ms | 0.44 ms |
| round trip, p99 | **21.04 ms** | **2.58 ms** | 1.32 ms | 1.34 ms |
| completed per second | 3 469 | 6 705 | 29 820 | 31 520 |

So the carriers are **at parity wherever a connection is reused** — `rr-16` agrees
to within 2 % on the tail and 5 % on the rate — and they separate by a factor of eight
on the tail when every request needs a new connection. The cell's rate is sixteen
connections **a second**, so this is not contention: about one request in a hundred pays
twenty milliseconds where TCP's worst hundredth pays two and a half. The median is level
with TCP's, and the cell's multi-second outliers were a separate defect, fixed. The cause is
structural rather than a tuning error: every visitor channel runs its **own Noise
handshake** before its first byte can move, while a TCP tunnel runs one handshake
and multiplexes every stream inside it. A visitor's channel open costs 2 648 µs,
of which 1 818 µs is that handshake; everything after it is at parity, and the
first byte follows the pairing in 12 µs.

### Where mhd wins, and where it loses (2026-11-07)

The carrier question has an answer per path condition, and it is not one carrier. Same binary, same
round, `--scenarios timeline`, one arm per carrier with `MOLEHILL_DATA_CARRIER` pinning each:

| cell | what the condition is | mhd p99 | TCP p99 | mhd Gbit/s | TCP Gbit/s | verdict |
|---|---|---|---|---|---|---|
| `clean` | nothing shaped | **59.0 ms** | **14.4 ms** | 8.61 | 8.50 | **TCP**, by 4× on the tail |
| `rtt100` | 100 ms added delay | 403.2 | 410.1 | 2.57 | 2.51 | parity |
| `rate100` / `rate20` | shaped to a rate | 233.2 / 549.1 | 228.1 / 508.0 | 0.09 / 0.02 | 0.09 / 0.02 | parity |
| `loss1` | 1 % loss | 1 061.1 | 1 052.1 | **4.79** | **6.07** | **TCP**, on throughput |
| `jitter` | 20 ms ± jitter | **118.0** | **171.8** | 4.02 | **5.82** | mixed — mhd's tail, TCP's throughput |
| `loss5` | 5 % loss | **1 454.9** | **2 100.8** | **1.11** | **0.30** | **mhd**, both axes |

**Read it as one sentence**: mhd's own recovery beats a TCP carrier inside a tunnel once the path loses
enough to make the kernel stack collapse — at 5 % loss it hands over **3.7× the throughput and a 31 %
tighter tail** — and it loses everywhere the path is clean, by **4× on the tail**, because a new
visitor channel's handshake queues behind the bulk already in its shared UDP sockets while a TCP
tunnel gives every channel its own connection. The crossover sits **between 1 % and 5 % loss**: at 1 %
the carriers agree on latency and TCP is 27 % ahead on throughput.

So the honest statement for an operator is not "one carrier is better". It is: **on a lossy path mhd
is the better first carrier; on a clean path TCP is.** The default tries mhd and falls back to TCP when
a session cannot be established — which is a statement about *establishment*, not about *which is
faster*, and those are different questions. A deployment that expects packet loss should prefer mhd; a
deployment on a datacentre or a well-provisioned link should prefer TCP, and the `clean` row is the
reason.

Three things this table does **not** say. It does not compare either carrier against other tools — that
is the sweep's job, and on the same `clean` cell the peers measured 1.02–1.09 ms at p50 against both
molehill carriers' 2.63–2.82 (§"The carrier axis is a path condition now" above explains why the pool
pays that). It does not establish the `loss5` ratio: that row was gathered with the condition on the
**visitor** leg, which is the leg the carrier does not cross, and a later run of the same binary and
method on the carrier's own leg read the TCP side at **1.16 Gbit/s** against the 0.30 above — a factor
of four, so "3.7×" is one run's number rather than a ratio this model can resolve. The row stays as the
published record it is; the carrier axis is being re-measured on the leg the carrier actually crosses
(`--condition-leg tunnel`). And it is loopback in a container: the loss and jitter cells are shaped, the
clean one is not, so `clean` is a lower bound on what a real path does to both carriers.

### Warm and cold

The staged schedule's first stage and its last carry the **same condition**, and they
disagree by a factor of six at the median. Both numbers are real and they answer
different questions:

| | p50 | p90 | p99 |
|---|---|---|---|
| first `clean` stage, minutes after start | 12.13 ms | 51.65 ms | 103.70 ms |
| last `clean#2` stage, the same run | **1.87** | **2.27** | **15.07** |

Read against TCP on the same cell — p50 2.24 ms, p99 2.58 — **the warm carrier is faster
at the median and at the ninetieth percentile and slower at the ninety-ninth**, while the
first stage after a restart is six times worse at the median than the carrier will be.
So a deployment is choosing between a carrier that beats TCP once it has been running and
one that is slower for the first stage of a run.

**And that stage's cost is not a start-up cost.** Within it the latency *rises*: the stage's
first twenty arrivals sit at 2.14 ms — the speed the carrier keeps — and its remaining few
hundred at 72.78 ms. A start-up cost would do the opposite. So the table's twelve
milliseconds is an average over a stage that begins fast and slows.

**Where it goes is measured, in two distributions over one second of a round.** Every stage
of an arrival is timed behind `MOLEHILL_MHD_STATS`, at one scale:

| segment | p50 | p99 |
|---|---|---|
| accepting the visitor | 15 µs | — |
| opening a channel (placement, Noise handshake, prologue) | **1.73 ms** | 4.66 ms |
| waiting for a channel to be provided | 0.95 ms | **20.1 ms** (now 7.6) |
| writing the pairing command on it | 2 µs | — |
| the visitor's first byte after pairing | 13 µs | — |

**The tail was a queue with a lottery in it.** The wait for a channel was longer than
everything else in the path put together — the client's whole side of an arrival is 2.1 ms
at the median and 7.3 at its worst — and the reason was that the queue woke *every* waiting
pairing on *every* arrival while exactly one of them could win. Ten wakes per arrival made a
visitor's wait the number of draws it lost: about five for the median and about a hundred for
the unlucky hundredth, which is the nineteen milliseconds measured. A channel now names the
request that asked for it, in four bytes of both prologues, and is delivered to that pairing
alone. An interleaved A/B with its own twin put `churn-16`'s p99 at **10.27 ms against
21.95, −53 %**, and the pairing's own p99 at **7.6 ms**. The median rose 2.93 → 3.71 ms,
because the old one was borrowed from other visitors; it is now the true cost of one channel.

Two explanations are recorded as **rejected** rather than merely unproven. The congestion
window reaches its 2 MiB slow-start ceiling within the first second of traffic, so it cannot
account for a median that stays six times worse across a whole stage. And the handshake
alone cannot: it is what one open costs, and the queue multiplied it — which is why the
derivation attempts that halve the open moved the tail without closing it, while removing the
lottery halved it while leaving the open alone.

A schedule that measured only the first stage would report the cold number and a schedule
that measured only the last would report the warm one. This one reports both, in order,
which is why the order is part of the method.

Removing the handshake by deriving each stream's keys from the lane's own was
built and measured three times. It takes the tail to 3.79 ms and doubles the
arrival rate at half the CPU per bit, and **every version cost the bulk cell** —
samples that failed outright, not percentages that needed a floor. A deployment
whose traffic is mostly short-lived connections should therefore measure its own
path before choosing a carrier; one whose connections are reused has nothing to
trade.

## The transparent-L3 wire question (the acceptance harness)

An L3 client carries whole IP packets, so the question "how much of the wire is
header, and how much of *that* could a compressor take?" has its own instrument:
`bench/src/bench/utils/l3/run.sh` samples `/proc/net/dev` inside its namespaces around
each arm, and `bench/src/bench/utils/l3/wire_report.py` turns the difference into
carried packet sizes and a ceiling. Two interfaces matter — the veth the client
dials the server on (the tunnel's wire, both directions, including the carrier's
own TCP/IP headers) and the two TUN devices (the packets the L3 path actually
carries). The denominator is always stated: **wire** is the veth's bytes, which
is the link's real cost; **carried** is those packets plus this protocol's
2-byte length prefix. The harness drives IPv4; IPv6 shares the framing and GSO
path but has a 40-byte base header, so these ceilings are IPv4 numbers.
`just l3-accept-ipv6` is the IPv6 functionality gate, not a measurement.

| Arm (one connection each) | Carried | Mean carried packet | Wire | Wire per carried packet | Header-compression ceiling |
|---|---|---|---|---|---|
| bulk: 200 000 B echoed | 434 packets / 422 584 B | 974 B | 493 633 B | 1137 B | 3.1 % of the wire (3.6 % of carried) |
| small: 2000 round trips of 64 B | 4010 packets / 464 536 B | 116 B | 785 983 B | 196 B | 17.9 % of the wire (29.7 % of carried) |

The ceiling is `35 B × carried packets`, an **upper bound** rather than a
measurement: an IPv4+TCP header is 40 B (52 B with the timestamps every Linux
host sends) and a VJ-style per-flow delta carries about 5, every packet is
assumed compressible, and the first packet of each flow, ICMP and fragments
would each cost some of it back.

What the numbers decided: **header compression was not built.** On the
small-packet arm — the workload it exists for — the ceiling is under the 20 %
wire-bytes bar the decision was written against. The reason is visible in the
same table: at 196 B of wire per 116 B carried packet, the tunnel's own
transport costs about 80 B per packet (this protocol's 2-byte length, the
multiplexer's frame, the carrier's TCP/IP header and its acknowledgements),
which is more than twice what compressing a 40–52 B header could recover. On
the bulk arm the ceiling is 3.1 %: the packets are full-sized, so there is
almost no header share to win.

### What the same instrument then changed

Every change below was made because the table above named the cost, and each was
measured on the arm it was meant to move. The relative deltas are from the debug
build the harness used at the time; the absolute rates are from the **release**
build, because that is what an operator runs and the two are not close (debug
measured 783 Mbit/s on the bulk arm where release measures 1986).

| Arm (release build, one host) | Throughput | CPU per carried packet | Same workload, no tunnel |
|---|---|---|---|
| bulk: 200 MB streamed in 65 KB chunks, 1400-byte TUN MTU | 1986 Mbit/s | 6.0 µs | 12 024 Mbit/s |
| the same with an 8000-byte TUN MTU (9000-byte link) | **3579 Mbit/s** | 18.4 µs (2.3 ns/byte) | 11 722 Mbit/s |
| paced: 2000 strict round trips of 64 B | 10 362 round trips/s | 29.9 µs | — |
| 16 flows: 16 × 200 of the same round trips | 22 543 round trips/s | 32.0 µs | 23 388 round trips/s |

What each change bought, on the arm that showed the cost:

- **Writing batches** (`[u16 length][packet]` runs in one write): 12 % off the
  bulk arm's wire bytes. Nothing on a paced round trip, which has no second
  packet to wait for — a batch is handed over the moment the device runs dry, so
  coalescing costs no latency and buys nothing when there is nothing to
  coalesce.
- **`mode = "direct"`** as the L3 default: 6 % of the wire, a third of the CPU
  and 65 % more round trips per second on the paced arm. A claim has exactly one
  channel, so the multiplex frame was pure per-packet cost
  ([configuration.md](configuration.md#transparent-l3-services)).
- **Reading batches too** (one socket read, then a run of frames parsed out of
  the buffer): +27 % throughput and −56 % CPU per packet on the bulk arm,
  because a burst stopped costing two awaits per frame.

**Concurrency batches by itself**: at 16 flows the arm runs at 96 % of its
control (23 388 round trips/s straight at the service), with no change at all,
because packets queue on their own. That is also what retired the queue-per-CPU
idea: parallel TUN queues (`IFF_MULTI_QUEUE`) would parallelise a device read
that is not what limits a busy host, and the control is what makes that
statement checkable instead of plausible.

**What is left is per-packet syscall cost — and the packet size inside the
device is now ours to change.** A `perf` profile of the release build during
the bulk arm puts **84 % of the CPU in the kernel**, 10 % in molehill and 5 %
in libc, with no symbol above 11 %: the cost is one TUN read, one socket
write, one socket read and one TUN write per packet, spread thin. No
userspace hotspot exists to fix — a synchronous data path would chase the
10 %, not the 84 % — and the levers that move it are **the packet size**:
- **The device's own offload (a code change, in v0.11.0):** the TUN device is
  attached with `IFF_VNET_HDR` and declares `TUNSETOFFLOAD`, so the kernel
  hands the reader TCP super-frames instead of segmenting first. Since protocol
  v5.1 that is the `Vnet` wire, negotiated only when both ends are Linux with
  offload; a pair with a plain-IP device negotiates `Ip` and the Linux side
  splits/checksums at its boundary, which is not measured by these Linux-pair
  numbers. Measured on a routed forward path (two netns, one TUN hop, frames
  relayed whole):
  3.85 GB of bulk TCP arrived as 576 071 reads of 1 500 bytes without the
  declaration and as 59 160 reads averaging 65 212 bytes with it — the same
  ratio of reads to bytes the 8000-byte-MTU operator lever buys below, taken
  by the binary instead of the deployment. UDP GSO segments before the reader
  on this kernel even with the declaration (UDP offload is TX-only here), so
  the win is TCP-shaped; the probe is
  [`bench/src/bench/utils/l3/tcp_tso_census.py`](../bench/src/bench/utils/l3/tcp_tso_census.py),
  and the
  wire's length prefix moved to `u32` the same release so a whole super-frame
  fits one frame.
- **The TUN MTU (an operator setting):** the same bytes in 8000-byte packets
  instead of 1400 measured 1.8× the throughput at half the CPU per byte. That
  is a deployment choice, not a code change: the TUN MTU, with a link MTU to
  match ([deployment.md](deployment.md#transparent-services)).

**What lending lanes buys, and when.** The L3 rows below are measured on claims
that hold the lanes their share of the carrier budget gives them; the client also
*lends* an idle lane of one claim to a claim carrying more flows than lanes
(`docs/internals.md`, "Lanes are lent"). Whether that is worth anything is a
question about what limits the path, and the model answers it: with the
condition on the **carrier** leg (a 100 ms tunnel, so a claim's throughput is
limited per connection), lending eight flows over the lanes an idle claim lends
is worth **+27 %** for that claim (2.395 against 1.754 Gbit/s, ten rounds, a
claim by the model's own rules: materiality 10 %, sign agreement 1.0, and the
run's A/A floor cleared). Under 1 % loss the same comparison is +12 % and
*directional* — the effect survives loss, ten rounds cannot separate it from the
scatter — and on a clean loopback it is indistinguishable. It buys that with
CPU and memory rather than efficiency (+8 % CPU per byte, +32 % CPU cores, +11 %
peak RSS, all directional): more connections carrying more traffic. With the condition on the visitor leg, or on a clean
loopback where the L3 path is limited per packet, the difference is inside the
noise. Reproduce it as an arm rather than a build: the catalog carries
`l3-budget10` (lending on) and `l3-budget10-nolend` (the same topology and the
same binary with `[transparent.data].lend = false`), so
`just bench run --arms l3-budget10,l3-budget10-nolend --condition rtt100
--condition-leg tunnel` is the whole experiment; the record is in `HANDOFF.md`.

**Attributing the multiplexing overhead.** The L4 rows above say what
multiplexing costs (a single flow on a pinned pool carries about half what a
connection of its own does, at roughly twice the CPU per byte); where that CPU
goes is `bench/src/bench/utils/mux/gap_profile.sh`, which samples both daemons with
`perf` while one iperf3 flow crosses one tunnel and prints the top symbols per
daemon. The shape it reports on this host: **~35 % kernel, ~15 % libc's memory
routines, and the mux's own Rust code a few percent per symbol** — every leaf is
small and the sum is in the copies and syscalls per byte, not in a hotspot. That
is the same conclusion the TUN path reached below, and it is why the candidates
are structural (how many times a packet is copied, how large a framed write is)
rather than a function to optimise.

**Method note: the bulk arm used to measure a deadlock.** It sent one large blob
and only then read the echo, which cannot work past the buffers — the visitor
waits for the echo, the echo waits for the visitor to read — and two runs of
20 MB and 100 MB were reported as throughput when they were five-second stalls.
The arm now streams in `BULK_CHUNK` chunks, which exercises the same full-size
packets without the deadlock, and `PROFILE_BUILD=release` selects the build (the
default is `debug`, because it builds fastest). A number from this instrument
without its build is not a number.

**Comparability.** One host, no shaping, one connection per arm, measured
between `9507a7a` and this revision; the arms are comparable with each other and
with nothing else — in particular not with the soak numbers above, which use
another instrument, a shaped path and many connections. The instrument is
root-only and lives outside the check chain (see [checks.md](checks.md));
reproduce it with `just l3-accept` and read `bulk.report` / `small.report` in
the artifact directory. Its instrument parameters are environment overrides,
because they change what the numbers mean: `BULK_BYTES` (200 000 by default;
raise it past a second of traffic when the CPU counters' 100 Hz resolution
matters), `SMALL_REQUESTS` and `SMALL_BYTES` (2000 × 64 B), `CLAIM_LANES` (unset,
which measures the product's own default of one carrier connection per claim; a
higher number spreads the claim's flows over that many connections) and
`CLAIM_LANE_CAP` (unset; a value below `CLAIM_LANES` measures the server's
`[server.data].max_tunnels` valve refusing a claim's surplus lanes
while the claim keeps serving on the rest).

## The UDP queue question (a molehill-only diagnostic)

A UDP service's datagram ceiling is a property of the *service*, not of the
stage schedule, so it has its own instrument: `bench/src/bench/utils/udp_stress.py`
starts a real pair with a configurable `max_udp_workers`, blasts paced or flat-out
visitors at it, reads the server's `MOLEHILL_UDP_STATS` line around each step,
and ends with a control step that blasts the same visitors **straight at the
sink** — without that control a slow sink and a slow tunnel look identical from
the counters. Measured on one host, 1400-byte datagrams: a single visitor never
drops (its own socket buffer throttles it at ~29.8k datagrams/s), many visitors
saturate the pool at ~1 Gbit/s regardless of `max_udp_workers` (1.14 / 1.00 / 0.98 at
1 / 2 / 4) or visitor count, the spread stays even, and the drops equal the
excess over that ceiling to within 0.07 %. The number a user needs is on the
configuration page beside `max_udp_workers`. The probe behind it is
`bench/src/bench/utils/udp_stress.py`; run it the same way to reproduce the ceiling on
your own host.

## Comparability
- **Same model, same method, same host.** Every results file records the
  fingerprint, the host identity and both calibration probes; a number from
  another host or another method is context, not a baseline.
- **The comparison is refused, not approximated.** `just bench compare` names
  every blocker — a differing method key, a missing probe, a host that
  disagrees beyond its tolerance (25 % for the CPU probe, 15 % for the loopback
  one; the same two probes the gate uses) — and a run with no A/A pair reports
  every between-file difference as `directional`, never as a claim.
- **Within one campaign the arms are comparable by construction**: one
  topology, one backend, one binary per arm, one rotation, and a control in
  every round.
- **The soak records are history, not a baseline.**
  `benches/records/results-soak-vX.Y.Z.json` were measured with the model this
  one replaced: a different schema, different instruments and a different
  definition of a cell. The gate refuses one as a baseline, and the numbers in
  the released tables stand as what was measured then, with
  [the per-decision table](#what-each-configuration-choice-costs-per-decision-measurements)
  as the only surviving basis for choosing between configurations.
- **Variance is stated, not smoothed.** If a difference sits inside the run's
  own floors, it is reported as indistinguishable and no claim is made from it.
- **Stages are compared by occurrence, not by name.** The schedule opens and
  closes with the same `clean` condition, so `clean#2` of one run is compared
  with `clean#2` of another: the return stage — the recovery axis — is judged
  against the baseline's *return*, not against its fresh start.
- **The tunnel pool is pinned, so its size *is* a setting.** A build
  establishes `[client.data].tunnels` connections at service start and
  keeps them (a dead one is repaired). The `MOLEHILL_POOL_STATS` line reports
  the configured `count` beside the live `size`: a `size` below `count` is a
  pool whose repair is being refused, and an arm's width is a method parameter
  exactly like its carrier.

## Reproduce it yourself

On a Linux host with `iperf3`, `tc` and root (see `just bench-deps`):

```bash
just bench doctor                      # is this host ready?
just bench peers fetch                 # the reference tools' release binaries
sudo -n just bench --profile dev --aa  # a change, measured
just bench plot ~/tmp/bench-*.json     # its charts and tables
```

The release sweep is one command, and the gate is the other:

```bash
sudo -n just bench --profile sweep --out benches/records/results-bench-vX.Y.Z.json
just bench gate benches/records/results-bench-vX.Y.Z.json \
     --baseline benches/records/results-bench-v<previous>.json
```

`just bench run --help` lists the profiles, the arms, the conditions and the knobs;
`just bench list` prints the registry; and every parameter that changes what a
number means is recorded in the results file (the resolved scenario parameters,
the condition, the timeline, the instrument cadences, the SLO), so a chart can
always be traced back to the method that produced it.

The release gate — what a published number must satisfy before a tag can carry
it — is documented in [release.md](release.md).
