# HANDOFF: Working State & Future Work

> **State as of 2026-10-22.** **The v1.0.0 development cycle is open on the
> branch `feat/v1.0.0`; `main` stays at the released 0.11.2 while the
> manifest moves to `1.0.0`.** A major number describes the product, not the
> wire: `CURRENT_PROTO_VERSION` is still **5.2**, and the one wire change this
> cycle has committed to is the carrier redesign — **6.0**, a new dialect
> rather than an extension of 5.2 (AGENTS.md §5). There is no 5.3. **Maintainer
> decision (2026-10-23): 6.0 is the target dialect; the plan text's
> 5.2 -> 5.3 is superseded.** 6.0 keeps no backward compatibility, and none is
> expressible: a 5.2 peer drives KCP over UDP and yamux over TCP, a 6.0 peer
> drives neither, so each end refuses the other on the connection it happens on
> and both upgrade together. The constant flips in the commit that swaps the
> carrier, not before.
> `[Unreleased]` already carries the configuration refusals that landed on
> `main` after 0.11.2; they ride this release.
>
> **v0.11.2 is released: the tag names `64d8ebc`, the release commit on
> `main`.** The GitHub release is published (ten
> archives and `SHA256SUMS`), GHCR carries `:v0.11.2` and `:latest`, and
> crates.io has `molehill-rathole 0.11.2`. v0.11.2 moves the wire to
> **protocol v5.2** (dynamic striping; `StartForwardTcp` and
> `[server.data].stripe_count` are gone), consolidates the configuration
> model (one pool per forwarding session, one L3 lane budget, fixed KK Noise
> in four flat layers), derives the UDP and heartbeat policies from the path,
> adds a global `max_tunnels`, splits the status API into configured/actual
> state, and gives Linux the symmetric L3 device/address step. Its sweep is
> the next baseline (the details are under "Where this stands").
>
> **v0.11.1 is released: the tag names `4874f05`,
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
> **The v0.11.2 branch is merged and released (2026-10-20).** The tag names
> the `chore(release)` commit `64d8ebc`; `main` carries it plus this record.
> `[Unreleased]` is empty and the next cycle is not open yet. The merged
> branch `feat/v0.11.2` was retired locally and on `origin` on 2026-10-22;
  **Found it, in my own code (2026-10-23): one waker per stream, two directions.**
  transport/quic/conn.rs:44 keeps 'wakers: Mutex<HashMap<StreamId, Waker>>' and
  park() inserts by stream id alone. A direct channel is the one place that reads
  and writes **at the same time** on one stream: the writer parks, the reader parks
  and overwrites it, and when the write becomes possible wake() wakes only the
  reader — so the writer never wakes and the lane stalls after its first exchange.
  The seam test missed it because it strictly alternates (write, read, write); a
  lane does not. That also explains the 41 re-dials: each attempt comes up, moves a
  few frames and stalls. The fix is the map's key — (StreamId, Dir) — with park and
  wake carrying the direction; quinn_proto::Dir is already imported next door.
  **Fix landed, lane still stalled (2026-10-23).** The waker map is now keyed by
  (StreamId, Wait), with Wait::Read and Wait::Write carried through park and wake;
  the code builds, the quic suite stays at 325 passed. It is a real bug and a real
  fix — but the lane still does not carry: the same smoke still ends in the client
  timing out after 86 s. So the two-waker stall was A bug, not THE bug, or it was
  one of several. What has not been ruled out yet: the write half's credit path
  (whether a write that the engine refuses with Blocked is retried once the task
  drains), and whether the claim's read loop ever polls the lane after the hello.
> the local `backup/v0.11.2-presquash` branch is the only non-`main` ref
> left, and it is unmerged by design (the pre-squash history).
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

- **Pass-through, bigger windows and a 32-datagram batch moved the carrier
  2-3x (2026-10-24).** The plaintext path no longer copies each byte twice (a
  framed buffer on the way out, `raw`/`plain` on the way in); the selector byte
  is the only thing that still needs a buffer. With the same commit's window
  changes the same cells now read:

  | cell | UDP carrier | KCP (same commit) |
  |---|---|---|
  | `bulk-1` (`loss1`) | **2.42-2.61** Gbit/s | 3.26-3.45 |
  | `bulk-n` (`loss1`, 8 streams) | **4.686** | 11.99-12.38 |
  | `cost` (`clean`) | **5.849** at 0.934 s/Gbit | 12.81 at 0.624 |

  The eight-stream cell moved most (2.57 -> 4.686), which says its earlier gap
  was per-byte cost rather than bytes in flight: engine plus adapter were
  burning 2.5 cores to move 2.7 Gbit/s where KCP spends the same cores on 12.
  What is left is 1.3x on one stream, 2.6x on eight and 2.2x on a clean path,
  with per-byte cost now 0.934 against KCP's 0.624.
  - **A 64-datagram transmit batch is a trap**: the cells fell to
    0.80-0.83 Gbit/s (a 76 KiB `UDP_SEGMENT` message is not something this path
    takes), so 32 stays. 8 MiB and 32 MiB of socket buffer measure the same at
    these rates; 32 MiB stays because it is what KCP asks for.
  - **The last copy is gone and it was worth nothing measurable**: the driver
    now hands its transmit buffer to the sender task and takes it back (a
    recycled channel), so the engine writes into the buffer that leaves
    instead of a scratch buffer that gets copied out. Measured after:
    `bulk-n` 4.19-4.99, `bulk-1` 2.62, clean 0.937-0.952 s/Gbit — all inside
    the noise of the numbers above. Kept because it removes work and costs
    none, but the remaining gap is **not** in the copies.
  - **Two claims scale 1.6x, where KCP's scale 1.9x**: `bulk-pair` reads
    4.04-4.17 Gbit/s against KCP's 6.07-6.24, so the per-claim path is 1.5x
    behind on its own and the eight-stream cell (3-4 flows on a lane) loses
    more than that again — multiplexing several flows onto one ordered stream
    is where the rest of the gap lives.
  - **Congestion control is not the limiter, and the window is already past
    its knee (2026-10-24).** Two controls settled it. A fixed-window pipe
    controller (12 MiB, no loss response — KCP's own bargain) measured
    2.66-2.70 / 4.47-5.04 / 0.94 s/Gbit: the same as BBR within noise, so it
    was reverted rather than kept. An 8 MiB initial window instead of 2 is
    actively harmful — `bulk-n` stops finishing at all (26 s harness bound)
    — so 2 MiB is at the edge of what this path absorbs. What is left is not
    a knob: per-byte cost is 1.5x KCP's with the same hub and the same TUN
    writes, so the difference is inside the carrier's own datagram path and
    needs a profile (`perf` on the tool's processes during a `clean` run),
    not another window.
  - **The carrier's own ceiling is 2.39 Gbit/s per stream, so the bench cells
    are carrier-bound, not path-bound (2026-10-24).** A new ignored test
    (`transport::quic::socket::tests::a_stream_carries_at_full_speed`) moves
    256 MiB through one stream over loopback, no relay and no loss: 268 MB in
    0.90 s = 2386 Mbit/s (300 MB/s). That is the same number the 1 % loss leg
    gives for `bulk-1` (2.7), which rules out the path, the windows and the
    controller and puts the cost inside the carrier's own datagram path.
  - **The `cost` failure is upstream of the carrier: the iperf workload's traffic
    never enters the tunnel at all (2026-10-24).** The same cell on both builds
    with `MOLEHILL_L3_STATS=1`, counting claim `2402` (the iperf service):

    | | client flows / offered | server offered / forwarded |
    |---|---|---|
    | UDP carrier (fails) | **0 / 0** | **0 / 0** |
    | KCP (passes) | 5 / 133 971 | 9 451 455 / 8 849 274 |

    Zero frames offered on *both* ends means the visitor's device never produced
    a packet for that service — iperf3 never got a SYN out, or its SYN was
    dropped before any lane existed for the claim — while the three probe
    series on the neighbouring claims ran normally throughout. So the `cost`
    cell is not measuring the carrier's throughput at all on this build: it is
    measuring a cold-start race between the stage's iperf dial and the claim's
    first lane, and the 86 s bound expires while iperf3 backs off.
  - **Both carriers make the same lane moves, so the difference is whether the
    packets ever reach the hub (2026-10-24).** Per run: identical lends (5 in
    both, each claim giving an idle lane to `2401`), and every retired lane for
    `2402` reads `offered=0` in both. Over TCP the surviving member then carries
    121 669 frames and 8.65M injected; over UDP both members read zero. The UDP
    pool dialled 22 lanes overall (two of them for `2401`) and never one for
    `2402`, which is consistent with the allocator's rule — a claim dials when
    it has more *placed flows* than lanes, and no flow is ever placed. The one
    measurement that would close it is a count of packets the device reader saw
    *before* placement, which is five lines in the hub and the next round's
    first change.
  - **The `cost` cell passes over the TCP carrier on the same binary (2026-10-24).**
    Run with no `MOLEHILL_DATA_CARRIER` on the very build that fails over UDP:
    83 s per round, 0.1466-0.1555 s/Gbit, three rounds. So the cell *is*
    measurable, and the failure is carrier-specific after all — this supersedes
    the "upstream of the tunnel" reading. What differs, from the same lane
    counters: over TCP claim `2402` has **two live members** and carries
    (offered 121 669, injected 8.65M); over UDP it has **one live and one dead**
    member and offers nothing. Also: `wait_listener` only inspects listening
    sockets (`ss -Htln`), so the one-shot sink is not being consumed by it.
  - **So the two UDP-lane symptoms seen in this cell are the same defect**: a
    lane that is marked dead and never replaced inside the stage, and a lane
    that accepts frames and delivers none. Both leave a claim unable to carry
    while its neighbours are healthy. The next step is the client's lane
    lifetime over the UDP carrier — every dial, retirement and re-dial for claim
    `2402` in one failing run, next to the same lines from the TCP run.
  - **Lane starvation is not the cause either (2026-10-24).** The per-second lane
    picture is identical in the failing and the passing run: claim `2401` grows
    to 6-9 live lanes in both, every other claim settles at one live and one
    dead member, and `2402` has one live lane throughout both. KCP carries 9.4M
    frames through that same one lane; the UDP carrier's run offers none, so the
    difference is upstream of the tunnel — the visitor's packets for `2402`
    never reach the tool's device on that build. The remaining suspect in reach
    is the harness: `wait_listener` dials the tool's exposed address before the
    iperf test, and the sink behind it is a one-shot `iperf3 -s -1`, so a probe
    connection can consume the only test the sink will serve. That is a method
    question, and it is read from `bench/src/bench/` next, not guessed at.
  - That supersedes the "half-dead lane" reading of the previous round: the
    five frames seen then were one run's partial placement, not the mechanism.
    The `written_bytes`/`write_errors` counters stay (they are what showed the
    difference), and the next step is to watch claim `2402`'s `live=` lane count
    against the probes' claims from the first second of the stage.
  - **The lane now reports what its pump wrote (2026-10-24).** `LaneCounters`
    gained `written_bytes` (what the pump handed to the carrier, as opposed to
    what the queue accepted) and `write_errors`, printed in the per-member
    `l3-stats` line. The re-run sharpened the picture rather than settling it:
    on that run claim `2402` never placed a flow at all (`flows=0`,
    `offered=0` on both ends), so the `cost` cell fails in two different ways —
    a lane that accepts frames and delivers none, and a connection whose flows
    are never placed — and the counters are what will tell them apart once a
    run catches either in the act.
  - **The `cost` cell has a mechanism now: a lane that accepts frames and
    delivers none (2026-10-24).** With `RUST_LOG=debug` and `MOLEHILL_L3_STATS`
    on the failing cell, claim `2402` (the iperf service) has **one** lane with
    **one** flow and the client counts `forwarded=5 enqueued=5 overflow=0
    no_channel=0`, while the server counts `injected=0` for the same member:
    five frames were accepted by the hub and never arrived. The reverse
    direction on that lane works (server `forwarded=10`, client `injected=7`),
    so the lane is half-dead, and iperf3's control connection stalls on the
    first exchange — which is exactly the observed 86 s timeout. The probes'
    claim (`2401`) meanwhile has seven lanes with ~294 flows each and carries
    everything it offers, so this is not load. `MemberGuard::drop` does call
    `vacate`, so the queue has a consumer: the frames reach a pump that writes
    them into a carrier stream that delivers nothing, without an error the
    hub counts. The next step is to make that visible — a per-lane count of
    frames the pump actually wrote, and of write errors — so a quiet pump
    stops looking like a healthy lane.
  - **An in-process multi-endpoint probe stalls, and that is suspicious in itself
    (2026-10-24).** Three lanes, each with its own server socket, one flow each:
    1.09 MB in 120 s. Three flows each: 2.7 MB in 120 s. The bench carries ten
    lanes in one process at 5.8 Gbit/s, so the probe is wrong somewhere — but
    the shape of the failure (nothing flows once more than one endpoint exists,
    while one endpoint is healthy) is the same one that produced 41 re-dials in
    the first session. Until it is either reproduced on the bench or explained,
    it is a lead and not a finding, and the in-process harness is not a number
    this file will quote.
  - **The controller is ruled out a second time, at the current best (2026-10-24).**
    Re-running the fixed-window pipe (12 MiB, no loss response) on top of the
    MTU and 128-datagram-burst changes gives `bulk-1` 3.10/2.89 and `bulk-n`
    5.76/5.18 against BBR's 3.198 and 5.815/5.864: no difference outside the
    noise, reverted again. Also measured this round: the loopback ceiling on a
    four-thread runtime is 2866 Mbit/s against 2817 on one thread, so a single
    stream is not serialised by the runtime — its ceiling is per-packet CPU in
    the engine, and `perf` is not installed in this image.
  - **What is left is the flows-per-lane question.** One flow on one lane
    reaches 3.198 Gbit/s at 1 % loss (KCP 3.26, parity); eight flows on three
    lanes reach 5.86 (KCP 12.0). Since the lane count is the same, the deficit
    is in carrying *several flows* per lane: KCP holds 3.6 Gbit/s per lane with
    8/3 flows on it, this carrier 1.95. The next round measures it in-process
    with one server socket per lane (the earlier probe lost a stream because
    four clients shared one server endpoint).
  - **The lost-lane lead is dead (2026-10-24).** With both stats on
    `MOLEHILL_QUIC_STATS` and `MOLEHILL_L3_STATS`, `bulk-n` shows three
    connections carrying >1k packets on each side and three hub lanes carrying
    frames: the counts match, so no lane is silently down. It is a **download**
    (server sends 1.47M packets per lane, client sends 18k), and each lane runs
    at 1.9 Gbit/s against the 2.82 single-stream ceiling — a 33 % concurrency
    loss that both carriers pay, ours with a higher per-packet cost underneath.
    Two knobs were tried against this and rejected on measurement: an MTU of
    1472 (ceiling 2680 against 2817 at 1452) and `packet_threshold(2)`
    (`bulk-n` 5.86 -> 5.25/3.43, `bulk-1` 3.198 -> 2.71/2.81). Neither is the
    missing 33 %, and the conservative value of each stays.
  - **A probe that will not run, and what it suggests (2026-10-24).** An
    in-process version of the many-lane cell (four client endpoints, one
    server endpoint, no hub) never gets its fourth stream: three arrive, the
    fourth times out even when nothing is writing yet. It could be the test's
    own shape, but it is worth chasing because the same symptom — a lane that
    never carries — is what made the pool re-dial 41 times in the first
    session, and it would depress every many-lane cell. The next round
    confirms it on the bench instead: `MOLEHILL_QUIC_STATS=1` on `bulk-n`,
    count how many connections carry data against how many lanes the hub
    reports.
  - **KCP's eight-stream cell uses the same three lanes and gets 3.6 Gbit/s
    per lane (2026-10-24).** `bulk-n` with `MOLEHILL_L3_STATS=1` on a KCP-only
    build: 10.73-10.86 Gbit/s over three members (2.92M + 2.80M + 2.02M
    frames), i.e. **3.6 Gbit/s per lane** against this carrier's 2.03 — and our
    own single-lane ceiling is 2.82, so the deficit is two things: per-lane
    cost (1.28x, the carrier's packet path) and a 28 % loss of per-lane
    throughput once three lanes run at once. The lane count is not the
    difference; both carriers got three.
  - **Fewer acknowledgements are slower, not faster (2026-10-24).** The
    ACK_FREQUENCY extension at one ACK per eight ack-eliciting packets (5 ms
    cap, immediate ACK on reordering) moved the loopback ceiling 2817 -> 2561
    Mbit/s and was reverted. The reasoning it was tried on — that a one-way
    transfer spends half its packets on ACK frames — is true by count and
    false by cost: ACK packets are ~60 bytes, and the sender's growth depends
    on them arriving often.
  - **A 128-datagram receive burst is the biggest single win so far
    (2026-10-24).** The loop took 32 already-queued datagrams per readiness
    wake; at these rates a wake carries more than that, so the endpoint was
    paying a timer sweep, a transmit sweep and an event pass every 32 packets.
    128 moves the loopback ceiling 2726 -> 2817 Mbit/s and the cells to
    `bulk-1` 3.198 (KCP 3.26 — parity inside this run's noise), `bulk-n` 6.099,
    and clean 0.70 s/Gbit (7.7 Gbit/s, KCP 12.81 at 0.624). Per byte the clean
    path is now within 13 % of KCP; the remaining clean gap is that KCP spreads
    over 8 cores where this build uses 5.4.
  - **40 datagrams per transmit measured the same as 32** (2809 against 2817),
    so 32 stays; the earlier 64-datagram trap was a 76 KiB `UDP_SEGMENT`
    message over the kernel's limit, not the batch itself.
  - **A 1452-byte datagram instead of 1200 is worth 4-14 % (2026-10-24).**
    QUIC's 1200 is a guaranteed minimum, not a choice, and this carrier's cost
    is per packet: the loopback ceiling moved 2386 -> 2726 Mbit/s, `bulk-1`
    2.70 -> 2.80, `bulk-n` 5.04 -> 5.64, and the clean cell 0.94 -> 0.82 s/Gbit
    (5.7 -> 6.6 Gbit/s). Path MTU discovery stays off; 1452 is safe under a
    1500-byte link, which is what every path this carrier rides on carries.
  - **The next lever is the send path's ownership, not a knob**: quinn copies
    each write into its stream buffer and again into the packet it builds,
    because our seam is `AsyncWrite` and hands it a `&[u8]`. The hub already
    builds `Bytes` batches, and KCP's carrier takes them by reference
    (`send_owned`); a `send_owned`-shaped method on the carrier seam would let
    the UDP carrier take the same `Bytes` and drop one of the two copies.
  - **Where the rest has to come from**: per-byte cost is still 1.5x KCP's
    (0.93 against 0.62 s/Gbit), and the multi-lane cell scales worse than
    KCP's (three lanes give us 1.9x where KCP gets 3.7x), which points at the
    per-connection receive task — one core each, no batching across lanes —
    rather than at the socket path. Profiling the receive side is the next
    step, not another knob.
- **The write path could strand a frame, and never told the engine it was
  blocked (2026-10-24).** Two defects in `transport/quic/conn.rs`, both proved by
  one test: `poll_write` handed the engine a whole framed chunk and reported the
  caller's bytes as written even when the engine took only part of it — so
  `write_all` returned, no further write came, and the peer waited for ever for
  the rest of a frame that never left. `flush` had the matching fault: it
  stopped after the first short write, and the engine only publishes a stream as
  writable again after a call returned `Blocked`, so the waker a blocked writer
  parked was never fired. A lane is exactly this shape — write a batch, then
  wait for the answer — and the symptom is a carrier that carries nothing while
  both engines sit idle.
  - `flush` now loops until the buffer is empty or the engine refuses outright
    (that last call is what registers the blocked stream);
  - `poll_write` claims only bytes that are all in the engine, parks otherwise,
    and remembers the claim (`pending_from`, `owed`) so the retried buffer is
    reported instead of written twice;
  - `transport::quic::socket::tests::a_partially_accepted_frame_still_reaches_the_peer`
    is the reproducer: 8 KiB of send credit against a 10 KiB write, no flush,
    and the peer must still see all ten. It failed before the fix (5 s timeout,
    nothing past the window) and passes now.
  **This did not fix the `cost` cell** — that run still times out, with the
  series probes answering at 20-42 ms throughout, so the stall is upstream of
  the carrier. The bug is real and worth its commit regardless: it is the
  failure shape of any lane whose batch does not fit the window.
- **The `loss1` cell is a mixed-workload defect, not a bulk-path one
  (2026-10-23).** Three measurements on the `l3` arm at `loss1`
  (`--condition-leg tunnel`), forced to the UDP carrier
  (`MOLEHILL_DATA_CARRIER=kcp` on a `quic` build):
  **Corrected 2026-10-24: KCP was measured live on the same cells, and the
  carrier is behind it everywhere — the "0.41 Gbit/s" this file used to quote
  was a stale figure from an older code state.** One KCP-only build of the same
  commit, same arm, same conditions:

  | cell (`loss1`) | UDP carrier | KCP | ratio |
  |---|---|---|---|
  | `bulk-1` | 0.454 Gbit/s (1.049 s/Gbit, 0.57 cores) | **3.26-3.45** (0.146, 0.41) | 7.2x |
  | `bulk-n` (8) | 2.73-2.78 (0.985, 2.49) | **11.99-12.38** (0.232, 2.51) | 4.4x |
  | `cost` | 86 s timeout | **4.36** (0.148, 0.67) | — |
  | `clean` `cost` (control) | 4.85 (1.072, 5.12) | **12.81** (0.624, 8.0) | 2.6x |

  **Updated 2026-10-24, after the controller's initial window, the transmit
  batch and the flow-control windows were raised: `bulk-1` reads **2.405
  Gbit/s** (from 0.454; KCP 3.26) and `bulk-n` **2.57** (KCP 12.0).** The
  single-stream cell was a window problem end to end — ten segments of initial
  window, 8 datagrams per paced wake and an 8 MiB stream window against a 7.5 MB
  bandwidth-delay product kept both ends idle — and the eight-stream cell did
  not move with the same change, so its gap is somewhere else (per-stream
  fairness across eight connections, or the hub's batch schedule) and is the
  next lever.
  Two facts fall out of the pairs. The loss1 gap is **not CPU** — 0.454 Gbit/s
  costs 0.57 cores where KCP's 3.26 costs 0.41 — it is bytes in flight: KCP holds
  a bandwidth-delay product (8.5 MB at 4 Gbit/s and a 20 ms leg, inside its
  8192-segment window), while the engine's own counters read a cwnd under 1 MB
  and both ends idle. The clean gap *is* per-byte cost: 1.072 s/Gbit against
  KCP's 0.624, so the frame copy, the record seal and the lock per 16 KiB chunk
  are all still on the table.
  - The `cost` cell is a separate defect: the 4-stream iperf never finishes
    there (86 s timeout, every round) while `bulk-n` carries eight streams at
    2.7 Gbit/s, and the three series probes keep answering at 20-42 ms through
    the whole stall — so the tunnel is healthy and *iperf3 itself* never gets a
    transfer going on that service. It is not a bulk-path failure and it is not
    the gate's only cell; it is queued behind the two gaps above.
  - `--scenarios udp-blast` at `loss1`: **82.6 % of the offered datagrams do
    not arrive**, 29.5 Mbit/s received. A reliable stream behind an
    offered-rate blast is a queue question rather than a network one (the hub's
    batch queue sheds first), and it is the second cell to understand.
  - **Two of the mix's three series are healthy on their own**: `churn-16`
    reads 338 connections/s at a 20 ms setup p50 and a 45 ms rtt p99, and
    `rr-1` reads 45.6 request/s at 88 ms p99 — one round trip per request on a
    20 ms path, not a collapse. That leaves the datagram series and the
    *concurrency* of the four workloads as the untested parts of `cost`; the
    blast's 82.6 % points at the datagram flow first.
- **The receiver's 212 KiB socket buffer was reading as 34 % packet loss
  (2026-10-23).** No QUIC endpoint set `SO_RCVBUF`/`SO_SNDBUF`, so every
  carrier socket kept the kernel default; KCP asks for 32 MiB in
  `KCP_SOCKET_BUF_BYTES`. On the `loss1` cell with `MOLEHILL_QUIC_STATS=1`
  the busiest lane reported `sent` 2.2 M, `lost` 757 k (**34 %** of what it
  sent), a cwnd inflated to 281 MB, and an app-level read gap of **677 ms**,
  while the app only offered ~200 pps. `socket::spawn` now requests 8 MiB each
  way (`SOCKET_BUF_BYTES`, clamped by `net.core.{r,w}mem_max`): `lost`
  **1.3 %**, read gap **129 ms** on the same cells, and the per-lane loss the
  earlier entry blamed GSO for is gone. GSO was not the mechanism: the
  in-process harness below carries 8 MiB in ~0.39 s (**173 Mbit/s**) with the
  same 1 % loss whether a drop costs one datagram or a burst of eight.
  - **A lossy path you can run in a test**:
    `transport::quic::socket::tests::a_stream_carries_over_a_lossy_link`
    (`--ignored --nocapture`, `--release`) puts two engines on real sockets
    with a 10 ms, 1-in-100 relay between them and asserts a 50 Mbit/s floor.
    `MOLEHILL_TEST_LOSS_BURST=8` makes one drop cost a segment batch. The
    relay is the test's own ceiling, so the number is a floor, not a
    measurement of the carrier.
  - The `MOLEHILL_QUIC_STATS` line now carries a per-connection `id` (the
    engine key is only unique per endpoint) and the **server** endpoint reports
    one per connection too, which is what made the 34 % attributable.
- **The UDP carrier's clean path improved 4x, and its `loss1` cell is now the
  open failure (2026-10-23).** The sender was moved into its own task (a
  `send_to` waiting on a full qdisc must not stop receive; that was the PTO
  spiral) and the clean L3 cell read **4.26-4.42 Gbit/s at 1.13 s/Gbit, p99
  3.8-5.2 ms** (from 3.39-3.63 at 0.95, p99 5.9-7.4). CPU per Gbit is 1.8x
  TCP. Then `loss1` — the cell the whole design is argued on — failed: the
  20 s, 4-stream iperf did not finish inside the harness's 86 s bound on
  either arm, at `--scale 0.05` or `0.1`, on every build tried (the ordered
  build, the receive-burst build, and the split build). It is not a dead
  lane: the l3-stats kept counting (server `forwarded` ~200 pps, client
  `injected` ~140 pps) with read gaps up to **774 ms**, and per-lane
  `MOLEHILL_QUIC_STATS` read rtt **20-21 ms**, cwnd **240-300 KB**, 5-14
  losses/s and ~1 000 datagrams/s sent — a few Mbit/s per lane, not zero, and
  far below KCP's 0.41 Gbit/s on the same cell. The likely mechanism is the
  bench's per-skb loss: with 8-datagram GSO a 1 % skb loss is an ~8 %
  datagram loss, and the ordered stream's head-of-line block turns it into
  the 774 ms read gaps; the engine's controller response then settles at a
  few Mbit/s. That is a hypothesis, not a verdict: the next step is a
  server-side stats reporter and a deterministic-loss unit test over two
  in-memory drivers, before any more clean-path tuning. **Until loss1 is
  understood and met, the gate's premise is unmet: no flip, KCP stays.**
  The investigation also found that a UDP lane was reported as `carrier=tcp`
  in l3-stats (the `ClientStream::Udp` arm fell through to `Raw(_)`); fixed
  in the same arc.
- **The datagram layer moved the UDP carrier 3.5×, and the clean gate is still
  missed (2026-10-23).** Two changes to the endpoint's socket loop, measured on
  the same L3 `cost`/`clean` cell, `--condition-leg tunnel`, `--scale 0.1`,
  forced UDP carrier, with the run's A/A copy:
  - **One wake serves a burst.** After the datagram that wakes the endpoint,
    up to 32 already-queued datagrams are taken with `try_recv_from` before the
    loop's single `drain` (one timer, one transmit sweep, one event pass):
    1.04 Gbit/s at 3.25 s/Gbit became **1.65-1.71 at 1.91-1.95**.
  - **A multi-datagram transmit goes out as one `UDP_SEGMENT` message**
    (Linux; `sendmmsg`/per-datagram fallback, remembered after a refusal):
    **3.33-3.68 Gbit/s at 0.93-0.98 s/Gbit**, p99 **6.7-7.7 ms** (from 187-222
    with the receive burst alone — the sender's queue was the tail).
  - **BBR and an 8 MiB stream / 16 MiB connection window** (`params.rs`, the
    engine configuration both ends now use) read **3.39-3.63 at 0.95-0.97**,
    p99 5.9-7.4: no throughput change beyond the A/A floor, so they are kept as
    the design's declared bound, not claimed as a win.
  Against the TCP pool's ~12 Gbit/s at ~0.62 s/Gbit on the same model and host,
  the CPU per Gbit is now **1.53× TCP** (the gate's line is 1.5×) and the clean
  throughput is still **~3.4× short** of the 15 % window. The gap is not the
  flow-control window any more (doubling it did nothing); the next levers are
  the stream/connection lock contention and `recvmmsg` (one syscall per receive
  burst), not more window. The gate remains unmet; nothing flips.
- **The pool rides the UDP carrier, and its clean-path number says do not
  flip the order (2026-10-23).** The L4 pool now has a second implementation:
  a tunnel is one QUIC connection and every data channel is a stream on it
  (`ClientTunnel::from_carrier`, `channel::connect_connection`). The server's
  first stream on a connection decides what the connection is — a direct
  channel (the lane path, unchanged) or a pool tunnel, whose later streams are
  routed by the 8-byte service prologue — and a connection-loss notice releases
  the tunnel's `max_tunnels` slot instead of waiting for a session teardown.
  Proof, on the stage `cost` scenario with the carrier forced
  (`MOLEHILL_DATA_CARRIER=kcp`, recorded in the run's `molehill_switches`),
  `--condition clean --condition-leg tunnel --scale 0.1 --rounds 1 --arms
  molehill` plus the run's own A/A copy: **4 tunnels established on the
  carrier, 8/8 samples ok**, throughput **0.87-0.96 Gbit/s**, CPU
  **3.77-3.91 s/Gbit**, p99 68-96 ms. The TCP pool on the same model and host
  carries about **12 Gbit/s at ~0.62 s/Gbit** (the 2026-10-22 first-carrier
  measurement). The A/A pair agreed on throughput within 4 %, so this is not
  scatter: **the `clean` cell of the plan's gate is missed by an order of
  magnitude, and Gate 2 is not met. Per the gate's own rule, nothing flips and
  KCP stays.** The gap is CPU, not loss: the path moves 2.1 M small datagrams
  for 2.6 GB, one `send_to` per engine datagram, and the record layer's
  seal/open on every write. **A buffer-reuse pass did not move the number**:
  the per-read 65 KiB scratch (only its length was ever used, so the allocator
  had already elided it) is gone, the write frame is built in its pending
  buffer, and the driver's engine scratch is reused — the lane cell then read
  1.03-1.07 Gbit/s at 3.20-3.31 s/Gbit against 1.03-1.09 at 3.21-3.30 before
  it (`/home/dsh/tmp/udp-alloc-l3.json`). The next lever is the plan's own
  datagram layer — `udp_batch`/`UDP_SEGMENT` on send and `recvmmsg` on receive,
  so one syscall and one wake cover a burst — plus the measured transport
  parameters, not more allocation work and not a carrier decision. The formal
  `--ab-arm` gate (two builds, one fingerprint, `loss1`/`rtt100`/`clean`) is
  still owed; this cell is a smoke, not the record.
- **The UDP carrier's lane carries end to end, and the two bugs were
  ordering (2026-10-23).** The first working smoke of a transparent claim over
  the QUIC-shaped carrier: the same `stage` run, `--condition clean`,
  `--condition-leg tunnel`, `--scale 0.2`, forced to the UDP carrier, two
  rounds plus its own A/A copy — **8/8 samples ok, 93.4 s** against the
  profile's 150 s budget (`/home/dsh/tmp/udp-ordered.json`). The lane had been
  silently dead in two independent ways, and both were in the stream handle,
  not the engine:
  - **A multi-datagram `poll_transmit` was sent as one datagram.** `quinn-proto`
    fills one buffer with up to `MAX_DATAGRAMS` datagrams and marks it with
    `Transmit::segment_size`; the driver ignored the marker and the socket sent
    the whole buffer in one `send_to`. A 1-RTT packet has no length field — a
    short header extends to the end of the datagram — so the peer parsed the
    next packet's first byte as a frame type and closed the connection with
    `invalid frame ID`. `Outgoing::datagrams()` now splits at the segment size,
    and one test pins the split.
  - **Stream reads were unordered.** `RecvStream::read(false)` returns segments
    in arrival order with their offsets; the handle appended them as if
    contiguous. Under any retransmission a later segment arrived first and the
    byte stream shifted. The first 32 frames of a 1.6 MiB write arrived intact
    and the rest were a frame ahead. `read(true)` is the fix; the record layer's
    framing cannot reassemble by offset.
  Two robustness fixes landed with them because the same tests exercise the
  shapes: the connection lock is held across a reader's park (so a `Readable`
  event cannot fire into the gap between the failed read and the registration),
  `park` refuses to register after `close` (no lost `ConnectionLost`), and
  `poll_flush`/`poll_shutdown` drain a short write's tail instead of dropping
  the last frame. Proof: two new tests in `transport::quic::socket` — 1.6 MiB
  one way, and 1.6 MiB each way concurrently past the first stream window. The
  carrier's own smoke also stopped re-dialing: no `carrier connection is gone`
  in either end's log. The lane is the first thing on this branch that carries
  visitor traffic over UDP; the pool followed (see the entry above), and its
  clean-path measurement is what keeps the carrier order TCP-first.
- **The carrier rule's first live measurement: it fires, it costs the tail, and
  its handover was the reason (2026-10-23).** Two builds of the same commit,
  differing only in the rule's default (on against off — the trick the GSO A/B
  used, so one fingerprint holds both), on `--profile stage --condition
  udp_throttled --condition-leg tunnel` at 60 s stages, two rounds plus the
  run's own A/A copy. What it did: **one probe per rule-on arm** (`carrier rule:
  suspicion confirmed; probing the other carrier with one tunnel probe="kcp"`)
  and none in the rule-off arms; throughput and CPU per Gbit did not move
  (12.414/12.432 against 12.221/12.568 Gbit/s, 0.636/0.634 against
  0.641/0.633 s/Gbit). What it cost: the interactive tail — p99 **72.9 and 2.0**
  ms on the two rule-on cells, **72.3 and 63.6** on its A/A twin, against
  **2.01 and 2.33** with the rule off, i.e. 30x on three of the four rule-on
  cells with the twin agreeing (the model labels it directional at n=2 and
  cannot resolve p99 by rounds at this scatter). Why: the probe's handover never
  landed. `MOLEHILL_POOL_STATS=1` shows `handover=2/kcp:draining` for the rest
  of the arm — the UDP policer starves the KCP dial, so the drained tunnel stays
  drained, the pool runs a tunnel short, and `probe_judge` waited out its whole
  30 s budget with nothing to compare. Fixed in the same commit: the probe's
  handover gets **5 s** to put a tunnel on the other carrier, and a carrier that
  does not establish inside that is the verdict (give the tunnel back, back off
  for 10 minutes) — a carrier the path cannot reach is a result, not a wait.
  **The re-measurement with both fixes ran** (same command, two fresh builds at
  `ba4c9c5`): the probe is given back after 6 s with `reason="the probed carrier
  did not establish"`, and `MOLEHILL_POOL_STATS=1` shows the drain for **5 s
  instead of the rest of the arm** (`handover=0/kcp:draining` for five samples,
  then `-`). The tail came down with it: p99 39.5-45.0 ms in three of the four
  rule-on cells and 3.1 in the fourth, against 63.6-77.0 before the cancel and
  2.17-2.27 with the rule off; throughput is unmoved (12.371-12.590 against
  12.409-12.439 Gbit/s). So the remaining cost is **the probe's own 5 s
  establish window**, not a stuck handover — and because the backoff is ten
  minutes, a deployment pays that window roughly once per ten minutes rather
  than once per 60 s stage, which is why the arm-to-arm number overstates it.
  Still not a claim (n=2, and this cell's p99 resolution is 41 %), and the rule
  stays opt-in: the next lever, if it is ever to be on by default, is for the
  probe to **dial an extra tunnel** on the other carrier and close it rather than
  drain a serving one, so the comparison costs no width at all.
- **The carrier rule can now ask its question on either carrier (2026-10-23).**
  It had exactly one suspicion — a sender's re-sent share — and that counter
  exists only on KCP, so a TCP-carrying pool could never probe the other
  carrier: the peer-level choice was one-way by construction. It now has a
  second suspicion the *path* answers: the pool's median tunnel round trip
  against a floor it keeps (drifting up 1/64 per window, ~44 s to double, and
  lowered at once by a faster reading), so what fires is the transition — the
  path getting worse — while a path that is merely always busy settles at its
  own floor and stops asking. The probe's verdict gained the round trip as its
  tie-breaker: when the carried bytes are level (the 1.5x edge) the carrier that
  answers in half the time wins the pool, at a 2x edge, and a missing
  measurement decides nothing. Both signals read the counters `f8288da` added,
  and both decisions are pure functions with tests (a steady queue must stop
  probing; a carrier that carries less is not redeemed by answering faster).
  Still off by default behind `MOLEHILL_TUNNEL_CARRIER_RULE=1`; its end-to-end
  measurement is the open thread below.
- **The shaped carrier campaign ran (2026-10-23), and its first finding was a
  regression of my own: the KCP window retune also narrowed the *receive*
  window.** `set_wndsize(want, want)` sized both from the session's own send
  rate, and a bulk *receiver* sends almost nothing — so it advertised the
  floor (128 segments, 188 KB) to the peer that was sending it data. On a clean
  100 ms carrier path (`--condition rtt100 --condition-leg tunnel`, KCP pinned,
  4 streams, two rounds, `--ab-arm` against the previous build, the run's own
  A/A copy): **0.613 / 0.824 Gbit/s against 0.022 / 0.030** — a 28× loss, claim
  +2679.5 %, CPU per Gbit 1.158/0.995 against 1.767/1.873, and the A/A twin
  (0.614 / 0.620, 1 % apart) says it is not scatter. On the lossy leg the same
  comparison reads +36.05 % (0.322 against 0.190-0.256) because that cell is
  dominated by the ARQ collapse below. The counter that named it was
  `MOLEHILL_KCP_STATS`: the sending end's `wait_snd_peak` stood at 11 399
  segments against `peer_wnd` 128, with its own `snd_wnd` never the constraint.
  Fixed in `apply_window` (the send window follows the path, the receive window
  stays at its configured bound — it is the peer's allowance, not our sending),
  with a unit test that fails against the old rule, and the window in force is
  now read from the engine instead of a copy kept in the pacer.
- **GSO's win survives a lossy carrier path, so it needs no guard.** The same
  campaign's A/B (GSO build against the same commit with `gso_allowed()` forced
  false): on `loss1` over the carrier leg **+18.24 % throughput** (claim,
  0.252/0.193 against 0.188/0.189) with CPU per Gbit −32 % and p50 −50 %
  (directional), and on `clean` **+14.15 %** (8.99/8.74 against 8.40/7.13)
  with p99 and RSS worse by direction (83.9/16.6 ms against 50.1/4.2; 159/128
  MiB against 114/85). The mechanism held: the leg's own qdisc drops were
  1.006 % of 484 880 offered **frames**, and those frames are 64 KiB with
  segmentation offload on — the loss arrives in bursts and still pays. What
  `loss1` costs on this path is not GSO but the ARQ: the same code carries 9
  Gbit/s clean and 0.2-0.3 under 1 % frame loss, which is the documented
  congestion collapse (docs/benchmarks.md, "Loss on the carrier's own leg"),
  unchanged by anything in this cycle. The consequence for the carrier policy is
  the operator's own direction: KCP is the carrier for the paths where TCP is
  *refused*, not the carrier for lossy paths, and the peer-level selection has
  to weigh reachability before quality.
- **A campaign's `--condition` is now imposed or refused, so a shaped cell is a
  shaped cell (2026-10-23).** The staged runner took the *scenario's* own
  condition (`cost` walks the `single` timeline → `clean`) and imposed it
  *after* `_open_topology` had imposed the campaign's, so a run labelled
  `loss1` measured a clean path while its method record claimed otherwise —
  three such runs came back at 7.03–7.08 Gbit/s with zero qdisc drops. Now:
  a scenario whose own condition is a *single* one has it replaced by the
  campaign's (`model.effective_conditions`); a scenario that scripts a
  *schedule* of several refuses `--condition` with an argument error before the
  first namespace exists; the capacity ramp takes the campaign's condition and
  puts it back afterwards (it used to leave its own on the leg for every later
  scenario of the arm); and an MTU class is undone by a condition that names
  none, with `meta.mtu_restore_to` written for the first time (the page had
  claimed it since the class was added). Proof on `cost` (4 streams, 20 s,
  `--profile stage --scale 0.2`, one arm plus the run's own A/A copy):
  `clean` 12.01 / 11.77 Gbit/s, qdisc drops 0, p50 0.80 ms, p99 1.6 ms, UDP
  loss 0 %; `loss1` 11.27 / 10.94 Gbit/s, **6 062 qdisc drops = 1.0 % of
  484 880 offered frames**, p50 41 ms, p99 1 074 ms, UDP loss 4.0 / 1.8 %.
  Two method facts came out of the reading: a loss class is **per frame** (with
  segmentation offload the frames were 64 KiB, so `loss1` is 1 % of the bytes in
  ~64 KiB bursts), and the drain predicate can burn its whole 60 s budget
  (one of the four drains above did) because a probe's standing socket counts
  as "can still send". Both are on `docs/benchmarks.md`.
- **The KCP window follows the path (2026-10-23), and the only effect that
  cleared the run's own noise is memory.** Fixed 8192 segments (about 12 MB per
  session) became "**the send window** sized to twice the bandwidth-delay
  product, clamped to 128..8192, retuned once a second on a session that is not
  re-sending" (the receive window is the peer's allowance and stays at its
  configured bound — see the entry above for the 28× loss that rule cost on a
  long path when it did not). The A/B on the
  forced KCP carrier (`--profile screen --ab-arm --aa --rounds 6`, the run's own
  A/A copy): peak RSS 189.0 -> 147.5 MiB, **the one reading that cleared its
  floor** (10 %, p 0.031 against alpha 0.05, 6/6 rounds); throughput 8.78 ->
  9.06 Gbit/s, cpu_s_per_gbit 1.309 -> 1.383, p99 10.3 -> 18.0 ms — all three
  inside this run's scatter, and the p99 cell's own A/A pair disagreed by 154 %
  (10.3 against 27.2), so the tail is *not* a reading from this run. The
  direction of the trade is worth keeping: a smaller window means smaller GSO
  batches, so CPU per byte and the carried packet (8.8 -> 7.7 KiB) move against
  the previous change while the bytes in flight — and with them the queue —
  fall. Shipped as a *bound* (an arbitrary 8192 is not a design), not as a win;
  the tail needs a run this model can resolve, which the shaped campaign below
  makes possible for the first time.
- **The root dependencies are current (2026-10-23).** `cargo outdated
  --root-deps-only` is clean: `parking_lot` 0.13 (with `lock_api` 0.5,
  `parking_lot_core` 0.10), `tokio` 1.54.1, `libc` 0.2.191, `tun-rs` 2.8.13,
  plus the transitive set. No code change was needed. One property moved without
  a call site: parking_lot dropped **automatic eventual fairness**, so the mux
  engine's `Mutex` no longer gets the thread-queue fairness it used to inherit —
  nothing here uses a fair-unlock API, and the locks are held for a few
  instructions, but a contention-heavy run is now the only thing that would show
  it, which is why the next sweep's `rtt_p99` cells get a second look rather
  than an assumption.
- **The L4 data plane's cost was decomposed with the bench model (2026-10-22),
  and the carrier dominates every other term.** Three runs on this host, no
  claim beyond what each one supports: `--profile stage` (24 samples, the run's
  own `molehill~aa` copy as its noise floor) reads the clean cell as control
  (no tool) 49.2 Gbit/s / p99 2.66 ms, molehill L4 7.06 Gbit/s (A/A 7.02) at
  12.2 cores and 1.74 s/Gbit with p99 9.83 ms (A/A 13.22 — this cell's scatter
  is ±35 %, so no p99 claim is available from one run), L3 5.85 Gbit/s at 7.13
  cores with p99 2.10 ms. Two `--profile screen` runs on the same fingerprint
  isolate the carrier by the documented diagnostic switch: **KCP (the default)
  7.11 Gbit/s, 12.23 cores, 1.73 s/Gbit, p50 1.02 ms, p99 7.96 ms; TCP
  12.31 Gbit/s, 7.74 cores, 0.64 s/Gbit, p50 0.84 ms, p99 1.59 ms** — +73 %
  throughput, −63 % CPU and −80 % p99 for the same workload. `perf` on this
  host (reconnaissance, no claim) puts the two daemons' CPU at ~50–65 % kernel
  packet path, ~15 % the Noise AEAD and ~15 % libc memcpy/malloc, with the rest
  spread over the layers, so neither framing nor allocation is where the cost
  is. An ad-hoc topology (8 bulk streams + 8 interactive threads, not the model)
  agrees: KCP 8.7 Gbit/s / p50 2.9 ms / p99 6.7 ms, TCP 15.7 / 0.84 / 2.4, KCP
  with `data_enable = false` 9.5 / 1.99 / 4.74 — the data-plane AEAD is worth
  ~9 % of throughput, the userspace ARQ ~45 %. Both are *product* decisions (a
  carrier policy that prefers TCP on a clean path, or a cheaper AEAD), not
  defects to patch. What was fixable in the framing is in `5632f66` (no memset
  and no per-chunk allocation; measured at the noise floor, so no claim).
- **The carrier order was reversed on that evidence (2026-10-22): TCP first,
  KCP as the fallback.** The A/B of the change itself (`--profile screen
  --ab-arm molehill --aa --rounds 6`, the two binaries interleaved on one
  fingerprint, `cost[clean]`, the run's own A/A copy as its noise floor): old
  (KCP-first) 7.060 Gbit/s, 1.743 s/Gbit, 12.25 cores, p50 0.988 ms, p99
  5.77 ms, 79.1 MiB peak RSS; the A/A copy 7.065 / 1.740 / 12.21 / 0.992 /
  8.77 / 87.1; new (TCP-first) **12.518 Gbit/s, 0.626 s/Gbit, 7.77 cores, p50
  0.823, p99 1.79, 49.2 MiB** — +77 % throughput, −64 % CPU per Gbit, −69 %
  p99, −38 % peak RSS, and `wire_per_visitor_byte` 1.068 → 1.008, against an
  A/A scatter of 0.07 % (throughput) and 0.2 % (CPU). All six rounds agreed and
  the paired sign test reads p 0.031 against an alpha of 0.05; the model still
  labels the comparison *directional*, because the run was not pre-registered
  (`--primary` was not passed) — the floors themselves were cleared, with
  `cpu_s_per_gbit`'s 0.66 % floor under an effect of 178 points of it.
  `rtt_max_ms` is the one cell that got worse (23.9 → 39.2 ms, a single tail
  event); the p99 and the median both improved.
  The full stage schedule (`--profile sweep --scenarios timeline --scale 0.3`,
  one round per stage) is where the trade shows: throughput clean 6.99 → 14.94
  Gbit/s and loss1 6.90 → 12.97, equal on the shaper-limited cells (rate100
  0.094, rate20 0.019, jitter 0.019 both ways), and CPU per Gbit down on every
  stage (clean 1.73 → 0.65, loss1 1.71 → 0.58, loss5 1.43 → 0.49, rate100 4.66
  → 2.78). The interactive tail is the price: that schedule's one round per cell
  reads clean p99 8.2 → 81.7 ms and loss1 286 → 1076 ms, while loss5 improves
  4023 → 2088 ms and the repeat clean stage 14.6 → 4.0 ms — the interleaved
  A/B's median p99 (7.82 → 1.68 ms, five rounds) is the better-controlled
  reading of the same cell, and the stage cells are one round each. This is the
  documented "KCP wins interactive round trips on a lossy leg" case, now with
  its throughput price beside it.
  The records are `/home/dsh/tmp/t-new.json` (new default) and `t-kcp.json`
  (KCP-first, forced by the switch); their fingerprints differ only because a
  bench-model condition description changed in the same commit (the engine hash
  covers those bytes), which is worth remembering before comparing them.
- **v0.11.2 is released, and its sweep is the next baseline (2026-10-20).**
  The published run is the second one: 19:19–21:04 UTC (6265.5 s, 90 samples) at
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
  build's data path). Charts and README are refreshed from the record. The
  release workflow then built all ten archives, published the GitHub release
  (`v0.11.2`, 2026-10-20), the GHCR image and crates.io `molehill-rathole
  0.11.2`.
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
- **The repository state**: v0.11.2 is released; the tag names `64d8ebc`
  (the release commit), and `main` carries it plus this record.
  `CHANGELOG.md` keeps the dated `## [0.11.2] - 2026-10-20` section and an
  empty `[Unreleased]`; the next cycle is not open yet. `v0.11.1` names
  `4874f05`, `v0.11.0` names `f3295e8`. The merged `feat/v0.11.2` branch
  was retired locally and on `origin` on 2026-10-22;
  `backup/v0.11.2-presquash` (24 pre-squash commits, its tree superseded by
  `main`) is the only local non-`main` branch left, kept as the squash's
  recovery point. The measured v0.11.2 revision is `69e8bd9` (`tree_clean`
  true; the record commit is docs/assets only, so the tag review's drift check
  held). The previous cycle's branches
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

- **QUIC carrier, in progress: the engine wants one key handover per packet
  space, and the 1-RTT upgrade additionally precomputes a key update
  (2026-10-23, understood).** Where it stands: `src/transport/quic/` holds
  the socketless driver, the socket task, the stream handles and a
  Noise-driven `Session`; the handshake completes over a real loopback
  socket and a stream now carries bytes both ways through it. The two
  requirements that cost the most to find, both now in the code:
  - **One handover per space.** The engine upgrades `Initial → Handshake →
    1-RTT` and asks `write_handshake` again for each; a session that hands
    its keys once leaves a connected connection with no 1-RTT keys, so it
    acknowledges nothing and retransmits its last handshake packet forever
    (an endless 45-byte datagram was the tell).
  - **`next_1rtt_keys()` must answer.** Installing the 1-RTT space calls it
    and `.expect("handshake should be complete")`s on the result
    (`connection/mod.rs:2197`); returning `None` panicked there. A
    pass-through pair is the answer — a peer-initiated update then "updates"
    to the same keys.
  The marker byte each side sends in the Handshake space stays: the engine
  promotes a connection to established only while handling a Handshake packet,
  and KK has no third message.
  **The phase order moved, and the plan's Gate 1 with it.** The plan put the
  direct-channel wiring and "Gate 1 on the L3 arm" before the carrier seam.
  That cannot hold: a transparent lane rides the *carrier*, so the L3 arm
  cannot run on QUIC until the seam exists and the UDP carrier implements it.
  The order is therefore the seam (P2), then the UDP implementation behind it
  (P3), then the gates — Gate 1's cells are measured as part of Gate 2's run
  rather than before it. The carrier's name stays `Kcp` until the seam makes
  the carrier choose an implementation: renaming it to `Udp` while the bytes
  are still KCP's would be a lie in the other direction. The direct-channel
  primitive (`connect_stream`) is in place and tested, so wiring it into
  `ClientStream` can wait for the seam instead of landing twice.
  **The seam is landed, tested, and gated on the `quic` feature.** It carries
  `CarrierIo`/`CarrierStream` (one boxed stream type above the line),
  `CarrierConn` (open a stream, name the carrier, say whether it is alive,
  report `CarrierStats`), and two implementations: `MuxConn` for the yamux
  session (which rides either TCP or the KCP carrier, so the carrier is a
  field, not the type's name) and `UdpConn` for a QUIC connection. One
  contract function — write, echo, read back, still alive, report stats — runs
  against both. Nothing in the default build holds a `CarrierConn` yet and
  the gate denies dead-code warnings, so the types un-gate in the commit that
  makes the pool hold one.
  **The structural fact worth keeping:** yamux needs a byte stream, while QUIC
  hands out a stream *factory*. That asymmetry is why the seam sits at the
  stream level and why each carrier owns its connection type; a seam at the
  byte-stream level cannot express the UDP carrier at all.
  **The remaining order, each step green on its own:** (1) the pool holds the
  seam — done (below); (2) the lanes dial through the UDP carrier — a lane *is*
  a direct channel, so this is `connect_direct_channel`'s QUIC branch plus the
  server's UDP accept, and it is what makes the L3 arm measurable; (3) the gates
  in one run; (4) the rename to `Udp` with the implementation swap, KCP's
  deletion, the four feature lists, the status JSON, the bench vocabulary and
  the wire's move to 6.0 — the constant, which is what refuses a 5.2 peer;
  (5) the release ritual.
  **The pool's stream-type switch, landed (2026-10-23).** `MuxStream` **is**
  `CarrierStream` (`e57716d`: a yamux stream boxes into it at the two places
  one leaves the driver, and `CarrierIo`/`CarrierStream` left the `quic`
  feature). The pool now holds the seam: `PoolEntry`/`Reservation` carry
  `Arc<dyn CarrierConn>`, `complete()` opens through the trait and classifies
  its two errors (`Busy` is a refusal the open falls through on, `Closed` a
  death it repairs), repair reads `is_alive()`, `OpenError::Refused` carries
  `StreamError`, and `CarrierConn`/`StreamError` are un-gated. `MuxConn` is
  gone; `impl CarrierConn for ClientTunnel` is the TCP carrier's whole side.
  `ClientTunnel::start_with_health` keeps its signature because the carrier
  turned out not to belong on it (below).
  **Two trait items were dropped rather than invented**, and the reasons are the
  design. `stats` had no production caller — placement and the status snapshot
  are synchronous and non-blocking, and a synchronous reading has nothing to read
  until the UDP carrier caches what its engine reports — so it stays an inherent
  method on `UdpConn` until the lanes need it. `carrier()` made a second source
  of truth for a fact the pool already records per entry, and the first fixture
  that simulated a carrier (a pool labelled `kcp` over loopback yamux sessions)
  made the two disagree: the snapshot column is the pool's record, and the trait
  does not repeat it.
- **A lane can ride the UDP carrier (2026-10-23, wired, not yet measured).** A
  direct channel is one connection carrying one stream, so it is the first place
  the UDP carrier can stand in for KCP: `connect_udp_carrier` dials a QUIC
  connection with the service's own Noise pair (the data port keeps an identity
  whether or not `data_enable` puts the record layer above it), and the server
  accepts that stream on the endpoint it binds under the same client-first rule the
  KCP listener follows. A build keeps exactly one implementation per carrier slot:
  `quic` takes the direct path and leaves KCP the pool, a build without it
  keeps KCP for both. A tunnel hello on that listener is refused rather than
  half-served: a pool tunnel is one connection carrying many streams, which is the
  next step. **The proof is still owed** — both suites are green (310 default, 325
  with `quic`) but nothing yet drives a lane end to end, so this is wiring and
  the carrier run (step 3) is what measures it.
- **The gate does not compile the `quic` feature, and a release build found it the
  hard way (2026-10-23).** The direct-channel slice passed `just check` and both
  suites, then failed `cargo build --release --features quic`: the extracted
  helper ended in a match whose arms had no value, which is an error only where that
  code is compiled at all — the default and minimal passes strip it. Two rules follow.
  **Build the feature you touched**: `--features quic` is not in the gate, so a
  slice that lands behind it is not proven by a green check chain. **Provenance is a
  marker, not a timestamp**: `build.rs` does not re-run when only sources change,
  so `--version`'s `Build Timestamp` can describe an older tree than the
  binary carries. A string only the new code contains is the signal that survives
  (`strings <binary> | grep -c 'UDP carrier'` is 8 in the build that has it, 0 in the
  one that does not).
- **The data port is one UDP socket, and a lane does not carry yet (2026-10-23; superseded the same day — the lane carries, see "Where this stands": a multi-datagram transmit was sent whole and stream reads were unordered).**
  KCP's listener and the QUIC endpoint are both UDP on `[server.data].bind_addr` and
  cannot coexist: the first quic build to serve a registration died with
  `EADDRINUSE` on the KCP bind, which is the structural reason the two
  implementations must *swap* in the carrier slot rather than stand side by side.
  A `quic` build therefore no longer starts the KCP listener, and the conflict is
  gone — but the lane still does not carry: registration succeeds, the visitor
  then times out (86 s, both arms). So the direct-channel path is wired and its
  port handling is right, and the bytes are not flowing yet. What to look at next,
  in order: whether the client's dial reaches the server's endpoint at all (the
  server logs an accept or nothing), the stream's first-byte selector against
  `data_enable = false`, and the hello the server reads (a timeout there is
  logged as a debug line, not a WARN). **Do not build a claim on this slice**; the
  **First narrowing, from that run**s own logs:** the server *does* bind —
  `Listening for UDP carrier connections at 10.10.0.254:2400` appears once per
  arm — so the carrier declaration, the client-first bind rule and the registration
  path all work; what fails is everything after the socket. The next run needs the
  subject at debug, because the accept loop and the hello reader both report there
  and the harness captures WARN and INFO only: a silent timeout cannot otherwise
  tell "no connection arrived" from "the server is waiting for bytes".
  **A hypothesis worth one check before any debugging (2026-10-23): the lanes may
  not be direct channels after all.** A claim's lanes are documented as the
  client's *carrier-connection budget* (`[transparent.data].tunnels`), and the
  transparent path draws them from the pool — so a build whose KCP listener was
  switched off (the port fix above) would fail to bring the *pool* up over KCP and
  starve the claims, which produces exactly the observed `visitor timed out` with a
  successfully bound UDP endpoint. Check before changing code: does the lane for a
  claim come from `TunnelPool` or from `connect_direct_channel`? If it is the pool,
  then the UDP carrier must first serve *pool tunnels* (one connection, many
  streams) and the direct-channel wiring above is the wrong end of the problem.
  **Evidence for the pool reading (2026-10-23):** the client's data channel on the
  main path is built from a tunnel stream — `ClientDataChannel::Mux(TunnelStream)` at
  `src/core/client.rs:1534` — while `ClientStream` (the type the direct-channel
  path returns) is a separate enum with its own `Plain/Noise/Kcp/Udp` variants. So the two
  paths coexist rather than nest, and which one a claim's lane takes is still the
  open question: grep the transparent path for the acquisition call, not the type.
  **Answered (2026-10-23), and it reverses the pool reading.** src/core/client.rs:753-756
  says a transparent service "opens its **lanes** — one carrier connection per lane,
  from the carrier's budget — and keeps them", and channels = cfg.lanes() at :762
  counts exactly those lanes. So a lane **is** one carrier connection opened by the
  client: the direct-channel path. The wiring is at the right end after all, and the
  timeout is a real bug in the new path (client dial or server accept), not a starved
  pool. The pool reading stays in the record, with what disproved it, because it looks
  plausible and the next reader will meet it again.
  **Where the debug lines go missing, corrected the same day (2026-10-23).** A run
  with RUST_LOG=debug produced the same single INFO line and no debug output, which
  first read as "the level is not read from the environment" — wrong: src/logging.rs:115-120
  says RUST_LOG wins when set (EnvFilter::try_from_default_env). The subject does
  honour it, so what drops the debug lines is the harness's own capture. The lever
  is therefore in the harness's log filter, not in the binary. The next step is therefore one of two changes, not a rerun:
  **And with the log raised to INFO: no stream is ever accepted (2026-10-23).**
  handle_udp_stream's failure line moved from debug to info, the binary was
  rebuilt and the same smoke ran (86 s per arm). The run shows the endpoint
  binding twice and **no channel-end line at all** — and a channel that had been
  accepted would have hit its 20 s hello timeout and logged. So the server never
  sees a stream: the bug is on the client's dial side or in the packets never
  reaching the server's endpoint, not in the hello the server reads. Next to check,
  in order: whether connect_udp_carrier runs at all for a transparent claim (and
  whether its failure falls back to TCP, which would have made the visitor work),
  then whether the address the client dials is the one the server's endpoint bound.
  **Both of those checks answer cleanly (2026-10-23):** parsing.rs:204 says a
  transparent service does *not* uses_pool, so its channels are direct channels and
  connect_udp_carrier does run; and the run's own configs show the client's
  default_remote_addr equal to the server's data bind_addr (10.10.0.254:2400 both),
  so the dial target is the socket the server bound. A client that dials the right
  address and a server that accepts nothing, with no fallback visible in the
  visitor's behaviour, leaves two candidates: the handshake packets are lost before
  the endpoint sees them, or the dial never returns and the lane is blocked behind
  its budget. The next observation is on the client: log the carrier outcome
  **The handshake works — 31 times (2026-10-23).** With the client logging its
  carrier outcome, one arm of the same smoke reports 31 lines of "Direct data
  channel came up on the UDP carrier". So the client's QUIC handshake completes and
  the server's endpoint answers it: the engine, the Noise session and the pass-through
  keys are all fine. What is still silent is the server's own handler — neither a
  successful dispatch nor a channel-end line appears, so the accepted streams never
  reach run_udp_listener. The next line to add is at its top: one INFO per received
  stream, which separates "the listener never started" from "the streams never
  **They do arrive: 41 of them (2026-10-23).** With a line at the top of
  run_udp_listener, the same smoke reports 41 "an accepted stream reached the
  listener" and 41 client-side "Direct data channel came up" — and still no
  channel-end line, so handle_udp_stream reads the hello, reads the prologue and
  dispatches successfully every time. The carrier, the accept loop and the prologue
  are therefore all proven, and the failure has moved past them into the claim's own
  plumbing. **The number is the clue**: 41 establishments for a handful of lanes means
  the client is re-dialing, i.e. a lane is dropped right after it comes up (or the
  claim never adopts it). Next: log one line when a lane is handed to, or retired
  by, the claim's lane bookkeeping.
  arrive".
  (succeeded or fell back, with the reason) at info in connect_udp_carrier.
  pass a log level through the bench adapter (a method-level change, so it belongs
  in the fingerprint) or hand-start a pair outside the harness — a transparent
  claim needs TUN and root, both available here.
  run's own record is `/home/dsh/tmp/udp-lane-smoke2.json`.
- **The engine requires `handle_timeout` for every due timer** (2026-10-23,
  fixed in the same arc): skipping it leaves an expired timer in the set, so
  `poll_timeout` reports "now" forever and the driver spins without sending;
  a PTO never fires either.

- **The KCP sender now uses UDP GSO (2026-10-22, shipped), and it moved the
  carrier's two numbers in opposite directions for a reason worth keeping.**
  The shape: a batch already arrives as one iovec stream of equally sized
  segments, so a run of two or more equal datagrams leaves in **one** `sendmsg`
  with `UDP_SEGMENT` set — the kernel builds one skb and segments it at the
  destination — while a mixed batch, a lone datagram, a run longer than
  `GSO_MAX_SEGMENTS` (eight) or a kernel/path that refuses GSO
  (`EINVAL`/`EMSGSIZE`/`EIO`/`EOPNOTSUPP`, remembered for the process) keeps
  `sendmmsg`. Measured:
  - the sender's own shape (loopback, 32 × 1472 B, receiver draining in the same
    process): **0.153 s/Gbit with `sendmmsg`, 0.046 s/Gbit as one GSO
    message** — 3.3×;
  - the bench A/B on the forced-KCP carrier (`MOLEHILL_DATA_CARRIER=kcp`, six
    rounds, the run's own A/A copy), twice with different caps:

    | cell | no GSO | uncapped | cap 8 |
    |---|---|---|---|
    | throughput (Gbit/s) | 7.03 | 8.67 (+23 %) | 8.71 (+24 %) |
    | cpu_s_per_gbit | 1.75 | 1.25 (−28 %) | 1.33 (−24 %) |
    | rtt_p50 (ms) | 0.99 | 1.45 | 1.36 |
    | rtt_p99 (ms) | 7.3 | 32.8 (4.5×) | 26.6 (4.8×) |
    | peak RSS (MiB) | 84 | 213 (2.5×) | 193 (2.4×) |
    | carried packet (KiB) | 1.3 | 20.0 | 8.8 |

    Every round agreed on both directions (p 0.031 against alpha 0.05; the A/A
    floors were 0.3 % for the CPU figure and 0.7 % for throughput), and a third
    run at a different cap reproduced it.
  **The mechanism, and why the trade is accepted rather than hidden**: before
  this change the sender's own syscall rate paced the ARQ, so KCP's 8192-segment
  send window (≈ 12 MB per session) never filled; with GSO the window does fill,
  which is where the extra 23 % of throughput comes from — and the same bytes are
  now *queue*, which is where the interactive tail and the +110 MiB peak RSS come
  from. That makes the tail a **window-sizing** question, not a GSO one: the
  carrier runs with `nodelay(1, 10 ms, fastresend=2, nc=1)` and a fixed window,
  i.e. congestion control off. Closing it means sizing the window to the path (or
  turning `nc` back on) — a measured decision of its own, recorded as the next
  item below, and the reason this entry keeps both columns instead of claiming a
  win.
- **Closed (2026-10-23): the bench model recorded a `--condition` the staged
  runner did not impose.** A condition a run cannot impose is now an argument
  error, and one it can impose is what every stage imposes — the record, with
  the clean/loss1 proof pair, is "Where this stands". The consequence for older
  files: the three `screen` runs quoted for GSO were **clean-path numbers in all
  three**, so the GSO comparison they carry is a clean-path comparison; a shaped
  one is owed, and it is the next measurement this file records.
- **The probe should dial rather than drain (2026-10-23, design).** The rule's
  two live measurements agree on the shape of its cost: the probe takes a
  *serving* tunnel out of the pool for the length of its establish window (5 s),
  and that window is visible in the interactive tail (p99 39-45 ms against 2.2
  with the rule off). The comparison itself does not need the pool's width: the
  cheapest probe is an **extra** tunnel dialled on the other carrier, carrying
  whatever placement sends it, closed when the verdict lands. That costs no
  serving capacity, makes the establish bound a dial timeout rather than a
  drain budget, and removes the one state (`handover=`) the first probe could
  get stuck in. It is a design, not a measurement — nothing has been built or
  timed — and it is the shape to build if the rule is ever to leave opt-in.
- **The carrier policy is going peer-level (2026-10-22, in progress), and the
  first instrument is in.** The operator's direction: TCP and KCP are peers, and
  the pool should pick per path instead of shipping a fixed order. The signal
  both carriers can produce is the **tunnel connection's own RTT**
  (`Connection::rtt` → `TunnelCounters::rtt` → `PoolSnapshot::rtt`, in the
  pool-stats snapshot): on TCP it is the kernel's, on KCP it also carries the
  ARQ's queueing, so a carrier whose backlog grows shows up in it whatever the
  cause. The rest of the design — equal-opportunity placement across carriers so
  the offered load is comparable, a per-window per-carrier verdict, and the
  existing `prefer`/`begin_handover` machinery under the 120 s hold — is the
  next commit, and the window-sizing question above is its first input.
- **Decided (2026-10-22): the data plane dials TCP first; KCP is its
  fallback.** The measurement is in "Where this stands" (the A/B is +77 %
  throughput, −64 % CPU per Gbit, −69 % p99 on the clean cell; the full stage
  schedule did not reverse it). Three things about the shape are deliberate and
  must not be undone by accident:
  - the registration declares the client's **capability**, not its dial order
    (`carrier::declared_carrier`), because the server opens its KCP UDP
    listener only for a client that declared KCP — the first version of this
    change declared the dial order and would have left a fallback with no
    listener to fall back to;
  - the fallback is remembered per endpoint and re-probed after 60 s
    (`CarrierChoice::failed`/`succeeded`), and a quality decision is still
    held for 120 s without a reachability event undoing it;
  - the pool's KCP-evidence rule is untouched and still moves a KCP-carrying
    pool to TCP; nothing moves a *TCP* pool to KCP on evidence yet, because a
    TCP tunnel's retransmit state lives in the kernel (`TCP_INFO` would need a
    production `unsafe` block or a parsed `/proc/net/tcp`, and the first is a
    policy regression this repository has deliberately earned away). That is
    the recorded next instrument if a deployment ever needs it.
  Residuals: no bench condition can exercise the **TCP → KCP** fallback, because
  the control channel shares the leg — `udp_blocked` now measures "TCP carries
  it with no fallback available" instead of "the fallback works", and its
  description in the model and in `docs/benchmarks.md` says so. The other,
  smaller lever is the data-plane AEAD at ~9 % of clean throughput:
  AES-256-GCM is only ~1.4× ChaCha20-Poly1305 on this Haswell host (openssl
  speed: 2.40 against 1.70 GB/s), so it would buy little here. The UDP
  record layer's cipher sits inside the 6.0 dialect from the start, so the
  question reopens without a second wire break.
- **The carrier seam did not move the TCP hot path (2026-10-23, P2's proof).**
  The three commits that moved the pool onto `CarrierConn` (`MuxStream` →
  `CarrierStream`, the pool's `Arc<dyn CarrierConn>`, yamux private to TCP) were
  measured against the build immediately before them: one fingerprint, the same
  method, `--profile stage --ab-arm molehill --binary <seam> --binary-b
  <pre-seam> --condition clean|loss1 --condition-leg tunnel` with
  `MOLEHILL_DATA_CARRIER=tcp`, both binaries built from committed trees and told
  apart by build timestamp.
  `clean`, 2 rounds at `--scale 0.2` (141 s, 12 samples): **12.16 Gbit/s**
  (12.11–12.21) against **12.22** (12.08–12.35) — 0.45 %, inside the run's 1.39 %
  floor and *smaller* than the 1.41 % by which the seam build differs from a
  second copy of itself (12.33). No claim cleared the floors.
  `loss1`, 3 rounds at full length with a warmup: medians **7.76** against
  **7.82 Gbit/s** (0.8 %), and the cell is bimodal for *every* arm — rounds land
  near 6.0 or near 7.9 whichever build runs — so its own floor is 16.4 % and it
  resolves nothing. The record says that rather than reading 0.8 % as a win: at
  3 rounds the cell would need ~11 to resolve its own scatter.
  **Tooling note, paid for twice:** `just bench run` re-splits its arguments, so
  `--hypothesis` must be one token — a value with spaces arrives as loose
  arguments, and one with an apostrophe breaks the recipe's shell line before the
  tool sees it.
  What the pair does say: the refactor added no throughput the harness can
  resolve, and one `Box` plus one dynamic dispatch per open are invisible next to
  a 12 Gbit/s copy path. This is a stage-profile spot check, not the release
  sweep; the sweep re-measures it on the release method.
- **Closed (2026-10-22): a forced carrier is no longer invisible to the
  comparability gate.** `MOLEHILL_DATA_CARRIER` (and the scheduled-switch
  hatches) stay in `meta.molehill_switches`, beside the method rather than
  inside the fingerprint — a declared treatment is what a comparison contrasts.
  What was missing was the *reader*: `analysis.comparability` now refuses two
  files whose switches differ (`_switch_blockers`), so the gate's
  `_baseline` check and `just bench compare` both name it, and the model's
  self-check asserts it (710 checks). This is deliberately *not* a fingerprint
  change: folding the switches in would have re-identified every record ever
  published and forced a method re-baseline in a release ritual, which the hole
  did not warrant.
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
    per-service inheritance it is the pool's width, not a default; the name
    stays `tunnels` (decided 2026-10-22, see the naming entry below).
  - The UDP-derived floor this entry planned was **dropped in 0.11.2**
    (correction 2026-10-22): a UDP service starts one worker and grows on
    demand (`max_udp_workers`), and those workers are streams inside the pool,
    so the width has nothing to cover. Validation and runtime still read one
    source (`[client.data].tunnels`); the first-service-width defect below
    disappears with the model either way.
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
- **Recipe B's single-IP client side is documented (2026-10-22; found
  2026-10-19).** With the claim on the server's own public address, the client
  assigns that address to its TUN, so the kernel treats it as local and the
  client's control channel to `<server-ip>:2333` loops back to itself. Both
  deployment pages now carry the client half of the recipe: the demoted local
  lookup (`ip rule add pref 200 lookup local; ip rule del pref 0`) and
  `ip rule add pref 100 iif lo to <server-ip> lookup main`, which sends
  locally generated traffic out by the gateway while the packets the server
  delivers still reach the local lookup on the tunnel interface. The steps
  were verified in a network namespace on 2026-10-19. Still open: a single-IP
  acceptance case (the L3 harness has no such arm), so this stays a
  documentation fix until one exists.
- **Decision (2026-10-22): the key stays `[client.data].tunnels`; there is
  no rename to `default_tunnels`.** The question was raised on 2026-10-19,
  while the key was still the client-wide default for every service that did
  not write its own `[client.services.<name>].tunnels` — the shape the
  `default_token` / `default_remote_addr` / `default_retry_interval` family
  names. The one-pool decision above removed the child key: the width belongs
  to the session's pool, nothing inherits it, so `default_tunnels` would name
  a relationship that no longer exists. Keeping `tunnels` also keeps 0.10's
  deliberate replacement of `default_count` intact (the pinned pool ended the
  "initial size" reading), avoids a removed-key migration for a name whose
  meaning the configuration page already states, and leaves
  `[transparent.data].tunnels` — a lane budget, a different quantity with its
  own default — untouched. `[client.services.<name>].tunnels` stays removed.
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
  and `from_str`; `just bench selfcheck` loads each arm's config). **The
  general hole is closed for the rules that were still late (2026-10-22):**
  a config-intrinsic refusal belongs in `from_str`, and the two that were
  still enforced later are there — a forwarding `local_addr` without a
  usable port (refused with the service named, instead of failing at the
  first visitor's dial) and, on Linux, a TUN name the kernel cannot carry
  (refused with the key named, instead of `interface does not exist` at the
  first packet). What stays at runtime is what `from_str` cannot judge: host
  state (devices, routes, forwarding, privileges) and peer behaviour. The
  macOS and Windows device-name rules stay at device creation, because the
  deployment page shows every platform's names and the doc-example test
  parses all of them on the running platform.
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
- **Deferred by operator decision (2026-10-18, re-affirmed 2026-10-22):
  macOS/Windows L3 real-host acceptance, the `tun-rs` screen A/B and the
  lending measurement.** Linux `l3-accept` on both wire formats remains the
  only end-to-end L3 evidence; the macOS pf/utun and Windows Wintun/routing
  recipes are documented but unvalidated on hardware. No tun-rs performance
  claim or lending claim is made by any release so far. These are accepted
  gaps, not blockers, and the next cycle does not pick them up unless the
  operator asks: the 2026-10-22 decision was to leave them out of scope.
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
