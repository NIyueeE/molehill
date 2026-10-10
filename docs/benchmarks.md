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

`benches/scripts/bench/` is the standard and the only runner. The sweep the
release publishes is not a second instrument: it is this model's `sweep`
profile — the `timeline` and `capacity` scenarios over the product and the
reference tools — which is why the staged schedule and the load ramp travel in
one results file, under one schema, behind one gate.

```bash
sudo -n just bench                     # the smoke profile, about a minute
sudo -n just bench --profile dev --aa  # the default loop for an optimization
sudo -n just bench-doctor              # what this host can and cannot measure
just bench-list                        # every metric, condition and profile
just bench-report ~/tmp/bench-*.json   # a stored run: tables, floors, verdicts
just bench-plot ~/tmp/bench-*.json     # its charts and markdown tables
just bench-gate RESULTS.json           # may this run be published?
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

- **A metric is defined once, in code.** Unit, direction, the denominator it is
  a ratio over, the workload kinds that can produce it and the smallest
  difference worth calling a claim all live in `model.METRICS`; the table below
  is generated from that registry and `just bench-list` prints it. A metric
  described one way and computed another fails `just bench-selfcheck`.
- **A control arm is mandatory.** Every scenario also runs on the same
  topology, ports and backend with no tool in the path. Without it a slow probe
  and a slow tunnel are the same reading, which is why the model always reports
  a tool arm *against its own control* rather than on its own.
- **A run states its whole method before its first sample.** The results file
  carries a fingerprint over the topology, the conditions and timelines, the
  scenarios and their *resolved* parameters, the instrument cadences and the
  model's own code. Two files may only be compared when it matches, and
  `just bench-compare` names every key that differs instead of printing a
  difference between two methods.
- **The run measures its own resolution.** One arm is measured twice under two
  names (`--aa`; on by default in `dev`, `full` and `stage`), and the paired
  difference between those two halves is the smallest difference the run can
  believe. Every verdict raises the metric's own materiality floor to it, and
  the run prints the floors beside its tables. A resolution of 16 % is a
  statement about the run, not an excuse: it says what to change (longer
  workloads, more rounds) to resolve more.
- **A verdict has five states, and "no claim" is one of them.** `claim` needs
  every paired round to agree in sign *and* both floors cleared; `directional`
  is a direction with a disagreement or a floor in the way; `indistinguishable`
  is inside the run's own scatter; `single-round` refuses to read a direction
  into one measurement; `unavailable` names what was not measured. A metric no
  arm could produce is reported with the instrument's reason, never as zero.
- **Evidence stays with the number.** Each cell records the commands it ran,
  the counters it read, the endpoint the probe dialed, the peers the backend
  saw, the raw per-round sample arrays and the log its probe left behind. The
  results file is written as the run goes, so an interrupted campaign is still
  analysable.
- **Results live outside the tree** unless the run *is* the release sweep: a
  run writes `~/tmp/bench-<stamp>.json` and its artifacts beside it (the
  invoking user's home, even under `sudo`); `--out`/`--work` move them, and an
  existing `--out` is refused unless `--force`.

### The metrics

Generated from `model.METRICS`; `just bench-list` prints the same registry, and
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
| `wedge_count` | count | lower is better | interactive silences longer than 5 s: a tool that stops answering under degradation, which an average cannot show | one silent stretch |
| `wedge_max_s` | s | lower is better | the longest interactive silence in the run | one silent stretch |
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
| `udp_rtt_p99_ms` | ms | lower is better | 99th percentile of the probe's datagram echo round trips | one echoed datagram |
| `setup_p50_ms` | ms | lower is better | median time to establish one connection, when the workload opens a fresh one per request (the setup a visitor pays to arrive) | one connection |
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
| `dropped_packets` | count | lower is better | packets the topology's interfaces dropped over the window | one packet |


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
| `churn-16` | `rr` | a fresh connection per request: the setup cost a visitor pays, and the per-visitor state the architecture keeps | `rate_per_s` |
| `udp-pace` | `udp` | a paced UDP session: what fraction arrives, and how it is spaced | `udp_loss_pct` |
| `udp-ladder` | `udp-ladder` | where a UDP path starts shedding, per offered rate | `udp_loss_pct` |
| `timeline` | `staged` | how the tool behaves as the path degrades and recovers: one interactive stream, N bulk streams, a churn stream and a UDP session, over a scripted schedule of conditions, in place | `rtt_p99_ms` |
| `soak` | `staged` | over a long run on a rotating path: does anything leak, drift or degrade | `rtt_p99_ms` |
| `cost` | `staged` | at one fixed operating point, how many CPU-seconds one carried Gbit/s costs | `cpu_s_per_gbit` |
| `capacity` | `capacity` | how much bulk load the tool carries while a fresh interactive connection still meets the SLO | `capacity_streams` |
| `reconnect` | `reconnect` | what a visitor pays after a restart: time from process start to answering | `startup_s` |
| `udp-blast` | `udp` | the datagram ceiling when the probe offers as fast as it can | `udp_recv_mbit` |

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
| `sweep` | 1 (+0 warm-up) | ~2400 s | the release sweep: the staged schedule and the load ramp |

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
  context instead of picking a winner in them.
- **A stage that carried no reading says so, with the reason.** `— (reason)` in
  the plot's table, and `bulk_gbps_source` in the results file, distinguish "the
  path carried nothing" from "nobody could measure what it carried". A bare
  `0` cannot: it is the number a defeated sender reports for a path that was
  full.
- **The per-stage numbers carry their sample count.** A stage's `rtt_p99` is a
  nearest-rank percentile of that stage's interactive samples, so with fewer
  than a hundred samples the "p99" *is* the worst observation — a stage that
  wedged for most of its window (the shaped stages of a saturated run often
  carry tens of samples) reports its worst single measurement, not a tail
  estimate. The charts mark it with the worst-second tick; the results file
  carries `rtt_n` per stage so a reader can tell which kind of number a cell is.

## The conditions and the stage schedule

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

A **single-condition** run (`--condition NAME`, on every other scenario) is the
same vocabulary without the schedule: one cell, one condition, one number.

| Stage | What it emulates | Applied to the path | `sweep` hold |
|---|---|---|---|
| `clean` | a healthy network (the control) | nothing | 150 s |
| `rtt100` | a long-haul or satellite link | 100 ms delay | 120 s |
| `loss1` | a lossy wifi or mobile link | 10 ms delay, 1 % loss | 120 s |
| `loss5` | a badly congested path | 100 ms delay, 5 % loss | 120 s |
| `rate100` | a 100 Mbit uplink | 100 Mbit/s, 20 ms delay, 2000-packet queue | 120 s |
| `rate20` | a 20 Mbit/s uplink | 20 Mbit/s, 40 ms delay, 2000-packet queue | 120 s |
| `loss1_rate100` | a 100 Mbit uplink that also loses packets — the lossy WAN the carriers are chosen for | 100 Mbit/s, 20 ms delay, 1 % loss | focused cell only |
| `jitter` | a bufferbloated access link | 20 ms delay ± 10 ms | 120 s |
| `clean` | recovery — is the tool still the tool it was? | nothing | 150 s |

The schedule, the durations, the sample rates and the SLO are recorded in every
results file (`meta`), so a chart can always be traced back to the method that
produced it. So is the shaping applied to each class — including the rate
stages' queue depth (`rate`/`limit 2000`), which bounds how much traffic the
shaper may hold and therefore what a burst through it can do.

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
together with the `carrier = "kcp"` row below.

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

## Running it

```bash
# the fast loop, and the loop an optimization should use
sudo -n just bench
sudo -n just bench --profile dev --aa

# the release sweep: the staged schedule and the load ramp, four arms
just bench-peers
sudo -n just bench --profile sweep --out benches/records/results-bench-vX.Y.Z.json

# one condition, the whole workload (a shape question in minutes)
sudo -n just bench --profile stage --scenarios cost --arms molehill,frp

# an A/B: the same arm twice, two builds, interleaved, verdicts against the
# run's own noise floor
sudo -n just bench --profile screen --ab-arm molehill --binary-b /path/to/build

# an experiment with a knob the catalog does not carry: declare it
sudo -n just bench --arm id=l3-deep,txqueuelen=20000
```

`just bench-doctor` states what this host can and cannot measure before a run
has to find out (root, `/dev/net/tun`, iperf3, the peer binaries). The peer
binaries come from `just bench-peers`: the latest GitHub release assets, never
built from source, with the resolved versions recorded in the results meta.

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
  probes) and the arm list; `samples` is one record per arm, round, scenario
  and cell, each with its metrics, its typed absences and its evidence;
  `summary` is what the analysis derived from those samples — and
  `just bench-report` recomputes it, because the samples are the evidence and
  the summary is a rendering of it.
- **`just bench-plot` renders exactly the figures this page describes** from
  any results file: the timeline master, the small multiples, the capacity
  curve, the UDP ladder, the drift panels and the cost bars.

## The gate

`just bench-gate` is what a run must satisfy before its numbers may be
published. It reports what it saw for every question and fails the ones a human
must act on:

- **coverage** — every cell the scenarios declared, for every arm;
- **the endpoint invariant** — no L4 or peer arm dialed the backend it forwards
  to (a transparent arm is the documented exception: the visitor dials the
  address the client owns, and the client's kernel delivers it);
- **the SLO on the clean stages** — for the product's own arms; a reference
  peer that misses it is reported with its number and never blocks a tag;
- **the drift and wedge axes** — a leak is a slope over time and a wedge is an
  interactive silence; a slope is only judged when the run is long enough to
  carry one (fifteen minutes), and a shorter run's slopes are reported as
  context;
- **the capacity ramp** — a ramp that sustained nothing is not a reading;
- and, with `--baseline`, the **regression half**: the same comparison
  `just bench-compare` makes, refused unless the two files' method records
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
| `carrier` | `"tcp"` (default) | ahead of the KCP carrier in every unflagged measurement (loopback 1-stream 5.8 vs 3.7 Gbit/s against the kcp4 arm on the noise transport), and far cheaper in memory (RSS 26 vs 85 MiB). One 8-stream loopback cell (14.9 vs 1.1 Gbit/s) is excluded here: it was bimodal across repetitions on both builds, so it is not evidence of anything |
| `carrier` | `"kcp"` | only when TCP data tunnels are blocked or throttled, or to A/B a UDP game on a high-latency path: its one measured win is UDP session quality at rtt100 (0 % loss, 20 ms maximum inter-packet gap vs 100+ ms for the TCP arms) |
| transport | `"plain"` | 10.0 / 19.5 Gbit/s (1 / 8 streams) on loopback |
| transport | `"noise"` | 5.8 / 14.9 Gbit/s; sub-millisecond RTT cost; CPU parity under full load |
| `pool_size` | 8 TCP / 2 UDP (defaults) | setup-to-first-byte p99 ~3.5 ms at 16-way churn; UDP shards distinct visitors across channels and never splits one session (session affinity) |
| `[server.data].stripe_count` | `K = 4` (a striped group spreads one visitor connection over K data channels; see [configuration.md](configuration.md)) | a single long-lived connection stops being bounded by one tunnel flow: 1-stream throughput +48.7 % on loopback, at the cost of a reorder buffer, +8.7 % RSS and +40.8 % CPU (per-frame CPU is halved, because the frames spread over four driver tasks) |

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

**A claim can now hold a member set**, and that is what finally moved the
ceiling (`members` / `[transparent.data].default_members`, default 1 — so every
arm above keeps its meaning). With four members, inner flows hashed across them
by a canonical 5-tuple, jumbo path, four measured rounds an arm and the A/A twin
in the run:

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

The operator's copy of the packet-size finding, with the recipe, is
[deployment.md](deployment.md#transparent-services); what is left on the table —
a device path that can carry more than one packet per syscall — is the next
lever, and it needs its own arm before it is a claim.

### The carrier axis: TCP versus KCP (2026-10-10, this model)

`mode` and `carrier` are independent, so the two axes are measured apart:
`l3` / `l3-mux` are the same L3 arm over TCP with and without the multiplexer,
and `l3-kcp` / `l3-mux-kcp` are the same pair over KCP. Every figure below is
one results file, two measured rounds per arm, the A/A twin in the run as its
own noise floor, and the condition named on the row — `clean` is an unshaped
path, `tunnel` means the shaping lands on the link the carrier crosses.

**Clean path, one bulk flow** (`--profile smoke --condition clean`, L3 arms, the
control arm is the same topology with no tool in it):

| Arm | Mode / carrier | Throughput | CPU per Gbit | Wire per visitor byte |
|---|---|---|---|---|
| control | — (no tool) | 41.813 Gbit/s | — | 1.0010 |
| `l3` | direct / tcp | 4.173 Gbit/s | 0.855 s | 1.0745 |
| `l3-mux` | multiplex / tcp | 2.995 Gbit/s | 1.435 s | 1.0898 |
| `l3-kcp` | direct / kcp | 2.188 Gbit/s | 2.554 s | 1.1483 |
| `l3-mux-kcp` | multiplex / kcp | 2.511 Gbit/s | 2.310 s | 1.1414 |
| `l3~aa` | direct / tcp (the same arm twice) | 4.008 Gbit/s | 0.868 s | 1.0746 |

Read it as two costs that stack: the multiplexer costs **28 %** of the bulk
throughput and **68 %** more CPU per byte (`l3` → `l3-mux`), and the KCP carrier
costs **48 %** of it and **3×** the CPU per byte (`l3` → `l3-kcp`) on a path with
nothing wrong with it. The same ordering holds for one connection doing
request-response round trips (`rr-1`: 16 283/s at p99 0.070 ms on `l3`,
13 866/s at 0.086 ms on `l3-mux`, 9 742/s at 0.137 ms on `l3-kcp`), while
16 concurrent connections finish within 12 % of each other on every arm — that
scenario is bounded by the probe, not by the carrier.

**Loss on the carrier's own leg** (`--condition-leg tunnel`), one bulk flow:

| Condition (tunnel leg) | control | `l4` (mux/tcp) | `l4-kcp` | `l3` (direct/tcp) | `l3-kcp` | A/A twin |
|---|---|---|---|---|---|---|
| `loss1` — 10 ms delay, 1 % loss | 4.425 | 2.506 | **0.395** | 2.030 | **0.413** | 2.810 |
| `loss1_rate100` — 100 Mbit/s, 20 ms, 1 % loss | 0.096 | 0.093 | **failed, 3/3 rounds** | 0.089 | 0.059 | 0.095 |

All figures Gbit/s. KCP is **five to six times behind TCP** once the path loses
packets, in both architectures, and on the rate-limited lossy leg the
multiplexed KCP arm does not finish its test at all: three rounds out of three
died with `iperf3: control socket has closed unexpectedly`, and the daemon's own
debug log names the cause — `KCP session dead link`. The visitor's connection
dies with the session, where a TCP carrier would simply have slowed down.

The mechanism is in the carrier's own counters (`MOLEHILL_KCP_STATS=1`, the
sender's side of a 3-second cell): **22 % of the datagrams it sent were
retransmissions** and the path's own loss accounted for a fraction of that. With
`nc=1` the ARQ has no congestion control — the pacer's only signal is a PONG
that fails to arrive within 2.5 s — so a window-sized burst goes into whatever
queue the path has; the standing queue delays the acknowledgements past KCP's
escalating RTO, the retransmissions enlarge the queue, and on a rate-limited
path the ARQ exhausts its 20-retransmit budget and declares the peer dead. That
is a congestion collapse, self-inflicted, and it is the thing to fix before the
carrier is worth choosing for a lossy path.

**Where the carrier stands across the whole workload.** The `dev` profile — every
scenario, four measured rounds plus a warm-up, one condition, the A/A twin in the
run — on a jumbo path, with the shipped build:

| Scenario | `l3` (TCP carrier) | `l3-kcp` | `l3~aa` (the twin) |
|---|---|---|---|
| `bulk-1` (1 flow) | 7.554 Gbit/s | 7.136 | 7.496 |
| `bulk-n` (8 flows) | 6.023 Gbit/s | **7.418** | 6.243 |
| `bulk-pair` | 13.644 Gbit/s | 12.357 | 13.827 |
| `rr-1` | 14 167/s @0.086 ms | 10 177/s @0.133 ms | 16 139/s @0.079 ms |
| `rr-16` | 32 984/s @1.317 ms | 30 264/s @1.312 ms | 32 994/s @1.309 ms |
| `churn-16` | 7 139/s @2.374 ms | 6 779/s @2.417 ms | 7 209/s @2.291 ms |
| `udp-pace` (loss) | 0.802 % | 0.835 % | 0.790 % |
| `udp-ladder` @2 Gbit/s (loss) | 0.60 % | 1.05 % | 0.60 % |

Read against the twin's spread, that is: **level on one bulk flow and on churn,
19 % ahead on eight flows, behind on short round trips** (`rr-1` −28 %, `rr-16`
−8 %, at equal p99), and **roughly twice the inner-UDP loss** at every offered
rate on the ladder (0.60 → 1.05 % at 2 Gbit/s, 0.55 → 1.04 % at 5 Gbit/s). The
last one is the quality gap to keep in mind rather than a throughput one: UDP
cannot recover what the carrier drops, and what drops it is the same hub that
serves the TCP carrier — the difference is how fast the carrier's writer drains
the endpoint's channel behind it.

**The long-haul cell is the one that is still behind, and it is the path's
queues, not the carrier's arithmetic.** On `rtt100` (100 ms each way, no loss
configured) the TCP carrier moves 0.461 Gbit/s and `l3-kcp` 0.178 — 2.6× behind,
the widest gap left anywhere in this model. The carrier's own counters say why:
**21 % of the datagrams it sent never reached the receiver** on a path that drops
nothing on purpose, and **28 % of what it sent was recovery traffic** — split
4 239 RTO-driven resends against 2 163 fast-retransmit ones, i.e. mostly the
sender timing out on acknowledgements that were merely late. The mechanism is a
whole window going out at once: 2 048 segments is 16 MiB at a jumbo datagram
size, a bottleneck queue holds a thousand packets, and the ARQ has no congestion
control to spread the difference.

Two repairs were tried against that, and the measurements chose between them:

* **RTO headroom — kept.** The reference's timeout is `srtt + max(interval,
  4·rttvar)`, so this adapter's 10 ms flush interval is also the timeout's only
  margin; on a low-jitter 200 ms path the timeout lands at **205 ms** and the
  acknowledgements arrive at 200 ms plus the receiver's own batching. A margin of
  `srtt/8` (nothing changes below an 80 ms round trip) cuts RTO-driven resends by
  **42 %** and the wire ratio from **1.20 to 1.09** on that cell, with throughput
  flat (0.178 against 0.169–0.197 across the runs).
* **Pacing the burst to the bandwidth-delay product — measured, and left out.**
  Capping the send rate at `window / srtt` (655 Mbit/s on this cell) is the
  textbook answer to a bursting sender, and as first built it was **35 % worse**:
  0.114 [0.104..0.124] against 0.175 [0.160..0.181] Gbit/s over three rounds
  each. The reason was not the rate, it was the refusal: a datagram the pacer
  would not take was **dropped**, so every act of rate control became a loss
  event with a recovery round trip behind it. Parking refusals instead (below)
  makes the same cap worth **+21 %** on that cell — 0.204 [0.176..0.244] against
  0.169 [0.154..0.201] — but the wire ratio grows by **14 %** with it, so it is a
  trade rather than a win and stays out of the shipped path until something
  decides that trade. Its numbers are here so the decision can be made on them.
* **Parking what the pacer or the kernel refuses — kept.** The drain used to
  discard a datagram it could not send (a denied span, a partial `sendmmsg`, an
  `EAGAIN`) and let the ARQ re-send it a round trip later. Holding it instead, in
  order and bounded by half the retransmission timeout, is worth **12 % on the
  lossy leg** — 1.824 [1.822..1.855] against 1.627 [1.521..1.732] Gbit/s, ranges
  disjoint — at an identical wire ratio, and it is neutral on the clean and
  long-haul cells. The bound matters: a parked datagram the engine also times out
  goes on the wire twice, which is where the pacing experiment's 14 % of extra
  wire bytes comes from.

**And the signal itself was wrong.****And the signal itself was wrong.** That PONG timeout is the pacer's only
input, and it fires on *any* late PONG — including one queued behind a peer that
is busy sending, which is exactly the state a fast path is in. The cut is 25 %
and the recovery is 5 % per four clean PONGs, so one heavy transfer ratchets the
rate down four times (12 → 3.8 Gbit/s, measured on the stats line) and leaves it
there: the *next* transfer in that session reads a fraction of what the same
transfer reads on a fresh session. Measured on a jumbo path, `bulk-n` twice in
one session: **7.79 then 7.67 Gbit/s** after the fix, against **6.18 then 1.00**
before it, and a `bulk-1` cell followed by `bulk-n` read 7.19 then 7.44 against
7.1 then 2.0. The fix is the one the signal was missing: a late PONG only means
congestion when the send window is **not** moving — a peer that is acknowledging
data is working, whatever its keepalive looks like — so the pacer now holds its
rate while progress is being made, and still cuts when a session stalls. The
slow regimes confirm it costs nothing: `rtt100` 0.169 → 0.182 Gbit/s and `loss5`
unchanged to three decimals.

A window *byte* budget was tried alongside this, to stop the buffers growing 5.7×
on a jumbo path, and it is **falsified**: it did not touch the collapse (the
pacer was the cause) and it cost the lossy leg two thirds of its throughput —
0.45 against 1.67 Gbit/s on `loss1` with jumbo datagrams — because a window
barely one bandwidth-delay product wide leaves fast retransmit nothing to ride
on. The windows stay segment-counted and, on a lossy path, wider than the BDP is
the point.

**The one place KCP won.** On the same `loss1_rate100` leg, 16 concurrent
connections of short round trips (`rr-16`) came out *ahead* on both KCP arms —
338.6/s for `l3-kcp` and 337.8/s for `l4-kcp` against 323.2 control, 328.0 `l4`,
335.3 `l3` and 323.3 for the A/A twin — with the best p99 of any arm (92.8 ms
for `l4-kcp`, against 286.2 ms for the control). Fast retransmit is worth
something on a lossy path; it is the bulk path where the missing congestion
control costs more than the recovery gains.

**The lever that did work: the datagram size follows the path.** Every number
above was taken with the carrier pinned at 1400-byte datagrams — KCP's protocol
default — because the adaptation that existed was shrink-only: it lowered the
size for a small path and never raised it. The per-datagram cost that ceilings
the carrier (a UDP send, a receive, a header, an acknowledgement and a loss
event, all *per datagram*) is therefore paid 5.7× more often than a jumbo path
requires. Letting the size follow the kernel's path-MTU answer, up to an 8 KiB
ceiling, measures (two rounds an arm, the A/B twin beside it, `l3-kcp`, one bulk
flow):

| Condition | pinned at 1400 | follows the path | CPU per Gbit |
|---|---|---|---|
| clean, jumbo link (`--link-mtu 9000`) | 2.170 Gbit/s | **3.026 Gbit/s (+39 %)** | 2.556 → **1.829 s (−28 %)** |
| `loss1`, jumbo link | 0.407 | **1.713 (+4.2×)** | 1.749 → **1.051 (−40 %)** |
| `loss1`, 1500-byte link | 0.406 | 0.434 (neutral) | 1.734 → 1.743 |
| `loss1_rate100`, jumbo link, `rr-16` | 337.8/s | 350.5/s (+3.8 %) | — |

The third row is the safety property, and it is why this is done by following
the probe rather than by raising a constant: on a 1500-byte path the datagram
stays at what that path carries, so the change is neutral there, while the wire
ratio improves in every jumbo row (1.1483 → 1.0892 on clean).

**With both fixes, a jumbo path brings the two carriers level — for one flow.**
The same cell that the table above measures at a pinned 1400, re-measured with
the TUN MTU at 8000 *and* the link at 9000, one bulk flow, the A/A twin in the
run:

| Arm | `bulk-1` (1 flow) | CPU per Gbit | `bulk-n` (8 flows) |
|---|---|---|---|
| `l3` (TCP carrier) | 7.340 Gbit/s | 0.305 s | 5.460 |
| `l3-kcp` | 6.996 Gbit/s | 0.745 s | **7.548 Gbit/s** |
| `l3~aa` (the twin) | 6.982 Gbit/s | 0.305 s | 6.447 |
| `l4` (forwarding) | 7.766 Gbit/s | 0.235 s | 23.170 |

So on a path that carries jumbo datagrams: one bulk flow is a tie — `l3-kcp`
6.996 against the TCP carrier's 7.340, whose own twin reads 6.982, i.e. the three
are inside the run's 15 % noise floor — and **eight flows come out ahead of the
TCP carrier** (7.548 against 5.460 and a twin at 6.447, so the gap clears the
floor). The cost is CPU: 2.4× per byte, unchanged all along. What the carrier
does not win is short round trips (28 648/s against 31 364/s, at the same p99)
or the lossy regimes above.

**The failure itself was then fixed, and it was the rule, not the size.****The failure itself was then fixed, and it was the rule, not the size.** The
`KCP session dead link` above is the engine's reference behaviour: a segment
retransmitted twenty times ends the session. That conflates two different
things — a peer that is *gone* and a peer that is *slow*. On a rate-limited
path the acknowledgements sit behind megabytes of shaped traffic, so a segment
collects its twenty retransmissions while the peer is answering everything
else, and a live session is closed. Measured on the same `loss1_rate100` leg,
the multiplexed arm failed `bulk-1` in **every** round before this change and
completes **both** rounds after it (0.058–0.059 Gbit/s, 17 % fewer wire bytes
than the direct arm and a third less CPU per byte), and the direct arm's CPU per
byte fell by a fifth. The rule now needs both: the retransmit count *and* a send
window that has not moved for five seconds — a gone peer stops acknowledging,
a slow one keeps acknowledging something.

**The obvious fixes were tried, and they are wrong.** `nc = 0` (the engine's
own congestion control) as an A/B against the shipped build on `loss1_rate100`
measured 0.002 Gbit/s against 0.043 on bulk and 256/s against 344/s on `rr-16`:
its window collapses on a lossy path and does not recover. Capping the send
window to what fits the path instead of blasting 2.8 MiB into it (2048 → 256
segments, an A/B on the same condition) left the bulk cell where it was (0.039
against 0.044 Gbit/s) and did not stop the multiplexed arm's dead link. So the
missing piece is not a window size either: the carrier's recovery loop retransmits
**24 % of its output** on that leg against a 1 % loss rate, and it needs a
controller driven by what the path actually delivers — which is a redesign of
the adapter's pacing, not a constant.

What this means for the guidance: `carrier = "kcp"` stays a choice for paths
where TCP tunnels are blocked or throttled, not a general improvement, and the
bulk-collapse above is a reason to prefer it only where the workload is
many short interactions. The open work — and the reason this section exists in
this shape — is a carrier whose pacing reacts to the path's rate instead of to a
2.5-second timeout; the model A/Bs it against the current build with
`--ab-arm l3-kcp --binary-b <other build>` on these same conditions.

Which setting to pick, and why:
[configuration.md](configuration.md#choosing-your-configuration-decision-tree);
for what a per-decision figure is and is not comparable with, see
[Comparability](#comparability).

## The transparent-L3 wire question (the acceptance harness)

An L3 client carries whole IP packets, so the question "how much of the wire is
header, and how much of *that* could a compressor take?" has its own instrument:
`benches/scripts/l3/run.sh` samples `/proc/net/dev` inside its namespaces around
each arm, and `benches/scripts/l3/wire_report.py` turns the difference into
carried packet sizes and a ceiling. Two interfaces matter — the veth the client
dials the server on (the tunnel's wire, both directions, including the carrier's
own TCP/IP headers) and the two TUN devices (the packets the L3 path actually
carries). The denominator is always stated: **wire** is the veth's bytes, which
is the link's real cost; **carried** is those packets plus this protocol's
2-byte length prefix.

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

**What is left is per-packet syscall cost, and it is not ours to remove.** A
`perf` profile of the release build during the bulk arm puts **84 % of the CPU
in the kernel**, 10 % in molehill and 5 % in libc, with no symbol above 11 %:
the cost is one TUN read, one socket write, one socket read and one TUN write
per packet, spread thin. No userspace hotspot exists to fix — a synchronous
data path would chase the 10 %, not the 84 % — and the lever that does move it
is the **packet size**: the same bytes in 8000-byte packets instead of 1400
measured 1.8× the throughput at half the CPU per byte. That is an operator
setting, not a code change: the TUN MTU, with a link MTU to match
([deployment.md](deployment.md#transparent-services)).

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
matters), `SMALL_REQUESTS` and `SMALL_BYTES` (2000 × 64 B), and `CLAIM_MODE`
(unset, which measures the product's own default; `multiplex` selects the other
data-channel mode).

## The UDP queue question (a molehill-only diagnostic)

A UDP service's datagram ceiling is a property of the *service*, not of the
stage schedule, so it has its own instrument: `benches/scripts/udp_stress.py`
starts a real pair with a configurable `udp_workers`, blasts paced or flat-out
visitors at it, reads the server's `MOLEHILL_UDP_STATS` line around each step,
and ends with a control step that blasts the same visitors **straight at the
sink** — without that control a slow sink and a slow tunnel look identical from
the counters. Measured on one host, 1400-byte datagrams: a single visitor never
drops (its own socket buffer throttles it at ~29.8k datagrams/s), many visitors
saturate the pool at ~1 Gbit/s regardless of `udp_workers` (1.14 / 1.00 / 0.98 at
1 / 2 / 4) or visitor count, the spread stays even, and the drops equal the
excess over that ceiling to within 0.07 %. The number a user needs is on the
configuration page beside `udp_workers`. The probe behind it is
`benches/scripts/udp_stress.py`; run it the same way to reproduce the ceiling on
your own host.

## Comparability
- **Same model, same method, same host.** Every results file records the
  fingerprint, the host identity and both calibration probes; a number from
  another host or another method is context, not a baseline.
- **The comparison is refused, not approximated.** `just bench-compare` names
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
  establishes `[client.data.tcp|kcp].tunnels` connections at service start and
  keeps them (a dead one is repaired). The `MOLEHILL_POOL_STATS` line reports
  the configured `count` beside the live `size`: a `size` below `count` is a
  pool whose repair is being refused, and an arm's width is a method parameter
  exactly like its carrier.

## Reproduce it yourself

On a Linux host with `iperf3`, `tc` and root (see `just bench-deps`):

```bash
just bench-doctor                      # is this host ready?
just bench-peers                       # the reference tools' release binaries
sudo -n just bench --profile dev --aa  # a change, measured
just bench-plot ~/tmp/bench-*.json     # its charts and tables
```

The release sweep is one command, and the gate is the other:

```bash
sudo -n just bench --profile sweep --out benches/records/results-bench-vX.Y.Z.json
just bench-gate benches/records/results-bench-vX.Y.Z.json \
     --baseline benches/records/results-bench-v<previous>.json
```

`just bench --help` lists the profiles, the arms, the conditions and the knobs;
`just bench-list` prints the registry; and every parameter that changes what a
number means is recorded in the results file (the resolved scenario parameters,
the condition, the timeline, the instrument cadences, the SLO), so a chart can
always be traced back to the method that produced it.

The release gate — what a published number must satisfy before a tag can carry
it — is documented in [release.md](release.md).
