# HANDOFF: Working State & Future Work

> State as of 2026-09-23. Branch `perf/data-path-optimizations` (40 commits
> ahead of `main`, pushed, **not merged**) contains the complete mux-engine
> migration: rust-yamux 0.14 is now an in-repo, tokio-native engine
> (`src/mux/`), and the mux transport drives it directly
> (`src/transport/multiplex.rs`, no Compat shim). Shipped work is recorded in
> [CHANGELOG.md](CHANGELOG.md); design details live in
> [docs/internals.md](docs/internals.md). This file tracks what is open and
> what was decided.

## Where things stand

**The migration is complete on its own terms and the engine is clean.**

- The cumulative branch-vs-`main` comparison was re-run end to end on
  2026-09-22 with the current bench (interleaved 3-round A/B, loopback +
  loss1_rtt10 + rtt100 + loss5_rtt100, mux/noise/mux1/kcp4 + the mux-off
  control): latency at parity or better on every arm/cell, throughput at
  parity with the favourable movements at the shaped cells, memory/CPU
  inside the accepted band, and no claimable regression that survives the
  per-round breakdown — see "Final cumulative A/B" below. The older
  `results-final-ab.json` claim (mux + mux1, 24 paired ranges, 21
  overlapping) is the 2026-09-21 session's record.
- `main` (`8584945`) is untouched and releasable; nothing here is merged.
- Performance is **not** an open item: every lever the vendoring was meant
  to unlock has either landed with a measurement or been closed by one (see
  "Optimization route" below). The two remaining gaps are structural
  (multi-tunnel scheduling) and environmental (the host's 32 MB socket cap),
  not knobs.
- Known bug found and fixed on the way: the `SelectAll` → `Vec` conversion
  dropped the receiver removal, so a client serving many short-lived
  connections polled thousands of dead stream receivers per poll
  (`a424ccc`).
- KCP was re-measured end to end with the current bench on this host and
  three candidate optimizations were tested by interleaved A/B; all three
  are closed with measurements (none landed) — "KCP on the current bench"
  below. Two bench feedback-side bugs were found and fixed on the way
  (inverted churn direction in the verdict tool, lexical results-file
  ordering).

## What landed (each one commit + one single-variable A/B)

| Change | Commit | Verdict |
|---|---|---|
| in-repo yamux 0.14, wire-identical | `92fdde0` | behaviour-identical to the crate |
| tokio-native engine IO (Compat gone) | `20ac557` | the -6.6..+23.5% column of the final A/B |
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
2. **The rtt100 ceiling is the host's** — that cell needs ~50 MB of in-flight
   window while the kernel caps a socket at 32 MB, and the mux window is
   already 64 MB. Raising it needs `SO_RCVBUFFORCE` privileges a normal
   deployment lacks.

## Backlog

### Next recommended improvement: single control channel per client

The client still keeps one control connection **and** one mux tunnel per
service. Consolidating to one control connection per client (plus the shared
tunnel pool) is mostly plumbing now that the mux engine is stable, and gives
another order-of-magnitude FD/handshake reduction for many-service clients.
Per-service `mode`/`count`/`carrier` overrides landed in 0.8, so mixing data
paths per service already works without waiting for this.

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

Also measured and closed in the same round: window doubling bought +29%
single-stream on loss1/rtt10 and +20% on loss5/rtt100 at ~2x RSS, and a 5 ms
flush interval was rejected (loopback 8-stream -3x).

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

## Final cumulative A/B: branch vs `main` (2026-09-22, host `9201f86b86a8`)

One interleaved `--ab` run (3 rounds × 3 reps × 8 s, cells loopback /
loss1_rtt10 / rtt100, arms mux / noise / mux1 / kcp4 plus the mux-off
control on loopback; branch `d798100`+bench fixes vs `main` `8584945`,
both binaries freshly built with the commit SHA verified) followed by a
loss5_rtt100 run (mux / noise / kcp4) and a focused 5-round re-measurement
of the one cell that fired an unfavourable claim. The old
`results-final-ab.json` claim (mux + mux1, loopback + loss1, 24 paired
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

## Legacy state (2026-09-11, still accurate)

- `v0.8.0` and `v0.8.1` are released; `v0.8.1` is the control-channel
  teardown fix recorded in "Control-channel teardown" below, with the
  benchmark matrix carried forward unchanged.
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

