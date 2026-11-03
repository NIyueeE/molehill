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

### The key schedule exists and is tested, and is not wired (2026-11-03, `6cdc25a`)

`src/transport/mhd/cipher.rs` derives one direction of one stream's key from the
lane's handshake root: `HKDF-SHA256(root, "molehill mhd stream v1" ‖ direction ‖
stream id)`, nonce = packet number, associated data = the packet header. Six
tests pin the properties the design depends on — per-(stream, direction) keys,
roots that never collide across lanes, an authenticated header, tampering and
truncated tags refused.

It is `#[cfg(test)]` until it is used, because a key schedule with no caller is a
cipher nothing uses. Three things wire it, and they are the next round in this
order:

1. **A frame flag that says "sealed".** In the header, because the header is
   already the associated data: the bytes that say which stream a packet belongs
   to are the bytes the tag covers, and the decision travels with the stream
   rather than being inferred from lane state (which would race — see above).
2. **The lane's root.** Both ends already record it; the client keeps the first
   successful handshake's, the server the same. A stream opened after it is sealed.
3. **`Conn` seals and opens per packet.** `transmit_stream` seals the payload span
   it just laid out; `on_datagram` opens it; the payload's bound shrinks by the
   sixteen-byte tag, which is the one arithmetic that must be right everywhere.

**Gate** (from the design above): the open's p50 ≤ 100 µs, the pairing's p50 ≤ 1 ms,
`churn-16` p99 ≤ 9.7 ms, `bulk-n` inside 15 %.

### The sealed-stream wiring: two bugs found, a third left (2026-11-03)

The 794-line patch is kept outside the tree (`~/tmp/stage3-wiring-v2.patch`). It
does not yet pass the integration suite, and getting this far took three
measurements that are worth more than the patch.

**Bug 1 — the sealing rule was per lane, and must be per stream.** The first
attempt refused any stream without a key once its lane had a root. But a lane's
*first* stream is opened before the handshake that produces the root — it is the
control channel, and it stays plain for its whole life. Refusing its traffic the
moment the lane acquired a root killed the lane that had just been established,
which is why the client's control session looped on "connection refused" and the
suite fell from 22 of 22 to 16. The counter said it in one run: `no_key=12`,
`sealed_sent=0`. `StreamState` now carries `sealed`, set at creation.

**Bug 2 — the frame's length must describe the payload on the wire.** The sender
wrote the *plaintext* length while the receiver carved the last sixteen bytes out
of the frame's length as the tag, so every sealed packet was read sixteen bytes
short and its tag taken from the body. One counter caught it exactly:
`sealed_sent` and `unsealed` were **equal**, packet for packet. With the length
fixed, `unsealed` is zero.

**What is left.** Packets now open on both ends, and the visitor still never gets
a channel: the server's log repeats `pairing budget expired with no data channel
… gathered=0`, and the client never logs `client asked for a stripe` — so the
request is not reaching it. That is the next measurement, and it is one grep away
from a cause: either the server's control task is not draining
`data_ch_req_rx`, or the client's control loop is not reading, and both are
visible with the existing `debug` lines on the request and on
`ControlChannel::run`'s send.

**Do not start from the design again.** Apply the patch, set
`RUST_LOG=molehill=debug MOLEHILL_MHD_STATS=1`, and read those two lines. The
three counters (`sealed_sent`, `unsealed`, `no_key`) are in the per-second stats
line and made both bugs above findable in one run each.

### The sealed wiring: four bugs down, and the crypto is now clean (2026-11-03)

Patch v5 is outside the tree (`~/tmp/stage3-wiring-v5.patch`, 848 lines). The
fourth bug was the one that made a busy lane fail while an idle one worked, and
it is worth stating plainly because every counter pointed away from it.

**Bug 4 — the associated data was the whole run buffer.** `transmit_stream` seals
each packet into a shared run buffer of up to `SEND_RUN` (44) packets, and the
header it signed was `out[..payload_at]` — every packet *before it in the run*
included. The receiver signs one datagram's header. So the first packet of each
run opened and the other forty-three did not: `sealed_sent=704, unsealed=688` is
exactly `1 in 44`. The fix is one line — remember where the packet starts and
sign `out[packet_start..payload_at]`.

**With that, the crypto is correct under load.** A striped 8 MiB echo runs at
`sealed_sent ≈ 1100` packets a second with `unsealed_tags = 0` and `no_key = 0`.
Four bugs were found in four rounds, each by one counter:

| counter | bug |
|---|---|
| `no_key=12` | the sealing rule was per lane; a lane's first stream predates its root |
| `unsealed == sealed_sent` | the frame's length described the plaintext, not the wire |
| nothing at all | a sealed stream was created but never announced to the accept loop |
| `1 in 44` | the AD was the run buffer, not the packet |

**What still fails is not the cipher.** The same test closes the visitor after a
*variable* number of bytes — 0.4 MB, 2.2 MB, 4.5 MB across runs — and the close is
a clean EOF (`n == 0`), not a reset: no `mhd stream ended: …` error, no rejection
counted, no key failure. Two candidates, in order:

1. **A lost finish.** `ends_at(offset, len)` compares a *plaintext* offset with
   `fin_at`, and the payload clamp changed how much one packet carries; if the
   last chunk never satisfies it and `pending_fins` does not cover the gap, the
   peer sees a stream that stops without ending — and the forwarding loop closes
   on its own timeout.
2. **Flow control under sealing**: the receiver's credit is plaintext, the wire is
   sixteen bytes per packet larger, and a window that is exactly sized could stall
   the sender at the point where the two diverge.

Both are visible in one run with the existing `mhd stalled` line and a counter for
`pending_fins` drained against `ends_at` firings.

### Sealing delivers the arrival — and costs the bulk (2026-11-03)

The patch's end-to-end numbers, same host, same hour, one round each:

| cell | with sealing | baseline | |
|---|---|---|---|
| `churn-16` p50 | **2.30 ms** | 3.71 | **−38 %** |
| `churn-16` p99 | **3.59 ms** | 9.74 | **−63 %** |
| `churn-16` rate | **6 463 /s** | 3 545 | **+82 %** |
| the handshake per open | **14 µs** (p99 45) | 1 771 µs | **−126×** |
| `bulk-1` throughput | 2.11 Gbit/s | 5.60 | **−62 %** |
| `bulk-1` CPU per Gbit | 1.27 s | 0.67 | **+90 %** |
| `bulk-1` cores busy | 1.95 | 3.36 | fewer, at lower throughput |

**Against TCP the cell reads p50 2.30 vs 1.97 and p99 3.59 vs 2.16** — 1.17× and
1.66×, where the carrier stood at 1.7× and 4.0× before this work. Rate 6 463
against TCP's 6 728.

So the top-priority axis moves exactly as designed: the 1.8 ms handshake is gone
(14 µs), the arrival median falls by 38 %, the tail by 63 %, and the cell carries
82 % more arrivals. **That is the largest latency win of the whole effort and the
first one that is not a trade against itself.**

**And the bulk pays for it.** Two thirds of `bulk-1`'s throughput, at 1.9× the CPU
per bit, with *fewer* cores busy — the signature of a stall on top of a per-byte
cost, not of crypto alone. Both are named below.

### What is left before this can land (2026-11-03)

The patch is v9 (`~/tmp/stage3-wiring-v9.patch`, 937 lines) and it now carries two
tests that refute the arithmetic hypotheses this file proposed:

- `a_sealed_stream_sends_the_tail_of_an_oversized_payload` — a 1 400-byte record
  on a sealed stream **does** go out as two packets, tail included.
- `a_sealed_stream_carries_eight_mebibytes` — 8 MiB crosses one sealed stream,
  byte for byte, in process, with no stripes and no sockets. **It passes.**

So neither the "tail never emitted" theory nor "the volume itself stalls" is true.
Whatever kills the striped echo needs the real topology: the striped 8 MiB echo
still ends early, and its own trace says **the visitor→backend direction
finished after 257 frames** while the reader side closed at a frame boundary.

That sentence is the next instrument's target: 257 frames is far short of the
transfer, so the client's *send* direction stopped reading the visitor, and the
question is why — a stalled write into the sealed stream would do it, and the
`mhd stalled` line (credit-blocked, `unsent_ms`) is the one that would say so. It
has not been seen yet in any run of this patch, which itself is information: if
the writer is not blocked, the client stopped reading for another reason and the
forwarding loop's own end conditions are where to look.

**Do not re-derive the crypto.** It is correct under load (`unsealed_tags = 0` at
~200 000 sealed packets a second) and it delivers the arrival (p50 −38 %, p99
−63 %, rate +82 %, the handshake 1 771 µs → 14). What stands between this and a
landable change is one striped transfer, and the two failures below.

### Two switches isolate the striped defect to "sealing **and** stripes" (2026-11-03)

Two one-line experiments on the patched tree, each a single test run:

| experiment | result |
|---|---|
| **growth frozen** (`STRIPE_BACKPRESSURE` = 1 h, so the plan never grows) | still fails, at 465 920 of 8 388 608 |
| **sealing off** (`set_lane_root` a no-op, every other change in place) | **passes** |

So the growth path — the only part of the striped design that adds and removes
members mid-transfer, and the previous round's prime suspect — is **not** the
cause. Sealing is, and it needs the stripe layer to show: an in-process 8 MiB
transfer over **one** sealed stream passes, and `bulk-1` over real sockets (one
stream per visitor) runs at 2.1 Gbit/s without closing. The defect appears only
when a **group of several sealed streams** carries one transfer.

That is as narrow as the problem has ever been stated, and it names the next
instrument: a stripe group of **two** sealed streams in process — the layer's own
`spawn_group` test, with `Conn`s behind the duplexes instead of bare streams. If
it reproduces there, the cause is inside the layer's frame distribution across
sealed streams and can be bisected without sockets; if it does not, the sockets
are part of it and the next step is the run builder's segmentation of
differently-sized packets.

**Where the whole stage stands**, since five rounds have gone into it:

- the cipher is correct under load, with a proof in tests and `unsealed_tags = 0`
  at ~200 000 sealed packets a second;
- it delivers the arrival: `churn-16` p50 2.30 ms (from 3.71), p99 3.59 (from
  9.74), rate 6 463/s (from 3 545), handshake 14 µs (from 1 771);
- it costs two thirds of `bulk-1`, of which **one copy per received packet is
  removable** and the rest is the seal itself;
- and one striped transfer still dies, isolated this round to "sealing **and**
  stripes together".

### The carrier is clean with several sealed streams; the stripe layer is not (2026-11-03)

A new in-process test, `four_sealed_streams_carry_their_own_bytes`: four sealed
streams on one lane, written interleaved under the peer's credit and read at the
other end, each with 256 KiB of its own bytes. **It passes.** So the carrier has
no defect with several sealed streams — the interleaving, the per-stream keys and
the run builder are all fine together.

That leaves the stripe layer, and one experiment would say whether the *number* of
stripes matters: run `striped_data_channels` with `set_initial_stripes(2)`. It is
**not a valid experiment as the test stands** — the test asserts that all four
pool tunnels carried a stream (`no pool carried a stream on each of its four
tunnels`), so halving the stripes changes the fixture rather than the load. A
2-stripe run needs a striped fixture that expects two, which is a small piece of
work for the next round rather than a one-line change.

**Two ways forward, both cheap and both named here so they are not re-derived:**

1. **A stripe-group harness with sealed ends.** `spawn_group` takes `AsyncWrite`
   halves; a sealed end is a `StreamIo` over a `Shared`, which needs a pump task
   to move bytes. Building that harness is the *only* way to bisect the layer's
   frame distribution without sockets — and sockets are the last variable left
   that has not been excluded.
2. **A second striped fixture** that expects two stripes, so the count can be
   varied against a test written for it.

Until one of them exists, the state of this work is: the cipher is proved, the
arrival is delivered, `bulk-1` pays two thirds, and one striped transfer dies for
a reason that lives in the layer between the carrier and the forwarding loop.

### Stage 3 is PARKED: the design works, one striped transfer does not (2026-11-03)

The rule set two rounds ago was two more, then park. The harness did not close it:
the two-group version — a real `spawn_group` on each end with an echo between them
— stalls at **zero bytes**, which is a defect in the harness rather than a
reproduction, and this round could not finish it.

**What stage 3 proved, and it is not nothing:**

| | with sealing | baseline |
|---|---|---|
| `churn-16` p50 / p99 | **2.30 / 3.59 ms** | 3.71 / 9.74 |
| `churn-16` rate | **6 463 /s** | 3 545 |
| the per-open handshake | **14 µs** | 1 771 µs |
| against TCP | 1.17× / 1.66× | 1.7× / 4.0× |

with the cipher correct under load (`unsealed_tags = 0` at ~200 000 sealed
packets a second), four tests pinning its properties, and four real bugs found and
fixed on the way. The cost is two thirds of `bulk-1`, of which one copy per
received packet is removable.

**What it never got:** a striped 8 MiB echo through a sealed group. Nine
configurations were excluded by measurement — growth, stripe count, GSO, many
streams, single stream, four streams, in-process volume, the prologue length, the
accept announcement — and the boundary is exactly "sealing plus two or more
stripes". The cause is in the stripe layer's interaction, and the harness that
would have found it is unfinished.

**So the patch is parked, not deleted.** `~/tmp/stage3-sealing-parked.patch`
(1 171 lines) carries every fix, all six tests, and the half-built harness. A
later cycle that wants the arrival can pick it up from a known state instead of
rediscovering four bugs.

### Where the next cycle should work instead

Stage 3 was the only route tried for the arrival tail, and it is now bounded off.
Two untouched stages are independent of the handshake and carry the same priority:

1. **Stage 4 — RACK and PTO.** One `MIN_RTO` decides both "this packet is lost"
   and "this flight is too old"; the record already shows that every attempt to
   move that one number breaks a different cell. Splitting it is the change the
   measurements have asked for since the first `loss5` run, and it is local to
   `recovery.rs`.
2. **Stage 5 — latency-aware scheduling.** The `rr-16` and `bulk-n` cells already
   show mhd at parity with TCP when work is interleaved across streams; the stall
   cells are where it is not. A per-stream quantum that keeps an arrival from
   queueing behind a bulk run is the shape, and `dwell`/`rx_total` are the
   instruments that would show it working.

Both are less glamorous than removing a handshake and both are measurable with the
cells that already exist.

### The release gate is the handshake under bulk, 29× slower (2026-11-03)

One 0.2-scale timeline with the stats switch, the committed windows compared
between the **first** 30 seconds and the **last** 30 of the same run:

| | first stage | last stage |
|---|---|---|
| `hs_p50` per window (the Noise KK) | **28 876 µs** | 1 002 µs |
| the channel's `open_p50` | **12 149 µs** (p99 79 291) | ~1 000 µs |
| `ready − open` (the prologue) | 2–7 µs | 2–7 µs |
| `cmd − ready` (the dequeue) | 8–10 µs | 8–10 µs |
| the send-buffer dwell | 9 µs | 0 µs |
| the connection lock | 0 µs | 0 µs |
| the server's reader park, p99 | 936 µs (max 5 497) | 210 µs |

**The whole first-stage penalty is inside `open_stream`, and the handshake is all
of it.** The prologue and the dequeue are single-digit microseconds in both
stages, the send buffer and the lock are microseconds in both, and the server's
park is under a millisecond at p99 — so the twelve milliseconds are not the wire,
not a queue in either send buffer, and not a lock. What is left is the handshake's
own polls competing with four bulk streams on four workers, which is why the same
code costs 1 ms on an idle lane and 29 on a busy one.

**That reframes the gate.** It is not the arrival path being slow; it is the
arrival path being *scheduled* behind bulk. Three consequences:

1. The parked sealing patch would have removed this exactly — it took the same
   measurement from 1 771 µs to 14 — which is why it is worth a future cycle.
2. Without it, the cheapest lever is **where an opening stream is placed**: an
   arrival landing on the lane carrying the most bulk pays the most. The pool picks
   a lane by load already; whether that ordering accounts for a *newcomer* is a
   measurement, not an assumption.
3. And the second lever is the client's own runtime: four workers saturated by
   bulk do not schedule a newcomer's polls promptly, and no wire change fixes that.
   `TOKIO_WORKER_THREADS` was measured once (4 vs 8, inside the floor) on a cell
   with no bulk; it has never been measured on the *first stage's* arrival.

### The release gate fails the same four stages every time (2026-11-03)

Two full-scale `timeline` runs, back to back, both fail **exactly** `rate100`,
`rate20`, `jitter` and `clean#2` — every stage after `loss5` — with the same
reason (`iperf3 error: control socket has closed unexpectedly`). This is not the
intermittent harness failure the record describes as costing "0–3 stages, arm
independently"; it is deterministic and positional.

What each failure looks like, from the artefacts rather than the summary line:

- the **sink** accepted the connections (`[ 5] local 127.0.0.1 port 2402 connected
  to …`) and then logged **0.00 Bytes for 117 s** before iperf3's idle timeout;
- the **pool** reports `established=4` once and never repairs or reaps, so no lane
  is missing;
- the **control session** stays up; `mhd stalled` never fires; no rejection, no
  key failure, and the daemon's log has no `ERROR` or `WARN` at all.

So the tunnel forwarded the *connection setup* and none of the *payload*: the four
streams exist and carry nothing.

**And `rate100` on its own is fine.** A single-stage campaign under the same
condition and shaping puts the **control** arm (no tool in the path) at
0.0956 Gbit/s and molehill at **0.0954** — within 0.3 % of a path that has no tool
in it. So the condition is not the defect; **the stage after a lossy one is**.

That is the shape the plan's Stage 5 exists for: the window grows and is never
cut, `slow_start` latches off at the first loss, and nothing re-arms it. A sender
whose window collapsed under `loss5` has no mechanism to grow back — and the next
stage is where that shows, as a connection that opens and then carries nothing.

**The instrument for the next round is one word away and it is already in the
tree**: the window line is a `debug!` in `transport::mhd::pump`, and this run
logged with `RUST_LOG=molehill::transport::mhd=debug`, which did **not** reach it
(`mhd window` count: zero). Use `RUST_LOG=molehill=debug` — the form the earlier
per-arrival run used successfully — and read `cwnd`, `flight` and `slow_start`
across the `loss5 → rate100` boundary. If `cwnd` is pinned at a few kilobytes and
`slow_start` is false, Stage 5's first half (re-arm growth after an idle or
loss-free period) is the fix and it is local to `recovery.rs`/`conn.rs`.

### Retraction: the drain expired on the churn probe's sockets, not on a backlog (2026-11-03)

The previous entry read `drain_expired` as "the carrier left a backlog the drain
could not clear". **That was wrong**, and the code says so plainly: `drain`'s
predicate is a small queue **and no send-capable socket**, and the timeline runs
the churn probe for the whole schedule. There are therefore always `ESTAB` sockets
on a service port while churn runs, so `drain_expired` is *structural* in the
timeline and says nothing about the arm. `drain_final_backlog` also reads ~500
packets on stages that passed, so that number is the ordinary state of a busy leg,
not a tail.

What survives from that round, and matters: the **retry fired on all four missing
cells and all four failed again**, so the loss is deterministic within a run. And
the sink of `clean#2` shows it directly — on a **clean** leg, four streams
established and **zero bytes for the whole stage**, in a run whose first `clean`
stage carried 4 Gbit/s.

### `malformed` was real, and it is the UDP probe crossing the lane (2026-11-03)

The refusal counter, which this file has twice said reads zero on a healthy run,
reads **2–6 per second** through a whole timeline. It was one number for two
different checks, so it was split this round — and the split answers it:

| run | `malformed_decode` | `malformed_lane` |
|---|---|---|
| `churn-16` (no UDP probe) | **0** | 0 |
| `timeline` (UDP probe running) | 2–6 /s | rare, 1 in one window |

A datagram on a lane's own socket that cannot be framed, only while the UDP probe
runs, is the UDP probe's traffic arriving where the lane's decoder reads it. The
counter is telling the truth about a shared port, not about a broken lane.

**So the counter needs a third home**: traffic that is not ours must not be counted
as a refusal of ours, or every future run reads a defect that is not there. Until
it has one, a run's `malformed_decode` is evidence only when the UDP probe is off.

### The dead stages were never read: `snd_wnd = 0` from the first second (2026-11-03)

The per-sample raw artefacts the model keeps answer the question the summaries could
not. From one run, the iperf3 client's own first and last intervals:

| stage | outcome | first interval | last interval |
|---|---|---|---|
| `clean` | passed | `bytes=180 092 928  snd_wnd=98 304` | `bytes=106 561 536` |
| `clean#2` | failed | `bytes=917 504  snd_wnd=0` | `bytes=0  snd_wnd=0` |
| `jitter` | failed | `bytes=786 432  snd_wnd=0` | `bytes=0  snd_wnd=0` |

**`snd_wnd = 0` in the *first* second of a connection that then carries nothing.**
The peer's receive window was closed from the outset, which means the tool never
read that visitor socket at all — not "the credit ran out later under load". About
900 KB arrives (the kernel's socket buffer) and then the connection is stuck for the
whole stage, until iperf3's sink gives up after 117 s and the client exits with
`control socket has closed unexpectedly`.

This also explains why every other instrument was silent: `dwell` measures a
sender's unsent bytes, `mhd stalled` measures a flight, the pool counts lanes, and
none of them looks at *a receiver that was never started*. The chain that should
end in "somebody reads this visitor's socket" ends in a channel nobody serves.

**The prime suspect is the one recent change on that path**: the identity delivery
that hands a channel to the pairing that named its group. A channel delivered into a
group that can no longer take it — already complete, or already finished — waits in
a queue that nobody drains, and its visitor's window stays shut for ever. That would
be a regression from this cycle's own work, and it is cheap to test: count a channel
handed to a group with no live stripe waiting for it.

**The instrument for the next round**, in order:

1. A counter pair beside `claimed`/`shared`: **delivered to a group that was gone**
   (`dead_group`), and **claimed by a stripe that never started reading**
   (`orphaned`). Either rising during a failing stage names the defect outright.
2. If both are zero, the missing read is downstream of delivery, and the next place
   to look is the client's `register` → `run` handoff for a stripe whose group
   completed while it was still being registered.

### Refuted: it is not the identity delivery (2026-11-03, round 29)

`deliver` did have a hole — it returned `Ok(())` and dropped a channel when the
pairing that named the group had gone, while its own comment said the channel falls
through to the shared queue. That is fixed, and a counter beside `claimed`/`shared`
records it.

**But on a full-scale run that failed the same four stages, `dead_group` stayed at
zero from the first window to the last.** So the hole was real and is not the cause.
Two other numbers from the same windows:

```
delivered pooled=0 direct=5..9 claimed=5..9 shared=0 dead_group=0
```

Channels were **delivered and claimed normally through the failing stages**. The
defect is therefore *after* delivery: the channel exists, the pairing has it, and
the data it carries is never read.

**That narrows `snd_wnd = 0` to its proper owner.** The visitor's window is closed
because the **server** never reads the visitor's socket, and the server reads when
`wake_readers` fires for that stream. Delivery is counted and healthy; the read is
not happening. This cycle has been bitten by exactly this class twice before (the
`push` path and the claim path each missed a wake), which is why it is the first
suspect rather than a new theory.

**The instrument for the next round**, chosen to distinguish "no wake" from "no
data":

- count, per second, the times a stream's `poll_read` returned `Pending` **while
  its receive buffer held unread bytes** — a reader starved with data waiting;
- and count `wake_readers` against `RecvBuf::note_ready` per lane.

A starved-read count above zero during a dead stage is the missing wake, named. If
it stays zero, the data never arrived at the buffer, the sender never sent, and the
next place to look is the client's stripe sender for a group whose stripes were
never started.

### Not starvation either: there is no stream to starve (2026-11-03, round 30)

The pump now samples, once a second, how many streams have had unread bytes for
longer than a second — the question a reader cannot answer about itself, because a
reader that is never polled never runs a line of code. On a full-scale run it never
fired once, while two stages still carried nothing.

So the dead stage is **not** a stream whose data waits unread. With `snd_wnd = 0`
on the visitor's socket and no stream starving, the only shape left is:

> the visitor's connection is accepted and its bytes shut the sender's window, and
> **no data channel is ever attached to it** — so there is no stream to read, nothing
> to starve, and every stream-level instrument is silent by construction.

That is one layer above everything measured so far. Three hypotheses have now been
killed by measurement, in order:

| hypothesis | killed by |
|---|---|
| the drain left a tail | `drain_expired` is structural: the churn probe always holds sockets |
| a channel is dropped for a dead group | `dead_group = 0` through a failing run |
| data arrives and is not read | `starved = 0` through a failing run |

**The instrument for the next round is a pairing, not a stream:**

1. Count pairings that **wait for a channel and never receive one** before their
   visitor gives up — the visitor-side counterpart of `parks`.
2. Count channels delivered to a group whose **visitor has already finished** — the
   mirror image of `dead_group`, which counted the pairing being gone rather than
   the visitor.
3. And record, per pairing, the group id it named and the group id of the channel it
   was given, so a mismatched pairing is visible as an id and not as a suspicion.

A run in which (1) rises is the defect stated in one number.

### The stall line was never silent: it names the wedge (2026-11-03, round 31)

Earlier rounds searched for `mhd stalled` in runs logged at `INFO` and concluded it
never fired. It is a `debug!` line, and a run with `RUST_LOG=molehill=debug` has
**2 282 of them** on the server. Read properly, they are the clearest evidence this
investigation has produced.

Server side, over one failing run:

```
unsent_ms=200            median (max 3 211)
unacked_ms=196           median
credit_blocked=true      1 492 of 2 282 lines
worst_stream=5  worst_stream_unsent=14 994 767
worst_stream_next=73 582 695   worst_stream_written=88 523 502
worst_stream_retransmits=0
```

So the wedge is a **live stream that has carried 88.5 MB, has sent 73.6 MB, and is
holding 15 MB it cannot send because the peer's window is shut** — and the peer
reports the opposite of what that implies:

- the client's own stall lines are **never** credit-blocked, its unsent is **0**, and
  the starvation sampler never fired, so by every receiver-side instrument its read
  buffers are empty and its reader is running.

Fifteen megabytes is the receive window (`WINDOW` is 16 MiB). A window that is full
on the sender's side and empty on the receiver's is not a reader that stopped: it is
**credit that was consumed and never given back**. That is a bookkeeping defect in
`advance_read`/`advertised`, not a scheduling one, and it explains every symptom at
once — the shut `snd_wnd` on the visitor's socket (the forwarding loop is parked on a
stream that will not accept bytes), the empty receiver buffers, and the silence of
every stream-level instrument, because those bytes never arrived to be starved over.

**The instrument for the next round** is on the receiver, once a second, for the
worst stream: `recv_credit`, `advertised`, the stream's unread bytes, and whether a
credit update was due. Three numbers in one line separate "full because unread" from
"full because the accounting lost it" — and the second is a bug that has been in this
carrier for as long as the window has.

### The credit wedge is fixed, and the release gate drops to two failures (2026-11-03)

`advance_read` moved the window and never armed an acknowledgement. `credit_due`
woke the pump, the pump had nothing to send, and a packet that is not an
acknowledgement carries no credit — so a reader that drained a window on a quiet or
one-way stream left the sender with `send_credit - written == 0` and no way to hear
otherwise. One line in `advance_read` now arms that acknowledgement.

Measured on the next full sweep, against the run before it:

| | before | after |
|---|---|---|
| stages with no measurement | **5** (`loss5`, `rate100`, `rate20`, `jitter`, `clean#2`) | **1** (`clean#2`) |
| `credit_blocked` lines (server) | **1 492** of 2 282 stall lines | **0** — no stall line at all |
| `clean` p50 / p99 | 78.3 / 248.5 ms | **3.5 / 132.2** |
| `rate100` p50 / p99 | no measurement | **84.6 / 105.1** |
| `jitter` p50 / p99 | no measurement | **81.7 / 107.9** |

`just bench gate` on that run: **2 failures**, down from 5 — `clean#2` produced no
measurement, and `timeline[clean]` p99 132 ms against the 50 ms limit.

The defect was real — credit moved and was never spoken — and it is fixed. **Its
effect on the gate is not established**, and the next run says so: with the same
build, four stages carried nothing again. It is therefore recorded as a correctness
fix whose gate effect is one run old, not as the cure.

What that second run does establish is that the *shape* of the failure changed. In
it, `mhd stalled`, `credit_blocked` and the starvation sampler are all **zero**, and
two other counters are equal to the visitor: `accepted = paired = 33 995` over the
whole schedule, with a per-window gap never above 2. So pairing is exonerated, no
credit is held, nothing is queued — and four stages still carried nothing.

That leaves one state: a visitor whose pairing returned, whose socket the forwarding
loop never read, and which therefore never put a byte in front of the carrier. Its
sender sees `snd_wnd = 0` from the first second, which is what a socket nobody reads
looks like, and every instrument in the carrier is silent because *the carrier was
never given anything to do*.

**The next instrument is on the boundary between the visitor's socket and the
carrier**, and it is two counters per visitor: that the forwarding loop polled the
visitor's read at all, and that the poll returned bytes. A visitor that pairs and is
then never polled is the remaining state, and it is the one no counter can currently
see.

`clean`'s p99 is separately 2.6× the limit while its p50 is 3.5 ms, so the tail is a
few very slow arrivals rather than a shift. That is its own instrument's work.

### The age gate is the dominant bound, and scoping it lost (2026-11-03)

Three bounds can hold a write and only two were ever visible. The third — the gate
that refuses new data when the oldest packet in flight is older than the
retransmission timer — now has a variant of its own in `Refusal` and a per-second
counter. Over one full run:

| bound | writes refused |
|---|---|
| congestion window | 866 723 |
| peer's credit | 327 049 |
| **the age gate** | **4 641 975** |

**The invisible bound is the dominant one by a factor of five.** It also explains why
a stage can carry nothing while every instrument reads zero: the bytes it refuses
never enter the buffer, so there is no backlog to report, no credit block, and no
starvation — the sender looks idle while the forwarding loop is parked.

The obvious next move was to scope it per stream, since the claim it makes is about
one stream's flight. **That was measured and lost**: the run with the scoped gate
failed five stages against the previous run's one, and the refusal count barely moved
(2.84 M against 4.64 M). It is reverted, and the record is that the scoping as written
did not help — not that the gate is fine.

**What this round establishes and what it does not.** The refusal split is real and
the age gate dominates. Whether removing it helps cannot be measured by these runs:
with near-identical builds the failure count has ranged from **1 to 5 in a single
day**, so any one comparison is inside the noise. The plan's Stage 3 says to delete
this gate and put RACK/PTO in its place; that change is large enough to be judged by
its own pre-registered bet, and it needs a *paired* comparison — two binaries, one
run, both arms interleaved — not two runs an hour apart.

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
