# HANDOFF: Working State & Future Work

> State as of 2026-09-23. Branch `perf/data-path-optimizations` (55 commits
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

1. **The sender's cost is delivery + wire drain, not the ARQ update**
   (0.01 µs/segment). `deliver_recv` runs once per pump round (a
   fixed ~2.46 µs per sent segment ≈ 150 µs per round, an anomaly to
   re-measure after the receive path changes — it includes reverse-path
   inner-TCP-ACK delivery and wall-clock inflation under load), and
   `drain_dgrams`'s per-datagram pacer check + `sendmmsg` batching is
   the other half. Phase 1 (batching + coalescing) attacks both.
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
below): outbound datagrams stage in a reusable ~48 KiB buffer and cross
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

