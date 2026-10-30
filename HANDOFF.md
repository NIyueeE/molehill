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

   - **Item (2): the cell now passes 6 of 7, and the fix was a window leak
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
     - **What is still open on this cell:** the connect shape. One sample of
       seven now fails at `iperf3 -c` with `Connection refused`, and the earlier
       table put that failure at 0.3–0.4 s — which is `INITIAL_RTO` again. The
       next probe is the visitor's own connect timing against a lane whose first
       stream open costs the timer.
     - **The 74 057 `dropped data for a retired stream` lines are a symptom.**
       That count was already learned the hard way (§ above, the revert): it goes
       to zero without the failure rate moving, because a reset ahead of queued
       data discards bytes the visitor still wanted.
     - **The design question is still open, and it is not what this round
       measured.** `Conn::send_credit` is a **connection-wide** byte window:
       `recv_credit = read + WINDOW`, where `read` is everything consumed across
       every stream. A multiplexer with only a connection window blocks every
       stream on one slow reader's progress — yamux and HTTP/2 both carry a
       per-stream window beside it for this reason. The leak above was fixed
       *inside* that design; whether the design itself still needs the per-stream
       window is unmeasured, and the instrument that would say so is the
       `backlog_streams` / `backlog_worst_bytes` pair now on the stall line.

   - **The width change does not touch the transparent path.** `just l3-accept`
     (root, three namespaces, fifteen checks) reads the same at four workers as at
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
   - **The `MIN_RTO` trade ran, and this run's own floor is wider than the
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
     - **The trigger is still the second claim, and that is now the only
       variable left.** One claim with two lanes passes (`16/16 in 0.106 s`,
       above); two claims with four lanes stall — with lending on **and** with it
       off, which is what makes "lending" the wrong name for this defect. Both
       failing arms share exactly one thing the passing ones do not: an `idle`
       claim holding two lanes that carry nothing, on the same TUN device and the
       same runtime. The next probe is what those two silent lanes cost the busy
       claim's flows — they share the hub's single routing lock and the device's
       read loop, and nothing yet measures either.
     - **The probe that does choose between them, and it is cheap:** the TUN's own
       queue counters across *this* arm. `drop_snapshot` currently runs once,
       before the many-flows arm, so nothing reads `l3cli0`/`l3srv0` around the
       lending arm at all — a device whose queue fills while the daemon is between
       reads drops silently there, and `tx_dropped` moving would settle it.
     - **The per-flow question is answered, and it moves the defect
       (2026-10-30, load 1.3, `just l3-accept` with the flow trace on).** The
       lending arm lost **11 of 16** flows (`5/16 in 6.284 s`) against **13/16**
       for the lending-off control, and the backend's own `CLOSED` lines —
       written this time by the new instrument — say exactly what happened to
       each one:

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
