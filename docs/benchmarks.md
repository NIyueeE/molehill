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
  50 ms and 0.5 % errors for a path to be considered usable. It is the one
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

**A class is applied to the visitor's leg** — the ports the workload dials on
the tool's exposed side — plus, for the KCP carrier, the tunnel's own UDP port,
the one tunnel the harness can name on the wire. The tool's backend leg (the
client process to the backend it forwards to) stays unshaped, because a real
deployment's WAN is on the visitor's side and the backend is next to the tool.
Shaping both legs was the model until 2026-09-29 and it is not the same
measurement: one HTB class serves both legs, so a `rate100` class carried
100 Mbit *in total* — all four tools read ~42 % of the nominal rate end to end
— and every injected delay was paid twice, which is why a `rtt100` stage's
interactive floor read 802 ms then and 401 ms now (a fresh connection per ping
pays the one-way delay twice: handshake and request). `SOAK_SHAPE_LEGS=both`
restores the old scope for reproducing a run measured under it; the value a run
used is in its `meta.shape_legs`.

**A rate class bounds the bulk client's socket window** (`SOAK_RATE_SOCKET_WINDOW`,
default `256K`; `off` reproduces a run measured before it). An unbounded sender
defeats its own accounting against a rate shaper: its writes complete into a
socket buffer far larger than the shaped path can drain, the measured intervals
then read zero bytes while the path keeps carrying them, and at `rate20` the
client is still blocked past the stage boundary, so its summary never arrives at
all. The bounded window keeps the writes tracking the path. Measured on one
host, `rate20`: the zero-byte share of the stage's intervals falls from 72-79 %
to 13-14 %, and the cell reads **0.0196 Gbit/s — 98 % of the 20 Mbit the class
applies** — where the unbounded client read 0.0334 (70 % *above* nominal,
because the transfer outlived the stage it was measured in); `rate100` reads
0.1000 against 0.0935-0.1081. It is applied to rate classes alone: a window is
meaningful only where the path's rate is known, and on a delay-only or clean
stage it would cap the bandwidth-delay product and change the measurement it
exists to serve.

A stage does not start until the previous one has gone quiet. The boundary
kills the bulk client, and a killed TCP socket keeps delivering what its
kernel side still holds — and keeps retransmitting its FIN through whatever
qdisc is installed. Reshaping at that instant puts the old stage's drain into
the same queue as the new stage's handshake, and because a tool's data-plane
ports share one netem class, a SYN dropped behind that drain costs the next
stage its first tens of seconds. Measured with **no tool in the path at all**
(htb + netem on `lo`, 20 bulk streams killed as the qdisc changed): a fresh
connect timed out after 10.5 s and the next round trip took 3.5-6.7 s,
reaching the steady state only ~15 s in. So the harness waits, at the *old*
shaper, until the path is quiet by two measurements — no more than a frame's
worth queued (`64 KiB`, one `lo` MTU) **and** no socket on the throughput port in
a state that can still send (`ESTAB`, `FIN-WAIT-1`, `CLOSE-WAIT`, `SYN-SENT`,
`SYN-RECV`) — both holding for two consecutive polls, and only then applies the
next stage's shaping. `SOAK_DRAIN_BUDGET` (180 s) is a **safety net, not the
mechanism**: the wait ends on the predicate, and a budget that fires is logged
and recorded rather than absorbed.

Both halves are tolerances for a measured reason. The queue never reaches zero,
because the interactive, churn and UDP probes share the tool's class and leave
~1.2 KB in it permanently; a drain that waits for an exactly empty queue can
only ever end by burning its budget, which makes the whole transition
timer-driven — the next stage's start state then depends on the clock rather
than on the path. And "no socket" cannot mean "no non-LISTEN socket" either:
`FIN-WAIT-2` and `CLOSING` linger for *minutes* after a bulk client is killed
while carrying nothing, whereas `FIN-WAIT-1`/`CLOSE-WAIT` are exactly the
states that retransmit the tens of MB such a client's kernel still holds. The
states above are the ones that can still send; the ones left out cannot.

Sized from the measurement, not from taste: after a `rate20` stage's bulk client
is killed, its kernel still holds tens of MB and delivers them at that stage's
own 20 Mbit/s. Measured over three runs of the shipped schedule, the transitions
of a `rtt100 → loss5 → rate100 → rate20` timeline cost 0.5-7 s each — and the
same timeline under the two-leg scope (`SOAK_SHAPE_LEGS=both`) costs 25-28 s at
its `rate20` boundary. A shorter budget would start the next stage inside a
flush that is still carrying the previous stage's bytes, and its dial is then
closed however often it is retried. The wait is not part of any stage's window,
and the queue half is a *reading*, not an assumption: `tc` renders the backlog
with a unit suffix (`b`, `Kb`, `Mb`, `Gb`), so the drain parses the suffix into
bytes before it decides anything. What the wait cost and what it left behind travel with the stage it
precedes
(`drain_s`, `drain_expired`, `drain_final_backlog`, `drain_busy_sockets`), and
so does the transition the spine itself saw (`spine_sockets` on both throughput
legs, `backend_restart`, `spine_attempts`, `spine_first_interval_s`) — a drain
that ends on its budget leaves the next stage's bulk starting in a state that
is recorded rather than implied.

A drain that ends on its budget is not the end of the story, because a
transition cannot always be made quiet *quickly*: a killed 20-stream client's
teardown retransmits what its kernel holds at the stage's own rate, and on a
20 Mbit path that outlives the next stage's first dial. The spine is
therefore dialed at `SOAK_SPINE_RETRY_S` (seconds into the stage; default
`0,25,50,80`) and stops at the first dial that carries intervals, and a dial
that has carried *nothing* is abandoned at the next offset so a stuck one
cannot eat the retry it exists to leave room for. A healthy stage never notices
— its first dial runs the stage out, and the later offsets cost it nothing —
while a stage that needed a later dial says so: `spine_attempts` counts the
dials and `spine_first_interval_s` says when the bulk actually started. Under
the visitor-leg scope no stage of the shipped schedule needed a retry (three
runs, `spine_attempts: 1` on every stage); the schedule stays as the second
line of defence for a path the drain cannot quiet.

For the same reason the single-test `iperf3` backend is
restarted before every stage's bulk attempt and not only after a failed one: a
teardown that lands on it can leave it answering `Bad file descriptor`, after
which every later dial hangs.

Only the data plane is shaped. The tool's control channel stays on the
unshaped path: shaping it kills the heartbeat and turns a capacity measurement
into a wedge study.

## The SLO

An interactive stream's p99 at or under **50 ms**, with an error rate at or
under **0.5 %**. It is the break condition of the `capacity` test, the dashed
line in every chart, and what the release gate checks on the clean stages.
Degraded stages are *expected* to sit far above it — that is the measurement,
not a failure.

The error term is a rate rather than "zero" because zero is not a property of a
tool here: one run across four arms measured frp 0, rathole 0.05 %, molehill
0.07 % and nps 0.31 % on their clean stages, so an absolute zero flagged the
middle of the spread as a release blocker while two reference arms were worse.
Half a percent is above every arm measured so far and far below anything a user
would notice; a run may tighten it with `SOAK_SLO_ERROR_RATE`.

## Test types

`--test` takes a comma list, because one artifact can carry several types: the
release sweep runs `rrul,capacity`, so the staged schedule and the load ramp
travel in the same file (one `meta`, one host, one revision). They are **two
different instruments** and never cross-check each other: the schedule answers
"what happens as the path changes over time", the ramp answers "how much can it
carry before the SLO breaks". Each test entry in the file carries its own
`test` type, and the gate compares like with like (per tool *and* test type).

| Type | The question it answers |
|---|---|
| `capacity` | how much bulk load can the tool carry while a fresh interactive connection still meets the SLO? (sustainable load + the full response-time curve) |
| `rrul` | under saturation, what happens to a new visitor's latency as the path changes over time? (the figure in the README) |
| `soak` | over a long run on a rotating path: does anything leak, drift or degrade? |
| `cost` | at a fixed operating point, how many CPU-seconds does one carried Gbit/s cost? |
| `screen` | for a development change: is the difference between two builds — or between two configurations of one build — a claim or noise? |

### Two ways to isolate one variable

The stage schedule compares a tool against its peers. The two interleaved modes
compare something against itself, which is what a development decision usually
needs: both arms run inside every load step of one run, so they sample the same
machine state, and sequential before/after runs — defeated by epoch drift — are
never used to decide anything.

- **`--ab BIN_A,BIN_B`** (`screen`) swaps two *builds* at every step.
- **`--ab-variants VAR_A,VAR_B`** (`screen`) swaps two *configuration variants of
  one binary* the same way: one build, two configs. This is how a configuration
  decision — `direct` versus a shared pool, for instance — is measured without a
  build axis riding along. The axis is recorded in the results file
  (`meta.builds.axis`), and the verdict tool prints it, so a reader cannot
  mistake a configuration pair for a build pair.

**Both metrics get their own table and their own verdict** — throughput and
interactive p99 — whenever the run carries both. One metric used to be chosen
for the whole run (throughput whenever any step had it), and that choice can
hide the answer: measured, a screen whose throughput was pure noise while the
p99 favoured one arm on nineteen of its twenty steps, and a screen whose bulk
probe died on every step and flipped to response time without the columns saying
so. A claim is still "every step agrees in sign *and* by at least 15 %", per
metric.

Both are refused where they cannot apply rather than silently ignored: the
variant pair only makes sense for `screen` (the staged types run one
configuration throughout) and a mistyped variant name is rejected instead of
measuring one configuration against itself.

### The slow visitor (`SOAK_SLOW_VISITOR_BPS`)

Off by default. With it set, every stage also runs one slow visitor: a connection
to the tool's echo service that reads the response back at that rate, so the
return path stays backpressured for the whole stage. It exists to make
head-of-line blocking measurable — whether one slow visitor's stream delays the
interactive stream sharing its pool — and the number to read beside it is the
stage's interactive p99, never the visitor's own rate: a paced reader reports the
knob it was given, which makes its series a stall detector (a stage below the
knob is the finding), not a capacity number.

An injected visitor is part of the workload, so a run that carries one records a
different **method version** (`meta.workload_version` 1 without it, 2 with it).
The gate therefore refuses to compare a probe run against a run without one, and
refuses two probe runs at different rates. The probe runs in its own process
like every other probe — the harness never sits in the path it measures — and
its outcome is recorded per stage (`completed` / `failed` / `killed`, with the
typed reason) and never fatal. It is refused for `reconnect`, which measures
cold starts rather than a workload.

### Instrumentation switches

Three environment variables turn on opt-in, aggregated diagnostics. They are
off by default, they never change the forwarding path, and the runner records
whichever ones a run inherited in the results meta (`instrumentation`), so an
instrumented run is never mistaken for a clean one.

| Switch | Emitted | Carries |
|---|---|---|
| `MOLEHILL_MUX_STATS=1` | one INFO line per second per tunnel | cumulative yamux framing counters (`written`, `read`, `bytes`) — frames per second, and with a CPU sample, CPU per frame |
| `MOLEHILL_POOL_STATS=1` | one INFO line per second per live pool | the pool's key, carrier, size, cap, UDP floor, live streams, pinned peers, the per-tunnel `streams/pending/pinned` triple, and the timeline of size changes with the reason for each (`+load:1->2`, `-idle:2->1`) |
| `MOLEHILL_PLACEMENT_STATS=1` | one INFO line per second per process | that interval's placements: how many, how many fell back to another tunnel, the candidate and chosen load sums, `mean_spread` — the average gap in stream slots between the best and the worst candidate at the instant of a placement, i.e. what a smarter rule could have won — and the open latency's mean and maximum |

The pool and placement lines are the S1 observation of the shared elastic pool
(what it does, and why the numbers are aggregated rather than per event:
[internals.md](internals.md#the-tunnel-pool)). Every switch here is INFO like
the rest of the family: turning it on *is* the consent, and a line an operator
has to raise `RUST_LOG` to see never reaches a results file. The server's UDP
line (`MOLEHILL_UDP_STATS=1`) is a fourth switch and carries the affinity
table's size, its evictions and each worker's pinned peers.

### What this model cannot answer

Stated so a reader does not ask a chart for something it never measured:

- **One sample per stage.** A stage's numbers come from one walk of that
  schedule, so a single run cannot state its own repeatability for a class the
  schedule visits once. The schedule visits `clean` twice (the run's own
  replicate, reported by `just soak-check`), and the shaped classes' repeatability
  was measured separately: three runs of one unchanged method moved a shaped p99
  cell by 5-24 % and a shaped bulk cell by 0.2-3 % on this host (HANDOFF.md,
  "Shaped-cell resolution"). A cross-run difference smaller than the class's own
  spread is not resolvable by one pair of runs — that is what the `screen`
  interleave is for, and it is why `soak-check` refuses a difference verdict on a
  shaped stage at all (see Comparability).
- **The control plane under degradation.** Only the data plane is shaped; the
  tool's own control channel stays on the unshaped path, because shaping it
  turns a capacity measurement into a wedge study. Anything the control channel
  does *under* loss (heartbeat survival, reconnection behaviour) is outside
  this model's reach.
- **A tool's own internals.** Everything measured is externally observable, so
  a peer tool and molehill are measured identically — and no internal counter
  of either appears in the comparison. The opt-in `MOLEHILL_*` switches add
  molehill-only diagnostics to a run, and a run that inherits one records it in
  `meta.instrumentation` so it is never mistaken for a clean one.
- **Which configuration a peer number describes.** The published four-tool
  table is molehill's **default configuration** (plain transport, `multiplex`
  mode, one pool per service). The shared elastic pool (`--variants shared`),
  the Noise transport, the KCP carrier and `direct` mode are separate arms, and
  the per-decision measurements below are the only recorded basis for choosing
  between them.

## What each configuration choice costs (per-decision measurements)

The elastic pool's own thresholds — grow at 12 % of a tunnel's stream capacity
(7 of 64), refuse placement at 56, reap a forward that moves nothing for 5
minutes — are internal constants, not settings: their calibration (the
two-stage reproduction that found the growth rule's threshold, and the sweep
records) is in HANDOFF.md, and they change only with a measurement behind
them.

These figures are from the **retired per-cell model** (the v0.8.x method: one
cold-started average per tool per network condition, reported as a median over
repetitions). They are kept because they are still the only measured basis for
a few configuration decisions, and they are **not comparable** with the
workload-over-time figures above — the v0.10.0 run covers the default
configuration only. Treat them as directional, and re-measure your own case.

| Decision | Option | Measured basis (retired per-cell model) |
|---|---|---|
| `mode` | `"multiplex"` (default) | 1-stream 10.0 Gbit/s on loopback vs 19.2 for `direct`; at 8 streams 19.5 vs 23.3; multiplex absorbs per-connection setup (churn ~4.8k connects/s) and saves FDs / ports / NAT mappings |
| `mode` | `"direct"` | raw single-stream throughput; one physical tunnel per stream (FD / port / NAT cost scales with stream count) |
| `count` | `1` | one tunnel for everything: no aggregation and one retransmit domain shared by every stream (loopback 8-stream aggregate 9.2 vs 19.5 Gbit/s at count = 4; loss5 head-of-line max 2.5 s vs 1.6 s) |
| `count` | `4` (default) | aggregates beyond one flow (loss1 8-str 12.3 vs 4.5 Gbit/s) and isolates head-of-line blocking (rtt10 max gap 80.6 vs 100.1 ms at count = 1); yamux ceiling `count × 64` concurrent connections |
| `count` | `8+` | ~512 concurrent connections (8 tunnels × 64 yamux streams); 8 physical tunnels per service (NAT mappings ×8) |
| `carrier` | `"tcp"` (default) | ahead of the KCP carrier in every unflagged measurement (loopback 1-stream 5.8 vs 3.7 Gbit/s against the kcp4 arm on the noise transport), and far cheaper in memory (RSS 26 vs 85 MiB). One 8-stream loopback cell (14.9 vs 1.1 Gbit/s) is excluded here: it was bimodal across repetitions on both builds, so it is not evidence of anything |
| `carrier` | `"kcp"` | only when TCP data tunnels are blocked or throttled, or to A/B a UDP game on a high-latency path: its one measured win is UDP session quality at rtt100 (0 % loss, 20 ms maximum inter-packet gap vs 100+ ms for the TCP arms) |
| transport | `"plain"` | 10.0 / 19.5 Gbit/s (1 / 8 streams) on loopback |
| transport | `"noise"` | 5.8 / 14.9 Gbit/s; sub-millisecond RTT cost; CPU parity under full load |
| `pool_size` | 8 TCP / 2 UDP (defaults) | setup-to-first-byte p99 ~3.5 ms at 16-way churn; UDP shards distinct visitors across channels and never splits one session (session affinity) |
| `[server.data].stripe_count` | `K = 4` (a striped group spreads one visitor connection over K data channels; see [configuration.md](configuration.md)) | a single long-lived connection stops being bounded by one tunnel flow: 1-stream throughput +48.7 % on loopback, at the cost of a reorder buffer, +8.7 % RSS and +40.8 % CPU (per-frame CPU is halved, because the frames spread over four driver tasks) |

Which setting to pick, and why: [configuration.md](configuration.md#choosing-your-configuration-decision-tree).

## Comparability

- **Same model, same method, same host.** Every results file records the method
  version, the host and the harness revision; a number from another host or
  another model is context, not a baseline.
- **The gate compares the method record, not just its version number.** A
  method version is one integer for the whole model, so it cannot notice that
  the stage transition, the drain predicate, the retry schedule or a probe rate
  changed underneath it — five such keys changed during the v0.10.0 cycle while
  the version stayed `1`. `soak_check.METHOD_KEYS` lists the keys that carry the
  method (the schedule, the shaper classes, the load, the SLO, the probe rates
  and the transition settings), and a comparison is refused — naming the keys
  that differ, and the keys a file does not record at all — rather than printed.
  An absent key is not read as a default: it means that file predates the
  instrument, and inventing a value for it would invent a method.
- **Same host — and the same measured state.** The host is recorded three ways:
  `hostname` (what a reader recognises), `host_id` — the machine id plus the CPU
  model and core count, hashed — and `host_calibration`, a fixed, tool-free
  workload the runner measures before every run (SHA-256 over a 192 MiB buffer,
  median of three readings, MiB/s; its own spread travels with it). The path,
  the CPU budget and the loopback ceiling are properties of the *machine*, so a
  containerized bench host changing its hostname on every restart must not break
  comparability — that is what `host_id` is for, and the rename it survived is
  recorded in HANDOFF.md. But an identity key is a *name*, and on a host with no
  readable machine id it reduces to `cpu_model | nproc`: two different machines
  can then hash to the same host. The calibration is the measurement that closes
  that hole — a run is comparable only if the identity matches **and** the fixed
  workload measured within 25 %
  (`soak_check.HOST_CALIBRATION_TOLERANCE_PCT`), which is also what catches one
  machine measured in two different states (busy, throttled, or thermally
  limited). A results file that predates the probe is reported as *unverifiable*
  rather than read as agreement, and the comparison then rests on the identity
  key alone; a file with no `host_id` at all falls back to the recorded
  hostnames, which is conservative in the safe direction (refusing to compare).
- **Every blocking reason is reported, not just the first.** A baseline can
  fail more than one test, and naming only the first would suggest that
  clearing it makes the pair comparable.
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
  it (`bulk_gbps_source`). The rate classes used to be treated as defeated by
  construction, which was true while their sender was unbounded; now that their
  window is bounded (above) they are read from the sender like every other
  class, and the fallback is left for the cells that still need it (`jitter`).
- **A shaped stage's interactive cell is context, not a verdict.** The netem
  queue the harness installed dominates it, and a single run does not repeat it:
  three runs of one unchanged method on this host moved the shaped p99 cells by
  5-24 % (and the two-leg scope's by up to 48 %) against the 25 % limit the gate
  applies to a per-stage p99. So the gate *reports* a shaped stage's number and
  fails only a blow-up (3× or more), and the README marks those columns as
  context instead of picking a winner in them. The measured per-class spread and
  the command that produced it are in HANDOFF.md, "Shaping scope, the rate cells, and the shaped-cell rule".
- **A stage that carried no reading says so, with the reason.** `— (reason)` in
  the plot's table, and `bulk_gbps_source` in the results file, distinguish "the
  path carried nothing" from "nobody could measure what it carried". A bare
  `0` cannot: it is the number a defeated sender reports for a path that was
  full.
- **A repeated stage class is the run's own replicate.** The schedule measures
  `clean` at both ends of every timeline, so those two readings are two samples
  of one condition about an hour apart: `just soak-check` reports their spread,
  and it is the scale every between-tool difference has to clear. It is
  reported, never judged — variance is data, and a threshold on it would be
  invented.
- **Older releases are a different instrument.** Releases up to v0.8.x measured
  one average per tool per network condition, in a cold-started process, and
  reported a median over repetitions. Those tables cannot be compared with these
  figures — an average per cold cell cannot see a wedge, and several of this
  model's findings are wedges. Historical numbers stay in the release notes of
  their own version.
- **Variance is stated, not smoothed.** If a difference sits inside the spread
  of the runs being compared, it is reported as directional and no claim is
  made from it.
- **The per-stage numbers carry their sample count.** A stage's `rtt_p99` is a
  nearest-rank percentile of that stage's interactive samples, so with fewer
  than a hundred samples the "p99" *is* the worst observation — a stage that
  wedged for most of its window (the shaped stages of a saturated run often
  carry tens of samples) reports its worst single measurement, not a tail
  estimate. The charts mark it with the worst-second tick; the results file
  carries `rtt_n` per stage so a reader can tell which kind of number a cell is.
- **Stages are compared by occurrence, not by name.** The schedule opens and
  closes with the same `clean` condition, so the k-th `clean` of one run is
  compared against the k-th `clean` of the other: the return stage — the
  recovery axis — is judged against the baseline's *return*, not against its
  fresh start.
- **The tunnel pool is elastic, so the pool's size is a *result*, not a
  setting.** A build with the shared elastic pool (`[client.data].shared_pool`,
  `[client.data].idle_timeout`, `[client.data.tcp|kcp].max_tunnels`) starts
  **cold** — no tunnel exists until a visitor needs one — and then grows and
  shrinks on its own up to `max_tunnels`. The per-decision `count` figures below
  were measured under the fixed-count model (`count` and `default_count` are
  retired keys; the upgrade table is in
  [configuration.md](configuration.md)) and describe what a *pinned* pool size
  cost. They are the basis for choosing that cap, not a prediction of what a
  run's pool will do — the `MOLEHILL_POOL_STATS` timeline is what records the
  size a run actually used.

## Reproduce it yourself

On a Linux host with `iperf3` and `tc` (see `just bench-deps`):

```bash
just soak-peers    # download the peer tools' latest release binaries
just soak          # one tool (or a batch) through the stage schedule
just soak-plot     # render the charts and print the markdown tables
just soak-check    # verdict: completeness, endpoints, SLO, drift
```

The release sweep is one command — the staged schedule and the load ramp in one
artifact (see [release.md](release.md), "Benchmarks"):

```bash
just soak --test=rrul,capacity --tools molehill,frp,rathole,nps \
     --out benches/scripts/soak/results-soak-vX.Y.Z.json
```

`just soak --help` lists the test types, the variants (`mux`, `shared`,
`direct`, `noise`, `mux1`, `kcp4`, `noise-direct`, and `mux-off`, the historical
spelling of `direct`), the stage schedule and the batching controls; the load,
SLO and sample-rate knobs are environment variables (`SOAK_*`) and every one of
them is echoed into the results meta.

To compare **two of your own builds** — or two configurations of one build —
without a full run:

```bash
just soak --test=screen --path=loss1 --streams-max=8 \
     --ab /path/to/bin-a,/path/to/bin-b --out results-screen.json
just soak --test=screen --path=clean --ab-variants mux,direct \
     --out results-screen-variants.json
just soak-check --screen results-screen.json
```

The verdict claims a difference only when every step agrees in sign and exceeds
the threshold, and calls everything else directional.

The release gate — what a published number must satisfy before a tag can carry
it — is documented in [release.md](release.md).
