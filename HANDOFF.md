# HANDOFF: current state and the plan

> **2026-11-03.** This page is the execution document: where the tree stands,
> what is measured, what is open, and what happens next. It is deliberately
> short — the running narrative of every cycle, with its refutations and its
> numbers, is in [HANDOFF-archive.md](HANDOFF-archive.md), which this page
> indexes. Per AGENTS.md §3 this is an agent page: user-facing facts belong on
> the docs pages, shipped behaviour in [CHANGELOG.md](CHANGELOG.md).

## Where the tree stands

| | |
|---|---|
| Branch | `feat/v1.0.0` |
| Version | `1.0.0` in `Cargo.toml` **and** `Cargo.lock` |
| Changelog | `## [Unreleased]` empty; one dated `## [1.0.0] - 2026-11-01` |
| `just check` | green end to end (both hook layers) |
| `just interop` | 3 of 3 — 6.0 and v0.11.2 refuse each other on the connection it happens on, and each keeps serving its own dialect |
| Bench peers | frp 0.71.1, rathole 0.5.0, nps 0.26.10, cached |
| `benches/records/results-bench-v1.0.0.json` | **absent on purpose** — no record is committed that its own gate refuses |
| Wire dialect | 6.0, unreleased; its commands may still be extended in place |

**No tag has been created and none will be by an agent.** Publishing is a human
act (AGENTS.md §5); what is missing is a gate that passes every time, and the
numbers that say why are on this page.

## The goal, and what may not change

The objective: **make `mhd` the more reasonable first carrier — better than
TCP where it can be, and honest where it is not.** The trade order is fixed and
binding:

> **latency > throughput > memory > processor**

Ties are broken by that order, not by which number is easier to move.

What may **not** change, under any redesign below:

- **The configuration model and every field's meaning.** No key added, removed,
  renamed, re-defaulted or re-scoped. `src/config/` may change only prose that
  names the carrier (`KCP` → `mhd` in a doc comment or a removal message).
  `diverged`/`REMOVED_KEYS` behaviour stays exactly as it is.
- **`Carrier::{Tcp, Mhd}`, the `"mhd"` wire tag, the TCP fallback path**, and
  the runtime choice between them.
- **No new privilege and no new `unsafe`.** The crate-wide deny and the
  per-module opt-ins of [docs/lint-policy.md](docs/lint-policy.md) hold.
- **Log levels** follow the contract in
  [docs/configuration.md](docs/configuration.md) — `tests/log_budget_test.rs`
  fails a healthy run that emits one `WARN`/`ERROR`.
- **Both languages** for user-facing pages, in the same commit.
- **A number is only a claim** when it clears its metric's materiality floor
  *and* the run's own measured noise (AGENTS.md §10). A mechanism that would
  explain a number is not evidence that it produced it.

## What is measured, on this tree

Everything below was produced by the bench model on this host. Absolute values
describe this machine; the shapes and the orderings travel.

### Warm: a connection that already exists

| cell | mhd | TCP | note |
|---|---|---|---|
| `rr-16` p50 / p99 | **0.46 / 1.32 ms** | 0.44 / 1.34 | at parity |
| `rr-1` p50 / p99 | 0.098 / 0.130 ms | — | |
| `bulk-n` throughput | **10.98–11.49 Gbit/s** | 10.90 | A/A twin 10.88–11.53 |
| `capacity_streams` | **8 of 8** (good window) | 8 | = `streams_max` |

The last clean stage of a 0.2-scale timeline — the warm state — is the best
arrival measurement this cycle has:

| stage | n | p50 | p90 | p99 |
|---|---|---|---|---|
| `clean#2` | 850 | **1.87** | **2.27** | 15.07 |
| TCP, same cell | — | 2.24 | 2.59 | — |

**At the median and the ninetieth percentile the warm carrier beats TCP.**

### Cold: a visitor that has just arrived

`churn-16` is **sixteen concurrent workers, each opening a fresh connection per
request**: 6 400 arrivals in a round, about **four thousand a second**. (The
`churn_per_s: 16` field in the model's parameters belongs to the *timeline's*
churn probe — a different instrument at one arrival every 62 ms. Reading it as
this cell's rate is what withdrew the concurrency explanation for a dozen rounds,
and it was wrong; see the correction below.)

| cell | mhd | TCP |
|---|---|---|
| `churn-16` p50 | 1.95–2.51 ms | 2.24 |
| `churn-16` p99 | **17.97–21.75 ms** | **2.58** |
| `churn-16` rate | 3 500–4 061 /s | 6 728 /s |

**Where an arrival's time goes, in one second of a measured round.** Every stage
is timed behind `MOLEHILL_MHD_STATS`, at one five-millisecond-or-finer scale, as
a per-second window:

| segment | p50 | p99 | max |
|---|---|---|---|
| accepting the visitor (`accept_us`) | 15 µs | — | 133 µs |
| opening a channel: placement + Noise handshake + prologue (`open_us`) | **1.73 ms** | 4.66 ms | 7.6 ms |
| **waiting for a channel to be provided (`wait_us`)** | 0.95 ms | **20.1 ms** | 50.9 ms |
| writing the pairing command on it (`write_us`) | 2 µs | 7 µs | 15 µs |
| the visitor's first byte after pairing (`first_*`) | 13 µs | 117 µs | 575 µs |

**Read the two bold rows together, because they decide the diagnosis.** The
pairing's p99 is more than twice the *worst* open the client performed in the
same second, so the tail is not a slow handshake. And the pairing's *median* is
below the open's median, which is only possible when a visitor is served by a
channel that was already in flight for somebody else.

That is the mechanism: **channels are interchangeable.** The client opens one per
request, hands it to a shared queue, and `pair_striped_group` takes whichever
channel is at the head — so with sixteen opens in flight, a visitor's own channel
can be delivered to another visitor and the waits add up. The control is the same
pairing at sixteen arrivals a second with no bulk beside it: **one pairing in
~480** over five milliseconds, against 471 of ~480 in a scheduled stage's first
`clean`. Bulk is what puts a queue in front of an arrival, and the queue's depth
is what one open costs.

The four explanations this replaces, and why each is now retired: the tail is not
a stall (the multi-second ones are fixed), not the handshake alone (a 4.66 ms p99
cannot make a 20.1 ms one), not the command write (2 µs), and not the forwarding
loop (13 µs after pairing).

The first stage of a timeline is where this lands as an SLO failure. Across the
23 timeline records this cycle produced:

| | first `clean` | last `clean#2` |
|---|---|---|
| p99 median | **179.9 ms** | **6.06 ms** |
| p99 range | 36.3 – 800.9 | 3.2 – 375.7 |
| under the 50 ms SLO | **3 of 23** | **18 of 21** |

and per-stage, in one 0.2-scale run:

| stage | n | p50 | p90 | p99 |
|---|---|---|---|---|
| `clean` (first) | 621 | **12.13** | 51.65 | 103.70 |
| `clean#2` (last) | 850 | **1.87** | 2.27 | 15.07 |

**The same binary, the same stage definition, sixty times apart at the median,
in one run.** That is the defect the gate reports, and it is why a single
"carrier is fast/slow" summary is not a usable statement.

### What the old instruments still say

The pairing diagnostic fires once per successful pairing, at and past 5 ms,
behind `MOLEHILL_MHD_STATS=1`. It is the *conditional* distribution of the slow
subset, not the population's — 25 % of arrivals in a `churn-16` round — so it is
read as "how bad is the slow part", never as a median:

| field | p50 | p90 | p99 | max |
|---|---|---|---|---|
| `accept_us` | 17 µs | 25 µs | 37 µs | 62 µs |
| `paired_us`, on pairings over 5 ms | 8.0 ms | 17.6 ms | 29.1 ms | 57.8 ms |

and per stage at 0.2 scale, out of ~480 visitors, the count of pairings over
5 ms: `clean` **471**, `loss1` 243, `loss5` 4, `clean#2` **1**. The probe's own
rate field confirms the last stage was fully loaded (`churn_per_s` 15.99–16.005
in `clean#2` in all 23 records), so no stage's numbers are an idle artefact.

### What landed this cycle, each with its own measurement

| change | evidence |
|---|---|
| one-flight open (hello and ack inside the handshake) | open 3 313 → **927 µs** idle; `wire_per_visitor_byte` −15 % |
| the flight may not outlive its timer, openings exempt | capacity worst arrival **3 000–5 000 → ≤ 204 ms**; `loss5` tail 1 427 → **862 ms** in an interleaved A/B |
| a lane needs an *opening* datagram | 29 stray lanes → **0**, by a test rather than by a run |
| a stage restores its own condition | a cascade that cost five stages now costs none |
| the reorder loop wakes itself; a parked buffer has a deadline | three rounds in six read p99 7.7–49 ms where none did |
| pairing patience 25 s → 10 s (`PAIR_ATTEMPTS` 2) | no regression on three cells |
| three diagnostics at one 5 ms scale | the tail became visible at all; it had been measured 4× too coarse |
| `Cargo.lock` records 1.0.0 | a release build now leaves the tree clean |

## What is open

Each item states its own gate. A change that misses its gate is reverted, and
the reason is recorded — not tuned around.

### O1. The first `clean` stage fails the SLO (latency, blocking)

`timeline[clean]` p99 is 36–801 ms against a 50 ms bar, in 20 of 23 records,
and the first stage's median is 12.13 ms against the last stage's 1.87 in the
same run. The mechanism is localised (above): a queue of interchangeable channels
in front of an arrival, whose depth is what one open costs.

- **Gate**: both clean stages independently under 50 ms p99 with error rate
  under 0.5 %, in a full-scale run, with the first stage's hold unchanged.
- **Not allowed**: a warm-up round, a shortened first stage, dropping the stage,
  or averaging the two.

### O2. `churn-16` p99 is 8× TCP (latency)

17.97–21.75 ms against 2.58 at ~4 000 arrivals a second from sixteen concurrent
workers. The median is level (1.95–2.51 against 2.24). Same mechanism as O1, on a
cell with no bulk: the pairing's p99 is 20.1 ms while the worst open in the same
second is 7.6 ms, so the waits are adding up across concurrent arrivals rather
than each one being slow.

- **Gate**: within the run's own A/A floor of TCP's 2.58 on an interleaved A/B,
  with `rr-16` and `bulk-n` unmoved.

### O3. Throughput under loss and on a long path (throughput)

`loss1` p99 is one RTO for every tool in the record (molehill 1 076, frp 1 065,
rathole 1 058), so a p99 claim there is a claim about TCP's retransmission, not
about this carrier. `rtt100`'s 1.70 Gbit/s is 16 MiB ÷ 400 ms over four lanes —
the window is the ceiling by arithmetic.

- **Gate**: `bulk-n` within 15 % of its baseline on an interleaved A/B, and no
  shaped stage regressed, when recovery or the window changes.

### O4. The harness loses cells to an intermittent sink failure

A `control socket has closed unexpectedly` costs 0–3 stages per run,
arm-independently, and one `rathole` capacity cell aborted after 7.83 s. This is
not the carrier: it hits every arm, the tool's group outlives the client's own
exit, and a stage's condition is now restored properly.

- **Gate**: a full sweep with no missing cell for any arm. Until then a
  coverage failure is an instrument result, not a product one.

### O5. Constants nothing measures

`MAX_CWND`, `MAX_BUFFERED`, `OPEN_RESERVE`, `SEND_RUN`, `KEEPALIVE`,
`DEAD_AFTER` are unexercised by every measurement this cycle made. The audit
that says so is in the archive ("P4′: the constants audited").

- **Gate**: a constant is changed only with the cell that shows it binding.

## The plan

Ordered, each stage gated on its own measurement, each revertible alone.
Configuration keys do not move in any of them.

### Stage 0 — this page (done here)

Consolidate the handoff; keep the negative results as a table and the narrative
in the archive.

### Stage 1 — attribution (done, 2026-11-03)

The arrival is now attributable to five named segments, each timed behind
`MOLEHILL_MHD_STATS` and reported per second: `accept_us`, `open_us` (placement
plus handshake plus prologue), `wait_us` (the pairing's wait for a channel),
`write_us` (the command on it) and the existing `first_*` (the visitor's first
byte afterwards). The numbers are in "Where an arrival's time goes" above, and
they answer the question Stage 1 was written for: the tail is a queue of
interchangeable channels, not one slow step.

What it took, and what is worth keeping from it:

- Two instruments, both at the same five-millisecond-or-finer scale as the older
  two, plus a per-second window on each end — because a per-event line at a
  threshold shows the tail and never the population, and the population is what
  distinguishes "a few slow opens" from "every open is slow".
- Three instruments had to be re-scaled first: the client's lane open (5 ms), the
  client's open-to-prologue (was 100 ms), the client's task-scheduling delay (was
  100 ms), and both windows (were 10 s, against a 5 s cell). An instrument coarser
  than the thing it measures reports a healthy system — four times in this cycle.
- One earlier conclusion had to be withdrawn *before* it could mislead the plan:
  that `churn-16` runs at sixteen arrivals a second. It runs at about four
  thousand, and the `churn_per_s: 16` field belongs to the timeline's probe.

Measurement discipline from here (unchanged from AGENTS.md §10, restated because
it is what the next stages will be judged by): per-stage numbers, no added
warm-up, the TCP contrast from `MOLEHILL_DATA_CARRIER=tcp` on the same build
family, every optimisation an interleaved `--ab-arm`/`--binary-b` A/B with its
own A/A twin, and artifacts under `target/evidence/` or `~/tmp/`.

### Stage 2 — a channel belongs to the request that asked for it (latency, first)

**This is the change the measurements above ask for, and it is ahead of the
handshake work** because the tail is queueing, not a slow step.

Today a channel is anonymous: the client opens one per request, drops it in a
shared queue, and `pair_striped_group` takes whichever is at the head. So a
visitor's own channel can be delivered to another visitor, and with sixteen opens
in flight the waits add up. Give the pairing an identity:

1. A channel names the request it was opened for. The group id already exists on
   the wire and in both ends' hands; it goes into the tunnel prologue next to the
   service and tunnel ids, which is four bytes and no new key.
2. The server reads it where it already reads the prologue and delivers the
   channel to **that group's waiter**, with the shared queue kept for plain
   visitors. A waiter registers before its request is written, so a channel that
   arrives early is not lost.
3. An abandoned group is released as it is today (`ReleaseGroup`, plus a TTL),
   so a visitor that gives up cannot leak a waiter.

This also removes a latent correctness hole rather than only a latency one: with
`stripes > 1` the current FIFO can hand a channel opened for group A to group B,
which is exactly the "K stripes on K distinct tunnels" invariant the client's
placement works to keep.

- **Gate**: `churn-16` p99 falls toward TCP's 2.58 ms; the pairing's p99 stops
  exceeding the open's p99; `bulk-n` inside 15 %; both clean stages improved; the
  striped integration test still passes with `stripes > 1`.
- **Revert**: no movement beyond the run's floor, or any bulk regression.
- **Tests first**: a channel for group A never satisfies group B; an early
  arrival is held for its waiter; an abandoned group leaks no waiter; a plain
  visitor still takes the shared queue.

### Measured: identity delivery does not move the tail (2026-11-03, `ed6fa76`)

The change the queueing story predicted, built and interleaved against a build
of `78923cb` on `churn-16` (4 rounds, `--aa`, `--ab-arm l4 --binary-b`):

| arm | p50 | p99 | max | rate |
|---|---|---|---|---|
| `l4` (identity delivery) | 2.28 ms | **19.52 ms** | 41.0 | 3 688 /s |
| `l4~aa` (the same build again) | 2.84 | 21.49 | 51.1 | 3 235 /s |
| `l4~b` (before) | 2.57 | 21.74 | 44.3 | 3 489 /s |

**No claim**: the run's own p99 floor is 8.19 %, the point estimate is −10 %,
and the A/A twin sits *between* the two arms. The traffic floor is 12.2 % from
four rounds.

So the exchange-of-channels story is refuted as the cause of the tail, though it
was the right reading of the earlier distributions: the pairing median does sit
below the open median, and with identity delivery it no longer can — yet the
tail is unchanged. **The 15 ms that separates the pairing's p99 from the open's
p99 is somewhere else**, and it is now the only unexplained interval left:

- not accept (15 µs), not the command write (2 µs), not the first byte (13 µs);
- not the open, whose p99 is 4.66 ms;
- and not the exchange, which this A/B removed.

The change is **kept as a correctness fix and as the identity the next
instrument needs**, explicitly not as the fix for the tail — the same disposition
the small-flight timer got earlier. It closes a real hole: with `stripes > 1` the
old FIFO could hand a channel opened for group A to group B, which is exactly the
"K stripes on K distinct tunnels" invariant the client's placement keeps.

### The residual, and three candidates excluded (2026-11-03)

With the group identity in place the two ends can finally be matched **by
identity** rather than by magnitude. Twelve events had both ends' diagnostics in
one round, and they bound the two intervals that were left:

| interval | p50 | p99 | max |
|---|---|---|---|
| control request: server enqueued → client reads it | **132 µs** | 357 µs | 357 µs |
| prologue written → server has paired | **3.0 ms** | 12.2 ms | 12.2 ms |

and the second one is not any of the things inside it, each measured on its own:

| candidate | reading | verdict |
|---|---|---|
| the send-buffer dwell: bytes accepted → a packet carries them | client p50 **5 µs**, p99 498 µs, max 591 µs; server p50 4 µs, p99 25 µs | **excluded** |
| the per-tunnel accept loop reading prologues serially | **0** route lines over 5 ms in two rounds | **excluded** |
| the exchange of channels between visitors | interleaved A/B: p99 19.52 against 21.74, twin 21.49 | **excluded as the cause** |

So every instrument inside the path reads microseconds while the arrival's tail
is twenty milliseconds — the same shape the archive records for the earlier
search: **the time is between the stages, not inside them.** Two intervals are
still unmeasured and both are scheduling rather than work:

1. the client's pump posting a new stream's first packet → the server's listener
   dispatcher handing that stream to the lane's accept loop;
2. the lane's accept loop waking the stream's reader task and that task being
   polled.

The instruments that would separate them are a wake count and a poll-gap on the
accept path — the same pair that was needed for the reader park earlier.

### Every segment measured, and the one interval left (2026-11-03)

Ten instruments, each at five milliseconds or finer and each behind the stats
switch. In one second of a `churn-16` round (~3 600 arrivals):

| segment | p50 | p99 | max |
|---|---|---|---|
| accepting the visitor | 16 µs | 80 µs | 153 µs |
| control request crosses to the client | 132 µs | 357 µs | 357 µs |
| the client's channel open + prologue | 1.70 ms | 4.66 ms | 8.05 ms |
| bytes accepted → a packet carries them (client) | 5 µs | 26 µs | 210 µs |
| waiting for the connection lock (client) | 0 µs | 2 µs | 246 µs |
| a stream's reader park (server) | 21 µs | 752 µs | 2 639 µs |
| a pairing woken → polled | **14 µs** | 52 µs | 254 µs |
| the command write on the channel | 2 µs | 7 µs | 15 µs |
| the visitor's first byte after pairing | 14 µs | 120 µs | 451 µs |
| **the pairing's whole wait** | **0.97 ms** | **18 987 µs** | 42 615 µs |

**Nothing inside the path costs the twenty milliseconds, and the pairing is not
descheduled**: it parks about 37 000 times a second — ten wakes per arrival —
and is polled 14 µs after each one. So a pairing wakes, finds no channel, parks
again, and repeats until one arrives. The channel itself arrives late.

Excluded so far, each by its own measurement: accept, the control trip, the
client's open, the send-buffer dwell, the connection lock, the pairing's own
scheduling, the reader's park, the per-tunnel prologue read, the command write,
the post-pairing forwarding, the worker count (4 vs 8 is inside the floor), and
the exchange of channels between visitors.

**The one interval no instrument covers is on the client: from the control
command being read to the task that opens the channel being spawned.** The
client's `ready_us` starts when that task is first polled, so a command that
waited in the control loop's own queue is invisible — and the loop is a single
task that reads, decodes, places and spawns, at four thousand commands a second.

### The mechanism: a lottery over a shared queue (2026-11-03)

The last interval is now measured too. On the client, from the control command
leaving the loop to the tunnel prologue being written — the whole client-side
path, including the open — is **2.1 ms at the median, 5.3 at p99 and 7.3 at
worst** across a second. Add the control trip (132 µs), the server's accept
(16 µs), its routing, the reader park (0.75 ms at p99) and the command write
(2 µs), and the *entire* path from a visitor's accept to a channel sitting in the
queue is **under eight milliseconds even at its worst**.

The pairing it feeds reports 23 ms at p99. **The wait is larger than everything
in the path it is waiting for**, which leaves one explanation, and the counters
say so directly: a pairing parks ~37 000 times a second against ~3 600 arrivals —
**ten wakes per arrival**. `ChannelQueue::push` calls `notify_waiters`, so every
channel wakes every parked pairing and exactly one of them wins. A visitor's wait
is therefore the number of times it loses a lottery with nine other waiters: the
median waits ~5 draws (1.7 ms) and the unlucky hundredth waits ~100 (23 ms), at
3 000 arrivals a second. That is the measured shape, and the arithmetic closes.

**So the identity work was right about the defect and wrong about the path.** A
visitor's channel on an `mhd` lane is a *direct* stream, and it reaches the queue
through `push` — counted as `pooled=0 direct=5 770` in one round. The group id
I put in the tunnel prologue only rides the multiplexed path, which an mhd lane
never takes, which is why its A/B measured nothing.

### It works: the lottery was the tail (2026-11-03, `74b4258`)

Interleaved A/B, four rounds, `--aa`, `--ab-arm l4 --binary-b a495d1d`:

| arm | p50 | p99 | rate |
|---|---|---|---|
| `l4` (identity delivery) | 3.71 [3.56..3.83] | **10.27 [10.06..11.30]** | 3 545 /s |
| `l4~aa` (the same build again) | 4.71 [3.74..8.14] | 10.78 [9.40..14.32] | 3 241 /s |
| `l4~b` (before) | 2.93 [2.56..3.33] | **21.95 [20.22..23.96]** | 3 207 /s |

**`rtt_p99_ms: −53.20 %`, and the model records it as a claim** — beyond the
15 % threshold, beyond the run's floor, with the twin inside the new arm's
range and outside the old one's. The pairing's own p99 fell from 19–23 ms to
**7.6 ms**, which is the first time it has been below the client's own worst
open plus transit.

**The median paid for it, and the reason is worth stating:** p50 rose from 2.93
to 3.71 ms (+27 %). The old median was *borrowed* — a visitor often took a
channel that had been opened for somebody else, so its wait was short and the
other visitor's was long. Removing the borrowing moves the median to the true
per-channel cost and halves the tail. Against TCP (p50 2.24, p99 2.58) the cell
is now 1.7× at the median and 4.0× at p99, where it was 1.3× and 8.5×.

By the objective's own order — latency first, and the SLO is stated on p99 —
this is a net win, and the p50 is now the number to attack. It equals the true
cost of one channel: 2.1 ms on the client (measurement + prologue) plus transit.
That is Stage 3's subject, and the identity work has made the median *mean*
something it did not before.

### What is left of the arrival

| segment | before | now |
|---|---|---|
| the pairing's p99 | 19–23 ms | **7.6 ms** |
| `churn-16` p99 | 21.95 | **10.27** |
| `churn-16` p50 | 2.93 | 3.71 |
| wakes per arrival | ~10 | **1.0** |

The claim is hit for essentially every channel (`claimed=3581 shared=0` in a
second), and removing the blanket notify that still fired on a claimed delivery
took the wakes per arrival from ten to one. **That change bought no latency** —
a paired A/B reads p99 9.74 against 9.65 and p50 3.73 against 3.73 — so it is
kept for the work it removes, not as a latency claim.

**The median is now the target.** It is the true cost of one channel: 2.1 ms of
client-side work (the per-stream Noise handshake and the prologue) plus transit,
and it is what Stage 3 attacks. The tail that remains — p99 9.7 against a p50 of
3.7 — is the same shape as before on a quarter of the base, and the next
instrument for it is the one already built: `cmd_us` and `open_us` per window,
now read against a pairing that no longer waits for a lottery.

### Stage 3 — no per-visitor Noise handshake (the median, quantified)

**The target is now a single number.** On every open, not only the slow ones:

| | p50 | p99 | max |
|---|---|---|---|
| the stream's Noise KK handshake | **1 771 µs** | 5 781 µs | 8 348 µs |
| the whole open (placement + handshake + prologue) | 1 779 µs | 5 748 µs | 8 369 µs |
| the command's whole path (dequeue → prologue) | 1 806 µs | 5 784 µs | 8 377 µs |

**The handshake is 99.5 % of what a channel costs.** Removing it takes the open to
single-digit microseconds and the pairing's median from 2.6 ms to roughly 0.8 —
which is where TCP's 2.24 ms p50 actually is.

The design, and its one honest trade:

1. **One handshake per lane.** A lane's first stream runs the KK it runs today;
   both ends keep the handshake hash.
2. **Derived keys per stream.** `HKDF-SHA256(lane hash, info = direction ‖ stream
   id)` gives each direction of each stream its own 32-byte key and its own
   counter; the packet number is the AEAD nonce and the header is the associated
   data, so **a packet is the record** — there is no separate framing to park
   in, which is the failure mode that killed the three record-layer attempts.
3. **The selector stays in the clear.** A new selector byte says "derived"; the
   rest of the first packet and every later packet of that stream are sealed.

The trade, stated so a reviewer can refuse it: **forward secrecy moves from
per-stream to per-lane.** Today every visitor's channel has its own ephemeral
X25519 exchange; with derivation, a lane's compromise exposes its streams'
keys. Mutual authentication is unchanged — the lane's KK is the same KK — and a
lane is per **session and carrier connection**, so the window is one pool
connection's lifetime rather than one channel's. Two further constraints keep it
honest: stream ids never reuse within a lane, and a lane re-establishment starts
a new hash.

- **Gate**: the open's p50 falls to ≤ 100 µs and the pairing's p50 to ≤ 1 ms;
  `churn-16` p99 stays at or below its current 9.7 ms; `bulk-n` inside 15 %;
  CPU per carried Gbit is measured but does not gate (priority four).
- **Revert**: any bulk regression, or any failure in the crypto tests below.
- **Tests before the wire**: (key, nonce) never reused across stream or
  direction; a packet replayed, reordered, truncated or from another stream is
  refused; a lane re-established with the same four-tuple does not decrypt the
  old lane's traffic; plain (`PLAIN_SELECTOR`) and per-stream KK paths both still
  work; the striped, half-close, reset and hot-reload suites stay green.

### The derivation's root is proved, and the rule that must not be broken (2026-11-03)

Two tests settle the premise:

- `both_ends_agree_on_the_handshake_hash` — the initiator and the responder of one
  session end on the **same** hash, and two sessions over the same keys do
  **not** share one. The second half matters as much as the first: a root that
  repeats across sessions would make one lane's keys derivable from another's.
- `both_ends_agree_with_a_payload_each_way` — the same for the carrying shape
  production uses, where each side's hello rides a handshake message. The
  payloads are part of the transcript, so an implementation that mixed them
  differently on the two ends would still round-trip its own bytes and still
  disagree on the root.

The process-level stream ids that did not line up (client 3, server 1) were an
instrumentation artefact, not a protocol one: both `Conn`s allocate stream ids by
**side parity** (`next_stream` starts at the dialer's bit and steps by two), so a
client-opened stream is odd on both ends and the first hashed stream on each lane
is 1. The client's `3` is the first stream whose wrap *succeeded* — the earlier
ones are the lane's reservation probe and a stream that ended first.

**A design rule that fell out of writing this down, and that would otherwise have
shipped as corruption.** "Sealed" must not be inferred from *lane state* ("is
this lane's root set?"). Streams are opened concurrently: a second stream can be
created while the lane's first handshake is still in flight, so one end would
decide "sealed" and the other "not", and each would read the other's bytes as the
wrong thing. The decision has to travel **with the stream**. The cheapest place
is the frame header the carrier already writes — a sealed-stream frame kind or
flag — because the header is also the AEAD's associated data, so the same bytes
that say "sealed" are the bytes the tag covers.

### Stage 4 — RACK plus PTO, instead of one timer for everything (latency)

Split what one `MIN_RTO` decides today, following the shape MsQuic documents
([loss_detection.c](https://github.com/microsoft/msquic/blob/21a4d9c4affa8da13b7f3758ed02c6180e567629/src/core/loss_detection.c)):
a **RACK** deadline for packets with later acknowledgement evidence, a **PTO**
that sends one or two probes instead of declaring a flight lost, and the existing
peer-silence rule for a dead path. Retire the global "oldest data packet older
than the RTO blocks every non-opening write" gate; the opening reserve stays as
anti-starvation, not as a mask.

Take the ack-timing rules from a reference that has them
([quic-go's received-packet tracker](https://github.com/quic-go/quic-go/blob/aa265c3fc6489e3bfa8071b333be95e0e887a51d/internal/ackhandler/received_packet_tracker.go)):
ack a new stream's first bytes, a packet previously reported missing, and a new
gap immediately; aggregate only within a bounded delay.

- **Gate**: a single lost opening recovers without a wide-timer step; `bulk-n`
  does not storm; `churn-16`, `rr-16`, both clean stages and `loss1`/`loss5`/
  `rtt100` each judged on their own cell.

### Stage 5 — latency-aware internal scheduling (latency)

With the arrival path cheap, the remaining queue is the one bulk makes. Replace
the flat per-stream round robin with classes: control and repair; a stream still
opening; interactive; bulk. Borrow the queueing shape from Quinn's
[priority plus recency round-robin](https://github.com/quinn-rs/quinn/blob/2c5750139fa09aab588bf0f939c83f521d658baf/quinn-proto/src/connection/pacing.rs)
and quic-go's urgency queues, but with **no new configuration key** — the class
is derived from the stream's own state.

- **Gate**: arrivals under bulk are not slowed; `bulk-n` unmoved; a burst of new
  streams cannot starve a mature one (bounded quantum per class).

### Stage 6 — a rate- and BDP-driven window (throughput, third)

The current window grows additively and is never cut, `slow_start` latches off at
the first loss forever, and a flight may outlive its own timer. Add delivery rate
and `app_limited` sampling (the shape in
[MsQuic's BBR](https://github.com/microsoft/msquic/blob/21a4d9c4affa8da13b7f3758ed02c6180e567629/src/core/bbr.c)),
then a simplified Startup/Drain/ProbeBW with a token-bucket pacer whose burst is
bounded by RTT and MTU.

- **Gate**: `bulk-n` and `rtt100` improve or hold; `rate20`/`rate100` do not
  collapse; RSS and CPU are compared only after latency and throughput hold.
- **Revert**: the controller alone, keeping stages 2–5.

### Stage 7 — memory, then processor

Only after the two axes above hold: reclaim windows and blocks from the real peak
flight, keep the pooled receive blocks and the sliced send payload, and give the
recovery table bounded, low-allocation storage. Do not retry region pinning, a
piece per datagram, or a chunk queue without a cursor — each was built, measured
and rejected.

### Stage 8 — the full ritual

`just fmt`, `just py-fmt`, `just check`, `just interop`, `just l3-accept`, then a
repeated `dev` plus `timeline` plus `capacity` campaign, then the five-arm
`sweep`. Only a gate-green record becomes
`benches/records/results-bench-v1.0.0.json`, with charts and both READMEs in the
same commit. A method change makes it a re-baseline release (no `--baseline`),
recorded as such in the changelog.

## Negative results: do not retry these blind

Every row was built (or measured) and reverted with its numbers. The archive has
the reasoning; this table exists so a future round does not spend it again.

| mechanism | result |
|---|---|
| yamux over an mhd lane | arrival p99 21.5 → 3.77 ms, but `bulk-n` −23.75 % and CPU/bit +29.37 % |
| a Noise record layer above the carrier | arrival p99 −74 % at three record sizes, and the bulk cell failed at all three |
| pre-opened channel stock | 6 400 arrivals; the stock of 4 is consumed by the first 4 |
| more lanes (4/8/16) | flat within the scatter; the tail is not lane queueing |
| larger `INITIAL_CWND` | first stage **doubled** (79.4 → 153.9 ms p99) |
| smaller `MIN_RTO` (200 → 20 ms) | every bulk cell failed |
| `MIN_RTO_SMALL_FLIGHT` / `SMALL_FLIGHT_INITIAL_RTO` at 5 ms | p99 22.6 / 22.1 against twins 22.3 / 20.3 |
| `TOKIO_WORKER_THREADS` 8 or 16 | monotone the wrong way; the lock is contention, not starvation |
| timer armed from the oldest packet (any kind) | `churn-16` p50 2.44 → 16.07 ms |
| timer armed from the oldest **data** packet | `bulk-n` −60 %; the flight outlives the RTO on this host |
| hole recovery by distance | neutral on every cell |
| eager credit advertisement | `churn-16` p99 +1 068 %; credit frames preempt data |
| `describe_hole` (send table for a receive hole) | answers about the wrong direction |
| per-datagram lock batching | +0.35 %, inside a 9.81 % floor |
| receive-region ownership / pinned pieces | RSS 376 MiB, but throughput 2.76 Gbit/s |
| chunk queue without a read cursor | `cpu_s_per_gbit` 46.7 |
| widening the pool for `soak` | 448 slots, same four stage failures |
| `warmup_rounds` to cure the first stage | a round's first stage is cold again; the probes restart per round |

## Environment

- **20 cores, 23 GB, kernel 6.12.** `/etc/machine-id` is absent, so `host_id`
  hashes CPU model + core count only; the hostname changes without moving it.
- **The cgroup is 13 GiB**, and `oom_kill` is a counter to read per run. Build
  with `CARGO_INCREMENTAL=0`, prefer one release build over many debug feature
  permutations, and check `ps -eo rss,comm --sort=-rss` before a long run — a
  `rustc` for another project competes for the same memory.
- **The host is shared.** Its load swung between 1 and 87 in one afternoon, and
  `bulk-n` swings 3 ⇄ 11 Gbit/s and `capacity` 2 ⇄ 8 streams with no tree
  change. Every run records the load average; §10's "do not conclude across
  variance" is the operative rule.
- **`sudo -n` works, and it drops the environment.** The stats switches need
  `sudo -n env MOLEHILL_MHD_STATS=1 just bench …`; a run whose `server.log` has
  no diagnostic lines is a run with the switch off.
- **Do not run two chains or two bench runs at once.** The suites bind fixed
  ports and the bench holds network namespaces; a second run produces
  `ip netns exec` failures that look like product failures.
- **`/tmp` is periodically wiped.** Keep artifacts under `target/evidence/` or
  `~/tmp/`.
- **`timeout N` orphans the run**: it signals `uv run`, not the python child,
  which keeps going and holds the bench lock.
- **`iperf3` is installed at runtime**, so a container restart drops it
  (`command -v iperf3` first). It is version-sensitive in the fields the model
  reads.
- **Peers and the interop binary are cached** (`~/tmp/bench-peers`,
  `~/tmp/interop`), so a normal run needs no network. `just bench peers fetch`
  re-resolves each peer to its latest GitHub release and records the versions in
  the results meta. `just bench doctor` states what this host can and cannot
  measure before a run has to find out.
- **A baseline-less gate is the norm here.** Every stored baseline either has no
  `host_id` or predates the current method, so a fresh sweep is gated on its own
  absolute checks — coverage, the endpoint invariant, the SLO, drift, wedge and
  the ramp — and the gate names what it could not verify instead of assuming it.
- **A `sudo` bench run can leave root-owned files** in `.git/index` and
  `~/.cache/molehill-bench/venv`; both are repaired with `chown -R dsh:dsh` on
  the path named in the error.

## The archive

[HANDOFF-archive.md](HANDOFF-archive.md) holds every closed record, with a
contents list at the top. The section that matters most is
[the v1.0.0 cycle](HANDOFF-archive.md#the-v100-cycle-the-mhd-carrier-moved-2026-11-03):
the carrier swap, the cost investigation, all ~80 rounds of the arrival-tail
search, and the release ritual as it was run.

**No number in the archive may be quoted as a measurement of the current code**,
compared against a newer run, or used to gate anything. The live numbers are in
this page's "What is measured" section, and the method's home is
[docs/benchmarks.md](docs/benchmarks.md).
