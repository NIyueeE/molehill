# HANDOFF: Working State & Future Work

> State as of 2026-09-23. Branch `perf/data-path-optimizations` (71 commits
> ahead of `main`, pushed, **not merged**) contains the complete mux-engine
> migration: rust-yamux 0.14 is now an in-repo, tokio-native engine
> (`src/mux/`), and the mux transport drives it directly
> (`src/transport/multiplex.rs`, no Compat shim). Shipped work is recorded in
> [CHANGELOG.md](CHANGELOG.md); design details live in
> [docs/internals.md](docs/internals.md). This file tracks what is open and
> what was decided.
>
> **Read this first (2026-09-23, late session): the `--ab` harness was
> broken.** `bench.py --ab` assigned `knobs.molehill_bin` per arm but every
> spawn site read a frozen global, so every `--ab` run on this branch —
> including both "final cumulative A/B" runs — measured the DEFAULT binary
> against itself. Their non-overlapping claims are withdrawn; see "The
> `--ab` harness bug" below. The fix is `064c55a`; the first A/B taken with
> the fixed harness is the stripe experiment ("Stripe A/B (K=4)").
>
> **Two follow-up harness fixes (2026-09-23, KCP session), both label-layer
> only:** `2754142` — two worktree binaries share the basename `molehill`,
> so the `--ab` label suffix collided and the two sides silently overwrote
> each other (`ab_compare` saw no pair at all); `5d1d9dd` — `ab_compare`
> sorts a pair by label, so a verdict's first-printed value is NOT
> necessarily the left `--ab` entry, and every `--ab` file now records
> `meta.ab_bin_paths` (label → path) which the verdict tool prints before
> the table. **Read every older `--ab` verdict in this file with that
> mapping in mind** — the KCP Phase 1 A/B was misread exactly this way and
> its winning change was briefly reverted ("Phase 1 A/B — LANDED", the
> misread post-mortem).

## Where things stand

**The migration is complete on its own terms and the engine is clean.**

- The cumulative branch-vs-`main` comparison was re-run end to end on
  2026-09-22 with the current bench (interleaved 3-round A/B, loopback +
  loss1_rtt10 + rtt100 + loss5_rtt100, mux/noise/mux1/kcp4 + the mux-off
  control): latency at parity or better on every arm/cell, throughput at
  parity with the favourable movements at the shaped cells, memory/CPU
  inside the accepted band, and no claimable regression that survives a
  focused re-measurement — see "Final cumulative A/B" below. **Caveat
  (2026-09-23): that run's `--ab` interleave spawned one binary on both
  sides (the harness bug), so its arm-to-arm movements are interleaved
  same-binary noise, not binary differences.** The older
  `results-final-ab-2026-09-21.json` claim (mux + mux1, 24 paired ranges, 21
  overlapping) is the 2026-09-21 session's record — also taken through the
  broken `--ab`.
- `main` (`8584945`) is untouched and releasable; nothing here is merged.
- The stripe prototype landed (`f91e6bb`) with a measuring A/B that answers
  the single-stream-ceiling question — "Stripe A/B (K=4)" below. With the
  ceiling partly recovered, the two structural gaps of the optimization
  route (multi-tunnel scheduling, the host socket cap) are addressable by
  configuration rather than by engine work; the route table below is
  annotated where that changes the picture.
- Known bug found and fixed on the way: the `SelectAll` → `Vec` conversion
  dropped the receiver removal, so a client serving many short-lived
  connections polled thousands of dead stream receivers per poll
  (`a424ccc`).
- KCP was re-measured end to end with the current bench on this host and
  three candidate optimizations were tested by interleaved A/B; all three
  are closed with measurements (none landed) — "KCP on the current bench"
  below. Five bench feedback-side issues were found and fixed on the way:
  the inverted churn direction in the verdict tool, the lexical
  results-file ordering, the RSS key that never matched (the memory axis
  was missing from every A/B verdict), the udp ping waiting out its full
  wall bound, and the checkpoint dying on a wiped output directory (the
  "What landed" table lists them with commits). A sixth, the `--ab`
  binary-swap bug above, was found while A/B-ing the stripe prototype.
- **2026-09-23 follow-up session on the KCP data path** (plan: attack the
  kcp4 arm's structural per-segment cost and feedback latency with the
  attribution table as the target): the `MOLEHILL_KCP_STATS` attribution
  counters landed (`66fb0d2`), send batching + receive coalescing landed
  on its A/B after a misread revert (`ec9b322`), the ARQ loss-event pacer
  was closed by measurement (`bea6317` reverts `eb4a491`), and the rtt100
  8-stream wedge was attributed to the stream-establishment layer above
  KCP and recorded as a known structural limit (`416aafe`). Sections
  "KCP attribution (Phase 0)" through "Phase 3" below. Two more bench
  feedback-side issues were found and fixed on the way, both label-layer:
  the `--ab` label collision between same-named worktree binaries
  (`2754142`) and the unrecorded label↔path mapping that let a verdict be
  read backwards (`5d1d9dd`).

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

## Backlog

### Next recommended improvement: single control channel per client

The client still keeps one control connection **and** one mux tunnel per
service. Consolidating to one control connection per client (plus the shared
tunnel pool) is mostly plumbing now that the mux engine is stable, and gives
another order-of-magnitude FD/handshake reduction for many-service clients.
Per-service `mode`/`count`/`carrier` overrides landed in 0.8, so mixing data
paths per service already works without waiting for this.

**Scoping notes (2026-09-23, from the stripe session):**

- *Protocol shape.* The control channel must stay 0.8.x-interoperable, so
  the consolidated form is a **new hello variant** (e.g.
  `ControlChannelHelloMulti`), not a silent change to the v3 grammar: the
  client announces which dialect it speaks and the server adapts per
  connection, exactly like `DataChannelTunnelHello` does on the data
  listener. Commands then need a service id (`CreateDataChannel`,
  `HeartBeat` carry one), and the server keeps per-service state (pool
  task, heartbeat timer, data-ch request channel) keyed under one client
  session. The auth handshake stays per connection (the nonce/token
  exchange is what binds the session).
- *Measurement.* The bench's probes all dial the exposed port, so a
  control-channel consolidation is invisible to them — the honest axes
  are FD/handshake counts per client and a cold-start/reconnect probe
  that has to be ADDED (e.g. time from client start to every registered
  port answering, for an N-service client). Per §10 ("a metric without
  contrast is not a measurement"), the change ships with its probe or not
  at all; the FD-count axis is directly measurable from the client
  process's `/proc/<pid>/fd` in-process or via the bench's process table.
- *Handshake resume (the latency half).* The Noise handshake is a full
  pattern run per connection (232 µs/pair measured, DH-dominated). A
  resume path (cache the handshake state, prove possession with a MAC on
  reconnect, server-issued ticket + nonce for replay protection) is a
  crypto-protocol change on top of `src/transport/noise.rs`, and the PSK
  support already present (`psk`/`psk_location` in `[transport.noise]`) is
  NOT a substitute — it still runs a full DH exchange, it only adds a
  second authenticator. Design it as its own commit with its own probe
  (reconnect-time benchmark), after the control-channel consolidation
  lands.

### Open items

- [ ] HTTP API for configuration (hot reload is files-only today)
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Per-service bandwidth limiting (token bucket around the copy loops)
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

## Transport layer: what was compared, and where KCP stands

Four carriers were implemented, integration-tested end to end and measured
against each other (the QUIC arm's history survives in the local
`archive/transport-test` tag):

| Arm | Topology | Status |
|---|---|---|
| 0 | 1 TCP tunnel | `count = 1` |
| 1 | N TCP tunnels (bench N=4) | **default (`count = 4`)** |
| 2 | N KCP sessions (bench N=4) | merged, optional (`carrier = "kcp"`) |
| 3 | 1 QUIC connection (quinn) | **archived** — behind on loss cells (quinn "too many gaps" at rtt10), no peer auth on the QUIC leg, and N×TCP measured better in every comparable cell |

**KCP's measured position:** it loses throughput to a TCP carrier in *every*
cell (loopback ~2.5 vs ~11 Gbit/s 1-stream; loss5/rtt100 ~0.03 vs ~0.28) and
wins the latency axis on the worst cell (udp p50 601 vs 826 ms, HoL max gap
1515 vs 1808 ms over the noise-TCP arm). So KCP is **not an optimization
direction** — it is a capability with a defensible niche: UDP-only paths
(TCP blocked or throttled by firewall/NAT) plus latency-first interactive
traffic on high-loss, high-RTT links. Trying to make it win on throughput
would mean re-tuning an upstream congestion controller that is deliberately
not yamux-shaped, and the numbers say that is not where its value is.
(Those figures are the v0.8.0 matrix's; the section below re-measures the
same cells with the current bench on a second host.)

### KCP on the current bench (2026-09-22, host `1cb438346ebe`)

The v0.8.0 matrix is the last full KCP characterization and it predates
the bench's measurement revision; the `results-opt-*` KCP files are from
2026-09-08, pre-revision, and two of them were taken on *different hosts*
(so the "+29% window doubling" was a cross-host pair — invalid per §10).
The branch re-measured KCP with the current instrument
(`results-opt-kcp-baseline.json`, 2 reps × 2 rounds; the experiment files
carry 3 × 3). Noise arm beside it, same cells:

| cell | kcp4 1-str | noise 1-str | kcp4 8-str | noise 8-str | kcp4 udp p50 | kcp4 loss% | noise loss% | kcp4 RSS | noise RSS |
|---|---|---|---|---|---|---|---|---|---|
| loopback | 3.1-3.7 | 6.1-6.8 | 1.4-6.3 (bimodal) | 20-21 | 0.43 ms | 0 | 0 | 95-182 MiB | 29 MiB |
| loss1_rtt10 | 0.46 | 3.80 | 0.69-0.79 | 12.9 | 60.6 ms | 2.5-4.0 | 5.0 | 78-155 MiB | 38 MiB |
| rtt100 | 0.011-0.057 | 0.667 | wedge (None) | 1.06 | 600 ms | 0 | 0 | 28-54 MiB | 17 MiB |
| loss5_rtt100 | 0.038-0.057 | — | None | — | 610-704 ms | 16.5-21.5 | — | 47-54 MiB | — |

Three findings that matter:

1. **The UDP echo path is *better* over KCP on loss cells** — loss1_rtt10
   2.5-4.0% lost vs noise's 5.0%, max gap 55-60 ms vs 80 ms, jitter 1.16 vs
   1.32 ms. The pacer's smoothed emission keeps the forwarder's queues
   shallow where TCP's burstiness overflows them (the hub drops on full).
   That is KCP's niche measured, not argued.
2. **rtt100 is where KCP falls over**: 0.011-0.057 Gbit/s 1-stream (7× rep
   spread) and the 8-stream test times out in *every* round — the v0.8.0
   matrix recorded the same wedge, so it is not new, but the cell cannot
   carry a claim for either engine.
3. **The loopback 8-stream cell is bimodal within one arm** (rep0 often
   0.4-0.6 Gbit/s, later reps 2-3.2): a cold-start mode. This is what made
   the pre-revision "-3x" reading possible, and it is why no KCP loopback
   8-stream number above is quotable as a median.

The three optimization attempts this session (pacer recovery, doubled
windows, 5 ms interval) are all closed with measurements in the backlog
section; the files are `results-opt-kcp-{pacer,w4096,int5}.json`.

### KCP attribution (Phase 0, 2026-09-23, host `0d…`)

The counters above are now an instrument: `MOLEHILL_KCP_STATS=1` makes
every molehill process log a per-second `kcp-stats` line (datagrams
in/out, retransmits, acks and SACKs sent, pump rounds, and coarse
per-phase milliseconds — input / deliver / writer / output / update;
commit `perf(kcp): attribution counters`). One loopback kcp4 run with
the bench (1 rep × 8 s, both processes instrumented;
`~/tmp/results-kcp-stats.json`, arm logs
`molehill_kcp4_loopback.{client,server}.log`) measured the cells at
3.14 / 2.63 / 7.44 Gbit/s (1/8/64-stream — inside the 2026-09-22
baseline's ranges) and attributes the pump's per-segment cost in the
1-stream window (~295.8 K segments/s each way, **zero** retransmits):

| side | per segment | input | deliver | writer | output | update |
|---|---|---|---|---|---|---|
| sender (bulk out) | **5.63 µs** | 0.08 | 2.46 | 0.62 | 2.46 | 0.01 |
| receiver (bulk in) | **1.68 µs** | 0.41 | 0.77 | 0.00 | 0.49 | 0.01 |

All figures are **wall-clock** per pump phase (the timers wrap the
phase's awaits, so `output` includes the pacer's writable park and
`deliver` any descheduling). Whole-run check against the process CPU:
the sender's five phases sum to 7.95 µs per sent segment against
9.8 µs of measured CPU per sent segment (181.9% avg × 82 s / 15.19 M
segments) — the phases explain ~81% of the sender's CPU, clearing the
≥70% attribution bar; the receiver's remainder is the Noise/yamux/iperf
stack, a layer the pump phases do not cover by design.

Three findings that set the targets:

1. **The sender's cost is the wire drain + the writer, not the ARQ
   update** (0.01 µs/segment). *Corrected 2026-09-24 — the original
   reading of this finding attributed ~2.46 µs per sent segment to
   delivery and called it an anomaly; that was an instrument bug (the
   deliver timer's binding outlived its statement and booked the rest
   of the pump round into the phase), and the number was mostly the
   wire drain. The re-based table is in "Zero-copy route: the
   attribution re-baseline" below: with the fix, the sender's delivery
   phase is 0.36 µs/round and the wire drain 2.55 µs/segment — the wire
   drain is the cost, and Phase 1's send batching (L2's predecessor)
   plus L2/L3 attack it.* `drain_dgrams`'s per-datagram pacer check +
   `sendmmsg` batching is the other half.
2. **A loss signal the pacer never sees.** The 64-stream window shows
   the sender's retransmit rate climbing 2.4% → 34% while the out-rate
   decays 638 K → 150 K segments/s — a collapse driven by datagrams
   that never arrive, while the pacer's only cut signal remains the
   2.5 s PONG timeout. The 1- and 8-stream cells retransmit nothing.
   Whether the drops are kernel socket-buffer overflow (both sockets
   request 32 MiB, granted by this host's `rmem_max`) or the pacer's
   own token denials is not yet attributed; the counters now carry the
   timing to attribute it, and every retransmit is a loss event with a
   timestamp — the input Phase 2's ARQ-driven pacer needs.
3. **The counters are zero-cost enough to leave in the bench**: the
   same run's arm totals (CPU 181.9% server / 154.8% client) sit inside
   the 2026-09-22 baseline's band, and the line format is additive (a
   parser that does not know `kcp-stats` ignores it).

### Phase 1 A/B (2026-09-23): send batching + receive coalescing — LANDED

The attribution table's two largest phases (delivery 2.46 µs/segment and
the wire drain 2.46 µs/segment on the sender) were attacked by one
commit (`a38bf4b`, re-landed as `ec9b322` after the misread revert
below): outbound datagrams stage in a reusable ~46 KiB buffer and cross
the pump channel as ONE message per batch (closed at 32 datagrams, the
staging cap, or the engine's flush boundary — the engine's `flush` now
calls `Output::flush`), and the reader side coalesces consecutive
segments into one channel message per ~16 KiB. Both are pure
amortization: same datagrams, same byte stream, same ARQ semantics; a
pacer denial drops only that span. The new `blobs_out` counter measured
the mechanism in the bench itself: **7.8-11.1 segments per reader
message** (was 1), per-segment phase cost down on both sides (sender
5.92 → 5.59 µs, receiver 1.78 → 1.71 µs in the 1-stream window).

One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback /
loss1_rtt10 / rtt100, arms kcp4 + the loopback mux-off control,
binaries `66fb0d2` vs `a38bf4b`, both SHAs verified; data
`results-ab-kcp-batch.json`, audited: 0 cell errors, one transient
iperf3 hole (ab2 head / loopback `mixed bulk_gbps`, control socket
closed — one metric of one arm, typed reason recorded), the rtt100
8-stream gap documented on both sides as the known wedge):

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

The gate ("cpu/段 or 1-stream 出现非重叠正向才记为胜") is met on the
1-stream and 64-stream cells with CPU and cpu/kframe favourable; the
only non-overlapping unfavourable is the bimodal 8-stream cell, which
the 2026-09-22 record already declares unquotable. The mux-off control
moved +12.1% on its 1-stream cell (the arm that runs first measures
faster on that cell) — the loopback 1-stream kcp4 claim is read against
that floor, and it exceeds it.

**The misread revert (recorded because it cost a day):** this A/B was
first read with the two binaries swapped (`ab_compare` sorts the pair
by label, and the two worktree binaries share a basename, so the labels
are content hashes — nothing tied them back to the `--ab` command
line). The +6.6% 1-stream win was read as a -6.2% regression and the
change was reverted (`ab5eb2c`); the mapping was caught when the
Phase 2 A/B's parent arm measured a 200× collapse that the "parent"
label could not explain. bench.py now writes `meta.ab_bin_paths`
(label → resolved path) into every `--ab` file and `ab_compare` prints
it before the table (`5d1d9dd`); this change was then re-landed by
reverting the revert. The data file never changed — only the reading.

### Phase 2 A/B (2026-09-23): ARQ loss-event pacer — DROPPED

The pacer's only cut was the 2.5 s PONG timeout, while the engine knows
a loss the moment it retransmits. `eb4a491` fed that signal in: a
per-session retransmit total (exposed as `Kcp::retransmits`) cuts the
rate 0.75× per loss event, cooled to once per RTT (min 100 ms), with a
SACK notification cutting through the same path. Its A/B (`eb4a491` vs
the same parent, data `results-ab-kcp-pacer.json`) plus a focused
one-round re-measurement (`results-ab-kcp-loss1-focus.json`, both
binaries, same epochs) read with the corrected mapping shows the change
**breaks the cells it targeted**:

- loss1_rtt10 1-stream: **0.0019 vs 0.4325 Gbit/s** (head vs parent) —
  a 200× collapse; the 8-stream test fails outright (iperf3 control
  socket closed). The head's loopback 8/64-stream cells time out in all
  three rounds of the full run.
- The mechanism is the timescale mismatch the plan itself flagged: on
  a 1%-loss path the retransmit stream is continuous, so the cooled cut
  fires every RTT/100 ms and walks the rate to `PACER_MIN` within about
  a second, while the 1.05×-per-4-clean-PONG recovery needs minutes to
  climb back. The pacer pins at the floor for the whole test — exactly
  the "重传风暴把速率打到 PACER_MIN" failure the cooldown was meant to
  prevent, because the cooldown bounds the cut *frequency* but nothing
  bounds the cut *depth* over a sustained loss stream.
- Where there is no loss (loopback 1-stream, zero retransmits) the new
  path never fires — the cell is inside spread. The inertness holds
  exactly where the feature has nothing to do.

The head's low CPU/RSS in that run are the mirror of the same fact: its
tests stalled, so there was no work to measure. Reverted (`bea6317`).
**This closes the S3 route with evidence**: an ARQ-driven cut cannot
work while recovery stays on the PONG-probe path — the two timescales
differ by three orders of magnitude on the cells in question. A design
that could: cut toward a remembered goodput (not toward a floor) with
an ACK-clocked multiplicative recovery, i.e. make the loss signal set a
ceiling the probe can re-approach, not a rate the probe must rebuild
from the floor. Recorded, not attempted — the same conclusion the
closed pacer-recovery experiment reached from the other side ("the
pacing rate is not the binding constraint in any cell this bench
measures").

### Phase 3 (2026-09-23): the rtt100 8-stream wedge, attributed

The plan's last conditional step: with the ARQ-pacer route closed, the
rtt100 8-stream cell still wedges (None on BOTH binaries in the Phase 1
A/B — the 2026-09-22 re-measurement and the v0.8.0 matrix recorded the
same), so it was re-measured focused and attributed with the Phase 0
counters: `focused_run.py --variant kcp4 --cell 0/100 --streams 1,8
--secs 10 --reps 3`, binary `ec9b322` (the re-landed Phase 1), data
`results-kcp-rtt100-focus.json` (raw per-rep iperf3 JSON kept; the arm
logs' `kcp-stats`/`mux-stats` lines are the attribution source).

- **1-stream works**: 0.096 / 0.089 / 0.027 Gbit/s sent across three
  reps (0.04 / 0.037 / 0.0074 received-own-window) — inside the
  baseline's 0.011-0.057 spread. The cell's own retransmit rate is
  25-60% at this rate (the ARQ at a 100 ms RTT is the long-known weak
  point), but the cell completes.
- **8-stream fails all three reps**: rep0 genuinely (iperf3 "control
  socket has closed unexpectedly" after 8.4 s, 0 bytes sent and
  received), and reps 1-2 then hit "the server is busy" — the wedged
  single-test iperf3 backend §10 warns about (one failure poisoning the
  later samples). The wedge itself reproduces; the rep1/2 nulls are
  instrument contamination, recorded as such.

**Attribution (the counters settle it):** during the 8-stream window
the KCP pumps are IDLE — the sender emits 0-6 datagrams/s at the
~1200 rounds/s idle cadence, no retransmit storm, no send-queue
buildup — while the mux framing counters show control frames crossing
at the 100 ms RTT cadence. The tunnel is alive; the eight streams'
DATA never starts. The brief retransmit-heavy burst at the end
(7.5 K retransmits/s, 50%+ of the out-rate) is the sessions dying, not
the cause. So the wedge is **not** the ARQ or the pacer (the plan's
alternative hypothesis) — it is the stream/data-path ESTABLISHMENT
layer above KCP: one session serializing eight yamux stream setups
over a 100 ms path (S1), consistent with the ceiling finding in the
mux route ("per-frame cost scales with tunnel count"; count=4 is the
largest structure this arm was measured at).

**Conclusion — recorded as a known structural limit, per the plan:**
the kcp4 arm's rtt100 8-stream cell cannot carry a claim for either
engine and is not worth further investment from this route; a fix
belongs to the multi-stream scheduling structure (the same place the
mux route's residual gap lives), not to the KCP data path. The cell
stays documented as None-with-reason in future runs, and the
iperf3-backend restart discipline (§10) is the reason rep1/2's nulls
must not be quoted.

## Zero-copy route: the full-path copy map (2026-09-24)

A code-level review of every hop on the data path (all arms), counting
userspace copies per application byte. The headline: **the copy burden
is asymmetric across arms — the TCP arm pays W2/W4/K1-K4 inside the
kernel, while the kcp4 arm pays 4-5 extra userspace copies per byte
(write: 4, read: 2-3) in molehill's own code.** That is a structural
part of why kcp4 loses every throughput cell, and it is attackable
without touching the wire format.

### The map (per app byte; "yes" = removable in this program)

| # | copy | where | removable |
|---|---|---|---|
| W1 | kernel → `copy_bidirectional_with_sizes` stack buffer | src/core/server.rs:1260 | no (syscall; that buffer is also the write buffer) |
| W2 | stack buffer → yamux frame body (`Vec::from(&buf[..k])`) | src/mux/connection/stream.rs:365 | **yes (M1)** |
| W3 | frame body → Noise record ciphertext | src/transport/noise_stream.rs:447 | no (AEAD; ciphertext must be contiguous with the 2 B length header) |
| W4 | ciphertext → kernel | tokio | no (syscall) |
| K1 | record → `Bytes::copy_from_slice` for the out_tx channel | src/transport/kcp.rs (`KcpStream::poll_write`) | **yes (L3)** |
| K2 | app data → segment data (`send()`) | src/kcp.rs:549 | **yes (L3, deepened)** |
| K3 | segment → engine `self.buf` (encode) | src/kcp.rs:197 | **yes (L2)** |
| K4 | datagram → DatagramOut batch staging | src/transport/kcp.rs (`DatagramOut::write`) | **yes (L2)** |
| R1 | kernel → Noise `bufs.scratch` (record accumulation) | src/transport/noise_stream.rs | no (record framing needs the length header first) |
| R2 | scratch plaintext → caller buffer (decrypt) | noise_stream.rs (incl. the e463391 fast path) | **yes (N1: read the whole record into the caller's buffer and decrypt in place)** |
| R3 | mux frame body → stream read buffer | src/mux/connection.rs (`into_body`) | no (already a move) |
| R4 | stream buffer → `copy_bidirectional` stack buffer | tokio | no (that copy IS the socket write) |
| K5 | recvmmsg batch buffer → owned `Bytes` per datagram | src/transport/kcp.rs (ingress) | no (batch buffers are reused; per-datagram allocation would be worse) |
| K6 | datagram → segment data (`input()` parse) | src/kcp.rs | no (ownership copy; input buffers are transient) |
| K7 | segment → `recv_buf` | src/transport/kcp.rs (`deliver_recv`) | **yes (L1)** |
| K8 | `recv_buf` → coalesce blob | src/transport/kcp.rs (`deliver_recv`) | **yes (L1)** |
| S1 | read chunk → stripe frame body | src/stripe.rs:170 | **yes (S1: `send` takes `Bytes`, the read buffer becomes the frame)** |

Not attempted (recorded so it is not re-litigated): the syscall
boundaries; the write-path AEAD copy (W3); splice/sendfile (already
measured and not recommended); io_uring / UDP `MSG_ZEROCOPY` (1.4 KiB
datagrams are below the threshold and conflict with the batch design);
the L5 frame-body pool (dropped by measurement — cpu/frame identical —
note M1 removes the COPY, which L5 did not); the QUIC-style segment
ring-buffer that would remove per-segment allocation (the rewrite the
KCP plan excludes).

### Sequence (one commit + one single-variable A/B per link)

| link | removes | arms affected | risk |
|---|---|---|---|
| **L1** KCP receive owned-segment (`recv_owned`, `ReadBatch { parts: Vec<Bytes> }`) | K7+K8 | kcp4 | low |
| **L2** KCP send two-iovec datagrams (header in staging + payload by reference, `msg_iovlen = 2`) | K3+K4 | kcp4 | medium (engine Output boundary + sendmmsg) |
| **L3** KCP write owned records (Noise produces owned records; `KcpStream` owned write; deepened to `send_owned`) | K1+K2 | kcp4 | medium |
| **M1** mux frame body owned write (`Frame.body` Vec→Bytes + owned write API + an owned proxy loop) | W2 + one alloc/frame | all mux arms | medium |
| **N1** Noise in-place record decrypt on the read fast path | R2 | noise, kcp4 | medium |
| **S1** stripe owned chunk | S1 | stripe (opt-in) | low |

Order rationale: kcp4's copy density is highest and the arm is isolated
(the cleanest single-variable A/B); L2/L3 share the engine Output
boundary and are consecutive; M1/N1 touch the default arm and the
vendored engine / Noise state machine, after the technique is proven on
KCP; S1 last (opt-in arm). Expected magnitude is honest: L1 ≈ 0.2-0.3
µs per received segment (~15% of the receiver's cost, visible at the
8/64-stream and loss cells), L2+L3 ≈ 0.3-0.4 µs per sent segment (~6%,
the loopback 1-stream cell is sender-bound and is where a claimable win
would show), M1/N1 ≈ 2-5% CPU on the default arms. Not a step change;
the step change is the excluded QUIC-style rewrite.

### The attribution re-baseline (2026-09-24, loopback kcp4, 1 rep × 8 s)

The deliver-phase split (below) surfaced a **bug in the Phase 0
instrument itself**: `let _t = PhaseTimer::new(&KCP_NS_DELIVER);` is a
named binding, and named bindings drop at the end of their SCOPE, not
their statement — so the "deliver" timer booked everything from the
delivery call to the end of the pump round (the SACK check, the ack
flush, the wire drain, the liveness tail) into the delivery phase. The
deliver phase is now block-scoped and the split is coherent (the inner
timers sum to the phase). Re-based 1-stream window (sender ~296.7 K
segments/s out, receiver ~296.6 K in, zero retransmits both sides; four
sessions per process):

| side | per segment (µs) | per round (µs) | notes |
|---|---|---|---|
| sender | writer 0.60, **wire drain 2.55**, input 0.09, update 0.01 — pump body ≈ 3.26 | deliver 0.36 (spill 0.064 + recv 0.199), writer 59.7, wire drain 252.4, input 8.9, update 0.9 | rounds/s 3002 (Phase 0 measured 4842 on the pre-Phase-1 code: send batching cut pump rounds ~38%) |
| receiver | input 0.42, **deliver 0.41** (recv loop 0.40 + spill 0.01), output 0.42 (ack send), update 0.01 | input 5.18, deliver 5.03 (spill 0.030 + recv 4.897), output 5.24 | segments per reader message 8.0 (the coalescing ratio); recv per delivered segment 0.397 µs; empty probes exactly 1.00/round |

Two records fall out of the fix:

1. **The "sender delivery anomaly" is retired.** It was never real: the
   2.46 µs/segment attributed to delivery in the Phase 0 table was the
   wire drain (now measured correctly at 2.55 µs/segment) plus the
   round tail, double-booked. With the fix the sender's delivery is
   0.36 µs/round — and the receiver's delivery is 5.03 µs/round, ~14×
   higher per round in the OPPOSITE direction from the artifact,
   because the receiver delivers 296 K segments/s against the sender's
   283. No asymmetry to chase.
2. **The targets re-aim.** The sender's cost is the wire drain
   (output: 2.55 µs/segment) and the writer (0.60 µs/segment, which
   contains the K3/K4 copies); the receiver's delivery is 5.03 µs/round
   of which the recv loop is 4.90 µs/round (the K7/K8 copies) — i.e.
   exactly the copies L1 and L2 remove. The counters are the gate for
   both: L1's win must show in `ms_deliver_recv` per delivered segment
   (0.397 µs) falling, and `segments_delivered/blobs_out` (8.0) staying
   put; L2's in `ms_writer` + `ms_output` per out-segment.

The kcp-stats line gained four fields (`segments_delivered`,
`recv_empty`, `ms_deliver_spill`, `ms_deliver_recv`), all additive.

### Link L2 A/B (2026-09-24): two-iovec PUSH send — landed on mechanism, no throughput claim

The send path's two copies (the engine's `encode` staging and the
adapter's batch staging) are gone for stream-mode PUSH datagrams: the
engine emits a 24-byte header plus the segment's own `Bytes` payload
(`DatagramSink::write_datagram`), the adapter records the payload as a
second iovec (`Span::Split`), and `sendmmsg` writes both without the
payload ever being copied. One interleaved `--ab` run (3 rounds x
3 reps x 8 s, cells loopback / loss1_rtt10 / rtt100, arms kcp4 + the
loopback mux-off control, binaries `b623a96` vs `41acf1f`, both SHAs
verified; verdict read with the printed `ab_bin_paths` mapping —
`molehill-580e51e5` = head, `molehill-5edafb32` = parent; data
`results-ab-kcp-zc-l2.json`, audited: 36 arms, 0 cell errors).

**The mechanism, verified structurally** (the per-binary counters are
not separable post-hoc — the interleaved rounds share one arm log and
the kcp-stats line carries no binary identity, so the mechanism is
proven by the tests instead): a PUSH datagram's payload crosses from
the engine segment to `sendmmsg` as one `Bytes` handle share — the
engine test asserts the retransmit re-emits the *same allocation*
(`Bytes::ptr_eq`-style pointer equality) and that the split form's wire
bytes are byte-identical to the packed form; the adapter test asserts
the batch carries `Span::Split` with the payload pointer unchanged.
The wire format, the datagram count and the ARQ semantics are
untouched (same datagrams, same bytes).

**The throughput verdict — honest read, no claim:**

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | -0.2% (non-overlapping, trivial) | -25.8% median — the documented BIMODAL cell (both binaries span 0.4-3.2), unquotable | -1.0% (non-overlapping) | -4.6% / +12.4% / -0.8% (median-only) |
| loss1_rtt10 | +0.2% (inside spread) | -11.6% median (inside spread) | — | -1.7% / +16.7% / -4.8% (median-only) |
| rtt100 | -40.0% (non-overlapping — see below) | wedged on both | — | -11.3% / -25.2% / -9.9% (median-only) |

The gate's positive (a non-overlapping 1-stream or cpu-per-segment
win) did not materialize: the loopback 1-stream cell is sender-bound
and the sender's cost is the wire drain (2.55 us/segment, per the
re-baselined attribution), which L2 does not touch — it removes the
copy half of the writer+output phases (0.60 + the staging), roughly a
tenth of the sender's per-segment cost, and the receiver side of the
proxy chain is unchanged. Recorded as a mechanism change with the
throughput upside explicitly not claimed, on the same no-regression
grounds the frame-split change landed on.

**Why the rtt100 1-stream claim is not attributable** (the one
claimable negative, -40% by the tool's non-overlap rule): the cell is
bimodal on BOTH binaries. Per-round detail from the data file: head
ok reps [0.0105, 0.0546] with round ab3 failing entirely (0 ok reps);
parent ok reps [0.0104, 0.0548, 0.0651, 0.0735, 0.0089, 0.0642]. The
parent itself samples the 0.01 mode twice (ab1 min 0.0104, ab3 min
0.0089) and the head samples it once; the medians differ because the
head had two ok reps against the parent's six — the median-of-few-draws
artifact section 10 names, on a cell whose documented spread is 7x
(0.011-0.057). Retransmits are 0-1 on both sides (no ARQ storm
difference). Per section 10 ("refuse to build a claim on a difference
inside it"), the difference is inside the cell's own spread and is
recorded as unquotable. The mux-off control on the same binaries
measured +4.9% on that cell, i.e. the cell's ordering bias runs the
other way.

**RSS note** (median-only, no spread recorded): loopback +12.4% and
loss1_rtt10 +16.7% — the two-iovec path keeps the segment's payload
`Bytes` alive in the batch until the pacer allows it, so a pacer-denied
span now holds its payload instead of a staged copy. Bounded by the
batch size (32 datagrams) and the pacer's token bucket; the rtt100 cell
(where the pacer denies most spans) measured -25.2% the other way.

### Link L1 A/B (2026-09-24): zero-copy receive — landed on mechanism

The code commit was amended with this record after the run, so the
binaries below carry the pre-amend code SHA; the amended commit's
`src/` tree is byte-identical (only docs changed), so the numbers
describe exactly the code that stands. The change (the read path
hands the engine's own segment buffers to the reader channel by
ownership; both per-byte copies gone) against its parent `f998f27`,
one interleaved `--ab` run (3 rounds ×
3 reps × 8 s, cells loopback / loss1_rtt10 / rtt100, arms kcp4 + the
loopback mux-off control, both binaries' SHAs verified, verdict read
with the printed `ab_bin_paths` mapping; data
`results-ab-kcp-zc-l1.json`, audited: 21 arms, 0 cell errors, 0 holes,
6 warnings all the rtt100 8-stream wedge documented on both sides).

**The mechanism, measured by the counters (per delivered segment, the
1-stream windows):**

| cell | recv loop (µs/seg) parent → head | segs/reader-message parent → head |
|---|---|---|
| loopback | 0.30-0.58 → **0.045-0.051** (-85..-92%) | 8.1-9.4 → 7.7-7.9 (held) |
| loss1_rtt10 | 0.72-0.79 → **0.042-0.046** (-94%) | 11.6 → 11.6 (identical) |

The input and output phases are unchanged (the change is confined to
the recv path), the per-segment totals are both lower and far more
stable (the parent's recv windows swung 0.30-0.58 with scheduling; the
head's sit at ~0.05), and the coalescing ratio the Phase-1 batching
bought is intact — the plan's gate for L1 ("`ms_deliver_recv` per
delivered segment falling with `segments_delivered/blobs_out` held")
is met with margin.

**The throughput verdict — honest read, no claim:**

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | head +11.9% median, inside spread | head +74% — the documented bimodal cell (both inside 0.4-3.2), unquotable | head +2.9% claimable, inside the control's own 11.6% ordering bias on that cell | -3.2% / -10.1% / -1.0% (median-only) |
| loss1_rtt10 | head +0.3% (hair claim) | inside spread | — | -5.2% / -3.8% / -6.3% (median-only) |
| rtt100 | **head +5.4% non-overlapping** | wedge on both | — | -12.5% / -17.2% / -15.5% (median-only) |

Reading every claim against the floors: the only non-overlapping
throughput movement attributable to L1 is rtt100 1-stream +5.4% — the
one cell without a mux-off control to establish its bias floor, and a
cell whose 1-stream swung -13.5% in the Phase 1 A/B — so it is recorded
as directional, not proven. The gate metric (a non-overlapping
1-stream or cpu-per-segment win) did NOT materialize: the loopback
1-stream cell is sender-bound (the sender's wire drain is its cost, per
the re-baselined table), and L1 attacks the receiver, whose payoff
shows up as the medians moving the right way on every secondary axis
rather than as a claim. Per §10 ("variance is data"), this is recorded
as a mechanism win with no attributable regression — landed, with the
throughput upside explicitly not claimed. The copy map predicted
exactly this: the copies were never the binding constraint at these
cells, which is why the link that removes them buys stability (and
CPU/RSS) rather than a step change.
The behaviour is unchanged: the phases are timers around existing
code, and the fix only moves a drop point.

### Link L3 A/B (2026-09-24): owned Noise records into the writer channel

The code commit was amended with this record after the run, so the
binaries below carry the pre-amend SHA; the amended commit's `src/` tree
is byte-identical (only this file, CHANGELOG.md and the bench data
changed), so the numbers describe exactly the code that stands. The
change (the kcp4+noise write path hands the Noise record to the writer
channel by ownership and the engine shares it per segment with O(1)
`Bytes` splits — K1 and K2 both gone) against its parent `73da491`
(L2), one interleaved `--ab` run (3 rounds x 3 reps x 8 s, cells
loopback / loss1_rtt10 / rtt100 requested, arms kcp4 + the mux-off
control, both binaries' SHAs verified from `--version`: parent
`v0.8.1-82-g73da491`, head `v0.8.1-83-g1b13dd9`; verdict read with the
printed `ab_bin_paths` mapping — `molehill-44aff44c` = parent,
`molehill-7a5b37a0` = head).

**Coverage, stated plainly:** the run was interrupted at the start of the
rtt100 cell (its first arm-run was still in flight when the bench saved
"completed arms"), so the data file covers **loopback + loss1_rtt10
only** — 12 arms, audited: 0 cell errors, 0 holes, 0 warnings. The
rtt100 cell is not part of this link's comparison; data
`results-ab-kcp-zc-l3.json`.

**The mechanism, proven by the tests** (the per-binary counters are not
separable at phase precision — see the note below): the engine test locks
the owned write against the slice write byte for byte, the Noise
round-trip suite runs through the owned-record path, and the KCP
integration tests (kcp4, noise, mixed) cover the stream end to end.
`TAKES_OWNED` is a const, so a plain-TCP transport keeps the pooled-buffer
path with the dispatch folded away at monomorphization.

**The throughput verdict:**

| cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| loopback | **+5.7% CLAIM** (3.826 -> 4.046, non-overlapping — ab2's rep ranges disjoint, ab1/ab3 overlap) | -2.0% inside spread (the documented bimodal cell) | **+24.4% CLAIM** (6.353 -> 7.902, non-overlapping in all 3 rounds) | -0.0% / +7.5% / -6.1% (median-only) |
| loss1_rtt10 | -0.3% tool claim, inside the cell's own spread — not attributable | -0.4% tool claim, same | — | -5.3% / -6.7% / -6.9% (median-only) |

**The two loss1_rtt10 tool claims are not attributable** (the tool's
rule fires on any single round's disjoint ranges; both fire inside the
cell's own rep spread): 1-stream round medians parent
[0.439, 0.443, 0.433] vs head [0.438, 0.428, 0.483] with rep ranges
spanning 0.37-0.49, 8-stream [0.734, 0.806, 0.720] vs
[0.731, 0.724, 0.739] with reps to 0.96; retransmits are comparable
(parent 74-106 vs head 84-103 on 1-stream, 214-265 vs 212-248 on
8-stream — no ARQ difference). Per §10 ("refuse to build a claim on a
difference inside it"), recorded as unquotable.

**The control arm's reading** (the mux-off arm is plain TCP in direct
mode — it shares no code with this change, so it measures the run's own
ordering bias): loopback 1-stream **-10.3%** by the tool's rule, but the
claim rests on ab1 alone (parent [21.904, 22.351] vs head [18.650,
21.623]; ab2/ab3 overlap heavily) and the same control measured
**+11.3%** on 64-stream (head higher in all three rounds) and -0.1% on
loss1 8-stream — the bias runs both ways inside one run, the documented
control noise. Read against it, the kcp4 64-stream +24.4% exceeds the
control's own +11.3% on that cell, and the kcp4 1-stream +5.7% moved the
opposite way to the control's -10.3% bias.

**Counter note (why no phase-level claim, same stance as L2):** the run
carried `MOLEHILL_KCP_STATS=1` (844 stats lines per arm log) and each
process logs its commit SHA in the version line, so per-binary
attribution IS possible post-hoc. It was exercised: six controlled 1-rep
x 8 s loopback kcp4 runs (three per binary, sequential, MOLEHILL_BIN per
run) give server-side per-datagram writer-phase rates of parent
[0.26, 0.33, 1.08] vs head [0.06, 0.22, 0.24] us — the within-binary
scatter (driven by the retransmit rate, which varied 0.6M-7.8M per
window between identical runs) spans the between-binary difference, and
the receiver side's writer/output phases were identical across the two
binaries (0.178/0.172 and 7.835/7.858 us per datagram) as expected for a
path L3 does not touch. The phase timers are wall-clock and this cell's
loss environment is the dominant variance source, so the counters are
recorded as evidence the mechanism is in the measured path, not as a
claim. The copy map's L3 target (the writer phase's two copies) is
structurally gone; the tests are the proof.

**Gate:** the plan's gate (no attributable CLAIM REGRESSION; a
non-overlapping 1-stream or cpu-per-segment win) is met on the covered
cells — two claimable favourable throughput cells, every secondary axis
median-only favourable or flat, no attributable regression.

<<<<<<< Updated upstream
=======
### Link N1 (2026-09-24): parked — the premise did not survive the read-path audit

The map's R2 row ("scratch plaintext → caller buffer (decrypt)") was
written against the pre-`e463391` read path, where every record was
decrypted into `bufs.payload` and served out of it. That fast path has
since landed (and is quoted in the map itself): **when the record's
plaintext fits the caller's buffer, the decrypt already writes straight
into it**, so the staging copy R2 named is gone from the path the bench
measures. What remains per record on that path is exactly two buffer
traversals of the ciphertext — the accumulation copy (kernel → scratch,
R1, the syscall boundary) and the AEAD's own read — plus the plaintext
write that is the AEAD's output. Both are inherent; neither is a copy
molehill adds.

N1 ("read the whole record into the caller's buffer, decrypt in place,
plaintext shifted left 2 B") relocates the ciphertext's landing buffer
from `scratch` to the caller's buffer and makes the AEAD's read hit that
buffer instead. The byte traffic is unchanged — one kernel copy plus one
AEAD pass either way — so there is no copy left in R2 for it to remove.
Three further facts, all checked against the code this session:

1. **Its fit condition is strictly stricter.** N1 needs the WHOLE record
   (2 B header + ciphertext + 16 B tag) inside the caller's buffer; the
   current fast path needs only the plaintext. A 16 KiB mux body record
   is 2 + 16384 + 16 = 16402 bytes and does not fit the 16 KiB ask —
   and the header records (2 + 12 + 16 = 30 B) do not fit the 12 B frame
   header ask either — so on the mux path N1 would fall back to the
   scratch path for every frame, i.e. strictly more work than the
   current fast path, which covers both asks exactly (the writer's write
   boundaries align with the reader's asks, which is why `e463391`
   measured +12.3% there).
2. **It multiplies syscalls.** The current path amortizes one inner read
   per ~64 KiB accumulation chunk across however many records the chunk
   holds; N1's per-record read (2 B header, then a record-sized read)
   pays one syscall per record — worst on the mux path's 14-byte
   plaintext header records.
3. **snow exposes no in-place decrypt.** `TransportState` offers
   `read_message` only; an in-place N1 would have to take the Noise
   record cipher (nonce management plus ChaCha20-Poly1305) in-house —
   a rewrite of the decrypt state machine for a mechanism with zero
   byte-traffic win.

Recorded as parked (same discipline as "L3 window/streams decoupling —
premise refuted"): the expected "M1/N1 ≈ 2-5% CPU on the default arms"
applies to M1 alone. If a future read-path change (e.g. a larger record
granularity, or dropping the length header from the record path) makes
the whole-record-into-the-caller-buffer shape profitable, this is the
note to revisit first. Not implemented; no commit, no A/B.

### Link M1 A/B (2026-09-24): owned frame bodies — the gate failed, reverted

The code commit (`fa34343`, replayed as `cfc06be` by the L3 record
amend — src trees byte-identical) measured a net loss against its
parent and was reverted in the same session; this section is the
record, and the revert commit carries the same numbers. The change:
`Frame.body` `Vec`→`Bytes` (the read path freezes the receive buffer in
place; `Stream::poll_write_owned` shares the caller's buffer as the
frame body with an O(1) slice) and the forwarding legs moved to
`forward_bidirectional`, whose data-channel direction reads into a
fresh 32 KiB `BytesMut` that becomes the write buffer by ownership.

One interleaved `--ab` run (3 rounds x 3 reps x 8 s, cells loopback /
loss1_rtt10, arms mux / noise / kcp4 plus the auto-appended mux-off
control on loopback; parent `1b13dd9` = L3, head `fa34343` = M1, both
SHAs verified from `--version`). The verdict was read with the printed
`ab_bin_paths` mapping — per the 5d1d9dd lesson the pair sorts by
label, so the tool's A side is the HEAD binary (`molehill-4244970c` =
ab-m1-head) and its B side the parent (`molehill-fe25b6e9` =
ab-m1-parent); every delta below is re-read parent -> head. Data
`results-ab-mux-zc-m1.json`, audited: 42 arms, 0 cell errors, 1 hole
(kcp4 ab2 head's mixed-bulk probe: iperf3 "control socket has closed
unexpectedly" — a probe failure with its reason recorded; that
arm-run's throughput reps completed).

**The verdict (M1 vs parent):**

| arm / cell | 1-stream | 8-stream | 64-stream | CPU / RSS / cpu-per-kframe |
|---|---|---|---|---|
| mux loopback | -5.4% inside spread (parent ahead all 3 rounds) | -2.9% inside spread | **-15.9% CLAIM REGRESSION** (17.622 vs 20.418; parent ahead 2 of 3 rounds) | -2.0% / **+25.1%** / -0.9% (median-only; RSS higher in all 3 rounds) |
| noise loopback | **+13.4% CLAIM favourable** (5.410 vs 4.683; M1 ahead all 3 rounds) | -4.7% inside spread | -0.6% claim (hair) | +0.2% / **+22.8%** / no change (median-only; RSS higher all 3) |
| kcp4 loopback | +9.1% inside spread (M1 ahead all 3 rounds) | +3.0% inside spread | **-13.0% CLAIM REGRESSION** (7.784 vs 8.794; parent ahead 2 of 3) | -0.3% / -3.8% / +5.7% (median-only) |
| mux loss1_rtt10 | **-3.1% CLAIM REGRESSION** (4.322 vs 4.454; parent ahead 2 of 3) | -0.2% inside spread | — | **+11.4%** / +7.6% / +9.5% (median-only) |
| noise loss1_rtt10 | -0.9% inside spread | -0.6% inside spread | — | -1.8% / -11.5% / -1.5% (median-only) |
| kcp4 loss1_rtt10 | **-2.2% CLAIM REGRESSION** (0.441 vs 0.451; parent ahead 3 of 3) | **+4.8% CLAIM favourable** (0.786 vs 0.749; M1 ahead 2 of 3) | — | +1.2% / +1.0% / +8.4% (median-only) |
| mux-off control loopback | +5.3% claim (the cell's ordering bias) | -1.1% inside spread | -3.9% claim (same) | -1.3% / -8.1% / — |

**Reading.** The gate (no attributable CLAIM REGRESSION) fails on four
cells. Two are the single-rep 64-stream points: the mux-off control
shows that cell's ordering bias at -3.9% and every arm's rounds
alternate (parent ahead 2 of 3), so part of the median is artifact —
but both arms' movements exceed the control's own bias and the
mechanism has a plausible cost at that cell, so they are not
dismissible. The two loss1_rtt10 1-stream claims are hair claims inside
the cells' own ±5% rep spreads; the kcp4 one nevertheless has the
parent ahead in all three rounds. Note the mux-off arm is NOT an
untouched control for this link — M1 rewrote that loop too (its
borrowed shape, measured here at parity) — which weakens the bias
estimate for the mux arms.

**The cost mechanism is identifiable.** The owned forwarding direction
allocates AND zeroes a fresh 32 KiB `BytesMut` per read
(`BytesMut::zeroed(READ_CHUNK)` in `transfer_owned`). At the mux arm's
framing-bound 1-stream rate that is ~1.3 GB/s of memset, and the
allocation residency is the RSS: +25.1% (mux) and +22.8% (noise) on
loopback, higher in all 3 rounds — a real, attributable memory-axis
cost. The upside is real too: noise loopback 1-stream +13.4% with M1
ahead in all 3 rounds (the control's own +5.3% at that cell leaves
~+8% attributable), kcp4 loopback 1-stream +9.1% (3 of 3 rounds,
inside spread), CPU -2.0% on mux (3 of 3 rounds).

**Net:** a mechanism that removes the copy but pays a per-read
allocation plus zero-fill for it — the plan's expected "M1 ≈ 2-5% CPU
on the default arms" did not survive contact (the CPU axis is flat,
the memory axis pays). Per the link discipline (a failed gate reverts;
no further investment), M1 is reverted and the evidence stands here.
The refinement the data points at — read into the BytesMut's spare
capacity without the zero-fill (`AsyncReadExt::read_buf` instead of
`zeroed` + `ReadBuf::new`) — is recorded as the starting point if this
link is retried; it was NOT attempted here. The copy map's W2 row
returns to "yes (M1)" as an open link, not a removal.

### Link S1 A/B (2026-09-24): stripe chunks read into their frame — landed on mechanism

The code commit was amended with this record after the run, so the
binaries below carry the pre-amend SHA; the amended commit's `src/`
tree is byte-identical (only this file, CHANGELOG.md and the bench data
changed), so the numbers describe exactly the code that stands. The
change (the stripe send direction reads each chunk directly into the
payload region of its frame buffer, behind the 10-byte header written
in front once the length is known, and hands the frame to the stripe by
ownership — the send direction's staging copy is gone; the stripe write
itself keeps the borrowed boundary) against its parent `1b13dd9` (L3 —
the tree the M1 revert restored), one interleaved `--ab` run (3 rounds
x 3 reps x 8 s, cell loopback, arms mux-stripe + the unstriped `mux`
inertness control + the mux-off control; both binaries' SHAs verified
from `--version`: parent `v0.8.1-83-g1b13dd9`, head `v0.8.1-84-g852de3d`;
verdict read with the printed `ab_bin_paths` mapping — in this run the
labels sorted parent-first, so the verdict's A side IS the parent
(`molehill-7a5b37a0`) and B the head (`molehill-f2aff66d`); data
`results-ab-stripe-zc-s1.json`, audited: 18 arms, 0 cell errors, 0
holes, 0 warnings).

**The mechanism, proven by the tests:** the existing frame-ordering
tests are re-driven through the owned API, and the wire format and the
round-robin/commit semantics are unchanged (the frame is the same
bytes, built in place).

**The verdict (mux-stripe, loopback):**

| axis | parent | head | reading |
|---|---|---|---|
| 1-stream | 17.098 [15.478, 17.994, 17.098] | 18.758 [19.555, 18.758, 17.324] | **+9.7% median, inside spread** — the head ahead in all 3 rounds (ab1 +26%, ab2 +4%, ab3 +1%) but the ranges overlap |
| 8-stream | 20.161 [19.283, 20.161, 20.568] | 19.497 [19.462, 22.118, 19.497] | -3.3% median, inside spread (rounds alternate) |
| churn / echo p50 / udp p50 / HoL gap | 4649/s, 0.283 ms, 0.411 ms, 33.42 ms | 4657/s, 0.284 ms, 0.436 ms, 33.41 ms | median-only, no movement (udp p50 +6.1% is that cell's jitter) |
| RSS / CPU / cpu-per-kframe | 24216 KiB / 591.2% / 0.029 | 26813 KiB / 610.6% / 0.029 | +10.7% / +3.3% / +1.4%, median-only, the head higher in all 3 rounds on the first two |

**The controls:** the unstriped `mux` arm (which this change does not
touch) sits at parity — 1-stream +6.5% and 8-stream +1.0% inside
spread, 64-stream +3.4% a claim on the single-rep cell's noise — and
the mux-off control's only claim is 8-stream -0.5%, the documented
noise floor on a path S1 does not touch.

**Reading:** the staging copy is structurally gone (the read buffer
becomes the frame), the arm shows no attributable regression, and the
1-stream median moved the right way in all three rounds without
reaching a claim. The cost side is the same shape M1's audit found,
one size down: the per-chunk frame buffer (32 KiB + 10 B header,
allocated per read instead of reused) is the stripe arm's +10.7% RSS
and +3.3% CPU, both median-only and bounded to the opt-in arm.
Recorded as a mechanism change with the throughput upside explicitly
not claimed, on the same no-regression grounds the frame-split change
landed on.

>>>>>>> Stashed changes
## Final cumulative A/B: branch vs `main` (2026-09-22, host `9201f86b86a8`)

One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback /
loss1_rtt10 / rtt100, arms mux / noise / mux1 / kcp4 plus the mux-off
control on loopback; branch `d798100`+bench fixes vs `main` `8584945`,
both binaries freshly built with the commit SHA verified) followed by a
loss5_rtt100 run (mux / noise / kcp4) and a focused 5-round re-measurement
of the one cell that fired an unfavourable claim. The old
`results-final-ab-2026-09-21.json` claim (mux + mux1, loopback + loss1, 24 paired
ranges, 21 overlapping) is the 2026-09-21 session's; this one re-runs it
with the current bench and adds the noise, kcp4 and loss5 arms.

**The instrument's noise floor, measured on the control arm:** the
mux-off arm carries no mux engine, yet the two binaries (which differ
everywhere in its path too — setup allocation, the noise... no, mux-off
is plain TCP) claim ±1.8% non-overlapping on the single-rep 64-stream
point, and the mux 1-stream/8-stream rounds ALTERNATE direction
(round 1 head +6%, round 2 main +11%, round 3 tie). Round-to-round
noise on the loopback 8-stream cell is ±5-9%. Every verdict below is
read against that floor.

| axis (priority order) | verdict |
|---|---|
| **latency** (echo p50/p99, steady p50/p99, udp p50, HoL max gap) | **no regression anywhere**: every arm/cell at parity (mostly <1%) or better. mux-off loopback steady ping -13.2%/-12.5% and mux1 loopback -3.3%/-4.1% better; kcp4 loss5 steady p99 -30.0%, HoL gap -31.2%, udp loss -25%. The only wrong-way median movements are sub-ms items (noise loopback steady p50 +10.8% = 0.36→0.40 ms), one cell (mux1 rtt100 HoL gap +23.3%, no spread recorded) and the loss5 connect-path echo p50 (+19-21% median-only on all three arms — that cell's own run-to-run echo spread is ±19%: main measured 1253.9 ms in one run and 1053.4 ms in the other). rtt100 cells: 0.0% on every latency metric. |
| **throughput** | **no systematic regression**. mux (the gated row): loopback -3.5%/-4.4%/-3.6% with rounds alternating direction (round noise ±5-9%), loss1_rtt10 +0.9%/+0.2%, rtt100 +4.7%/+1.7% → parity. mux1: loopback -5.9%/+0.3%, loss1 +0.7%/**+9.0%**, rtt100 -3.9%/-4.3%. noise: loopback +6.3%/-7.5%/+13.8%, loss1 +3.9%/-2.5%, rtt100 +1.6%/+1.1%. kcp4: loopback (bimodal, unquotable), loss1 +1.6%/+1.8%, rtt100 +5.1%. |
| **memory / cpu** (growth accepted) | RSS flat (±1-2%) on mux/mux1/noise, kcp4 loopback -7.2%, mux1 loss1 -6.6%, noise loss1 +15.1%; CPU +0.4-6.0% on the mux/noise arms and -1.6..-7.3% on mux1. All inside the accepted band. |

Claimable (non-overlapping) movements — 4 favourable, 3 unfavourable,
all at the noise floor except where noted:

| arm / cell | metric | branch | main | delta |
|---|---|---|---|---|
| kcp4 loopback | 64-stream | 8.257 | 7.759 | branch **+6.0%** |
| noise loopback | 64-stream | 14.996 | 13.181 | branch **+13.8%** |
| noise rtt100 | 1-stream | 0.676 | 0.666 | branch **+1.6%** |
| mux-off loopback (control) | 64-stream | 18.795 | 18.461 | +1.8% — the floor itself |
| mux loopback | 64-stream | 19.165 | 19.889 | -3.6% — single-rep, rounds alternate |
| noise loopback | 8-stream | 15.890 | 17.174 | -7.5% — 1 of 3 rounds disjoint; the other two overlap (round 2: 17.05 vs 17.07) |
| noise loss1_rtt10 | 8-stream | 8.112 | 8.323 | -2.5% — thin rep samples |

**loss5_rtt100 (follow-up run, mux/noise/kcp4):** mux 1-stream +6.3%
(claim, not re-confirmed below), mux 8-stream -22.2% (claim),
kcp4 1-stream +2.8% (claim) with steady p99 -30.0% / HoL gap -31.2% /
udp loss -25% / RSS -10.9% better, noise 8-stream +11.1% (claim) with
CPU -13.3%. The mux 8-stream claim was the only consistent-direction
unfavourable signal of the whole session (main higher in all three
rounds' medians), so it was re-measured focused: **5 rounds, mux arm
only, same binaries — 8-stream 0.835 vs 0.836 (-0.2%, no claim)**. The
first reading was a small-sample artifact of the cell's ±30% rep spreads
(2-3 ok reps per arm-run; the median of 2-3 draws). Data:
`results-ab-final-2026-09-22-loss5.json` and
`results-ab-mux-loss5-focus.json`.

**Reading:** the branch does not regress either priority axis and gains
where the engine's work is visible at shaped cells (mux1 loss1 8-stream
+9.0% with RSS -6.6%, kcp4 loss1 +1.6/+1.8%, kcp4 loss5 latency and
RSS, mux rtt100 +4.7%). The unfavourable claims are all single-rep,
one-round-of-three or thin-sample artifacts at the control arm's own
±1.8-6% noise floor; the one that survived its first three rounds
(mux loss5 8-stream) did not survive five. The per-change wins the
branch accumulated — the leaner noise stream, the direct decrypt, the
32 KiB frame split — were each measured against their immediate parent
and are inherited; this cumulative run confirms they cost nothing
against `main` on any axis. Data: `results-ab-final-2026-09-22.json`
(78 arms, audited: 0 cell errors, one documented kcp4 rtt100 gap, no
unexplained holes).

**One data caveat, recorded:** that run's file carries **no
`framing_cpu` column** (0/78 arms) — the host's environment dropped the
`MOLEHILL_MUX_STATS` propagation sometime between 20:44 and 02:11 that
night, so the engine's per-frame attribution was silently absent from
the final run and the loss5/focus runs. The throughput, latency, memory
and CPU verdicts above are unaffected (they come from iperf3 and the
samplers, not the framing counters), and every framing number quoted in
this document comes from the experiment runs, where the counters were
present and verified. The symptom could not be reproduced afterwards
with the identical bench code and binaries (a 1-rep rerun produced 6 472
stats lines), so it is filed as an environment fault; the bench now
records a typed `partial_metrics` note when a framed arm produces no
mux-stats lines, so the same silent loss cannot recur unnoticed.

**Scope, stated plainly:** the cumulative claim covers four of the nine
matrix cells (loopback, loss1_rtt10, rtt100, loss5_rtt100). The pure-delay
rtt10 cell, the two rate-shaped cells (r100/r20) and the jitter cell are
NOT part of the branch-vs-main comparison — they are exercised by the
release baseline (`results-v0.8.1.json`, same code as `main` plus the
noise-stream work) but not A/B'd against it. If one of those regimes
matters for a merge decision, that is the remaining bench work;
everything measured above holds within its stated cells — except that the
whole run shared one binary across its `--ab` sides, so its arm-to-arm
movements measure that binary against itself (see "The `--ab` harness
bug").

## The `--ab` harness bug (found 2026-09-23, fixed in `064c55a`)

**Every `--ab` run on this branch measured the DEFAULT binary against
itself.** `bench.py`'s `--ab` loop assigned `knobs.molehill_bin = ab_bin`
per arm, but every spawn site (`start_molehill`, `noise_keys`,
`tool_version`) read the module-level `_KNOBS["bin"]` global, seeded once
from the environment before the cell loop and never updated. The label
suffix (`(ab1:molehill-main)` vs `(ab1:molehill-head)`) therefore did not
describe what ran: both sides of every interleave were the same binary,
sampling the same epochs.

Affected files: `results-final-ab-2026-09-21.json`,
`results-ab-final-2026-09-22.json`,
`results-ab-final-2026-09-22-loss5.json`,
`results-ab-mux-loss5-focus.json`. Their "branch vs main" movements are
interleaved same-binary noise, and their non-overlapping claims are
withdrawn — §10's provenance rule ("a run must correspond to a committed
revision and a freshly built binary; check the binary's reported
version/hash before trusting its numbers") failed silently because the
label checked out while the process did not.

What still stands: the per-change A/Bs taken as two separate invocations
(`MOLEHILL_BIN` per run, then `ab_compare --baseline` — e.g. the leaner
noise-stream pair and the direct-decrypt pair), the KCP experiment files,
and the release baselines: those did compare the binary each run named.
The 2026-09-22 "cumulative A/B" must not be quoted as branch-vs-`main`
evidence until it is re-run with the fixed harness — that re-run is the
outstanding bench work for a merge decision.

The fix removes the global entirely: the binary path now lives only in
`knobs` (which the interleave swaps), and the spawn helpers take it as a
parameter.

## Stripe A/B (K=4): the single-stream ceiling experiment

The prototype: `[server.data] stripe_count = K` spreads a visitor
connection over K data channels ( Design: docs/internals.md,
"Data-channel striping"). The experiment arm differs from `mux` only by
the per-arm `MOLEHILL_STRIPE_COUNT=4` measurement override — configs are
identical, and a binary that predates the striped command ignores the
variable, so the baseline arms are the unstriped path by construction.

One interleaved `--ab` run with the FIXED harness: loopback, arms
`mux` / `mux-stripe` + the loopback `mux-off` control, 3 rounds × 3 reps ×
8 s; binary A = `5e6719c` (parent), binary B = `f91e6bb` (stripe), both
freshly built with the commit SHA verified from `--version` before the
run. Data: `results-stripe-k4.json` (18 arms, audited: 0 cell errors, 0
holes).

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

Reference points from the same run: the direct mode (`mux-off`, both
binaries at parity — 19.9–21.9 Gbit/s 1-stream) is the no-tax ceiling, and
the unstriped mux arm is the taxed one: 10.73 vs ~20.4 is the 2.19x tax
this host's loopback cell shows; striping recovers 1.7 of the ~10.7 Gbit/s
of it (a residual ~1.27x gap to direct remains). The cpu/kframe halving is
the ①-scaling effect the design predicted (frames spread over four driver
tasks instead of one).

**Inertness at K=1** (the prototype's own non-regression check): the `mux`
arm, same run — 1-stream 9.376 vs 9.259 (inside spread), 8-stream
+3.2% inside spread, churn +0.1%, CPU +0.7%, RSS +2.1%. The single-rep
64-stream cell showed base 19.38 vs stripe 16.89 (-12.9% disjoint in one
round), which the claim rule flags as a regression — a focused 5-round
re-measurement (`results-mux64-focus.json`, 16 arms, audited clean)
re-fired the same claim (-12.8%), so it was not a three-round artifact.
The cell is single-rep by construction and **bimodal on both binaries**:
over the 13 rounds combined, base spans 16.46-19.54 (median 19.1, 2 of 7
rounds in the low mode) and stripe 16.77-19.55 (median 16.8, 1 of 6 in the
high mode) — the distributions overlap completely, so per §10 ("refuse to
build a claim on a difference inside it") the difference is a mode-frequency
shift inside the cell's own span, not a claim in either direction. The
multi-rep cells of the same arm (1-stream, 8-stream) show the prototype
inert. Follow-up, recorded: the bench's 64-stream scale point is
single-rep, which makes that cell structurally undecidable; making it
multi-rep (like the other throughput points) is a bench change on its own.

**Reading:** the tax is real and striping removes more than half of it on
the strongest cell, at a bounded cost (churn -7.7%, CPU +40.8%, RSS +8.7%,
sub-millisecond latency +5% — all median-only, none claimable). The
mechanism works as designed (ceiling ×K, window ×K, cpu/kframe ÷K), and
the residue is the reassembly path, not the protocol.

## Noise session resume: the setup-cost experiment

The premise (session resume saves the handshake's cost) was measured
before it was built: a release-mode phase attribution over the production
pattern (`noise_stream.rs`, `handshake_phase_attribution`, `7b19457`)
showed the DH turns at ~97% of the ~445 us a pair costs on the state
machine (initiator turn 1 `e,es` ~122 us, responder turn `e,ee` ~233 us,
initiator turn 2 the `ee` read ~72 us; everything else <1 us).

The implementation (`src/transport/noise_resume.rs`, opt-in via
`[transport.noise] resume = true` on both sides):

- **Ticket.** After a full handshake the responder seals the session's
  handshake hash with a key derived from its Noise static private key
  and returns it as the first exchange record; the client caches it per
  server static key. The client's first record after the handshake is a
  one-byte `want` — the exchange runs on *every* full handshake, on both
  sides, because that byte is what tells the responder an exchange
  follows (gating it per side deadlocks: the responder would block on a
  byte the initiator never sends). What the configuration decides is
  whether a ticket is *issued* and whether a cached one is *attempted*.
- **Resumed connect** (selector `0x02`): the client sends
  `[ticket][client_nonce][MAC]`, the responder verifies (open the seal,
  check the 24 h TTL, check the MAC, reserve the nonce) and answers
  `[status][server_nonce][MAC]`; both derive fresh record keys with
  HKDF-SHA256 over the cached hash and both nonces and speak
  ChaCha20-Poly1305 with the Noise nonce convention. Old servers reject
  the unknown selector cleanly, and the client falls back to a full
  handshake on a fresh connection.
- **Replay/FS.** A repeated `(ticket, client nonce)` is rejected (the
  store reserves the nonce), a captured request cannot be completed
  without the cached hash, and the tradeoff is documented in
  docs/transport.md: resumed sessions' keys derive without a fresh DH,
  so a later static-key compromise reaches them — hence opt-in.

**Measurement** (`noise_stream.rs`, `resume_setup_cost`, release build,
N=200, in-process pairs over a tokio duplex with the exchange records
and IO included):

| shape | per pair |
|---|---|
| full handshake + ticket exchange | 442.70 us |
| resumed exchange | 38.52 us |
| saving | 404.18 us (91%) |

The saving matches the phase attribution (the DH turns are the
difference), and the resumed pair's 38.5 us is symmetric crypto plus the
two records. End-to-end correctness is covered by the integration
scenario `noise_session_resume` (a client restart over the noise
fixture with `resume = true`, engaging the selector-0x02 path — the test
run logs 16 resumed sessions for the control channel, pools and
tunnels), plus unit tests for the ticket seal/unseal, tamper, staleness,
replay and decline paths.

**What is not yet measured**: a bench-level reconnect-latency axis. The
bench's probes dial the exposed port and never tear down a control
channel, so the 404 us saved per reconnect has no bench cell today; the
in-process probe is the evidence (the same standard the connection-setup
allocation probe `3297d65` was held to). A cold-start/reconnect probe
would be the follow-up, and it belongs with the single-control-channel
item (below).

## How to A/B on this branch

Sequential before/after runs are **not usable** — several cells drift ~12%
between epochs, which is what hid both a real regression and a bug for a
whole session, and made a -31% "regression" turn out to be a bimodal cell on
an outlier. Always:

```bash
TMPDIR=~/tmp MOLEHILL_REPS=3 MOLEHILL_SECS=8 MOLEHILL_SECS_WEAK=10 \
  just bench --tools=molehill --cells=0/0,1%/10 --variants=mux,mux1 \
       --ab /path/to/bin-a,/path/to/bin-b --fresh --out results-ab.json
just bench-ab results-ab.json     # CLAIM only where reps are disjoint
```

Details in [docs/release.md](docs/release.md) ("Comparing two builds").

### Environment notes (this host, measured 2026-09-22/23)

- **Verify `iperf3` before a long run.** The container's apt layer dropped
  the `iperf3` package twice mid-session without a reboot. The bench then
  fails *cleanly* — every arm records the `Backends: … [Errno 2] iperf3`
  error with its reason (continue-on-error, no fabricated numbers) — but a
  whole matrix spends its hour producing nothing.
- **/tmp is periodically wiped.** It took one 35-minute final A/B with it
  (every checkpoint of the run). The checkpoint now recreates its output
  directory (`baa4eab`), but keep `--out` and logs under `~/tmp` or the
  repo regardless.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the
  python child, which keeps executing and holds the bench lock — a later
  invocation then exits immediately with "another bench run is active".
  Let the orphan finish (or reap it) before starting the next run.

## Legacy state (2026-09-11, still accurate)

- `v0.8.0` and `v0.8.1` are released; `v0.8.1` is the control-channel
  teardown fix (a service whose control channel ended kept its public port
  bound until a new registration took it over — recorded in CHANGELOG.md's
  `## [0.8.1]` section), with the benchmark matrix carried forward
  unchanged.
- The benchmark measurement method was revised 2026-09-10/11 (rate-cell
  shaping, per-rep throughput isolation, a UDP capacity ladder); the v0.8.0
  baseline was re-measured in full from it on host `0b073ddbf222` (52 arms,
  zero holes, charts and README regenerated).
- Only same-method results (`results-v0.8.0.json` and later) are comparable.
  The v0.7.2 file predates the revision and comes from another container, so
  it measures a different instrument and is never a regression signal.
- `src/transport/udp_batch.rs` is the only `unsafe` site in the codebase
  (the recvmmsg/sendmmsg FFI), audited 2026-09-20.

## Appendix: measurements for work already released

Referenced by [CHANGELOG.md](CHANGELOG.md), kept here so the release notes
do not need to carry the tables.

### Leaner Noise record stream (`0870bf1`, released)

`count=4` + noise arm, 3 reps, 8 s, loopback + loss1_rtt10, against parent
`4903fb4`; both binaries freshly built with the commit SHA verified
(§10 provenance). The `mux-off` control arm never touches `NoiseStream`, so
it shows what the instrument did.

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| noise loopback 1-stream | 4.601 [3.987, 4.832] | **5.016 [4.854, 5.276]** |
| noise loopback 8-stream | 16.884 [15.239, 17.446] | 14.813 [11.739, 16.826] |
| noise loss1_rtt10 8-stream | 8.212 [7.925, 8.555] | **8.869 [8.733, 9.046]** |
| mux-off loopback 1-stream (control) | 20.815 [18.195, 23.775] | 19.843 [19.122, 19.895] |
| mux-off loopback 8-stream (control) | 31.193 [26.481, 31.917] | 30.259 [26.387, 31.105] |

Two cells improve with **non-overlapping** reps: loopback 1-stream **+9.0%**
and loss1_rtt10 8-stream **+8.0%** — both cells where the per-record cost is
visible. Everything else is inside the spread and not a claim (loopback
8-stream's -12% median has overlapping ranges; the 64-stream points are
single-rep references). Secondary: CPU 464.5% → 440.1% (directional),
echo p50 and churn first-byte flat, RSS 26.2 → 29.0 MiB (same buffer sizes;
treated as noise). A focused A/B, not a re-baselining.

### Direct decrypt into the caller's buffer (`e463391`, released)

Same method on the default `count=4` + noise arm: **+12.3%** on the loopback
8-stream cell (non-overlapping). The plain-path control moved nothing, and
the `noise-direct` arm shows the plain path is already at its ceiling — the
gain belongs to the decrypt, not the instrument.

**Re-measured 2026-09-22 (cumulative A/B, single-invocation `--ab`):** the
noise loopback 8-stream cell sits at **-7.5%**, `main` ahead in all three
rounds. The +12.3% came from the pre-`--ab` hand-interleaved form, which
does not cancel epoch drift (the two-file mode `ab_compare.py` warns about
exactly this), so the figure did not reproduce; the change's gain is not
visible in the cumulative picture — see "Final cumulative A/B".

### Connection-setup allocation (`3297d65`, released)

Counting-allocator + getrusage probe over `NoiseStream` pair setups
(release build — in debug, curve25519-dalek is 50-100× slower and the probe
reported a bogus 19.5 ms/pair): 312 → 232 µs CPU, 900 KiB → 4 KiB
allocated, 16 → 0 minor faults per pair. System level the direct-mode churn
arm gained +11.7% connects/s and -10.7% first-byte p50 with RSS -24%. The
interesting row: moving the handshake onto stack buffers alone *slowed*
setup (transient 64 KiB buffers had been ballast keeping glibc from trimming
the freed record buffers back to the OS) — hence the pool, not just the
stack allocation.

**Re-measured 2026-09-22 (cumulative A/B):** the direct-mode (mux-off)
loopback churn arm is at parity — 5029 vs 5038 connects/s, first-byte p99
inside the spread — so the +11.7% did not reproduce either (same
pre-`--ab` method caveat). The in-process probes (µs, allocations, faults)
are allocation-level facts and stand on their own.

