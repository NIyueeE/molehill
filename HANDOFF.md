# HANDOFF: Working State & Future Work

> **State as of 2026-10-19.** **v0.11.1 is released: the tag names `4874f05`,
> the `main` tip at tag time; `main` carries `version = "0.11.1"`.** v0.11.1 moves the wire to
> **protocol v5.1** (a deliberate v5.0 break), makes transparent (L3)
> services cross-platform through `tun-rs` — Linux/macOS clients and servers,
> Windows servers, a Windows client refused with the WSL2 path — and removes
> the crate's three production `unsafe` blocks. The earlier branch content
> (CI build-matrix change `5e585b7`, the bench-harness refactor) is part of
> the merged release.
> **v0.11.0 is released**: the tag names `f3295e8`, and the release sweep at
> `d0f22b9` passed `just bench gate` (90 samples over five arms; the record is
> `benches/records/results-bench-v0.11.0.json`). The v0.11.1 sweep ran on
> 2026-10-18 (90 samples; `just bench gate` passed without a baseline); its
> record is `benches/records/results-bench-v0.11.1.json` and it was the
> baseline until the v0.11.2 record.
>
> **v0.11.2 is ready for review on `feat/v0.11.2` (2026-10-19):** the
> branch's manifest carries `version = "0.11.2"`; `main` stays at the released
> 0.11.1 until the PR merges. The cycle consolidates the configuration model
> (one pool per forwarding session, one L3 lane budget, fixed KK Noise in four
> flat layers), adds a global `max_tunnels`, adaptive retry/heartbeat
> defaults, the path-derived UDP policy, protocol v5.2 with dynamic striping,
> the configured/actual status API and the symmetric L3 device/address step.
> `just check` is green. **The release sweep was re-run after the CI fix**
> (19:19–21:04 UTC, 90 samples, five arms, fingerprint `257343fa3fd158d3`;
> `just bench gate` PASS **without** a baseline); the record is
> `benches/records/results-bench-v0.11.2.json` and it is the next baseline.
> The branch is squashed to eight commits, with the benchmark record as a
> docs-only commit on top. No tag exists: the release is a human decision.
>
> **Operator decision (2026-10-19):** the maintainer's deployment stays on
> forwarding (L4) until L3 is stable; the open threads record the gaps that
> hold it back and the `tunnels` naming question.
>
> **This file owns the working state**: what is open, what was decided and why,
> and the release checklist. Per AGENTS.md §3 it is an agent page — user-facing
> facts belong in the docs pages, and anything released belongs in
> CHANGELOG.md. The closed records — incidents, superseded sweeps, measurement
> records, retracted diagnoses, the release-preparation history and this
> cycle's whole investigation — live in
> [HANDOFF-archive.md](HANDOFF-archive.md), which this page indexes.
>
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).

## Where this stands

**The carrier swap has landed (2026-10-27).** The product now runs `mhd` — the
new engine under `src/transport/mhd/` — for its UDP data plane, and both KCP
files are deleted: `src/mhd.rs` (the engine) and the 4,146-line adapter body
that used to be `src/transport/mhd.rs` (that path is now the new module root).
The pool holds `Arc<dyn PoolLane>`; a YamuxLane (TCP) and an `MhdLane`
(`mhd`) implement it, and the server's `mhd` listener serves each stream a lane
accepts through the same transport-selector + Noise + hello opening a TCP data
channel uses. Noise is per stream; a validation stream at lane establishment
makes the server reserve the lane's `max_tunnels` slot before the pool counts
it.

The swap was not a rename of the adapter: the engine grew the pieces the product
needs and the integration suite found the defects. Fixed here, each with a test
where one was possible:

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

**The cost investigation (2026-10-28) fixed the engine's data path.** The P2
measurement above is the result; what it cost to get there is the record below,
because each of these was a real defect, not a tuning knob:

- the receive buffer was sized from the caller's nominal batch (32 KiB) while
  the kernel coalesces up to 64 KiB of segments per `GRO` run: every run was
  truncated by `recvmsg` and the tail was dropped *silently* — an 8 MiB transfer
  stalled 4 KiB in while the sender re-sent 220 MB. `MSG_TRUNC` now detects it
  and the region is `GRO_BUF`;
- the reader staged the whole contiguous prefix and memmoved the remainder per
  caller-sized slice (a memmove per byte); it now fills the caller's buffer
  directly, and a received range that continues the piece before it is appended
  to it instead of becoming a tree entry per 1365 bytes;
- a packet that fell below the acknowledgement bitmap's 64-number window could
  never be retired by any later ack, so it was re-sent for the connection's
  life. The block now carries a cumulative frontier (`Ack::below`) and retires a
  flight of any width;
- control packets consumed packet numbers but were never retransmitted, so a
  lost one left a permanent hole in that frontier (the lane then stalled with its
  window open). They repeat the last data packet's number and carry
  `FLAG_UNNUMBERED`, so the receiver does not count them;
- a finish sent as an unacknowledged reset could be lost and the peer's reader
  then waited for ever. A finish is now a tracked, empty stream frame at the
  stream's end offset, retransmitted like data;
- data for a stream this end had already retired was rejected as
  `UnknownStream` and therefore never acknowledged; the sender counted ten
  unanswered retransmissions and declared the *lane* dead. Every short-lived
  visitor connection eventually killed the lane the others shared — the bench's
  `iperf3: control socket has closed unexpectedly`. Such data is now dropped and
  its packet acknowledged;
- the record layer's resumed `poll_write` answered with the *old* buffer's
  length: a 32778-byte count for an 8202-byte buffer, which the stripe sender
  recorded and then sliced past the end of (a panic and a dead group);
- the stripe sender rotated a frame to another stream after a `Pending`, on the
  assumption that `Pending` means nothing was taken. A record layer that has
  already encrypted the bytes into a record breaks that assumption, so one frame
  could land on two streams and tear the receiver's framing. A frame now stays
  on the stripe it was offered to.

**Verification so far.** `just check` — both hook layers, i.e. the whole chain the
objective names — is **green on `90fc20b`** and again on the phase-timer commit
(2026-10-29): `cargo fmt --check`, the
secret scan, `cargo machete`, the docs-alignment gate, ruff lint *and* format,
pyright, the bench self-check, both clippy passes (default features and the
minimal `server,client` build), then `cargo audit`, `cargo deny`,
`cargo outdated` and the full serial suite: **twelve test binaries, all green**,
304 lib tests among them. `integration_test` is 22/22 under
`MOLEHILL_DATA_CARRIER=mhd` and 22/22 under `=tcp` with the same tree.
(`/home/dsh/tmp/v1-cost/check2.log`, `exit=0`.)
**`just check` is intermittently red on one timing test.**
`pool_test`'s `a_saturated_pool_still_serves_the_next_visitor` has now failed
twice inside `just check` — once with the host busy from a bench run, once on an
idle one — and passed **six times out of six** standalone on the same two trees,
at 9.2–10.2 s against 133–150 s for the failures. Its failure signature is a pool
that reports `size: 1, count: 2, grows: 0`, i.e. a pool that never attempted to
repair, which is a liveness gap worth its own investigation rather than a load
artefact; the earlier note here blamed load, and the second occurrence falsifies
that. Until it is understood: a red run of *this test alone* is not evidence
about a change, and a green `just check` is what the gate means.
`pool_test` under a *forced* `tcp` still fails the three tests that use the
diagnostic cross-carrier handover hatch (the server never binds UDP when the
client declares TCP); that is a harness/diagnostic interaction, not a data-path
failure, and the forced modes the release gate names are the integration
suite's.

**Still open, in order:**

1. **The cost gate fails on the bench model, and it is the stop condition.**
   Two `screen` runs with `--aa` on this host, 2026-10-28, the same method
   fingerprint (`2bdcdece60a72f0c`), binaries built from `47a0016`:

   - **TCP carrier** (`/home/dsh/tmp/bench-20261028-163041.json`): `l4` clean
     **0.6459 s/Gbit** (11.92 Gbit/s, 7.58 cores); `l4~aa` 0.6529 — the run's
     own floor is **1.1 %**. `l3` 0.2051 s/Gbit (21.10 Gbit/s).
   - **mhd carrier** (`/home/dsh/tmp/bench-20261028-163945.json`): `l4` clean
     **0.9819 s/Gbit** (7.10 Gbit/s, 6.50 cores); `l4~aa` 0.8764 (7.15
     Gbit/s). That is **1.52×** TCP's per-byte cost against a 1.1 % noise
     floor, so `cpu_s_per_gbit(mhd) ≤ TCP` is **not established** — it is
     refuted, by a wide margin.
   - **Two defects remain in the same arm, both reproduced twice:** the `l3`
     arm **times out** (the harness's 106 s client bound, two rounds), and
     `l4` failed one measured round with `iperf3 error: control socket has
     closed unexpectedly`. The warm-up cell completes now (it did not before
     `47a0016`), so the arm is no longer dead — but it is not reliable either.
   - **Re-measured 2026-10-29 in one profile, both carriers, three samples
     each, after slow start:** mhd `l4` clean **0.9645 / 0.8958 / 0.8994**
     (median 0.899) against TCP **0.6424 / 0.6369 / 0.6297** (median 0.637) —
     **1.41×**, still refuted. The liveness fixes did not move the per-byte
     cost, which is what the analysis predicted: the cost is the path's
     per-datagram delivery, not the carrier's bookkeeping.
     (`/home/dsh/tmp/bench-20261029-054214.json` mhd,
     `/home/dsh/tmp/bench-20261029-054422.json` TCP.)
   - **The `cost` scenario hangs on `mhd` at full scale, and only at full
     scale.** I first wrote this down as "the `smoke` profile's `l4` arm fails",
     which was wrong: what actually happens is `--scenarios cost` with the *cost
     scenario's own* params (`scale: 1.0`, six bulk streams) — the profile only
     supplied the round count. At that scale the arm hangs at the visitor's
     connect (successive runs took 246 s, 651 s, 1 375 s and 1 410 s before
     their timeout), with the **last known-good binary** (`47a0016`, built in a
     worktree) exactly as with the current one, while **TCP completes the same
     cells** in the same topology. `screen` — the same scenario at
     `scale: 0.5` — completes `mhd` in 33 s.
     So this is a **load-dependent stall in `mhd`**, the same theme as the L3
     lending failure below, and not a regression from the liveness work: it
     reproduces on the pre-slow-start binary. The `screen` numbers are the
     comparable ones; the full-scale stall is the next measurement to take, and
     it should be taken with the serve path instrumented rather than from the
     bench's connect timeout.
   - **The `l3` timeout is the transparent path under concurrency, and it is
     half fixed.** `just l3-accept` (root, three namespaces) is the focused
     harness, and on mhd it fails where TCP passes. Sixteen concurrent L3
     flows over one lane lost traffic badly: 14 of 16 timed out. The cause of
     the first half was a window leak in the probe path — `on_timeout`'s early
     probe re-sent the flight while keeping it, so the same bytes were counted
     twice and the congestion window closed on any loss (`8f0f728`). Two flows
     now time out instead of fourteen.
     **The rest of it was the congestion window, and that is now fixed.**
     Sixteen concurrent L3 flows of 64-byte round trips ran at **576 round
     trips/s aggregate against 17 398 on TCP**, with the carrier's pump idle
     ninety percent of the time. An 8 MiB *initial* window made the same arm
     pass, which identified the cause rather than guessing at it: the window
     grew additively from the first acknowledgement — congestion avoidance
     from the start — so it opened as a function of *bytes acked*, and a
     workload of small packets needs tens of thousands of acknowledgements to
     reach a window the path could carry in a few round trips.
     The window now opens in slow start (every acknowledged byte buys a byte of
     window) up to a two-megabyte ceiling, latches onto the additive rule at the
     first loss, and is never cut (`6a20a9e`, `0536741`). **Same arm, after:
     2 990 round trips/s — 5.2× — and it passes.** A single flow went from
     1 760/s to 1 794/s, i.e. unchanged, which is the point: the fix was for the
     concurrency collapse, not for latency.

     What the window fix needed to be safe, and why:

     - the ceiling, because the doubling outgrew the peer's socket buffer and
       the path dropped everything long enough for the pool to **reap a live
       lane** (`reaped dead tunnels ... reason="dead"`, seen in a bench arm);
     - a dead lane is now declared on twenty seconds of *silence from the
       peer*, not on ten expired timers: a peer whose buffer is full drops
       every datagram but its keepalives still arrive, which is what tells
       "saturated" from "gone".

     **Still open in this arm: the two lane-lending checks, and they are
     load-dependent.** With sixteen concurrent flows an idle claim's lane moves
     to the busy claim while its flows run, and half of them time out (8/16 ok
     in 6.4 s, against a 1.07 s pass for the same workload without a move).
     At **eight** flows the *whole* acceptance run passes, lending included —
     so the move is correct and the drain is fine; what it does not survive is
     sixteen flows. TCP runs the same two claims and the same sixteen flows and
     passes, which is why this is filed as a carrier-side stall under the move
     rather than a defect in the lending mechanism.
     The next step is to instrument the move window itself — the drain, the
     lane's close, and the re-registration on the busy claim — with the same
     per-phase tracer, at sixteen flows. The hypothesis worth testing first is
     that the move's drain waits on the carrier's write path while the lane is
     already saturated, and that the wait is charged to the flows on the
     *other* lanes.
   - **One theme covers both of the above, and it is the thing to fix next:**
     `mhd` stalls under high concurrency — sixteen L3 flows with a lane move,
     and the cost scenario's six streams at full scale — while TCP carries the
     same loads. Both stalls are load-dependent, both reproduce with a
     pre-slow-start binary, and both sit above the per-packet path (the
     L3 verdict was that the carrier's own pump is idle ninety percent of the
     time while the workload waits). What is *not* yet known is where the wait
     is: the hub's per-lane queues, the serve path's Noise record layer, or the
     block of code the two paths share. That question has one instrumented
     answer and it is the next measurement to take.
   - **Reproduced on loopback, and narrowed to the round trip.** A probe service
     on one lane, with a bulk transfer running on another, over a two-service
     config (`/home/dsh/tmp/cost2/probe.sh`): connect a fresh TCP visitor to the
     probe's exposed port, send four bytes, wait for the echo. Times are
     milliseconds, median and worst:

     | carrier | bulk streams | connect | round trip (median / worst) |
     |---|---|---|---|
     | tcp | 4 | 0.2 / 1.6 | **1.9 / 9.4** |
     | mhd | 1 | 0.2 / 1.4 | 2.2 / 11.3 |
     | mhd | 4 | 0.4 / 3.7 | **42.8 / 1 596** |
     | mhd | 6 | 0.3 / 3.2 | 9.9 / 657 |

     So the TCP *accept* is fine on both carriers (0.2–0.4 ms), and the stall is
     the first data round trip of a new stream: from 2.2 ms with one bulk stream
     to a 1.6-second worst case with four. A second and a half is the shape of a
     timer, not of queueing — the same class of defect as the lost wake-up fixed
     in the first round, but on the stream-open path. The next measurement is
     that path with the phase tracer: `open_stream` → hello → ack, and where the
     handshake's wake-up goes missing while the window is held by bulk streams.
   - **It is the per-stream Noise handshake, and what holds it up is a stream
     nobody retires.** Instrumenting `open_stream` split the 502 ms exactly:
     `open_channel` (which is the Noise KK handshake) **502.3–503.9 ms**, hello
     **4 µs**, ack 0.2–1.7 ms. 502 ms is `INITIAL_RTO` — the sending side of that
     lane never sent data, so it has no round-trip sample, and one lost handshake
     packet costs the whole initial timer.
     Why the packet is lost is the finding that matters: the client's log has
     **39 576 `dropped data for a retired stream`** in one probe run — a peer
     sending megabytes into streams whose reader is gone. A visitor's connection
     ends, the client drops its handle, and `close_stream` half-closes the stream
     (a *finish*), which tells the peer "my write direction is over" and leaves it
     sending. The lane then carries data nobody will read, and a new stream's
     handshake queues behind it.
     **Three fixes were written, measured and reverted** — the honest record is
     that they did not show a win: abort instead of finishing on a dropped handle;
     send control frames before stream data in `poll_transmit` (the reset branch
     was unreachable while any stream had bytes, exactly like the credit frame in
     the first round); repeat a reset three times because it is unacknowledged.
     After them the probe read 6/9/24 probes per run with 4–5 timeouts, against
     8/25/37 with 0–3 before: not a win, and possibly a regression, so they are
     not in the tree. The diagnosis stands, the fix does not exist yet.
     **The tighter experiment was then run, and the fourth attempt failed too.**
     `probe2.sh` — a fixed 25 ms probe cadence, three repeated runs, the drop
     counter as the primary signal — measured the abort-on-drop change against
     the baseline: drops **27 139 / 31 909 / 38 678** against **39 687 / 40 539
     / 56 188** with no timeouts 0/2/2 against 3/3/4. The counter moved by a
     third, which is inside the baseline's own spread (39 687–56 188), and p50
     went 13.2/636.6/43.2 → 70.7/94.1/72.5 ms, i.e. the probe's variance is
     still larger than any effect. Reverted, not landed. **Stopping here on this
     defect** (three-strikes, AGENTS.md §9): the diagnosis is sound, four fix
     attempts produced no measurable win, and the next attempt needs a different
     instrument — a per-phase trace of the stream-open path *with the bulk load
     held constant*, not another protocol change measured through this probe.
   - **Therefore the sequence stops here** — the gate is red and the plan says a
     failed P2 gate stops the phases. What no longer holds is the *reason* it was
     called unreachable: see the correction below. P3b/P3c, P4, P5 and P6 stay
     unstarted, and no cost or tail claim may be made about `mhd`.
   - **Where the gap comes from: the earlier answer was wrong. This is the
     correction (2026-10-29).** The record said the bench's `veth` paths refuse
     `UDP_SEGMENT` with `EINVAL`, that the sender therefore always falls back to
     `sendmmsg`, and that the receiver pays one `recvmsg` per 1400 bytes. **That
     is false, and the probe that produced it was the defect:** `UDP_SEGMENT`'s
     control message carries a **`u16`**, and a four-byte payload is rejected by
     the kernel's own length check with `EINVAL` — which is exactly the error
     the probe reported. With a `u16` payload the same `sendmsg` is accepted:
     - **the product already emits GSO on that path.** Temporary counters in
       `send_run` (reverted; the tree carries none) logged, in one two-second
       transfer over a 1500-MTU `veth` pair at 6.0 Gbit/s, **24 000+ successful
       GSO batches of 44 spans** and no bulk fallback, and both sockets accepted
       `UDP_GRO` (no `refused GRO` line).
     - **GRO pairs with GSO and with nothing else.** Through a *forwarding* hop —
       three namespaces, `net.ipv4.ip_forward=1`, the shape the bench has — one
       GSO super-packet of 32 × 1400 bytes arrives as **one 44 800-byte run**
       with `UDP_GRO` and as 32 datagrams without it; a plain burst of 256
       datagrams is **256 `recvmsg` calls either way**, because separate skbs are
       not coalesced.
     So the per-byte syscall cost on the gate's path is a forty-fourth of the
     datagram count, not one per datagram, and the claim that the gap is "not
     the engine's own code" is withdrawn. What the numbers say now: the same
     binary reads 0.549 s/Gbit on loopback and 0.982 on `veth`, with the same
     1400-byte datagrams — so the path contributes roughly 0.35 s/Gbit and this
     engine's own per-datagram work (parse, stream lookup, copy, ack
     bookkeeping, once per 1400 bytes) roughly 0.55. Kernel TCP pays that work
     once per 64 KiB TSO segment. **The gap is therefore mostly inside this
     engine's per-datagram path** — measurable, attributable and reachable —
     rather than a property of the path that forbids a fix.
   - **Re-measured from scratch before any of the above (2026-10-29, `90fc20b`).**
     The `screen` profile, `--aa`, one carrier per run, the same method
     fingerprint (`2bdcdece60a72f0c`): TCP `l4` **0.6342** s/Gbit (12.25 Gbit/s,
     7.72 cores) with its twin at 0.6341 — a 0.02 % floor; mhd `l4` **0.9013**
     (5.56 / 6.65 / 9.12 Gbit/s across three rounds, 6.0 cores) with its twin at
     0.8827 but rounds of 0.79 / 1.01 / 0.88. The ratio reproduces at **1.42×**,
     and two things beside it are new: the mhd arm's own A/A spread is **±13 %**,
     twenty times TCP's, and its `rtt_p99` in the same cell was 330 / 692 /
     2 358 ms against TCP's 1.8 ms. **The carrier is not yet stable enough for a
     15 % claim on one run**, which bounds what any optimisation can be *shown*
     to do; the phase timers below are what that instability gets explained with.
     (`/home/dsh/tmp/v1-cost/base-tcp.json`, `base-mhd-l4.json`.)
   - **The instrument exists now, and it answers the question (2026-10-29).**
     `MOLEHILL_MHD_STATS=1` was documented and absent (the KCP adapter's text
     survived the swap with no code behind it); it is now implemented in
     `src/transport/mhd/stats.rs` and the two configuration pages describe it.
     `tx_build`/`tx_send` are timed on every pass; `rx_total`, `decode`, `ack`
     and `append` are sampled one datagram in sixteen, so a timestamp pair costs
     the path about one percent of the work it measures.
     One `cost` cell at `screen`'s scale, `MOLEHILL_DATA_CARRIER=mhd`, the
     release binary, the payload carried 30.40 GB (243 Gbit), the cell reading
     `cpu_s_per_gbit` 0.9096 at 6.655 cores (`bench-20261029-075613`):

     | phase | cumulative | per Gbit | share of the carrier |
     |---|---|---|---|
     | sender `tx_build` (566 403 runs, 41.8 µs each) | 23.66 s | 0.097 | 38 % |
     | sender `tx_send` (509 712 GSO syscalls, 27.0 µs each) | 13.76 s | 0.057 | 22 % |
     | receiver `rx_total` (0.838 µs per datagram) | 18.2 s | 0.075 | 29 % |
     | receiver `tx_send` (acks, 10.7 µs each) | 5.47 s | 0.023 | 9 % |
     | everything else (`rx` acks, receiver `tx_build`) | 1.3 s | 0.005 | 2 % |
     | **the carrier's own path** | **62.4 s** | **0.257** | 100 % |

     Total CPU for that cell is `cpu_s_per_gbit × 243 Gbit` ≈ 221 s, so **the four
     timed phases are 28 % of everything this carrier spends**, and they name the
     carrier's framing and receive path at **0.257 s/Gbit**.
     The other 72 % — 0.653 s/Gbit — is not a remainder to wave at: it holds the
     record layer, the `StreamIo` copies between the record layer and the stream
     buffers, the pool's copy loop, and the kernel, and only the last two of those
     are shared with the TCP lane. **The tempting reading — "0.653 is TCP's whole
     0.634, so the gap is exactly the timed phases" — is a coincidence until the
     untimed carrier work is measured**, because it assumes mhd's above-carrier
     cost equals TCP's total, which cannot be true while TCP has a carrier cost of
     its own. Until then the honest claim is: framing and receive cost 0.257 s/Gbit
     in four named phases, and **at least part of the remaining 0.276 gap is
     untimed carrier work** — `StreamIo::poll_read`/`poll_write` and the record
     layer are the two places it can be, and measuring them is the next step.
     Inside `rx_total`, `append` (the copy into the stream's receive buffer) is
     52 %, `ack` 12 %, `decode` 5 %, and 30 % is the rest of the datagram's
     bookkeeping.
   - **The budget closes, and it does not favour this carrier (2026-10-29, same
     day, the record layer and the stream copies now timed).** Two `cost` cells in
     one profile, one carrier each, the same release binary, the switch on:
     mhd `0.8736 s/Gbit` (26.24 GB, 32.36 s, 5.51 cores) against TCP `0.6432`
     (47.6 GB, 32.25 s, 7.53 cores) — **1.36×**. The per-Gbit decomposition, every
     figure measured rather than inferred:

     | component | mhd | TCP | how it was read |
     |---|---|---|---|
     | record layer (`seal` + `open`) | **0.207** | **0.266** | 26.5 µs per 31.8 KB record against 9.7–13.6 µs per 10.8 KB — the same 0.83–0.90 ns per byte, and mhd's bigger records make it *cheaper* |
     | the carrier's own framing, send and receive | **0.257** | — | the four phases of the previous table |
     | the stream handle's two copies (`io_read`, `io_write`) | 0.033 | — | 2.86 s + 3.94 s |
     | everything else (pool, kernel, wakeups, the guest loop) | 0.405 | 0.377 | the residue of each total |
     | **total** | **0.874** | **0.643** | `cpu_s_per_gbit` |

     The residue is a residue, so the honest statement is the difference of two
     measured totals, not a measured 0.405. What makes the table worth trusting is
     that it *closes*: mhd's four measured groups are 98.4 s of its 183 s, and the
     0.231 s/Gbit gap is 0.257 (carrier) + 0.033 (copies) + 0.028 (residue) − 0.059
     (the record layer it wins) = 0.259. Two independent cells, one fingerprint.
   - **Re-measured after a data loss, and the range is the honest number.** Every
     artifact this page cites lived under `~/tmp`, and on 2026-10-29 at 10:09 the
     whole directory was emptied by something outside this repository (the runs'
     work directories with it; nothing here deletes it — `grep` finds no `rmtree`
     in the bench). Evidence therefore lives in `target/evidence/` from now on,
     inside the tree and git-ignored, and the pair was re-taken:
     **TCP `0.6275 s/Gbit` (12.31 Gbit/s, 7.66 cores) against mhd `0.9646`
     (7.27 Gbit/s, 6.98 cores) — 1.54×** (`target/evidence/cost-tcp.json`,
     `cost-mhd.json`). Three same-day mhd cells now read 0.8736, 0.9093 and
     0.9646; the *ratio* moves with them, from 1.36× to 1.54×, which is the
     instability again and the reason every claim below is stated as a range with
     its method rather than as a point. What does not move: the record layer's
     share (mhd 0.21–0.22, TCP 0.26–0.27 s/Gbit) and the carrier's own
     0.25–0.26 s/Gbit, which is the part this project owns.
   - **What that means for P2's gate, in one line.** This carrier's own
     incremental cost is **0.29 s/Gbit**; closing the gap means cutting it by
     about eighty percent, and no amount of per-datagram tuning removes the fact
     that TCP's equivalent work happens inside the kernel's socket layer while
     ours happens in `poll_transmit`. The plan's gate is therefore **refuted with
     a complete account**, not merely unmet — which is the answer the sequence was
     waiting for, and the decision it hands back is the human's.
   - **The carrier's own 0.257, split per packet (2026-10-29, sampled 1 in 16).**
     On the sender, per packet inside `tx_build` (35.6 µs for a 42-packet run):
     stream selection 0.092 µs, framing and the payload copy 0.262 µs, flight
     table and cursor 0.194 µs — 66 % of the build, with the rest the per-packet
     lock, the clock read and the loop. **No phase is big enough to be the
     answer**, which is the same conclusion as the coarse table at finer grain:
     the cost is spread over six small steps, and the only lever that is not a
     micro-optimisation is doing them on the thread that produced the payload.
     That is what the reference implementations do: a QUIC connection is driven
     by the task that writes to it (`quinn-proto`'s `Connection::poll_transmit`
     runs from the application's own poll), while here the writer appends to the
     send buffer and wakes a *different* task, which then reads the payload back
     across a core boundary. The experiment that follows from this is written
     below, not guessed at.
   - **Where a real attempt would still go.** `tx_build` is the largest single
     phase inside the carrier (0.083–0.097): 41.8 µs to frame forty-four
     datagrams is a microsecond each, roughly three times a 1400-byte copy — the
     ack block is re-encoded per packet, the stream range is a map lookup per
     packet, and the run is `Vec::extend`ed packet by packet. Then `rx_total`
     (0.064), where `append` is the piece to look at, then `tx_send` (0.054), which
     is the kernel segmenting a sixty-kilobyte `UDP_SEGMENT` write and is not
     reachable from here without bigger datagrams, which the plan forbids on
     loss-amplification grounds.
   - **A batched receive was tried and is refuted; it is not the gap.** The
     receive side only ever had the GRO path (one run per `recvmsg`), so the
     obvious next move was the `recvmmsg` half the module header describes. Two
     implementations were built and measured: a run-or-batch mode with
     hysteresis, and a batch-only receive with coalescing switched off. Both
     moved data; the first broke the striped integration test (a run read out of
     the wrong buffer — a mode that flips *after* staging a run hands the caller
     zeros), and the second passed every test but measured **worse on both
     paths**: loopback 0.653 s/Gbit against 0.549 with GRO, and the bench's
     `l4` arm **1.10–1.32 s/Gbit against 0.982** (`bench-20261028-191140.json`),
     with the cell taking 63 s instead of 33. A `recvmmsg` whose queue holds one
     datagram pays the whole descriptor setup for one packet, so batching a
     shallow queue costs more than the syscall it saves.
     **Conclusion: the receive syscall is not where the bench's 0.34 s/Gbit
     lives.** That still holds, and the reason it is not there is the correction
     above: GSO and GRO are working, so a pass already carries 44 datagrams in
     one syscall and comes back as one run. Batching a path that is already
     batched can only add work — which is what both implementations measured.
   - **The other question, measured the same day, and it does not go better
     (2026-10-29).** Success criterion 3 asks whether the carrier wins *one* cell
     beyond the run's own noise — a `loss1`/`loss5` throughput cell or a
     multi-flow `rr-*` p99. The `dev` profile's three cells were run at
     `--condition loss1`, one carrier per run, `control` and `l4` and an A/A twin,
     three rounds (`loss1-tcp.json`, `loss1-mhd.json`, fingerprint
     `5735319b28a1bd44`):

     | cell | control | TCP `l4` | mhd `l4` |
     |---|---|---|---|
     | `bulk-1` P1 | 4.41 Gbit/s | 3.80 (3/3) | 3.96 but **1 of 3 failed** |
     | `bulk-n` P8 | 31.2 | **14.93 (3/3)** | **0 of 3 — every attempt failed** |
     | `rr-16` c16 | 614.9/s | **608.7/s** | **541.3/s (−11 %)** |
     | `rr-16` p99 | 246.2 ms | 246.4 | 246.4 (the condition's own floor) |

     The twins bound the noise: TCP's `l4~aa` is within 1.4 % on every cell, and
     mhd's within 1.5 % on `rr-16`, so the 11 % is real. The failures are typed by
     the harness — `client timed out after 25s` and `iperf3 error: unable to
     receive results` — on the eight-stream bulk, three times out of three, while
     TCP carries the same cell next to it. **The carrier does not win a loss cell;
     it fails one TCP completes.**
   - **The 502 ms stall survives the obvious fix, and the floor is why.** The
     opening stream's end is a numbered packet, so its acknowledgement is the
     connection's first round-trip sample: the dialer was made to wait for it,
     bounded at 50 ms, before the lane was returned. Measured with the carrier's
     own new `us=` field on `mhd stream opened and acknowledged`, under the same
     probe: **502 053 and 502 498 µs — unchanged.** Two reasons, and the second is
     the one that matters: no sample arrived inside the wait (a bulk load keeps the
     pump busy), and *even with one* `Recovery::rto` clamps to `MIN_RTO`, which is
     **200 ms** — forty times the round trip a healthy path here reports. Reverted
     and recorded: a fix for this stall has to move the recovery floor, which is a
     change to loss recovery on every path rather than a local repair. The `us=`
     field stays, because it is one clock read per stream open and it is how the
     stall is visible in any run.
   - **Compared against a mature implementation, and we already match it.** The
     obvious question at this point is what a production userspace transport does
     that this one does not, so `quinn-proto` 0.11.9 was read next to this tree
     (`/home/dsh/tmp/ref/quinn-proto-0.11.9`, fetched from crates.io). Three of
     its four cost mechanisms are already here, at the same shape:
     `Connection::poll_transmit(&mut self, now, max_datagrams, buf: &mut Vec<u8>)`
     is called with a **caller-owned, reused buffer** exactly like `Staging`;
     it fills up to `MAX_TRANSMIT_SEGMENTS` datagrams per call for one
     `sendmsg` with `UDP_SEGMENT`, as `send_run` does; and its send side copies
     the payload out of the stream's `Bytes` chunks into that buffer, as
     `encode_stream` does. The second copy is not something a better
     implementation avoids.
     The one place it is genuinely different is the **receive** side: quinn's
     `assembler::Assembler` stores `Bytes` chunks (`insert(&mut self, offset, bytes:
     Bytes, …)`, `bytes.split_to(...)`), so a datagram's payload is reference
     counted, never copied, and handed to the reader as a slice — while
     `RecvBuf::insert` here copies every payload into a fresh `Vec` piece. That is
     `append`, measured at 0.19–0.44 µs per 1400-byte datagram, or **0.02–0.04
     s/Gbit** — real, worth having, and one tenth of the 0.25 s/Gbit the carrier
     owns. Adopting it means the datagram buffer becomes `Bytes` (one allocation
     per received run instead of one `Vec` per datagram) and `wire::decode`
     reports a payload *range* rather than a slice, which is a contained change to
     `udp_batch.rs`, `conn.rs` and `stream.rs`.
   - **And it is under this host's measurement floor.** The zero-copy receive is
     worth 0.02–0.04 s/Gbit against cells whose own A/A twin spreads by **13 %**
     of 0.87–0.96, i.e. ±0.11–0.13 s/Gbit. The improvement is a fifth of the noise:
     it could be landed, but it could not be *shown*, and this page has refused
     four changes for exactly that reason. So it is written down rather than
     built, and it stops being buildable the moment the number it targets is
     smaller than the number that measures it.
   - **The measurement floor is now the problem, and it has a target.** Every
     remaining carrier item is 0.02–0.10 s/Gbit while the same 30 GB cell reads
     0.8736, 0.9093 and 0.9646 across three same-day runs, with per-round
     throughput of 5.56 / 6.65 / 9.12 Gbit/s — a **1.6× swing inside one run**.
     A carrier that is stable would make every item above measurable, and the
     swing itself is a defect a visitor would feel. The next instrument is the
     per-round `mhd_health` pair (`datagrams_out`/`retransmits`) beside the
     per-round throughput, three rounds each of a stable and an unstable cell: if
     the slow rounds retransmit more, the instability is recovery; if they do not,
     it is scheduling.
   - **The swing is not recovery (measured the same day).** The same cell, four
     rounds, `MOLEHILL_POOL_STATS=1` beside `MOLEHILL_MHD_STATS=1`, the per-tunnel
     `mhd_health` pair read at the end of each round
     (`target/evidence/instability.json`):

     | round | s/Gbit | Gbit/s | datagrams out | retransmits | retransmit share |
     |---|---|---|---|---|---|
     | 0 | 0.8901 | 8.99 | 425 876 | 107 | 0.03 % |
     | 1 | 0.9328 | 8.58 | 592 926 | 153 | 0.03 % |
     | 2 | 0.9779 | 8.18 | 577 702 | 100 | 0.02 % |
     | 3 | 0.8038 | 9.95 | 395 542 | 90 | 0.02 % |

     Throughput swings 1.2× and the retransmit share does not move: on a clean cell
     the carrier loses almost nothing in every round, so the swing is **not**
     recovery. What moves with it is cost per byte, in the wrong direction — the
     slow rounds spend *more* CPU per byte than the fast one — which is the
     signature of work that grows when the bytes arrive differently, not of work
     repeated. The next suspect is the receive side's scheduling (the per-pass
     wake-ups and the channel between the dispatcher and the lanes) rather than
     loss recovery, and the same four-round shape measures it.
   - **The runtime width was costing a quarter of the CPU — for both carriers
     (2026-10-29).** The `cost` cell, mhd, four rounds at each width
     (`target/evidence/mhd-workers*.json`, `tcp-w2.json`, `default4-*.json`):

     | worker threads | mhd s/Gbit | TCP s/Gbit | ratio |
     |---|---|---|---|
     | 2 | 0.51 | 0.39 | 1.32× |
     | 3 | 0.60 | — | — |
     | 4 | 0.66 | 0.49 | 1.34× |
     | 6 | 0.81 | — | — |
     | 8 | 0.92 | — | — |
     | 20 (one per core) | 0.93 | 0.64 | 1.45× |

     A worker thread with nothing to run still looks for work, and both daemons
     started one per core. The width is a *multiplicative* win — both carriers get
     roughly a quarter cheaper — so **the ratio does not move** (1.32–1.45×), which
     is the cleanest confirmation yet that the gap is the carrier's own
     per-datagram work rather than a tuning artefact. Throughput moves much less
     than cost: mhd carried 6.8 Gbit/s at two threads and 8.0 at four, against
     12.1 for TCP at either.
     The product now starts `min(4, cores)` workers with `TOKIO_WORKER_THREADS`
     overriding it (`src/main.rs`, `docs/deployment*`, CHANGELOG), because four is
     the widest setting that still ties the TCP carrier's cost.
   - **The pair after the width change, with its own floors (2026-10-29).**
     `screen`, `--aa`, `--arms l4`, three rounds, one carrier per run
     (`target/evidence/final-tcp.json`, `final-mhd.json`):

     | | l4 median | rounds | A/A twin | run's floor |
     |---|---|---|---|---|
     | TCP | **0.4830** | 0.4689–0.4910 | 0.4848 | 0.97 % |
     | mhd | **0.6494** | 0.6393–0.6579 | 0.6791 | 4.35 % |

     **1.34×**, gap **0.166 s/Gbit**, far outside both floors. Against the start of
     the same session (mhd 0.90–0.96, TCP 0.62–0.64, gap ~0.29) both carriers are
     ~25 % cheaper per byte and the carrier's excess has **halved in absolute
     terms** — half of the 0.257 s/Gbit recorded earlier was runtime contention
     rather than carrier work, which is what the width sweep says it had to be.
     The *ratio* is unchanged, so the gate is still red; what changed is that the
     remainder is now small enough to be measured: mhd's floor fell from 13 % to
     4.35 %, which is a better instrument than any of the optimisations it can
     now resolve.
   - **Item (2), the cell that fails under one percent loss: measured, and not
     fixed (2026-10-29).** `bulk-n` under `loss1`, `--arms l4`, six or more rounds
     a run: the TCP carrier passes **12 of 12** samples at 16.7–17.5 Gbit/s, mhd
     passes 2–4 of 8–12 at 9.3–10.4 Gbit/s. It fails in two shapes — a visitor
     that cannot connect at 0.3–0.4 s, and a result exchange that dies at
     17.6–18.9 s after a transfer that ran.
     Four mechanisms were tried, and only one is kept:
     - **Kept: `INITIAL_RTO` 333 ms** (RFC 9002 §6.2.1's `kInitialRto`) instead of
       500. A lane whose first handshake packet is lost waits this timer before it
       can open anything; four lanes dialled under one percent loss measured
       **504 ms** of establishment and read **335.4–336.4 ms** with 333.
     - **Tried and reverted: abort instead of half-close on a dropped handle, and
       a reset ahead of stream data.** The number that recommended it was real —
       `dropped data for a retired stream` went from **68 492** lines in one failed
       round to **0** — but the failure rate did not move (9 of 12 against 4 of 8
       without it), and the drop count flattered the change: a reset that goes
       ahead of queued data discards bytes the visitor still wanted. It was the
       wrong number, and the lesson is that a *count* of discarded work is not a
       measure of a fix.
     - **Tried and reverted: `MIN_RTO` 200 ms → 20 ms.** No change (2 of 12 against
       3 of 12), so the retransmission timer's floor is not what makes the cell
       fail.
     - **Tried and reverted: nothing else yet.** The remaining suspects are
       ordering and queueing at teardown — the control stream's results share a
       lane with eight bulk streams — and the instrument for that is a trace of the
       control stream's own delivery, which does not exist.
     So item (2) stays open, with its rates and its two shapes on the record and
     three candidate mechanisms excluded by measurement. The next attempt should
     instrument the *visitor's* connection — time from "the carrier handed the
     last byte to the forwarder" to "the visitor read it" — because every failure
     so far is the visitor giving up, and nothing in the daemon's own logs says
     why.

   - **The interruptions were OOM, not load, and the cgroup is 13 GiB
     (2026-10-30).** Every "the run died on its own" in this file has a second
     explanation, and it is not the host's load average:

     | reading | value |
     |---|---|
     | `memory.max` | **13 GiB** (not unlimited — an earlier entry in this file wrongly read it as `max`) |
     | `memory.peak` | **13.03 GiB**, i.e. the limit was reached |
     | `memory.events` | `oom_kill 20`, `max 301 233` |

     Twenty kills in this container's lifetime. Two things were feeding it:
     `target/debug/incremental` had grown to **74 GB** across many ~900 MB
     sessions (every feature-set permutation of `cargo clippy` adds one, and
     cargo *loads* them), and **the harness builds its own crates in the same
     cgroup** — `codex_thread_store`, `codex_core_plugins` and friends were
     compiling at 660–680 MB of RSS per `rustc` while a bench run was in flight.
     So "the host spiked" and "my run vanished" have been the same event all
     along, and the process that vanished was often the one holding the bigger
     share.
     - **What to do about it:** the incremental cache was deleted (74 GB); keep
       it from coming back by running builds with `CARGO_INCREMENTAL=0`, prefer
       one `--release` build over many debug-feature permutations, and check
       `ps -eo rss,comm --sort=-rss` before starting a long measurement — if a
       `rustc` for `thread-store`/`core-plugins` is running, that is not this
       repository's build and it is competing for the same 13 GiB.
     - **A symbol-bearing build is a memory event.** Building with
       `CARGO_PROFILE_RELEASE_DEBUG=1` to profile raised `oom_kill` from 21 to
       **23** in one round (a 36 MB binary, and `-C debuginfo` is expensive to
       compile). Profiling is worth it, but not alongside another build, and the
       counter should be read before and after.
     - **Verified after the cleanup (2026-10-30):** four full `just check` runs
       and a dozen bench runs left `oom_kill` at **20** — no new kills — with
       `target/debug` growing by about a gigabyte instead of accumulating another
       incremental session. It then went to **21** during a round that rebuilt
       both `MIN_RTO` binaries and ran a six-round A/B at a fifteen-minute load of
       13, with the harness compiling its own crates alongside: the cleanup
       reduces the pressure, it does not remove the other tenant. Watch the
       counter per run, not once.
     - **It also re-reads part of this file's history:** the `MIN_RTO` attempts
       that "died to host load" (47.9/97.5, 29–104, 3/32/77) were at least partly
       OOM kills of the run's own daemons, which is consistent with cells that
       took 1 561 s and 3 748 s before giving up.

   - **A `sudo` bench run leaves root-owned files behind, and two of them
     break the session (2026-10-30).** `just bench` and `just l3-accept` run
     under `sudo`, and two things they touch every time end up owned by root:
     `.git/index`, which the bench's own `git status` refreshes — fixed with
     `--no-optional-locks` — and files inside
     `~/.cache/molehill-bench/venv`, which `uv` writes when it re-syncs. The
     first makes every later `git` command fail with
     `.git/index: Permission denied`; the second makes `just check` fail in
     `bench-selfcheck` with `Permission denied` on `uv_cache.json`. Both are
     repaired with one `sudo chown -R dsh:dsh` over the path named in the error,
     and neither is a repository fault.

   - **Both carriers now pass that cell, and the price is visible in the same
     numbers (2026-10-30).** The TCP twin of `bulk-n` at `loss1`, three rounds
     with its own A/A twin, on the same host twenty minutes later:

     | carrier | `throughput_gbps` | same build again | `cpu_s_per_gbit` |
     |---|---|---|---|
     | `mhd` | 9.47 [9.02..10.41] | 9.91 [5.57..10.58] | 0.6431 / 0.6542 |
     | `tcp` | **16.14** [11.59..16.62] | 15.89 [8.38..16.21] | **0.4666 / 0.4738** |
     | both | 8 of 8 and 14 of 14 samples passed | | |

     So the cell's failure is gone for both carriers and what is left is a
     *rate*: **mhd carries 60 % of what TCP does under one percent loss**
     (9.5–9.9 against 15.9–16.1 Gbit/s), and costs **1.37×** per byte
     (0.643–0.654 against 0.467–0.474). Both A/A pairs agree to within 1.7 %, so
     the ratio is far outside either run's scatter even though the two runs are
     not one campaign — state it as a cross-run comparison, which is what it is.
     That 1.37× is the same gap item (1) has been chasing (1.34× committed), now
     measured at the condition that used to fail outright.
   - **The other `loss1` cells are clean too   - **The other `loss1` cells are clean too (2026-10-30, load 5.5).**
     `bulk-1`, `bulk-pair`, `rr-1` and `rr-16` at `loss1`, two rounds each with
     the A/A twin: **24 of 24 samples passed**, both arms 12 of 12.

     | cell | `throughput_gbps` (arm) | same build again | `cpu_s_per_gbit` |
     |---|---|---|---|
     | `bulk-1` | 3.97 [3.92..4.02] | 3.67 [3.65..3.69] | 0.584 / 0.570 |
     | `bulk-pair` | 7.14 [7.09..7.19] | 6.92 [6.90..6.94] | 0.617 / 0.619 |

     So the whole `loss1` condition is measurable again, on a host at load 5.5 —
     which is the other half of what this item asked for ("re-measure the loss1
     cells"), and the first time it has been true in this file's history.
   - **Item (2) is closed: the cell passes every sample   - **Item (2) is closed: the cell passes every sample (2026-10-30, load
     2.6 / 4.0 / 4.9).** `bulk-n` at `loss1`, six rounds, both builds fresh:

     | arm | result |
     |---|---|
     | `l4` | **7 / 7** |
     | `l4~aa` | **7 / 7** |

     **14 of 14**, with no failure of either shape. Two fixes got there and both
     matter:
     - **The product fix** was the window leak (below): the result-exchange shape
       went from 1 of 7 to 6 of 7 in its own A/B.
     - **The measurement fix** was the readiness gap: the run waited on the arm's
       declared endpoint — the **echo** port — while an arm registers **five**
       services in sequence, and a scenario that dials the *iperf* port could
       start before that listener joined. The visitor then got a reset in **20 ms**
       from a tool a hair from ready, with the backend listening and never
       accepting (its own log says `Server listening` and nothing else). The
       workload now waits on the port it is about to dial, which is only safe
       because the sink serves its whole lifetime instead of one test — with
       `-1`, a single readiness connection made every sample fail `control socket
       has closed unexpectedly` (0 of 14, measured the round before).
   - **Item (2): the cell now passes 6 of 7, and the fix was a window leak   - **Item (2): the cell now passes 6 of 7, and the fix was a window leak
     (2026-10-30).** Three earlier runs of this cell — 3 of 14, then 1 of 14 —
     were **all taken on a stale binary**: the results files say
     `provenance.binaries[0].stale: true`, because the binary in
     `target/release/` was from `1fbbd63` while HEAD had moved four commits on.
     The model recorded the field and it says so, which is the §10 rule doing
     its job; the paragraphs below stay as **context**, not evidence, and the
     numbers that count are the ones from the fresh build.
     - **Measured, with the run's own control.** `bulk-n` at `loss1`, six rounds
       per arm, `--aa` beside an A/B of two builds, host load 1.6 (one minute),
       both binaries `stale: false`:

       | arm | build | passed |
       |---|---|---|
       | `l4` | `2c1d83a`, before the fix | **1 / 7** (5 × no results, 2 × no connect) |
       | `l4~aa` | the same build again | **1 / 7** (5 × no results, 1 × no connect) |
       | `l4~b` | `d8eff14`, with the fix | **6 / 7** (1 × no connect) |

       The twin agrees with its own arm sample for sample, so the arms are not
       drifting, and the *result exchange* shape — the dominant one — is gone
       from the fixed arm entirely.
     - **The seventh sample is unexplained again, and one "harness fault"
       reading was wrong.** Its `stderr` says `Connection refused` with
       `wall_s: 0.02` and `connected: []` — an immediate reset, not the 0.3–0.4 s
       timer stall an older entry blamed, and with no connection ever made. The
       sink's log beside it was **zero bytes**, which looked like proof the
       backend never started; it is not. iperf3's stdout is block-buffered into a
       file, so a perfectly healthy sink writes nothing until its buffer fills.
       A readiness check built on that text failed **every one** of the next
       A/B's samples (`the iperf3 sink never started`, 24 of 24), which is what
       exposed the misreading. `Sink::ready` now asks only the two things it can
       know — our own process is alive, and the kernel reports the port
       listening — and the sink runs under `stdbuf -oL` so its log is usable as
       evidence the next time. Which leaves the seventh sample where it was:
       twenty milliseconds with no connection made says the visitor's SYN met no
       listener, and the probe that answers it is the *subject's* listener
       lifecycle, not the carrier's.
     - **What the defect was.** The credit a peer is granted follows
       consumption, and `Conn::read` was the only place `read` advanced. Two
       sites discarded received bytes without counting them: a frame arriving
       for a stream already retired, and `settle` removing a stream that ended
       with unread bytes. So every short-lived stream narrowed the lane by its
       unread bytes for as long as the connection lived — the window never
       reopened, and a lane carrying many short connections ends up
       `credit_blocked` with every reader up to date. Fixed by
       `release_unread`/`advance_read`. Which is also why the earlier "3 of 14"
       looked like a design limit: it was a leak that had already eaten most of
       the window by the second round.
     - **What the earlier captures still tell us, at the level of code that has
       not changed since.** The server's `mhd stalled` line fires with
       `credit_blocked=true` (21 of 34 samples in one round), `unsent_ms` 200 ms
       to 2 209 ms, the last one 19 ms after iperf3 gave up; the transfer before
       it is healthy at **10.4 Gbit/s** on 8 streams; the first four stream opens
       cost **335 ms** each (`INITIAL_RTO` plus one lost packet at 1 % loss)
       against 1.0–1.5 ms for every open after them. Those lines describe
       `conn.rs` and `pump.rs` as they still are, but the runs they came from do
       not satisfy §10, so they are leads, not measurements.
     - **Re-verified, and the remaining shape is the tool's listener, not the
       carrier (2026-10-30, load 0.57).** Six more rounds on a fresh build
       (`stale: false`): **5 of 7** on the arm and **7 of 7** on its A/A twin —
       **12 of 14**, and both failures are the `unable to connect to server`
       shape rather than the result-exchange one. With `stdbuf -oL` now on the
       sink, the artifacts for those two samples are readable and they say the
       backend was **not** the problem: its log carries `Server listening on
       2402 (test #1)` and it never accepted a connection, while the visitor's
       `meta.json` records `wall_s: 0.02` and `exit 1`. Twenty milliseconds and
       no connection made means nothing was listening on the address the arm
       dials — the *tool's* exposed port — at that instant, moments after
       `wait_target` had found it up.
     - **The readiness probe's gap, and what happened when it was closed
       (2026-10-30).** The run's readiness step probes the arm's declared
       endpoint, which is the **echo** port for every arm — so a scenario that
       measures the *iperf* port starts against a listener nothing had confirmed,
       which is a real gap in the model's own rule that an arm is ready when the
       workload's first packet would succeed.
     - **Closing it turned 12 of 14 into 0 of 14, and the reason is worth
       keeping.** Probing the iperf port with an ordinary connect made every
       sample fail with `iperf3 error: control socket has closed unexpectedly`:
       the sink runs as `iperf3 -s -1`, which serves **exactly one test**, and the
       probe *is* a test — it consumes the sink and the real client then finds
       nothing. Reverted. If the gap is closed later it must be closed by making
       the sink multi-test for its lifetime (it is already killed per sample) or
       by a check that does not complete a connection.
     - **And the failed experiment answers the question anyway.** The probe's
       connection was accepted and forwarded to the sink — that is what consumed
       it — so the **tool's iperf listener is up whenever a sample runs**. The
       twenty-millisecond `Connection refused` is therefore *not* a missing
       listener on the tool's side: it is a refusal delivered back through the
       tunnel, so the backend refused, or the tool closed the forwarded
       connection.
     - **The single-test sink is not it either (2026-10-30, load 0.5).** Removing
       `-1` from the sink so a stray connect could not consume it read
       **6 of 7** on the arm and **7 of 7** on its A/A twin — **13 of 14**, with
       the same one `unable to connect` shape. That is within noise of the
       12 of 14 with `-1`, so the flag was reverted: it is a documented safety
       property (a sink that exits on its own cannot linger across samples) and
       nothing measured here argues against it.
     - **What is left for that last sample:** the refusal arrives through the
       tunnel in twenty milliseconds from a live tool listener to a live backend
       listener, so the forward attempt itself is where to look — the server's
       data-channel pairing for a visitor it has just accepted.

   - **The width change does not touch the transparent path.** `just l3-accept`
     (root, three namespaces, sixteen checks) reads the same at four workers as at
     twenty: thirteen pass, and the two that fail are the known sixteen-flow
     lending pair (`the busy claim lost traffic while a lane was lent`,
     `lending-off either moved a lane or lost the workload`) — the same two, in the
     same order, on both widths. The other thirteen include the 16-flow
     byte-exactness check, so the change is not hiding behind a green summary.
   - **The phase table at the new default, and why item (1) ends here
     (2026-10-29).** One instrumented cell at four workers
     (`target/evidence/phases-w4.json`, `phases-w4b.json`), 246.7 Gbit carried,
     158 CPU-seconds in total:

     | phase | seconds | s/Gbit | share of all CPU |
     |---|---|---|---|
     | record layer `seal` (sender) | 21.58 | 0.0875 | 13.7 % |
     | record layer `open` (receiver) | 20.85 | 0.0845 | 13.2 % |
     | `tx_build` (sender) | 17.53 | 0.0711 | 11.1 % |
     | `rx_total` (receiver) | 13.47 | 0.0546 | 8.5 % |
     | `tx_send` (sender) | 10.35 | 0.0420 | 6.6 % |
     | `io_write` / `tx_send` (acks) / `io_read` | 10.35 | 0.0421 | 6.6 % |
     | **measured total** | **95.10** | **0.3855** | **60.2 %** |

     The record layer is the largest single line and it is **cheaper than TCP's**
     (0.172 against 0.264 s/Gbit, because 31.8 KB records amortise better than
     10.8 KB ones). What is left is 0.214 s/Gbit of carrier work spread over five
     phases of 0.012–0.071 each.
     Sampled per packet, `tx_build`'s 0.91 µs turn is stream selection 0.054,
     framing and copy 0.342, flight table 0.164, and **0.352 for the connection
     lock plus `poll_transmit`'s preamble** — and no single one of those is worth
     more than ~3 % of the cell. Batching the lock over eight packets would save
     ~0.22 µs of the 0.91 and land at 2.7–3.2 %; the clock read is 3 %; the two
     payload copies are load-bearing (the send buffer must hold the bytes for ARQ,
     and GSO needs one contiguous run, so neither copy can go). **Every remaining
     item is under the 4.35 % floor this run can resolve**, and the one change
     big enough to clear it is architectural — one task owning a connection's
     socket and its stream buffers, so the payload is never read back across a
     core — which is a redesign, not an optimisation of this one.
     So item (1) ends here, with the numbers that say so, and the work moves to
     the failure that costs more than 3 %: an eight-stream bulk cell that mhd
     cannot carry under one percent loss.
   - **One diagnostic kept**: a lane that decides its peer is gone now logs the
     silence age at `DEBUG` (`id`, `silent_ms`). It exists because a *client-side*
     pool reaping its own lanes is what turns an intermittent 5 s stall into a
     half-empty pool (`reaped dead tunnels reason="dead" from=4 to=2`, once in ten
     cells), and the age is the evidence that separates "the peer vanished" from
     "we never heard it at all". Twelve server-side reaps of `silent_ms=20009`
     were observed in four clean cells — retired lanes ageing out, harmless — and
     no client-side one reproduced on demand since.
     **What that says about the gate**: the difference between this carrier and a
     mature one is not a missing mechanism, it is where the work happens — the
     kernel's socket layer amortises a TCP stream's per-byte cost over a
     `sk_buff` it owns, and a userspace carrier pays per datagram for the same
     bytes. Quoting a reference implementation cannot change that; it can only
     shave the copies, which is what the item above is.
   - **The decision this leaves for the human** (the plan names it): ship
     `mhd` as a loss-tail-only carrier with an honest "costs ~1.5× TCP on a
     clean path, wins on lossy multi-flow tails" claim, or stop the carrier
     and delete it. Nothing in this branch is released, so both remain open.
     The correction above changes what the honest claim would have to say — the
     gap is ours to close, not the path's to forbid — but it does not change the
     fact that today's numbers are 1.41× TCP.
   - The local single-host proxy that guided the work (one forwarded TCP
     stream through an iperf3 pair, `cpu-split.sh` outside the tree) reads
     mhd 5.98 Gbit/s at 0.549 s/Gbit against TCP 4.91 at 0.462 — the same
     ordering, a smaller gap. The bench's namespaced topology amplifies the
     per-datagram cost, whichever half of the path it is charged to.
   - **Item (3): the stalled flows stall at their first reply (2026-10-29).**
     Three more instruments, each answering one question:

     | question | instrument | answer |
     |---|---|---|
     | does the kernel drop these packets? | per-interface `rx_drop`/`tx_drop`/errs in all three namespaces, printed after the arm | **one packet** across the whole arm (`l3srv0 tx_drop=1`) |
     | where does a stalled flow stop? | the visitor now reports its progress (`timed out after k/200 round trips`) | **`0/200`**: it connected, read the banner, and the first reply never came |
     | is the peer's credit what blocks it? | `credit_blocked` on the `mhd stalled` line | **false** in every sample; the only wait is 200–232 ms of flight = `MIN_RTO` |

     So a flow's request goes out, its reply never arrives, and nothing in either
     daemon nor in the kernel accounts for it. The cliff is sharp and reproducible:

     | lanes for the busy claim | flows per lane | visitor |
     |---|---|---|
     | 4 | 4 | 16/16 in 0.09–1.9 s |
     | 2 | 8 | 5/16, 13/16, 15/16 — the rest stall 6 s |

     and the same run can have both arms, so it is a property of the lane's flow
     count, not of the run.
   - **What that re-opens: the "the carrier is unstable" reading.** Earlier in this
     log the mhd arm's own A/A spread (13 % in one run, 21 % across another's
     rounds) was read as the carrier being unstable, against TCP's 0.97 %. With
     the host swinging between 1 and 87, part of that is the host drifting between
     the arms of a run, and the *interleaving* the model uses (`--aa` twins, and
     the A/B round order) is the only thing that separates the two. The claim is
     therefore **doubtful, not established**: what survives is that the runtime
     width change took the same arm's floor from 13 % to 4.35 % on a quiet host,
     which is a real improvement; what does not survive is the inference that the
     carrier itself was the unstable part.
   - **The measurement host is shared, and `/proc/loadavg` is its, not ours
     (2026-10-29).** The container is not isolated: the load average it reads is
     the *host's*, and the host swung between **1 and 87** during one afternoon
     (28 → 15 → 7 → 3.6 → 44 inside twenty minutes). Every bench run taken in a
     peak failed, and not subtly: `ip netns exec` alone outran the harness's own
     five-second preflight, and `pool_test`'s timing test failed in three of four
     `just check` runs and passed 11/11 plus 1/1 standalone on the same tree.
     **So a run must record the load it was taken at, and a run above a few is not
     evidence** — the model's `--aa` floor is the mechanism that was supposed to
     catch this, and it only catches it when the drift is inside the run rather
     than over the whole of it.
   - **What the next calm window measured, and why the floor is still 200 ms
     (2026-10-29).** One clean sample got through before the host spiked again:
     `cpu_s_per_gbit` **0.6723** at 8.97 Gbit/s with `MIN_RTO` at 20 ms, against
     the committed baseline's median **0.6494** (0.6393–0.6579). So the change that
     buys the transparent path **4.4×** of latency costs something on the clean
     bulk path — a retransmission for every ack that arrives later than 20 ms — and
     one sample cannot say how much. Reverted again, and the trade is now written
     where the decision is: `recovery.rs` carries both numbers.
     Which is also why the bench now records the **host load average** in every
     run's `meta` (`bench/src/bench/hostinfo.py`, `instruments.py`, and a sentence
     on both benchmarks pages): three cost cells in a row were taken at loads of
     44, 57 and 49 and none of them was evidence, and a reader of a results file
     has no way to know that today.
   - **The benchmarks pages describe the carrier that exists now
     (2026-10-30).** Both pages still carried a ~275-line section measuring the
     **KCP** carrier this release deletes, plus the switch value
     `MOLEHILL_DATA_CARRIER=kcp` — a value the code now warns about and ignores —
     and the configuration row `[client.data.tcp|kcp].tunnels`. All of it is
     replaced: the carrier axis is stated as a *path condition* (a clean path
     carries mhd, `--condition udp_blocked` carries the fallback), the two
     diagnostic switches are named with what they do, and the KCP-era figures are
     pointed at `benches/records/` as published history with the reason they are
     not a baseline. Both languages, same 13 sections each, and the docs gate is
     green.
     - **What remains of that debt:** the *other* tables on those pages still
       quote figures from the model this one replaced (the per-decision table's
       throughput and RSS numbers, for instance). They are not KCP claims — they
       are old-method numbers — and they need a sweep measured with the current
       model, which is a release-ritual job rather than a correction.
   - **Which copy can actually go, and why it is not a phase fix (2026-10-30).**
     Walking the four copy sites against what each layer can see:

     | copy | site | eliminable? |
     |---|---|---|
     | caller → send buffer | `io_write` | **no** — the caller's slice is only valid during `poll_write` |
     | send buffer → GSO run | `tx_encode` | **no** — `UDP_SEGMENT` needs the segments contiguous |
     | batch → receive buffer | `append` | **yes**, if the batch is owned per read |
     | receive buffer → caller | `io_read` | **no** — `poll_read` fills the caller's slice |

     So exactly one copy can go, it is worth ~4.4 % of total CPU, and the *ratio*
     would move from 1.39 to about 1.33 — a real narrowing that still does not
     reach the objective's `≤ TCP`.
     - **What it costs to get it:** the datagram contract becomes owned rather
       than borrowed — `RecvBatch::recv` allocating a `BytesMut` per read and
       freezing it, the `on_datagram` closures (`FnMut(SocketAddr, &[u8])`, four
       call sites), `Conn::on_datagram`, `apply_datagram` and
       `RecvBuf::insert`/`pieces` all moving to `Bytes`, plus the GRO split
       yielding slices of the run. That is the reassembly path, the most
       correctness-sensitive code in the carrier, and it is four files of
       signature changes rather than one phase.
     - **And even that one is not profitably removable, on arithmetic.** An owned
       datagram needs a buffer that outlives the read, so the region becomes
       per-read instead of reused. The safe way to get one is `vec![0u8; n]` —
       `recvmsg` needs *initialised* memory (`IoSliceMut` cannot point at
       uninitialised bytes, and the safe uninitialised route, `BorrowedCursor`,
       only serves `Read::read_buf`, not `recvmsg`). Zeroing a whole GRO run is
       ~55 KB per read, i.e. **~1.4 KB of memset per datagram, ≈0.14 µs** at
       memory bandwidth — against the **~0.3 µs** the copy costs. The change buys
       about half of what it was supposed to, which lands *below* the 4.35 %
       floor. Escaping the memset means new `unsafe` in a module that does not
       have it, and the lint policy wants a waiver argued for, not added late.
     - **Which is why it was not attempted.** Against a 100-commit branch that must
       stay shippable, a subtle reassembly regression costs far more than 4.4 %,
       and the prize does not reach the criterion anyway. **Item (1) therefore
       ends this objective unfinished and fully characterised**: the gap is
       user-space (kernel already 1.04×), the AEAD is optimal, the copies are the
       only removable item and only one of them is removable, and the change that
       would take it is a contract rewrite across the receive path.
   - **The copy is the second-largest item, and it is `rep movsb` (2026-10-30).**
     The same system-wide profile puts **5.01 % of every sample inside a single
     libc address**, and disassembling it confirms what it is:
     `162b87: f3 a4  rep movsb` — glibc's `memcpy`. With the daemon at ~25 % of
     the profile that is on the order of a fifth of its time in copying, which
     makes the copies the biggest thing after the AEAD and the only item left
     that a code change could remove.
     - The three following libc addresses (1.45 %, 1.04 %) are the same routine's
       other entry points, so the family is larger than the single symbol.
     - **Which copy, exactly, is still unattributed.** The callchains resolve to
       addresses (`perf report` cannot name libc's internals without debug
       symbols, and `-p` on the daemons sampled almost only kernel addresses), so
       "5 % is mhd's own copies" is *not* established — some of it belongs to
       iperf3 and the python probes in the same profile. The four sites that could
       be it are already metered: `append` (batch into the receive buffer),
       `io_read` (receive buffer out to the caller), `io_write` (caller into the
       send buffer) and `tx_encode` (send buffer into the GSO run).
     - **The arithmetic that makes this actionable:** removing one of the two
       copies on the receive side is worth ~0.3 µs of a 5.2 µs per-datagram
       user-space budget, i.e. ~6 % of user time or ~4.4 % of total CPU against a
       4.35 % floor. It is *at* the boundary, not comfortably past it — which is
       the honest reason it was not attempted with the rounds left, and the reason
       the next attempt should remove two copies at once (receive *and* send) so
       the effect is unambiguous.
   - **The ratio at HEAD, measured with its floors (2026-10-30).** Six rounds per
     arm with the A/A twin, one cell per carrier:

     | carrier | `cpu_s_per_gbit` | twin | A/A diff | floor | host load |
     |---|---|---|---|---|---|
     | mhd | 0.6570 [0.6348..0.7062] | 0.6470 | **0.16 %** | 7.34 % | 2.3 |
     | tcp | 0.4568 [0.4376..0.5096] | 0.4815 | 5.59 % | 11.59 % | 6.7 |

     So mhd 0.652 against TCP 0.469 is **1.39×**, and the floors are 7.3 % and
     11.6 % — the host was not quiet enough for better, so the *magnitude* of the
     gap is only good to about ±0.18. It is consistent with the committed 1.34×
     and with the 1.37× measured under `loss1`, and the ratio has not moved: the
     credit-leak fix changed whether the cell passes, not what a byte costs.
   - **A profiler exists after all, and the profile confirms the shape   - **A profiler exists after all, and the profile confirms the shape
     (2026-10-30).** `perf` was not installed, but `apt-get install linux-perf`
     works and the container has `CAP_SYS_ADMIN` (which `perfmon_capable()`
     accepts even at `perf_event_paranoid=2`). Kernel symbols stay restricted, so
     a profile of the *release* binary shows only addresses — build one with
     `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1 cargo build
     --release` and pass it with `--binary`, and user symbols resolve at full
     optimisation. This is the method the item-(1) work was missing.

     System-wide sampling at 1 999 Hz for eight seconds of a `cost` cell
     (`/tmp/sym.data`, 174 804 samples), restricted to the daemon's binary:

     | symbol | share of all samples |
     |---|---|
     | `ring_core_…_chacha20_poly1305_seal_avx2` | **8.08 %** |
     | `ring_core_…_chacha20_poly1305_open_avx2` | **7.87 %** |
     | `mhd::conn::Conn::on_datagram` | 1.38 % |
     | `mhd::pump::flush` (closure) | 1.06 % |

     **Nothing else reaches one percent.** The two AEAD symbols are ~16 % of the
     whole system's cycles while the daemon is ~25 % of it, so the record layer is
     ~26 % of the daemon's own time — the same share the committed budget gives it
     — and the remaining three quarters are spread across hundreds of sub-1 %
     sites in the per-packet path.
     - **What that means for item (1):** there is **no hotspot to fix**. The one
       large item is the AEAD, and it is already the fastest implementation
       available (AVX2, from `ring`), so it is not a lever. What is left is
       diffuse per-byte work — copies, framing, locks — which is exactly what an
       architectural reduction in copies would remove, and exactly why no bounded
       change has moved the ratio: this file has now measured and reverted the two
       candidates (lock consolidation, and the syscall half) and the profile
       explains why both were flat.
     - **Honest bound on the claim:** the percentages are of the *whole system*
       profile, not of the daemon alone, because `perf report --comms` does not
       re-base them. The daemon's share (25 %) and the phase table's independent
       26 % agree, which is why the ratio is stated as ~26 % rather than read off
       the profile directly.
   - **Item (1)'s gap is *all* user space, and that rules out a whole class of   - **Item (1)'s gap is *all* user space, and that rules out a whole class of
     fixes (2026-10-30).** `cpu_s_per_gbit` is `utime + stime`, and the sum cannot
     say which half to attack — so the instruments now record the split
     (`cpu_user_s` / `cpu_sys_s` in a sample's evidence, from
     `/proc/<pid>/stat`). One `cost` cell per carrier, same host, back to back:

     | carrier | role | user | kernel | kernel share | `cpu_s_per_gbit` |
     |---|---|---|---|---|---|
     | mhd | client | 67.37 s | 28.44 s | 29.7 % | 0.6541 |
     | mhd | server | 80.00 s | 32.94 s | 29.2 % | |
     | tcp | client | 79.79 s | 42.31 s | 34.7 % | 0.4666 |
     | tcp | server | 65.05 s | 51.79 s | 44.3 % | |

     Normalised by the bits each carrier moved in its own cell — mhd 319 Gbit,
     TCP 512 — the two halves separate cleanly:

     | half | mhd | tcp | ratio |
     |---|---|---|---|
     | **kernel** | **0.192 s/Gbit** | **0.184 s/Gbit** | **1.04×** |
     | user | 0.462 s/Gbit | 0.283 s/Gbit | **1.63×** |

     **mhd's kernel cost per bit is already the same as TCP's.** The entire 1.37×
     is user-space work: the framing, the copies into and out of the stream
     buffers, and the lock traffic around them.
     - **What that rules out:** fewer or bigger syscalls — larger `SEND_RUN`,
       `sendmmsg`, fewer boundaries crossed. The kernel half is equal, so no
       amount of syscall batching moves the ratio. That was the cheapest
       remaining hypothesis and it is now dead by measurement.
     - **What it leaves:** the per-byte user-space work, which is the
       architectural direction the budget has pointed at all along — one owner of
       the socket and the stream buffers, so a byte is copied once instead of two
       or three times. The new finding is that this is *quantified*: **0.18 s/Gbit
       of user-space work to remove**, and the floor it must clear is 4.35 % of
       0.64, i.e. 0.028 s/Gbit. Removing a third of one copy would do it.
     - **Caveat stated:** one round per carrier, two separate runs, host at load
       5–10. The absolute cost is therefore context, but the *ratio between the
       halves* is what the claim rests on, and a 1.04× against a 1.63× is not a
       noise artefact of that size.
   - **A correction first: the "24 µs receiver" number was a mis-assigned role   - **A correction first: the "24 µs receiver" number was a mis-assigned role
     (2026-10-30).** Reading the same instrumented cell by *datagrams per run*
     rather than per run settles it, and the two sides swap places:

     | process | datagrams / run | `tx_build` per run | **per datagram** |
     |---|---|---|---|
     | client (sends acks) | 1.0 | 0.97 µs | 0.99 µs |
     | server (posts bulk) | 38.9 | 23.99 µs | **0.617 µs** |

     So the 24 µs is a run of **39 datagrams**, not one, and there is no anomaly:
     the sender spends 0.617 µs per datagram framing and copying a 1400-byte
     packet into its GSO run, which is the per-packet turn the committed budget
     already carries as `tx_build` 0.0711 s/Gbit. The entry that called this
     "backwards, 8.9 % of CPU, bigger than any phase in the budget" was reading a
     per-run cost as a per-datagram one; deleted.
     - **What the cell *does* say at HEAD**, now with the right denominators: the
       record layer is ~22 s per side of a 158 CPU-second run (**26.9 %**, the same
       share the budget was built on), `tx_build` is 0.62 µs per datagram on the
       sender, and `tx_send` is 21.7 µs per run of 39 datagrams — 0.56 µs of
       syscall per datagram. Those three are 0.26 of the 0.64 s/Gbit, and none of
       them is a surprise; the split has not moved.
     - **The lesson worth keeping:** a phase counter divided by its own event count
       is only interpretable once you know what an event *is*, and this file now
       has three phases whose events are runs, one whose event is a write and
       several whose event is a datagram. Dividing the wrong one produced a
       40-fold error and a confident, wrong diagnosis.

     `poll_transmit` is not the answer on inspection — `AckSet::packet` copies
     three fields, `next_stream_range` walks a two-entry map, and the credit frame
     is eight bytes. What is left is **waiting**: the pump takes the connection's
     mutex once per run, and the reader takes it once per datagram to copy up to
     64 KiB out of the buffer. A run that queues behind a reader's copy is charged
     the whole wait, and the average is dragged up by a minority of such runs —
     the same contention the one-lock-per-run experiment would have *worsened*,
     which is one reading of why that measured +0.35 % rather than a win.

     **The measurement that would settle it**: charge the mutex wait separately —
     time the acquisition, not the body, and report it as its own phase. If the
     wait is the 24 µs, the fix is on the *reader* (copy outside the lock, or hand
     the bytes over by ownership instead of by copy) and it is a real item-(1)
     lever with a denominator this file has not yet used. If the wait is small,
     the number is somewhere in the ack path and needs a profile. Not attempted at
     this host's load (15–23 during the round), which is why it is written down
     rather than measured.
   - **The phase split's shape is unchanged at HEAD (2026-10-30).** One
     instrumented `cost` cell (`target/evidence/phases-r25.*`, `work-r25/`) was
     taken with `MOLEHILL_MHD_STATS=1` to check that the split the item-(1)
     budget was built on still holds after the credit-leak fix and the traces.
     The raw lines are per **event**, and an event is not a datagram for every
     phase — a `seal` is a whole GSO run of ~23 datagrams, a `decode` is one
     datagram — so the only honest reading without per-phase byte denominators
     is the shape:

     | side | phase | µs per event | events |
     |---|---|---|---|
     | receiver | `open` | 22.6 | 976 012 |
     | sender | `seal` | 23.4 | 976 319 |
     | sender | `tx_send` | 21.7 | 524 488 |
     | receiver | `tx_build` | 24.0 | 583 562 |

     Both record-layer phases are ~22 s of a run whose CPU total was ~158 s, so
     the **26.9 % share the budget was built on still holds** — the split has not
     moved. Turning these lines back into `s/Gbit` needs the bytes each phase
     covers, which is the earlier table's method and not something to guess at;
     the committed table (0.3855 s/Gbit measured of 0.6406, phases in the item-1
     section) remains the reference.
   - **The per-datagram lock is not a lever, measured and reverted
     (2026-10-30).** The phase table charges 0.352 µs of the 0.91 µs per-packet
     turn to "lock + preamble", and the send path took the connection's mutex
     **once per datagram** — up to 44 times per run. Building the whole run under
     one guard was the obvious way to attack the largest item, so it was built
     and A/B'd against its own parent with the A/A twin beside it, six rounds per
     arm:

     | arm | `cpu_s_per_gbit` |
     |---|---|
     | `l4`, lock per datagram (parent) | 0.6328 [0.6198..0.7172] |
     | `l4~aa`, the same build again | 0.6482 [0.6244..0.7516] |
     | `l4~b`, one lock per run | 0.6350 [0.6225..0.6472] |

     **+0.35 % against a 9.81 % floor** — inside the noise, and slightly the
     wrong side of it. The host was at load 13 (fifteen-minute), which is why the
     floor is that wide, but the discipline is the same at any floor: a change
     with no measured gain is not kept. Reverted. What survives is a negative
     result worth having: either mutex acquisition is not what the 0.352 µs
     measures, or the effect is below what this host can resolve, and the
     architectural rewrite is still the only lever big enough to be seen.
   - **The `MIN_RTO` trade is settled: 20 ms does not cost more, it fails   - **The `MIN_RTO` trade is settled: 20 ms does not cost more, it fails
     (2026-10-30).** Two builds differing only in that constant, six rounds per
     arm with the A/A twin beside them, on a host at load ~1.5:

     | arm | build | result |
     |---|---|---|
     | `l4` | `MIN_RTO` 200 ms | **5 of 6** cells, 0.6779 [0.6276..0.7018] |
     | `l4~aa` | the same build again | **5 of 6**, 0.6640 [0.6493..0.6724] |
     | `l4~b` | `MIN_RTO` 20 ms | **0 of 6** — every one `control socket has closed unexpectedly` |

     The A/A floor came down to **1.74 %** with six rounds (it was 5.07 % with
     two), which is below the 4.35 % materiality floor — so the instrument can
     now resolve this cell, and the answer is not a cost difference but a
     categorical one: at 20 ms the connection dies, every time.
   - **And the dead-link rule was the first suspect, correctly (2026-10-30).**
     `DEAD_LINK` counts *retransmissions*, and the count only measures silence
     through the timer: at the default floor ten backed-off retries span minutes,
     at 20 ms they span a fraction of a second, so a busy host's ordinary
     scheduling delay was being read as a dead lane. `DEAD_FLOOR` now requires
     five seconds of silence as well — a no-op at the current setting, and a
     genuine correctness improvement to the rule.
   - **But it does not rescue 20 ms.** A second A/B with the floor in place had
     the 20 ms build fail every sample again (`14.74 s`, `17.70 s`, `16.60 s`,
     `21.01 s` — each dying early) before the host reached a load of 8.3 and the
     run was stopped as unusable, that time with the *200 ms* arm failing a cell
     too. So the dead-link rule is not what makes 20 ms fail; something else
     about a timer shorter than the path's own acknowledgement spacing is, most
     likely the spurious-retransmission pressure the constant's comment always
     warned about. That is the next thing to measure, and the decision stands.
   - **The `MIN_RTO` trade ran, and this run's own floor is wider than the   - **The `MIN_RTO` trade ran, and this run's own floor is wider than the
     effect it was asked to resolve (2026-10-30).** Two release builds of
     `9ee4666` — `target/evidence/molehill-rto200` and `molehill-rto20`, the only
     difference being `MIN_RTO` — were compared as an interleaved A/B with the
     A/A twin beside them, on the calmest window of the day (load
     **0.67 / 1.07 / 13.13**):

     | arm | build | `cpu_s_per_gbit` |
     |---|---|---|
     | `l4` | `MIN_RTO` 200 ms | **0.6712** [0.6568..0.6856] |
     | `l4~aa` | the same build again | **0.6388** [0.6365..0.6411] |
     | `l4~b` | `MIN_RTO` 20 ms | **0.6662** [one sample] |

     **The A/A floor is 5.07 %**, against a materiality floor of 4.35 % and the
     +3.5 % one earlier sample hinted at. So the run cannot resolve the trade in
     either direction — but it does say that the 20 ms build's point estimate is
     *below* the 200 ms build's, which is not what the earlier single sample
     suggested, and it removes that sample as a reason to keep the floor where
     it is. What it would take is stated by the model itself: most of this
     cell's metrics read "too noisy for rounds to fix — longer workloads, not
     more of them", so the next attempt should lengthen the workload rather than
     add rounds, on a host whose one-, five- and fifteen-minute loads are all
     under two.
     - **Both binaries are flagged `stale: true`, and the flag is wrong here.**
       `src/main.rs` has an mtime of 08:21 with no content change — `git status`
       is clean and `git diff` on it is empty — so the model's mtime comparison
       fired on a touch. Both binaries report `9ee4666` and `src/` has not
       changed since that commit (the three commits after it are `bench/` and
       this file). Verified rather than assumed, because the round before this
       one shipped a genuinely stale binary.
     - **Three of four attempts never reached a table.** They died in warm-up or
       round 0, each time on the harness's own five-second `ip netns exec`
       preflight or a cell that took 1 561 s, at loads of 47.9 / 97.5,
       29–104 and 3 / 32 / 77. Only the fourth, cut to two rounds, finished.
     - **A signal too small to claim:** `iperf3 error: control socket has closed
       unexpectedly` appeared **0 of 6** times on the 200 ms build, **1 of 6** on
       its twin and **2 of 6** on the 20 ms build. Same class as item (2)'s
       result-exchange failure; three samples out of eighteen is not evidence of
       anything, and it is recorded so the next run counts it deliberately.
   - **One measurement survived it, and it is a real lead on item (3).** On a calm
     window (load 1.4) the acceptance harness's sixteen-flow arm — one claim, four
     lanes — ran **16/16 in 1.517 s** with `MIN_RTO` at 200 ms and **16/16 in 0.344 s**
     with it at 20 ms, in the same topology, minutes apart. A strict
     request/response flow loses a packet every few hundred round trips and each
     loss then costs the *floor*, so the floor dominates that workload's latency by
     more than four to one. The floor is still 200 ms: lowering it buys that
     latency with a retransmission rate on the bulk paths that could not be
     measured, because the cost cell taken for it came back at a host load of 44.
     That cell, at a calm host, is the first thing the next window should run.
   - **A flake that blocks the gate, with its own numbers.** `pool_test`'s
     `a_saturated_pool_still_serves_the_next_visitor` failed **three of the last
     four** `just check` runs and passed **11/11 plus 1/1 standalone** on the same
     tree, with the stall instrument on. Its scenario — saturate one lane with
     `ENGINE_STREAM_CAP + 8` visitors, drop them, then demand one more — is the
     same shape as the two defects above, and the instrument shows a lost packet
     costing 200 ms and, with backoff, up to **714 ms**. So the flake and the L3
     cliff are probably one defect wearing three coats: something that makes a
     *new* stream's first bytes on a busy-then-quiet lane take far longer than the
     timer suggests.

   - **Item (3) re-measured in a calm window, and its attribution was wrong
     (2026-10-30, load 0.3–1.5).** Three `just l3-accept` runs back to back read
     **14/15**, **13/15**, **13/15**, and *which* checks failed moved between them.
     That alone kills the earlier reading of a fixed pair of lending checks:
     - **Lending is not the cause.** The lending-*off* control arm — same two
       claims, same budget, `lend = false`, so no lane ever moves — fails exactly
       like the lending arm (`8/16`, `6/16`, `0/200` per stalled flow), and the
       lending arm itself **passed once** (`PASS the busy claim stays byte-exact
       through the lane change`, 16/16 in 1.696 s). The check that failed in all
       three runs is the *sixteen-flow arm itself*, in whichever section reached
       it first.
     - **The cliff is a flake, not a cliff.** The same 16 flows over the same two
       lanes measured `16/16 in 0.094 s`, `14/16 in 5.010 s` and `6/16 in 6.102 s`
       in one afternoon. The old table's "4 flows/lane passes, 8 fails" was a
       host-load boundary: at load ~1 the 8-per-lane arm runs in **94 ms**, twenty
       times faster than the pass it used to record.
     - **The missing instrument now exists.** `Routes::pick` emits one `DEBUG`
       line per **newly placed** flow — claim, slot, the `flow::key` both ends
       compute, and the five-tuple — and `visitor.py` prints `FLOW <n>
       local=<ip:port>` so a visitor's flow number can be followed onto the
       tunnel. Both are per-flow, not per-packet, and cost one comparison per
       packet when off.
     - **What it answered, and what it did not.** All 16 flows are placed on
       **both** ends, 8/8 across the two slots, all inside one second, so nothing
       is misrouted or unplaced; two flows never received even the banner, and ten
       read it and then got no reply to any of their 200 round trips. Every
       counter stays at zero — `unclaimed`, `no_channel`, `overflow`,
       `queue_wait_max_ms`, and all four columns of `/proc/net/dev` in all three
       namespaces — so the loss is not at any place this code books a loss.
     - **The one stall-shaped fact.** In the client, the device-read gap and the
       carrier-write gap stop and restart at the **same millisecond** for
       **1.66 s** (`read_gap_at_ms == write_gap_at_ms`). A single whole-path
       stall, not one flow's, and the same field reads **5.0 s** in an arm that
       passed.
     - **The control says the tunnel is the unstable part, not the host.** The
       same run measures the same sixteen threads straight at the service, with
       no tool in the path. Across three runs that control read **22 774**,
       **23 829** and **22 945** round trips/s — a **4 %** spread — while the
       tunnel read **2 498**, **3 883** and **633**: a **6×** spread, and 9–36×
       slower. So "the shared host explains it" cannot carry this arm: a probe
       stable to 4 % in the same instant on the same host is exactly the contrast
       the arm exists for, and what moves is the tool. This also supersedes the
       width reading — the control was never the thing that changed.
     - **Runtime width is not the cause.** `TOKIO_WORKER_THREADS=8` on the same
       topology read **12/15** (three failures, both lending checks among them)
       against **14/15** at four workers, with the tunnel arm at **629** round
       trips/s: no better. Excluded, like the three mechanisms before it.
     - **The carrier is cleared, with both ends' counters agreeing
       (2026-10-30, load 0.8, `MOLEHILL_L3_STATS=1`).** The same arm, re-run:
       eleven of sixteen flows passed and five stalled, and the per-member
       counters say *nothing was lost between the two daemons*:

       | lane | client offered → | client injected ← | server injected → | server offered ← |
       |---|---|---|---|---|
       | 1 | 1429 | 625 | 1429 | 625 |
       | 2 | 820 | 1640 | 820 | 1640 |

       Each direction of each lane is mirrored exactly on the other end, with
       `no_channel=0`, `overflow=0`, `disconnected=0` and
       `peer_refused=0` everywhere, and `queue_wait_max_ms=0`. So the lane
       delivered every packet that was offered to it, in both directions, and the
       carrier's queues were never the constraint.
     - **What the timing says, and what it does not.** In that same run the
       client's **device read** was silent for **4 442 ms** (`read_gap_max_ms`,
       inside that arm's own window) while its carrier write never stalled
       (`write_gap_max_ms=1`), and the server's read was silent 1 882 ms. That is
       consistent with two very different stories and **does not choose between
       them**: either the visitors' packets were dropped before this end's TUN, or
       the visitors simply had nothing to send because they were waiting for
       replies. The mirrored per-lane counts above cannot distinguish them
       either. The claim counters refuse almost nothing (seventeen `unclaimed`
       on the server, everything else zero), which is one fact against wholesale
       dropping and is not decisive.
     - **The probe ran, and it closes the loss hypothesis (2026-10-30, load
       0.6).** `drop_snapshot` now brackets both lending arms and `drop_delta`
       reports what moved across them. Across the lend: **nothing**. Across the
       same load with lending off: **nothing**. No interface in any of the three
       namespaces dropped or errored a single packet, so the visitors' packets
       were not being dropped before the TUN — they were not being *sent*,
       because the flows were waiting for replies that took seconds to come.
     - **What that leaves is latency, and it is bimodal.** The one sixteen-flow
       arm that passed in that run did so at **21 332 round trips/s** through the
       tunnel against **23 658** straight at the service — a tunnel that costs
       ten percent when it works. The two that failed took **6.05 s** for the
       same work, which at 16×200 round trips is a per-flow wait of seconds.
       So the cell is not losing traffic and not short of window: it either runs
       at within ten percent of a direct path or it stalls, and nothing in
       between has been seen.
     - **The second claim was never the trigger, and a control proves it
       (2026-10-30, load 0.3).** The harness gained a fourth tunnel arm — the
       same sixteen flows, the same four lanes, **one** claim, lending off — so
       "a sibling claim starves the busy one" and "four lanes cost more than two"
       stop being the same experiment. It **failed too** (`12/16 in 6.039 s`),
       and in that run every tunnel arm failed while the direct control passed
       `16/16 in 0.099 s` at **21 327** round trips/s:

       | arm | configuration | result |
       |---|---|---|
       | `many` | 1 claim, default budget | **8/16 in 5.075 s** |
       | control | straight at the service | 16/16 in 0.099 s |
       | lending | 2 claims, 4 lanes, lend on | 11/16 in 6.061 s |
       | lending off | 2 claims, 4 lanes, no lend | 12/16 in 6.048 s |
       | **one claim** | 1 claim, 4 lanes, no lend | **12/16 in 6.039 s** |

       So the claim count, the lane count and the lending decision are all
       cleared: the sixteen-flow arm is **intermittently** slow, in every
       configuration. Reading the runs that have both shapes, the plain
       `many` arm alone has passed at 16/16 in 0.093–1.122 s and failed at
       8/16 and 14/16 — four failures in nine runs — so the two "lending" checks
       were failing because they run sixteen flows, not because they lend.
     - **The stalled flows never carry a byte, and the backend says so itself.**
       In the one-claim arm the echo service's own per-connection lines split the
       sixteen flows exactly in two: **eleven** read `reads=200 bytes=12800`
       (200 × 64 B, the whole workload) and **four** read `reads=0 bytes=0` —
       and those four are precisely the flows that printed no `FLOW` line, i.e.
       the four that timed out at `0/200`. So their connection was accepted, the
       banner was sent, and **the banner never reached the visitor**, which is
       why not one request ever existed to be lost. The per-lane counters agree:
       the lane carrying them shows `forwarded=616` out and `offered=211` back,
       mirrored exactly on both ends, so nothing was dropped *inside* the tunnel.
     - **Which names the lost packet: the first reply on a fresh flow.** Every
       loss counter the harness can read is zero — `no_channel`, `overflow`,
       `unclaimed` (sixteen packets), `queue_wait_max_ms`, and the kernel's own
       `rx_drop`/`tx_drop` deltas across the arm — so the banner is lost between
       the lane and the visitor, on the return path, for a flow that has just
       been placed. That is a *per-flow first-reply* defect, not a throughput,
       window or drop-count one, and it is the same shape the very first
       instrument reported: `timed out after 0/200` after reading the banner —
       except that for these flows even the banner is missing.
     - **The carrier is cleared to the frame, and the packet dies after
       injection (2026-10-30, load 1.3–1.7).** Two per-packet traces — `l3 frame
       out` where this end hands a frame to a lane, `l3 frame in` where the far
       end receives it — settle the tunnel half outright. A one-claim arm with
       four dead flows (`12/16 in 6.024 s`) read:

       | visitor port | client `out` | server `in` | hand-off |
       |---|---|---|---|
       | 54778 (dead) | 4 | **4** | 0.50 ms |
       | 54822 (dead) | 4 | **4** | — |
       | 54882 (dead) | 4 | **4** | — |
       | 54890 (dead) | 3 | **3** | — |
       | 54738 (live) | 206 | 206 | — |

       Every frame the client handed over arrived, with the timestamps half a
       millisecond apart. So the lane lost nothing, and the banner — five copies
       of it, the echo's own retransmissions — reached the server's **device
       write** and then vanished with no counter moving anywhere: the `nstat`
       probe added this round (routing errors, discards, listen-queue overflows,
       TCP errors and timeouts) reads **empty in all three namespaces**, and so
       does the interface-drop delta.
     - **The timing is the remaining clue.** For that dead flow the first banner
       crossed at `09:16:39.405` and the next three at `09:16:44.412` — **5.006 s
       later, within 0.4 ms of each other**, exactly the visitor's timeout. So the
       packet after the first was held for five seconds and then released in a
       burst, which is why the earlier runs' `read_gap_max_ms` reads 761 ms,
       1 882 ms and 4 442 ms depending on the run. A stall of that shape, on one
       end, in a run whose other end reports a one-second gap, is what the two
       daemons' read loops show — and nothing else in either log does.
     - **Two consecutive green acceptance runs, and the objective's other
     criteria checked mechanically (2026-10-30).** The suite that passes once may
     be a good draw, so it was run again at a load of 12–14: **16 PASS, exit 0**,
     every arm **16/16** (0.109 s / 0.090 s / 1.134 s / 1.122 s / 1.128 s). Two
     runs in a row at load 12–14 is a much stronger statement than one at load 1,
     and the lending arm — the intermittent one — passed both.
   - **"No new configuration key" is verified, not assumed.** `git diff
     v0.11.2..HEAD -- src/config/` touches two files and **every changed line is
     prose**: `KCP` → `mhd` in documentation comments and in the removal messages
     that tell a reader which key to delete. No field, no `serde` attribute, no
     alias, no new table. The branch adds no key and removes nothing a user could
     still write.
   - **The accept queue was one of the two defects, and it is fixed
       (2026-10-30).** The backend called `srv.listen(8)` while the arm opens
       **sixteen** connections at once, so the excess sat in the accept queue —
       the kernel completes the handshake, the *application* has not accepted, and
       no banner is sent. That is precisely the shape measured: visitor
       ESTABLISHED, nothing pending, nothing unacked, `0B of 5B` of banner, and
       the backend's own `CLOSED … reads=0` written only after the visitor had
       given up.

       With the backlog at **128**, three of the four tunnel arms pass **16/16**
       and the verdict goes from **13/15 to 15/16**:

       | arm | before | after |
       |---|---|---|
       | 16 concurrent flows | 8–16 of 16 | **16/16 in 0.111 s** |
       | lending | 2–13 of 16 | 8/16 in 6.073 s (still failing) |
       | lending off | 5–16 of 16 | **16/16 in 1.117 s** |
       | one claim | 11–16 of 16 | **16/16 in 1.122 s** |

       The accept timestamps confirm it: all sixteen are now accepted within
       **3 ms** of each other, where before the stragglers were accepted only
       after their visitor had timed out.
     - **Which splits item (3) in two, and both halves are now answered
       (2026-10-30).** The *banner* losses were the accept queue. The remainder
       was the lending arm's first-request stall — and with the queue confound
       removed, **lending is the differentiator after all**: the lending-off
       control passes 16/16 in 1.117 s while the lending arm failed 8/16 in
       6.073 s in the same run. The earlier "lending is innocent" reading was
       taken with the accept-queue defect in place, which cost both arms
       equally.
     - **And the next run passed everything.** `just l3-accept` printed
       **sixteen PASS and exit 0**, with every arm at 16/16:

       | arm | result |
       |---|---|
       | 16 concurrent flows | 16/16 in 0.105 s |
       | control, no tunnel | 16/16 in 0.090 s |
       | lending | 16/16 in 1.174 s |
       | lending off | 16/16 in 1.169 s |
       | one claim, four lanes | 16/16 in 1.188 s |

       Both checks this item was opened for — `the busy claim lost traffic while a
       lane was lent` and `lending-off either moved a lane or lost the workload` —
       are green, along with the two new controls. **The item is closed with one
       caveat stated plainly:** the lending arm is still intermittent, 8/16 in one
       run and 16/16 in the next, so the check can still go red on a bad draw.
       What is fixed is the systematic cause; what is left is a rate.

       ```
       timed out before the banner (pending=0 tcp_state=1 rto_ms=201 unacked=0)
       ```

       `tcp_state=1` is **ESTABLISHED**; `pending=0` says nothing arrived;
       `unacked=0` says nothing this end sent is outstanding; and `rto_ms=201`
       says the timer is a healthy derived value, not a backed-off one. So the
       visitor's connection is up, empty and idle, and no segment it sent is
       waiting for an acknowledgement — the banner is not being held anywhere on
       the visitor's side, and nothing there is retrying.
     - **What that pins down.** Combined with the rest of this section — the
       frame traces putting the banner in the server's hand 0.5 ms after the
       client let go of it, the per-lane counts mirrored exactly, all three
       namespaces' interface deltas empty, and `nstat`'s routing, listen-queue
       and TCP error counters empty — the banner is transmitted, arrives at the
       visitor namespace, and never reaches a socket that is demonstrably waiting
       for it. Every instrument this harness has now says the same thing, and the
       one that could say more is a packet capture inside the namespaces
       (`tcpdump`/`tshark`, neither installed).
     - **The blind write ran, and the defect is the return path alone
       (2026-10-30, load 0.85).** A stalled flow never sends, so the forward
       direction was untested for exactly the flows that fail. The probe now
       writes one request *after* the banner times out and listens for a second,
       and the backend's own per-connection counters close the loop:

       | flow | what the visitor saw | what the backend recorded |
       |---|---|---|
       | 0, 2, 12, 14 (stalled) | `pending=0 rto_ms=201 unacked=0`, then `TimeoutError` after the blind write | `reads=1 bytes=64` — **the blind write arrived and was echoed** |
       | 15, 10, 8, 11 (passed) | 200 round trips | `reads=200 bytes=12800` |

       So the **forward direction works for the same five-tuple that fails**: the
       64-byte request crossed, the echo answered it, and that answer never
       arrived — exactly like the banner before it. Placement, claims, lanes and
       the client's device-read are therefore all exercised successfully by the
       same flow that cannot hear a reply.
     - **Three more exclusions, and one hard constraint (2026-10-30).**
       - **Not a partial read.** The probe now reports how much of the banner
         arrived before the timeout, and every stalled flow reads **`0B of 5B`** —
         nothing at all, not a truncated reply.
       - **Not memory pressure.** The container's cgroup has a **13 GiB** limit
         with `oom_kill 20` recorded, so a failing acceptance run was checked
         before and after: `20` before, `20` after. The stall is the product's.
       - **And packet capture is impossible here.** `tcpdump` installs
         (`apt-get` worked), but the container has **no `CAP_NET_RAW`** in its
         effective, permitted or bounding set, so every capture attempt fails with
         `You don't have permission to perform this capture on that device`. The
         question this section reduces to — *who acknowledged the backend's
         replies* — therefore cannot be answered by watching the wire in this
         environment, and the instrument has to live in the product.
     - **The next instrument is in-product:** count what `inject_wire` returns
       for frames arriving on a lane (written / refused, with the errno), and
       report the receiving socket's `TCP_INFO` from the daemon's own side. Those
       two together say whether the frame reached the device and whether the
       kernel's view of the connection matches the application's.
     - **The backend never retransmitted, so its replies were acknowledged
       (2026-10-30, load 0.95).** The echo service now reports its own kernel's
       `tcpi_total_retrans` on the connection's closing line, which is the one
       number that separates "the reply was lost on the way" from "the reply was
       never needed again":

       | flows | backend reads | backend retransmissions |
       |---|---|---|
       | 5, 4, 10, 9, 13 (stalled) | **1** (the blind write) | **0** |
       | 3, 15, 8 (passed) | 200 | 0 |

       A reply stays unacknowledged in `tcpi_unacked` and is resent while it is
       not; `retrans=0` means every byte the backend sent — the five-byte banner
       and the 64-byte echo — was **acknowledged**.
     - **Which is the sharpest fact in this whole section, and it is a
       contradiction to resolve rather than a cause.** The visitor's socket is
       ESTABLISHED with an empty receive queue, and the backend's replies were
       acknowledged. Both cannot be true of the same socket unless the
       acknowledgement came from something that is not the visitor's socket. That
       is where the next investigation starts: not "where did the packet go" but
       "who acknowledged it", and the answer is inside the client namespace's
       stack, where the claim's address is a local address.


     - **The `unclaimed` counter names a real defect: the first SYN of every
       flow is dropped at arm start (2026-10-30, load ~1).** Sixteen frames per
       one-claim arm are counted `unclaimed` on the server, consistently
       (16, 17, 16, 17 across runs), and the five-tuple says exactly what they
       are:

       ```
       l3 device frame unclaimed proto=6 src=10.10.0.2:44672 dst=10.99.0.1:8443
       ```

       They are the **visitors' first SYNs**, one per flow, and they all land
       inside **1.5 ms** at the start of the arm (`09:36:08.2617` to
       `.2632`). The claim has no *live lane* yet at that instant — the client
       registers the claim and then dials its lanes — so the server's routing
       table has no entry for `10.99.0.1:8443` and the packets are dropped as
       unclaimed rather than held. Each affected flow then waits for its kernel's
       SYN retransmission, about **one second**, which is the slow half of the
       bimodal shape this arm has shown all along.
     - **What that does and does not explain.** It explains a one-second delay on
       every flow that loses its first SYN and nothing more: the visitor's
       timeout is five seconds, so a single SYN retransmission cannot by itself
       produce the `timed out after 0/200` failures. The banner — the packet the
       visitor actually waits for — is a separate loss, still unexplained, and
       the flows that fail do *connect* (sixteen `CONNECTED` lines against ten
       to thirteen `FLOW` lines, so the handshake always completes and the banner
       is what goes missing).
     - **What is now excluded, all by measurement, on this one cell:** the
       carrier (frames cross with 0.5 ms hand-off and identical counts), flow
       control (no credit, backlog or queue wait), kernel drops (`/proc/net/dev`
       deltas empty), the kernel's own error accounting (`nstat` empty: routing,
       discards, listen overflows, TCP errors and timeouts), the claim and lane
       counts (the one-claim control fails too), lending, runtime width, and TUN
       checksum/GSO offload (a build with plain-IP frames fails identically:
       11/16, 13/16, 12/16).

       | what the visitor saw | how many | what the backend recorded |
       |---|---|---|
       | 200/200 round trips | 5 | `reads=200 bytes=12800` |
       | `timed out after 0/200` | 5 | `reads=110–113 bytes≈7 KB error=ConnectionResetError` |
       | never got the banner | 6 | `reads=0 bytes=0` |

       So **the requests do reach the backend** — that hypothesis is dead — and
       five flows carried more than a hundred round trips before their socket to
       the echo service was **reset**. The six that never moved a byte were
       still *accepted*: the echo answered the handshake and sent its banner, and
       the banner never arrived. The loss is therefore on the **return** path (6
       flows) and in a **mid-transfer reset** (5 flows), not in the outbound
       direction.
     - **Every carrier-side explanation is closed for this run.** The one
       `mhd stalled` line the server wrote reads `unsent_ms=0 unacked_ms=208
       credit_blocked=false`, and the new backlog fields beside it are all zero
       (`backlog_streams=0`, `backlog_bytes=0`): nothing was waiting to go out,
       no reader was behind, and no credit was being withheld. Both ends placed
       **16 of 16** flows, and no reset, no dead lane and no drop appears in
       either daemon's log. What that leaves is the client's forwarding path —
       its socket to the backend was closed under it — and the frame it injects
       back into the device.
     - **The next run must turn `MOLEHILL_L3_STATS=1` on.** The per-member
       counters (`injected`, `unclaimed`, `no_channel`, `overflow`, `peer_refused`)
       are the only place that can say whether the six lost banners were
       injected and dropped by the kernel, or never injected at all — and this run
       had the switch off, which is why the question is still open with the
       answer written down one level away.
     - **The per-flow question has an instrument now (2026-10-30).** The echo
       service printed `PEER` and then per-read `RECV` lines from one thread per
       connection, interleaved and untagged, so a per-flow read count could not
       be read out of the log at all. Every connection now ends with one line
       that names it — `CLOSED <ip>:<port> reads=<n> bytes=<b>` plus
       `error=<kind>` when the peer reset it — so the question "did this flow's
       requests reach the backend, and with how much" is one `grep` against the
       address the probe printed for that flow (`FLOW <n> local=<ip>:<port>`).
       A reset is recorded rather than raised: one failed flow must not take the
       harness down with a traceback.
     - **A fifth run, after the carrier's window leak was fixed (2026-10-30,
       load 1.6):** **13/15**. The plain sixteen-flow arm passed **16/16 in
       1.122 s** and the control **16/16 in 0.091 s** (**23 513** round trips/s
       against the tunnel's **2 741**), so the leak fix changed nothing here —
       expected, because a lane in this topology carries one long stream, not
       many short ones. The two that failed are the same pair: the lending arm
       **8/16 in 6.360 s** and the lending-off arm **15/16 in 6.049 s**. The
       lending arm is the worse of the two in every run that has both, which is
       the one thing the flake does say consistently.

2. **P3b/P3c** — the unreliable datagram/L3 frame classes are in the wire but
   not yet wired into the UDP-forwarding and transparent paths; the header MAC,
   the unauthenticated-state budget, the PMTU probe, pacing, `SO_REUSEPORT`
   accounting on the client and the `SessionHealth`/carrier-rule evidence are
   not done. `mhd` streams currently carry those paths through the reliable
   stream, which is correct but loses the loss-isolation win. **The `l3` timeout
   above is this path**: the bench's transparent arm over mhd does not complete,
   which is the first thing to fix if the carrier is kept.
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
| `52cb6e6` | rename the carrier to `mhd`: engine module, adapter, feature, runtime enum, stats switch, status counters; the wire dialect moves 5.2 → **6.0** because the registration carries the carrier name |
| `9b26073` | the pool holds `Arc<dyn PoolLane>` instead of a concrete yamux handle, so a second carrier can hand it lanes |
| `28686e5` | GSO: a batch of equal-size datagrams leaves as one `sendmsg` with `UDP_SEGMENT`; the kernel segments, the NIC emits |
| `1291c42` | GRO: the receive batch holds one region per run and splits it on the segment size the kernel reports |
| `3de5953` | path MTU from the kernel's own `EMSGSIZE`: halve to the engine floor; no ICMP dependency |
| `3dde835` | one listener socket per core under `SO_REUSEPORT`, and the recorded decision that pacing stays in userspace |
| `1f191ee` | stage a session's batch for the datagram it runs at (45 KiB, not 256 KiB), with the bound pinned by a test |

**The P2 gate is unmeasured, and that is the one open question.** The bench
model refuses to start here: its topology is three network namespaces and two
TUN devices, so it needs root, and this container has no working `sudo`
(`sudo: unable to send audit message`). `cpu_s_per_gbit` for mhd against TCP
is therefore **not** known, and no claim about the carrier's cost is made
anywhere. The run that would answer it is
`MOLEHILL_DATA_CARRIER=tcp|mhd sudo -n just bench --profile screen --aa`,
interleaved on one host; until it exists, phases 3-5 proceed on the plan's
reasoning and not on evidence.

**Fixed in the new engine (P3a).** The connection now re-sends what the recovery
declares lost. The defect was in the send buffer, not in the recovery rules: an
acknowledgement advanced the base to the highest offset acknowledged instead of
over the contiguous run that starts at it, so acking packets 1..5 walked the base
past the hole at 0 and the bytes behind it were gone before the rewind arrived.
The buffer now keeps the acknowledged ranges and advances only over the run that
starts at its base, which is what makes a hole survive until it is re-sent. Two
tests that stated the intent now pass rather than being ignored:
a_dropped_datagram_is_recovered_without_losing_a_stream and
a_lost_datagram_holds_one_stream_and_not_the_other.

Two defects were found by the phases themselves, both worth remembering:
losing the receive region on the first `EAGAIN` (a batch that can never receive
again) and parking an oversize datagram instead of shrinking (a retry loop that
never terminates).

**Feasibility of the "one owner" rewrite, measured (2026-10-31).** The question
was whether one task owning a connection's socket and its stream buffers — a
byte crossing user space once instead of two or three times — closes the 1.39×
`cpu_s_per_gbit` gap. It does not, and the study found a ship-blocking defect
on the way. The budget (`MOLEHILL_MHD_STATS=1`, `screen`/`cost`, 272 Gbit per
round, both daemons, per carried Gbit): record `seal` 0.0896 + `open` 0.0880,
`tx_build` 0.0932, `tx_send` 0.0505, sender `rx_total` 0.0479, receiver
`rx_total` 0.0670, `io_write` 0.0199, `io_read` 0.0164 — 0.4725 measured
against the run's 0.728. The timers charge their own clock reads (about 0.1 µs
per event) to the phase they time, so a copy phase is overstated by roughly a
tenth. The copies come to 0.081 s/Gbit in total (`append` 0.029, `io_write`
0.019, `tx_encode` 0.017, `io_read` 0.016), and the rewrite removes at most
0.076 of them, because the iovec split that would save `tx_encode`'s copy
costs 55 ns per datagram of kernel work back (494 against 439 ns for a
44-datagram run, measured). **Projection: 1.39× → about 1.22×, not parity.**
The rest is per-datagram bookkeeping (about 0.08: stream selection, the flight
table, the lock, dispatch) and the syscall (about 0.05), and neither has a TCP
counterpart to remove — TCP's kernel does the same per-segment work, which is
why the two kernel halves already measure equal (0.192 against 0.184).
**The defect**: the mhd sender's memory grows without bound under load.
`rss_peak_mib` is 2 742–7 379 for mhd against 46–61 for TCP on the same cell —
6.7 GiB inside one 40-second round. A probe build found a single stream holding
up to 3.7 GB, with 987 000 acknowledged ranges queued behind a 31 KB hole at
its base. Two defects produce it: `Recovery::on_ack` deletes a packet when it
declares it lost, so the late acknowledgement of the original finds no entry
and `SendBuf::ack` is never called for that range; and `apply_ack` rewinds the
lost ranges in ascending order to `sent = offset.max(base)`, so only the
highest one is retransmitted. `base` therefore never moves, `compact` never
runs, and the buffer grows with everything written — in the later sample, 1.5
GB of the 8.7 GB carried. Changing that
one line to `sent = sent.min(offset.max(base))` took the cell from 2 742–7 379
MiB to **768 MiB** with no CPU change (0.728 → 0.720, inside the cell's own
7 % spread), but a hole still formed (17 KB, 31 000 ranges, 110 MB buffered),
so that is a mitigation, not the fix. A carrier that grows 150 MB/s while
carrying 8.5 Gbit/s cannot ship, whatever happens to the rest.
**The plan this leaves**, in order: (0) fix the acknowledgement hole — keep a
lost record until some transmission of that range is acknowledged, and never
let a rewind advance the send point; ships whatever else happens; (1)
receive-side zero copy (`append` into `Bytes` slices of a pooled region, then
`io_read` through an owned-read seam), 0.03–0.05; (2) send-side owned writes
behind an enum carrier rather than a boxed trait object, 0.015–0.02; (3)
re-measure and stop at about 1.22×, where the remaining choice is batching the
per-datagram bookkeeping (one lock, one timestamp, one table walk per run) or
accepting the carrier's cost and documenting it. The full study, its commands
and the raw logs are `target/evidence/feasibility-one-owner.md` and
`target/evidence/work-{phases-mhd,phases-tcp,rss-cost,rewind}`.

**Where the memory actually is, as of 2026-10-31.** The acknowledgement hole is
fixed (the record survives until acked, `rewind_to` only moves back,
overlapping ranges merge, and the ack block now carries the receiving stream's
byte prefix), and the resident peak came down from 2 742-7 379 MiB to 650-730
MiB. What is left is **not the send buffer**, and three measurements say so:

| experiment | `rss_peak_mib` | reading |
|---|---|---|
| `MAX_BUFFERED` at 2 × WINDOW (64 MiB) | 650-730 | the committed build |
| `MAX_BUFFERED` at 1 × WINDOW (32 MiB) | 571 | halving the cap does not halve the peak: the cap is not binding |
| `MALLOC_ARENA_MAX=2` | 494 | the peak is allocator-sensitive |
| `+ MALLOC_TRIM_THRESHOLD_=0` | 464 | but it is **not free space**: trimming what glibc can reach does not reclaim it |

So the residual is a fragmented heap held by live allocations, about four to
five times the live buffers the phase probe can see (send buffers ~50-116 MB per
lane, receive backlog ~0 because the readers keep up), and it grows within a
round with the bytes carried. The per-datagram allocator churn is the suspect —
`Recovery::in_flight` and `SendBuf::acked` are `BTreeMap`s whose nodes are
allocated and freed once per packet, roughly 700 000 times a second here, across
fourteen threads and their arenas. That is a shape change (slab or vector
storage), not a copy to remove, and it is what the next round has to price
before the receive-side zero copy: the copy is 0.029 s/Gbit, the memory is
650 MiB, and only one of them is a release blocker.

**Correction (same day): it is a plateau, not a leak, and a third of it was the
send buffer.** Two measurements settled it. A cell four times longer
(`--scale 2`) reads 652.5 MiB against 650.4 for the normal one, so the peak is a
bounded working set, and the `BTreeMap` churn above is not a leak. And a send
buffer that empties now gives its capacity back (`IDLE_CAPACITY`, 256 KiB):
**652.5 → 464.6 MiB**, with `cpu_s_per_gbit` 0.728 → 0.737 and throughput
8.28 → 7.91 Gbit/s, both inside the run-to-run scatter of this host. The peak a
stream once reached was otherwise its footprint until the stream retired, and a
lane holds every live stream's peak at once — which is why halving
`MAX_BUFFERED` did nothing: the cap was never what set the footprint.
Shrinking on *every* compaction (to twice the live length) was measured and
rejected: 497.7 MiB and 6.75 Gbit/s, worse on both, plus one cell that failed
with `control socket has closed unexpectedly`. The residual 464 MiB is still
above the 300 MiB target, and one cell in three fails on this host under load
4-8 with the same iperf3 error, which is the measurement environment, not the
carrier (it reproduces on a build with no change in it).

**The three carrier changes were re-verified against their gates (2026-10-31).**
`just l3-accept` exits 0 with all sixteen checks passing, including the two
lane-lending arms and the sixteen-flow arm. `bulk-n` under one percent loss with
an A/A twin (`--profile dev --scenarios bulk-n --condition loss1 --rounds 4
--aa`) records no failure in either arm: 8.60-8.99 Gbit/s for the measured arm
and 8.97-9.16 for its twin. So the acknowledgement rework, the byte prefix and
the buffer shrink did not move the loss recovery or the transparent path, which
is what those gates exist to catch.

**The receive-side zero copy was tried and rejected (2026-10-31).** The design
was the one the plan named: `RecvBatch` hands each datagram on as a refcounted
slice of a pooled 64 KiB region, and `RecvBuf` stores those slices instead of
copying into a growable `Vec`. It works, and it made the copy cheaper — `append`
fell from 0.40 to 0.26 µs per datagram (0.029 to 0.023 s/Gbit) — and it is still
a loss: throughput **8.28 → 3.68 Gbit/s**, `cpu_s_per_gbit` **0.728 → 1.099**,
resident set unchanged (633 against 652 MiB). The mechanism is the pinning: a
piece keeps its whole region alive until the reader consumes it, and at a 32 MiB
credit window one bulk stream spans about five hundred regions, so the eight
region pool is always dry and every `recvmsg` allocates and zeroes a fresh 64 KiB
region. Growing the pool does not help (the regions are pinned by the window
itself), and refusing the wake instead — the first version — turns one stuck
reader into a stalled connection, which `pool_test` caught twice
(`a_pinned_pool_starts_at_its_count_and_stays_there`,
`one_unanswerable_visitor_does_not_park_the_service`). Copying each datagram into
a warm `Vec` is what keeps the region free, and that is the copy the plan wanted
to remove. **So the receive path stays as it is**; the remaining 464 MiB is not
this, and the next attempt should copy out of the region into a *right-sized*
`Bytes` (one allocation, no pinning) rather than hand the region itself on.

**The chunked send buffer was tried and rejected too (2026-10-31).** Replacing
the single growable `Vec` with a `VecDeque<Bytes>` (one chunk per written record)
does remove the compaction memmove — `compact` became a `trim` that pops whole
chunks — and every unit test passed. It is also **quadratic**: `range()` walks the
queue from its front to find the offset a packet carries, and the send point is
near the tail, so each packet walks the whole window. Measured: `cpu_s_per_gbit`
**46.7** and **0.032 Gbit/s** (against 0.73 and 8.3). Reverted. The structure is
still right — a chunk queue frees a record the moment it is acknowledged, which
is what the `Vec` could not do — but it needs a **read cursor**: the queue index
`next_offset()` currently points into, advanced as the send point moves and reset
when the base passes it, so a packet finds its chunk without scanning. That is
the next attempt, and it should be measured against the sender's `ack` phase
(0.053 s/Gbit) rather than guessed at.

**The cursor was built and the queue still loses (2026-10-31, third attempt).**
With the cursor in place every unit test passed, and the first measurement was
still catastrophic because `written()` summed the queue on every call — a second
quadratic hidden in a helper. With that fixed to an incremental `tail`, `ack`
fell to 4.82 µs per acknowledgement (the `Vec` build reads 19.89) and the cell
still read `cpu_s_per_gbit` **4.38** and **1.35 Gbit/s** against 0.72 and 7.47.
The reason is packetisation, not the queue: `range()` can only hand `wire` a slice
of **one** chunk, so a packet never spans a record boundary, the writer is
re-entered six times as often (52.9M `io_write` calls against 8.4M) with
proportionally smaller writes, and the buffer grows because the producer is
faster than the packetisation can drain it. Making the queue work therefore needs
`wire::encode_stream` to take the payload as a list of slices (an iovec-sized
change in the sender), which is a different project from "remove the compaction
memmove". Reverted again; the `Vec` stays, with its 256 KiB idle shrink, until
that is decided.

**Where the carrier stands, measured at `cf0319d` (2026-10-31).** Two `screen`
campaigns, one per carrier, same binary, same host, back to back:

| arm | `cpu_s_per_gbit` | `rss_peak_mib` | `throughput_gbps` |
|---|---|---|---|
| `mhd` | 0.7246 [0.6653..0.7838] | 452 [385..520] | 7.20 |
| `tcp` (`MOLEHILL_DATA_CARRIER=tcp`) | 0.5456 [0.5434..0.5478] | 50 | 12.48 |

**1.33×**, and the honest bound is wider than the point: mhd's own two rounds
span 0.665-0.784 while TCP's span 0.543-0.548, so the ratio this host can
resolve is 1.22-1.44. The memory is **nine times TCP's** and, unlike the CPU,
it is not in doubt: 452 MiB against 50, flat across a cell four times longer
(652.5 against 650.4 before the idle-buffer shrink landed, and the shrink is
what took it to the 450-500 band).

**What that means for the remaining plan.** Step (1) is done: the byte prefix
is in, the acknowledgement hole is closed, and the resident set went from
2 742-7 379 MiB to 452-520. Steps (2) and (3) were each measured twice and each
failed for a reason that is **not in the buffer structure**:

- the receive side pins a 64 KiB region per datagram slice, and the credit
  window spans about five hundred of them, so the pool is always dry;
- the send side cannot hand `wire` more than one chunk, so a packet never spans
  a record and the writer is re-entered six times as often.

Both need a **layer interface** to change — the receive region's ownership, and
`wire::encode_stream` taking a list of slices — which is a larger, separate
project than "remove a copy". The decisions this leaves are therefore two, and
they are the human's: accept 1.33× and 452 MiB as the carrier's documented
cost, or commission the interface work (receive-region ownership + sliced send
payload) as its own effort with its own gates.

**The human commissioned it, and the sliced payload works — the queue's memory
does not (2026-10-31).** `wire::encode_stream` now writes only the header and the
stream appends its payload straight into the staged GSO run through
`SendBuf::append_to`, which walks the chunk queue from a cursor and **crosses
chunk boundaries**. One contiguous run, no iovecs. Measured on the same cell as
`base7b` (0.7229 s/Gbit, 7.47 Gbit/s, 496 MiB):

| | `base7b` | sliced payload |
|---|---|---|
| sender `ack` | 0.0401 s/Gbit (19.89 µs/ack) | **0.0287 (13.08 µs)** |
| `io_write` calls | 8.4M | 6.47M |
| `cpu_s_per_gbit` | 0.7229 | 0.7748 |
| `throughput_gbps` | 7.47 | 6.36 |
| `rss_peak_mib` | 496 | **825** |

So step (A)'s gate is met — the compaction memmove is gone and `ack` is 28 %
lower — and the change is still net-negative: 15 % of throughput and two thirds
more memory. The memory is the thing to understand before anything else: the
queue's live bytes are `tail - head`, which is exactly what `buffered` counts and
what `MAX_BUFFERED` caps at 2 × WINDOW per connection, so four lanes should not
reach 825 MiB. Either a path adds to `buffered` without the queue growing, or a
chunk is retained past its acknowledgement — `trim()` only pops chunks **wholly**
below the base, so a 32 KiB record the peer acknowledged halfway keeps the whole
chunk alive, and at a window of thousands of chunks that is the right order to
explain the difference. **Next round re-applies the change, logs `buffered`,
`tail - head` and the chunk count once a second beside RSS, and settles which of
the two it is.** The change itself is not on the tree: it was reverted after the
measurement, so a re-apply starts from this record.

**It is in, and the L3 gate cannot be attributed to it (2026-10-31).** The
sliced payload was re-applied and measured twice more before landing: sender
`ack` 0.0287 s/Gbit (gate met), `cpu_s_per_gbit` 0.6418/0.6465 against 0.7229,
throughput **9.65/9.70 Gbit/s** against 7.47, and `bulk-n` under one percent loss
at **11.45-12.03 Gbit/s** in both arms against 8.60-8.99 before. Resident peak is
628/774 MiB against 496, which the cell explains by carrying thirty percent more.
Committed as `0eed55e` after a full serial suite and both clippy passes.

`just l3-accept` did **not** reach sixteen: two runs on the chunk build gave
14 PASS / 2 FAIL and 15 PASS / 1 FAIL, naming the lending arms. The same harness
on the build **without** the chunk queue then gave 15 PASS / 1 FAIL, naming a
different check ("lending-off either moved a lane or lost the workload"), so the
failures are the host and the harness, not the change — the 16/16 passes were
taken on a calmer machine. The honest state: the L3 gate is **unverified today on
either build**, and the next attempt should run the two alternately on a quiet
host rather than treat one red run as a verdict.

**Receive-region ownership was built and rejected (2026-10-31).** The shape the
plan named: the batch keeps its 64 KiB regions, and the connection copies each
datagram out into a right-sized `Bytes` so the region is never pinned. It does
what it says on memory — `rss_peak_mib` **376** against 628-774 for the chunk
build — and it costs so much elsewhere that it cannot ship:

| | chunk build (`0eed55e`) | copy-out |
|---|---|---|
| `rss_peak_mib` | 628-774 | **376** |
| `cpu_s_per_gbit` | 0.642-0.647 | **1.597** |
| `throughput_gbps` | 9.65-9.70 | **2.76** |
| `append` | 0.023 | 0.0273 |
| `io_read` per call | 1.15 µs | **5.15 µs** |

Two of its three gates miss: `append` 0.027 > 0.020, and throughput is a third
of the baseline. The mechanism is in `io_read`: a piece per datagram means the
reader walks a table of thirteen hundred entries per megabyte instead of a few
merged runs, so the copy out of the receive buffer — the one copy the design
could not remove — got four times more expensive per call. Reverted.

**What this leaves.** The two directions pull against each other: merging the
receive buffer into runs is what makes reading cheap, and it is also what makes
a stream's buffer hold its peak. The memory target of 300 MiB is therefore not
reachable by moving the copy; it needs either a smaller credit window (a
behaviour change, not a refactor) or a receive buffer that merges into a
*pooled* run and returns it when read — which is where this stopped.

**The window is not a cheap lever either (2026-10-31).** One constant, three
campaigns, the same `cost` cell at four times the normal length:

| `WINDOW` | `rss_peak_mib` | `cpu_s_per_gbit` | `throughput_gbps` |
|---|---|---|---|
| 32 MiB (shipped) | 628 | 0.647 | 9.70 |
| 16 MiB | **521** | 0.705 | **7.26** |
| 8 MiB | — (the cell failed on an iperf transport error, not on the carrier) | | |

Halving the window buys 107 MiB for a quarter of the throughput. Extrapolating
to 300 MiB would cost the carrier most of what `0eed55e` just won, so the answer
to "is 300 MiB a constant away" is **no**. The remaining way to it is the pooled
receive run, which is a design of its own; the alternative is to accept
450-650 MiB, bounded and flat, as the carrier's documented cost.

**The pooled receive run landed, and it is worth 240 MiB (2026-10-31).** The
design the last paragraph named: a receive piece is no longer a growable `Vec`.
It is a `VecDeque` of spans into 16 KiB blocks taken from a per-connection
`Blocks` pool (`stream.rs`, `Blocks`/`Held`/`Piece`; the pool is a field of
`Conn`), the front span is trimmed as the reader passes it, and a block goes
back to the pool the moment the reader has consumed it. `discard` and the
hole-folding `absorb_next` return blocks the same way, and the keepalive gives
the whole pool back when nothing is on loan (`Blocks::shrink`), so an idle lane
does not sit on a window's worth of blocks.

One trade is worth stating: a piece costs at least one block, so a path that
delivered many isolated fragments would spend 16 KiB where it used to spend the
fragment's own bytes — the credit window bounds the bytes, not the number of
fragments. The peer here is the other molehill daemon over an authenticated
channel, so that shape is a badly reordering path and not an adversary; none of
the measured paths came near it (the probe never saw more than one piece).

Two measurements made the mechanism explicit before the change:

| where the memory was | reading |
|---|---|
| heaptrack's top peak site | `RecvBuf::insert` → `RawVecInner::finish_grow`, i.e. the piece vector doubling |
| live piece capacity at one probe | 86 MiB of capacity for 79 MiB held (the doubling's overshoot) |
| RSS minutes after the transfer, capacity 1.3 MiB | 178 MiB — the heap never came back |
| the same manual loopback pair on TCP | client peak **12 MiB** against mhd's **423** at the same 11.6 Gbit/s |

A per-datagram `to_vec` plus a doubling growth is what left that heap behind;
blocks are exact (one per 16 KiB carried, the last one partial) and reused, so
the allocator leaves the data path in steady state. The same manual pair on this
build peaks at **112 MiB** (client) and 137 (server) at 12.05 Gbit/s, against 423
and 150 before it — with TCP's 12 MiB still the floor to beat.

Measured with the model's `cost` cell, `screen` profile, `scale 2`, l4 arm, as
an interleaved A/B against the previous build in one campaign (`--ab-arm`),
so the host's own noise is shared by both arms:

| arm | `cpu_s_per_gbit` | `throughput_gbps` | `rss_peak_mib` |
|---|---|---|---|
| previous (`b5cabf3`) | 0.6955 / 0.6931 | 8.24 / 8.89 | 663.0 / 694.5 |
| pooled blocks | 0.6604 / 0.6941 | 8.90 / 8.74 | **382.4 / 435.5** |

Two further samples of the same cell on the new build read 373.4 and 379.2 MiB
(`cpu` 0.6774 / 0.7422, 8.72 / 7.98 Gbit/s). **The memory moves by a third; the
CPU and the throughput do not move** — every difference there is inside the
scatter this host shows between two runs of one build, which is why the A/B is
interleaved rather than quoted across hours. The phase budget says the same
from the other side: the receiver's `append` is now **0.0161 s/Gbit at 0.18 µs
per datagram**, against 0.023 s/Gbit on the chunk build and 0.0273 in the
rejected copy-out, and `io_read` is 0.75 µs per call — the merged run is still
one walk per read.

The gates: `append` 0.0161 ≤ 0.020, `rss_peak_mib` 373-436 < 452, throughput
8.7-8.9 ≥ the `base7b` baseline (0.7229 s/Gbit, 7.47 Gbit/s). `bulk-n` under
one percent loss with an A/A twin is green on the l4 arm (12.20-12.78 Gbit/s,
four rounds; its twin 12.29-12.85, with one cell lost to the host's
`control socket has closed unexpectedly`). The `l3` arm times out in the same
campaign on **both** builds, which is the host problem recorded below, not this
change.

`just l3-accept` on this build: the first run passed 13 of 16 and failed the
three concurrent-claim arms, the second passed **16 of 16**. The failure set
moved, and no L3 frame enters the receive buffer at all (`Frame::L3` is
acknowledged and handed on, never stored in a stream), so the first run was the
host's load — the same shape as the 14- and 15-of-16 runs recorded above for
both other builds.

**One gate is not green, and it is not this change.** `just check` passes under
the default carrier and under `MOLEHILL_DATA_CARRIER=tcp`; under `=mhd` it fails
on `pool_test`'s `a_saturated_pool_still_serves_the_next_visitor` ("a fresh
visitor must be served after the pool emptied"). It fails identically on the
build *without* this change — the working tree stashed, rebuilt, same suite:
10 passed, 1 failed, same message — and the same test passes three times out of
three when run alone, so it is an order-dependent failure in the serial suite
under a forced carrier. Worth opening on its own: the forced carrier is what the
carrier measurements use, and a suite that only fails there is a gate nobody can
trust. The pool snapshot at the failure is `size: 1, count: 2, streams back to
zero, draining: false`, i.e. the capacity did return and the next visitor was not
placed.

**It is the burst's tail, and the test was measuring the queue (2026-10-31,
third pass).** The failing assertion was asking one question with a ten-second
budget: connect a fresh visitor, send four bytes, read four back. Timing the
pieces showed where the time goes:

| measurement (single test, `=mhd`) | reading |
|---|---|
| `TcpStream::connect` to the exposed port | **136-226 µs** — the kernel completes the handshake from the backlog |
| the four bytes back, as the test was written | **3.13 / 3.13 / 3.13 s** |
| the same, after five seconds of quiet before the visitor | **47.6 / 47.8 ms** |

So the fresh visitor was not the slow part: it was arriving in the *tail* of the
burst. The visitors are accepted asynchronously, and the ones the placement
ceiling refuses each wait out the pool's capacity timeout, so the pool's stream
count reaches zero while the server still has a queue of them — the test's "the
pool emptied" fires inside that queue, and the visitor then waits behind it. On
a loaded host the queue outlives the ten-second budget and the test fails; on an
idle one it does not, which is the whole of the load correlation recorded above.
The carrier's own opens in that window confirm the shape: they all complete at
one instant, with durations descending in a regular step (458, 437, 417 … 143 ms)
— a queue being released in a batch, not a path that is slow per open.

The test now asks the question repeatedly instead of once: a fresh visitor every
200 ms, two seconds of budget each, fifteen seconds in total, so it measures the
property it owns (a saturated service *recovers*) rather than the length of the
burst's tail. A service that never answers still fails, and fails on the total
window. Verified with five single-test runs (all pass, 9.1-9.8 s) and six full
serial runs of `pool_test` under a forced mhd carrier (all pass).

**Open thread from that measurement: the burst's tail is a serial queue.** The
refused visitors appear to be paired one at a time, each paying the pool's
250 ms capacity wait — sixteen of them is the ~3 s tail, which matches to the
order. Whether the pairing path can overlap them is a product question this test
no longer sees; it is worth measuring on its own before deciding.

**The failure needs host load, and nothing isolated reproduces it (2026-10-31,
second pass).** Sixteen full-suite runs under `MOLEHILL_DATA_CARRIER=mhd` across
the day split by host load and by nothing else: at load 4-6 it failed five times
in eight, at load 1.3 once in eight. The attempts that did *not* reproduce it,
each of which rules out a hypothesis:

| attempt | result | what it rules out |
|---|---|---|
| three suite runs with `MOLEHILL_MHD_STATS=1` | 3/3 pass | the stats reporter's overhead is not the trigger |
| one suite run with twelve spinning CPUs | pass | contention *beside* the suite is not enough |
| the burst-then-fresh scenario repeated 20× in one process | 20/20 served, 142 s | the scenario is sound: the pool, the valve and the backends recover round after round |
| five more suite runs, idle host | 5/5 pass | — |

The snapshots say the two outcomes are the same up to the last step: a passing
run's pool at the drain reads `bytes: [(1197, 798)]`, `mhd_health: (467, 4)`,
one tunnel, zero streams — the *same* byte counts the failing runs printed
(`(463, 7)`), so the burst is carried and drained identically in both, and what
differs is only how often the lane re-sent something. The next instrument is
contention of the kind the failed runs had: pin the suite and a spinner to the
same two CPUs (`taskset`), or catch a failing run with the pool snapshot printed
*at* the failure — the test now does the latter, since its message names the
pool.

**That thread was opened, and it is a latency problem, not a harness one
(2026-10-31).** Eight full serial runs of `pool_test` under `=mhd`: five failed,
three passed; four runs of the single test passed, and the whole check is green
under the default carrier and under `=tcp`. Three things narrow it:

- **Not the wrong pool.** A temporary probe printing every registered pool at
  the failure showed exactly one, keyed `session:127.0.0.1:2370` (this test's
  own), with `streams=0`. The wait watched the right pool and the pool was
  empty.
- **Not the standalone topology.** The same shape rebuilt outside the suite —
  `tests/for_tcp/tunnel_valve.toml` (client asks for two tunnels, the server's
  valve allows one), an echo backend, a scripted burst of 72 concurrent
  visitors — serves 56 of 72 (the ceiling), opens in `p50` 23 ms / max 38 ms, and
  answers a fresh visitor after the burst in **2-3 ms**. Idle first-byte latency
  is 2.5 ms (`p50`); under four bulk streams, 54 ms.
- **The suite's own opens are seconds long.** The carrier already logs its open
  latency (`mhd stream opened and acknowledged`, `us=`). Across three *passing*
  suite runs: n = 1210, min 41.8 ms, **`p50` 1.17 s, `p90` 2.31 s, max 3.68 s**.
  That is the order the test's ten-second budget runs out on, and it is the same
  order as the keepalive (2 s) that a write on an idle lane used to wait for.

So the failing test is the suite's way of showing a real latency defect: under
repeated churn a stream open costs seconds where an idle lane costs 2.5 ms. The
next instrument is a **sustained** churn harness — repeated bursts on one lane,
not one burst — or a dump of the server-side lane state at the moment the fresh
visitor is not answered; the one-shot reproduction above is not enough, and the
DEBUG logging that would carry the dump hides the failure (three runs, three
passes, with `RUST_LOG` on).

**What is left is 80-130 MiB above the target, and it is not this buffer.** With
the receive path exact, the carrier's own footprint per daemon is the send side
(50-116 MB of `SendBuf` at a 32 MiB credit window) plus the process floor: the
same loopback pair on TCP peaks at 12 MiB total, so almost all of it is the
carrier's buffers. The next lever is the send buffer's window accounting, not
another copy to remove — and the target of 300 MiB should be re-costed before
it is chased again, because 382 MiB bounded is now the number a release would
publish.

**The send buffer is where the manual pair's memory is, and the window frees it
— but not on the cost cell (2026-10-31).** heaptrack on the *sending* daemon of
the manual loopback pair puts its whole peak in one place: `StreamIo::poll_write`
→ `Conn::write` → the send buffer, 121.76 MB of 125.53 MB, with 98.88 MB of that
arriving through the Noise/stripe write path. Halving the credit window frees it
there:

| manual pair, `WINDOW` | server peak | client peak | throughput |
|---|---|---|---|
| 32 MiB (shipped) | 162.6 MB | 112.5 MB | 12.39 Gbit/s |
| 16 MiB | **89-91 MB** | **71-73 MB** | 12.83-13.03 Gbit/s |

and it does not carry that win to the model's cell. Interleaved A/B of the two
builds on the `cost` cell (`--ab-arm`, two rounds each): `rss_peak_mib` 375.7 /
386.5 for the 16 MiB window against 407.1 / 331.4 for the 32 MiB one — the arms
overlap, and `cpu_s_per_gbit` (0.690 / 0.676 vs 0.691 / 0.746) and throughput
(8.78 / 9.17 vs 8.78 / 7.38) are indistinguishable. So the cost cell's 380 MiB is
**not** its send buffers; the manual pair's is. The next instrument is heaptrack
on the bench's own daemons during a cell, not another constant: the two
topologies disagree about where the bytes are, and only one of them is the
published number.

**The cell's peak is a fifth allocator retention, and the churn is per packet
(2026-10-31).** Attaching heaptrack to a running daemon needs
`/proc/sys/kernel/yama/ptrace_scope = 0`, which this container refuses even as
root; the way in is a wrapper as `--binary` that execs the real binary under
`heaptrack -o` (and passes `--version` straight through, or the model's
provenance check reads heaptrack's version). Both daemons of one `cost` cell
then profile as: **server peak heap 197.27 MB, client 66.39 MB**, and the
server's share is the send path throughout — `NoiseStream::poll_write` called
from `stripe::send_loop`, 127.64 MB plus 20.99 MB of one backtrace. That is the
*slowed* regime, where the peer's reader cannot keep up and the credit window
fills, so it prices the shape and not the native peak.

The native peak is partly retained free memory, and trimming it prices that:

| same cell, `screen`/`cost`/`scale 2`, l4 | `rss_peak_mib` | `cpu_s_per_gbit` | `throughput_gbps` |
|---|---|---|---|
| committed build | 373-436 | 0.68-0.74 | 8.7-8.9 |
| `MALLOC_TRIM_THRESHOLD_=0` | **313.5** | 0.6648 | 8.70 |

So roughly 60-120 MiB of the peak is free bytes glibc was holding rather than
live data — and the churn that produces them is now visible by count: **9.25M
allocations in one cell's two minutes (77k/s)**, with two sites at 2.18M calls
each, both inside `Conn::on_datagram` on the dispatcher path — a `Vec` reserve
and `Vec<recovery::Sent>::grow_one`, the recovery table's record growing once
per packet. Their *live* contribution is kilobytes (89 KB and 3.15 MB at peak);
what they cost is the rate. The next change is therefore a storage shape: a
slab for the recovery table so a packet takes no allocation, which is the same
direction the send buffer took when it became a chunk queue.

**The acknowledgement's own vectors were the first slice of that, and they did
not move the number (2026-10-31).** `on_ack` built an `Acked` of two `Vec<Sent>`
plus two `Vec<u32>` number lists per acknowledgement — four allocations on a
path that runs tens of thousands of times a second. The change was written and
measured: `Acked` becomes a field of `Conn` that is cleared in place, and the
number lists are taken out of `Recovery`, cleared, filled and put back. It does
what it says — a manual loopback pair's sending daemon drops from **2 427 025
allocation calls (148 269/s) to 2 056 274 (130 814/s)**, with temporary
allocations 200 788 → 146 718 — and it changes nothing that is gated:

| interleaved A/B on the cost cell | `cpu_s_per_gbit` | `throughput_gbps` | `rss_peak_mib` |
|---|---|---|---|
| with the reused outcome | 0.6774 / 0.6505 | 8.54 / 8.96 | 533.3 / 339.7 |
| before it | 0.6344 / 0.6409 | 10.27 / 9.96 | 399.4 / 422.0 |

Two samples an arm, no A/A twin, and every column overlaps — so neither a win
nor a regression is shown, and the peak stayed where the send buffer puts it
(124.6 MB against 128.6 MB on the manual pair). **Reverted**, with the numbers
kept here: the deletion is real, but a change that adds a field and a second
call shape has to earn its place with a measured effect, and this one is a

precondition for the slab rather than a win of its own.

**The window is where the cell's memory is, and sixteen megabytes puts it under
the target (2026-10-31).** The attribution said the peak was neither the receive
blocks nor the send-buffer *cap*: it moves with the **number of lanes**. One
lane on the same cell reads **104.9 MiB** against 373-436 for four, and the
client's own line says `tunnels=4 established=4`. A lane holds up to a window of
receive blocks, and the send buffer's ceiling is written in terms of the same
constant, so the credit window is the one number that sets the total. Halving it
was measured twice, interleaved, on the model's own cell:

| `cost` cell, `screen`, `scale 2`, l4, interleaved A/B | `cpu_s_per_gbit` | `throughput_gbps` | `rss_peak_mib` |
|---|---|---|---|
| 32 MiB (HEAD `4c04b7e`), 4 rounds | — | — | 349 / 497 / 428 / 449, median **438** |
| 16 MiB, 4 rounds | 0.732 | 8.74 | 262 / 265 / 280, median **265** |
| 32 MiB, 5 rounds | 0.763 | 7.62 | 296 / 482 / 331 / 370 / 416, median **370** |
| 16 MiB, 5 rounds | 0.758 | 7.52 | 297 / 256 / 187 / 254 / 229, median **254** |

The separate A/A twin (three rounds, the sixteen-megabyte build against itself)
reads `rss_peak_mib` 237 / 278 / 285 for the arm and 282 / 377 / 449 for its
twin, so this metric's noise on this host is wide — which is why the claim rests
on two independent interleaved campaigns rather than one, and why the *bound* is
the part that is argued: halving the window halves what a lane may hold. CPU and
throughput do not move outside that scatter in either campaign (the largest
difference is −8 % on one four-round arm, −1 % on the five-round one, against a
15 % materiality floor).

Two further confirmations, both from topologies with much less noise: the manual
loopback pair's peak falls from 162.6 / 112.5 MB (server / client) to **100.6 /
72.0 MB**, and the single-lane cell falls from 104.9 to 82.1 MiB. The gates that
are not about memory: `bulk-n` under one percent loss reads 10.49 / 10.69 /
10.91 / 11.03 Gbit/s on the l4 arm (one 5.06 outlier) with its A/A twin at
10.68 / 10.98 / 9.45 / 11.58 and one host `control socket has closed` cell, and
`just l3-accept` passes **16 of 16**.

The trade is a wide-area one and it is arithmetic, not measured here: a lane
stays full while its window covers `bandwidth × RTT`, so 16 MiB holds a lane at
about 650 Mbit/s across a 200 ms path, where 32 MiB held 1.3 Gbit/s. A
deployment that needs the long path's rate raises `[client.data].tunnels`, which
the configuration page already prices at 0.5-0.8 MiB per lane per end.

**The timer's floor is scoped to the flight, not to the connection
(2026-11-01).** The carrier had one retransmission floor serving two situations
that are not alike, and the record above says what each of them measured:

| situation | what a lost packet costs | what a re-send costs |
|---|---|---|
| a stream opening: one packet of the Noise handshake | the floor, `200 ms` — measured as `open_channel` **502.3-503.9 ms** against a 4 µs hello and a 0.2-1.7 ms ack on the same lane | one datagram the peer drops by offset |
| a burst: thousands of packets in flight | the floor, correctly | the whole flight, as new numbers, on every fire |

Lowering the floor *globally* was measured twice and fails both times (six of
six bulk cells, then every sample again with `DEAD_FLOOR` in place), and the
mechanism it failed through is the second row: a cheap timer on a wide flight
re-sends the flight on scheduling noise. So the floor is now chosen by what is
actually in flight — at most two packets arms the 20 ms floor
(`MIN_RTO_SMALL_FLIGHT`), anything wider keeps the 200 ms one — and it is a
clamp *under* `smoothed + 4 × variance`, so it only binds on a path whose
estimate is already milliseconds.

Three tests pin the scope rather than the number: a small flight arms 20 ms, one
packet past the limit arms 200 ms, and a path with no sample at all still arms
`INITIAL_RTO` (333 ms), which is what an opening on a fresh lane pays. **The
measurement is not taken yet**: the instrument is the `us=` field on
`mhd stream opened and acknowledged` (p50 1.17 s, p90 2.31 s, max 3.68 s across
three passing suite runs at `=mhd`), and the run that would show the floor moving
it is the saturated-pool scenario with the open latency read from the log, plus a
`cost` cell for the invariant that the wide path is unchanged.

**A loss is now an owed range, not a rewind of the send point (2026-11-01).** The
send buffer kept one cursor, so a declared loss moved it back to the hole and the
transmitter re-sent from there to the tail of the buffer: every byte already
delivered and every byte not yet sent, as new packet numbers, on **every** loss
event. The repair is now a range beside the cursor — merged with the ranges it
touches, trimmed as it goes out, dropped when the peer's byte prefix passes it —
and the send point only ever moves forward.

The harness measures what that cost: one lost 1 349-byte datagram re-sent
**12 000 bytes** before the change and **1 349** after, which is the difference
between a loss costing one datagram and a loss costing the stream's whole
buffered remainder.

**And it does not move the cell it was aimed at, which is the finding.** An
interleaved A/B of the two builds on `bulk-n` under one percent loss (`dev`
profile, four measured rounds, `--aa`, the carrier forced to mhd, host load 1.54,
`target/evidence/bench-repair-loss1.json`):

| arm | build | `throughput_gbps` | `cpu_s_per_gbit` |
|---|---|---|---|
| `l4` | this change (`b728cac`) | 12.082 [11.908..12.422] | 0.5771 [0.5696..0.5905] |
| `l4~aa` | the same build again | 11.726 [11.309..12.191] | 0.5893 [0.5694..0.5976] |
| `l4~b` | before it (`af839e2`) | 12.064 [11.929..12.314] | 0.5806 [0.5770..0.5845] |

The run's own floor is **3.76 %** on throughput and 2.39 % on CPU, and both arms
sit inside it: **no claim**, and the one directional reading (`retrans_segments`,
the new build re-sending more) is flagged fragile — dropping a single round flips
it. So the rewind amplification was real and is gone, but it was **not what
limited that cell**, and the October record's 9.5-9.9 Gbit/s is not comparable
with these 12.0 (different host, different method run). What the change is kept
for is what it removes: a loss now costs its own range, and the send point is
monotonic, which is the precondition for a scheduler that can bound what a stream
has outstanding. The next suspect for the loss gap is therefore *not* the
re-send volume; the two candidates left are the shared flight/credit accounting
(one cwnd for every stream of a lane) and the absence of pacing, and both need
the phase budget on this cell rather than another guess.

Two properties the change keeps, because they were the rewind's excuse: a repair
is *ordered* only by offset in the buffer, never against fresh data (the receiver
reassembles by offset, so both may be in flight), and the probe path that
re-sends a whole silent flight now queues those ranges the same way instead of
moving the cursor past them.

**The loss cell's bound is the tool, and the phase budget says where
(2026-11-01).** Three runs on the same cell (`bulk-n`, `loss1`, mhd forced,
`dev`, two rounds) change what the next work item is:

- **The path is not the limit.** `--arms control,l4` on that cell: the tool-free
  arm carries **60.81 % more** than `l4` (12.1 Gbit/s against about 30.8), so
  there is headroom above this carrier and the cell is a tool question.
- **The sender is not waiting on the window.** 4 346 `mhd stalled` lines in one
  30-second arm, and every one reads `unsent_ms=0 unacked_ms=0
  credit_blocked=false` with a single stream's receive backlog at 1.0-1.1 MB.
  Nothing is queued behind the window: the stream's send buffer is empty because
  its *writer* has not produced. That is the copy pipeline, not the congestion
  window — so the shared flight and the missing pacer are **not** what limits this
  cell either.
- **The phase budget, one round of 81.9 Gbit carried** (`bench phases`): the
  server side costs about 0.32 s/Gbit and the client 0.17, against the cell's
  0.59. Server, largest first: `seal` 0.0844 (22.83 µs per record), `tx_build`
  0.0652 (**31.91 µs per pass** for 2.7 packets), `tx_call` 0.0671 (0.75 µs per
  packet over 462 050 packets), `tx_send` 0.0442, `ack` 0.0221 (**13.26 µs per
  acknowledgement** against the client's 0.08 µs on the same code), `io_write`
  0.0193.

**Batching that run is refuted by the phase number it was aimed at.** The change
was built and measured: `Conn::poll_run` filled the whole run under one lock and
one clock read. It does what it says — the server's `tx_build` fell from **0.0652
to 0.0516/0.0544 s/Gbit** (31.9 → 25.2/26.6 µs per pass) and `tx_call` from
0.0671 to 0.0571/0.0594, about **0.024 s/Gbit of timed work removed** — and it
does not survive into the metric:

| arm | build | `throughput_gbps` | `cpu_s_per_gbit` |
|---|---|---|---|
| `l4` | batched run | 12.076 [11.932..12.164] | 0.5784 [0.5748..0.5820] |
| `l4~aa` | the same build again | 12.218 [12.043..12.363] | 0.5753 [0.5511..0.5806] |
| `l4~b` | before it (`70cb419`) | 12.326 [11.662..12.425] | **0.5670** [0.5607..0.5939] |

Floor 1.31 % on throughput and about 2 % on CPU; every difference is inside them,
and the point estimates lean the wrong way. The mechanism is the same lock:
holding it for a whole run is holding it against the dispatcher, which applies the
peer's acknowledgements — the 0.024 s/Gbit saved on the build side is paid back as
contention on the receive side, where no phase charges it. **Reverted, not
landed** (`target/evidence/bench-runbatch-loss1.json`,
`target/evidence/phases-batch-work`). The lesson is the round's: a phase-budget
win is a hypothesis about the total, not a result, and the lock the sender saves
is the lock the receiver needs.

What is left by size is therefore not the run boundary: `seal` 0.0844 s/Gbit (the
AEAD at 0.7 ns per byte, which is the cipher's own rate), `tx_send` 0.044, and the
server's `ack` at **13.3 µs per acknowledgement against the client's 0.08 µs on
the same code** — a 165x asymmetry that is state, not code, and the one number
here nobody has explained. It is the next thing to measure rather than the next
thing to change.

**The latency axis has one measured number, and it is the whole story so far
(2026-11-01).** The model's `capacity` ramp on mhd, one bulk stream (L1): the
interactive probe's **p50 1.9-2.1 ms and p99 2 704 / 5 002 / 5 002 ms** across
three runs, with the ramp never reaching a second level it could pass — so
`capacity_streams` reads **0**. The same ramp on the TCP carrier holds
`capacity_streams` **6 [4..8]** (its own A/A twin 2 [1..4], so the instrument is
wide, but the direction is not in doubt: TCP is at single-digit milliseconds
where mhd is at seconds). **The trigger is bulk, and that is measured rather than
assumed**: `rr-16` without a bulk stream reads p99 **1.37-1.41 ms** over 48 000
samples at 29 500 round trips/s, on the same binary and host.

Two instruments then excluded the two obvious causes:

| instrument | reading | what it excludes |
|---|---|---|
| `Conn::budget` at the moment a slow open completes (11 samples) | `credit=16 777 112`, `window=13 490` (a whole initial cwnd, so **nothing in flight**), `buffer=33 554 432`, `queued_streams=0` | the open is **not** waiting on a write budget |
| the server's own opening split (selector / handshake / hello / ack), every open of one ramp | **0** openings past 100 ms | the stall is **not** the server's per-stream work |

So the seconds are spent getting a small message *delivered* while bulk
saturates the lane, in a direction neither instrument covers yet. The reserve
(`OPEN_RESERVE`) is in the tree as a **guarantee** — a stream that opens behind
bulk can always write ten kilobytes, with a test that fails without it ("the bulk
stream stops a reserve short of the credit, left 0") — but it is **not** a
latency win: with it, p99 still read 10.8, 5 002, 5 002 and 450 ms across four
cells. Stated plainly: mechanism pinned, cause not yet found, no claim.

The next instrument has to timestamp **one opening's packets on both ends** — the
dialer's write of the handshake, the server's receipt of it, the server's reply
and the dialer's receipt — because each end currently measures only its own
awaits and each says it was fast.

**And the tail is not per-connection at all, which the pool's own counters show
(2026-11-01).** With `MOLEHILL_POOL_STATS=1` on the same ramp, the pool holds four
tunnels with one stream each — `1/0/0, 1/0/0, 1/0/0, 1/1/0` — and the datagram
counts say which one carries the bulk transfer: `mhd_health` reads 96-129
datagrams on three of them and **67 909** on the fourth. Least-loaded placement
put every probe channel on a *different* connection from the bulk stream, so the
two share no credit, no congestion window and no send buffer. The tail is still
there (p99 **2 563 ms**, p50 2.26 ms, same run). So the seconds are **not** the
carrier's per-connection budgets — my own reserve, and the shared-flight
hypothesis before it, were both aimed at a resource the two streams do not share.

What is left is process-wide: the busy lane's datagram rate against a lock the
accept path needs. The two candidates are the listener's connection table
(`registry.lock()`, taken once per datagram by the dispatcher) and each
connection's own mutex, which `wait_stream`/`accept` needs to hand a new stream to
its task — the dispatcher holds that mutex per datagram, and the accept happens
*before* the server's per-stream instrumentation starts, which is exactly why the
server measures a fast opening while the dialer waits seconds. That is the next
thing to measure, and the fix it points at is structural: hand the stream to its
task from inside the critical section the dispatcher already holds, instead of
making the accept task contend for it.

**Two defects were then separated, and only one of them is the timer
(2026-11-01).** The dialer's opening was split into its own phases
(`hs_us` the Noise handshake, `ack_us` hello-plus-acknowledgement), and the
split immediately named a cluster: **335-337 ms**, which is `INITIAL_RTO` to the
millisecond, on lanes that had never carried a round-trip sample. The server's
counterpart — the wait for a stream's first byte, captured inside
`transmit_stream` — read **752 ms**, i.e. that timer twice.

That is a different defect from the one the earlier entry named, and it has a
different fix: `INITIAL_RTO` protects a *wide* flight (re-sending a thousand
packets costs a thousand duplicates), and a stream opening puts one or two packets
in the air, where a duplicate is one datagram the peer drops by offset. So a
sample-less small flight now arms **20 ms** instead, with the exponential backoff
bounding duplicates on a long path. Measured on the dialer:

| | opens over 100 ms | p90 | max |
|---|---|---|---|
| `SMALL_FLIGHT_INITIAL_RTO` 333 ms | 5 of 58 | 337 543 µs | 5 645 589 µs |
| 20 ms | **1 of 118** | **6 664 µs** | 2 788 322 µs |

An interleaved A/B of two builds differing only in that constant, three rounds
with the A/A twin (`target/evidence/bench-fastopen.json`), reads p99 **-99.28 %**
for the fast build — but the verdict engine refuses it as a claim (n=[2,2], p 1.0,
and the ramp's own `capacity_streams` difference is flagged fragile), which is the
honest reading of a cell this noisy.

**The `capacity` ramp's p99 is still pinned by a wall neither build moves, and it
is the next target.** In that A/B, `l4` (20 ms) read 21.3, 22.5, 25.8 and 10.4 ms
at L1 while **`l4~aa` — a second copy of the same build — read 5 004 and 5 002**,
and `l4~b` (333 ms) read 4 860 and 5 003. So the 5-second term is arm-independent
and it dominates the percentile; the timer work is real and invisible to that
metric until the wall is gone. Exactly 5 000 ms is `PAIR_WAIT_BUDGET`
(`core/server.rs`), so the first hypothesis is a visitor's channel request that
goes unanswered for one budget and is then served by the retry — a defect with no
log line until now, which this round added (`pairing budget expired with no data
channel`). The run that followed it counted **0** expiries and produced no 5 000 ms
sample, so the wall is intermittent and the count has to be taken on a run that
hits it; that is the next measurement, and it is instrumentation the tree now has.

**Confirmed: the 5-second wall is exactly one expired pairing budget
(2026-11-01).** Four consecutive runs of the same cell with the counter in place:

| run | expired budgets | p99 at L1 and L2 |
|---|---|---|
| 1 | **0** | 776, 1 815 ms |
| 2 | **1** | 4 173, **5 002** |
| 3 | **1** | **5 003**, 240 ms |
| 4 | **1** | **5 003**, 22, 642 ms |

One expiry, one five-second sample, in every run — and the run without an expiry
has no wall at all. So a visitor's `CreateDataChannelFor` goes unanswered for a
whole `PAIR_WAIT_BUDGET` and the retry is served at once. That is the defect the
`capacity` cell is mostly measuring, and it is a *product* lock-up, not a
statistical tail.

What is excluded about it: the client's own open is fast (p90 6.7 ms after the
timer fix), the server's per-stream opening is fast (selector, handshake and hello
sum to under 100 ms in every sample), the pool is not at its ceiling (0 saturation
lines, at most 5 streams of a 56 ceiling), and the pairing queue does not stall (0
`waited for the pairing queue` lines in three further runs, which also produced no
wall and p99 1.4-2.2 s). So the missing link is between "the server asked" and
"the client's stream arrived at the pairing queue", and the instrument for that
leg now exists.

Note what the last column also says: with the wall gone the cell still reads
1.4-2.2 s, so there is a second term under it. It is not the same defect and it is
not yet named.

**Bracketed: the command leaves the server and never reaches the client's loop
(2026-11-01).** Both ends now log the request and its receipt, so one losing
incident is a two-line comparison:

```
server  asking the client for data channels group=46 stripes=1 send_us=0
client  (nothing for 46)
server  asking the client for data channels group=47 ...   [50 ms later]
client  client asked for a stripe group=0000002f ...       [102 us after ITS ask]
```

Group 46's request was queued by the server with **`send_us=0`** — the unbounded
`sink.send` neither blocks nor drops — and the client's command loop never entered
that arm for it, while the very next request crossed in **102 µs**. No
`Unknown control channel command tag` and no `Failed to read cmd` appear in
either log, so the frame was not misparsed into a different command: it was not
read at all.

Two candidate causes are already excluded by measurement:

| hypothesis | instrument | reading |
|---|---|---|
| the client's spawned open was starved of a worker | expired budgets at `TOKIO_WORKER_THREADS=4` vs `16`, three runs each | **1 vs 4** — more threads make it *worse*, which is contention, not starvation |
| the carrier's blocking connection mutex (`parking_lot`, so a waiter parks an OS thread) | acquisition wait, same runs | **0 waits over 100 ms** in every run, including the one that expired |

The control reader had a defect worth fixing on its own account, and it is now
fixed: `read_control_cmd` read the tag byte into a **local** array and then the
body with a second `read_exact`, while the client's command loop creates that
future fresh inside `tokio::select!` — so whenever the other branch won, the
future was dropped between the two reads and the tag byte it had already consumed
was gone. The frame now lives in the caller's state (`ControlFrame`) and a dropped
future resumes it. **The test reproduces the desync exactly**: with the old
shape, a frame read across a cancellation comes back as `CreateDataChannel`
because the body's first byte (0) was read as a tag.

**And it is not this stall.** Five runs of the same cell with the fix in place
still expire the budget 0-2 times each, with the same 5 002/5 003 ms samples:

| run | expired budgets | p99 |
|---|---|---|
| 1 | 2 | 5 002, 5 003 |
| 2 | 1 | 9, 5 003 |
| 3 | 1 | 5 003, 11, 2 230 |
| 4 | **0** | 3 023, 20, 507 |
| 5 | 1 | 18, 2 947 |

Run 4 is the one to keep in mind: **no expired budget and still 3 s**, which is
the second term this page already noted.

**The two terms are one defect, and it is the pairing wait (2026-11-01).** The
missing instrument was the *successful but slow* pairing: the expiry line above
only fires when a whole budget is spent, so every wait that came in under it
was invisible. Logging the measured duration instead:

| run | expired budgets | slowest pairings (µs) | p99 |
|---|---|---|---|
| 1 | 2 | 5 001 910, 5 001 689, 559 982 | 5 003, 5 003 |
| 2 | **0** | 967 552, 819 933, 293 140 | 1 657, 23, 294 |
| 3 | **0** | 3 512 305, 1 363 876, 928 606 | 1 365, 3 513 |

So the 5-second term is a pairing that spent one or more budgets, and the
1-3 second term is the *same* wait arriving under one. There is one continuous
distribution from tens of milliseconds to five seconds, not two defects, and the
`capacity` cell's entire p99 is the time a visitor waits for its data channel.

That also revises the previous entry's conclusion: the control-read desync was
worth fixing on its own account (and is fixed, with a test that reproduces it),
but it was not "neither of two stalls" — it was not part of a single stall whose
whole population is this one.

**Every stage of that wait is now instrumented, and every instrument is silent
(2026-11-01).** Two more counters closed the remaining gaps — the delay between
spawning the client's open task and its first poll, and the time a control frame
spends in the session writer's queue (frames carry their queueing instant) — and
then three runs of the cell read:

| run | client's spawn waits | server's writer-queue waits | slowest pairings (µs) |
|---|---|---|---|
| 1 | none | none | 5 001 434, 3 249 175, 547 792 |
| 2 | none | none | 5 002 054, 672 523, 491 149 |
| 3 | none | none | 3 828 623, 2 866 764, 463 557 |

So the writer picked every frame up immediately, the client's open task started
immediately, and the visitor still waited seconds. Nine stages are excluded by
their own numbers: the client's open (p90 6.7 ms), the server's per-stream opening
(selector, handshake and hello all under 100 ms), the pool's ceiling, the pairing
queue, the carrier's blocking connection mutex, runtime-worker count (more is
worse), the control-read desync (fixed), the spawn delay, and now the writer's
queue.

**Correction: no frame is lost, and the wait is the client's own open
(2026-11-01).** The group id rides the wire, so the two logs answer the question
without a new instrument: in six arm-rounds the server's `asking the client for
data channels` and the client's `client asked for a stripe` counted **54/54,
51/52, 86/86, 56/56, 217/419, 51/51**. Every request that was asked for was
received, so the earlier "written and not read" reading was wrong — the two lines
it compared were 50 ms apart in a stream of one request per visitor, and one lost
line was a coincidence of that cadence.

Timing the client from the start of its open to the prologue settles where the
wait is, because it matches the server's pairing wait to within 120-600 µs:

| run | client open→prologue (µs) | server pairing wait (µs) |
|---|---|---|
| 1 | 3 212 752 / 162 869 | 3 212 872 / 163 091 |
| 2 | 3 641 981 / 2 283 435 | 3 642 105 / 2 284 062 |
| 3 | 6 643 947 / 2 224 649 | 5 001 552 (budget) / 2 225 049 |

So the visitor's entire wait — and therefore the whole `capacity` cell — is the
client's *stream open*, and the wire plus the server's routing add a few hundred
microseconds on top. The pool's own placement timer agrees: its `max_us` reads
1.8-2.6 ms normally and **3 918 256 µs** in the interval that had one slow open,
matching the client's 3 918 263 µs to seven digits.

**And the open's slow half is the Noise handshake, not the acknowledgement.**
Splitting the slowest opens:

```
us=5 266 433   lane=1   hs_us=5 265 951   ack_us=481
us=   23 531   lane=0   hs_us=   23 433   ack_us= 98
```

The handshake is three round trips over one mhd stream, so a 5.27-second value is
not a queue: it is **20 + 40 + 80 + 160 + 320 + 640 + 1 280 + 2 560 = 5 100 ms**,
the exponential backoff after eight unanswered timers, which is what
`MIN_RTO_SMALL_FLIGHT` climbs through since this round's earlier change. The
**One retransmit, not eight — so the ladder was the wrong suspect, and capping it
proves it (2026-11-01).** The per-stream retransmit count settles the arithmetic:

| run | slow opens (µs) | retransmits recorded |
|---|---|---|
| 1 | 3 827 595 / 2 684 117 / 969 407 | **1 / 1 / 1** |
| 3 | 3 373 875 / 2 283 357 / 1 644 988 | **1 / 1 / 1** |

A stream that opened in **3.8 seconds** had its ranges declared lost **once**, so
the eight-doublings reading above was wrong, and with it the idea that the sum of
the backoff steps is the mechanism.

The cap that reading suggested was still worth making, and is in: a small flight's
ladder now stops at the wide timer, because a cheap first step must not extend how
long the ladder runs (20, 40, … 2 560 ms sums past five seconds where the flat
333 ms it replaced summed to three). **It does not fix the stall** — three runs
with the cap still read opens of 7 003 819, 4 857 053 and 4 262 882 µs — so the
ladder is excluded as well. What is left is that the probes are not getting
through at all, for seconds.

**Sharing a lane is what costs, and it is now measured as a variable
(2026-11-01).** `pool_cap` writes `[client.data].tunnels`, so the same cell can be
run with the handshake on four lanes and with every stream on one:

| `tunnels` | slow opens | slowest (µs) | p99 |
|---|---|---|---|
| 4 (the default) | 7 | 885 300, 426 843 | 431, 887 |
| 1 | 4 | **3 174 945, 2 939 585** | **2 941, 2 796** |

Four to seven times worse when the handshake has to share a lane with a bulk
stream. That is the first *positive* evidence about this defect's mechanism —
every earlier measurement excluded a candidate, this one names a variable — and it
makes the plan's own fix the justified next build: **a small stream's bytes must
not queue behind a bulk stream's.** The reserve already in the tree is not enough
on its own, and the reason is visible in the numbers above: when the lane is idle
the write budgets are not binding at all (`window` reads one initial cwnd,
`credit` is full, `queued_streams` is 0), so what a shared lane costs is not the
budget but the *turn* — the pump's run, the socket's queue, and the sender's
starvation, none of which a per-write reservation touches.

**And the first attempt at that fix is refuted (2026-11-01).** The reading was that
a small stream's packet must enter the socket *before* a bulk stream's, since a
shared lane means one shared socket; so `next_stream_range` served an opening
stream ahead of the round-robin walk, tracked by a `BTreeSet` of streams that had
written less than `OPEN_RESERVE` (a set rather than a scan, because this is read
once per packet). Two rounds per lane count, against the same cell:

| `tunnels` | slowest opens (µs) | p99 |
|---|---|---|
| 4 | 6 259 040, 6 056 186 | 5 003, 11, 185, 5 003 |
| 1 | 5 014 086, 5 002 959 | 15, 28, 2 276 |

That is not a win: the four-lane tail is worse than the 885 ms it started from,
and the one-lane p99 moved the right way only inside this host's scatter for a
cell this short. Reverted, not landed — the run is `target/evidence/prio{4,1}.json`.

So the "turn" is not the round-robin order either.

**And it is not the socket (2026-11-01).** The pump's `send_run` waits on
`socket.writable()` before each chunk it posts, which was the last uncovered wait
in that loop; instrumented (gated on the phase switch, because the probe costs a
`try_io`), two runs read:

| run | socket waits ≥ 100 ms | slowest opens (µs) |
|---|---|---|
| 1 | **0** | 3 996 381, 701 095 |
| 2 | **0** | 2 096 250, 1 896 885 |

Four seconds of open with the pump never blocked on the socket. Which is the
conclusion worth writing down plainly: **every instrumented stage of this open is
fast while the open is slow**, and each of them measures work *inside* a stage.
The interval that is left is between stages — the time between a stream's
`poll_write` returning `Pending` and that task being polled again — and no
per-stage timer can see it, which is why nine of them in a row have read "fast".

**Built, and it names the wait: a read park of seven seconds (2026-11-01).**
`StreamIo` now stamps both directions — the instant a `poll_write` or `poll_read`
answers `Pending`, and the interval charged on the poll that resumes it. Two runs
of the cell:

| run | write parks ≥ 20 ms | read parks ≥ 20 ms | longest read parks (µs) | slowest opens (µs) |
|---|---|---|---|---|
| 1 | **0** | 66 | **7 004 863**, 7 004 604, 7 003 393, 7 001 846 | 3 451 921, 1 361 265 |
| 2 | **0** | 82 | **7 006 343**, 7 005 816, 7 004 043, 7 003 488 | 5 367 516, 1 489 641 |

So the wait that nine per-stage timers could not see is a **parked reader**, and
the writer never parks at all. Two things follow from the shape of those numbers.

- The clusters are at **7.0 seconds to within a few milliseconds**, across runs
  *and* across streams. A queue produces a spread; a single value repeated to four
  digits is a **cadence** — a wake-up that arrives on somebody else's timer rather
  than when the data did.
- Nothing in the carrier has a 7-second period. What it does have is a 2-second
  keepalive, so 7 s is the shape of "woken by the pump three or four times before
  the bytes it is waiting for are found" — which is the signature of `wake_readers`
  firing before there is data to read, with the reader's waker already taken by
  that wake (`Wakers::readers` is emptied by `mem::take` on every wake, so a second
  wake with nothing new to read leaves the reader parked with no waker at all).

**The lost-wake-up reading is refuted, and the fault is on the sending side
(2026-11-01).** The discriminator is the age of the *data* rather than of the
wake-up: the receive buffer stamps the instant bytes first became readable at the
read offset, and the resumed poll reports that stamp beside the park. Two runs:

| run | parked (µs) | bytes had been ready for |
|---|---|---|
| 1 | 7 003 911 / 7 003 708 / 7 001 715 / 7 001 549 | **16 / 20 / 6 / 5 µs** |
| 2 | 7 003 801 / 7 003 454 / 7 001 574 / 7 001 431 | **14 / 27 / 9 / 8 µs** |

Seven seconds of waiting for data that had existed for **sixteen microseconds**.
So the wake-up was not late — it fired when the bytes arrived — and the bytes
themselves arrived seven seconds after the reader began waiting. The first
instrument got this wrong in a way worth recording: it read the stamp *after*
`read_into` had already cleared it, so every sample said `u64::MAX`; the stamp is
now read inside the same critical section, before the read.

Since the parked reader is the *client* and the bytes it waits for are the
server's, the fault is in the server's send path — and 7.0 s repeated to four
digits across runs and streams is still a cadence, not a distribution. The
server's own opening instrument brackets its work (it wrote the acknowledgement
inside 100 ms in the samples it caught), so what is left is the server's *pump*:
a stream write that succeeds and calls `wake_pump`, whose bytes then wait seven
seconds for a pass. That is where the next instrument goes, and it is the same
`wake_pump`/`wait` pair on the accepted-lane path (`spawn_sender`) rather than the
dialed one.

**Three of my own instruments were wrong, and they are corrected
(2026-11-01).** Working the server's send path turned up errors in the tools
rather than in the carrier, and each is worth recording because each cost rounds:

1. **Half the instruments are gated on `MOLEHILL_MHD_STATS`, and several
   measurements were taken without it.** The "first stream byte waited for the
   socket", "pump built nothing", "pump waited on the socket" and every `tx_*`
   phase line are silent unless the switch is on. Re-running the same cells with
   it set is what made the third item below visible at all. Any negative that
   names one of those lines has to say the switch was on.
2. **"pump built nothing while bytes were queued" counted streams the scheduler
   may never serve.** A stream the peer reset keeps its queued bytes and is
   skipped by design, so the count included it and produced
   `queued=1 … reason="no eligible stream"` — a line that contradicts itself and
   sent three rounds after an artifact. The count now applies the scheduler's own
   eligibility test.
3. **And the alarm still lied, for a different reason: its counts were sampled
   after the fact.** The pump asked for the snapshot *after* `flush` returned,
   which is a second lock acquisition — so a writer landing in between made the
   counts describe a later instant than the reason sitting beside them. The
   reporter now logs only the snapshot taken **at the refusal**, in the same
   critical section as the walk that produced it. With that, the alarm is silent
   on a cell whose slow opens still read 4 219 347 and 1 830 857 µs: the alarm was
   never the defect.

A stream owing bytes is found by the walk whatever the cursor says — that
invariant is now pinned by a unit test (`a_stream_owing_bytes_is_found_whatever_the_cursor_says`),
including the state the bench logged: a cursor past every live id, which is what
a retired stream leaves behind.

**The buffers are not it either, so the seven seconds is between the two sockets
(2026-11-01).** The dwell counter — stamped when a write makes a stream non-empty,
charged when a packet carries its oldest owed byte — reads, over two runs, with
both ends logging:

| run | server dwells | longest (µs) | client dwells |
|---|---|---|---|
| 1 | 7 | 23 957, 23 604 | **0** |
| 2 | 7 | 32 762, 25 323 | **0** |

So a byte enters a send buffer and leaves it within tens of milliseconds on both
ends, and the client's buffer never holds anything at all. Put beside the rest,
the picture is now complete enough to say where the time is *not*:

- the client asks for a channel and the server asks for it in microseconds;
- both ends' stream opens, handshakes and acknowledgements complete inside 100 ms;
- neither end parks a writer, stalls a socket, or holds a byte in a buffer;
- the client's reader waits seven seconds, and the bytes appear in its receive
  buffer **16 microseconds** before it reads them.

Everything in userspace is fast, and the packet spends its seven seconds between
the server's socket and the client's receive path.

**Two more candidates are out, and this is where the honest list stops
(2026-11-01).** The qdisc's queue depth was the obvious kernel-side suspect, so the
cell was run at `txqueuelen` 1000 and 10 000 — a deeper queue must make a queueing
tail *worse*:

| `txqueuelen` | p99 (ms) | `dropped_packets` |
|---|---|---|
| 1000 | 3 388, 311, 22, 229 | 0, 0, 0, 0 |
| 10 000 | 688, 1 903, 3 537 | 0, 0, 0 |

Same tail, and **nothing dropped anywhere in the topology**. Nor is a lane being
torn down and replaced under the wait: `lane declared dead`, `mhd connection
closed` and `reaped dead tunnels` are all zero on both ends across three runs, so
the 5-second `DEAD_FLOOR` theory is out as well.

The exclusion list for the `capacity` cell's tail is therefore complete for
everything the process can see: request delivery, both ends' stream opens,
handshakes and acknowledgements, write and read parks, socket writability, the
send buffer's dwell, the receive buffer's readiness, worker count, lock
contention, scheduling order, lane sharing (measured as a 4-7x factor), the pool's
ceiling, the pairing queue, qdisc depth, and drops on every device and qdisc the
model counts.

**And the kernel's socket queues are empty too (2026-11-01).** The first reading
from outside the process: `/proc/net/udp` in both bench namespaces, sampled every
100 ms through a whole cell — 984 samples — for `tx_queue` (bytes the sender holds
unsent) and `rx_queue` (bytes the receiver holds unread):

| reading | deepest seen |
|---|---|
| `tx_queue`, server namespace | **960 bytes** (one datagram) |
| `rx_queue`, server namespace | **2 880 bytes** (two datagrams) |

So a saturating UDP sender never has more than one datagram waiting in its socket,
and the receiver never holds more than two. There is no socket backlog anywhere,
which is the same conclusion the qdisc gave (`dropped_packets` 0 at both queue
depths) from a different direction.

That is twelve rounds on one defect, and it stops here by the repository's own
rule (AGENTS.md §9: three failed attempts, then write the findings down and ask).
The socket-queue reading above was the first of the outside-the-process family, and
it closes that family too: one datagram in flight at the sender, two at the
receiver, nothing dropped, no qdisc backlog. **Every stage, every buffer and every
kernel queue this host exposes is measured and clear, and the wait is still seven
seconds** — which no longer points at a place to instrument next; it points at the
measurement itself. The one thing not yet questioned is whether the seven seconds
belongs to the visitor whose open is slow at all: the park is per stream, the
`paired_us` is per visitor, and the two were correlated by magnitude rather than by
identity.

That is the decision this needs: either correlate one slow open end-to-end by
identity (a correlation id through the request, the open, the handshake and the
park), or set the tail aside and spend the remaining effort on the work packages
this defect has been crowding out. **The second is what this page recommends**, and
the first is worth one round if the tail is the priority.

**The transparent path is green after all of this (2026-11-01).** `just l3-accept`
on the current tree: **16 of 16 PASS**, including the sixteen-concurrent-flow arm
and both lane-lending arms — the two that have flaked on this host for the whole
cycle. So the timer's small-flight floor, the per-stream repair queue, the
backoff cap and the four instrumentation rounds did not disturb L3: byte-exact
echo, the visitor's real address preserved, no server-side flow state, 200 KB
streamed through a 65 KB device, 2000 small round trips exact, the policy refusal
preceding any device access, and the valve naming its cap.

This is the first evidence in the cycle that the carrier changes are safe *outside*
the L4 cost cell, and it matters because the L3 path is the one the maintainer's
own deployment waits on.

**The soak profile produces nothing on this host, and that is a measurement
failure rather than a carrier one (2026-11-01).** `--profile soak` on mhd, one
round, 683 s: the first stage (`clean`) passed and **every stage after it failed**
with the same typed reason — `iperf3 error: control socket has closed
unexpectedly` — on `loss1`, `rtt100`, `loss5` and `clean#2`, leaving no metric for
any shaped stage. Host load was 0.55 at the start, so this is not the load
correlation recorded earlier.

**The answer is in, and it is the carrier (2026-11-01).** The same profile, one
round, with `MOLEHILL_DATA_CARRIER=tcp`:

| stage | mhd | tcp |
|---|---|---|
| `clean` | ok, **p99 1 247.2 ms** | ok, **p99 48.6 ms** |
| `loss1` | **failed** — control socket closed | ok, p99 1 059.3 ms |
| `rtt100` | **failed** | ok, p99 401.2 ms |
| `loss5` | **failed** | ok, p99 2 452.2 ms |
| `clean#2` | **failed** | ok, p99 50.7 ms |

TCP carries every stage of the same topology on the same host minutes apart, so
the wedged-server explanation is wrong and the profile is usable. Two things
follow, and the second is the one that changes the plan:

- **The drift axis cannot be taken on `mhd` until this is fixed**: four of five
  stages produce no metric, so there is nothing to slope.
- **The tail is not a `capacity`-cell artifact.** The soak's own `clean` stage
  reads **1 247 ms** on mhd against TCP's **48.6 ms** — twenty-five times — in a
  workload with a different shape, a different load and a different stage
  schedule. And the stages that fail do so by *killing a control connection*,
  which is the same signature as the `bulk-n` `loss1` failures recorded earlier
  in this cycle.

**Correction, one round later: the soak failure is pool exhaustion, not the tail
(2026-11-01).** The daemons' own logs name it, forty-six seconds in:

```
06:18:38 INFO pool-stats: pool at its stream ceiling; raise [client.data.mhd].tunnels
                   pool=session:10.10.0.254:2400 carrier="mhd" count=4 ceiling=56
06:18:38 INFO the pool refused a data channel: every tunnel is at its stream ceiling
```

So the pool reached `4 × 56 = 224` streams and refused the next data channel, which
killed the visitor that needed it — i.e. the `iperf3` control connection of the
stage that was running. Nothing about the latency tail is involved, and the
attribution in the paragraph above was wrong.

**That is itself the finding, and a more tractable one**: a soak that merely holds
connections for minutes exhausts the pool on `mhd` in under a minute while TCP
carries the same workload for seventeen. Either the workload genuinely wants more
than 224 concurrent streams — in which case the profile is misconfigured for the
default `tunnels = 4` and says so honestly — or **`mhd` streams are not being
retired**, so the count only ever climbs. The second is a leak, it is the family
`pool_test` exists for (`a saturated pool still serves the next visitor`, the
"give the window back" fix), and it would explain a great deal besides: a pool
pinned at its ceiling answers every later open with a 250 ms capacity wait and then
a refusal, which is exactly the shape of a latency tail that no per-stream counter
can see.

**The count is not a leak: it is the workload (2026-11-01).** The same soak with
`MOLEHILL_POOL_STATS=1`, live stream count against the clock:

| sample | 1 | 20 | 40 | 60 | 80 | 100 | 120 | last |
|---|---|---|---|---|---|---|---|---|
| `streams` | 0 | 23 | 27 | 61 | 69 | 103 | **23** | **224** |

It climbs, **falls back to 23** and climbs again to the 224 ceiling, with two
refusals logged. So retirement works — a monotonic leak would never fall — and what
happens is that this profile's own burst is wider than `4 x 56` streams.

Which leaves a finding that must not be papered over: **the soak profile was never
verified on `mhd` at the default `tunnels = 4`, and it does not fit.** TCP carries
the same profile at the same width, so either its streams retire faster under this
shape or its burst stays under the ceiling; either way the honest statement is that
the drift axis cannot be taken on `mhd` at the default width until someone decides
between two things: raise `[client.data].tunnels` for this workload (which the
configuration page already prices at 0.5-0.8 MiB per lane per end), or find out why
`mhd` needs more stream slots per carried connection than TCP does for the same
workload. **Tuning the profile until it passes would hide the second question**, and
the repository's measurement rules forbid that — a gate is not made green by
changing the thing it measures.

**And widening the pool does not help, which changes the diagnosis again
(2026-11-01).** The same soak at `pool_cap = 8` — 448 slots instead of 224 —
fails exactly the same four stages with the same reason, and **still logs two
"pool at its stream ceiling" refusals**:

| `tunnels` | slots | ceiling refusals | stages failed |
|---|---|---|---|
| 4 | 224 | 2 | 4 of 5 |
| 8 | 448 | **2** | **4 of 5** |

A workload that is merely *wide* would have fitted in 448. A count that reaches
224 and then 448 is a count that grows with the space given to it, so the ceiling
is what stops it rather than the workload — the signature of **leases held rather
than released**. The dip to 23 in the earlier table shows release happens *in
bulk* when a stage boundary closes everything, which is what a leak looks like
when a burst ends: it falls back only when something shuts the doors.

**The held-time instrument answers it, and it reunifies the defect (2026-11-01).**
Each lease now records its whole life, and the pool's line reports what came back
since the last window with its mean and worst hold. Through one soak:

| phase | `streams` | returned in window | mean hold | worst hold |
|---|---|---|---|---|
| early | 18 → 49 → 67 | 41 → 330 → 572 | 200-430 ms | 425-755 ms |
| at the ceiling | **224** | **0, 0, 0, 0, 0 …** | — | — |
| after it broke | 174 | 192 | **2 094 ms** | **5 024 ms** |

So the slots are not leaked and the workload is not merely wide: at the ceiling
**nothing is returned for tens of seconds at all**, and when the pool finally
breaks free the returned slots had been held for **two seconds on average and five
at worst**. Existing forwarding channels stopped finishing — that is the same
family as the seven-second open, and it now explains the soak failure, the
`capacity` cell's tail and the earlier loss-cell failure as one thing:

> some `mhd` streams stop making progress for seconds and nothing recovers them;
> the pool fills with them, refuses the next data channel, and the visitor that
> needed it — `iperf3`'s control connection — dies.

That is a single mechanism spanning three separate cell families, and it is worth
more than any width or any per-stage timer.

**Reading those three on a stall named the chain, and one link is now fixed
(2026-11-01).** The stall line gained the stream that is holding the wire back and
its own facts, and the soak then reads, on the same instant:

```
server  unsent_ms=208 credit_blocked=true  worst_stream=5  unsent=16 073 916  retransmits=0
client  unsent_ms=0   credit_blocked=false backlog_worst_stream=17 backlog_bytes=1 060 839
```

So the server cannot send because **its peer's credit is spent**, and the client is
holding **a megabyte unread** on one stream with nothing of its own queued. Nothing
is lost (`retransmits=0` on both), nothing is over the wire — the client's reader
simply is not consuming, so its advertised credit never moves.

The pump woke readers only when a receive batch landed, so a reader whose wake-up
arrived while it was between polls waited for the pump's timer. Readers are now
woken on every pass — a `mem::take` of an empty list when there is nothing to
wake — and the effect on that profile is categorical:

| | before | after |
|---|---|---|
| peak live streams | **224** (its ceiling) | **17** |
| ceiling refusals | **2** | **0** |

**What it does not fix**: the four stage failures are still there, and the `clean`
stage still reads p99 1 801 ms (from 3 892 ms, so it moved, but a tail of seconds
remains). So the pool-filling mechanism is closed and the latency that fed it is
not. That is a real narrowing and it is the first change in this cycle that made a
measured failure category disappear.

Confirmed on the headline cell, with a control binary differing only in that one
line (`--ab-arm`, `--aa`, three rounds, `target/evidence/bench-wake.json`): the
per-cell p99 readings at L1 are `18, 24, 5 003, 23` for the fixed build, `16,
5 002, 994, 5 002` for the control — and **`4 632, 15, 2 771, 2 279` for the A/A
twin**, which is the same build measured twice. So the tail is in both arms and in
the twin, the run makes no claim, and `capacity_streams` reads 1 [0..1] against the
control's 0 [0..0] as a fragile directional reading only.

Two things are therefore settled and they should not be conflated again: the pump's
wake-up **was** delaying readers (224 → 17 peak streams is categorical), and the
multi-second tail on an open **is not** that wake-up. The next instrument is the
one the chain implies — a timestamped waker, so that "woken at T, polled at T+Δ"
is measurable at all, because after this fix the only place left for a parked
reader to lose time is between the wake and the poll.

**Where the cycle stands after these fixes, measured rather than asserted.** Three
changes are in the tree and each was verified against the thing it targeted:

| change | committed | evidence |
|---|---|---|
| a lost range is re-sent as itself, not as the rest of the stream | `b728cac` | the harness: 12 000 bytes re-sent before, 1 349 after |
| the retransmission floor is scoped to the flight, and a small one's ladder stops at the wide timer | `c22c504`, `4988786` | the dialer: opens over 100 ms fell from 5 of 58 to 1 of 118, p90 337 ms → 6.7 ms |
| readers are woken on every pump pass | `c04e939` | the soak: peak live streams 224 → 17, ceiling refusals 2 → 0 |

And the cell those first two were aimed at, re-taken today on this host
(`bulk-n`, `loss1`, three rounds with the A/A twin,
`target/evidence/loss1-after-fixes.json`):

| arm | `throughput_gbps` | `cpu_s_per_gbit` | failures |
|---|---|---|---|
| `l4` | **11.780** [10.644..12.081] | 0.5890 [0.5766..0.5924] | **0 of 8** |
| `l4~aa` | 12.003 [11.958..12.251] | 0.5752 | 0 of 8 |

So that cell now carries about twelve gigabits per second under one percent loss
with **no failed sample**; the October record's "mhd 9.3-10.4 Gbit/s, 2-4 of 8-12
samples passing" was a different method and is not a comparison. The claim this
supports is narrow and it is the honest one: the loss cell is not the failure it
was, and the two carrier fixes above are not regressions.

**What is still not met**: the soak's four stage failures, the multi-second tail on
a fresh connection (which is what the `capacity` cell and the soak's `clean` stage
both read), and therefore every part of the objective that depends on them — the
drift axis, the release sweep, and any statement that this carrier is preferable
to TCP on a latency-sensitive path.

**And the last userspace suspicion is now eliminated, by counting wakes
(2026-11-01).** A parked reader cannot know whether it was reached, so the
connection now counts its own `wake_readers` calls and each park records the count
it began at. One cell, and the numbers end the question this page has been asking
for thirteen rounds:

| parked (µs) | the bytes had been ready for | **wakes while parked** |
|---|---|---|
| 7 003 866 | 26 µs | **280** |
| 7 003 718 | 10 µs | **533** |
| 7 001 611 | 5 µs | **432** |
| 2 892 303 | 24 µs | **71 480** |

So the reader was woken **hundreds — and in one case seventy-one thousand — times**
during the stall, took every one of those wake-ups, was polled, ran `poll_read`, and
found nothing to read. The wake path works, the task is scheduled, and the bytes are
genuinely absent.

That is the end of the carrier as a suspect for this symptom. Every layer inside
`mhd` is now measured and clear: the request, both ends' opens, handshakes and
acknowledgements, write and read parks, the socket's writability, the send buffer's
dwell, the receive buffer's readiness, the wake path itself, the per-stream and
per-connection budgets, the pool's slots, the kernel's socket queues and qdisc, and
drops on every device. What produces the bytes — the forwarding loop above the
tunnel stream and the backend behind it — has never been instrumented at all, and
that is now the only place this symptom can live.

**And it is not a hole either (2026-11-01).** The obvious remaining explanation was
that bytes had arrived but could not be handed out — `arrived()` only advances over
a contiguous prefix, so a missing range would make a reader wake hundreds of times
and read nothing. So the park line now carries the buffer's own state:
`(read, arrived, piece count, first piece)`.

| parked (µs) | ready for | wakes | hole |
|---|---|---|---|
| 7 003 659 | 23 µs | 137 | `(343, 343, **0**, None)` |
| 7 003 114 | 9 µs | 244 | `(371, 371, **0**, None)` |
| 5 457 832 | 5 µs | 126 108 | `(104, 104, **0**, None)` |

`read == arrived` and **zero pieces**: the buffer is empty and the reader sits
exactly at its frontier. Nothing has arrived beyond what has already been read, so
there is no hole and nothing to hand out — the peer sent nothing at all during
those seven seconds.

Put together with the wake count, that closes the receive side completely: the
client's reader is woken hundreds of times, is polled, calls `poll_read`, and the
stream it reads from has received nothing. **Every instrument that has been
pointing at the client has therefore been measuring a symptom on the wrong end.**

**And the forwarding loop names the stage: the visitor's bytes wait in their socket
while the server waits for a data channel (2026-11-01).** `stripe::send_loop` was
timed in both halves — the read from the visitor and the write into the stripe
group — and one cell answers it:

| end | reads that waited ≥ 20 ms | longest | writes that waited |
|---|---|---|---|
| server | 4 | **7 006 589 µs**, 7 004 215, 7 003 971 | 28 |
| client | 4 | 7 001 698, 7 001 672, 7 001 478 | 0 |

The server's **first read from the visitor socket** waited seven seconds. That is
before any tunnel exists for that visitor: `serve_tcp_visitor` accepts the socket,
then `pair_striped_group` waits for a data channel, and only when the gather
completes does `spawn_group` start `send_loop`. So the visitor's four bytes sat
unread in their socket for exactly as long as the pairing took.

**That is the whole tail, and it is where it was first measured — in round 10, as
`visitor waited for its data channels`, with waits of 293 ms to 5 s and
`PAIR_WAIT_BUDGET` expiries at exactly 5 000.** Everything measured since then
described the same wait from further downstream: the client's reader waiting for
bytes that had not been sent, the pool filling because connections were slow to
finish, the soak's stages dying because a visitor was refused. The instruments were
right and they were pointed at symptoms.

So the question is now one question, stated once: **why does a data channel take
hundreds of milliseconds to seconds to arrive after `CreateDataChannelFor` is
sent?** Its two ends are measured — the request crosses in 102 µs and the client's
open completes in milliseconds — so the interval is inside the gather, and the next
instrument belongs there rather than anywhere downstream.

**And the gather has a suspect with the code in front of it: the queue's mutex is
held across the wait (2026-11-01).** `take_channel` is

```rust
rx.lock().await.recv().await          // core/server.rs
```

with `SharedChannels<C> = Arc<tokio::sync::Mutex<mpsc::Receiver<C>>>`. A pairing
therefore holds the mutex for as long as it waits, so **only one visitor can be
waiting for a data channel at a time** — every other visitor queues on the mutex,
not on the channel, and a visitor that finds the queue empty waits out its own
`PAIR_WAIT_BUDGET` before the next one can even look. That is the serial queue this
page has been describing since before the cycle started, and the type's own doc
comment says the opposite of what the code does: "the mutex serializes only the
*take*, never the wait". The comment is the intent; the code is the defect.

**The fix landed, and it does not move the cell (2026-11-01).** `ChannelQueue` now
owns both ends of its `mpsc`: `try_recv` under a short `std::sync::Mutex`, a
`tokio::sync::Notify` raised by the producer after every push, and the notification
registered *before* the second look so a push landing between them cannot be lost.
All four consumer paths were converted in one commit — the L4 pool, the UDP pool,
the transparent service and the direct-channel route — and `take_channel`'s
signature is unchanged, so every call site stayed mechanical. `just check` green,
330 lib tests, both clippy passes.

Two cells of the `capacity` scenario afterwards: p99 **4 109 / 5 003 / 3 242 /
2 352 / 12 / 12 / 255** with p50 at 2.1-2.4 ms, and the A/A twin itself at
**5 003** and 5 002. So the serialization was real and is gone, and **it is not
what the tail is** — the twin proves the tail is present in the same build measured
twice.

That is worth stating plainly because it is the fourth mechanism this cycle has
found and removed without moving the metric, and the pattern is now unmistakable:
each of them was a genuine defect found by reading the code, and none of them is
the one the `capacity` cell is measuring. The tail has now survived a repair-queue
rewrite, a timer re-scoping, an unconditional wake, and the removal of a
service-wide serialisation.

**What is still not met**: the soak's four stage failures, the multi-second tail on
a fresh connection (which is what the `capacity` cell and the soak's `clean` stage
both read), and therefore every part of the objective that depends on them — the
drift axis, the release sweep, and any statement that this carrier is preferable
to TCP on a latency-sensitive path.

Meanwhile the work packages this defect has been crowding out are untouched, and
the objective names them: the `Frame::Datagram`/`Frame::L3` classes are declared by
the wire and **dropped** by the connection (`conn.rs`, `Frame::Datagram { .. } |
Frame::L3 { .. } => { self.ack_pending = true; }`), the owned-write path is
implemented for `StreamIo` by neither end, and the carrier's two-way quality rule
is still off by default. Those are next, and the tail above stays open with this
record.

**The stripe width is the arrival tail, and it is now measured rather than
argued (2026-11-01).** The plan's first phase was an instrument, and its gate was
that the instrument must reproduce the symptom before any fix is written. It did
not reproduce it — it *falsified the premise*, which is the more useful result.

What the instrument showed. The probe's `setup_us` (now `setup_p50_ms` and
nothing more) times the **visitor-to-server TCP handshake**, and the server
accepts before it pairs, so that number never sees a tunnel: `churn-16` reads
0.10 ms on mhd and 0.095 ms on TCP while the visitor actually waits tens of
milliseconds. A metric named "the setup a visitor pays to arrive" that reads
0.1 ms beside a real cost of 26 ms is worse than no metric, so the two
percentiles added to it were withdrawn and the definition was corrected in place.

Where the cost actually is, `dev` profile, both carriers, same host, 5 rounds
with the run's own A/A twin:

| cell | mhd | TCP |
|---|---|---|
| `churn-16` `rtt_p50_ms` | **34.66** | 2.815 |
| `churn-16` `rtt_p99_ms` | **55.65** | 3.461 |
| `churn-16` `rate_per_s` | **452.9** | 5341 |
| `rr-16` `rtt_p50_ms` / `p99` | 0.695 / 2.19 | 0.578 / 1.80 |
| `bulk-n` `throughput_gbps` | 8.155 | 10.901 |

So mhd is at parity with TCP once a connection exists (`rr-16`), and 12–16×
worse on a connection that has just arrived. That is the whole deficit, and it is
in one place.

Then three runs pinned the cause without touching the code:

| run | `churn-16` `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| full `dev` profile (bulk ran first) | 34.66 | 55.65 | 453 |
| `churn-16` alone, default target | **3.0–4.2** | 15–25 | 2081–3217 |
| `churn-16` alone, `MOLEHILL_STRIPE_COUNT=4` | **25.3–32.2** | 49–73 | 214–566 |

The same cell, the same binary, moves 8× when the stripe target is four instead
of one — with no bulk traffic anywhere in the run. And running the cell alone
(no bulk before it) recovers the target-1 behaviour, which is why the full-profile
number was 34 ms: the `bulk-*` scenarios before it had grown the plan.

The mechanism is therefore in two pieces of code, and both are load-bearing:

- `StripePlan` is **global and monotone**: one target per server, grown by
  `note_stall` on any group's backpressure, with no decay and no per-visitor say.
  One bulk burst raises it, and every later visitor pays for it, whatever its own
  traffic looks like. That is why the cell's numbers swing between rounds.
- `stripe_gather` is a **global mutex held for a whole gather**: one visitor at a
  time may collect K channels, each of which requires a control round trip and a
  fresh stream open. Sixteen concurrent fresh connections therefore queue on the
  lock, and the cost is `16 × K` serialized opens — 34 ms at K=4, which is the
  measured p50.

This is why the plan's phase order changes: **the gather goes first.** Prefill (the
former P1) cannot help while the gather exists — a warm queue is drained by the
first K-wide gather, and the lock still serializes the rest. mhd at K=1 is already
at parity with TCP, so the fix is to stop making ordinary visitors into gathers,
not to make gathers faster.

The revised order, each phase still gated on its own measurement:

| phase | what it changes | the cell that decides it |
|---|---|---|
| P1′ (was P3) | no global target and no gather lock: a visitor starts on one stripe immediately, and a *live* group is given more stripes only when its own sender parks | `churn-16` `rtt_p50_ms` back to the target-1 range (≈3 ms) with bulk before it; `bulk-n` within the 15 % floor |
| P2′ (was P1) | the residual cold-path round trip: authenticated channels ready before demand | `churn-16` p50 under the parity line; `capacity_streams`; the soak's five stages |
| P3′ (was P2) | the RTO as mhd's only repair path, using the SACK block it already carries | `loss1`: `rtt_p99_ms`; `bulk-n` within the floor |
| P4′ | the dead wire (`Frame::Datagram`/`Frame::L3`, acknowledged and dropped) and the 25 s pairing patience | all checks; no metric regression |
| P5′ | — | the release ritual: sweep, gate without a baseline, records, docs |

What landed with this record: `FirstByte` in `server.rs` (stamp at `accept`, at
pairing, at first byte, one line over 100 ms under `MOLEHILL_MHD_STATS=1`), the
`churn-16` cell headlined on the metric that carries its cost, and the corrected
`setup_p50_ms` definition on the benchmarks page in both languages. Nothing in the
data path changed, so every number above measures the tree as it stood before the
stripe work.

**What P1′ has to preserve, measured before it is written.** Striping is worth
something real: one bulk visitor (`bulk-1`, isolated run, three rounds each)
carries **5.35–5.64 Gbit/s** at width 1 and **6.62–7.10** at width 4 — +23 %, for
`cpu_s_per_gbit` 0.62–0.66 → 0.77–0.81. So the fix cannot be "stop striping"; it
has to be "stripe only the visitor that needs it, and only after it starts".

The design that follows from the two measurements, decided here so the next round
does not re-derive it:

1. **A visitor always starts on one stripe.** No global target is read on the
   arrival path, and `stripe_gather` is deleted with the atomic-gather invariant
   it existed for.
2. **The stripe's group is named on the data channel itself.** A growth stripe is
   requested for one group, so the client writes that group id in the tunnel
   prologue (four bytes, `None` for the first stripe, which is opened before a
   group exists). The server then delivers the channel to that group's own waiter
   instead of a shared queue — which is what makes concurrent gathers impossible
   to interleave without a mutex, and it retires `StripeGroups`' parked-group
   table, its TTL, its tombstones and `ReleaseStripedGroup` with it.
3. **Growth follows the group's own sender.** `StripeSender` already measures a
   contiguous park (`STRIPE_BACKPRESSURE`, `STRIPE_GROWTH_DWELL` exist); when a
   live group parks, the server asks for one more stripe for *that* group and the
   running group attaches it — one more write half for the sender's rotation, one
   more reader into the same reorder channel. A request that never parks stays at
   one stripe for its whole life.
4. **The width is bounded by the visitor's own behaviour, not by history.** A
   group that stops parking stops growing; nothing raises a target another
   visitor will pay for.

**Its first half landed and is measured (2026-11-01).** The process-wide width is
off the arrival path: a visitor starts on one channel, the width comes from the
internal measurement override (`initial_stripes`, 1 in production), and the
gather lock is only taken when a test or a measurement asks for more than one.
One build, both cells, four rounds with the run's own A/A twin
(`target/evidence/p1-single.json`, `p1-bulk1.json`):

| cell | before | after | TCP |
|---|---|---|---|
| `churn-16` `rtt_p50_ms` | 34.66 | **2.5–4.5** | 2.815 |
| `churn-16` `rtt_p99_ms` | 55.65 | **14.8–22.6** | 3.461 |
| `churn-16` `rate_per_s` | 453 | **2467–3523** | 5341 |
| `bulk-n` `throughput_gbps` | 8.155 | **11.8–12.4** | 10.901 |
| `bulk-1` `throughput_gbps` | 5.5–5.6 (width 1) / 6.6–7.1 (width 4) | 5.54–6.00 | — |

Two things to read carefully. The multi-stream cell **improved** by 46 % and now
sits above TCP: the old width was costing bulk throughput too, because a wide
gather multiplied every visitor's channel count against the same pool. And the
single-stream cell lost the +23 % that width four was worth — that is the debt
the growth half repays, and it is confined to one visitor moving bulk alone.
`churn-16`'s p99 is still 4–6× TCP's, which is the residual cold path (one
control round trip and one stream open before the visitor's first byte): that is
P2′'s target, and the `FirstByte` line says which half of it is which.

The full `dev` profile (every scenario, five rounds, its own A/A twin) confirms it
in context — and this is the run that also says where the remaining tail is:

| cell (full profile) | mhd before | mhd after | TCP |
|---|---|---|---|
| `churn-16` `rtt_p50_ms` | 34.66 | **2.62** | 2.815 |
| `churn-16` `rtt_p99_ms` | 55.65 | **21.49** | 3.461 |
| `churn-16` `rate_per_s` | 453 | **3328** | 5341 |
| `bulk-n` `throughput_gbps` | 8.155 | **11.88** | 10.901 |
| `bulk-1` `throughput_gbps` | 3.8–4.1 | 5.63 | — |
| `rr-1` `rtt_p50` / `p99` | 0.14 / 0.19 | **0.098 / 0.130** | — |
| `rr-16` `rtt_p50` / `p99` | 0.695 / 2.19 | **0.463 / 1.347** | 0.578 / 1.80 |

So on a path where connections already exist, mhd now **beats** TCP (`rr-1`,
`rr-16`) and carries more bulk (`bulk-n` 11.88 against 10.90). Arrival cost is at
parity in the median and 6× in the tail. That tail — 21.5 ms against TCP's
3.46 — is the next measurement, and the mechanism to look for is *queueing*, not
absence: sixteen concurrent pairings each need a control round trip and a stream
open, and none of them is slow by itself.

**The tail is the per-stream Noise handshake, and it is measured (2026-11-01).**
The arrival instrument (`MOLEHILL_MHD_STATS=1`) prints both halves of an arrival
every ten seconds; one window of `churn-16` (2 627 visitors) reads:

| half | p50 | p99 | max |
|---|---|---|---|
| `accept → paired` | **2 603 µs** | **14 127 µs** | 28 513 µs |
| `paired → first byte` | **12 µs** | 48 µs | 125 µs |

So the whole arrival cost is the pairing wait and none of it is the forwarding
loop. Lowering the mhd slow-opening bar to 5 ms (the same switch) then splits the
openings that matter: of 213 openings over 5 ms, the mean split was selector
**5 µs**, Noise handshake **1 168 µs**, hello-and-acknowledgement **4 538 µs**.
And the client's own line, at `debug` for every open (25 628 samples in one
round), gives the shape without any queueing in it: a channel open costs
**2 648 µs**, of which **1 818 µs (69 %) is the Noise handshake** and 829 µs is
the hello and its acknowledgement.

So a visitor's arrival pays a full Noise handshake — two round trips and two key
setups — once per connection, and sixteen concurrent visitors queue behind that
work. That is exactly what TCP does not pay: a yamux stream open carries no
handshake, which is why the same cell reads 3.46 ms there.

Three consequences, and they close the phase as it was written:

- **Prefill cannot fix it.** A warm channel costs the same open to produce; a
  spare pool moves the handshake off the visitor's path only until the visitors
  arrive faster than the pool is refilled, which is the case that is failing
  (3 300/s).
- **Growth cannot fix it.** Striping is a bulk concern; this is the arrival path.
- **The fix is to stop paying a handshake per visitor.** The lane already runs a
  Noise handshake once, when it is established (the validation stream). Deriving
  each stream's keys from that lane secret — `HKDF(lane handshake hash, stream
  id)`, a per-direction counter as the AEAD nonce — keeps every stream's crypto
  independent while making an opening a selector byte, a hello and an
  acknowledgement. Expected: an open of ~0.85 ms instead of 2.65, so a pairing
  p99 near 5 ms instead of 14.

That is the next change (P2″), and it is a wire change inside the unreleased
6.0 dialect: a stream's prologue names whether its keys are derived or
handshaked, so a mixed pair still works while the work lands.

**The multiplexer was tried first and reverted (2026-11-01), because it broke
striping.** The simpler route to "no handshake on arrival" is the one TCP takes:
put a multiplexer on the lane and let a visitor's channel be a stream of it —
the engine, the pool and the ceiling all already exist. It was built and
measured. A lane carried N long-lived Noise-wrapped streams, each hosting a yamux
session, and a visitor's channel became a stream of one of them.

| carriers per lane | `bulk-n` Gbit/s | `churn-16` p50 | `churn-16` p99 | rate/s |
|---|---|---|---|---|
| (before: one mhd stream per visitor) | 11.88 | 2.62 | 21.49 | 3328 |
| 1 | 6.7–9.1 | 1.37–2.29 | 2.76–9.02 | ~4200 |
| 4 | 9.88 | 1.38 | 3.73 | 4579 |
| 8 | 10.31 | 1.37 | 3.77 | 4444 |
| TCP, same profile | 10.90 | 2.82 | 3.46 | 5341 |

The arrival axis is exactly what the plan wanted — p99 **21.49 → 3.77 ms**,
level with TCP — and bulk's 13 % is inside the metric's materiality. What failed
is not a number: `integration_test::striped_data_channels` **fails on mhd and
passes on TCP** with the same tree. A four-stripe transfer ends at 5.9 MiB of
8.4 with twelve frames parked behind sequence 187, and the carrier's own dwell
instrument shows the stripes' streams sitting 20–87 ms in the send buffer before
their bytes leave.

The reason is structural and it is the thing to remember: **every carrier of one
lane shares one `Conn` — one window, one pump.** Adding carriers therefore adds
streams competing for the same budget instead of more budget, which is why bulk
moved only 9.88 → 10.31 as the count doubled, and why a four-stripe group on a
contended lane starves. Moving the handshake off the arrival path by sharing a
connection moves the cost into the window those visitors share.

So the change was reverted to `f0368ba`, with the striping suite green again on
the same tree. What survives is the measurement and its consequence: the next
attempt must make a stream cheaper **without** making many visitors share one
flow — which is what the per-stream key derivation above does, keeping each
stream its own mhd flow with its own offsets, credit and window. The other
candidate this rules out is "widen the lane until mux works": that is a
measurement of the carrier's window and pump, not of the arrival path, and it
belongs in its own phase if it is attempted at all.

One observation from the same runs, recorded because it is not caused by this
change and not yet explained: one round in three of `churn-16` collapses to
~190–270 visitors/s against ~4500 in the others, and runs from before this change
show the same bimodality.

**Prefill was tried next, three ways, and it is measured out too (2026-11-01).**
The plan's P2′ was "keep authenticated channels ready before demand": the same
opens, charged to a background tick instead of to a visitor's first byte. It was
built in three shapes and each was measured on the same two cells (`bulk-n`,
`churn-16`, four rounds, medians):

| warm-pool shape | `bulk-n` Gbit/s | `churn-16` p50 | p99 |
|---|---|---|---|
| none (the tree as it stood) | 11.88 | 2.62 | 21.49 |
| 16 ready, refilled on a 25 ms tick, target halved on any unanswered ask | 9.94 | 3.14 | 14.02 |
| the same with the target recovering +4 per tick | 11.34 | 2.89 | 13.98 |
| the same, refilled the moment a channel is taken | 10.28 | 3.47 | 16.93 |

The best of them is 14.0 ms against the tree's own 21.5 and TCP's 3.46, and it
costs 5–16 % of `bulk-n`. The arithmetic says why, and it is the reason this is
not a tuning problem:

- a channel costs **2.65 ms** to open, so a service that answers 3 300 arrivals
  per second needs **nine opens in flight continuously**, on top of the ready
  depth that absorbs a burst of sixteen — a target of ~25, not 16;
- at that width the pool's background work competes with the arrivals it exists
  to serve: the third shape, which refills on every take (the most aggressive),
  measured the *worst* arrival tail of the three;
- and every ready channel is a stream the pool has charged, which is exactly what
  `bulk-n` loses.

So prefill buys a constant factor and pays for it twice. It was reverted, and the
conclusion is written down rather than tuned around: **a 2.65 ms open cannot be
hidden by a pool — it has to stop costing 2.65 ms.** That is the key-derivation
change above, which keeps every stream its own flow (so the mux experiment's
failure does not repeat) and removes the two round trips a stream pays today.

**And the arrival cost has two halves, which is what the next attempt has to
respect.** A channel open is 2 648 µs: 1 818 µs of Noise handshake and 829 µs of
hello-and-acknowledgement. Key derivation removes the first and leaves the
second, so its ceiling is about **0.85 ms per arrival** — roughly a 3×
improvement on 2.65 and still a round trip that TCP does not pay. The only shape
that removes both is the one the mux experiment tried, and it *did* reach parity
(3.77 ms against 3.46): its failure was not the shape but a data-path bug — a
four-stripe transfer losing the last twelve frames while the carrier's own dwell
line showed the streams sitting 20–87 ms in the send buffer. So the two
candidates are now ordered by what is left, not by preference:

1. **Diagnose the mux's lost frames.** One test reproduces it
   (`striped_data_channels` on mhd, passes on TCP), and the candidate is the
   stream-end path: what `finish` does to a stream whose send buffer still holds
   bytes when the session above it is torn down. If that is a flush-ordering bug,
   fixing it buys arrival parity *and* keeps the shape TCP has.
2. **Key derivation**, if the mux path proves to be structurally lossy. It costs
   one round trip instead of two and no shared window; it cannot reach parity on
   its own.

**The mux's loss is not striping's fault, and it is not a torn frame (2026-11-01).**
The change was re-applied to diagnose it, and the test was run at three widths
through the internal override:

| `MOLEHILL_STRIPE_COUNT` | result |
|---|---|
| 1 | fails after 17.8 s (a stall, not a truncation) |
| 2 | fails after 18.4 s |
| 4 | fails after 3.8 s, `echo service closed after 5 807 104/8 388 608 bytes` |

So it fails at width **one** — every visitor on this carrier shares one stream and
one window — and the width only changes how it presents. With the reader's own
end logged, the failing direction *sent* 202 frames while its four readers ended
at sequence **195, 201, 176 and 150**, every one of them a clean EOF **at a frame
boundary**. Bytes disappeared before the FIN, on several stripes at once, with no
torn frame anywhere. That rules out the striping logic and points at the layer
underneath: a yamux session carrying its streams over one mhd stream loses data
when that stream is under its own backpressure — the carrier's dwell line put
those writes 20–87 ms in the send buffer. Two candidates remain, both worth a
dedicated round: what mhd's stream does with queued bytes when its own buffer is
near `MAX_BUFFERED` and the peer's window is closed, and whether a `MuxStream`
reset can discard frames the yamux session had already queued.

**And it was none of those: a stream's FIN was overtaking its own data
(2026-11-01, fixed).** The bug reproduces in **60 ms** without any of the server
or pool machinery — `a_multiplexed_stream_carries_four_streams_without_losing_bytes`
in `src/transport/mhd/pump.rs` opens one carrier stream, runs a yamux session over
it, and has four streams write a megabyte each. Before the fix, a stream that
wrote **1 048 576** bytes delivered **950 272** and then a *clean EOF* — six
16 KiB frames lost, and the EOF is what named the culprit: a clean end means the
peer received a FIN, so nothing below had errored.

The FIN came from `Connection`'s `control_frames`, a bookkeeping queue that is
drained **before** `pending_frames` so a window update never waits behind
megabytes of payload — a reasonable rule for a bodyless control frame whose
stream has nothing queued, and a data-losing one for a **close**: the FIN arrived
at the peer before the frames still sitting in `pending_frames`, the peer ended
the stream, and those frames were discarded. TCP hid this for two years because
the socket accepts data fast enough that `pending_frames` is nearly always empty
when a close is queued; a carrier that *backpressures* the mux keeps it full, and
the hazard becomes routine.

The fix is exactly that distinction: a close whose stream still has queued body
bytes goes to `pending_frames` (so it follows its own stream's data) and it still
jumps the bookkeeping queue when nothing of that stream is queued. The connection
keeps `queued_body: IntMap<StreamId, usize>` for the question — decremented where
a frame leaves the data queue, removed at zero.

That closes the last known blocker on the multiplexed lane: the arrival path it
measured (p99 21.49 → 3.77 ms, level with TCP's 3.46) can now be re-applied on a
carrier that no longer loses bytes, which is the next round's work. It also
stands on its own as a fix to the shipped TCP carrier: the same ordering hazard
existed there, just rarely enough that the suite never caught it.

**The lane was re-applied on the fixed carrier, and it is a trade the gate
refuses (2026-11-01).** With the close-ordering fix in, `striped_data_channels`
passes on `mhd` for the first time and the arrival numbers are the best this cycle
has produced — but the second axis pays for them. Same build, four rounds,
medians:

| cell | direct mhd streams (today) | multiplexed lane | TCP |
|---|---|---|---|
| `churn-16` `rtt_p50_ms` | 2.62 | **1.51** | 2.82 |
| `churn-16` `rtt_p99_ms` | 21.49 | **5.17** | 3.46 |
| `churn-16` `rate_per_s` | 3328 | **4335** | 5341 |
| `rr-16` `rtt_p50` / `p99` | 0.695 / 2.19 | **0.474 / 1.21** | 0.578 / 1.80 |
| `bulk-n` `throughput_gbps` | 11.49 | **8.76** | 10.90 |
| `bulk-n` `cpu_s_per_gbit` | 0.587 | **0.759** | — |

The bulk row is a same-day A/B (`--ab-arm id=l4 --binary-b` against a binary of
this commit without the lane, four rounds): the lane carries **−23.75 %**
throughput and **+29.37 %** CPU per bit, both past their floors. The cause is
structural and not a tuning problem — a multiplexer over a byte stream makes one
more copy of every byte, and `mhd`'s direct stream had none. So the lane buys the
arrival tail with the two axes the objective ranks *after* it but the release
still has to respect: `bulk-n` is the product's primary workload, and a 29 % CPU
regression contradicts the carrier's whole case.

Reverted again, and this time the conclusion is a decision rather than a
suspicion: **the arrival cost has to be removed inside the carrier's own stream,
not by putting a multiplexer over it.** That is key derivation — one lane-level
handshake, per-stream keys from it, every stream still its own flow. Its ceiling
is the 0.83 ms of hello-and-acknowledgement that remains after the 1.82 ms
handshake goes: arrival p99 should land near 5 ms with `bulk-n` untouched,
because nothing about the data path changes.

**Key derivation was built, it delivered the arrival win, and it was reverted
because it costs the bulk cell (2026-11-01).** Three attempts, all measured the
same way (`--ab-arm id=l4 --binary-b` against a build of `891129d`, four rounds,
mhd forced, `bulk-n` + `churn-16`):

| attempt | `churn-16` p99 | `bulk-n` throughput | `bulk-n` samples |
|---|---|---|---|
| baseline (`891129d`) | 21.5 ms | **11.69 Gbit/s** | **4 of 4** |
| record layer, 32 KiB, framed in a second buffer | −74 % (3.79 ms) | −15.67 % (CLAIM) | 2 of 5 |
| record layer, 16 KiB records | −78 % | 9.96 Gbit/s | 2 of 5 |
| record layer, one copy, prefix written apart | −78 % | — | **0 of 10** |
| two-copy version re-measured against the same baseline | — | — | **0 of 4** |

So the arrival half is real and large: p99 **21.5 → 3.79 ms** (TCP's is 3.46),
rate **+100 %**, CPU per bit **−52 %**. And the bulk half is not a tuning
problem — the last row re-measured the *first* attempt against the same baseline
and it failed 0 of 4, so the record size and the copy count were never the cause.
The tree is back at `891129d`, where `bulk-n` passes 4 of 4.

**What the failure looks like, for whoever picks this up.** The transfer runs at
full rate for most of the cell — the pump fills 42-datagram runs and 8.1 GB
crosses — and then degrades: the server's pump drops to a handful of datagrams
per second with **10 MB still queued on one stream**, the client's readers park
with **empty** receive buffers (`read == arrived`, `pieces == 0` — no hole, no
backlog), and `iperf3` times out at 25 s. Nothing in the carrier's own
instruments is alarmed: no socket wait, no reset, no `built nothing while bytes
were queued`, no repair owed, and the scheduler's walk never refuses while bytes
are queued. What is left after all of that is the **credit path**: the server is
`credit_blocked` with 10 MB queued, and the credit is advertised only when the
reader has consumed `WINDOW / 4` (4 MB) past the last advertisement. A record
layer that will not hand out a *partial* record makes the reader's consumption
lag the sender's by up to one record — and with a 16 MB window against a 4 MB
advertisement threshold, the sender can park mid-record with less than 4 MB of
reader progress since the last advertisement, which is exactly the state the logs
show. The Noise path had the same shape but its records were small enough that
the sender almost never parked mid-record, so the suite never caught it.

**The two ways forward, both worth the round:**

1. **Make the credit follow the reader, not a threshold.** `credit_due()` is a
   4 MB batching rule that exists to keep ack traffic down; on a carrier whose
   reader can stall behind an incomplete record it is a liveness rule. Advertising
   on every read that moves the frontier (or on a much smaller threshold, e.g.
   `WINDOW / 64`) would remove the deadlock window without touching the record
   layer. This is the cheaper fix and it is testable in isolation.
2. **Derive the keys without adding a record layer** — seal inside the carrier's
   own packet framing, so there is no second length prefix and no partial-record
   wait. That is a wire change inside the unreleased 6.0 dialect and a bigger
   piece of work than (1).

Both are gated on the same cell: `bulk-n` must pass 4 of 4 *and* `churn-16` p99
must stay near 4 ms. Until one of them lands, the tree carries the per-stream
handshake and the arrival tail, and this entry is the record of what the
alternative costs.

**Way (1) was tried and it is refuted, and the credit path was a red herring
(2026-11-01).** `credit_due()` became `recv_credit > advertised` — any movement
asks for a pass — and the derived keys were re-applied over it. `bulk-n` still
failed **0 of 8** across both arms where the baseline passes 4 of 4, and
`churn-16`'s p99 got **1 068 % worse** (a directional claim past its 15 % floor):
`poll_transmit` answers a due credit *instead of* a data packet, so a credit due
on every read preempts the path the arrival itself needs. Reverted.

The `credit_blocked=true` in the stall line that sent this investigation here is
**not** evidence of a spent window: `last_refusal` is stamped by any *partial*
`Conn::write` and cleared by nothing, and a record layer writing 32 KiB at a time
into a shared window is partial almost every time. The line reads `true` on a
healthy connection. That is a separate small defect worth fixing (the predicate
should be re-derived per pass, like `none_snapshot` was), and it cost this
investigation two runs.

**And the arrival tail is now the release blocker, not a latency footnote
(2026-11-01).** The sweep's `timeline` scenario drives an **interactive probe
that opens a fresh TCP connection per ping** (`bench/src/bench/probes/series.py`:
"a visitor arriving, not a session already open"), twenty per second, beside four
bulk streams, sixteen churn connections per second and a UDP session. So the
stage's p99 *is* the arrival cost under load, and the 1.0.0 sweep's molehill arm
read: clean **125 ms** (the v0.11.2 record's 30.8), rtt100 421, loss1 1 087,
loss5 2 471, and **loss5, rate100, jitter and clean#2 all FAILED** with
`iperf3 error: control socket has closed unexpectedly` — the loss5 cell carried
**0.0 Gbit/s for all 118 of its seconds**. The capacity ramp stopped at
**2 streams**. The same scenario at `--scale 0.2` passes every stage (the holds
are too short for the degradation to develop), which is why the failure needs
the full schedule to appear.

That reframes the trade the last three entries recorded: the arrival tail is not
a number on a latency chart, it is what fails four of eight stages of the
release sweep and holds the capacity ramp at two streams. The derived keys would
fix it (p99 3.79 ms, level with TCP) and their cost is the bulk cell — which the
eager-credit experiment shows is *not* the credit path. So the remaining
candidate is way (2), sealing inside the carrier's own framing, and it is the
one piece of work this cycle has not attempted. It is a wire change inside the
unreleased 6.0 dialect: a stream's prologue names its key material, the packet
carries the tag, and there is no second length prefix and no partial-record wait.

**And the arrival cost is mostly scheduling, not crypto (2026-11-01, measured).**
A release-mode micro-benchmark of the exact handshake this carrier runs
(`Noise_KK_25519_ChaChaPoly_BLAKE2s`, snow 0.10 with the ring resolver), 200
rounds:

| act | cost |
|---|---|
| `Builder::new(params.clone())` + `build_initiator()` | **45.0 µs** |
| initiator `write_message` (msg 1: `e, es`) | **181.8 µs** |
| initiator `read_message` (msg 2: `e, ee, se`) | **141.8 µs** |
| `into_transport_mode()` (both ends) | 0.3 µs |

So the whole handshake is **369 µs of crypto** — and the in-process instrument
reads **1 818 µs** for the same phase on the arrival path. The remaining
**~1 450 µs is the wire and the scheduler**: the carrier's own write act costs
103 µs (three `poll_write`s through `Conn::write`), and the rest is await
queueing under the sixteen concurrent arrivals the cell drives. Two details
worth knowing before anyone optimises the crypto: snow's ring resolver returns
`None` from `resolve_dh`, so X25519 runs `curve25519_dalek`'s `mul_clamped`
rather than ring's, and `resolve_hash` returns `None` for BLAKE2s, so the hash
runs the pure-Rust `blake2` crate — both sit inside that 369 µs, and neither is
the bottleneck.

That reframes the defect. The derived keys remove 369 µs of crypto *and* three
awaits from the arrival path, which is why they took the p99 by 74 % on a
handshake that is 14 % of the measured open cost — the saving is the scheduling,
not the arithmetic. So the arrival tail is an **await-count problem**, and the
two ways to reduce it that do not touch the record layer are: fewer messages in
the handshake (session resumption, which `src/protocol.rs`'s selector doc names
a `0x02` for and no module implements), and fewer pump hand-offs per arrival.
Both are un-attempted, and both are smaller than the record layer.

**And the one-round-trip open was built, measured, and it is a real win that the
cell cannot see (2026-11-02).** The handshake now carries one payload each way
(`NoiseStream::handshake_carrying`): a lane whose slot the server already holds
sends its tunnel hello in the handshake's *opening* message and reads the
acknowledgement from the *reply*, so a channel open is one flight instead of
two. The lane's first stream still pays two flights, because the server has to
reserve before it can answer; the two shapes are told apart by the payload's
presence, so neither end has to agree in advance and a disagreement is a
fallback rather than a deadlock. `just check` green, 331 lib tests, 22
integration.

The measurement, with `MOLEHILL_MHD_STATS` off and the open's own debug line
counted per sample (churn-16, one round, mhd forced):

| | baseline (`891129d`) | one-round-trip |
|---|---|---|
| first 500 opens, p50 | **3 313 µs** | **927 µs** |
| first 500 opens, p90 | 6 703 µs | 1 098 µs |
| first 500 opens under 1.5 ms | 87 of 500 | **494 of 500** |
| whole run, p50 | 3 168 µs | 3 420 µs |

So the mechanism works — a 3.6× cheaper open when the path is idle — and the
cell still reads no difference, because the steady state is queueing: sixteen
concurrent arrivals on four lanes put three to five opens in flight per lane,
and the wait for the pump's turn is larger than the flight it saves. The
instrument's own numbers say so: in the slow regime `hs_us` is 0-1 µs (the
handshake itself is free) and `ack_us` carries the whole open, which is the
round trip *plus* the queue.

**Two other things this round established.** First, a run whose `--binary-b`
names a missing file does not fail: the bench reported ten `not ready` samples
for the baseline arm and produced a table anyway. Rebuilding the baseline from
a worktree at `891129d` (`git worktree add`) fixed it — and note that
`cargo build --target-dir` in the *main* tree still stamps the worktree's HEAD,
because vergen reads the git directory, not the target directory. Second, the
`churn-16` cell read `6400 of 6400 replies did not match` on every sample of
every arm, which is the payload echo in `handshake_carrying` answering the
initiator with the responder's own payload; that is fixed, but the cell's
mismatch verdict is what a correctness break looks like and it is worth
remembering that it appears as a *latency* cell's failure line.

**The one-flight open landed (`f02ba51`), with two bugs the suite caught and one
number the cell does show.** The shape is in `NoiseStream::handshake_carrying`: one
payload each way, produced by a closure because the responder has to read the
initiator's message before it can decide what to answer. The bugs, both of which
would have shipped a dead data path:

1. **The acknowledgement was written twice.** The reply carried it and then the
   tunnel-hello arm wrote it again after reserving, so the client read a second
   ack as its first bytes of service data. `hot_reload_test` caught it as "did not
   answer AAAA within 15 s" — a *forwarding* test catching a *wire* defect. The arm
   now asks the handshake whether the reply already answered and skips the write.
2. **The plain data plane has no carrying shape.** A service with
   `data_enable = false` writes a selector byte and no handshake at all, so there
   is no first message to put a hello in; gating the branch on the lane's flag
   alone made every open on such a lane return `Closed` and killed the lane, which
   `mixed_transports` and `per_service_transport` saw as a pool that repaired and
   reaped in a loop. The branch is now gated on the lane being encrypted as well.

The number the cell *does* resolve is the wire ratio: **-15.22 % on
`wire_per_visitor_byte`** (CI `[-0.747..-0.649]`, so the interval does not cross
zero), because two records that used to cross the wire now ride the handshake's
own messages. The model marks it directional rather than a claim only because it
was not pre-registered as this run's endpoint. The terminal latency stays flat, and
that is the honest reading: on this cell the queue is longer than the flight.

**And the cell's 16x collapses were never the carrier (2026-11-02, `15d43a0`).**
The intermittent `churn-16` rounds that read ~180 arrivals/s where the same code
reads 2 900 turned out to be the **visitor namespace running out of ephemeral
ports**: a fresh connection per request at ~2 900/s accumulates ~174 000 sockets
in `TIME_WAIT` against a 28 231-port default range, the probe's connect is
refused, and it backs off 50 ms per refusal — waits that land inside the measured
window. The raw sample states it: `connect_errors=3773` of 6 400 attempts, and
`2627 + 3773 = 6400` exactly. It hit the A/A twin as readily as the arm under
test, which is the signature that should have been noticed far earlier: **a
"collapse" that respects no arm is an instrument, not a subject.**

The topology now sets `tcp_tw_reuse=1` and `ip_local_port_range=1024 65535` in the
visitor namespace only, and a round with more than one connect in twenty refused
is a failed cell naming port exhaustion. Six rounds after: `connect_errors=0` on
every one, 1.76–2.12 s each. **Every `churn-16` number recorded anywhere in this
file before `15d43a0` carries that contamination**, and the ones on rounds that
did not starve are fine while the ones on rounds that did are 16x low.

**The one-flight open, re-measured on the repaired instrument** (`--ab-arm`,
`--binary-b` at `891129d`, six rounds, mhd forced). The A/A twin reported 0 claims,
so the noise floor is clean at this run's alpha:

| | vs baseline |
|---|---|
| `churn-16` `wire_per_visitor_byte` | **−15.09 %** (CI [−0.727, −0.656], p 0.031) |
| `churn-16` `rtt_p50_ms` | −23.64 % (FRAGILE, p 0.219) |
| `churn-16` `rate_per_s` | +16.67 % (FRAGILE, CI crosses zero) |
| `bulk-n` `throughput_gbps` | +39.55 % (CLAIM at n=6, but the weakest one the sign test allows) |

Only the wire ratio is beyond argument: its interval is tight, it does not cross
zero, and the mechanism *is* a reduction in wire bytes. The p50 and the rate move
in the right direction and stay inside what six rounds can settle. The `bulk-n`
claim is reported by the model and is `FRAGILE` — one round dropped flips it, and
the baseline's own samples contain two rounds at ~3.1 Gbit/s against its usual
6.5, which is a bimodality on *that* cell that has not been explained. **Do not
build on it**: the honest status is "unresolved, needs ~28 rounds".

**And that `bulk-n` claim is refuted, along with the window hypothesis that was
supposed to explain it (2026-11-02).** Five runs of the cell, same host, same
binaries, within forty minutes:

| run | shape | `l4` (A) | twin / B | spread |
|---|---|---|---|---|
| `p39` | three arms, `-t 5` | 6.53 | 6.71 (A/A) | **52 %** |
| `p40` | two arms, `-t 5` | 11.52 | 11.67 (A/A) | 7.4 % |
| `p42` | two arms, `-t 20` | 11.04 | 10.62 (A/A) | 14.9 % |
| `p43` | two arms, `-t 5` | 11.45 | 11.69 (A/A) | 5.4 % |
| `p44` | three arms, `-t 5` | 11.83 | 11.55 (B) | **3.9 %** |

Two conclusions, and one of them kills the other:

1. **The `+39.55 %` `bulk-n` claim and the "6.5 vs 11.5" reading were the run,
   not the code.** `p44` runs the *same* A/B as `p39` — three arms, `-t 5`, the
   baseline binary beside the current one — and reads A **11.83**, B **11.55**, a
   2.4 % difference inside a 3.9 % floor; `p39` read 6.53 and 5.02 inside a 52 %
   one. So the one-flight open does not move bulk throughput, and nothing in this
   file's earlier `bulk-n` percentages that came from a wide-floor run is
   trustworthy.
2. **The window hypothesis is wrong and is reverted.** `p42` and `p43` hold
   everything else fixed and vary only the transfer length: four seconds of
   measurement gives a 5.4 % twin, eighteen seconds gives 14.9 %. `secs` went back
   to 5 (dev) and 10 (full), and the docs paragraph that had claimed a 52 %→7.4 %
   improvement from the window was replaced with what the runs actually show.

**What separates a 4 % run from a 52 % run is host state, and it is not
controlled.** All five are the same binary pair on the same machine within forty
minutes. The rule, now on the benchmarks page: a bulk comparison quotes the twin's
floor **from its own run**, and a run whose floor exceeds the threshold is
reported as unable to answer rather than as a small difference. Reading a
percentage from a run whose floor was never quoted is the mistake that produced
both the "record layer costs 15 % of bulk" and the "+39.55 %" readings.

**And the release blocker is gone (2026-11-02, `c8c5267`).** A full `--scale 1.0`
sweep of the `timeline` over the current tree completes **all eight stages**, where
the same scenario previously failed four of them outright:

| stage | before the port fix | now |
|---|---|---|
| clean | 125.0 ms | **42.1 ms** |
| rtt100 | 421.5 | 402.3 |
| loss1 | 1 087.2 | 1 082.6 |
| loss5 | **FAILED** (0.0 Gbit/s for all 118 s) | **2 459.7 ms**, 0.46 Gbit/s |
| rate100 | **FAILED** | **96.4 ms** |
| rate20 | 191.6 | 189.8 |
| jitter | **FAILED** | **243.5 ms** |
| clean#2 | **FAILED** | **5.9 ms** |

The capacity ramp is still unmeasured in that run (`--scenarios timeline`), so the
gate's ramp half is open, but the timeline half is green and that half is what
failed. **What cleared it was the visitor's port exhaustion**, not the arrival
work: the churn probe inside every stage was hitting it, and four stages died on
it.

Two things the run also shows, both worth a later round. `rtt100`'s 1.70 Gbit/s
is the 16 MiB window divided by the 400 ms round trip over four lanes — the
memory-for-throughput trade the CHANGELOG already states, visible here as a
number. And `clean#2` at 4.08 Gbit/s against `clean`'s 9.21 on the same condition
is a 2.2× difference between two identical stages of one run, which the host's
own variance does not explain; it is the next thing to look at before the sweep is
trusted as a whole.

**The carrier comparison, on the repaired instrument** (`MOLEHILL_DATA_CARRIER`
pinning one per run, same binary, six rounds, each run's own A/A twin):

| | `churn-16` mhd | `churn-16` tcp | `rr-16` mhd | `rr-16` tcp |
|---|---|---|---|---|
| p50 | 2.42 ms | 2.24 | 0.46 | 0.44 |
| p99 | **20.51** | **2.58** | 1.35 | 1.34 |
| rate/s | 3 612 | 6 728 | 29 949 | 31 545 |

**At parity wherever a connection is reused, eight times worse on the tail where
every request needs a new one** — and that is the per-stream Noise handshake,
which is the one item on this cycle's list that no attempt has moved without
costing the bulk cells.

**The throughput ceiling is the congestion window, and slow start never runs
(2026-11-02, measured).** A `--scale 0.35` sweep of the timeline with
`MOLEHILL_MHD_STATS=1` and the mhd module at `DEBUG` now logs each lane's window
once a second. Across the whole schedule, 2 023 samples:

- **`slow_start` is true in 0 of 2 023.** The latch the doc comment describes —
  "it latches off at the first loss and never comes back" — is off before the
  first sample and never returns, so every byte after that is bought additively.
- **`cwnd` spans 13-60 KiB**, i.e. ten to forty-odd packets, and is **never above
  60 KiB**; `MAX_CWND`'s 16 MiB is not approached in a single sample.
- **`flight` never exceeds 0 KiB** (under 1 KiB throughout), so the sender is
  never bound by what is in flight: `cwnd` itself is the ceiling.
- The trajectory across the schedule is 13.2 → 42.2 (rising through `clean`) →
  **19.8** after the first lossy stage → 25.0 → 26.3 → 27.7 → 30.3 → 32.9 → 35.6
  with occasional excursions to 55-59, and the throughput tracks it: 10.66 Gbit/s
  in the first `clean`, 4.08 in the repeat.

So throughput is `cwnd × lanes ÷ RTT`, and at this host's ~0.1 ms path a 40 KiB
window is about 3 Gbit/s per lane — which is exactly the 10-11 Gbit/s cell and
exactly the 4 Gbit/s repeat. **The carrier is window-bound, not window-ample**, and
a single loss early in a run costs it the rest of the run's growth. The doc
comment's premise ("the rate is the pacer's business; this is only the bound") does
not hold on a path whose bandwidth-delay product is larger than tens of kilobytes,
which on a real WAN is every path.

**The next change is therefore a slow-start restart**, the mechanism TCP already
has: after a period with no loss — and after an idle period, which the carrier also
lacks — re-arm exponential growth instead of paying additive-only for the rest of
the process's life. It is on the throughput axis, it is local to `conn.rs`'s growth
path, and the instrument above is how its effect gets measured: `slow_start=true`
must reappear, and `cwnd` must reach the path's BDP rather than 60 KiB.

**That paragraph was wrong twice, and the second error was the interesting one
(2026-11-02).** The first version of the window line carried no lane id, so four
connections reported into one stream and "13 → 42 → 19.8 → 26.3 KiB" was four
interleaved series read as one; that is fixed. The second error is worse: **the
line was read on the wrong side of the tunnel.** `bulk-n`'s visitor sends, so the
*server* is the bulk sender and the *client* only sends acknowledgements — and the
client's lanes therefore read `cwnd` = 13 490 B with `slow_start` false and
`acked_since_growth` frozen, which says nothing about throughput at all. Read on
the sender, the same run says:

| side | `cwnd` | `slow_start` |
|---|---|---|
| server (sender) | **2 MiB = `SLOW_START_CEILING`**, every lane, every sample | **true** in all |
| client (receiver) | 13 KiB, the initial value | false |

So on a clean path the window does what it was designed to do: it doubles to its
ceiling and stays in slow start. The alarm that `bulk-n`'s window "never grows by a
byte" was an artefact of sampling the acknowledgement-only side, and the lesson is
the one this file keeps relearning — **the workload decides which end is the
sender, and a per-end instrument read on the wrong end is not a measurement.**

This leaves the original question untouched: whether the *sender's* window is what
fails to recover across a shaping schedule. That is what the timeline run below
measures, on the server side.

**Read on the sender across a whole schedule, the window recovers and is not the
limit (2026-11-02).** A `--scale 0.35` timeline with the sender's window logged
once a second, the four tunnel lanes only:

| lane | first | `slow_start` true | max | `flight` median |
|---|---|---|---|---|
| 0 | 2 MiB | 15 of 443 samples | **8 919 K** | 0 K |
| 1 | 2 MiB | 109 of 421 | 6 228 K | 0 K |
| 2 | 2 MiB | 36 of 485 | **11 721 K** | 33 K |
| 3 | 2 MiB | 17 of 350 | 5 582 K | 0 K |

Three things, and the third retires this hypothesis:

1. **A clean path does reach the ceiling.** Every lane opens at 2 MiB —
   `SLOW_START_CEILING` — with slow start still on, which is the mechanism working
   as designed.
2. **`slow_start` does latch off at the lossy stages and never returns.** The
   samples where it is true are the early ones; after `loss1`/`loss5` every lane
   is in additive growth for the rest of the run. That is the design wart the code
   comment admits to ("never comes back"), and it is worth fixing for its own sake.
3. **It does not explain the throughput.** Additive growth still carried the
   window from 2 MiB to **5.6-11.7 MiB** by the end, and `flight` is 0-33 KiB at
   the median — the sender is never window-blocked. So the `clean#2` stage's
   4.08 Gbit/s is not a window problem, and this instrument has now ruled out the
   hypothesis it was built for.

The limiter is therefore somewhere the window does not reach. Two candidates are
left and both are cheap to read with what is already logged: **the pacer** — the
code says in as many words that the rate is the pacer's business and the window is
only the bound, so a paced send rate that does not recover from a shaping schedule
would produce exactly this — and **the visitor's own TCP**, whose four connections
each recover from the lossy stages on their own schedule. The next measurement
separates them without a new instrument: compare the tunnel's own carried bytes
per second across the two clean stages with the visitor's, since equal tunnel bytes
and unequal visitor bytes puts the deficit outside the carrier.

**It is outside the carrier, and `backlog_bytes` is what proves it
(2026-11-02).** The stall reporter already logs what is waiting. Across the whole
`--scale 0.35` timeline — every stage, including the two `clean` ones — the
**largest `backlog_bytes` in 807 stall lines is 92 bytes**, and the sender's
`flight` sits at 0-33 KiB while its window is 5.6-11.7 MiB. So during the stage
that reads 4.08 Gbit/s the carrier had, at every sampled instant, essentially
nothing to send and an eleven-megabyte window it could not use.

The tunnel cannot be the limiter of a transfer it is not holding. The deficit is
**upstream of it**, in the visitor's own four TCP connections, and the mechanism is
visible from the other side: while the tunnel is slow during `loss1`/`loss5` the
server's per-stream buffer fills, it stops draining the visitor's socket, the
visitor's TCP backs off, and its congestion window does not recover before the next
stage begins. The carrier's own backpressure creates that state, so it is
carrier-adjacent rather than foreign — but the limiter measured in `clean#2` is the
visitor's TCP, not `mhd`'s pacer and not its window.

That closes the thread with a negative result on three hypotheses in a row (the
window, the credit threshold, the pacer) and one live question that is worth a
round of its own: **whether draining the visitor socket sooner after a slow stage
would let its TCP recover.** The instrument for it is the same stall line, read the
other way: `backlog_bytes` should stay near zero *and* the server should not be
sitting on a full per-stream buffer while it refuses to read.

**And the capacity ramp reads zero because of one 3.68-second arrival, not because
of latency (2026-11-02).** `just bench --profile sweep --arms molehill --scenarios
capacity` runs the gate's ramp half: one bulk stream, a fresh-connection ping every
50 ms, SLO `rtt_p99_ms <= 50` and `error_rate_pct <= 0.5`. It stopped at **level
1** with `capacity_streams = 0`. The level's own series says why:

| samples | errors | p50 | p99 | max |
|---|---|---|---|---|
| 64 | 0 | **2.11 ms** | **3 676.65 ms** | 3 676.65 ms |

Sixty-three arrivals were fine and the median is **2.11 ms** — better than the
`churn-16` median, because one bulk stream is a lighter load than sixteen
concurrent connections. One arrival took 3.68 s and that single sample is the
maximum, which at n=64 is also the 99th percentile.

**3.68 s is an RTO ladder, not a queue**: with `MIN_RTO` at 200 ms and doubling,
the fourth retransmission lands at 3.0 s and the fifth at 6.2 s. So the cell is
decided by one opening that lost its first packet and had nothing behind it to
trigger a fast retransmit — which is exactly what P3′ is for, and the reason it
matters more on an *opening* than on any other packet. The wire evidence records
zero drops on all three legs (`rx_dropped: 0`, `drops: {}`), so the packet was
either never posted or the reply was; that is the next thing to look at.

Two method notes beside it. `capacity_streams` is a **max statistic over ~64
samples**, so one stalled arrival collapses the whole cell from 8 to 0; that is a
brittle instrument for an SLO and the ramp should report the count of stalled
arrivals beside the percentile. And the published record has
`capacity_streams = 8` for **every** arm — `l3`, `frp`, `rathole`, `nps`, all of
them — which is `streams_max`, so the ramp never bound for anybody and the metric
carried no contrast in that run (AGENTS §10: "a metric without contrast is not a
measurement").

**The stall is on the client, in the opening, and the ladder arithmetic that
looked like the answer does not survive its own evidence (2026-11-02).**
Reproducing the capacity cell three times gave `0, 1, 0` carried streams, so it is
intermittent but common. In a failing round the **client's** log has one line:

`mhd slow stream open: lane=2 us=6405892 credit=16776190 window=13490
buffer=33554432 queued_streams=0 retransmits=1`

What that establishes, and what it does not:

- **Established:** an opening on the client-side lane took **6.4 s**. At the moment
  it finished the lane had 16 MiB of credit left, an **empty** send buffer
  (`buffer=33 554 432` is `MAX_BUFFERED`, i.e. the *room*, not the occupancy — that
  was my second wrong reading of the round), a 13 490-byte window with zero flight,
  and **one** retransmit.
- **Not established, and I wrote it up too fast:** the 6.2 s RTO ladder
  (0.2+0.4+0.8+1.6+3.2) matches 6.4 s closely enough to be seductive, but
  `retransmits=1` is one retransmission, and one RTO from a 200 ms floor is 200 ms.
  A five-step ladder would leave five retransmits. **The magnitude matches and the
  evidence does not**, so the ladder is a coincidence of arithmetic, not a
  mechanism.

What is left standing is narrower and still useful: the wait is in
`open_stream`'s own window on the client, without a retransmit storm on that
stream, and the budgets were not the constraint at the end of it. Two instruments
would settle it and neither is large: the client-side opening already computes
`hs_us`/`ack_us` for its *other* debug line and this one omits them, so carrying
the split here costs nothing; and the pump's own park timing is already logged as
`mhd pump waited on the socket`.

Two side findings from the same reproduction. The **server** logged its own slow
opening in one round at `hs_us=279 144` — a handshake waiting 279 ms, which is one
RTO on the opening message, so the one-flight shape removed a flight but not the
loss path. And a cell that reads `0, 1, 0` across three rounds cannot be a release
gate until either the opening stall or the max-statistic changes.

**With the split instrumented, the answer is one RTO on the opening packet
(2026-11-02).** The client's slow-open line now carries `hs_us`/`ack_us` beside the
budgets, and three consecutive capacity rounds all fail level 1 the same way:

`lane=0 us=278712 hs_us=1 ack_us=278710 credit=16773650 window=13490
buffer=33554432 queued_streams=0 retransmits=1`

**`hs_us=1`** — the Noise handshake is not the cost on this path at all. **`ack_us=278 710`** — the wait is entirely the acknowledgement, and 278 ms is one step of
`MIN_RTO`. The server's own line from the same event reads `hs_us=279 144`: *the
server* waited 279 ms for the client's handshake message. So the client's opening
message — msg 1 of the handshake, the one the one-flight shape put the hello in —
**did not reach the server on its first attempt**, and only the RTO resent it.

No leg recorded a dropped packet (`rx_dropped: 0`, `drops: {}`), so this is very
likely not a path loss but a **post the carrier refused**: `transmit_stream` has
refusal paths ("range is empty", "no eligible stream") and a refused opening that is
only retried by the timer looks exactly like this. That is the next thing to read,
and the instrument is already there — the refusal paths are logged and
`retransmits=1` says the recovery was a retransmit, not a second attempt in the same
pass.

One correction to the paragraph above: the 6.4 s opening was an outlier from one
round; the reproducible shape is **~279 ms, one RTO**, and it is enough on its own to
break a 50 ms SLO at the 99th percentile of a 64-sample level.

**The opening is never refused, so the refusal theory is dead too (2026-11-02).**
`Conn::write` now logs which bound had no room when an *opening* stream cannot take
a byte. Three capacity rounds, all failing level 1, produced **zero such lines**: the
handshake message was always accepted into the send buffer immediately, which is
consistent with `hs_us=1`.

So the wait is after the buffer, and the two remaining candidates are both about
what happens between the buffer and the peer's reply:

1. **An empty stream frame may be announcing the stream.** `open()` only inserts
   state, but the caller then calls `wake_pump()`, and `transmit_stream` has an
   explicit "range is empty" refusal — which means a stream can be visited with
   nothing to send. If such a frame is what creates the stream on the server, the
   server's task starts *before* msg1 exists, and the 279 ms the server reports is
   mostly waiting for a message that was not written yet. That would make the
   measured latency an artefact of the order of two writes rather than a loss.
2. **msg1's own packet is lost and only the RTO recovers it.** `retransmits=1` on the
   opened stream supports this, and it is P3′'s subject.

The two are separable with one more line: log when `transmit_stream` refuses a
stream with an empty range and that stream has never carried a byte. That single
line decides whether the arrival path wastes a frame per open, and it is the next
thing to write rather than another capacity run.

One measurement note: this run failed level 1 in **3 of 3** rounds where the earlier
one gave `0, 1, 0`. Either the added logging costs enough to matter at this margin or
the cell is more variable than three samples can say; treated as the latter until
measured, because the logging is off unless the stats switch is set.

**And the defect has a name: the small-flight timer keys on the lane, not on the
packet (2026-11-02).** `Recovery::rto` decides the cheap floor with

`let small = self.live <= SMALL_FLIGHT_PACKETS;`

`self.live` is how many packets *this connection* has in flight. On a lane carrying
bulk that number is large, so `small` is false and a retransmission waits
`MIN_RTO`'s 200 ms and doubles — even when the packet being re-sent is an
**opening**, which is one handshake packet in the air no matter what else the lane
carries. The design intent is already written down and already tested (there is a
test named "a lost opening packet costs the cheap timer, not the burst's"); what it
keys on is the wrong quantity. That is why the exemption works on an idle lane and
does nothing on a busy one, and it matches every number this round produced: 279 ms
= one step of the wide timer, `hs_us=1` because the handshake is free once the
packets flow, `retransmits=1` because exactly one retransmission was needed, and no
drops anywhere because nothing was refused.

**The change, for the next round:** carry "this packet is an opening" on `Sent` and
let it take the small-flight floor regardless of `live`. It is a flag, one predicate
and a test, and it has a gate that is already measured: the capacity level-1 stall
should fall from ~279 ms toward the 20-40 ms the cheap timer costs, `churn-16`'s p99
should fall with it, and `bulk-n` must not move — the last one because a bulk lane's
retransmissions are not openings and must keep the wide timer.
**And the tail is not worker starvation — measured, three settings (2026-11-01).**
The process starts `min(cores, 4)` workers, so the obvious escape hatch is more.
`churn-16`, four rounds each, `TOKIO_WORKER_THREADS` set explicitly, mhd forced:

| workers | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| 4 (the default) | **3.25** | **16.15** | **2 630** |
| 8 | 3.69 | 18.60 | 2 349 |
| 16 | 4.03 | 18.88 | 2 169 |

Monotone in the wrong direction on all three, so the tail is a fixed chain of
hand-offs rather than a starved runtime, and raising the worker count is not a
fix. (One round in each run collapsed to ~180 arrivals/s — the bimodality noted
earlier, still unexplained.) The numbers sit in `target/evidence/workers-*.json`.

**And the record layer is measured at three sizes: it buys the arrival tail at
every one of them and costs the bulk cell at every one of them (2026-11-01).**
Each row is the derived-key tree over the same baseline build (`--ab-arm`,
`--binary-b`, four rounds, mhd forced):

| record size | `churn-16` p99 | `churn-16` rate | `bulk-n` throughput | `bulk-n` samples |
|---|---|---|---|---|
| none (the shipped tree) | 15.1–21.5 ms | 2 581–3 328/s | **12.29–12.32** | **5 of 5** |
| 32 KiB | −74 % (3.79 ms) | +100 % | −19 % (9.96) | 2 of 5 |
| 16 KiB | −78 % | — | 9.96 | 2 of 5 |
| **1 348 B (one packet)** | **−72 % (4.21 ms)** | **+146 %** | **−53 % (5.75)** | 4 of 8 |
| 1 348 B + per-stream credit | −72 % (4.21 ms) | +146 % | −53 % (5.75) | 4 of 8 |

So the two effects are both structural properties of "put an AEAD record layer
between the application and the carrier", and the record size only trades one
against the other: a bigger record costs less throughput (fewer AEAD calls) and
a smaller one fails less often (a shorter partial-record window), and neither
direction reaches both goals. **The record layer is the wrong mechanism**, and
this is the third granularity at which that has been measured.

**A fifth thing was tried and it is also refuted: the per-stream credit.** The
connection-wide `send_credit` is only refreshed by a dedicated `Frame::Credit`
every `WINDOW / 4` of reader progress, while the peer's per-stream read frontier
rides *every* acknowledgement block for free (`Ack::prefix`, already consumed by
`SendBuf::advance_to` to free buffer). So `Conn::write`'s bound used a batched
number where a current one was available — which looked exactly like the
deadlock. Making the bound `base + WINDOW - written` for the stream being
written changed **nothing measurable** (identical row above), so the credit path
is not the bulk cell's failure either. Reverted; the finding is recorded because
the reasoning was sound and the measurement is what rules it out.

**What is left, in order of size:** the arrival tail is an await-count problem
(measured above), the record layer cannot fix it without costing bulk, and the
two mechanisms that reduce awaits without a record layer are session resumption
(one round trip instead of two, with a ticket the lane issues at establishment)
and collapsing the pairing path's hand-offs. Both are unattempted.

**P4′ is half done, and the half that is left is left on purpose (2026-11-01).**
The two frame classes nothing sent are gone: a datagram frame and a whole-IP-packet
frame were declared, acknowledged and dropped by the connection, and no sender in
this dialect ever produced one — a UDP service and a transparent claim both ride
reliable streams. The kinds are now `Ack`, `Stream`, `Credit` and `Reset`, the
decoder refuses anything else, and the reader's size constants and round-trip case
list went with them.

The other half — tightening the pairing patience — is **not** done, because no
measurement says it should be. `PAIR_WAIT_BUDGET` (5 s) × `PAIR_ATTEMPTS` (5) is a
25 s ceiling on one visitor's wait, and the number that matters is not the ceiling
but that a *legitimate* cold open can take most of one budget: the client's own
dial has a 15 s budget, and the first visitor after a registration waits for a
lane that does not exist yet. Shortening it without a cell that measures the cold
path would trade a rare long wait for a rare truncated visitor. It stays, with the
reason written here rather than a number changed to look tidy.

**And P3′ was built and measured out (2026-11-01).** The rule was the one TCP
cannot implement: a packet is declared lost the moment the peer's *per-stream
cumulative byte prefix* passes its range, instead of waiting for three later
packets to be acknowledged. It was unit-tested in two cases (the proof fires; a
prefix on one stream never judges another), then A/B'd against the same build
without it, under `loss1`, on `bulk-n` and `rr-16`, four rounds with `--ab-arm`
and `--binary-b`:

- `rtt_p99_ms`: the run's floor is **0.10 %**, and the difference is inside it —
  no repair-latency effect at all;
- `bulk-n` `throughput_gbps`: floor 2.3 %, no difference;
- `retrans_segments`: **−1 464** directional for the rule, and *fragile* — one
  round's removal flips it.

Reverted, because a change with no measured win is one to drop rather than keep to
look tidy. The mechanism is sound and the reason it does not pay here is worth
recording: the request/response cells have **no bytes after a lost packet on the
same stream**, so the prefix can prove nothing and the peer waits for the
retransmit either way; and `loss1` is a *drop-based* shaper, so the bulk cell's
throughput is not RTO-limited and a faster repair has nothing to buy.

The A/A reading also needs one correction to an earlier entry: a twin that
reproduces a tail is evidence that the tail is *reproducible*, not that the cell
cannot resolve a change. What the twin cannot do is price a change by itself —
that is what `--ab-arm` with `--binary-b` is for.



- **The release sweep is done and the PR is ready (2026-10-19).** The
  published run is the second one: 19:19–21:04 UTC (6265.5 s, 90 samples) at
  `69e8bd9` with `tree_clean: true`, host `86ab43dd4d4010c3` (the same
  identity as the v0.11.1 record; CPU probe 417.3 MiB/s, loopback
  21.06 Gbit/s), fingerprint `257343fa3fd158d3`. The first run
  (16:46–18:29, at `04f5c5d`) was discarded when CI found the no-multiplex
  forwarding defect: fixing it changed `src/`, so the record had to be
  re-measured rather than name a revision whose code no longer matched.
  `just bench gate` passed the absolute half without a baseline on both; the
  v0.11.1 record is **refused** as a baseline because the engine hash changed
  with the flat Noise configs (`c4d337fbd8ee2adc` → `deec5d29798c5a68`).
  Every arm carried the ramp's full eight streams (a floor) and no arm wedged.
  The clean-stage bulk reading is 8.641 Gbit/s on L4 (clean#2 7.066) and 5.811
  on L3; the previous record's 8.907 is **not** a comparison — the method
  changed, and striping now starts at one channel and grows only on
  backpressure. The L4 clean p99 is 30.8 ms here against 8.8 ms in the
  discarded run with the same host identity and calibration; both runs pass
  the gate's 50 ms SLO, and the difference is recorded as run-to-run variance,
  not a claim about the fix (its cfg changes add nothing to the default
  build's data path). Charts and README are refreshed from the record.
- **The interop matrix was fixed and is green (2026-10-19).** v0.11.2 moved the
  Noise tables, and the matrix wrote one config for both binaries, so the
  released side refused the file before the wire was tested. Each side now
  gets its own schema, and the released side is configured with KK (a
  supported v0.11.1 setting) so the hello's version check is reachable: 3/3
  pass. Two observations from the same session: the default v0.11.1 client
  (NK) is refused cleanly, while a v0.11.1 client with its own static key (KK)
  leaves both sides waiting (ESTAB, no bytes) instead of closing — reproduced,
  not root-caused; and the L3 harness passed 13/15 on its first run at
  `04f5c5d` (the two lane-lending arms failed on the known ~5 s stall) and
  15/15 on a second run.
- **CI caught the no-multiplex forwarding defect before merge (2026-10-19).**
  The `noise` integration leg (`server,client,noise,hot-reload`, which is
  exactly the `embedded` set) registered its services but every visitor hung:
  v5.2 made the striped command the only TCP forward, and the client refused it
  without `multiplex`. The stripe registry and its release path are
  transport-agnostic and are compiled in every client build now; a group of one
  runs over the direct channel the server opened. The same CI run also caught
  the featureless compile (client/server-gated constants in `stripe`,
  client-gated socket2 imports in the keepalive helper, a wrong parameter name
  in `transport/tcp.rs`) and two Noise-key tests that need the `noise`
  feature. `just check`, the feature powerset, both lib legs and the no-mux
  hot-reload suite pass.
- **The branch history is squashed for review (2026-10-19).** 24 commits became
  eight. The configuration/runtime-policy consolidation is one commit because
  the intermediate trees' bench configs were unreadable by their own binary
  (the original gate passed on a stale release build), so they could not each
  satisfy `bench-selfcheck`; the other groups are striping, the L3 step, the
  status schema, the deployment docs, the cycle opener, the interop fix and the
  no-multiplex forwarding fix. The squash preserved the tree, and every commit
  passes its own gate.
- **The 0.11.2 configuration migration is in (2026-10-19).** The branch
  replaces the per-service pool/tunnel/transport model with one session pool,
  one L3 lane budget, four flat KK Noise layers, dynamic
  striping/worker/heartbeat/valve defaults and wire v5.2. `just check` is
  green. Open threads: manual acceptance of the full §4 table and the old-key
  refusals, and the tag decision.
- **Linux L3 takes the same device/address step macOS and Windows own
  (2026-10-19).** A Linux transparent service now creates its persistent TUN
  device when one is missing and, on the client, assigns each claimed address
  that is not already on it — exactly what macOS already does on its own (the
  device and the claimed addresses) and what the Windows server does for its
  adapter. No config key gates it: an existing device is used as it is (create
  it first to choose its MTU), and an address already assigned is left alone,
  so a second claim or a restart does not bounce a live interface. Routes,
  policy routing and forwarding stay the operator's on all three, which is the
  symmetry the maintainer asked for. An earlier version automated the routing
  half too (mark, policy rules, sysctls, `accept_local`); it was proven in a
  three-namespace topology and then withdrawn by decision. Two facts survive
  it: recipe B needs `net.ipv4.conf.<tun>.accept_local=1` (the manual recipe
  now documents it), and a device the daemon creates appears at
  startup/registration, so per-device sysctls must come from
  `net.ipv4.conf.default.*` before the start and routes that name the device
  must be applied after it appears — the ordering macOS and Windows recipes
  already have. The narrowed mode was re-proven on this host in the same
  topology, both recipes: the daemons created the devices and the client's
  address, the operator's steps followed, and the visitor was echoed through
  the claim. The default L3 harness ran again at `04f5c5d` on 2026-10-19:
  13/15 on the first run (the lending arms stalled) and 15/15 on a second; the
  device/address arms passed on both. A local variant that skipped the
  harness's own device setup created both
  devices and the client's address and passed every device-path arm, but its
  two lane-lending arms failed: the 16-flow arm stalled for about five seconds
  when a lane was lent, a window the default load finishes before. That is a
  lane-allocator observation, not root-caused here and not part of the device
  step.
- **Striped-group release and the striping signal were corrected (2026-10-19).**
  Two defects from the migration's striping commit. A group that ended by
  aborting a direction left its reader tasks parked on the mux streams, so the
  client's stream leases never came back — the saturated-pool test only passed
  because it restarted the client — and the target grew off *gather* latency, a
  proxy on the pairing path rather than path backpressure. The readers are now
  owned by the receive direction and aborted with it; the server releases an
  abandoned group on the client (`ReleaseStripedGroup`, tag 8 of the still
  unreleased v5.2 dialect, so it is extended in place); the client tombstones a
  released group so a stripe still in flight is dropped; and growth follows a
  group's own parked send direction (at least 250 ms, one step per 500 ms,
  capped at `MAX_STRIPES` 64). `a_saturated_pool_still_serves_the_next_visitor`
  now asserts the pool gets its streams back within 15 s and no longer restarts
  the client. No throughput claim is made from this: the sweep has to measure
  it, and it re-baselines anyway.
- **Platform cfg and CI verification were hardened after review (2026-10-18).**
  `build.rs` now emits the `mh_*` platform aliases (`mh_linux`, `mh_macos`,
  `mh_windows`, `mh_l3`, `mh_l3_client`, `mh_l3_server`) and makes
  `native_target` OS-aware; L3 code uses those aliases instead of repeated
  `target_os`/feature combinations. `transparent` now implies `carrier` (the
  KCP/TCP choice, fallback and lane budget) and `noise` (its control channel),
  not `multiplex`; `multiplex` includes `carrier`, so the L3 client works
  without yamux and the config capability, runtime gate and test build agree.
  CI builds every target with
  `--target` and runs both macOS architectures on native runners (`macos-26`,
  `macos-26-intel`); the test-build target list gained `macos-x64`. This is not
  a wire or config-schema change; the default release set is unchanged.
- **The v0.11.1 harness change is in, and the sweep that follows is a
  re-baseline.** The bench model moved from `benches/scripts/bench/` to the
  top-level `bench/` uv project (one console script, `ruff` + `pyright`
  configured in `bench/pyproject.toml`, adapters in
  `bench/src/bench/targets/`). The engine hash now covers the method modules,
  the adapters and the probes — not the CLI, the plotter or the utilities — so
  v0.11.1's fingerprints differ from v0.11.0's by construction. The sweep
  ran on 2026-10-18 and the gate passed **without** a baseline; the new file
  is the next baseline, and no cross-version method comparison is presented
  ([docs/release.md](docs/release.md), "Benchmarks: per-tag ritual").
  The refactor's decisions: one `ToolAdapter` contract starts the subject,
  the control and every peer (`bench/src/bench/targets/`), so a new peer is an
  adapter plus a `PEERS` row and the runner has no tool branches; the engine
  hash names the method modules, `targets/**` and `probes/**` explicitly and
  excludes `cli.py`, `plot.py`, `paths.py` and `utils/**`; pyright runs in
  `standard` mode at zero errors with the adapter files opted into `strict`
  (whole-model strict was measured at 4,811 errors, a separate annotation
  project). `--peer-dir` makes the peer cache configurable on `run` and
  `doctor`, and `provenance.binaries` still records only the subject binary.
- **v0.11.0 is released, and the release record is closed.** The tag was
  deleted and re-pushed once (the first push pointed at `9ad3362`, whose
  release run failed); the released tag is the re-push. The preparation
  record — four tag attempts, the squashes, the wedge gate and the offload fix
  — is in the archive.
- **v0.11.1 is the first `bench`-model baseline.** The v0.11.0 sweep predates
  the model (only the two *soak* files remain), so v0.11.1 was gated on the
  absolute checks — coverage, the endpoint invariant, the SLO, drift, wedge and
  the capacity ramp — and its record is what future releases compare against:
  `benches/records/results-bench-v0.11.1.json`, 90 samples, fingerprint
  `f246a9601305cc75`. The measured revision is `0b56438`; the preservation
  branch was retired with the other non-`main` branches on 2026-10-19, so the
  hash is recorded here — a fresh clone cannot resolve it.
- **An L3 claim's lane budget is a live tuning surface, and its default is
  two.** The throughput ceiling is a serialized per-packet path, so a claim's
  throughput follows its lanes; four lanes buys 121 % of bulk throughput and
  costs the interactive stream its SLO headroom, so the default is the largest
  value that keeps both. The measurement, the cost in connections and the
  upgrade path are owned by [`DEFAULT_CLAIM_LANES`](src/common/constants.rs)
  and the configuration page; the full record — including the first version of
  that constant, which chose four from the bulk-only table — is in the archive.
- **Lending is the one mechanism this leaves unmeasured.** `lend` is on by
  default and moves an *idle* lane to the claim with more flows than lanes, and
  the new default now gives it slack to move. Its own benefit is still
  unestablished: the 7-second `bulk-n` cell cannot show it (lending needs ticks
  and confirmations, and each `dev` round restarts the processes), and the one
  run that did show it converging was voided by the shaper bug recorded in the
  archive. A long-hold scenario is what would settle it; the arms already exist
  (`l3-budget10-lending` against `l3-budget10-nolend`).
- **The repository state**: v0.11.1 is released and the tag names `4874f05`,
  the `main` tip at tag time; `main` carries `version = "0.11.1"`. The
  `## [0.11.1] - 2026-10-17` section is dated, and the 0.11.2 work sits under
  `[Unreleased]`. `v0.11.0` names `f3295e8`. `feat/v0.11.2` carries
  `version = "0.11.2"` and is squashed to eight commits plus the benchmark
  record; the PR is open and no tag exists. The measured v0.11.2 revision is
  `69e8bd9` (`tree_clean` true; the record commit is docs/assets only, so the
  tag review's drift check still holds). The previous cycle's branches
  (`feat/v0.11.1-builds`, `bench/v0.11.1-measured` and the four local
  `backup/*`) were retired on 2026-10-19. The measured v0.11.1 revision is
  `0b56438`, recorded here and in the results file's `meta.provenance`; the
  tag review's drift check applies wherever that commit is reachable, so for
  this record a fresh clone prints a note instead of the verdict.
  The v0.11.1 tag was re-created on `4874f05` after a transient 0.11.2 bump was
  reset off `main`; that commit's tree is identical to the first tagged commit,
  so the rebuilt archives and image describe the same code. A re-tag re-runs
  the whole workflow, whose `crates.io` job then fails: `molehill-rathole
  0.11.1` is already published and cannot be uploaded twice. The workflow has
  no skip guard for that case, so a re-tag leaves the run red after the
  release, artifacts and GHCR image are updated.
- **Only docs and assets may follow a sweep.** `githooks/pre-tag` reads the
  results file's recorded revision and fails the tag if `src/`, `tests/`,
  `Cargo.*`, `build.rs` or the bench model's own code changed since — the
  changelog date and the README numbers may, code may not (AGENTS.md §10,
  "prove provenance").
- **The release path builds and smoke-tests; it no longer re-runs the suite.**
  `release.yml` builds every artifact and runs `--help` on the native ones
  the runner can execute; CI owns the test suite (it now also tests
  x86_64-musl natively), and a tag-only push runs the release review without
  the local heavy chain. The
  v0.11.0 release blockers were platform flakes in the release matrix, not
  product failures.

## Open threads for the next cycle

- **Decision (2026-10-19): one top-level tunnel pool in both modes; the
  per-service/per-claim model is rejected, not renamed.** There is exactly one
  tunnel pool per control session (one per data endpoint), and every service
  draws its channels from it; `[client.services.<name>].tunnels` is removed.
  The same shape holds for L3: `[transparent.data].tunnels` is the client's one
  carrier-connection budget, and `[transparent.claims.*].tunnels` is removed —
  that key is a dead field today (the parser accepts it, the lowering never
  reads it; verified 2026-10-19 with a budget of 4 and two claims writing 8 and
  6: the client started and ran on 2 lanes each). The current per-service
  implementation is wrong; 0.11.2 replaces it with the model below.
  - **Defaults are the measured optima, not the cheapest value.**
    - **L4: `tunnels = 4`** — the shipped knee, and now the *total* width of
      one pool rather than four per service: yamux spreads streams over it (up
      to 56 per tunnel), and the 0.10 A/B found 8 does not move a mixed
      workload's throughput while costing 42-61 % of interactive p99 at 15+
      streams. One pool serves about 224 concurrent streams; a deployment
      above that raises the one key.
    - **L3: `tunnels = 2 × claims`** — the measured knee (1 lane 2.351 Gbit/s,
      2 lanes 3.283, 4 lanes 5.191 but p99 271 ms from three streams). Two per
      claim also leaves the spare lane `lend` needs: at one per claim the
      allocator can never lend, because a donor must have more than its last
      lane. The public `DEFAULT_CLAIM_LANES` goes; the derivation lives
      privately in the L3 module.
    - An explicit `tunnels` is always the **total**: L4 clamps to `1..=64`; L3
      must cover the claim count and is capped at 1024, with integer division
      leaving the remainder unallocated (write a multiple of the claim count).
  - `[client.data].shared_pool` stops being a switch: one pool is the only
    shape, so the key is removed (or kept as a no-op for one release — to be
    decided).
  - The proposed `default_tunnels` name no longer describes the key. With no
    per-service inheritance it is the pool's width, not a default; the final
    name is open (`tunnels` today).
  - The UDP-derived floor becomes the deepest demand across every UDP service
    that draws on the pool, and validation and runtime must read the same
    source; the first-service-width defect below disappears with the model.
  - Per-service `remote_addr` and `transport` still fragment the L4 pool today
    (the key is session + data endpoint, and the transport is the first
    service's), so the model needs a rule for them: refuse the override, or
    move both to the client-wide block. L3's per-claim `remote_addr` is fine —
    there is no shared pool, only a total count.
  - Pinned vs elastic stays open. "On-demand" today means channel placement on
    a pinned pool; the 0.10 elastic pool (grow on load, shrink on idle) was
    replaced in 0.11 for capacity predictability (CHANGELOG, "The tunnel pool
    is pinned"). If 0.11.2 also restores growth, that is a second decision.
  - The 0.10 measurement (shared p99 8.15 ms against per-service 5.00 at 19/20
    steps) is why the per-service default shipped. This decision overrides it,
    and the single-pool model needs its own measurement before any claim; `4`
    and `2 × claims` are the best-evidenced defaults until that sweep.
  Implemented in 0.11.2: `[client.data].tunnels` is the session's total
  width and `[transparent.data].tunnels` the L3 lane budget. This entry stays
  as the decision record.
- **Operator decision (2026-10-19): the production pair stays on forwarding
  (L4) until L3 is stable; L3 remains the target.** The maintainer's own
  deployment (ssh/http/https, Minecraft UDP, Mumble TCP+UDP) keeps the
  forwarding model for now and moves to claims when the gaps below close — the
  visitor's real source address is what a claim buys, and it is worth waiting
  for. The 0.11.1 client migration that decision needs is schema only:
  `default_mode`, `default_carrier`, `[client.data.tcp]`, `[client.data.kcp]`
  and the per-service `carrier` are gone, the carrier is chosen at runtime
  (KCP, TCP fallback), and a host that both forwards and claims runs two
  processes. No code change is implied by this decision.
- **Recipe B's single-IP client side is a documentation gap (found
  2026-10-19).** With the claim on the server's own public address, the client
  assigns that address to its TUN, so the kernel treats it as local and the
  client's control channel to `<server-ip>:2333` loops back to itself.
  `docs/deployment.md` says the client side is "recipe A's, unchanged"; that is
  not sufficient. Verified in a network namespace: demote the `local` table
  (`ip rule add pref 200 lookup local; ip rule del pref 0`) and add
  `ip rule add pref 100 iif lo to <server-ip> lookup main` — locally generated
  traffic then leaves via the gateway while inbound visitor packets are still
  delivered locally. Both language pages need the extra steps, and a single-IP
  acceptance case would keep it from regressing. A documentation and
  acceptance gap, not a data-path defect.
- **Naming decision pending: `[client.data].tunnels` vs `default_tunnels`
  (raised 2026-10-19).** The key is the client-wide default for every service
  that does not write its own `[client.services.<name>].tunnels`, which is
  exactly what `default_token`, `default_remote_addr` and
  `default_retry_interval` signal, so the unprefixed name reads as an
  inconsistency. It is `tunnels` today because the per-service key has the same
  name (the `default_` family is used where the child key is named
  differently), and because 0.10 deliberately replaced `default_count` with
  `tunnels` when the pool became pinned — the old "initial size" reading no
  longer applied. Options: a sentence in the configuration page naming it the
  default for services (cheap, preferred while the line is stabilizing), or a
  rename to `default_tunnels` through the removed-keys migration (parser
  refusal, the `githooks/check-docs` removed-key table, both language pages, an
  invalid-config test and an `[Unreleased]` entry). A rename leaves the
  per-service `tunnels` and `[transparent.data].tunnels` (a lane budget) alone.
  Superseded (2026-10-19) by the one-pool decision above: with no per-service
  inheritance the key is the pool's width, not a default, so `default_tunnels`
  no longer describes it. The final name is open.
- **`shared_pool`'s width is decided by whichever service activates first
  (verified 2026-10-19).** With `[client.data].shared_pool = true`, the session
  pool is created by the first service to become active and uses *that
  service's* `tunnels` (its override, else `[client.data].tunnels`), so a
  per-service override can size the shared pool. The comment on
  `ClientConfig::tunnels` says the client-wide count is the one a shared pool
  uses, and the config validation checks only that count against the UDP
  floor, so the runtime and the stated rule disagree. Reproduced with two
  services (`tunnels = 16` and `4`, client-wide `8`): four runs logged
  `tunnels=16` twice and `tunnels=4` twice, tracking activation order. Not a
  deployment issue for the pair above (it does not set `shared_pool`), but a
  defect to fix before the key's semantics are frozen: either honour the
  client-wide count in shared mode, or validate the per-service overrides and
  document the first-service rule. No fix made. Subsumed by the one-pool
  decision above (2026-10-19): the model removes the per-service pool and the
  switch, so the defect goes away with the replacement rather than with a local
  fix.
- **The 32-bit embedded targets are compile-checked, not test-run.**
  `5e585b7` closed the aarch64 half — the two full-set musl targets build in
  CI (x86_64 on `ubuntu-latest`, aarch64 natively on `ubuntu-24.04-arm`) and a
  native aarch64-musl test job runs the release's test command on every PR —
  and added a cross compile job for the three 32-bit `embedded` targets. A
  qemu test run for those remains the optional next step. The closure record
  is in the archive.
- **A config the product refuses was a config every check accepted
  (2026-10-13).** The mandatory-Noise rule lived in `run_instance`, so every
  parser-based check was blind: 24 documented examples, every bench arm's
  configs, both L3-acceptance ends and all three interop cases refused to start
  while `just check` stayed green. Two guards now exist (the doc-example test
  and `from_str`; `just bench selfcheck` loads each arm's config). **The general
  hole is open**: a rule enforced at runtime rather than parse time has no
  check that names it; start the next one in `from_str`.
- **Build times were decomposed (2026-10-13, 20 cores, rustc 1.99); the
  workspace split is measured and declined for now.** The inner loop is fast
  (check 1.3 s, clippy pass 1 9 s, the lib test binary 4.5 s); the felt
  slowness is the release build (71 s, user ≈ wall) under fat LTO. LTO-off is
  61 s and thin LTO 68 s, so fat LTO costs ~10 s; a split buys only finer
  check/test incrementality (1–4.5 s). Revisit when check times reach minutes
  or an architectural boundary needs enforcing; the dependency direction is
  already clean (`mux` ← `transport` ← `core` ← `main`, `mux` a
  zero-reverse-dep leaf). The benches-only classification now covers both hooks
  (`f9c0187`).
- **The safe-`nix` replacement for `udp_batch` is in.** The shipped variant
  rebuilds `MultiHeaders` per call (it is `!Send`), so the module has no
  `unsafe` left; a stored-header + `SmallVec` variant kept three `Send`/`Sync`
  proofs and measured the same. On the KCP `l4` path
  (`just bench --profile screen --arms l4 --ab-arm l4`, four runs) the
  raw-FFI baseline measured 1.566-1.578 s/Gbit against 1.590-1.602 for the nix
  variants: ~1.5-1.9 % higher CPU, consistently in the same direction. That is
  inside `cpu_s_per_gbit`'s 15 % materiality floor (no claim), so the safety
  win was accepted.
- **`quinn-udp` is not a drop-in replacement for this batching.** Its
  `poll_send` takes one `Transmit`, and GSO requires contiguous, equal-sized
  segments; KCP's batch is a mix of `Staged` and `Split` spans, so using it
  would mean copying every datagram or paying one syscall per datagram. Its
  `poll_recv` enables GRO, which returns coalesced datagrams that KCP would
  have to split by `stride` before its per-datagram accounting. That is a
  transport redesign with its own A/B, not a wrapper swap, and the `nix`
  wrapper already removed the local unsafe.
- **`tun-rs` is accepted for the cross-platform L3 cycle.** The earlier trial
  measured 1.1969 s/Gbit for the audited ioctl attach against 1.2045 for
  `tun-rs` (~0.6 % slower, inside the 15 % floor), and adds 29 crates and ~300
  lockfile lines; the cross-platform requirement is what changed the decision,
  not the cost. The three audited ioctls are gone, so production `unsafe` is
  now zero; `tun-rs`'s syscalls sit behind the dependency audit/deny gates.
  Vendoring only the used Linux subset was still rejected: 4,588 lines and 41
  `unsafe` blocks against the 299-line wrapper.
- **Protocol v5.1, and why it is a patch-release wire break.** The version is
  now `major.minor` (two bytes) and `StartForwardTransparent` carries the frame
  format (`Ip` or `Vnet`). A Linux pair negotiates `Vnet` and keeps the GSO
  fast path; any pair with a plain-IP device negotiates `Ip`, and the Linux
  side converts in `src/transparent/frame.rs`. v5.0 is refused on the
  connection it happens on. AGENTS.md §5 was updated: `5.1` is a dialect
  label, not a SemVer-compatible minor.
- **Both Linux L3 acceptance modes pass (2026-10-17).** `just l3-accept`
  (Vnet) and the same harness with `MOLEHILL_L3_WIRE=ip` (plain-IP wire) both
  returned PASS on every verdict, including transparency, no server-side
  flow state, bulk/MTU, 2,000 small round trips and 16 flows. The plain-IP
  run's bulk arm carried 314 packets at a mean 2,237 B/packet instead of 52
  super-frames at 7,614 B, which is the expected shape of the conversion
  path. The screen A/B for `tun-rs` on Linux has not been re-run since the
  cross-platform changes; it is deferred for v0.11.1 by the operator decision
  below, so this release makes no tun-rs A/B claim.
- **IPv6 is in v0.11.1, not deferred.** The v5.1 frame carries opaque IP
  packets, so this needed no wire bump: the work is the IPv6 parser (base
  header, common extension-chain walk with a header-count ceiling, fragment
  header, TCP/UDP ports, ICMPv6/ESP address-only, jumbograms rejected), the
  `gso_split` family argument, macOS `/128` assignment, Linux `ip -6`
  rules/routes and forwarding check, the Windows `/128` recipe, and tests.
  `just l3-accept-ipv6` passes on the `Vnet` wire and with
  `MOLEHILL_L3_WIRE=ip`: a visitor on an IPv6 path reaches a client-owned
  `/128`, the backend sees the visitor's IPv6 address, the server holds no
  socket, and a 60 KiB stream round-trips byte-exactly. The macOS/Windows
  IPv6 recipes still need real-host validation, like their IPv4 counterparts;
  that validation is deferred for v0.11.1 (see the deferral note below).
- **Windows L3 client is out of scope for v0.11.1.** Windows has no built-in
  source-address policy routing; a native client would need a WFP callout or
  WinDivert-class redirector, with a driver and its own security review. The
  config refuses it at parse time and names WSL2. macOS uses a pf
  `route-to` rule; the recipe still needs validation on a real Mac, and the
  exact syntax may depend on the utun's point-to-point peer. Windows **server**
  support is implemented (Wintun + `wintun.dll` + route/forwarding recipe) but
  also needs a real-host acceptance run; that run is deferred for v0.11.1 (see
  the deferral note below).
- **Deferred by operator decision for v0.11.1 (2026-10-18): macOS/Windows L3
  real-host acceptance, the `tun-rs` screen A/B and the lending measurement.**
  Linux `l3-accept` on both wire formats remains the only end-to-end L3
  evidence; the macOS pf/utun and Windows Wintun/routing recipes are documented
  but unvalidated on hardware. No tun-rs performance claim or lending claim is
  made from this release. These are accepted gaps, not blockers.
- **The five test-only env-mutation `unsafe` blocks can be wrapped, but the
  trade was declined.** `temp-env` 0.3.6 (`async_closure`) makes
  `set_var`/`remove_var` scoped and safe at the call site; a worktree
  conversion of the three multiplex tests and the pool handover test passed
  the full suites. It needs a dev-dependency (`futures` via `async_closure`)
  and re-resolves unrelated `windows-sys` entries in `Cargo.lock`, so it is
  not worth changing a sanctioned test-only waiver for it.
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench project with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).
- **Repository settings are a human's to make**: branch protection for `main`
  and the release secrets. Nothing in this repository can enable them; they are
  the last release item outside the tree.

## Environment notes (this host, re-checked 2026-10-01)

- **Verify `iperf3` before a long run** (`command -v iperf3`). It is installed
  at runtime rather than baked into the image, so a container restart drops it:
  measured on 2026-10-01, when a restart (new hostname, uptime reset) left the
  binary gone and a sweep produced an artifact of typed failures in 14 seconds.
  The bench fails cleanly — every test records `No such file or directory:
  'iperf3'` and the gate refuses the file — but it looks like a finished
  artifact until someone reads it.
- **Never run two `just check`s at once.** The integration suites bind fixed
  ports by design (`--test-threads=1`), so a second full chain running beside the
  first makes them collide — measured this cycle as `status_api_test` failing
  inside one chain while a fresh chain on the same tree passed, and the suite then
  passing three times out of three standalone. A red gate on a tree that is green
  when re-run alone is worth checking for a concurrent chain before it is blamed on
  the code.
- **`/tmp` is periodically wiped.** Keep `--out` and logs under `~/tmp` or the
  repo. The bench model's own work directories (`~/tmp/bench-*`) are normal
  residue and are never deleted by the harness.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python
  child, which keeps executing and holds the bench lock.
- **Peers are cached** in `~/tmp/bench-peers` and the interop binary in
  `~/tmp/interop`, so `just bench` and `just interop` normally need no network.
  `just bench peers fetch` re-resolves each peer to the **latest GitHub
  release** and re-downloads when the cached version is behind; the resolved
  versions are recorded in the results meta beside the numbers.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.
- **This host is `3f8b4508ab91`**: 20 cores, 23 GB RAM, kernel
  `6.12.0-160000.38-default`. `/etc/machine-id` is **absent**, so the bench
  model's `host_id` hashes `cpu model + core count` only
  (`host_id_basis.machine_id: false`) — *not* the hostname. The container's
  hostname changes between sessions without moving `host_id`, which is the
  point: the comparable runs in this cycle's files share `host_id
  d764f9da9c7e5b2a`, and a file *without* a `host_id`
  (`results-soak-v0.9.0.json`, `98c48ea3fa68`) falls back to the hostname, so
  the gate refuses it.
- **`iperf3` is version-boundary-sensitive.** The carrier/KCP numbers of this
  cycle were measured across an `iperf3` reinstall (now 3.18); a version can
  change the UDP summary fields the model reads, so re-measure the baseline in
  the same session whenever the pair matters. The model should record the
  version with the run.
- **A baseline-less run is the norm here**: the gate's own self-check
  (completeness, endpoint invariant, absolute SLO) is what a fresh sweep is
  gated on, because every stored baseline either has no `host_id` or predates
  the current method. The v0.9.0 file is still compared where the method keys
  agree, and the gate names what it could not verify rather than assuming it.

## Archive

Everything this cycle closed is in [HANDOFF-archive.md](HANDOFF-archive.md),
with a contents list at the top and the detailed index further down: the
v0.11.0 release preparation (squashes, the wedge gate, the release audit, the
offload fix, the sweep record), the L3 cycle's decisions and investigation
records, the v0.10.0 cycle, the incidents and the superseded sweeps. The
records that predate the 2026-09-28 freeze were archived into git history
earlier and are indexed there too (`git show f2156de^:HANDOFF.md`).

The rule that keeps the split honest: **a record in the archive says what the
branch's authors believed at the time, and no number in it may be quoted as a
measurement of the current code, compared against a Soak result, or used to
gate anything.** The live numbers are
[the release sweep record](benches/records/results-bench-v0.11.0.json), whose
method's home is [docs/benchmarks.md](docs/benchmarks.md).
