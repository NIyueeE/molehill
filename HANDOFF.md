# HANDOFF: Working State & Future Work

> **State as of 2026-10-01.** **v0.10.0 is released.** PR #4 merged into
> `main` (merge commit `dffd7d8`), the tag `v0.10.0` published a GitHub
> Release, the GHCR image and crates.io, and the branch that carried the work
> (`feat/session-and-pool`) is deleted — `main` is the only line now. The theme
> is **M1** (one control session per endpoint, protocol v4), **M2a** (one
> shared elastic pool per carrier, plus the S1 observation), **M6** (the
> configuration surface) and **M7** (`direct`'s role, measured).
>
> What is left is not code: the repository-settings items a human has to make,
> and the three open threads at the bottom of this page.
>
> **This file owns the working state**: what is open, what was decided and why,
> and the release checklist. Per AGENTS.md §3 it is a contributor page —
> user-facing facts belong in the docs pages, and anything released belongs in
> CHANGELOG.md. The cycle's closed records (incidents, superseded sweeps,
> measurement records, retracted diagnoses) live in
> [HANDOFF-archive.md](HANDOFF-archive.md), which this page indexes; the
> archive was split out on 2026-09-30 because the live state had become a third
> of a 1,600-line file.
>
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).

## Where this stands

- **v0.11.0 is prepared and waiting for a human to merge and tag.** The work is
  on `feat/transparent-l3` as a PR; `version = "0.11.0"` is committed, the
  CHANGELOG section is written, and `just bench-gate` **passes** on the sweep
  that ran before the last product change (90 cells, the endpoint invariant, the
  SLO on the clean stages, drift, wedges, and the ramp). Both axes that blocked
  this release are resolved: the L3 wedge was the harness's own shaper queue
  (stated and recorded now), and the wedge axis was measuring the probe's own
  patience (it measures unanswered attempts now). What remains is the human's:
  review the PR, merge to `main`, then `just tag` and push `v0.11.0`
  (AGENTS.md §5 — an agent must never create a release tag on its own
  initiative). Three things for that review:
  - **the CHANGELOG's date is `2026-10-14` and the tag will not be.** The
    section is `## [0.11.0] - 2026-10-14`; `githooks/pre-tag` only checks that
    exactly one dated section exists, so a tag cut later should carry the date
    it is cut on. That is a human's edit to make in the release commit.
  - **the final sweep is the one running as this is written**, on the commit
    that changes an L3 claim's default to four lanes (`bfb1ef4`); it replaces
    `benches/records/results-bench-v0.11.0.json`, the `assets/bench-v0.11.0*.png`
    chart set and the README's benchmark tables. The sweeps before it measured
    either the harness's shaper or the previous default, and are not the
    artifact.
  - **the L3 row will jump, and the reason is the default, not the code.** A
    claim now holds four carrier connections where it held one, which is
    measured at 2.2× on a multi-flow claim ([`DEFAULT_CLAIM_LANES`] carries the
    table) and costs `lanes x claims` connections — a breaking change with a
    one-line upgrade path, both stated in the changelog and the configuration
    page.

- **The cycle being released**: transparent (L3) services, wire **v5**, based on
  `main` at `cf01091`. Nothing of it is in `v0.10.0` (verified: the tag's tree
  has no transparent code at all).

- **The first `bench`-model release has no baseline.** `benches/records/` holds
  only the two *soak* files, and `analysis.comparability` refuses any file
  whose `meta.model != "bench"` — so v0.11.0 is gated on the absolute checks
  (coverage, the endpoint invariant, the SLO, drift, wedge, ramp) and becomes
  the baseline for v0.12. Worth remembering when the next release's gate is
  read.

- **The branch was squashed twice: 187 commits to 13 (2026-10-14), then 36 to
  29 (2026-10-15, after the wedge cycle).** Each commit is one topic, and each
  was a state that passed the gate chain when first committed; both times the
  tree came out byte-identical to the tip it replaced. Backups:
  `backup/l3-pre-squash-2026-10-14`, `backup/l3-pre-squash2-2026-10-15` and
  `backup/pre-squash3-2026-10-15`. A squash changes every SHA, which is why the
  sweep is re-measured rather than carried over: `githooks/pre-tag` checks the
  results file's recorded revision against the commit being tagged, and an
  artifact citing a revision nobody can check out is not provenance.
  - The second squash folded only what was genuinely one topic or genuinely
    superseded: the three fixups that followed on the shaper's instrument, the
    tail-dropping limits, the re-measured release artifacts, and the four
    wrong-turn HANDOFF records — which became one commit whose subject does not
    assert the conclusion they had reached. `433a60e`'s subject claimed the
    wedge *was* bufferbloat; it is reworded to claim only what survived, which
    is that the loss is a queue and the product's own is clear. The bufferbloat
    reading turned out to be right; the attribution is what it could not make.
  - The first squash's re-run was not merely a re-stamp, and the reason is
    worth recording: the first sweep's method record carried
    `engine_hash a08ba5c5cfd439dc`, and the `analysis.py` coverage fix landed
    *after* it, so the new run's was `1771ce182f09d566`. Same numbers' worth of
    code, different engine — which is exactly what the engine hash exists to
    make visible, and exactly why a file measured under the old one could not
    have gated that commit.
  - That re-run's agreement with the one before it is the reading the README
    published: molehill's clean bulk 9.209 → 8.495, frp 6.106 → 6.040, rathole
    28.986 → 25.026, nps 0.135 → 0.134.
- **RESOLVED 2026-10-15: the release gate failed on one axis, and the failure was
  the harness's shaper rather than the tool.** The sub-bullets below are the
  investigation as it happened, wrong turns included — they are worth keeping
  because most of them are hypotheses a reader would otherwise re-run, and
  because the answer took a dozen of them. **The short version: the shaped
  leg's netem queue was left on its implicit 1 000-packet default, which caps a
  100 ms leg at 10 000 packets/s, and the L3 arm's return stream is 37 600
  packets/s; 73 % of it was dropped at the shaper, invisibly, and the
  interactive cell was reading that loss. The queue is now stated (10 000
  packets) and the shaper's own drops are recorded; the same arm on the same
  timeline reads a 400.8 ms median with zero timeouts where it read 400–3 182 ms
  with six.** Jump to "The fix is the model's own" below for the measurement.
  The investigation opened like this (2026-10-14): adding `l3` to the sweep was
  the point — it is the first time the architecture has been measured over the
  stage schedule — and it immediately produced something, **`l3` wedging 6
  times (longest 8.27 s) where `molehill` (L4) wedged 0 times** in the same run.
  - The gate is deliberate about this, not arbitrary: `SUBJECT_KINDS = ("l4", "l3")`
    and `_drift` fails on *any* wedge in a subject arm, while a **peer** that
    wedges is reported and never blocks (`nps` wedged once this run and passed).
    So the product's own architectures must not go silent; the reference tools may.
  - **The obvious hypothesis is wrong, and is recorded so nobody re-runs it.**
    A claim never multiplexes, and the `l3` arm's budget is unwritten, so each of
    the topology's five claims gets **one** lane — one connection, no
    head-of-line isolation, where L4 has four tunnels. That predicts more lanes
    should wedge less. A scaled run (`--scale 0.25`, arms molehill/l3/l3-budget10)
    gave the opposite: `l3` 1 wedge (5.21 s), `l3-budget10` 4 wedges (8.32 s),
    `molehill` 0. The lane budget is not the mechanism. That run is short and
    noisy — `l3` read 6 wedges in the full sweep and 1 in the scaled one — so it
    rules out the hypothesis without establishing a new one.
  - What is solid: L4 does not wedge, L3 does, in three independent runs (the
    full sweep, `--scale 0.5` and `--scale 0.3`). What is not yet known: why.
  - **Localised: the wedge is one stage, not a property of the run**
    (2026-10-14). The wedge metric lives on the `run` cell, so nothing said
    *where* the silences were. They were localised from the run's own artifacts
    (`round0/arm-l3/series-l3-*.ndjson`): all six fall between t=166 s and
    t=261 s of a 950 s run — **every one inside `rtt100`** (a clean 100 ms
    delay, no configured loss), and every other stage is clean. Each wedge is a
    single `error TimeoutError` line: one attempt past the probe's own 5 s cap,
    not minutes of silence. In that stage `l3` completes 74 interactive pings
    where L4 completes 309, with `err%=7.5`, `p99=4992 ms`, `retrans=137355`,
    and **8528 `tx_dropped` on the server's own TUN (`mhbsrv0`) — a device L4
    never drops a packet on**.
  - **Second hypothesis tested and refuted, with a clean discriminator**
    (2026-10-14). `tx_dropped` on a TUN counts what the kernel handed a reader
    that was not reading, so the natural reading is *the server's TUN queue
    overflows and those drops are the wedge*. The model already prices this axis
    (`Arm.txqueuelen`, 1000 vs `DEEP_TXQUEUELEN` 10000), so `l3` and `l3-deep`
    ran together over the same rtt100-bearing timeline at two scales. **A deeper
    queue removes the drops entirely and leaves the wedge untouched — sometimes
    worse:**

    | scale | arm       | txqueuelen | server-TUN drops | rtt100 p99 | interactive n |
    |-------|-----------|------------|------------------|------------|---------------|
    | 0.5   | `l3`      | 1000       | 5205             | 4476 ms    | 44            |
    | 0.5   | `l3-deep` | 10000      | **0**            | 4948 ms    | 32            |
    | 0.3   | `l3`      | 1000       | 640              | 3118 ms    | 33            |
    | 0.3   | `l3-deep` | 10000      | **0**            | 2464 ms    | **19**        |

    So the overflow is collateral damage of whatever stalls the reader, not the
    cause: a queue drops packets when something upstream stops draining it.
    **Read alone, that result was misleading — it tested only one direction of
    the right mechanism.** The finding is **bufferbloat in the kernel's TUN
    txqueue**, and the discriminator is the *shallow* direction (2026-10-15,
    `--scale 0.3`):

    | arm         | txqueuelen | clean n | clean p99 | rtt100 n | rtt100 p99 |
    |-------------|------------|---------|-----------|----------|------------|
    | `l3base`    | 1000       | 939     | 1.25 ms   | 25       | 4524 ms    |
    | `l3shallow` | 60         | 336     | 1462 ms   | **86**   | **1435 ms** |

    A shallow queue **cures the wedge** (4524 → 1435 ms, 3.4× as many pings
    land) but starves the clean path (1.2 M drops). Three points on one axis
    are now monotone — shallow costs throughput and buys latency, deep costs
    latency and buys drops, 1000 sits between — which is what makes this a
    queueing-depth finding rather than three unrelated numbers.

    **The product's own queue is exonerated, and that is measured, not assumed.**
    `QUEUE = 128` batches × `BATCH_BYTES = 16 KiB` ≈ 2 MiB of standing buffer
    inside `hub.rs` was the obvious suspect, and it has a counter built for
    exactly this question (`overflow_packets`, "this end's backpressure, not the
    path's loss", behind `MOLEHILL_L3_STATS`). A run with the switch on reports
    **3 060 746 packets offered, 0 overflow, 0 no_channel, 0 disconnected** —
    `enqueued == offered` for every lane, whole run. The internal queue drains
    fine; the standing buffer is the kernel's, and what limits it is the drain
    *rate*, not the depth.
  - **Third finding: the model's `txqueuelen` axis was inert, and that is fixed
    (2026-10-15).** A TUN device comes up on the host's default qdisc (`fq` on
    this host), and `fq` does **not** derive its `limit` from `txqueuelen`:
    measured on a fresh device, `txqueuelen` 60 / 1000 / 10000 all left the
    qdisc reading `limit 10000p flow_limit 100p`. So every arm that stated a
    queue length measured the host's default instead — including the release
    sweep, which recorded `txqueuelen: 1000` while actually running a
    10 000-packet queue. `topology.set_txqueuelen` now rebuilds the qdisc with
    the stated number as its limit, which is what makes the arm's declaration
    the measured queue. This also means **the earlier queue-depth dose-response
    in this entry was measuring an artifact** and is withdrawn: those runs
    varied an attribute that never reached a queue.
  - **Hypotheses refuted since, each with a real run** (so nobody re-runs them):
    an explicit `fq_codel` on both TUNs (3 wedges, p99 4480 ms — no better);
    a *real* 1000-packet queue against 10 000 (1 wedge vs 2, p99 4758 vs
    3500 ms — no better, if anything worse); the probe's 4 MiB `SO_RCVBUF`
    (interactive rate 0.59/s vs 0.71/s — unchanged); BDP/bufferbloat (no
    monotone relation across stages); and the lane budget (recorded above).
  - **What the refutations leave standing, and it is the sharpest clue yet.**
    In the real-knob A/B the `limit 10000p` arm wedged **with 0 drops on every
    interface** and 138 361 TCP retransmits. A stall with *no loss anywhere*
    cannot be a drop-based mechanism: the packets are being **delayed**, not
    discarded. Every queue in the path is far too shallow to hold five seconds
    of traffic (TUN 10 000 packets ≈ 14 MB ≈ 94 ms at 1.19 Gbit/s; the claim's
    own lane queue ≈ 2 MiB ≈ 13 ms; netem ≈ 1 000 packets ≈ 9 ms), so the
    remaining candidate is a **stall in the product's own task/pacing path** —
    something that stops draining a queue for seconds and then resumes. The next
    step is in-process instrumentation (timing around the lane write and the
    pacer's parked queue), not another black-box knob: six black-box hypotheses
    have now been refuted, and the AGENTS.md §9 timebox says to record and ask
    rather than keep guessing.
  - **In-process instrumentation, and what it settles (2026-10-15).** The
    reader now records the longest gap between two device reads that returned
    data (`read_gap_max_ms` / `read_gap_at_ms` in the `MOLEHILL_L3_STATS` line).
    A reader that stops draining is invisible in the per-packet counters — the
    packets it never read are dropped by the kernel, not by this process — so
    this is the number that says whether the stall is here. **It is not: the
    server's longest gap over a whole run was 989 ms and the client's 1000 ms,
    both at a stage transition, while the interactive probe was timing out at
    5.2 s.** The reader drains; the stall is downstream of it.
  - **The bulk claim's lane queue overflows ~1 M packets, and that is the real
    anomaly.** Same run, per claim, server side: `2401` (interactive/churn)
    offered 40 135 and forwarded 40 135 with **zero** drops; `2402` (bulk)
    offered 28 166 823 and refused **992 414** on `overflow` — the lane's own
    queue, the documented "this end's backpressure". The client reports the
    same number as `peer_refused=992 414` for the same claim, so the two ends
    agree on it. The storm starts ~9 s before the carrier switch (KCP → TCP,
    "the carrier in use kept refusing this claim's traffic at the peer"), runs
    for minutes, and **the wedges fall inside it** (t=72 s and t=82.5 s of a
    322 s run, against a storm from t≈54 s).
  - **So the wedge is contention, not the interactive stream's own path.** The
    interactive claim's counters are clean on both ends, which rules out "the
    interactive lane is broken"; what it shares with the bulk claim is the TUN
    device, the carrier, and the CPU. The open question is now narrow and
    specific: **why the bulk claim's lane queue cannot drain at `rtt100`**, and
    whether the carrier switch that follows is a cause or a consequence of it.
    The next step is to instrument the lane's *write* side (the pump's gap
    between successful carrier writes) the way the read side now is — that
    splits "the carrier cannot carry it" from "the queue is fed faster than the
    pump is scheduled".
  - **Both halves instrumented, and the product is cleared (2026-10-15).** The
    write side now records the longest gap between two batches the carrier
    accepted (`write_gap_max_ms` / `write_gap_at_ms`). Over a whole run the
    server read 789 ms and wrote 789 ms, the client read 959 ms and wrote
    959 ms — **the same value and the same timestamp on each side**, i.e. both
    maxima are the same inter-stage idle window, not a stall. Neither half of
    the product stops: it reads, it writes, and it forwards every packet the
    interactive claim offers (40 940 offered, 40 940 forwarded, `overflow=0`)
    while the *bulk* claim refuses 890 094 on its lane queue.
  - **Therefore the interactive loss is upstream of the product**, at the TUN
    device the claims share: the bulk claim's backpressure fills that queue and
    the interactive SYN is dropped there — before this process ever sees it,
    which is exactly why no product-side counter has ever shown a drop for the
    interactive claim, and why six product-side hypotheses failed. The sweep's
    own `tx_dropped=8528` on `mhbsrv0` is that drop site.
    **This conclusion is wrong. It is corrected in the next bullets and kept
    here rather than deleted, because the record of what was believed and why is
    what stops the next reader from re-deriving it from the same evidence.**
  - **The correction, and it is the round's main result (2026-10-15, later the
    same day).** A staged timeline re-run with a *per-second* rate instrument
    added (`offered_per_s` / `forwarded_per_s`, sampled as a delta against the
    previous sample, because a whole-run average cannot see a five-second
    window) shows the wedge **reproducing with `tx_dropped = -619` — no drops at
    all on the server's TUN**:

    | round | stage | rtt100 p99 | visitor retrans | `mhbsrv0` tx_dropped |
    |-------|-------|-----------|-----------------|----------------------|
    | 0 | rtt100 | 4487 ms | 114 285 | 0 |
    | 1 | rtt100 | 5971 ms | 49 295 | **-619** (none) |

    So the wedge is **not** a drop at the shared TUN, and the sweep's 8528 was
    one run's incidental noise. A wedge with no loss anywhere is the same
    discriminator the earlier `limit 10000p` arm produced, and it points at the
    same place: **delay, not discard**.
  - **The first reading of that instrument was wrong, and the correction is the
    round's real result (2026-10-15, same day, after placing the samples on the
    timeline).** The bulk claim's rate does not collapse "for five seconds
    inside `rtt100`": **08:33:28–08:33:33 is the tail of the *clean* stage.**
    The stage boundaries come from the run's own artifacts (each stage's
    iperf3 server log mtime): `clean` ends 08:33:27 and `rtt100` runs
    08:33:27 → 08:34:13. Inside `rtt100` the bulk claim offers a steady
    **100 000–145 000 packets/s**, the device **10 000–14 000/s**, and the
    interactive claim **2–17/s** — and the reader never stops at all: the
    device's own rate stays flat through the whole stage, which is the number
    the first reading inverted.
  - **What actually happens at the wedge, and it is the whole story.** During
    `rtt100` the interactive stream's round trips are bimodal: 20 samples at
    0.5–1.2 ms (the clean stage's tail, before the delay was applied at
    08:33:28.4), then **every sample at a clean 401 ms** — exactly the stage's
    configured 100 ms — except eleven outliers at **1410, 1408, 1427, 1456,
    1007, 2431, 1418, 1418, 5972 ms**. Those decompose precisely:
    `401 + 1009`, `401 + 2029`, `401 + 5570` — a **TCP RTO ladder of 1 s, 2 s
    and 5.5 s** on top of the path's own 401 ms. The probe's cap is 5 s, so the
    5 972 ms sample is the wedge, and it is a *retransmission timer*, not a
    queue.
  - **The interactive claim's counters collapse at exactly those moments, on
    both ends.** At 08:33:57–08:34:09 the server's claim 2401 offered
    **1–3 packets/s where it had been doing 12–17/s**, and the client's claim
    2401 did the same 1–9/s where it had been 9–12/s — simultaneously, with
    `overflow=0` and `no_channel=0` on both. So the product was offered less
    traffic and refused none of it: **the visitor's own TCP stopped sending,
    because its retransmission timer had not expired.** That is the mechanism,
    and it is the visitor's congestion control, not the tool.
  - **And nothing is dropped anywhere.** Every interface reads `rx_dropped=0,
    tx_dropped=0` through the whole `rtt100` stage in both rounds (round 1's
    `mhbsrv0` reads **-619**, a counter-delta artifact). The product's own
    counters are `not_ipv4=1, malformed=0, unclaimed=0, channel_errors=0`. So
    the earlier reading — that the interactive SYN is dropped at the shared TUN
    queue — is refuted twice over: there is no drop, and there is no stall.
  - **What this leaves, and it is a different problem from the one this cycle
    has been chasing.** The L3 arm's interactive stream at `rtt100` pays
    **p99 4 492–5 972 ms against every other arm's 401 ms** (molehill l4
    402 ms, frp 401 ms, rathole 401 ms, nps 416 ms), with 49 295–114 285
    retransmitted segments and a 7 % error rate, and it does so with **no loss
    anywhere and no product-side stall** — the delay is the visitor's TCP
    backing off. The mechanism that distinguishes L3 from L4 at that stage is
    the one thing the measurements have not yet separated: L3 carries the
    interactive stream's packets through the same reader task and the same
    carrier as a bulk claim offering **100 000+ packets/s**, while L4 gives it
    a tunnel of its own. Whether that contention is what makes the visitor's
    TCP back off, or whether it is the carrier's own behaviour at 100 ms RTT
    under a bulk load, is the open question — and it is now answerable, because
    the instrument that was missing (a per-second rate on the device *and* per
    claim) is the one that was just added.
  - **The last link, measured: it is the carrier switch and the drain it asks
    for (2026-10-15, same run).** The per-claim state shows the whole sequence
    inside the `rtt100` stage (08:33:27 → 08:34:13, from each stage's iperf3
    log mtime):

    | time | event |
    |------|-------|
    | 08:33:27 | `rtt100` begins; interactive RTT 0.7 ms → 401 ms |
    | 08:33:37 | bulk claim 2402 begins refusing: `overflow` starts |
    | 08:33:38 | first overflow = 13 026; one ping at 1 410 ms (= 401 + 1 009) |
    | 08:33:41 | **client: `lane 1 refused its share for 3 ticks; asked the peer to drain it so its replacement rides Tcp`, and the carrier switches `kcp → tcp`**; server sets `peer_draining=true` on 2402 |
    | 08:34:01 | probe timeout #1 (the wedge) |
    | 08:34:10 | probe timeout #2 (the wedge) |
    | 08:34:12 | **client: `no drain answer within 30 s; calling the drain off`** |
    | 08:34:13 | `rtt100` ends |

    And the interactive claim's own path is **provably clean**: server
    10 833 offered / 10 833 forwarded / `overflow=0` / `disconnected=0`, client
    7 295 / 7 295 / `0` / `0`, `unclaimed=0` and `tx_dropped=0` on every
    interface for the whole stage. The product carries every packet it is
    offered, both ways, the whole time. So the five-second silences are the
    visitor's own kernel deciding not to send — and what it is reacting to is
    the claim's lane being drained while the bulk claim that shares the carrier
    with it is refusing 36 999 packets.
  - **What that sequence does and does not explain.** The drain is asked for
    **claim 2402, the bulk claim**, so `live_slots()` for 2402 alone goes empty
    and *its* new flows stop routing — which is exactly what the counters show:
    2402's offered rate falls to ~0 from 08:33:49 and its overflow freezes at
    36 999 and never moves again. **Claim 2401 is untouched by it** (its own
    slot, `peer_draining=false` throughout, offered == forwarded for the whole
    stage). So the drain explains the bulk claim going quiet, and it does
    **not** explain the interactive wedge — that was the mistake in the first
    draft of this bullet, and the per-claim counters are what caught it.
    The interactive wedge's cause is still open, and it is now the *only* open
    question: every candidate it has been tested against (the TUN queue, the
    reader, the flow table, the lane budget, the probe's buffers, BDP) is
    refuted by a measurement in this entry, and the interactive claim's own
    path is measurably perfect while its round trips climb to 5 972 ms.
  - **That prediction is tested, and it is refuted (2026-10-15, same day).**
    `l3bud` (`pool_cap=10`, so two lanes per claim) ran the same staged timeline
    three times against `l3`'s default. Lane lending gave claim **2401 up to six
    lanes** (its own two plus four lent from the quiet claims 2403/2404/2405),
    and the interactive claim **never once had `peer_draining=true`** — so
    `live_slots()` was never empty for it. It still wedged:

    | round | arm | rtt100 p99 | err % | wedge_count | wedge_max_s |
    |-------|-----|-----------|-------|-------------|-------------|
    | 0 | `l3bud` | 3 939 ms | 11.11 | 3 | 6.22 |
    | 1 | `l3bud` | 2 475 ms | 7.89 | 3 | 5.21 |
    | 2 | `l3bud` | 3 507 ms | 16.67 | 4 | 5.01 |

    And in the same run the carrier still switched `kcp → tcp` (09:55:16) and the
    drain was still requested — on claim 2402, the bulk claim, exactly as before.
    So neither the lane budget nor the drain nor the carrier switch is the
    wedge's cause: the interactive stream wedges while holding six routable
    lanes and being drained by nobody.
  - **And the `l3` arm of that comparison is void, for a reason worth
    recording.** `pool_cap=1` was written for the control arm, and the config
    layer refused it outright: *"`[transparent.data].tunnels = 1` is below what
    this client's claims need: 5 claim(s) draw on it, and a claim's lane is a
    connection of its own"*. The arm therefore never started (three rounds of
    `not ready: no visitor could reach 10.99.0.1:2401`), which is the check
    doing its job — and the honest way to state the result is that the
    comparison is `l3bud` against `l3`'s *published* numbers, not against a
    co-measured control in this run. A clean A/B on the same timeline still
    needs to be run before any claim of the form "more lanes is better/worse"
    is made.
  - **Therefore the queue-depth work could not fix it, and the reason is
    precise**: the wedge is a retransmission timer, and a queue's depth only
    decides how long a packet that *was* delivered waits. **A deeper TUN queue
    delays the packets that were read; a shallower one drops them; neither
    changes when a sender retransmits** — which is where the 5 972 ms comes
    from. This is also why six black-box knobs failed in a row: none of them
    changes the thing the wedge is made of.
  - **What the evidence does support, and what it does not.** Every mechanism
    this cycle proposed for the wedge has now been measured and refuted: the
    shared TUN queue (zero drops, all stage), the reader starving (device rate
    flat at 10–14 k/s), the flow table (1770 → 1230 is settling to the
    20/s × 60 s equilibrium), the lane budget and the drain (six routable lanes
    and no drain on the wedging claim), the carrier switch (present on the arm
    that wedges *and* on the arms that do not), and the probe's own buffers.
    What is left is a single unexplained fact: **at `rtt100`, and only there,
    the L3 arm's interactive stream pays 2 475–5 972 ms where every other arm
    pays ~401 ms, with the product's own counters clean on both ends.** The
    one structural difference that has not been separated is *where the
    interactive flow's packets are carried*: on L4 they ride a tunnel of their
    own, and on L3 they ride the same carrier connection, the same reader task
    and the same kernel as a bulk claim offering 100 000+ packets/s. That
    contention is the remaining candidate, and it is now the only one.
  - **That experiment is run, and it settles it (2026-10-15, same day).** The
    staged timeline ran again with the bulk spine removed entirely
    (`--profile nobulk`, `streams: 0`, the new diagnostic profile —
    `throughput_gbps` is a typed absence, not a zero, and no iperf3 is dialled
    at all). The L3 arm still wedges:

    | round | clean p99 | **rtt100 p99** | err % | interactive silences > 5 s |
    |-------|-----------|----------------|-------|----------------------------|
    | 1 | 1.22 ms | **4 508 ms** | 13.04 | 2 (max 17.46 s) |
    | 2 | 1.36 ms | (stage clean of wedges) | 0 | 0 |

    and its round trips carry the same ladder as before — 1 045, 1 416,
    1 651, 1 834, 2 088, 2 092, 3 344, 4 508 ms against a 401 ms baseline, i.e.
    **+644, +1 015, +1 687, +2 943 and +3 871 ms of extra delay with no bulk
    traffic in the path at all.** A KCP nodelay RTO ladder from the 100 ms floor
    (100 → 150 → 225 → 338 → 506 → 759 → 1 139 → 1 709 → 2 563, ×1.5 per
    retransmit) reproduces those numbers, which is what a retransmission round
    costs when the path's own RTT is 100 ms.
  - **So the wedge is the L3 path's own behaviour at a 100 ms RTT, and the bulk
    claim's contention is refuted as its cause.** That was the last structural
    candidate this cycle had left. Every mechanism proposed for the wedge — the
    shared TUN queue, the reader starving, the flow table, the lane budget, the
    drain, the carrier switch, contention with the bulk claim, and the probe's
    own buffers — has now been measured and refuted, each by a run that
    separated it. What remains is a fact rather than a hypothesis: **an L3
    arm's interactive stream pays a retransmission-scaled penalty at `rtt100`
    that no other arm in the sweep pays** (l4 402 ms, frp 401 ms, rathole
    401 ms, nps 416 ms — all at the stage's configured delay), while the L3
    arm's own counters stay clean on both ends: `offered == forwarded`,
    `overflow=0`, `disconnected=0`, `unclaimed=0`, and `rx_dropped=tx_dropped=0`
    on every interface for the whole stage.
  - **What that says the next step is.** The loss is at the *carrier*, not the
    device, the reader or the claim: KCP's ARQ on a 100 ms path is what adds the
    seconds, and the wedge is one handshake whose retransmission timer grew past
    the probe's 5 s cap. The lever the product already has for exactly this is
    the carrier switch it fires when a claim's traffic keeps being refused —
    but in this run the switch fired *after* the ladder had already started
    (10:53:35, with the first 1 022 ms ping at 10:53:32), and it was asked for
    by claim 2402, the bulk claim, while the wedging stream rides 2401. **A
    switch keyed to the interactive claim's own round trips, rather than to a
    bulk claim's refusals, is the change the measurements now point at.** That
    is a design change and it needs the human's decision before it is built —
    the AGENTS.md §9 timebox has been exceeded many times over on this thread,
    so it is recorded here rather than pursued further unattended.
  - **The mechanism recorded in the bullet above is refuted, and this is the
    correction (2026-10-15, second pass).** It said the wedge is the L3
    carrier's ARQ on a 100 ms path. The carrier never retransmits anything. Run
    with `MOLEHILL_KCP_STATS=1` on the release timeline (`--profile sweep
    --arms l3 --scenarios timeline`), the `rtt100` stage that pays p99 4 524 ms
    with 7.7 % errors and **32.4 % datagram loss** reads, on **both** ends, for
    every second of the stage:

    | counter | client | server |
    |---|---|---|
    | `retransmits` / `resends_rto` / `resends_fast` | 0 / 0 / 0 | 0 / 0 / 0 |
    | `rto_ms` / `srtt_ms` | 30 / 1 | 30 / 1 |
    | `wait_rcv` / `advertised_wnd` / `peer_wnd` | 0 / 4096 / 4096 | 0 / 4096 / 4096 |
    | `reader_queued_bytes` / `spill_bytes` | 0 / 0 | 0 / 0 |

    So the ARQ is idle, its windows are open and its reader is never behind.
    The RTO-ladder match that produced the claim was arithmetic done on a run
    whose carrier counters were not recorded at all — it fitted the numbers and
    was never a measurement of them. **A ladder of numbers is not evidence of a
    timer; the timer has to be read.**
  - **The bulk spine is what the wedge needs, and the no-bulk control is now
    run properly.** `--profile carrier` (new: the `wedge` timeline — clean 30 s,
    `rtt100` 180 s, clean 30 s — with `streams: 0`) ran three rounds: **540 s of
    `rtt100` at median 401 ms, p90 401.4 ms, max 401.9 ms, zero errors, zero
    retransmissions, zero drops.** The earlier "`nobulk` refutes contention"
    claim does not reproduce, and the artifact it rested on was not a `nobulk`
    run at all: it forwarded 9.4 M packets with claim 2402 (the bulk port) at
    240 000 packets/s, and its stages were ~100 s where the `nobulk` profile's
    are 36 s. Contention is back on the table, and it is the only candidate
    still standing.
  - **The product carries everything it is offered, and the packets are lost
    where no counter in this repository was looking.** Per-second per-claim
    counters on both ends, for the interactive claim *and* the UDP claim, read
    `offered == forwarded == injected` with `overflow = 0`, `no_channel = 0`,
    `disconnected = 0` at every second of the stage; what moves is the *rate*
    (216 packets/s → 1–20 packets/s). The visitor's kernel stopped sending
    because it is waiting out a retransmission timer of its own. And the loss is
    the shaper's: read live, `mhbsrv/v-srv` — the server's egress toward the
    visitor, the leg the condition is imposed on — **dropped 9 900 packets/s
    against 34 000 sent (23 %) during `loss1`**, with `tx_dropped` reading 0 on
    that interface the whole time. **`/proc/net/dev` cannot see a qdisc's
    drops**: netem drops at enqueue, before the packet reaches the device, so
    the one place this topology sheds traffic was the one place the model did
    not read. Every mechanism this cycle blamed — the TUN queue, the reader, the
    lane budget, the drain, the carrier — was blamed because the real loss was
    invisible.
  - **The mechanism, and it is arithmetic.** A qdisc holds `offered rate x
    delay` packets in flight, so a netem queue of `limit` packets on a leg with
    `delay` has a packet-rate ceiling of `limit / delay` — and netem's default
    limit is **1 000 packets**. Read live inside the running namespaces at
    `rtt100` (both ends shaped, so both ends read):

    | interface | qdisc | sent | dropped | backlog |
    |---|---|---|---|---|
    | `mhbsrv/v-srv` (server → visitor) | `netem limit 1000 delay 100ms` | 9 400 pkt/s | **28 180 pkt/s** | **1000p — pinned at the limit** |
    | `mhbvis/v-vis` (visitor → server) | `netem limit 1000 delay 100ms` | — | 0 | 65–136 GSO skbs |

    So the shaped leg's capacity is 1 000 / 0.1 s = **10 000 packets/s**, the
    server offers it **37 600 packets/s**, and 75 % is dropped. The interactive
    probe's replies and the datagram echoes are in that stream, so they are
    lost, and the visitor pays its own retransmission timer: ~1 000 ms for a
    fresh connection's initial RTO (the observed 1 005–1 045 ms cluster),
    ~1 400 ms for a lost SYN (1 000 + the path's 401 ms), and ladders up to the
    probe's 5 s cap. **And the default is not neutral between the arms**: L3
    offers ~37 600 return packets/s for 0.83 Gbit/s (one acknowledgement per
    ~2.8 KB, because the client's TCP stack sees the bulk arrive through its TUN
    in 1 400-byte units) where L4 offers 6 277/s for 3.38 Gbit/s (the server's
    TCP stack GRO-aggregates the visitor's 64 KB skbs, one acknowledgement per
    ~67 KB) — a **24× higher acknowledgement-per-byte ratio**, which is what
    tips a marginal shaper over. That ratio is a real architectural property of
    the L3 path and worth its own entry; it is not what the `rtt100` row was
    measuring.
  - **The fix is the model's own, applied where it was missing, and it is
    verified.** The model already knew: `loss1_buf` exists precisely to put
    "tail drops out of the way without the hundred-megabyte standing queue that
    inflates RTT" (10 000 packets), `loss1_deep` goes to 100 000, and
    `docs/benchmarks.md` already said "**a shaped stage's interactive cell is
    context, not a verdict** — the netem queue the harness installed dominates
    it". What was missing is that the same implicit default was still on
    `rtt100`, `loss5`, `jitter` and `loss1_mtu1280`, and that a queue which
    *dominates* a cell also *drops* from it. Those four now state 10 000
    packets, `loss1` and `loss1_mtu1280` state the 1 000 they mean, and the
    model records every stage's qdisc drops and standing backlog. The same
    release timeline, the same arm, the same binary, one method parameter
    apart:

    | `rtt100` | before (`limit 1000`) | after (`limit 10000`) |
    |---|---|---|
    | shaped leg's sent rate (live `tc`) | 9 400 pkt/s | **30 000 pkt/s** |
    | shaped leg's drop rate (live `tc`) | **28 180 pkt/s** | 1 790 pkt/s |
    | queue backlog (live `tc`) | **1000p — pinned at the limit** | 308–1442p |
    | interactive p99 (recorded) | 4 524 / 7 390 / 4 992 ms | **1 423 ms** |
    | interactive median / p90 (recorded) | 400.8 ms / up to 3 182 ms | **400.8 ms / 401.1 ms** |
    | interactive samples | 65–96 per 120 s | **288** (the probe's own rate) |
    | probe errors | 6–7 (7.7–9.7 %) | **0** |
    | datagram loss | 32.4–36.5 % | **2.72 %** |
    | bulk throughput | 0.83–1.19 Gbit/s | **1.42 Gbit/s** |
    | shaper drops, as recorded | (instrument broken) | **260 828 over the stage** |

    **This is not a number chosen to flatter the tool**: it is the value the
    model already used for exactly this purpose, the row it changes was
    measuring the shaper, and the reading is now in the evidence rather than
    promised. **What is left is a finding, not an artifact**: the L3 arm still
    sheds ~4 % of its return stream against the peers' 0 % at `rtt100` (and
    ~8 % against their 1.3–2.6 % at `loss1`), which costs it three or four ~1 s
    outliers in eighty samples and no timeouts. That residue is the packet-rate
    amplification below meeting a finite buffer — a property of the
    architecture on a modelled link — and it is now visible as a number instead
    of being attributed to the carrier, the TUN queue or the reader. A deeper
    buffer would absorb it, and choosing one is a method question with its own
    justification to make; it is deliberately not being tuned until the L3 row
    matches L4's.
  - **The L3 packet-rate amplification is a real finding and is *not* what the
    row was measuring.** L3 offers ~37 600 return packets/s for 0.83 Gbit/s
    (one acknowledgement per ~2.8 KB, because the client's TCP stack sees the
    bulk arrive through its TUN in 1 400-byte units) where L4 offers 6 277/s for
    3.38 Gbit/s (the server's TCP stack GRO-aggregates the visitor's 64 KB skbs
    into one acknowledgement per ~67 KB) — a **24× higher
    acknowledgement-per-byte ratio**. That is inherent to carrying whole IP
    packets to a kernel that then re-segments them, it is worth its own entry
    in the benchmarks page, and the obvious lever for it (GRO/TSO offloads on
    the TUN, so the receiving stack can coalesce) is a feature and not part of
    this release.
  - **A second harness defect, found by the re-run and fixed the same way
    (2026-10-15).** The sweep that followed the shaper fix read `wedge_count 2`
    on **L4** — an arm that had read 0 in the previous sweep. Both were located
    in the raw series and neither is a silence:

    ```
    gap 5.50s  stage=loss5  prev_rtt=401.8  next=rtt_ms 5498.0
    gap 5.12s  stage=loss5  prev_rtt=1060.4 next=rtt_ms 5119.8
    ```

    Two **successful** answers of 5.5 s and 5.1 s, in the 5 %-loss stage. Two
    defects stacked to produce that reading. The metric measured **gaps between
    samples**, and the interactive probe is serial — so the gap is the *next
    attempt's own duration*, never a silence of the tool's; and the threshold
    was 5.0 s, exactly the probe's `ATTEMPT_TIMEOUT_S`, so a single unanswered
    attempt was a "silence" by construction. Underneath both, the budget was not
    a budget: set as a per-socket-operation timeout it let one attempt spend it
    three times (connect, send, receive) and still report success, which is how
    a 5.5-second *successful* ping exists at all.
  - **The fix, and the shape of it.** `ATTEMPT_TIMEOUT_S` now bounds the whole
    attempt (verified: a socket that accepts and never answers ends the attempt
    at 5.01 s); the probe emits each attempt's `started_at`; `wedges()` counts a
    silence as a **run of consecutive unanswered attempts**, measured from the
    first one's start to the last one's end; and the threshold is stated as
    **two attempt budgets**, because one budget is the probe's patience rather
    than the tool's silence. `max_s` is now the longest silence rather than the
    longest gap — a run with no failed attempt reads 0.0, and the slowest answer
    is `rtt_max_ms`'s to report. Four selfcheck cases pin it: the ordering of
    the two constants (the invariant that was violated — both were 5.0 and
    nothing noticed), that a slow successful answer is not a wedge, that a run
    of failures is one measured end to end, and that a silence surrounded by
    answers is still found. **The re-run is the point**: a gate that fails a
    subject arm on any wedge cannot be allowed to flip on whether a lost SYN
    costs 3 s or 7 s, and every tool pays that on a 5 %-loss path — the peers'
    `loss5` p99s are 1.7–2.7 s and `nps` wedged once in the previous sweep,
    which was reported and not gated only because it is a peer.
  - **The gate passes, and the wedge axis is what changed.** The first sweep of
    this branch that is green end to end: 90 measured cells, the endpoint
    invariant on 64 samples, the SLO on 6 clean-stage cells, 0 drowned cells,
    drift and wedges both checked, and all five arms sustaining 8 streams.
    **Every arm reads `wedge_count = 0`** — not just the two subject arms. That
    is the metric doing its job: with a silence defined as unanswered attempts,
    a 5 %-loss stage no longer reports a slow answer as one, and nothing has to
    be waived.
  - **A third harness defect, found by that sweep and fixed (2026-10-15).** The
    provenance verdict — "is the tree clean" — ran `git status -- .` from
    `benches/scripts/bench/`, so an uncommitted change under `src/`, which is
    the case §10's rule exists for, could not be seen; the run read
    `tree_clean: true` with two files uncommitted (documentation, as it
    happens, so its numbers were sound and its flag was not). The status now
    runs from the repository root, and `provenance.dirty_paths` carries **which**
    files were dirty, because a boolean nobody can check is a claim. Four
    selfcheck cases pin it against a throwaway repository, including the one
    that matters: a dirty `src/` seen from the nested directory.
  - **The L3 throughput ceiling, and the default that was measuring it
    (2026-10-15).** The ceiling is a **serialized per-packet path** — one device
    reader and one injector per claim — so a claim's throughput follows its
    *lanes*, and the budget decides how many it may hold. Measured on one host,
    one binary, five claims, `bulk-n` (eight inner flows), everything else at
    its default:

    | lanes per claim | throughput | CPU per Gbit | wire/visitor byte |
    |---|---|---|---|
    | 1 (the default until this cycle) | 2.351 Gbit/s | 2.482 s | 1.1401 |
    | 2 | 3.283 | 3.076 | 1.1460 |
    | **4** | **5.191** | **2.329** | **1.0246** |
    | 8 | 5.255 (+1.2 %, inside the run's own 1.13 % floor) | 2.253 | 0.9950 |

    Four is the knee — two steps past it buys 1.2 %, two below it costs 55 % —
    and a claim carrying a **single** flow gains nothing from any of them
    (1.966 / 1.962 / 2.050 / 2.087 across one to eight, i.e. flat inside the
    floor). **The product's default was one lane per claim**, which is why the
    release's L3 row read 2.27 Gbit/s on the staged timeline while four lanes
    read 5.19 on the same host: the published number was the weakest
    configuration of the architecture, and the configuration page had even
    documented that "with the default no claim ever has a lane to spare and
    nothing is lent".
  - **So the default changed** (`DEFAULT_CLAIM_LANES = 4`, a BREAKING note in
    the changelog): an L3 client that writes no `[transparent.data].tunnels` now
    holds four carrier connections per claim. The cost is priced where an
    operator meets it — the configuration page carries the table above plus
    `lanes x claims` connections, ≈2.6 FDs and 0.5–0.8 MiB RSS **on both ends**,
    and a slot in the server's `max_tunnels_per_client` valve (unlimited by
    default) — and `tunnels` equal to the claim count is the documented path
    back to the old shape. **What was *not* done, and why:** an earlier reading
    of this same ceiling said the lever was TUN offloads (TSO/GRO) and possibly
    a wire change. That is wrong twice over: the packet-size lever is real but
    needs a **jumbo path** (the docs' 1400 → 8000 TUN MTU measurement ran with
    `--link-mtu 9000`, and on a 1500-byte path the server's injection would
    produce oversized packets for the visitor leg), and TSO on a TUN implies
    `NETIF_F_HW_CSUM`, so userspace would receive partial checksums and
    forwarding them needs the checksum/GSO state carried per packet — v6. The
    lane count is the lever that needs neither.
  - **Lending is the mechanism this leaves in place, and it is now the open
    one.** `lend` is on by default and moves an *idle* lane to the claim with
    more flows than lanes; with four lanes per claim the measured `bulk-n` is
    the same with lending on and off (5.191 against 5.187), because four lanes
    already reach the knee. Its value is therefore in the *budget-with-slack*
    regime — which the new default now creates — and **its own measurement is
    still missing**: the 7-second `bulk-n` cell cannot show it (lending needs
    ticks and confirmations to converge, and each `dev` round restarts the
    processes), and the one run that did show it converging (`l3bud`, up to six
    lanes on the busy claim) was voided by the shaper bug at the top of this
    entry. A long-hold scenario is what would settle it; the arms already exist
    (`l3-budget10-lending` against `l3-budget10-nolend`).
  - Two things about the instrument, learned the hard way and worth keeping:
    **`--condition` has no effect on a staged run** (every stage sets its own
    condition from the timeline), so the only way to get a delayed L3 path is a
    timeline that contains a delay stage — the first attempt at this experiment
    therefore measured clean paths and proved nothing; and a shortened timeline
    does not reproduce the phenomenon reliably, so a scaled run can *disprove*
    but should not be used to *measure* the effect. A third: **a live `tc -s
    qdisc show` inside the running namespaces is the fastest way to see this
    topology's loss**, and it is worth reaching for before another mechanism is
    proposed.
- **The v0.11.0 pre-release checks that are not in the chain, all green**: the
  interop matrix (`just interop`, 3 cases — both directions refuse the other's
  dialect and keep serving a peer of their own version, against the released
  v0.10.0 binary) and the L3 acceptance harness (`just l3-accept`, 15
  assertions including the negative policy half). Both were broken by the
  mandatory-Noise change and both were repaired with it; neither would have
  been caught by `just check`.

- **v0.10.0 shipped on 2026-10-01**: the release run is green on all thirteen
  jobs (nine platform builds, GitHub Release, GHCR, crates.io), and the
  released artifact is `benches/records/results-soak-v0.10.0.json` — measured
  at `96bda00` with the model the bench model replaced, and gated by the
  sweep's own check with no waiver. That schema is history now: the next
  release publishes `results-bench-vX.Y.Z.json` and gates it with
  `just bench-gate`.
- **The release cost four tag pushes, and the three failures are worth
  remembering**: the emulated cross legs (`native_target` guard missing on a
  new spawn-based test), the musl leg (an assertion pinned to glibc's wording),
  and the `aarch64` cross leg (the release matrix's cache shared across legs
  with different glibc). The first two are in `tests/` — which forced the sweep
  to be re-run on the commit that carries them — and the third is in
  `release.yml`. All three are fixed; the open thread at the bottom of this
  page proposes the check that would catch the next one mechanically.
- **The repository state**: `version = "0.10.0"`, `CHANGELOG.md` carries
  `## [0.10.0] - 2026-10-01` with `[Unreleased]` empty, and the release tag
  `v0.10.0` names `104626f`.
- **Only docs and assets may follow a sweep.** `githooks/pre-tag` reads the
  results file's recorded revision and fails the tag if `src/`, `tests/`,
  `Cargo.*`, `build.rs` or the bench model's own code changed since — the
  changelog date and the README numbers may, code may not (AGENTS.md §10,
  "prove provenance").
- **The repository-settings items are a human's to make** — branch protection
  for `main` and the release secrets. They are tracked as an open thread below;
  nothing in this repository can enable them.

## The transparent-L3 cycle (unreleased, branch `feat/transparent-l3`)

**State as of 2026-10-09.** The feature is committed and green
(`45adb6e` the feature, `edffa53` never-encrypted, `b1ce03b` the striping
note): the transparent (L3) client makes itself the owner of a public
`ip:port`, carried as whole IP packets over a TUN device on both ends, with the
wire at **v5** and `just l3-accept` (root-only, outside the check chain)
proving the transparency end to end. Because none of it is released, the
configuration surface is free to change — and this cycle changes it.

### Decisions

| # | Decision |
|---|---|
| L1 | L3 is a **run mode**, not a service type: touching a NIC is a process-level capability, so an entry-level key must not be what opens it |
| L2 | The mode is chosen by its own top-level table (`[transparent]`), exactly as `[server]`/`[client]` are; a CLI flag only overrides |
| L3 | `[client]` and `[transparent]` in one file is `Undetermine`: two roles are two processes (capability and restart isolation) |
| L4 | The server gets **no** mode. It is a policy host: one config serves encrypted L4 clients and plaintext L3 ones |
| L5 | `[server.transparent]` **is** the server's switch: absent, a registration is refused by policy *before* any device is looked at, so a client can never be what makes the server reach for `/dev/net/tun` (shipped first in this cycle) |
| L6 | **No** `allow_addresses`: one shared `default_token` is one trust domain, so a global address list cannot discriminate between clients; the address universe is already expressed by the operator's own routing |
| L7 | Noise and L3 are **not** mutually exclusive — the Noise keys simply have no effect on an L3 client. The documented consequence: enabling L3 means that server accepts plaintext connections from it |
| L8 | `allow_ports` keeps gating L3 too: it is documented as the master switch for dynamic registration, and an L3 claim is a registration |
| L9 | A claim's address key stays `remote_bind_addr`, and `proxy` stays in `[transparent.transport]` (the only key there — no encryption key has a home in that table) |
| L10 | Header compression is an **intermediate state**: keep it when the measurement shows benefit, remove it when it does not, against criteria written down *before* the run |
| L11 | No startup INFO for the switch; the plaintext consequence lives in the docs instead |
| L12 | `tunnels` is one quantity in both models — **how many carrier connections this client holds for that carrier**: L4 spends them on a pool that carries unbounded per-visitor streams, L3 hands them to claims as lanes |
| L13 | The data plane's shape is **derived from the service type**, not configured: a forwarding service always multiplexes, a transparent claim never does. `mode` is deleted, and a config that still carries it is refused by name |
| L14 | A claim's lane count is `max(1, floor(tunnels / claims on that carrier))`, and `tunnels` unset means one lane per claim; a budget below the claim count is refused with the number to write |
| L15 | A lane **is** a connection, so reclamation moves **idle lanes only** — at most one per tick, never below one per claim: taking a busy lane would drop that claim's packets while its replacement was dialled anyway |
| L16 | Flow placement is a **per-flow table**, not a hash modulus: assignment happens once, into a *live* slot; established flows never move; entries expire after silence in either direction, so a lane change cannot reorder a flow |
| L17 | `[server.data].max_tunnels_per_client` counts a client's **carrier connections**: a transparent claim's direct lane registers against it, a per-visitor channel (the no-`multiplex` build's shape) does not |
| L18 | The mux×1-versus-direct gap is attributed with the model before it is optimised, and each optimisation lands only if it clears that run's own noise floor. No reorder buffer, no mid-stream framing switch, no compact single-stream mode |

### Next, in order

1. ~~**C — the `[transparent]` run mode and its config model.**~~ **Done**: the
   `[transparent]` block, `[transparent.claims.<name>]`, `RunMode::Transparent`
   with `--transparent`, the `[client.transparent]` and
   `protocol = "transparent"` redirects, and the nine per-key refusals deleted
   (those keys have no home in the schema). The lowering is a front end: the L3
   block becomes the same `ClientConfig` the forwarding path already runs on, so
   `core/client.rs`, the data path and the wire were not touched. Validation
   takes the model as an argument (`ClientModel`) rather than being forked, and
   its messages follow it — a claim is a *claim*, and the block named is
   `[transparent.control]`, not `[client.control]`.
2. ~~**M — measure the L3 path before writing a compressor.**~~ **Done**
   (2026-10-09, `9507a7a`): the harness samples `/proc/net/dev` per arm and
   `benches/scripts/l3/wire_report.py` reports carried packet sizes and a
   header-compression ceiling. Campaign in `docs/benchmarks.md`, "The
   transparent-L3 wire question".
3. ~~**Z — only if M passes.**~~ **Not built: the measurement says no.** The
   ceiling on the workload compression exists for (2000 round trips of 64 B) is
   **17.9 % of the wire** — under the 20 % bar written before the run, and an
   upper bound before exemptions. The same table says why: 196 B of wire per
   116 B carried packet, i.e. ~80 B of tunnel transport per packet (our 2-byte
   length, the multiplexer's frame, the carrier's TCP/IP header and its ACKs)
   against ~35 B of compressible header. **Framing is the lever, not headers**;
   batching several packets per frame is the next candidate and needs its own
   measurement.
4. ~~**V — the verdict, before any `v0.11` tag.**~~ **Recorded above, and it is
   a no**: nothing was added to the wire, so there is nothing to remove and no
   v6 risk.
5. **Done after M: the L3 data path was optimised where the measurement pointed.**
   Batching (drain the device, frame each packet where it is read, hand a run of
   frames over as one write) took 12 % off the bulk arm's wire bytes; the L3
   model's `[transparent.data].default_mode` is now `direct` (a claim has one
   channel, so the multiplexer was pure per-packet cost), worth another 6 % of
   the wire, a third of the CPU per packet and 65 % more small round trips per
   second. Numbers and method in `docs/benchmarks.md`, "The transparent-L3 wire
   question". What the same instrument now says is that **CPU, not the wire, is
   the next lever** (~135 µs of daemon time per carried packet on the small
   arm), and that it **cannot yet answer the many-flow case**: every arm is one
   connection, so parallel TUN queues (`IFF_MULTI_QUEUE`, one reader per queue)
   are a candidate that needs a concurrent-flow arm before it can be claimed
   either way. That is the next measurement, not the next feature.

   **Both of those are now answered, and neither is the next thing to build.**
   The harness gained a multi-flow arm *and a control beside it* (the same flows
   with the tunnel out of the path): at 16 connections the tunnel carries
   19 900 round trips/s against the control's 23 750 (84 % of a ceiling that is
   the probe's own), at 64 connections 27 700 against 30 900 (90 %). So parallel
   TUN queues have no measured need — the device reader is not what limits a
   busy host, and a control is what makes that statement checkable instead of
   plausible. Bulk is the arm where the path *is* the limit (338 Mbit/s against
   the control's 602), and it moved a long way by batching the read side as well
   as the write side (+27 % throughput, −56 % CPU per packet).

   **The remaining levers were then attributed, and none of them is a code
   change.** Two instrument errors had to be fixed first, and both are worth
   remembering: the bulk arm sent one blob and read afterwards, so any run past
   the buffers measured a *deadlock* (two runs of 20 MB and 100 MB were reported
   as throughput when they were five-second stalls), and the harness built
   **debug** while the product ships release — the same arm measures 783 Mbit/s
   in debug and 1986 Mbit/s in release. Both are fixed (`BULK_CHUNK`,
   `PROFILE_BUILD=release`), and the numbers below are release.

   Release, one host, with the no-tunnel control beside each arm: bulk 200 MB in
   65 KB chunks **1986 Mbit/s** against the control's 12 024; a paced single flow
   **10 362 round trips/s**; 16 flows **22 543** against the control's 23 388 —
   **96 % of a ceiling that is python's, not the path's**.

   A `perf` profile of the release build during the bulk arm puts **84 % of the
   CPU in the kernel**, 10 % in molehill, 5 % in libc, with no symbol above 11 %:
   the cost is four syscalls per packet (TUN read, socket write, socket read, TUN
   write), spread thin. That retires the synchronous data path as a plan — it
   would chase the userspace tenth, not the kernel's five sixths — and it names
   the lever that does work: **packet size**. The same bulk bytes in 8000-byte
   packets instead of 1400 measured **3579 Mbit/s** at half the CPU per byte, so
   the TUN MTU (an operator setting, with a link MTU to match) is worth more than
   any remaining code change, and the deployment recipes now say so.
6. **One model, and it owns the sweep too (2026-10-10, unreleased).**
   `benches/scripts/bench/` is now the only runner: the soak sweep, the
   L3-versus-L4 comparison, the memory sampler, the HTTP latency script and the
   peer adapters are all *this* model — its profiles, its scenarios, its arms.
   What it grew in the migration:

   - **peer arms** (`peers.py`): frp, rathole and nps are arms like any other,
     configured per topology, with their release binaries fetched by
     `just bench-peers` and their versions recorded in the results meta. The
     first end-to-end peer run (one condition, `cost`, 60 s) read frp at
     6.08 Gbit/s and molehill at 13.58 Gbit/s, with the p99 and CPU-cost
     metrics beside them — the four-tool comparison, on the model's terms.
   - **conditions and timelines** (`model.CONDITIONS`, `model.TIMELINES`):
     `clean`, `rtt100`, `loss1`, `loss5`, `rate100`, `rate20`, `jitter` and the
     two MTU classes, imposed *in place* on a veth leg (`tc qdisc replace`), so
     the tool keeps running while the path changes. `--scale` shortens a
     timeline's holds without changing its shape, and the scale is part of the
     fingerprint.
   - **the staged scenario** (`timeline`, `soak`, `cost`): a bulk spine
     re-dialed per stage after a drain predicate (queue below a tolerance and
     no send-capable socket, held for two polls, with a budget that is recorded
     when it fires), three continuous series probes (interactive, datagram,
     churn) sliced per stage, a drift sampler (RSS/fds/threads) across the
     whole run, and wedge detection (>5 s of silence).
   - **the capacity ramp** and **reconnect**: one cell per stream level with a
     fresh-connection ping beside it and the SLO verdict per level, and five
     timed cold starts.
   - **`plot.py`**: the six figures (timeline, stages, capacity, UDP, drift,
     cost) and five markdown tables, rendered from the new schema, each figure
     carrying its method in the footer.
   - **`bench.py gate`**: coverage of every declared cell, the endpoint
     invariant, the SLO on the clean stages (the product's arms only — a peer
     that misses it is reported, never blocked), the drift and wedge axes (a
     slope is only judged past five minutes), the capacity ramp, and the
     regression half against a baseline.

   The retired tree (`benches/scripts/soak/`) is deleted; its published records
   moved to `benches/records/` as history, and the release ritual now names
   `results-bench-vX.Y.Z.json` + `assets/bench-vX.Y.Z*.png` and gates with
   `just bench-gate`. The model is self-contained: the iperf3 conventions
   (`iperf.py`), the provenance probes (`hostinfo.py`) and the peer adapters
   all live in it, with no import from the retired sweep.

   Not migrated, deliberately, with the reason: the **slow visitor** probe
   (opt-in head-of-line instrument; the model's UDP and TCP shapes cover the
   load shapes, and a HOL question would come back as a scenario), and
   `udp_stress.py` (it reads molehill's *internal* UDP queue counters to answer
   a molehill-only design question, which the model's externally-observable
   rule forbids in a comparison — it stays as a diagnostic, outside the model).

   What is still open: the sweep has not been run end to end on the migrated
   engine (the pieces are validated: a staged timeline over three arms, the
   capacity ramp with the SLO per level, a peer arm, the drift and wedge
   metrics, the figures from a real staged file), so the first release sweep is
   also its acceptance test. The 5 Gbit/s UDP ladder cell and the loopback
   ceiling remain the two places where a single run cannot order the arms.

7. **The carrier axis and the L3 data path, measured (2026-10-10, unreleased).**
   The hypothesis on the table was "L3 + KCP is better overall, and the
   multiplexer should not cost much". The model answered both, and the answers
   are *no* and *it costs 28 %* — with one exception each way, and with the
   real L3 lever showing up in the same campaign.

   **The carrier is now independent of the mode** (`08e72aa`): a `direct`
   service or claim can ride KCP — one session per channel, no yamux above it —
   where the config used to refuse the pair. The server's KCP listener reads the
   data-channel hello beside the tunnel hello, the client dials the carrier its
   service declared, and the model gained the axis to measure it (`fd4c9aa`):
   arms `l3` / `l3-mux` / `l3-kcp` / `l3-mux-kcp` / `l4-kcp`, the carrier in the
   generated config and in the evidence, and the condition `loss1_rate100` (a
   100 Mbit/s, 20 ms, 1 %-loss leg — the lossy WAN the choice is about).

   What the model measured (`--profile smoke`, two rounds, A/A twin in every
   run, conditions on the **tunnel** leg, tables in
   [docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model)):

   | Condition | control | tcp carrier | kcp carrier |
   |---|---|---|---|
   | clean, `l3` (direct) | 41.8 | 4.17 Gbit/s, 0.855 s/Gbit | 2.19 Gbit/s, 2.554 s/Gbit |
   | clean, `l3-mux` | — | 2.99 Gbit/s, 1.435 | 2.51 Gbit/s, 2.310 |
   | `loss1`, `l4` / `l4-kcp` | 4.43 | 2.51 | 0.40 |
   | `loss1`, `l3` / `l3-kcp` | 4.43 | 2.03 | 0.41 |
   | `loss1_rate100`, bulk | 0.096 | 0.093 (`l4`), 0.089 (`l3`) | 0.059 (`l3-kcp`), **failed 3/3** (`l4-kcp`) |
   | `loss1_rate100`, `rr-16` | 323.2/s | 328.0 (`l4`), 335.3 (`l3`) | **337.8 (`l4-kcp`)**, **338.6 (`l3-kcp`)** |

   Read: the multiplexer costs **28 %** of the bulk throughput and 68 % more CPU
   per byte; KCP costs **48 %** and 3× the CPU per byte *on a clean path*; under
   loss KCP is **five to six times behind TCP** in both architectures; and on a
   rate-limited lossy leg the multiplexed KCP arm does not finish its test at
   all — `KCP session dead link`, which kills the visitor's connection with it.
   The mechanism came out of the carrier's own counters
   (`MOLEHILL_KCP_STATS=1`): **22 % of the datagrams it sent were
   retransmissions**, because with `nc=1` there is no congestion control and the
   pacer's only signal is a PONG that does not arrive within 2.5 s — a
   window-sized burst goes into whatever queue the path has, the standing queue
   delays the acks past the escalating RTO, and the retransmissions enlarge the
   queue until the ARQ declares a live-but-rate-limited peer dead.

   **Both obvious fixes were tried and both are wrong.** `nc=0` (KCP's own
   congestion control) as an A/B against the shipped build on `loss1_rate100`
   measured **0.002 Gbit/s against 0.043** on bulk and 256/s against 344/s on
   `rr-16` — KCP's built-in control collapses the window on a lossy path and
   never recovers. And capping the send window to the path's BDP (2048 → 256
   segments, `set_wndsize`) left bulk where it was (**0.039 against 0.044**) and
   the multiplexed arm still died. The counters say why a knob cannot do it: on
   that leg the sender retransmits **24 % of its output** against a 1 % loss
   rate, so the recovery loop itself is the defect — it needs a controller
   driven by what the path delivers (delivery rate and/or RTT inflation), in the
   adapter, with the `loss1_rate100` A/B as its gate. The engine exposes
   `set_wndsize`/`wait_snd` but not `snd_una`/`rx_srtt`, so such a controller
   starts with two accessors. The one place KCP won is worth keeping in view: on the same leg,
   many short interactions (`rr-16`) were fastest on both KCP arms with the best
   p99 — fast retransmit pays, and it is the bulk path where the missing control
   costs more.

   **The larger finding is about L3 itself.** The same campaigns measured the
   data path against the forwarding path on one host
   ([table](docs/benchmarks.md#the-l3-data-path-what-limits-it-2026-10-10-this-model)):

   | Arm | `bulk-1`, TUN MTU 1400 | `bulk-n` (8 flows) | `bulk-1`, TUN MTU 8000 |
   |---|---|---|---|
   | `l3` | 4.34 Gbit/s | 3.93 | **8.00** |
   | `l4` | 8.99 | 22.33 | 8.00 |

   Two conclusions, both actionable. **L3 does not scale with flows** (eight
   flows move less than one) — the ceiling is a serialized per-packet path, not
   a window, so more connections through a claim cannot raise it. And **packet
   size is the lever**: an 8000-byte TUN MTU (link MTU to match) took the same
   flow to 8.00 Gbit/s (+84 %) at a **third** of the CPU per byte, matching the
   forwarding path at one flow. That is an operator setting, and
   [deployment.md](docs/deployment.md#transparent-services) now carries the
   number.

   At that packet size a second measurement says *what* the remaining ceiling
   is — the forwarding path with its pool capped, so that one arm prices one
   carrier connection:

   | Arm (TUN MTU 8000) | `bulk-1` | `bulk-n` (8 flows) |
   |---|---|---|
   | control | 38.672 | 44.805 |
   | `l3` | 7.136 | 5.847 |
   | `l4` (pool ≤ 4) | 8.286 | 21.604 |
   | `l4-mux1` (one tunnel) | 9.819 | 12.646 |
   | `l4-mux8` | 9.058 | 21.955 |

   One carrier connection tops out near 8–10 Gbit/s here (the one-tunnel arm
   says so at one flow *and* at eight streams over that tunnel), and the
   forwarding path passes it only by spreading streams over several tunnels. A
   claim has exactly one channel, so it sits at the per-connection ceiling by
   construction — that is why its `bulk-n` is below its `bulk-1`. Going past it
   is a **design** decision (per-flow sharding: several channels per claim, one
   per inner flow), not a tuning one, and the model cannot price it before it
   exists.

   Open, in the order the measurements argue for them:

   - **A device path that carries more than one packet per syscall** — chosen
     as the next lever (2026-10-10), and **the kernel half is now verified**
     (spike outside the repo, same host):
     - attaching to an operator-created device with `IFF_VNET_HDR` works, and
       so do `TUNSETVNETHDRSZ` (its argument goes **by pointer**) and
       `TUNSETOFFLOAD` (its argument goes **by value** — mixing the two
       conventions returns `EINVAL` for every mask, which cost an hour of the
       spike);
     - `TUN_F_CSUM|TUN_F_TSO4|TUN_F_TSO6` is accepted, so the device advertises
       TSO/GSO;
     - one 54 918-byte **USO** write (`gso_type = UDP_L4`) was delivered as
       **40 datagrams of 1372 B** to a local socket: one syscall, forty packets,
       segmented by the kernel;
     - one 54 440-byte **TSO** super-packet was accepted and crossed a veth as
       *one* 54 KB skb (the virtual link preserves GSO), so segmentation is
       deferred to wherever it is really needed.

     Two more things the same day's experiments settled, both of them reasons
     the header does **not** land on its own:

     - **The offload flag is two-way, and enabling it alone breaks every
       claim.** With `TUN_F_CSUM` on and the virtio header dropped on the floor,
       no visitor could reach a claim at all (measured: both the framing run and
       the readiness probe failed for every round). The reason is the topology's
       own veth: it hands over packets whose transport checksum is still
       partial, the device advertises that it may do so, and a daemon that
       forwards the packet without its flags forwards something the far end can
       only drop. The header's `flags`/`csum_*` fields have to travel with the
       packet — which is the wire change, not a local one.
     - **The header without the offloads is a pure cost.** Attach with
       `IFF_VNET_HDR` only (offloads off, packets complete, everything still
       working) measured **4–5 % below** the released build on `bulk-1`: three
       rounds, the A/A twin beside it, every round below both old arms
       (4.041 against 4.188/4.255 Gbit/s). The header costs a 10-byte copy on
       read and a two-segment `writev` on write, and buys nothing until the
       writes coalesce. So it lands *with* the GSO write, as one change whose
       A/B has a win to show.

     **What is still unverified** is the half the win depends on: whether the
     kernel *coalesces* on the read side. The spike could not answer it — a UDP
     stream arriving on a veth is fragmented, not GRO-able, and the reads came
     back one packet each with the header all zeroes (10 169 reads of 1406 B,
     no `gso_type`) — so the TCP case has to be measured where TCP flows exist:
     in the bench, with the L3 arm at 1400 and at 8000, watching
     `mean_carried_packet_b` (a frame above the MTU is a coalesced read) and
     `syscalls_per_s`. If the read side never coalesces, the win is only the
     inject side's, and the design has to coalesce a run in userspace — which is
     why the two slices below are ordered that way.

     The design that follows, in two slices:

     * **Slice 1, no wire change.** Both TUNs attach with `IFF_VNET_HDR` and
       enable the offloads; the *inject* side groups consecutive packets of one
       flow (same 5-tuple and direction, consecutive TCP sequence numbers or
       equal-size UDP payloads, no SYN/FIN/RST inside the run, DF set) and
       writes the run as one GSO packet, falling back to one write per packet
       whenever the run does not qualify or the device has no offloads. The
       frames on the wire stay one packet each, so nothing needs a version bump
       and an old peer keeps working.
     * **Slice 2, with a wire change.** The virtio metadata travels in the L3
       frame, so a super-packet read from one TUN is carried as one unit and
       injected as one write at the far end (and the reverse direction too).
       This is a protocol change (v6) and is only worth it after slice 1 has
       been measured.

     Measurement plan, all of it on the model as it stands: the L3 arms
     (`l3`, `l3-deep`) at the default 1400-byte TUN MTU and at 8000, on
     `bulk-1`/`bulk-n`/`rr-16` and on `udp-pace` (`bytes_per_syscall` and
     `syscalls_per_s` are the readings that must move — they are device-I/O
     metrics, so an L3 arm is exactly where they are valid), with `l3-deep`'s
     deeper queue as the control for "is it syscalls or queueing". Success is a
     throughput gain the run's own A/A floor can see, at no cost to `rr-16`'s
     latency or the wire ratio.
   - **The datagram size follows the path — landed, measured, kept.** The
     carrier had been pinned at 1400 bytes with a shrink-only adaptation, so
     every jumbo path paid the per-datagram cost 5.7× more often than it had
     to. `Kcp::set_mtu` (was `shrink_mtu`) now follows the caller in both
     directions, the adapter computes the size from the kernel's path-MTU
     answer up to an 8 KiB ceiling (`datagram_for_path`), re-reads it once a
     second, and every inbound buffer is sized from the *ceiling* rather than
     from the engine's default — the spike's first jumbo attempt arrived as a
     retransmit storm because `DGRAM_BUF` was 2048 against 8000-byte segments.
     A/B against the previous build, two rounds an arm, `l3-kcp`, one bulk
     flow: clean jumbo **2.170 → 3.026 Gbit/s (+39 %)** at −28 % CPU per Gbit
     and a better wire ratio; `loss1` jumbo **0.407 → 1.713 (+4.2×)** at −40 %
     CPU; `loss1` on a 1500-byte link **neutral** (0.406 → 0.434), which is the
     safety property — the probe keeps a normal path at the size it carries;
     `rr-16` on `loss1_rate100` +3.8 %.
   - **The dead link was the rule, and it is fixed.** The engine inherited the
     reference's dead-link rule — twenty retransmissions of one segment ends the
     session — which conflates a peer that is gone with one that is slow. On a
     rate-limited path the acknowledgements queue behind the shaper, so a
     segment collects twenty retransmissions while the peer answers everything
     else, and a live session is closed. The rule now needs the count *and* a
     send window that has not moved for five seconds (`KCP_DEAD_GRACE_MS`, with
     `last_progress` set wherever `snd_una` advances). Measured on
     `loss1_rate100` + a jumbo link: the multiplexed arm failed `bulk-1` in
     **every** round before and completes **both** after (0.058–0.059 Gbit/s,
     17 % fewer wire bytes than the direct arm), and the direct arm's CPU per
     byte fell by a fifth. The one cost: a genuinely vanished peer is now
     detected up to five seconds later, which the session-level watchdogs
     (`FORWARD_IDLE_TIMEOUT`) bound anyway.
   - **The collapse was the pacer, and it is fixed.** The trigger turned out to
     be blunt: in a session's lifetime, the *first* cell of a turn runs at full
     speed and *every later* cell runs at a fraction, whatever the scenario
     (`bulk-n,bulk-n` 6.18 → 1.00; `bulk-n,bulk-1` 5.86 → 1.08; `bulk-1,bulk-n`
     7.1 → 2.0). The state was the adaptive pacer's allowance, visible once a
     gauge for it was added: 12 → 9 → 6.75 → 5.06 → 3.8 Gbit/s across the first
     cell's PONG timeouts, held for the rest of the session because the recovery
     is 5 % per four clean PONGs. A PONG that is late because the peer is busy
     sending is not congestion, so `PaceState::on_ping_timeout` now cuts only
     when the send window has not moved since the PING (`Kcp::snd_una`, sampled
     at PING time). Measured after the fix: `bulk-n,bulk-n` **7.79 then 7.67**,
     `bulk-1,bulk-n` **7.19 then 7.44** — no collapse — while `loss1` jumbo
     stays at 1.67–1.71, `rtt100` is 0.169 → 0.182 and `loss5` is unchanged.
     Two detours are recorded rather than hidden: a **window byte budget** (to
     stop the buffers growing 5.7× at a jumbo MTU) fixed nothing and cost the
     lossy leg two thirds of its throughput (0.45 against 1.67 Gbit/s on
     `loss1`), so it was reverted — a lossy path wants the window *wider* than
     the BDP, not closer to it; and a **1 ms ack-batching window** was −6 % to
     −12 % on the clean path and was reverted. What remains open from the
     original finding: nothing — the collapse is gone, and the numbers that
     replaced it are on the benchmarks page.
   - **L3+KCP matches the TCP carrier on a jumbo single flow and now leads it on
     eight (historical note; see the entry above for the fix).** With the segment size following
     the path and the dead link fixed, one bulk flow through an L3 claim at a
     jumbo TUN MTU (8000) and a jumbo link (9000) measures **7.265 Gbit/s
     against the TCP carrier's 7.086** in the same run (A/A twin 6.948, so both
     are inside the noise of each other) at 2.3× the CPU per byte — the
     user-facing claim "L3+KCP can be as fast as L3+TCP" now has a measurement.
     What does *not* hold is the eight-flow cell, and the trigger is
     reproducible and narrow:
     - `--arms l3-kcp --scenarios bulk-n` (alone): **5.9–6.2 Gbit/s** across
       four runs;
     - `--arms l3-kcp --scenarios bulk-1,bulk-n` (a bulk cell first): **1.99 and
       3.54 Gbit/s** in two runs — and in a 4-arm, 3-round campaign, 2.06 in
       every measured round while the TCP carrier beside it read 5.6–5.9;
     - the engine's own counters are *identical* in both cases (server
       retransmits ~15–18 % of its datagrams either way, client `ms_input`
       1.5–1.9 s, `ms_output` per datagram 7.5 µs), and the difference is the
       **bytes per datagram**: 6.2 KB when it is fast, 3.4 KB when it collapses
       — the sender is producing smaller datagrams, not more work per datagram.
     So the next step is to find why a preceding bulk cell makes the carrier
     emit half-full datagrams: the candidates are the hub's batching (it flushes
     "the moment the device runs dry", and eight flows drain the device
     differently from one) and the KCP writer's chunking downstream of it.
     Reproduce with `sudo -n uv run benches/scripts/bench/bench.py run --profile
     smoke --arms l3-kcp --scenarios bulk-1,bulk-n --condition clean
     --link-mtu 9000 --tun-mtu 8000 --rounds 1 --warmup-rounds 0`.
   - **The whole-workload sweep (`dev`, jumbo path, A/A twin) says where this
     stands**: level with the TCP carrier on one bulk flow (7.136 against 7.554
     and a twin at 7.496) and on churn (6 779 against 7 139, twin 7 209),
     **19 % ahead on eight flows** (7.418 against 6.023, twin 6.243), **behind on
     short round trips** (rr-1 10 177/s against 14 167/s, rr-16 30 264/s against
     32 984/s, p99 equal) and **about twice the inner-UDP loss** at every rung of
     the ladder (1.05 % against 0.60 % at 2 Gbit/s). The UDP loss is the one to
     chase next if quality matters more than throughput: it is the hub's
     endpoint channel dropping when the carrier's writer is slower than the
     ingress, and the loss is unrecoverable for a UDP flow.
   - **The long-haul gap is the next real target, and its mechanism is now
     measured.** `rtt100` (100 ms each way, no configured loss): TCP carrier
     0.461 Gbit/s, `l3-kcp` 0.178 — the widest gap left in the model. The
     carrier's counters put 21 % of the sender's datagrams somewhere on the
     floor and 28 % of its output into recovery, split 4 239 RTO against 2 163
     fast-retransmit (so mostly spurious timeouts). **Kept:** RTO headroom of
     `srtt/8` (`KCP_RTO_HEADROOM_DIVISOR`), worth −42 % RTO resends and a wire
     ratio of 1.20 → 1.09 with throughput flat. **Falsified:** pacing the burst
     to the BDP (`window / srtt`) is 35 % *slower* (0.114 against 0.175 Gbit/s,
     three rounds each, ranges disjoint) and does not change the wire ratio —
     the pacer's bucket-and-rate gate does not remove the losses, it only slows
     the sender. So the next attempt has to find where those datagrams actually
     die (the receiving socket's buffer, the veth queue between the namespaces,
     or the ARQ's own flush cadence) rather than throttle the sender: the
     obvious throttles have all been measured now. The diagnostics this needed
     are on the `MOLEHILL_KCP_STATS` line for good: `resends_rto` vs
     `resends_fast`, `rto_ms`/`srtt_ms`, the window state, the reader/spill
     residency and the pacer's allowance.
     Two more results from chasing it, both landed: **the drain parks what the
     pacer or the kernel refuses** instead of dropping it (worth +12 % on
     `loss1`, 1.824 against 1.627 Gbit/s with disjoint ranges, at an identical
     wire ratio) and the park is bounded by half the RTO so the ARQ does not
     duplicate what the pacer is holding. And one **trade left on the table**:
     with parking in place, capping the send rate at `window / srtt` is +21 % on
     `rtt100` (0.204 against 0.169 Gbit/s) but **+14 % wire bytes** there; it is
     measured, documented on the benchmarks page, and deliberately not carried
     until someone decides that trade. A quick follow-up worth one run: whether
     a *larger* pacer bucket (more burst allowed, less queueing delay) keeps the
     throughput and loses the wire cost.
   - **A rate-aware pacer for the KCP carrier** (above) — **attempted
     2026-10-10, measured, reverted.** The design: sample the peer's
     acknowledged progress every 20 ms, convert it to segments per second, and
     set the send window to `rate × srtt × 1.5`, never below 64 segments. Its
     A/B against the shipped build, same arm, two rounds each:
     clean **neutral** (2.174 against 2.188 Gbit/s), `loss1_rate100` **slightly
     ahead** (0.059 against 0.053 Gbit/s, 9 % fewer wire bytes), and `loss1`
     **twelve times worse** (0.033 against 0.411). The reason is the signal, not
     the intent: `snd_una` measures *contiguous* acknowledgement, so it stalls
     on every gap and the window shrinks exactly when loss needs the opposite —
     and a window too small starves fast retransmit, which is driven by the acks
     of *later* segments; with nothing in flight the sender waits out a 30 ms+
     RTO instead. Two things follow for the next attempt: the rate signal must
     come from acks (or from `KCP_SACKS_SENT`/retransmit counters) rather than
     from contiguous progress, and the window needs a loss-aware reserve with a
     hard floor under fast retransmit's needs. The shipped constant window stays
     until that exists.
   - **The churn scenario reports zeros, and only for L4 arms.** Three runs on
     2026-10-10 (`churn-16`, 2/5/10 rounds) produced four rounds with no samples
     at all and three rounds where the readiness probe could not reach the
     exposed port for its full 30 s — every one of them on an L4 arm or its
     twin, never on L3, and the probe's own log for a zero round *does* contain
     latency samples. Two candidate explanations, neither established: the
     architecture (a fresh visitor connection costs L4 a data channel and a
     pair of ephemeral ports that L3 does not spend) or the probe (which reports
     a rate without saying why connections failed). Until the probe records the
     failure reason, no churn number may be quoted — including the tempting
     reading that L3 beat L4 in both runs' means (6366/7131 against 5638/4932).
     The instrument is the thing to fix first; if the architecture is the cause,
     it is a finding worth having.

     **Closed 2026-10-12: it was the instrument, and the mechanism is now
     named.** The surviving probe logs from the zero rounds read
     `Exception in thread Thread-N: … OSError: [Errno 99] Cannot assign
     requested address` from `Worker._connect` — **ephemeral-port
     exhaustion**, not an architecture effect. `churn-16` opens a fresh
     connection per request; the faster arms (which the L4 ones were, at
     thousands of fresh connections a minute per worker against a ~28 k port
     range with 60 s of TIME_WAIT) exhausted the visitor namespace's source
     ports mid-run, and the resulting `OSError` was raised *outside* the
     worker's try/except, so every thread died uncounted: `ok=0 failed=0
     errors=[]`, a rate of zero, and a cell that looks like "the path carried
     nothing" when it means "the visitor could not open any more sockets".
     L3 read lower rates and never crossed the threshold — which is why the
     zeros were L4-only and the L3-vs-L4 comparison was never evidence.
     Fixed in the probe: a failed connect is now a **counted reading**
     (`connect_errors` in the JSON, in the summary line and the cell's
     `probe` evidence), the worker backs off 50 ms and keeps issuing —
     proven by forcing `_connect` to raise (80 attempts, all counted, no
     thread death) and by a clean 49 183-round-trip local run. **Every churn
     number before this fix is still unquotable** (the count did not exist),
     but new runs can distinguish "slow" from "port-starved", and the
     L3-beats-L4 reading is retracted as an instrument artefact.
   - **What the multiplexer's 28 % actually is.** The earlier per-cell model
     measured 6 % of wire and 33 % of CPU per packet for `direct` over
     `multiplex`; this model measures 28 % of throughput and 68 % of CPU per
     Gbit. The two are not the same reading and the difference is worth
     attributing (yamux frame copies vs the pool's stream machinery) before
     anything is changed for it.

**Pre-registered criteria (kept as written, for the record).** Small packets
(≤128 B payload): wire bytes down ≥ 20 %. Mid (512 B–1 KB): ≥ 5 %. Bulk
(1400 B): throughput down ≤ 2 %. The small-packet and the bulk conditions must
both hold; a difference inside the variance counts as *no difference*, and
either failure removes the feature. The small-packet arm failed on the ceiling
alone, so the other arms were not run for the verdict.

#
## Open threads

- **The mux write boundary's owned-write chain was built and REVERTED — the
  clean cells cannot resolve it (2026-10-12, round 37).** The full chain
  existed and compiled: `KcpTunnelStream::Plain`/`ClientStream::Kcp` taking
  owned writes (`TAKES_OWNED`), the mux frame writer's body stages in `Bytes`
  and handing `buffer.slice(offset..)` over the `AsyncWriteOwned` boundary —
  one full-payload copy fewer per yamux frame on the plain-KCP tunnels. The
  A/B (`l4`, `bulk-1`/`bulk-n`, clean, 3+1 rounds, `--aa`, binaries
  `molehill-{pre-p3,p3}.bin`, results `/home/dsh/tmp/p3-owned.json`,
  fingerprint `642a8ced3bef53df`) read **no claim**: throughput −1.37 %/−0.35 %
  (inside the run's scatter on the 8-stream cell, at it on the 1-stream one),
  `cpu_s_per_gbit` +2.2 %/+0.3 %, RSS −3.7 % inside its 9 % scatter. The
  round-37 attribution above is the mechanism: the kernel's UDP send path
  owns the CPU on these cells, so a userspace memcpy under 1 µs per ~64 KiB
  frame is invisible behind it. The candidate binary is kept
  (`/home/dsh/tmp/molehill-p3.bin`); the diff is rebuildable from this
  description in ~30 min (the files it touched: `src/mux/frame/io.rs`
  `WriteState` in `Bytes` + `poll_write_stage` const-dispatch,
  `src/mux/connection.rs` + `src/transport/multiplex.rs` + the server's
  `upgrade_to_tunnel` bounds on `AsyncWriteOwned`, and the
  `AsyncWriteOwned` impls on `KcpTunnelStream`, `ClientStream` and
  `ServerStream` — `src/common.rs`'s `owned_write` module ungated). What
  would re-open it:
  a cell where the *userspace* share dominates (e.g. `udp-pace`, whose
  paced-rate regime is pump-bound, not kernel-bound — untested here), or a
  kernel-side lever (GSO) landing first.

  The pump-bound re-test that entry promised **also read no claim, and
  closed the thread for good** (2026-10-12): the same saved binaries on
  `l4`/`udp-pace`/clean (3+1 rounds, `--aa`, `/home/dsh/tmp/p3b-pace.json`)
  — gap p99 1.349 → 1.335 ms (−1.0 %, inside its 0.96 % A/A scatter),
  throughput and wire bytes identical to the third decimal,
  `cpu_s_per_gbit` 30.53 → 35.56 (+16.5 %) against a floor of 5.5 % — but
  the per-round values tell the truth a median hides: base [37.6, 30.5,
  29.2] against candidate [35.6, 30.2, 38.2], the base's own first round
  higher than the candidate's median, sign agreement 0 of 3. The paced
  regime at 0.27 cores is too scattered over three rounds to resolve a
  sub-microsecond-per-frame userspace change, and the candidate is exactly
  level everywhere the run can see. The owned-write chain stays reverted:
  built twice, measured twice, no cell resolved it — any further attempt
  waits for the kernel-side term to move first (GSO), because that term is
  what both attributions priced as dominant.

- **The send path is priced: the 3.5 µs/datagram is the kernel's, not ours
  (2026-10-12, round 37 — the round-19 named test, now run).** The KCP stats
  line gained `proc_cpu_ms` (the process's own user+system CPU, from
  `/proc/self/stat`, `_SC_CLK_TCK`-exact), and one clean `l3`/`bulk-1` run
  (`MOLEHILL_DATA_CARRIER=kcp`, run fingerprint `1d95dee85018d12f`,
  `MOLEHILL_KCP_STATS=1`; results at `/home/dsh/tmp/p2-attrib.json`) answered
  it:
  the sending process's CPU rose **3.2× faster than `ms_out_send`** through
  the run's saturated seconds (Δcpu 2,280-2,310 ms/s against Δms_out_send
  700-726 ms/s, four windows agreeing). The process runs both pumps, so the
  user-space total visible on the same line (`ms_out_send` + `ms_input` +
  `ms_deliver` + `ms_writer` + `ms_update`) accounts for roughly 1.0 s/s of
  the 2.28 s/s the process actually burned — **the kernel's UDP send path
  takes more CPU than all of the pump's user-space phases combined.** That
  closes the round-19 question with the verdict it anticipated: no
  userspace send-path change (iovec reuse, `SendBatch` pooling, a larger
  `BATCH`) can pay materially; the lever is datagram *count* — the MTU, an
  operator setting already measured at +84 % (deployment.md) — or GSO,
  which changes the wire path and is the separately-recorded `udp_batch`
  A/B. The send-path thread is closed, not parked.

- **S1 for the L4 trigger: the carriers diverge on the l4 arm, and the
  signal is per-session ARQ health (2026-10-12, round 37).** Two carrier-
  pinned runs through the model (`l4`, `udp-pace` + `udp-ladder`,
  `loss1_buf`, tunnel leg, 2 rounds + warm-up, `--aa` each, identical
  method fingerprints `7cfd30c7a7de38bc`; default binary on KCP-first arm
  vs `MOLEHILL_DATA_CARRIER=tcp`; results `/home/dsh/tmp/p4-s1-{kcp,tcp}.
  json`): **KCP delivers every ladder cell with materially less inner loss**
  — 200M 0.89 % vs 2.79 % (−1.91 pts), 1000M 12.3 % vs 17.3 % (−4.9),
  2000M 52.5 % vs 56.3 %, 5000M 73.1 % vs 76.6 % (every paired round
  agreeing, both floors cleared), and `retrans_segments` **1 vs ~5,000** on
  the ≥1 Gbit/s cells (a floor of 1,000) — while `udp-pace`'s gap p99 is
  51.5 ms against 64.2 ms (directional; its own A/A floor did not clear).
  The cost is stated beside it (B, the TCP arm, is the *cheaper* one):
  `cpu_s_per_gbit` +73.6 % (200M) / +17.4 % (1000M), `rss_peak_mib`
  +58–360 % (a KCP session carries state a kernel TCP socket keeps in the
  kernel), fds −27 % (KCP's per-session sockets).

  What this settles for the rule the mechanism was built for: **the
  degradation signal is the KCP session's own ARQ counters** —
  `retransmits`/`datagrams_out` per session is 0 on the healthy arm and
  ~5,000 per cell on paths where the carrier is losing, a separation orders
  of magnitude wide, and the counters already exist per pump (`KCP_NS_*`,
  `MOLEHILL_KCP_STATS`). TCP has no userspace equivalent, so the rule's
  shape is one-sided by construction: a KCP tunnel whose retransmit share
  over a window clears a floor for `CARRIER_CONFIRM_TICKS`-style ticks is
  a carrier losing its traffic, and `begin_handover` to TCP is the probe —
  never the reverse direction from a TCP signal (there is none), which is
  also why the L3 rule's probe path (the only way to learn is to put a
  lane on it) is the right precedent. Not wired this round: the S2 rule
  needs per-tunnel KCP health surfaced next to the pool's per-tunnel bytes
  (the round-37 counter refactor's natural next piece) and its own A/B on
  exactly this cell — the standing caveat applies (the A/B must cover the
  degrade → switch → fresh-flows window).

- **The delivery ratio is not a discriminator either — the ARQ hides the loss
  (2026-10-13, round 7 of the switching goal).** The peer's report was extended
  with the bytes the server hands each tunnel (a counting adapter on the accepted
  connection, counted *above* the wire so a policer's drops stay visible), and the
  verdict moved to a delivery ratio: what this end received against what the peer
  handed over, with a 64 KiB volume floor and a 60 % threshold. On the policed
  path the number came back **100.0 %** — with the tunnel's `handed` at
  9 289 105 bytes per window, i.e. exactly the policer's own ceiling. The reason
  is the protocol doing its job: KCP *is* reliable, so a datagram the policer
  drops is re-sent until it arrives, and the loss surfaces as **throughput**, not
  as a delivery gap. A ratio between an application's writes and the peer's reads
  cannot see a carrier that is 10x slower while losing nothing.

  That closes the search for a *threshold* on either local quantity: the re-send
  share fires on both classes (round 6) and the delivery ratio fires on neither
  (this round). **What is left is a comparison, not a threshold** — and it is the
  shape the L3 lane policy already uses: the only way to learn what the other
  carrier would do is to *put a tunnel on it*. The handover mechanism does that
  non-disruptively (a tunnel is drained, its replacement dialed on the other
  carrier, the tunnel count unchanged), so the L4 rule's next form is a **probe
  and compare**: keep at least one TCP tunnel beside the KCP ones, measure each
  carrier's carried bytes per second per tunnel on this end, and move a tunnel
  when the other carrier's tunnels are carrying materially more — the measured
  separation for that comparison is the 10-13x the policed cells show. It needs
  no new wire field: the client already counts what it received per tunnel, and
  the peer's report already carries what was handed.

- **The ARQ share is not a discriminator — measured, and it misfires (2026-10-13,
  round 6 of the switching goal).** With the peer's report in place the rule now
  fires: on the UDP-policed condition it handed a tunnel over exactly once. It
  **also fired once on the healthy control** (`loss1_buf`, the class where the L4
  S1 measured KCP delivering *less* inner loss than TCP on every ladder cell) —
  so as it stands the rule would move traffic from the better carrier to the
  worse one. The per-window distribution says why, and it is not a threshold
  problem: the share is

  | condition | busiest tunnel, share per window | windows ≥ 4 % |
  |---|---|---|
  | `udp_throttled` (move is worth 10–13x) | min 14.0, median 28.7, max 29.9 | 7/7 |
  | `loss1_buf` (KCP is the better carrier) | min 1.5, median 3.5, max 6.8 | 3/8 (runs of 4 and 3) |

  The two overlap around the floor, and no constant fixes that: **re-sending is
  what an ARQ does on any lossy path**, and a lossy path is exactly where this
  carrier is the right choice. The metric is wrong, not its calibration — the
  same lesson the L3 evidence chain learned when it stopped reading a sender's
  counters and started reading what was *offered* against what was *refused*.

  **What the rule needs instead, and it is one field away:** a **delivery
  ratio**. The client already counts what it *received* per tunnel
  (`TunnelCounters::delivered`), and the peer's report is the natural place to
  carry what the server *offered* that tunnel — a per-tunnel counter on the
  server's stream path, keyed by the same prologue id the report already uses.
  Then the verdict is "this tunnel delivered a fraction of what was handed to
  it" (measured: ~10–20 % policed against ~95–100 % on the healthy class), which
  is a property of the path rather than of the protocol's own retransmission
  behaviour. Until that lands the rule stays off by default, and its doc comment
  now records that its current signal fires on both classes.

- **The peer-side tunnel report landed — the instrument works, the rule still
  does not fire (2026-10-13, round 5 of the switching goal).** The L4 half's
  missing instrument is built and verified end to end: a new control command
  (`ControlChannelCmd::TunnelHealth(service, tunnel, emitted, re-sent, seq)`,
  fixed-width like the lane's `CarrierReport`) carries **the server's** session
  counters for one tunnel back to the end that dialed it. The identity needs no
  wire change: a tunnel's stream prologue already names the pair
  `(ServiceId, TunnelId)`, the server records it in a slot the mux driver and the
  stream dispatch share, and its reporter samples the tunnel's own
  `SessionHealth` every 50 ms and sends one frame per change on the session's
  control writer. The client routes a report to the pool that serves that
  service and records it on the tunnel's counters; `health_counts()` is the
  **sum of both ends**, which is what makes the share direction-independent, and
  the pool-stats line now prints that sum — the number the policy acts on.

  Verified by running it: the server logs one report per tunnel
  (`emitted=`/`resent=` growing), the client records it
  (`summed=Some((76162, 18469))` on the policed condition — a **24 % share**
  against the 0.04 % its own half reads), and the rule's window log shows the
  peer's traffic arriving (`window=[(17, 12202, 455)]`).

  **And the rule still does not fire on `udp_throttled`** — with the evidence in
  place the blocker moved to the threshold's interaction with the window, and
  that is measured rather than guessed: the tunnel the rule judges reads
  **3.7 %**, just under the 4 % floor calibrated on the S1's *saturated* cells
  (5.7 %); a 2 % floor (the midpoint of the three measured points: healthy
  0.6 %, policed 3.7 %, saturated 5.7 %) was tried and *still* produced no
  firing in a full acceptance run, because the per-window share dips below it
  often enough to reset the confirm streak. That experiment is reverted — an
  unproven threshold is not worth carrying — and the next attempt needs the
  per-window **distribution** (how often, and by how much, a policed path's share
  dips) rather than a third guess at a constant. The floor's doc comment records
  this in the code.

- **The regime the L4 rule exists for, now measurable — and the rule is blind
  in it (2026-10-13, round 4 of the switching goal).** The wiring run left the
  rule off by default because its signal looked rate-dependent on a 1 %-loss
  path. That path is not the rule's case: a carrier riding UDP is *policed* by
  middleboxes, not merely lossy. The model gained the condition that says so —
  `udp_throttled`, an `iptables -m limit` accept-then-drop for UDP on the
  tunnel leg (10 000 packets/s, the module's own ceiling, ≈ 117 Mbit/s of
  1472-byte datagrams, so a carrier on UDP is starved while one that is not
  rides free). Two carrier-side runs through it (`l4`, `udp-ladder`,
  2 rounds + warm-up, `--aa`, same binary; KCP side with
  `MOLEHILL_TUNNEL_CARRIER_RULE=1` and `MOLEHILL_POOL_STATS=1`, TCP side with
  `MOLEHILL_DATA_CARRIER=tcp`) read:

  | `udp-ladder` | KCP under the policer | TCP under the policer |
  |---|---|---|
  | 200M | 100.8 Mbit/s, **43.87 %** loss | 200.0, **0.000 %** |
  | 1000M | 78.5, **86.39 %** | 998.9, 0.097 % |
  | 2000M | 78.2, **89.99 %** | 1047.1, 47.34 % |
  | 5000M | 77.7, **92.76 %** | 1046.0, 71.56 % |

  So the move this rule exists to make is worth **10–13× the delivered rate and
  40–90 points of inner loss** in the policed regime — the opposite of the
  1 %-loss cells, where the same carrier comparison favours KCP on every cell
  and the rule correctly stays quiet.

  **And it cannot fire there.** With the rule armed for the whole run, not one
  `carrier rule` line appears, while the pool's own `kcp_health` samples read per
  tunnel `11139/7`, `17763/6`, `4/0`, `4/0` — thousands of datagrams and a
  *handful* of retransmits, a 0.06 % share against a 4 % floor. The first two
  explanations were checked and both are wrong:

  - **Not a calibration problem.** The 512-datagram window floor is indeed
    ≈ 10 240 datagrams/s per tunnel, above what the policed path carries — but
    the counters themselves say the sessions were *not* struggling: they sent
    thousands of datagrams and re-sent six or seven.
  - **Not death and repair either.** The "a broken carrier's sessions keep
    dying" reading is falsified by the run's own log: **zero repair lines** in
    all three rounds. Nothing died, because a KCP session's dead-link rule needs
    *unacknowledged data of its own*, and this end had almost none in flight.

  **The real cause is direction, and it is structural.** The ladder is
  downstream through the tunnel (visitor → server → tunnel → client → sink), so
  the traffic that the policer destroys is the **server's** sends. `kcp_health`
  is a *dialing end's* surface — the server has no pool at all, and no
  `pool-stats` line to carry one — so the rule's window only ever saw the
  client's own upstream sends, which the policer barely touched. The deciding
  end cannot see the loss because the loss is on the peer's sending side; the
  same asymmetry the L3 evidence chain solved by having the *server* report what
  it offered each lane and what that lane refused (`CarrierReport`,
  `src/protocol.rs`). The L4 half has no such report, which makes the instrument
  the earlier record called "the recorded next instrument" — a **peer-side
  tunnel report** — *necessary* rather than optional, now measured rather than
  argued.

  What that costs and needs, in order: a shared tunnel identity (the client's
  index carried in the tunnel hello, the way a lane carries its id), a control
  command carrying the peer's per-tunnel `datagrams_out`/`retransmits`, and the
  rule reading the peer's numbers beside its own. Then this pair of runs is the
  acceptance case: with the report in place, the rule fires on `udp_throttled`
  and the handover buys the 10–13× the table above measures. Until then the rule
  stays off by default, and the docs already say why — though the reason they
  give (rate-dependent floors) is not the measured one and wants correcting with
  this finding.

- **S2: the rule is wired, and its own wiring run measured the sender-side
  signal as rate-dependent — the rule ships off by default (2026-10-12,
  round 38).** The plumbing landed as designed: `SessionHealth` (per
  session's `datagrams_out`/`retransmits`, bumped by the engine's
  retransmit sites and the staging writer), travelling with the tunnel into
  `TunnelCounters`, surfaced in the pool snapshot (`kcp_health`) and on the
  pool-stats line; the pure verdict (window ≥ 512 datagrams, share ≥ 4 %,
  three consecutive 50 ms ticks) acting through `begin_handover` and the
  endpoint's `prefer` under the switch cooldown; the switch read per pool
  at creation. The A/B (`l4`/`churn-16`+`udp-ladder`/`loss1_buf` tunnel
  leg, 3+1 rounds, `--aa`, `molehill-{prerule,rule}.bin`) then read **no
  claim anywhere — and zero rule firings in any log**, which was the run's
  real finding: probed on the paced cell at the same 1 % path loss, the
  sessions' actual retransmit share is **0.6 %** — under the 4 % floor the
  S1 saturated cells (5.7 %) had calibrated. The mechanism is the same
  asymmetry the L3 evidence chain was built around, now measured on the L4
  half: most of the delivered-loss difference the S1 saw is the **peer's
  queue refusing segments**, which a sender's ARQ counters cannot see; a
  sender's share rises only when the sender itself is saturating into the
  path. So the floor calibrated on the saturated cells cannot fire on the
  cells where traffic actually flows, and lowering it without a
  measurement behind the new number would be guessing.

  What shipped: the rule, its floors, the health plumbing, the snapshot
  surface and the kill switch — **`MOLEHILL_TUNNEL_CARRIER_RULE=1` arms
  it** (per pool at creation, uncached after the round-36 hatch lesson);
  the docs state what the signal reads and why the default is off. What
  this leaves open, in order: the peer-side tunnel report (the lane
  `CarrierReport`'s L4 analogue — a control command carrying what each
  tunnel's *receiver* accepted/refused) is the instrument that would make
  the sender's signal honest; and re-calibrating the sender-share floor on
  a rate-swept grid before any default flip. The churn/ladder A/B's other
  readings are noise-internal (no claim on any cell), consistent with a
  rule that never acted.

- **One instrument, and the boundary is a profile (closed 2026-10-10).**
  The earlier decision — a sibling instrument for L3, the sweep left alone —
  was a consequence of the model not existing. There is one runner now: the
  sweep is the `sweep` profile, the staged schedule is the `timeline`
  scenario, the drift axis is the `soak` profile, the screen is `--profile
  screen --ab-arm`, and a peer is an arm. What stays distinct is *evidence*:
  the model writes results outside the tree, and only a run the ritual
  publishes lands in `benches/records/`.
- The benchmark comparison against v0.10.0 must build that state (worktree at
  the tag): there is no released asset that speaks v5, and no two tags are
  compatible, so each side runs as a complete pair.

### The derived data plane: what landed, and what is next

The plan's L12–L18 are in, as four commits:

- the schema change: `mode` and `members` are gone, `uses_pool` derives the
  shape from the service type, `[transparent.data.tcp|kcp].tunnels` is the
  lane budget with an equal-share derivation and a refusal below the claim
  count, and the bench model moved with it (`l3-lanes4`, `l3-kcp4`; `l4-mux1`
  dropped — its one-tunnel pool cannot start against the topology's UDP floor);
- the placement table: a claim's flows are placed once and remembered, so a
  lane change cannot reorder a flow, a new flow never lands in an empty slot,
  an entry expires after 60 s of silence in either direction, and
  `MOLEHILL_L3_STATS` reports each lane's `flows`;
- the valve (L17): `[server.data].max_tunnels_per_client` now counts a
  transparent claim's lanes, not just multiplexed tunnels, and the harness case
  that proves it (cap `1` against a two-lane claim) is in `run.sh`;
- the allocator (L15): `src/transparent/lanes.rs` decides, `LanePool` in
  `src/core/client.rs` reads the hubs and dials, a lane is asked to end only
  when it holds no flow at all, and the receiver's lane is dialled only after
  the donor's has ended. The target is **a lane per talking flow**, and the
  first measurement of it is the harness's lending case: two claims sharing a
  budget of four, one carrying the 16-flow arm, the other nothing —
  `Lane allocator: lent lane 0 of claim 10.99.0.1:8444 to claim 10.99.0.1:8443
  on the Tcp carrier`, with the busy claim byte-exact through the change.

**Lending, measured (2026-10-11).** Four A/B runs through the model, each on
`bulk-n` (eight flows on one claim), ten rounds, with the run's own A/A twin
beside it. The final three are the ones to quote: they use the **same binary**
and two catalog arms that differ only in the knob
(`l3-budget10-lending` against `l3-budget10-nolend`, i.e.
`[transparent.data].lend` on and off), so the only thing between them is the
decision under test.

| condition (leg) | lending | no lending | A/A floor | verdict |
|---|---|---|---|---|
| clean loopback | 5.107 Gbit/s | 5.086 | 2.09 % | indistinguishable (+0.4 %) |
| **100 ms, carrier leg** | **2.395** | **1.754** | 11.39 % | **CLAIM: +26.75 %** (materiality 10 %, sign agreement 1.0, n=[10, 10]) |
| 1 % loss, carrier leg | 5.105 | 4.509 | 12.04 % | directional +11.69 % (sign agreement 0.8 — one round of ten disagreed) |

An earlier, cruder run of the same shape — a scratch build with the allocator
stubbed out as the second *binary* — put the long-haul effect at +39.95 %
(2.400 against 1.715). Same direction, larger point estimate, and it is the run
that found the retire path's cost (below); the arm-based numbers supersede it
because anyone can reproduce them from the catalog.

What that says, in order of how much it matters:

- **Lending's value is conditional on what limits the path.** On a clean
  loopback the L3 path is limited per *packet* (the TUN path's own cost, see the
  L3 attribution in `docs/benchmarks.md`), so more lanes buy nothing. With the
  carrier connections long-haul, a claim's throughput is limited per
  *connection* (window over RTT), and spreading its eight flows over the lanes
  the idle claims lent it is worth **+27 %** — a claim by the model's own rules.
  Under 1 % loss the same comparison is +12 % and directional: the effect
  survives loss, but ten rounds cannot separate it from the run's 12 % scatter.
- **It buys that with CPU and memory, not efficiency**: under the long-haul
  condition `cpu_s_per_gbit` 0.846 against 0.783 (+8 % per byte),
  `cpu_cores` +31.89 % and `rss_peak_mib` +11.27 % (both directional) — more
  connections carrying more traffic. A deployment on a fast, short path should
  expect neither the throughput nor the cost, and can turn the mechanism off
  with `[transparent.data].lend = false`.
- **The condition's leg decides whether the experiment can see anything.** The
  same A/B with `--condition rtt100 --condition-leg visitor` (the default) gave
  +2 % and nothing above the floor, because the *carrier* was never the
  bottleneck; four runs went into learning that.
- **The retire path cost 8 % before it was fixed.** The first A/B had the
  pre-lending binary ahead (4.857 against 5.072 in one run, consistently across
  four), and the cause was a lock the packet path took per relayed packet
  (`8bee843`): mirroring the draining mark in an atomic and building the retire
  wait once per lane instead of once per packet recovered more than the whole
  regression. Without that measurement the cost would have shipped as "lending
  made it slightly slower".
- **The model could not state a config-axis claim until this run.** Two catalog
  arms were never compared: `_pairs_of` paired each arm with the control and
  each `~b`/`~aa` twin with its base, so a decision that is a *field* rather
  than a second binary had no verdict. An arm may now declare `against = "<arm
  id>"` and the analysis pairs them — which is how the table above is a claim
  and not two columns a reader has to subtract.

Still true, and the reason the mechanism is designed the way it is: **a single
flow cannot be spread over two lanes** — its packets must stay in order, which
is what pins it to one — so a one-flow workload gains nothing from lending by
construction, and the target is one lane per talking flow.

### KCP, second thread: the inner-UDP loss item, measured (2026-10-11)

The recorded item was "inner-UDP loss 1.05 % vs 0.60 %" on the KCP carrier. It is
now measured the way §10 requires — the metric's own floor **and** the run's own
A/A — with the contrast the new condition gives: the same arm, the same path, the
carrier chosen by the product (KCP) or forced for isolation (TCP).

```
sudo -n uv run benches/scripts/bench/bench.py run --profile smoke --arms l3,l4 \
  --scenarios udp-pace,udp-ladder --condition loss1 --condition-leg tunnel \
  --rounds 3 --warmup-rounds 1 --aa --out /home/dsh/tmp/k4-udp-kcp.json
sudo -n env MOLEHILL_DATA_CARRIER=tcp uv run benches/scripts/bench/bench.py run \
  --profile smoke --arms l3,l4 --scenarios udp-pace,udp-ladder --condition loss1 \
  --condition-leg tunnel --rounds 3 --warmup-rounds 1 --aa --out /home/dsh/tmp/k4-udp-tcp.json
uv run benches/scripts/bench/bench.py compare /home/dsh/tmp/k4-udp-kcp.json \
  /home/dsh/tmp/k4-udp-tcp.json
```

Both runs are on `6b5ea61`, three rounds an arm, one warm-up, `--aa` on, the
`tunnel` leg shaped with 10 ms delay + 1 % loss. `A` is the KCP run, `B` the
TCP one.

**The recorded item is noise.** On the paced session (`udp-pace`, 2000
datagram/s, 1200 B) the inner-UDP loss is 0.821 % [0.775..0.975] on KCP against
0.920 % [0.736..0.980] on TCP — a 0.1-point difference, inside the same run's
A/A twin (KCP twin 1.075 %) and far below the metric's 1.0-point materiality
floor. It is not a claim in either direction, so the note that started this
thread was reading scatter.

**What is a claim, and is worth the optimisation work:**

1. **KCP's ARQ collapses where TCP's holds**, on the bulk ladder with the same
   1 %-loss tunnel leg: at 1000 Mbit/s offered, KCP delivers 384.6 Mbit/s with
   **59.5 %** inner-UDP loss against TCP's 986.6 Mbit/s with **0 %**; at
   2000 Mbit/s it is 386.0 Mbit/s / 79.0 % against 1972.9 Mbit/s / 0.002 %
   (`udp_recv_mbit` -61.0 % and -80.4 %, `udp_loss_pct` +59.5 and +79.0 points,
   every paired round agreeing, both floors cleared). Above roughly 150 Mbit/s
   the KCP carrier stops being a usable path for a UDP workload on a lossy link
   while the TCP fallback is unaffected — and note *both* carriers make inner
   UDP reliable, so this is KCP's reliability machinery losing, not a
   retransmission-policy difference.
2. **A paced UDP session costs 6.8× the CPU per delivered byte on KCP**:
   `cpu_s_per_gbit` 83.04 vs 12.19 (+581 %, floor 15 %), `syscalls_per_s` +49.6 %
   and `bytes_per_syscall` -30.6 % beside it, `rss_peak_mib` +19.1 %. The same
   run shows the *latency* shape KCP is chosen for (gap p99 57.6 ms against
   TCP's 248.8 ms), but it is not a claim: the TCP twin's own rounds disagreed
   (one round saw 521 ms), which is exactly what the A/A floor is for.

**A method artefact hid part of the number, and a deeper queue names the real
ceiling.** `loss1` pins no netem queue limit, so the class runs on netem's
default 1000-packet queue: on a 1 Gbit/s ladder burst that queue tail-drops
*beyond* the 1 % it emulates, and part of what looked like KCP's loss was the
shaper's. The new `loss1_deep` class is the same 1 % loss with a 100 000-packet
queue, and the ladder cell moves from 384.6 Mbit/s / 59.5 % loss to 545.0 /
42.5 % — while the TCP fallback on the *same* class delivers 988.4 Mbit/s with
**0.051 %** loss (A/A twin 985.8 / 0.010 %, three rounds, `--aa`). So the
carrier gap is real and large once the shaper stops adding its own drops: the
KCP carrier carries about **55 % of what the TCP fallback carries** on a
1 %-loss path, and the A/A twin shows the run can resolve differences of a few
percent, so this is not scatter.

**First optimisation attempt: the RTO floor. REVERTED — it did not clear the
floors.** The counters said `rto_ms=30` with `srtt_ms=16` (the nodelay floor
binding while the path's queueing delay exceeds it) and `resends_rto=71631` of
`retransmits=100021`, so raising the nodelay minimum RTO to 60 ms was the cheap
test. Built as a second binary and A/B'd in one run (`--ab-arm l3 --binary
…/molehill-base --binary-b …/molehill-rto60`, `loss1_deep`, tunnel leg, 3 rounds
+ 1 warm-up, `--aa`, 60 samples): `udp-ladder[1000M]` throughput 536.7 →
547.5 Mbit/s (+2.0 %) and inner loss 42.04 → 42.98 %, with the run's own A/A twin
at 539.1 Mbit/s — inside the run's noise, and **no claim cleared the floors**.
The change is reverted (the product source is byte-identical to the baseline,
verified by binary hash) and the engine keeps upstream's floor. What that rules
out is the floor itself: the retransmissions are not caused by an
under-estimated RTO, so the 21 % re-send rate is either affordable or comes from
somewhere else in the send path.

**The diagnosis first, and it named the ceiling: the send window.** Running the
ladder under `MOLEHILL_KCP_STATS=1` and reading the deltas at the busiest second
(the 1 Gbit/s cell) gives, on the **sender**: `datagrams_out +54791`, of which
`retransmits +3135` (5.7 %, not the 21 % the shallow-queue class showed),
`wait_snd_peak=4107` — the send queue sitting at its limit (`2 × snd_wnd`) while
the app keeps writing — and per-phase times of `ms_output +122`, `ms_writer
+24.7`, `ms_input +17.5`, `ms_update +15.1` per second, i.e. **~18 % of a core**,
with the pacer at 12 Gbit/s. The receiver's second is `datagrams_in +21373` with
`acks_out +21373` (one ack per datagram) at ~8 % of a core. So neither end is
busy, nothing is spilling, no window is *waiting* on the receive side — and the
sender's queue is full: the ceiling is in-flight data, not CPU.

The arithmetic then fits the measurement: 2048 segments × 1472 B ≈ 2.8 MiB over
the ~44 ms round trip a lossy path with a large buffer settles at is **~533
Mbit/s**, and the ladder cell measured 533-556 Mbit/s across three different
queue classes. That is the window, exactly.

A third condition followed from the same reasoning: `loss1_deep`'s 100 000-packet
queue (≈147 MB) is not a network anyone has — it is deep enough to add ~1 s of
queueing at these rates — so `loss1_buf` was added (10 000 packets ≈ 14.7 MB,
≈27 ms of standing queue at 545 Mbit/s): deep enough that tail drops are not the
story, shallow enough that the RTT stays a path property. The baseline measures
556.5 Mbit/s / 42.3 % there, i.e. the same ceiling.

**Third optimisation attempt: send window 2048 → 8192 segments. KEPT — it
clears both floors, and the checks are clean.** Two-binary A/B, `--aa`, three
rounds each:

| cell (`loss1_buf`, tunnel leg) | baseline (2048) | candidate (8192) |
|---|---|---|
| `udp-ladder[1000M]` | 556.5 Mbit/s, **42.31 %** loss | **0.16 / 0.37 / 0.66 %** loss per round, +43.2 % delivered |
| `udp-ladder[2000M]` | 533.5 Mbit/s, 70.87 % | +49.5 % delivered, −27.0 points loss |
| `udp-ladder[5000M]` | — | −16.3 points loss |

Every paired round agreed in sign and both floors cleared (`udp_loss_pct`
+41.9 points against a 1-point floor; `udp_recv_mbit` +43.2 % against 10 %). The
cost is real and stated: `cpu_cores` +47.5 % and `cpu_s_per_gbit` +46.6 % on that
cell (it carries 43 % more), `rss_peak_mib` +15 %; at 2000M the extra cost is
−36 % cores. The two regression checks the old note demanded: on a clean path
with one lane (`l3`, `bulk-1`/`bulk-n`/`rr-16`/`udp-pace`) the only claim in
twenty-four comparisons is in the candidate's favour (`setup_p50_ms` −26 %), and
on the four-tunnel burst case (`l4`, clean, `bulk-1`/`bulk-n`) there is **no
claim at all** (bulk-n 9.704 against 9.789 Gbit/s) — so the 2048-era burst
collapse does not reproduce at 8192 with the current socket buffers and pacer.

**Where the inner-UDP item stands after the window fix (2026-10-11).** The same
`loss1_buf` class, three rounds an arm with `--aa`, KCP against the forced TCP
fallback, `l3` and `l4` in one run each:

| `udp-ladder` cell | KCP (`l3`) | TCP (`l3`) | KCP (`l4`, four tunnels) | TCP (`l4`) |
|---|---|---|---|---|
| 200M | 197.7 Mbit/s, 0.000 % loss | 194.8, 1.465 % | 195.1, 1.163 % | 193.8, 1.774 % |
| **1000M** | **985.7, 0.120 %** | **986.6, 0.009 %** | 840.9, 14.486 % | 837.0, 14.323 % |
| 2000M | 1033.7, 43.99 % | **1973.6, 0.000 %** | 870.2, 53.7 % | 874.1, 54.2 % |

Three things this settles:

1. **The recorded item is closed at the rate that matters.** At 1 Gbit/s the KCP
   carrier now delivers 985.7 Mbit/s with 0.120 % inner-UDP loss against TCP's
   986.6 / 0.009 % — the 42 % the earlier rounds measured was the 533 Mbit/s
   send-window ceiling starving the path, not loss recovery failing, and the
   fix is the 8192-segment window that shipped.
2. **Above 1 Gbit/s the carrier still caps at ~1.05 Gbit/s** (2000M cell) while
   TCP reaches 1.97 — that is the receive-window ceiling the fourth attempt
   chose not to buy, and this run is the independent confirmation of that
   choice: the ceiling is real, deliberate, and stated.
3. **The remaining KCP cost is CPU per delivered byte**: 1.661 s/Gbit against
   TCP's 0.586 at the same 1 Gbit/s cell — **2.8×** — with the four-tunnel arm
   identical on both carriers (2.29 against 2.05) because the multiplexer, not
   the carrier, is the ceiling there. This is the same family as the paced-UDP
   figure from the first thread (6.8× per delivered byte at 2.4 Mbit/s), and it
   is the open item the next round starts from: the carrier's per-packet work,
   not its window or its recovery.

**Fourth attempt: receive window 4096 → 8192. REVERTED — it buys throughput past
the point where the carrier's efficiency holds.** Same method (two binaries, one
run, `--aa`, 3 rounds, `loss1_buf`):

| cell | shipped (snd 8192 / rcv 4096) | candidate (8192 / 8192) |
|---|---|---|
| `udp-ladder[1000M]` | 981.6 Mbit/s, **0.171 %** loss, 1.610 s/Gbit | 986.9 Mbit/s, 0.000 %, **2.071 s/Gbit (+28.6 %)** |
| `udp-ladder[2000M]` | 1046.7 Mbit/s, 43.3 % | **1746.2 Mbit/s, 11.1 %** |
| `udp-ladder[5000M]` | 1044.6 Mbit/s, 66.8 % | 1606.0 Mbit/s, 51.1 % |
| `udp-pace[1000/s]` | 0.971 % loss | 0.869 % (inside the A/A band), plus `cpu_cores` and `cpu_s_per_gbit` worse by ~23 % |

The claims are on both sides with sign agreement 1.0: the candidate delivers
+40 % and loses 32 points less on the 2000 Mbit/s cell, while the shipped build
is better on `cpu_cores` (−39 %), `cpu_s_per_gbit` (−35 %), `syscalls_per_s`
(−19 %), `rss_peak_mib` (−29 %) and `wire_per_visitor_byte` (−38 %). The
distinction that decides it is the one the round-3 change was kept for: at
1 Gbit/s — the rate the carrier has to serve — the shipped build already
delivers 981.6 Mbit/s with 0.171 % loss, and doubling the receive window buys
**0.5 % throughput for +28.6 % CPU per delivered byte**. What the candidate
really does is move the ceiling from ~1.05 to ~1.75 Gbit/s, and it pays ~2× CPU
per bit above 1 Gbit/s to do it; on a proxy whose typical host is a small box,
that is not a change to make by default. It is `HANDOFF`-recorded rather than
shipped, and a *capped* middle (rcv 6144) is the obvious next variant if a
high-rate lossy path ever becomes the target.

The receive window stays 4096, so the peer's advertised window is still what
caps the sender at ~1.1 Gbit/s — the deliberate end state, not an oversight.

**Second optimisation attempt: congestion control on. REVERTED — it is far
worse.** Built as a second binary (`set_nodelay(..., nc=false)` against the
baseline's `nc=true`) and A/B'd in one run on `loss1_deep` with four scenarios
(`udp-ladder,udp-pace,bulk-1,bulk-n`, 3 rounds + 1 warm-up, `--aa`, 84 samples):

| cell | baseline (`nc=1`) | candidate (`nc=0`) |
|---|---|---|
| `udp-ladder[1000M]` | 531.9 Mbit/s, 41.8 % inner loss | **2.7 Mbit/s, 97.3 %** (two of three rounds failed) |
| `udp-ladder[2000M]` | 534.5 Mbit/s, 71.1 % | **all three rounds failed** |
| `bulk-n[P8]` | — | `cpu_cores` +1639 %, `syscalls_per_s` +15130 %, `rss_peak_mib` +69 % |

The engine's `nc=0` path is the reference's: `cwnd` grows by slow start, halves
on a fast retransmit and restarts at **one segment** on a timeout. On this path
that restart never recovers — 15 000 % more syscalls is a pump spinning on a
one-segment window — so the candidate is not a slower carrier but a stalled one.
Reverted, and the product source is byte-identical to the committed baseline
again.

**What two reverted attempts establish:** neither the RTO floor nor the
congestion window is the knob that fixes the KCP bulk ceiling on a lossy path.
Both are now *measured* rather than assumed, and the numbers say the next step
is diagnosis rather than another policy guess: with `nc=1` the sender spends
21 % of its datagrams on re-sends and reaches ~55 % of the TCP fallback's
throughput, so the question is which phase of the send path is saturated —
`MOELEHILL_KCP_STATS=1`'s per-phase timings under the deep-queue class
(`ms_input` / `ms_deliver*` / `ms_writer` / `ms_output` / `ms_update`) on both
ends, read *while* the ladder runs at 1 Gbit/s, before any further change. A
bound on in-flight data tuned to the path's bandwidth-delay product is the
shape the evidence points at (the window is 2048 segments ≈ 2.8 MiB, and the
deep queue's own limit is 100 000 packets), but that is a hypothesis to test
with the same A/B, not a conclusion.

**Next hypothesis to test, not to assume:** the session is configured with
congestion control **off** (`set_nodelay(true, 10, 2, /*nc=*/true)`, and the
listener logs `nc=1`), so a sender with a 2048-segment window keeps blasting
into a lossy path and re-sends what the path drops — which is consistent with a
21 % re-send rate, a 55 %-of-TCP ceiling and an output phase that dominates the
sender's time. The engine implements `ssthresh`/`cwnd` when `nc=0`, so the next
A/B is congestion control **on**, measured on the same cells (throughput, inner
loss, `retransmits` per delivered byte) and reverted the same way if it does not
clear them.

**The window/pacing hypothesis is falsified, and the real one is in the
retransmission counters.** A ladder run with `MOLEHILL_KCP_STATS=1` on the same
condition (`54f...`/`6b5ea61`, one round, `--aa`) shows, on the **client**:

```
retransmits=0 resends_rto=0 resends_fast=0 wait_snd_peak=0 wait_rcv=0
advertised_wnd=4096 peer_wnd=4096 pacer_mbit=9000 spill_bytes=0
```

— no window wait, no queue spill, and a 9 Gbit/s pacer: nothing at the client
is the ceiling. The **server's** counters name it:

```
datagrams_out=483665 retransmits=100021 resends_rto=71631 resends_fast=28390
ms_output="1509.263" ms_input="269.649" ms_writer="253.221" srtt_ms=16 rto_ms=30
```

The sender retransmits **21 % of its datagrams on a path that loses 1 %** — a
20× spurious-retransmission rate — and the output phase is where its time goes
(1.5 s against 0.27 s for input and 0.25 s for the writer). So the bulk
collapse is not a window, a pacer or a queue: it is KCP's loss recovery
retransmitting far more than the path loses, which both wastes the wire and
consumes the send path that would have carried payload. The next change to try
is therefore in RTO/fast-resend policy (`rto_ms=30` against `srtt_ms=16`;
28 % of the resends are the fast path), measured on exactly these counters —
`retransmits` per delivered byte — rather than on throughput alone.

### Quality-driven carrier switching: the plan, and the first piece

The shipped fallback reacts to a carrier that **cannot be established**. The
measurements say that is not the only case worth reacting to: on the same 1 %-loss
class the two carriers differ by ~90 % of delivered throughput at 2 Gbit/s
(KCP 1033.7 Mbit/s with 44 % inner loss against TCP's 1973.6 with 0.000 %), while
at 1 Gbit/s they are level and on a clean path TCP is the cheaper one. So the
policy that follows from the data is "stay where you are unless the carrier in
use is measured to carry materially less", delivered by machinery that already
exists:

- **The decision** lives in `CarrierChoice` (per data endpoint, one per client):
  `prefer(carrier, why)` moves it deliberately, is refused while a
  `SWITCH_COOLDOWN` (120 s) from the previous switch is running, prints one
  `INFO` line per switch and then `DEBUG`, and holds the new carrier against the
  re-probe (a switch *to* TCP writes the same 60 s retry gate a fallback does).
  **Landed with tests**; the policy is the only thing still missing.
- **The move** is a lane drain, and the hub already implements it: `retire_lane`
  marks a slot draining, a draining slot takes **no new flows** (`live_slots`
  excludes it) and keeps serving the flows already placed on it, and when the
  slot vacates the *server's* replacement request dials a new channel into it —
  which asks the choice, and therefore rides the new carrier. So a switch never
  reorders: existing flows finish on the old carrier, new ones start on the new
  one, and a busy lane's flows are only moved when they end on their own.
- **The diagnostic hatch** (`MOLEHILL_CARRIER_SWITCH_AT=<seconds>`, landed) does
  exactly one switch that way, at a fixed time, so its *effect* can be measured
  before the policy that decides one is written: what the following lanes
  capture of the ~90 % the A/B says is available, and what the drain costs the
  flows that were on the retired lane.

**Measured: a switch captures the whole prize, and costs nothing where the
carriers are level.** `l3-budget10` (ten lanes over five claims — two each, so a
claim always has one lane left while the other is drained), `loss1_buf` on the
tunnel leg, `udp-ladder`, three rounds plus a warm-up with `--aa`, same binary,
the only difference being `MOLEHILL_CARRIER_SWITCH_AT=6` (one switch, six seconds
in, every claim of the endpoint):

| `udp-ladder` | carrier held (all KCP) | one switch at 6 s | change |
|---|---|---|---|
| 200M | 197.4 Mbit/s, 0.000 % loss | 197.4, 0.000 % | — |
| 1000M | 984.1, 0.140 % | 979.6, 0.126 % | level (inside the run's noise) |
| **2000M** | **1057.6, 40.73 %** | **1883.5, 4.49 %** | **+78 % delivered, −36 points of loss** |
| **5000M** | **1041.8, 61.99 %** | **3067.5, 7.87 %** | **+194 % delivered, −54 points of loss** |
| CPU per delivered byte (2000M) | 0.821 s/Gbit | 0.552 | −33 % (it delivers more per CPU-second) |

The A/A twin in the carrier-held run is tight on the divergence cell (1056.8 /
40.83 against 1057.6 / 40.73), so the baseline is not scatter; the switched run's
cells are tight too (1883.2..1883.7). The switched figure is 95 % of what forcing
TCP outright delivers on the same cell (1973.6), which is the right reading: one
of a claim's two lanes moved, and the claim now carries a KCP lane *and* a TCP
lane. Nothing on the cells where the carriers are level moved — so the policy has
room to be conservative and only act on evidence.

### FIXED: the black hole behind a retired lane (2026-10-11)

The bug below is fixed by a **CLOSE control frame** (`CTRL_CLOSE = 0x04`, the
same 16-byte vocabulary as PING/PONG/SACK): when both halves of a session's
stream are gone and its ARQ tail is flushed, the session sends three CLOSE frames
20 ms apart, and a session that receives one ends immediately. The regression
test that failed at the 30 s timeout now passes in 0.03 s, and the bench cell
that reproduced it end to end — `l3-budget10` over `udp-ladder,udp-pace` on
`loss1_buf`, which read **100 %** loss for whole rounds — reads 0.873 / 1.360 /
1.129 / 0.825 % across four rounds. Frames are sent only on the orderly close
path (never in answer to one), so two peers cannot ping-pong them, and an older
peer that does not know the kind already ignores unknown control frames (the
catch-all is `trace!`).

### The bug it fixed: lending on + ladder-then-paced loses the paced flow (2026-10-11)

The fix above made `l3-budget10` runnable for the first time, and its
`udp-pace` cell reads **100 % loss** in some rounds. Bisected as far as the
evidence goes:

- **Lending is implicated, not the switch.** The 100 % appears with the hatch
  unset (`switch-off2`, i.e. shipped behaviour) and with it set, so the switch is
  not the cause. `l3-budget10-nolend` with the same scenarios and rounds is
  **healthy** (1.122 / 0.773 / 0.870 %), and `l3-budget10` with lending on and the
  paced scenario run **alone** is healthy too (1.251 / 1.306 / 0.978 %).
- **The trigger is the ladder running first.** `l3-budget10` +
  `udp-ladder,udp-pace` is where it shows, intermittently: three reproduction
  attempts produced healthy rounds (1.075 %, then 0.976 / 1.081 / 1.179 %), so
  the failing case is timing-dependent rather than a fixed configuration.
- **What the allocator is doing at the time** (`MOLEHILL_L3_STATS=1` run, the
  client's own lines): the ladder's traffic makes claim `…:2405` the busy one, so
  the allocator lends to it from every other claim —
  `lent lane 1 of claim …:2401 to claim …:2405`, then `lane 0` of `2402`, `2403`
  and **`2404`** — and `2404` is exactly the claim the paced probe dials
  (`UDP_PORT`). A live flow cannot lose its lane (`plan` only names a donor with
  more than one lane and a lane with `flows == 0`, and the hub re-checks under
  its own lock), so the remaining candidate is what happens *after* the lend: a
  claim that ends up with no live lane has an empty `live_slots()`, the placement
  then falls back to the hash alone, and a flow placed in a memberless slot is
  dropped as `no_channel` for as long as it lives — which is 100 % loss for a
  single-flow probe.

**First theory, falsified.** The obvious reading of the allocator trace — a
replacement takes the *lowest* vacancy, so a lane lent away earlier gets the
replacement while the flows pinned to a dead slot are stranded forever — does not
hold: `Routes::vacate` removes the claim from the table entirely once no slot
holds a queue, and the claim's flow placements go with it, so a claim with no
lanes re-places its next packet fresh instead of pinning it. A hub test written
to demonstrate the stranding fails for exactly that reason (its assertion finds
`member_stats()` empty), and the change that would have made a replacement prefer
a pinned slot was therefore reverted rather than kept on a theory.

**Caught, with both ends' counters — the lane is a black hole after a lend.**
A run that reproduced it (`MOLEHILL_L3_STATS=1`, `l3-budget10` +
`udp-ladder,udp-pace`, `loss1_buf`, tunnel leg, 3 rounds + 1 warm-up) gives rounds
of 0.870 % then **100.000 / 100.000 / 100.000 %**, and the per-second lines say
exactly where the traffic stops:

- the allocator's own line at 11:02:51: `lent lane 0 of claim 10.99.0.1:2404 to
  claim 10.99.0.1:2405` — the paced probe's claim lends a lane away, as designed;
- **client**, claim `:2404`: `member=1 live=true` for the whole failure (so the
  claim is *not* dead), `flows=0 forwarded=0 no_channel=0` on both slots, and the
  aggregate line **stops growing entirely** (`forwarded=70`, `unclaimed=16`, then
  constant for minutes) — nothing arrives at the client's device at all;
- **server**, same claim: `member=1 live=true flows=1 forwarded=2370 … 2396` and
  climbing — the server is still handing the visitor's datagrams to a lane.

So the client retired one of the claim's two lanes, and the *server* keeps writing
into the channel that lane was: its flow entry is pinned to that lane's slot (the
rule that makes a lane's death invisible to flows), while the client's end of the
channel is gone. The client's remaining live lane receives nothing because the
server is not sending to it.

Why the server never notices is the part that makes this a defect rather than a
race: a KCP session whose *lane task* has ended is not necessarily a session the
peer sees fail. If the client drops the reader but the session's pump keeps
answering — which is exactly what a KCP session does when only its consumer goes
away — the server's writes are acknowledged, its keepalives are answered, and its
dead-link detector (`~20` unacknowledged RTOs) never fires. The result is a
channel that looks healthy from both ends and carries nothing.

**Reproduced deterministically, and the mechanism is now complete.**
`src/transport/kcp.rs`, `a_dropped_stream_ends_the_session_for_the_peer` (committed
`#[ignore]`d with its reason): a session pair on the loopback, a round trip both
ways to prove it works, then the consumer's stream is dropped while the peer keeps
writing. Three things follow from it, each verified rather than reasoned:

1. **The end is prompt on the side that ends.** A temporary probe in the pump's
   exit path (reverted) showed the dropped stream's session going
   `writer half gone` → `reader_closed=true` → `quiescent=true` → exit, inside the
   5 s `CLOSE_FLUSH_TIMEOUT` the code promises. The half-close default is not the
   problem either: dropping the whole stream closes both halves, as a retired lane
   does.
2. **The peer is never told.** KCP has no close handshake, so the other end learns
   only from its own unacknowledged traffic. The test's failure message measures
   exactly that: in 30 s the peer sent **7876 datagrams of which 7288 were
   retransmissions** — it knows perfectly well that nobody is listening — and it
   still holds the session open.
3. **Its dead-link rule cannot fire.** That rule wants 20 retransmits *and* a send
   window that has stood still for 5 s. A lane forwarding a visitor's traffic
   keeps taking new data, so its window is never still; with the nodelay RTO
   growing ×1.5 per retransmit, "20 retransmits" is minutes even when it could
   apply. For a claim whose lane was retired or died, the black hole is therefore
   **permanent** for as long as the visitor keeps sending — which is what the bench
   captured (the server's `forwarded` climbing for minutes while the client's
   counters did not move).

**The fix this points at — a close frame.** The adapter already carries control
frames it recognises by a magic byte (PING/PONG/SACK, `is_ctrl_frame`), and the
wire is the unreleased v5 line, whose commands may be extended in place. Adding a
CLOSE frame, sent when a session's stream ends, tells the peer to end its session
at once: its lane task sees the end, vacates its slot, and the flows pinned to it
are re-placed onto the lanes that remain. Tightening the dead-link rule is not a
substitute — a lane with traffic has no still window to trigger it — though the
rule is worth revisiting separately for the idle case.

**The fix to design (next round, with a test that fails first):****The fix to design (next round, with a test that fails first):** a lane's end
must be *visible to the peer*. When a transparent lane retires, drains or its task
exits, its carrier connection has to be shut down — not merely unread — so the
peer's lane task sees the end, vacates its slot, and its pinned flows are
re-placed onto the lanes that remain. The two halves to check are the client's
lane teardown (does ending a lane close its KCP session/TCP connection for the
peer?) and the server's own placement rule (a flow pinned to a lane that vanished
without a close is the same stranding shape this bug has already shown twice).

**Superseded: the earlier hypothesis below is wrong** — the claim does keep a live
lane through the failure, so "lending turns a claim into an all-or-nothing shape"
is not what happens. Kept for the record because it is the road that led to the
counters above.

**An earlier hypothesis, since falsified.** Lending turns a claim
into an all-or-nothing shape: with two lanes one death leaves one, but the
allocator moves a claim down to `target = 1` when it lends its idle lane, and a
single lane death then leaves that claim with **no** lane at all. Whether it
heals depends on the server asking for a replacement channel and on
`wants_another_lane` answering yes (live 0 < target 1) — and the paced probe's
single long-lived flow is the exact shape that would read the whole round as
100 % loss if that ask does not come or is refused. It also explains the bisect
precisely: with lending off the claim keeps two lanes, one death leaves one, and
nothing is total.

The next step is instrumentation rather than another theory: run
`l3-budget10` + `udp-ladder,udp-pace` with `MOLEHILL_L3_STATS=1` until a round
fails (it appeared in roughly half the rounds when it appeared at all), then read
**both ends'** per-slot lines for the paced claim — `live=false` on every slot
with `no_channel` growing is the all-or-nothing shape, and a client line
showing `wants_another_lane` refusing (or no replacement ever joining) names the
half that is broken. Until that is understood, **the carrier-switching policy
does not ship**: it moves lanes with the same machinery this bug lives in.

**Two things the measurement found on the way**, both worth fixing before the
policy is written:

1. **Every `pool_cap` arm had been unrunnable since the carrier tables were
   merged** (the K3 rename made the topology writer emit a second
   `[transparent.data]` header, a duplicate key the loader refuses). `l3-budget10`
   and the two lending arms failed all cells with "not ready"; the self-check had
   only *built* the configs, never parsed them. Both are fixed in this commit:
   one table per block, and `bench.py selfcheck` now runs every catalog arm's
   server and client config through `tomllib` (556 → 582 checks).
2. **`l3-budget10` on `udp-pace` measures 100 % loss**, in *both* runs and
   therefore independently of the switch: a single long-lived UDP flow over a
   ten-lane claim loses everything, while the same arm's `udp-ladder` cells are
   healthy. This could not be seen before because the arm's config never loaded.
   It is a real open item — either the lane path under one long-lived flow with
   lending on, or the model's paced workload — and it is the first thing to
   diagnose next round, before the policy is built on a claim shape that may not
   work.

**What the policy still needs, in order** (next rounds):

1. Per-lane evidence, which does not exist yet: the allocator's snapshot carries
   `flows` per lane but not `forwarded`/`no_channel`. The hub counts both per
   slot already, so this is plumbing plus the previous tick's values to make a
   delta (`forwarded/s` and the drop ratio, per lane, per carrier).
2. The rule itself: keep a split across the carriers the endpoint can use
   (at least one lane on each while a claim holds two), compare the carriers'
   delivered rates and drop ratios over a window, drain a lane of the loser and
   let its replacement ride the winner — one move per tick at most, never more
   often than the cooldown. With converged evidence, stop probing on every tick
   and re-probe on a long timer, or the clean path pays KCP's per-byte cost
   forever for a comparison it has already won.
3. The forwarding-pool half (L4): a *per-tunnel* handover, never a whole-pool
   swap, because the server's `max_tunnels_per_client` valve counts tunnels and
   an overlap would be refused.

### The policy did not trigger, and the read-only review says why (2026-10-11)

The round-3 hypothesis — "the policy reads the *send* side, and `udp-ladder` is
downstream traffic, so the client's per-slot counters stay at zero" — is
**partly right and partly wrong**, and both halves were established by reading
the code rather than by another run:

- **Right about the direction being unproven by the bench.** `run_udp_ladder`
  dials the tool's exposed port from the visitor namespace
  (`topology.VIS_NS`) towards the iperf3 sink the client namespace binds
  (`benches/scripts/bench/workloads.py`, `iperf_sink` + `run_udp_ladder`), and
  the command carries no `-R`. So the ladder is **visitor → client**, not the
  downstream direction the earlier note assumed — which does not exonerate the
  policy, because what it reads is not delivery either (below).
- **Wrong about what the counters mean.** `Routes::flush` counts `forwarded`
  when a batch is *accepted by the slot's channel* and `no_channel` when it is
  not — a local enqueue result, on the end that read the packet from its own
  device. It is not "delivered to the peer", and it is not a drop the carrier
  made. Two different causes share the counter: a full 128-message queue (the
  path's own backpressure) and a slot whose lane is gone. So `drop_pct()` in the
  round-3 policy is a mis-named quantity: it is "offered packets this slot did
  not enqueue", and for a claim whose lanes are alive and fast it is 0 even
  when the carrier two hops away is losing a third of what it was handed.

That is the real reason a policy built on it cannot fire on the divergence cell,
and it is not fixed by adding a receive counter alone. What the policy needs is
a **per-lane identity plus both ends' view**: which lane (not which slot) rode
which carrier, what this end offered it, what the *other* end received from it,
and whether that lane is empty on **both** ends before it is retired. The
round-3 change reads slots, has no lane identity, and takes the client's own
flow table as proof of emptiness — which a lane carrying only downstream
traffic never satisfies on the client.

Three more defects the same review found, all of them in the machinery the
policy would have shipped on:

1. **`CarrierChoice::prefer` does not hold what it promises.** Moving to TCP
   sets `retry_at_ms` to `KCP_RETRY_COOLDOWN` (60 s) while the switch's own
   permission lasts `SWITCH_COOLDOWN` (120 s), so between the two the endpoint
   re-probes KCP, and a successful session calls `reached_kcp()` — which stores
   KCP and clears the cooldown. A *quality* decision can therefore be undone by
   a *reachability* event that says only "KCP came up", before the 120 s the
   decision was supposed to be held for. Reachability and quality are two
   decisions about one endpoint and must not spend each other's permission.
2. **A lane's carrier is recorded on the slot, not on the lane.**
   `MemberSlot.carrier` is overwritten by whoever takes the slot next, and the
   round-3 policy compares "lanes whose carrier is `chosen`" — so after one
   switch, the *old-carrier* lanes it still needs to migrate are invisible to
   it, and it cannot complete a handover that is already half done.
3. **A draining lane is not known to the peer.** `Routes::retire` marks a slot
   draining on this end only; the server keeps writing into the channel because
   its own flow entry still names that lane. The client's retire path is what
   the black-hole fix (above) made visible, and it is the same shape: a handover
   has to be agreed by both ends before a lane is allowed to end.

**Confirmed by a run, not by reading (2026-10-11).** One round of
`l3-budget10` / `udp-ladder` / `loss1_buf` (tunnel leg, baseline `490198c`,
binary `1083c5849745eb1b`, `MOLEHILL_L3_STATS=1 MOLEHILL_KCP_STATS=1`) with the
same claim read on both ends:

| | client | server |
|---|---|---|
| aggregate `forwarded` (last stats line) | **61** | **1 050 617** |
| aggregate `no_channel` | **0** | **909 766** |
| busiest slot of the loaded claim (`:2405 member=1`) | ≈0 | 9 039 (first line) and climbing |

So the direction *is* the one the round-3 note guessed — the client's per-slot
counters are four orders of magnitude below the server's on the loaded claim —
and the mechanism is now sharper than "the policy read the wrong direction":

- the client's slots are **live** and its `no_channel` is **zero** the whole
  run, so nothing about the *client's* enqueue path is failing — there is simply
  nothing being offered to it, because the traffic is handed to the lanes at the
  **server** end (it reads the visitor's packets off its own device);
- the丢 is on the server, and it is counted there: `no_channel` climbs to 909 766
  beside 1 050 617 forwarded, i.e. **the server's lane queues are refusing
  ~46 %** of what the device offers them, which is the ladder's 44.5 % inner-UDP
  loss at 2000M seen from the other end of the same path;
- the round-3 policy reads `dropped` from `no_channel` **at the client**, where
  it is 0 — so `drop_pct()` is `None` for every lane, every tick, and the policy
  cannot fire. Not because it was unlucky, but because the counter it reads
  lives on the end that is not dropping anything.

That also names what a fix cannot be: **a receive counter at the client is not
enough**. This workload's loss happens where the packets are *offered* (the
server's hub), and the client has no evidence of it at all — it only ever sees
what arrived. The two ends therefore have to report to each other: the offerer
counts what it offered and what its lane queues refused, the receiver counts
what it actually injected, and the policy compares carriers on the *pair*.

**What this supersedes.** "Add per-slot receive counters and compare delivery
rates" is not the next step: it measures the wrong thing (enqueue, not
delivery), has no lane identity to compare *across* carriers, and would be
driven by a preference switch that the reachability path can silently reverse.
The work is a per-lane identity, both ends' counters for it, and a two-sided
drain — in that order.

### The evidence chain, landed (2026-10-11)

What that ordered list produced, in the order it landed:

- **Lane identity** (`a3d0067`). `protocol::LaneId`, four fixed bytes, minted
  by the client and named in a transparent lane's prologue (8 bytes: service,
  then lane). The server learns the *width* from the registered service before
  it reads — the prologue is the first bytes on a direct channel, so nothing
  on it can say how long it is — and reads the lane half with `read_lane_half`.
  The hub records `LaneKey` (id + generation) per slot, and names the lane in
  the `l3-stats` line.
- **Per-lane counters** (`9fa82a1`). A lane now carries `offered` (counted
  before the answer), `enqueued`, and the two refusals *apart*: `overflow`
  (a full queue — this end's backpressure) and `disconnected` (no lane held
  the slot — a replacement window). Plus `injected`: what the lane delivered
  into this end's kernel, the one "delivery" in the set and the half its peer
  cannot see. The legacy `forwarded`/`no_channel` counters keep their meaning;
  they just no longer have to carry a cause they cannot express.
- **The archive.** A lane's counters are kept when it leaves, mirrored on the
  claim, and kept past the claim's own removal: a handover on a one-lane
  claim retires the only lane and empties the routing table in the same
  instant, so the evidence has to outlive the entry that held it.

Two defects in the *machinery* were fixed first, because a decision built on
top of them could not have been trusted whatever it decided
(`b3b2bba`, `2bad8b9`): a batched control frame was fed to the ARQ and
dropped — so a CLOSE arriving behind data was lost, which is the retired-lane
black hole reachable from an ordinary burst — and a 60 s reachability
re-probe could undo a 120 s quality decision, because `reached_kcp` stored
KCP and cleared the cooldown with it.

**Still missing, and the reason the policy cannot ship yet:** the counters are
local. The ladder's loss happens at the **server** (which offers the packets),
so the client — the only end that decides — still has no evidence of it. The
server has to report its per-lane counters over the existing independent
control channel, and a drain has to be agreed by both ends before a lane is
allowed to end (the client's own flow table cannot see a lane that is only
carrying traffic *towards* it). Both are the next slice.

### The report, and the pair it makes readable (2026-10-11)

The first of those two is landed (`0516d43`): the server attributes its
counters to the lane the client named (`5d79ab4`), and reports them
(`CarrierReport`, tag 5, fixed 17 bytes — service, lane, offered, refused,
sequence) once a second and only for lanes whose counters moved. The client
records each report **against the lane id**, on the hub that owns it, and
prints it beside its own view. Measured on the divergence cell, on the end
that decides:

| client slot | its own `injected` | `peer_offered` | `peer_refused` | refused share |
|---|---|---|---|---|
| `member=3` (lane 4) | 358 632 | 625 008 | 266 376 | **42.6 %** |
| `member=5` (lane 6) | 309 463 | 668 726 | 438 044 | **65.5 %** |

That is the evidence the policy needs, and the client's own counters — 1
forwarded, 0 `no_channel` — could never have produced it: on this workload
the deciding end's local view of those two lanes is empty by construction.
The pair also settles the mechanism the earlier round could only guess at:
the loss is the **peer's lane queues refusing**, not a missing lane
(`disconnected` is 0 throughout) and not the client's own scheduling.

**What is still missing before a policy can act:**

1. **A two-sided drain.** — **Landed (`51e92da`).** The dialer asks
   (`SessionCmd::DrainLane` / `CancelDrain`), the peer stops placing new flows
   in that lane and answers `ControlChannelCmd::LaneDrainAck` (tag 6, fixed 9
   bytes); its per-claim reporter sends "ready" once its own flows in the lane
   reach zero, and the dialer retires the lane only then. A drain nobody answers
   inside the budget is called off, so a lane is never left unusable.

   **The first run found the bug this design exists to prevent, from the other
   side.** The peer's ask set the *local* draining flag, which wakes its own
   lane task — so it tore the connection down before the dialer had its answer,
   and the dialer saw a dead lane instead of a reply: three "the peer answered
   ready but this end holds no such lane" in one run, with the lanes' `live`
   flipping to false between two stats lines. The peer's ask now sets a separate
   `peer_draining` mark that stops placement and nothing else.

   Measured with the diagnostic switch on the divergence cell: three lanes
   retired on the peer's confirmation, zero teardown races, zero unanswered
   drains, and the two lanes the peer still had flows in waited rather than
   being taken.

2. **The rule itself**, on the pair the report makes readable: a lane whose
   peer-side queue refuses a material share of what it is offered is a carrier
   that is not delivering, and the decision is which carrier to prefer for the
   next lane — never a running lane's flows, which must stay where they are.

   **Landed (`9c84a77`), with the arbitration it needed (`97755a2`).** The rule
   is `lanes::carrier_move` (pure, with the pre-registered constants), the
   reading and the action are `LanePool::carrier_step`, and a handover in flight
   now outranks a lend. It fires by itself on the divergence cell — "lane 4
   refused its share for 3 ticks; asked the peer to drain it so its replacement
   rides Tcp" — with no diagnostic hatch.

   **The first A/B of it is a `no claim`, and the reason is worth keeping.**
   Forced KCP (the policy disabled) against the policy live, three runs of
   `l3-budget10` / `udp-ladder` / `loss1_buf`, three rounds plus a warm-up with
   `--aa`, identical fingerprints: every difference was inside the floors. Two
   mechanisms explain it, and both are properties of the design rather than of
   the rule:

   - **An existing flow never moves.** A lane change cannot reorder a flow, so
     the flow that is being refused stays on its lane until it ends; only flows
     that *start* after the switch can ride the replacement. The ladder's cells
     are fresh flows, but the policy needs three ticks of evidence and the
     divergence cells (2000M/5000M) are the last ones — by the time it fires,
     the round is over.
   - **And until this round the drain could not complete at all**: three runs,
     three drains asked, three answered `pending`, none retired. Readiness was
     `flows == 0`, and the placement table remembers a flow for
     `FLOW_IDLE_EVICTION` (a minute) after its last packet, so a flow that
     stopped seconds ago held the lane for a minute — longer than the dialer's
     30 s budget, so every drain was called off. Readiness is now
     `active_flows == 0` (`DRAIN_QUIET`, 3 s) for a *peer's* drain only; the
     local retire and the lending path keep the stricter `flows == 0`.

   **Measured after the fix** (four rounds plus a warm-up, policy live), the
   whole chain in one run, to the millisecond:

   ```
   16:27:20.566773  Data carrier for 10.10.0.254:2400 switched from kcp to tcp
                    (the carrier in use kept refusing this claim's traffic at the peer)
   16:27:20.566801  Data carrier for claim 10.99.0.1:2405: lane 3 refused its share
                    for 3 ticks; asked the peer to drain it so its replacement rides Tcp
   16:27:23.567831  Lane 3 of claim 10.99.0.1:2405: the peer holds no flow in it;
                    retiring it (still carrying this end's flows)
   ```

   The 3.001 s between the ask and the retirement is exactly `DRAIN_QUIET`: the
   quiet window is what unblocked the handover, and the retirement waited for
   the peer's answer rather than taking the lane.

   **The second A/B is also a `no claim`, with a control that is now really
   inert, and it fails for a reason the workload cannot fix.** The first
   comparison's control was not a control at all: the forced-carrier arm still
   asked the peer to drain a lane once per round, because the rule ignored
   `MOLEHILL_DATA_CARRIER` (`prefer` refused the move, the drain went out
   anyway). That is fixed (`2250385`), and the comparison re-run on the `dev`
   profile — whose ladder has five cells of four seconds, so a switch early in a
   round has cells after it — is still a `no claim`: the only reading outside
   the floors is the policy's *cost* (`rss_peak_mib` +14.81 % on the 1000M cell,
   directional).

   The instrumented runs say why, and it is a property of the workload shape
   rather than of the rule. Per round: the policy fires, the carrier switches,
   the drain is asked — and the drain does **not** complete, with **no TCP lane
   ever carrying a packet**. A round is ~20 s; the rule needs three ticks of
   evidence (it fires ~16 s in), the peer then needs `DRAIN_QUIET` (3 s) of
   silence on that lane, and the round is over before the replacement can carry
   anything. The one completion measured so far (the four-round smoke run above)
   landed at the very end of its round, with no cell left to show a difference.

   So the benefit is **not demonstrated, and the current model cannot
   demonstrate it**: what it needs is a workload whose flows *start* after the
   switch, which means a round long enough to contain both the three-tick
   confirmation and several fresh flows afterwards (a 60 s+ cell, or a
   churn-shaped cell). Until that exists and is run, this policy is mechanism
   evidence only — and the honest options are the ones HANDOFF already commits
   to: price it in a release sweep once the model can, or revert it by
   discipline if it cannot.

   **What is still not demonstrated is the policy's *benefit*.** The mechanism
   is measured end to end; a `CLAIM` it is not. The A/B that can show one needs
   a workload whose flows *start* while the policy is already in force — a long
   campaign where round 1 switches and the later rounds' flows ride the
   replacement, or a churn-shaped cell — and that measurement has not been run.
   Until it is, this policy is shipped on mechanism evidence and stays on the
   list of things a release sweep must price.

   **The arbitration it needed is landed** (`97755a2`): `LaneLoad` carries
   `handover` and donor selection goes through `ClaimLoad::lendable()`, so a
   handover in flight is never spent on a lend — repair, then handover, then
   lending, in that order.

3. **L4's version** (a per-tunnel handover), which the report does not cover
   yet: a multiplexed tunnel's streams are not lanes, and the pool's repair
   dialer still pins the carrier a pool was established on. The design it needs,
   so the next round does not re-derive it:

   - **One logical pool, N physical tunnels, a carrier per tunnel.** `PoolEntry`
     gains its own `Carrier` and a `Serving`/`Draining`/`Dialing` state; the
     pool's single carrier becomes a summary the telemetry prints. Placement
     already reasons per tunnel (`order_candidates`, the per-tunnel ceiling, the
     round-robin tie-break), so it only has to skip a draining entry.
   - **The dialer takes a target.** `tunnel_dialer` builds one closure per
     carrier today; it becomes `dial(carrier)` and the repair tick calls it with
     the endpoint's preferred carrier, which is what lets a replacement ride the
     carrier the policy chose rather than the one the pool was born on.
   - **Draining a tunnel is not "streams == 0".** A pre-opened idle TCP channel
     and a UDP worker with no pinned peer can go at once; a tunnel whose streams
     are a visitor's live connections keeps them to the end (a stream cannot move
     without reordering it, exactly as a lane's flows cannot). `PinRegistry`
     already answers the UDP half (`pinned_on`), and `StreamLease` answers the
     rest.
   - **The count never grows.** The server's `max_tunnels_per_client` counts
     tunnels, so a replacement may only be dialed once the drained one has
     released its slot — `PoolEntry::_shutdown` dropping is that moment. A
     drain that cannot complete inside the budget is cancelled and the tunnel
     returns to service, the same rule the lane drain follows.
   - **The failure mode that motivates it, verified by reading the code and
     worth a regression test of its own**: the pool's carrier is part of its
     *identity* (`(session, carrier)`, "two pools wearing one name"), and
     `repair` calls the dialer closure that was built for that carrier at pool
     creation. So a pool established on KCP keeps repairing on KCP — and
     `CarrierChoice::on_kcp_failure` is only reached from the *initial*
     establishment (`establish_tunnels`) and from a direct L3 lane
     (`connect_direct_channel`), never from a failed tunnel repair. The
     consequence: **a path that carries KCP at startup and loses UDP later
     leaves the pool repairing into a dead carrier for the process's life**,
     serving on whatever tunnels survive, instead of following the fallback the
     client already knows how to make. The acceptance case for the slice is
     exactly that sequence: establish on KCP, block UDP, kill a tunnel, and the
     replacement must come up on TCP — with the pool's reported carrier
     following its tunnels rather than contradicting them.
   - **The edit list, with the anchors, so the slice is one pass and not a
     re-derivation** (verified against the tree at `41f152c`):
     1. `src/transport/multiplex.rs:429` — `Dialer` becomes
        `Arc<dyn Fn(Carrier) -> Pin<Box<dyn Future<Output = DialResult> + Send>> + Send + Sync>`:
        a dialer that is *told* which carrier to establish.
     2. `multiplex.rs:391` — `PoolEntry` gains `carrier: Carrier`, and
        `multiplex.rs:360` — `PoolSnapshot::tunnels` carries it per tunnel
        (`Vec<(usize, usize, usize, Carrier)>` or a small struct), because a pool
        whose tunnels ride two carriers must report that rather than the carrier
        it was born on.
     3. `multiplex.rs:655` — `with_dialer` gains
        `preferred: Arc<dyn Fn() -> Carrier + Send + Sync>` (the *decision*, not
        `CarrierChoice`: the pool must not learn about the carrier module), and
        `multiplex.rs:1241` — `grow_locked` dials `dial(self.inner.preferred())`
        and records what came back on the entry. That single line is the fix for
        the verified defect: a repair after a fallback comes up on TCP.
     4. `src/core/client.rs:1756` and `:1781` — the two `Arc::new(move || …)`
        closures must merge into **one** closure that takes `Carrier` and
        branches, reusing both bodies (build `kcp_dialer: Option<Dialer>` and
        `tcp_dialer: Dialer` first, then compose a dispatcher), and
        `client.rs:1709` — the establishment pass's `dials.spawn(async move {
        dial().await })` — must pass the carrier it is establishing, so the cold
        path keeps its current behaviour. **An attempt at steps 1–3 confirmed
        the rest of the list** (the pool compiles against a dialer that takes a
        carrier and a snapshot that carries it per tunnel, with every existing
        `with_dialer` caller unchanged through a delegating `with_preference`);
        it was reverted before step 4 because that merge, plus its call sites
        and tests, did not fit the session's remaining budget — deliberately, so
        the branch stayed green rather than half-migrated.
     5. The four test dialers in `multiplex.rs` (and any in `tests/pool_test.rs`)
        take the parameter and ignore it (`|_| …`), except the one that proves
        the slice: a dialer whose `preferred` flips to `Tcp` after the first
        failure, asserting the replacement is `Tcp` and the snapshot says so.
     6. **Placement skipping a draining entry is the *next* slice**, with the
        per-entry state (`Serving`/`Draining`/`Dialing`) — keep it out of this
        one so the diff stays about the carrier a repair rides.
   - **Evidence — settled, by inventorying what the client can actually see.**
     The local route was "decide from client-observable signals", and it is
     **not viable**, which the inventory shows rather than argues: the client's
     per-tunnel numbers are `streams`, `pending`, `pinned` and `is_alive` —
     *occupancy*, which is exactly what placement exists to equalise. There is no
     per-tunnel delivered-bytes, loss or RTT anywhere on this end (the bench
     model's `retrans_segs` is host-wide, read from `/proc/net/snmp`), so two
     tunnels can be compared only by how full they are, and a carrier judged on
     occupancy is the mistake this cycle already made once with `no_channel`.

     So L4's evidence has to be reported, like L3's: **extend `CarrierReport` to
     name a tunnel instead of a lane** (the report's shape already carries an
     identity, a pair of cumulative counters and a sequence, so the extension is
     a second identity kind rather than a new message). The server can attribute
     what it offered and what a tunnel's stream set refused, because a stream's
     prologue already names the tunnel it arrived on — the same fact the L3 path
     uses per lane. The wire line is unreleased v5, whose commands may be
     extended in place, so this costs no version bump.

     **The first slice's order, therefore**: the per-tunnel carrier and state in
     the pool (`PoolEntry`), the dialer that takes a target, and placement
     skipping a draining entry — all three testable without the report, with the
     UDP-blocked acceptance case above as their proof; then the report extension
     and the rule, in that order, so no rule is ever written against evidence
     that does not exist.

     **The report extension's prerequisite, corrected (round 24).** Round 23
     concluded that the tunnel id has to go into the hello frame — the tunnel id
     is a client-local counter (`next_tunnel_id`, `multiplex.rs:143`) and
     `Hello::DataChannelTunnelHello` carries none — and then found that the hello
     has no length prefix, so the field would have to go into all three variants
     (or the frame would have to learn a length). That is the expensive answer,
     and it is **not needed**: the tunnel id rides in the **stream prologue**,
     which is a separate path from the direct channel's and already exists.

     - The client writes a mux stream's prologue at `client.rs:1278`
       (`run_mux_data_channel`) and the server reads it at `server.rs:1829`
       (`route_tunnel_stream`) — neither touches the hello, the direct channels
       (`client.rs:1115` / `server.rs:822`, `:1948`) or `PACKET_LEN`.
     - The client knows which tunnel a stream belongs to (it dialed it, and
       `TunnelStream` already answers `tunnel_id()` for the pin accounting), so
       the stream can name it the same way a transparent lane names itself: four
       more fixed bytes after the service id, giving the 8-byte shape `LaneId`
       already uses.
     - The server then attributes what it offered, and what that tunnel's streams
       refused, with the identity it read — and `CarrierReport` carries it
       unchanged (an identity, a pair of cumulative counters, a sequence).

     So the second slice's first step is a prologue extension on the mux path
     alone, not a framing change — a far smaller diff, and one that cannot
     disturb the frame every connection is established with.

     **What L4's quality signal is — settled from the code, not by analogy
     (round 26).** L3's evidence is a *refusal* share: the hub's lane queue drops
     when it is full, and the peer can say how much it refused. **L4 has no such
     path.** A tunnel carries one yamux stream per visitor, and the server pairs
     the visitor with a stream through `copy_bidirectional_with_idle`
     (`server.rs:3191`) — a byte-copy loop with **backpressure, no drop**. A slow
     carrier therefore does not lose a tunnel's traffic; it makes the visitors on
     that tunnel slow, which is a *rate* symptom, not a loss percentage.

     So L4's evidence is **delivered bytes per tunnel per window** (optionally
     with write-blocked time), measured at both ends once the id travels with
     the stream: the server counts what it wrote into each tunnel, the client
     counts what it read out of each, and the pair gives the same "both ends
     report" property L3 has — without inventing a drop counter for a path that
     does not drop. `CarrierReport`'s shape still carries it (an identity, two
     cumulative counters, a sequence; the counters are bytes rather than
     packets/refusals, which the variant's own doc comment already frames as
     "what that lane's queue refused" — the L4 variant names bytes instead).

     **…and the both-ends part is not needed for the first cut (round 27).**
     Re-checking why L3 needed a report: there the *loss* happens on the peer's
     offering side, which the deciding end cannot see at all. **L4 has no such
     asymmetry.** A carrier that is slow shows up as a low per-tunnel rate, and
     backpressure makes both ends observe the same rate — so the client, which is
     the deciding end, can measure it locally: `TunnelStream::tunnel_id()` is
     already there at the point the stream is handed to the forwarder
     (`client.rs:1295`), and a counting adapter over that stream (bytes read and
     written, per tunnel) is a self-contained change with no wire and no server
     involvement.

     The confound — a low rate may be the visitor's application and not the
     carrier — is handled the way L3 handles it: compare **across carriers** on
     comparable tunnels, require a material and sustained gap, and never act on
     one window. So **the wire tunnel id is not on L4's critical path**: it stays
     landed (it is what will let the server attribute the same thing per tunnel
     later, and it cost one prologue extension), and the first cut is
     client-local: per-tunnel byte counters → the drain state in `PoolEntry` with
     placement skipping it → the rule.

     **And what the counter says on its first run (round 27).** `l4-mux2`,
     `bulk-1`, clean, `MOLEHILL_TUNNEL_STATS=1`, two lines one second apart:

     ```
     tunnel-stats: … 8:165/4 9:1157103422/0
     tunnel-stats: … 8:165/4 9:1614544702/0
     ```

     Tunnel 9 carried ~457 MB in that second (≈3.9 Gbit/s) — the whole flow.
     Tunnel 8 carried 165 bytes. Tunnels 0/2/3/6/7 carried **exactly one byte**
     each: the pre-opened spare channels. **One flow ⇒ one tunnel**, which is
     correct and is the same pinning a lane has (a flow cannot be split without
     reordering it), but it means **a cross-tunnel rate comparison has nothing to
     compare with on a single-flow workload** — the second tunnel is idle by
     construction, not because its carrier is worse.

     That is the L3 problem reappearing in the one place L4 cannot escape it:
     L3 got out of it by using the *offering* end's refusal counters (a loss
     signal that exists even for a single flow), and L4 has no loss signal — its
     degradation is a rate, and a rate needs a second subject. So the L4 design
     must pick one of:
     1. **comparable subjects**: require (or wait for) several concurrent flows
        spread across tunnels, then compare per-tunnel rates across carriers —
        the honest analogue of the lane rule, but it can only fire on multi-flow
        traffic; or
     2. **a before/after probe on one tunnel**: drain a tunnel to the other
        carrier and compare *its own* rate across the change — self-controlled,
        so it works with any number of flows, and it is what the handover already
        does.

     (2) looks like the one that fits how the pool actually behaves, and it is
     the shape to price next.

     **What the second slice is, then, and what it is not.** The *client* half of
     a handover is landed (`38e4dc2`: a repair rides the preferred carrier, and
     the pool reports what each tunnel actually rides). What is missing is the
     **trigger** — and a trigger needs the evidence above. So the second slice
     is: tunnel id in the hello → per-tunnel report → the drain state in
     `PoolEntry` with placement skipping it → the rule. Writing the drain state
     and the placement filter *first* would be a mechanism with nothing to
     decide it — the shape this cycle has already rejected twice.

     **The drain mechanism landed anyway, with the honest reason attached
     (round 29, `fe46bfb`).** The note above is half right and half wrong, and
     the round's first act was to find out which half by building it. What the
     note got wrong: the pool-level drain is not "a mechanism with nothing to
     decide it" in the sense the lane drain was, because its decision already
     exists — repair dials the preferred carrier, so the handover's *effect*
     is mechanical (old-carrier tunnels stop being replaced by themselves) and
     its *trigger* is the one thing left unwired. What it got right: no rule
     ships with this commit, and no speedup is claimed. What landed is exactly
     the acceptance shape the slice's own note pre-registered, without the
     trigger:

     - `PoolEntry.phase` (`Serving`/`Draining`) — a draining entry keeps its
       live streams (a stream cannot move without reordering it) and receives
       no new opens; `reserve` filters it out of the candidate order, and a
       pool left with one serving tunnel refuses a *new* handover rather than
       draining its last placement target.
     - One handover at a time, two bounded phases (`Draining` waits
       `SWITCH_COOLDOWN` for its streams to end on their own, then the entry
       returns to service; `Dialing` waits `COLD_GROW_WAIT` for the
       replacement, then releases the record so ordinary repair dials the live
       preference) — the lane drain's budget rule, at pool scale: a pool one
       tunnel short for ever is worse than the carrier the move was trying to
       leave. The replacement dial follows the handover's own target even
       before the endpoint's choice has moved (a diagnostic must not spend a
       quality decision's permission), and the record clears when a tunnel of
       the target carrier is in the pool.
     - The diagnostic hatch `MOLEHILL_TUNNEL_SWITCH_AT=<seconds>` fires one
       handover per pool, its `from` read from what the pool's tunnels
       actually ride (after a fallback that may not be the birth carrier), and
       the pool-stats line carries per-tunnel `phase`, a `handover` field and
       `-handover` in the size timeline. Pinned UDP peers block the retirement
       (D30), and a placement refusal inside the window is not reported as
       saturation.
     - Proven by `a_handover_excludes_new_streams_until_existing_lease_ends`:
       a held stream keeps its tunnel alive and un-dialed, new streams land on
       the serving tunnel only, dropping the lease lets the tick retire the
       drained tunnel and dial exactly one TCP replacement, and the pool ends
       at its configured count with a TCP member. Suites at this commit: 254
       lib (1 ignored), 11 pool, 23 integration, clippy ×2, all serial where
       serial is required.

     **What is still open, and what the next round starts from.** The trigger.
     Two honest options were recorded before this round and the mechanism
     keeps both reachable: (1) comparable subjects — require several
     concurrent flows spread across tunnels, compare per-tunnel rates across
     carriers; or (2) a before/after probe on one tunnel — the hatch above is
     the instrument for it, and the comparison is *the slot's* rate across the
     change, not the physical tunnel's (its id and its flow set both change),
     so the rule has to be written against pool-aggregate rates around the
     switch, never one tunnel's lifetime bytes. The human's standing caveat is
     on the record and applies to any rule built here: a policy's benefit is
     conditional on the path having degraded, so the A/B has to cover the
     degrade → switch → fresh-flows window, or it correctly reads "no claim"
     without saying anything about the rule.

     **The hatch's first real run found its own bugs, and the mechanism is now
     measured end to end (round 29, `596b15b`).** Three campaigns on
     `l4-mux8` / `bulk-n` / `loss1_buf` (tunnel leg, 3 rounds + warm-up,
     `--aa`, same binary on both sides, the only difference being
     `MOLEHILL_TUNNEL_SWITCH_AT=6`):

     - **The first run was a method defect, not a result.** The switch env
       never reached the daemons (launched without it) and nothing in the
       results file recorded that — the runner's comment claimed the record
       inherited `MOLEHILL_*` switches and nothing implemented it. Fixed
       alongside: the record now carries `molehill_switches` *beside* the
       method, not inside it — a declared treatment is what a between-file
       comparison contrasts (`--ab-arm`'s two binaries are the same shape),
       and putting it in the method would invalidate every stored method
       record. The L3-era switched pair had identical methods only because
       nothing recorded the env; that hole is closed.
     - **The first *switched* run churned**: the hatch's fired flag was set
       but never read, so the 50 ms tick re-requested a handover every tick —
       136–175 requests per measured round, 167 KCP repairs, zero TCP, all
       masked by the repaired line logging the pool's *birth* carrier. Two
       fixes in one commit: the flag is consumed by its own check (`swap`),
       and the repaired line names `wanted`, the carrier the replacement
       actually dialed.
     - **The corrected run, verified line by line**: six hatch requests per
       pool per round (one per pool — warm-up included), the tunnel marked
       `draining` in the timeline for 2.449 s (`waited_ms` in the drain line),
       retired, and its slot refilled by a repair dialing **`carrier="tcp"`**
       — six per round, zero KCP. No-disruption holds: the streams on the
       drained tunnel ran to the end; placement pressure inside the window did
       not report saturation.
     - **The verdict on effect: `no claim`, correctly.** Pooled over two
       matched pairs (5 clean base rounds each side, one twin each), the
       switched medians sit at +4.07 % (4.809 → 5.004 Gbit/s) — inside the
       control run's own A/A scatter (5.07 % on the off run's twin), so the
       model refused the claim twice, as it should. The structural signature
       is exact and consistent: `fds_peak` 100 → 110 in every switched round
       (the TCP tunnel's sockets), and one pair read `rss_peak_mib` −10.65 %
       (directional; single pair). A +4 % directional lean on a 1 %-loss
       single-flow cell is consistent with the carrier table (KCP ≈ TCP at
       1 Gbit/s, TCP ahead above it) but is not a claim at this profile.
     - **What a real claim needs, stated as method**: the switch's benefit is
       a *fresh-flows* effect — a long round where the switch lands early and
       the cells after it ride the replacement, exactly the shape the L3
       policy's A/B is waiting for. One instrument could serve both; until
       then, `loss1_buf` + `bulk-n` at smoke rounds reads the mechanism, not
       the policy.

     **The fresh-flows window finally ran — and the read is now about the
     policy's *scope*, not its existence (round 31, work dirs
     `bench-20261012-104917` / `-105603`).** With the churn instrument fixed
     (round 30), the A/B the two policies were waiting for is runnable:
     `l3-budget10`, dev profile, `--scenarios udp-ladder,churn-16` (ladder
     first each round so the policy fires; churn after it so its fresh flows
     ride the replacement), `loss1_buf` on the tunnel leg, 4 rounds +
     warm-up, `--aa`, same binary both sides — control pins KCP
     (`MOLEHILL_DATA_CARRIER=kcp`, recorded), candidate leaves the policy
     live. Fingerprint identical; both files carry their switches.

     - **The whole chain fired in every candidate round**: switch → ask →
       retire, one lane per round (lane 3 of the loaded claim `:2405` in
       rounds 1–2, lane 4 in 3–4 — the rule moves the lane the evidence
       names, not a fixed one).
     - **Churn after the switch is instrument-healthy and level**: rates
       305.7–310.0/s (candidate) against 304.9–307.4/s (control), medians
       +1.11 % with the A/A twins at 1.9–2.4 % spread, `connect_errors` 0
       everywhere — the fixed instrument's first policy run produced real
       numbers on both sides.
     - **The ladder's divergence cells stayed level** (2000M: ~40 % loss both
       sides; 5000M: ~62 % both), where the earlier *diagnostic hatch* run
       read +78 %/+194 %. The difference is scope, and it is the finding:
       the hatch moved **every** claim's busiest lane; the automatic policy,
       acting on its own evidence, moved **one lane of one claim** — 1 of
       `l3-budget10`'s 10 lanes — so ~10 % of the path upgraded and the
       ladder could not see it. On the cells where the carriers are level
       (1000M: 0.03–0.34 % loss both sides) nothing moved, which is the
       conservative behaviour the policy was built to have.
     - **What this leaves**: the policy is proven to fire on evidence, move
       exactly what it proved, and cost nothing where the carriers are
       level; its benefit at full scope is a *scale* question — per-claim
       policy reach (every loaded lane, not one per round) or a workload
       narrow enough that one lane is the path (the single-claim shape
       `l3` measures). Neither is a defect: the conservative scope is what
       keeps a wrong decision cheap. The A/B artifacts live outside the
       tree (`~/tmp/churn-policy-{off,on}.json`), as evidence rather than
       published numbers.

     **The single-claim A/B exposed two real defects in the evidence chain,
     and fixing them made the handover complete (round 32, `a9ff0fa`).** The
     same A/B on the `l3` arm (one claim, one lane — the shape where the
     policy's move *is* the whole path) fired on the wrong claim and never
     finished a handover, and both failures were proven by instrumented runs,
     not review:

     - **Carrier reports were matched by lane id alone; every claim mints its
       lane ids from 1 in one shared hub.** With `MOLEHILL_L3_STATS=1` the
       ladder claim's counters sat at zero while an idle sibling read
       `peer_offered=1 000 088 / peer_refused=268 964` — the report for
       :2405's lane 1 landed on :2404's lane 1 (iteration order, not
       attribution), and the policy had been judging evidence attached to the
       wrong claim. Fixed: the client resolves the reporting service's
       claimed endpoint and the hub matches claim first, lane second. The
       regression test `a_report_lands_on_the_claim_that_owns_the_lane`
       pins the collision (two claims, both lane id 1) that the old lookup
       resolved silently.
     - **The lane's own retirement waited for an empty flow table; the table
       holds a flow for 60 s of silence.** The drain ask marked the lane
       draining at 12:00:49 and its counters froze (`injected` stuck at
       731 124 to end of round), while the stats line kept `flows=6` —
       `should_retire` required `entry.flows == 0`, and the ladder's flows
       would not have been evicted until 12:01:49, after the round. The
       replacement never joined; every cell read level because nothing had
       moved. Fixed: the lane's own retire now uses the quiet window the
       peer's drain already answers (`DRAIN_QUIET`, 3 s), which is what
       makes a handover complete inside a round.
     - **Verified end to end after both fixes** (`l3`, `loss1_buf`, ladder
       first, one attributed round): the policy names the loaded claim
       (:2405) every round, the KCP lane retires ~5 s after the ask, and the
       replacement joins riding **TCP** mid-round — lane 2, `carrier=tcp`,
       live reports flowing. The effect on the affected cell: the 5 Gbit/s
       ladder cell, held at **67 % loss / ~1.03 Gbit/s** on KCP across four
       control rounds, read **10.7 % loss / 3.0 Gbit/s** and **5.1 % / 3.2
       Gbit/s** with the handover complete — the top cell finally rides the
       carrier the evidence chose. The 2 Gbit/s cell, which the switch (16 s
       in) had not yet reached, stayed at ~43 % exactly as the held-carrier
       control predicted — the A/B's own internal control.
     - **What this changes about the earlier reads**: the round-31
       `l3-budget10` campaign's level divergence cells now have a mechanism
       (reports landing on the wrong claim) *and* an incomplete handover
       (the same 60 s wait); the round-31 verdict "scope, not existence"
       stands for that run, but its numbers are not evidence of the policy's
       ceiling. The next policy A/B, on the fixed chain, is the first one
       that can read the policy's real effect.

     **The fixed chain's first full policy A/B — and the claim the whole thread
     was building toward (round 33, work dirs `bench-20261012-122119` /
     `-122736`).** `l3`, dev, `udp-ladder,churn-16`, `loss1_buf` on the tunnel
     leg, 4 rounds + warm-up, `--aa`, same binary; control pins KCP (recorded),
     candidate leaves the policy live. The policy fired in **every** candidate
     round on the loaded claim (:2405), and the handover completed inside the
     round — the two defects the round-32 fixes removed are gone.

     The result is the honest shape a progressive handover produces, and the
     A/B's own timestamps prove the mechanism:

     | round | 5 Gbit/s cell done at | lane retired at | cell read |
     |---|---|---|---|
     | 1 | 12:29:54 | 12:29:58 | 65.9 % / 1041 Mbit/s (missed — cell ran first) |
     | **2** | 12:30:36 | **12:30:32** | **13.7 % / 2910 Mbit/s (captured)** |
     | 3 | 12:32:29 | 12:32:33 | 65.1 % / 1052 (missed) |
     | **4** | 12:33:11 | **12:33:07** | **7.4 % / 3080 (captured)** |

     The cell moves **only** in the rounds whose handover completed before the
     cell began — 3x the delivery at a fraction of the loss, in exactly the
     rounds the timestamps name, and untouched where it did not. That is the
     internal control proving the movement is the handover's and nothing
     else's. Churn after the switch: level (304.2-308.6 against
     304.4-308.3/s), the fresh flows pay nothing. One claim, one lane, the
     whole claim moved: **the policy's effect is now measured, not just its
     mechanism.**

     The cost is stated beside it: `syscalls_per_s` +31.37 % on the 5 Gbit/s
     cell is a **claim by the model's own floors** (threshold 30.05, n=[4,4],
     every round agreeing) — TCP's per-datagram work against KCP's batched
     send, paid while delivering 3x. `cpu_s_per_gbit` in the captured rounds
     is actually *better* (0.499/0.527 against 0.574-0.606): carrying 3x on a
     carrier whose per-byte cost is lower. `cpu_cores` +45.66 % is directional
     (its floor was 43.91), the same shape seen at the carrier axis.

     What this closes, and what it opens: the carrier-quality chain — per-lane
     identity, both ends' counters, two-sided drain, the rule, and now a
     measured effect — is done as designed; the L4 pool half follows the same
     pattern and its mechanism is verified, its trigger still unwired. The
     round-31 and round-32 instruments' earlier "no claim" reads are
     superseded by this one; the next release sweep prices the policy at its
     published scope.

     **The L4 pool's acceptance case is now a test, and it forced two hatch
     fixes (round 34, `7063148`).** The design notes' acceptance sequence —
     live flows unharmed while the pool hands a tunnel over — existed only as
     a unit test on pool internals; it is now an integration scenario on a
     real server/client pair (`a_live_pool_hands_a_tunnel_over_without_
     dropping_its_flows`, `#[ignore]`d — see below): a two-tunnel pool
     carrying a held visitor stream, the hatch excluding one tunnel at three
     seconds, and the assertions being the held stream echoing **after** the
     handover and again after the replacement joins (a flow is never moved,
     so it must not notice), a tunnel riding the other carrier with the
     configured count intact (the replacement is dialed only after the
     drained tunnel has released its slot, so the server's valve never sees
     an overlap), and the handover record cleared.

     Writing it forced two product fixes, both found by the test failing in
     the suite while passing alone:

     - **The hatch was read once per process**, not per pool: whichever pool
       was created first consumed `MOLEHILL_TUNNEL_SWITCH_AT` for every pool
       created after it. The switch is documented as "that many seconds after
       the pool starts", so each pool now reads it at its own creation.
       *(Round 36: incomplete — the call moved to pool creation but the
       process-wide `OnceLock` stayed inside the reader; see below.)*
     - **The hatch was spent even when no handover started**: a tick that
       found no eligible candidate (a visitor mid-open, both tunnels busy)
       burned the flag, and the pool never tried again. It is now spent only
       when a handover actually starts; a transient no-candidate tick leaves
       it armed.

     **The scenario is `#[ignore]`d under the timebox rule, and the reason is
     precise**: it passes alone and in pairs, but as the suite's second test
     it timed out four times with the pool healthy (two tunnels, all Kcp,
     `grows: 0`, `handover: None`) — the hatch never fired, though the env
     var is asserted present at the wait. The suspect is suite-context
     interference on the pool's maintenance task, not yet pinned down; the
     next round starts there (a maintenance task that never runs for a pool
     created late in a process would be a real defect beyond the hatch).
     Until then the scenario runs alone:
     `cargo test --test pool_test a_live_pool_hands -- --test-threads=1 --ignored`.
     Suites at this commit: 254 lib, 11 pool + 1 ignored, 23 integration,
     clippy x2, all green.

     **Round 36: the ignore is lifted — the defect was real, and it was not
     suite-context interference.** The failing snapshot (pool healthy,
     `handover: None`, env asserted present) pointed away from the
     maintenance task, and re-reading the round-29 fix showed why: the
     per-pool call had been moved to `with_preference`, but the reader still
     cached its answer in a **process-wide `OnceLock`**
     (`diagnostic_handover_at()`). The suite's first pool is created with
     `MOLEHILL_TUNNEL_SWITCH_AT` unset, so the cache pinned `None` for the
     whole process — the log shows ten pools established before the handover
     scenario's (`service:0:127.0.0.1:41045`, line 48 of the captured run),
     all of them told `hatch_at: None`. Alone, the scenario's pool is the
     process's first, so the hatch always fired: the exact
     alone-vs-suite split, without any interference. It was a product defect,
     not a test artifact — any process that reconfigures the hatch across
     pools (or reads it at all after a first pool) got the first pool's
     answer.

     The fix: the reader parses the environment **per pool** again (the
     cache is gone), and an invalid value is reported through
     `logging::RepeatNotice` — loud once per process, `DEBUG` after —
     instead of a `warn!` that pool creation could repeat. The scenario now
     runs as part of the serial suite: `cargo test --test pool_test --
     --test-threads=1 --include-ignored` is **12 passed, 0 failed** (three
     runs: the failing reproduction, then two greens), the alone run still
     passes (3.26 s), and the doc comment's stale ignore story is replaced
     by the per-pool-read note. Suites at this commit: 254 lib, 12 pool, 23
     integration, clippy x2, all green.

The round-3 policy in `src/transparent/lanes.rs` (still uncommitted) is
**not** the basis for any of this: it reads `no_channel` as carrier loss, has
no lane identity, and cannot see the report. It is superseded by the pair
above and should be rewritten against it rather than adapted.

That policy has since been reverted out of the worktree, so the branch is
clean; it is preserved **outside the repository** as
`~/tmp/p0/pending-strategy-round3.patch` (and the four files as they stood in
`~/tmp/p0/`). Its lending allocator (`plan`) was never part of that diff and
is still in `lanes.rs` at HEAD.

Build gaps fixed on the way (all pre-existing — the baseline does not compile
`transparent` without `multiplex`): the lane allocator, the carrier policy
and the direct-channel carrier reader are gated by the features that define
them. A `noise`-without-`multiplex` combination still fails on an unrelated
`NoiseConfig` import; it fails identically on `490198c` and is not this
cycle's.

**What the new counters say on the divergence cell** (one round,
`l3-budget10` / `udp-ladder` / `loss1_buf`, tunnel leg, `9fa82a1`, binary
`026a526622d8f9a2`; 5000M cell: 61.2 % inner loss, 1055.7 Mbit/s delivered):

| server slot of the loaded claim | `offered` | `enqueued` | `overflow` | `disconnected` |
|---|---|---|---|---|
| `member=3` | 623 296 | 349 366 | **273 930** | **0** |
| `member=4` | 941 626 | 331 224 | **610 402** | **0** |
| `member=1` | 375 127 | 374 922 | 205 | 0 |

So the loss is **this end's lane queues refusing packets**, and it is not a
single even failure: two lanes of one claim refused 44 % and 65 % of what the
device offered them while a third, carrying the same claim, refused 0.05 %.
`disconnected` is 0 throughout, i.e. no lane was missing — the round-3
policy's counter would have reported "the carrier is fine" for every lane
that had a lane in its slot, and "the carrier lost everything" for none.

That is the shape the policy has to react to, and it is why the counters had
to be split before anything could be decided on them: a per-lane queue
overflow under load is a *carrier* property (a lane that cannot take data
fast enough is a lane whose carrier is not delivering), while an empty slot
is this end's own scheduling and says nothing about the path.

Note `lane=-` on the server: the receiving end does not know the lane id its
peer named, because the server's lane prologue is read but not yet handed to
the hub. That is the next slice — the server has to attribute its counters to
the lane it read, and report them, or the deciding end still has no evidence
of the only place where the loss happens.

### KCP, first thread: the datagram-size collapse does not reproduce (2026-10-11)

The open item carried since the MTU work — "`bulk-n` after `bulk-1` collapses the
datagram size from 6.2 KB to 3.4 KB, with a repro command" — is **falsified on
`41f4005`**, on the KCP path as the product now ships it:

```
sudo -n uv run benches/scripts/bench/bench.py run --profile smoke --arms l3-kcp \
  --scenarios bulk-1,bulk-n --condition clean --link-mtu 9000 --tun-mtu 8000 \
  --rounds 1 --warmup-rounds 0
```

Both scenarios share one daemon pair and one arm, so the ordering the finding
described is the ordering this run had. Result: `bulk-1` 6.836 Gbit/s and
`bulk-n` 6.874 Gbit/s (one round each; a smoke run, so the number is a control
point and not a claim), and the per-session adaptation log — read with
`RUST_LOG=debug`, since the once-per-process `INFO` line is the log contract's
limit — shows **exactly five adaptations, all at establishment**
(`1400 -> 8000`, one per lane), and none afterwards: a *decrease* would have
logged, because the probe only reports a change. The datagram size is therefore
stable across the two workloads, and there is nothing to fix here.

What the thread leaves behind is the observation that the size is worth seeing
per run: `MOLEHILL_KCP_STATS=1` carries no size field, so the diagnosis above
needs `RUST_LOG=debug`. If a future size question comes up, add the field there
rather than promoting the per-session line back to `INFO` (the log-budget test
fails a healthy default-level run that repeats one message shape).

### The carrier migration (2026-10-11): KCP first, TCP as its fallback

Landed in one commit because the halves are one change: removing the carrier key
while TCP was still the default would have left KCP unconfigurable, and making
KCP the default without a fallback would have broken every UDP-blocked
deployment. What the work found on the way, in the order it was found:

- **The wire never had to change.** The server serves both carriers per
  connection and only *opens* the KCP listener when a registration declares it,
  so the fallback needs no protocol work: the client declares the carrier it
  will try first and dials either one.
- **The server bound a KCP listener per registration rather than per address,
  and the socket outlived its server.** With KCP as the default this surfaced in
  the integration suite as `Address already in use` — a session holds the shared
  state, which held the acceptor, so the UDP port stayed bound while sessions
  drained. The acceptor is now held **weakly** by that state and strongly only
  by the listener task, so the port closes with the server (`src/core/server.rs`).
- **The suite's fixtures shared a data port** (`data_plane_separate.toml` and
  `per_service_tunnels.toml` both on 2344) — invisible while only TCP-declaring
  clients bound anything, fatal once every server does. One fixture moved.
- **A healthy run's log budget broke**: the KCP path reported its path-MTU
  adaptation once per session at `INFO`, and the budget test fails a run that
  repeats one message shape. It reports once and then at `DEBUG` now.
- **A test that needs a specific carrier can no longer write one**, which is
  what `MOLEHILL_DATA_CARRIER` is for: `pool_test`'s spawned scenarios (the
  operator's valve, the pool's own lines) force TCP.
- **The model's carrier axis followed the key out.** An arm cannot name a
  carrier; the contrast is a *path* condition. `--condition udp_blocked
  --condition-leg tunnel` installs an `iptables -j DROP` for UDP on the leg the
  carrier crosses, so the KCP attempt cannot be established and the run measures
  the fallback — verified end to end (`l4`, `bulk-1`): the client logged
  `KCP could not be established to 10.10.0.254:2400 (KCP establishment
  timed out); using TCP for it and retrying KCP in 60s` and every pool
  established with `carrier="tcp"`. The condition is *refused* on any other leg:
  on the visitor's leg it drops the payload and the carrier comes up as usual,
  which is a run whose method says "fallback" while every packet rode KCP — the
  first attempt at this measurement did exactly that and was caught by the
  pool-stats line, not by the number. Isolating one carrier is now the inherited
  `MOLEHILL_DATA_CARRIER` switch, recorded as instrumentation.

Known and deliberate: with the carrier forced there is **no** fallback — a
failing KCP run fails rather than quietly measuring TCP — and an operator who
wants TCP always pays one bounded attempt (2 s) per endpoint per cooldown before
the fallback engages. Both are properties of the state machine rather than
configuration, and both are stated in `docs/configuration.md`.

**All of L12–L18 is landed and measured**, and the two open questions this
section used to carry are answered: lending is a `CLAIM` at +26.75 % when the
carrier is the bottleneck and indistinguishable on a clean path (above), and the
mux×1 gap is priced as the engine's per-byte work rather than a fixable defect
(above). What is left is follow-up work, in the order it is worth picking up:

1. **The L3 arms in a routine run.** The default `l3` arm holds one lane per
   claim, so it exercises the *product default* and cannot show lending at all;
   anything about lanes should name `l3-budget10-lending` and
   `l3-budget10-nolend` (the pair the model now compares itself). If a future
   sweep should carry the axis rather than an on-demand A/B, that is the pair to
   add — and the cost is real: each condition needs the long-haul leg to say
   anything at all.
2. **The L3 path's own ceiling (M, above).** The measurement said the TUN path
   is per-packet bound and the lever is the packet size (the MTU), not a
   compressor and not more lanes. Nothing there has changed; the L3 acceptance
   harness measures the wire at whatever MTU the operator sets.
3. **The gap's kernel half.** `perf` cannot name kernel symbols in this
   container, so ~35 % of the mux profile is an unattributed number. On a host
   where `/proc/kallsyms` is readable, `gap_profile.sh` would name it — worth one
   run before anyone proposes work on the kernel-side copies.
4. **`docs/benchmarks.md`'s history.** The measurement records still name the
   arms their runs used (`l3-mux`, `l3-raw4`, `l4-mux1`, …). That is deliberate —
   they are published history and the page's note maps them to today's catalog —
   but a future release sweep should add today's names rather than renumber the
   old ones.

### Per-byte or per-datagram: the two-size fit (2026-10-11, round 19)

The split said the send half owns the phase; the next question is what that half
charges *for*, and one run answers it without touching code — the same clean
`bulk-1` cell at a jumbo MTU, against the 1472-byte reading above:

| datagram size | `ms_out_send` per outbound datagram |
|---|---|
| 1472 B | **3.85 µs** |
| 8000 B | **5.25 µs** |

Fitting `cost = a + b·size` over the two points gives **a ≈ 3.5 µs fixed per
datagram** and b ≈ 0.21 ns per byte — so at 1472 bytes the *fixed* term is 92 %
of the cost, and the per-byte term is nearly noise. That is the opposite of a
copy-bound shape and two orders of magnitude above per-datagram syscall
amortisation inside a 32-message `sendmmsg`, so what it prices is the per-message
work below our staging: the kernel's UDP send path per datagram, plus whatever
per-span setup our side does.

**What that closes, and what it leaves.** Three measured attempts now agree that
this path's cost is not where the obvious code-level candidates were: the
inbound copy removal was a cost (RSS), gating the diagnostics was noise, and the
pacer is 1.7 % of the phase. The lever the numbers do point at is **datagram
count**, which is the TUN/link MTU — an operator setting, and one this
repository already measured at +84 % throughput and a third of the CPU per byte
at 8000 ([deployment.md](docs/deployment.md#transparent-services)). The
remaining code-level question — how much of the 3.5 µs is ours and how much is
the kernel's — is answerable by comparing the pump's `ms_out_send` against the
process's own system CPU time; if they match, the answer is "kernel", and no
change on this path will pay.

### And the split says which half (2026-10-11, round 18)

The measurement the note above asked for, taken on the same cell (two
consecutive stat lines one second apart, server side):

| half of the output phase | ms in that second | share |
|---|---|---|
| **`ms_out_send`** (the batched wire send) | **+729.3** | **98 %** |
| `ms_out_pace` (the pacer's per-span decision) | +12.6 | 1.7 % |

with `datagrams_out` +189 396 in the same second — so **≈3.85 µs per outbound
datagram is spent in the send half**, and the pacer costs ~0.07 µs per datagram.
At up to 32 datagrams per batch that is ≈123 µs per `sendmmsg`, one to two
orders of magnitude above what handing 32 iovecs to the kernel should cost.

**The pacer is exonerated, and the search moves into `send_spans` and the
`udp_batch` send path beneath it** — where the remaining candidates are the
per-batch iovec/msghdr construction and whatever the split-span path does with
a header plus a payload reference (a copy there would be ~276 MB/s of memcpy at
this rate, which is real but does not by itself explain 729 ms). The two new
fields are on the stats line (`ms_out_pace`, `ms_out_send`) so the next attempt
starts from this split rather than from the phase total.

## Open threads for the next cycle

- **A config the product refuses was a config every check accepted
  (2026-10-13, found while preparing v0.11.0).** The mandatory-Noise control
  channel was enforced in `run_instance` alone, so everything that *parses* a
  config was blind to it: 24 documented examples, every bench arm's
  `server.toml`/`client.toml`, both ends of the L3 acceptance harness and all
  three interop cases refused to start, while `just check` and 620 selfcheck
  assertions stayed green. Two guards now exist (the doc-example test covers
  the deployment pages and `from_str` carries the rule; `bench.py selfcheck`
  asks the built binary to load each arm's config). **The general version of
  this is still open**: any rule the daemon enforces at *runtime* rather than
  at parse time has the same hole, and there is no check that names them. The
  rule now lives in `from_str`, which is the right place for this one; a
  future one should start there.
- **Build times were decomposed (2026-10-13, host with 20 cores, rustc
  1.99); the workspace split is measured and declined for now.** The daily
  inner loop is already fast (check 1.3 s, clippy pass 1 9 s, the lib test
  binary 4.5 s; rustc 1.99's thin debuginfo makes a `debug = 1` dev-profile
  override worthless — measured, no change). The felt slowness is the
  **release build: 71 s, user ≈ wall**, which is single-crate serial
  codegen under fat LTO (`codegen-units = 1`); LTO-off is 61 s and thin LTO
  is 68 s wall (3.8 min CPU), so fat LTO costs only ~10 s and `mold` is a
  no-op on it (69.8 s vs 69.8 s — the linker is not on the critical path;
  it is installed on this host now anyway). Consequences:
  - **A workspace split would not buy the release build back**: fat LTO runs
    at the final link over the whole program, so any crate change pays the
    full 70 s regardless of how the tree is partitioned. What a split buys
    is finer check/test incrementality — which currently costs 1–4.5 s.
    Revisit when check times reach minutes or an architectural boundary
    needs enforcing; the dependency direction is already clean for it
    (`mux` ← `transport` ← `core` ← `main`, `mux` a zero-reverse-dep leaf).
  - The gate chain's benches-only classification now covers both hooks
    (`f9c0187`), which is where the actually-claimable latency lived.
- **Replacing the `udp_batch` FFI waivers (`quinn-udp`) is an A/B, not a
  cleanup.** This is the decision record behind the one place where
  `docs/lint-policy.md`'s "fix the code first" rule runs into "no equally
  reasonable alternative" — the eight `unsafe_code` expectations in
  `src/transport/udp_batch.rs` (the `recvmmsg`/`sendmmsg` batching FFI):
  - the raw syscalls have no `std` equivalent; `socket2` (already a dependency)
    covers everything *except* the batching calls;
  - **`nix` does not remove them.** Its `MultiHeaders<S>` holds
    `Box<[libc::mmsghdr]>` (raw pointers inside) and is therefore itself
    `!Send`/`!Sync`, so the three `unsafe impl Send`/`Sync` proofs — the part
    that carries the real soundness argument — would still be required, at the
    cost of a new dependency and a per-call `Vec<IoSliceMut>` allocation in the
    hot path. Verified against `nix` 0.31.3's source, not assumed;
  - **`quinn-udp` could remove all eight** (`UdpSocketState` is a plain
    `Send + Sync` struct whose `recv`/`send` take caller-owned slice buffers),
    but it also brings GSO/GRO segmentation, i.e. it changes the send path's
    syscall shape. That is a measurable change to the KCP data path, so it
    belongs in its own `just soak --test=screen` run against the current
    batching rather than in a lint cleanup.
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench/test entries with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).

## Environment notes (this host, re-checked 2026-10-01)

- **Verify `iperf3` before a long run** (`command -v iperf3`). It is installed
  at runtime rather than baked into the image, so a container restart drops it:
  measured on 2026-10-01, when a restart (new hostname, uptime reset) left the
  binary gone and a sweep produced an artifact of typed failures in 14 seconds.
  The bench fails cleanly — every test records `No such file or directory:
  'iperf3'` and the gate refuses the file — but it looks like a finished
  artifact until someone reads it.
- **`/tmp` is periodically wiped.** Keep `--out` and logs under `~/tmp` or the
  repo. The Soak harness's own work directories (`/tmp/molehill-bench.*`) are
  normal residue and are never deleted by the harness.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python
  child, which keeps executing and holds the bench lock.
- **Peers are cached** in `~/tmp/bench-peers` and the interop binary in
  `~/tmp/interop`, so `just soak` and `just interop` normally need no network.
  `just soak-peers` re-resolves each peer to the **latest GitHub release** and
  re-downloads when the cached version is behind; the resolved versions are
  recorded in the results meta beside the numbers.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.
- **This host is `3f8b4508ab91`**: 20 cores, 23 GB RAM, kernel
  `6.12.0-160000.38-default`. `/etc/machine-id` is **absent**, so the Soak
  harness's `host_id` hashes `cpu model + core count` only
  (`host_id_basis.machine_id: false`) — *not* the hostname. The container's
  hostname changes between sessions without moving `host_id`, which is the
  point: the comparable runs in this cycle's files share `host_id
  d764f9da9c7e5b2a`, and a file *without* a `host_id`
  (`results-soak-v0.9.0.json`, `98c48ea3fa68`) falls back to the hostname, so
  the gate refuses it.
- **`iperf3` is version-boundary-sensitive, and this session crossed one.**
  The carrier/KCP numbers of this cycle were measured with an `iperf3` that was
  no longer on PATH when this session began (a container restart); it was
  reinstalled and is **3.18** now. A version can change the UDP summary fields
  the model reads (`end.sum_received`, the loss percent, the interval shape), so
  a number from the other side of that boundary is context, not a comparable
  baseline — re-measure the baseline in the same session whenever the pair
  matters. The model should record the version with the run rather than leaving
  it to be rediscovered.
- **A baseline-less run is the norm here**: the gate's own self-check
  (completeness, endpoint invariant, absolute SLO) is what a fresh sweep is
  gated on, because every stored baseline either has no `host_id` or predates
  the current method. The v0.9.0 file is still compared where the method keys
  agree, and the gate names what it could not verify rather than assuming it.

## Archive

Everything this cycle closed is in [HANDOFF-archive.md](HANDOFF-archive.md),
in the order it happened, with an index at the top of that page: the incidents,
the superseded sweeps, the measurement records, the retracted diagnoses, and
the closed threads of this cycle. The records that predate the 2026-09-28
freeze were archived into git history earlier and are indexed there too
(`git show f2156de^:HANDOFF.md`).

The rule that keeps the split honest: **a record in the archive says what the
branch's authors believed at the time, and no number in it may be quoted as a
measurement of the current code, compared against a Soak result, or used to
gate anything.** The live numbers are the sweep record under "Release
(v0.10.0)" on this page.
