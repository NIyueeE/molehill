# HANDOFF: Working State & Future Work

> State as of 2026-09-21. Branch `perf/data-path-optimizations` (40 commits
> ahead of `main`, pushed, **not merged**) contains the complete mux-engine
> migration: rust-yamux 0.14 is now an in-repo, tokio-native engine
> (`src/mux/`), and the mux transport drives it directly
> (`src/transport/multiplex.rs`, no Compat shim). Shipped work is recorded in
> [CHANGELOG.md](CHANGELOG.md); design details live in
> [docs/internals.md](docs/internals.md). This file tracks what is open and
> what was decided.

## Where things stand

**The migration is complete on its own terms and the engine is clean.**

- The data path beats `main` everywhere that can be claimed and loses
  nowhere: interleaved 3-round A/B, mux + mux1 arms, loopback + loss1_rtt10,
  24 paired rep ranges of which 21 overlap and the three that do not all
  favour the branch (`results-final-ab.json`). The churn regime that
  regressed mid-migration is fixed and now at parity.
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

## What landed (each one commit + one single-variable A/B)

| Change | Commit | Verdict |
|---|---|---|
| in-repo yamux 0.14, wire-identical | `92fdde0` | behaviour-identical to the crate |
| tokio-native engine IO (Compat gone) | `20ac557` | the -6.6..+23.5% column of the final A/B |
| stream cap 32 → 64 | `a97e1ef` | ceiling probe: 15 → 47 usable streams |
| control-frame coalescing (L4) | `a1fe0bb` | 8-stream +5.7..9.2%, ranges overlap |
| frame split 16 → 32 KiB | `fda6fd6` | mux1 loopback 8-stream +45.7% non-overlapping |
| drop finished stream receivers | `a424ccc` | the churn fix above |
| `--ab` interleaved A/B in bench.py | `fc87a6c` | cancel epoch drift between binaries |
| framing counters in the bench | `efbcac4` | frames/s + cpu-per-frame per arm |
| A/B verdict tool (`ab_compare.py`) | `040edbe` | encodes the §10 claim rule |
| arm watchdog + lock diagnostics | `040edbe` | a hung arm no longer blocks the matrix |
| peers: bore → nps 0.26.10 | `79855a1` | frp/rathole/nps; all three carry every probe |

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

- [ ] KCP pacer slow-recovery study: PONG-timeout cuts (x0.75) recover at only
      +5% per 4 clean PONGs — on sustained loss the pacing rate can pin low
      for minutes. Not dominant in the measured cells; revisit if KCP gets
      production use.
- [ ] HTTP API for configuration (hot reload is files-only today)
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Per-service bandwidth limiting (token bucket around the copy loops)
- [ ] Replace the python bench/test entries with `cargo-script` once it is
      stable — until then `uv run` stays the entry
- [ ] QUIC transport on main: implemented and measured, parked in the
      `archive/transport-test` tag (N×TCP won every comparable cell, and the
      QUIC leg lacks peer auth). Revisit if a UDP-only path or multi-stream
      loss isolation becomes a requirement.

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

The one KCP item still open is the **pacer slow-recovery study** (backlog):
PONG-timeout cuts recover at only +5% per 4 clean PONGs, so on sustained loss
the pacing rate can pin low for minutes. Not dominant in the measured cells.

Also measured and closed in the same round: window doubling bought +29%
single-stream on loss1/rtt10 and +20% on loss5/rtt100 at ~2x RSS, and a 5 ms
flush interval was rejected (loopback 8-stream -3x).

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

