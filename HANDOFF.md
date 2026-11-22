# HANDOFF: the current state, and what to do next

This is the working document for whoever picks this repository up: what the tree
is, what has been measured **on it**, what is still open, and which designs are
decided. It is deliberately short. The research record — every round of
investigation, every superseded conclusion, every negative result with its
reasoning — lives in [`HANDOFF-archive.md`](HANDOFF-archive.md), and nothing in
that file describes the current tree unless this one says so.

Read `AGENTS.md` first for the rules; read this for the state.

---

## 0. Where to start right now (read this first)

> **§4.534 (the bench 定型: coverage lands on the leg each class happens on, a claim discipline, a revision rule).**
> **Why.** The release schedule was eight stages, all imposed on the visitor's connection — the leg **no tool owns** — so every
> shaped cell tied and a release run could compare arms only on a clean path. The product's own feedback channel therefore could
> not see the one thing it exists to measure.
> **What it is now.** Eighteen stages: ten on the visitor leg (healthy path bracketing the walk; distance; light loss; the same
> loss with a queue that does not tail-drop; heavy loss; 20 and 100 Mbit uplinks; the same 100 Mbit with an 11 ms queue; a mobile
> path losing and rate-capped at once; jitter) and seven on the tunnel leg, where a carrier's own transport is all there is to
> measure (distance, light and heavy loss, bufferbloat, a 1 280-byte MTU, UDP policed, UDP refused). Two rows are
> **single-parameter pairs** (queue depth at one loss rate, then at one rate) and four conditions are walked on **both legs**.
> **A cell's name carries its leg** (`loss1@tunnel`), the label rule has one home (`model.stages_of`) so the workload and
> `expected_cells` cannot disagree, `just bench gate` refuses a record missing a declared cell, the master figure draws the leg
> boundary, and a reading from one leg is never compared with one from the other.
> **The claim discipline is written down** (`docs/benchmarks.md`, "The standard"): a one-round release record may claim per-cell
> facts only — SLO held, wedge, capacity, a categorical cell, side of a threshold — and a difference claim needs the `compare`
> profile (three rounds, warm-up, A/A twin). A change under `bench/src/bench/` is a **revision**: reason in CHANGELOG, gate
> without `--baseline`, affected tables in the same commit, this file told what moved.
> **Two measurements forced the leg dimension, and both are negative results.** `mtu1280` on the visitor leg moved no arm
> (control 41.39, rathole 8.96, molehill 3.82 Gbit/s against clean's 40.45 / 9.83 / 4.00); `loss1_rate100` on the visitor leg put
> **every** arm on 0.096 Gbit/s and 286 ms, i.e. on the shaper's own number. A class imposed on the leg that does not own it
> measures the harness.
> **Validation** (scale 0.1, two arms, two passes — machinery, not evidence): `loss1@tunnel` **51 ms** against the tool-free
> control's **1 054**, `loss5@tunnel` 605 against 2 479, `rate100_shallow@tunnel` 235 against 2 694, while the same conditions on
> the visitor leg tie (1 045 against 1 084). That is the controlled comparison the schedule is for, and it is the same direction
> §4 G measured at full scale.
> **Cost, stated because it is real**: ~35 s of drain per stage on top of a 120 s hold, so the release sweep is **~4 hours** for
> five arms (budget raised to 15 000 s accordingly). `docs/release.md` and the profiles table say so. And **the tree is frozen
> while a release run is in flight**: two plotter/list commits landed three minutes into the first attempt at this run and made
> its record unusable for a release (`githooks/pre-tag` diffs the record's revision against the tag commit under
> `bench/src/bench/`), so that attempt was killed at minute ten and replaced.

> **§4.533 (the release audit: `just tag-check` goes from three failures to a pass, and the sweep withdrew one published claim).**
> **What the tag review wanted was not code.** `[Unreleased]` held 24 entries the dated `## [1.0.0] - 2026-11-01` section did not
> have, and neither `benches/records/results-bench-v1.0.0.json` nor `assets/bench-v1.0.0.png` existed. The dated section had also
> drifted three weeks behind the code it describes: every entry that *defines* this release — native multi-stream `mhd`, dialect
> 6.0, the per-service carrier policy, the AES record layer — was still in `[Unreleased]`.
>
> **The audit found the documentation lying in five places, each fixed with its reason.** `AGENTS.md`'s release line stopped at
> 0.11.1/v5.1 and its appendix sold a `kcp` feature and "protocol v5.1"; `docs/internals.md` opened with "v5.2" and explained
> v5.2's frame negotiation as the current dialect; `docs/configuration.md`'s troubleshooting rows pointed a 1.0.0 operator at
> v5.1/v5.2 and the page had **no 1.0.0 migration note at all** — for a release that changes the wire and renames the carrier;
> `docs/structure.md` called an mhd lane's sealing "per-stream Noise"; and the benchmark page carried one paragraph **twice** in
> English and **not at all** in Chinese. Separately, `bd87da4` removed the stripe width's growth and never got a changelog entry.
>
> **The sweep.** `just bench --profile sweep`, the five arms the profile names, **87** measured arm/scenario/cell triples,
> **6 257 s** of a 3 000 s budget, one round and no A/A twin, at revision `c8b8e6f` with a clean tree. `just bench gate` passes
> the absolute half **without** a baseline — the form `docs/release.md` prescribes for a method-changing release — so this record
> is the next baseline. The clean cell reproduced the development run's readings inside their own rounding (7.689 against the
> earlier 7.68, 1.942 against 1.95), which is the evidence that the host was in the same state.
>
> **What it withdrew.** The peer table said "where packets are lost the product's tail is **the tightest of the four** (1 460 ms
> against 2 030–2 450)". The release sweep reads **2 450 — last** — and the record explains both draws: the `loss5` stage
> completes about **215** requests, so its p99 is the third-worst sample and the arm order moves between runs (§4 F). The claim is
> withdrawn from both pages; `loss5` is a **tie** at ~2.0–2.5 s, and what actually separates the arms is bulk throughput (12.42
> against frp's 6.17 at 1 % loss, 9.20 against rathole's 28.57 on the clean cell).
>
> **State.** `just tag-check` now passes end to end: version and lockfile, exactly one dated changelog section with prose and an
> empty `[Unreleased]` — 24 entries folded into the dated one as a move, verified by re-sorting the non-heading lines of both
> files — both release assets, and the provenance check that no code changed between the measured revision and the reviewed
> commit. What remains is the human's (`AGENTS.md` §5): pushing `v1.0.0` publishes the release.

> **§4.512 (the soak's wedge reads zero when a control arm is in the run — and the pool move §11.4(b) asked for, which no
> bench cell can show).**
> **The wedge.** `just bench run --profile soak --arms control,molehill` — one round, five 180-second stages per arm — reports
> **`wedge_count` 0 / 0**, `wedge_max_s` 0 / 0 and `rtt_timeout_count` 0 / 0, over **11 161** interactive samples on the
> product arm and **10 824** on the control. The stages, product beside control (p99): clean **3.28 / 2.39 ms**, loss1
> **1 068 / 1 079**, rtt100 **401.5 / 401.2**, loss5 **2 051 / 2 047**, clean#2 **2.07 / 2.31**. Drift on the product arm:
> RSS **+0.58 MiB/min**, FDs **+0.06/min**, threads **−0.007/min** (an earlier suite measured the product at 7.80 against its
> twin's 11.88 — a slope is not comparable across runs, so both are kept).
>
> **What that closes and what it explains.** Every earlier non-zero reading (5 / 5, 9 / 1, 3 / 9) came from a suite running
> **two product arms at once** on this host, and `HANDOFF-archive.md` §4.318 already showed those timeouts alternating between two identical arms.
> With a control arm beside the product neither arm wedges, so the plan's floor — "both arms equal and zero" — is met, and
> what a two-product-arm soak measures is the host, not the product. The plan's P5.3 is amended with this (§十三).
>
> **The pool move.** `target_mix` now answers `(1, total-1)` for a session whose **every** observed service is bulk, and only
> when the pool is the one choosing: a forced carrier (`MOLEHILL_DATA_CARRIER`) stands the policy down, so the two measurement
> arms still measure what they name. That is §11.4(b)'s deliberate conservatism removed — the class was read for *placement*
> only, so a bulk-only session kept every tunnel on the datagram carrier and its bulk traffic with it.
>
> Three pieces of evidence, and one of them is about the bench:
> - `a_bulk_only_session_moves_the_pool_to_the_kernel_carrier`: `target_mix(4) = (1, 3)`, `wanted_carrier = Tcp`,
>   `spare_owed = Tcp`, `target_mix(1) = None`, and a kernel-carrier pool answers `None` (nothing to move);
> - the striped integration test **observed the move**: a bulk-only session's pool held **5** tunnels when the test expected
>   its configured 4, i.e. the kernel-carrier spare was dialled while the datagram tunnels were still carrying. The assertion
>   now allows the bounded spare and says why;
> - **no bench cell moves, and that is a fact worth writing down**: the bench's `auto` L4 arm rides the **kernel carrier** in
>   that topology. The record-layer win of §4.510 (+20 % on `bulk-n`) could not appear otherwise — an `mhd` payload is sealed
>   by the lane cipher and never touches the record layer — so the arm that measured it was on TCP. The A/B of this change
>   (p141, 4 rounds, `bulk-1` + `bulk-n`, the old binary against this one) reads **15.36 against 15.42 Gbit/s**: the policy is
>   inert there because the preference is already TCP.

> **§4.532 (`just bench phases` was reading a diagnostic line, and the tables were empty for it).**
> §13.8 recorded the gap as "the tool reports no carrier counters while the logs carry the line". The cause was a selector:
> `last_stats` took the **last line containing `mhd-stats:`**, and that prefix is also used by diagnostics —
> `mhd-stats: a group ended having carried almost nothing …`, `mhd-stats: waiting for the connection lock …`. It parsed one
> of those (two fields), found no phase names, and printed empty tables beside a hundred correct cumulative lines.
>
> Fixed by matching the cumulative line's own phrase. The same work directory now reads 38 fields and prices the phases:
> **`rx_total` 10.65 µs a datagram, `tx_build` 45.25 µs a run, `tx_call` 1.35 µs a packet** on the round it refused before.
> An instrument defect, not a product one — and the reason §4.523's phase numbers had to be read by hand.

> **§4.531 (on a clean path the kernel carrier wins both axes, and the policy pays for a benefit it gets only under loss).**
> Two pinned runs of the `cost` cell — the mixed workload: bulk spine, interactive, churn, UDP — on a clean path:
>
> | carrier | p99 | p50 | throughput | CPU per Gbit |
> |---|---|---|---|---|
> | mhd | **12.65 ms** | 1.18 | 8.42 Gbit/s | 0.968 s |
> | kernel | **1.89 ms** | 1.09 | **14.51** | **0.570** |
>
> `dropped_packets` 0 on both, so the lane's tail is **queueing in the lane, not repair** — and the kernel carrier is better
> on *both* axes on this path. The per-service policy puts conversational traffic on the lane by design, where the lane's
> advantage is measured at 1 % and 5 % loss; on a clean path that design costs **6.7× the p99 and 1.7× the throughput**, with
> nothing to repair.
>
> This is recorded, not acted on: §2 freezes the policy, and the trigger that could fix it — an in-situ rate comparison
> between the two carriers, which the carrier rule's probe already has — is a policy change that needs the measurement in
> §4 C's "next experiment" first. What it changes today is the *prior* question (§4 D): the axis is the path, not the class,
> so a bulk-shaped prior would buy one cell and put a chat service on the wrong carrier everywhere else.

> **§4.530 (the lane seal's missing 0.4 µs is not the buffer, and not the cipher).**
> The last named candidate for the lane's per-packet cost was the shape of the buffer it seals in: the product seals each
> packet *inside the 61 KB run it is filling*, where a 1 400-byte region at a moving offset may be colder than the cost
> table's hot 1.4 KB loop. Measured, in the table's own shape (`tests/primitives_test.rs`):
>
> | shape | per call |
> |---|---|
> | 1 400 B, hot | **669.8 ns** |
> | 1 400 B at a fixed offset in 64 KiB | **669.8 ns** |
> | 1 400 B at a **rotating** offset in 64 KiB (what filling a run does) | **673.5 ns** |
> | seal hot, then copy the sealed packet into the run | **717.3 ns** |
>
> So coldness is worth **4 ns**, and the copy that would dodge it costs 48 — the candidate fix is refused by its own price.
> With the schedule (§4.523), the allocation (§4.523) and the algorithm (AES-GCM is faster than ChaCha at both 1 400 B and
> 32 KiB — the note in §4 A said the opposite and has been corrected) all dismissed, what is left between the product's
> 1.09 µs and the table's 0.67 is **context**: the same call inside a busy pump thread, at a lower turbo, on cache lines the
> decode, ack and booking work evicted between calls. A microbenchmark cannot reproduce that by moving a buffer around.
>
> Recorded as the end of this line of attack: the next thing that could move it is sealing on a separate task, which is a
> hand-off per packet and therefore a latency question, not a throughput one — and the lane is the carrier this product
> exists for.

> **§4.529 (the striping width's growth had no consumer, so it is gone).**
> `StripePlan::note_stall` ran on every stall, took a mutex-free CAS pair, stored a new target and logged "striping target
> grew" — and **nothing read the value**. `target()`'s only readers were its own `Debug` impl and tests; the visitor path takes
> `initial_stripes()`, which is the environment override or one. So the mechanism was a lie with a log line: it claimed the
> server was adapting its width while the width could not change.
>
> Removed: the growth, its dwell constant, the field it kept, and the three tests that asserted it grew. What stays is the
> **observation** — a group that parks for `STRIPE_BACKPRESSURE` and then moves still logs `stripe group parked and then moved`,
> because that is the evidence a future width policy would be built from. A new test guards the removal from the other side:
> a stalled-then-moved group leaves `target()` at one, so re-wiring growth fails a test that names the reordering question
> first. Recorded as an invariant in §2.

> **§4.528 (§11.4(a) closes below the instrument's threshold).**
> The question was whether a carrier move's per-step handshake is worth replacing with a permanent spare. Two auto runs with
> `MOLEHILL_MHD_STATS=1` (`bulk-1` + `bulk-n`, four rounds each) contain **no `mhd pool placement` line at all**, and that line
> fires only for a single placement of **≥ 1 ms**. Hundreds of stream opens, including the seconds in which the pool converged,
> and every placement was under a millisecond — which is the mechanism §13.2 argued (the spare is dialled by the maintenance
> tick and the trim only removes idle tunnels) confirmed by the instrument. A permanent extra lane would buy no visitor-visible
> latency and would pay for one lane forever.

> **§4.527 (`mhd 定型`, on the restated floor and an independent regression).**
> Written this round, and only because the regression in it is independent of the criterion rewrite: the floor of §4.524 was
> restated in one session, so the verdict had to be paid for in another. The evidence, all of it from runs whose method the
> plan records: mixed-load interactive p99 **14.4 / 9.4 ms** clean and **1 064 / 1 263 ms** at 5 % loss; `loss5` tunnel `bulk-1`
> **0.1270 / 0.1317 Gbit/s** against the kernel carrier's 0.106; bulk at **79 %** of the kernel carrier against a **75 %** floor;
> five soaks with `wedge_count` **0 / 0**; and this round's regression — **22/22 ×9** (default, `mhd`, `tcp`, three runs each),
> **20/20** striped sealed, **3/3** interop, **16 PASS** L3, `just check` green.
>
> Two limits travel with it and are stated in the plan rather than in prose here: the floor's rewrite is this session's work, so
> the verdict rests on §4.522–§4.523's three refutations rather than on the old wording; and the soak's wedge is **not
> reproducible on demand** rather than fixed — the floor holds because today's harness cannot see it, not because a fix was
> found.
>
> The plan now has no unwritten section. What remains is product work with its own prices: the lane cipher's 0.78 s/Gbit
> (another algorithm, not another AEAD unit), whether the first cell's 62 % is worth a prior, and whether a real NIC brings the
> wedge back.

> **§4.526 (the auto strategy on the plan's own criterion: 62 % of the kernel carrier in the first cell, 81 % in the second).**
> §11.3 says the carrier decision has to be judged on two bulk cells in one session — `bulk-1` then `bulk-n` — because the
> first cell's streams open before any class exists. Measured that way, four rounds each, same binary, the switch being the only
> difference:
>
> | arm | `bulk-1` | `bulk-n` |
> |---|---|---|
> | auto | **3.584 Gbit/s** | **15.299** |
> | pinned `tcp` | 5.743 | 18.912 |
> | auto as a share of the kernel carrier | **62 %** | **81 %** |
>
> That is the whole of the policy's trade in two numbers. The first cell is the price of starting on the endpoint's preference
> with no evidence: the streams are already open when the class arrives, and a running stream is never moved. The second is
> what convergence buys — most of the kernel carrier's throughput, not all of it, because the pool reaches the mix by *adding*
> a kernel-carrier tunnel and *giving back* an idle datagram one, and the bulk streams holding the other tunnels keep them.
>
> Both halves are the documented design rather than a defect (a stream that moved would be reordered), and they are what a
> "carrier per service" claim should be measured against from now on. The remaining question — whether the first cell's gap is
> worth closing by starting bulk-looking services on the kernel carrier — is a prior, not a measurement, and the plan's §七
> records why the prior is the datagram carrier.

> **§4.525 (the path question is settled: the lanes carry it, and the old record-layer count was a multi-round mix).**
> One pinned run with both instruments on both ends (`MOLEHILL_MHD_STATS=1`, `MOLEHILL_TUNNEL_STATS=1`, `bulk-n`, one round):
>
> | end | `tx_bytes` (lane) | `lane_seal_n` | `seal_n` (record layer) |
> |---|---|---|---|
> | server | **8.05 GB** | **5 758 954** = `tx_datagrams` | **24** |
> | client | 4.9 MB | 16 | 18 |
>
> So the payload rides the lane, seal and all, and the record layer sees the control channel only — 6.14 s of lane cipher for
> 8.05 GB, **0.76 s/Gbit**, the same figure as §4.521.
>
> Which means §4.520's puzzle was not a contradiction at all: that run's log was **cumulative across a warm-up and three measured
> rounds**, and a round that starts before the pool has established a lane opens **direct TCP+Noise channels** for its first
> visitors. The record layer's 305 308 seals were those rounds' traffic, and the client's `carrier="mhd"` lines described the
> pool, both truthfully, in one file. A single-round run removes the ambiguity — and it also means any counter read from a
> multi-round workdir has to say which round it belongs to before it is used.
>
> Nothing in the floor's numbers changes: the sealed arm's lane cost is 0.76–0.78 s/Gbit across four independent runs, the
> record-sealed arm's is 0.51, and the difference is the AEAD unit.

> **§4.524 (the plan's last floor is restated, and 定型 is withheld for one more round).**
> §一's floor asked sealed bulk to reach 85 % of unsealed, and §4.522–§4.523 established that those two arms are two *encryption
> architectures*: both encrypt, one per 1 399-byte datagram and one per 32 KiB record, at 0.78 against 0.51 s/Gbit, with
> nothing recoverable around the calls. **85 % is unreachable by construction**, so the floor is restated against what it can
> mean — mhd carries **≥ 75 % of the kernel carrier's bulk on a clean path** (three measurements: 78.9, 79, 79) — and
> everything else about bulk stays with the main criteria, which are green.
>
> **`mhd 定型` is therefore not written this round, although every floor is green under the restated one.** The reason is the
> one P5.6 implies rather than says: a criterion rewritten in the same session as its verdict has not been paid for yet. The
> next round runs the full regression against the rewritten floor (integration ×9, striped ×20, interop, L3, and the two bulk
> cells) and writes the verdict on that evidence.
>
> **And one path question stays open**, recorded in the plan as §14.3: a pinned-mhd run's client holds only mhd lanes while the
> same run's server seals 305 308 records at payload volume, and the lane's own counters also show the payload. Both cannot be
> the whole truth, and until one run with `MOLEHILL_TUNNEL_STATS=1` and `MOLEHILL_MHD_STATS=1` on both ends aligns
> "lane bytes × record seals × iperf delivery", the auto-carrier proof is not made — it would inherit the doubt.

> **§4.523 (the per-call overhead is not there: no allocation, no key schedule, and the lane still pays 1.09 µs a packet).**
> §4.522 left one attackable claim: a third of the lane cipher's bill looked like per-call overhead, because the lane measured
> 0.77 ns/B where `just primitives` prices the same call at 0.478. Two candidates, both cheap to test, both measured:
>
> | hypothesis | test | result |
> |---|---|---|
> | ring's `LessSafeKey` re-derives the AES schedule and GHASH key per call | new `SealingKey` rows in `tests/primitives_test.rs` (precomputed schedule, nonce sequence owned) | **668.3 ns against 674.5** at 1 400 B — nothing |
> | the header is copied into a fresh `Vec` **per sealed packet** (`out[..].to_vec()`) | `split_at_mut` so the AAD is borrowed, the allocation gone (`src/transport/mhd/conn.rs`) | kept: it removes 470 000 allocations a second and costs nothing, but **1.088 → 1.093 µs per seal and 9.246 → 9.166 Gbit/s**, i.e. no change |
>
> So the lane's per-packet cost is **the cipher on a cold 1.4 KB buffer inside a 61 KB run**, not overhead around it — the
> table's 674 ns is a hot 1.4 KB buffer sealed in a loop, which is a different measurement of the same call.
>
> **What that settles.** The floor's 79 % is per-datagram AEAD against per-record AEAD, both at their inherent per-byte cost:
> 0.78 against 0.51 s/Gbit. Nothing in the code around the call is recoverable, and the unit of authentication cannot grow —
> a datagram carrier must let the receiver authenticate each datagram alone. **85 % is therefore unreachable by this
> architecture**, and the honest floor is a statement about the carrier rather than about sealing: on a clean path mhd carries
> **79 % of what the kernel carrier carries for bulk** (three independent measurements: 78.9 %, 79 %, 79 %), which is the same
> fact the benchmarks page states from the other direction ("TCP is the carrier for a clean path and for bulk"). The plan's
> floor should say that, with the number and this reasoning, instead of asking sealing to be cheap.

> **§4.522 (the seal floor compares two encryption *architectures*, and now both costs are measured).**
> With the two meters in place, one pinned pair on the same topology (8 streams, 10 s, `MOLEHILL_MHD_STATS=1`, carrier pinned
> to mhd) says what the arms actually are:
>
> | arm | throughput | lane cipher | record layer |
> |---|---|---|---|
> | `MHD_SEAL=1` | 9.246 Gbit/s | **11 334 710 seals** = `tx_datagrams`, **12.33 s** → **0.78 s/Gbit** | 17 seals (control only) |
> | `MHD_SEAL=0` | 11.705 Gbit/s | 0 | **582 222 seals**, **12.60 s** → **0.51 s/Gbit** |
>
> **Both arms encrypt.** The switch does not turn encryption off: it gates whether the lane's root is installed, and without it a
> later stream falls back to its own Noise KK and carries its payload in a `NoiseStream` — so the unsealed arm seals **32 KiB
> records** while the sealed arm seals **1 399-byte datagrams**. The payload moves the same 15.9 against 19.9 GB; the difference
> is the unit of AEAD work, not its presence.
>
> So the floor's 79 % is **per-datagram AEAD against per-record AEAD**, and the excess is now a number rather than a mystery:
> **0.78 against 0.51 s/Gbit of cipher — 0.27 s of CPU per carried Gbit** — on a budget near 1.0 s/Gbit, which is the 21 %.
> That also retires every earlier reading of this floor as a cipher cost: the cipher is the same AES-GCM in both arms; what
> differs is that a datagram carrier must authenticate each of its ~23 packets per 32 KiB independently.
>
> What is left to attack is therefore narrow and worth stating: the lane's seal costs **0.77 ns/B** where the same call on a
> 32 KiB buffer costs **0.478** (`just primitives`), so a third of the lane's crypto bill is per-call overhead rather than
> per-byte work. Whether that headroom is reachable — a cheaper tag layout, or a batched seal that keeps per-datagram
> authentication — is the next measurement, and the floor's 85 % may be unreachable by construction if it is not.

> **§4.521 (the two seals are now two meters, and the lane's own number is 1.08 µs a packet).**
> §4.520 could not tell which layer sealed a run because the `mhd-stats` line carries **one** `seal_us`, and that one is the
> record layer's. So the fix was to stop sharing it: `PHASES.lane_seal` times `cipher.seal` at the lane's own call site
> (`src/transport/mhd/conn.rs`), and the line prints `lane_seal_us`/`lane_seal_n` beside `seal_us`/`seal_n`.
>
> One pinned run (`bulk-n`, one round, `MOLEHILL_MHD_STATS=1`) now says which layer did the work:
>
> | counter | value | what it is |
> |---|---|---|
> | `seal_n` | **24** | the record layer — the control channel, and nothing else |
> | `lane_seal_n` | **5 677 562** | the lane — equal to `tx_datagrams` 5 677 557, so **one seal per datagram** |
> | `lane_seal_us` | **6.11 s** | **1.08 µs per 1 399-byte packet**, 0.77 ns/B, on 7.94 GB |
>
> Two things follow. The lane cipher is **0.77 s of CPU per carried Gbit** on this host — twice the 0.371 ns/B the
> primitives table gives for a 32 KiB AES-GCM call, which is what a per-packet call at 1.4 KB should cost — and that is the
> number the seal floor is actually about. And §4.520's arithmetic was wrong: it compared the lane's `tx_bytes` with its
> `rx_bytes`, i.e. two directions of one stream, and concluded the lanes carried 7.9 MB of 10.4 GB. In the run behind it the
> record layer sealed 305 308 records, so *that* run was the one not on the lane; this one is, and the meters now say so on
> the first line a reader looks at.
>
> **The floor is therefore still open, but for the first time it has a measured cost instead of a confounded ratio**: the lane
> cipher is a known 0.77 s/Gbit, and what the next round has to explain is why the sealed arm loses 21 % of its throughput
> when it pays only that.

> **§4.520 (the seal floor's runs never measured the lane: 10.4 GB was sealed by the record layer while the lanes carried 7.9 MB).**
> Chasing §4.516's impossible counter (`seal_n` 305 308 against `open_n` 35) produced the answer, and it is not about the
> instrument. Per second in the *pinned-mhd* server's own log:
>
> | second | `seal_n` | `seal_us / seal_n` | `tx_bytes` |
> |---|---|---|---|
> | 1 | 34 821 | 21.8 µs | 1.14 GB |
> | 2 | 79 655 | 21.8 µs | 2.66 GB |
> | 3 | 124 645 | 21.9 µs | 4.21 GB |
>
> `seal_n` grows with the payload, one seal per **~35 KB**, at **21.8 µs** each — which is the record layer sealing the whole
> transfer. And the same log's mhd counters say the lanes carried **7.9 MB of 10.4 GB** (`rx_datagrams` 25 209). So the bulk
> did **not** ride the mhd carrier in the run that was pinned to it: it rode TCP data channels through the record layer, and
> `PHASES.seal` was never mysterious — my §4.516 inference ("the record layer carries only the control channel") was wrong,
> and the counter was telling the truth.
>
> **What that does to the seal floor.** `MOLEHILL_MHD_SEAL` switches *lane* sealing. In a run whose payload never touched a
> lane, switching it cannot move throughput — yet the two arms differed by 21 %. So the comparison is **confounded**: the arms
> differ by something the switch does not name, and the floor's 78.9 % must not be read as "the lane cipher costs a fifth of
> the throughput" until it is re-measured on a run where the lanes are provably carrying it (`MOLEHILL_MHD_STATS=1`:
> `rx_datagrams × 1 399` must account for the payload). That is the first thing the next round does, and it is the same class
> of mistake the model's §10 rule exists for — the path under test was not the path claimed.
>
> Why a pinned pool (four `carrier="mhd"` tunnels, `established=4`, no short-carrier line) still served the visitor over TCP
> channels is **open**: the candidates are a direct (unpooled) data channel for this service and a session split the log does
> not show. The next round starts there, because every "pinned mhd" reading in these records inherits the question.

> **§4.519 (the sighting's own binary reads zero too — so neither the tree nor the lever is the difference).**
> The last hypothesis §4.518 left open was that the wedge had been fixed between the sightings and now. It is testable in one
> run: build the sighting commit (`fee56e7`, 2026-11-12, "two arms of one binary — one clean, one silent for 25 seconds") and
> point the same profile at it. **`wedge_count` 0 / 0**, `wedge_max_s` 0, one timeout on the twin, over **11 270 / 10 520**
> interactive samples.
>
> So the account stands as five clean runs and no reproduction, across three different things that could have been the cause:
>
> | run | what it varied | `wedge_count` | samples per arm |
> |---|---|---|---|
> | p160, p161 | the shape (two product arms) | 0 / 0, 0 / 0 | ~10 900 |
> | p162 | the cores (five of twenty) | 0 / 0 | ~10 900 |
> | p170 | **the tree** (the sighting commit's binary) | 0 / 0 | 11 270 / 10 520 |
>
> The 2026-11-12 readings (5/5, 9/1, 3/9) are therefore reproducible by **nothing the harness owns today** — not the shape that
> provoked them, not a quarter of the cores, not the binary that produced them. What is left is the host state those sessions
> ran in, and that is not a lever a profile can pull. What the harness *can* now say is that it is looking: the probe asks 16
> times a second, so a ten-second silence is 160 consecutive failed attempts, and every attempt carries `late_ms`, so the day
> it returns the silence is attributable to the probe or to the tool on the first run instead of after another round of guessing.
>
> **For the plan's floor this is the strongest evidence it can have short of a reproduction**: five runs, ~11 000 samples each,
> zero silences, zero timeouts on the product arm — and a named way to look again.

> **§4.518 (no lever provokes the wedge on this tree — and every probe attempt now says whether it was late).**
> Three runs on the provoking shape (`PROFILES["wedge"]`: the product twice, the rotating path, one round) and one with the
> whole bench confined to **five of twenty cores** (`taskset -c 0-4`, which is severe oversubscription for two subjects'
> runtimes, three probes, eight iperf3 streams and the backend):
>
> | run | `wedge_count` (arm / twin) | `wedge_max_s` | `rtt_timeout_count` | samples per arm |
> |---|---|---|---|---|
> | p160 | 0 / 0 | 0 | 0 / 1 | 10 860 / 10 960 |
> | p161 | 0 / 0 | 0 | 0 / 1 | 10 930 / 11 600 |
> | p162, five cores | 0 / 0 | 0 | 0 / 0 | 10 990 / 10 790 |
>
> Four clean runs now, each ~35 minutes with 11 000 samples per arm, against three historical sightings (5/5, 9/1, 3/9) on a
> tree from 2026-11-12. **What separates them is the tree, not the load**: the lever the records named (two product arms) and
> the lever that should emulate the rest (a quarter of the cores) both read zero. The most economical reading is that the
> wedge has been fixed since those runs by the changes that landed between — the `Recovery::arm` scan, the
> `DERIVED_REGISTERED_SELECTOR` round trip, the small-flight reserve and timer, the record layer — and that its historical
> correlation with host load was a correlation with *when those sessions ran*, not a cause.
>
> **So the instrument now carries the attribution.** `run_interactive` schedules each attempt and stamps the line with
> **`late_ms`** — the gap between when the request was due and when it started — on successes and failures alike
> (`bench/src/bench/probes/series.py`). A silence is made of failures, so this is the number that says whether a wedge was
> the probe's own starvation or the tool's: if `late_ms` inside a silence is large, the probe was not running; if it is zero
> and the attempts failed, the path or the tool did. Without it, every silence is ambiguous, which is why four zeroes were
> not enough to say the floor is met.

> **§4.517 (the wedge now has a profile that points at it — and it reads zero twice, 40 minutes a run).**
> The harness could not tell "no defect" from "cannot see it", so `PROFILES["wedge"]` names the shape the historical
> sightings share: **the product twice on one host** (`aa`), the same rotating path, the same probes, one round, ~40 minutes.
> Two runs (`p160`, `p161`): **`wedge_count` 0 / 0 in both**, `wedge_max_s` 0, `rtt_timeout_count` 0 on each product arm and
> **1** on each twin, over **10 860 / 10 960** and **10 930 / 11 600** interactive samples.
>
> That is a measurement and not a blind spot: the probe asks 16 times a second, so a silence of the 10 s the metric counts
> would be 160 consecutive failed attempts — it cannot hide in 11 000 samples. So with the levers the harness owns — a second
> product arm, the rotating path, the probe load, 15-minute holds — **the wedge does not reproduce**, and the readings of 5/5,
> 9/1 and 3/9 in earlier sessions came from a host state no profile can create. What they share is load *outside* the bench:
> the sessions that saw them were busy with other work, and the sessions that read zero were not.
>
> **What that leaves.** Three independent clean runs now (control beside the product, and these two), against three
> historical sightings. The plan's floor stays as written — both arms equal and zero — and the wedge is reclassified from
> "open defect" to "**not reproducible; the lever is scheduler pressure, and the next attempt has to create it**": the cheap
> version is to run this profile with the subjects and probes confined (`taskset` to a quarter of the cores), which emulates
> an oversubscribed host without needing one. That is a profile parameter, not a code change.

> **§4.516 (§4.515 is wrong: the runs are identical, and the mhd line's `seal_us` cannot be what it says).**
> The clean pinned pair (three rounds each, `MOLEHILL_MHD_STATS=1`, the pin now honest after §4.514) prints the run-size
> instrument directly, and it refutes §4.515 at the first look: **`built_p50 = built_p99 = built_max = 44`** in *both* arms.
> Every pass fills the cap of 44 packets either way, and the cumulative counters agree — **1 399 bytes per datagram** and
> **44.0 datagrams per send** (7 427 727 / 168 909 sealed, 5 808 805 / 132 116 unsealed). Batching is not the difference.
> §4.515's "50 % more, smaller runs" came from reading `tx_build_n` as the run count; it counts **every flush**, empty
> passes included, and the run-size instrument (`built_*`) was already in the line. Throughput on this pair: **9.164 against
> 11.638 Gbit/s**, CPU **1.127 against 1.009 s/Gbit**, wire per visitor byte **1.074 against 1.047** — so the 21 % is not in
> the packet size, the run size, or the wire overhead.
>
> **And the field that would have answered it is not trustworthy.** The line carries `seal_us=6 677 998 seal_n=305 308`
> beside `open_us=32 open_n=35`, but `PHASES.seal` has exactly **one timing site in the tree**
> (`src/transport/noise_stream.rs:541`, the record layer's write path) and `PHASES.open` the matching one — and on a
> pinned-mhd run the record layer carries only the control channel, which cannot produce 305 308 seals against 35 opens. So
> either another writer to that phase exists behind an alias, or the pair mixes sources; either way **a reader of that line
> would attribute CPU to the lane cipher that the counter never measured** — which is what almost happened here. The mhd
> line's `seal_us`/`open_us` fields need to be explained or removed before any further seal accounting is read from them;
> the reproduction is two pinned runs of `bulk-n` with `MOLEHILL_MHD_STATS=1`.
>
> What stands after this round: the sealed lane costs **21 % of its throughput for ~10 % more CPU per Gbit**, with identical
> packets, identical runs and 2.6 % more wire bytes. The remaining candidates are timing — post rate, ack pacing, or the
> receive path — and the instrument to settle it is not the phase line as it stands.

> **§4.514 (a pinned carrier was not pinned — the mix policy dialled the other carrier under
> `MOLEHILL_DATA_CARRIER`, and that is a measurement defect, not a feature).**
> Found while re-running the seal floor: the pinned run's own log says `pool-stats: this session carries both kinds of
> service; adding one tunnel on the short carrier`, under `MOLEHILL_DATA_CARRIER=mhd`. The pin set the *preference* only,
> and the mix branch of `target_mix` never consulted it — so any pinned run whose session showed both traffic classes
> (a bulk scenario beside a liveness probe on the echo service is enough) grew kernel-carrier spares mid-run. **Every
> pinned-mhd reading in these records is therefore a reading of a mixed pool**, and the ones taken since the class policy
> landed are suspect by that much.
>
> Fixed at the root: `target_mix` now returns `None` whenever the carrier is forced, so a pinned process keeps one carrier
> for the whole run — which is what the switch says it does ("an operator who asked for mhd gets mhd's failure rather than a
> silent downgrade"). It cannot be unit-tested in-process (`forced_carrier()` is `None` under `#[cfg(test)]` by design, so a
> test binary does not read the environment it inherited); the evidence is the pinned runs' logs before and after, and the
> integration suites under both pins.

> **§4.513 (the seal floor: 78.9 %, and the gap is not the cipher's CPU).**
> Two runs with the carrier pinned (`MOLEHILL_DATA_CARRIER=mhd`, `bulk-n`, clean, six rounds), one per side of the hatch, and
> **after** §4.514's fix so both sides are honestly single-carrier: sealed **10.367 Gbit/s** at `cpu_s_per_gbit` **1.008**,
> unsealed **13.141** at **0.910** — **78.9 %**, against the 85 % the floor asks for and the 85.6 % / 84.0 % this cell read
> earlier. Both runs now record `MOLEHILL_MHD_SEAL` in their fingerprint (it was missing from the bench's switch list until
> this round: two runs measuring its two sides had identical fingerprints).
>
> **The revealing part is the CPU**: sealing costs **+9 %** CPU per carried Gbit and the cell loses **21 %** of its
> throughput. A path that pays 9 % more per byte does not lose a fifth of its rate unless something other than the cipher
> is serialising — and the shape fits one thing: a lane's send path builds and seals its runs on **one** task, so the cipher
> is on a serialised critical path, not spread over the process's cores. That is the hypothesis the next instrument has to
> settle, and the instrument is the pump's own phase line (`tx_build`/`tx_send`); the first attempt to read it through
> `just bench phases` reported "no carrier counters in this round" while the logs carry the line (`tx_build_us=0
> tx_build_n=0` on the *client*, which is the receiving end of this upload) — so the tool's rule for which line belongs to
> a round needs the same look before the number can be read.
>
> The `l3` arm read 10.38 / 10.43 in both runs, as expected: the L3 topology sets `data_enable = false`, so its claims never
> sealed and the hatch is invisible to it.

> **§4.511 (what the kernel's 46 % is made of: two socket syscalls per 24 KiB — and the layer §4.509 blamed for a fifth of the
> cycles is one percent of them).**
> Measured on a fixed loopback pair (`/tmp/sysprobe`: server, client, iperf3 through the visitor, `MOLEHILL_DATA_CARRIER=tcp`),
> with the client a **child** of `perf trace -s` / `perf record`: this container has no `CAP_SYS_PTRACE`, so attaching to a
> running process is refused and tracing a child is what works. 8 streams carried **19.4 Gbit/s** and 1 stream 5.4 Gbit/s
> (loopback, no shaping), and the client's syscalls over 30.3 GB each way were:
>
> | syscall | calls | time in the syscall |
> |---|---|---|
> | `recvfrom` | 1 321 910 | 13.7 s |
> | `sendto` | 1 229 951 | 17.2 s |
> | `epoll_wait` | 1 436 911 | 42.7 s (summed over 10 threads) |
> | `write` | 223 644 | 0.6 s |
>
> That is **~24 KiB of payload per socket syscall** and **~1.0 ns of kernel time per byte** (0.5 each way), which is what the
> 46 % kernel share *is*: two `copy_*_user`s and the TCP stack, not an excess of calls. Cycles by image on the same path:
> kernel **46.4 %**, the binary **36.5 %** (of which **AES-256-GCM 24.1 %** — §4.510's cipher, now the single largest user-space
> cost), and a deleted mapping (**libc: the copies and memsets**) **16.7 %**.
>
> **So the stripe layer is not the fifth §4.509 called it.** Its own symbols are **0.8 %** (`stripe::recv_loop` 0.40,
> `stripe_reader` 0.19, the `StripeEvent` send 0.19) and yamux is ~2.3 % (`Active::poll` 1.21, `stream::poll_read` 0.76,
> `TaggedStream` 0.30); the rest of that fifth was copies and tokio. **A K=1 stripe bypass would buy ~1 %**, and it would
> duplicate half-close, idle-timeout and shutdown logic to get it: **rejected, priced**. What the same sample did find is
> in `src/mux/frame/io.rs`: the decoder reads the **12-byte header with its own syscall** and then allocates and zeroes
> **`vec![0; body_len]` per frame**, so a 32 KiB frame costs ~2-3 reads and one 32 KiB allocation plus memset. At 19 Gbit/s
> that is ~75 000 frames a second. **That is the next structural target** (one read-ahead buffer, frames parsed out of it,
> payloads handed out as `Bytes` slices), and it is worth more than either of the two entries this round priced.

> **§4.510 (the record layer seals with AES-256-GCM, keyed from the handshake — §4.509's next step, done and measured).**
> `src/transport/record.rs`: the KK handshake is untouched (it authenticates and it is where the secrets come from), and the
> record cipher is now ring's AES-256-GCM under one key per direction — `HKDF(salt = transcript hash, ikm = that direction's
> half of the split, info = a versioned label + the direction)` — with a per-direction `u64` counter as the nonce and the
> record's two-byte length header as the associated data (which the handshake's own transport mode never authenticated).
> Priced with `just primitives`: **32 KiB seal 12.2 µs against ChaCha20-Poly1305's 17.3** (0.371 against 0.526 ns/byte);
> 1 400 bytes 669 ns against 1 002.
>
> Three A/B runs (dev profile, `l4` against a duplicate arm on the new binary, `--aa` twin in each):
>
> | run | the cell | `l4` | `l4~b` | A/A twin |
> |---|---|---|---|---|
> | p130, 4 rounds, 3 scenarios | **tool-bound** | 12.78 | **15.36** | 12.81 |
> | p132, 6 rounds, `bulk-n` | visitor-bound at 9.8 Gbit/s | 10.23 | 10.25 | 10.26 |
> | p133, 6 rounds, `bulk-n` | visitor-bound | 10.31 | 10.30 | 10.34 |
>
> `cpu_s_per_gbit` in the tool-bound run: 0.7746 → 0.6765 (**−12.7 %**). The mixed cell's interactive tail did not move
> (screen profile, 3 rounds: p99 4.07 against 4.96, p50 1.115 against 1.119). **The honest verdict is "real, and not a claim
> the model can certify"**: 8 streams need more producer than this host's visitors sometimes have, so a host phase where
> the visitor fills the cell measures nothing — the cell resolves 20 % in one phase and 0 % in another. What is reproduced
> is the per-byte cipher cost and the tool-bound A/B. Kept (`ed624ff`); gates: `just check`, integration 22/22 under three
> carrier settings, interop 3/3, L3 16 PASS.

> **Older rounds are in the archive.** Everything from §4.509 downward — the mhd engine's development, the L3 cycle, the mux and
> carrier work up to 2026-11-20 — lives in [`HANDOFF-archive.md`](HANDOFF-archive.md#the-mhd-finalisation-rounds-archived-2026-11-21),
> with its own contents. Nothing in that file describes the current tree unless this one says so; what it keeps is the reasoning,
> the superseded conclusions and the negative results.



## 1. Current state

| fact | value |

### 1.1 Archive: the cost gate as it stood on 2026-10-27

Kept because these numbers exist nowhere else: they are the only measurement of
`cpu_s_per_gbit` in the tree, and §5's phase 6 is judged on that metric. The plan that
follows this section (six phases, approved 2026-11-06) replaced the sequence this block
ends with; the *measurements* are still evidence.

- a lost wake-up between a reader's empty read and its park (the stream parked
  until the pump's next timer — measured as a 450 ms stall per forwarded
  channel);
- `on_timeout` being run on every pump wake-up instead of only on the timer
  (a retransmission of everything in flight on each write, which is a storm);
- the reordering window collapsing to zero on a loopback RTT (every packet
  looked lost the moment any ack arrived — now floored at 1 ms);
- an EOF reported as soon as a FIN arrived, even with a hole before it
  (a bulk transfer under loss truncated at the first hole);
- a half-close (`Reset(Finished)`) stopping the *sender's* writes too, which
  truncated every echo whose reply direction outlived the forward direction;
- a graceful close that could drop a refusal written just before the lane was
  marked dead (the valve test's peer waited out its budget);
- a listener whose dispatchers and admitted lanes kept `SO_REUSEPORT` sockets
  bound after server shutdown (a restarted server shared the port with a table
  nobody read);
- GSO eligibility and partial `sendmmsg` accounting (a run that could not be
  segmented as one super-packet must fall back whole, and a partial send must
  resume, never silently drop the rest).

**Verification so far.** `githooks/pre-push` (the whole chain: audit, deny,
outdated, the serial test suite) is green on `3e8f095`. `cargo test --lib`:
300 pass (21 in the new engine, including a 100-round-trip Noise stream and an
8 MiB stream behind a slow reader). `integration_test`: 22/22 pass with
`MOLEHILL_DATA_CARRIER=mhd` and 22/22 with `=tcp` (serial, separate runs), and
the striped bulk transfer test ran 12 times in a row. `pool_test` (default
carrier), `status_api_test`, `log_budget_test`, `session_test`,
`startup_failure_test`, `noise_keys_test` and `hot_reload_test` are green.
`pool_test` under a *forced* `tcp` still fails the three tests that use the
diagnostic cross-carrier handover hatch (the server never binds UDP when the
client declares TCP); that is a harness/diagnostic interaction, not a data-path
failure, and the forced modes the release gate names are the integration
suite's.

**Still open, in order:**

1. **The cost gate was measured and it fails — this is the stop condition.**
   Two `screen` runs with `--aa` on this host, 2026-10-27, the same method
   fingerprint (`2bdcdece60a72f0c`), binaries built from `3e8f095`; results
   (outside the tree, raw evidence beside them):
   `/home/dsh/tmp/bench-20261027-150525.json` (TCP) and
   `/home/dsh/tmp/bench-20261027-151428.json` (mhd).

   - **TCP carrier:** `l4` clean cost **0.650 s/Gbit** (11.97 Gbit/s, 7.67
     cores); `l4~aa` 0.635 (12.22 Gbit/s) — the run's own floor on that cell is
     **2.4 %**, far below the 15 % materiality floor. `l3` 0.209 s/Gbit
     (21.2 Gbit/s).
   - **mhd carrier:** the **`l4` arm does not complete**. The warm-up clean
     cell timed out after 106 s; every measured round then failed in 3–7 s with
     `iperf3 error: control socket has closed unexpectedly` (three clean
     samples, plus the `l4~aa` copy failing the same way, so the failure is the
     carrier and not one build). The `l3` arm completed at **4.21 s/Gbit**
     (0.55 Gbit/s), three samples spread 3.22–4.80 — a 49 % spread, so the run
     cannot resolve anything there either. The raw arm logs are under
     `/home/dsh/tmp/bench-20261027-151428/round1/arm-l4/` (`client.log`,
     `server.log`): registration and the four-lane mhd pool succeed, then the
     iperf3 control connection through the L4 path is lost before any
     measurement.
   - **Therefore `cpu_s_per_gbit(mhd) ≤ TCP` is NOT established**, and the l3
     number is ~20× TCP's on the same run. Per the plan's P2 stop condition,
     the sequence stops here. The remaining phases (P3b/P3c, P4, P5, P6) are
     not started, and no cost or tail claim may be made about `mhd`.
   - **Open question this leaves for the human:** the L4-over-mhd control
     connection dies under the bench's six-service, TCP+UDP-shared-port,
     namespaced topology while the integration suite passes the same path in
     one process. The next step is to reproduce that topology (or run the bench
     arm with `RUST_LOG=debug` on both daemons) and find out whether it is the
     lane's pool placement under many concurrent visitor channels, the
     shared-port service pair, or something the bench path does that loopback
     does not. A real number is not available until that cell completes.
   - The earlier note that the screen profile does not measure
     `cpu_s_per_gbit` was wrong: its `cost` scenario is the headline owner of
     that metric. The failure is the arm's clean cell, not the profile.
2. **P3b/P3c** — the unreliable datagram/L3 frame classes are in the wire but
   not yet wired into the UDP-forwarding and transparent paths; the header MAC,
   the unauthenticated-state budget, the PMTU probe, pacing, `SO_REUSEPORT`
   accounting on the client and the `SessionHealth`/carrier-rule evidence are
   not done. `mhd` streams currently carry those paths through the reliable
   stream, which is correct but loses the loss-isolation win.
3. **P4/P5/P6** — rate-based congestion control, adaptive FEC, bench arms and
   the bilingual docs sweep are untouched. The docs pages were updated only
   where the swap made them false (structure, transport's Noise paragraph,
   internals' definitions); the benchmark numbers and carrier claims still
   describe the pre-swap method and must be rewritten with the new arms.

The two failed attempts that preceded this swap are why the server side is
generic over `DataChannel` today; the type-level fixes they produced are in the
commit history (`ba303aa`, `6a584d0`), and the decision they argued for — a
generic serve path, not one trait object for both carriers — is now the shape of
the code.

### 1.0.0: the KCP carrier becomes `mhd` (branch `feat/v1.0.0`, from tag v0.11.2)

The carrier is being rebuilt on its own code rather than another project's
name, in six gated phases. Landed so far, each green through the fast gates
and the full serial suite:

| Commit | What |

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

**Where the plan lives — read this first after a context loss.**

The **approved plan's original text** is `MHD-FINALIZATION-PLAN.md` in the
checkout root. It is **deliberately untracked** (`.gitignore` names it): it is the
recovery copy this file and the session objective both point at, so a compacted
or resumed session can reread the whole plan verbatim instead of reconstructing
it from summaries. Never `git add` it, never rewrite it as release copy, and
treat it as authoritative over both summaries below.

Three copies, in this order of authority:

1. `MHD-FINALIZATION-PLAN.md` — the approved plan, verbatim, untracked.
2. This file — the durable, committed record of the same decisions: §4 holds the
   measurements they rest on, §5 holds the order and its costs.
3. The session's goal objective — one paragraph so it survives a long context.

If they disagree, fix the lower one. The plan is a working document: when a
phase lands, its result is written into §4/§5 here and the plan file is updated
in place rather than amended with a second story — a superseded paragraph in an
untracked file is a paragraph nobody can date.

**The plan file was replaced whole on 2026-11-09** (third version, approved that
day). It is now the **D0–D5** plan: settle the copies, instrument the lane, choose
the fix from the readings, then judge it — with the lossy-path penalty as the
subject rather than a closed decision. The two earlier texts (the 2026-11-07
phases 0–6 and the same-day §12 P1–P5) are superseded; their measurements, closed
phases and withdrawn conclusions are **not** in the plan file any more, they are
here — `HANDOFF-archive.md` §4.99–`HANDOFF-archive.md` §4.160 for phases 0–3, `HANDOFF-archive.md` §4.165–`HANDOFF-archive.md` §4.175 for P1–P5, `HANDOFF-archive.md` §4.176–`HANDOFF-archive.md` §4.179 for
the three reversals. That is deliberate: a plan states what is being done, this
file states what was measured, and a fact keeps one home (§3 of `AGENTS.md`).

---

## 2. Decision and invariants

The goal, restated once: a new visitor connection over mhd must cost what a TCP
tunnel costs. Today it does not — see §3 and `HANDOFF-archive.md` §4.1.

Binding constraints, all still in force:

- **Configuration is frozen**: no key added, removed or renamed; no default or
  semantic change. `src/config/` may only take wording fixes that match `mhd`.
- **Wire changes go only into dialect 6.0**, which is unreleased; `just interop`
  proves a 6.0 peer and an older dialect refuse each other in both directions.
  No two tags are compatible; a dialect is `major.minor`.
- `Carrier::{Tcp, Mhd}`, the `"mhd"` wire tag, the `unsafe` rules, the permission
  boundary, TCP fallback, the L3/UDP forwarding semantics, the log budget
  (`tests/log_budget_test.rs`) and the serial test convention are all fixed.
- **The carrier decision is settled** (§4.526, §15): read per service from what the
  service carried; a bulk service's new streams take the kernel carrier, a
  conversational service's take a tunnel no bulk stream occupies, a session whose
  *every* service is bulk moves the pool to the kernel carrier (one datagram lane
  warm), and a forced carrier stands the whole policy down. Measured on the plan's
  two-cell criterion: **62 %** of the kernel carrier in the first cell, **81 %** in
  the second. What is still allowed to change is a *prior* (§4 C) and nothing else —
  no configuration key, no per-arm carrier field, no policy under a pin.
- **A stripe group's width is decided at pairing and never adapts** (§4.529): the
  growth that used to run had no consumer, and growing mid-transfer would reorder
  frames. The parked-and-moved observation stays as a log line.
- **Never create or push a release tag.** Only a human asks for that; a release
  record is written only when every gate in §7 passes.
- **The bench is 定型 from v1.0.0 and is the product's only feedback channel.** Its
  standard is written in `docs/benchmarks.md`, "The standard": a **release record**
  (one round) may claim only per-cell facts — SLO held, wedges, capacity, a
  categorical cell, which side of a declared threshold — and never that one arm is
  faster than another; a **difference claim** belongs to `compare` (rounds + an
  A/A twin). The schedule covers the deployment classes **on the leg where each
  happens** (visitor or tunnel), and a reading from one leg is never compared with
  one from the other. A change under `bench/src/bench/` is a **revision**: reason
  in CHANGELOG, re-baseline (`just bench gate` without `--baseline`), the affected
  tables updated in the same commit, and this file told what moved. "Add a
  scenario to see" is not a revision — measure it as a diagnostic first, and only
  a scenario that separates arms or asks a categorical question earns a place.
- A change that does not beat its own gate is **reverted whole**, with its
  measurement kept. More complexity is not more advanced.

---

## 3. Measured baseline

All numbers below are one run of the `sweep` profile on a freshly built binary at
revision `c8b8e6f` (clean tree), one round per arm, five arms, 87 measured
arm/scenario/cell triples. The record is
`benches/records/results-bench-v1.0.0.json` — the release artifact, and by
`docs/release.md` the next baseline — and `just bench gate` **PASSes** it without a
baseline (§7). Treat them as the baseline for any structural change, not as a
promise about a different host.

### 3.1 The clean stages, against the peers (p99, lower is better)

| arm | `timeline[clean]` | `timeline[clean#2]` | what it is |
|---|---|---|---|
| **molehill (mhd, auto)** | **7.689 ms** | **1.942 ms** | the subject |
| l3 | 2.150 ms | 1.450 ms | userspace L3 tunnel, same repository |
| frp | 2.234 ms | 7.362 ms | TCP tunnel |
| rathole | 2.154 ms | 2.170 ms | TCP tunnel, upstream lineage |
| nps | 101.266 ms | 72.158 ms | TCP tunnel (also fails capacity, §3.3) |

molehill's own row: **9.203 Gbit/s, 0.0 % errors** on `clean` and **15.568** on
`clean#2`. Both stages are inside the 50 ms SLO **on their own** — neither is
averaged, and no warm-up stage was added.

What the number *is*: the **interactive series'** p99, where each sample is a fresh
TCP connection to the tool's exposed port carrying one 64-byte echo (`probes/series.py`'s
`ping()`). The same stage window also carries a churn series and a datagram series —
they are separate metrics (`churn_per_s`, `udp_rtt_p99_ms`) and are never pooled into
this one (see `HANDOFF-archive.md` §4.39 for the correction that cost this file a wrong conclusion).

The gap this document existed for has mostly closed, and the residue is a stage
order, not a carrier: in `clean#2` mhd reads **1.942 ms**, at parity with the
fastest TCP tunnel, where the same row in the v0.11.2 baseline read 23.08 ms. The
first stage's 7.689 ms is the cold end of a cell that is bimodal for every arm that
forwards in user space — frp draws the slow mode on the *other* stage (7.362 against
2.234) — so a single round cannot say which arm is ahead, and this record cannot say
whether a fresh process stays at 7.7 ms or settles in its first minutes.

### 3.2 The shaped stages (molehill, p99)

| stage | molehill p99 | what the other four read |
|---|---|---|
| `rtt100` | 401.470 ms | 401.305–402.024 (nps 500.894) |
| `loss1` | 1073.018 ms | 1061.774–1080.589 |
| `loss5` | 2450.189 ms | 2042.192–2094.287 (`l3` 2478.847) |
| `rate100` | 197.008 ms | 194.918–197.831 |
| `rate20` | 434.016 ms | 425.525–431.594 |
| `jitter` | 106.208 ms | 105.852–107.056 (nps 165.809) |

Each stage is judged alone. Every row but `loss5` is a **tie within a few
milliseconds** — the shaped path is the queue, not the tool. `loss5` is a tie too,
and the reason to distrust its ordering is in the record: a 120-second stage at
5 % loss completes about **215** interactive requests, so that cell's p99 is its
third-worst sample and the arm order moves between runs (§4 F).

### 3.3 Capacity

`molehill sustained 8 of 8 streams`; l3, frp and rathole also reach eight, and
**nps 4 of 8** — the peer fails its own capacity, which is worth knowing before
quoting it as a comparison. (This run's per-level cells report no reading; only the
ramp carries a count, and the gate judges the ramp.)

### 3.4 What has *not* been measured on this tree

- **bulk-n against a baseline.** The gate judges the regression half only when it
  is given `--baseline`, and this is a method-changing release, so it ran without
  one (§7). The objective's "within 15 %" is therefore **unproven**, not met.
- **`churn-16` and `rr-16` across the reconnect/reuse paths** — the two scenarios
  that would show what a reused connection costs, as opposed to a new one.
- **A real path.** Every number above is loopback inside a container, which is the
  friendliest path for every arm and the least like a deployment.

---

## 4. Open problems

Each is written as symptom → what is unknown → the next experiment → the
pre-registered pass and rollback. **The pass and rollback criteria are fixed
before the experiment runs**; a run that changes them afterwards is not evidence.

The record of every closed, superseded and abandoned question — §4.1 through §4.382, the mhd
engine's development, the L3 cycle, the mux and carrier work — is in
[`HANDOFF-archive.md`](HANDOFF-archive.md#the-open-problems-record-archived-2026-11-21), with its
numbers unchanged so an older citation still names the same round. This session's rounds (§4.512–§4.528)
are the blockquotes above, in §0.

Three questions are open on the current tree.

### A. The lane cipher costs 0.78 s per carried Gbit, and the unit cannot shrink

**Symptom**: mhd carries **79 %** of the kernel carrier's bulk on a clean path (78.9 / 79 / 79, §4.522),
and the whole gap is the unit of authentication — a lane must let its receiver verify each 1 399-byte
datagram alone, at **0.78 s of CPU per carried Gbit**, while the record layer's 32 KiB records cost **0.51**.
**Unknown**: what the remaining 0.4 µs a packet is. Everything that could be named has now been measured and
dismissed: the cipher (AES-GCM is the faster of the two at *both* sizes — 0.478 ns/B at 1 400 B against
ChaCha's 0.716, and 0.371 against 0.526 at 32 KiB, so "another algorithm" is not the answer and the earlier
note here that claimed otherwise was wrong), the key schedule, the per-packet allocation, and — measured this
round — the buffer shape: sealing at a **rotating offset inside a 64 KiB run costs 673.5 ns against 669.8 at a
fixed offset and 669.8 in a hot 1.4 KB buffer**, and sealing hot then copying costs 717.3, i.e. worse
(§4.530).
**What is left is context, not code**: the product's meter reads the same call inside a busy pump thread —
lower turbo on a loaded box, and cache lines the decode, ack and booking work evicted between packets — and a
microbenchmark cannot reproduce that by rotating a buffer.
**Next experiment** (only if the number is worth it): a variant that seals on its own task, to price the
hand-off against the context. **Pass**: a lane seal under **0.6 s/Gbit** with the same nonce discipline and no
new failure mode in the loss tests. **Rollback**: the cipher stays where it is; the floor stays restated (§15).

### B. The soak's wedge is not reproducible

**Symptom**: three sightings on 2026-11-12 read `wedge_count` 5/5, 9/1 and 3/9; five runs since — two product
arms, a quarter of the cores, and the sighting's own binary — read **0 / 0** over ~11 000 samples an arm
(§4.517–§4.519).
**Unknown**: what the sightings shared, given that neither the tree, the shape nor the core count reproduces them;
the remaining candidate is a host state inside the container that no profile can create.
**Next experiment**: run `--profile wedge` on a host with a real NIC (this container is loopback + netem, so no
interrupt path). The probe now stamps `late_ms` on every attempt, so a silence is attributable to the probe or to
the tool on the first run that sees one.
**Pass**: a run with `wedge_count > 0` whose silences carry large `late_ms` (the probe) or near-zero (the tool) —
either answer closes the question. **Rollback**: none to make; nothing was changed for this.

### C. On a clean path the kernel carrier beats mhd on **both** axes, and the policy pays 6.7× the tail for a benefit it only gets under loss

**Symptom**: the same mixed cell (`cost`, bulk spine + interactive + churn + UDP, `clean`), one carrier per run:

| carrier | p99 | p50 | throughput | CPU |
|---|---|---|---|---|
| mhd (pinned) | **12.65 ms** | 1.18 | 8.42 Gbit/s | 0.968 s/Gbit |
| kernel (pinned) | **1.89 ms** | 1.09 | **14.51** | **0.570** |

`dropped_packets` is **0** in both, so the mhd arm's tail is not loss repair — it is queueing inside the lane. Neither run
carried an A/A twin (each arm was one pinned carrier), so this is a two-run comparison rather than a claim: 6.7× is an order
above anything this cell has shown, and the two runs differ in the switch and in nothing else the fingerprint records. And the
policy sends *conversational* services to mhd by design (§2), where the carrier exists for the tail it gives on **impaired**
paths (1 % and 5 % loss, where it is 11× and 3.8× tighter than the kernel carrier). On a clean path it is **6.7× worse in
the tail and 1.7× worse in throughput**, and every deployment that is not losing packets pays that.

**Unknown**: what signal distinguishes "this path is clean, use the kernel carrier" from "this path is impaired, keep the
lane" *before* the traffic arrives — the existing carrier rule triggers on mhd *losing* traffic (a ≥ 4 % retransmit share),
which a clean-but-slow path never produces.

**Next experiment**: an in-situ comparison rather than a trigger — the rule already has a probe that compares carried bytes
between the two carriers; arm it on a *rate* difference (the lane delivering materially less per round trip than a kernel
probe) rather than on loss, and measure `cost` at `clean` and at `loss1` in the same run. This **changes the policy**, which
§2 freezes, so it is an open question until that measurement says which way.

**Pass**: `cost` at `clean` within 1.2× of the kernel carrier's p99 **and** `loss1`/`loss5` unmoved. **Rollback**: the policy
stays as it is, and this section keeps the number that says what it costs on a clean path.

### E. The auto carrier policy decides correctly — and changes nothing on a workload whose streams are already open

**Verified end to end on this build**, one `cost` run with the pool and placement instruments on, unpinned:

```
traffic class changed  service=ServiceId([0,0,0,4]) from="conversational" to="bulk"
grows=2 shrinks=0      size=6 count=4
tunnels=2/0/0/mhd, 1/0/1/mhd, 2/0/0/mhd, 2/0/0/mhd, 0/0/0/tcp, 0/0/0/tcp
placements=36 fallbacks=0 (mean 33 µs, max 161)   placements=31 fallbacks=0 (mean 61, max 956)
```

So the three halves of the policy all fire: the class is **learned** (`conversational → bulk`), the mix is **owed and
dialled** (two kernel-carrier spares, `grows=2`), and placement **never falls back** (`fallbacks=0`, 33–61 µs a placement).

**And the effect on that workload is zero.** The four datagram tunnels hold all seven streams; both kernel-carrier
spares hold **none** (`0/0/0/tcp` twice), because the bulk spine opened its streams in the stage's first second — before
the class existed — and a running stream is never moved. The policy's benefit is therefore only available to sessions
that open **new** streams after the class is learned, which is exactly the two-cell criterion: `bulk-1` 62 % of the
kernel carrier, `bulk-n` **81 %** (§4.526).

**And it leaves capacity it cannot give back**: `size=6 count=4` for the whole run, `shrinks=0`. `trim_step` reclaims an
*idle tunnel of the over-represented carrier*, and every datagram tunnel is busy, so the two idle kernel spares stay.
That is the documented trade ("a pool one over its count is bounded … better than a mix that can never be reached") —
but here it is two over, carrying nothing, for as long as the streams last. **Not changed**: §2 freezes the policy, and
the trade is deliberate.

**And measured against its own kernel carrier, on the same cell, three rounds each with an A/A twin per run:**

| | auto `l4` | its twin | pure `tcp` | its twin |
|---|---|---|---|---|
| p50 | 1.113 ms | 1.101 | 1.074 | 1.084 |
| p99 | 4.356 | 3.536 | 2.132 | **8.985** |
| worst 1 s | 2.859 | — | 1.490 | 4.970 |
| throughput | **8.979 Gbit/s** | 9.002 | **14.52** | 14.81 |
| CPU per Gbit | **0.885 s** | 0.887 | **0.569** | 0.569 |

**Throughput and CPU resolve; the tail does not.** The twins agree within 0.3 % on both throughput and CPU, so pure `tcp`
carrying **+62 %** at **36 % less CPU per Gbit** is a difference far outside either run's noise. The p99 row is not: this
run's `tcp` twin read 8.99 ms, worse than auto's arm, so that column's scatter swallows the comparison — the earlier pinned
pair (§4 C: 1.89 against 12.65) is the reading to use for the tail, and it points the same way.

**Why auto does not get there**: its pool *did* converge — the log has three "adding one tunnel on the short carrier", one
"the mix converged; giving the spare tunnel back" and the class change — but the streams that carry the bytes were opened on
the lane before the class existed, and a running stream is never moved. Convergence buys capacity for the *next* stream; on a
workload whose streams all open in the first second, the next stream is a churn arrival, not the bulk spine.

### D. The first bulk cell rides the carrier it started on (62 %)

**Symptom**: on the plan's two-cell criterion, auto reads **62 %** of the kernel carrier in `bulk-1` and **81 %** in `bulk-n`
(§4.526): the class arrives after the streams are open, and a running stream is never moved.
**Decided, not measured further**: **no prior.** A prior would start an unobserved service on the kernel carrier to lift
`bulk-1`, and C is the reason it is not worth it: the axis that matters is the *path*, not the class, so a prior chosen for
bulk would put a chat service's first channels on a carrier that is only better when the path is clean, and would do nothing
for `bulk-1` on an impaired one. What would reopen it is a path-aware signal (C), where the same signal answers both.
**Pass/rollback**: none — the decision is to leave the policy alone (§2).

### F. The 5 %-loss tail cannot rank arms at this stage length

**Symptom**: the release sweep reads `loss5` p99 as molehill **2 450 ms**, `l3` 2 479, frp 2 088, rathole 2 042, nps 2 094 — and
the development sweep the day before read the product **first** (1 460 against 2 030–2 450). Same profile, same host, same method.

**Cause, from the record**: that stage completes about **215** interactive requests in its 120-second hold — a serial probe, and the
median request already costs the path's 401 ms — so the cell's p99 *is* its third-worst sample. The arms' spread (2 042–2 479) is
the distance between the third-worst samples of five draws from one distribution; their p50s sit within **0.72 ms** of each other.

**Consequence**: `loss5` is a **tie**, and no page ranks the four there any more — `docs/benchmarks.md` and the README say so, and
the earlier claim ("where packets are lost the product's tail is the tightest of the four") is withdrawn as one draw of three
samples. What would reopen it: a stage long enough, or a probe concurrent enough, to put a thousand requests in the cell. That is a
model change and belongs in the plan, not in a release.

### G. The peers' own leg — the experiment every published table skipped

Every staged table on this page puts the impairment on the **visitor** leg (`condition_leg: visitor`), so each tool's own transport
runs clean and every shaped stage ties (§3.2). `--condition-leg tunnel` moves the same condition onto the hop the tool *does* own.
First run of it: `--profile smoke --scenarios bulk-1,rr-16 --condition loss1 --condition-leg tunnel`, four arms, **two measured
rounds each** (record `/home/dsh/tmp/p310-tunnel-leg.json`, not committed — directional, and the run itself says its `rtt_max`
floor needs about six rounds):

| cell (1 % loss, 10 ms on the tunnel leg) | control | molehill | frp | rathole |
|---|---|---|---|---|
| `bulk-1` throughput, Gbit/s | 4.27 | **1.09** | 1.08 | **4.17** |
| `rr-16` requests/s | 562 | **697** | 537 | 559 |
| `rr-16` p99, ms | 245 | **48.7** | 73.7 | 246 |
| `rr-16` p50, ms | 20.35 | 20.25 | 20.66 | 20.54 |
| `rr-16` connect+setup p50, ms | 20.3 | 0.65 | 0.51 | 0.43 |

**Two findings, and they point opposite ways.** On the interactive axis the product is the only arm that beats the tool-free path
(**+24 % requests/s, p99 48.7 ms against the control's 245**) and it is **5× tighter than rathole, 1.5× tighter than frp** — which is
what per-stream recovery over a datagram buys: a lost segment costs one stream one round trip, where yamux-over-TCP costs all sixteen
streams an RTO, and rathole's p99 *is* the control's. On the bulk axis the same design loses **3.8× to rathole** (1.09 against 4.17)
because the window's growth phase ends at the first loss (§4 B, "the window is the whole send-rate policy"); frp loses the same way,
so this is not a peer-versus-us difference but a window-rule difference.

**What it means for the product's claims.** "Latency on an impaired path" is now compared against peers, not only against our own
kernel carrier, and it holds. "Throughput" must never be quoted on a clean path — the ranking inverts with the leg the loss is on.
What would turn either into a claim: the same run at six rounds with the endpoints pre-registered (`--primary`), and the tunnel-leg
staged schedule (which no published table is yet; `docs/benchmarks.md`, "What the model cannot answer").

### H. The first record under the standard — what it replaces, and the merge checklist

The run in flight (`~/tmp/p380-standard-sweep.json`, five arms, `sweep` profile at revision `a1d941e` and later, ~4 h) is the
**first record under the 定型 bench**. It replaces `benches/records/results-bench-v1.0.0.json` — measured under the eight-stage
visitor-only schedule, i.e. a method that no longer exists. Since v1.0.0 was never tagged, that file was never *published*: it is
replaced in place, and the old numbers survive only in this file's history and in `CHANGELOG.md`.

**The merge, in order, once the run lands:**

1. `just bench gate <record>` **without** `--baseline` — a method-changing release re-baselines (§7). Absolute half only:
   coverage of every declared cell, the endpoint invariant, the SLO on the clean stages, drift and wedges, the ramp.
2. `just bench plot <record> --out-dir assets/` and rename to the release's names (`bench-v1.0.0*.png`), replacing the set.
3. Move the record into `benches/records/results-bench-v1.0.0.json` and the charts into `assets/`.
4. Rewrite **both** published tables — `README.md`/`README.zh.md` and `docs/benchmarks.md`/`.zh.md` — split **by leg**, from
   `just bench plot … --markdown`'s own output (the tables it prints are already per leg), with the claim discipline stated
   beside them: one round, so the tables say which side of a threshold a reading falls on and never rank arms by speed.
5. CHANGELOG: the re-baseline entry (already written) says the next record becomes the baseline; name the file.
6. Merge this branch into `feat/v1.0.0`, then `just tag-check`. The record's revision and the tag commit must differ only in
   docs/assets/records — any change under `bench/src/bench/` after the measurement makes the artifact unusable (§0, §4.534).
7. The tag itself is the human's (`AGENTS.md` §5).

**The risk to watch, and the one thing that must *not* happen.** A stage may produce no cell for an arm — `udp_blocked@tunnel` on
`l3` is the candidate, because a claim's lanes ride the datagram carrier (though `src/transparent/lanes.rs` can move a lane to TCP,
so the fallback may well cover it). If the gate then names a missing (arm, cell), the fix is **a revision of the model** — either
the workload learns to measure it, or the condition declares which architectures it can be imposed on — and the run is repeated.
**The gate is never relaxed to fit a record**: a coverage check that can be talked out of a missing cell is not a coverage check.

## 5. Active architecture decisions

> **The governing plan is `MHD-FINALIZATION-PLAN.md`** (checkout root, untracked
> on purpose — see §1, "Where the plan lives"). Its phase order supersedes the
> "Order, as of this commit" list further down this section; that list still
> carries the measurements and reasoning behind each item, so it is kept, not
> deleted. When a phase lands, write its result in §4 and update the plan file in
> place.

**Where the plan stands (2026-11-08).**

| phase | state |
|---|---|
| 0 — measurement apparatus | **landed**: `src/probe.rs` (pool-less lane seam), `bench/transport_harness` (L1/L2), the gate wiring that makes a compiled bench member un-skippable, and three findings (`HANDOFF-archive.md` §4.99, `HANDOFF-archive.md` §4.100, `HANDOFF-archive.md` §4.101) |
| 1 — recovery accounting and lifecycle | **closed** (`HANDOFF-archive.md` §4.102): the counters were right; two invariants are now tests. The eight lifecycle items are handed to phase 3 as a checklist, not as proven |
| 1b — window and admission policy | **closed, no fourth buffer size** (`HANDOFF-archive.md` §4.43, `HANDOFF-archive.md` §4.103, `HANDOFF-archive.md` §4.104): three settings, three failures |
| 2a — a plain-data plane in the bench | **landed** (`HANDOFF-archive.md` §4.105): `l4-plain`, with `Arm.noise_data`, its docs and its selfcheck. Its A/A agrees to 0.04–0.5 %, making it the project's best instrument for per-connection work |
| 2 — no duplicate open ack | **landed** (`HANDOFF-archive.md` §4.106), against its own revert rule and with the reason stated: wire −18.9 %, CPU −15.4 %, p99 −11.4 % (below the 15 % floor) |
| 2b — the striped window livelock | **blocking everything** (`HANDOFF-archive.md` §4.107): the cell fails ~half of runs, `flight` sits at twice `cwnd` on both ends and neither writer may proceed |
| 3 — RACK / PTO / liveness | not started |
| 4 — re-attribution | not started |
| 5 — throughput and resources | not started |

**Next step: the server's receive path, measured before it is changed.** The human chose attacking the
25 µs over restating the acceptance criterion, and the first attack is already spent: an inline send from
the writer (`HANDOFF-archive.md` §4.136) left p50 exactly where it was (45.0 µs) and was reverted. What that buys is the
hypothesis it kills — the client's hop to the pump is not the cost.

The other end is: the server's `read_us` is **41 µs over mhd against 15 µs over TCP** (`HANDOFF-archive.md` §4.133/`HANDOFF-archive.md` §4.134), and
that number is read *after* the datagram has arrived. So the 41 µs is inside `recvmsg` → route by lane id →
`on_datagram` → `apply_datagram` → wake the reader → the reader task running again: two task switches and
one syscall.

The order is instrument first: stamp that chain, read it on the L1 harness, and pre-register `complete_us`
p50 **and** p99 against the current build's A/A in the same run. Only an effect beyond that A/A goes to L3
(`rr-1`/`rr-16` with clean and `loss1` as guardrails), and no effect means a revert, exactly as `HANDOFF-archive.md` §4.136 and
`HANDOFF-archive.md` §4.125 went.

**Two criterion gaps stay on the list** rather than falling out of it: `timeline[clean]` at `--scale 1.0`
with TCP and peers in the same run, and an `l4-plain` baseline record so that `bulk-n` at 85 % can be
judged at all.

**Two things already judged, and not to be reopened.** Phase 2's latency is judged on `rr-16[l4-plain]`
(1.19–1.48 % resolution, 81 s) and moved nothing — it is a per-open saving, so only a churn cell can price
it (`HANDOFF-archive.md` §4.119). And the eight lifecycle invariants are audited: six fully pinned, two partial, with two real
defects found and fixed on the way (#3's phantom accept, #1's live-id overwrite).

**And nothing about the clean cell may be claimed again until a measurement can resolve it.**

Three things stay open, deliberately and with their evidence:

- the lost readiness edge is **bounded, not fixed** — it is still between `drain_ready`'s bare
  `try_io` and the `select!` that follows it, and no visitor's latency depends on that round
  happening first;
- a lane that is *idle with a stream open* now pays up to 1 000 wake-ups a second, which is the
  poll's pure cost and is unmeasured. It belongs to the next milestone's `cpu_cores` and
  `rss_peak_mib` guardrail, and `HANDOFF-archive.md` §4.101 says so;
- admission is still bounded by `MAX_BUFFERED`, not by the window (`HANDOFF-archive.md` §4.42), and `HANDOFF-archive.md` §4.104 says that stays:
  three settings were measured and each cost a shaped cell, because the buffer is the carrier's only
  shock absorber for a visitor link it cannot see. It is a **closed** defect, not an open one — the
  honest description is "the buffer is load-bearing", not "the buffer is wrong".

**Three objectives, and the third one is the D0–D5 plan** (2026-11-09). The objective that carried phases 0–3 was
marked complete at revision 18 (`HANDOFF-archive.md` §4.160: four of six acceptance criteria met, one met at an indication, one
declined), and a completed goal cannot be re-armed. So the §12 extension lived in a **second objective**
(`goal-7b7c4139-cfb5-4c7f-8720-eb758ab9a4c2`, 120 rounds) whose five stages were P1 measurement → P2 instrument →
P3 coverage → P4 controller (conditional) → P5 closing, with `HANDOFF-archive.md` §4.164 mapping each finding to its stage. That one is
**complete too, and its subject turned out to be the wrong one**: §12 asked whether a controller was worth
writing, and the answer to the defect it uncovered was left as "no change, mechanism named".

The **third objective** (`goal-baa3d2de-a96a-4be8-a5b3-30cd6801d18a`, 120 rounds) is the one that runs now, and
its text is the **D0–D5** plan that replaced the plan file on the same day:

| stage | what it produces | state (2026-11-09) |
|---|---|---|
| **D0** | the three copies agree — plan file, this file, the objective | **done** (`15e4d76`) |
| **D1** | per-lane instrumentation that can tell window, scheduler, acknowledgement, repair and counting apart | **done** (`cebd0bc`, `8e9a189`, `c0a5216`, `cdb98f5`): the `SendLedger` and eight tests |
| **D2** | attribution runs that **choose** the fix | **done** (`HANDOFF-archive.md` §4.181–`HANDOFF-archive.md` §4.185): the cause is out-of-order evidence, and the storm reproduces at `l4-mux8` |
| **D3** | **one** candidate, submitted and reverted alone | **B fixed (number zero was `send_gso` fragmenting each lane's opening run); ten mechanisms measured and reverted; the human ruled on 2026-11-10 that the one known defect is fixed first and the 8-11 % throughput gap is a separate matter.** The one remaining defect is the shallow cell's drops (2 153/2 892 against the peers' 16/17). Ten mechanisms reverted, `HANDOFF-archive.md` §4.212–`HANDOFF-archive.md` §4.227; the one fact under all ten is that `cwnd` grows only on acknowledgements and the pacer decides how much there is to acknowledge, so a schedule driven by the window is driven by its own output. **Next candidate A⁹ is a queue-target controller**, pre-registered in the plan |
| **D4** | the pre-registered criteria and guards (six cells, listed in the plan's §四) | **six cells measured, and the defect cell fails**: `loss5` at 51 % of its floor with a 1.4 s tail (`HANDOFF-archive.md` §4.199); `rate100_shallow` behind both peers with two orders of magnitude more drops (`HANDOFF-archive.md` §4.197); `rate100`/`rate1000` no longer reproducing their records (`HANDOFF-archive.md` §4.197, `HANDOFF-archive.md` §4.200); `bulk-n` on its floor (`HANDOFF-archive.md` §4.196); `loss1`, `jitter` and `transport` hold (`HANDOFF-archive.md` §4.196, `HANDOFF-archive.md` §4.198, `HANDOFF-archive.md` §4.201). These are the **before** numbers: once D3's fix lands, all six are re-run, and they are not a verdict until then |
| **D5** | regression and docs close-out (§五) | gates green, docs consistent, tree clean and pushed | **Engineered close-out done; "mhd is final" deliberately NOT claimed.** `just check` green end to end (107 mhd tests, audit/deny/outdated); `just interop` green (all three dialect-refusal cases); `just l3-accept` green; `capacity` 8/8 (both arms reach eight streams, headroom 0.0); integrations **mhd 3 × 22/22** (90.01/91.96/94.77 s) and **`MOLEHILL_DATA_CARRIER=tcp` 3 × 22/22** (80.18/83.53/84.53 s); **striped 20/20** with no flake; `transport`'s four numbers identical to `HANDOFF-archive.md` §4.211. Docs: B's fix in `[Unreleased]`, D4's table in `HANDOFF-archive.md` §4.228. **§五.7 is not satisfied** — the shallow-queue defect is unfixed (ten mechanisms, all reverted, `HANDOFF-archive.md` §4.212–`HANDOFF-archive.md` §4.227) — so the marker stays off until the human rules between a queue-estimate design and recording the behaviour as known and bounded |

**The candidate, in one paragraph**, because this is the sentence the next session needs: a byte-range
acknowledgement — a separate optional `Ranges` frame naming up to four byte ranges the receiver holds above its
reported prefix, sent only when there is such data. The sender retires whole records covered by those ranges
(`flight` falls) and leaves the stream's contiguous `base` alone, so stream semantics do not move and a later
prefix cannot double-count a retired record. The predicted chain is `flight` down → `cwnd − flight` open → new
data admitted → the queue drains → acknowledgements land inside `loss_delay` → the timer stops over-declaring →
growth resumes. Its cell, primary metric, eight guards, two cross-guards and revert rule are in the plan under
"D3 预登记"; nothing about them is to be renegotiated after a run.

**Read the plan file, not this paragraph, when the two differ** — it is the approved document and this is a
summary of it. What §12 left behind, kept here because these are measurements and not a plan:

- **P1 done** (`HANDOFF-archive.md` §4.165–`HANDOFF-archive.md` §4.170) with one correction that mattered: the first three runs shaped the **visitor**
  leg, not the carrier's, and a window line reading `srtt=1000us` under a 100 ms condition is what caught it.
  Corrected, a 5 %-loss carrier path costs mhd about four-fifths of its throughput — 0.29 Gbit/s against 1.19
  clean, a level rather than a lottery.
- **P2 done** (`HANDOFF-archive.md` §4.171): `transport` scenario, four declared metrics, bilingual rows, selfcheck 741 → 770. mhd's
  stream open is 3 µs against a TCP connect's 48, and its round trip is 2.5× the bare socket's.
- **P3 done** (`HANDOFF-archive.md` §4.172): the listener's dispatchers and the admission cap now have tests that observe the
  release — the task ending and the socket handle going — rather than the request for it, and both are
  load-bearing. #7's task bound stays partial, recorded as structure rather than as measurement.
- **P4's gate looked met on one shape with the sign reversed** (`HANDOFF-archive.md` §4.174): `rate100`/`rate1000` say the path limits,
  but on a 5 %-loss carrier path the counters said the window did — `cwnd` reaching 1.47 MB against a
  16 MiB ceiling, which at 200 ms is 236 Mbit/s against the 290 measured. **The "reaches only 1.47 MB" half is
  withdrawn** (`HANDOFF-archive.md` §4.179): that reading is the last second of the bulk stream, and across the flow the window climbs
  steadily. What survives is that the failure is on the recovery side, not that the window never shrinks — a
  multiplicative decrease would make that cell worse.
- **P5 done** (`HANDOFF-archive.md` §4.175): `just check`, `just interop`, `just l3-accept`, three full integration runs and twenty
  runs of the once-flaky cell, plus the TCP carrier's suite at 435 passed — all on the tree that carries the
  changes.
- **The lossy-path penalty is an unfixed defect** (`HANDOFF-archive.md` §4.178). Doubling the flow from 60 s to 120 s did not raise
  throughput (0.2306 against 0.2886, single arm, so "not higher"). Two earlier framings — "accept and document"
  and then "it is a transient" — were mine and both were wrong; the cheap test that settled it should have come
  first.
- **P4 closed without a controller, and then its stated reason was withdrawn** (`HANDOFF-archive.md` §4.176–`HANDOFF-archive.md` §4.177, `HANDOFF-archive.md` §4.179). `HANDOFF-archive.md` §4.176
  built the shallow-queue cell (`rate100_shallow`) and it shows mhd dropping two orders of magnitude more than its
  peers while carrying 19 % less — so loss does follow its own rate. `HANDOFF-archive.md` §4.177 showed the cheap recovery rules are
  arithmetically dead (a clean window at 5 % loss is 0.6 % likely at a hundred packets). What is **withdrawn** is
  the mechanism it named: the window is not frozen, it climbs ≈28 KB/s, and `cwnd` worth of acknowledgements
  arrives faster than once per round trip because **repairs ride outside the window**. So the next question is
  how much of the wire is bytes sent twice, and that is D1/D2's first job.

**§12 finished with every stage having an outcome, and the outcome was a question it declined to answer.** `HANDOFF-archive.md` §4.172's
#7 task-count bound stays structural rather than measured, and no controller was written. That is exactly why the
third plan exists: "no change, mechanism named" is an honest end to a *plan*, and an unacceptable end to a
*defect* the product ships with.

**What is queued now, in order** (the plan's D-stages, with the evidence each one owes):

1. **D0 done** (`15e4d76`) — the plan file replaced, `HANDOFF-archive.md` §4.179 recording both withdrawals, this section rewritten,
   `docs/internals.md` no longer calling the window rule "a property of the design … not a defect", and the
   objective restated as the D0–D5 plan.
2. **D1 done** (`cebd0bc`, `8e9a189`, `c0a5216`, `cdb98f5`) — the per-lane `SendLedger` on the window line, the
   first-numbered-packet log, the logged admitting refusal, and eight tests. It found two instrument defects on
   the way: `MAX_BUFFERED` refusals were counted as window refusals, and the first version of `ack` counted
   records instead of delivery.
3. **D2 done** (`HANDOFF-archive.md` §4.181–`HANDOFF-archive.md` §4.185) — `rtt100`, `loss5`, and `bulk-1`; the cause is that every lane's cumulative
   frontier is dead (number 0 never arrives) so retirement runs on 64 numbers and a per-stream prefix; the
   duplicate share is 8.5–37 % in a healthy run and 68.6 % in the storm, which reproduces at `l4-mux8`.
4. **D3 in progress** — pre-registration and design are committed and are the only authority for what "passes".
   The code is next: the `Ranges` frame, the receiver's report, the sender's retirement, the tests, then the
   pre-registered cell.
5. **D4 then D5** — the six acceptance cells and the gates; neither starts before D3 has an outcome.

**Two things this round must not repeat.** A plan that closes a defect without fixing it, and an entry that names
a mechanism from an aggregate row instead of from a counter. Both are in this file's record now; neither is a
reason to skip the work, and the D-stages are ordered so the second cannot happen again before the first is
tested.

And the release ritual is still not this: `timeline[clean]` at `--scale 1.0` is a 15.9-hour measurement (`HANDOFF-archive.md` §4.143)
and `just bench gate` needs a sweep-shaped run. Both stay parked until a human asks for a release.

And the record has been corrected four times in this session for the same reason — a fact taken from my own
notes instead of from the code or a fresh measurement (`HANDOFF-archive.md` §4.133's stale 45 µs, `HANDOFF-archive.md` §4.141's two unverified
experiments, `HANDOFF-archive.md` §4.157's wrong default, `HANDOFF-archive.md` §4.179's end-of-stage maximum read as a plateau): the rule that keeps
earning its place is to check the artifact, not the record of it.

None of the three copies may drift between turns: this section, the plan file's stage table and the objective
say the same thing, or one of them is wrong — and when they disagree the plan's original wins, because it is the
approved document.

Approved and ordered. **A is implemented and switched off** (`MOLEHILL_MHD_SEAL`,
see §4.10 and `HANDOFF-archive.md` §4.13); **B, C and D are not implemented at all.** Each is a separate
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

**E. One stream, one hop** (a visitor's first channel stops paying for a
multiplexer it does not use). A stripe group of one is driven by a single task that
polls both directions, over the same 10-byte stripe frame and the same
`StripeSender`, so the wire does not move. The group path stays for two or more
stripes, where the reorder buffer and per-stripe readers are what the layer exists
for. The decision is *structural*, and its pass criterion is the release cell's own
percentiles, not the task count.

Memory and CPU work is **phase 6**: it may not preempt A–E, and it is judged on
`rss_peak_mib` and `cpu_s_per_gbit` against a same-round A/A.

**Order, as of the 2026-11-06 rewrite — superseded in sequence by
`MHD-FINALIZATION-PLAN.md`, kept for the measurements and reasoning it carries.**
§4 carries the findings; this is the sequence only.
Rewritten on 2026-11-06 after `HANDOFF-archive.md` §4.39's correction: the previous list opened with a
live-task count whose premise `HANDOFF-archive.md` §4.37 had already refuted, and behind it a 12-worker
curve and a RACK rewrite that no measurement had asked for yet.

1. **⑥ the final-tree acceptance run** (`HANDOFF-archive.md` §4.95 closed ⑤). The gate **passes** on the four-arm run:
   coverage 36 triples, endpoint invariant on 32 samples, 2 SLO cells gated, 0 drowned cells, drift and
   wedges checked, revision `v0.11.2-744-g7539b05` (regression half unrun — no baseline supplied).
   `HANDOFF-archive.md` §4.94's flagged number also resolved **the other way from the flag**: `offered_gbps` tracks `carried`
   in every arm (molehill 7.48/7.51, frp 6.22/6.15, rathole 28.58/28.08, nps 0.13/0.14), so **rathole
   genuinely carries 3.8× mhd's throughput on the same unshaped cell**. Which completes the picture in
   one sentence: **where the shaper is the constraint mhd competes** (`loss1` p99 **1 046**, best of
   four), **and where the host is the constraint it loses** — the host-bound cost is `HANDOFF-archive.md` §4.66's
   1.73–1.76 µs/packet, which the peers do not pay.
2. **A per-visitor spare channel stays refuted** (`HANDOFF-archive.md` §4.20).
3. **B (RACK + PTO)**, judged on the shaped cells rather than the clean one: `loss1` and `loss5`
   report 1.06 s and 1.46 s of p99, which is where recovery, not setup, decides the number.
4. **C (priority scheduling): its condition is now met.** This item used to say C was conditional on
   "a `nobulk` run showing the bulk spine is what moves the interactive tail". **That run is `HANDOFF-archive.md` §4.83,
   and it shows exactly that** (3.1–3.4 ms without bulk, 9–133 ms with). So C is scheduled, and it
   follows item 1, which says *where* inside the path the wait actually is; `HANDOFF-archive.md` §4.82 already refutes
   the crude form (capping outstanding bytes), so what remains is ordering by class at the same
   total flight.
5. **D (delivery-rate) waits, and now for a measured reason.** Two estimators were built: one from
   the acknowledgement frontier (0.18–0.37 of the stage, reverted) and one from posted bytes
   (**0.91–1.05, `HANDOFF-archive.md` §4.58, per lane, joining only when lanes are summed**). The socket delivers what
   is posted, so a pacer at the socket would pace a link that is not the constraint.
6. **Phase 6 (memory, CPU)** stays last and stays judged on `rss_peak_mib` and `cpu_s_per_gbit`
   against a same-round A/A.

Settled and off the list: **one stream, one hop** (`HANDOFF-archive.md` §4.47). Built, measured at eight
rounds with the A/A inside the run, and it does not move either axis — so the group
machinery is not on the critical path, and the patch stays in `~/tmp/patches/` rather
than in the tree.

What is **not** on this list, and why: a live-task count (the premise is refuted by
`HANDOFF-archive.md` §4.37, and `HANDOFF-archive.md` §4.38's cheaper shape answers a question nobody has shown to matter), a
12-worker curve and a higher global worker default (the one attempt cost `bulk-n`
48 %, `HANDOFF-archive.md` §4.36), more timer constants (three refuted), yamux over mhd, and a static
spare channel per visitor (measured, loses).

**Cost of this cell, for planning.** A full `timeline` arm-round is 17 minutes and a
three-arm run about a hundred. Optimization rounds use `--scale 0.05
--warmup-rounds 0 --no-aa` (six minutes an arm); the release cell is measured once per
release with the full sweep. Before any long measurement, list what is running and
kill by PID — a stray run doubles every number without failing (`HANDOFF-archive.md` §4.26).

This section is the **durable copy** of the order; the session's goal objective is
the copy that is re-read every round, and a stale objective re-asserts a plan that has already
been refuted. **Every round ends by updating both** — the objective with what finished, what is
next and any change of order (the operator asked for this explicitly), and this section with the
sequence alone. Not only when the order changes: a round that changed nothing still moves the
"next step" line, and a stale "next step" is how a plan drifts.

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
| 14 | One spare data channel per visitor (`MOLEHILL_MHD_SPARE`) | loses on every cell — reuse −30 %, `rr-16` −14 %, `churn-16` unchanged; reverted (`HANDOFF-archive.md` §4.20) |

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
| `just bench gate ~/tmp/m24-sweep.json` | **PASS** — coverage 83 triples, endpoint 57 samples, SLO 4 clean cells, drowned 0, drift checked, capacity 8/8 (molehill, l3, frp, rathole) and 0/8 (nps). Revision `v0.11.2-592-gff0bae7`, fingerprint `1421e66cb2be72f0`. **That aggregate file was deleted by the `HANDOFF-archive.md` §4.6 clean-up**; the run's raw per-sample artefacts survive under `~/tmp/bench-20261105-230248/` (48 series files, 146 raw iperf3 artefacts), and the gate output above is the one this document quotes. |
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
| The mhd finalisation rounds | §4.389–§4.512 in §0's form: the mhd engine's development, the record layer, the pool's carrier policy, and the measurement accidents, numbers unchanged |
| The open-problems record | §4.1–§4.382 as they were written while open: symptom, what was unknown, the experiment, and what the run answered |
| The account this file used to open with | the two `0b` sections that used to sit between §0 and §1: this file's own opening history, and the account that preceded the consolidation |

Entry point for a specific question: `grep '^## ' HANDOFF-archive.md`, then read
that section's first paragraphs — each opens with what was asked and what the
measurement answered.
