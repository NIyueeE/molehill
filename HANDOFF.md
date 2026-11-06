# HANDOFF: the current state, and what to do next

This is the working document for whoever picks this repository up: what the tree
is, what has been measured **on it**, what is still open, and which designs are
decided. It is deliberately short. The research record — every round of
investigation, every superseded conclusion, every negative result with its
reasoning — lives in [`HANDOFF-archive.md`](HANDOFF-archive.md), and nothing in
that file describes the current tree unless this one says so.

Read `AGENTS.md` first for the rules; read this for the state.

---

## 1. Current state

| fact | value |
|---|---|
| branch | `feat/v1.0.0` (never tagged; no release has been cut) |
| version | `Cargo.toml` `1.0.0` |
| wire dialect | **6.0**, unreleased; `SUPPORTED_PROTO_VERSIONS` is 6.0 alone |
| carrier default | `mhd`, with `Carrier::{Tcp, Mhd}` and the `"mhd"` wire tag unchanged |
| gates | `just check`, `just bench gate`, `just interop`, `just l3-accept` (§7 has the evidence and dates) |

The 1.0.0 line exists to make **mhd the carrier a user should want by default**,
not merely one that passes a gate. Priorities are fixed:
**latency > throughput > memory > processor**. Nothing about the configuration
model moves: no key is added, removed or renamed, and no default changes.

**Where the plan lives.** This file is the durable copy of the plan (§4 open
problems, §5 decisions and order). The session's goal objective carries the same
order in one paragraph so that it survives a long context; §5 says how the two are
kept in step. If they ever disagree, this file wins and the objective is corrected.

---

## 2. Decision and invariants

The goal, restated once: a new visitor connection over mhd must cost what a TCP
tunnel costs. Today it does not — see §3 and §4.1.

Binding constraints, all still in force:

- **Configuration is frozen**: no key added, removed or renamed; no default or
  semantic change. `src/config/` may only take wording fixes that match `mhd`.
- **Wire changes go only into dialect 6.0**, which is unreleased; `just interop`
  proves a 6.0 peer and an older dialect refuse each other in both directions.
  No two tags are compatible; a dialect is `major.minor`.
- `Carrier::{Tcp, Mhd}`, the `"mhd"` wire tag, the `unsafe` rules, the permission
  boundary, TCP fallback, the L3/UDP forwarding semantics, the log budget
  (`tests/log_budget_test.rs`) and the serial test convention are all fixed.
- **Never create or push a release tag.** Only a human asks for that; a release
  record is written only when every gate in §7 passes.
- A change that does not beat its own gate is **reverted whole**, with its
  measurement kept. More complexity is not more advanced.

---

## 3. Measured baseline

All numbers below are one run of the `sweep` profile on a freshly built binary,
one round per arm, five arms, 83 samples — `just bench gate` **PASS** (§7). Treat
them as the baseline for any structural change, not as a promise about a
different host.

### 3.1 The clean stages, against the peers (p99, lower is better)

| arm | `timeline[clean]` | `timeline[clean#2]` | what it is |
|---|---|---|---|
| **molehill (mhd)** | **22.505 ms** | **23.08 ms** | the subject |
| rathole | 2.063 ms | — | TCP tunnel, same model, upstream lineage |
| frp | 2.377 ms | — | TCP tunnel |
| l3 | 3.475 ms | — | userspace L3 tunnel, same repository |
| nps | 101.388 ms | — | TCP tunnel (also fails capacity, §3.3) |

molehill's own row: **7.57 Gbit/s, 0.0 % errors** on `clean`; `clean#2` 23.08 ms
with the same zero errors. Both stages are inside the 50 ms SLO **on their own**
— neither is averaged, and no warm-up stage was added.

The gap this document exists for: a TCP tunnel opens a visitor in **2–3.5 ms**;
mhd takes **22.5 ms**. Roughly ten times, on the cheapest path there is.

### 3.2 The shaped stages (molehill, p99)

| stage | p99 |
|---|---|
| `rtt100` | 402.9 ms |
| `loss1` | 1058.1 ms |
| `loss5` | 1464.3 ms |
| `rate100` | 103.1 ms |
| `rate20` | 184.0 ms |
| `jitter` | 109.9 ms |

Each stage is judged alone. `loss5` and `rate20` are the two the earlier cycle
spent longest on; both are far from the clean floor and neither is improvable by
touching that floor.

### 3.3 Capacity

`molehill sustained 8 of 8 streams`; L1–L8 read 3.1–24.5 ms and the ramp reaches
eight. The same run gives l3, frp and rathole 8 of 8, and **nps 0 of 8** — the
peer fails its own capacity, which is worth knowing before quoting it as a
comparison.

### 3.4 What has *not* been measured on this tree

- **bulk-n against a baseline.** The gate only judges the regression half when it
  is given `--baseline`; that has not been run on this tree. The objective's
  "within 15 %" is therefore **unproven**, not met.
- **`churn-16` and `rr-16` across the reconnect/reuse paths** — the two scenarios
  that would show what a reused connection costs, as opposed to a new one.
- **`MOLEHILL_DATA_CARRIER=tcp` as a matched A/B arm.** The peer arms above are
  TCP tunnels from other projects; the in-tree TCP carrier under the *same*
  binary, same method, same round has not been run.

---

## 4. Open problems

Each is written as symptom → what is unknown → the next experiment → the
pre-registered pass and rollback. **The pass and rollback criteria are fixed
before the experiment runs**; a run that changes them afterwards is not evidence.

### 4.1 A visitor pays for a Noise handshake that TCP does not

- **Symptom.** Clean p99 22.5 ms against 2–3.5 ms for the TCP peers (§3.1), and
  the earlier cycle measured `open_us` p90 45.8 ms / max 101.5 ms on individual
  opens — a per-stream tail, not a throughput problem.
- **Where it is, measured (A0 done).** The client already splits an open into
  `started_us` (spawn) / `open_us` (the carrier's stream open) / `ready_us`
  (prologue) / `cmd_us` (command), and the carrier splits the open again into
  `hs_us` (handshake) and `ack_us` (hello acknowledgement). In the failing
  samples **all of the wait is `hs_us`**:

  ```
  started_us=2..16   open_us=8005..21924   ready_us=open_us+2..4   cmd_us=ready_us+3..17
  mhd slow stream open: lane=0 us=21924 hs_us=21820 ack_us=0
  ```

  So the whole clean tail is the **per-visitor Noise handshake**, and a stream
  open costs 5–22 ms where a loopback round trip costs 0.1.
- **Two hypotheses already refuted, both one-constant A/Bs on the `dev` profile**
  (`churn-16`, A/A in the same run, noise ±0.5 ms): the small-flight RTO clamp
  (`MIN_RTO_SMALL_FLIGHT` 20 → 2 ms) and the unmeasured small-flight timer
  (`SMALL_FLIGHT_INITIAL_RTO` 20 → 2 ms). `churn-16` read **10.2 ms** at the
  baseline, **9.95 ms** and **9.5–10.7 ms** after — no change, both reverted. The
  20 ms is therefore **not** a retransmission timer, even though the code contains
  two constants of exactly that value on the same path. (A number matching a
  constant is a reason to look, not a finding.)
- **A proviso on that second A/B.** I rebuilt the binary *while its run was still
  going*, so only that run's first four rounds belong to the 2 ms build (9.720,
  10.472, 7.373, 10.733 ms); the rest measured a mixed binary and are not used.
  The conclusion does not change — the first four are the whole of the evidence —
  but the rule is worth restating: **build, then run, then read; never rebuild
  under a measurement.**
- **Unknown.** Where inside `open_channel_carrying` the time goes: the first
  Noise message's write, the pump's post of it, or the wait for the reply. The
  instrument that answers it is three timestamps inside that call.
- **A0b/A0c, done — and they reframe the question.** Two instruments and three
  runs (`churn-16` and `rr-16`, ~2 000 opens, same-run A/A) say:

  1. an idle write reaches the wire inside five milliseconds — `mhd idle write
     waited for the pump` fired **zero** times;
  2. the server's own path is microseconds — `accept_p50 6 µs`, `post_p50 12 µs`;
  3. in the churn scenario **no open exceeded 5 ms at all** — the
     `mhd open round trip` line never fired.

  So the 21.8 ms `hs_us` that A0 found is **not** the clean path's cost: it is an
  open measured **under contention** (during bulk), which is phase C's problem,
  not the handshake's.

  What the plan's completion condition compares against TCP is the bench's
  `churn-16` cell: `kind="rr"`, sixteen connections each carrying four hundred
  requests, every connection fresh, and the cell is **the round trip of the first
  request on each new connection** — its own claim says so, and that the connect
  itself is `setup_p50_ms` because the server accepts before it pairs. That cell
  reads **10.4 ms** here against 2.1–3.5 ms for the TCP peers.

  The carrier's share of it is now bounded from both ends: the channel open never
  reached 5 ms in these runs (point 3), and the server's accept and post are
  microseconds (point 2). So the residual ~10 ms is in the **forward and echo
  path** — the pair, the backend round trip, the response leg — and that is where
  the next instrument goes. (The probe now states the same connect/echo split on
  its churn line that it already stated for the interactive one; `churn-16` itself
  is an `rr` scenario and does not use that probe.)

- **Unknown.** Whether RACK-style loss detection plus a separate PTO — the split
  MsQuic makes in `core/loss_detection.c` — removes both the age gate and the
  full-flight retransmission storm without reintroducing spurious repairs.
- **Next experiment.** Phase B below.
- **Pass.** A single lost opening recovers at low latency; bulk never re-sends a
  healthy flight; `churn-16`, `rr-16`, both clean stages and `loss1/loss5/rtt100`
  each judged alone; retransmission volume and CPU compared in the same round.
- **Rollback.** Any healthy full-flight retransmission storm, or a first-stage
  throughput drop.

### 4.3 A new stream queues behind bulk

- **Symptom.** `conn.rs` schedules one packet per stream round robin, so a
  44-packet GSO run can belong entirely to mature bulk streams while an opening
  waits its turn.
- **Unknown.** Whether bounded per-class quanta (control/repair/opening,
  new-stream, bulk by byte deficit) move the clean tail without costing single
  stream throughput.
- **Next experiment.** Phase C below.
- **Rollback.** p99 improves while throughput falls; or fairness arguments end up
  starving new streams under an adversary that keeps opening them.

### 4.4 The window only grows, and it grows from a proxy

- **Symptom.** `cwnd` never decreases on loss and slow start never returns; the
  flight total is measuring *received* bytes rather than *delivered* ones, which
  this cycle showed is load-bearing: freeing it on what the peer holds made the
  transfer four to five times worse.
- **Unknown.** Whether a delivery-rate/BDP controller (a simplified BBR: startup,
  drain, probe-BW, app-limited sampling) makes the clean stage stable and the
  shaped stages recoverable, and whether its pacing avoids adding a new
  sub-millisecond wait.
- **Next experiment.** Phase D below.
- **Rollback.** Less stable than today's cwnd on a clean host; keep the recovery
  and scheduler work, revert the controller alone.

### 4.5 The runner's summary prints dashes over real data

- **Symptom.** The sweep summary shows `run: -` and `L1..L8: -` for every arm
  while the results file holds those samples; `just bench gate` reads them fine.
- **Unknown.** Which metric the summary selects per cell — a display bug, not a
  data one.
- **Next experiment.** Fix the summary to name the metric it prints, so a future
  reader cannot mistake "no metric chosen" for "no measurement".

### 4.6 Evidence has been lost to its own clean-up

- **Symptom.** A clean-up glob `m[0-9]*-*` matched the result files as well as the
  scratch directories and deleted `~/tmp/m2*.json`, including the aggregate of the
  gate-passing sweep (§7). The raw per-sample artefacts of that run survive under
  `~/tmp/bench-20261105-230248/`, and the gate output is quoted here.
- **Rule for next time.** Clean up by **directory type** (`bench-<date>-<time>/`,
  `m<round>-<topic>/`), list what will be deleted and count it first, and keep
  result JSONs in a directory that no clean-up pattern covers.

### 4.20 A spare channel per visitor loses everywhere (2026-11-06)

With the pairing measured at 2.0 ms — all of it a channel opened *after* the visitor arrived — the
obvious design answer is to keep one ready. It was implemented behind `MOLEHILL_MHD_SPARE` (one
`DataChannelRequest::Plain` per paired visitor) and measured in the same round, four samples a cell:

| cell | spare on | this session's baseline |
|---|---|---|
| `churn-16` | 10.73–13.18 ms | 10.17–11.88 ms |
| `rr-1` | **0.208–0.210 ms** | 0.151–0.163 ms |
| `rr-16` | **20 095–20 290 1/s** | 23 382–23 754 1/s |

**It loses on every cell**: a third slower on reuse and fourteen percent slower at sixteen
connections, while the cell it was meant to help does not improve. One spare per visitor doubles the
channels the client opens, and opening a channel costs a handshake — so the belt is fed faster than
it is consumed and the cost lands on the data path.

By the rule this objective was started with — a change that does not win is reverted whole, with its
measurement kept — the code is reverted and the numbers stay here. The idea is not refuted in
principle; *one spare per visitor* is. A reserve of a fixed small depth, refilled only when the pool
is empty, is a different experiment and has not been run.

### 4.19 The five milliseconds were the tail of a gated log; the median pairing is 2.0 ms (2026-11-06)

`report_pairing` opens with `if facts.paired_us < 5_000 { return }`. **Every pairing number this
file has quoted since §4.13 is the tail of a threshold-gated sample**, not a median: 907 lines out of six
thousand four hundred pairings in one run — the slow 14 % — and 117 in an earlier one.

Measured on all of them instead, by joining four absolute stamps on the group id:

```
client open                    p50 1.0 ms
open end -> channel delivered  p50 0.0 ms   (mean 0.31, max 3)
deliver -> pairing takes it    p50 0.0 ms   (99 % in the same millisecond)
client start -> paired         p50 2.0 ms
```

**The pairing waits two milliseconds, not five.** The wake after delivery is instantaneous, the
delivery follows the client's open immediately, and the whole of it is one channel open.

So the cell's decomposition is corrected too:

| term | value | shared? |
|---|---|---|
| visitor accepted and paired | **~2.0 ms** (not 5.4) | ours |
| the first request's back-end echo | ~4.8 ms | every arm pays it |
| the request and reply's carrier hops | ~0.4 ms | ours |

and against a TCP peer's 7.2 ms the difference is **~3 ms of per-visitor setup**, of which half is
the open (§4.9) — not the six-millisecond wait this file chased for a dozen rounds.

This is the eighth time in this cycle that a threshold-gated log produced a false number, and the
worst: it cost more rounds than any other error here. The rule already exists in §7 and was
violated by the instrument that prints at five milliseconds. **A line that prints only the slow
cases may never be used to characterise a wait** — the four-stamp join, printed for every group,
is what the same question needed.

### 4.18 It was never a wake-up: the pairing takes somebody else's channel (2026-11-06)

`deliver` was stamped where it hands a channel to the pairing's own queue, and `take_for` where
the pairing takes it from there. Six thousand four hundred of the first, and **not one of the
second**:

```
mhd channel delivered   6 400
mhd channel taken           0
```

**The pairing never takes the channel that was delivered for its group.** Every visitor is served
from the **shared pool**, with a channel that was delivered for an *earlier* group — and its own
channel, arriving later, goes into that pool for the next visitor. The pool is a rotating belt.

So the three and a half milliseconds are not a wake-up at all. They are **the wait for the next
channel to arrive on the belt**, which is one whole open cycle: the previous visitor's channel has
to be produced, and the visitor behind it takes that one.

That is a structural limit, and it is the first one in this file that a *design* answer could
remove: the cell is not slow because any hop is slow, but because **the pool holds nothing and
every visitor queues behind production**. It also finally gives the archive's rejected "static warm
pool" its mechanism — the objection was that a warm pool changes behaviour without a measured
reason, and the reason was invisible until now: the pool is empty at every arrival.

The next step is **A0y**: the shared queue's depth at the moment a pairing looks, to see how often
it is empty (which the 5.4 ms wait predicts is always), and what the belt's period actually is.

### 4.17 The missing 3.4 ms is a wake-up after delivery, on the server's own queue (2026-11-06)

Four stamps per group, all six thousand four hundred matched, and the order they came back in is
the answer:

```
client open               p50  1.0 ms
open-end -> server task   p50 -1.0 ms     (negative: the task starts before the open ends)
server setup              p50  1.0 ms
sum of the three          p50  2.0 ms
```

**The negative gap is correct and it corrects the model**: the server's stream task must run to
answer the handshake, so it starts *before* the client's open returns. From the client starting its
open to the server delivering the channel is therefore **2.0 ms**.

The pairing's own line says the rest:

```
accept_us 17-41 µs   enqueue_us 0   wait_us 5.1-5.5 ms   write_us 2-6 µs
```

and the command crosses to the client in 0.02 ms. So of the 5.4 ms the visitor waits, **2.0 ms is
the channel being opened and delivered and 3.4 ms is the pairing not noticing that it has been**.

That last stretch is a wake-up on this end's own in-process queue: `deliver` puts the channel where
`take_for` will find it, and the pairing task does not resume for three and a half milliseconds.
It is the same shape as the pump's parked writer (§4.11) and the stripe reader's deadline (§4.6),
and it is the only unexplained interval left in this cell.

The next step is **A0x**: stamp `deliver` and the `take_for` that follows it (absolute time, per
group), which turns "a wake-up" from an inference into a measurement — and, if it holds, makes the
pairing's wake the third member of a family this file can then fix once.

### 4.16 Every leg is measured and they do not add up to `wait_us` (2026-11-06)

The last named candidate — the server's spawn of the stream-serving task — is two microseconds:

```
spawn             p50 2 µs      p99 14-20 µs
server setup      p50 971-974 µs (selector, handshake, hello, prologue, delivery)
client open       p50 1 235 µs
carrier hops      ~200 µs        (established groups, §4.12)
control command   p50 20 µs
```

**Their sum is 2.4–2.9 ms. The pairing reports 5.4.** Nothing else is left to instrument on this
path, and a ninth stamp would be measuring one of these again.

So the discrepancy is in a **definition**, not in an interval: either `wait_us` begins somewhere
other than the moment the request is written (`pairing_started` is 22 µs after the visitor was
accepted, which is consistent), or one of the five numbers above is not the interval its name
claims — the likeliest being the client's `open`, which is measured at `open_data_channel`, the
service-level entry, while the lane's own open is measured deeper and the pool's placement between
them has only ever been sampled at a one-millisecond threshold.

The honest state of this cell: **every leg is measured, they are each microseconds to 1.2 ms, and
they do not account for the 5.4 ms a visitor waits.** The next step is to make one interval carry
both ends — a single line per group with the client's open start, the client's open end, the
server's task start and the server's delivery — and check that they are monotone and sum to the
pairing's own number. Until they do, the cell's ten milliseconds are attributable in order of
magnitude but not to the millisecond.

### 4.15 The pairing wait, measured: a two-millisecond control hop and a 1.7 ms open (2026-11-06)

The stamp is on the first request at last, in absolute epoch milliseconds, and it joins with the
client's own arrival stamp for the same group. Six thousand four hundred groups:

```
control hop (server writes the request -> the client's stream delivers it)   p50 2.0 ms  mean 2.03  max 10
server `wait_us` for the same groups                                        p50 5.41 ms mean 5.59  max 9.2
```

**A0w corrected the label on that first term, and the correction matters.** `ask_ms` to the
client's `got_ms` is not a control hop: `got_ms` is a *data* frame on the group's lane, so the
interval already contains the open. Stamped where the client actually begins the open — absolute
epoch milliseconds, with how long the command sat in its queue:

```
command trip (server writes the request -> the client starts the open)   p50 0.0 ms  mean 0.02  max 2
client queueing before it starts                                        p50 0.00 ms
```

**The control session is not slow.** A command crosses it in twenty microseconds.

**The server's own share is smaller than that arithmetic predicted, and the rest has no owner.**
Two histograms inside `serve_mhd_stream` — task start to the hello decoded, and task start to the
channel delivered:

```
hello    p50 500 µs   p99 1 141 µs
deliver  p50 1 040 µs p99 2 320-2 387 µs
```

So the whole server-side setup is **1.04 ms at the median**, split into 0.5 ms of selector,
handshake and hello, and 0.54 ms of prologue and delivery. Against the pairing's 5.4 ms:

| term | measured |
|---|---|
| command across the control session | 0.02 ms |
| the client's channel open (§4.9) | 1.7 ms |
| the server's per-stream setup | 1.04 ms |
| **unaccounted** | **~2.6 ms** |

**The residual is one effect, and this cycle has now measured it from three directions.** The
line that would have said which branch the open took is silent, and its silence is the answer it
was built for: `log_slow_open` requires `reader_ready_at`, which is `None` when the acknowledgement
arrives **inside** the handshake rather than as stream data — so the carrying branch is the one in
use, its `hello_ack` is zero by construction, and the 2.6 ms cannot be a hello round trip.

That leaves one term in the whole cell: **a new stream's first exchange costs two to three
milliseconds, while the same carrier's later data costs 0.2.** The measurement has now hit that
same shape three times without ever naming a mechanism:

- the open's own round trip: 1.7 ms (§4.9) where an idle lane's is 0.1;
- the pairing's residual: 2.6 ms (§4.15) between a client's open and a server's task start;
- and, earlier, the 5–8 ms opens that sent twenty rounds after the handshake (§4.7).

Three independent intervals, all "the first thing on a new stream", all an order of magnitude
above the established path. That is a property of how this carrier starts a stream, not of any one
hop, and it is where the next experiment belongs: **first packet against second packet on the same
stream**, on the data path, with no handshake involved.

The whole cell, with every number now measured rather than inferred:

| term | value | shared? |
|---|---|---|
| visitor accepted and paired — control hop + open + arrival | **~5.4 ms** | ours |
| the first request's back-end echo | **~4.8 ms** | every arm pays it |
| the request and reply's carrier hops | ~0.4 ms | ours |

The next step is **A0w**: split the control hop — when the server's request leaves the control
session's sink against when the client's command loop reads it — so the two milliseconds are
attributed to a queue, a wake-up, or the mux layer above the carrier.

### 4.14 The pairing wait is not a request-and-response at all (2026-11-06)

The last unexplained piece was the five to six milliseconds a visitor waits for a data channel
to exist. The obvious model — the server asks, the client opens — has an instrument: the request
is written by `ask_missing`, and a stamp there joins, by group, with the client's arrival stamp.

**It never fired — and that was the instrument, not the system.** `ask_missing` is only the
pairing's **retry** path; the first request is sent directly, a few lines before the gather loop
(`data_ch_req_tx.send(stripe_request(...))` in a `for index in 0..stripes`). The stamp sat on the
retry and nothing ever retried, so it proved only that the first attempt succeeded.

**So the pairing *is* a request and a response**, as it always looked: request written, the client
opens a channel, the channel arrives, the visitor pairs. The five to six milliseconds contain
that open (1.7 ms, §4.9) plus whatever the request and the arrival cost, and the question the
section asked — how deep the client's inventory is — does not arise, because there is no
inventory: the channel is opened on demand.

The stamp belongs on the first request, in absolute epoch milliseconds so it joins with the
client's stamps for the same group. It was moved there and then reverted: the change kept landing
between `pair_striped_group`'s attribute and its body, and the function is at its line budget. The
next round makes it in one piece, as a helper above the attribute.

So the pairing is not a request and a response. The client keeps a speculative inventory of
data channels — unclaimed, opened ahead of demand — and `take_for(group)` adopts the next one that
arrives. **The five to six milliseconds are that inventory's arrival cadence**, not a round trip
that could be shortened by making a message smaller.

That is the same shape as everything else this cycle has measured about this cell: the visitor
is not waiting on the carrier's work but on *another task's schedule*. It is also the first
attribution the archive's rejected "warm pool" ever had — the objection was made without it, and
the question it raises is now specific and cheap: **how deep is the inventory, and what sets its
cadence?**

The next step is **A0v**: count, per second, how many unclaimed channels are waiting and how often
one arrives with none waiting. That number decides whether a deeper reserve would remove the wait
or merely move it.

### 4.13 Sealing is a net loss, and the cell's own arithmetic closes (2026-11-06)

Sealing was measured properly this time — two rounds, eight samples a cell, the A/A arm inside
the same invocation:

| cell | sealed | unsealed (this session) |
|---|---|---|
| `churn-16` | 10.17–10.43 ms | 10.27–11.88 ms |
| `rr-1` | **0.168–0.175 ms** | **0.151–0.163 ms** |
| `rr-16` | **22 591–22 718 1/s** | **23 382–23 754 1/s** |

So it is neutral on fresh connections (inside their spread) and **costs eight percent on reuse
and four on the sixteen-connection rate** — the per-packet AEAD. With §4.10's finding that it
does not move the cell either, the switch stays **off**: it is faster where nothing needs it and
slower where requests are small.

And the cell's arithmetic now closes, on this build:

```
pairing wait   5.0-6.4 ms   (server: visitor accepted -> a channel exists)
backend echo   4.8 ms       (shared with every arm, TCP tunnels included)
carrier hops   0.5 ms       (0.2 + 0.3, §4.12)
                ---------
               ~10.3 ms     = the cell's 10.2 ms
```

**The two terms that matter are the pairing wait and a backend every tool pays.** The visitor's
own connect is 0.08 ms and the transport is half a millisecond.

So the next question is entirely inside that 5–6 ms: the visitor is accepted, the server asks the
client for a channel, the client opens one (1.7 ms, §4.9), and the channel arrives. One and a
half of those five milliseconds are therefore *not* the open, and no instrument yet says where
they go — the control hop measured 0.1–1.2 ms and the placement under one.

The next step is **A0u**: split the pairing wait on the server into "request written to the
control session" and "channel delivered", against the client's own stamps for the same group —
now joinable, because both ends finally number a group the same way (§4.12).

### 4.12 The five-times disagreement was the join key; joined properly, the carrier's hops are 0.2 ms (2026-11-06)

The previous entry's two ends could not be compared because their group ids were **different
counters**: both ends called `spawn_group` without a tag, so the layer numbered its groups
locally. The protocol already carries a group id — the server names it and sends it to the client
— and the server was already passing it as the tag; the client was not. One line fixed it, and
then both ends' absolute stamps join on the group.

Six thousand four hundred groups, every one matched on both ends:

```
hop1  (request handed to the carrier -> arrives at the far side)   p50 0.0 ms  mean 0.2  max 5
echo  (far side: arrives -> echo read from the backend)            p50 5.0 ms  mean 4.8  max 9
hop2  (echo handed back -> reply arrives at the near side)         p50 0.0 ms  mean 0.3  max 5
```

**The carrier's two hops are a fifth of a millisecond each.** Everything the plan has been
chasing in the transport — the handshake, the timers, the scheduler, the acknowledgement — is
0.2 ms against a five-millisecond backend echo that every arm pays, TCP tunnels included.

The cell's ten milliseconds therefore decompose as: **~4.8 ms of backend echo (shared), ~1.7 ms
of per-visitor open (ours, and removable by sealing as §4.10 measured), ~0.2 + 0.3 ms of carrier
hops, and the rest in the visitor's own connect and pairing.** Against a TCP peer's 7.2 ms the
3.2 ms difference is almost exactly our open plus our pairing — not our transport.

That is the most complete accounting of this cell so far, and it is also the least flattering to
the plan: the transport was never the bottleneck at this cell.

### 4.11 The data path's two ends disagree by five times, and that is the finding (2026-11-06)

The cell is the data path under concurrency (§4.10), so the next step was to split the server's
`wire` leg — the one part it cannot see is the far side's. A fourth histogram was added on the
client, `echo`, spanning the request's arrival from the carrier to the echo being read from the
backend. Both ends, same run:

```
server  wire   p50 1000 µs   p90 6000 µs   (n = 4300)
client  echo   p50 5000 µs   p90 6000 µs   (n = 2147)
```

**These span the same wall-clock interval from opposite ends and differ by five times.** One of
them is not measuring what it says, and until they agree nothing about the data path's
attribution is trustworthy — which is the same lesson as `ack_us` in §4.9 and the round before
it: an instrument that has not been cross-checked against another view of the same interval is
a guess with a number attached.

The obvious candidates on inspection: the server's `sent_ms` is stamped at the **first handoff**
while `got_ms` is stamped at the **first received frame**, and a group's first received frame is
not necessarily the reply to its first handoff — a group can receive the request's own frames
from a client that opened it, and the first round trip of a stripe group is not the first
exchange the group carries. The client's `echo`, meanwhile, starts at the first frame *from* the
carrier and ends at the first read *from the backend*, which is a different pair of events.

The next step is **A0t**: one line per group stating both ends' stamps for the *same* round trip
(group id, `sent`, `got`, `echo`), so the two numbers are defined in terms of the same two
events before either is used again.### 4.10 Sealing works now, removes the 1.7 ms handshake, and barely moves the cell (2026-11-06)

The mechanism phase A needs **already exists**: with `MOLEHILL_MHD_SEAL=1` the first stream's
handshake leaves a lane root behind, every later stream writes one derived selector byte, and
`open_channel_with_id` skips Noise entirely. It was off by default because sealing deadlocked
the striped transfer four runs in eight — and that deadlock's cause (the group's two-second
reorder deadline against an eleven-second carrier stall) was fixed in §4.6.

So it was measured again, and the open's own number collapses:

| phase | unsealed | sealed |
|---|---|---|
| `open_channel` | **1 693–1 743 µs** | **10 µs** |
| `hello_ack` | 0 µs | 129 µs |

**A hundred and seventy times faster on the phase that carries the handshake**, exactly as
§4.9 predicted. And the striped integration test now passes **6 of 6** with sealing on, against
11 of 12 unsealed — the stall that kept this switch off is gone.

**But the cell does not follow.** `churn-16` reads 9.29–10.26 ms sealed against 10.3–12.1
unsealed — about a millisecond, inside its own spread — and `rr-1` gets *worse*: 0.167–0.178 ms
against 0.151–0.163. The reuse cost is the per-packet AEAD that sealing adds to a 64-byte
request.

Two conclusions, and they are worth more than the speedup:

1. **the `churn-16` cell is not the open.** Removing 1.7 ms of handshake from every fresh
   connection leaves the cell where it was, so the seven to ten milliseconds it reports are the
   **data path under sixteen-way concurrency**, which §4.7's leg measurements put on the server
   side of the round trip (`wire` 1–5 ms, `up` and `down` zero, the client's legs zero).
2. **sealing stays off by default for now**, not because it is unsafe — it is not, any more —
   but because it buys nothing on the cell and costs thirteen percent on reuse. It becomes the
   right default the moment the data path is fixed, or if the target is open latency itself.

### 4.9 Lost wake-ups are excluded, and the shape is "two waits where the peer has one" (2026-11-06)

The tail's most plausible mechanism left was a **lost pump wake**: a write signals the
pump, but if that signal does not reach it the packet waits for the timer, and the pause is
the timer's width — exactly a wide tail over a healthy median. A counter on the pump's timer
passes answers it: a pass that begins from the timer **with bytes queued** is a lost wake.

Over a `churn-16` run: **one timer pass in the whole run, and it found nothing to send.**
The pump is woken by its signal every time. Excluded.

With that, the whole set of exclusions for the fresh-connection tail is:

| candidate | how it was excluded |
|---|---|
| the pool growing | never grows during the scenario (§4.7) |
| the connection lock | 2 µs at p99 (§4.7) |
| the open's own preface | 1 µs (§4.7) |
| the driver hop | mhd implements `PoolLane` inline; the hop is yamux's (§4.7) |
| worker threads | 16 workers read the same as 8 (§4.7) |
| the carrier's own round trip | 0.1 ms on an idle lane (§4.7) |
| a lost wake | one timer pass per run, finding nothing (§4.9) |

**Then the corrected instrument changed the picture once more, and for the better.**
`ack_at` was only ever assigned in the plain branch, so for a *carrying* open it still
held the function's entry stamp — and `ack_us` therefore measured the whole handshake.
That is why the two phases read identical. Stamped in both branches:

```
preface       p50 1 µs
open_channel  p50 1693–1743 µs
hello_ack     p50 0 µs
```

So an open is **one wait, not two**: `open_channel` — the Noise handshake, with the
hello and its acknowledgement carried inside it, which is the shape that was
already optimised. `hello_ack` is zero because the reply arrives with the
handshake, exactly as designed.

The interesting number is the ratio: **the carrier's own round trip on an idle lane
is 0.1 ms, and the handshake's is 1.7 ms.** Sixteen times, on loopback, with every
hop measured in microseconds and both wake-ups accounted for. The difference is the
per-visitor handshake itself — two X25519 exchanges and the two task wake-ups that
carry them — and that is precisely what phase **A** (lane-root keys, one handshake
per lane instead of per visitor) removes.

So the A0 series ends where the plan began, but now with a number instead of an
assumption: **1.7 ms of every fresh connection is a handshake that a lane root
would not need to run at all.**

### 4.8 Bulk has no published baseline, and is not behind where it was measured (2026-11-06)

The objective asks that `bulk-n` stay within fifteen percent of "the baseline". There is no such
baseline in the record: `benches/records/results-bench-v0.11.2.json` holds **only timeline and
capacity cells** — the `sweep` profile does not include bulk at all — and its method fingerprint
(`257343fa3fd158d3`) differs from the current one (`1421e66cb2be72f0`), because this cycle changed
the bench probes. So the criterion has never had a number to be measured against, and a
comparison against that record would be refused by the gate's own comparability rule.

What can be measured is a same-round comparison, and it says the opposite of "behind":

| cell | molehill | frp (a TCP tunnel) |
|---|---|---|
| `bulk-1` | 5.35 / 5.37 Gbit/s (one A/A sample read **1.75**) | 4.58 / 4.78 |
| `bulk-n`, eight streams | 6.55 / 6.69 / 9.18 / **10.73** | 6.14 / 6.01 |
| `bulk-pair` | 8.47 / 8.32 / 5.57 | 5.96 / 5.90 |

The medians are at or above the TCP tunnel's; the **spread is several times as wide** — the same
shape as the latency cells, where the median is at parity and the tail is ours.

This run becomes the bulk baseline for this tree: `~/tmp/baseline-dev-bulk-111.json`, dev profile,
two arms, A/A arm included. A future release compares against it (or against a freshly measured
one), not against a published record that has no bulk cells.

### 4.7 The clean cell, measured end to end (A0i–A0k)

One connection's path, from three histogram instruments on both ends of a group
(1 400–2 200 groups a second, `churn-16`, stats switch on):

| leg | what it spans | p50 | p90 |
|---|---|---|---|
| server `up` | visitor read → frame handed to the carrier | 0 µs | 0 µs |
| server `wire` | handoff → the reply frame arrives | **1 ms** (5 ms in another run) | 6 ms |
| server `down` | reply frame → written to the visitor | 0 µs | 0 µs |
| client `from_src` / `to_dest` | the far side's two hops | 0 µs | 0 µs |
| setup (A0h) | pool placement + the lane's open | 1.4 ms | — |

So neither end is slow in its own two hops, and the whole cost is the trip in
between — the carrier's posts, the client's stripe layer, the backend's echo, and
back — at four to six task hops of roughly a millisecond each.

**The cell is not that round trip.** `churn-16` is sixteen connections each making
four hundred *fresh* requests; its run took 2.87 s, so 7.2 ms per request per
connection, and the cell prints 10.4 ms. Against the 2.4 ms an uncontended
connection costs, that is a **three-to-four-fold penalty from sixteen-way
concurrency** — and the TCP peers answer the same cell in 2.1–3.5 ms, about their
uncontended cost.

**A0l priced it, and it corrected the paragraph above.** Same round, same binary,
with the A/A arm in it:

| cell | molehill | frp (a TCP peer) |
|---|---|---|
| `rr-1` — one connection, reused | **0.155 ms** | 0.162 ms |
| `rr-16` — sixteen connections, reused | 23 382–23 754 1/s | 25 552–25 714 1/s |
| `churn-16` — sixteen connections, **fresh** | **10.40–12.12 ms** | **7.20–7.35 ms** |

Two corrections follow. First, **on reuse we are at parity**: 0.155 against
0.162 ms, and 8 % behind on the sixteen-connection rate. Second, the gap on fresh
connections is **1.44×, not the three-to-four times claimed above** — that claim
compared our `churn-16` against the peers' *timeline* clean stage, which is a
different cell. Against the same cell, the peer pays 7.2 ms where we pay 10.4.

So both tunnels pay a large contention penalty for fresh connections — roughly
four times their own uncontended cost — and ours is 44 % worse than a plain TCP
tunnel's. Our own setup (1.4 ms, A0h) is a large part of that difference; the rest
is the extra hop the carrier adds per connection.

**A0m is done, and it moves the target to the tail.** The same run, per-metric:

| `churn-16` | molehill | frp |
|---|---|---|
| `rtt_p50_ms` | **6.89 / 7.32** | **6.54 / 6.55** |
| `rtt_p99_ms` | **10.40 / 12.12** | **7.20 / 7.35** |
| `setup_p50_ms` | 0.085 / 0.093 | 0.075 / 0.076 |

So the **median is at parity** — within five percent of a plain TCP tunnel — and the
`1.44×` above is entirely a **p99** difference. The connect to the tunnel, which
neither tool can avoid, is eighty microseconds for both.

The distribution is also ten times as variable: our A/A pair reads 10.40 and 12.12
for p99 against the peer's 7.20 and 7.35.

**So what remains is the tail of a fresh connection, not its typical cost** — and
that is exactly what the objective asks for ("new-connection p99 close to TCP's").
A tail that is wide only when sixteen visitors arrive at once points at
something that happens *once per burst* rather than per connection: the pool
growing (dialling a tunnel when the burst exceeds what it holds), a lane being
created, or the first group's stripe plan.

**A0n is done, and the tail is the channel open's own.** Two instruments:

1. **the pool never grew** during a `churn-16` run — not once, so growth is not
   the tail (the config sizes it up front and sixteen visitors stay inside it);
2. **every** channel open now goes into a histogram, not only the slow ones:

```
open_p50 = 1.23–1.26 ms      open_p99 = 4.23–4.56 ms
```

The median matches A0h's 1.4 ms, and the **p99 is 4.2–4.6 ms** — a 3.4 ms tail
against a 1.2 ms body. The cell's gap to the TCP peer is 3.2–4.7 ms. **The gap
is the open's tail.**

And the tail is not where the obvious suspects are: the pool's reservation never
reached a millisecond and the lane's own open never reached one either, in the same
run. So the 4.2 ms at p99 is inside the stretch between them — the stripe
placement's bookkeeping — which has only ever been measured by a threshold line
that read microseconds in a different run.

**A0o is done, and it puts the whole open in one call.** The four steps as
histograms, 2 200 opens a second:

```
entry  p50 0 µs      p99 0
used   p50 0 µs      p99 0
open   p50 1192 µs   p99 4270 µs
record p50 0 µs      p99 0
```

`open` is `tunnel.open_stream_on_distinct(...)` and it holds both the median and
the tail; the placement map and the bookkeeping are zero at every percentile.

Two more numbers pin it down. The pool's reservation (`place_us`) never reached a
millisecond even at its p99 — because it is taken **before** `complete().await` —
and `complete`'s own measurement, which this repository already had, reads
**mean 1.4 ms, max 6.9 ms** with `MOLEHILL_PLACEMENT_STATS=1`. The lane's internal
open (`hs_us`) stays under a millisecond throughout.

So the cost is **inside `complete()`'s `tunnel.open_stream()`**, between the pool's
reservation and the pump's own open, and that is the last stretch of a visitor's
setup that has never been split.

**A0p closes the chain without needing another instrument: the open is a task
hop.** `ClientTunnel::open_stream` does not open anything itself:

```rust
let (tx, rx) = oneshot::channel();
self.open_tx.send(tx).await?;   // hand the request to the driver task
rx.await?                        // wait for it to hand a stream back
```

So a fresh connection's channel open is **a request to a driver task and its
reply** — two wake-ups — while the carrier's own open inside that driver
(`hs_us`) stays under a millisecond, and the pool's reservation, the placement map
and the stripe bookkeeping are all microseconds.

That is the answer to the whole A0 series, and it explains the shape of every
number this cycle: the median of a fresh connection costs 1.4 ms because two task
hops cost about that under load, and the p99 costs 4–7 ms because a driver serving
sixteen concurrent opens takes that long to get to some of them. It is not the
handshake, not the timer, not the pool, and not the acknowledgement — each of those
was measured and excluded.

**A0q corrects that in two ways, and finds an instrument bug on the way.**

1. **mhd does not take the driver hop.** `MhdLane` implements `PoolLane` itself
   (`pump.rs:1040`), so `complete()` calls it inline; the request/reply through
   `ClientTunnel` is the **yamux** path only. The paragraph above describes the
   other carrier.
2. **`ack_us` has always read zero, by construction.** The stamp was taken
   immediately beside the measurement:

   ```rust
   let ack_at = Instant::now();
   let ack_us = ack_at.elapsed();   // zero, every time
   ```

   so the hello's round trip was never measured, and every `ack_us=0` in this
   file's history — including one read as "the acknowledgement is free" — meant
   nothing. The stamp is now taken after the hello is flushed and before the
   answer is read.

With it fixed, both phases go into histograms, and the open turns out to be
**two serial waits of the same size**:

```
open_channel  p50 1235–1246 µs   p99 4.3–4.9 ms
hello_ack     p50 1235–1244 µs   p99 4.3–4.7 ms
```

Two independent ~1.2 ms waits inside one open, where the carrier's own round trip
on an idle lane is a tenth of a millisecond. That is what a fresh connection pays
twice, and it is why its median sits at 1.4 ms above the peer's.

**A0r narrows it once more and then stops at a wall of equal numbers.** Five
phases now, and two of them are exonerated:

```
preface       p50 1 µs      p99 5 µs     (hello serialisation, closed check)
lock_wait     p50 2 µs      p99 3 µs     (the connection lock, for `open()`)
open_channel  p50 1227 µs   p99 4363 µs
hello_ack     p50 1226 µs   p99 4362 µs
channel_call  p50 22.8 ms   (one sample: the first open on a fresh lane)
```

`open_channel_with_id` is synchronous, its preface is a microsecond, its lock is
two — and `open_channel` still reads 1.2 ms. `hello_ack` is a second, independent
1.2 ms, which is also what an idle lane's round trip would not cost.

**Two unrelated waits landing on the same number is the clue**: that is the shape
of two "let the runtime run something else" hops, not of two pieces of work. The
measurement cannot separate them further with stamps on the caller's side, because
both waits are inside the callee and both are dominated by time this task is simply
not running.

**4.8 is a separate front and it closed too**: bulk has no published baseline
(the record holds only timeline and capacity), and where it was measured it is at
or above a TCP tunnel's median with a much wider spread. That baseline is now
stored as `~/tmp/baseline-dev-bulk-111.json`.

**A0s rules out the runtime as well.** The binary runs a multi-threaded runtime
with `min(cores, 8)` workers, and sixteen visitors against four to eight workers is
exactly where queueing would live. Raising it to sixteen:

```
churn-16  10.28–10.39 ms   (8 workers: 10.32–10.50)
rr-1       0.151–0.161 ms  (8 workers: 0.153–0.158)
```

**Identical.** So it is not thread starvation, not the connection lock (2 µs), not
the open's preface (1 µs), not pool growth (never happens), not the driver hop (mhd
does not take it), and not the carrier's own round trip (0.1 ms idle).

What is left is the honest state of this investigation: **a fresh connection pays
two waits of about 1.2 ms each, both inside calls this task makes, and every
internal stamp says those calls are microseconds.** Either the wait is time this
task is not scheduled — which the worker experiment argues against — or the
instrument's own clock reads are not where the cost is.

Before spending more rounds on it, the cheap experiment is a **clock sanity check**:
time a `sleep(0)`-free no-op loop and a second `Instant::now()` pair inside the same
function during the churn, to confirm the timestamps themselves behave.

The alternative was taken in the same round: §4.8 measured `bulk-n` against a TCP
tunnel in one round — parity or better at the median, several times the spread — and
found that the objective's "within fifteen percent of the baseline" has **no
published baseline to compare with**, because the record has no bulk cells.
Closing that gap is a documentation-and-ritual question, not a carrier one, and it
is now recorded with a stored baseline to compare against next time.

---

## 5. Active architecture decisions

Approved and ordered; **none of A–D is implemented yet**. Each is a separate
commit with its own A/B and its own revert.

**A. Lane-root key derivation** (removes the per-visitor handshake). One Noise
session per lane at establishment; every mhd stream derives its own key and nonce
from `(lane handshake hash, direction, stream id)`. No second record layer, no
yamux on top, no per-stream handshake. Keys and nonces are never reused across
streams or directions; a rebuilt lane gets a fresh root and rejects replays.

**B. RACK + PTO** (splits recovery from congestion). A RACK deadline from
packets with later acknowledgement evidence, a PTO deadline from the newest
ack-eliciting packet with variance and backoff, and a dead-peer decision from
actual silence. Delete the global age gate; keep a bounded probe exemption for
openings.

**C. Priority scheduling** (openings do not queue behind bulk). Bounded quanta
per class, byte-deficit round robin inside bulk, and GSO only over packets
already decided.

**D. Delivery-rate controller** (a window that can shrink). Simplified BBR with
app-limited sampling; loss is repaired, not treated as proof of capacity loss.

Memory and CPU work is **phase 6**: it may not preempt A–D, and it is judged on
`rss_peak_mib` and `cpu_s_per_gbit` against a same-round A/A.

**Order, as of this commit.** §4.1 carries the findings; this is the sequence.

The clean cell is **1.4 ms of setup plus a 5 ms round trip**, and A0j puts that
whole round trip in one leg (`wire`), with this end's own read-and-hand-off and its
write-back both at zero. So what remains is a handful of task hops through the
client and the backend, about a millisecond each.

**A0l is done** (§4.7): on reuse we are at parity with a TCP peer (0.155 against
0.162 ms; 8 % behind on the sixteen-connection rate), and on sixteen *fresh*
connections we are 1.44× slower (10.4–12.1 against 7.2–7.3 ms).

**A0m is done** (§4.7): the median is at parity with a TCP tunnel — 6.9–7.3
against 6.5–6.6 ms — and the whole difference is the **p99**, 10.4–12.1 against
7.2–7.4. The tail, not the typical case, is what is left.

**A0n is done** (§4.7): the pool never grew during `churn-16`, and every channel
open is now histogrammed — **p50 1.23–1.26 ms, p99 4.23–4.56 ms**. The cell's gap
to the TCP peer is 3.2–4.7 ms, so the gap *is* the open's tail, and neither the
pool's reservation nor the lane's open reaches a millisecond in the same run.

**A0o is done** (§4.7): the four placement steps are histograms now, and `open`
alone carries both the median (1.19 ms) and the tail (p99 4.27 ms). The pool's
reservation never reaches a millisecond; `complete`'s own line reads mean 1.4 ms
and max 6.9 ms; the lane's internal open stays under one. So the cost is inside
`complete()`'s `tunnel.open_stream()`, the last unsplit stretch.

**A0p closes the chain** (§4.7): `ClientTunnel::open_stream` is a request to a
driver task and a reply — two wake-ups — and everything inside it (the carrier's
own open, the pool's reservation, the placement map) is microseconds. So an open's
1.4 ms median and 4.3 ms p99 are **task-hop latency**, which is phase **C**'s
ground.

**A0q corrected that and found an instrument bug** (§4.7): mhd implements
`PoolLane` inline (`pump.rs:1040`), so the driver hop is the *other* carrier's path;
and `ack_us` had always read zero because its stamp sat beside its own measurement.
Fixed, the two phases are histograms and the open is **two serial waits of ~1.2 ms
each** (p99 4.3–4.9 ms), where the carrier's idle round trip is 0.1 ms.

**A0r** is next: a wake-up stamp either side of each wait, to say what it waits for.

Then, in order: **A** (lane-root keys) → **B** (RACK + PTO) → **C** (priority
scheduling, which is where the hop latency belongs) → **D** (delivery-rate
controller) → **phase 6** (memory and CPU) → **phase 7** (full verification, then
the release record).

**Keep this order in two places, and update both together.** `HANDOFF.md` is the
durable copy; the session's goal objective is the copy that is re-read every round,
and a stale objective re-asserts a plan that has already been refuted. When a
result changes the order — a step finished, a hypothesis refused, a phase
promoted — the same commit updates this section *and* the objective.

**Reading list for B–D.** Four implementations are checked out under
`~/tmp/mhd-references/`; each is cited where the plan uses it:

| project | file | what to take |
|---|---|---|
| msquic | `src/core/loss_detection.c` | the RACK / PTO split, and what each is allowed to declare lost |
| msquic | `src/core/bbr.c` | delivery-rate filter, app-limited sampling |
| quic-go | `internal/ackhandler/received_packet_tracker.go` | which packets force an immediate acknowledgement |
| quinn | `quinn-proto/src/connection/pacing.rs` | token-bucket pacing with a bounded burst: capacity is `window × 2 ms / rtt`, clamped to 10 ms of traffic — the shape §5.D needs |
| quiche | `quiche/src/ack.rs` | range-based acknowledgement instead of a fixed-width bitmap |

---

## 6. Negative results: do not retry these blind

Every row was measured on this repository and reverted. The reasoning lives in
`HANDOFF-archive.md`; this is the index a future experiment should check before
spending a day.

| # | attempt | measured result |
|---|---|---|
| 1 | Remove the pinned cumulative watermark (make number zero arrive) | mechanism fixed and *proved* (watermark advances to thousands); **3/12** integration runs against a 13/24 baseline — worse |
| 2 | Repair the frontier range on its own clock | 4/12; gated to the closed-window state: **0/12**, the worst result recorded |
| 3 | Widen the acknowledgement bitmap 64 → 512 numbers | 5/12, plus 56 bytes on every packet — reverted |
| 4 | Separate "held" from "delivered" so the peer can free flight | **1/12**; the flight total is load-bearing backpressure |
| 5 | Declare stale records lost on the acknowledgement path | neutral (7/12); the premise was wrong — in a deep flight every record is "old" |
| 6 | Post a lane's first data packet twice | with the condition fixed, 3/12 |
| 7 | `SEND_RUN` 8 or 96 instead of 44 | 5 530 and 7 853 slow opens, stage medians 2.5 → 13 ms |
| 8 | `AckSet::AHEAD` 16 384 | 4/8 against 5/8 |
| 9 | Round-robin or laggard-named byte prefix | 2/8 and 6/12 against 5/8 and 7/12 |
| 10 | Decoupled echo in the test harness | 6/12 — the harness was never the cause |
| 11 | Per-visitor Noise record layer, mux-over-mhd, more lanes / window / threads, static warm pool | rejected earlier; reasoning in the archive |
| 12 | `MIN_RTO_SMALL_FLIGHT` 20 → 2 ms | `churn-16` 10.2 → 9.95 ms, inside the ±0.5 ms A/A noise — the 20 ms open is not this clamp |
| 13 | `SMALL_FLIGHT_INITIAL_RTO` 20 → 2 ms | `churn-16` 10.2 → 9.5–10.7 ms, no change — not the unmeasured small-flight timer either |
| 14 | One spare data channel per visitor (`MOLEHILL_MHD_SPARE`) | loses on every cell — reuse −30 %, `rr-16` −14 %, `churn-16` unchanged; reverted (§4.20) |

One row is **not** in this table because it worked: the striped group's reorder
deadline was **2 s**, shorter than an 11.9 s carrier stall, so the reader failed
the group before the transport could finish. Thirty seconds gives **11/12**
integration runs against 13/24. That is the only structural change this cycle
that moved a number, and it moved the *bulk* stall, not the clean tail.

---

## 7. Environment and reproduction

**Tools and entry points**

- `just check` — the full chain (fmt, machete, ruff, pyright, bench selfcheck,
  clippy twice, serial tests, docs, secrets).
- `just bench doctor` — **run this first after any container restart.** The
  release binary survives in `target/` while source mtimes do not, so a stale
  build silently measures the previous tree: one `cargo build --release` turned a
  91.98 ms "failure" into a 22.50 ms pass.
- `sudo -n env MOLEHILL_MHD_STATS=1 just bench run --profile sweep --out ~/tmp/<name>.json --force`
  — the full sweep, five arms, roughly two hours at scale 1. `just bench gate
  <file>` judges it; add `--baseline <record>` for the regression half.
- `just interop`, `just l3-accept` — dialect refusal and the L3 acceptance run.

**Gate evidence on this tree**

| gate | result |
|---|---|
| `just bench gate ~/tmp/m24-sweep.json` | **PASS** — coverage 83 triples, endpoint 57 samples, SLO 4 clean cells, drowned 0, drift checked, capacity 8/8 (molehill, l3, frp, rathole) and 0/8 (nps). Revision `v0.11.2-592-gff0bae7`, fingerprint `1421e66cb2be72f0`. **That aggregate file was deleted by the §4.6 clean-up**; the run's raw per-sample artefacts survive under `~/tmp/bench-20261105-230248/` (48 series files, 146 raw iperf3 artefacts), and the gate output above is the one this document quotes. |
| `just check` | **green** at `83eb22e` (0 errors; the serial suite, both clippy passes, audit, deny, outdated, ruff, pyright and the bench selfcheck's 708 checks all pass) |
| `just interop` | **3 of 3** at `83eb22e` — new server refuses an old client and keeps serving, old server refuses a new client and says so, old server rejects an unknown dialect and survives |
| `just l3-accept` | **16 PASS, 0 FAIL** at `83eb22e` (run as root: the recipe skips with exit 77 without namespaces; `sudo -n bash bench/src/bench/utils/l3/run.sh`) |

All three were re-run on this tree rather than carried over from an ancestor: this cycle changed
the stripe layer, the stats instruments and the server's stream setup, and a gate result from
before those changes would have described a different binary.

**Host and harness traps** (each cost real time this cycle)

- **Fixed ports.** The integration test binds 2333/2334. A killed test leaves its
  binary alive holding them, and every later run then fails at startup with
  "connection refused", which reads like a product failure. Check
  `ss -ltn | grep :2333` and `pgrep -af 'deps/integration_test-'` before a batch.
- **`pkill` self-match.** A pattern that appears in the loop's own command line
  kills the loop. Match the child only (`target/debug/deps/integration_test-<hash>`).
- **Network namespaces.** A bench that dies leaves `mhbvis`/`mhbsrv`/`mhbcli`
  behind; delete them before the next run, and never delete one while a bench is
  still running — that is how one sweep was destroyed.
- **Container restarts.** The container is recreated between sessions: `/tmp` is
  wiped, load averages reset, and a sweep running under `/tmp` dies with it.
  Artifacts and evidence go to `~/tmp`.
- **Measurement discipline.** A twelve-run binary comparison resolves about 45
  points; eight runs resolve nothing. Per-round `A/A` gives the noise floor, and
  a difference inside it is not a result. Two of this cycle's three "losses" were
  population mistakes (window versus lifetime counters, one direction subtracted
  from its opposite) — name the convention in the field name, as the window line
  now does (`*_per_s` versus `*_total`).

---

## 8. Archive index

`HANDOFF-archive.md` holds the research record. Enter it by topic, not by date.

| topic | what is there |
|---|---|
| The transparent-L3 cycle | closed records from the L3 work, including the three fixes measurement refused |
| Carrier evolution | the mhd carrier's design history: lanes, pool, striping, sealing, the stride metric |
| Measurement accidents | every instrument that lied: direction subtraction, window-versus-total, stale binary, kbuild-queue, port poisoning |
| Refused experiments | per-experiment reasoning, with the numbers, for every row in §6 |
| The pre-consolidation body | the full 5,460-line `HANDOFF.md` as of `6c50265`, verbatim |

Entry point for a specific question: `grep '^## ' HANDOFF-archive.md`, then read
that section's first paragraphs — each opens with what was asked and what the
measurement answered.
