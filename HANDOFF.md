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

### mhd's last clean stage has never carried bulk, so the pair never existed (2026-11-05)

Every full-scale timeline record of this cycle, for mhd, has **no bulk throughput in its
last clean stage**:

| run | `clean` (first) | `clean#2` (last) |
|---|---|---|
| m88 | p99 285.99, **7.31 Gbit/s** | p99 23.51, **none** |
| m90 (two builds) | 146.03 / 25.79, 6.54 / 6.61 Gbit/s | 23.65 / 23.60, **none** |
| m95 | 302.13, 4.10 Gbit/s | 22.54, **none** |
| m102 (**TCP**) | 120.00, 13.54 Gbit/s | 19.54, **12.20 Gbit/s** |

So the sentence this file has been repeating — "the first stage is the SLO failure and
the last stage is fine" — was comparing a stage with four bulk flows against a stage
whose bulk instrument **had died**, every time. The `iperf3` cell reports
`control socket has closed unexpectedly` in the original and in the retry, while the
boundary echo probe answers in 5.5–6.0 ms, and my round-two lenient path kept the
stage's *latency* while recording the loss as a typed absence — which is why the number
looked fine and the row looked green.

TCP's pair is fair (13.54 against 12.20 Gbit/s) and its gap is real: 120 against 19.5 ms.
mhd's pair has to be re-measured before anything can be said about mhd's own first-versus-
last gap, and before condition 1 can be attributed to either.

What the wedged stage looks like from inside, on the server, during `clean#2` of m95:

```
mhd-stats: writes refused, by the bound that refused credit=16 window=269 aged=0 opening=0
```

— the response direction refused by the **peer's credit** sixteen bytes short of its
window, and by its own congestion window, with the age gate silent and no stall line at
all. On the client, in the same seconds, two or three datagrams a second are refused as
`malformed` and one or two as `malformed_decode`, which on loopback with no corruption
means a datagram was written that the decoder rejects. And the sink, for the whole 150
seconds, reports `0.00 Bytes` on all four streams before its idle timeout.

So the bulk flows are wedged end to end while a one-byte request through the same tool
answers in six milliseconds: the server has bytes to send and a window that refuses it,
the client has a stream that is not being read, and the backend has four connections with
nothing arriving. That is the next reproduction to build, and it is a *throughput*
defect in the carrier, not a harness one.

### The first clean stage fails for TCP too, so it is not mhd's (2026-11-05)

The comparison objective condition 2 asks for, on the cell that decides the release,
and the first time this cycle has run it: the same full-scale timeline, the same host
(load 0.73), with `MOLEHILL_DATA_CARRIER=tcp`.

| stage | p50 | p99 |
|---|---|---|
| `clean` (first) | 29.83 ms | **120.0 ms** |
| `clean#2` (last) | **1.22 ms** | **19.54 ms** |

**TCP misses the 50 ms SLO on the first clean stage by the same factor mhd does**, with
the same first-versus-last gap. A yamux-over-TCP data path and an mhd lane share almost
nothing except the topology, the probes and the fact that *a tool is in the path at all*
— so the first stage's tail is not a property of either carrier's design.

Where TCP's version of it lives is measured, because the probe's split was in this run:

| window | connect p50 | echo p50 |
|---|---|---|
| t = 0–25 s | 0.223 ms | 3.036 ms |
| t = 25–50 s | 0.281 | **31.999** |
| t = 50–150 s | 0.28–0.30 | 32.5–34.4 |

`connect` never moves — so this is not the accept queue, not ephemeral-port exhaustion,
not the listener. The **wait for the echo steps up tenfold at t ≈ 25 s and stays there**,
which is a small request queueing behind the bulk spine: the same sentence this file
wrote about mhd, in a different carrier, an order of magnitude larger.

Two consequences for the objective, and they are uncomfortable but they are what the
data says:

- **condition 1 as stated — both clean stages at or under 50 ms — is not currently
  reached by either carrier on this host**, so it cannot be used to judge mhd until it
  is reached by TCP as well. "mhd must pass a cell that TCP fails" is not a carrier
  property;
- the first stage measures **cold start plus the bulk spine**, which the earlier
  `--warmup-rounds 1` diagnostic already showed (34.7 ms against 135.3), and which the
  archive records as a method property. The honest options are to state the SLO on a
  stage that is not the run's first, or to make the *product* avoid accepting visitors
  until it can serve them promptly — and the second is a real change, not a
  measurement, because it is the same for both carriers.

What is mhd's own, and remains: on the warm stage it reads 22.5 ms at p99 against TCP's
19.5, and its arrival performs a per-stream handshake that TCP does not — which is the
11.5 ms term the accounting closed last round, and the reason sealing is still the lever
worth unblocking.

### The plan's stages, as they actually stand (2026-11-04)

The plan was re-cut this cycle: instead of one fixed chain (sealing → RACK/PTO →
scheduler → BBR), it runs four independent work fronts and lets each measurement
say which one is next. Everything below is judged by its own A/B against the
run's own floor, and nothing is called a win because it is cleverer.

| stage | state |
|---|---|
| **A1** load-generator health | **done.** The bench echo backend's two directions are decoupled (16–19 → 22–27 MiB absorbed while its peer reads nothing), and the change's *justification* was refuted on the way: 16 MiB of room means an 8 MiB echo cannot have blocked, so "the backend was the stall" is withdrawn. A permanent check runs it: `bench/src/bench/utils/echo_check.py` |
| **A2** a stage's shape | **done.** Every staged cell now carries `interactive_per_s` (per-second n/p50/p99/max/failures), `interactive_cadence` (the probe's own intervals and missed cadences), the churn rate asked for beside the one achieved, and the window's edges in unix time. `series_logs` names the probes' files instead of pasting every sample into the results file as text |
| **A3** what the gate can see | **done.** The SLO is judged on *held stages* only (`hold_s`), a clean stage with no round trip **fails** instead of passing silently, and a ramp that stops short of `streams_max` fails instead of printing a line. Re-checked against the published `v0.11.2` record, which still reads `gate: PASS` |
| **B** position of the first stage | **done, and it is the carrier's.** One full-scale run, same binary, control and molehill together: control's first `clean` stage p99 **2.57 ms**, molehill's **112.85 ms** (44×), 38× its own p50. The server's `paired_p99_us` 112 454 agrees to 0.4 %. *The follow-up run that was meant to confirm a fix is **void** and is not a regression: its host carried a five-minute load average of **4.74** against **0.84** in the reference run, and its first stage read p50 66.9 ms — that run measured the host, not the carrier* |
| **B2** the harness's four lost stages | **half done.** A failed bulk instrument no longer deletes its stage's arrivals (four cells per run were being discarded, including the last clean stage); the `iperf3` wedge itself after `loss5` is still open |
| **C** no per-visitor handshake | **wired, tested, OFF.** The root is now the handshake's *secret* split salted with its transcript (not the transcript alone). Sealing deadlocks a four-stripe group: 3 of 6 runs pass with it on, 6 of 6 with it off, so it ships behind `MOLEHILL_MHD_SEAL` until that is fixed |
| **D** fewer round trips per arrival | not started; `StartForward` speculation is the cheap half, a server-initiated visitor stream the large one |
| **E** recovery and control | not started. The refusal mix is honest (credit 64 %, window 29 %, age gate 7.5 % at 0.3 scale), and the age gate has since been caught blocking a stream whose flight was ~0 |
| **F** scheduling | **not refuted, and now the strongest remaining lever**: the tail was measured as a small record queueing on a lane carrying bulk (`us == hs_us`, share 1.000), which is what class-aware transmit order exists to prevent |
| **G** release gate | **fails on one thing**: the first clean stage's p99, whose within-campaign spread is larger than the 50 ms limit. Coverage is complete when the harness cooperates |

The gate, measured on the last full-scale run before this cycle's changes:
eight of eight stages measured, `clean#2` 1.89 / 3.91 ms, `clean` 6.06 / **134.1** ms
— the first stage is the only SLO failure and the only thing between this branch
and a release record.

**What the first stage costs, and where**: an arrival's channel spends **its whole
open inside the per-stream Noise handshake** — `us == hs_us` with a share of
1.000 at p50 and p90, `ack_us` zero, and on the server `hs_us == total_us` with
the selector and hello at zero — while credit reads 16.5 MB and the buffer sits
at its ceiling. On an idle lane that handshake is 1.8 ms; under bulk its median
among the slow openings is **11.5 ms** and its p99 128.7, because two small
records have to be scheduled on a lane that is carrying megabytes. The count of
round trips is what sealing deletes, and *when* those records get the wire is
what scheduling owns.


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

### Retraction: the age gate's refusals were counted wrong (2026-11-03, round 35)

The entry that stood here said the age gate refused 4.6 million writes against 0.87
million by the congestion window, and called it the dominant bound. **That number is
wrong and is withdrawn.** The label was read back from the previous call's
`last_refusal`, and `Some(Aged)` short-circuited: once the gate had refused one write,
every later refusal — on any stream, of any kind — was labelled `Aged`. The counter
measured "refusals after the first aged one", not the gate.

The label is now built for the call it describes, and the gate's own input is logged
once a second: how many packets the recovery table holds and how old its oldest
unacknowledged one is. That separates a gate doing its job (a real hole older than the
timer) from a gate held open by a record nothing will retire — and the first samples
of the fixed counter show the gate firing with **1 580 packets in the table and the
oldest at 200 ms**, which is exactly `MIN_RTO`: the gate working as designed, not
stuck.

**What this costs the record.** Two of this cycle's entries leaned on that number: the
"dominant bound" claim and the experiment it justified. The revert of the per-stream
scoping stands on its own measurements and stays; the claim that the gate dominates
does not.

**A lesson worth its own line**: the instrument bug was invisible because the counter
was *plausible*. A number that large was read as a finding — "the hidden bound is the
big one" — instead of as a bug report about the instrument. The repository's own rule,
"re-check every consumer after reshaping data", did not catch it because the reshaping
was in the label, not in the data.

### First green gate, and the refusal mix measured honestly (2026-11-03)

With the refusal label fixed, a 0.3-scale `timeline` — every stage measured, none
failed — gives the proportions the broken counter had inverted:

| bound | writes refused | share |
|---|---|---|
| the peer's credit | **565 039** | 64 % |
| the congestion window | 255 688 | 29 % |
| the age gate | **66 251** | 7.5 % |

**The age gate is the smallest of the three, not the largest.** The dominant bound is
the peer's reader, which is what `credit_blocked` said all along and what the earlier
1 492-of-2 282 ratio suggested before the label went wrong.

And the run itself:

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.17 ms** | **22.3 ms** |
| `rtt100` | 402 | 403 |
| `loss1` | 42.0 | 1 054 |
| `loss5` | 403 | 1 460 |
| `rate100` | 82.3 | 103 |
| `rate20` | 163 | 164 |
| `jitter` | 81.6 | 109 |
| `clean#2` | **2.18 ms** | **23.0 ms** |

**`bench gate`: PASS** — the first green gate of this effort. Both clean stages are
inside the 50 ms SLO by a factor of two on the median and by more than a factor of two
on the tail, and the coverage is complete.

Two caveats, both stated by the gate itself and neither to be dropped: the timeline ran
500 s, which is **under the 900 s a drift slope needs**, so the drift axis is only
partly gated; and this is scale 0.3. Every full-scale run this cycle has lost between
one and five stages, so the 0.3 → 1.0 transition is the open question, not the SLO.

### What the gate refuses now is the first stage's tail, and nothing else (2026-11-04)

A full-scale run after the refusal fix:

| stage | p50 | p99 | |
|---|---|---|---|
| `clean` (first) | 22.0 ms | **135.3** | SLO fail |
| `rtt100` | 402 | 422 | |
| `loss1` | 42.0 | 1 068 | |
| `loss5` | 402 | 1 463 | |
| `rate100` | 84.8 | 105 | |
| `rate20` | 359 | 397 | |
| `jitter` | — | — | failed, instrument |
| `clean#2` (last) | **1.74 ms** | **2.41 ms** | |

Seven of eight stages measured, and **`clean#2` — which failed in every recent full
run — is now the best cell in the schedule**. The one SLO failure is the *first*
stage, whose own last-stage twin reads 1.74/2.41 in the same code and the same run.

That fixes the target precisely: the first stage's arrivals queue behind the run's own
start-up — pool establishment, the control session, the bulk spine's first flow — and
a queue is a scheduling problem, not a handshake one. The parked sealing patch would
have removed the handshake entirely (1 771 µs → 14) and is still parked; the remaining
route is Stage 4, whose subject is exactly this: an opening must not wait behind bulk.

`jitter`'s single loss is the instrument's (`control socket has closed unexpectedly`),
one stage in eight, after `reap_iperf`. That is the harness's remaining leak, and it is
separate from the carrier.

### The sealed striped failure is the half-close grace, and it is not deterministic (2026-11-04)

The previous entry read a repeat at exactly 465 920 bytes as proof of a deterministic
defect. **It is not deterministic**: a third run of the same tree failed at
**5 184 512** bytes. The "same byte" was a coincidence of two runs, and the claim is
withdrawn.

What the trace does say is better. On the failing run, with `molehill=debug`:

```
stripe send direction finished, frames and bytes seq=257 bytes=8388608
stripe reader ended at a frame boundary  last_seq=Some(253..256)
stripe receive direction finished after 257 frames
stripe half-close grace expired; cutting the other direction      <- seven seconds later
Error: echo service closed after 5184512/8388608 bytes
```

Read carefully, that trace says less than it seems and something important:

- **One** direction logged `seq=257 bytes=8388608`; the log does not say which, and the
  other direction logged only `finished after 257 frames` with its readers ending at
  `last_seq` 253–256. So at least one direction's readers ended **before the last
  frame**, which is a gap the reorder queue cannot fill.
- The grace is **10 s, started when the first direction ended** (17.93 → 28.04 in the
  trace), and it does not drain: it aborts the other direction outright.

So the two candidate mechanisms are now distinguishable and neither is guessed: a frame
that never arrived on its stripe (leaving `last_seq` short), or a direction aborted at
the grace with frames still buffered. What is certain is that a close overtook data,
which the archive already states as binding — "FIN/reset/close must not overtake queued
data".

**The instrument that separates them is one counter per direction**: frames expected
(the highest `seq` seen plus one) against frames delivered in order, logged when the
grace fires. A gap in one stripe and a truncated direction look identical in the
visitor's byte count and completely different in that number — and the fix differs
with it: a drain predicate on the grace for the second, a missing-frame repair for the
first.

**And a correction this cycle has now made twice**, worth stating once: a single repeat
is not a mechanism. Two runs at the same byte looked like determinism; a third run
dissolved it. The instrument that would have shown it is a repeat count, and the rule
is to run the failing test three times before naming a number.

### The sealed striped failure is intermittent, and a direction ends after one frame (2026-11-04)

Three runs of the same tree, in a row, on the nine-second test:

| run | outcome |
|---|---|
| 1 | **passed**, 56 s |
| 2 | failed at **465 920** of 8 388 608 |
| 3 | failed at **465 920** |

So it **can pass** — the design is not broken, the layer has a race — and when it fails
it lands on the same byte. That is the shape of a start-up ordering defect, not of a
missing feature, and it is the first time this work has produced a *pass*.

What the trace adds:

```
stripe receive direction finished after 1 frames
Error: echo service closed after 465920/8388608 bytes
```

A group's receive direction ends after **one frame**, and the visitor is left with
465 920 bytes — which is a kernel socket buffer, not a coincidence: the tool read
almost nothing before the group closed under it. The half-close grace never fired in
these runs (no `grace` line at all), so the loss is not the grace and not a truncation
this time: a direction reached its own clean end far too early.

**465 920 is therefore the size of an untouched receive buffer**, and the number to chase
is not it but *who ended the group after one frame*. The instrument is in place: the
receive direction now tallies frames seen and frames delivered, and the send direction
tallies frames and bytes, so a group that ends after one frame while the visitor still
has megabytes queued is a single line away.

### Two failure modes, told apart by the group id (2026-11-04)

With the group id on every lifecycle line, three runs of the sealed striped test say
what one line could not:

| run | outcome | what the groups did |
|---|---|---|
| 1 | **passed** | group 6 sent 258 frames / 8 388 608 bytes and received 258 |
| 2 | failed at **465 920** | groups 1 and 2 each carried **one frame of 4 bytes** — the pool probes — and no group carried the transfer at all |
| 3 | failed at **4 939 776** | group 3 sent 8 388 608 and group 4 received 257 frames, then group 4 sent 257 back; the response stopped at 4.9 MB |

**So there are two defects, not one.**

- **Mode A — the transfer's group never starts.** The visitor is accepted and its socket
  buffer fills with 465 920 bytes while the only groups in the log are the pool probes.
  Whatever the server paired is not the group that carries the visitor.
- **Mode B — the response truncates part-way.** The request completes (8 388 608 bytes
  across a group), and the response's own send direction stops at 4.9 MB with no gap in
  its receive direction (`frames == seen`), so the *source* ended: the client's read of
  the backend returned EOF early.

Both are start-up ordering defects in the layer, both reproduce in nine seconds, and
they need different instruments: mode A wants a line at the pairing's end carrying the
group it spawned, and mode B wants the client's send direction to say *why* its source
ended — EOF from the backend socket, or an error.

**For the record**: this test passes about one run in three. It is not a broken design
and it is not a fixed byte; it is a race with two outcomes, which is why single runs
have misled this file twice.

### The visitor's group exists, and its send direction never carries a byte (2026-11-04)

Both instruments landed, and one more thing was needed to read them: **the group id
collided between the two ends**, because both processes count groups from one and this
test runs both in one process. The server now tags its groups `(1 << 32) | group`, and
with that the three runs say the same thing:

```
DEBUG stripe: stripe source ended: eof group=1 frames=1              <- client, 4 bytes
DEBUG stripe: stripe receive direction finished group=4294967296 frames=1 seen=1
DEBUG ...: pair_striped_group: Visitor paired with a striped group group=1 stripe_count=4
Error: echo service closed after 138240/8388608 bytes
```

Read as roles rather than ids:

- the **client's** group sent one 4-byte frame and its source — the backend socket —
  reached EOF, so the backend had four bytes to echo and nothing more;
- the **server's** group (`4294967296`) exists, was paired with the visitor, and its
  receive direction saw **one** frame;
- the server's send direction **never logged a finish**, and the visitor's socket held
  138 240 / 990 208 bytes when the test gave up.

So the server read nothing from the visitor: its send loop is parked on the first frame
it tried to write into a stripe. That is a *bounded* statement — the pairing is done,
the group exists, and the first write blocks — and it points at the one thing that can
block it: the peer's group had not started reading yet, so the frame waits on a stream
nobody is draining.

**That is exactly what a lane-level root would fix**, and it is the last mile of the
parked design: with the handshake removed the peer's group starts in microseconds
instead of after a KK, so the window in which the writer blocks closes. The next attempt
should therefore **measure the gap between the server's first write and the client's
`group started`** — one timestamp each — rather than add another theory.

### The sealed stream ends with megabytes unaccounted (2026-11-04)

The previous entry read a partial delivery as a close that resets a connection. **A
clean EOF refutes it** — the visitor's read returned zero, not an error — and the byte
counter added this round replaces the guess with a number from each end:

```
stripe receive direction finished group=3 frames=257 seen=257 written=8388608   <- client to its backend
stripe send direction finished group=2 frames=257                                  <- client sent 257
stripe half-close grace: what the receive direction had left
        highest=167 delivered=167 missing=0                                        <- server, at the cut
Error: echo service closed after 5464064/8388608 bytes
```

Read across, that is:

- the **client** wrote all 8 388 608 bytes of request into its backend, so the request
  crossed the carrier intact;
- the client then sent **257 frames** of response;
- the **server's** receive direction had seen **167** when the grace cut it, and
  `missing=0` says it had delivered every frame it ever saw — the last ninety frames
  were never seen at all;
- 167 frames is 5 472 256 bytes, and the visitor saw 5 464 064. The arithmetic closes.

So the loss is inside the **sealed stream**, not in the stripe layer's ordering: 90
frames' worth of bytes were accepted by the sender's carrier, never arrived at the
receiver's, and the stream then ended **cleanly** — which is why nothing counts a gap,
a rejection or a retransmission. This is the third appearance of one rule in this file:
**a close must not overtake queued data.** Here it is the close of a *sealed stream*
that overtakes its own send buffer.

**The instrument for the next round is in `Conn`, where both ends are already known**:
at stream close, log the stream's `written`, `sent` (highest offset put on the wire) and
the peer-side `arrived` from the last acknowledgement. A stream that closes with
`sent > arrived` is this defect, named, on the exact stream — and it is one line per
stream while the stats switch is on.

### Two causes refuted, and the arithmetic that is left (2026-11-04)

The close line landed and refuted the first candidate on three runs: **every stream
closes with `unsent = 0`** — `written == next` — so nothing is dropped from a send
buffer, and the close does not overtake its own queue.

```
mhd stream closed stream=5 written=2097800 next=2097800 unsent=0 reset=None
    peer_fin=false local_fin=true
```

The second candidate was the FIN's ordering, and it is refuted by reading the code:
`poll_transmit` tries credit, then **stream data**, then reset, then **fin** — so a
finish cannot precede its own data, and `send_run` loops until every span of a run is
accepted (`at += sent`), falling back from GSO to the batched syscall, so a partial send
is not a silent loss either.

That leaves the arithmetic, and it is now the only thing left to explain:

| | bytes |
|---|---|
| request, into the client's backend | **8 388 608** (counted, `written`) |
| response, frames the client sent | **257 × 32 KiB** |
| response, frames the server's reader saw | **167** (`highest = delivered = 167`) |
| response, the visitor received | **5 464 064** |

The request crossed intact; the response lost ninety frames on the way, both ends
reported clean, and the losing end — the sender — reports `written == next`, which means
it believes every byte went out. **So the number that decides it is one nobody has
printed yet: the carrier's `next` on the client's own streams.** If the client's four
streams closed with `written ≈ 2.09 MB` each (8 MiB total), the carrier accepted the
frames and lost them downstream; if they closed at ≈ 1.4 MB each, the stripe layer never
handed the last ninety frames over despite logging 257.

`local_fin=true, peer_fin=false` on every server-side stream also dates the close: the
receiver finished its half before the peer finished sending, which is what let the rest
of the response be dropped as "unknown stream" — the path that documents itself in
`apply_datagram` as *"a retired stream"*.

### The grace was cutting a direction that was still working (2026-11-04)

The previous entry blamed `release_unread` for acknowledging discarded bytes. **That was
wrong** and is withdrawn: `RecvBuf::arrived()` returns the *contiguous* frontier and stops
at a hole, and `discard()` does not move it, so releasing unread bytes cannot over-ack.

What the numbers did say is where the bytes stopped: the sender's streams closed with
`written == next == acked ≈ 2.1 MB` each — every byte legitimately received — and the
stripe layer's receive direction had delivered **167 of 257 frames** when the group's
half-close grace **aborted it**. The grace was a fixed ten seconds from the moment the
*other* direction ended, and it did not care whether the surviving direction was still
working. It cut a receive direction in the middle of a transfer; the frames that never
came were then dropped at close by the carrier, correctly, because the stream's own
frontier had never reached them.

**So the grace now waits for ten seconds of no progress**, using the activity clock the
group already keeps for its idle watchdog — which is the condition it was always meant to
describe. Three runs of the sealed striped test after the fix: **two passed**, one failed
at the familiar 465 920 (the mode where the transfer's group never starts at all). Before
it, one of three passed.

That is a real improvement and not yet a cure, and the remaining failure is a *different*
defect — the one the group-id trace isolated as "no group carries the visitor" — with its
own line still to write.

**One more retraction for the pile, and it is the same lesson twice in three rounds**: a
mechanism that fits the numbers (`release_unread` lying) is not evidence that it is the
mechanism. Reading `arrived()` took two minutes and would have saved the round. Read the
code before naming the cause; measure before writing the entry.

### The sealed striped test is seven in eight, and the failure is the second visitor (2026-11-04)

With the grace fix in place, **eight runs of the sealed striped test**: **seven passed**,
one failed. That is the first quantitative statement of the sealed path's health, and it
replaces three rounds of "one in three" impressions.

The failing run's trace says exactly where it goes:

```
stripe group started with 4 stripes group=1          <- first visitor, client side
stripe receive direction finished group=2 frames=257 seen=257 written=8388608
stripe receive direction finished group=1 frames=257 seen=257 written=8388608
Visitor paired with a striped group group=2           <- SECOND visitor, server side
stripe group started with 4 stripes group=2
stripe group started with 4 stripes group=3
stripe send direction finished group=2 frames=257
Error: echo service closed after 1243136/8388608 bytes
```

**The first visitor's transfer completes in both directions — 257 frames and 8 388 608
bytes each way.** The failure is in the second phase, the back-to-back visitors the test
exists to check: the second visitor's group starts after the first one's streams have
already closed, and its transfer stops at 1.2 MB.

So the remaining defect is **a group whose stripes are recycled after a previous group's
streams closed** — the pool hands the second visitor channels that were just retired, and
their first frames go nowhere. That is a much narrower target than "the striped path is
broken", and it has its own instrument already: the close line now prints `written`,
`next`, `acked` and `peer_fin` for every stream, so a recycled channel shows as a stream
that closed `peer_fin=false` shortly before the next group's first frame.

### The failing run drops data on the lane's first stream (2026-11-04)

Looping the sealed striped test until it failed, then reading the carrier's own log:

```
mhd dropped data for a retired stream stream=1      (five times, in the failing run)
```

`Conn::open_at` allocates ids monotonically by two and **never reuses one**, so `stream=1`
is the *first* stream this end ever opened on that lane — the one the lane's validation
used. The receiver had retired it; the sender was still writing to it; every one of those
datagrams was dropped, and `apply_datagram`'s own comment says why that is invisible: the
packet is still acknowledged, because a sender that keeps re-sending an unacknowledged
packet counts ten unanswered retransmissions and declares the lane dead.

Reading the carrier's own plumbing corrects the guess that stood here: `CarrierStream` is
`Box<dyn CarrierIo>` and a data channel **is** a stream on the lane, so `stream=1` is
simply the first channel the client ever opened — not a validation stream. And the server
retired it because **its visitor finished**, which is the normal end of a channel: the
server's side of a channel serves exactly one visitor, and when that visitor goes the
stream is retired.

**So the defect is channel reuse across visitors.** The client's pool takes a channel back
after its visitor ends and hands it to the *next* visitor's group, while the server has
already retired that stream. Every frame the new visitor's group writes into it is dropped
by the receiver with five lines at `debug`, and the visitor carries nothing.

That is Mode A as an identity: **a channel outlives its server-side stream**, and the
second visitor — the one the back-to-back phase creates — is the one that pays. The fix is
a rule about ownership: a channel must not return to the pool once its stream is closed;
it must be replaced, and the two ends must agree on when that happens.

**The next step is a decision, not an experiment**: either the first stream is a data
channel on both ends (and the server must not retire it), or it is validation-only on
both (and the client must never hand it to the pool). The measurement that picks between
them is already in place — the eight-run rate — and either choice must take it to eight
of eight.

### A retirement reset made it worse, and is reverted (2026-11-04)

`Conn`'s reset queue was **only ever drained**: nothing pushed into it, so retiring a
stream said nothing to the peer, whose next frames were dropped with a debug line. That is
a real hole, and the fix looked obvious: queue a reset on retirement unless the peer had
already finished.

Measured on the instrument this investigation trusts — eight runs of the sealed striped
test — **it made things worse: four of eight, against seven of eight before.** A reset
tells the peer to forget the stream, and the peer here is a stripe group that is *still
transferring*: the reset truncated transfers that would otherwise have finished.

So it is reverted, per the plan's rule, and the numbers stay in this file. The queue being
write-only is still a defect; the lesson is that "retired" is not one condition. A stream
retired **after both ends finished** needs no reset — telling the peer anything is what
breaks it — while one retired **while the peer may still write** needs a reset, and the
two cases are distinguishable: `peer_fin` says which. The attempt that failed checked
`peer_fin` and still cut live transfers, which means the *retirement itself* was happening
too early in these runs, and that is the thing to fix, not the announcement.

**Eight-run rates so far, all on the sealed striped test**: one of three before the grace
fix, seven of eight after it, four of eight with the retirement reset. The instrument is
that test and the number is the pass count; no other claim in this file depends on it.

### The false acknowledgement is load-bearing, and that is the real finding (2026-11-04)

Two halves of one obvious fix, both measured on eight runs of the sealed striped test:

| change | pass rate |
|---|---|
| none (after the grace fix) | **7 / 8** |
| a retirement announces itself with a reset | 4 / 8 |
| returning a dead stream's window no longer counts as delivery | 5 / 8 |

Both are *correctness* fixes and both are reverted. Their combination is what the design
currently runs on: a stream that ends with unread bytes returns its window **and** claims
delivery, so the peer retires the stream, the channel goes back to the pool, and the next
visitor gets one. Take either half away and streams stop completing — the pool stalls, the
transfer truncates, and the pass rate falls.

**So the defect cannot be patched locally, and five rounds of local attempts are the
evidence.** The sealed path's remaining failure is a *lifecycle* problem: a channel is
created per visitor, its server side ends with that visitor, and the client's pool then
hands the same channel to the next one. Every honest way of ending a stream early makes
that reuse fail; the dishonest way works seven times in eight.

**What the design needs, stated as a contract**: a channel that has served a visitor is
finished for both ends, and the pool must replace it rather than recycle it — with the
replacement opened *before* the next visitor needs it. That is the same shape as the
pre-opened-stock idea this file rejected long ago, and the difference that makes it
legitimate here is that the stock is *per lane, one deep, and refilled by the retreat of a
channel* rather than a fixed pool of four.

That is a design change of its own, with its own measurement, not a patch. Until it exists,
the sealed path stays parked at seven in eight, and this file says so rather than
pretending a local fix is one line away.

### The first stage is cold start, and the cold cost is establishment (2026-11-04)

A diagnostic, not a way to pass: the same full-scale schedule run once with
`--warmup-rounds 1`, so the pool, the control session and the first channels exist before
the measured stage begins.

| stage | cold (no warmup) | **warm (one round first)** |
|---|---|---|
| `clean` p50 / p99 | 22.0 / **135.3** ms | **2.01 / 34.7** ms |
| `clean#2` p50 / p99 | 1.74 / 2.41 | (stage lost to the instrument) |

**With the pool already up, the first stage is inside the SLO** — 34.7 ms against a 50 ms
limit — and it reads like the last stage. So the SLO failure is not the carrier's steady
state and not the handshake's cost on a quiet lane: it is what a *cold* process pays to
establish four lanes, a control session and its first channels while the churn probe is
already arriving.

**Two readings since then sharpen it, and both refute the obvious next step.**

1. **Establishment is not the cost.** The client's own log: control session at **+1.5 ms**,
   `pool-stats: tunnel pool established tunnels=4 established=4` at **+46 ms**. Four lanes
   in forty-six milliseconds, and `establish_initial` already spawns them concurrently in
   one `JoinSet` — so "the lanes are built one after another" was never true, and there is
   nothing there to win.
2. **The first stage is not slow from the start.** Its first two windows read
   `paired_p50` **1 111 and 1 094 µs** with 5 and 24 arrivals; from the third window on it
   is **4 865–13 360 µs** at 34–37 arrivals a second and stays there for the whole stage.
   The break happens about a second in, when the churn probe reaches full rate and the
   bulk spine starts.

So the slow phase is **load**, not cold start, and it is not the arrival rate either: the
last stage carries the same 36 arrivals a second over the same bulk spine and reads
1 166 µs. Something differs between the first minute of a run and the twentieth with the
same offered load, and that difference — not the handshake, not establishment — is what
the next round measures: the same counters (`dwell`, reader park, lock, `credit_blocked`)
compared at minute one and minute twenty, to find which one carries the factor of ten.

That is a measurement, not a theory, and the plan's ban stands untouched: no warmup stage,
no deleted first stage.

### The first minute is credit pressure; the twentieth is reader parks (2026-11-04)

The same counters, from one full-scale run, at minute one and in its last ten minutes:

| counter (server) | minute 1 | t = 600–1 200 s |
|---|---|---|
| send-buffer dwell p50 | 0 µs | 6 µs |
| **reader park p99** | **434 µs** | **40 704 µs** (max **2 001 414**) |
| connection lock p99 | 46 µs | 2 µs |
| **writes refused by credit** | **8 489 / s** | **3 / s** |
| writes refused by the age gate | 1 678 / s | 0 / s |

**The two phases are opposite, and neither is the handshake.**

- **Minute one**: credit refusals at eight and a half thousand per second, tiny parks, no
  dwell. The bulk spine is filling the peer's window and every write — including an
  arrival's — waits on it. That is the phase whose arrivals read 5–13 ms.
- **Late**: almost no credit refusals, and readers parked **40 ms at p99 and two seconds at
  worst**. The window is not the constraint any more; readers are waiting for bytes that
  the peer has not sent yet.

The earlier reading of the same run — "dwell 0, park 54 µs, so nothing is queued" — was
taken from the *first* 120 seconds and averaged over a phase whose parking is genuinely
tiny. It was the refusals, not the dwell, that carried the cost, and they were not in that
comparison at all.

**So the arrival's slowness in the first stage is credit pressure from the bulk spine**, and
`OPEN_RESERVE` — the byte reserve that exists to keep an opening writable — is a *byte*
reserve against a *connection-wide* credit, which is exactly the wrong shape for this. The
next round's change is small and measurable: make the reserve big enough that one opening
flight always fits, and watch `refused credit` at minute one and the first stage's
`paired_p50` together.

### The reserve works, and the first stage is not refused anything (2026-11-04)

A refusal now says whether the stream that was refused was still opening. A 0.3-scale
timeline, refused writes per second by bound:

```
credit=478   window=12 296   aged=128   opening=0
credit=2 915 window=10 317   aged=973   opening=0
credit=2 222 window=8 327    aged=0     opening=0
```

**`opening=0` in every window.** The eight-payload reserve does exactly what it was built
to do — an opening finds room against a window the bulk spine has filled — so the first
stage's slowness is **not** a refused arrival, and the hypothesis that motivated the counter
is dead.

Also worth reading: the bound that refuses most often on a shaped run is the **congestion
window**, not credit and not the age gate — a different picture from the minute-one window
of the run compared earlier, which is why per-window numbers and not per-run totals are the
only ones this file should quote.

**So what is left for the first stage?** Not establishment (46 ms), not credit (0 refused
openings), not the send buffer (dwell 0 µs), not a lock (46 µs). What remains is the
handshake's own round trips spent while the host is busy — and the one measurement that ever
moved it was the worker count, 26 501 µs at four workers against 12 970 at eight, on a cell
whose run-to-run range is wider than the effect.

That makes the next step an A/B, not a change: **eight workers against sixteen, both
binaries in one campaign, interleaved**, judged on the first stage's p99 and `bulk-n`. If
sixteen wins outside the paired noise, the default moves and the first stage improves; if it
does not, the first stage is host-bound and this file says so.

### In flight: eight workers against sixteen (2026-11-04)

**Result — refuted by its own A/A twin.** Channel-open `open_p50` in each arm's first ninety
seconds, half scale, three arms in one campaign:

| arm | workers | first 90 s | last 90 s |
|---|---|---|---|
| A | 8 | 18 108 µs | 1 151 |
| **B** | **16** | **14 298 µs** | 1 149 |
| A/A twin | 8 (same binary as A) | **15 742 µs** | 1 163 |

**The two copies of the same binary differ by 15 %, and sixteen workers is 9 % below one of
them and 21 % below the other.** The effect is inside the run's own noise, so it is not a
result and the default stays at eight. The last ninety seconds agree to within 1 % across
all three arms, which is the other half of the answer: the steady state does not care about
the worker count at all.

**So the first stage is host-bound, as the pre-registration said it would be if the lever
did not win.** The lever that removes the cost is the one that removes the round trips — the
parked sealing design — and nothing in the scheduling or thread count moves it. That is now
measured three ways: establishment (46 ms), refused openings (0), and worker count (inside
noise).

The A/B the previous entry calls for is running as this entry is written: two binaries
built from the same tree, differing only in `DEFAULT_WORKERS` (8 and 16, both `min`-clamped
to the core count), run as three arms in one campaign so the A/A twin measures the same
run's noise. Scenario: full-scale `timeline`. Result file when it lands:
`~/tmp/m61-workers.json`.

The binaries are saved so the comparison can be repeated without rebuilding:
`~/tmp/molehill-8w` and `~/tmp/molehill-16w` (distinct, verified by hash).

**What decides it**: the first `clean` stage's p99 against the run's own A/A floor, with
`bulk-n` inside 15 % as the throughput guard. If sixteen does not win outside that floor,
the first stage is host-bound and the entry above stays as it is — the honest answer, and
the one that has to be written rather than a change made anyway.

### There is no channel reuse in the pool — the third wrong mechanism in a row (2026-11-04)

`StreamLease` holds `stream: CarrierStream`: a **stream**, not a cached connection. Its
`Drop` decrements a counter and wakes the capacity waiters; nothing goes back on an idle
list, because there is no idle list. `open_stream` builds a new stream every time.

So the entry two rounds ago — "the pool takes a channel back after its visitor ends and
hands it to the next visitor" — **is wrong and is withdrawn**, and with it the contract
that entry proposed. That is the third mechanism in this area to be refuted by reading the
code after being written down as a finding (the validation stream, the false ack, and now
reuse). The pattern is clear enough to state as a rule for this file: **an instrument
finding names a symptom; only the code names a mechanism.**

What the evidence actually says, with the reuse story gone: the server retires stream 1
because its own group ended — `pair_striped_group` hands `read` and `write` to
`spawn_group_tagged`, and when both directions finish the group's channels are dropped, each
`StreamIo` calling `close_stream`. The client, still holding its side of that stream, writes
into a stream whose peer is gone, and every such datagram is dropped with a debug line. So
the question is not who *reused* the channel but **which side ended it first, and whether it
was entitled to** — and that is a question about the stripe layer's two directions, which
the frame and byte counters are already placed to answer.

**No code change this round.** The next attempt starts from that corrected question, and it
does not get to skip it: three wrong mechanisms in one area is enough evidence that the
next sentence written here about the sealed path must quote a line of code.

### What the corrected question is, precisely (2026-11-04)

With the reuse story gone, the failing run's numbers read as follows, and all three lines
are from the same run:

- the **client** wrote all 8 388 608 bytes of response into its four stripe streams, and
  every one of them closed with `written == next == acked` — the carrier put them on the
  wire and the peer acknowledged them;
- the **server's** receive direction delivered **167 of 257 frames** and then ended,
  `missing = 0`, which means it delivered every frame it ever *saw*;
- the visitor received 5 472 256 bytes, which is 167 frames, and the arithmetic closes.

So the bytes were sent, acknowledged, and not read by the layer that was supposed to read
them. A FIN cannot overtake data here: `fin_sent` is only set once `unacked == 0`, and
`poll_transmit` offers stream data before fins. That leaves **the server's stripe readers
ending before their streams were drained**, and the instrument for that is per-stripe at
`recv_loop`'s end: bytes read against the stream's `written` and `acked`, one line per
stripe.

If those agree, the layer read everything and its frames went elsewhere. If they do not, a
reader stopped with bytes still available — and that is a reader-park or a wake problem in
`lane.rs`, not in the carrier's close path. Either way the next entry quotes a line.

### `just check` cannot run while the advisory database is unreachable (2026-11-04)

`just check` stops at its first step: `cargo audit` fetches
`github.com/RustSec/advisory-db` and the fetch fails. The audit itself passes offline —
`cargo audit --no-fetch` loads 1 403 advisories from the cached copy and finds nothing — and
the rest of the chain was run by hand: `cargo fmt --check` clean, `cargo clippy
--all-targets` with no warnings, the library suite 344 passed, the tree clean.

So the tree is sound and the gate is blocked by the network, which is what this file has
recorded before. Nothing about the carrier depends on it, but no commit should be described
as "gate-green" today without saying which half of the gate ran.

### The request crosses and the response never starts (2026-11-04)

The trace could not be read until the group id carried the **role** — both processes number
their groups from one, and the parked patch had lost the top bit the client/server split
needs. Restored, one failing run reads:

```
server group=4294967297  send direction finished, frames and bytes seq=257 bytes=8388608
client group=2           receive direction finished frames=257 seen=257 written=8388608
Error: echo service closed after 450560/8388608 bytes
```

- the **server read all 8 388 608 bytes** from the visitor and put them on the stripes (257
  frames);
- the **client read all 257 frames** and wrote 8 388 608 bytes into its backend;
- **the response direction never appears at all**: no `send direction finished` for the
  client's second group, and the server's receive direction never logs either.

So the request crosses in full, both ends agreeing. The response — the backend's echo back to
the visitor — never starts. Ten seconds later the group's grace expires, drops the visitor's
write half, and the test sees a clean EOF after 450 560 bytes.

**That is the whole defect, and it is in the response direction's *start*, not its end.** The
client's `send_loop` for that group is alive (its `recv_loop` for the same group finished) but
nothing it reads is reaching the stripes; no `stripe source read waited` and no `stripe write
waited` line appears, so neither its source read nor its stripe write parked for twenty
milliseconds — which is only possible if it is not being polled at all.

**The next instrument is therefore one line in `send_loop`**: log the frame index and the
elapsed time on its first iteration, and again whenever it goes a second without a frame. A
direction that never logs its first iteration is a task that was never polled, and that is a
different fix from one that reads and writes and loses the bytes.

### The sealed path has more than one remaining defect, and that is the answer to "how far" (2026-11-04)

The first-frame line answers the question it was built for, and the answer is *both*:

```
stripe send: first frame read group=4294967297 bytes=32768   <- request direction starts
stripe send: first frame read group=2 bytes=32768            <- response direction starts
Error: echo service closed after 3844608/8388608 bytes
```

In this run the response direction **did** start, read 32 KiB and carried 3.8 MB before the
visitor's socket closed — where the previous run showed no response line at all. So the sealed
path's remaining failure is not one defect: there is at least one in which a direction never
starts and one in which it starts and stops early, and they appear in different runs of the
same build.

**That settles how far this arc is from landing**: not one fix away, and not measurable as a
single rate until each shape is separated. The instrument to separate them exists now (role-
tagged group ids, first-frame lines, per-direction frames/bytes, the grace's residue), and the
work of chasing each shape is its own arc — one this cycle has already spent twenty-five
rounds on and cannot finish by continuing to patch the same test.

**The decision this file records, and why**: the sealed design stays parked. What it proved is
real (handshake 1 771 µs → 14, `churn-16` p99 −63 %), its cost is real (bulk −62 %), and its
blocker is a channel-lifecycle problem with at least two shapes. Landing it needs its own
cycle with the instruments now in place, not another patch inside this one.

**What this cycle leaves behind instead** is on the current tree, all measured and all
independent of sealing: the credit-wedge fix (five stages restored, `clean` 78 → 3.5 ms), the
grace fix, the per-stream sealing rule and prologue that made the diagnosis possible, the
worker default, four harness fixes, and a 0.3-scale sweep that passes `bench gate`.

### In flight: does the tool still answer when a stage's instrument fails? (2026-11-04)

The retry re-measures a stage whose `iperf3` client died, and in several runs the retry failed
the same way — which says the path was still broken seconds later but not *what* was broken.
The harness now asks, before the retry changes anything, whether the tool still answers on the
port the stage failed to carry on, and records it in the sample's evidence: reachable means a
carrier that stopped forwarding, unreachable means a listener that went away.

A full-scale sweep is running with that in place (`~/tmp/m63-reach.json`). It is the only
remaining gate failure that does not depend on the parked sealing design, so it is the one worth
finishing first: every lost stage is a cell the gate reports as missing, and until the harness
can say which of the two it is, each loss costs a round to investigate.

### Two bench chains at once destroys both, and a clean 0.3 sweep passes (2026-11-04)

The full-scale sweep launched last round lost **every** stage — `iperf3 exit -9: no output` on
all eight, the new reachability field absent, and in its log `Cannot open network namespace
"mhbsrv"`. The cause is not the carrier and not the harness code: the worker-count A/B from the
round before was **still running**, and its teardown deleted the namespaces the sweep was using.
Two chains overlap, both die, and the one that survives writes a record that means nothing.

That rule is already in this file ("do not run two bench chains at once"). This round broke it,
and the cost was a 22-minute run and a round. The lesson is not new, only re-earned: **check for
a running chain before launching one**, and treat `Cannot open network namespace` as proof that
*another* run owns the host, not as a reason to retry.

On a clean host, immediately after: a 0.3-scale sweep with the new probe in place —

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.74 ms** | **44.4 ms** |
| `rtt100` | 402 | 423 |
| `loss1` | 42.4 | 1 053 |
| `loss5` | 402 | 1 670 |
| `rate100` | 84.9 | 106 |
| `rate20` | 170 | 217 |
| `jitter` | 81.5 | 109 |
| `clean#2` | **2.20 ms** | **22.4 ms** |

**`bench gate: PASS`** — every stage measured, both clean stages inside the SLO with room, and
this is the second independent pass at this scale. The reachability probe did not fire because
nothing failed, which is the correct way for it to be idle.

**So the harness is sound and the carrier passes at 0.3 scale.** What remains is the full-scale
run, and it must be launched when nothing else is running.

### The probe's answer was thrown away with the cell it was recorded on (2026-11-04)

A full-scale sweep on a verified clean host reproduced the familiar shape — `clean` 58.1 / 281.1
ms, then **five stages lost from `loss5` onward**, each retried and each failing again — and the
new reachability field was **absent on every one of them**, even though `retried_after` was set.

That is an instrument bug, not a measurement: `_one_stage` recorded the answer on the first
attempt's cell and then **returned the retried cell**, discarding the first one and its evidence
with it. The facts are now copied across before the retry's cell is returned.

**So the round's datum is still missing, but the shape is not**: five consecutive stages after
`loss5` fail, and their retry fails the same way, on a host where nothing else was running. That
is deterministic and it is the last gate failure that does not depend on the parked sealing
design.

### One gate failure left, and the first stage passes at full scale (2026-11-04)

A full-scale sweep on a clean host, with the evidence fix in place:

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.27 ms** | **20.0 ms** |
| `rtt100` | 402 | 404 |
| `loss1` | 42.4 | 1 068 |
| `loss5` | 403 | 2 477 |
| `rate100` | 177 | 199 |
| `rate20` | 360 | 402 |
| `jitter` | 82.1 | 108 |
| `clean#2` | — | — (lost) |

**`bench gate`: 1 failure** — the coverage of `clean#2` and nothing else. The SLO passes at full
scale, the first stage reads 2.27 / 20.0 ms against a 50 ms limit, and every shaped stage is
measured. That is the best gate state of this whole cycle: five failures, then two, now one.

**And the probe answered**: the lost stage records `reachable_after_failure = True`. The tool
still accepts on the port it just failed to carry on — so it is not a dead listener, it is a
carrier that stopped forwarding for one stage and then kept accepting connections. The retry
re-measured and failed the same way, so the state lasts longer than one stage.

**That is the last gate item, and it now has an address**: a lane that stops carrying after
roughly twenty minutes of run while its socket still answers. The instruments for it exist — the
credit refusals, the age gate, the reader parks, and the close line — and the one counter this
run can name is that the failure comes *late*, in the stage that follows the last shaped one.

### The lost stage is a bulk-only silence, with every instrument idle (2026-11-04)

The counters from the run whose only failure was `clean#2`, over the last 160 seconds — the
stage that was lost:

| counter | server | client |
|---|---|---|
| writes refused by credit | **0** | — |
| writes refused by the age gate | **0** | — |
| writes refused by the window | 24 / s median | — |
| `mhd stalled` lines | **0** | **0** |
| `credit_blocked=true` | **0** | **0** |
| reader park p99 | 46 930 µs (last 447 743) | 48 484 µs (last 560 943) |
| send-buffer dwell | 9 µs | — |
| pairing p50 | **1 357 µs** | — |
| visitors whose forwarding read ran / returned bytes | **5 760 / 5 758** | — |

So nothing in the carrier is stuck: no refusal, no stall, no credit block, dwell of
microseconds, visitors pairing in 1.4 ms, and *every* visitor's first read returning bytes.

**And the bulk transfer carried nothing for the whole stage.** The churn probe kept working
through it — thirty-six arrivals a second, paired in a millisecond — so the tool was serving
small connections perfectly while one bulk flow moved zero bytes. Both ends' readers park on
empty because there is nothing to read: the bulk data never entered the tool.

That is the shape to chase, and it is narrower than any hypothesis this file has carried: **not
a lane that stops, but a flow that never starts, on a tool that is otherwise healthy.** The next
instrument is a per-visitor byte count with the worst one reported per window — `fed` says a
visitor's first read returned *something*, and the question is now how much, and for how long it
stops growing.

### The starved-group counter names iperf3's control connection, not the defect (2026-11-04)

The counter found its thirteen groups and each now names itself:

```
mhd-stats: a group ended having carried almost nothing bytes=972 alive_ms=152079
mhd-stats: a group ended having carried almost nothing bytes=963 alive_ms=123809
mhd-stats: a group ended having carried almost nothing bytes=966 alive_ms=122305
```

**972 bytes over two and a half minutes is `iperf3`'s control connection**: the cookie
handshake, and then an idle socket for the length of the test while the data flows on four
other connections. So the counter is counting a connection that is *idle by design*, and its
"13" was a coincidence of arithmetic, not the defect. **It is withdrawn as a defect signal**;
it stays as a lifecycle count, which is all it ever was.

Two more readings from the same run, and both matter more than the counter:

- **Zero** `stripe write waited`, **zero** `stripe source read waited`, zero `stripe receive
  stalled` — and that is not evidence of health. Those lines are logged *after* a read or write
  returns, so a loop parked for ever logs nothing at all. A silent instrument and a healthy
  path look identical in a log read this way, which is the fourth time that has cost a round.
- The gate is at **one or two failures depending on the run**: `clean#2` was lost again, and
  `clean`'s p99 read **261.7 ms** on this run against 20.0 on the previous one, same binary.
  The first stage's p99 across runs is now known to span **20 to 262 ms** on this host, so the
  SLO verdict is a property of the host's moment, not of the carrier.

**What the file should do next, and does not need another sweep to say**: `clean#2` is the
*last* stage and has failed in nearly every full run. A stage that fails because it is last is
not a stage-level defect; it is something about the run's end, and the only instrument that can
see it is one placed at the *stage boundary* — the pool's state and a single served visitor —
rather than inside the carrier, where every counter reads clean.

### The boundary now asks the tool to serve a visitor (2026-11-04)

Five rounds of instruments inside the carrier have each ended the same way: the counters read
clean while the stage carries nothing, because a loop parked for ever logs nothing and a stage
that never gets data never moves a counter. So the question moved outside the tool.

Before every stage's condition and before its own traffic, the harness now connects to the
**echo** service through the tool, sends one byte, reads the echo back, closes, and records the
outcome and the time in the sample's evidence (`echo_probe`). One connection, three seconds at
most, no effect on the stage it precedes.

That is the instrument the last round's entry asked for: at the boundary, `ok` in milliseconds
means the tool was serving visitors and the stage's own client is what failed; `fail` or `eof`
means the tool had stopped, and then the carrier's silence is the thing to explain. **`clean#2`
is where it will be read first**, because that stage fails for being last and nothing inside it
can say why.

### The tool serves a new visitor while one flow stops delivering (2026-11-04)

The boundary probe answered on its first full run, and the answer is not the one the exit was
built for:

| stage | outcome | boundary probe | stage |
|---|---|---|---|
| `clean` | measured | **ok, 5.84 ms** | 3.45 / 56.2 ms |
| `rtt100` | measured | ok, 406 ms | 402 / 403 |
| `loss1` | measured | ok, 45.4 ms | 42.1 / 1 092 |
| `loss5` | measured | ok, 406 ms | 403 / 2 448 |
| `rate100` | **lost** | **ok, 85.8 ms** | — |
| `rate20` | **lost** | **ok, 167 ms** | — |
| `jitter` | **lost** | **ok, 91.5 ms** | — |
| `clean#2` | **lost** | **ok, 5.49 ms** | — |

**Every failing stage answered a complete visitor round trip at its own boundary** — `clean#2` in
5.5 ms on a clean path — and then carried nothing. So the tool was not wedged, its pool was not
empty, and its listeners were not gone: **it served a fresh connection perfectly and then failed
to carry one bulk flow.**

That kills the last shape this file was carrying ("a lane that stops") and replaces it with a
narrower one: **a flow whose source stops delivering while the tool keeps serving everyone else.**
The send loop's own log could never say so, because every "read waited" line is written after the
call returns — a read that never returns prints nothing. It now reads under a two-second deadline
and records a park with the group, its frames and its bytes, so the next lost stage will name the
flow instead of the silence.

**Where the gate stands**: `clean` 3.45 / 56.2 ms (inside the SLO at the median and just outside
at p99), four stages lost. The four are the ones after `loss5`, which is the run's shape whenever
it goes wrong — and now the instrument to read them is in place.

### Eight of eight measured, and the gate is down to the SLO alone (2026-11-04)

A full-scale run with the read deadline in place:

| stage | p50 | p99 |
|---|---|---|
| `clean` | 6.06 ms | **134.1 ms** |
| `rtt100` | 402 | 462 |
| `loss1` | 42.2 | 1 073 |
| `loss5` | 402 | 1 700 |
| `rate100` | 84.8 | 96.6 |
| `rate20` | 170 | 192 |
| `jitter` | 80.8 | 107 |
| `clean#2` | **1.89 ms** | **3.91 ms** |

**`bench gate`: 1 failure — the SLO on `clean`'s p99, and nothing else.** Every stage measured,
the coverage complete, and the last stage reading 1.89 / 3.91 ms. That is the first run this
cycle in which the gate's only complaint is the number the plan says may not be relaxed.

**And the read-park instrument is honest about its own limit**: 579 parks across **13 groups**,
every one of them at `bytes = 166` and `frames = 2–3`, each repeated 61–75 times over minutes. A
connection that carries 166 bytes and then waits is `iperf3`'s control connection, idle by
design — the same thirteen the starved counter found. So the park line names *a* flow that stops
delivering and cannot, on its own, say whether that flow *should* be delivering: an idle
connection and a wedged one look identical to a carrier.

**That fixes what the last mile actually is.** Not "a lane that stops" (the boundary probe refuted
it), not "a flow that stalls" (idle flows park the same way), but **the first stage's p99**, which
spans 20 to 262 ms across runs of one binary on this host and is therefore a property of the
moment, not of the carrier — and the one thing that would remove it for good is the round trips
that the parked sealing design deletes.

### The first stage's own noise is bigger than the SLO (2026-11-04)

Two rounds of one campaign, one arm, one binary, half scale — the closest thing to an A/A this
harness can do for the timeline:

| stage | round 0 | round 1 |
|---|---|---|
| `clean` p50 | **5.25 ms** | **65.97 ms** |
| `clean` p99 | **171.5 ms** | **223.0 ms** |
| `clean#2` p50 | 2.18 ms | 2.17 ms |
| `clean#2` p99 | 3.46 ms | 22.53 ms |

**The first stage's median moves by a factor of twelve between two consecutive rounds of the same
binary**, and its p99 never comes near the 50 ms limit in either. Across every full-scale run this
cycle has collected, `clean`'s p99 spans **20 to 281 ms** — a factor of fourteen — with the same
code.

**So the SLO cannot be decided by one run on this host, and that is a measurement, not an excuse.**
A cell whose own within-campaign spread exceeds its threshold will pass or fail according to which
minute it was measured in. Two consequences the plan has to face:

1. **A defensible release needs the distribution moved, not the tail clipped.** The only lever
   measured to move it is the round trips per arrival — the parked sealing design — because
   everything else (establishment, credit reserve, worker count, scheduling) has been tested and
   refused by its own numbers.
2. **Until then, the honest record is what the gate says on a given run, plus this floor.** Quoting
   a green SLO from one run, or a red one from another, would both be wrong: the file now carries
   the spread, which is the only number that makes either verdict readable.

`clean#2`, for its part, is stable to 1 % at the median across the two rounds — so the *last*
stage is not the noisy one. The noise belongs to the stage that starts a run.

### The sealed failure is the *backend* stopping, not the carrier (2026-11-04)

With the read deadline in place, the sealed striped test's failure finally names its own shape:

```
stripe send: first frame read group=2 bytes=32768
stripe source read has not returned for two seconds group=2 frames=119 bytes=3844608   (x3)
Error: echo service closed after 3844608/8388608 bytes
```

Two things match exactly: **the group read 119 frames / 3 844 608 bytes and then its source stopped
delivering**, and **the visitor received 3 844 608 bytes**. The direction is the *client's* response
path, whose source is the local **echo backend** — the bench's own Python service.

So the flow did not stall inside the carrier: the carrier delivered every byte it was given, and
then waited for a source that never sent the rest. The request side had already crossed in full
(8 388 608 bytes, both counters agreeing), so the backend had eight megabytes to echo and stopped
after 3.8.

**That reframes the whole remaining investigation.** The "flow that never starts" the boundary
probe left open is, at least in these runs, **a bench backend that stops echoing under load** —
and the same backend serves the churn probe, whose thirty-six arrivals a second kept working
because each connection is small and quick. A bulk echo through one core, with the run's own load
on the same host, is a different matter.

**The test that decides it is one line of grep away**: if `stripe write waited` never fires in the
same failure, the client was not blocked writing into the stripes and was genuinely waiting for its
backend; if it does fire, the frames had nowhere to go and the carrier is implicated again. That is
the first thing the next round does, and it costs one nine-second run rather than a sweep.

### The grep answered: the client was waiting for its backend (2026-11-04)

The one-line test the previous entry asked for, on a failure caught in four runs of the rebased
sealed tree:

```
stripe write waited                                  count: 0
stripe send: first frame read group=2 bytes=32768
Error: echo service closed after 793600/8388608 bytes
```

**Zero.** The client's send direction never blocked writing into the stripes, so the frames had
somewhere to go and the loop was genuinely waiting for its **source** — the bench's local echo
backend. That confirms the previous entry's reading rather than the carrier's.

**So the sealed striped test's remaining failures are, in these runs, the bench backend stopping
under load**, not the carrier failing to carry. That does not make the sealed design landable — it
still needs the lifecycle shapes separated and its 8/8 — but it does mean the last several rounds
were measuring the load generator.

> **Retracted below.** The next entry measures the backend and finds it cannot have
> blocked: it accepts 19 MiB with the peer reading nothing. The observation this
> paragraph rests on stands; the inference from it does not.

**PAUSED HERE at the user's request (round 72).** State as left:

- branch `feat/v1.0.0`, tree clean, `just check` green, HEAD `810f913`;
- the parked sealed tree is `~/tmp/stage3-sealing-parked.patch` and applies to today's HEAD with
  one conflict in `stripe.rs`'s send tail, resolved by keeping `note_group` plus the group id;
- the gate's last full-scale run: **eight of eight stages measured**, `clean` 6.06 / 134.1 ms,
  `clean#2` 1.89 / 3.91 ms, **one gate failure — the SLO on `clean`'s p99** — with that cell's
  within-campaign spread measured at **12× at the median**;
- the next step, in one sentence: **give the bench's echo backend enough capacity that it is not
  the load, then re-run the sealed test to see whether it is eight of eight.** *(Done, and it
  refuted the premise: see the next entry.)*

### The echo backend is decoupled, and it cannot have been the stall (2026-11-04)

The harness's echo service ran one thread per connection, alternating `recv` and
`sendall`. That shape *couples its two directions* — it cannot accept a byte
while it is blocked writing — and the previous entry read the sealed test's stall
as exactly that. **The coupling is real; the attribution was not.** Measured on
this host, both arms from their parsed source, one run each:

| | bytes it accepts while the peer reads nothing (3 s) | full-duplex 8 MiB |
|---|---|---|
| old: one thread, `recv`/`sendall` | **16–19 MiB** | completes, 0.01 s |
| new: reader → bounded queue (64 × 64 KiB) → writer | **22–27 MiB** | completes, 0.01 s |

The number moves ±5 MiB between runs because it is socket autotuning, so the
honest reading is the *floor*: **every run absorbed more than an 8 MiB transfer
needs, in both shapes**. A backend that could have accepted the whole echo
cannot have been blocked writing it, so "the backend stopped delivering because
it was blocked" is refuted for this transfer — and with it the sentence that
three rounds of carrier work had been spent on the load generator. What the
sealed trace actually says is narrower: the client's response direction was
*reading*, and its source returned nothing for two seconds. Why a source with
that much room had nothing to give is open again.

Three things are kept, each on its own merit rather than on that story:

- the **decoupling**: the queue is worth 4–8 MiB of headroom in every run, and a
  load generator that stalls when its peer is slow is one whose stall will
  eventually be blamed on the carrier;
- an **`ECHO DONE read=… wrote=…` line** for any connection over 64 KiB, so a
  stalled transfer can be attributed by the backend's own counters instead of by
  inference. Small connections stay silent: at four thousand arrivals a second a
  line per connection would make the log the load;
- a **re-runnable check**, `bench/src/bench/utils/echo_check.py` (outside the
  gate, seconds, no root): full-duplex byte equality, the coupling bound, and
  the presence of the announcement.

Its first version reported a log-formatting defect that did not exist: it sliced
`ECHO_SRC` out of the file's raw text instead of taking the parsed constant, so
the child received `\\n` and printed a literal backslash. Fixed, and the reason
is written into the function — it is the same mistake as measuring a copy of the
thing instead of the thing.

Neither is a claim that the sealed path is fixed. The check that would settle it
is on the carrier's side and is still the next round's: for the failing direction,
count bytes *read from the source* against bytes *queued into the group*, so a
source that has nothing and a group that will not take it stop looking alike.

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

### The first stage is the carrier's, and its tail is the per-stream handshake queueing (2026-11-04)

One campaign, full-scale `sweep` schedule, one round, `control` and `molehill` in
the same run, same binary, `MOLEHILL_MHD_STATS=1`. This is the row that decides
whether the first stage is the run or the carrier:

| arm | stage | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| control (no tool) | `clean` (first) | 3 035 | **1.58** | 2.03 | **2.57** | 5.41 |
| control | `clean#2` (last) | 3 034 | 1.57 | 1.90 | 2.28 | 3.32 |
| molehill | `clean` (first) | 2 934 | 2.94 | **15.15** | **112.85** | 244.72 |

**The tool-free path's first stage is 2.6 ms at p99 and does not degrade for
being first.** molehill's is 112.85 — 44× the control on the same stage, 38× its
own p50. So the first-stage failure is a property of the **carrier's arrival
path**, not of the harness, the host, or the position in the schedule. And it is
not "the first stage warms up": its first ten-second bucket already reads p99
**37.7 ms** and every bucket after it reads 80–209, with a flat p50 near 3 ms and
a p90 of 7–21. A stage that began fast and slowed would look nothing like this.
*(The earlier "first two windows read 1 111 and 1 094 µs, then it slows" was
measured on the server's own `paired_p50`, a different instrument on a different
run; this cell's probe disagrees with it and the older sentence is not a reading
of this one.)*

The shape says which part of the arrival to look at, and the carrier's own
counters name it. In the same second the probe reads 112.85 ms, the server's
`visitor arrival, split at the pairing` window reads `paired_p50_us 1 901` and
`paired_p99_us 112 454` — the probe and the pairing agree to 0.4 %, so the tail
is **accept → a channel in hand**, and nothing after it.

Then the split, over every slow (`≥ 5 ms`) opening the client logged in that arm
(1 141 of them):

| field | p50 | p90 | p99 | max |
|---|---|---|---|---|
| the whole open (`us`) | 11.50 ms | 41.22 | 128.72 | 371.74 |
| **the Noise KK inside it (`hs_us`)** | **11.50** | **41.22** | **128.71** | **371.74** |
| the acknowledgement (`ack_us`) | 0.00 | 0.00 | 0.00 | 0.00 |

**The handshake is the entire open — a share of 1.000 at p50 and at p90** — and
the server says the same from its side: of the openings slow enough to log,
`sel_us ≈ 0`, `hello_us ≈ 0`, and `hs_us` is `total_us` to the microsecond. Its
own bound snapshot says this is not a budget: `credit` 16.5 MB, `buffer` at its
32 MiB ceiling, `queued_streams` 0, `retransmits` ≤ 9.

So the cost is **not** the KK's cryptography. On an idle lane the same handshake
measures 1 771 µs. Here its **median is 11.5 ms**, which is the two small records
it exchanges **waiting on a lane that is carrying bulk** — the one thing the
five-millisecond-or-finer instruments inside each stage could never see, because
what they measured was the CPU, and what the arrival pays is the queue.

Two consequences for the plan, and this is the whole point of the row:

1. **sealing (stage C) attacks exactly this**, and now for a measured reason
   rather than a design preference: it deletes the two records and the round trip
   they wait on. The parked patch's own end-to-end A/B put `churn-16` p99 at 3.59
   against 9.74 ms with the handshake at 14 µs;
2. **the scheduler (stage F1) is not made redundant by it** and is not
   speculative either: the measured defect is a small record queueing behind a
   full run, which is what a class-aware transmit order exists to prevent. C
   first, because it removes the work; F1 after, because the remaining small
   records (selector, hello, prologue, ack) will still queue.

**And the harness still eats four stages.** Everything after `loss5` failed with
`iperf3 error: control socket has closed unexpectedly` — `rate100`, `rate20`,
`jitter` and `clean#2`, i.e. no `clean#2` for molehill at all, which is why the
table above has no fourth row. That is the release gate's coverage problem and it
is *not* the carrier: it must be fixed before any sweep can be published.

### The lane root is the handshake's secret, and the transcript only salts it (2026-11-04)

The parked sealing design derived each stream's key from the lane's **handshake
hash**. That is wrong, and it is wrong in the direction that matters: the
handshake hash is a digest of the messages both ends exchanged. Anyone who
watched the handshake — or who holds either static key and can replay the
exchange — recomputes it. A key schedule built on it would move forward secrecy
from per-stream to per-lane and then hand it away, which is worse than the
per-stream handshake it exists to replace, because that handshake was at least
secret.

So [`LaneRoot`](src/transport/mhd/cipher.rs) takes two inputs and documents
why neither alone will do:

- the **raw split** — Noise's own secret output, the exact material
  `into_transport_mode` installs, reached through snow's `risky-raw-split`
  feature (`dangerously_get_raw_split`). Named "risky" by the crate because it
  exports key material; that is precisely what a derivation root needs, and it
  is the reason the feature is the right tool here rather than a re-implementation
  of the state machine;
- the **handshake hash**, as the HKDF **salt** — a public value that binds the
  root to one handshake without ever being derivable from it.

`HKDF-SHA256(salt = transcript, ikm = split[0] ‖ split[1], info = "molehill mhd
lane root v1")` gives the root; `StreamCipher::derive` then derives one
(direction, stream) key from it as before. A test pins both halves: changing
the secret while holding the transcript fixed changes every stream key, and so
does changing the transcript while holding the secret fixed — the regression
that would ship a transcript-only root while still passing every "both ends
agree" test.

Two consequences written down rather than discovered later:
- `from_handshake` answers `Option`, and `None` means **do not seal** — the
  caller falls back to the per-stream handshake instead of sealing under a root
  nobody computed;
- the nonce is the packet number, which is `u32` per connection. A wrap would
  reuse a nonce with the same key, so the sealing path must refuse near the
  wrap (or re-establish the lane) rather than wrap silently. 2³² datagrams is
  ~6 TB, which no test will reach — which is exactly why it has to be an
  explicit refusal and not an assumption.

### The sealed failure is not the harness, and its signature was not what it looked like (2026-11-05)

Two corrections, both from measuring with a detector that cannot be fooled by a
debug line:

- **`mhd transport selector timed out` is not the failure signature.** It appears in
  *passing* runs too — four streams whose selector never arrives while the group still
  gathers enough channels and carries the whole transfer. An earlier entry read it as
  the server waiting on the streams that mattered; it does not say that.
- **the test's echo is not the cause.** With a strict detector (`test result: ok`), the
  sealed striped test passes **3 of 8** with the coupled `io::copy` echo and **2 of 8**
  with the decoupled reader/queue/writer one. Reverted, for the second time and now
  with a detector worth trusting.

So the sealed path fails about five times in eight and nothing about the harness
explains it. The trace of a failing run is consistent from run to run: the client's
response direction — whose source is the local echo — reads a variable number of frames
(123 / 3 975 680 bytes in one run; 455 KiB, 440 KiB, 826 KiB in others) and then
`stripe source read has not returned for two seconds`, forever, while the visitor reads
EOF at that byte. The number is not stable across runs (the three appearances of
465 920 were one batch, not a law), so it is a stopping point rather than a boundary.

What is now excluded for this path: the two never-sent packet numbers (fixed), the
repair path itself (a sealed stream repairs a dropped packet correctly in a unit test),
the harness's echo (measured twice), and the control plane and every arrival stage
(measured in the rounds above). What is left is the stripe layer's own accounting over
sealed streams — where `MAX_SEALED_PAYLOAD` clamps the packet but the frame is 32 KiB,
and the reorder loop's `written` is the byte count the visitor's EOF is measured
against.

### A second never-sent packet number, and the deadlock gets better but not gone (2026-11-05)

The first leak was a refused packet consuming a number. There was a **second**, one
level further in and reachable only on the sealed path, which is why sealing alone
deadlocked stripes: `seal_stream_payload` can fail after `encode_stream` has already
written the header and after the number was taken, and the `?` there abandoned the
packet with its number consumed — a hole in the peer's cumulative frontier that can
never be filled, and every later acknowledgement re-declares everything above it lost.

A lane that cannot seal is now **closed**, where the pool can rebuild it, instead of
continuing with a hole: `note_no_key` counts it and `closed` is set with a reason of its
own. A silent repair loop becomes one lane restart.

Two more things were measured while looking:

- a sealed stream **does** repair a dropped packet. `a_sealed_stream_repairs_one_dropped_packet`
  drops one stream packet off the wire, fires the timer, and asserts every byte lands —
  it passes, so the loop is not in the repair path itself;
- with the fix, the sealed striped test improves from two-or-three of six to **four of
  six**. Not cured. Two of the failures stopped at **465 920 bytes**, which is the same
  number two runs reached earlier in the cycle — three times now for the same value, on
  a transfer whose frame size is 32 KiB, so it is not a frame boundary and it is not yet
  explained.

So sealing is still off. What has changed is that a lane with a hole now says so
instead of looking healthy, and the sealed path's failure rate is halved by a defect
whose mechanism is understood rather than by luck.

### Sealing is wired, tested, and OFF: a striped group deadlocks with it (2026-11-04)

The derivation is now plumbed end to end — `NoiseStream::session_roots`, a lane
root installed on `Conn`, per-stream ciphers, the frame flag, the sealed
`StreamState`, and `DERIVED_SELECTOR` — and it is **disabled by default** behind
`MOLEHILL_MHD_SEAL`, because on the one topology that shares a lane between
several streams at once it hangs. The A/B is six runs of the same test on one
host, the only difference being whether the root is installed:

| `striped_data_channels` (4 stripes, 8 MiB echo) | runs passed |
|---|---|
| sealing **off** (`io::copy` echo, per-stream KK) | **6 of 6** |
| sealing **on** | **3 of 6** |

What the sealed failures look like, from the carrier's own lines:

```
mhd stalled: bytes waiting unsent_ms=212 unacked_ms=213 credit_blocked=false
  backlog_streams=0 worst_stream=5 worst_stream_unsent=950569
  worst_stream_retransmits=0 worst_stream_next=117312 worst_stream_written=950570
mhd window lane=3 cwnd=76893 flight=0
```

`written` 950 570 against `next` 117 312 — 833 KB queued behind a send cursor
that stops moving — with the peer's credit untouched, **no retransmission ever
recorded**, and a congestion window of 63–77 KB against a flight of ~0. The
server, meanwhile, logs four `mhd transport selector timed out` half a minute
later: the streams it had accepted never received their first byte. Nothing was
refused (`unsealed_tags=0`, `no_key=0`, and the two new refusal counters this
round added — `unsealed_no_root`, `unsealed_no_tag`, `unsealed_no_key` — all
read zero on the next capture), so the bytes were **not sent**, not lost.

**That missing counter was itself the defect in the instrument.** A sealed packet
this end cannot open was dropped in total silence: `unsealed_tags` counts *tag
failures*, so a refusal for "no root here" left every counter reading healthy
while the peer waited out a thirty-second deadline. It is now counted apart and
logged once with the stream id, so the next round starts from a measurement
instead of from a timeout.

Two experiments were spent ruling out the neighbours, and both are recorded
because they cost the time:

- **the echo is not the cause.** Decoupling `tests/common`'s echo — the same
  reader/queue/writer shape the bench backend got — moved the failure rate from
  4 of 5 to 3 of 6, i.e. not at all, and the change was reverted;
- **the in-process stripe harness is not an instrument.** The parked patch's
  `a_sealed_stripe_group_carries_a_transfer` stalls at **zero** bytes without
  wake-ups, at **128** bytes with them, and at **384** bytes with sealing
  *disabled* — so it never carried a transfer in any configuration, and it was
  deleted rather than left red. (Its lesson is worth keeping: a harness that
  steps `Conn` by hand must call `wake_readers`/`wake_writers` after every
  datagram, because in production the pump task does, and a missing wake-up reads
  exactly like a stalled layer.)

So the arrival path's largest measured lever is built, proved on the arithmetic
(one handshake per lane instead of one per visitor), and **not shipped**, because
the striped configuration deadlocks and stripe count grows under backpressure in
production. The next round's first job is the deadlock, with the switch as its
reproduction, not a re-derivation of the cipher.

### The arrival's chain is closed, and it lands on the per-stream handshake (2026-11-05)

Three stamps on one wall clock, joined by group id — one `SystemTime` read per event,
gated on the stats switch, which is the only way to time an interval that crosses two
processes. 1 201 groups of one staged cell, each with exactly one request:

| interval | p50 | p90 | p99 |
|---|---|---|---|
| **control hop**: server sends the request → client's loop reads it | **0.056 ms** | 0.107 | **0.219** |
| **hand-off**: client's loop reads it → server pairs the visitor | 1.611 ms | 8.376 | **47.513 ms** |

And the server's own four stamps for the 188 pairings it calls slow:

| phase | p50 | p99 |
|---|---|---|
| `accept → pairing started` | **0.058 ms** | 0.123 |
| `pairing started → request enqueued` | 0.001 | 0.005 |
| **`request enqueued → first channel taken`** | **13.933 ms** | **122.455** |
| `first channel → forwarding command written` | 0.004 | 13.636 |
| total, `accept → paired` | 14.206 | 122.481 |

The two instruments agree to 0.4 % once the populations are matched — the cross-process
hand-off on those same 187 groups reads p50 **13.875**, p99 **122.430**. (The first
version of this entry called that a contradiction; it was comparing the hand-off's
median over *all* pairings with the server's median over the *slow* ones, which is the
population mistake this file keeps warning about and kept making.)

So the whole chain, in one column, for arrivals that are slow:

| | p50 |
|---|---|
| accept → pairing started | 0.058 ms |
| → the request is on the wire | 0.001 |
| → the client's control loop reads it | 0.056 |
| **→ the client has opened the stream and written its prologue** | **11.5 ms** |
| → the server takes the channel (prologue transit + push) | ~2 |
| → the pairing is woken and takes it | **0.004** |
| total | 14.2 |

**Every term is now measured, and the 11.5 ms is the per-stream mhd handshake** — the
same interval that reads `us == hs_us` with a share of 1.000, and the same one that
sealing deletes. The wake is 4 µs, the control plane 56 µs, the accept 58 µs, the
forwarding write 4 µs: none of them can be the tail any more, and no scheduling change
can touch a term that is four microseconds.

That closes the question this cycle spent four rounds on. The arrival is slow because
it performs a Noise handshake per stream, on a lane that is carrying bulk, and every
other explanation is now excluded by its own measurement. **The lever is sealing, and
its only blocker is the striped deadlock** — 3 of 6 runs, a repair loop with
`retransmits=1532`, narrowed but not yet cured.

### The tail is the pairing wait, and the arithmetic finally closes (2026-11-05)

The probe now splits its own round trip at the send. 555 arrivals of one staged cell:

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| the whole round trip | 14.44 ms | 72.78 | **228.56** | 457.28 |
| connect + send | 0.193 ms | 0.271 | 0.452 | 1.403 |
| **waiting for the echo** | **14.19 ms** | 72.65 | **228.40** | 457.05 |

So the tool's accept path is fast and the tail is the wait for the reply. The first
version of this entry guessed the *kernel accept queue* and was refuted in one
experiment: `ss -ltn` inside the server's namespace, sampled 134 times across a run of
the same cell, read `Recv-Q = 0` **every time**. Nothing was piling up.

The same run's server-side windows then close the arithmetic exactly:

| phase (34 one-second windows, median of the window p50s) | |
|---|---|
| `accept → paired` | **10.232 ms** (p99 42.2) |
| `paired → first byte` | 0.022 ms |
| the echo (first byte → reply byte written) | 3.296 ms |
| **sum** | **13.550 ms** |
| the probe's own "waiting for the echo" | **14.19 ms** |

**The pairing wait is the tail** — 10.2 ms of the 14.2 — and everything after it is
microseconds plus a three-millisecond echo. That is the same conclusion the first stage
reached from a different direction (`paired_p99` 112 ms in the full-scale run); what is
new is that it is now the *whole* explanation of a warm cell, not a fragment of a
cold one.

So the arrival's cost is: server accepts, **asks the client for a channel**, the client
opens an mhd stream, the server pairs. The client's own phases are known — dequeue
~0, `spawn → first poll` 4–12 µs, handshake 1.34–1.38 ms — so of the 10.2 ms, about
1.4 is the open and the rest is the **control-plane round trip plus the pairing
hand-off**: the request crossing to the client's control loop, and the channel's
arrival waking the pairing.

That is the last interval in an arrival that has never been stamped, and it is
pre-registered for the next round: emit the server's send time and the client's receive
time for the same `CreateDataChannelFor` group id — both ends are on one host, so epoch
milliseconds are comparable — and split the 8.8 ms between the control hop and the
pairing hand-off. Whichever it is decides between the two remaining structural
candidates, a server-initiated stream (which deletes the request entirely) and a
direct hand-off (which deletes the wake).

### The carrier's whole round trip is fast, and the tail is outside it (2026-11-05)

The interval every previous instrument stopped short of now has a distribution: from
the request byte the server's forwarding loop read to the first byte it wrote back to
that same visitor — the carrier's two directions plus the client and the backend.

| | p50 | p99 | max |
|---|---|---|---|
| a visitor's echo after its own request | **538–708 µs** | 1.2–23.3 ms | 23.3 ms |

In the same rounds the cell reads **p99 104.3, 37.7, 116.5 and 7.6 ms**. The echo's
p99 never reaches the cell's, and its *maximum* in a second is below the cell's p99
over thirty seconds — so for these runs **the carrier's round trip is not where the
tail is**, and everything this cycle built for the arrival is confirmed as fast from
the other side.

What is left is the two boundaries the server cannot see, and they are the probe's own
view of the tool:

- **before the request byte**: the visitor's `connect()` and its `send`, i.e. the
  *tool's* accept path. `accepted_at` is stamped after `accept()` returns, so a
  connection that sat in the kernel's accept queue is invisible to every counter here;
- **after the reply byte**: the probe's own read, which on this host is a Python
  selector and a syscall.

That is a smaller remaining question than the ones this file has been asking, and it is
the first time the answer has been *outside* the carrier for a population this large.
The next instrument follows from it: stamp the listener's accept boundary — the time
between a connection being ready and this end taking it — and compare it with the
probe's own measured round trip. If the accept path accounts for the rest, the SLO is
being decided by the server's accept loop and not by mhd at all, which changes what
the release claim can honestly say.

### Every stage of the arrival is microseconds, and a second population is not (2026-11-05)

The dispatcher's admission of a new lane — registry lock, admission table, `Conn`, a
`Shared`, a spawned sender task, two insertions — costs **p50 27 µs, p99 39 µs**. That
was the last hypothesis about the receiving end, and with it every interval the
arrival crosses now has a distribution:

| interval | p50 | p99 |
|---|---|---|
| a datagram's wait for its stream's accept | 7–8 µs | 12–90 µs |
| a lane admitted inside the receive callback | 27 µs | 39 µs |
| a stream's first byte waiting for the wire (dwell) | 2–11 µs | 0.2–0.5 ms |
| the pump's post to the socket | 10–25 µs | 20–104 µs |
| the client's spawn → first poll (`started_us`) | **4–12 µs** | — |
| the client's dequeue → prologue (`cmd_us − ready_us`) | ~0 | — |
| the per-stream handshake, every open | 1.34–1.38 ms | 1.78 ms |
| the server's pairing (`paired`) | 1.5–1.9 ms | 2–6 ms |
| the server's first forwarded byte (`first`) | 19–22 µs | 37–46 µs |

Add the medians and an arrival costs about **three milliseconds**. The cell it was
measured in reports **p99 44.5, 50.7, 53.9 ms** in its three warm rounds (and 219 ms in
the round that starts the process) — ten to a hundred times the sum of every stage.

**So in these runs the tail is not in the arrival.** It is in what the probe does
after its request has been forwarded: the 64-byte echo has to come back through the client, the
backend and the carrier before the probe can stop its clock, and the probe's sample is
the whole round trip. The instruments above stop at "the server forwarded the first
byte" — which is why a cell can read tens of milliseconds while every one of them
reads microseconds.

That also reconciles the population sizes that have been nagging this file: the slow
*opens* are rare (13 in four minutes, about one arrival in four hundred, at 5–83 ms),
while the probe's p99 is slow in *every* round, so the two cannot be the same
phenomenon. One is a rare handshake, the other a common return path.

It is a statement about the runs that were measured, not about the carrier in general:
an earlier full-scale run had a server-side `paired_p99` of **112 ms**, i.e. a tail
inside the arrival. The two runs disagree about *where* the tail is because the tail
moves — which is the same lottery this file keeps meeting, seen from the other side.

**Pre-registered next instrument**: split the probe's own round trip at the boundary
this cycle has never timed — from the forwarding of the request to the visitor's echo
being read back. On the server that is `first_*` plus the return; on the client it is
the response direction's own path. The gate is the same as always: the interval's
distribution, then whatever it names, judged on its own A/B.

### The tail is a transport delay, and the timer only marks its boundary (2026-11-05)

The accept queue is not the wait: a stream waits **p50 7 µs, p99 12–14 µs** between
the datagram that names it and the loop that takes it (new instrument, reported as
`a stream waiting for its accept`). So the second of the two intervals this carrier
could not see is also fast.

The first version of this entry read one log line as "a fresh lane pays the
small-flight timer" — `window=13490` is `INITIAL_CWND`, so the lane had carried
nothing. **That inference is wrong**: `window` is `cwnd − flight`, and a lane whose
congestion window has stayed near its initial value reads the same number after a
megabyte. What the whole population says is better, and is the reason this entry was
rewritten rather than extended. Every slow open in one run, in order:

| open | remaining window | retransmissions |
|---|---|---|
| 5.5 / 6.0 / 10.9 ms | 21 584 / 13 490 / 22 933 | **0** |
| 15.2 / 15.8 / 16.7 / 20.6 ms | 13 490 each | **0** |
| 23.1 / 23.4 / 23.4 / 23.6 ms | 13 490 each | **1** |
| 27.6 / 33.4 / 39.7 / 45.6 / 66.4 / 83.4 ms | 13 404–20 144 | 1–3 |

Read down the middle: the opens climb from 5.5 to **20.6 ms with no retransmission at
all**, and only then does a cluster appear at 23+ with exactly one. So the
retransmission is not the cause of the tail — it is the *marker* of the 20 ms
`SMALL_FLIGHT_INITIAL_RTO` boundary, adding a duplicate and three milliseconds to
opens that were already late. **The delay itself is in the handshake's transport, and
it happens before any timer fires.**

That is a smaller claim than the one it replaces and it is the one the numbers
support: between 0.5 and 20 ms an open is slow with nothing lost and nothing refused
(`credit` 16.7 MB, `queued_streams=0`, `retransmits=0`), while every instrument the
carrier owns — dwell, post, accept, lock, park — reads microseconds.

**Pre-registered next experiment**: the remaining candidate is the dispatcher's own
batch. A datagram for a lane the listener has never seen is handled *inside* the
receive callback (registry lock, admission check, `Conn::new`, a `Shared`, a sender
task spawned, two map insertions) before the next `recv` of that batch, so bulk
traffic and a new lane's first packet share one task's critical section. Measure the
batch's processing time with its datagram count; if it reaches tens of milliseconds,
move the new-lane setup off the receive path and re-measure. Gate: the batch time
stays in microseconds and the arrival's p99 on the `cost` cell falls below the 20 ms
line that the timer cluster marks.

### Fewer wake-ups help, and do not touch the first stage (2026-11-05)

A lane carries four or five streams and a receive batch usually delivers bytes for
one, so the pump was waking every parked reader on the lane for each batch. It now
records which streams changed (`Conn::woke`) and wakes one reader per id; the
blanket wake stays for the events that really are connection-wide.

**On a staged cell with six bulk streams** (interleaved, three rounds, `--aa`):

| metric | change |
|---|---|
| `rtt_p99_ms` | **−79.35 %** |
| `rtt_p50_ms` | −51.70 % |
| `rtt_worst_1s_ms` | −75.52 % |
| `rtt_max_ms` | −66.27 % |
| `throughput_gbps` | **+16.79 %** (CI 1.28..2.08, entirely beyond its floor) |

Every latency metric moves the same way and throughput moves with them, which is
what fewer spurious wake-ups should look like — but the verdict is `directional`,
not `claim`, because at three rounds the cell's own A/A scatter is 48 % on p99.

**On the cell that decides the release** it does not fix the first stage. Full
scale, one arm, host quiet (load 0.24 / 0.75):

| stage | n | p50 | p99 | max |
|---|---|---|---|---|
| `clean` (first) | 1 964 | **36.47** | **302.13** | 509.68 |
| `clean#2` (last) | 2 396 | **2.48** | **22.54** | 24.77 |

The last stage is the best it has ever read (22.54 against 23.60 and 23.65 in the
two previous runs), so the change is worth keeping. The first stage is the worst it
has ever read — and that is the finding: across four runs of near-identical code
the first clean stage has now read **25.79, 112.85, 146.03 and 302.13 ms** while the
last has read **22.5–23.7** every time. The *last* stage is a measurement; the
first is a lottery, and every lever tried against it so far (credit reserve, worker
count, scheduler order, wake scope) has left it a lottery.

Two things the four numbers do establish, and they are not the same problem:

- **the warm stage is 10× the control** — 22.5 ms against 2.28 — so even with no
  cold start there is a tail: about one arrival in a hundred pays twenty
  milliseconds inside a handshake whose own median is 1.0–1.45 ms;
- **the first stage adds a further factor of one to thirteen**, and which one is
  decided by something outside the arrival path: the same binary, the same host
  class, four different answers.

The intervals excluded by measurement so far are the send buffers (dwell p50 2–4 µs
client, 9 µs server), the pump's post (p50 11–20 µs, p99 20–35 µs), the server's
handshake processing (under 5 ms in 1 141 of 1 191 slow opens), the wire itself (the
asymmetry above), the credit reserve, the worker count, and the scheduler's order.
**What is left uninstrumented is the datagram's wait between the socket and the
dispatcher, and the spawn-to-first-poll of the per-stream task** — the server's own
clock starts *after* the stream exists, so neither interval appears in any counter
this cycle built.

**A discipline this cycle learned the hard way**, and it now costs a run each time
it is forgotten: a bench record is only comparable with one whose host was as idle.
The run that read 285.99 ms had a five-minute load average of 4.74 against 0.84 in
its reference; the one killed before it started had 5.71 and would have been the
same story. Check `meta.provenance.host.loadavg` before comparing two files, and
before believing that a change moved a first stage.

### The arrival's tail is not in the carrier's send path (2026-11-05)

Every instrument the carrier owns now reads microseconds at full scale, and the
arrival still reads a hundred:

| what | client | server |
|---|---|---|
| a stream's first byte waiting for the wire (dwell) | p50 **2–4 µs**, p99 ~0.5 ms | p50 **9 µs**, p99 0.14 ms, max 12–70 ms |
| the pump's post to the socket | p50 **11 µs**, p99 20 µs, max 0.18 ms | p50 20 µs, p99 **35 µs**, max 0.35 ms |
| a stream's reader parked for bytes | p50 131 µs, p99 **46 ms**, max **2 s** | p50 64 µs, p99 45 ms, max 2 s |
| the per-stream Noise handshake, every open | p50 **1.0–1.45 ms**, p99 **23–107 ms** | *(not kept on this side)* |

Read together with the earlier attribution — `us == hs_us` for every slow open,
share 1.000 — the handshake is still the whole of an arrival's cost, and its p99
(107 ms) is the arrival's p99 (112.85) to within a few percent. What is new is
that **the handshake's time is not being spent in either end's send path**: the
buffers drain in microseconds and the pump's post never exceeds a third of a
millisecond. So the wait is the interval between a handshake record being written
and the peer's reply being *read* — two task crossings per message, and the reader
parked across them (p99 46 ms, and a 2 s tail that is the keepalive interval to the
microsecond, i.e. a reader that missed its wake-up and waited for the pump's own
timer).

The same asymmetry says it is not the wire: of 1 191 slow client opens, only **50**
were slow on the server's own clock (its `selector + handshake + hello`, under 5 ms
in the other 1 141). Both directions would be slow if the path were; one side is.

Which makes the lever *fewer task crossings per arrival*, not a faster stage — and
that is what sealing does (it deletes both handshake messages and the round trip
they wait on), and why the scheduler work is secondary: a class-aware order cannot
help a packet that is already on the wire while its reader sleeps.

**And the second failed experiment is recorded.** The opening reserve, taken off
every bound, was measured interleaved against the build without it on a quiet host
(load 0.32): `clean` read **5.98 / 146.03 ms** with it and **2.35 / 25.79** without,
while `clean#2` was identical to 0.3 %. The comparison is order-confounded — one
round, two arms run one after the other, on a cell this file has measured at 25.79
and 112.85 for the *same* binary — so it is not a claim about the mechanism. It was
enough to revert, because a change with no demonstrated benefit and a large
apparent harm does not get to stay while someone looks for a better measurement.

### The sealed deadlock is a repair loop, and the instrument was lying twice (2026-11-04)

Two instrument defects came out of following the stall, and each of them had been
hiding it:

- **`note_rejected` collapsed every sealing refusal into `unknown`.** The
  label-to-counter map recognised `malformed`, `malformed_decode`,
  `malformed_lane` and `over_window`, and sent *everything else* — including
  `unsealed`, `unsealed_no_root`, `unsealed_no_tag`, `unsealed_no_key` — to the
  **unknown-stream** counter. So a run whose sealing was failing printed either
  `datagrams refused: unknown=N`, which reads as a routing defect, or (when only
  the sealing labels fired) nothing at all, because the print condition counted
  the four statics that cannot receive them. Sealing now has its own counter,
  matched by prefix so a fourth site cannot fall back to "unknown" again.
- **the stall line could not say *why* a stream with unsent bytes was passed
  over.** It now carries `worst_stream_reset` and `worst_stream_repair`: those
  two are the only ways the scheduler skips such a stream without building a
  packet for it.

With them, the sealed failure names itself:

```
mhd stalled: unsent_ms=0 unacked_ms=569 credit_blocked=false
  worst_stream=5 worst_stream_unsent=1136478 worst_stream_retransmits=1130
  worst_stream_next=2119826 worst_stream_written=2119826
  worst_stream_reset=false worst_stream_repair=true
```

`next == written` — that stream has put **every byte on the wire** — and it is
sitting on a **repair that is re-declared lost and re-sent 1 130 times**, with
no write ever refused (`credit_blocked=false`, `unsent_ms=0`). So the lane is not
stuck for want of room or for want of data: it is stuck spending its flight on
one range it will never see acknowledged, and the four streams whose selectors
the server timed out never got a turn behind it.

That is much narrower than "sealing plus stripes", and it is testable without
sockets: the question is *why a range that the peer has already read is declared
lost again*, which lives in the acknowledgement-to-frontier path, not in the
cipher. The next round starts there — and the first thing it needs is the same
line again, which now exists.

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
