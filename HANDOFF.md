# HANDOFF: Working State & Future Work

> **State as of 2026-09-25.** Branch `perf/data-path-optimizations` (106
> commits ahead of `main` at `8584945`, pushed, **not merged**). It contains
> the data-path rework — the in-repo tokio-native mux engine, the zero-copy
> route, KCP batching, data-channel striping, opt-in Noise session resume —
> plus the Soak benchmark model that replaced the measurement matrix. Shipped
> work: [CHANGELOG.md](CHANGELOG.md). Design: [docs/internals.md](docs/internals.md)
> and [docs/structure.md](docs/structure.md). Method and how to read the
> numbers: [docs/benchmarks.md](docs/benchmarks.md).
>
> **This file owns the working state**: what is open, what was decided and
> why, and the measurement record of work that shipped without a released
> number. Per AGENTS.md §3 it is a contributor page — user-facing facts belong
> in the docs pages, and anything released belongs in CHANGELOG.md.
>
> **Not this branch's business:** the configuration model. That landed on
> `main` (`ee8e1a2`, "0.8 configuration model"), before this branch was cut;
> here the config surface only gained the two options this branch's features
> need (`[transport.noise] resume`, `[server.data] stripe_count`) plus tests
> and documentation fixes.

## Next: the theme for the next update

**Theme — "measure the three unmeasured axes of the data path, then fix what
they show": establishment, fragmentation, reconnect.**

Every remaining data-path question in this file is blocked by an instrument,
not by an idea: the rtt100 8-stream cell wedges on *every* arm (so several
A/B verdicts in this file are unquotable), no bench cell exists for a
reconnect (so the session-resume win has no bench-level claim), and loopback
is MTU 65536 (so fragmentation — the one real-world failure mode no molehill
arm handles — cannot be measured at all). Per AGENTS.md §10 the instrument
comes first: a metric without contrast is not a measurement, and for
fragmentation the **cell is the deliverable**.

| # | Deliverable | Gate it must pass | Why this and not something else |
|---|---|---|---|
| 1 | **Establishment** — overlap the data-channel setups (candidate **B**: one batched `CreateDataChannels(n)` command, or N concurrent spawns) | the rtt100 8-stream cell completes a rep instead of `None`, with 1-stream inside its spread | it is the last cell where the engine's own behaviour cannot be seen at all; every earlier link's verdict on that cell was "not attributable" for this reason |
| 2 | **Fragmentation** — PMTU-aware KCP segment size (candidate **D**: `IP_MTU_DISCOVER = DO` + `IP_RECVERR`, shrink-only, never grow) **plus the MTU-1280 shaped cell it needs** | the new cell shows the unfixed build losing roughly half its throughput to fragment-loss amplification and the fixed build recovering it | a 1400 B datagram on an MTU-1280 path is silently fragmented and one lost fragment costs the whole datagram; every TCP arm gets PMTU from the kernel, KCP gets none. D must ship as a pair — DF without the error-queue handler turns a sub-MTU path from "fragmented" into "black hole" |
| 3 | **Reconnect** — a cold-start/reconnect probe: client start → every registered port answering, for an N-service client | the probe reports per-service and total time, with enough repetitions to state a spread | it is the missing gate for the Noise-resume work (setup 442.7 → 38.5 µs per pair, currently only an in-process probe) *and* for the single-control-channel consolidation below; without it that change "ships with its probe or not at all" |

Not in this theme, recorded so they are not re-litigated: candidates **A**
(state-aware channel placement) and **C** (UDP drop counter first, load-aware
assignment second) — both real, neither blocked by an instrument, so they wait
until the three above have their numbers. The single-control-channel
consolidation is a *later* theme: its design is scoped below, and deliverable 3
is what gates it.

### Phase log (this session)

The theme above is being executed in phases; each lands as one commit with its
own measurement, and the harness changes it needed are recorded with it.

| Phase | State | Evidence |
|---|---|---|
| 1 — UDP drop counters (`MOLEHILL_UDP_STATS=1`) | **landed** (`38b7acd`) | routing returns `UdpRouteOutcome`; unit-tested without racing on the globals |
| 2 — the fragmentation axis in the harness (`mtu1280`, `loss1_mtu1280`) | **landed with the D commit** | the classes are `PathClass{netem, mtu}`; the shaper applies `ip link set lo mtu` and **verifies the restore**; `restore_stale_mtu` at startup cleans up after a SIGKILLed run; `meta.mtu_restore_to` records the interface's starting MTU |
| 3 — D: PMTU-aware KCP | **gate met** | alternating parent/child `cost` runs on `loss1_mtu1280` (kcp4, 1 stream, 3 pairs): **A 0.000/0.000/0.000 vs B 0.303/0.294/0.369 Gbit/s**; control cell `loss1` (same loss, MTU 65536) overlaps (A 0.313/0.320/0.316, B 0.311/0.399/0.348), so the change costs nothing where it does not apply |
| 4 — B: parallel establishment | **not shipped — no measurable effect** | see below |
| 5 — reconnect probe (`--test=reconnect`) | **landed** | ~154 ms clean cold start; A/B interleaved, five reps per build |
| 6-7 — docs, release sweep, gate | **done; gate green** | `results-soak-v0.9.0.json` on `v0.8.1-122-g710186c`, `tree_clean: true`, binary fingerprint recorded and not stale, four tools × 8 stages, `soak-check`: "OK: no gate violation" |

The sweep's first two attempts are worth recording, because both were caught by
the harness rather than by reading it:

- **nps failed outright** with `EXDEV` — its shipped conf files were hard-linked
  from a peer cache on another filesystem, while the binaries' own links had a
  fallback. Fixed in `edaee3c` (copy when a link is impossible).
- **the gate failed on one clean stage**: molehill recorded 2 interactive errors
  in 2912 samples (0.069%). The same run measured frp 0, rathole 0.05 % and nps
  0.31 % on their clean stages — so an absolute zero-error SLO was flagging the
  middle of the host's own spread. The rule became a 0.5 % rate (`710186c`), and
  the artifact was re-measured under it rather than judged by the stricter rule
  that happened to be in force when it ran.
- **two false starts on the command line**: the ritual's documented invocation
  omitted `--test=rrul`, and the default is `capacity`, so a four-minute ceiling
  probe was written to the release path and looked like a release artifact
  (`f8f5e37` fixes the page). The same shape of mistake — a binary built before
  a revert, claiming the later revision — is now impossible to record silently:
  the meta carries the binary's sha256, size and mtime plus a `stale` flag
  (`553a41d`).

**B's premise is real in the code and costs nothing measurable on today's
instruments.** The tunnel driver held a *single* pending open and a single SYN
announcement, so N concurrent callers were served one SYN round trip at a time —
the serialization the rtt100 wedge was attributed to. Opening them concurrently
(a queue of pending opens, `MAX_OPENS_IN_FLIGHT`) was implemented and measured:

| Metric | Parent | Concurrent opens | Reading |
|---|---|---|---|
| rtt100, 8-stream `cost` bulk | 1.039 / 1.160 Gbit/s | 1.130 / 1.105 Gbit/s | overlapping — and the cell **no longer wedges on either build** |
| rtt100, 26-stream `cost` bulk | 1.512 Gbit/s | 1.336 Gbit/s | no win; steady-state rate does not depend on setup order |
| clean cold start (`reconnect`, 5 reps) | median 0.1540 s | median 0.1532 s | identical |

The reason is structural: streams start flowing as soon as *their own* setup
completes, so serialized setup delays **when stream N starts**, not the rate
once it is running — and cold start never opens data channels at all (they are
opened per visitor connection). So the change was reverted rather than shipped
on a premise, and the patch is parked at `/tmp/b-parallel-opens.patch` (and in
this session's transcript) with its unit-test-free diff.

**What B actually needs is a different instrument**: N visitors connecting
*simultaneously*, timed until all N are established. That is the honest gate for
"parallel establishment", it does not exist yet, and it belongs with the next
theme — the `reconnect` probe landed here measures registration cold start, not
concurrent establishment, so the two are deliberately separate numbers.

Two harness defects were found and fixed on the way, both by running the tool
rather than reading it:

- **`screen` recorded `--path` without applying it**, so a screen run labelled
  `loss1` was clean traffic and its results meta described a path the run never
  had. It now applies the path once, before the interleave (constant for both
  builds — a shape differing between them would be the second variable).
- **The screen verdict chose its metric from step 1**: a cell hostile enough to
  kill the bulk probe on the first step flipped the whole verdict to response
  time while the columns still read like throughput. It now uses throughput
  whenever any step has it, labels the unit, and says so when it falls back.
  On the fragmentation cell this correctly reports "0 usable steps" — the
  interleaved `iperf_burst` probe cannot survive that cell, which is why D's
  gate uses the stage sampler (`--test=cost`) instead.

Known limit of D: the probe is **IPv4-only**. `IPV6_MTU` has no safe wrapper in
this crate's dependencies, and the alternatives were both rejected — `unsafe`
for one `getsockopt` in a crate that denies it, or a blanket clamp to the IPv6
minimum (1280), which would cost throughput on every IPv6 path including the
65536-byte ones. An IPv6 session therefore keeps kernel fragmentation until a
safe wrapper exists. Recorded here as the remaining half.

### Scoping notes carried forward

- **Single control channel per client** (later theme, design already
  reviewed). The client currently keeps one control connection *and* one mux
  tunnel per service; consolidating is mostly plumbing now that the engine is
  stable and gives another order-of-magnitude FD/handshake reduction for
  many-service clients. Two constraints: (a) the control channel stays
  0.8.x-interoperable, so the consolidated form is a **new hello variant**
  (`ControlChannelHelloMulti`) announced per connection — never a silent change
  to the v3 grammar — with a service id added to the commands that need one
  (`CreateDataChannel`, `HeartBeat`) and per-service server state keyed under
  one client session; the auth handshake stays per connection. (b) The bench
  probes all dial the exposed port, so the change is invisible to them — it
  ships with deliverable 3 or not at all. Its crypto half already landed
  separately: the Noise session resume (`resume = true`) removes the
  handshake's DH turns on reconnect, opt-in, with replay and forward-secrecy
  trade-offs documented in docs/transport.md.
- **Scheduling-review candidates** (A, B, C, D) keep the full statement of
  what is static today, why it is believed true and the gate each must pass —
  that table stays in the backlog below, and its B/D rows are this theme's
  first two deliverables.


## Provenance of the published v0.9.0 numbers

The release sweep was measured on `710186c` (`tree_clean: true`, binary
fingerprint recorded, `soak-check` green). The release commit differs from it by
the platform-portability work CI forced (`4b2ea0a`, `82a26e2`): `nix` moved to a
target-scoped dependency, the musl `msg_iovlen` conversion is `cfg`-split, and
the portable half of a datagram batch moved to `transport::dgram` while the
Linux module kept the syscall machinery.

**On the measured platform that is a no-op**, and that is checkable rather than
asserted: every changed line is either `cfg`-gated away from Linux/glibc, or a
verbatim move of a constant or an enum whose shape and values are unchanged
(`BATCH = 32`, `Span` with the same two variants), or the same expression
(`staging.len() - buf.len()`), or a dependency that Linux still resolves
identically. So the published numbers describe the released code on the host
they were measured on; they were not re-measured after a refactor that cannot
move them. A platform-portability change that *did* touch the measured path
would have needed a fresh sweep, and the gate's provenance fields (`revision`,
`tree_clean`, `molehill_bin_fingerprint`) are what make the difference visible.

## Release blocker: the KCP send path is Linux-only (found by the first CI run)

The branch's first CI run (PR #2) failed four platform builds. Three distinct
causes, all invisible to local glibc testing:

1. **`nix` was an unconditional dependency** — every non-Unix target failed to
   compile it. Fixed: declared for `cfg(target_os = "linux")`, probe gated.
2. **`msg_iovlen` is `size_t` on glibc, `c_int` on musl** — the musl build
   failed on the assignment, and the two spellings cannot share one expression
   (a `try_into` is a no-op and a lint error on the wide one). Fixed: the
   conversion is `cfg`-split by `target_env`.
3. **The KCP send batching is Linux-only without a fallback** — `Span`,
   `SendBatch` and `DgramBatch` live behind `#![cfg(target_os = "linux")]`
   while `DatagramOut` and the pump's send arm use them unconditionally, so
   macOS and Windows do not compile. **This is pre-existing**: it came in with
   the send-batching work (`ec9b322`), which was measured on the bench host and
   never built elsewhere. The *receive* path already has a per-datagram
   fallback for other platforms (see the `#[cfg(not(target_os = "linux"))]`
   arm in the ingress task); the send path never got one.

The fix is a bounded port: give the non-Linux build a per-datagram send path
(the behaviour the code had before batching — one channel message per datagram,
`send_to` each), keeping `Span`/`SendBatch` Linux-only. It cannot be verified
locally beyond `cargo check` (macOS and Windows cross-builds stop in `ring`'s
build script for want of a cross C toolchain), so the honest verification is
(a) a temporary inverted `cfg` to type-check the fallback on Linux and (b) the
macOS/Windows CI jobs on the PR.

**Resolution (same session):** the portable half of a datagram batch — the
[`Span`] vocabulary and `BATCH` — moved to `transport::dgram`, which the
non-Linux send arm can see; `udp_batch` keeps only the `recvmmsg`/`sendmmsg`
machinery behind its own `cfg`. Verified three ways: `cargo clippy --all-targets
-- -D warnings` on glibc, `cargo check --target x86_64-unknown-linux-musl`, and
a **scratch copy with `target_os = "linux"` flipped** so the non-Linux branches
(the fallback included) type-check locally — plus the macOS and Windows CI jobs,
which are the only real proof.

**No tag until those jobs are green**: `release.yml` builds every platform, and
shipping a release whose macOS and Windows artifacts cannot compile is not a
release.

## Where things stand

**The data-path rework is complete on its own terms; the release is prepared
and waiting on a benchmark run.**

This session added two measured improvements on top of it — the KCP path-MTU
fix (0.000 → 0.303/0.294/0.369 Gbit/s on the fragmentation cell, no cost
where it does not apply) and the UDP drop counters — plus the fragmentation
axis and the cold-start probe in the harness, and the docs-only path in the
hooks. See "Phase log" below.

- **Cumulative branch-vs-`main` A/B (2026-09-24, the fixed harness):** latency
  at parity or better on every arm and cell; throughput net favourable (15
  claimable cells against 6, of which five are not attributable — the mux
  loopback 1/8-stream cost is the one real regression, and one loss1 CPU cost
  is unisolated); CPU net favourable. kcp4's RSS doubling is the one axis that
  regresses, and the user ruled memory out of the gate. Detail: "Final
  cumulative A/B (2026-09-24)" below.
- **`main` is untouched and releasable.** Nothing here is merged.
- **v0.9.0 is prepared, not tagged:** `## [0.9.0] - 2026-09-25` in the
  changelog, the benchmark assets committed, `githooks/pre-tag` green, and
  `just check` green on the tree. Tagging is a deliberate human act
  (AGENTS.md §5).
- **One open provenance item:** the committed `results-soak-v0.9.0.json`
  predates the harness's provenance record, so `just soak-check` reports 5
  `LEGACY` checks (endpoint invariant, revision) it cannot answer from that
  file. The numbers stand; a re-run with the current harness is the
  recommended way to close it, and the gate prints the count until it is.
- **The measurement model changed under it:** the Soak model replaced the
  matrix (a workload over time under a scripted stage schedule, not one cold
  average per cell). Numbers from the matrix (v0.8.x) are a different
  instrument and never a regression signal against it.

## What landed (each one commit + one single-variable A/B)

| Change | Commit | Verdict |
|---|---|---|
| in-repo yamux 0.14, wire-identical | `92fdde0` | behaviour-identical to the crate |
| tokio-native engine IO (Compat gone) | `20ac557` | the -6.6..+23.5% column of the 2026-09-21 final A/B (the 2026-09-22 re-run shows the mux arm's loopback cells inside its noise floor — see "Final cumulative A/B") |
| stream cap 32 → 64 | `a97e1ef` | ceiling probe: 15 → 47 usable streams |
| control-frame coalescing (L4) | `a1fe0bb` | 8-stream +5.7..9.2%, ranges overlap |
| frame split 16 → 32 KiB | `fda6fd6` | mux1 loopback 8-stream +45.7% non-overlapping (2026-09-21 A/B; the figure did not reproduce in the 2026-09-22 cumulative run — the same cell measured +0.3% with rounds alternating direction — so the default stands on no-regression grounds, not as a proven win) |
| drop finished stream receivers | `a424ccc` | the churn fix above |
| `--ab` interleaved A/B in bench.py | `fc87a6c` | cancel epoch drift between binaries |
| framing counters in the bench | `efbcac4` | frames/s + cpu-per-frame per arm |
| A/B verdict tool (`ab_compare.py`) | `040edbe` | encodes the §10 claim rule |
| arm watchdog + lock diagnostics | `040edbe` | a hung arm no longer blocks the matrix |
| peers: bore → nps 0.26.10 | `79855a1` | frp/rathole/nps; all three carry every probe |
| bench: un-invert churn direction | `9f27685` | verdict-table contract fixed before a spread exists |
| bench: semantic results-file order | `8a2c07c` | v0.10.0 sorted before v0.8.0 in the gate/plot |
| bench: udp ping quiescence + meta | `40d54ad` | same numbers, ~7 s less wall per weak arm |
| bench: checkpoint dir recreated | `baa4eab` | a wiped output path no longer kills a live run |
| bench: RSS key that exists | `675be8a` | `total_kb` never matched; the memory axis was missing from every A/B verdict |
| bench: framing counters go missing loudly | this commit | a framed arm with zero mux-stats lines now records a typed partial_metric instead of silently dropping the attribution column (the 2026-09-22 final A/B lost it that way — see below) |
| bench/doc hygiene (phase-2 review) | `7fb0e39` `b36ba03` `0a06123` | doc-example test covers all 4 markdown files; bore→nps + 5 stale references fixed; the three non-reproducing A/B figures annotated; dead `PaceState.rtt_ms`, 2 stale lint waivers removed — behaviour-neutral (KCP smoke inside the baseline spread) |
| data-channel striping (K data channels per visitor) | `f91e6bb` | the prototype + its A/B: loopback 1-stream **+48.7% non-overlapping** (10.73 → 15.96 Gbit/s), 8-stream parity, churn -7.7% median-only, CPU +40.8% median-only — "Stripe A/B (K=4)" |
| bench: `--ab` spawns the binary it is given | `064c55a` | harness bug fix: every earlier `--ab` run compared the default binary against itself — "The `--ab` harness bug" |
| noise setup-cost phase probe | `7b19457` | the DH turns are ~97% of the ~445 us a handshake pair costs — the input for the resume design |
| Noise session resume (opt-in) | this commit | setup 442.7 -> 38.5 us per pair (-91%), release-mode probe; full suite + the resumed integration scenario green |
| KCP path attribution counters (`MOLEHILL_KCP_STATS`) | `66fb0d2` | the per-segment phase table the kcp4 optimization route was planned against: 5.63 µs/sent segment (sender), 1.68 µs/received segment (receiver), ~81% of the sender's CPU attributed — "KCP attribution (Phase 0)" |
| KCP send batching + receive coalescing | `ec9b322` | one channel message per ~32 outbound datagrams, one per ~16 KiB inbound: **+6.6% loopback 1-stream, +7.6% loopback 64-stream, +9.3% loss1_rtt10 8-stream (all non-overlapping)**, CPU -7.2% / cpu-per-kframe -20.7% median-only; RSS +73% median-only is the open cost — "Phase 1 A/B" |
| bench: `--ab` labels unique per binary | `2754142` | two worktree builds share the basename `molehill`; a colliding suffix made the interleave's sides overwrite each other and `ab_compare` see no pair |
| bench: record the label↔path mapping | `5d1d9dd` | `meta.ab_bin_paths` in every `--ab` file + printed by the verdict tool, after a verdict read with the sides swapped briefly reverted a winning change |

## Optimization route: closed

Every candidate was measured rather than argued. The record for each:

| Candidate | Outcome | Evidence |
|---|---|---|
| L0 Compat shim, L1 split size, L2 cap, L4 coalescing | **landed** | table above |
| L3 window/streams decoupling | **parked — premise refuted** | 4× the connection window moved nothing on any cell; the rtt100 ceiling is outside the engine (mux-off gives the same number) |
| L5 frame-body pool | **dropped** | cpu/frame identical to within 1% on every cell |
| explicit `SO_RCVBUF`/`SO_SNDBUF` | **dropped — refuted** | a fixed 8 MiB cost -26% on rtt100 1-stream; kernel auto-tuning wins |
| UDP sendq 1024 → 128 | **dropped** | no cell moved; RSS +2..12% the wrong way |
| write-path body copy | **architecturally required** | a frame must own its body to cross the command channel |
| churn cost blamed on the mpsc swap | **wrong diagnosis** | it was the leaked receivers; the swap is innocent |
| KCP pacer recovery 1.05/4 → 1.25/2 | **dropped — re-measured 2026-09-22** | no claimable gain on loopback/loss1/loss5; udp jitter and loss medians moved the wrong way (`results-opt-kcp-pacer.json`) |
| KCP ARQ windows 2048/4096 → 4096/8192 | **dropped — re-measured, refuted again** | loss5_rtt100 1-stream **-52% non-overlapping**; no gain anywhere (`results-opt-kcp-w4096.json`) |
| KCP flush interval 10 → 5 ms | **dropped — re-measured, old verdict holds** | 8-stream -11% loopback / -8% loss1, both non-overlapping; 1-stream identical (`results-opt-kcp-int5.json`) |

**The two things left are not single-variable optimizations**, recorded
rather than attempted:

1. **Per-frame cost scales with tunnel count** — ~35% lower on one tunnel
   than on four (0.0248 vs 0.0395 cpu%/kframe) at the same frame size and
   rate. So the residual mux-vs-mux-off gap is driver scheduling, not
   per-frame fixed work; reducing it means changing the multi-tunnel
   structure, and the direct mode already wins those cells.
   *Update (2026-09-23):* the stripe A/B measures the mechanism behind
   this — cpu/kframe halves on the striped arm (0.051 → 0.031) because
   one connection's frames now spread over four drivers. The structural
   fix is "spread each connection's frames", which `stripe_count` now
   does from configuration.
2. **The rtt100 ceiling is the host's** — that cell needs ~50 MB of in-flight
   window while the kernel caps a socket at 32 MB, and the mux window is
   already 64 MB. Raising it needs `SO_RCVBUFFORCE` privileges a normal
   deployment lacks.
   *Update (2026-09-23):* `stripe_count = K` multiplies the in-flight
   window by K without any privilege (K streams × the engine window), so
   the shaped-cell ceiling is now also addressable from configuration;
   the rtt100 cell itself is untested by the stripe A/B (loopback only)
   and is the first follow-up measurement if the feature is enabled.

## Backlog (gated candidates, open items, closed items)

### From the 2026-09-24 scheduling review (candidates — none has an A/B yet)

A code-level pass over the *scheduling decision points* — the places where the
code picks one of several places to put work with no input from the state of
the candidates — plus the MTU thread that came out of the same review. Recorded
here as candidates with the evidence that motivates each and the gate each
must pass; nothing below is planned work until its gate is met.

| # | Direction | What is static today | Why it is believed true | Gate |
|---|---|---|---|---|
| B | parallelize data-channel establishment | `CreateDataChannel` is one command per channel and the client spawns one `open_stream()` per command (`core/client.rs`, `transport/multiplex.rs`), so N channels cost N serial control-channel round trips | Phase 3 attributed the rtt100 8-stream wedge to "one session serializing eight yamux stream setups over a 100 ms path (S1)"; both binaries wedge there, so the cell carries no claim for either engine today | the rtt100 8-stream cell produces a completed rep instead of `None` on the mux arm, with 1-stream inside its spread |
| A | state-aware placement of data channels | `TunnelPool::open_stream` is an atomic round-robin; the server's TCP pool hands channels FIFO; UDP assignment round-robins over workers and drops on a full queue | two measured effects pull opposite ways for placement — cpu/kframe rises with tunnel count (0.0248 → 0.0395, "driver scheduling") while striping *halves* cpu/kframe by spreading frames over more drivers (+48.7% 1-stream). Round-robin resolves the conflict by ignoring it; the weights (yamux send credit, queue depth, an RTT sample) already exist | the mixed-workload probe (bulk + interactive through two services of one client) and churn first-byte p99 move favourably, no throughput cell regresses |
| C | make the UDP drop visible, then load-aware | `route_udp_datagram` drops on a full worker queue by design; nothing counts the drops | the UDP probe is one light session, so the cost of that drop is *unmeasured* — per §10 there is no claim in either direction today | step 1 (a drop counter) is free and lands alone; step 2 (assign by queue depth, or spill over instead of dropping) only after the counter shows drops under a mixed UDP load |
| D | PMTU-aware KCP segment size — shrink-only | the KCP socket sets only `SO_RCVBUF`/`SO_SNDBUF`: no DF, no error queue (`src/transport/kcp.rs`) | on an MTU < 1400 path a 1400 B datagram is silently IP-fragmented and one lost fragment costs the whole datagram — a 1% fragment loss is ~2% datagram loss. Every TCP arm gets PMTU handling from the kernel (default DF); KCP gets none | a new bench cell shaped at MTU 1280 (`ip link set lo mtu` inside the cell's netem on/off lifecycle, the same place the qdisc is managed) shows the unfixed build losing roughly half its throughput to fragmentation amplification and the fixed build recovering it |

Shapes, if a gate is met: **B** = a batched `CreateDataChannels(n)` command (or N
concurrent spawns) so the stream opens overlap instead of queueing; **A** = one
weight per channel, three consumers (client tunnel placement, server pool
hand-out, UDP worker assignment); **C** = a relaxed counter first; **D** =
`IP_MTU_DISCOVER = DO` + `IP_RECVERR`, consumed by the session's existing
dispatch loop, lowering the engine's `mss` once on `EMSGSIZE` — never growing
it (a one-session-start probe may grow). D must ship as a pair: DF without the
error-queue handler turns a sub-MTU path from "fragmented" into "black hole",
which is worse. Note D's only claimable cell does not exist yet — loopback is
MTU 65536, so the current matrix cannot measure fragmentation amplification at
all; the cell *is* the deliverable.

**Multi-session pump consolidation** (one dispatcher driving N KCP sessions,
sharing the timer/update path) is **evaluated and closed — the amortization
premise is refuted by the re-based attribution**: of the 322 µs per pump round
only 1.26 µs (update 0.9 + deliver 0.36) is session-independent; the other
320 µs is data-driven and cannot be shared. 3002 rounds/s × 1.26 µs × ¾ ≈
**0.5% of one core**, against the kcp4 arm's ±7% rep spread — a claim that
cannot be built inside the noise, the fourth such candidate after the L5
pool, L3 window/stream decoupling and the pacer recovery study. The one axis
that survives is not amortization but *cross-session latency isolation*: a
bulk writer on one session can delay another session's ack flush and escalate
the peer's RTO (the failure mode `pump_tail`'s design notes name). That needs a
probe showing cross-session interference first, and its natural terrain is the
rtt100 8-stream cell — which is also direction B. Re-open only if that cell is
still anomalous after B lands; the striping result (cpu/kframe *halves* when
frames spread over more driver tasks) is evidence against consolidation on the
throughput axis.

### Open items

- [ ] HTTP API for configuration (hot reload is files-only today)
- [ ] Replace the python bench/test entries with `cargo-script` once it is
      stable — until then `uv run` stays the entry
- [ ] QUIC transport on main: implemented and measured, parked in the
      `archive/transport-test` tag (N×TCP won every comparable cell, and the
      QUIC leg lacks peer auth). Revisit if a UDP-only path or multi-stream
      loss isolation becomes a requirement.

### Closed in the 2026-09-22 session (with measurements)

- [x] **KCP pacer slow-recovery study** — closed by measurement, not by
      argument. The backlog's premise (recovery too slow, the rate pins low
      for minutes) was tested as a single-variable A/B with the current
      bench: pacer up-factor 1.05 → 1.25 and the clean-PONG threshold 4 → 2,
      plus not counting a PING that never reached the kernel (a full socket
      buffer is a local condition, not a path signal) as a congestion cut.
      No claimable gain on loopback / loss1_rtt10 / loss5_rtt100 — every
      throughput movement sits inside its rep spread — and the UDP echo
      quality medians (loss 4.0 vs 2.5%, jitter 1.68 vs 1.48 ms on loss1)
      moved the wrong way. The pacing rate is not the binding constraint in
      any cell this bench measures; the item is closed, not parked
      (`results-opt-kcp-pacer.json`).
- [x] **KCP flush interval 5 ms** — closed by re-measurement. The old
      "-3x on loopback 8-stream" verdict predates the in-repo engine's
      interval clamp (which floors any interval below 10 ms back to 10), so
      it described a code state that no longer expresses the variable. With
      the clamp lowered and the new bench: 8-stream **-11% loopback and -8%
      loss1_rtt10, both non-overlapping**, 1-stream identical, CPU +6%. The
      old verdict holds; the historical magnitude is explained by the
      loopback 8-stream bimodality now visible per rep (rep0 often lands at
      0.4-0.6 Gbit/s while later reps reach 2-3.2 — a cold-start mode a
      1-rep pre-revision run could sample alone)
      (`results-opt-kcp-int5.json`).
- [x] **KCP ARQ windows 2048/4096 → 4096/8192** — closed by re-measurement
      of the pre-revision "collapse" finding: **loss5_rtt100 1-stream -52%
      non-overlapping** (0.029 vs 0.045 Gbit/s), loss1_rtt10 8-stream -2.5%
      non-overlapping, no gain on any cell, CPU +18% on loopback. Bigger
      windows burst past the shared listener socket buffer with four
      sessions and the loss escalates into RTO backoff — the mechanism
      recorded pre-revision, reproduced with the current bench
      (`results-opt-kcp-w4096.json`).

### Deliberately not doing

- Zero-copy splice/sendfile: measured, not recommended (keep as-is).
- Tracing span gating: the backlog asked for it "if profiling shows
  overhead"; no profiler on this host, and the framing counters show the
  per-frame cost is dominated by work a level-filtered macro does not touch.
- Upstream tracking of rust-yamux fixes: no tracking; the in-repo engine is
  maintained for molehill's own scenarios only.

## How to read the historical records below (and why they are not evidence)

Everything measured before the Soak model — the per-cell matrix and every
`--ab` run taken with it — was produced by an instrument that has since been
proven wrong in ways that were not visible at the time: the `--ab` interleave
spawned one binary on both sides for an unknown stretch, the verdict tool could
be read with the sides swapped, the memory axis read a key that never existed,
and several headline figures never reproduced when re-measured. Those files are
gone with the matrix (its runner, charts and results were deleted in the
v0.9.0 cycle).

So the records kept below are **historical context, not evidence**: they say
what the branch's authors believed at the time and why a decision was taken,
and they are useful for exactly that. No number in them may be quoted as a
measurement of the current code, compared against a Soak result, or used to
gate anything. Where a claim is still load-bearing for the next theme (the
rtt100 8-stream wedge's attribution, the absence of an MTU cell, the
striping and resume results), the paragraph says so explicitly and names the
gate that re-establishes it. Everything measured from the Soak model onward
carries its own provenance in the results meta (`revision`, `endpoints`,
`workload_version`) and is the only comparable material.

The release notes for the work summarised below are in
[CHANGELOG.md](CHANGELOG.md); this file no longer duplicates them.

## Transport layer: what was compared, and where KCP stands

Four carriers were implemented, integration-tested end to end and measured against each other (the QUIC arm's history survives in the local `archive/transport-test` tag):

| Arm | Topology | Status |
|---|---|---|
| 0 | 1 TCP tunnel | `count = 1` |
| 1 | N TCP tunnels (bench N=4) | **default (`count = 4`)** |
| 2 | N KCP sessions (bench N=4) | merged, optional (`carrier = "kcp"`) |
| 3 | 1 QUIC connection (quinn) | **archived** — behind on loss cells (quinn "too many gaps" at rtt10), no peer auth on the QUIC leg, N×TCP measured better in every comparable cell |

**KCP's measured position (v0.8.0 matrix figures):** it loses throughput to a TCP carrier in *every* cell (loopback ~2.5 vs ~11 Gbit/s 1-stream; loss5/rtt100 ~0.03 vs ~0.28) and wins the latency axis on the worst cell (udp p50 601 vs 826 ms, HoL max gap 1515 vs 1808 ms over the noise-TCP arm). Verdict: **not an optimization direction** — a capability with a defensible niche: UDP-only paths (TCP blocked or throttled by firewall/NAT) plus latency-first interactive traffic on high-loss, high-RTT links. The section below re-measures the same cells with the current bench on a second host.

### KCP on the current bench (2026-09-22, host `1cb438346ebe`)

The v0.8.0 matrix is the last full KCP characterization and predates the bench's measurement revision; the `results-opt-*` KCP files are from 2026-09-08, pre-revision, and two of them were taken on *different hosts* (so the "+29% window doubling" was a cross-host pair — invalid per §10). Re-measured with the current instrument (`results-opt-kcp-baseline.json`, 2 reps × 2 rounds; the experiment files carry 3 × 3). Noise arm beside it, same cells:

| cell | kcp4 1-str | noise 1-str | kcp4 8-str | noise 8-str | kcp4 udp p50 | kcp4 loss% | noise loss% | kcp4 RSS | noise RSS |
|---|---|---|---|---|---|---|---|---|---|
| loopback | 3.1-3.7 | 6.1-6.8 | 1.4-6.3 (bimodal) | 20-21 | 0.43 ms | 0 | 0 | 95-182 MiB | 29 MiB |
| loss1_rtt10 | 0.46 | 3.80 | 0.69-0.79 | 12.9 | 60.6 ms | 2.5-4.0 | 5.0 | 78-155 MiB | 38 MiB |
| rtt100 | 0.011-0.057 | 0.667 | wedge (None) | 1.06 | 600 ms | 0 | 0 | 28-54 MiB | 17 MiB |
| loss5_rtt100 | 0.038-0.057 | — | None | — | 610-704 ms | 16.5-21.5 | — | 47-54 MiB | — |

1. **The UDP echo path is *better* over KCP on loss cells** — loss1_rtt10 2.5-4.0% lost vs noise's 5.0%, max gap 55-60 ms vs 80 ms, jitter 1.16 vs 1.32 ms. KCP's niche measured, not argued.
2. **rtt100 is where KCP falls over**: 0.011-0.057 Gbit/s 1-stream (7× rep spread) and the 8-stream test times out in *every* round — the v0.8.0 matrix recorded the same wedge, so it is not new, but the cell cannot carry a claim for either engine.
3. **The loopback 8-stream cell is bimodal within one arm** (rep0 often 0.4-0.6 Gbit/s, later reps 2-3.2): a cold-start mode; this is what made the pre-revision "-3x" reading possible, and why no KCP loopback 8-stream number above is quotable as a median.

The three optimization attempts this session (pacer recovery, doubled windows, 5 ms interval) are all closed with measurements in the backlog section; files `results-opt-kcp-{pacer,w4096,int5}.json`.

### KCP attribution (Phase 0, 2026-09-23, host `0d…`)

The counters are now an instrument: `MOLEHILL_KCP_STATS=1` makes every molehill process log a per-second `kcp-stats` line (datagrams in/out, retransmits, acks and SACKs sent, pump rounds, coarse per-phase milliseconds — input / deliver / writer / output / update; commit `perf(kcp): attribution counters`). One loopback kcp4 run with the bench (1 rep × 8 s, both processes instrumented; `~/tmp/results-kcp-stats.json`, arm logs `molehill_kcp4_loopback.{client,server}.log`) measured 3.14 / 2.63 / 7.44 Gbit/s (1/8/64-stream — inside the 2026-09-22 baseline's ranges) and attributes the pump's per-segment cost in the 1-stream window (~295.8 K segments/s each way, **zero** retransmits):

| side | per segment | input | deliver | writer | output | update |
|---|---|---|---|---|---|---|
| sender (bulk out) | **5.63 µs** | 0.08 | 2.46 | 0.62 | 2.46 | 0.01 |
| receiver (bulk in) | **1.68 µs** | 0.41 | 0.77 | 0.00 | 0.49 | 0.01 |

All figures are **wall-clock** per pump phase (the timers wrap the phase's awaits, so `output` includes the pacer's writable park and `deliver` any descheduling). Whole-run check against process CPU: the sender's five phases sum to 7.95 µs per sent segment against 9.8 µs of measured CPU per sent segment (181.9% avg × 82 s / 15.19 M segments) — the phases explain ~81% of the sender's CPU, clearing the ≥70% attribution bar; the receiver's remainder is the Noise/yamux/iperf stack.

1. **The sender's cost is the wire drain + the writer, not the ARQ update** (0.01 µs/segment). *Corrected 2026-09-24 — the original reading attributed ~2.46 µs per sent segment to delivery and called it an anomaly; that was an instrument bug (the deliver timer's binding outlived its statement and booked the rest of the pump round into the phase). The re-based table is in "Zero-copy route: the attribution re-baseline" below: with the fix, the sender's delivery phase is 0.36 µs/round and the wire drain 2.55 µs/segment — the wire drain is the cost, and Phase 1's send batching plus L2/L3 attack it.* `drain_dgrams`'s per-datagram pacer check + `sendmmsg` batching is the other half.
2. **A loss signal the pacer never sees.** The 64-stream window shows the sender's retransmit rate climbing 2.4% → 34% while the out-rate decays 638 K → 150 K segments/s, while the pacer's only cut signal remains the 2.5 s PONG timeout; the 1- and 8-stream cells retransmit nothing. Whether the drops are kernel socket-buffer overflow (both sockets request 32 MiB, granted by this host's `rmem_max`) or the pacer's own token denials is not yet attributed.
3. **The counters are zero-cost enough to leave in the bench**: the same run's arm totals (CPU 181.9% server / 154.8% client) sit inside the 2026-09-22 baseline's band, and the line format is additive.

### Phase 1 A/B (2026-09-23): send batching + receive coalescing — LANDED

Commit `a38bf4b`, re-landed as `ec9b322` after the misread revert below. Outbound datagrams stage in a reusable ~46 KiB buffer and cross the pump channel as ONE message per batch (closed at 32 datagrams, the staging cap, or the engine's flush boundary — the engine's `flush` now calls `Output::flush`); the reader side coalesces consecutive segments into one channel message per ~16 KiB. Pure amortization: same datagrams, byte stream and ARQ semantics; a pacer denial drops only that span. The new `blobs_out` counter: **7.8-11.1 segments per reader message** (was 1), per-segment phase cost sender 5.92 → 5.59 µs, receiver 1.78 → 1.71 µs (1-stream window).

One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms kcp4 + the loopback mux-off control, binaries `66fb0d2` vs `a38bf4b`, both SHAs verified; data `results-ab-kcp-batch.json`: 0 cell errors, one transient iperf3 hole (ab2 head / loopback `mixed bulk_gbps`, control socket closed — one metric of one arm, typed reason recorded), the rtt100 8-stream gap documented on both sides):

| arm / cell | metric | parent | batch | verdict (batch vs parent) |
|---|---|---|---|---|
| kcp4 loopback | 1-stream | 3.156 | **3.363** | **CLAIM +6.6% (non-overlapping)** |
| kcp4 loopback | 8-stream | 2.128 | 1.290 | -39% non-overlapping — the known BIMODAL cell (0.4-3.2 Gbit/s modes), unquotable per the 2026-09-22 record |
| kcp4 loopback | 64-stream | 6.402 | **6.888** | **CLAIM +7.6% (non-overlapping)** |
| kcp4 loss1_rtt10 | 1-stream | 0.428 | 0.433 | inside spread |
| kcp4 loss1_rtt10 | 8-stream | 0.750 | **0.820** | **CLAIM +9.3% (non-overlapping)** |
| kcp4 rtt100 | 1-stream | 0.096 | 0.083 | inside spread (parent ahead 15.1%) |
| kcp4 rtt100 | 8-stream | None (wedge) | None (wedge) | still wedged on BOTH binaries |
| kcp4 loss1_rtt10 | cpu/kframe | 0.111 | 0.088 | median-only -20.7% (better) |
| kcp4 loopback | CPU | 243.2% | 225.7% | median-only -7.2% (better) |
| kcp4 loopback | RSS | 122.0 MiB | 211.8 MiB | median-only +73% (worse — the coalesced-blob channel residency, bounded at ~32 MiB under a full reader stall; the measured jump is larger than that bound explains and is the one open question on this change) |

The gate ("a non-overlapping positive on cpu/segment or 1-stream counts as a win") is met on the 1-stream and 64-stream cells with CPU and cpu/kframe favourable; the only non-overlapping unfavourable is the bimodal 8-stream cell, which the 2026-09-22 record already declares unquotable. **Correction (2026-09-25 audit):** the sentence that used to follow claimed the loopback 1-stream claim "exceeds" the mux-off control's +12.1% ordering floor — it does not (the claim is +6.6%), so **that throughput claim is not attributable** and the change stands on the mechanism (`ec9b322`'s coalescing is covered by tests), the CPU and cpu/kframe readings, and the absence of an attributable regression. The run's own provenance is also uncertain — see "How to read the historical records below".

**The misread revert (recorded because it cost a day):** this A/B was first read with the two binaries swapped (`ab_compare` sorts the pair by label, and the two worktree binaries share a basename, so the labels are content hashes — nothing tied them back to the `--ab` command line). The +6.6% 1-stream win was read as a -6.2% regression and the change was reverted (`ab5eb2c`); the mapping was caught when the Phase 2 A/B's parent arm measured a 200× collapse that the "parent" label could not explain. bench.py now writes `meta.ab_bin_paths` (label → resolved path) into every `--ab` file and `ab_compare` prints it before the table (`5d1d9dd`); the change was then re-landed by reverting the revert. The data file never changed — only the reading.

### Phase 2 A/B (2026-09-23): ARQ loss-event pacer — DROPPED

`eb4a491` fed the engine's retransmit signal into the pacer: a per-session retransmit total (exposed as `Kcp::retransmits`) cuts the rate 0.75× per loss event, cooled to once per RTT (min 100 ms), with SACK notification cutting through the same path. Its A/B (`eb4a491` vs the same parent, `results-ab-kcp-pacer.json`) plus a focused one-round re-measurement (`results-ab-kcp-loss1-focus.json`, both binaries, same epochs), read with the corrected mapping, shows the change **breaks the cells it targeted**:

- loss1_rtt10 1-stream: **0.0019 vs 0.4325 Gbit/s** (head vs parent) — a 200× collapse; the 8-stream test fails outright (iperf3 control socket closed). The head's loopback 8/64-stream cells time out in all three rounds of the full run.
- Mechanism: the timescale mismatch the plan flagged — on a 1%-loss path the retransmit stream is continuous, so the cooled cut fires every RTT/100 ms and walks the rate to `PACER_MIN` within about a second, while the 1.05×-per-4-clean-PONG recovery needs minutes. The cooldown bounds cut *frequency*; nothing bounds cut *depth* over a sustained loss stream.
- Where there is no loss (loopback 1-stream, zero retransmits) the new path never fires — the cell is inside spread.

The head's low CPU/RSS are the mirror of the same fact: its tests stalled. Reverted (`bea6317`). **Closes the S3 route with evidence**: an ARQ-driven cut cannot work while recovery stays on the PONG-probe path. A design that could — cut toward a remembered goodput with an ACK-clocked multiplicative recovery — is recorded, not attempted; the same conclusion the closed pacer-recovery experiment reached from the other side ("the pacing rate is not the binding constraint in any cell this bench measures").

### Phase 3 (2026-09-23): the rtt100 8-stream wedge, attributed

With the ARQ-pacer route closed and the cell still wedging (None on BOTH binaries in the Phase 1 A/B; the 2026-09-22 re-measurement and the v0.8.0 matrix recorded the same), it was re-measured focused with the Phase 0 counters: `focused_run.py --variant kcp4 --cell 0/100 --streams 1,8 --secs 10 --reps 3`, binary `ec9b322`, data `results-kcp-rtt100-focus.json` (raw per-rep iperf3 JSON kept; the arm logs' `kcp-stats`/`mux-stats` lines are the attribution source).

- **1-stream works**: three reps sent 0.096 / 0.089 / 0.027 Gbit/s (received-own-window 0.04 / 0.037 / 0.0074). The *received* figures are the ones comparable with the baseline's 0.011-0.057 spread, and rep 2's 0.0074 sits just below it; the sent column is the sender's post-`-O` accounting, which the baseline table does not quote. The cell's own retransmit rate is 25-60% at this rate (the ARQ at a 100 ms RTT is the long-known weak point), but the cell completes.
- **8-stream fails all three reps**: rep0 genuinely (iperf3 "control socket has closed unexpectedly" after 8.4 s, 0 bytes sent and received), and reps 1-2 then hit "the server is busy" — the wedged single-test iperf3 backend §10 warns about. The wedge itself reproduces; the rep1/2 nulls are instrument contamination, recorded as such.

**Attribution (the counters settle it):** during the 8-stream window the KCP pumps are IDLE — the sender emits 0-6 datagrams/s at the ~1200 rounds/s idle cadence, no retransmit storm, no send-queue buildup — while the mux framing counters show control frames crossing at the 100 ms RTT cadence. The brief retransmit-heavy burst at the end (7.5 K retransmits/s, 50%+ of the out-rate) is the sessions dying, not the cause. So the wedge is **not** the ARQ or the pacer — it is the stream/data-path ESTABLISHMENT layer above KCP: one session serializing eight yamux stream setups over a 100 ms path (S1), consistent with the mux route's ceiling finding ("per-frame cost scales with tunnel count"; count=4 is the largest structure this arm was measured at).

**Conclusion — recorded as a known structural limit:** the kcp4 arm's rtt100 8-stream cell cannot carry a claim for either engine and is not worth further investment from this route; a fix belongs to the multi-stream scheduling structure, not the KCP data path. The cell stays documented as None-with-reason in future runs, and the iperf3-backend restart discipline (§10) is why rep1/2's nulls must not be quoted.

## Zero-copy route: the full-path copy map (2026-09-24)

**Route closed 2026-09-24.** Every link resolved: three landed (L1/L2/L3 on the kcp4 arm), one landed on mechanism (S1, opt-in), one reverted after its A/B failed the gate (M1), one parked with its premise refuted (N1). The landed set is confirmed against `main` by the final cumulative A/B below — latency at parity, throughput net-positive on all four arms, CPU net-positive.

Headline from the code-level review (userspace copies per application byte, all arms): **the copy burden is asymmetric — the TCP arm pays W2/W4/K1-K4 inside the kernel, while the kcp4 arm pays 4-5 extra userspace copies per byte (write: 4, read: 2-3) in molehill's own code.**

### The map (per app byte; "yes" = removable in this program)

| # | copy | where | removable |
|---|---|---|---|
| W1 | kernel → `copy_bidirectional_with_sizes` stack buffer | src/core/server.rs:1260 | no (syscall; that buffer is also the write buffer) |
| W2 | stack buffer → yamux frame body (`Vec::from(&buf[..k])`) | src/mux/connection/stream.rs:365 | **yes — OPEN: M1 attempted it and was reverted after its A/B (see "Link M1 A/B")** |
| W3 | frame body → Noise record ciphertext | src/transport/noise_stream.rs:447 | no (AEAD; ciphertext must be contiguous with the 2 B length header) |
| W4 | ciphertext → kernel | tokio | no (syscall) |
| K1 | record → `Bytes::copy_from_slice` for the out_tx channel | src/transport/kcp.rs (`KcpStream::poll_write`) | **yes (L3)** |
| K2 | app data → segment data (`send()`) | src/kcp.rs:549 | **yes (L3, deepened)** |
| K3 | segment → engine `self.buf` (encode) | src/kcp.rs:197 | **yes (L2)** |
| K4 | datagram → DatagramOut batch staging | src/transport/kcp.rs (`DatagramOut::write`) | **yes (L2)** |
| R1 | kernel → Noise `bufs.scratch` (record accumulation) | src/transport/noise_stream.rs | no (record framing needs the length header first) |
| R2 | the decrypt's ciphertext read (the `bufs.payload` staging copy was already removed by the e463391 fast path) | noise_stream.rs (incl. the e463391 fast path) | **no — parked with N1, premise refuted: see "Link N1"** |
| R3 | mux frame body → stream read buffer | src/mux/connection.rs (`into_body`) | no (already a move) |
| R4 | stream buffer → `copy_bidirectional` stack buffer | tokio | no (that copy IS the socket write) |
| K5 | recvmmsg batch buffer → owned `Bytes` per datagram | src/transport/kcp.rs (ingress) | no (batch buffers are reused; per-datagram allocation would be worse) |
| K6 | datagram → segment data (`input()` parse) | src/kcp.rs | no (ownership copy; input buffers are transient) |
| K7 | segment → `recv_buf` | src/transport/kcp.rs (`deliver_recv`) | **yes (L1)** |
| K8 | `recv_buf` → coalesce blob | src/transport/kcp.rs (`deliver_recv`) | **yes (L1)** |
| S1 | read chunk → stripe frame body | src/stripe.rs:170 | **yes — done (S1 landed): the read buffer becomes the frame** |

Not attempted (recorded so it is not re-litigated): the syscall boundaries; the write-path AEAD copy (W3); splice/sendfile (already measured and not recommended); io_uring / UDP `MSG_ZEROCOPY` (1.4 KiB datagrams are below the threshold and conflict with the batch design); the L5 frame-body pool (dropped by measurement — cpu/frame identical — note the reverted M1 would have removed the COPY, which L5 did not); the QUIC-style segment ring-buffer (the rewrite the KCP plan excludes).

### Sequence (one commit + one single-variable A/B per link)

| link | removes | arms affected | status / risk |
|---|---|---|---|
| **L1** KCP receive owned-segment (`recv_owned`, `ReadBatch { parts: Vec<Bytes> }`) | K7+K8 | kcp4 | landed on mechanism (low risk) |
| **L2** KCP send two-iovec datagrams (header in staging + payload by reference, `msg_iovlen = 2`) | K3+K4 | kcp4 | landed on mechanism (medium: engine Output boundary + sendmmsg) |
| **L3** KCP write owned records (Noise produces owned records; `KcpStream` owned write; deepened to `send_owned`) | K1+K2 | kcp4 | landed (medium) |
| ~~**M1** mux frame body owned write (`Frame.body` Vec→Bytes + owned write API + an owned proxy loop)~~ | ~~W2 + one alloc/frame~~ | ~~all mux arms~~ | **attempted and reverted — failed its A/B gate** ("Link M1 A/B") |
| ~~**N1** Noise in-place record decrypt on the read fast path~~ | ~~R2~~ | ~~noise, kcp4~~ | **parked — premise refuted** ("Link N1") |
| **S1** stripe owned chunk | S1 | stripe (opt-in) | landed on mechanism (low) |

Order: kcp4 first (highest copy density, isolated arm, cleanest single-variable A/B), L2/L3 next (share the engine Output boundary), S1 last (opt-in). N1 was dropped after its read-path audit, M1 after its A/B (four cells failed, with a +25% RSS cost on the default arms). Expected magnitude was honest and small: L1 ≈ 0.2-0.3 µs per received segment (~15% of the receiver's cost, visible at the 8/64-stream and loss cells), L2+L3 ≈ 0.3-0.4 µs per sent segment (~6%, the loopback 1-stream cell is sender-bound and is where a claimable win would show); the ~2-5% CPU the M1/N1 pair was expected to bring to the default arms was not realized by either link. Not a step change; the step change is the excluded QUIC-style rewrite.

### The attribution re-baseline (2026-09-24, loopback kcp4, 1 rep × 8 s)

The deliver-phase split surfaced a **bug in the Phase 0 instrument itself**: `let _t = PhaseTimer::new(&KCP_NS_DELIVER);` is a named binding, and named bindings drop at the end of their SCOPE, not their statement — so the "deliver" timer booked everything from the delivery call to the end of the pump round (SACK check, ack flush, wire drain, liveness tail). The deliver phase is now block-scoped and the split is coherent (the inner timers sum to the phase). Re-based 1-stream window (sender ~296.7 K segments/s out, receiver ~296.6 K in, zero retransmits both sides; four sessions per process):

| side | per segment (µs) | per round (µs) | notes |
|---|---|---|---|
| sender | writer 0.60, **wire drain 2.55**, input 0.09, update 0.01 — pump body ≈ 3.26 | deliver 0.36 (spill 0.064 + recv 0.199), writer 59.7, wire drain 252.4, input 8.9, update 0.9 | rounds/s 3002 (Phase 0 measured 4842 on the pre-Phase-1 code: send batching cut pump rounds ~38%) |
| receiver | input 0.42, **deliver 0.41** (recv loop 0.40 + spill 0.01), output 0.42 (ack send), update 0.01 | input 5.18, deliver 5.03 (spill 0.030 + recv 4.897), output 5.24 | segments per reader message 8.0; recv per delivered segment 0.397 µs; empty probes exactly 1.00/round |

1. **The "sender delivery anomaly" is retired.** The 2.46 µs/segment was the wire drain (now 2.55 µs/segment) plus the round tail, double-booked. With the fix the sender's delivery is 0.36 µs/round — and the receiver's delivery is 5.03 µs/round, ~14× higher per round in the OPPOSITE direction from the artifact, because the receiver delivers 296 K segments/s against the sender's 283. No asymmetry to chase.
2. **The targets re-aim.** The sender's cost is the wire drain (output: 2.55 µs/segment) and the writer (0.60 µs/segment, which contains the K3/K4 copies); the receiver's delivery is 5.03 µs/round, of which the recv loop is 4.90 µs/round (the K7/K8 copies) — exactly the copies L1 and L2 remove. Gates: L1's win must show in `ms_deliver_recv` per delivered segment (0.397 µs) falling with `segments_delivered/blobs_out` (8.0) holding; L2's in `ms_writer` + `ms_output` per out-segment.

The kcp-stats line gained four fields (`segments_delivered`, `recv_empty`, `ms_deliver_spill`, `ms_deliver_recv`), all additive.

### Link L2 A/B (2026-09-24): two-iovec PUSH send — landed on mechanism, no throughput claim

The engine now emits a 24-byte header plus the segment's own `Bytes` payload (`DatagramSink::write_datagram`), the adapter records the payload as a second iovec (`Span::Split`), and `sendmmsg` writes both without the payload ever being copied. Binaries `b623a96` vs `41acf1f`, one interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms kcp4 + the loopback mux-off control, both SHAs verified; verdict read with the printed `ab_bin_paths` mapping — `molehill-580e51e5` = head, `molehill-5edafb32` = parent; data `results-ab-kcp-zc-l2.json`: 36 arms, 0 cell errors).

**Mechanism, verified structurally** (the per-binary counters are not separable post-hoc — the interleaved rounds share one arm log and the kcp-stats line carries no binary identity, so it is proven by the tests instead): the engine test asserts the retransmit re-emits the *same allocation* (`Bytes::ptr_eq`-style pointer equality) and that the split form's wire bytes are byte-identical to the packed form; the adapter test asserts the batch carries `Span::Split` with the payload pointer unchanged. Wire format, datagram count and ARQ semantics are untouched.

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | -0.2% (non-overlapping, trivial) | -25.8% median — the documented BIMODAL cell (both binaries span 0.4-3.2), unquotable | -1.0% (non-overlapping) | -4.6% / +12.4% / -0.8% (median-only) |
| loss1_rtt10 | +0.2% (inside spread) | -11.6% median (inside spread) | — | -1.7% / +16.7% / -4.8% (median-only) |
| rtt100 | -40.0% (non-overlapping — see below) | wedged on both | — | -11.3% / -25.2% / -9.9% (median-only) |

The gate's positive (a non-overlapping 1-stream or cpu-per-segment win) did not materialize: the loopback 1-stream cell is sender-bound and the sender's cost is the wire drain (2.55 µs/segment), which L2 does not touch — it removes the copy half of the writer+output phases (0.60 + the staging), roughly a tenth of the sender's per-segment cost, and the receiver side is unchanged. Recorded as a mechanism change with the throughput upside explicitly not claimed.

**Why the rtt100 1-stream -40% is not attributable:** the cell is bimodal on BOTH binaries. Per-round detail: head ok reps [0.0105, 0.0546] with round ab3 failing entirely (0 ok reps); parent ok reps [0.0104, 0.0548, 0.0651, 0.0735, 0.0089, 0.0642]. The parent samples the 0.01 mode twice (ab1 min 0.0104, ab3 min 0.0089), the head once; the medians differ because the head had two ok reps against the parent's six — the median-of-few-draws artifact, on a cell whose documented spread is 7× (0.011-0.057). Retransmits are 0-1 both sides. Per §10 the difference is inside the cell's own spread and is recorded as unquotable. The mux-off control on the same binaries measured +4.9% on that cell, i.e. the cell's ordering bias runs the other way.

**RSS note** (median-only, no spread recorded): loopback +12.4%, loss1_rtt10 +16.7% — the two-iovec path keeps the segment's payload `Bytes` alive in the batch until the pacer allows it. Bounded by the batch size (32 datagrams) and the pacer's token bucket; the rtt100 cell (where the pacer denies most spans) measured -25.2% the other way.

### Link L1 A/B (2026-09-24): zero-copy receive — landed on mechanism

The read path hands the engine's own segment buffers to the reader channel by ownership (both per-byte copies gone); parent `f998f27`. The code commit was amended with this record after the run, so the binaries carry the pre-amend code SHA; the amended commit's `src/` tree is byte-identical (only docs changed). One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms kcp4 + the loopback mux-off control, both SHAs verified, verdict read with the printed `ab_bin_paths` mapping; data `results-ab-kcp-zc-l1.json`: 21 arms, 0 cell errors, 0 holes, 6 warnings all the rtt100 8-stream wedge documented on both sides).

**Mechanism, measured by the counters (per delivered segment, 1-stream windows):**

| cell | recv loop (µs/seg) parent → head | segs/reader-message parent → head |
|---|---|---|
| loopback | 0.30-0.58 → **0.045-0.051** (-85..-92%) | 8.1-9.4 → 7.7-7.9 (held) |
| loss1_rtt10 | 0.72-0.79 → **0.042-0.046** (-94%) | 11.6 → 11.6 (identical) |

Input and output phases unchanged; per-segment totals lower and far more stable (the parent's recv windows swung 0.30-0.58 with scheduling; the head's sit at ~0.05); the Phase-1 coalescing ratio is intact — the plan's gate for L1 ("`ms_deliver_recv` per delivered segment falling with `segments_delivered/blobs_out` held") is met with margin.

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | head +11.9% median, inside spread | head +74% — the documented bimodal cell (both inside 0.4-3.2), unquotable | head +2.9% claimable, inside the control's own 11.6% ordering bias on that cell | -3.2% / -10.1% / -1.0% (median-only) |
| loss1_rtt10 | head +0.3% (hair claim) | inside spread | — | -5.2% / -3.8% / -6.3% (median-only) |
| rtt100 | **head +5.4% non-overlapping** | wedge on both | — | -12.5% / -17.2% / -15.5% (median-only) |

Reading every claim against the floors: the only non-overlapping throughput movement attributable to L1 is rtt100 1-stream +5.4% — the one cell without a mux-off control to establish its bias floor, and a cell whose 1-stream swung -13.5% in the Phase 1 A/B — so it is recorded as directional, not proven. The gate metric did NOT materialize: the loopback 1-stream cell is sender-bound (the sender's wire drain is its cost), and L1 attacks the receiver, whose payoff shows up as medians moving the right way on every secondary axis rather than as a claim. Per §10 this is a mechanism win with no attributable regression — landed, throughput upside explicitly not claimed. The copy map predicted exactly this: the copies were never the binding constraint at these cells, which is why the link that removes them buys stability (and CPU/RSS) rather than a step change. Behaviour is unchanged: the phases are timers around existing code, and the fix only moves a drop point.

### Link L3 A/B (2026-09-24): owned Noise records into the writer channel

The kcp4+noise write path hands the Noise record to the writer channel by ownership and the engine shares it per segment with O(1) `Bytes` splits — K1 and K2 both gone. Parent `73da491` (L2); the code commit was amended with this record after the run, so the binaries carry the pre-amend SHA whose `src/` tree is byte-identical (only this file, CHANGELOG.md and the bench data changed). One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100 requested, arms kcp4 + the mux-off control, SHAs verified from `--version`: parent `v0.8.1-82-g73da491`, head `v0.8.1-83-g1b13dd9`; verdict read with the printed `ab_bin_paths` mapping — `molehill-44aff44c` = parent, `molehill-7a5b37a0` = head).

**Coverage:** the run was interrupted at the start of the rtt100 cell, so the data covers **loopback + loss1_rtt10 only** — 12 arms, 0 cell errors, 0 holes, 0 warnings; data `results-ab-kcp-zc-l3.json`. The rtt100 cell is not part of this link's comparison.

**Mechanism, proven by the tests** (the per-binary counters are not separable at phase precision — see the counter note below): the engine test locks the owned write against the slice write byte for byte, the Noise round-trip suite runs through the owned-record path, and the KCP integration tests (kcp4, noise, mixed) cover the stream end to end. `TAKES_OWNED` is a const, so a plain-TCP transport keeps the pooled-buffer path with the dispatch folded away at monomorphization.

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | **+5.7% CLAIM** (3.826 → 4.046, non-overlapping — ab2's rep ranges disjoint, ab1/ab3 overlap) | -2.0% inside spread (the documented bimodal cell) | **+24.4% CLAIM** (6.353 → 7.902, non-overlapping in all 3 rounds) | -0.0% / +7.5% / -6.1% (median-only) |
| loss1_rtt10 | -0.3% tool claim, inside the cell's own spread — not attributable | -0.4% tool claim, same | — | -5.3% / -6.7% / -6.9% (median-only) |

**The two loss1_rtt10 tool claims are not attributable** (the tool's rule fires on any single round's disjoint ranges; both fire inside the cell's own rep spread): 1-stream round medians parent [0.439, 0.443, 0.433] vs head [0.438, 0.428, 0.483] with rep ranges spanning 0.37-0.49; 8-stream [0.734, 0.806, 0.720] vs [0.731, 0.724, 0.739] with reps to 0.96. Retransmits comparable (parent 74-106 vs head 84-103 on 1-stream, 214-265 vs 212-248 on 8-stream — no ARQ difference). Per §10, recorded as unquotable.

**The control arm's reading** (the mux-off arm is plain TCP in direct mode — shares no code with this change, so it measures the run's own ordering bias): loopback 1-stream **-10.3%** by the tool's rule, but the claim rests on ab1 alone (parent [21.904, 22.351] vs head [18.650, 21.623]; ab2/ab3 overlap heavily); the same control measured **+11.3%** on 64-stream (head higher in all three rounds) and -0.1% on loss1 8-stream — the bias runs both ways inside one run. Read against it, the kcp4 64-stream +24.4% exceeds the control's own +11.3% on that cell, and the kcp4 1-stream +5.7% moved the opposite way to the control's -10.3%.

**Counter note (why no phase-level claim, same stance as L2):** the run carried `MOLEHILL_KCP_STATS=1` (844 stats lines per arm log) and each process logs its commit SHA in the version line, so per-binary attribution IS possible post-hoc. Six controlled 1-rep × 8 s loopback kcp4 runs (three per binary, sequential, MOLEHILL_BIN per run) give server-side per-datagram writer-phase rates of parent [0.26, 0.33, 1.08] vs head [0.06, 0.22, 0.24] µs — the within-binary scatter (retransmit rate varied 0.6M-7.8M per window between identical runs) spans the between-binary difference, and the receiver side's writer/output phases were identical across the two binaries (0.178/0.172 and 7.835/7.858 µs per datagram) as expected for a path L3 does not touch. The counters are recorded as evidence the mechanism is in the measured path, not as a claim; the tests are the proof.

**Gate:** the plan's gate (no attributable CLAIM REGRESSION; a non-overlapping 1-stream or cpu-per-segment win) is met on the covered cells — two claimable favourable throughput cells, every secondary axis median-only favourable or flat, no attributable regression.

### Link N1 (2026-09-24): parked — the premise did not survive the read-path audit

The map's R2 row was written against the pre-`e463391` read path, where every record was decrypted into `bufs.payload`. That fast path has since landed: **when the record's plaintext fits the caller's buffer, the decrypt already writes straight into it**, so the staging copy R2 named is gone from the path the bench measures. What remains per record is exactly two buffer traversals of the ciphertext — the accumulation copy (kernel → scratch, R1, the syscall boundary) and the AEAD's own read — plus the plaintext write that is the AEAD's output. Both are inherent; neither is a copy molehill adds.

N1 ("read the whole record into the caller's buffer, decrypt in place, plaintext shifted left 2 B") relocates the ciphertext's landing buffer from `scratch` to the caller's buffer; the byte traffic is unchanged, so there is no copy left in R2 for it to remove. Three further facts, checked against the code:

1. **Its fit condition is strictly stricter.** N1 needs the WHOLE record (2 B header + ciphertext + 16 B tag) inside the caller's buffer; the current fast path needs only the plaintext. A 16 KiB mux body record is 2 + 16384 + 16 = 16402 bytes and does not fit the 16 KiB ask — and the header records (2 + 12 + 16 = 30 B) do not fit the 12 B frame header ask either — so on the mux path N1 would fall back to the scratch path for every frame, strictly more work than the current fast path, which covers both asks exactly (which is why `e463391` measured +12.3% there).
2. **It multiplies syscalls.** The current path amortizes one inner read per ~64 KiB accumulation chunk; N1's per-record read (2 B header, then a record-sized read) pays one syscall per record — worst on the mux path's 14-byte plaintext header records.
3. **snow exposes no in-place decrypt.** `TransportState` offers `read_message` only; an in-place N1 would have to take the Noise record cipher (nonce management plus ChaCha20-Poly1305) in-house — a rewrite for a mechanism with zero byte-traffic win.

Recorded as parked (same discipline as "L3 window/streams decoupling — premise refuted"): the expected "M1/N1 ≈ 2-5% CPU on the default arms" applies to M1 alone. Not implemented; no commit, no A/B.

### Link M1 A/B (2026-09-24): owned frame bodies — the gate failed, reverted

Commit `fa34343`, replayed as `cfc06be` by the L3 record amend (src trees byte-identical), reverted in the same session; the revert commit carries the same numbers. The change: `Frame.body` `Vec`→`Bytes` (the read path freezes the receive buffer in place; `Stream::poll_write_owned` shares the caller's buffer as the frame body with an O(1) slice) and the forwarding legs moved to `forward_bidirectional`, whose data-channel direction reads into a fresh 32 KiB `BytesMut` that becomes the write buffer by ownership.

One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10, arms mux / noise / kcp4 plus the auto-appended mux-off control on loopback; parent `1b13dd9` = L3, head `fa34343` = M1, both SHAs verified from `--version`). Verdict read with the printed `ab_bin_paths` mapping — the pair sorts by label, so the tool's A side is the HEAD binary (`molehill-4244970c` = ab-m1-head) and its B side the parent (`molehill-fe25b6e9` = ab-m1-parent); every delta below is re-read parent → head. Data `results-ab-mux-zc-m1.json`: 42 arms, 0 cell errors, 1 hole (kcp4 ab2 head's mixed-bulk probe: iperf3 "control socket has closed unexpectedly" — a probe failure with its reason recorded; that arm-run's throughput reps completed).

| arm / cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| mux loopback | -5.4% inside spread (parent ahead all 3 rounds) | -2.9% inside spread | **-15.9% CLAIM REGRESSION** (17.622 vs 20.418; parent ahead 2 of 3 rounds) | -2.0% / **+25.1%** / -0.9% (median-only; RSS higher in all 3 rounds) |
| noise loopback | **+13.4% CLAIM favourable** (5.410 vs 4.683; M1 ahead all 3 rounds) | -4.7% inside spread | -0.6% claim (hair) | +0.2% / **+22.8%** / no change (median-only; RSS higher all 3) |
| kcp4 loopback | +9.1% inside spread (M1 ahead all 3 rounds) | +3.0% inside spread | **-13.0% CLAIM REGRESSION** (7.784 vs 8.794; parent ahead 2 of 3) | -0.3% / -3.8% / +5.7% (median-only) |
| mux loss1_rtt10 | **-3.1% CLAIM REGRESSION** (4.322 vs 4.454; parent ahead 2 of 3) | -0.2% inside spread | — | **+11.4%** / +7.6% / +9.5% (median-only) |
| noise loss1_rtt10 | -0.9% inside spread | -0.6% inside spread | — | -1.8% / -11.5% / -1.5% (median-only) |
| kcp4 loss1_rtt10 | **-2.2% CLAIM REGRESSION** (0.441 vs 0.451; parent ahead 3 of 3) | **+4.8% CLAIM favourable** (0.786 vs 0.749; M1 ahead 2 of 3) | — | +1.2% / +1.0% / +8.4% (median-only) |
| mux-off control loopback | +5.3% claim (the cell's ordering bias) | -1.1% inside spread | -3.9% claim (same) | -1.3% / -8.1% / — |

**Reading.** The gate (no attributable CLAIM REGRESSION) fails on four cells. Two are the single-rep 64-stream points: the mux-off control shows that cell's ordering bias at -3.9% and every arm's rounds alternate (parent ahead 2 of 3), so part of the median is artifact — but both arms' movements exceed the control's own bias and the mechanism has a plausible cost at that cell, so they are not dismissible. The two loss1_rtt10 1-stream claims are hair claims inside the cells' own ±5% rep spreads; the kcp4 one nevertheless has the parent ahead in all three rounds. Note the mux-off arm is NOT an untouched control for this link — M1 rewrote that loop too (its borrowed shape, measured here at parity) — which weakens the bias estimate for the mux arms.

**The cost mechanism is identifiable.** The owned forwarding direction allocates AND zeroes a fresh 32 KiB `BytesMut` per read (`BytesMut::zeroed(READ_CHUNK)` in `transfer_owned`). At the mux arm's framing-bound 1-stream rate that is ~1.3 GB/s of memset, and the allocation residency is the RSS: +25.1% (mux) and +22.8% (noise) on loopback, higher in all 3 rounds — a real, attributable memory-axis cost. The upside is real too: noise loopback 1-stream +13.4% (M1 ahead in all 3 rounds; the control's own +5.3% at that cell leaves ~+8% attributable), kcp4 loopback 1-stream +9.1% (3 of 3 rounds, inside spread), CPU -2.0% on mux (3 of 3 rounds).

**Net:** the mechanism removes the copy but pays a per-read allocation plus zero-fill for it — the plan's expected "M1 ≈ 2-5% CPU on the default arms" did not survive (CPU axis flat, memory axis pays). Per the link discipline M1 is reverted and the evidence stands here. The refinement the data points at — read into the BytesMut's spare capacity without the zero-fill (`AsyncReadExt::read_buf` instead of `zeroed` + `ReadBuf::new`) — is recorded as the starting point if this link is retried; it was NOT attempted. The copy map's W2 row returns to "yes (M1)" as an open link, not a removal.

### Link S1 A/B (2026-09-24): stripe chunks read into their frame — landed on mechanism

The stripe send direction reads each chunk directly into the payload region of its frame buffer, behind the 10-byte header written in front once the length is known, and hands the frame to the stripe by ownership — the send direction's staging copy is gone; the stripe write itself keeps the borrowed boundary. Parent `1b13dd9` (L3 — the tree the M1 revert restored); the code commit was amended with this record after the run, so the binaries carry the pre-amend SHA whose `src/` tree is byte-identical (only this file, CHANGELOG.md and the bench data changed). One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cell loopback, arms mux-stripe + the unstriped `mux` inertness control + the mux-off control; SHAs verified from `--version`: parent `v0.8.1-83-g1b13dd9`, head `v0.8.1-84-g852de3d`; labels sorted parent-first, so the verdict's A side IS the parent (`molehill-7a5b37a0`) and B the head (`molehill-f2aff66d`); data `results-ab-stripe-zc-s1.json`: 18 arms, 0 cell errors, 0 holes, 0 warnings).

**Mechanism, proven by the tests:** the existing frame-ordering tests are re-driven through the owned API, and the wire format and the round-robin/commit semantics are unchanged (the frame is the same bytes, built in place).

| axis | parent | head | reading |
|---|---|---|---|
| 1-stream | 17.098 [15.478, 17.994, 17.098] | 18.758 [19.555, 18.758, 17.324] | **+9.7% median, inside spread** — the head ahead in all 3 rounds (ab1 +26%, ab2 +4%, ab3 +1%) but the ranges overlap |
| 8-stream | 20.161 [19.283, 20.161, 20.568] | 19.497 [19.462, 22.118, 19.497] | -3.3% median, inside spread (rounds alternate) |
| churn / echo p50 / udp p50 / HoL gap | 4649/s, 0.283 ms, 0.411 ms, 33.42 ms | 4657/s, 0.284 ms, 0.436 ms, 33.41 ms | median-only, no movement (udp p50 +6.1% is that cell's jitter) |
| RSS / CPU / cpu-per-kframe | 24216 KiB / 591.2% / 0.029 | 26813 KiB / 610.6% / 0.029 | +10.7% / +3.3% / +1.4%, median-only, the head higher in all 3 rounds on the first two |

**Controls:** the unstriped `mux` arm (untouched by this change) sits at parity — 1-stream +6.5% and 8-stream +1.0% inside spread, 64-stream +3.4% a claim on the single-rep cell's noise — and the mux-off control's only claim is 8-stream -0.5%, the documented noise floor on a path S1 does not touch.

**Reading:** the staging copy is structurally gone (the read buffer becomes the frame), the arm shows no attributable regression, and the 1-stream median moved the right way in all three rounds without reaching a claim. The cost side is the same shape M1's audit found, one size down: the per-chunk frame buffer (32 KiB + 10 B header, allocated per read instead of reused) is the stripe arm's +10.7% RSS and +3.3% CPU, both median-only and bounded to the opt-in arm. Recorded as a mechanism change with the throughput upside explicitly not claimed.

## Final cumulative A/B: branch vs `main` (2026-09-22, host `9201f86b86a8`) — VOID

**Every number in this section is withdrawn** (the `--ab` harness bug below: both sides ran one binary). Retained for audit only.

Run shape: one interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms mux / noise / mux1 / kcp4 plus the mux-off control on loopback; branch `d798100`+bench fixes vs `main` `8584945`), followed by a loss5_rtt100 run (mux / noise / kcp4) and a focused 5-round re-measurement of the one cell that fired an unfavourable claim. Supersedes the old `results-final-ab-2026-09-21.json` claim (mux + mux1, loopback + loss1, 24 paired ranges, 21 overlapping).

Instrument noise floor, measured on the control arm: the mux-off arm carries no mux engine, yet the two binaries claim ±1.8% non-overlapping on the single-rep 64-stream point, and the mux 1-stream/8-stream rounds ALTERNATE direction (round 1 head +6%, round 2 main +11%, round 3 tie). Round-to-round noise on the loopback 8-stream cell is ±5-9%.

| axis (priority order) | verdict |
|---|---|
| **latency** (echo p50/p99, steady p50/p99, udp p50, HoL max gap) | no regression anywhere: every arm/cell at parity (mostly <1%) or better. mux-off loopback steady ping -13.2%/-12.5% and mux1 loopback -3.3%/-4.1% better; kcp4 loss5 steady p99 -30.0%, HoL gap -31.2%, udp loss -25%. Wrong-way medians: noise loopback steady p50 +10.8% (0.36→0.40 ms), mux1 rtt100 HoL gap +23.3% (no spread recorded), loss5 connect-path echo p50 +19-21% median-only on all three arms (that cell's own run-to-run echo spread is ±19%: main measured 1253.9 ms in one run and 1053.4 ms in the other). rtt100 cells: 0.0% on every latency metric. |
| **throughput** | no systematic regression. mux (the gated row): loopback -3.5%/-4.4%/-3.6% with rounds alternating direction (round noise ±5-9%), loss1_rtt10 +0.9%/+0.2%, rtt100 +4.7%/+1.7% → parity. mux1: loopback -5.9%/+0.3%, loss1 +0.7%/**+9.0%**, rtt100 -3.9%/-4.3%. noise: loopback +6.3%/-7.5%/+13.8%, loss1 +3.9%/-2.5%, rtt100 +1.6%/+1.1%. kcp4: loopback (bimodal, unquotable), loss1 +1.6%/+1.8%, rtt100 +5.1%. |
| **memory / cpu** (growth accepted) | RSS flat (±1-2%) on mux/mux1/noise, kcp4 loopback -7.2%, mux1 loss1 -6.6%, noise loss1 +15.1%; CPU +0.4-6.0% on the mux/noise arms and -1.6..-7.3% on mux1. All inside the accepted band. |

Claimable (non-overlapping) movements — 4 favourable, 3 unfavourable, all at the noise floor except where noted:

| arm / cell | metric | branch | main | delta |
|---|---|---|---|---|
| kcp4 loopback | 64-stream | 8.257 | 7.759 | branch **+6.0%** |
| noise loopback | 64-stream | 14.996 | 13.181 | branch **+13.8%** |
| noise rtt100 | 1-stream | 0.676 | 0.666 | branch **+1.6%** |
| mux-off loopback (control) | 64-stream | 18.795 | 18.461 | +1.8% — the floor itself |
| mux loopback | 64-stream | 19.165 | 19.889 | -3.6% — single-rep, rounds alternate |
| noise loopback | 8-stream | 15.890 | 17.174 | -7.5% — 1 of 3 rounds disjoint; the other two overlap (round 2: 17.05 vs 17.07) |
| noise loss1_rtt10 | 8-stream | 8.112 | 8.323 | -2.5% — thin rep samples |

**loss5_rtt100 (follow-up run):** mux 1-stream +6.3% (claim, not re-confirmed), mux 8-stream -22.2% (claim), kcp4 1-stream +2.8% (claim) with steady p99 -30.0% / HoL gap -31.2% / udp loss -25% / RSS -10.9% better, noise 8-stream +11.1% (claim) with CPU -13.3%. The mux 8-stream claim was the only consistent-direction unfavourable signal of the whole session, so it was re-measured focused: **5 rounds, mux arm only, same binaries — 8-stream 0.835 vs 0.836 (-0.2%, no claim)**; the first reading was a small-sample artifact of the cell's ±30% rep spreads (2-3 ok reps per arm-run). Data: `results-ab-final-2026-09-22-loss5.json`, `results-ab-mux-loss5-focus.json`.

The per-change wins the branch accumulated — the leaner noise stream, the direct decrypt, the 32 KiB frame split — were each measured against their immediate parent and are inherited. Data `results-ab-final-2026-09-22.json` (78 arms, 0 cell errors, one documented kcp4 rtt100 gap, no unexplained holes).

**Data caveat:** that run's file carries **no `framing_cpu` column** (0/78 arms) — the host's environment dropped the `MOLEHILL_MUX_STATS` propagation sometime between 20:44 and 02:11 that night. The throughput, latency, memory and CPU verdicts are unaffected (they come from iperf3 and the samplers, not the framing counters), and every framing number quoted in this document comes from the experiment runs, where the counters were present. The symptom could not be reproduced afterwards with the identical bench code and binaries (a 1-rep rerun produced 6 472 stats lines), so it is filed as an environment fault; the bench now records a typed `partial_metrics` note when a framed arm produces no mux-stats lines.

**Scope:** the cumulative claim covers four of the nine matrix cells (loopback, loss1_rtt10, rtt100, loss5_rtt100). The pure-delay rtt10 cell, the two rate-shaped cells (r100/r20) and the jitter cell are NOT part of the branch-vs-main comparison — they are exercised by the release baseline (`results-v0.8.1.json`, same code as `main` plus the noise-stream work) but not A/B'd against it. Everything measured above holds within its stated cells — except that the whole run shared one binary across its `--ab` sides.

**Superseded 2026-09-24:** this section's run is void (the harness bug below). The valid cumulative comparison is the next section, run with the fixed harness and the current tree.

## Final cumulative A/B: branch vs `main` (2026-09-24, host `a249c64b88c6`) — VALID

The first VALID branch-vs-main comparison after the `--ab` harness fix (`064c55a`). One interleaved `--ab` run: 3 rounds × 3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms mux / noise / mux1 / kcp4 plus the auto-appended mux-off control on loopback; binaries freshly built from `main` `8584945` and the branch tip `2c6d5e1` (L1+L2+L3+S1, M1 reverted), both SHAs verified from `--version`. Verdict read with the printed `ab_bin_paths` mapping: `molehill-38be37ba` = main (A side, ran first each round), `molehill-9d54fcaf` = branch (B side). Data `results-ab-final-2026-09-24.json`: 78 arms, 0 cell errors, 0 holes, 24 warnings (the recurring "no mux-stats lines" `partial_metrics` on ab1's kcp4/mux1/noise arm-runs — the typed-loud failure mode `f5e6719c` added, the same environment fault the 2026-09-22 run lost silently — plus the documented rtt100 8-stream gaps and one zero-byte stream). Scope: the same three of nine matrix cells the 2026-09-22 run claimed; loss5_rtt100, rtt10, the rate-shaped and jitter cells remain un-A/B'd.

**Latency — no regression anywhere.** Every arm/cell at parity on echo p50/p95/p99, tcp steady, udp p50/p99 and HoL max gap (almost all <1%; rtt100 cells 0.0% by construction). Medians moved favourably in places: noise loss1 jitter -62.5%, mux/mux1/noise rtt100 jitter -95 to -97%, udp p50 -5 to -7% on the kcp4 and noise loopback cells, mux1 loss1 HoL gap -9.1%.

**Throughput — net strongly favourable; 15 claimable cells vs 6, of which five are not attributable:**

| arm / cell | branch vs main | reading |
|---|---|---|
| kcp4 loopback | 1-stream **+34.2%** (3.092→4.150, branch ahead 3/3), 64-stream **+14.8%** | the zero-copy route's cells; consistent |
| kcp4 loss1_rtt10 | 1-stream **+6.6%** | claim |
| kcp4 rtt100 | 1-stream **+43.9%** (0.057→0.082) | claim |
| kcp4 loopback 8-stream | -40.8% | **not attributable** — the documented bimodal cell (both inside 0.4-3.2); main's ab3 (1.069) sits inside the branch's own range |
| mux1 loopback | 8-stream **+51.1%** (10.289→15.550) | claim |
| mux1 loss1_rtt10 | 8-stream **+13.3%** | claim |
| mux1 rtt100 | 8-stream **+17.2%** | claim |
| mux loopback | 64-stream **+9.0%** | claim |
| mux loopback | 1-stream -12.2%, 8-stream -17.0% | **attributable cost** — see below |
| mux loss1_rtt10 | 8-stream -6.2% | claim, alongside the loss1 CPU cost below |
| mux rtt100 | 8-stream **+6.6%** | claim |
| noise loopback | 8-stream **+15.3%**, 64-stream **+13.7%** | claims |
| noise loopback / loss1 1-stream | -0.2% / -0.2% | hair claims (0.2%), inside any spread |
| noise loss1_rtt10 | 8-stream **+43.9%** | claim |
| noise rtt100 | 8-stream **+6.3%** | claim |
| mux-off control loopback | 1-stream **+6.5%**, 8-stream **+1.2%**, 64-stream -8.2% | the control's own reading; the 64-stream claim is that single-rep cell's bimodality (both span 23-27) |

The mux arm's loopback 1/8-stream cost is the one real throughput finding: main ahead in 3 of 3 rounds at 8-stream (28.4/31.8/26.2 vs 23.7/23.6/22.9) and 2 of 3 at 1-stream, and the mux-off control at the same cells reads +6.5%/+1.2% FAVOURABLE for the branch — so it is not the run's ordering bias (which runs the other way there). It is attributable to the branch's mux-path changes (the vendored engine and the stream-cap raise are the only mux-path differences; cpu/kframe is flat at 0.046→0.045, so the cost is the data path's throughput bound, not per-frame work). Localizing it — an A/B of the cap raise alone, or of the vendoring alone — is the follow-up bench item recorded here.

**CPU — net favourable** (median-only except where noted): kcp4 loopback **-17.2%** and cpu/kframe **-14.5%**, noise loopback **-16.1%** / cpu/kframe **-14.3%**, mux1 loopback **-10.3%** / loss1 **-20.7%**, mux loopback -3.5%, mux rtt100 -7.4%. The cost cells: mux/noise loss1 **+37.2%/+45.0%** with cpu/kframe +37.3%/+45.0% — measured on ab2/ab3 with the framing counters present (frames/s flat: 15.0-15.8k main vs 15.7-16.2k branch, avg frame bytes identical at ~31.8 KiB), i.e. the branch's per-frame CPU on the lossy cell genuinely rose while the cell's 8-stream throughput rose +43.9% (noise) / fell -6.2% (mux). Which branch change pays it is not isolable from this cumulative run (no mux-off control exists on the loss cell); recorded, not explained.

**Memory — the one axis that regresses, as the user ruled it out of the gate**: kcp4 loopback RSS **+194%** (main [76, 69, 70] MiB vs branch [206, 170, 206], higher in all 3 rounds) — the KCP send staging (46 KiB per session) plus the receive-coalescing/parts residency, times the 64 concurrent sessions that cell holds; loss1 cells +23-105% correlate with their throughput movements; rtt100 +5-36%; mux/mux1/noise loopback +0-13%. The Phase 1 record's "+73%" was against a different parent; the cumulative figure is this one.

**Reading:** the branch does not regress the latency axis anywhere, is strongly net-positive on throughput across all four arms and all three cells (the kcp4 and mux1 arms gain on every cell; the shaped cells gain most), and is net-positive on CPU — against one attributable mux-arm loopback cost (-12%/-17% at 1/8-stream), one loss1 CPU cost (+37-45% median-only, mechanism unisolated), and the kcp4 memory doubling the user ruled out. The favourable cells include the ones the zero-copy route targeted: kcp4 loopback 1-stream +34.2% (L1's stability plus L3's write path), kcp4 rtt100 1-stream +43.9%, mux1 8-stream +51.1%.

## Release preparation (2026-09-25): what this session changed, and what it left open

The branch was prepared for the v0.9.0 tag. Everything below is committed on `perf/data-path-optimizations`; the tag itself is a human act (AGENTS.md §5) and has **not** been created.

### Fixed in the harness (four silent defects, each with a real consequence)

1. **`lib.noise_keys` raised `NameError` on first use** — the function read a module global that was never defined, so every `noise` / `kcp4` / `noise-direct` variant died before measuring anything. Now an `functools.cache`d function keyed by binary path (so a screen's build swap cannot reuse another build's keypair).
2. **The single-run lock and the stale-process sweep looked for `bench.py`** — the runner the matrix retired. Consequences: concurrent soak runs were no longer refused, and `sweep_stale` could SIGKILL a live run's processes. Both now identify the runner by `lib.RUNNER_NAME`.
3. **Instrumentation that was accepted and ignored**: `SOAK_SLO_RTT_P99_MS` and `SOAK_SOAK_LOAD_FRACTION` were read into `Knobs` and never used; the capacity verdict ignored the interactive error rate its own SLO documents; `SOAK_RAMP_STEP` / `SOAK_STAGE_TIMEOUT_S` were dead. Dead knobs are deleted, live ones are wired, and the meta records every knob the run used (`slo`, `load_fractions`, `streams_max`, `settle_s`, sample rates) plus `revision`, `molehill_bin`, `molehill_version` and the opt-in `instrumentation` set.
4. **The charts' per-stage statistics selected no samples at all** — the stage windows were computed in one time base and applied in another, so every "stage p50/p99" overlay the new plot draws was silently empty. `soak_plot` now separates `stage_windows` (absolute, for selecting samples) from `stage_spans` (relative, for drawing).

The runner no longer forces `MOLEHILL_MUX_STATS=1` on every molehill spawn: that was molehill-only work inside the measured path, the peers have no equivalent, and nothing in the Soak model parses the lines. Diagnostics are opt-in via the environment and recorded in the meta when set.

### The gate (`soak_check.py`) is now a self-check plus a comparison

Step 1 checks the run against itself: series completeness per claimed coverage axis, the throughput endpoint invariant (recorded in the data as `endpoints`, re-checked by the gate), and the absolute SLO applied **per clean stage** — a saturated `rrul` stage is above the SLO by design and is reported as a note. The SLO gates molehill; a peer that misses it is reported with its number (`rathole` 81/78 ms, `nps` 74/82 ms on the v0.9.0 clean stages) and does not block. Step 2 is the per-type comparison against the previous tag's file, now including the worst-1s threshold that the docs always listed but no code applied.

**Open, and the reason the committed v0.9.0 results report 5 LEGACY checks:** `results-soak-v0.9.0.json` was produced before the harness recorded `revision` and `endpoints`. Its numbers stand (the measurement path is unchanged apart from an extra `udp_attempt` line per UDP ping, which the loss rate now needs), but the gate cannot verify the endpoint invariant or tie the run to a checkout from that file. Two ways to close it, both a human call: re-run the ritual with the current harness (hours, on the bench host), or record the waiver here. **The re-run is the better option and is recommended.** `soak_check.py` prints the count in its summary.

### Documentation: truthfulness pass, audience split, ownership contract

Each topic now has exactly one home; the rule and routing table live in AGENTS.md §3 ("One topic, one home"), and the audience-and-scope table (owner + "does not own" per page) in docs/structure.md. What moved:

- the benchmark method left the README for `docs/benchmarks.md` (+ `.zh.md`): what is measured, how to read each chart, the stage schedule, the SLO, the test types, the per-decision measurements, comparability and how to reproduce a run (including the two-build screen). The README keeps the numbers, three bullets on reading them and a link;
- the measured per-decision cost table left `docs/configuration.md` for the same page (with its provenance);
- `docs/release.md` no longer re-explains the method: it owns the ritual, the artifacts and the gate;
- the README no longer carries the "what replaced the old tables" narrative, the release-gate mechanics or harness-development asides;
- the Chinese landing page's measured-configuration table (no English counterpart, cited this file) was replaced by the same structure the English page uses, pointing at the measurement page.

The gate enforces what it can: a page without an owner in docs/structure.md fails, as does a user-facing page whose mirror is missing or has a different heading structure, or a README that does not link its own language's page.

Five documented behaviours were wrong or unenforced and now say what the code does: the privileged-port rule in `allow_ports` (**never implemented** — the whitelist admits any port it contains; the OS decides whether the bind succeeds), the tunnel-count ceiling (64 streams per tunnel), the `retry_interval` backoff-cap semantics (and the 1 s fallback after three tries), the real scope of `nodelay` (client-side sockets only), and the PSK requirement (silently unused without a PSK modifier in `pattern`). `--genkey x448` was dropped (the shipped `snow` backend has no X448). The configuration pages' per-decision cost table is now labelled: its numbers come from the **retired per-cell model** and are not comparable with the Soak figures above it (the v0.9.0 run covers the default arm only). Docs are grouped by audience: README/configuration/transport for people running molehill (Chinese mirrors kept), contributor and governance docs English-only per AGENTS.md §3.

### Open items this session recorded but did not close

0. **The FFI batching module is the last `unsafe` (evaluated 2026-09-25, not changed).** `src/transport/udp_batch.rs` carries 8 `unsafe_code` expectations; the rest of the tree has none and `unsafe_code` is now `deny`. Checked against their sources before the v0.9.0 run: `nix` 0.31.3's `MultiHeaders<S>` is `Box<[libc::mmsghdr]>` and therefore `!Send`/`!Sync`, so adopting it would keep the three `unsafe impl Send`/`Sync` proofs and add a per-call `Vec<IoSliceMut>` allocation in the hot path — not worth a dependency. `quinn-udp` *could* remove all eight (`UdpSocketState` is plain `Send + Sync` and takes caller-owned slice buffers), but it also brings GSO/GRO segmentation, i.e. it changes the send path's syscall shape on the KCP carrier — a data-path change that belongs in its own `screen` A/B. Reasoning lives in docs/lint-policy.md ("Unsafe") and at the top of the module.
1. **The configuration test gaps** (audit of the docs against `tests/`, ranked by user impact). Fixed in this session: the three vacuous tests (`type = "udp"` had been renamed to `protocol`, and two invalid fixtures failed on a missing `remote_bind_addr` instead of the defect they name — every invalid fixture now declares `# expect: <substring>` and the harness asserts it) and a `documented_defaults_are_pinned` test covering the documented heartbeat/pool/retry/UDP/health-check/keepalive defaults. Still missing, in priority order: `allow_ports` **rejection** end to end (nothing asserts the client's "Port rejected" path or the empty-whitelist master switch); `health_check` end to end (unregister → visitors fail fast → re-register); per-service `token` / `heartbeat_timeout` / `retry_interval` resolution; `max_pool_size` clamping; the UDP knobs' documented effects; a PSK handshake (and the "pattern must carry a PSK modifier" rule); hot-reload service add/delete/modify; `--genkey` curve behaviour (now documented as X25519-only); the doc-example parse gate is `#[cfg(feature = "multiplex")]`, so the `embedded` legs never parse the documented examples.
2. **The privileged-port rule** is documented as *not* implemented. The stricter behaviour would be a code change (reject a registration whose port is <1024 and is admitted only by a range) plus a test — a behaviour decision for the human.
3. **The Soak variant sweep** (`mux-off`, `noise`, `mux1`, `kcp4`) has still not been measured with the new model, which is why the configuration page's decision table still quotes the retired model.
4. `docs/build-guide.md` claims a 574 KiB `x86_64-unknown-linux-glibc` binary (the label is not a real triple) and says nothing about the musl/embedded artifacts the release actually ships.

### Peripheral infrastructure hardened

`githooks/check-docs` now checks every command a hook runs (not only `cargo` ones), checks the reverse direction (a documented gate that no hook runs), and pins the docs index on both READMEs; `githooks/pre-commit` and `just py-lint` run `uvx ruff format --check` beside the lint. `release.yml` refuses an undated changelog section and a missing soak result/chart, takes a concurrency group, and checks out full history so `--version` can report a real revision (`build.rs` now emits the SHA and marks a dirty tree). The retired matrix's last two files (`benches/scripts/bench/results-v0.9.0.json*`) are deleted, which makes the CHANGELOG's removal claim true.

## The `--ab` harness bug (found 2026-09-23, fixed in `064c55a`)

**Every `--ab` run on this branch measured the DEFAULT binary against itself.** `bench.py`'s `--ab` loop assigned `knobs.molehill_bin = ab_bin` per arm, but every spawn site (`start_molehill`, `noise_keys`, `tool_version`) read the module-level `_KNOBS["bin"]` global, seeded once from the environment before the cell loop and never updated. The label suffix (`(ab1:molehill-main)` vs `(ab1:molehill-head)`) therefore did not describe what ran: both sides of every interleave were the same binary, sampling the same epochs.

Affected files: `results-final-ab-2026-09-21.json`, `results-ab-final-2026-09-22.json`, `results-ab-final-2026-09-22-loss5.json`, `results-ab-mux-loss5-focus.json`. Their "branch vs main" movements are interleaved same-binary noise, and their non-overlapping claims are withdrawn — §10's provenance rule failed silently because the label checked out while the process did not.

What still stands: the per-change A/Bs taken as two separate invocations (`MOLEHILL_BIN` per run, then `ab_compare --baseline` — e.g. the leaner noise-stream pair and the direct-decrypt pair), the KCP experiment files, and the release baselines. The 2026-09-22 "cumulative A/B" must not be quoted as branch-vs-`main` evidence until it is re-run with the fixed harness — that re-run was the outstanding bench work for a merge decision (done 2026-09-24, above). The fix removes the global entirely: the binary path now lives only in `knobs` (which the interleave swaps), and the spawn helpers take it as a parameter.

## Stripe A/B (K=4): the single-stream ceiling experiment

The prototype: `[server.data] stripe_count = K` spreads a visitor connection over K data channels (design: docs/internals.md, "Data-channel striping"). The experiment arm differs from `mux` only by the per-arm `MOLEHILL_STRIPE_COUNT=4` measurement override — configs are identical, and a binary that predates the striped command ignores the variable, so the baseline arms are the unstriped path by construction.

One interleaved `--ab` run with the FIXED harness: loopback, arms `mux` / `mux-stripe` + the loopback `mux-off` control, 3 rounds × 3 reps × 8 s; binary A = `5e6719c` (parent), binary B = `f91e6bb` (stripe), both freshly built with the commit SHA verified from `--version`. Data: `results-stripe-k4.json` (18 arms, audited: 0 cell errors, 0 holes).

| axis | base (`5e6719c`) | stripe (`f91e6bb`) | verdict |
|---|---|---|---|
| **1-stream throughput** | 10.73 [9.08, 14.06] | 15.96 [15.43, 15.98] | **CLAIM favourable +48.7% (non-overlapping, all 3 rounds)** |
| 8-stream throughput | 18.35 | 19.29 | inside spread, +5.1% |
| churn connects/s | 5052.7 | 4665.7 | median-only -7.7% |
| echo RTT p50 | 0.265 ms | 0.278 ms | median-only +4.9% |
| udp RTT p50 | 0.417 ms | 0.437 ms | median-only +4.8% |
| HoL max gap | 33.44 ms | 33.42 ms | no change |
| RSS | 22.9 MiB | 24.9 MiB | median-only +8.7% |
| CPU | 427.1% | 601.2% | median-only +40.8% |
| cpu/kframe | 0.051 | 0.031 | median-only -40.0% |

Reference points from the same run: the direct mode (`mux-off`, both binaries at parity — 19.9–21.9 Gbit/s 1-stream) is the no-tax ceiling, and the unstriped mux arm is the taxed one: 10.73 vs ~20.4 is the 2.19x tax this host's loopback cell shows; striping recovers 1.7 of the ~10.7 Gbit/s of it (a residual ~1.27x gap to direct remains). The cpu/kframe halving is the ①-scaling effect the design predicted (frames spread over four driver tasks instead of one).

**Inertness at K=1** (the prototype's own non-regression check): the `mux` arm, same run — 1-stream 9.376 vs 9.259 (inside spread), 8-stream +3.2% inside spread, churn +0.1%, CPU +0.7%, RSS +2.1%. The single-rep 64-stream cell showed base 19.38 vs stripe 16.89 (-12.9% disjoint in one round), which the claim rule flags as a regression — a focused 5-round re-measurement (`results-mux64-focus.json`, 16 arms, audited clean) re-fired the same claim (-12.8%), so it was not a three-round artifact. The cell is single-rep by construction and **bimodal on both binaries**: over the 13 rounds combined, base spans 16.46-19.54 (median 19.1, 2 of 7 rounds in the low mode) and stripe 16.77-19.55 (median 16.8, 1 of 6 in the high mode) — the distributions overlap completely, so per §10 the difference is a mode-frequency shift inside the cell's own span, not a claim in either direction. The multi-rep cells of the same arm (1-stream, 8-stream) show the prototype inert. Follow-up, recorded: the bench's 64-stream scale point is single-rep, which makes that cell structurally undecidable; making it multi-rep is a bench change on its own.

**Reading:** the tax is real and striping removes more than half of it on the strongest cell, at a bounded cost (churn -7.7%, CPU +40.8%, RSS +8.7%, sub-millisecond latency +5% — all median-only, none claimable). The mechanism works as designed (ceiling ×K, window ×K, cpu/kframe ÷K), and the residue is the reassembly path, not the protocol.

## Noise session resume: the setup-cost experiment

The premise (session resume saves the handshake's cost) was measured before it was built: a release-mode phase attribution over the production pattern (`noise_stream.rs`, `handshake_phase_attribution`, `7b19457`) showed the DH turns at ~97% of the ~445 µs a pair costs on the state machine (initiator turn 1 `e,es` ~122 µs, responder turn `e,ee` ~233 µs, initiator turn 2 the `ee` read ~72 µs; everything else <1 µs).

The implementation (`src/transport/noise_resume.rs`, opt-in via `[transport.noise] resume = true` on both sides):

- **Ticket.** After a full handshake the responder seals the session's handshake hash with a key derived from its Noise static private key and returns it as the first exchange record; the client caches it per server static key. The client's first record after the handshake is a one-byte `want` — the exchange runs on *every* full handshake, on both sides, because that byte is what tells the responder an exchange follows (gating it per side deadlocks). What the configuration decides is whether a ticket is *issued* and whether a cached one is *attempted*.
- **Resumed connect** (selector `0x02`): the client sends `[ticket][client_nonce][MAC]`, the responder verifies (open the seal, check the 24 h TTL, check the MAC, reserve the nonce) and answers `[status][server_nonce][MAC]`; both derive fresh record keys with HKDF-SHA256 over the cached hash and both nonces and speak ChaCha20-Poly1305 with the Noise nonce convention. Old servers reject the unknown selector cleanly, and the client falls back to a full handshake on a fresh connection.
- **Replay/FS.** A repeated `(ticket, client nonce)` is rejected (the store reserves the nonce), a captured request cannot be completed without the cached hash, and the tradeoff is documented in docs/transport.md: resumed sessions' keys derive without a fresh DH, so a later static-key compromise reaches them — hence opt-in.

**Measurement** (`noise_stream.rs`, `resume_setup_cost`, release build, N=200, in-process pairs over a tokio duplex with the exchange records and IO included):

| shape | per pair |
|---|---|
| full handshake + ticket exchange | 442.70 us |
| resumed exchange | 38.52 us |
| saving | 404.18 us (91%) |

The saving matches the phase attribution (the DH turns are the difference), and the resumed pair's 38.5 us is symmetric crypto plus the two records. End-to-end correctness is covered by the integration scenario `noise_session_resume` (a client restart over the noise fixture with `resume = true`, engaging the selector-0x02 path — the test run logs 16 resumed sessions for the control channel, pools and tunnels), plus unit tests for the ticket seal/unseal, tamper, staleness, replay and decline paths.

**What is not yet measured**: a bench-level reconnect-latency axis. The bench's probes dial the exposed port and never tear down a control channel, so the 404 us saved per reconnect has no bench cell today; the in-process probe is the evidence (the same standard the connection-setup allocation probe `3297d65` was held to). A cold-start/reconnect probe would be the follow-up, and it belongs with the single-control-channel item.

## How to A/B on this branch

Sequential before/after runs are **not usable** — the path drifts between epochs, which is what hid both a real regression and a harness bug for a whole session. The Soak model's answer is a single interleaved run:

```bash
# both builds freshly compiled; one screen run over one path class
just soak --test=screen --path=loss1 --streams-max=8 \
     --ab /path/to/bin-a,/path/to/bin-b --out results-screen.json
just soak-check --screen results-screen.json   # per-step verdict
```

The verdict names a CLAIM only where every step favours the same build by more than the threshold; anything else is *directional*. The retired matrix's `--ab` mode and its `MOLEHILL_REPS` / `--cells` interface are gone with the matrix — this section is the current recipe. Details in [docs/release.md](docs/release.md) ("Comparing two builds (development screening)").

### Environment notes (this host, measured 2026-09-22/23)

- **Verify `iperf3` before a long run.** The container's apt layer dropped the `iperf3` package twice mid-session without a reboot. The bench then fails *cleanly* — every test records the `Backends: … [Errno 2] iperf3` error with its reason (continue-on-error, no fabricated numbers) — but a whole run spends its hour producing nothing.
- **/tmp is periodically wiped.** It took one 35-minute final A/B with it (every checkpoint of the run). The checkpoint now recreates its output directory (`baa4eab`), but keep `--out` and logs under `~/tmp` or the repo regardless.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python child, which keeps executing and holds the bench lock — a later invocation then exits immediately with "another bench run is active". Let the orphan finish (or reap it) before starting the next run.

## The Soak benchmark model (2026-09-25): what replaced the matrix

The v0.9.0 cycle replaced the measurement model. The matrix (`benches/scripts/bench/`, `results-v*.json`, `assets/benchmark-*.png`) is deleted, not wrapped: no migration path, no dual mode. Method and ritual: docs/release.md, "Benchmarks: per-tag ritual"; the model as shipped: `benches/scripts/soak/`.

**The model.** The unit of measurement is a *workload over time*, not a cell: one tool, one process pair, driven through an identical client-side workload (an interactive stream — a fresh TCP connection per ping, the SLO instrument — N bulk TCP streams, C short connections per second and one UDP session) while the path follows a scripted stage schedule (`clean 150s → rtt100 → loss1 → loss5 → rate100 → rate20 → jitter → clean 150s`), applied in place (`tc qdisc change` per tool class) so the tool's session is never rebuilt. Test types: `capacity` (ramp the load until the interactive stream breaks the SLO), `rrul` (N = cpu count; the interactive stream's RTT distribution over time), `soak` (the drift/leak axis), `cost` (CPU-seconds per carried Gbit at a fixed operating point) and `screen` (a fast development A/B with the two builds interleaved inside every load step and a sequential decision). Everything measured is externally observable, which is what keeps the peers measurable with the same workload — and keeps molehill's internal counters out of the comparison.

**Three decisions that came out of building it (each measured, not argued):**

1. **The SLO instrument must not perturb what it measures.** The first runner put the interactive echo backend in the harness process; under 20 bulk streams the GIL made the interactive stream read ~2 s where an isolated one reads milliseconds. Both probes now run in their own process (they are also the tool's forward targets, so the harness is out of the measured path entirely).
2. **Clean stages must not cross a class.** Always classifying a tool's traffic into an HTB class cost ~25% of throughput on the clean stages (12.5 → 9.3 Gbit/s) and the netem child another ~20% — a model that shaped the clean stages would measure the shaper, not the tool. Clean stages now keep the filters in the unshaped default class; only a degraded stage puts the tool in its class. `tc filter replace` ADDs a second filter when the match differs (the first match wins), so a stage change deletes the tool's priority group before re-adding — the first version of that rewrite silently left traffic in the previous class (an unshaped interactive stream on a "rtt100" stage, caught by comparing against the retired matrix's historical 1001 ms).
3. **Batching is validated, not assumed.** The self-check (the same tool alone vs inside a two-tool batch) showed the per-stage numbers inside the claim rule, so `batch=2` on this 20-core host is sound; the batch size is derived from the CPU budget and recorded in the meta.

**The v0.9.0 release run** (4 tools x 8 stages, batch 2 of 20 cores, `results-soak-v0.9.0.json`, charts `assets/soak-v0.9.0*.png`): every tool degrades under the shaped stages and **every tool recovers** on the return-to-clean stage (molehill 17.0 -> 2.4 -> 20.1 Gbit/s bulk; frp 5.9 -> 2.2 -> 5.9; rathole 16.9 -> 2.5 -> 17.0; nps 0.14). Two findings the retired matrix could not have produced:

1. **The control plane must stay unshaped.** The first version of the harness classified the tool's control channel into the shaped class; on the 100 Mbit cell the heartbeat timed out after 40 s and the tool was wedged for the rest of the run (measured in the tool's own log). The shaper now classifies data-plane ports only.
2. **Wedges are now first-class data.** Every tool shows flat segments on the shaped stages (molehill rtt100: 7 segments, longest 13.2 s; rathole rate100: 8, longest 14.6 s) and all recover — recorded as duration instead of a `None`, which is what a matrix of cell averages cannot express.

The model's first open question is the memory axis: molehill's server RSS slope is ~4.9 **MiB**/min over the run (measured 4932 KiB/min; frp -0.2, rathole +0.06, nps +0.3 MiB/min), inside the gate's ±50 MiB/min limit but large enough to want a dedicated `soak`-type run before it is called noise. (An earlier revision of this line said GiB/min, which would have been 100× the limit it claims to be inside — a unit slip, corrected 2026-09-25.)

**Open:** the `screen` verdict decides on per-step aggregates; a full sequential decision (pilot → spread → claim or budget) needs the per-sample split between the two builds, which the runner does not record yet. The `cost` axis is computed per stage but has no gate history. The dry-run tool list is molehill + frp + rathole + nps on the default arm only; the variant sweep (noise / mux1 / kcp4) and a multi-hour `soak` drift run are the follow-up measurements. The release gate had no same-model baseline (this is the first Soak release), so v0.9.0 is gated by the absolute SLO — stated in docs/release.md.

## Legacy state (2026-09-11; the entries below are history, re-checked 2026-09-25)

- `v0.8.0` and `v0.8.1` are released; `v0.8.1` is the control-channel teardown fix (a service whose control channel ended kept its public port bound until a new registration took it over — recorded in CHANGELOG.md's `## [0.8.1]` section).
- The retired matrix's method was revised 2026-09-10/11, and its v0.8.0 baseline was re-measured in full from it on host `0b073ddbf222` (52 arms, zero holes). Those result files are no longer in the tree; their numbers live in git history and the v0.8.x release notes.
- Only same-model results are comparable: the matrix's cell averages (v0.8.x, git history) and the Soak model's time series (v0.9.0 onward) are different instruments, and within the Soak model only same-schema, same-host runs compare — the results meta records `workload_version`, the host and the harness revision for exactly that reason.
- `src/transport/udp_batch.rs` is the only `unsafe` site in the codebase (the recvmmsg/sendmmsg FFI), audited 2026-09-20 and reduced on 2026-09-24: the send address is now built by `socket2` and every iovec pointer is derived from a bounds-checked index, so the unsafe left is the two zeroed C templates, the kernel-ABI address read, the two `mmsg` calls, and the `Send`/`Sync` impls (19 items -> 8). The same pass cut the production-code lint waivers from 49 to 14 — the record is in CHANGELOG.md's `## [0.9.0]` section.
