# HANDOFF: Working State & Future Work

> State as of 2026-09-20 (superseding the 2026-09-11 state below where
> they overlap), still on the v0.8.1 patch line with three uncommitted-to-a-
> tag commits on `main`: the leaner Noise record stream (backlog item done,
> A/B +9.0% / +8.0% on the two non-overlapping cells — see "Leaner noise
> stream A/B"), the `MultiMap` unsafe elimination, and the
> `feature_not_compile` cfg-gating plus waiver normalization (see "Unsafe
> and lint-waiver audit"). `src/transport/udp_batch.rs` is now the only
> unsafe site in the codebase. Everything below is otherwise as of
> 2026-09-11: the `merge-tcp4` branch was fast-forwarded into `main` and
> deleted, `v0.8.0` is released, `v0.8.1` is the control-channel teardown
> fix recorded under "Control-channel teardown" below, with the benchmark
> matrix carried forward unchanged. The UDP
> session-affinity fix, the template lint migration and the benchmark-matrix
> rework (uv/PEP 723, schema v3 through-tunnel measurements) have landed, the
> benchmark measurement method was revised on 2026-09-10/11 (rate-cell
> shaping, per-rep throughput isolation, a UDP capacity ladder — see the
> "Method revision" paragraph below), and the v0.8.0 baseline was then
> re-measured in full from it on host `0b073ddbf222` (52 arms, zero holes,
> charts and README regenerated). Shipped work is recorded in
> [CHANGELOG.md](CHANGELOG.md), and design details (protocol, muxing, UDP
> session affinity) live in [docs/internals.md](docs/internals.md). This file
> only tracks what is still open.

## Backlog

### Next recommended improvement: single control channel per client

Dynamic registration removed per-service server config, but the client still
keeps one control connection **and** one mux tunnel per service. Consolidate
to one physical control connection per client (plus the shared tunnel
pool), with
register/unregister messages multiplexed over it. With mux now stable this is
mostly plumbing and yields another order-of-magnitude FD/handshake reduction
for many-service clients. (Per-service `mode`/`count`/`carrier` overrides
landed in 0.8, so mixing data paths per service is already possible without
waiting for the consolidation.)

### Other deferred work

- [x] Leaner Noise record stream (done 2026-09-20, `0870bf1`): the wrapper
      is in-repo (`src/transport/noise_stream.rs`, vendored from snowstorm
      0.4.0 and adapted to snow 0.10 — snowstorm is unmaintained and pinned
      to snow 0.9, so the upgrade required vendoring it). The read path now
      accumulates the two-byte length header together with the ciphertext
      in one buffer — one `poll_read` sweep per record instead of a
      separate header read first — and decrypts in place from that buffer;
      a record that coalesces with its successor in a single wake is
      consumed without an extra copy. Writes encrypt straight into the
      framing buffer behind the header and address it by index; both
      buffers are allocated once and never resized, which retired the
      per-record `set_len` dance and with it the module's `unsafe`. The
      wire format is unchanged (u16 ciphertext length, tag included — the
      value the writer has always stored). Unit tests cover 1 B–70000 B
      round trips (max-size and multi-record writes), coalesced small
      records, and the zero-length record that must not surface as an EOF.
      **A/B measured** (same host, 3 reps, 8 s tests, noise arm, loopback +
      loss1_rtt10, before `4903fb4` vs after `0870bf1`): see the
      "Leaner noise stream A/B" note below. The default pattern stays
      BLAKE2s: the cipher is ring-served for every pattern (the hash only
      runs in the handshake), so a pattern change would add wire churn for
      zero gain.
- [ ] KCP pacer slow-recovery study: PONG-timeout cuts (x0.75) recover at
      only +5% per 4 clean PONGs (2 s cadence) — on sustained loss the
      pacing rate can pin low for minutes. Not the dominant factor in the
      measured cells (window-bound), but worth revisiting if KCP gets
      production use.
- [ ] HTTP API for configuration (hot reload currently files-only)
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Per-service bandwidth limiting (token bucket around copy loops)
- [ ] Lower default `udp_send_queue_size` (64–128)
- [ ] Gate tracing span creation on level filter if profiling shows overhead
- [ ] QUIC transport on main (implemented and measured; parked in the
      `archive/transport-test` tag — N×TCP won every comparable cell and
      the QUIC leg lacks peer auth). Revisit if a UDP-only path or
      multi-stream loss isolation becomes a requirement
- [x] Buffer pooling under high churn (done 2026-09-20, `3297d65`):
      measured first (see "Connection-setup allocation measurement") —
      pair setup 312 -> 232 us CPU, 900 KiB -> 4 KiB allocated, 16 -> 0
      minor faults; system level the direct-mode churn arm gained +11.7%
      connects/s and -10.7% first-byte p50 with RSS -24%. The three record
      buffers are pooled (64 sets, ~12 MiB high-water) and the handshake
      runs on stack buffers. Still open inside this item, measured but out
      of scope: tokio's two 32 KiB copy buffers per data channel, and
      snow's ~26 small per-handshake allocations.
- [ ] Replace the python (uv/PEP 723) bench/test entries with `cargo-script`
      once it reaches Rust stable — the single-language test entry would drop
      the uv/python runtime dependency; until then `uv run` stays the entry
- [ ] Zero-copy splice/sendfile: deliberately not recommended (keep as-is)

- [x] Re-baseline the README benchmark chapter on the merged defaults
      (done in the v0.8.0 matrix): results-v0.8.0.json, six charts (incl.
      the new cost chart) and the tables now describe the count=4 default,
      the ring-accelerated noise rows, and the new cells/metrics.
      **Regression-gate verdict (v0.8.0 vs v0.7.2): not a comparison.** The
      v0.7.2 file predates the measurement revision and comes from another
      container (the gate itself prints `different hosts`), so it measures a
      different instrument: only same-method results — v0.8.0 and later — are
      comparable, and nothing read through that boundary is a regression
      signal. No waiver is claimed because there is no comparable baseline to
      regress against; `results-v0.8.0.json` is what every later run is gated
      against.

### Leaner noise stream A/B (2026-09-20)

A/B of the leaner record stream against its parent (`4903fb4`), same host,
same method, both binaries freshly built (before `d75ac826`, after
`d7877f80` — the reported commit SHA differs, which is the §10 provenance
check; an earlier same-tree build shared a build timestamp with the
pre-change one and was rebuilt from the committed tree for this reason).
Noise arm (`count=4` + noise), 3 reps, 8 s per test, loopback and
loss1_rtt10 cells, `bench.py --tools=molehill --cells=0/0,1%/10
--variants=noise` (the runner force-adds the `mux-off` loopback control to
every loopback cell — bench.py:389 — so both runs carry the same three
arms). Results: `results-before.json` / `results-after.json` in the run
scratch (`~/tmp/ab/`). Headline is the median of 3 reps with the rep range
beside it:

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| noise loopback 1-stream | 4.601 [3.987, 4.832] | **5.016 [4.854, 5.276]** |
| noise loopback 8-stream | 16.884 [15.239, 17.446] | 14.813 [11.739, 16.826] |
| noise loopback 64-stream (1 rep) | 14.661 | 15.072 |
| noise loss1_rtt10 1-stream | 3.715 [3.699, 4.225] | 3.759 [3.429, 3.886] |
| noise loss1_rtt10 8-stream | 8.212 [7.925, 8.555] | **8.869 [8.733, 9.046]** |
| mux-off loopback 1-stream (control) | 20.815 [18.195, 23.775] | 19.843 [19.122, 19.895] |
| mux-off loopback 8-stream (control) | 31.193 [26.481, 31.917] | 30.259 [26.387, 31.105] |

**Verdict (§10 — claim only outside the spread):** two cells improve with
**non-overlapping rep ranges** — loopback 1-stream **+9.0%** median and
loss1_rtt10 8-stream **+8.0%** median. Both are cells where the per-record
cost is visible (single stream is record-latency-bound; the loss cell adds
reordering that makes the merged read pay). Everything else sits inside
the spread and is **not** a claim: loopback 8-stream's median moved -12%
but its ranges overlap and the after spread is wider (one slow rep at
11.7 — watch it if it recurs), loss1 1-stream +1.2%, and the 64-stream
points are single-rep references. The plain-path control (mux-off, which
never touches `NoiseStream`) shows no systematic change, so the deltas
belong to the noise path, not the instrument. Secondary metrics on the
noise loopback arm: CPU avg 464.5% -> 440.1% (directional, one sample per
arm), echo p50 0.274 -> 0.283 ms and churn first-byte p50 3.21 -> 3.15 ms
flat, RSS avg 26.2 -> 29.0 MiB (the buffers are the same size; treat as
noise until it recurs). This is a focused A/B, **not** a re-baselining:
the release matrix stays `results-v0.8.0.json` until a full same-method
run refreshes it.

### Direct decrypt A/B (2026-09-20, branch `perf/data-path-optimizations`)

Second step of the record-stream work (`e463391`, A/B against its parent
`349c123` — the bench-runner commit that added the `noise-direct` arm, so
the runner is identical on both sides and the only variable is the
binary). Same host, same method, both binaries freshly built (before
`349c123`, after `e463391`, commit SHA verified). Arms: `noise-direct`
(the new arm) and `noise` (the default `count=4` + noise arm, run as the
regression watch), 3 reps, 8 s tests, loopback + loss1_rtt10, with the
forced `mux-off` plain control on loopback. Results in `~/tmp/ab/
results-d2-{before,after}.json`; medians with rep ranges:

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| noise loopback 1-stream | 5.252 [5.123, 5.834] | 5.829 [5.517, 5.984] |
| noise loopback 8-stream | 15.304 [14.845, 15.368] | **17.179 [16.996, 17.780]** |
| noise loopback 64-stream (1 rep) | 15.054 | 13.337 |
| noise loss1_rtt10 1-stream | 3.416 (2 reps, 1 iperf3 fail) | 3.642 [3.577, 4.137] |
| noise loss1_rtt10 8-stream | 8.972 [8.423, 10.318] | 8.596 (2 reps, 1 iperf3 fail) |
| noise-direct loopback 1-stream | 6.533 [6.375, 7.400] | 6.542 [6.223, 7.142] |
| noise-direct loopback 8-stream | 26.625 [26.461, 30.411] | 26.441 [26.264, 26.470] |
| noise-direct loss1_rtt10 1-stream | 3.973 [3.943, 4.079] | 4.089 [3.226, 4.445] |
| noise-direct loss1_rtt10 8-stream | 14.915 [14.385, 15.831] | 14.400 (2 reps, 1 iperf3 fail) |
| mux-off loopback 1-stream (control) | 21.536 [18.896, 22.973] | 20.007 [19.556, 22.402] |
| mux-off loopback 8-stream (control) | 29.473 [29.087, 29.850] | 28.410 [25.699, 29.587] |

**Verdict (§10 — claim only outside the spread):** the default `noise`
arm's loopback 8-stream cell improved **+12.3% with non-overlapping rep
ranges** (before max 15.368 < after min 16.996), and the plain-path
control moved the other way (-3.6%, inside spread) with flat CPU
(474.0% -> 473.5%), so the delta belongs to the noise path and not to the
instrument. The 1-stream cells (both arms) sit at the ring cipher's
ceiling — snow's TransportState measured ~6.6 Gbps/direction and
noise-direct 1-stream runs at ~6.5 end-to-end — so they claim nothing in
either direction. The `noise-direct` arm itself is unchanged (+0.1% /
-0.7%): it was built to isolate this change but turned out to already sit
at the plain-path ceiling (26.6 vs the plain control's 29.5 Gbit/s
8-stream), so its staging copy was never its bottleneck.

**Mechanism correction worth recording:** the direct path fires in mux
mode too, contrary to the expectation when the arm was designed. yamux's
*writer* splits a frame into a 12-byte header write and a body write
(frame/io.rs `WriteState::Header`/`Body`), so the record stream emits a
tiny header record and a body record per frame; the frame *reader* then
asks for exactly those sizes (12-byte header, then `vec![0; body_len]`),
which makes the body record align 1:1 with the body ask — the whole
record lands in the caller's buffer and the staging copy disappears. Two
consequences for direction ① (owning the mux layer): the 12-byte header
record is a full encrypt + syscall for 12 bytes of payload (a per-frame
overhead worth removing), and an owned mux could read header+body in one
step and decrypt once into the frame buffer.

**No regression found:** every other movement is inside the spread. The
64-stream slots (single rep, no statistics) dropped 11-12% on **all**
arms including the plain control — an after-run machine-state effect, not
attributable to the change; that is what the control arm is for. The
loss1 cells each lost one rep to an iperf3 control-socket failure on both
sides, so their medians are 2-3 reps and are read as directional only.
echo p50/p99 and churn first-byte are flat on every arm.

### Connection-setup allocation measurement (2026-09-20, branch `perf/data-path-optimizations`)

Direction ③ ("buffer pooling under high churn — measure first"), measured
with a temporary counting-allocator + getrusage probe over
`NoiseStream` pair setups on a duplex (removed before commit; numbers
below are its output). **Measurement trap worth remembering: the probe
must run in a release build** — `cargo test` defaults to debug, where
curve25519-dalek is 50-100x slower and pair setup measured a bogus 19.5 ms
(getrandom was exonerated first: 100 calls in 15 us; the 9.5 ms per
handshake message was the unoptimized scalar multiplications).

Release-mode probe, one host, N = 2000 pairs after warmup:

| version | cpu/pair | allocs/pair | bytes/pair | minflt/pair |
|---|---|---|---|---|
| before (parent `ba48845`) | 312 us | 42 | 900 KiB | 16 |
| handshake on stack buffers only | 366 us | 34 | 388 KiB | **64** |
| + record-buffer pool (`3297d65`) | **232 us** | 28 | **4 KiB** | **0** |

The middle row is the interesting one: moving the handshake onto stack
buffers removed 512 KiB of allocation but made setup **slower** — those
transient 64 KiB buffers had been ballast that kept glibc from trimming
the freed 192 KiB record buffers back to the OS (trim threshold 128 KiB);
without them every connection re-faulted and re-zeroed 48 pages. The pool
(64 sets, ~12 MiB high-water, take on handshake / return on Drop, no
re-zeroing because every region is written before it is read) is what
makes the combination a win: **-26% setup CPU, -99.6% allocated bytes,
zero faults**.

Decomposition of the remaining 232 us: the snow handshake crypto itself
is ~178 us (2 keygens + 2 dh at ~22/67 us each, dalek release-measured) —
now the dominant, irreducible-without-crypto-changes part. snow's own
state allocations (~26 small allocs) remain.

**System-level A/B** (same host, same method, 3 reps, 8 s tests; before
`ba48845` / after `3297d65`, both freshly built, commit SHA verified;
arms `noise-direct` + `noise` with the forced mux-off control; results in
`~/tmp/ab/results-d3-*.json`):

| arm / metric | before | after |
|---|---|---|
| noise-direct loopback churn connects/s | 2724 | **3043 (+11.7%)** |
| noise-direct loopback first-byte p50 / p99 | 5.834 / 7.144 ms | **5.21 / 6.384 ms (-10.7% / -10.6%)** |
| noise-direct loopback RSS avg / peak | 32047 / 47232 kB | **24403 / 35296 kB (-24% / -25%)** |
| noise-direct loopback 1-stream / 8-stream | 6.540 / 29.360 | 7.349 / 29.518 (inside spread) |
| noise (mux) loopback churn p50 | 3.136 / 3.223 ms | 3.145 / 3.238 ms (flat — the in-run control: mux churn reuses pooled channels, no handshakes) |
| noise (mux) loopback 1-stream / 8-stream | 5.66 / 17.05 and 5.49 / 17.12 | 4.92 / 14.60 and **5.99 / 15.47** |
| mux-off (plain control) 1-stream / 8-stream | 20.27 / 24.86 | 20.98 / 25.78 |

**Regression watch, resolved:** the first after-run showed the mux noise
arm's medians ~13% low while the plain control rose, so the cell was
re-measured focused (second run of each binary, `results-d3b-*.json`).
The after binary's two 1-stream medians are 4.92 and 5.99 — a 22%
run-to-run spread that brackets both before medians (5.66 / 5.49) — so
the cell is bimodal on this host and the dip was a low sample, not a
regression. Mechanistically the pool only touches connection setup (the
mux arm sets up 4 tunnels per client), never the steady-state data path.
Verdict: no measurable regression; re-check the cell in the next full
matrix, where it has more reps behind it.

Not addressed by ③ (measured, out of scope): the two 32 KiB copy buffers
per data channel are allocated inside tokio's `copy_bidirectional_with_sizes`
(pooling them means replacing the copy loop), and snow's ~26 small
per-handshake allocations. The handshake crypto (~178 us) now dominates
setup; only a crypto-level change (session resumption / 0-RTT style
reuse, which the Noise protocol supports only via PSK patterns) would
move it.

### Unsafe and lint-waiver audit (2026-09-20)

Enumerated every `unsafe` item and every lint waiver in `src/` (AGENTS.md
§2) and acted on the findings:

- `src/common/multi_map.rs` — **eliminated**. The dual-key control-channel
  map shared one heap item between two hash maps through raw pointers
  (plus matching `unsafe impl Send/Sync` and a manual `Drop`). Storing the
  second key in both maps — as `map2`'s key and inside `map1`'s value —
  makes `remove1` total with no shared ownership; API and semantics
  (including duplicate-key rejection) are unchanged, `get2` pays one extra
  hash lookup.
- `src/transport/noise_stream.rs` — **eliminated** by the leaner rewrite
  above (the `set_len`-over-uninitialized-capacity pattern is gone).
- `src/transport/udp_batch.rs` — **cannot be eliminated**. `recvmmsg`/
  `sendmmsg` have no safe Rust surface, the sockaddr casts are inherent to
  the FFI, and the batching win is measured (+28–33% loopback 1-stream on
  the KCP arm). It is now the **only** unsafe site in the codebase, every
  unsafe item carrying a SAFETY comment plus an `#[expect(unsafe_code,
  reason = ...)]`. The two cross-references between the sites' module docs
  ("the other one is …") were stale in both directions and are fixed.
- Waiver sweep: `src/common/helper.rs` held the codebase's only `#[allow]`
  (`dead_code` on `feature_not_compile`) — replaced by real cfg gating per
  §2, so the item exists exactly when one of its `cfg(not(feature = ...))`
  callers does. Two KCP-engine cast waivers carried comment-only reasons
  and moved to the attribute-reason style; five more keep their
  multi-line design comments (guarded casts, deliberate single-function
  flush/input), which is the form §2 asks for. The noise_stream rewrite
  removed its three `unsafe_code` waivers outright. No `#[allow]` remains
  in `src/`.
- Not actionable now: `snow` is the last network-layer protocol engine
  still owned by an external crate (yamux is the dependency-sinking
  candidate if that line continues — see the transport-comparison record).

### Direction ① design document: owning the mux layer (2026-09-20, proposal)

Design for absorbing the multiplexing engine in-repo — the last
network-layer protocol engine owned by an external crate (rust-yamux
0.14), and the direction this branch's `perf/data-path-optimizations`
line is building toward. **Proposal only: no code until a human signs off
on the phasing and the decision points at the end.**

#### Reference study: frp's fork (fatedier/yamux)

frp does not use upstream yamux — it maintains
[fatedier/yamux](https://github.com/fatedier/yamux), a fork of
hashicorp/yamux (Go, MPL-2.0, still tracking upstream: `const.go` carries
IBM's copyright and `mux.go` is byte-identical to upstream). Diffed
against upstream master on 2026-09-20 (both cloned to `~/tmp`):

- **The fork's substance is stream-reset correctness, not throughput.**
  ~1150 changed lines across `session.go`/`stream.go`, plus a new
  "Resetting a stream" section in `spec.md` and five test files
  (`stream_reset_*.go`) with interop testdata. The work: a `pendingReset`
  map that reserves a stream ID until its RST send completes or is
  canceled (a SYN for the same ID is ignored meanwhile — no ID-reuse
  race); a `sendReady` frame queue with cancel/claim/commit hooks so
  queued data, window updates and FINs are cancelable *before* transport
  commitment while a committed write still awaits its real transport
  result; RST sent as a WindowUpdate frame with the RST flag; and an
  explicit error-precedence table (ErrConnectionReset vs ErrStreamClosed
  vs EOF vs ErrSessionShutdown) for the reset/close/shutdown races.
- **The fork changes no defaults** — `mux.go` is identical to upstream
  (AcceptBacklog 256, keepalive on at 30 s, ConnectionWriteTimeout 10 s,
  MaxStreamWindowSize = initialStreamWindow = 256 KiB). frp's own tuning
  is one line at the call site, on both ends:
  `fmuxCfg.MaxStreamWindowSize = 6 * 1024 * 1024`
  ([server/service.go:720](https://github.com/fatedier/frp/blob/master/server/service.go#L720),
  [client/connector.go:154](https://github.com/fatedier/frp/blob/master/client/connector.go#L154)),
  plus trace-level yamux logging. molehill already tunes the same way
  (`mux_config()` in [multiplex.rs:96](src/transport/multiplex.rs#L96):
  64 MiB window / 32 streams).
- **The fork's wire discipline is the model to copy.** Its spec.md
  states the compatibility boundary explicitly: reset "requires no
  negotiation, protocol version change, or matching public API on the
  peer"; receivers accept RST in Data *or* WindowUpdate frames and must
  still consume a Data frame's payload to preserve framing. A fork that
  documents what is and is not wire-visible is a fork that stays
  deployable across versions — molehill's absorption must do the same.

Lessons for molehill: (1) a fork's maintenance burden is real and
two-sided — correctness drift (frp's reset work) and upstream fixes to
cherry-pick — so the absorption needs an upstream-tracking policy from
day one (AGENTS.md §7's porting-credit rule is the precedent); (2) frp
tunes at the call site, which is exactly what molehill can *not* do for
the levers below — that gap is the justification for ownership; (3) the
reset lifecycle is a correctness surface we inherit whether we fork or
not — rust-yamux's current behavior under molehill's data-channel pooling
deserves an audit item in phase 0.

#### What ownership unlocks (the levers, with their evidence)

- **L4 — header/body coalescing (new, from this branch's direct-decrypt
  work).** yamux's writer splits every frame into a 12-byte header write
  and a body write ([frame/io.rs](https://github.com/paritytech/yamux/blob/master/src/frame/io.rs)
  `WriteState::Header` → `WriteState::Body`, no inner flush between
  them), so the record stream emits a ~30-byte record (2 + 12 + 16 tag)
  plus the body record per frame — a full encrypt and syscall for 12
  bytes of payload, every frame. An owned mux writes header+body as one
  frame; the read side then asks for one buffer and the record stream
  decrypts once into it (the direct-decrypt path from `e463391` already
  exists). Two implementation options: (a) coalesce in the record stream
  (hold a pending partial record, append the next `poll_write`, push on
  `poll_flush` — works with the crate today, but changes when bytes hit
  the wire and needs the flush-path argument); (b) one frame write in the
  owned mux (natural, no contract subtleties). Recommend (b) as part of
  the absorption, with (a) as a pull-forward candidate if a measured win
  is wanted earlier.
- **L5 — per-frame body allocation.** yamux's reader allocates
  `vec![0; body_len]` per data frame (same file, `ReadState::Body`) — a
  fresh 16 KiB allocation + zeroing per frame on the read path, the same
  churn pattern the record-buffer pool (`3297d65`) just removed on the
  connection path. An owned mux reuses a pooled frame buffer (direction
  ③'s mechanism, applied one layer up).
- **L1 — conditional frame split size.** `DEFAULT_SPLIT_SEND_SIZE` is a
  fixed 16 KiB. 64 KiB measured faster on loopback but 30-60% slower
  under round-trip delay (recorded at
  [multiplex.rs:89-95](src/transport/multiplex.rs#L89-L95)); the right
  answer is conditional on the path, which needs code ownership.
- **L2 — the 32-stream ceiling.** molehill pins 32 streams per tunnel;
  at the default `count = 4` that caps a client at 128 concurrent
  connections, where the path starts failing (64 is the measured working
  point — HANDOFF matrix trims). A cap is policy, not wire: ownership
  lets it be raised or made adaptive.
- **L3 — window/streams decoupling.** yamux couples them
  (`window >= streams * 256 KiB`, asserted on every setter); independent
  tuning measured 30x regressions, so 64 MiB/32 is a fixed pair chosen
  with the handcuffs on. An owned credit allocator can give each stream
  its credit from a shared pool without the reservation invariant.
- **L0 — drop the compat shim.** yamux 0.14 speaks futures-io; molehill
  adapts every stream through tokio-util's `Compat`
  ([multiplex.rs:30](src/transport/multiplex.rs#L30)). An owned engine
  implements tokio's `AsyncRead`/`AsyncWrite` directly — one abstraction
  layer per stream removed.

#### Phase 0 done: vendored, wire- and behavior-identical (2026-09-20, `92fdde0`)

rust-yamux 0.14 now lives in `src/mux/` (12 files, ~3.1k lines), the
`yamux` crate dependency is gone, and `multiplex` gates the module.
Deviations from the vendored copy are all mechanical (paths rebased under
`crate::mux`, `tracing` instead of the `log` facade, `std` instead of
`web-time`/`static_assertions`, edition-2024 binding-mode fixups,
upstream property tests dropped for lack of `quickcheck`); the unused
graceful-close subsystem (`poll_close`, the `Closing` state, `closing.rs`)
was removed — molehill drops the connection instead of closing it — and
`set_split_send_size` stays under `#[expect(dead_code)]` for phase 3. The
pedantic sweep over the vendored code is fixed in code, with `#[expect]`s
only for guarded casts and invariants, each with a reason; both clippy
passes and the full suite (73 lib + 12 integration, serial) are green.

**Phase-0 A/B** (before `c7be865` / after `92fdde0`, both freshly built
with the commit SHA verified, same host, same method, 3 reps, 8 s tests;
arms `mux` + the forced `mux-off` plain control, loopback + loss1_rtt10;
results in `~/tmp/ab/results-p0-*.json`):

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| mux loopback 1-stream | 9.416 [9.159, 10.671] | 9.182 [8.730, 11.178] |
| mux loopback 8-stream | 24.641 [23.513, 27.334] | 27.284 [26.869, 28.009] |
| mux loss1_rtt10 1-stream | 4.265 [4.221, 4.323] | 4.356 [4.115, 4.413] |
| mux loss1_rtt10 8-stream | 14.435 [14.423, 16.088] | 15.920 [14.745, 15.948] |
| mux-off loopback 8-stream (control) | 27.821 [24.424, 28.248] | 24.587 [24.514, 26.506] |

**Verdict (§10):** no measurable change in either direction — every
movement is inside the rep spread, churn (4834 -> 4971 conn/s) and
first-byte p50 are flat, and the control arm moved *down* 11.6% (it cannot
be affected by this change), i.e. the after-run's machine state was if
anything less favorable. The 8-stream medians' +10.7% is inside the
spread and claims nothing; a pure vendoring was not expected to move
performance, and it did not provably do so. Phase 1 (tokio-native IO,
dropping the `Compat` layer) is next.

#### Phase 1 done: tokio-native engine IO (2026-09-21, `20ac557` + `8077360` + `fa5f113` + `2eb20b7`)

The engine speaks tokio's `AsyncRead`/`AsyncWrite` directly; `MuxStream`
is the engine's `Stream`, `Connection::new` takes the tokio socket, and
the `futures` + `tokio-util` (`Compat`) dependencies are gone. The
futures `Sink`/`Stream` impls on the frame writer became plain methods
(`start_frame` / `poll_flush` / `poll_next_frame`), `SelectAll` became a
`Vec<TaggedStream>` polled directly, `poll_close` became `poll_shutdown`,
and the unused `Stream`-of-`Packet` impl was deleted.

**The conversion found a real deadlock, and the root cause is worth
recording.** In the vendored engine the connection's poll loop drove the
frame writer through `poll_ready` *at the top of the loop*, so every
queued frame was written within the same poll. The tokio-native version
moves the drive into `poll_flush`, which runs once per iteration: the
loop wrote at most one frame per poll and then returned Pending. With
the command channel unbounded the writers run far ahead (168 queued
frames observed in a 512 MiB / 8-stream in-process transfer), and the
connection went to sleep holding them — an idle writer registers no
write waker, drained receivers no channel wake, a quiet socket no read
wake, so nothing would ever wake it, and the stranded frames (window
updates included) deadlocked the tunnel until an unrelated timeout broke
the cycle. Roughly half of the runs stalled; the futures-based parent
commit is 10/10 clean on the same repro at ~12.9 Gbit/s. The fix
(`8077360`): `pending_frames: VecDeque` so the receivers are polled on
every iteration (tokio's mpsc clears a receiver's waker registration
when it wakes, so a skipped poll leaves nothing registered for later
sends), and a `continue` back to the queueing block after the writer
goes idle while frames are queued — the drain the vendored loop got for
free, cut short only by the cooperative budget whose deferred wake
resumes it. Two supporting fixes ride along: `poll_read` delivers
buffered bytes before attempting the window update (a reader parked on a
full channel while holding data starves the peer's sender of credit),
and both park sites re-check their condition after storing the waker
(the check-then-act race where a wake lands between the check and the
store and finds no waker). The repro itself is not committed — it takes
20 s to fail and would hang CI on a regression.

**Phase-1 A/B** (before `bcfe1b6` / after `2eb20b7`, both freshly built
with the commit SHA verified, same host, same method, 3 reps, 8 s tests;
arms `mux` + the forced `mux-off` plain control, loopback + loss1_rtt10;
results in `~/tmp/ab/results-p1f-before.json` and
`~/tmp/ab/results-p3-after.json`):

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| mux loopback 1-stream | 10.684 [8.827, 11.025] | 10.537 [9.298, 11.602] |
| mux loopback 8-stream | 32.080 [31.302, 32.649] | 30.376 [29.136, 34.148] |
| mux loss1_rtt10 1-stream | 3.929 [3.866, 4.137] | 3.820 [3.728, 4.272] |
| mux loss1_rtt10 8-stream | 15.634 [15.421, 16.858] | 15.242 [13.665, 15.242] |
| mux-off loopback 8-stream (control) | 28.481 [28.406, 31.917] | 28.253 [26.767, 30.732] |

**Verdict (§10) — CORRECTED 2026-09-21 after an interleaved re-measurement
under the fixed bench.** The original verdict ("every data-path movement
is inside the rep spread") was wrong for the loss1_rtt10 8-stream cell:
the before/after runs were taken ~10 minutes apart, and that cell drifts
~12% between epochs (the same `2eb20b7` binary measured 15.242 in one run
and 13.67 in another). Alternating the binaries (bcfe1b6 / fa5f113 /
2eb20b7, two rounds, same host) settled it: **15.567 / 15.445 / 13.733
Gbit/s**. The tokio-native engine without `unconstrained` (`fa5f113`) is
FLAT against the vendored Compat engine (-0.8%); the -11.8% is
`unconstrained` alone — an unconstrained driver monopolizes its worker
until the poll returns Pending, and on a high-RTT link each wake carries
a large, latency-sensitive workload (the credit-returning reader tasks
are delayed, so the sender stalls). **`unconstrained` is reverted
(`32cd4e5`).** The in-process repro's +10% was real but irrelevant: it
measures a loopback duplex with no competing latency-sensitive work.

What survives from the original verdict: the churn cost and the RSS
growth. The churn cell dropped ~7% on the default count = 4 arm
(5013/4989 -> 4668/4655 conn/s across runs; the `mux-off` control was
flat), and a direct interleaved probe on the count = 1 arm puts it at
~-30% (main 5965/5337 vs the branch 4307/4255 conn/s). Isolated by
binary: the vendored engine with futures' mpsc (`bcfe1b6`) matches main
(5593/6406 conn/s), so the cost is the **mpsc channel swap** (futures ->
tokio), i.e. the price of dropping the `futures` dependency — one tokio
bounded channel (semaphore + pre-allocated block) per stream is heavier
than futures', and connection churn creates one per connection. Peak RSS
grew ~3x (22 -> 69 MB), consistent across the bounded- and
unbounded-channel variants, so it is not the queue — most likely
allocator retention from tokio's mpsc allocation pattern. The user's
acceptance allows memory and CPU growth; the churn cost is the one
number that is not inside a spread and it is flagged here for a human
decision (accept the dependency-removal cost, or fund a lighter
per-stream command channel — a per-connection shared queue with the
waker registered under the queue lock would be both cheaper and
airtight). An intermediate unbounded-channel variant (`8077360`)
measured -27.5% / -15.6% on the 8-stream cells and was rejected — with
the channel bounded at the vendored depth of 10 (`fa5f113`) those cells
return inside the spread, which is why the depth is pacing, not
backpressure. Phase 2 (L4 frame coalescing + L5 pooled frame body) is
next.

#### Phase 1 final verdict: unconstrained reverted, one cost remains (2026-09-21, `32cd4e5`)

The full re-verification under the fixed bench (interleaved: bcfe1b6 and
the reverted HEAD alternating, two rounds, one host, 3 reps, 8 s tests,
mux + mux1 arms, loopback + loss1_rtt10; results in
`~/tmp/ab/results-vr-{p0,rev}-r{1,2}.json`):

| arm / cell | bcfe1b6 (Gbit/s) | reverted HEAD (Gbit/s) |
|---|---|---|
| mux loss1_rtt10 8-stream | 15.58 [15.30, 15.86] | 15.16 [15.13, 15.19] |
| mux loss1_rtt10 1-stream | 4.28 | 4.11 |
| mux loopback 8-stream | 28.08 | 28.41 |
| mux loopback 1-stream | 10.06 | 10.23 |
| mux1 loss1_rtt10 8-stream | 4.69 | 4.97 |
| mux1 loopback 8-stream | 10.23 | 11.51 |
| churn mux loopback (conn/s) | 4983 / 4941 | 4694 / 4641 |
| churn mux1 loopback (conn/s) | 5001 / 4962 | 2133 / 2148 |

**Verdict:** with `unconstrained` reverted the data path is clean —
every throughput movement is inside the rep spread (the loss1_rtt10
8-stream regression of -11.8% is gone: -2.7%), and the mux1 8-stream
loopback cell gained 12.6%. The tokio-native engine (phase 1 without
unconstrained) is indistinguishable from the vendored Compat engine.

**The one remaining cost is the connection-churn rate, and it is the
`futures`-dependency removal's price, not a bug.** Isolated by binary
with an interleaved direct probe (one host, count = 1, connect -> 1 byte
-> close, 16 workers): main (external yamux + futures mpsc) 5965/5337
conn/s, the vendored engine with futures mpsc (bcfe1b6) 5593/6406, every
tokio-mpsc build 4183-4307. The bench agrees: -5.7% on the default
count = 4 arm, -57% on the count = 1 arm, with the first-byte p50 rising
3.1 -> 7.1 ms there. One tokio bounded channel (semaphore +
pre-allocated block) per stream is heavier than futures', and churn
creates one per connection. A per-connection shared command queue (the
waker registered under the queue lock — cheaper AND airtight by
construction, unlike `AtomicWaker`'s clear-on-wake) would remove it;
that is a deliberate redesign, not a revert.

**Human decision pending:** accept the churn cost (the dependency
removal is the phase-1 deliverable; the user's acceptance allows memory
and CPU growth but not latency/throughput regression — the churn's
first-byte latency sits in that gap), or fund the shared-queue redesign.

#### Phase 2 done: control-frame coalescing (L4), L5 probed and dropped (2026-09-21, `a1fe0bb`)

Bodies up to 512 bytes are staged into one buffer with their 12-byte
header and written in a single call (`Io::start_frame`); larger bodies
keep the two-phase write so a 16 KiB payload is never copied twice.

**The probe that scoped it** (in-process 128 MiB / 8-stream duplex,
counters on `drive_write`): 8735 frames produced 31400 socket write
calls — 3.55 per frame, the excess being partial-write retries. The
payload copies, not the call count, dominate the bulk cells, so
coalescing a 16 KiB body would trade a syscall for a memcpy; the
control frames (SYN/ACK/FIN/window update/ping) are the ones where the
call count matters, and they are exactly the small ones.

**L5 (pooled frame body buffers) was implemented, measured, and
dropped.** The probe put its ceiling at ~1% (one allocation per frame),
the pool needs a global lock because bodies are allocated on the
connection task and freed on the stream task (a thread-local pool never
recycles), and the first cut drained the whole pool on a size mismatch
for an 8x in-process slowdown. Recorded here so the next attempt starts
from the probe rather than from the code.

**Phase-2 A/B** (before `2eb20b7` / after `a1fe0bb`, same host, same
method, 3 reps, 8 s tests; arms `mux` + the forced `mux-off` control,
loopback + loss1_rtt10; results in `~/tmp/ab/results-p3-after.json` and
`~/tmp/ab/results-l4-after.json`):

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| mux loopback 1-stream | 10.537 [9.298, 11.602] | 10.635 [10.342, 11.453] |
| mux loopback 8-stream | 30.376 [29.136, 34.148] | 29.192 [28.757, 31.008] |
| mux loss1_rtt10 1-stream | 3.820 [3.728, 4.272] | 4.125 [3.883, 4.790] |
| mux loss1_rtt10 8-stream | 15.242 [13.665, 15.242] | 13.253 [13.166, 13.477] |
| mux-off loopback 1-stream (control) | 21.931 [20.278, 22.743] | 19.215 [18.550, 22.012] |

**Verdict (§10):** no provable gain — every loopback movement is inside
the spread and the churn cell did not move (4655.7 -> 4660.0 conn/s).
The loss1_rtt10 8-stream median dropped 13.1% with non-overlapping rep
ranges, but the loopback control arm moved -12.4% in the same run, i.e.
the after-run's machine state was materially less favorable and the
drop cannot be attributed to the change. The coalescing stays because
it is principled (half the write calls on the control-frame-heavy
paths) and costless, not because it measured a win. Phase 3 (L1:
conditional frame split) is next — the probe points at per-frame
overhead as where the remaining few percent live.

#### Phase 3 measured and reverted: the 16 KiB split stays (2026-09-21, `b37c80d` + `b36822f`)

L1 raised `DEFAULT_SPLIT_SEND_SIZE` from 16 KiB to 32 KiB (matching the
pairing layer's `TCP_COPY_BUFFER_SIZE`, so one `poll_write` becomes one
frame instead of two). **A/B against the L4 commit** (same host, 3 reps,
8 s tests, mux arm + the forced `mux-off` control, loopback +
loss1_rtt10; results in `~/tmp/ab/results-l1-after.json`):

| arm / cell | L4 (Gbit/s) | 32 KiB split (Gbit/s) |
|---|---|---|
| mux loopback 1-stream | 10.635 [10.342, 11.453] | 8.585 [8.157, 9.614] |
| mux loopback 8-stream | 29.192 [28.757, 31.008] | 20.026 [20.012, 27.130] |
| mux loss1_rtt10 1-stream | 4.125 [3.883, 4.790] | 4.228 [4.000, 4.332] |
| mux loss1_rtt10 8-stream | 13.253 [13.166, 13.477] | 13.782 [13.685, 13.782] |
| mux-off loopback 1-stream (control) | 19.215 [18.550, 22.012] | 21.089 [20.120, 23.028] |

**Verdict (§10):** the trade-off the design anticipated is real and
large — loopback 1-stream -19.3% and 8-stream -31.4%, both
non-overlapping, while the control arm moved +9.8%/-0.9% in the same
runs (the machine state was favorable, so the drop is the change);
loss1_rtt10 8-stream +4.0%, also non-overlapping. Bigger frames help
the high-RTT loss cell and hurt loopback badly. A conditional rule
(`split = f(rtt)`) would need a mid-RTT cell to pick its threshold —
the matrix has only 0 ms and 10 ms — so shipping an unvalidated
threshold is worse than the vendored default, which upstream chose for
a reason (yamux issue #100). **Reverted; a conditional revisit needs a
1-5 ms RTT cell in the matrix first.**

#### Phase 4: L2 landed (2026-09-21, `a97e1ef`)

`DEFAULT_MUX_MAX_STREAMS` is raised 32 -> 64 (`src/common/constants.rs`),
doubling the per-client concurrent data-channel ceiling at the default
`count = 4` (128 -> 256). The yamux credit reservation grows from 8 MiB
to 16 MiB of the 64 MiB connection receive window, leaving 48 MiB (75%)
for the window auto-tuner (32 streams left 56 MiB); the existing unit
test fails the build if the reservation ever reaches half the window —
the configuration that measured ~0.1 Gbps at 10 ms RTT (a ~30x drop) —
so that failure mode cannot come back silently. Both clippy passes, the
full suite (73 lib + 12 integration, serial) and the docs (en + zh,
bench mirror constant) land in the same commit.

**The ceiling, probed directly** (one host, `count = 1`, `pool_size = 16`,
iperf3 -P N, fresh client and fresh iperf3 server per probe — a failed
probe wedges the tunnel *and* the single-test iperf3 server, which is
why the bench skips this cell):

| binary (cap) | works | fails |
|---|---|---|
| before (32) | -P15: 14.7 Gbit/s | -P16: "control socket has closed" |
| after (64) | -P47: 9.3-11.2 Gbit/s | -P48: "control socket has closed" |

The arithmetic is exact: cap - pool (16) - iperf3 control stream (1) =
15 and 47. Two findings fall out: (a) exceeding the cap does not merely
fail the open — it closes the whole tunnel connection (the client logs
`connection is closed` and the pool does not recover without a
reconnect), which is what made the bench skip the cell in the first
place; (b) the bench's ceiling model (`count * MUX_MAX_STREAMS`) does
not account for the per-service pools or the control stream, so the
64-stream scale point would need cap >= 81 on `mux1` even with the cap
at 64 — the cell stays skipped there, and the `mux` (count = 4) arm's
64-stream cell never hit the cap either way (22.4 -> 23.6 Gbit/s,
single-rep reference).

**L2 A/B** (before `88de03f` / after `a97e1ef`, same host, 3 reps, 8 s
tests, arms `mux` + `mux-off` + `mux1`, loopback + loss1_rtt10; results
in `~/tmp/ab/results-l2-{before,after}.json`):

| arm / cell | before (Gbit/s) | after (Gbit/s) |
|---|---|---|
| mux loopback 8-stream | 33.224 [30.285, 33.886] | 29.188 [28.819, 29.231] |
| mux loopback 64-stream | 22.369 (1 rep) | 23.587 (1 rep) |
| mux loss1_rtt10 8-stream | 13.474 [13.309, 13.810] | 13.557 [13.518, 13.557] |
| mux1 loopback 1-stream | 9.754 [8.770, 9.870] | 10.409 [9.178, 11.125] |
| mux1 loss1_rtt10 8-stream | 5.186 [5.018, 5.208] | 5.011 [5.004, 5.021] |
| mux-off loopback 8-stream (control) | 27.500 [27.281, 27.637] | 26.834 [26.615, 27.197] |

**Verdict (§10):** no regression attributable to the change. The mux
loopback 8-stream median dropped 12.1% with non-overlapping ranges, but
the before-run is a fast outlier for its code: the same engine measured
29.192 on `a1fe0bb` and 30.376 on `2eb20b7` in this session's other
runs, and the after-run's 29.188 matches those, while the control arm
moved only -2.4%. Every other movement is inside the spread. The
`mux1` loopback multi-stream cells fail identically before and after
(8-stream: client timeout; 64-stream: iperf3 control socket) — a
pre-existing condition of that arm, and a manual -P8 probe through a
`count = 1` tunnel transfers 11.4 Gbit/s cleanly, so it is a
bench/iperf3 artifact rather than the path.

**L3 (window/streams decoupling) remains open** and stays the risky
one: yamux couples the window to the stream count through the
`window >= streams * 256 KiB` invariant, independent tuning measured
30x regressions once already, and an owned credit allocator is a
redesign of the flow-control core that the design's own rule says needs
a full-matrix A/B. It should be scheduled deliberately, not attempted
on a whim.

#### Bench fixes for the L3 A/B basis (2026-09-21, bench.py + docs)

Two bench problems surfaced while measuring L2, and they turned out to
be one problem with one fix.

**The ceiling model over-promised.** `variant_stream_ceiling()` returned
`count * MUX_MAX_STREAMS`, but a tunnel also carries the channels the
bench itself holds open — the server pre-opens `pool_size` data channels
per registered service (iperf and echo, plus the UDP echo's own 2) at
registration, and the measurement client's control stream is one more.
Probed exactly on one host as `cap - pools - control`: a `count = 1`
tunnel with `pool_size = 16` failed at the 16th data stream with cap = 32
and at the 48th with cap = 64 (15 and 47 usable). The model now subtracts
the pools and the control stream, so `mux1`'s ceiling is 45 with the
default `pool_size = 8` and the count = 4 arms' is 237.

**Why it mattered more than a wrong skip decision.** Exceeding the cap
does not merely fail the dial — it closes the tunnel connection (the
client logs `maximum number of streams reached`, then `connection is
closed`, and the pool does not re-establish the tunnel within the probe
window). The bench runs the 64-stream scale point *before* the 8-stream
cell, so an over-limit scale point killed the tunnel and the following
8-stream cell hung against it: every iperf3 rep timed out at the harness
bound (28 s) and the cell came back `null`. That is the whole of the
`mux1` loopback multi-stream "hole" — it was not flaky and not a path
problem (a manual -P8 through a count = 1 tunnel transfers 11.4-14.7
Gbit/s). With the model fixed the scale point is skipped with an accurate
reason and the 8-stream cell measures again: **17.768 Gbit/s, 3/3 reps**
(was `null`), 1-stream 10.02 Gbit/s.

Docs updated in the same commit: both READMEs (`count × 32` -> `count ×
64` in four places each, plus the usable-ceiling phrasing where the scale
point is discussed), `docs/configuration.md` + zh, and `docs/release.md`.
The v0.8.1 matrix numbers stay valid — those cells never hit the cap
(64 streams over 4 tunnels = 16 per tunnel).

**Still open for a good L3 A/B:** the loopback 8-stream cell is bimodal
(29.2 / 30.4 / 33.2 Gbit/s for the same code across this session's runs),
so an L3 comparison there should raise `MOLEHILL_REPS` rather than trust
a 3-rep median; and a `count = 1` arm can never measure the 64-stream
scale point (its usable ceiling is 45), so L3's per-stream credit work
should be read off the 1-stream and 8-stream cells plus the rtt10 cells.

#### Bench audit + peer-set switch (2026-09-21, bench.py / plot_bench.py / fetch_peers.py / READMEs)

**The peer set is now nps 0.26.10 (ehang-io/nps), frp 0.71.0, rathole
0.5.0.** `bore` left the set (its setup, the `TCP_ONLY` special case and
the fetch entry are removed — git has them if it ever comes back); nps is
a widely deployed Go multiplexer, so the third peer is representative of
the class rather than a minimal TCP forwarder. nps carries all three
service shapes (two TCP proxies + one UDP) and the bench's full metric
set runs against it: churn 4505 conn/s, echo RTT p50 0.463 ms, UDP
capacity 16.3 Mbit/s, RSS 75 MB, 1/8/64-stream throughput 0.14/0.13/0.17
Gbit/s.

Three nps-specific findings, each verified with a direct probe:
- nps resolves its config relative to the **executable's** directory (a
  config in the CWD is silently ignored — it bound the shipped defaults),
  so `setup_nps` builds a per-arm tree: hard links to the ~24 MB of
  binaries, a real `conf/` (the shipped registry files `clients.json` /
  `hosts.json` are linked too — without them the server panics at
  startup), and the web assets by symlink. The server also writes an
  sqlite db, so `ArmProcs.spawn` gained a `cwd` parameter.
- npc's ini parser does **not** tolerate spaces around `=` (it then dials
  the wrong transport and dies on a UDP write to port 0). The generated
  `npc.conf` uses the shipped no-space style; `nps.conf`'s own parser
  accepts either.
- nps's bulk forwarding is slow (~0.14 Gbit/s, two process hops per byte
  with small buffers) while its connection path is fast, and a
  **full-duplex** bulk echo through its bridge stalls (one-directional
  bulk runs at 27 Gbit/s) — which is why its head-of-line probe measures
  zero and the chart renders an absent slot, not a zero.

**Bench design problems found in this audit** (the ones worth a ticket):
1. **No interleaving in the A/B path.** Before/after runs are separate
   invocations ~10 minutes apart, and several cells drift ~12% between
   epochs — the same `2eb20b7` binary measured 15.242 and 13.6746 on
   loss1_rtt10 8-stream in different runs. That drift is what hid the
   `unconstrained` regression for a whole session (see the phase-1
   correction above): the before-run happened to land in the fast epoch.
   The bench should offer an alternating mode (round-robin the two
   binaries within one invocation) — every A/B in this session had to
   script it by hand.
2. **3 reps is thin for the bimodal cells.** The loopback 8-stream cell
   has a ±12% spread inside one run and ~15% across runs for identical
   code, so a 3-rep median can land on an outlier (it did: the L2 A/B's
   -12.1% was a fast before-run, not a regression). An adaptive rule
   (extra reps when the spread exceeds a threshold) or a per-cell rep
   count would tighten it.
3. **Only the loopback cell has a control arm** (`mux-off` is added for
   loopback only), so the drift-prone shaped cells are exactly the ones
   without a same-run reference. Extending the control to every compared
   cell is the structural fix for (1).
4. The churn probe is a single 3 s window; the mux1 (count = 1) regime
   was invisible until a manual probe measured it (the -57% churn cost).

**Plot problems:** `PEERS` was a hardcoded tuple (now the new set), and
`PEER_PALETTE` holds exactly three colours — a fourth peer silently
reuses the first colour rather than failing, so a future peer should
either extend the palette or derive colours from a hash. The UDP panel
filter is already data-driven (it dropped bore when bore carried no UDP
metrics; its comment said "bore" and now says what it actually does).
The rest of the rendering contract holds: peer-less cells are not
plotted, a null inside a compared cell is a grey `x`, a measured zero is
labelled `0`.

#### Wire compatibility boundary

Vendoring keeps the format byte-identical (the yamux spec: 12-byte
header, version 0, Data/WindowUpdate/Ping/GoAway, SYN/ACK/FIN/RST flags,
256 KiB initial stream credit). The boundary for changes:

- **Internal (safe, no version decision):** frame split size, stream
  caps, credit allocation policy, buffer management, keepalive cadence,
  the IO trait layer. All of L0/L1/L2/L4/L5 live here.
- **Wire-visible (out of scope for v1):** window-update semantics,
  credit accounting rules, stream-ID allocation, RST/FIN/GoAway
  semantics. L3 touches credit *allocation* (internal) but must not
  change the window-update *protocol*; any such change needs a protocol
  version decision, cf. the v3 selector precedent in
  [docs/internals.md](docs/internals.md).

#### Layering (mirror the KCP absorption)

```
src/mux.rs                 ← engine: vendored rust-yamux 0.14 + the levers,
                             molehill's own code (parallels src/kcp.rs)
src/transport/multiplex.rs ← adapter/integration, unchanged at first
                             (parallels src/transport/kcp.rs)
Cargo.toml: multiplex gates the in-repo module; dep:yamux and the
tokio-util compat dep drop out of the feature
```

rust-yamux 0.14's src is 3340 lines — smaller than the KCP engine plus
adapter we already own (2971), and it is a pure framing state machine
with no crypto.

#### Phased migration (each phase = one commit + one single-variable A/B)

- **Phase 0 — vendor verbatim.** rust-yamux 0.14 copied in as
  `src/mux.rs` (plus its error/tagged-stream modules), `dep:yamux`
  removed, `Compat` still used at the boundary. Wire-identical, behavior
  identical; the whole suite green proves the vendoring. Also lands here:
  the reset-lifecycle audit item (what rust-yamux does on RST/FIN under
  data-channel pooling — frp's fork shows the scale of that surface).
- **Phase 1 — L0: tokio-native IO in the engine.** Remove the compat
  layer. A/B: mux arm, loopback + loss1.
- **Phase 2 — L4 + L5: one frame write, pooled frame body.** A/B: mux
  arm, loopback + loss1 (expect the record-count and per-frame allocation
  probes to confirm the mechanism before the bench).
- **Phase 3 — L1: conditional frame split.** A/B: mux arm across
  loopback / rtt10 / loss1 — the cells where the trade-off lives.
- **Phase 4 — L2 + L3: stream cap and credit decoupling.** The riskiest
  (the 30x trap was already bitten once): full matrix, not a focused A/B,
  plus the 64-stream scale point for L2.

#### Risks

- Inheriting rust-yamux's reset/close/shutdown correctness surface with
  no upstream to file bugs against (frp's ~1150-line fork diff and five
  test files are the scale indicator).
- Upstream drift: rust-yamux 0.14 is a moving target (0.13 → 0.14
  restructured the connection into `connection/`); a tracking policy is
  needed before phase 0 lands.
- Lint surface: 3340 lines under molehill's pedantic-deny rules; the KCP
  absorption needed per-site `#[expect]`s with reasons — budget for the
  same.
- Phase 4 could regress weak-network cells in ways a focused A/B cannot
  see (the 30x lesson) — hence the full-matrix gate.

#### Verification plan (AGENTS.md §10)

Each phase's A/B: parent commit vs phase commit, same host, same method,
both binaries freshly built with the commit SHA verified; arms = `mux`
(the changed path) + the forced `mux-off` loopback control + `noise` as
the untouched-transport watch; cells loopback + loss1_rtt10, 3 reps, 8 s.
Phase 4 additionally: the full matrix against the current
same-method baseline before any merge. New instruments already in place
from this branch: the `noise-direct` arm and the record/alloc probes.

#### Decision points for the human

1. Approve the phasing, and phase 0 (vendor verbatim) as the first
   landed change?
2. L4 option (a) — record-stream write coalescing — as a pull-forward
   measured change before the absorption, or folded into phase 2?
3. Window-update semantics explicitly out of scope for v1 (L3 limited to
   allocation policy)?
4. Upstream-tracking policy for rust-yamux fixes (cherry-pick with
   credit, per AGENTS.md §7) — who watches, and how often?

### Benchmark ritual (per tag — see docs/release.md)

The ritual steps, gate thresholds and the uv/PEP 723 runner are documented
in [docs/release.md](docs/release.md). Environment-specific notes: loss
cells need `CAP_NET_ADMIN` (granted in the current container — netem cells
run; without it loss cells auto-skip and rtt cells run via the userspace
`weakproxy.py` fallback); bore's `--to` only accepts a bare host (port 7835
implied), so proxied cells shift its control port to 127.0.0.2. Runner
lifecycle: a global lock refuses concurrent runs (they used to reap each
other's live processes); Ctrl-C/SIGTERM leave a clean state (arms killed,
netem removed, full meta checkpointed); `--fresh` backs up the previous
results file to `.bak` first; the full matrix is ~3 hours at full rigor
(trim with `--tools/--cells/--variants`).

Known measurement limits (2026-09-09, fixed in the runner, recorded in the
README methodology): on rate-limited cells (netem `rate` on loopback),
eight parallel iperf3 streams wedge iperf3's single-test server — netem's
packet `limit` counts GSO-sized segments (up to 64 KiB), so the buffer
holds seconds of data at 20 Mbit/s and the final results exchange never
completes; the wedged server then dies with EBADF, which used to poison
every later arm of the cell with refused dials. The runner now isolates
backends per arm (SIGKILL cleanup + EADDRINUSE retry) and runs the 8-stream
test after the cheap probes; the rate20 8-stream slot is null with the
timeout reason in `partial_metrics` (rate100 measures both). The 2026-09-09
targeted re-runs refreshed the rate cells, the loss2b25 steady-RTT probes
and the fake-zero HoL entries on the same host — all of which the
2026-09-10/11 revision below then replaced.

**Method revision (2026-09-10, this thread) — the above diagnosis was
half-wrong and is superseded.** Decisive measurements on this host with a
plain (tunnel-free) iperf3 pair across the shaped `lo`:
- The shaper itself was the bigger problem. `netem rate 100mbit delay
  20ms limit 1` carries 18 Mbit/s with `-P 8`; the same test at `limit
  1000` carries 99.6 Mbit/s. The rate cells had been shaped with a queue
  so shallow that whole GSO segments were tail-dropped, so a "rate cell"
  number was largely a property of the shaper. `bench_lib.RATE_QUEUE_LIMIT`
  is now 2000 and is recorded in the results meta (`netem_rate_limit`).
- The remaining `null`s were harness repetition hygiene, not the path: with
  per-rep isolation plus a client timeout scaled to the test length, the
  same kcp4 `rate100_rtt20` cell that used to return `null` measures 3/3
  reps at both 1 stream (~0.098-0.103 Gbit/s) and 8 streams
  (~0.091-0.106 Gbit/s) — i.e. at the 100 Mbit/s link ceiling. A stalled
  rep used to wedge the single-test iperf3 server and starve every later
  rep (`Backends.run_throughput` now restarts it after a failure).
- Throughput convention: the headline is bytes over the measured window,
  with the receiver's own (drain-inclusive) window recorded too, because at
  a shaped cell `sum_received.seconds` runs well past the sender's and made
  the two figures look like different tests.
- UDP capacity: one 20k-pps burst sat ~2x above the forwarder's knee
  (measured 2k/5k pps -> 0% loss, 10k -> 30% at ~9.5 Mbit/s delivered,
  20k -> 100%), so `loss_pct` carried no information. It is now two points
  (paced + saturating, reported by delivered Mbit/s), sampled BEFORE the
  bulk-UDP HoL blast because the UDP path does not recover within an arm
  after a 50 Mbit/s burst (the HoL probe's own pinger then records 100%
  loss) — a forwarder finding worth its own look.
- Two further measurement bugs were caught while validating that revision,
  both from reading the per-rep raw JSON the runner now keeps:
  1. iperf3's per-interval `seconds` is not the interval span (the interval
     after `-O` reports warm-up + interval), so summing it gave 9.0 s for an
     8 s test and deflated the loopback headline ~12%. The window is now
     `sum(end - start)` over non-omitted intervals.
  2. At `rate20_rtt40` the sender's post-omit count is 0 for the whole
     measured window — its `-O` warm-up dumped 153 MB at 1.22 Gbit/s into
     the shaper and backpressure blocked the rest — so every tool (molehill
     AND the TCP peers) reported a 0.0 Gbit/s 8-stream cell. The headline is
     now the sender's bytes over the measured window, falling back to the
     receiver's count only when the sender's accounting is degenerate
     (receiver > 2x sender); the rate20 8-stream value is then ~0.02 Gbit/s
     (the shaped link rate) instead of 0.
- `iperf-raw/` artifacts are written per ARM **and CELL** (the work dir is
  per run, so arms used to overwrite each other's evidence; a first fix
  without the cell name still let cells overwrite each other), and
  `audit_results.py` is the completeness gate: `None` holes, arm errors,
  per-stream inconsistencies, missing sampler output, and the throughput
  endpoint invariant (exposed port != backend port) — it exits non-zero on
  holes/errors. That endpoint check exists because the opposite mistake
  shipped a whole invalid baseline (see the VOID note above).

Consequence: **no pre-revision number is comparable to the refreshed
baseline** (the revision changed the shaping model and the throughput
window/accounting, and the refresh ran on `0b073ddbf222`); the full
re-measure was done on 2026-09-10 (52 arms; `audit_results.py` reports zero
holes and zero arm errors) and `results-v0.8.0.json` + the charts + the
README chapter now describe that run. The v0.7.2 regression gate is
therefore informational only, and a rate-cell-only difference against it is
never a signal.

### Baseline refresh notes (2026-09-10/11, host 0b073ddbf222)

> **The refresh was re-run on 2026-09-10 after an endpoint bug, and the
> corrected baseline is what is committed now.** An intermediate refresh
> was measured with `Backends.run_throughput` dialing the iperf3 BACKEND
> instead of the tunnel's exposed port, so its TCP figures were the
> loopback ceiling with every tool bypassed (loopback 1-stream 48-54
> Gbit/s instead of the tunnel's ~10). Fixed by `fix(bench): dial the
> exposed endpoint and isolate every sample`: the endpoint
> is explicit (`_throughput_exposed_port` / `_bench_backend_port`), equal
> ports raise, and `audit_results.py` fails such a run. The final baseline is
> one same-host run of the corrected method (52 arms, zero holes, zero arm
> errors, guard clean) and the README/strategy numbers are derived from it.
> Caught by diffing a raw artifact's `connected` port against the cell's port
> map — the reason raw artifacts are kept, and the first rule of AGENTS.md
> §10.

- Corrected outcome, tunnel-measured on `0b073ddbf222` (one run of the
  revised method): Noise retains ~58%/76% of plain throughput, `count = 4`
  aggregates at 8 streams (loopback 19.5 vs 9.2 Gbit/s, 1% loss 12.3 vs
  4.5), and the KCP carrier stays far behind TCP wherever the path is not
  the bottleneck (loopback 8-stream 1.1 vs 14.9 Gbit/s, ~3x RSS = 83 vs
  26 MiB), with UDP-only paths and rtt100 session quality as its uses.
- **The UDP-under-load lead is retracted.** The head-of-line probe's paced
  pinger lost 100% of its datagrams on the default arms in two earlier runs
  and 2% in this one; the ladder probe shows no reproducible penalty either.
  Treat it as variance, not a path property. The code-level hypothesis
  (sticky per-peer affinity plus drop-on-full in `route_udp_datagram`, with
  the server's routed queue hardcoded to `DEFAULT_UDP_SENDQ_SIZE` = 1024 and
  `udp_send_queue_size` honoured only client-side) is still worth a look as a
  fairness question. **The instruments were run (2026-09-11) and settle
  nothing**: the stock pinger lost 8.3% then 10.8% in the same arm, and the
  rolling-socket control lost 100% — but that control is invalid (a 200 ms
  socket lifetime is shorter than the lazy UDP session establishment plus
  the ~100 ms path RTT, so replies to a closed socket are simply lost). No
  `queue full` drops were captured at that loss level. Net: the lead is
  variance; the fairness question stays open with no demonstrated defect.
  Re-running `~/tmp/udp_affinity_probe.py` (or `focused_run.py --hol-udp`)
  with a socket lifetime of ~1 s and `RUST_LOG=molehill_rathole=debug` is
  the next step if it is ever picked up.
- Structural gaps that remain (each with a typed reason in the data): the
  20 Mbit/s rate cell's 8-stream slot (`null`, client timeout — eight
  parallel streams cannot finish through that bottleneck) and kcp4's rtt100
  8-stream slot. Everything else is measured.
- The UDP ladder replaces the two-point probe: 500 pps to the configured
  burst rate, 2000-datagram bursts, 1.5 s drain, tolerance `max(2%, cell
  loss + 2pp)`. Measured: unshaped loopback is a lower bound (27.2 Mbit/s at
  the top step), the 10 ms cell bends at 12 000 pps (10.9 Mbit/s), the
  100 ms cell at 1 000-2 000 pps (0.7-1.4 Mbit/s); loss cells have no step
  inside tolerance, so the knee plus its delivered rate is the informative
  pair there.
- Container note: the environment was recycled twice during this work
  (hostnames `ebb615bff576` -> `fbf069a0506b` -> `0b073ddbf222`; the final
  baseline is entirely from `0b073ddbf222`, verified via the results meta).
  A recycle wipes `/tmp` AND the benchmark tooling: `sudo apt-get install -y
  iperf3 iproute2` (i.e. `just bench-deps`) must run again, and runs should
  keep their scratch under `~/tmp` (`TMPDIR=$HOME/tmp`), which survives.
- `focused_run.py` no longer applies a netem to unshaped cells and dials the
  exposed port, so it is a usable independent check (loopback ~10 Gbit/s,
  matching the matrix).
- History note (2026-09-11): the 24 fix-on-fix commits on top of `0f44213`
  were folded into seven topical commits and `merge-tcp4` was force-pushed
  once; the branch was then fast-forwarded into `main` (19 thematic commits,
  no merge commit) and both it and the pre-rewrite backup
  `backup/merge-tcp4-20260911` (`4c70eeb`, identical tree) were deleted.
- Open after the v0.8.1 release (none block it): the UDP fairness question
  above (instrument ready, no claim) and a single-window full-matrix re-run
  on the target host (this baseline was assembled across one run plus three
  targeted merges after the container recycle, and v0.8.1 carries it forward
  unchanged). The KCP loss cells were
  re-measured with the current post-conserve binary in this baseline, so that
  item is closed, and the `merge-tcp4` -> `main` merge is done.

### Control-channel teardown (2026-09-11, the v0.8.1 fix)

Found while smoke-testing a migrated 0.8 config: after a client's health check
removed a service and its control channel then died (reset), the service could
not re-register — `Port N is already in use` — until the **server** was
restarted.

Root cause: the connection pool that owns the bound public listener only ever
stopped when a *new* registration took the service over (dropping the entry's
`ControlChannelHandle`, which closes the broadcast both the pool and the
control task watch). A channel that ended by itself therefore left its pool —
and its listener — alive forever, and `bind_with_retry`'s 5 s window expired
against a leak rather than a race. Evidence: the old channel logged
`Control channel shutdown` with **no** matching `run_tcp_connection_pool:
Shutdown`, and a live `LISTEN` socket remained on the port.

Fix: the pool also takes the control task's `JoinHandle` and stops with it
(`select!` arm in both pools, plus the TCP pairing loop, which could otherwise
wait forever for a data channel that no control channel will ever create).
Wrong turns worth recording, because both looked right and broke the restart
path in `tests/integration_test.rs`:
- removing the service's map entry from a cleanup task that awaited the
  control task — it interacts with takeover churn (a client restart leaves the
  old client's service tasks alive briefly, and both clients then re-register
  in turn), which turned into a registration ping-pong;
- `impl Drop for ControlChannelHandle` sending the shutdown — the data-plane
  path *clones* that handle (`get2(&nonce).cloned()`), so any tunnel finishing
  tore the whole service down.

Deterministic repro: start a client whose service is unhealthy, let the health
check remove it, then bring the local service back — the re-registration used
to be rejected and now succeeds (verified against a migrated
production-shaped config with three `carrier = "kcp"` services).
`finished_control_channel_releases_its_ports` fails without the fix (the
exposed port stays bound 10 s after the client left) and passes with it; it
needs the short-heartbeat fixture `tests/for_tcp/teardown_release.toml`
because the server's control channel has no read loop and notices a dead
client only on a failed heartbeat write.

Chart/runner follow-up (2026-09-10): absent measurements used to render as a
fake `0.0` bar in the cost chart (`mux1`'s unmeasured 64-stream and
mixed-bulk slots), and the two structural nulls looked like opaque runner
failures. `plot_bench.py` now marks a value missing inside a compared panel
with a grey `x`, and no longer draws rows/cells that are not part of a
comparison: the molehill-vs-peers per-cell panels plot only the five cells
the peers run, the mux chart is a single loopback row, and the cost chart's
64-stream panel omits `mux1`. `bench.py` skips by design the 64-stream scale
point above an arm's `count × 32` yamux ceiling (reason in
`partial_metrics`);
`bench_lib.throughput()` surfaces iperf3's own error/exit status instead of a
bare `KeyError`; `hol_probe.ping_tcp` records a connected pinger that
completes zero or one round trip as a stall (`ping_max_gap_ms` = the elapsed
wait) instead of `null`.

Targeted refreshes on the re-created container (`ebb615bff576`; **superseded** —
the 2026-09-10/11 revision replaced the whole file with a single run on
`0b073ddbf222`): `mux1` loopback (mixed bulk 9.0 Gbit/s, previously inherited
the wedged iperf3 server), and frp/rathole `rate20_rtt40` (HoL now
3003 / 1986 ms; both had zero-or-one pinger replies over repeated runs, so the
probe change — not a transient — is what records them). The host could not
reproduce the baseline for `mux-off` (~20% low: 22.6 vs 28.0 Gbit/s 8-stream
under current load), which is one reason that file was rebuilt from scratch
instead of patched further.

KCP optimization refresh (2026-09-10, full rigor, kcp4 arm only — also
superseded by the 2026-09-11 re-measure on `0b073ddbf222`, which is what the
committed file holds):
loopback 1-stream 2.496 -> 3.191 Gbit/s (+28%, A/B against the
pre-optimization code on the same host: 2.405), rtt10 8-stream 0.39 ->
0.653 (+67%), loss1 0.356/0.413 -> 0.418/0.686 (+17%/+66%), loss2b25
0.353/0.382 -> 0.393/0.691 (+11%/+81%), loss5 0.025/0.045 -> 0.029/0.038
(two complementary passes merged), rate100 0.023 -> 0.028, rate20 0.005
-> 0.004 (floor), jitter 0.127/0.114 -> 0.151/0.176, rtt100 0.04/0.051
-> 0.038/0.064. Cell fragility notes: loopback 8-stream measures ~1.4
today for BOTH code versions (A/B: old code 1.116, new 1.453; the
committed 5.9 was not reproducible on this host — same story as
`mux-off` above); rtt100 1-stream flaked to 0.008 in one pass and the
backfill's 0.038/old-code's 0.04 stand; rate100 1-stream flaked to
0.008 in the first pass, the backfill's 0.028 matches the old code's
0.025; rate100/rate20 8-stream stayed unrecordable: the 8-parallel-stream
iperf3 client timed out on every rep at the shaped bottleneck
(`iperf3 -P 8` wedges the single-test server; each attempt was killed
by the 30 s harness timeout — same reason the committed rate100 8s
was a fake zero and rate20 8s was already None). **Superseded by the
2026-09-10 method revision below: these slots are measurable now.** Peer
binaries are
cached under `~/tmp/bench-peers` (not `/tmp`, which session cleanup wipes)
and pinned to the baseline versions (frp 0.71.0 / rathole 0.5.0 / bore
0.6.0), fetched directly because the unauthenticated GitHub API was
rate-limited.

KCP in-repo self-maintenance (2026-09-10): the KCP protocol engine moved
from the `third_party/kcp` path dependency into `src/kcp/`, maintained as
molehill's own module whose algorithm follows the reference C
implementation by skywind3000. A path dependency cannot be published —
cargo strips it and compiles against the registry version, so `cargo
publish` would have failed against the unpatched crates.io kcp 0.6.0 (the
release workflow's `publish-crate` job runs in parallel with
release/docker, so the failure would surface only after the GitHub Release
and image were already out). `cargo package` now verifies cleanly.

KCP alignment pass (2026-09-10, audited against skywind3000/kcp master):
fastack-conserve — the kcp crate 0.6.0 cargo feature was INVERTED vs the C
define (feature ON = the aggressive/unconditional variant), so every
molehill release so far shipped the aggressive variant while its docs
claimed "conserve"; the engine now implements the reference's ts-gated
conserve semantics. Wire-visible only under loss/reordering: the kcp4 loss
cells in the benchmark were measured with the aggressive variant; the
2026-09-10/11 re-measure on `0b073ddbf222` used the post-conserve binary, so
that item is closed. Other fixes: the timeout ssthresh now halves the flush-entry cwnd
(`prior_cwnd`) like the reference; `KCP_PROBE_INIT` 7000→5000; stream-mode
`send` reports partial progress instead of erroring after appending
(unreachable in the adapter, which chunks at 64 KiB); `peeksize` no longer
overflows on a hostile `frg = 255`; the dead conv-adoption hook
(`input_conv`) removed (the adapter routes by (addr, conv) via `get_conv`).
Remaining benign deltas: relative `check()` return, `Err(NeedUpdate)` on
pre-first-update flush, bool `nodelay` (the reference's nodelay=2 mode is
unreachable — the adapter uses 1).

KCP optimization pass (2026-09-10, two tuning changes on top of the
absorption, both validated against the committed baseline on the same
host):
- datagram IO batching on Linux (`recvmmsg`/`sendmmsg`, up to 32
  datagrams per syscall — `src/transport/udp_batch.rs`, the second
  audited unsafe site after `src/common/multi_map.rs`): loopback
  1-stream +28-33% (A/B: 2.405 -> 3.191), weak cells flat-to-better;
  the EAGAIN path runs through `UdpSocket::try_io`, which clears
  tokio's cached readiness, so the park-then-drain loops never spin;
  a full kernel send buffer drops the datagram and KCP's ARQ re-emits
  it on the next flush.
- SACK thresholds 50 ms / 32 segments -> 10 ms / 16: the old cooldown
  sat ABOVE the nodelay RTO floor (~30 ms), so the SACK fired only
  after the RTO had already retransmitted — it was inert on loss
  cells. Tuned, the SACK beats the RTO backoff: the loss cells in the
  refresh above improved +11-81% (1-stream and 8-stream).
MTU is NOT a tuning direction (IP-fragmentation risk on real paths):
the `set_mtu` probe (1400 -> 8000 measured +84%..+476% on the weak
cells but -29% loopback 8-stream) was reverted and removed from the
tuning space.

Comparability boundary (v0.7.2 era): results files before schema v3 measured
throughput by dialing the **backend directly** (bypassing the tunnel), so
every tool
reported the loopback iperf3 ceiling (~46 Gbit/s) regardless of tool or cell;
the rtt cells ran via `weakproxy` (client↔server delay only), which the
bypassed throughput never traversed. `results-v0.7.2.json` was refreshed in
place with schema-v3 through-tunnel data; `results-v0.7.0.json` is the
pre-matrix (v1) baseline and is only kept for history. The gate's baseline
line is v0.8.0 vs v0.7.2 — a cross-method comparison rather than a gate: the
v0.7.2 file predates the revision and comes from another container, so only
v0.8.0-and-later same-method results are comparable (see the baseline
paragraph above).

## Transport comparison: 4 arms implemented, 3 merged (decision record)

All four arms were implemented, committed and integration-tested end to end
on the `transport-test` branch (kcp_tunnel and quic_tunnel ran their full
lifecycle in the serial suite). The branch is **deleted**; the comparison
history (including the QUIC arm) survives in the local tag
`archive/transport-test`. **Fork decision:** arms 0-2 (N×TCP default, KCP
optional) are merged into `main`; **arm 3 (QUIC) was left out** —
behind on loss cells (quinn "too many gaps" at rtt10), no peer auth on the
QUIC leg, and N×TCP measured better in every comparable cell. The bench
matrix on main covers mux / mux-off / mux1 / noise / kcp4 only.

| Arm | Topology | Stack | Status |
|-----|----------|-------|--------|
| 0 (baseline) | 1 TCP tunnel | transport `T` (Noise by default) + yamux | main (`[client.data].default_count = 1`) |
| 1 | N TCP tunnels (bench: N=4) | N × (`T` + yamux), round-robin stream placement | main (default `default_count = 4`) |
| 2 | N KCP sessions (bench: N=4) | N × (UDP + KCP + Noise + yamux) — KCP replaces only the plaintext TCP leg | main (optional `default_carrier = "kcp"`) |
| 3 | 1 QUIC connection | quinn (built-in transport crypto, no Noise/yamux); data channels = native QUIC bi-streams | **archived** (`archive/transport-test` tag; not on main) |

Design decisions (merged part):

- Config surface on main (0.8 layout): `[client.data]` holds the client-wide
  defaults (`default_mode` = `multiplex`/`direct`, `default_count` = N
  (default **4**; arms 1/2), `default_carrier` = `"tcp"`|`"kcp"`,
  `default_data_addr` = data-plane endpoint, defaults to the service's control
  endpoint) and each `[client.services.<name>]` can overlay
  `mode`/`count`/`carrier` plus `remote_addr`/`heartbeat_timeout`/
  `retry_interval`/`token` (defaults in `[client.control]` / `[client]`);
  per-service transport: `[client.services.<name>].transport` with
  `type` ("noise" / "plain" / unset = follow `[client.transport].type`)
  and per-service `noise` keys (fallback to the global keys); the client is dual-transport per service
  (`ClientStream`/`ClientTransport` enums, the KCP Noise wrapping uses the
  service's effective keys);
  the server declares no carriers (v3): the registration carries the
  service's carrier, and the server lazily binds its KCP UDP listener on
  the first `kcp` registration (bind failures are precise registration
  rejections); the listener uses `[server.data].bind_addr` (default = the
  control address). The
  yamux window/stream knobs (`mux_receive_window` / `mux_max_streams`) were
  removed from the config surface and fixed at 64 MiB / 32 — yamux couples
  them and independent tuning measured 30x regressions. Server needs no
  arm-1 change: every tunnel hello carries the same session nonce and feeds
  the same pool.
- KCP: crate `kcp` 0.6 (pure-Rust port of the C reference; state machine
  only) behind a thin in-repo tokio adapter (`src/transport/kcp.rs`):
  per-session pump task owning `Kcp` + UDP socket (select: writer channel /
  socket recv / `check()`-driven timer), bounded channels both ways for
  backpressure (`wait_snd()` cap on the write side), server listener
  demultiplexes sessions by `(peer addr, conv)` from `get_conv()`.
  **Fixed parameters (recorded for the comparison):** stream mode,
  `set_nodelay(true, 10ms, fast-resend 2, nc=1)`, snd window 2048 / rcv
  window 4096 segments (~2.8/5.7 MiB in flight at MTU 1400), 32 MiB socket
  buffers (a full-window burst overflows the ~208 KiB kernel default even on
  loopback, and every drop escalates that segment's RTO x1.5), dead_link
  default 20. Noise rides on top of the
  KCP byte stream (`NoiseStream<KcpStream>`) iff the control transport is
  `noise` (same keys/pattern); `carrier = "kcp"` composes with both `plain`
  and `noise`.
  Keepalive: a 2 s adapter-level PING/PONG keeps idle tunnels warm (NAT
  mappings) and doubles as the pacer's RTT/congestion signal; a vanished
  peer is still only confirmed on the next write (dead-link after ~20
  RTOs).
  **v3 (0.8):** every connection starts with a one-byte transport
  selector (`0x00` plain / `0x01` noise) — the server accepts both on one
  listener and `[server.transport]` holds only the Noise keys (no `type`;
  the client decides, informed by the noise-vs-plain bench); KCP sessions
  use the same selector on their byte stream. The `kcp` carrier is
  client-declared in the registration, so `[server.data].carriers` is
  gone and the UDP listener binds lazily on first use (no KCP clients =
  no UDP socket open).
  **Matrix trims (0.8):** peers run the loopback / rtt10 / loss1 cells
  plus the rate cells (reference points), probe durations shortened
  (hol 5 s, steady ping 100, weak cells 10 s), and `just bench-fast`
  gives a ~2-min molehill-only dev smoke matrix into results-dev.json
  (excluded from plot/regression). `just test-fast` runs the lib + core
  integration subset (~1 min vs the ~72 s full suite).
  **New metrics (0.8):** CPU% (per-process utime/stime delta, one-core
  base — the noise/KCP tradeoff rows finally have data), connection
  churn (connects/sec + setup-to-first-byte p50/p99 under 16 concurrent
  short connectors — the mux-vs-direct and pool_size guidance data),
  sustained UDP capacity (paced 20k pps, delivered pps + loss), a
  64-stream scale point, a mixed workload (iperf bulk + interactive
  latency on two services of one client concurrently), and per-rep
  min/max spread on throughput. New cells: rate-limited r100/20 and
  r20/40 (netem rate; fall back to plain weakproxy without a modern
  iproute2) and a jittery j20/10. Two real findings surfaced by the new
  probes: (1) yamux's fixed 32-streams-per-tunnel ceiling caps mux at
  count x 32 concurrent connections — at the default count=4 that is
  128, where the path starts failing (64 is the measured working point);
  **Throttle design (0.8):** the full matrix saturates every core by
  design and once froze the dev host; it now self-throttles — bench.py
  raises its nice value to 10 and every arm waits for loadavg < 0.7 x
  nproc (max 30 s) before starting, which both keeps the host responsive
  and makes each arm start from a quiet machine (cleaner numbers). The
  churn probe was cut to 16 concurrent connectors x 3 s (still a storm;
  ~1/4 the CPU) and the 64-stream scale metric runs a single rep (it is
  a working-point reference, not a statistic).
  (2) an unpaced UDP blast (>100k pps) wedges the shared mux tunnel
  (subsequent TCP metrics fail) while direct mode stays healthy — mux
  shares one sendq across services, so a pathological UDP flood can
  starve TCP on the same client. Neither is gated by bench-check yet.
  **Measured outcomes (optimization round, results-opt-*.json):** window
  doubling bought +29% single-stream on loss1/rtt10 and +20% on
  loss5/rtt100 (loopback flat) at ~2x RSS; a 5 ms flush interval was
  rejected (loopback 8-stream -3x). KCP still loses throughput to TCP
  carriers in every cell (loopback ~2.5 vs ~11 Gbps 1-stream; loss5/rtt100
  ~0.03 vs ~0.28) but wins the latency axis on the worst cell: udp p50
  601 vs 826 ms and hol max gap 1515 vs 1808 ms over the noise-TCP arm.
  Its defensible use case: UDP-only paths (TCP blocked/throttled by
  firewall/NAT) plus latency-first interactive traffic on high-loss,
  high-RTT links.
- QUIC: quinn (ring backend) + rcgen self-signed ephemeral server cert,
  client uses a no-op cert verifier — the tunnel payload is still bound to
  an authenticated control channel via the session-nonce hello, but the
  arm is **experimental: no peer authentication on the QUIC leg**. First
  bi-stream carries `DataChannelTunnelHello`/ack, later bi-streams are data
  channels (`DataChannel::Quic` on the server, one QUIC connection per
  control session).
- Feature `kcp` is additive and in the **default set** so the single
  `target/release/molehill` the bench tooling builds covers the tcp/kcp
  arms (embedded/minimal/container builds keep their explicit slim feature
  lists and are unaffected).
- Merge status: the merged part (arms 0-2 + the bench arms below) is in
  `main` (fast-forwarded on 2026-09-11, no merge commit);
  the QUIC arm and the full comparison history live in the local tag
  `archive/transport-test`. Bench variants on main are `mux` (plain,
  `count = 4`, the default baseline), `mux-off` (plain, `mode = "direct"`,
  loopback-only control), `mux1` (plain, `count = 1`), `noise`
  (`count = 4`) and `kcp4` (noise, `count = 4`, `carrier = "kcp"`). `kcp`
  is in the default feature set so
  the tooling's single `target/release/molehill` covers the merged arms
  (repro_e2e.py documents that default-features expectation).

## References

- rust-yamux: <https://github.com/paritytech/yamux>
- rathole benchmark: <https://github.com/rathole-org/rathole#benchmark>
- frp docs: <https://gofrp.org/en/docs/>
- KCP reference implementation: <https://github.com/skywind3000/kcp>
- quinn: <https://github.com/quinn-rs/quinn>
