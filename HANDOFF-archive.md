# HANDOFF archive: records and incidents

This page is the **archive** half of [HANDOFF.md](HANDOFF.md). It keeps the
cycle's closed records — decisions that were taken, incidents that were
diagnosed, measurement records that were superseded — in tree, so a reader can
follow *why* a choice was made without scrolling past a thousand lines of
history to reach the current state. The live page carries what is still open.

Two rules apply to everything below, and they are the reason it was split out:

- **A record here says what the branch's authors believed at the time**, and
  why a decision was taken. **No number in these records may be quoted as a
  measurement of the current code, compared against a Soak result, or used to
  gate anything.** The live numbers are the sweep record on the front page,
  and the method's home is [docs/benchmarks.md](docs/benchmarks.md).
- **Records are append-only history.** When a later record supersedes an
  earlier one, it says so at the top; the earlier text is left standing so the
  reasoning can be audited. Where a record was retracted in place, the
  retraction is marked in that record.

## The transparent-L3 cycle: closed records (archived 2026-10-13)

Moved from [HANDOFF.md](HANDOFF.md) when the cycle's release was prepared: each of these
settled something, and none of them is open work. The rules at the top of this page apply
to every one (no number here may be quoted as a measurement of the current code).

## Why L3 is slower than L4, and the three fixes measurement refused (2026-10-16, this session)

The question was whether the L3 arm's throughput deficit against L4 is a defect this
release should fix. It is not a defect: it is the price of carrying **packets** where the
forwarding path carries **bytes**, and three candidate fixes were built or measured and
refused. Recorded because the refutations are the expensive part and none of them should
be re-tried blind.

**What the deficit is made of**, measured on the `screen` profile's L3 arm (`cost`
scenario, clean, `MOLEHILL_L3_STATS=1`):

- **One TUN syscall per packet per direction.** ~900 k syscalls/s (`syscalls_per_s`
  822–972 k) at ~850 B/syscall, which is the data packet's read + write plus the
  reverse-direction acknowledgement's read + write — four per data packet, because the
  client's kernel (not the server's) terminates the TCP connection, so its ACKs have to
  cross the tunnel too.
- **The cost is system time, not compute.** Host split during the bulk phase: molehill
  ~7.7 cores (3.1 user + **4.6 system**), with the whole host at 14.2 of 20 cores busy.
  `cpu_s_per_gbit` 3.1 against the forwarding path's 1.57.
- **It is not a lock, and not one saturated stage.** `schedstat` over the daemons:
  **run ≈ 600 %, wait ≈ 1 %** across 22 threads — ~6 cores of genuine execution, no
  runqueue contention. (A tokio *task* migrates between workers, so "no thread at 100 %"
  alone would not have settled this; the run/wait split does.)

**Three fixes, refused:**

1. **A single serialized reader thread.** The code has one reader task per device doing
   read + parse + frame + carrier-write inline, and a task can use at most one core, so
   this looked like the funnel. *Refuted by the run/wait split above*: ~6 cores of real
   work is not one task's worth, and no stage was saturated.
2. **The global routing-table mutex.** `Routes::pick` and `Routes::touch` each take
   `Mutex<EndpointTable<ClaimSlots>>` — twice per packet, with a 5-tuple hash and an
   `Instant::now()` inside the critical section. *Refuted by the same measurement*:
   wait ≈ 1 % is not a contended lock.
3. **Multi-queue TUN readers.** Built and A/B'd (`--ab-arm l3`, one reader vs four,
   same commit, same device). The kernel side is exactly as needed — verified on this
   host that a `multi_queue` device hands each **flow** to one queue and keeps it in
   order (8 flows over 3 queues, every flow wholly on one, in order), so parallel
   readers cannot reorder a flow. *Refuted by the A/B*: throughput indistinguishable
   (3.12/2.76/3.11/2.09 against 2.97/3.00/3.05/2.92 Gbit/s) and `cpu_s_per_gbit`
   marginally **worse** (3.164 → 3.221), `bytes_per_syscall` unchanged. The change was
   reverted: it buys nothing, and a `multi_queue` device refuses a non-multi-queue
   attach, so adopting it would have made every pre-v0.11.0 binary unable to serve the
   device — a hard upgrade hazard for no measured gain.

A fourth test could not resolve anything and is **not** evidence either way: one reader
vs four at a 4-lane budget on the `capacity` ramp. Its own A/A twin differed by 100 % on
throughput (`throughput_gbps` floor 100.00 %), the model's own report says ~385 rounds
would be needed to resolve a level-1 difference, and the single "claim" it printed was a
`capacity_streams` delta that the same arm's self-comparison also produced. The config
also needed `pool_cap=20` for 5 claims × 4 lanes, which is not a shape the release
measures.

**So what moves it** (both already documented, neither a code change): **packet size** —
1.84× and a third of the CPU per byte at an 8000-byte TUN MTU on a jumbo path
(`benchmarks.md`, "The L3 data path") — and **the lane budget**, +58 % for four lanes
against two, paid for out of the interactive stream's SLO headroom.

**The deficit is one-sided, and the other side is L3's.** The same smoke run that
read the bulk gap (one flow, one connection in both arms: l3 1.998 against l4 2.878
Gbit/s) read the round-trip scenarios the other way — `rr-1` 9 524 against 8 525
requests/s, `rr-16` 27 863 against 26 823, `rr-1` p99 0.146 against 0.151 ms. Directional
only (2 rounds, no A/A, "no claim cleared the floors"), but the sign is what the
abstraction-count intuition predicts: where the workload is latency- and small-message-
bound, one layer less wins — which is also the release sweep's own headline (L3's clean
interactive p99 is 2.3 against L4's 4.3 ms). The bulk penalty and the round-trip advantage
are the same trade read from its two ends: the layer L4 adds is a *packer*, and a packer
costs latency and buys bulk.

**What remains unmeasured**, if anyone wants to reopen it: which of the four TUN syscalls
dominates (no `perf` and `ptrace` is blocked in this container, so `strace -c` could not
attach — `MOLEHILL_L3_STATS`'s `read_gap_max_ms`/`write_gap_max_ms` and
`/proc/<pid>/io` are what is available), and whether GSO on the TUN could cut the inject
half's syscalls without the wire change the docs assume it needs.

## An L3 claim holds a member set (2026-10-10, this session)

The transparent path's claim used to be one data channel, and one carrier
connection is a hard ceiling — measured flat at ~7.1 Gbit/s for one inner flow
and eight alike, while the forwarding path's per-connection channels reached
22-28 through the pool. A claim now holds **K data channels (its members)**, and
its inner flows are hashed across them by their canonical 5-tuple. Landed in
slices, each with its own acceptance:

- **S1 (`6a4c751`)** — the hub routes by *member slot* instead of one queue per
  endpoint: a fixed slot set per claim, batches keyed `(endpoint, slot)`, and
  slots that **outlive their members** (a replacement inherits its predecessor's
  index), so the routing target of a flow never moves when a channel does.
  Pure refactor at width 1; `MOLEHILL_L3_STATS` gained a per-member line, which
  is the instrument the feature has to be measured with.
- **S2a (`a804f6e`)** — `members` / `[transparent.data].default_members`
  (default 1, max 64; `tunnels < members` refused next to the UDP-workers
  floor). The client opens that many channels, placed on distinct tunnels in
  `multiplex` mode; the server supervises them as a set and replaces each one
  that ends. Acceptance was **indistinguishability**: `members = 4` against
  `members = 1` showed no claim above the run's 9.8 % floor, `fds_peak`
  identical at 69 (members in mux mode are streams on tunnels that already
  exist), RSS +1.3 MiB for 20 extra streams.
- **S2b (`229d3a4`)** — the flow hash: `src/transparent/flow.rs`, FNV-1a with a
  **splitmix64 finalizer**, fixed seed, over protocol + the two `(address, port)`
  endpoints sorted (portless packets hash on the address pair alone). Jumbo path,
  four rounds an arm, twin in the run, 8 flows: **one member 6.989 Gbit/s →
  four members 13.924 [11.528..15.091]**, the same configuration's twin at
  13.598 — **1.99x**, on a run whose own floors were 3.34 % (throughput) and
  0.17 % (the twin pair). `bulk-1` indistinguishable (6.201/6.244/6.299),
  `churn-16` +22 %, `rr-16` −1.8 %, `fds_peak` identical at 69,
  `wire_per_visitor_byte` unchanged, and the one-member claim's carrier
  retransmits 518 → 0. The fan-out is proven per member: 3-4 of 4 slots carry
  traffic in every round, and both ends report the same split to within a point
  (23/52/24/0 against 23/53/24/0).
- **S3 (`6ee0038`)** — a member's death is proven not to move the others: the
  slot stays, the survivors keep forwarding, only the dead slot counts
  `no_channel`, its replacement inherits the index, and the modulus never moves.

**The finding inside S2b is worth keeping.** The first run of the slice read
11.282/13.142 instead of 13.924: FNV-1a's low bits are weak, the byte that
varies between eight `iperf3` streams is its ephemeral port, every one of those
ports was even, and `h % width` reads exactly those bits — so eight flows landed
on two of four members. The splitmix64 finalizer fixes it and the regression
test pins the measured port set and every stride. The lesson generalises: a
modulo over a hash whose *low* bits are weak is a placement bug waiting for a
traffic pattern, and per-member counters are what make it visible.

**What this does not do, and the two facts to carry forward.** The claim still
trails the forwarding path (13.9 against 29.2 Gbit/s), which is the pre-stated
band that means *the constraint has moved to the device path* — one server TUN
reader, one client injection path, one syscall per packet. That is the next
lever, and it needs its own arm before it is a claim.
Also unclosed: there is **no server-side valve on concurrent members** in
`direct` mode (in `multiplex` mode the surface is streams on tunnels the pool
already bounds; in direct mode K connections per claim are held for the claim's
life), and the model has **no fault-injection axis** for a member's death, so
the slot-retention semantics are proven by unit and loopback tests rather than
by a measured recovery number.


## The tunnel pool is pinned (2026-10-10, this session)

The client's multiplex pool is no longer elastic. It establishes
`[client.data.tcp|kcp].tunnels` (default 4, raised to a UDP service's declared
worker count) at service start, keeps them, and **repairs** a dead one back to
the count; it never grows for load and never reaps for idleness. `max_tunnels`
and `idle_timeout` are refused with upgrade messages, the floor is validated at
config time, and the server's `max_tunnels_per_client` valve now bites at
startup.

The reasoning, in the order it was established:

- An elastic pool's width is a *result*: it depends on what the process was
  doing. That is a capacity-planning problem for an operator and a
  comparability problem for this repository's measurements (§10) — the same
  arm, run twice, can describe two different pools.
- The measurements that size the key are unchanged by pinning it: one tunnel is
  the worst configuration on every path measured (3.34 against 5.92 Gbit/s for
  8 flows on `loss1` from one tunnel to two; 12.85 against 22.81 on a clean
  jumbo path from one to eight), and an L3 claim on one carrier connection
  carries the same for one inner flow and for eight.
- Establishment is cheap and belongs at startup: **0.5–0.8 MiB RSS and 2.6 file
  descriptors per tunnel, no threads** (measured by varying the pool's width on
  a fixed workload), so a deployment can afford the count its concurrency
  needs, and the first visitor pays nothing.
- `mux` retains its purpose in that model: it decouples the count of *carrier
  connections* (fixed, configured) from the count of *logical connections*
  (unbounded, per visitor — a stream on an established connection costs no
  round trip). Where the logical count is known at configuration time and does
  not exceed the count — a single L3 claim — a raw channel is measurably
  cheaper (7.599 against 6.620 Gbit/s single-flow, 0.277 against 0.578 s/Gbit),
  which is why `direct` survives there.

What this changed, mechanically: `GrowReason` has one variant (`Repair`) and
`ShrinkReason` one (`Dead`); `grow_before_placing`, `grow_for_stripes`, the
floor-driven growth loop, `shrink_if_idle`, `may_shrink`, `GROW_PERCENT`,
`grow_threshold`, `tunnel_grow_at`, `MIN_WARM_HOLD`, `SHRINK_COOLDOWN` and
`OPEN_WAIT_BUDGET` are gone; placement's per-tunnel ceiling
(`TUNNEL_STREAM_CEILING`, 56) is now the *only* oversubscription bound, and the
UDP-derived floor became a validation input rather than a growth target. The
one invariant to keep in mind: the engine's own stream cap (64) is no longer
backstopped by growth, so the pool's ceiling has to stay strictly below it —
`tunnel_ceiling` does that, and the `pool_test` suite asserts it.

Still open from this work (next steps, in the order agreed):

1. ~~**A saturation notice.**~~ **Landed with the change.** A pinned pool
   cannot grow, so the refusal that used to trigger growth now reports itself:
   the first time a visitor is refused because every tunnel is at placement's
   stream ceiling, the pool emits one INFO line
   (`pool-stats: pool at its stream ceiling; raise [client.data.<carrier>].tunnels`)
   with the configured count and the ceiling, and every later refusal is DEBUG
   (`logging::RepeatNotice`). `OpenError::AtCapacity`'s text now names the
   setting instead of a growth that can no longer happen.
2. ~~**Member kinds and L3 flow hashing.**~~ **Landed 2026-10-10 (S1/S2a/S2b).**
   A claim now holds a *set* of members — `[transparent.data].default_members`,
   or a claim's own `members`, default 1 — and its inner flows are spread across
   them, one flow per member, by a hash of the flow's five-tuple. Both carrier
   kinds are members: a stream of the claim's tunnel pool in `multiplex` mode,
   a connection of its own in `direct` mode. The set is discovered by the hub
   (the channels that actually join), so nothing about it is on the wire; a
   member that dies keeps its slot and its replacement inherits the index.
   Measured (dev profile, jumbo path, A/A twin in the run, `bulk-n` = 8 inner
   flows): one member 6.989 Gbit/s, four members **13.924 / 13.598** (two
   copies, A/A throughput floor 3.34 %) against the L4 pool's 29.17 — the claim
   nearly doubles, at 0.653 against 0.560 s/Gbit, with `fds_peak` identical
   (69) and RSS +12–19 % for the 20 extra streams. `bulk-1` is unchanged (one
   flow, one member), `churn-16` gains ~22 %, `rr-16` loses ~2 %.
   **What is not covered yet, in the order it should be picked up:**
   - **No server-side valve on a claim's concurrent members in `direct` mode.**
     `[server.data].max_tunnels_per_client` counts *tunnels*, and a direct
     member is a data channel, not a tunnel, so a client may hold `members`
     connections per claim with nothing bounding the total. In `multiplex` mode
     the surface is streams on tunnels the operator already caps (and the pool's
     56-per-tunnel ceiling bounds it), so this is a direct-mode gap.
   - **No fault-injection axis in the bench model for member death.** The
     failure semantics — a dead member's slot keeps its index, its flows are
     dropped and counted `no_channel` on that slot until the replacement joins,
     the survivors untouched — are unit- and loopback-tested only; the model
     cannot yet price a member dying during a measured cell.
   - **Member kinds, measured (S4).** `l3-raw4` (four *direct* members, one
     connection each) against `l3-mux4` (four streams of a four-tunnel pool),
     same run, dev profile, jumbo path, A/A twin: `bulk-n` **15.013 / 16.196**
     against 13.187 Gbit/s, at **0.516 / 0.502 against 0.658 s/Gbit**; `bulk-1`
     7.316 / 7.189 against 6.640 at **0.337 / 0.321 against 0.561 s/Gbit**;
     `rr-16` +2-3 %, `churn-16` indistinguishable (11.9 % floor). `fds_peak` is
     identical (69) — the mux arm holds four *tunnels* per claim and the raw arm
     four *connections*, so the count matches when `members == tunnels` — and
     RSS is **lower** for raw (19.2 against 26.15 MiB at `bulk-n`, floor 9.17 %)
     because there is no yamux session state per member. So a raw member is the
     cheaper carrier per byte whenever the claim's member count is what the
     deployment is paying for; the multiplexed pool remains what lets *unbounded*
     per-visitor channels ride a bounded count of connections (the L4 path),
     which is not a claim's shape. The cost stands as recorded below: nothing
     bounds a claim's concurrent direct members server-side.
   - **The flow hash needed a finalizer, and that was found by a run, not by
     review.** A bare FNV-1a `% width` aliases with the *low* bits of the ports
     (its last step is a multiplication by a prime, which preserves their
     residue structure). The measured `iperf3` ports
     `52044, 52052, 52054, 52058, 52070, 52082, 52088, 52094` — all even,
     several four apart — took two of the four members, which is why the first
     measurement read 11.28/13.14 instead of 13.92/13.60. The per-member
     counters (`MOLEHILL_L3_STATS=1`) plus the iperf3 JSON's `local_port` list
     diagnosed it; splitmix64's finalizer fixes it, and the regression test
     pins both the measured port set and every port stride. The lesson for the
     next hash-shaped decision: `% width` on a raw FNV value is not a placement
     rule, and the per-member counters are the instrument that says so.


## The mux×1 gap, measured before touching it (2026-10-10, this session)

The decision to derive the data plane (L12–L18) rests on two facts, and the
second one was measured before any of it was written, because the comparison
stops being configurable the moment `mode` is deleted.

**The gap, on the pristine tree.** `l4-mux2` is one flow on a two-tunnel pool —
one stream on one session, the shape a sparse visitor or a lane gets — against
`l4-direct`, the same flow on a connection of its own. Both arms in one run,
the A/A twin on the mux arm as the run's own noise floor, `smoke` profile
(two measured rounds per arm), loopback, host `d764f9da9c7e5b2a`:

```
sudo -n just bench --profile smoke --arms l4-mux2,l4-direct,control --aa \
  --binary ~/tmp/molehill-c0-baseline --out ~/tmp/bench-c0-mux-vs-direct.json
```

| arm | bulk-1 throughput | bulk-1 CPU | rr-1 rate |
|---|---|---|---|
| `control` (no tool) | 40.65 Gbit/s | — | 32 000/s |
| `l4-direct` | **18.95** Gbit/s [17.52..20.37] | **0.110** s/Gbit | **15 885/s** |
| `l4-mux2` | **9.15** Gbit/s [9.00..9.29] | **0.207** s/Gbit | 13 357/s |
| `l4-mux2~aa` (twin) | 7.93 Gbit/s [7.19..8.67] | 0.250 s/Gbit | 13 583/s |

Read: the multiplexer costs **2.07× the throughput and +87 % of the CPU per
byte** on one stream, and this run's own A/A floor is **15.35 %** on that
metric — so the gap is real at this profile, resolvable, and worth attacking
(which is why L18 gates the engine work on this number rather than on a hunch).
Provenance is exact: the binary is the pristine `80b0e62` build
(`v0.10.0-47-g80b0e62`, sha256 `94549863c4f6392d`), run from a worktree at that
commit with that commit's model. The symbol-level attribution (which of the
per-write allocation, the 32 KiB frame split, the driver hand-off or the
window updates owns the gap) is the first step of the engine work itself, not
of this baseline.

**A model defect found by the same run.** The catalog arm `l4-mux1`
(`pool_cap = 1`) cannot start against the model's L4 topology at all: that
topology carries a UDP service with `udp_workers = 2`, so `tunnels = 1` is
below the UDP-derived floor and the client refuses to start —

```
`[client.data.tcp].tunnels = 1` is below what the services of that carrier
need: service `game` declares 2 UDP workers, and a pool keeps one tunnel per
worker so their shards stay on distinct tunnels. Write
`[client.data.tcp].tunnels = 2` or more, ...
```

Its four scenarios then report `not ready: no visitor could reach
10.10.0.254:2401` — the degenerate L4 cell recorded as an open thread in
[benchmarks.md](docs/benchmarks.md), now with a cause for at least this arm.
`l4-mux2` is the closest legal mux×1 shape and is what the table above uses;
the arm's own fix (raise `udp_workers`, or drop the arm) belongs with the model
change, not here.


## The clean cell's per-datagram output cost (2026-10-11, round 17)

The round-15/16 exchange ended with an instruction to itself: price the carrier
on a cell where the path keeps up, because a saturated one cannot. This is that
reading — `l3`, `bulk-1`, `condition clean`, one round, `MOLEHILL_KCP_STATS=1`,
two consecutive stat lines two seconds apart, on the **server**:

| phase | ms per second | share of a core |
|---|---|---|
| `ms_output` | **748** | **75 %** |
| `ms_input` | 46 | 4.6 % |
| `ms_writer` | 42 | 4.2 % |
| `ms_deliver` | 9.5 | ~1 % |
| `ms_update` | 5.7 | 0.6 % |

with `datagrams_out` ≈197 000/s (≈2.9 Gbit/s at the 1472 B datagram it had
adapted to), **`retransmits` 0**, **`wait_snd_peak` 376** against the 16 384
segment cap, `srtt_ms` 1–3, `peer_wnd` 4096.

**This is the clean measurement the earlier rounds could not make.** The queue
is empty and nothing is being retransmitted, so the output phase is not the cost
of congestion — it is **≈3.8 µs per outbound datagram**, spent inside
`drain_dgrams`: the pacer's per-span decision (which reads the clock once per
datagram), the span staging, and the `sendmmsg` batch that carries up to 32 of
them. 3.8 µs is one to two orders of magnitude above what staging and one
batched syscall should cost, so the next attempt's first job is to **split that
phase in two** — the pacer decision and the wire send — with a phase timer
around each, and let the split say which of them owns the time. That is a
measurement change, not an optimisation, and it is the one to make before any
constant is touched.


## Where the carrier's CPU is, measured at this commit (2026-10-11, round 15)

One reading, taken so the next attempt starts from data rather than from a
memory of older numbers: `l3`, `udp-ladder`, `loss1_buf`, tunnel leg, one round,
`MOLEHILL_KCP_STATS=1`, the busiest second (two consecutive stat lines
subtracted, so this is a rate and not a lifetime):

| phase | ms in that second | share of the pump |
|---|---|---|
| `ms_output` (pacer + wire drain + retries) | **+144.2** | **62 %** |
| `ms_writer` (app → ARQ) | +31.0 | 13 % |
| `ms_input` (wire → ARQ) | +31.1 | 13 % |
| `ms_update` (timers, retransmit decisions) | +25.4 | 11 % |
| `ms_deliver` (ARQ → reader) | +1.6 | <1 % |

with `datagrams_out` +53 305, `retransmits` +2 502 (4.7 % of output),
`pump_rounds` +8 704, `wait_snd_peak` **16 395** — the send queue sitting at its
cap (2 × 8192 segments) for the whole window — `srtt_ms` 10–17, `rto_ms` 30,
`datagram_b` 1472.

**A correction, made in the next round before anything was built on it.** The
first reading of this table (below, kept because the correction is the useful
part) said "the lever must be queueing". It is not, and the arithmetic says so:
the second being measured emitted ≈53 305 × 1472 B ≈ **628 Mbit/s while the cell
offers 1000** — the ladder's cells deliberately offer more than the path
carries, which is what a ladder is for. At that rate the BDP over the window's
17 ms is ≈**902 segments**, far under both the send window (8192) and the queue
cap (16 384): **neither is the constraint; the path is.** A pump that spends its
time trying to push a gigabit through six hundred megabits is the *intended*
shape of that cell, so "attempts that do not go out" is the workload, not a
defect to fix. The original reading, kept as the record of it:

> The carrier's time is in the send path, and the send queue is full while it
> is: a large part of that 144 ms looks like the cost of attempts that do not go
> out — `send_spans` hitting a full socket, parking the remainder, paying a
> syscall again next round.

What survives the correction is what it already ruled out: **allocations on
this path are not the lever** (this cycle's two allocation-level changes measured
as a cost and as noise respectively), and a saturated cell cannot price a
per-datagram change at all. The CPU-per-delivered-byte gap against TCP (1.661
against 0.586 s/Gbit at the 1 Gbit/s cell) has to be attacked where the path
keeps up — the **clean** cells, where the pump is not saturated — and the phases
that matter there are the ones this table shows as secondary (`ms_writer`,
`ms_input`, `ms_update`), not the one a congested cell makes dominant.

So the next attempt should aim at the queueing, and it has three shapes worth
pricing, in the order the reading argues for them: the socket buffer and the
pacer's interaction with it (a parked datagram is retried, and the parked queue
is bounded by half the RTO), the send-queue cap itself (`2 × snd_wnd` = 16 384
segments, which is what `wait_snd_peak` is pinned to), and the retransmit share
(4.7 % on a path losing 1 %) — each with its own A/B, and each with the caveat
that the throughput the cell is already reaching bounds how much any of them
can show.


## Falsified: the inbound payload copy is not the cost (2026-10-11, round 14)

The receive path allocated a right-sized buffer per accepted segment and copied
the datagram's payload into it (`BytesMut::zeroed` + `read_exact`), and the
adapter always held the datagram as a `Bytes` — so `Kcp::input_bytes` was added
to hand the payload on as a slice instead (`8854500`), with the property pinned
by pointer in a test. It looked like the obvious remaining CPU lever on the
carrier.

**Measured, and reverted (`e57a45b`).** Two binaries, `--ab-arm l3`, `l3`,
`bulk-1,bulk-n`, clean, three rounds plus a warm-up with `--aa` (24 samples,
baseline `ee9a73e004049d5b`, candidate `2753a897ab66c92d`): **one claim of
twenty-eight comparisons, and it is a cost** — `bulk-n[P8] rss_peak_mib`
**+18.26 %** on the candidate. Throughput and CPU per byte were inside the
floors on every cell.

The mechanism is worth keeping: a slice shares the *whole datagram allocation*
and keeps its `Bytes` header alive, where the copy produced a right-sized buffer
that died with the segment. At an MTU where one datagram carries one segment the
bytes are the same but the per-segment handle is not, and eight flows' worth of
in-flight segments is what the peak measures. So the receive-side copy is *not*
where the carrier's CPU goes; the lever the earlier rounds pointed at (the
per-segment work above it, and the send path's own costs) stays the one to
attack — with a run's floors as the gate, which is what this one did.


## Verified state of this branch (2026-10-11, round 9)

The branch is green end to end at `c45af2b`, and the two proofs that matter for
everything this cycle touched were re-run rather than inherited:

- **`just l3-accept`: 15/15 PASS** at this commit — the transparent data plane is
  intact *after* the two-sided drain, the automatic carrier move and the lending
  arbitration all landed. (Its previous green run predated all three, so this is
  the re-check, not a repeat.)
- **The heavy chain** (audit, deny, outdated, the full serial test suite) is
  green on this commit: it runs on every push, and the push that carried it
  passed.
- The suites beside them: 249 lib tests, 11 `pool_test` scenarios, 23
  `integration_test` cases, clippy twice (default and the minimal
  `server,client` build).

What is **not** proven, stated once more because it is the thing a reader is
most likely to assume: the automatic carrier move's **benefit**. Its mechanism
is measured end to end (above); its effect is not, and the current model's
workloads cannot show it (a round is shorter than the confirmation plus the
drain plus the flows that would follow). That is a decision for the human, not
a defect to fix quietly.


## The v0.10.0 theme

One control session per endpoint, one shared elastic pool per carrier,
transparent visibility and a quiet log. Four of the milestones were independent
and are already merged on `main` (unreleased, so they are part of this release);
the rest ship as **v0.10.0**, because the wire protocol and the configuration
surface both change.

### Decisions

| # | Decision |
|---|---|
| D1 | Session per `(client, remote_addr)` — in practice per `(remote_addr, effective transport)`: the selector byte is per-connection, so a plain and a Noise service on one endpoint cannot share a connection without downgrading one |
| D2 | Per-service auth inside one session; one denied registration never kills the session |
| D3 | Control channel direct and independent (dependency direction, liveness isolation, repair ability) |
| D4 | Pool per carrier; `carrier` stays a per-service selector |
| D5 | Growth needs no new control command (client-local, client-initiated — NAT-friendly) |
| D6 | `min_tunnels` deleted; warmth is derived (warm for `idle_timeout` after activity); a pool starts cold |
| D7 | UDP-derived floor = `ceil(channels / streams-per-tunnel)`, capped by `max_tunnels`; the worker set is maintained |
| D8 | UDP channels are never recycled (peer affinity + the local source port the backend sees) |
| D9 | `[server].max_pool_size` deleted → `[server.data].max_tunnels_per_client`, default `0` = unlimited |
| D10 | `health_check` deleted (probe, state change and key) |
| D11 | Heartbeat kept, but the server **declares** its interval in the session ack; the client derives `max(10 s, 2 × interval + 5 s)`; `interval = 0` ⇒ no client timeout |
| D12 | `pool_size` split: TCP warmth is client-local; UDP parallelism is `udp_workers` |
| D13 | Bounds live at the connection layer; the server does not arbitrate channel counts |
| D14 | Over-cap ⇒ typed, non-fatal refusal; growth stops, and a retry needs a tunnel's death or a shrink |
| D15 | Growth thresholds and hysteresis stay internal constants until measured (they still are) |
| D16 | `direct` mode kept (sparse visitors + measurement control arm); M7 measured its role |
| D17 | IPv6 path MTU is covered by an in-crate `Ipv6Mtu` declared with nix's exported `sockopt_impl!`/`getsockopt_impl!` — no `unsafe` |
| D18 | Log model: level contract + aggregation + a log-budget test |
| D19 | Config shape: client-level policy, service-level intent; no knob without evidence |
| D20 | One commit per milestone with its own verification; docs in the same commit |
| D21 | Scheduling is two layers: pool placement (client) and pairing/assignment (server); the server does not choose tunnels |
| D22 | Instrument before policy (falsifiable): S1 adds only read-only accessors + telemetry; if the state spread is inside the noise, S2 does not land — **it was zero, so it does not** |
| D23 | Placement policy = eligibility + rotation + hysteresis (never a weighted score) |
| D24 | A stripe group's K streams must land on K distinct tunnels — **structural since the group request landed**: the server names the group before its channels are opened and the client reserves one tunnel per stripe (v4, extended in place) |
| D25 | No RTT sampling; the algorithm may use stream count, pending opens, send credit, worker queue depth — nothing else (send credit is not exposed by the engine, so it is not used) |
| D26 | Growth/shrink is a hysteretic, rate-limited state machine (≤ 1 tunnel per maintenance tick) |
| D27 | UDP assigns a *new* peer to the shortest worker queue — **falsified by measurement** (2026-09-30): a single visitor never fills a queue (its own socket buffer throttles it), many visitors fill them evenly, and the drop equals the excess over a per-pool ceiling that `udp_workers` does not raise |
| D28 | Waiting visitors stay FIFO; **spare** streams would be picked from the least-loaded tunnel — not landed (S1) |
| D29 | Stripe pairing is atomic: K spares from K distinct tunnels, all or none — server-side today (a broken channel discards the whole group and re-requests it) |
| D30 | Shrink requires `pinned_peers == 0`: a channel with pinned-but-idle peers is not idle |
| D31 | New sources never create channels (invariant); floods cost affinity entries only; table size, evictions and per-tunnel `pinned_peers` are in `MOLEHILL_UDP_STATS`; no hard cap until measurement asks for one |

### Rejected, with the reason (so it is not re-litigated)

Configurable `min_tunnels` (warmth is derivable); keeping `[server].max_pool_size`
(the bound belongs at the connection layer); `health_check = false` as a boolean
axis; server-side inference of backend health (indistinguishable from a visitor
that hangs up); the "health check avoids control-plane churn" argument;
per-service health numbers; reversing the heartbeat direction (still a
follow-up); cross-carrier striping (the slowest carrier gates the group);
`carrier = "auto"`; the control channel inside the pool; a non-zero default for
the server's tunnel cap; negotiating channel counts; weighted placement scores;
an RTT sampler; stateful prioritisation of waiting visitors; treating "zero
streams" as sufficient for shrinking; a whole-group `open_streams(n)` entry
point in the client (it had no wire command to trigger it — the open thread
above is what would give it one).


## Release (v0.10.0)

1. ~~Freeze~~ **done (2026-09-28)** — `version = "0.10.0"`, the `[Unreleased]`
   content moved under a dated `## [0.10.0]` section (now dated 2026-10-01, the
   tag day), `[Unreleased]` left empty, the withdrawn v0.9.1 artifacts deleted.
2. ~~Sweep~~ **done (2026-10-01, at `96bda00`)** — the artifact, the charts and
   both READMEs describe this commit: four tools, 8/8 stages each plus the
   capacity ramp, `--test=rrul,capacity`, `shape_legs=visitor`, the bounded
   rate-class window, `tree_clean: true`, and **`just soak-check`: `OK: no gate
   violation`**, no waiver. The gate's own self-check is the verdict: the
   v0.9.0 baseline is not a gate input (another host, and a method record
   missing six keys), which is the documented behaviour for a fresh sweep.
   Two earlier sweeps are superseded — `bbe9664` and `f34788d` — both because a
   `tests/` change landed after them, which is exactly what the pre-tag rule
   treats as invalidating the artifact.
3. Before the tag: `just tag-check` must be green on the release commit (it is,
   and its provenance line reads `results measured at v0.10.0-1-g96bda00; no
   code changed since`). **Only docs and assets may follow the sweep** —
   anything under `src/`, `tests/`, `Cargo.*` or `benches/scripts/soak/*.py`
   invalidates it, and that cost this release two extra sweeps.
4. ~~`just check`, `just interop`~~ **both green (2026-09-30)** — the full
   chain, and the interop matrix's three cases (both cross-version directions
   refuse, the refusing process keeps serving its own version).
5. ~~CI green, merge, changelog date, `just tag`, push the tag~~ **the first two
   tag pushes failed, and nothing was published either time** (2026-10-01):
   - **The emulated legs.** The release workflow tests every target it
     publishes, and the cycle's new `tests/noise_keys_test.rs` spawns the
     freshly built binary — which cannot be exec'd from inside an emulated test
     binary, so `arm-unknown-linux-musleabi` died with `Exec format error` and
     cancelled the matrix. Second time this class has failed a release; the fix
     is the guard the first incident introduced (`native_target`, file-level on
     `noise_keys_test.rs`, site-level on `pool_test.rs`'s one spawning
     scenario), verified by falsification rather than assumed.
   - **The musl leg.** With the cross legs green, `x86_64-unknown-linux-musl`
     stopped on `startup_failure_test`'s `Address already in use` — musl spells
     EADDRINUSE `Address in use`. The binary was right in both; the assertion
     was pinned to glibc's phrasing, and only the release matrix ever runs
     these tests under musl (the native legs are glibc, the cross legs are the
     guarded ones). Reproduced locally (`rustup target add
     x86_64-unknown-linux-musl` + `musl-tools`, then the matrix's own command),
     fixed to match what every spelling shares, and verified by running the
     whole suite on both targets.
   - **The `aarch64` cross leg, on the cache this cycle added.** With both
     fixes in, `aarch64-unknown-linux-musl` died before compiling anything:
     `libc`'s and `generic-array`'s build scripts, restored from a 347 MB cache
     entry, could not run in the `cross` container (`GLIBC_2.28 not found`).
     `Swatinem/rust-cache` keys on the job, not on the matrix value, so every
     leg shared one entry — and the legs do not build in the same environment:
     the native legs compile host binaries with the runner's glibc, the `cross`
     legs execute them inside a container with an older one. `ci.yml` already
     keys its matrices per target; `release.yml` was the outlier and now does
     the same (`key: ${{ matrix.target }}-release`). This one is a workflow
     change, so it does not touch the sweep's provenance.
   Each failure was fixed, the sweep re-run where the rule demanded it, and the
   tag deleted and re-pushed — which is what AGENTS.md §5 allows, and the
   reason the rule exists.
6. **Next**: push the tag → the release workflow publishes (GitHub Release,
   GHCR, crates.io).
7. **Open, for the next cycle**: nothing mechanically stops the next
   spawn-based test from missing the `native_target` guard, or the next
   assertion from pinning one libc's wording — these two failures were the
   second and third of their kind. A cheap check would close both (fail when a
   `tests/*.rs` contains `CARGO_BIN_EXE` without `native_target`, and when a
   test asserts a bare libc phrase); it needs a home in the check chain and the
   docs that go with it, so it was not added mid-release.

### Release sweep (2026-10-01, `96bda00`)

`v0.10.0-1-g96bda00`, tree clean, `stale: false`, binary sha256
`228cb43cc2c1e493` (4 181 840 bytes), host `99919695eec2` / host_id
`d764f9da9c7e5b2a`, calibration 417.8 MiB/s, loopback probe 21.59 Gbit/s,
`shape_legs=visitor`, `rate_socket_window=256K`, `batch=2`, four tools, 8/8
stages each plus the capacity ramp, `--test=rrul,capacity`. **`just soak-check`:
`OK: no gate violation`**, no waiver. Charts re-rendered and both READMEs
refilled from the plot's own tables. The peers are the ones re-downloaded for
this release (frp 0.71.0, rathole 0.5.0, nps 0.26.10 — all still the latest).

| tool | clean bulk (Gbit/s) | replicate | clean p99 (ms) | loss1 | rate100 | rate20 | ramp |
|---|---|---|---|---|---|---|---|
| molehill | 15.921-16.830 | 5.4 % | 8.4-9.4 | 9.727 | 0.100 | 0.019 | 8/8, never broke |
| frp | 6.181-6.191 | 0.2 % | 2.9-3.0 | 5.862 | 0.099 | 0.019 | 8/8, never broke |
| rathole | 12.822-12.867 | 0.3 % | 102.3-104.5 | 9.679 | 0.100 | 0.020 | 8/8, never broke |
| nps | 0.134-0.135 | 0.5 % | 64.4-66.9 | 0.142 | 0.100 | 0.020 | 0/8, broke at 1 (p99 204.84) |

What this run is worth reading for:

- **Every cell is a measurement this time**, including all four ramps: three
  arms carry the ramp's full 8 streams (the subject and both TCP peers), and
  only `nps` breaks, at its first load level — the same shape the `bbe9664`
  sweep had, and unlike the `f34788d` attempt whose `rathole` ramp stopped on a
  dead iperf3 backend.
- **The subject's SLO is met on both clean visits** (8.4 and 9.4 ms against the
  50 ms limit, zero errors), and its completeness, endpoint invariant and drift
  checks all pass.
- **It is the third sample of the top pair of arms**, and the one that settled
  what a run can say about them: across three sweeps of identical code their
  clean readings span 15.9-22.2 and 12.8-20.4 Gbit/s — swings of 39 % and 59 %
  that reverse their order — while `frp` (6.04-6.19) and `nps` (0.133-0.136),
  an order of magnitude below this host's loopback ceiling, moved by under 3 %,
  and the host's own loopback probe stayed inside 21.6-21.9 Gbit/s throughout.
  The READMEs publish no ordering for those two rows and `docs/benchmarks.md`
  states the limit with all three samples behind it.

**Environment incident on the way here.** One attempt at this sweep produced a
complete-looking artifact in 14 seconds: `iperf3` was no longer installed (a
container restart had reset the filesystem and taken the runtime-installed
package with it), so every test recorded a typed
`No such file or directory: 'iperf3'` failure. The gate refused it, the
superseded artifact was restored from git rather than published, `iperf3` was
reinstalled, and the run repeated. The environment notes below now name the
mechanism; `command -v iperf3` before a long run is the cheap guard.


## The bench model's verdicts were rebuilt, then the record re-measured (2026-10-13)

The model's consistency leg was "every paired round agrees in sign" — at the
round counts the profiles run, a 25-50 % false positive rule that never
printed its own error rate. `497b58e` replaced it with an exact sign test
whose p prints beside `alpha_eff` (the level the run's length can honestly
test at), a bootstrap CI over the paired difference (seeded from the
fingerprint, so re-analysis is reproducible), pre-registered endpoints
(`--primary`, in the fingerprint; the headline metric by default) with a
Benjamini-Hochberg correction over that family, leave-one-out FRAGILE
marking, drowned-cell refusal (more than half a cell's rounds failed → not
quoted, gate fails), round-index pairing (a failed round no longer shifts a
side's replicate), the iperf3 version and an analysis version in the method
(schema 3), and an opt-in per-round control normalization. Two coverage
defects fell out of the first gated screen (`09c8df3`, `700cf11`): the gate
expected one empty-named cell from every non-staged scenario while five of
them name their operating point, so every bulk/rr/UDP run failed coverage
and it had never been noticed because only sweeps were gated; the naming is
now derived per workload kind.

Then the record was re-measured under the new model (all six files in
`~/tmp/recheck-*.json`, each gated PASS):

| decision | new-model read | verdict |
|---|---|---|
| pool width (`l4-mux2` vs `l4-mux8`, bulk-n) | 5.77 vs 12.93 Gbit/s, 2.24x, CPU/byte also better | holds, direction and magnitude far beyond the run's 0.78 % floor |
| carrier axis (KCP vs TCP pins, `loss1_buf`, ladder) | KCP delivers more on every cell (844 vs 772 at 1000M; 906 vs 820 at 2000M; 846 vs 791 at 5000M), less inner loss | holds |
| L3 TUN queue (`l3` vs `l3-deep`, clean) | 2.41 vs 2.38 Gbit/s, 1.5 % apart | no claim — the clean path is packet-bound, as recorded |
| lending (+27 % claim, `rtt100` carrier leg) | both arms 0.06 Gbit/s where the record says 2.4 | **changed, not confirmed — see below** |
| P1 lock removal (A/B binaries, rr-16 + bulk-n) | every pair `indistinguishable` with the interval *inside* the threshold (RSS ±0.2 MiB, throughput ±5 %, CPU −0.9 %) | holds, and sharpens: the old "RSS directional −5 %/−7 %" read becomes bounded evidence of no difference |

**The lending claim is window-era, not wrong.** On `rtt100` on the carrier leg
both `l3-budget10` arms now read 0.06 Gbit/s where the 2026-10-11 record says
2.395 — while the *control* (no tool) carries 3.44 on the same path and the
same arms carry 3.2-3.3 on a clean loopback, so the L3 data path is healthy
and the gap is the KCP carrier itself: `KCP_RCV_WND` stays 4096 segments
(≈ 5.8 MiB ≈ 0.47 Gbit/s at 100 ms — the constant's own doc comment says
receive was deliberately left for its own measurement) and the +27 % was
measured before the send window moved 2048 → 8192. The decision the record
supports today is therefore *narrower*: lending's benefit is conditional on
what limits the path, and at the current window on a 100 ms leg the path is
window-limited before it is lane-limited, so the claim does not transfer.
Re-measuring it belongs with the next KCP window change, not with lending's
own code. (Clean-loopback lending re-read: 3.289 vs 3.239 — indistinguishable,
as recorded.)

**Why the old record read differently**: the model said "claim" at sign
agreement 1.0 with a floor from two rounds; the new model says what the run
can test at and puts an interval around it. Nothing above contradicts a
direction the old model printed — it contradicts how far the old model let a
reader carry one.

**The probe-and-compare form (recorded, not landed).** The delivery signal
measures 100 % on the policed path — reliable ARQ turns policing into
throughput — so the rule's next form is the L3 lane policy's own move: put a
tunnel on the other carrier and compare what the two carry. The design is
settled and the constants are picked (a 1.5x carried-bytes-per-second edge,
the same ratio the lane policy uses; a 30 s probe budget; a 600 s backoff
after an inconclusive one); what stopped it landing here is the wiring it
needs: `begin_handover` does not report *which* tunnel it took (the probe
must follow its own tunnel's counters), and the probe's carried bytes need a
per-window per-tunnel byte counter beside the ARQ pairs in `HealthSample`.
Both are small, both are mechanical, and neither should be assembled at the
end of a session — the first wiring attempt was reverted for exactly that
(`git checkout` on a 285-line half-state-machine). The saved first draft is
in `~/tmp/l4-probe.wip.patch` for whoever picks it up; the shape that
survives review there is `probe_step` (start/backoff) + `probe_judge`
(per-window verdict), with `begin_handover` returning the tunnel id.


## The L4 probe landed, and the sighting protocol says what it can and cannot see (2026-10-13)

The carrier rule's probe-and-compare form landed (`460acb0`, `cbc28ae`): the
rule's suspicion — a tunnel re-sending ≥4 % of its window — hands one tunnel to
the other carrier through the pool's own handover, routes **new** streams to
the probe while it runs, and compares carried bytes per second per tunnel
(incumbent averaged, probe alone) past the lane policy's 1.5x edge. A probe
that carries materially more moves the endpoint's choice through `prefer`
(switch cooldown included); one that carries materially less gives the tunnel
back and parks the pool in a 600 s backoff. A probe whose window carried
nothing is **held**, not reverted — measured, four probes in a row reverted on
an idle-window artifact before that floor existed. The delivery-ratio verdict
stays as the immediate move for a tunnel that actually stops delivering. Unit
tests cover the verdict, the window arithmetic and both lifecycle outcomes on a
real pool; the suite is 262 lib + 12 pool + 23 integration green, clippy two
passes clean.

What the sighting protocol then established on the policed condition
(`udp_throttled`, tunnel leg, `RUST_LOG=debug`, `MOLEHILL_TUNNEL_STATS=1`):

- **The suspicion is the only local signal that fires there, and the probe
  starts on it.** Sighting 2: share 27-31 % per window while delivery stays
  pinned at 99-100 % — the ARQ re-sends until everything arrives, so the
  delivery verdict (60 %) never fires on a policed path at all. Sighting 3:
  four suspicions, four probes started, four gave back (the idle-window
  artifact, since fixed), zero false moves. Sighting 4 (after the placement
  preference and the idle floor): four probes started, none reverted, none
  moved — the probe tunnels sat idle their whole budget.
- **Why the probe stayed idle: the no-disruption rule.** A handover drains
  only an *empty* tunnel (streams == 0), so the probe never inherits traffic;
  placement routes only *new* opens; and the ladder is a **UDP** workload whose
  `udp_workers` channels open once at service activation, so a probe that
  starts mid-run never sees a new open. `churn-16` produces continuous new
  opens but too little traffic to arm the suspicion (15 KB/window against the
  64 KiB floor — sighting 5). The two needs (heavy traffic + fresh opens) are
  not met by any workload the model has today.
- **The seam for a fair comparison exists and is out of scope by the
  no-disruption rule.** `UdpHub::repin` (D30) moves a pinned peer onto another
  channel — but mid-flight moves drop datagrams (`try_send`), and the rule
  this cycle shipped on says a lane change routes *new* flows, never existing
  ones. A fair probe on a static-UDP workload would need a deliberate policy
  repin, which is a decision, not a defect.

**Net state: the probe is real, wired, safe and measured; what it cannot yet
demonstrate is a live Move on a bench workload, because no workload both arms
the suspicion and produces opens after the probe starts.** The next
instrument, recorded rather than promised: a ladder-flavoured churn (the
ladder's rate with churn's re-dialing) as a bench scenario, or an explicit
operator repin under the same audit the lane move carries. Until then the
rule stays opt-in (`MOLEHILL_TUNNEL_CARRIER_RULE=1`), and its Move path is
exercised by the pool test, not by a live bench run.


---

## Index

### Added 2026-10-16 (the release audit and the L3 throughput question)

- **Why L3 is slower than L4, and the three fixes measurement refused** — the
  per-packet cost that is the whole deficit (four TUN syscalls per data packet,
  ~4.6 cores of system time), the three candidate fixes that were built or
  measured and refused (a serialized reader, the routing-table mutex,
  multi-queue TUN readers), and the one test that could not resolve anything.

### Added in the 2026-10-16 move (v0.11.0 preparation)

- **The L3 cycle's investigation records** — the seven mechanisms that were
  proposed for the wedge and refuted by measurement, the two harness defects
  behind it (a shaper left on its implicit 1 000-packet queue, and a wedge metric
  that measured the probe's own patience), and the reading that resolved them.
- **The L3 cycle's "next, in order" list, as it closed** — five items, all
  closed, with the reasoning that closed them.
- **The v0.10.0 release, as it was recorded** — the shape of the previous
  release, and the three failures its four tag pushes cost.

The table below indexes the records that were moved before those, and the
section after it the ones that predate this file.

| Record | What it settled |
|---|---|
| [Fixed: striping with the elastic pool](#fixed-striping-with-the-elastic-pool) | the stripe group no longer picks a cold pool; the striping x pool interaction |
| [Milestones, status and evidence](#milestones-status-and-evidence) | M1/M2a/M6/M7 status at the 2026-09-28 freeze |
| [Config shape (as shipped)](#config-shape-as-shipped) | the 0.10 configuration surface as first shipped (now owned by configuration.md and CHANGELOG.md) |
| [Pre-merge triage](#pre-merge-triage-2026-09-30-the-branchs-ci-was-red) | four red CI jobs (feature-reduced fixture, Windows UDP datagram, cancelled matrix, cache key) and the dependency refresh |
| [Release sweep (2026-09-29)](#release-sweep-2026-09-29) | the first sweep on the visitor-leg scope |
| [Release sweep, finally](#release-sweep-finally-2026-09-30-fb2542a) | the sweep at `fb2542a`, superseded by the frozen-commit sweep |
| [D27's evidence, measured](#d27s-evidence-measured-2026-09-30) | the UDP drop-counter premise falsified; the per-pool ceiling documented |
| [One probe pinned the whole harness](#one-probe-pinned-the-whole-harness-2026-09-30) | a self-inflicted CPU pin on the loopback probe |
| [A failed start is no longer silent](#a-failed-start-is-no-longer-silent-2026-09-30) | an instance that cannot serve now ends the process |
| [The comparability key gets a path probe](#the-comparability-key-gets-a-path-probe-2026-09-30) | `host_loopback` joins `host_calibration` in the comparability key |
| [Release sweep (2026-09-30)](#release-sweep-2026-09-30) | the sweep the drain budget refused |
| [The dead bulk spine: four defects, one retraction](#the-dead-bulk-spine-four-defects-one-retraction-2026-09-29) | the drain predicate, the single dial, the budget — and two retracted diagnoses |
| [The instrument's claims](#the-instruments-claims-what-the-gate-now-refuses-to-say-2026-09-29) | what the gate refuses to say, and why |
| [The rate cells get a bounded sender](#the-rate-cells-get-a-bounded-sender-2026-09-30) | the rate classes' bounded socket window |
| [The pool-size and shared-pool questions, answered](#the-pool-size-and-shared-pool-questions-answered-2026-09-30) | `max_tunnels = 4` vs `8`, and shared vs per-service pools |
| [The stripe group's cold-pool cost, measured](#the-stripe-groups-cold-pool-cost-measured-2026-09-30) | what striping costs before the pool is warm |
| [The stripe group command, landed](#the-stripe-group-command-landed-2026-09-29) | `CreateDataChannelForStripe` rides the control channel |
| [The pool-size question, re-opened](#the-pool-size-question-re-opened-2026-09-29) | the placement half, measured instrument-first |
| [Shaping scope, the rate cells, and the shaped-cell rule](#shaping-scope-the-rate-cells-and-the-shaped-cell-rule-2026-09-29) | visitor-leg shaping, the shaped-cell rule, the load axis in one artifact |
| [The freeze found a gate that would have shipped empty release notes](#the-freeze-found-a-gate-that-would-have-shipped-empty-release-notes-2026-09-28) | the changelog gate's three properties |
| [Incident: the withdrawn v0.9.1 tag](#incident-the-withdrawn-v091-tag) | why v0.9.1 was withdrawn |
| [Release review](#release-review--the-state-a-reviewer-should-check) | the reviewer's snapshot at `d8233c4` (superseded by the front page) |
| [Pre-tag gaps found by the release audit](#pre-tag-gaps-found-by-the-release-audit-2026-09-28--all-closed) | the three audit gaps and their closure |

## Records archived before this file existed

The cycle's records *up to the 2026-09-28 freeze* — M1 (protocol v4), the
stream-cap leak investigation, M2a, M6, M7, the S1 placement observation, both
post-review rounds, the cycle's first four sweeps and the CI-verification
incident — were archived into git history at `f2156de chore(release): re-sweep
v0.10.0 on the release commit`, in the revision *before* the one that archived
them:

```
git show f2156de^:HANDOFF.md
```

Named by revision rather than by the `v0.10.0` tag on purpose: the tag lands on
a commit that already carries the shortened page, so `git show
v0.10.0:HANDOFF.md` returns that page, not the records. What each one settled,
so it can be navigated:

| Record | What it settled |
|---|---|
| M1 — one session per endpoint | one authenticated control session per `(remote_addr, effective transport)` carries every service; per-service auth inside it; the server declares the heartbeat cadence in the session ack |
| The stream-cap leak (2026-09-26) | the engine's 64-stream cap is reachable and the bulk spine dies when it is; the shaped `rate20`/`jitter` failures reproduce on the **released v0.9.0 binary** too, so those cells describe the shaped path, not a v0.10.0 regression — which is why the release decision does not rest on them |
| M2a — shared elastic pool | one pool per carrier, cold start, `max_tunnels` cap, the UDP-derived floor; growth is the client's own decision, shrink is conservative |
| M6 — configuration surface | exactly what 0.10 removed and what to write instead |
| M7 — `direct`'s role | what `direct` costs and buys against the mux; kept for sparse visitors and as the measurement control arm |
| S1 — placement observation | the per-tunnel state spread is zero, so S2/D28 do not land (D22 applied as written) |
| Post-review round 1 | burst spreading, per-visitor pairing, the stable host key |
| Post-review round 2 | striping on a v4 session, the mux reader/writer waker fix, and the v3 removal |
| The cycle's four sweeps | one per candidate release commit, each superseded by the next; a superseded sweep is evidence about the harness as much as about the tool |
| CI caught what `just check` cannot | the local chain never compiles every feature set, so a release-shaped change is not verified until the powerset and the minimal profile have both run — that is what those CI jobs are for |

## Historical records (pre-v0.10.0)

Everything below this line was measured before the Soak model, or by it, on
branches that are now history. Per the file's own rule (kept verbatim from the
previous revision): those records say what the branch's authors believed at the
time and why a decision was taken, and **no number in them may be quoted as a
measurement of the current code, compared against a Soak result, or used to gate
anything**. They live in git history; this table is the index.

| Record | Where it lives now |
|---|---|
| The data-path rework (vendored yamux, CPU/ceiling probes, the copy map, the KCP zero-copy links L1/L2/L3/S1, the reverted M1, the parked N1, the stripe K=4 experiment, the noise resume measurement) | `CHANGELOG.md` `## [0.9.0]`; HANDOFF at v0.9.0 in git history |
| The retired per-cell matrix, its A/B harness bug and every figure taken with it | withdrawn in the v0.9.0 cycle; see `CHANGELOG.md` `## [0.9.0]` |
| The cumulative branch-vs-`main` A/Bs (2026-09-22 void, 2026-09-24 valid) | HANDOFF at v0.9.0 in git history |
| The scheduling-review candidates A-D (parallel channel establishment, state-aware placement, UDP drop visibility, PMTU-aware KCP segments) | A and B/D were this cycle's M2a/M2b inputs; C and D remain candidates in git history, with their gates |
| The Soak model's introduction and the four harness defects it fixed | `docs/benchmarks.md` (the model) and git history (the defects) |
| The v0.9.0 provenance record and the KCP Linux-only release blocker | HANDOFF at v0.9.0 in git history; the platform lesson survives in `build.rs` |
| The v0.8.x transport comparison and the archived QUIC arm | `CHANGELOG.md` of those releases; the `archive/transport-test` tag |

The rules that came out of those incidents bind this cycle too, and they live in
[AGENTS.md](AGENTS.md) §10 (measurement discipline) rather than here.

The rules that came out of those incidents bind this cycle too, and they live in
[AGENTS.md](AGENTS.md) §10 (measurement discipline) rather than here.

---

# The cycle's records

## Fixed: striping with the elastic pool

**The stripe livelock had two causes, both fixed.** The earlier quarantine
(a v4 session served unstriped, `striped_data_channels` `#[ignore]`d — see the
archived records) is lifted: the test runs in the suite now and the server
serves `stripe_count > 1`.

1. **The gather never asked for its channels.** A v4 registration opens no
   data channels of its own — the tunnel pool starts cold and the server
   asks for one channel per visitor — but the striped gather waited for
   channels without sending a single `CreateDataChannelFor`. On a cold pool
   (the elastic pool's default state) it waited for the visitor's whole
   25 s budget and then shed it, which is what the old falsification
   matrix's "cold pool as it ships | 0/3" row measured. The gather now asks
   for one channel per stripe before its first wait, and re-requests only
   the stripes still missing when a budget expires. Falsified by disabling
   the request loop: `striped_data_channels` then fails at its readiness
   probe.

2. **A reader's park erased the writer's waker.** With the requests in
   place the group still hung about one run in seven — always as a bulk
   transfer that delivered most of its bytes and then stopped, with both
   sides' stripe send directions parked mid-frame. The vendored mux parks a
   stream's reader and writer on the connection's per-stream command
   channel, and both stored their waker in `Shared::writer`. A reader
   queueing a window update — the frame that returns the peer's send
   credit — parked *last* and overwrote the writer's waker, so the credit
   that came back woke nobody: the send direction slept until an unrelated
   resize happened to notify. Instrumentation showed the shape directly: the
   last no-credit park on the stalled stream, a peer reader that kept
   polling and refusing its own updates below the half-window threshold,
   and both stripe senders stuck on frame N while both receivers waited
   for it. Reproduced deterministically (a first attempt at a fix — lowering
   the window-update floor to one frame — made the failure 6/6, because a
   reader that queues an update per frame fills that channel from its side
   constantly), then fixed: the reader parks in `Shared::reader_park`, and
   `wake_stream_writer` wakes both slots. Pinned by
   `mux::connection::tests::a_readers_channel_park_keeps_the_writers_waker`,
   which fails with "a reader's channel park must not erase the writer's
   waker" when the fix is reverted; a 16-round × 8 MiB striped stress
   reproducer (`dbg_stripe_stress`, not kept) went from 1-in-7 to 0 failures
   in 100+ groups.

   The window-update floor stays at half the window: with the waker fixed
   it measured no throughput difference (5 s for a 16-round stress either
   way), and the smaller floor doubles the update frames on the read path.

**What would falsify the fix**: a green `striped_data_channels` in three
consecutive runs (it is in the suite now), the waker unit test above, and a
stress run of repeated 8 MiB striped round trips.

**Closed (2026-09-29)**: the guarantee is now structural. The open thread's
design landed as one command in the dialect this branch already has —
`ControlChannelCmd::CreateDataChannelForStripe(service, group, index, count)`,
part of **v4**, which is extended in place — so the server names the group
before its channels are opened, and the client (a) grows the pool to
the group's own count first, bounded by `max_tunnels`, and (b) reserves, per
stripe, a tunnel the group does not already hold, falling back to the ordinary
least-loaded rule when the pool cannot spread. A real cold pool is the evidence:
the client's `pool-stats` log during `striped_data_channels` reads
`reason="stripe" from=0 to=1 … 3 to=4` (it stayed at 1-2 before), and the test
asserts the pool reached its four stripes. The request vocabulary — one
`CreateDataChannelForStripe` per stripe, same group, index order, the group's
own count — is pinned against a hand-written peer by
`a_striped_gather_names_its_group_on_every_request`, so a change to what the
server asks for cannot hide behind the client that reads it. The placement rule
alone was never the guarantee, which is why this needed the wire.


### Milestones, status and evidence

| # | Milestone | Status | Evidence |
|---|---|---|---|
| M0 | Interop matrix | landed (merged) | `tests/interop_test.rs`, `just interop` |
| M1 | One control session per endpoint (protocol v4) | **landed** | archived record, "M1" |
| M2a | Shared elastic pool + S1 observation | **landed** | archived records, "M2a" and "S1" |
| M2b | S2 placement + D28 spare selection | **not landed, on purpose** | the S1 spread is zero (archived, "S1") |
| M2c | UDP shortest-queue assignment (D27) | **not landed — premise falsified by measurement** | the counters fire only when the whole pool is saturated, with an even visitor spread and no dependence on `udp_workers` (below, "D27's evidence, measured") |
| M3 | Transparent visibility (health check deleted) | landed (merged) | `CHANGELOG.md`, the dead-backend test |
| M4 | IPv6 path MTU | landed (merged) | the `#[ignore]`d netns test |
| M5 | Log model | landed (merged) | `tests/log_budget_test.rs` |
| M6 | Configuration surface | **landed** | archived record, "M6" |
| M7 | `direct`'s role | **measured** | archived record, "M7" |

### Config shape (as shipped)

```toml
[client.data]
shared_pool = false         # one pool for the session's services, per carrier
default_carrier = "tcp"
idle_timeout = 60
[client.data.tcp]
max_tunnels = 4             # cap; the pool grows below it, cold at first
[client.data.kcp]
max_tunnels = 4
[client.services.game]
protocol = "udp"
udp_workers = 2             # the UDP worker set's channel count
[server.data]
max_tunnels_per_client = 0  # 0 = unlimited (default); an operator valve
```

Removed (each warns once in this release and names its replacement; from the next
release the key is an error): `default_count`, a service's `count`, a service's
`pool_size`, a service's `heartbeat_timeout`, `[server].max_pool_size`. The pool
has no initial size: it starts cold and grows on demand.

### Pre-merge triage (2026-09-30): the branch's CI was red

Found while preparing the merge: the PR's CI run at `b996d24` was **red**, in
four jobs, all of them this branch's own defects — `just check` on Linux with
default features cannot see any of them, which is exactly what the matrix jobs
are for. All are fixed; none of them is a product regression on the code paths
the sweep measured except where noted.

1. **`test noise` — the striped v4 scenario loads a `multiplex`-only config.**
   `tests/session_test.rs::a_striped_gather_names_its_group_on_every_request`
   starts `tests/for_tcp/session_v4_striped.toml`, which sets
   `[server.data] stripe_count` — and `ServerDataConfig` is `#[cfg(feature =
   "multiplex")]`, so the leg without that feature failed to parse the fixture
   ("unknown field `data`") before the scenario ran. The scenario and its three
   constants are now gated on the feature, the way `integration_test.rs` gates
   its multiplex scenarios. Verified locally in the same feature set: 7 passed,
   0 failed.
2. **`build x86_64-pc-windows-msvc` — an oversized UDP datagram killed the
   service.** `udp_buffer_size` promises truncation, and POSIX delivers it
   (the kernel fills the buffer and returns its length). Windows fills the same
   buffer with the same prefix but returns `WSAEMSGSIZE` (10040) — `mio`
   documents that split in its `net` module notes — and both receive paths
   treated that error as fatal: the server's UDP pool died (the client was told
   the service was no longer exposed) and the client's per-peer forwarder broke
   on an oversized backend reply. Fixed at both sites: the server reads a
   whole-datagram buffer and applies the limit to what it read, because a
   failed `recv_from` is also where the visitor's address is lost; the client's
   connected socket reads the error as the full buffer it stands for
   (`common::helper::datagram_len`, with unit tests for the translation).
   `udp_buffer_size_bounds_a_datagram_without_breaking_the_channel` is the test
   that caught it and now pins both halves — **CI is the only place it can be
   verified from this host**, so that job is the evidence to watch on the next
   push, not the local run.
3. **The Linux and macOS `build` legs were not failing — they were
   cancelled.** The matrix had the default `fail-fast`, so the Windows failure
   cancelled the two in-flight legs, which reads like a platform verdict and is
   not one. `fail-fast: false` now, so a run answers "does every platform
   build?" instead of "which platform failed first?".
4. **The feature-matrix legs never had a cache.** `Swatinem/rust-cache`
   rejected the key ("cannot contain commas") because it was built from
   `matrix.args`; the key is `matrix.name` now.

**Dependency refresh (its own commit, per AGENTS.md §9).** `cargo outdated
--root-deps-only` is clean ("All dependencies are up to date"), which needed
two breaking bumps, not just the compatible ones: `base64` 0.22 → 0.23 (API
unchanged for `Engine`/`STANDARD`) and `chacha20poly1305` 0.10.1 → 0.11
(`aead` 0.6: `AeadInPlace` → `AeadInOut`; `Key::from_slice`/`Nonce::from_slice`
are deprecated in favour of the infallible `From<[u8; N]>`, which is what the
resume path now uses; the `std` feature no longer exists — `alloc` is the
equivalent). The Noise resume tests, `noise_keys_test` and the 20 lib noise
tests all pass on the new generation.

**What the refresh cost, measured.** `cargo deny`'s duplicate-version warnings
went 22 → 28 groups, and all six new ones (`aead`, `chacha20poly1305`,
`cipher`, `inout`, `poly1305`, `universal-hash`) are the same fact: **snow
0.10.0 pins the 0.10 RustCrypto generation**, so our 0.11 sits beside it. The
`license-not-encountered` warning is gone (the `BSD-2-Clause` allowance no
dependency used was removed from `deny.toml`). The one warning that stays is
the `chacha20@0.10.1` yanked advisory waived in `deny.toml` — still correct:
`rand` 0.10.3 (the newest) and `chacha20poly1305` 0.11 both require
`chacha20 ^0.10`, so no manifest of ours can remove it. Two ways to trade the
six new warnings back, if a later cycle wants them: keep our AEAD on 0.10.1
(one generation in the tree, but our direct dep is then a version behind), or
drop snow's `default-resolver-crypto` (the ring resolver carries the ciphers;
the handshake hash still needs the pure-Rust one) — the second is a real change
to the Noise backend and needs its own verification, not a cleanup commit.

**The changelog's upgrade instruction was wrong, and `just interop` is how that
was settled.** The `[0.10.0]` section still carried the sentence the v4 *client*
commit wrote — "a v0.10.0 server still serves a v0.9.0 client" — and advised
upgrading the server first. `df4f2e5` had already dropped the v3 server path,
and `docs/configuration.md` said so; the two pages disagreed. Run against the
released v0.9.0 binary, `just interop`'s three cases pass and name the truth:
`new_server_refuses_old_client_and_keeps_serving` and
`old_server_refuses_new_client_and_says_so`. The changelog now states that both
ends refuse each other's dialect, points at the configuration page for the
migration, and the wrong advice ("server first") is gone. This is the item the
pre-tag checklist's "CHANGELOG audit" exists to catch — a grep would not have:
both pages read plausibly on their own.

**The ordering consequence.** The release checklist above was written when the
last sweep was the last code change. It is not any more: the triage touches
`src/`, `Cargo.toml` and `Cargo.lock`, so `just tag-check`'s drift check will
fail the tag until a sweep is measured on the frozen commit. Freeze → sweep →
docs (changelog date, PR body) → merge → tag.

### Release sweep (2026-09-29)

`v0.9.0-101-g401aeda`, tree clean, fresh release binary (`stale: false`), host
`2967a5748835` / `host_id d764f9da9c7e5b2a`, four tools, 8/8 stages each,
`--test=rrul`, ~120 minutes. Charts and both READMEs are refreshed in the same
commit.

**`just soak-check`: `OK: no gate violation`.** Every one of the 32 tool-stages
carried its bulk spine inside its own window, **every one of them on its first
dial**, the endpoint invariant holds, and the released tool is inside the SLO on
both clean stages. The comparison half is skipped, as it is here by default:
`results-soak-v0.9.0.json` is the baseline candidate and it has no `host_id`, so
the run is gated by its own checks.

Per-stage bulk intervals / peak Gbit/s this run:

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean |
|---|---|---|---|---|---|---|---|---|
| molehill | 147 / 21.73 | 111 / 2.950 | 116 / 5.949 | 109 / 2.930 | 115 / 0.720 | 112 / 0.177 | 115 / 0.503 | 147 / 23.80 |
| frp | 147 / 6.797 | 111 / 2.765 | 115 / 5.392 | 108 / 3.320 | 116 / 0.707 | 114 / 0.000 | 115 / 0.000 | 147 / 6.881 |
| rathole | 147 / 23.75 | 111 / 3.121 | 116 / 5.381 | 108 / 3.394 | 116 / 1.062 | 114 / 0.000 | 115 / 0.090 | 147 / 23.33 |
| nps | 147 / 0.540 | 111 / 1.270 | 116 / 0.801 | 107 / 1.790 | 116 / 0.799 | 114 / 0.000 | 115 / 0.000 | 147 / 0.724 |

Two things this run shows that the previous one could not. **Every stage dialed
once**, so no cell is measured under a different load than its peers — the
previous sweep's molehill `jitter` needed a second dial at t+44 s and had to be
flagged as not load-matched. And the rate cells are no longer uniformly zero:
molehill's `rate20` and `jitter` now carry real (if thin) peaks, because the
stage starts from a path that actually satisfies the drain's predicate rather
than from whatever a fixed timer allowed. They are still mostly zero-byte
intervals (88 % and 96 % of them), so the README reports the peaks without
drawing a comparison.

The transitions themselves, per the stage records: 0.28 s, 4.1 s, 1.6 s, 9.0 s,
35.3 s, 71.0 s and 76.0 s — each ending because the path was quiet, none by
budget expiry. The previous harness spent a flat 120 s on every one of them.

**What this sweep cannot be used for.** Two boundaries a reader has to carry:

- **No cross-run comparison happened.** The gate skipped it — the baseline
  candidate (`results-soak-v0.9.0.json`) has no `host_id`, so the run is gated
  by its own completeness, endpoint and SLO checks alone. Nothing here says
  anything about v0.9.0.
- The method itself changed across this commit series (drain predicate, retry
  schedule, suffix parsing), so this sweep is not comparable to earlier sweeps
  of this cycle either; `workload_version` stays 1 because the drain has never
  been in a released version (introduced in `e33ece3`, after v0.9.0).

### Release sweep, finally (2026-09-30, `fb2542a`)

`v0.9.0-122-gfb2542a`, tree clean, fresh release binary, host `3f8b4508ab91` /
`host_id d764f9da9c7e5b2a`, calibration 421.3 MiB/s, loopback probe 22.28
Gbit/s, `shape_legs=visitor`, `rate_socket_window=256K`, four tools, 8/8 stages
each plus the capacity ramp, ~75 minutes. **`just soak-check`: `OK: no gate
violation`.**

| tool | clean bulk (Gbit/s) | replicate | clean p99 (ms) | loss1 | rate100 | rate20 | jitter |
|---|---|---|---|---|---|---|---|
| molehill | 16.331-16.438 | 0.7 % | 9.6-10.0 | 9.70 | 0.100 | 0.020 | no reading |
| rathole | 12.877-12.889 | 0.1 % | 96.9-102 | 9.68 | 0.100 | 0.019 | no reading |
| frp | 6.050-6.059 | 0.1 % | 3.0-3.1 | 5.73 | 0.100 | 0.019 | no reading |
| nps | 0.132-0.134 | 1.3 % | 57.7-64.6 | 0.142 | 0.100 | 0.020 | no reading |

The load ramp: molehill, frp and rathole carry its full 8 streams without
breaking the SLO (the ramp's own ceiling, so a floor), nps breaks at the first.
Compared with the 05:56 sweep this replaces, nothing moved beyond the run's own
replicate — which is the point: four sweeps of one day, three of them measuring
something other than the tools.

**What the three refused attempts cost and taught.**

- **06:00 — the probe pinned the harness** (below, "One probe pinned the whole
   harness"): the loopback probe's `sched_setaffinity` confined the runner, the
   tools, their iperf3 clients and servers, the pingers and the samplers to one
   core. Every fast number fell by a factor of three and the gate refused the
   run after 75 minutes. Fixed by moving the pin into a child process, plus a
   self-check that raises if the harness's own affinity changes.
- **08:32 — the new clean-stage guard fired on a healthy sweep**: rathole's
   clean p99 is ~100 ms by its own nature, and the guard applied the SLO to a
   reference peer that the gate explicitly reports and does not gate. Fixed by
   sharing one `SUBJECT` between the driver and the gate (`lib.SUBJECT`,
   re-exported by `soak_check`). Cost: the 40 minutes of batch 1's work, and a
   partial artifact (molehill + frp only) that was reverted rather than
   committed — the gate would have read it as complete.
- **07:15 — the same pin, seen before it was understood.** Listed here because
   the *diagnosis* is the durable part: a hand-rolled client (the same binary,
   an `iperf3` backend and an echo service beside it, 20 saturating streams)
   read 16.60 Gbit/s and 0.58 ms median / 4.64 ms p99 while the harness read
   5.2 Gbit/s and 163 ms at the same moment. That is how "the path is slow" was
   separated from "the thing measuring the path is slow", and it is worth
   reaching for first next time.

### D27's evidence, measured (2026-09-30)

D27 ("a new peer goes to the *shortest* worker queue") was the last milestone row
whose status read "gated on the drop counters, which have stayed at zero". Zero
under the soak schedule is not evidence — that schedule's UDP probe is a handful
of datagrams per second — so the question was taken to the counters with load
that can reach the queues: `benches/scripts/udp_stress.py`, a real pair
(`udp_workers` configurable), visitors blasting paced or flat out, the server's
`MOLEHILL_UDP_STATS` line read around every step, and **a control step that
blasts the same visitors straight at the sink**, so "the tunnel is the limit" is
a measurement instead of an assumption.

| step | offered/s | absorbed/s | Gbit/s | `queue_full` | spread |
|---|---|---|---|---|---|
| 1 visitor @ 10,000/s | 9,999 | 9,999 | 0.11 | 0 | — |
| 1 visitor @ 25,000/s | 20,115 | 20,115 | 0.23 | 0 | — |
| 1 visitor @ 50,000/s | 25,864 | 25,864 | 0.29 | 0 | — |
| 1 visitor @ 100,000/s | 29,837 | 29,837 | 0.33 | 0 | — |
| 16 visitors, flat out, `udp_workers = 1` | 166,443 | 101,657 | 1.14 | 518,776 | 17 |
| 16 visitors, flat out, `udp_workers = 2` | 162,015 | 89,474 | 1.00 | 580,730 | 10 / 10 |
| 16 visitors, flat out, `udp_workers = 4` | 162,696 | 87,236 | 0.98 | 603,889 | 4 / 4 / 5 / 4 |
| 64 visitors, flat out, `udp_workers = 4` | 154,261 | 88,674 | 0.99 | 526,900 | 16 / 16 / 17 / 16 |
| control: 16 visitors straight at the sink | 183,749 | 183,749 | 2.06 | 0 | — |
| control: 64 visitors straight at the sink | 188,570 | 188,562 | 2.11 | 0 | — |

1400-byte datagrams throughout; `no_worker` stayed 0 in every step.

**What it says.**

1. **A single visitor never fills a queue.** Offered 10k, 25k, 50k and 100k
   datagrams/s, the visitor's own socket buffer throttles it: the achieved rate
   stops at ~29.8k/s (0.33 Gbit/s) and the counters stay at zero. This is the
   case D27 was never about, and this is why.
2. **Many visitors do, and the drop is exactly the excess.** 162,015 offered
   against 89,474 delivered is a gap of **72,541/s**; the counter read 580,730
   drops over 8.0 s, i.e. **72,591/s** — a 0.07 % closure. The server's counter
   accounts for the whole gap, so nothing was lost in the visitors' kernels
   first: the pool dropped precisely what it could not carry.
3. **The spread is even, in every configuration.** Round-robin delivers 10/10
   across two workers, 4/4/5/4 across four, 16/16/17/16 with 64 visitors. There
   is no imbalance for a "shortest queue" rule to correct: any assignment of the
   same aggregate load drops the same datagrams.
4. **The ceiling is per pool, not per worker.** `udp_workers` at 1, 2 and 4
   carries 1.14, 1.00 and 0.98 Gbit/s; 64 visitors instead of 16 changes nothing
   either. The limit is shared, which matches the design's own comment on the
   visitor-facing socket: one `recv_from` per datagram in a single task ("a
   single reader also means one slow worker can never stall other peers").
5. **The control rules out the instrument.** The same visitors blasting the sink
   directly absorb 2.06-2.11 Gbit/s — twice what the tunnel carries — so the
   ~1 Gbit/s is the forwarding path, not the sink.

**Verdict: falsified, and the release is unaffected.** M2c does not land, with
numbers instead of a shrug: no policy is justified by this, because the condition
the policy addresses does not occur. The counters that measure it (`queue_full`,
`no_worker`, and the per-worker `pinned` list) ship with 0.10.0, so a deployment
that overloads a UDP service can tell, and `docs/configuration.md` now states the
ceiling beside `udp_workers`.

**The follow-up the measurement produced** (not needed for this release, not
started): if that ~1 Gbit/s per-pool ceiling ever matters, the lever is the
reader, not the fan-out — `transport::udp_batch`'s `recvmsg` batching already
exists in the tree and is used by KCP's socket loop, while this reader takes one
datagram per syscall. That is a capacity question with its own A/B, filed rather
than guessed at.

### One probe pinned the whole harness (2026-09-30)

The fourth sweep of the day — the one the release artifact needs — came back
with every fast number a third of its usual size, and **the bug was mine**, in
that morning's "pin the loopback probe to one CPU" change:

| | 05:56 sweep (healthy) | 06:00-07:15 sweep (pinned) |
|---|---|---|
| clean bulk, molehill | 16.42-16.63 Gbit/s | **5.16-5.68** |
| clean bulk, rathole / frp | 12.91-13.01 / 6.03-6.05 | **5.54 / 2.65-2.70** |
| clean bulk, nps | 0.13 | 0.13 (unchanged) |
| clean interactive p99, molehill | 9.6-9.8 ms | **163-385 ms** |
| clean interactive p99, frp | 3.0-3.2 ms | **205-216 ms** |
| clean interactive p99, nps | 64.3-64.5 ms | 60.1-60.2 (unchanged) |
| shaped stages | — | normal (`rate100` read 0.0997) |
| CPU probe / loopback probe | 424.4 MiB/s / 22.4 Gbit/s | 410.5 / 22.17 |

`host_loopback` pinned itself with `os.sched_setaffinity(0, ...)` — which sets
the affinity of the **process**, is inherited by every child, and was never
restored. So after the probe ran, the harness, the four tools under test, their
iperf3 clients *and* their iperf3 servers, the pingers and the samplers all
shared one core. Everything the sweep reported afterwards was a measurement of
that one core; the gate refused it, correctly, 75 minutes later. The tells that
should have been read sooner, all of them in the table above: a tool that is
single-threaded by nature (nps) was untouched, the *shaper-bounded* stages were
untouched (a rate class does not need a second core), and both calibration
probes — each of which needs exactly one CPU — read normal, because nothing was
wrong with the machine.

The diagnosis that settled it was measurement outside the harness, and it is
worth keeping as a technique: the same binary, configured by hand, with an
`iperf3` backend and an echo service beside it, reads **16.60 Gbit/s** through
the tunnel and **0.58 ms median / 4.64 ms p99** while 20 streams saturate it
(idle: 0.07/0.10 ms) — against the harness's 5.2 Gbit/s and 163 ms at the same
moment. A hand-rolled client with no harness in it is what separates "the path is
slow" from "the thing measuring the path is slow".

Fixed by moving the pinning into a **child process** (`_loopback_child`, spawned
with `sys.executable`), which cannot leak it into the run, plus a self-check that
raises when the harness's own affinity changes across the probe — an invariant
that is silent everywhere else and cost an hour to find. Verified: affinity 20
CPUs before and after, probe still 21.80-22.05 Gbit/s, and a 30 s clean stage
back to **11.2 ms p99 / 16.70 Gbit/s** from 201-310 ms / 5.2.

The artifact on disk was never touched: the failed sweep's file and charts were
reverted rather than committed, so `pre-tag` stayed red and said the artifact
still described `c265af3`. Two other things came out of it and are kept: a clean
stage that breaks the SLO now ends the run instead of spending the shaped stages
first, and the "scheduler-latency probe" idea is *withdrawn* — it was measured
during this incident and read 10-18 us, i.e. it would not have caught this, and
a probe that cannot catch the incident that motivated it does not belong in the
comparability key.

### A failed start is no longer silent (2026-09-30)

The finding filed with the config-gap tests, fixed. `run` spawned the instance
and only ever observed its `Result` when the *next* general configuration change
arrived, so **every** failure before that point was swallowed: the process stayed
up, logged nothing, and served nothing. Reproduced with the control port already
held:

```
$ molehill server.toml      # port 24444 busy
INFO  molehill v0.10.0 (...)
INFO  Using config /tmp/bind.toml
INFO  Running as a server
INFO  config_watcher{...}: Start watching the config
(no further output; process alive; nothing listening)
```

(The two cases the test-writing pass first reported — a key that is not valid
base64, an `allow_ports` range with its start above its end — are *parser*
failures, which `main` already turned into a message and a non-zero exit; the
silent class is a failure inside `run_instance`, and a busy port is the one an
operator actually meets.)

The instance now reports its own end on a channel the watcher loop selects on,
and any error is the process's error: `Error: the instance stopped: Failed to
listen at \`server.control.bind_addr\`: Address already in use (os error 98)`,
exit code 1. The restart path waits for that same report, so a failure arriving
with a reload is a failure too. `tests/startup_failure_test.rs` pins it — and
falsifies: with the old `run` restored it fails with "still running after 30s
with nothing serving".

### The comparability key gets a path probe (2026-09-30)

The other finding from the 2026-09-30 sweep, fixed. Runs now record
`host_loopback` beside `host_calibration`: 512 MiB through one loopback socket
pair (median of five readings, one discarded warm-up, Gbit/s), measured with no
tool in the path, and the gate refuses a comparison when it differs by more than
15 % (`soak_check.HOST_LOOPBACK_TOLERANCE_PCT`). Two probes because they answer
two questions: the CPU probe certifies *state* and is blind to the ceiling the
fast cells ride, which is exactly what the two container instances moved by
25-39 % while the CPU probe read 1.7 % apart.

**The probe had to be pinned, and finding that out cost a sweep.** The first
formulation — one socket pair per reading, unpinned, temperature and load as
they came — read **28.14-28.67 Gbit/s across five invocations** in its first
validation, and that validation was run while `just check` was using the host.
On an idle machine, five separate processes then read 29.4-29.7 Gbit/s and two
read 34.2-34.4: **bimodal**, not noisy — which cores the sender and reader
threads land on decide which copy path they get. A key with a 17 % spread
cannot certify anything the clean cells do (two sweeps an hour apart read
16.3-16.8 Gbit/s on those cells with the CPU probe 414.0 against 424.4 MiB/s),
and the sweep that had already been run with the unpinned probe was discarded.

Pinned to one CPU (`sched_setaffinity`, recorded as `host_loopback.cpu`), with a
pre-touched reused buffer so page faults stay out of the timed region: ten runs
on an idle machine span **20.99-22.57 Gbit/s** (typically ~1.5 % apart, worst
7 %), and four busy loops elsewhere on the host move it to 20.4 — the
sensitivity the key exists for. The tolerance is 15 %
(`soak_check.HOST_LOOPBACK_TOLERANCE_PCT`): an order of magnitude above the
instrument's own noise, well under the 25-39 % it has to catch, and loose enough
that the cost of a false refusal (one run) never becomes the reason a release
comparison is skipped. The level also brackets the cells the way it should —
12.9-16.7 Gbit/s of tool throughput under a ~22 Gbit/s per-core ceiling, with
the unpinned 29-34 Gbit/s ceiling above both.

Two implementation notes, both from getting it wrong first: `recv_into` needs a
*writable* buffer (a `bytes` chunk raised inside the reader thread and left the
sender blocked on a full socket — the probe became a hang, which is why both
sockets now carry a 30 s timeout and the failure is recorded as the typed
`{"ok": false, "reason": ...}` the gate already handles), and a probe that
cannot run must never be read as agreement: `calibration_note` reports *which*
probe did not run, so "the CPU probe ran" is no longer printed as if the path
had been compared.

### Release sweep (2026-09-30)

`v0.9.0-115-gc265af3`, tree clean, fresh release binary, host `3f8b4508ab91` /
`host_id d764f9da9c7e5b2a`, calibration 414.0 MiB/s, `shape_legs=visitor`,
`rate_socket_window=256K`, four tools, 8/8 stages each plus the capacity ramp,
`--test=rrul,capacity`, ~76 minutes.

**`just soak-check`: `OK: no gate violation`.** What the new method changed in
the artifact: **`rate20` now carries a reading and every arm reads 0.020 Gbit/s**
(98 % of the rate the class applies, 4 % zero-byte intervals) where the previous
sweep had no reading at all, and `rate100` reads 0.100 from the sender. The
bounded window also removed a measurement artefact from the interactive side:
`rate100`'s p99 fell from 3195-3294 ms to 1485-1581 ms, because an unbounded
bulk sender had been filling the shaper's queue with a socket buffer's worth of
bytes. `jitter` still carries no reading (all four arms, 89-97 % zero-byte
intervals): its zeros are congestion collapse, not a buffered sender, and a
window is only meaningful where the path's rate is known.

Clean path: molehill 16.29-16.75 Gbit/s at 9.0-9.6 ms p99, rathole 12.81 at
100-101 ms, frp 6.03-6.08 at 3.0-3.1 ms, nps 0.13 at 58.6-64.8 ms; the run's own
replicates are 0.0-2.7 %. `loss1` separates the throughput pair from frp (9.70 /
9.68 against 5.69 Gbit/s). The load ramp: molehill, frp and rathole carry its
full 8 streams without breaking the SLO — the ramp's own ceiling, so a floor —
and nps breaks at the first one.

**A finding this sweep paid for: the container instance moved, and the
calibration did not see it.** The hostname changed again
(`2967a5748835` -> `3f8b4508ab91`) while `host_id` stayed `d764f9da9c7e5b2a` —
`host_identity` working as designed — and the two arms that reach the loopback
ceiling lost a quarter to a third of their clean throughput between the two
instances (molehill 21.78 -> 16.75, rathole 21.18 -> 12.81 Gbit/s) while frp
(6.06 -> 6.08) and nps (0.134 -> 0.134) were flat. The **CPU calibration read
421.2 against 414.0 MiB/s (1.7 %)** — well inside its own tolerance — so the
probe that exists to catch "these two runs did not see the same machine state"
would have waved this pair through. It certifies CPU state; the bench's fast
cells are bounded by the loopback path (memory, cache, the kernel's copy path,
whatever else that container instance shared), and nothing in the run measures
that independently of the tools under test. Filed as an open thread below, with
these numbers. The practical consequence for this release is nil — the artifact
was replaced and every cross-sweep comparison is refused on the method change
anyway — but the next cycle has to decide what the comparability key should
measure.

### The dead bulk spine: four defects, one retraction (2026-09-29)

The cycle's sweeps lost one bulk cell per run, always at a rate transition
(stages 6-7). It turned out to be four separate things, found in this order,
each fixed only after the previous one was measured out of the way.

1. **The drain was a silent no-op** (`78984ef`). `tc` renders a queue's backlog
   with a unit suffix (`b`, `Kb`, `Mb`, `Gb`) and `_backlog`'s pattern accepted
   only the bare `b`, so a backlog large enough to print as `Kb` — which is
   every backlog at a rate-shaped transition — was read as *no qdisc at all*,
   and `Shaper.settle` returned without waiting. A/B on
   `rate100:120,rate20:120`: **3/3 dead before, 3/3 carrying their spine
   after**. The KiB scale was checked against `tc -s -j` at the same instant
   (`28447Kb` read 29129713 bytes).

2. **The spine had one dial** (`78984ef`). A dial that landed on a busy path
   died and took the stage's whole bulk axis with it. The spine is now dialed at
   `SOAK_SPINE_RETRY_S` seconds into the stage (default `0,25,50,80`) and stops
   at the first dial that carries intervals; a dial that has carried *nothing*
   is abandoned at the next offset by a watchdog thread, because the read loop
   blocks and a stuck dial would otherwise hold the stage to its end (measured:
   `exit -9`). Gaps cannot be tightened below ~25 s: healthy first intervals
   were measured at up to 13.5 s, so ~4 dials is the safe maximum per 120 s
   stage. A recovered stage is visible, not silent — `spine_attempts` and
   `spine_first_interval_s` travel in the stage record.

3. **The budget was shorter than the flush it waits for** (`0dc1692`). With 1
   and 2 in place the reproducer still failed 2 of 3, always at `jitter`. A 30 s
   budget started that stage *inside* the flush window, and the retry schedule
   only reaches 30+80 s, so it could not escape. At 120 s the reproducer went
   **3/3 green** — `jitter` recovering on dial 2 at t+39.8, t+44.6, t+47.5 s.

4. **But that was still a timer, not a predicate** (`401aeda`) — the defect the
   whole exercise was really about. `backlog == 0` was **unsatisfiable**: the
   interactive, churn and UDP probes share the tool's class and leave ~1.2 KB
   queued permanently. Measured with a 400 s budget, the queue settled at
   ~1.2 KB at t≈159 s and sat there for the remaining 240 s without ever
   reaching zero. So the drain could only ever end by *expiring*: the fixed
   120 s was a constant tuned until the gate stopped failing, and whether the
   next stage's first dial survived depended on whether the clock happened to
   allow enough time. That is the difference between a benchmark whose
   variables are controlled and one whose constants were fitted to the result.
   The predicate is now decidable, and both halves are tolerances for a
   measured reason:

   - the queue half is one `lo` frame (64 KiB), which separates the probes'
     1.2 KB floor from the tens of MB a killed bulk client leaves by three
     orders of magnitude;
   - the socket half counts the states that can still *send* — `ESTAB`,
     `FIN-WAIT-1`, `CLOSE-WAIT`, `SYN-SENT`, `SYN-RECV`. `FIN-WAIT-1` is the
     dominant carrier of the tail (the killed client's kernel retransmitting
     what it holds, measured per socket), while `FIN-WAIT-2`/`CLOSING` linger
     for *minutes* carrying nothing, so "any non-LISTEN state" never ends and
     `established` alone reads **0** from ~t+20 s while megabytes are still
     moving.

   `SOAK_DRAIN_BUDGET` is now a safety net sized above the measured worst case
   (180 s against ~159 s), and both the tolerance and the state set travel in
   `meta`. On `rate20:120,jitter:120`: drain **168.1 s with `expired: False`**,
   and `jitter`'s **first** dial carries its intervals from t+5.2 s (it needed
   a second dial and started at t+44 s under the timer).

**Retraction: this is not a product defect.** An earlier pass of this record
claimed the tool was leaking a dead visitor's data for ~100 s, on the evidence
that a no-tool control arm (identical shaping, identical 20-stream iperf3
SIGKILLed at the boundary) cleared the same class in **9.6 s** where the harness
took ~106 s. That claim was wrong, and the per-socket byte counters are what
refuted it: the traffic is the **killed client's own kernel** still delivering
what it holds, into a tool that is applying correct TCP backpressure. The
harness shapes the tool's **backend** leg at the same 20 Mbit/s as its visitor
leg, so the tool is squeezed between two rate limits, its receive window stays
mostly closed, and the client's kernel accumulates tens of MB during the stage —
which it then spends ~159 s flushing after the kill. The no-tool arm had no
second rate limit, so nothing accumulated and nothing had to be flushed. The
shaping is the cause; the tool's behaviour is what a proxy should do.

**What was tried and rejected, so it is not re-litigated.** The plan's four
hypotheses were falsified by measurement: the backend leg is clean from t+35 s
at the failing transition (`backend: {'LISTEN': 1}`), the engine's 64-stream cap
never appears in a tool log, and the `established`-only socket half is not what
failed (it is defect 4). Extending the drain's port scope to `iperf_backend`
would have waited on nothing. **Lifting the shaper** during the drain — so the
residual flushes at line rate instead of 20 Mbit/s — was tried and reverted:
`tc qdisc replace`/`change` cannot clear a netem queue at all (netem gives each
packet its departure time at enqueue, so a 29 MB backlog kept draining at the
old 20 Mbit/s after `rate 10Gbit` was set; only `qdisc del` drops it), and the
delete-based version, while it cut the drain to 0.006 s and passed
`rate20 -> jitter` once, regressed `rate100 -> rate20` to `exit -9` — it drops
the queue but not the client's kernel buffers, so the flood simply arrives
later, into the next stage's shaper.

**The rate cells are still degenerate, and that is separate.** `rate20` and
`jitter` peaks are `0.000` for every arm including three unrelated peers: the
shaper holds each interval's bytes past the interval's own accounting window.
The spine behind them is now real, which is what the gate checks; the numbers
are still not comparable, and the README says so.

**The cost went down, not up.** Ending on the predicate turned out to be
*cheaper* than the fitted timer it replaced: the seven transitions cost 0.28,
4.1, 1.6, 9.0, 35.3, 71.0 and 76.0 s — **197 s per tool in total**, against the
840 s the fixed 120 s spent expiring at every one of them. Waiting for the path
to say it is ready is both the controlled thing and the fast thing; the timer
was paying for the transitions that did not need it in order to cover the one
that did.

### The instrument's claims: what the gate now refuses to say (2026-09-29)

The release sweep was green and every cell carried its spine, and the numbers
still could not support the claims being read off them. Three defects, all in
what the gate and the plot *say* rather than in what they measure. None needed
a re-run: the current artifact already carries every key involved.

1. **Comparability was one integer.** `soak_check.comparability` checked
   `workload_version` and `host_id` and nothing else. `workload_version` stayed
   `1` across this whole cycle while **five** method keys changed under it (the
   drain's introduction, the log-suffix fix, the spine retry, the drain budget,
   the drain predicate), so the gate would have called two different instruments
   comparable and printed verdicts from that comparison — the one failure mode
   that makes a benchmark worse than no benchmark, because it looks like
   evidence. It now compares the runs' method records (`METHOD_KEYS`) and
   refuses, naming every key that differs **and** every key a file does not
   record at all; an absent key is an instrument that file cannot describe, not
   a default to assume. Every blocking reason is reported, not just the first —
   the shipped baseline fails on host *and* on method, and naming only the host
   would tell a reader that clearing it makes the pair comparable, which would
   cost them a run to disprove.

2. **A degenerate cell was published as a number.** At or over half of a
   stage's bulk intervals reading zero bytes, the peak that remains is not a
   throughput measurement. `just soak-plot` printed it anyway, and the README's
   bulk figures were transcribed by hand from an ad-hoc script. There is now a
   per-stage bulk table in the plot — the first time those numbers have been
   machine-generated — and both it and the README print the zero share instead
   of a figure. **The rule immediately caught an error of mine**: the README
   claimed "not one zero-byte interval for molehill, frp or rathole across
   `rtt100`, `loss1` and `loss5`", carried over from the previous sweep without
   re-checking; in this sweep molehill's `loss5` is **82 of 109 intervals at
   zero**. The claim is gone and the cell reads `— (75% zero)`.

3. **A run did not state its own noise.** Every stage is one sample, so a
   single run seemed unable to say how repeatable it is — but the schedule
   measures `clean` at both ends of every timeline, so each run contains a
   replicate of one condition about an hour apart. `just soak-check` now
   reports that spread per tool, and it is the scale every between-tool
   difference has to clear: 21.730-23.795 Gbit/s for molehill (8.7 %),
   6.797-6.881 for frp (1.2 %), 23.334-23.754 for rathole (1.8 %),
   0.540-0.724 for nps (25.4 %). Reported, never judged: variance is data and a
   threshold on it would be invented.

**What 3 changed about the release's own claim.** Applied to this sweep it
corrects the phrasing the README carried: on the clean path **molehill and
rathole are indistinguishable in throughput** — their replicate ranges overlap
— so "rathole is marginally ahead" was never supported. What *is* supported,
because each difference clears the replicate by an order of magnitude: frp
carries 3.2x less bulk than molehill, molehill's clean latency is 10.4x lower
than rathole's and 2.1x higher than frp's, and nps is behind on both. The
gate's own `METHOD_KEYS` and the degenerate-cell rule were also folded into
`docs/benchmarks.md` ("Comparability") and `docs/release.md`.

**Still not fixed, and now stated rather than implied:** the shaped interactive
cells remain worst-observations from tens of samples whose spread between runs
of the same code exceeds the between-tool differences, so they are context and
the README says so; the release artifact still carries no capacity number (the
README delegates it to the reader's own path); and the rate-shaped cells stay
unmeasurable under this method rather than merely unreported.

### The rate cells get a bounded sender (2026-09-30)

The rate classes were the one place the model could not measure: the client's
writes completed into a socket buffer far larger than the shaped path could
drain, the measured intervals read zero bytes while the path kept carrying them,
and at `rate20` the client was still blocked 30 s past the stage boundary, so its
summary never arrived. The thread's follow-up was to bound the sender's window;
the A/B (one binary, one host, `SOAK_RATE_SOCKET_WINDOW` the only variable,
timeline `clean:45,rate100:60,rate20:90`, balanced off/on/on/off) says it works
and that the old reading was not merely unreadable but **wrong**:

| stage | window | zero-byte intervals | reading (Gbit/s) | of nominal |
|---|---|---|---|---|
| `rate100` | 256K | 0 %, 0 % | **0.1000, 0.0999** | 100 % |
| `rate100` | off | 1.8 %, 5.3 % | 0.1081, 0.0984 | 98-108 % |
| `rate20` | 256K | 12.8 %, 14.0 % | **0.0196, 0.0195** | 98 % |
| `rate20` | off | 78.8 %, 72.1 % | 0.0334, 0.0333 | **167 %** |
| `clean` (control) | either | 0 % | 17.00-17.07 | (unchanged) |

The `off` arm's `rate20` number is 70 % above the rate the class applies, because
the transfer outlived the stage: the sender was still delivering inside the next
stage's window. So the window is now the method's default, and the reading rule
follows the *measurement* rather than the class — the sender speaks unless its
`end` event or its zero-byte share says it cannot — which also restores the
stated convention (sender's bytes over the measured window) on the rate cells.
The window is applied to rate classes alone: it is meaningful only where the
path's rate is known, and on a delay-only or clean stage it would cap the
bandwidth-delay product and change the measurement it exists to serve, which is
why `jitter` still carries no reading (its zeros are congestion collapse, not a
buffered sender). `SOAK_RATE_SOCKET_WINDOW=off` reproduces a pre-window run, and
the value travels in the meta and in `METHOD_KEYS`.

### The pool-size and shared-pool questions, answered (2026-09-30)

Two `screen` runs, one binary, interleaved arms inside one epoch, `--path=clean`,
1..20 bulk streams, `--ab-variants`. The screen tool used to print one metric's
verdict per run (throughput whenever any step had it), which hid the first of
these completely; it now prints one table and one verdict **per metric**.

**`mux` (cap 4) against `mux8` (cap 8)** — the pool-size question the M2b thread
re-opened: throughput A 9/20, B 2/20, 9 inside the threshold (noise); interactive
p99 A 9/20, B 3/20, 8 inside — mixed overall, but **load-dependent and
unanimous where it matters**: from 15 streams up, cap 4 wins all six steps by
42-61 % (median p99 5.56 against 6.16 ms over the whole ramp). So growing the
pool larger does not help the mixed workload, and under real load it hurts.
Verdict: directional, no claim by the screen's own rule (the low-load steps
disagree), and the *shipped* default is the better arm.

**`mux` (per-service pool) against `shared` (one pool per session)** — the
observation that started this thread (49.979 ms against 7.3 ms, single samples
on different schedules): here, matched, **the shared pool loses on interactive
p99 in nineteen of twenty steps and never wins one** (median 8.15 against
5.00 ms; step 4 alone is 7.99 against 1.85 ms), while the two are
indistinguishable on throughput (A 3/20, B 11/20, 6 inside — pure noise). The
earlier observation was not a fluke; `[client.data].shared_pool = true` costs
interactive latency at every load level, and the shipped default
(`shared_pool = false`) is the right one for a latency-sensitive service.

### The stripe group's cold-pool cost, measured (2026-09-30)

The last unverified half of the B1 change, on a real run
(`MOLEHILL_STRIPE_COUNT=4 MOLEHILL_POOL_STATS=1`, mixed workload): every pool
grows `0->1`, `1->2`, `2->3`, `3->4` with `reason="stripe"`, all four events
inside **0.5 ms** of each other, and the whole run records `grows=4` — the
`K-1` extra dials are paid once, at the first striped visitor to a cold pool,
and the pool then stays warm for `idle_timeout`. The spread holds under load,
not just in a test: the closing snapshot is `size=4 … streams=12
tunnels=3/0/0,3/0/0,3/0/0,3/0/0` (three concurrent visitors, four stripes each,
three streams on every tunnel). A pool test with a counting dialer pins the same
count without the wire (`src/transport/multiplex.rs`).

### The stripe group command, landed (2026-09-29)

One command, and it is **v4's**: `ControlChannelCmd::
CreateDataChannelForStripe(service, group, index, count)`, a fixed 11-byte
session command (tag 4, the group id as four raw bytes so the tag alone still
decides the length). `CURRENT_PROTO_VERSION` and `SUPPORTED_PROTO_VERSIONS` are
unchanged: the release this work belongs to is not tagged yet, so its dialect is
still being defined and the command is part of it (AGENTS.md §5). The
request-to-command mapping stays in one pure function (`data_channel_cmd`), so
"only a group request names a group" is a unit test rather than a reading of two
call sites.

The client side is where the guarantee lives. `open_stream_on_distinct(used,
stripes)` grows the pool to `min(stripes, max_tunnels)` **first** — one dial at
a time through the pool's own resize flag, terminating because every iteration
either returns or leaves the pool bigger — and then reserves, per stripe, a
tunnel whose id the group does not already hold. The exclusion is a
*preference with a floor* (`order_candidates_for`): when every candidate is
taken, the ordinary least-loaded order comes back, so a pool that cannot spread
still forwards. The group's used-ids map is per service and keyed by group,
bounded at 64 entries with oldest-first eviction; an entry is dropped once
`count` stripes are placed.

**Evidence.** `just check` green end to end (`pre-commit checks passed`,
`pre-push checks passed`); `cargo test --lib` 157 passed; the whole serial suite
green (integration 20, pool 10, session 8, log-budget 2); `just interop` 3/3
against the cached v0.9.0 binary. The defect is visible on a real cold pool: the
client's `pool-stats` log during `striped_data_channels` reads
`reason="stripe" from=0 to=1 … 3 to=4` (it stayed at 1-2 before the change), and
the test now asserts the pool reached its four stripes. Two falsifications were
run and reverted, and both bit: sending the group request for a plain visitor
fails the vocabulary test at the point where the hand-written peer expects the
plain command, and replacing the client's stripe open with the ordinary
`open_stream()` fails `striped_data_channels` with
`the stripe group must have grown the client's pool to its 4 stripes: [2]`.

**Design delta from the thread's sketch, with the reason.** The command rides on
the *control* channel rather than in the stream prologue: that is where the fact
already is — the server knows the group when it *asks* for the channel, and the
client needs it before it *places* the open, which is when it reads the request
— and it leaves the prologue and the whole data plane untouched, so
`StartForwardStripedTcp` stays the only place a stripe index is assigned and an
out-of-order gather stays correct.

**Both gaps this record left open are closed (2026-09-30).** (1)
`striped_data_channels` now holds a transfer in flight and polls the client's
live pool until one pool carries a stream on **every** one of its four tunnels,
so the per-instant half of D24 is checked end to end and not only by a pool unit
test. (2) The eager growth's cost is measured: `K-1` extra dials, all four
growth events inside 0.5 ms of each other, paid once at the first striped
visitor to a cold pool (`grows=4` for a whole run), with the spread holding
under load (`streams=12` over `3/0/0,3/0/0,3/0/0,3/0/0`). What remains
unverified is unchanged and unworrying: a *previous* binary's striped data
plane cannot be exercised, because the only previous binary is v0.9.0 (v3,
refused).

### The pool-size question, re-opened (2026-09-29)

The M2b/M2c thread said "do not land on this data: re-open with a *pool-size*
question (does growing earlier help a mixed workload?)". Taken instrument-first,
with the instrumentation D22 already built and **no code change**:

```
SOAK_KEEP=1 MOLEHILL_POOL_STATS=1 MOLEHILL_PLACEMENT_STATS=1 \
  just soak --test=rrul --tools molehill --variants shared \
  --timeline clean:60,rate100:90,rate20:90      # 20 bulk + interactive + churn + UDP
```

The pool's own timeline (client log, `pool-stats`): `+cold:0->1`,
`+udp_floor:1->2`, `+load:2->3`, `+load:3->4`, then `growth refused, holding
off: already at max_tunnels (4)`. Final state: size 4, 23 streams spread
7/6/5/5, one peer pinned. Placement over the same run: 4-8 candidates per
placement, `mean_spread` 0-2 stream slots, 0 fallbacks, open latency mean
18-73 us (max 20.8 ms — the cold dial, once).

**What this settles:** "growing earlier" is not the lever. The pool grew twice
*during* the burst under its own load rule and finished at its cap with the
placement spread at 0-2 slots, so S1's conclusion ("placement policy is not the
axis") now holds under the current in-path growth rule rather than the one S1
measured — and what is left is not *when* the pool grows but *how large it may
grow*. S2/D28 stay unlanded, on evidence rather than on the old instrument.

**What it does not settle:** whether a bigger cap helps. That is a different
question with a different instrument: a `screen` A/B of
`[client.data.tcp].max_tunnels = 4` against `8` under the same mixed workload
(the variants list already has the `mux1`/`kcp4` pattern to copy). It would have
to come first if anyone wants to move that default.

One observation recorded without being a claim: the `shared` variant's clean
interactive p99 read 49.979 ms here (803 samples) against the default
per-service pool's 7.3 ms in the same day's sweep. Two single samples on
different timelines (60 s against 150 s) settle nothing — but the gap is the
size that would matter, and it is the reason to A/B `shared` against the
default at a matched schedule before trusting either number.

### Shaping scope, the rate cells, and the shaped-cell rule (2026-09-29)

The open threads the gate work exposed, worked in one session. Every number
below is a measurement from this host (`a093c5fbe0dc` renamed to
`2967a5748835`; `host_id d764f9da9c7e5b2a`), on one binary, with only the knob
under test changed.

**1. The shaping scope: `visitor` vs `both` (A/B, adopted).**
`SOAK_SHAPE_LEGS` now selects which legs a stage class is applied to, and the
default is `visitor` — the visitor's access link plus (for the KCP carrier) the
tunnel's own UDP port. The old model shaped the tool's backend leg as well, and
one HTB class then served both legs.

```
# balanced in time: V B B V V; one binary, one method, only the knob differs
just soak --test=rrul --tools molehill --timeline rtt100:90,loss5:90,rate100:90,rate20:90
  (SOAK_SHAPE_LEGS=visitor|both, --out ~/tmp/shaped/ab{1..5}-*.json)
```

| legs | stage | p99 (ms), per run | bulk reading (Gbit/s) | interactive floor (ms) | transition (s) |
|---|---|---|---|---|---|
| `both` | rtt100 | 7430, 7439 | 2.408, 2.386 | 801, 802 | — |
| `both` | loss5 | 4338, 4998 | 2.667, 2.651 | 801, 802 | 3.6, 4.6 |
| `both` | rate100 | 8990, 8652 | **no reading** (78-85 % zero intervals) | 162 | 2.8, 3.1 |
| `both` | rate20 | 1985, 3826 | **no reading** (100 %) | 283, 322 | 25.0, 27.8 |
| `visitor` | rtt100 | 8198, 6753, 6234 | 5.211, 5.203, 5.205 | 401, 401, 401 | — |
| `visitor` | loss5 | 4248, 5215, 4110 | 5.224, 5.224, 5.284 | 401, 401, 401 | 0.5, 4.6, 2.8 |
| `visitor` | rate100 | 3094, 3157, 2987 | 0.0939, 0.0910, 0.0919 | 82, 82, 82 | 0.5, 2.3, 1.8 |
| `visitor` | rate20 | 7411, 8340, 7591 | **no reading** (95-100 %) | 82*, 163, 102 | 0.5, 0.5, 7.2 |

`*` a stale sample from the previous stage's class; the true rate20 floor is
163 ms (40 ms x 2 traversals x handshake+request).

What the A/B settled, none of it assumed:
- **The delay is now paid once.** A fresh connection per ping pays the one-way
  delay twice (handshake, request), so `rtt100` floors at 401 ms instead of the
  802 ms the two-leg class produced, and `rate100` at 82 ms instead of 162. The
  stage table's "100 ms delay" now describes the path it produces.
- **A rate class carries its nominal rate.** `both` shared one 100 Mbit class
  between two legs, so every arm read ~42 % of nominal end to end; `visitor`
  reads 0.0910-0.0996 (91-100 %), the spread being the shaper's own burstiness.
- **Transitions collapse.** The worst transition on this timeline is 7.2 s
  (`visitor`) against 25.0-27.8 s (`both`) at the `rate20` boundary — the tool
  no longer backpressures against a shaped backend leg.
- **The two-leg model was also *hiding* queueing.** At `rate20` its interactive
  p99 read 2.0-3.8 s against `visitor`'s 7.4-8.3 s: with both legs shaped, the
  end-to-end rate was halved, so the 20 Mbit class was never saturated and the
  bufferbloat a saturated 20 Mbit access link really produces never appeared.
  The `visitor` numbers are the honest ones for the path the docs describe.
- **Attenuation, stated as a limit:** the two-leg numbers above are *not*
  comparable with any other sweep, and `shape_legs` is in `METHOD_KEYS`, so the
  gate refuses that comparison rather than printing one. Every stored result
  before this date carries `both` (or, before the key existed, no scope at all).

**2. The receiver's window at a rate cell, and a defect in reading it.**
`_spine_once` now keeps reading past the stage boundary (bounded by
`SPINE_SUMMARY_GRACE_S`) for the client's `end` event, which is the only place
the receiver's own window is reported. The first version tested `proc.poll()`
in that loop, which **skips the output of a client that exits exactly at the
boundary** — the very case `-t` is sized for. Measured: the same `rate100` cell
read `0.0995` (receiver) in one run and `0.0902` (sender) in the next, i.e. one
cell, two instruments, ~10 % apart. With the poll guard removed and the reading
chosen by the *class* (a rate shaper defeats the sender's accounting by
construction, not by this run's zero-share), three runs of
`rate100:60,rate20:90` read **0.0995, 0.0996, 0.0995** from the receiver's own
window.

`rate20` still carries no reading, and that is now recorded rather than
papered over: `SOAK_SPINE_SUMMARY_GRACE_S=5,15,30` all end `truncated` — the
client's kernel is still delivering the stage's bytes 30 s past the boundary,
so its summary never arrives inside any grace a stage can afford. The cell
prints `— (the rate class defeats the sender's interval accounting (95-100 %
zero-byte intervals) and the dial produced no receiver summary (truncated))`.
**Follow-up, not yet measured:** a bounded socket window (`iperf3 -w`) on the
rate classes would keep the sender's writes tracking the path, which should
give both sides a live window; it changes an instrument parameter, so it needs
its own A/B before it becomes the method.

**3. The shaped-cell rule (A4/A6).** Three runs of one unchanged method give
the per-class repeatability a single run cannot state:

| class | p99 spread (3 runs) | bulk-reading spread | gate limit |
|---|---|---|---|
| `rtt100` | 24.0 % | 0.2 % | 25 % |
| `loss5` | 21.2 % | 1.1 % | 25 % |
| `rate100` | 5.4 % | 3.1 % | 25 % |
| `rate20` | 11.1 % | — | 25 % |
| (`both` arm, 2 runs) `rate20` | 48.1 % | — | 25 % |

The p99 spread sits *at* the limit the gate applies to a per-stage difference,
and the older reading's spreads (74-86 % on the peak-interval metric) were
larger still, so a difference verdict on a shaped stage is a verdict on the
harness's own queue. The gate now reports a shaped stage's number as context
and fails only a blow-up (3x); the README marks those columns and picks no
winner in them; the plot prefixes them with `~`. The spread values above live
in this record, not in the code: a table of per-class thresholds baked into the
gate would go stale with the next method change.

**4. The host key is now a measurement too (A3).** `meta.host_calibration`
records a fixed CPU-bound workload (`sha256_fixed_buffer`: SHA-256 over a
192 MiB buffer, median of three, MiB/s) taken before every run. The probe was
chosen by measurement: a 128 MiB loopback socket pair drifts 18.7 % across
median-of-five readings on this host (it follows the CPU's power state), while
the SHA-256 probe repeats to 1.0-2.2 % (415-424 MiB/s). `soak_check` refuses a
comparison whose calibrations are more than 25 % apart and *reports* a file
that predates the probe as unverifiable rather than reading its silence as
agreement — the hole this closes is that on a host with no `/etc/machine-id`
the identity key reduces to `cpu_model | nproc`, so two machines can name the
same host.

**5. The load axis is now in the release artifact (A5).** The release sweep is
one command, `--test=rrul,capacity`, and `--test` takes a comma list: the
staged schedule and the load ramp travel in one `results-soak-vX.Y.Z.json`, so
they share one `meta`, one host and one revision. The alternative the thread
weighed — a second artifact with a name of its own — was rejected after
checking what it would cost: the plot and the gate already render and compare
*every test entry in one file*, so a second file would have meant a second
naming scheme, a second resolution rule in three tools and a second pairing in
the ritual, for no gain. The two curves are still declared **two different
instruments** (docs/benchmarks.md) and are never cross-checked; the gate keys
each comparison by (tool, test type), so a capacity entry can only be compared
with a capacity entry.

### The freeze found a gate that would have shipped empty release notes (2026-09-28)

Checking the freeze preconditions turned up a duplicated, **empty**
`## [0.10.0] - 2026-09-26` section sitting in front of the real one, and that
combination defeated every check at once:

- all three extractors (`githooks/pre-tag`, `release.yml`'s verification step
  and `release.yml`'s extraction step) take the **first** match, so the
  published notes would have been a bare `### Changed`;
- pre-tag's "non-empty" check counted *lines between headings*, and the empty
  duplicate still had its `### Changed`, so it passed;
- and nothing checked that `[Unreleased]` was empty, so this cycle's four fixes
  would have been left out of the notes entirely while the review stayed green.

The duplicate was an artifact of a changelog-editing script used earlier in the
same session (the last bullet's scan ran past the bullet into the following
heading); it is removed, and the notes extraction is verified against the real
section. The gate now requires **exactly one** dated section for the version,
**prose** in it, and an empty `[Unreleased]` — implemented in `githooks/pre-tag`
and in both `release.yml` sites, with the extractors anchored on the *dated*
heading, and exercised against five synthetic changelogs (good, duplicate,
headings-only, unreleased-not-moved, undated) plus this tree.

The transferable lesson, worth the line: a check that counts lines is not a
check that reads content, and "the release notes come from CHANGELOG.md" is
only true when the *right* section is the one selected. A duplicate heading is
not a cosmetic problem when every consumer resolves it by first match.

**Non-code items to review before/after the tag** (found 2026-09-26):
`main` has no branch protection (a repo-settings change for a human; the
`full check chain` check and the no-force-push rule are convention today); the
repo has **no topics** and an empty homepage; issues are disabled while
`AGENTS.md`'s waiver rule mentions "a linked issue"; Dependabot *security*
updates are disabled while version updates are configured; and
`delete_branch_on_merge` is off, so the merged branch is deleted by hand. The
withdrawn v0.9.1 GHCR leftover is **already gone** — the package list shows no
v0.9.1 version and `:latest` is back on the v0.9.0 digest (verified 2026-09-26),
so the earlier note asking for `delete:packages` describes a state that no
longer exists.

## Incident: the withdrawn v0.9.1 tag

A `v0.9.1` was tagged and published with only M0/M3/M4/M5, on the reasoning that
the rest could wait. **It was withdrawn**: the GitHub Release and both tags
(local and remote) were deleted, the GHCR image was removed, and the branch went
back into development.

**Correction (2026-09-27, found by the release review): crates.io *did* receive
the version — and it is now yanked.** `molehill-rathole 0.9.1` was published
(2026-09-26, 14 downloads): the tag push ran `release.yml`, which publishes to
crates.io, and a crates.io version cannot be deleted, only yanked. This
paragraph claimed the opposite ("crates.io never received the version") and was
wrong; the withdrawal covered every surface except the one that cannot be
un-published. `cargo install molehill-rathole` therefore resolved to the
withdrawn build.

**Fixed the same day** (a human action on crates.io, taken by the owner), and
verified rather than assumed:

```
crates.io API  : 0.9.1  yanked=True
sparse index   : yanked versions: 0.9.1 | installable max: 0.9.0
cargo install  : Installed package `molehill-rathole v0.9.0`
```

Yank is the right primitive, not a compromise: it removes the version from
resolution (so `cargo install` and `cargo add` cannot pick it) while a lockfile
or an explicit `=0.9.1` pin still resolves, so nobody's build breaks, and it is
reversible if the decision is ever revisited. The mistake was not judging M1 large; it was
turning that judgement into a *release* without asking the person whose plan it
is — a release is a deliberate act (AGENTS.md §5), and "continue the plan, then
tag" is not a licence to redefine what the plan contains. The GHCR images were
removed as part of the withdrawal (see the note above); the only trace left is
this paragraph and the deleted-file history.

## Release review — the state a reviewer should check

The review below was last refreshed on `d8233c4` (`just check`, `just interop`
and the CI query were re-run there); the open `[ ]` items need a human.

- **PR**: #4, 64 commits (81 files, +14658/−3038), `mergeable=clean`, CI
  **12/12 green** at `d8233c4` (four platform builds, three feature-leg test
  jobs, full check chain, powerset, docs alignment, musl static, minimal build
  size) — queried through the API, not assumed.
- **Gates**: `just check` green on `d8233c4` (147 lib / 20 integration / 10
  pool / 7 session / 2 log-budget; 3 ignored interop cases); `just interop` 3/3
  against the released v0.9.0 binary (the new-server/old-client case is a
  refusal case now — v4 only); `just soak-check` `OK: no gate violation`; `just
  tag-check` "pre-tag review passed for v0.10.0".
- **Benchmarks**: `results-soak-v0.10.0.json` + four charts are in the release
  commit, the README pair carries the same four-tool table, and the withdrawn
  v0.9.1 file and charts are deleted. **The provenance caveat is closed:** the
  shipped sweep is `v0.9.0-101-g401aeda` with `tree_clean: true`, i.e. a fresh
  binary on the release commit — which is what the ritual asks for, and what the
  frozen-commit sweep (`ca4ab4a`) could not claim. The gate verdict on it is
  `OK: no gate violation`, with no waiver: 32 of 32 tool-stages carry their
  spine, every one of them on its first dial. The record is the "Release sweep (2026-09-29)" subsection above.
- **CHANGELOG**: the `[0.10.0]` section was audited against the cycle's commits
  and five user-visible fixes were added (`899eb6f`); it is dated
  `2026-09-28`, one dated section, `[Unreleased]` empty.
- **Container**: scratch from static musl, `bin/<arch>` for amd64 and arm64 from
  the same feature set (`server,client,noise,hot-reload,multiplex,kcp`), `USER
  1000:1000`, `--help` smoke test plus `imagetools inspect` in the workflow.
  (The image itself was not rebuilt here; the workflow does that.)
- **Docs defaults** checked against their constants: `max_tunnels` 4,
  `udp_workers` 2, `idle_timeout` 60, `max_tunnels_per_client` 0 (unlimited),
  `shared_pool` false.
- **Docs gap found 2026-09-28 (fixed in the same commit as this line):** the
  removal callouts for `health_check` in `docs/configuration.md` and
  `docs/configuration.zh.md` still promised the "warn for one release, error
  from the next" path that v0.10.0 replaced with a hard refusal — the same
  contradiction the freeze fixed in the changelog, missed in the user pages.
- [x] **`v0.9.1` on crates.io is yanked** (2026-09-27, by the owner; verified
  through the API, the sparse index and a real `cargo install`, see the incident
  section). The verification installed `molehill-rathole 0.9.0` into
  `~/.cargo/bin/molehill` to prove the resolution changed — it was uninstalled
  afterwards, because a stale binary earlier on `PATH` than the workspace one is
  exactly the provenance trap §10 exists for (the harness's own rebuild check
  caught that same class of mistake during this cycle's sweep).
- [x] **The `[0.10.0]` changelog date is `2026-09-28`** — the day the freeze was
  prepared and the same day as this sweep; re-date it in the release commit if
  the tag lands on a later day.
- Observation, not a blocker: `release.yml` runs `cargo publish --allow-dirty`.
  On a fresh checkout there is nothing dirty to allow, so it only matters if a
  build step ever starts modifying the tree; dropping the flag would make that
  impossible rather than permitted.

### Pre-tag gaps found by the release audit (2026-09-28) — all closed

The audit that produced "Release review" above was run again on `d8233c4` and
turned up four gaps. None of them was a defect in the product; three were in
the harness or the docs, and all four are fixed on the branch.

1. **A config-docs contradiction — `539f4f5`.** The `health_check` removal
   callouts in `docs/configuration.md` and `docs/configuration.zh.md` still
   described the "starts and warns for one release, errors from the next" path,
   while `reject_removed_keys` refuses the key outright and the changelog
   already said so. Two user pages carried an instruction that would not have
   survived contact with the binary; the migration table on the same pages was
   right. Both languages corrected together.
2. **Bench provenance, one commit weaker than the ritual asks — closed by the
   re-sweep.** The frozen-commit sweep's `meta.revision` is `ca4ab4a` and its
   binary sha256 is `0ab4072667c30498`; the release commit is two commits later
   and touches `src/common.rs` (a `cfg` gate) and `src/protocol.rs` (a doc
   comment). Rebuilding the default feature set on HEAD gives a 4155768-byte
   binary identical except for the embedded `git describe` string, so those
   numbers did describe HEAD's behaviour — but `molehill_bin_fingerprint.stale`
   only proves the binary was not older than its own tree, not that it was
   HEAD's. The ritual's answer is a fresh sweep on the release commit, and that
   is what the current results file is (see the sweep record for this date).
3. **The completeness gate did not look at the bulk spine per stage —
   `5971cee`.** `check_completeness` asked only that `coverage.tcp_bulk` be
   true *somewhere* in the run; molehill's `jitter` stage carried **no** bulk
   intervals at all (the client could not dial the exposed port) and the run
   still reported "complete (95660 samples, 8 stage(s))". The hole was
   disclosed in the README (`†`) and in the sweep record, so nothing was
   hidden — but §10's "the completeness of every test's series" was not what
   was implemented. The gate now counts each stage's intervals inside its own
   window against a floor of one per 30 s, and a run that carries such a hole
   fails it. That is what caught this cycle's dead cells rather than letting
   them ship: the sweep before the fix failed on molehill's `rate20`, and the
   sweep now shipped passes all 32 tool-stages.
   `docs/release.md`, `CHANGELOG.md` and
   `docs/benchmarks.md` describe the new verdict.
4. **The provenance exclusion had never worked — `9d8b85a`.** The run
   excludes its own results file from the clean-tree verdict, so that writing
   the artifact does not mark the run dirty. The `:(exclude)` pathspec was
   passed repo-relative while `git status` runs with `cwd=benches/scripts/soak`,
   so it matched nothing: measured by dirtying a tracked results file and
   calling the function, `tree_clean` came back `False` for both the relative
   and the absolute spelling. Every artifact written that way recorded
   `tree_clean: false` — which is why this is worth a line: the field silently
   lost its meaning instead of failing. Fixed by relating the path to the
   function's own cwd; an output path *outside* the repository cannot be
   excluded at all (git exits 128 with "outside repository"), so the exclusion
   is skipped there and the plain verdict answers instead. Verified on a clean
   tree in all four cases: in-tree output clean, outside output clean, the
   run's own dirty results file clean, an unrelated untracked file dirty.

Two further harness defects were found and fixed in the same pass, both of them
things the frozen sweep had already recorded as suspicions: the drain predicate
and the `soak_check` screen table's quadratic state count (`5971cee`).

**The drain predicate has its own table now** (`b32a5fb`). The 2026-09-28
release sweep failed the new per-stage gate on molehill's `rate20` stage — 0
intervals, `control socket has closed unexpectedly` — a stage the frozen-commit
sweep had measured fine, and the only difference was this session's drain
change. Three variants, all measured on the `rate100:120,rate20:120`
transition:

| drain predicate | measured outcome |
|---|---|
| exactly-empty queue + no `ESTAB` on the throughput port (frozen sweep's) | `rate20` 91 intervals then, 58 in the probe now |
| backlog < one frame + teardown states on the throughput port | `rate20` **0** intervals, reproduced twice |
| no `ESTAB` on all exposed ports | never re-measured to completion: the probe-heavy echo port makes it nearly unsatisfiable (30 s burned per transition) |

The middle variant is the one that reads best and fails: exiting the drain
*before* the old connection is gone is worse than waiting too long, because the
next stage's dial then races the previous stage's FIN and the loss is silent —
the stage simply carries no intervals. The predicate is back to the frozen
sweep's, the backlog tolerance went with it, and `_busy_sockets` carries the
table so it is not re-derived by guess.

**Retracted on 2026-09-29.** That table's comparison is not usable as evidence,
because both variants were measured while `_backlog` could not read a `Kb`
backlog at all (see "The dead bulk spine", defect 1): at exactly the transitions
the table is about, the drain returned before evaluating *either* half of its
predicate. The variants therefore differed only in whether the suffix bug
happened to trigger on that run, not in the predicate they claim to compare.
The conclusion drawn from it ("the middle variant fails") is withdrawn; what
replaced it is defect 3, the budget. The table stays as a record of what was
run — it is not a comparison to build on.

**Verified before spending the release run** (2026-09-28, evening): a two-stage
`rate20:120,jitter:120` probe on `molehill,frp` — the exact transition whose
spine was dead in the frozen sweep — now carries **99** intervals in molehill's
`jitter` stage (peak 0.157 Gbit/s) and 103 for frp, the drains return in 10.5 s
and 17.2 s instead of the 30 s budget, and the new gate passes the probe. That
probe is how the run below was de-risked rather than hoped for. (Those drain
durations belong to the *pre-fix* harness: at the time a large backlog made
`settle` return early. On the fixed harness the same transition spends the full
budget, as the 2026-09-29 record explains.)

## Closed threads of the v0.10.0 cycle

Every thread below was opened during the cycle and closed before the
release. They are kept because each one carries the evidence that closed
it — the measurement, the commit, or the falsified premise — which is what
keeps a closed question from being re-litigated. The threads still open at
the freeze are on the front page.

- ~~**The stripe group command**~~ — **landed** (`CreateDataChannelForStripe`,
  part of v4, whose release is still in development, so the dialect is still
  being defined — AGENTS.md §5). One design
  detail came out differently from this thread's sketch: the command rides on
  the **control** channel, not in the stream prologue. That is where the fact
  already is — the server knows the group when it *asks* for the channel, and
  the client needs it before it *places* the open, which is when it reads the
  request — and it leaves the prologue and the whole data plane untouched, so
  the striped command the server still writes on each gathered channel
  (`StartForwardStripedTcp`) stays the only place an index is assigned: arrival
  order can still decide which stripe is which, which is what makes an
  out-of-order gather correct.
- ~~**M2b/M2c (S2, D28, D27)**~~ — **the placement half stays unlanded, now on
  current evidence.** Re-opened as the *pool-size* question the thread asked
  for and measured instrument-first (below, "The pool-size question,
  re-opened"): under the mixed workload the pool grows twice during the burst
  and finishes at its cap with a placement spread of 0-2 slots, so a smarter or
  earlier growth rule has nothing to win. The *cap* question the thread left
  open — `max_tunnels = 4` against `8` at a matched schedule — is **measured
  and answered no**: from 15 streams up the shipped cap wins all six steps on
  interactive p99 by 42-61 %, and it never loses the whole ramp (below, "The
  pool-size and shared-pool questions, answered"). Nothing here lands: neither
  a placement rule nor a larger cap. **D27's UDP half is now measured and its
  premise is falsified** (below, "D27's evidence, measured"): the drop counters
  fire only under aggregate overload, the visitors are spread evenly, and
  `udp_workers` does not move the ceiling — so there is no imbalance for an
  assignment rule to fix. What the measurement did produce is a documented
  per-pool ceiling and a named lever if it ever matters (a batched reader).
- ~~**The v3 server path**~~ — **removed.** v0.10.0 is the first release that
  serves v4 only: the v3 handshake, its one-service-per-connection control
  path, the two-key registry (`MultiMap`) and `pool_size` on the wire are
  gone, and the removed-config keys are refused instead of warned about. The
  interop matrix's new-server/old-client case now pins the refusal, and
  `a_v3_hello_is_refused_on_its_own_connection` pins it on this tree.
- ~~**The drain's socket half is not provably sufficient.**~~ — **fixed**
  (`401aeda`). It now counts the states that can still send (`ESTAB`,
  `FIN-WAIT-1`, `CLOSE-WAIT`, `SYN-SENT`, `SYN-RECV`) rather than
  `established` alone, which read 0 from ~t+20 s while the killed client's
  `FIN-WAIT-1` sockets were still retransmitting megabytes. The teardown states
  are deliberately excluded and that is measured, not stylistic:
  `FIN-WAIT-2`/`CLOSING` persist for minutes after a kill and carry nothing, so
  including them makes the predicate unsatisfiable.
- ~~**The drain costs real wall time, and the queue half is a fitted
  constant.**~~ — **fixed** (`401aeda`). The queue half is now a tolerance (one
  `lo` frame, 64 KiB) instead of an unreachable zero, so the drain ends on the
  path's state rather than on the clock; `SOAK_DRAIN_BUDGET` is a safety net
  sized above the measured worst case. It is also **cheaper**: the transitions
  in the shipped sweep total 197 s per tool against the 840 s the fitted timer
  spent expiring on all seven.
- ~~**The rate cells still carry no comparison.**~~ — **closed, both halves.**
  The reading is no longer the peak interval (a property of the shaper's
  schedule) but the load over the stage's whole measured window, and a rate
  class is read from the **receiver's own window**: `rate100` now reads
  **0.100 Gbit/s on all four arms** — the shaper's own number, which is what a
  rate cell *is*, and the docs say so instead of implying a tool comparison.
  `rate20` and `jitter` carry **no reading** and say why (the sender's
  accounting is defeated, 90-100 % zero-byte intervals, and the dial never
  produces a receiver summary because the client is still blocked past the
  boundary) — a hole with a reason, not a defeated number. **The follow-up is
  done**: a bounded socket window on the rate classes gives them a live sender
  (zero-byte share 72-79 % -> 13-14 %), `rate20` reads 0.0196 (98 % of nominal)
  where the unbounded client read 0.0334 — 70 % *above* nominal — and the
  reading rule now follows the measurement rather than the class. It is the
  default; `SOAK_RATE_SOCKET_WINDOW=off` reproduces a pre-window run (below,
  "The rate cells get a bounded sender").
- ~~**The transition is long because the harness shapes both legs at once.**~~
  — **fixed, and the fix is now the default** (`SOAK_SHAPE_LEGS=visitor`). The
  A/B in "Shaping scope, the rate cells, and the shaped-cell rule" measured it
  on one binary: the worst transition of the shipped timeline drops from
  25.0-27.8 s to 7.2 s, the injected delay is paid once (a `rtt100` floor of
  401 ms instead of 802), and a rate class carries its nominal rate. `both`
  stays selectable for reproducing runs measured under it, and `shape_legs` is
  in `METHOD_KEYS`, so the gate refuses a comparison across the change.
- ~~**The host key is stable, but it is a *name*, not a calibration.**~~ —
  **closed.** The identity half was already fixed by `host_identity()`
  (`machine_id | cpu_model | nproc`, with `hostname` kept only for a reader);
  the measurement the thread asked for is now implemented as
  `host_calibration` — SHA-256 over a fixed 192 MiB buffer, median of three,
  1.0-2.2 % repeatable on this host, recorded before every run — and
  `soak_check` refuses a comparison whose two runs measured more than 25 %
  apart, while *reporting* a file that predates the probe as unverifiable
  rather than reading its silence as agreement. The probe was chosen by
  measurement: a 128 MiB loopback socket pair drifts 18.7 % across median-of-
  five readings (it follows the CPU's power state) and was rejected.
- ~~**One sample per stage, and the shaped cells are the ones that pay.**~~ —
  **closed, and re-measured on the current method.** The one-sample limit is
  permanent (a stage is one walk of the schedule), so the resolution has to come
  from repetitions *outside* the run: three runs of one unchanged method under
  the visitor-leg scope give a shaped p99 spread of 5.4-24.0 % and a bulk-reading
  spread of 0.2-3.1 %, against the 25 % limit the gate applies per stage. The
  figure this thread used to quote (74-86 %) belonged to the retired
  peak-interval reading under the two-leg scope; the two-leg scope's own p99
  spread measured here is up to 48 %. Every surface now states the resolution
  and refuses to conclude inside it (see "Shaping scope, the rate cells, and the
  shaped-cell rule", item 3). The files are scratch and uncommitted, so the
  command in that record is the source, not a path.
- ~~**The load axis is absent from the release artifact.**~~ — **closed.** The
  ritual runs `--test=rrul,capacity`, and `--test` takes a comma list, so the
  staged schedule and the load ramp travel in **one** artifact (one `meta`, one
  host, one revision). The second-artifact plumbing the thread priced was
  avoided after checking what it would cost: the plot and the gate already
  render and compare every test entry in a file, so a second file would have
  meant a second naming scheme and a second pairing in three tools for no gain.
  The two curves are declared two different instruments and never cross-checked;
  the gate keys each comparison by (tool, test type). See
  "Shaping scope, the rate cells, and the shaped-cell rule", item 5.
- ~~**The shaped interactive cells are published without a rule.**~~ —
  **closed, with the rule the thread named.** Three runs of one unchanged
  method measured the interval (shaped p99 cells move 5-24 %, bulk cells
  0.2-3 %; the two-leg scope's p99 by up to 48 %), the numbers are recorded in
  "Shaping scope, the rate cells, and the shaped-cell rule", and every surface
  now applies one rule: the gate *reports* a shaped stage's p99 and fails only
  a blow-up (3x), the plot prefixes those cells with `~`, the README marks the
  columns as context and picks no winner in them, and both pages state the
  measured resolution beside the tables. The per-class thresholds were
  deliberately **not** baked into the gate: a table of them would go stale with
  the next method change, and the measurement's home is the record.
- ~~**A third probe: scheduler latency**~~ — **withdrawn, on measurement.** Filed
  while the 06:00 sweep was being diagnosed as "the host is degrading", then
  measured during that same incident: a two-process ping-pong over a socketpair
  read 10-18 us median there, against 10.8 us after the fix — normal in both,
  so it would not have caught anything. The incident turned out to be a
  self-inflicted CPU pin (below, "One probe pinned the whole harness"), and what
  caught it was a hand-rolled measurement outside the harness plus an invariant
  check, not another probe. A probe that cannot catch the incident that
  motivated it does not go into the comparability key.
- ~~**The host calibration certifies CPU state, not the loopback path**~~ —
  **fixed** (2026-09-30), by taking the first option: a loopback-ceiling probe
  measured outside every tool, pinned to one CPU, at 20.99-22.57 Gbit/s across
  ten runs (~1.5 % apart, against the 25-39 % it has to catch), gating a
  comparison at 15 %
  (below, "The comparability key gets a path probe"). The other two options are
  recorded there and rejected: gating on a run's own `clean` reading is
  circular for a cross-tool claim, and dropping the calibration's claim would
  have left the gate unable to refuse the pair that started this.
- ~~**The config-test gaps**~~ — **closed** (2026-09-30). `allow_ports`
  rejection and per-service `token` were already covered; the rest now are too:
  `udp_buffer_size` (which **truncates**, it does not drop — the docs were
  wrong and are fixed), `udp_idle_timeout` (the source port really does change
  after the timeout), `udp_send_queue_size` (pinned at the queue; no
  deterministic end-to-end version exists), a PSK handshake (matching, wrong
  and missing), hot reload add/modify/delete (the deleted service's port is
  provably released, the untouched one keeps answering, one session
  throughout), and `--genkey` (default and x25519 keys, a real handshake driven
  by a generated pair, and x448 refused with a message that names the curve and
  the resolver instead of `GetDhImpl`).
- ~~**`MOLEHILL_TCP_BUFFER_BYTES`**~~ — **closed, and the note was stale.** The
  switch never existed; its doc-comment reference went with `d405360`, and the
  `diag_env` illustration the note still pointed at is gone too (the list there
  is `MUX_STATS`/`KCP_STATS`/`POOL_STATS`/`PLACEMENT_STATS`/`UDP_STATS`/
  `STRIPE_COUNT`). Nothing survives anywhere in the tree.
- ~~**An instance that fails to start is silent**~~ — **fixed** (2026-09-30).
  The instance reports its own end on a channel the watcher loop selects on, and
  any error ends the process with the cause and exit code 1; the busy-port
  reproduction and the falsification are in "A failed start is no longer silent"
  below, and `tests/startup_failure_test.rs` pins it. The restart path reports a
  failure arriving with a reload the same way.

## The L3 cycle's investigation records (2026-10-10 .. 2026-10-13, moved 2026-10-16)

Moved from [HANDOFF.md](HANDOFF.md) when the release was prepared: every
entry here is a dated record that settled something — what was measured,
what was refuted, and what was built and reverted. None of it is open work,
and the rules at the top of this page apply to all of it (no number here may
be quoted as a measurement of the current code).


- **The mux write boundary's owned-write chain was built and REVERTED — the
  clean cells cannot resolve it (2026-10-12, round 37).** The full chain
  existed and compiled: `KcpTunnelStream::Plain`/`ClientStream::Kcp` taking
  owned writes (`TAKES_OWNED`), the mux frame writer's body stages in `Bytes`
  and handing `buffer.slice(offset..)` over the `AsyncWriteOwned` boundary —
  one full-payload copy fewer per yamux frame on the plain-KCP tunnels. The
  A/B (`l4`, `bulk-1`/`bulk-n`, clean, 3+1 rounds, `--aa`, binaries
  `molehill-{pre-p3,p3}.bin`, results `/home/dsh/tmp/p3-owned.json`,
  fingerprint `642a8ced3bef53df`) read **no claim**: throughput −1.37 %/−0.35 %
  (inside the run's scatter on the 8-stream cell, at it on the 1-stream one),
  `cpu_s_per_gbit` +2.2 %/+0.3 %, RSS −3.7 % inside its 9 % scatter. The
  round-37 attribution above is the mechanism: the kernel's UDP send path
  owns the CPU on these cells, so a userspace memcpy under 1 µs per ~64 KiB
  frame is invisible behind it. The candidate binary is kept
  (`/home/dsh/tmp/molehill-p3.bin`); the diff is rebuildable from this
  description in ~30 min (the files it touched: `src/mux/frame/io.rs`
  `WriteState` in `Bytes` + `poll_write_stage` const-dispatch,
  `src/mux/connection.rs` + `src/transport/multiplex.rs` + the server's
  `upgrade_to_tunnel` bounds on `AsyncWriteOwned`, and the
  `AsyncWriteOwned` impls on `KcpTunnelStream`, `ClientStream` and
  `ServerStream` — `src/common.rs`'s `owned_write` module ungated). What
  would re-open it:
  a cell where the *userspace* share dominates (e.g. `udp-pace`, whose
  paced-rate regime is pump-bound, not kernel-bound — untested here), or a
  kernel-side lever (GSO) landing first.

  The pump-bound re-test that entry promised **also read no claim, and
  closed the thread for good** (2026-10-12): the same saved binaries on
  `l4`/`udp-pace`/clean (3+1 rounds, `--aa`, `/home/dsh/tmp/p3b-pace.json`)
  — gap p99 1.349 → 1.335 ms (−1.0 %, inside its 0.96 % A/A scatter),
  throughput and wire bytes identical to the third decimal,
  `cpu_s_per_gbit` 30.53 → 35.56 (+16.5 %) against a floor of 5.5 % — but
  the per-round values tell the truth a median hides: base [37.6, 30.5,
  29.2] against candidate [35.6, 30.2, 38.2], the base's own first round
  higher than the candidate's median, sign agreement 0 of 3. The paced
  regime at 0.27 cores is too scattered over three rounds to resolve a
  sub-microsecond-per-frame userspace change, and the candidate is exactly
  level everywhere the run can see. The owned-write chain stays reverted:
  built twice, measured twice, no cell resolved it — any further attempt
  waits for the kernel-side term to move first (GSO), because that term is
  what both attributions priced as dominant.

- **The send path is priced: the 3.5 µs/datagram is the kernel's, not ours
  (2026-10-12, round 37 — the round-19 named test, now run).** The KCP stats
  line gained `proc_cpu_ms` (the process's own user+system CPU, from
  `/proc/self/stat`, `_SC_CLK_TCK`-exact), and one clean `l3`/`bulk-1` run
  (`MOLEHILL_DATA_CARRIER=kcp`, run fingerprint `1d95dee85018d12f`,
  `MOLEHILL_KCP_STATS=1`; results at `/home/dsh/tmp/p2-attrib.json`) answered
  it:
  the sending process's CPU rose **3.2× faster than `ms_out_send`** through
  the run's saturated seconds (Δcpu 2,280-2,310 ms/s against Δms_out_send
  700-726 ms/s, four windows agreeing). The process runs both pumps, so the
  user-space total visible on the same line (`ms_out_send` + `ms_input` +
  `ms_deliver` + `ms_writer` + `ms_update`) accounts for roughly 1.0 s/s of
  the 2.28 s/s the process actually burned — **the kernel's UDP send path
  takes more CPU than all of the pump's user-space phases combined.** That
  closes the round-19 question with the verdict it anticipated: no
  userspace send-path change (iovec reuse, `SendBatch` pooling, a larger
  `BATCH`) can pay materially; the lever is datagram *count* — the MTU, an
  operator setting already measured at +84 % (deployment.md) — or GSO,
  which changes the wire path and is the separately-recorded `udp_batch`
  A/B. The send-path thread is closed, not parked.

- **S1 for the L4 trigger: the carriers diverge on the l4 arm, and the
  signal is per-session ARQ health (2026-10-12, round 37).** Two carrier-
  pinned runs through the model (`l4`, `udp-pace` + `udp-ladder`,
  `loss1_buf`, tunnel leg, 2 rounds + warm-up, `--aa` each, identical
  method fingerprints `7cfd30c7a7de38bc`; default binary on KCP-first arm
  vs `MOLEHILL_DATA_CARRIER=tcp`; results `/home/dsh/tmp/p4-s1-{kcp,tcp}.
  json`): **KCP delivers every ladder cell with materially less inner loss**
  — 200M 0.89 % vs 2.79 % (−1.91 pts), 1000M 12.3 % vs 17.3 % (−4.9),
  2000M 52.5 % vs 56.3 %, 5000M 73.1 % vs 76.6 % (every paired round
  agreeing, both floors cleared), and `retrans_segments` **1 vs ~5,000** on
  the ≥1 Gbit/s cells (a floor of 1,000) — while `udp-pace`'s gap p99 is
  51.5 ms against 64.2 ms (directional; its own A/A floor did not clear).
  The cost is stated beside it (B, the TCP arm, is the *cheaper* one):
  `cpu_s_per_gbit` +73.6 % (200M) / +17.4 % (1000M), `rss_peak_mib`
  +58–360 % (a KCP session carries state a kernel TCP socket keeps in the
  kernel), fds −27 % (KCP's per-session sockets).

  What this settles for the rule the mechanism was built for: **the
  degradation signal is the KCP session's own ARQ counters** —
  `retransmits`/`datagrams_out` per session is 0 on the healthy arm and
  ~5,000 per cell on paths where the carrier is losing, a separation orders
  of magnitude wide, and the counters already exist per pump (`KCP_NS_*`,
  `MOLEHILL_KCP_STATS`). TCP has no userspace equivalent, so the rule's
  shape is one-sided by construction: a KCP tunnel whose retransmit share
  over a window clears a floor for `CARRIER_CONFIRM_TICKS`-style ticks is
  a carrier losing its traffic, and `begin_handover` to TCP is the probe —
  never the reverse direction from a TCP signal (there is none), which is
  also why the L3 rule's probe path (the only way to learn is to put a
  lane on it) is the right precedent. Not wired this round: the S2 rule
  needs per-tunnel KCP health surfaced next to the pool's per-tunnel bytes
  (the round-37 counter refactor's natural next piece) and its own A/B on
  exactly this cell — the standing caveat applies (the A/B must cover the
  degrade → switch → fresh-flows window).

- **The delivery ratio is not a discriminator either — the ARQ hides the loss
  (2026-10-13, round 7 of the switching goal).** The peer's report was extended
  with the bytes the server hands each tunnel (a counting adapter on the accepted
  connection, counted *above* the wire so a policer's drops stay visible), and the
  verdict moved to a delivery ratio: what this end received against what the peer
  handed over, with a 64 KiB volume floor and a 60 % threshold. On the policed
  path the number came back **100.0 %** — with the tunnel's `handed` at
  9 289 105 bytes per window, i.e. exactly the policer's own ceiling. The reason
  is the protocol doing its job: KCP *is* reliable, so a datagram the policer
  drops is re-sent until it arrives, and the loss surfaces as **throughput**, not
  as a delivery gap. A ratio between an application's writes and the peer's reads
  cannot see a carrier that is 10x slower while losing nothing.

  That closes the search for a *threshold* on either local quantity: the re-send
  share fires on both classes (round 6) and the delivery ratio fires on neither
  (this round). **What is left is a comparison, not a threshold** — and it is the
  shape the L3 lane policy already uses: the only way to learn what the other
  carrier would do is to *put a tunnel on it*. The handover mechanism does that
  non-disruptively (a tunnel is drained, its replacement dialed on the other
  carrier, the tunnel count unchanged), so the L4 rule's next form is a **probe
  and compare**: keep at least one TCP tunnel beside the KCP ones, measure each
  carrier's carried bytes per second per tunnel on this end, and move a tunnel
  when the other carrier's tunnels are carrying materially more — the measured
  separation for that comparison is the 10-13x the policed cells show. It needs
  no new wire field: the client already counts what it received per tunnel, and
  the peer's report already carries what was handed.

- **The ARQ share is not a discriminator — measured, and it misfires (2026-10-13,
  round 6 of the switching goal).** With the peer's report in place the rule now
  fires: on the UDP-policed condition it handed a tunnel over exactly once. It
  **also fired once on the healthy control** (`loss1_buf`, the class where the L4
  S1 measured KCP delivering *less* inner loss than TCP on every ladder cell) —
  so as it stands the rule would move traffic from the better carrier to the
  worse one. The per-window distribution says why, and it is not a threshold
  problem: the share is

  | condition | busiest tunnel, share per window | windows ≥ 4 % |
  |---|---|---|
  | `udp_throttled` (move is worth 10–13x) | min 14.0, median 28.7, max 29.9 | 7/7 |
  | `loss1_buf` (KCP is the better carrier) | min 1.5, median 3.5, max 6.8 | 3/8 (runs of 4 and 3) |

  The two overlap around the floor, and no constant fixes that: **re-sending is
  what an ARQ does on any lossy path**, and a lossy path is exactly where this
  carrier is the right choice. The metric is wrong, not its calibration — the
  same lesson the L3 evidence chain learned when it stopped reading a sender's
  counters and started reading what was *offered* against what was *refused*.

  **What the rule needs instead, and it is one field away:** a **delivery
  ratio**. The client already counts what it *received* per tunnel
  (`TunnelCounters::delivered`), and the peer's report is the natural place to
  carry what the server *offered* that tunnel — a per-tunnel counter on the
  server's stream path, keyed by the same prologue id the report already uses.
  Then the verdict is "this tunnel delivered a fraction of what was handed to
  it" (measured: ~10–20 % policed against ~95–100 % on the healthy class), which
  is a property of the path rather than of the protocol's own retransmission
  behaviour. Until that lands the rule stays off by default, and its doc comment
  now records that its current signal fires on both classes.

- **The peer-side tunnel report landed — the instrument works, the rule still
  does not fire (2026-10-13, round 5 of the switching goal).** The L4 half's
  missing instrument is built and verified end to end: a new control command
  (`ControlChannelCmd::TunnelHealth(service, tunnel, emitted, re-sent, seq)`,
  fixed-width like the lane's `CarrierReport`) carries **the server's** session
  counters for one tunnel back to the end that dialed it. The identity needs no
  wire change: a tunnel's stream prologue already names the pair
  `(ServiceId, TunnelId)`, the server records it in a slot the mux driver and the
  stream dispatch share, and its reporter samples the tunnel's own
  `SessionHealth` every 50 ms and sends one frame per change on the session's
  control writer. The client routes a report to the pool that serves that
  service and records it on the tunnel's counters; `health_counts()` is the
  **sum of both ends**, which is what makes the share direction-independent, and
  the pool-stats line now prints that sum — the number the policy acts on.

  Verified by running it: the server logs one report per tunnel
  (`emitted=`/`resent=` growing), the client records it
  (`summed=Some((76162, 18469))` on the policed condition — a **24 % share**
  against the 0.04 % its own half reads), and the rule's window log shows the
  peer's traffic arriving (`window=[(17, 12202, 455)]`).

  **And the rule still does not fire on `udp_throttled`** — with the evidence in
  place the blocker moved to the threshold's interaction with the window, and
  that is measured rather than guessed: the tunnel the rule judges reads
  **3.7 %**, just under the 4 % floor calibrated on the S1's *saturated* cells
  (5.7 %); a 2 % floor (the midpoint of the three measured points: healthy
  0.6 %, policed 3.7 %, saturated 5.7 %) was tried and *still* produced no
  firing in a full acceptance run, because the per-window share dips below it
  often enough to reset the confirm streak. That experiment is reverted — an
  unproven threshold is not worth carrying — and the next attempt needs the
  per-window **distribution** (how often, and by how much, a policed path's share
  dips) rather than a third guess at a constant. The floor's doc comment records
  this in the code.

- **The regime the L4 rule exists for, now measurable — and the rule is blind
  in it (2026-10-13, round 4 of the switching goal).** The wiring run left the
  rule off by default because its signal looked rate-dependent on a 1 %-loss
  path. That path is not the rule's case: a carrier riding UDP is *policed* by
  middleboxes, not merely lossy. The model gained the condition that says so —
  `udp_throttled`, an `iptables -m limit` accept-then-drop for UDP on the
  tunnel leg (10 000 packets/s, the module's own ceiling, ≈ 117 Mbit/s of
  1472-byte datagrams, so a carrier on UDP is starved while one that is not
  rides free). Two carrier-side runs through it (`l4`, `udp-ladder`,
  2 rounds + warm-up, `--aa`, same binary; KCP side with
  `MOLEHILL_TUNNEL_CARRIER_RULE=1` and `MOLEHILL_POOL_STATS=1`, TCP side with
  `MOLEHILL_DATA_CARRIER=tcp`) read:

  | `udp-ladder` | KCP under the policer | TCP under the policer |
  |---|---|---|
  | 200M | 100.8 Mbit/s, **43.87 %** loss | 200.0, **0.000 %** |
  | 1000M | 78.5, **86.39 %** | 998.9, 0.097 % |
  | 2000M | 78.2, **89.99 %** | 1047.1, 47.34 % |
  | 5000M | 77.7, **92.76 %** | 1046.0, 71.56 % |

  So the move this rule exists to make is worth **10–13× the delivered rate and
  40–90 points of inner loss** in the policed regime — the opposite of the
  1 %-loss cells, where the same carrier comparison favours KCP on every cell
  and the rule correctly stays quiet.

  **And it cannot fire there.** With the rule armed for the whole run, not one
  `carrier rule` line appears, while the pool's own `kcp_health` samples read per
  tunnel `11139/7`, `17763/6`, `4/0`, `4/0` — thousands of datagrams and a
  *handful* of retransmits, a 0.06 % share against a 4 % floor. The first two
  explanations were checked and both are wrong:

  - **Not a calibration problem.** The 512-datagram window floor is indeed
    ≈ 10 240 datagrams/s per tunnel, above what the policed path carries — but
    the counters themselves say the sessions were *not* struggling: they sent
    thousands of datagrams and re-sent six or seven.
  - **Not death and repair either.** The "a broken carrier's sessions keep
    dying" reading is falsified by the run's own log: **zero repair lines** in
    all three rounds. Nothing died, because a KCP session's dead-link rule needs
    *unacknowledged data of its own*, and this end had almost none in flight.

  **The real cause is direction, and it is structural.** The ladder is
  downstream through the tunnel (visitor → server → tunnel → client → sink), so
  the traffic that the policer destroys is the **server's** sends. `kcp_health`
  is a *dialing end's* surface — the server has no pool at all, and no
  `pool-stats` line to carry one — so the rule's window only ever saw the
  client's own upstream sends, which the policer barely touched. The deciding
  end cannot see the loss because the loss is on the peer's sending side; the
  same asymmetry the L3 evidence chain solved by having the *server* report what
  it offered each lane and what that lane refused (`CarrierReport`,
  `src/protocol.rs`). The L4 half has no such report, which makes the instrument
  the earlier record called "the recorded next instrument" — a **peer-side
  tunnel report** — *necessary* rather than optional, now measured rather than
  argued.

  What that costs and needs, in order: a shared tunnel identity (the client's
  index carried in the tunnel hello, the way a lane carries its id), a control
  command carrying the peer's per-tunnel `datagrams_out`/`retransmits`, and the
  rule reading the peer's numbers beside its own. Then this pair of runs is the
  acceptance case: with the report in place, the rule fires on `udp_throttled`
  and the handover buys the 10–13× the table above measures. Until then the rule
  stays off by default, and the docs already say why — though the reason they
  give (rate-dependent floors) is not the measured one and wants correcting with
  this finding.

- **S2: the rule is wired, and its own wiring run measured the sender-side
  signal as rate-dependent — the rule ships off by default (2026-10-12,
  round 38).** The plumbing landed as designed: `SessionHealth` (per
  session's `datagrams_out`/`retransmits`, bumped by the engine's
  retransmit sites and the staging writer), travelling with the tunnel into
  `TunnelCounters`, surfaced in the pool snapshot (`kcp_health`) and on the
  pool-stats line; the pure verdict (window ≥ 512 datagrams, share ≥ 4 %,
  three consecutive 50 ms ticks) acting through `begin_handover` and the
  endpoint's `prefer` under the switch cooldown; the switch read per pool
  at creation. The A/B (`l4`/`churn-16`+`udp-ladder`/`loss1_buf` tunnel
  leg, 3+1 rounds, `--aa`, `molehill-{prerule,rule}.bin`) then read **no
  claim anywhere — and zero rule firings in any log**, which was the run's
  real finding: probed on the paced cell at the same 1 % path loss, the
  sessions' actual retransmit share is **0.6 %** — under the 4 % floor the
  S1 saturated cells (5.7 %) had calibrated. The mechanism is the same
  asymmetry the L3 evidence chain was built around, now measured on the L4
  half: most of the delivered-loss difference the S1 saw is the **peer's
  queue refusing segments**, which a sender's ARQ counters cannot see; a
  sender's share rises only when the sender itself is saturating into the
  path. So the floor calibrated on the saturated cells cannot fire on the
  cells where traffic actually flows, and lowering it without a
  measurement behind the new number would be guessing.

  What shipped: the rule, its floors, the health plumbing, the snapshot
  surface and the kill switch — **`MOLEHILL_TUNNEL_CARRIER_RULE=1` arms
  it** (per pool at creation, uncached after the round-36 hatch lesson);
  the docs state what the signal reads and why the default is off. What
  this leaves open, in order: the peer-side tunnel report (the lane
  `CarrierReport`'s L4 analogue — a control command carrying what each
  tunnel's *receiver* accepted/refused) is the instrument that would make
  the sender's signal honest; and re-calibrating the sender-share floor on
  a rate-swept grid before any default flip. The churn/ladder A/B's other
  readings are noise-internal (no claim on any cell), consistent with a
  rule that never acted.

- **One instrument, and the boundary is a profile (closed 2026-10-10).**
  The earlier decision — a sibling instrument for L3, the sweep left alone —
  was a consequence of the model not existing. There is one runner now: the
  sweep is the `sweep` profile, the staged schedule is the `timeline`
  scenario, the drift axis is the `soak` profile, the screen is `--profile
  screen --ab-arm`, and a peer is an arm. What stays distinct is *evidence*:
  the model writes results outside the tree, and only a run the ritual
  publishes lands in `benches/records/`.
- The benchmark comparison against v0.10.0 must build that state (worktree at
  the tag): there is no released asset that speaks v5, and no two tags are
  compatible, so each side runs as a complete pair.

### The derived data plane: what landed, and what is next

The plan's L12–L18 are in, as four commits:

- the schema change: `mode` and `members` are gone, `uses_pool` derives the
  shape from the service type, `[transparent.data.tcp|kcp].tunnels` is the
  lane budget with an equal-share derivation and a refusal below the claim
  count, and the bench model moved with it (`l3-lanes4`, `l3-kcp4`; `l4-mux1`
  dropped — its one-tunnel pool cannot start against the topology's UDP floor);
- the placement table: a claim's flows are placed once and remembered, so a
  lane change cannot reorder a flow, a new flow never lands in an empty slot,
  an entry expires after 60 s of silence in either direction, and
  `MOLEHILL_L3_STATS` reports each lane's `flows`;
- the valve (L17): `[server.data].max_tunnels_per_client` now counts a
  transparent claim's lanes, not just multiplexed tunnels, and the harness case
  that proves it (cap `1` against a two-lane claim) is in `run.sh`;
- the allocator (L15): `src/transparent/lanes.rs` decides, `LanePool` in
  `src/core/client.rs` reads the hubs and dials, a lane is asked to end only
  when it holds no flow at all, and the receiver's lane is dialled only after
  the donor's has ended. The target is **a lane per talking flow**, and the
  first measurement of it is the harness's lending case: two claims sharing a
  budget of four, one carrying the 16-flow arm, the other nothing —
  `Lane allocator: lent lane 0 of claim 10.99.0.1:8444 to claim 10.99.0.1:8443
  on the Tcp carrier`, with the busy claim byte-exact through the change.

**Lending, measured (2026-10-11).** Four A/B runs through the model, each on
`bulk-n` (eight flows on one claim), ten rounds, with the run's own A/A twin
beside it. The final three are the ones to quote: they use the **same binary**
and two catalog arms that differ only in the knob
(`l3-budget10-lending` against `l3-budget10-nolend`, i.e.
`[transparent.data].lend` on and off), so the only thing between them is the
decision under test.

| condition (leg) | lending | no lending | A/A floor | verdict |
|---|---|---|---|---|
| clean loopback | 5.107 Gbit/s | 5.086 | 2.09 % | indistinguishable (+0.4 %) |
| **100 ms, carrier leg** | **2.395** | **1.754** | 11.39 % | **CLAIM: +26.75 %** (materiality 10 %, sign agreement 1.0, n=[10, 10]) |
| 1 % loss, carrier leg | 5.105 | 4.509 | 12.04 % | directional +11.69 % (sign agreement 0.8 — one round of ten disagreed) |

An earlier, cruder run of the same shape — a scratch build with the allocator
stubbed out as the second *binary* — put the long-haul effect at +39.95 %
(2.400 against 1.715). Same direction, larger point estimate, and it is the run
that found the retire path's cost (below); the arm-based numbers supersede it
because anyone can reproduce them from the catalog.

What that says, in order of how much it matters:

- **Lending's value is conditional on what limits the path.** On a clean
  loopback the L3 path is limited per *packet* (the TUN path's own cost, see the
  L3 attribution in `docs/benchmarks.md`), so more lanes buy nothing. With the
  carrier connections long-haul, a claim's throughput is limited per
  *connection* (window over RTT), and spreading its eight flows over the lanes
  the idle claims lent it is worth **+27 %** — a claim by the model's own rules.
  Under 1 % loss the same comparison is +12 % and directional: the effect
  survives loss, but ten rounds cannot separate it from the run's 12 % scatter.
- **It buys that with CPU and memory, not efficiency**: under the long-haul
  condition `cpu_s_per_gbit` 0.846 against 0.783 (+8 % per byte),
  `cpu_cores` +31.89 % and `rss_peak_mib` +11.27 % (both directional) — more
  connections carrying more traffic. A deployment on a fast, short path should
  expect neither the throughput nor the cost, and can turn the mechanism off
  with `[transparent.data].lend = false`.
- **The condition's leg decides whether the experiment can see anything.** The
  same A/B with `--condition rtt100 --condition-leg visitor` (the default) gave
  +2 % and nothing above the floor, because the *carrier* was never the
  bottleneck; four runs went into learning that.
- **The retire path cost 8 % before it was fixed.** The first A/B had the
  pre-lending binary ahead (4.857 against 5.072 in one run, consistently across
  four), and the cause was a lock the packet path took per relayed packet
  (`8bee843`): mirroring the draining mark in an atomic and building the retire
  wait once per lane instead of once per packet recovered more than the whole
  regression. Without that measurement the cost would have shipped as "lending
  made it slightly slower".
- **The model could not state a config-axis claim until this run.** Two catalog
  arms were never compared: `_pairs_of` paired each arm with the control and
  each `~b`/`~aa` twin with its base, so a decision that is a *field* rather
  than a second binary had no verdict. An arm may now declare `against = "<arm
  id>"` and the analysis pairs them — which is how the table above is a claim
  and not two columns a reader has to subtract.

Still true, and the reason the mechanism is designed the way it is: **a single
flow cannot be spread over two lanes** — its packets must stay in order, which
is what pins it to one — so a one-flow workload gains nothing from lending by
construction, and the target is one lane per talking flow.

### KCP, second thread: the inner-UDP loss item, measured (2026-10-11)

The recorded item was "inner-UDP loss 1.05 % vs 0.60 %" on the KCP carrier. It is
now measured the way §10 requires — the metric's own floor **and** the run's own
A/A — with the contrast the new condition gives: the same arm, the same path, the
carrier chosen by the product (KCP) or forced for isolation (TCP).

```
sudo -n uv run benches/scripts/bench/bench.py run --profile smoke --arms l3,l4 \
  --scenarios udp-pace,udp-ladder --condition loss1 --condition-leg tunnel \
  --rounds 3 --warmup-rounds 1 --aa --out /home/dsh/tmp/k4-udp-kcp.json
sudo -n env MOLEHILL_DATA_CARRIER=tcp uv run benches/scripts/bench/bench.py run \
  --profile smoke --arms l3,l4 --scenarios udp-pace,udp-ladder --condition loss1 \
  --condition-leg tunnel --rounds 3 --warmup-rounds 1 --aa --out /home/dsh/tmp/k4-udp-tcp.json
uv run benches/scripts/bench/bench.py compare /home/dsh/tmp/k4-udp-kcp.json \
  /home/dsh/tmp/k4-udp-tcp.json
```

Both runs are on `6b5ea61`, three rounds an arm, one warm-up, `--aa` on, the
`tunnel` leg shaped with 10 ms delay + 1 % loss. `A` is the KCP run, `B` the
TCP one.

**The recorded item is noise.** On the paced session (`udp-pace`, 2000
datagram/s, 1200 B) the inner-UDP loss is 0.821 % [0.775..0.975] on KCP against
0.920 % [0.736..0.980] on TCP — a 0.1-point difference, inside the same run's
A/A twin (KCP twin 1.075 %) and far below the metric's 1.0-point materiality
floor. It is not a claim in either direction, so the note that started this
thread was reading scatter.

**What is a claim, and is worth the optimisation work:**

1. **KCP's ARQ collapses where TCP's holds**, on the bulk ladder with the same
   1 %-loss tunnel leg: at 1000 Mbit/s offered, KCP delivers 384.6 Mbit/s with
   **59.5 %** inner-UDP loss against TCP's 986.6 Mbit/s with **0 %**; at
   2000 Mbit/s it is 386.0 Mbit/s / 79.0 % against 1972.9 Mbit/s / 0.002 %
   (`udp_recv_mbit` -61.0 % and -80.4 %, `udp_loss_pct` +59.5 and +79.0 points,
   every paired round agreeing, both floors cleared). Above roughly 150 Mbit/s
   the KCP carrier stops being a usable path for a UDP workload on a lossy link
   while the TCP fallback is unaffected — and note *both* carriers make inner
   UDP reliable, so this is KCP's reliability machinery losing, not a
   retransmission-policy difference.
2. **A paced UDP session costs 6.8× the CPU per delivered byte on KCP**:
   `cpu_s_per_gbit` 83.04 vs 12.19 (+581 %, floor 15 %), `syscalls_per_s` +49.6 %
   and `bytes_per_syscall` -30.6 % beside it, `rss_peak_mib` +19.1 %. The same
   run shows the *latency* shape KCP is chosen for (gap p99 57.6 ms against
   TCP's 248.8 ms), but it is not a claim: the TCP twin's own rounds disagreed
   (one round saw 521 ms), which is exactly what the A/A floor is for.

**A method artefact hid part of the number, and a deeper queue names the real
ceiling.** `loss1` pins no netem queue limit, so the class runs on netem's
default 1000-packet queue: on a 1 Gbit/s ladder burst that queue tail-drops
*beyond* the 1 % it emulates, and part of what looked like KCP's loss was the
shaper's. The new `loss1_deep` class is the same 1 % loss with a 100 000-packet
queue, and the ladder cell moves from 384.6 Mbit/s / 59.5 % loss to 545.0 /
42.5 % — while the TCP fallback on the *same* class delivers 988.4 Mbit/s with
**0.051 %** loss (A/A twin 985.8 / 0.010 %, three rounds, `--aa`). So the
carrier gap is real and large once the shaper stops adding its own drops: the
KCP carrier carries about **55 % of what the TCP fallback carries** on a
1 %-loss path, and the A/A twin shows the run can resolve differences of a few
percent, so this is not scatter.

**First optimisation attempt: the RTO floor. REVERTED — it did not clear the
floors.** The counters said `rto_ms=30` with `srtt_ms=16` (the nodelay floor
binding while the path's queueing delay exceeds it) and `resends_rto=71631` of
`retransmits=100021`, so raising the nodelay minimum RTO to 60 ms was the cheap
test. Built as a second binary and A/B'd in one run (`--ab-arm l3 --binary
…/molehill-base --binary-b …/molehill-rto60`, `loss1_deep`, tunnel leg, 3 rounds
+ 1 warm-up, `--aa`, 60 samples): `udp-ladder[1000M]` throughput 536.7 →
547.5 Mbit/s (+2.0 %) and inner loss 42.04 → 42.98 %, with the run's own A/A twin
at 539.1 Mbit/s — inside the run's noise, and **no claim cleared the floors**.
The change is reverted (the product source is byte-identical to the baseline,
verified by binary hash) and the engine keeps upstream's floor. What that rules
out is the floor itself: the retransmissions are not caused by an
under-estimated RTO, so the 21 % re-send rate is either affordable or comes from
somewhere else in the send path.

**The diagnosis first, and it named the ceiling: the send window.** Running the
ladder under `MOLEHILL_KCP_STATS=1` and reading the deltas at the busiest second
(the 1 Gbit/s cell) gives, on the **sender**: `datagrams_out +54791`, of which
`retransmits +3135` (5.7 %, not the 21 % the shallow-queue class showed),
`wait_snd_peak=4107` — the send queue sitting at its limit (`2 × snd_wnd`) while
the app keeps writing — and per-phase times of `ms_output +122`, `ms_writer
+24.7`, `ms_input +17.5`, `ms_update +15.1` per second, i.e. **~18 % of a core**,
with the pacer at 12 Gbit/s. The receiver's second is `datagrams_in +21373` with
`acks_out +21373` (one ack per datagram) at ~8 % of a core. So neither end is
busy, nothing is spilling, no window is *waiting* on the receive side — and the
sender's queue is full: the ceiling is in-flight data, not CPU.

The arithmetic then fits the measurement: 2048 segments × 1472 B ≈ 2.8 MiB over
the ~44 ms round trip a lossy path with a large buffer settles at is **~533
Mbit/s**, and the ladder cell measured 533-556 Mbit/s across three different
queue classes. That is the window, exactly.

A third condition followed from the same reasoning: `loss1_deep`'s 100 000-packet
queue (≈147 MB) is not a network anyone has — it is deep enough to add ~1 s of
queueing at these rates — so `loss1_buf` was added (10 000 packets ≈ 14.7 MB,
≈27 ms of standing queue at 545 Mbit/s): deep enough that tail drops are not the
story, shallow enough that the RTT stays a path property. The baseline measures
556.5 Mbit/s / 42.3 % there, i.e. the same ceiling.

**Third optimisation attempt: send window 2048 → 8192 segments. KEPT — it
clears both floors, and the checks are clean.** Two-binary A/B, `--aa`, three
rounds each:

| cell (`loss1_buf`, tunnel leg) | baseline (2048) | candidate (8192) |
|---|---|---|
| `udp-ladder[1000M]` | 556.5 Mbit/s, **42.31 %** loss | **0.16 / 0.37 / 0.66 %** loss per round, +43.2 % delivered |
| `udp-ladder[2000M]` | 533.5 Mbit/s, 70.87 % | +49.5 % delivered, −27.0 points loss |
| `udp-ladder[5000M]` | — | −16.3 points loss |

Every paired round agreed in sign and both floors cleared (`udp_loss_pct`
+41.9 points against a 1-point floor; `udp_recv_mbit` +43.2 % against 10 %). The
cost is real and stated: `cpu_cores` +47.5 % and `cpu_s_per_gbit` +46.6 % on that
cell (it carries 43 % more), `rss_peak_mib` +15 %; at 2000M the extra cost is
−36 % cores. The two regression checks the old note demanded: on a clean path
with one lane (`l3`, `bulk-1`/`bulk-n`/`rr-16`/`udp-pace`) the only claim in
twenty-four comparisons is in the candidate's favour (`setup_p50_ms` −26 %), and
on the four-tunnel burst case (`l4`, clean, `bulk-1`/`bulk-n`) there is **no
claim at all** (bulk-n 9.704 against 9.789 Gbit/s) — so the 2048-era burst
collapse does not reproduce at 8192 with the current socket buffers and pacer.

**Where the inner-UDP item stands after the window fix (2026-10-11).** The same
`loss1_buf` class, three rounds an arm with `--aa`, KCP against the forced TCP
fallback, `l3` and `l4` in one run each:

| `udp-ladder` cell | KCP (`l3`) | TCP (`l3`) | KCP (`l4`, four tunnels) | TCP (`l4`) |
|---|---|---|---|---|
| 200M | 197.7 Mbit/s, 0.000 % loss | 194.8, 1.465 % | 195.1, 1.163 % | 193.8, 1.774 % |
| **1000M** | **985.7, 0.120 %** | **986.6, 0.009 %** | 840.9, 14.486 % | 837.0, 14.323 % |
| 2000M | 1033.7, 43.99 % | **1973.6, 0.000 %** | 870.2, 53.7 % | 874.1, 54.2 % |

Three things this settles:

1. **The recorded item is closed at the rate that matters.** At 1 Gbit/s the KCP
   carrier now delivers 985.7 Mbit/s with 0.120 % inner-UDP loss against TCP's
   986.6 / 0.009 % — the 42 % the earlier rounds measured was the 533 Mbit/s
   send-window ceiling starving the path, not loss recovery failing, and the
   fix is the 8192-segment window that shipped.
2. **Above 1 Gbit/s the carrier still caps at ~1.05 Gbit/s** (2000M cell) while
   TCP reaches 1.97 — that is the receive-window ceiling the fourth attempt
   chose not to buy, and this run is the independent confirmation of that
   choice: the ceiling is real, deliberate, and stated.
3. **The remaining KCP cost is CPU per delivered byte**: 1.661 s/Gbit against
   TCP's 0.586 at the same 1 Gbit/s cell — **2.8×** — with the four-tunnel arm
   identical on both carriers (2.29 against 2.05) because the multiplexer, not
   the carrier, is the ceiling there. This is the same family as the paced-UDP
   figure from the first thread (6.8× per delivered byte at 2.4 Mbit/s), and it
   is the open item the next round starts from: the carrier's per-packet work,
   not its window or its recovery.

**Fourth attempt: receive window 4096 → 8192. REVERTED — it buys throughput past
the point where the carrier's efficiency holds.** Same method (two binaries, one
run, `--aa`, 3 rounds, `loss1_buf`):

| cell | shipped (snd 8192 / rcv 4096) | candidate (8192 / 8192) |
|---|---|---|
| `udp-ladder[1000M]` | 981.6 Mbit/s, **0.171 %** loss, 1.610 s/Gbit | 986.9 Mbit/s, 0.000 %, **2.071 s/Gbit (+28.6 %)** |
| `udp-ladder[2000M]` | 1046.7 Mbit/s, 43.3 % | **1746.2 Mbit/s, 11.1 %** |
| `udp-ladder[5000M]` | 1044.6 Mbit/s, 66.8 % | 1606.0 Mbit/s, 51.1 % |
| `udp-pace[1000/s]` | 0.971 % loss | 0.869 % (inside the A/A band), plus `cpu_cores` and `cpu_s_per_gbit` worse by ~23 % |

The claims are on both sides with sign agreement 1.0: the candidate delivers
+40 % and loses 32 points less on the 2000 Mbit/s cell, while the shipped build
is better on `cpu_cores` (−39 %), `cpu_s_per_gbit` (−35 %), `syscalls_per_s`
(−19 %), `rss_peak_mib` (−29 %) and `wire_per_visitor_byte` (−38 %). The
distinction that decides it is the one the round-3 change was kept for: at
1 Gbit/s — the rate the carrier has to serve — the shipped build already
delivers 981.6 Mbit/s with 0.171 % loss, and doubling the receive window buys
**0.5 % throughput for +28.6 % CPU per delivered byte**. What the candidate
really does is move the ceiling from ~1.05 to ~1.75 Gbit/s, and it pays ~2× CPU
per bit above 1 Gbit/s to do it; on a proxy whose typical host is a small box,
that is not a change to make by default. It is `HANDOFF`-recorded rather than
shipped, and a *capped* middle (rcv 6144) is the obvious next variant if a
high-rate lossy path ever becomes the target.

The receive window stays 4096, so the peer's advertised window is still what
caps the sender at ~1.1 Gbit/s — the deliberate end state, not an oversight.

**Second optimisation attempt: congestion control on. REVERTED — it is far
worse.** Built as a second binary (`set_nodelay(..., nc=false)` against the
baseline's `nc=true`) and A/B'd in one run on `loss1_deep` with four scenarios
(`udp-ladder,udp-pace,bulk-1,bulk-n`, 3 rounds + 1 warm-up, `--aa`, 84 samples):

| cell | baseline (`nc=1`) | candidate (`nc=0`) |
|---|---|---|
| `udp-ladder[1000M]` | 531.9 Mbit/s, 41.8 % inner loss | **2.7 Mbit/s, 97.3 %** (two of three rounds failed) |
| `udp-ladder[2000M]` | 534.5 Mbit/s, 71.1 % | **all three rounds failed** |
| `bulk-n[P8]` | — | `cpu_cores` +1639 %, `syscalls_per_s` +15130 %, `rss_peak_mib` +69 % |

The engine's `nc=0` path is the reference's: `cwnd` grows by slow start, halves
on a fast retransmit and restarts at **one segment** on a timeout. On this path
that restart never recovers — 15 000 % more syscalls is a pump spinning on a
one-segment window — so the candidate is not a slower carrier but a stalled one.
Reverted, and the product source is byte-identical to the committed baseline
again.

**What two reverted attempts establish:** neither the RTO floor nor the
congestion window is the knob that fixes the KCP bulk ceiling on a lossy path.
Both are now *measured* rather than assumed, and the numbers say the next step
is diagnosis rather than another policy guess: with `nc=1` the sender spends
21 % of its datagrams on re-sends and reaches ~55 % of the TCP fallback's
throughput, so the question is which phase of the send path is saturated —
`MOELEHILL_KCP_STATS=1`'s per-phase timings under the deep-queue class
(`ms_input` / `ms_deliver*` / `ms_writer` / `ms_output` / `ms_update`) on both
ends, read *while* the ladder runs at 1 Gbit/s, before any further change. A
bound on in-flight data tuned to the path's bandwidth-delay product is the
shape the evidence points at (the window is 2048 segments ≈ 2.8 MiB, and the
deep queue's own limit is 100 000 packets), but that is a hypothesis to test
with the same A/B, not a conclusion.

**Next hypothesis to test, not to assume:** the session is configured with
congestion control **off** (`set_nodelay(true, 10, 2, /*nc=*/true)`, and the
listener logs `nc=1`), so a sender with a 2048-segment window keeps blasting
into a lossy path and re-sends what the path drops — which is consistent with a
21 % re-send rate, a 55 %-of-TCP ceiling and an output phase that dominates the
sender's time. The engine implements `ssthresh`/`cwnd` when `nc=0`, so the next
A/B is congestion control **on**, measured on the same cells (throughput, inner
loss, `retransmits` per delivered byte) and reverted the same way if it does not
clear them.

**The window/pacing hypothesis is falsified, and the real one is in the
retransmission counters.** A ladder run with `MOLEHILL_KCP_STATS=1` on the same
condition (`54f...`/`6b5ea61`, one round, `--aa`) shows, on the **client**:

```
retransmits=0 resends_rto=0 resends_fast=0 wait_snd_peak=0 wait_rcv=0
advertised_wnd=4096 peer_wnd=4096 pacer_mbit=9000 spill_bytes=0
```

— no window wait, no queue spill, and a 9 Gbit/s pacer: nothing at the client
is the ceiling. The **server's** counters name it:

```
datagrams_out=483665 retransmits=100021 resends_rto=71631 resends_fast=28390
ms_output="1509.263" ms_input="269.649" ms_writer="253.221" srtt_ms=16 rto_ms=30
```

The sender retransmits **21 % of its datagrams on a path that loses 1 %** — a
20× spurious-retransmission rate — and the output phase is where its time goes
(1.5 s against 0.27 s for input and 0.25 s for the writer). So the bulk
collapse is not a window, a pacer or a queue: it is KCP's loss recovery
retransmitting far more than the path loses, which both wastes the wire and
consumes the send path that would have carried payload. The next change to try
is therefore in RTO/fast-resend policy (`rto_ms=30` against `srtt_ms=16`;
28 % of the resends are the fast path), measured on exactly these counters —
`retransmits` per delivered byte — rather than on throughput alone.

### Quality-driven carrier switching: the plan, and the first piece

The shipped fallback reacts to a carrier that **cannot be established**. The
measurements say that is not the only case worth reacting to: on the same 1 %-loss
class the two carriers differ by ~90 % of delivered throughput at 2 Gbit/s
(KCP 1033.7 Mbit/s with 44 % inner loss against TCP's 1973.6 with 0.000 %), while
at 1 Gbit/s they are level and on a clean path TCP is the cheaper one. So the
policy that follows from the data is "stay where you are unless the carrier in
use is measured to carry materially less", delivered by machinery that already
exists:

- **The decision** lives in `CarrierChoice` (per data endpoint, one per client):
  `prefer(carrier, why)` moves it deliberately, is refused while a
  `SWITCH_COOLDOWN` (120 s) from the previous switch is running, prints one
  `INFO` line per switch and then `DEBUG`, and holds the new carrier against the
  re-probe (a switch *to* TCP writes the same 60 s retry gate a fallback does).
  **Landed with tests**; the policy is the only thing still missing.
- **The move** is a lane drain, and the hub already implements it: `retire_lane`
  marks a slot draining, a draining slot takes **no new flows** (`live_slots`
  excludes it) and keeps serving the flows already placed on it, and when the
  slot vacates the *server's* replacement request dials a new channel into it —
  which asks the choice, and therefore rides the new carrier. So a switch never
  reorders: existing flows finish on the old carrier, new ones start on the new
  one, and a busy lane's flows are only moved when they end on their own.
- **The diagnostic hatch** (`MOLEHILL_CARRIER_SWITCH_AT=<seconds>`, landed) does
  exactly one switch that way, at a fixed time, so its *effect* can be measured
  before the policy that decides one is written: what the following lanes
  capture of the ~90 % the A/B says is available, and what the drain costs the
  flows that were on the retired lane.

**Measured: a switch captures the whole prize, and costs nothing where the
carriers are level.** `l3-budget10` (ten lanes over five claims — two each, so a
claim always has one lane left while the other is drained), `loss1_buf` on the
tunnel leg, `udp-ladder`, three rounds plus a warm-up with `--aa`, same binary,
the only difference being `MOLEHILL_CARRIER_SWITCH_AT=6` (one switch, six seconds
in, every claim of the endpoint):

| `udp-ladder` | carrier held (all KCP) | one switch at 6 s | change |
|---|---|---|---|
| 200M | 197.4 Mbit/s, 0.000 % loss | 197.4, 0.000 % | — |
| 1000M | 984.1, 0.140 % | 979.6, 0.126 % | level (inside the run's noise) |
| **2000M** | **1057.6, 40.73 %** | **1883.5, 4.49 %** | **+78 % delivered, −36 points of loss** |
| **5000M** | **1041.8, 61.99 %** | **3067.5, 7.87 %** | **+194 % delivered, −54 points of loss** |
| CPU per delivered byte (2000M) | 0.821 s/Gbit | 0.552 | −33 % (it delivers more per CPU-second) |

The A/A twin in the carrier-held run is tight on the divergence cell (1056.8 /
40.83 against 1057.6 / 40.73), so the baseline is not scatter; the switched run's
cells are tight too (1883.2..1883.7). The switched figure is 95 % of what forcing
TCP outright delivers on the same cell (1973.6), which is the right reading: one
of a claim's two lanes moved, and the claim now carries a KCP lane *and* a TCP
lane. Nothing on the cells where the carriers are level moved — so the policy has
room to be conservative and only act on evidence.

### FIXED: the black hole behind a retired lane (2026-10-11)

The bug below is fixed by a **CLOSE control frame** (`CTRL_CLOSE = 0x04`, the
same 16-byte vocabulary as PING/PONG/SACK): when both halves of a session's
stream are gone and its ARQ tail is flushed, the session sends three CLOSE frames
20 ms apart, and a session that receives one ends immediately. The regression
test that failed at the 30 s timeout now passes in 0.03 s, and the bench cell
that reproduced it end to end — `l3-budget10` over `udp-ladder,udp-pace` on
`loss1_buf`, which read **100 %** loss for whole rounds — reads 0.873 / 1.360 /
1.129 / 0.825 % across four rounds. Frames are sent only on the orderly close
path (never in answer to one), so two peers cannot ping-pong them, and an older
peer that does not know the kind already ignores unknown control frames (the
catch-all is `trace!`).

### The bug it fixed: lending on + ladder-then-paced loses the paced flow (2026-10-11)

The fix above made `l3-budget10` runnable for the first time, and its
`udp-pace` cell reads **100 % loss** in some rounds. Bisected as far as the
evidence goes:

- **Lending is implicated, not the switch.** The 100 % appears with the hatch
  unset (`switch-off2`, i.e. shipped behaviour) and with it set, so the switch is
  not the cause. `l3-budget10-nolend` with the same scenarios and rounds is
  **healthy** (1.122 / 0.773 / 0.870 %), and `l3-budget10` with lending on and the
  paced scenario run **alone** is healthy too (1.251 / 1.306 / 0.978 %).
- **The trigger is the ladder running first.** `l3-budget10` +
  `udp-ladder,udp-pace` is where it shows, intermittently: three reproduction
  attempts produced healthy rounds (1.075 %, then 0.976 / 1.081 / 1.179 %), so
  the failing case is timing-dependent rather than a fixed configuration.
- **What the allocator is doing at the time** (`MOLEHILL_L3_STATS=1` run, the
  client's own lines): the ladder's traffic makes claim `…:2405` the busy one, so
  the allocator lends to it from every other claim —
  `lent lane 1 of claim …:2401 to claim …:2405`, then `lane 0` of `2402`, `2403`
  and **`2404`** — and `2404` is exactly the claim the paced probe dials
  (`UDP_PORT`). A live flow cannot lose its lane (`plan` only names a donor with
  more than one lane and a lane with `flows == 0`, and the hub re-checks under
  its own lock), so the remaining candidate is what happens *after* the lend: a
  claim that ends up with no live lane has an empty `live_slots()`, the placement
  then falls back to the hash alone, and a flow placed in a memberless slot is
  dropped as `no_channel` for as long as it lives — which is 100 % loss for a
  single-flow probe.

**First theory, falsified.** The obvious reading of the allocator trace — a
replacement takes the *lowest* vacancy, so a lane lent away earlier gets the
replacement while the flows pinned to a dead slot are stranded forever — does not
hold: `Routes::vacate` removes the claim from the table entirely once no slot
holds a queue, and the claim's flow placements go with it, so a claim with no
lanes re-places its next packet fresh instead of pinning it. A hub test written
to demonstrate the stranding fails for exactly that reason (its assertion finds
`member_stats()` empty), and the change that would have made a replacement prefer
a pinned slot was therefore reverted rather than kept on a theory.

**Caught, with both ends' counters — the lane is a black hole after a lend.**
A run that reproduced it (`MOLEHILL_L3_STATS=1`, `l3-budget10` +
`udp-ladder,udp-pace`, `loss1_buf`, tunnel leg, 3 rounds + 1 warm-up) gives rounds
of 0.870 % then **100.000 / 100.000 / 100.000 %**, and the per-second lines say
exactly where the traffic stops:

- the allocator's own line at 11:02:51: `lent lane 0 of claim 10.99.0.1:2404 to
  claim 10.99.0.1:2405` — the paced probe's claim lends a lane away, as designed;
- **client**, claim `:2404`: `member=1 live=true` for the whole failure (so the
  claim is *not* dead), `flows=0 forwarded=0 no_channel=0` on both slots, and the
  aggregate line **stops growing entirely** (`forwarded=70`, `unclaimed=16`, then
  constant for minutes) — nothing arrives at the client's device at all;
- **server**, same claim: `member=1 live=true flows=1 forwarded=2370 … 2396` and
  climbing — the server is still handing the visitor's datagrams to a lane.

So the client retired one of the claim's two lanes, and the *server* keeps writing
into the channel that lane was: its flow entry is pinned to that lane's slot (the
rule that makes a lane's death invisible to flows), while the client's end of the
channel is gone. The client's remaining live lane receives nothing because the
server is not sending to it.

Why the server never notices is the part that makes this a defect rather than a
race: a KCP session whose *lane task* has ended is not necessarily a session the
peer sees fail. If the client drops the reader but the session's pump keeps
answering — which is exactly what a KCP session does when only its consumer goes
away — the server's writes are acknowledged, its keepalives are answered, and its
dead-link detector (`~20` unacknowledged RTOs) never fires. The result is a
channel that looks healthy from both ends and carries nothing.

**Reproduced deterministically, and the mechanism is now complete.**
`src/transport/kcp.rs`, `a_dropped_stream_ends_the_session_for_the_peer` (committed
`#[ignore]`d with its reason): a session pair on the loopback, a round trip both
ways to prove it works, then the consumer's stream is dropped while the peer keeps
writing. Three things follow from it, each verified rather than reasoned:

1. **The end is prompt on the side that ends.** A temporary probe in the pump's
   exit path (reverted) showed the dropped stream's session going
   `writer half gone` → `reader_closed=true` → `quiescent=true` → exit, inside the
   5 s `CLOSE_FLUSH_TIMEOUT` the code promises. The half-close default is not the
   problem either: dropping the whole stream closes both halves, as a retired lane
   does.
2. **The peer is never told.** KCP has no close handshake, so the other end learns
   only from its own unacknowledged traffic. The test's failure message measures
   exactly that: in 30 s the peer sent **7876 datagrams of which 7288 were
   retransmissions** — it knows perfectly well that nobody is listening — and it
   still holds the session open.
3. **Its dead-link rule cannot fire.** That rule wants 20 retransmits *and* a send
   window that has stood still for 5 s. A lane forwarding a visitor's traffic
   keeps taking new data, so its window is never still; with the nodelay RTO
   growing ×1.5 per retransmit, "20 retransmits" is minutes even when it could
   apply. For a claim whose lane was retired or died, the black hole is therefore
   **permanent** for as long as the visitor keeps sending — which is what the bench
   captured (the server's `forwarded` climbing for minutes while the client's
   counters did not move).

**The fix this points at — a close frame.** The adapter already carries control
frames it recognises by a magic byte (PING/PONG/SACK, `is_ctrl_frame`), and the
wire is the unreleased v5 line, whose commands may be extended in place. Adding a
CLOSE frame, sent when a session's stream ends, tells the peer to end its session
at once: its lane task sees the end, vacates its slot, and the flows pinned to it
are re-placed onto the lanes that remain. Tightening the dead-link rule is not a
substitute — a lane with traffic has no still window to trigger it — though the
rule is worth revisiting separately for the idle case.

**The fix to design (next round, with a test that fails first):****The fix to design (next round, with a test that fails first):** a lane's end
must be *visible to the peer*. When a transparent lane retires, drains or its task
exits, its carrier connection has to be shut down — not merely unread — so the
peer's lane task sees the end, vacates its slot, and its pinned flows are
re-placed onto the lanes that remain. The two halves to check are the client's
lane teardown (does ending a lane close its KCP session/TCP connection for the
peer?) and the server's own placement rule (a flow pinned to a lane that vanished
without a close is the same stranding shape this bug has already shown twice).

**Superseded: the earlier hypothesis below is wrong** — the claim does keep a live
lane through the failure, so "lending turns a claim into an all-or-nothing shape"
is not what happens. Kept for the record because it is the road that led to the
counters above.

**An earlier hypothesis, since falsified.** Lending turns a claim
into an all-or-nothing shape: with two lanes one death leaves one, but the
allocator moves a claim down to `target = 1` when it lends its idle lane, and a
single lane death then leaves that claim with **no** lane at all. Whether it
heals depends on the server asking for a replacement channel and on
`wants_another_lane` answering yes (live 0 < target 1) — and the paced probe's
single long-lived flow is the exact shape that would read the whole round as
100 % loss if that ask does not come or is refused. It also explains the bisect
precisely: with lending off the claim keeps two lanes, one death leaves one, and
nothing is total.

The next step is instrumentation rather than another theory: run
`l3-budget10` + `udp-ladder,udp-pace` with `MOLEHILL_L3_STATS=1` until a round
fails (it appeared in roughly half the rounds when it appeared at all), then read
**both ends'** per-slot lines for the paced claim — `live=false` on every slot
with `no_channel` growing is the all-or-nothing shape, and a client line
showing `wants_another_lane` refusing (or no replacement ever joining) names the
half that is broken. Until that is understood, **the carrier-switching policy
does not ship**: it moves lanes with the same machinery this bug lives in.

**Two things the measurement found on the way**, both worth fixing before the
policy is written:

1. **Every `pool_cap` arm had been unrunnable since the carrier tables were
   merged** (the K3 rename made the topology writer emit a second
   `[transparent.data]` header, a duplicate key the loader refuses). `l3-budget10`
   and the two lending arms failed all cells with "not ready"; the self-check had
   only *built* the configs, never parsed them. Both are fixed in this commit:
   one table per block, and `bench.py selfcheck` now runs every catalog arm's
   server and client config through `tomllib` (556 → 582 checks).
2. **`l3-budget10` on `udp-pace` measures 100 % loss**, in *both* runs and
   therefore independently of the switch: a single long-lived UDP flow over a
   ten-lane claim loses everything, while the same arm's `udp-ladder` cells are
   healthy. This could not be seen before because the arm's config never loaded.
   It is a real open item — either the lane path under one long-lived flow with
   lending on, or the model's paced workload — and it is the first thing to
   diagnose next round, before the policy is built on a claim shape that may not
   work.

**What the policy still needs, in order** (next rounds):

1. Per-lane evidence, which does not exist yet: the allocator's snapshot carries
   `flows` per lane but not `forwarded`/`no_channel`. The hub counts both per
   slot already, so this is plumbing plus the previous tick's values to make a
   delta (`forwarded/s` and the drop ratio, per lane, per carrier).
2. The rule itself: keep a split across the carriers the endpoint can use
   (at least one lane on each while a claim holds two), compare the carriers'
   delivered rates and drop ratios over a window, drain a lane of the loser and
   let its replacement ride the winner — one move per tick at most, never more
   often than the cooldown. With converged evidence, stop probing on every tick
   and re-probe on a long timer, or the clean path pays KCP's per-byte cost
   forever for a comparison it has already won.
3. The forwarding-pool half (L4): a *per-tunnel* handover, never a whole-pool
   swap, because the server's `max_tunnels_per_client` valve counts tunnels and
   an overlap would be refused.

### The policy did not trigger, and the read-only review says why (2026-10-11)

The round-3 hypothesis — "the policy reads the *send* side, and `udp-ladder` is
downstream traffic, so the client's per-slot counters stay at zero" — is
**partly right and partly wrong**, and both halves were established by reading
the code rather than by another run:

- **Right about the direction being unproven by the bench.** `run_udp_ladder`
  dials the tool's exposed port from the visitor namespace
  (`topology.VIS_NS`) towards the iperf3 sink the client namespace binds
  (`benches/scripts/bench/workloads.py`, `iperf_sink` + `run_udp_ladder`), and
  the command carries no `-R`. So the ladder is **visitor → client**, not the
  downstream direction the earlier note assumed — which does not exonerate the
  policy, because what it reads is not delivery either (below).
- **Wrong about what the counters mean.** `Routes::flush` counts `forwarded`
  when a batch is *accepted by the slot's channel* and `no_channel` when it is
  not — a local enqueue result, on the end that read the packet from its own
  device. It is not "delivered to the peer", and it is not a drop the carrier
  made. Two different causes share the counter: a full 128-message queue (the
  path's own backpressure) and a slot whose lane is gone. So `drop_pct()` in the
  round-3 policy is a mis-named quantity: it is "offered packets this slot did
  not enqueue", and for a claim whose lanes are alive and fast it is 0 even
  when the carrier two hops away is losing a third of what it was handed.

That is the real reason a policy built on it cannot fire on the divergence cell,
and it is not fixed by adding a receive counter alone. What the policy needs is
a **per-lane identity plus both ends' view**: which lane (not which slot) rode
which carrier, what this end offered it, what the *other* end received from it,
and whether that lane is empty on **both** ends before it is retired. The
round-3 change reads slots, has no lane identity, and takes the client's own
flow table as proof of emptiness — which a lane carrying only downstream
traffic never satisfies on the client.

Three more defects the same review found, all of them in the machinery the
policy would have shipped on:

1. **`CarrierChoice::prefer` does not hold what it promises.** Moving to TCP
   sets `retry_at_ms` to `KCP_RETRY_COOLDOWN` (60 s) while the switch's own
   permission lasts `SWITCH_COOLDOWN` (120 s), so between the two the endpoint
   re-probes KCP, and a successful session calls `reached_kcp()` — which stores
   KCP and clears the cooldown. A *quality* decision can therefore be undone by
   a *reachability* event that says only "KCP came up", before the 120 s the
   decision was supposed to be held for. Reachability and quality are two
   decisions about one endpoint and must not spend each other's permission.
2. **A lane's carrier is recorded on the slot, not on the lane.**
   `MemberSlot.carrier` is overwritten by whoever takes the slot next, and the
   round-3 policy compares "lanes whose carrier is `chosen`" — so after one
   switch, the *old-carrier* lanes it still needs to migrate are invisible to
   it, and it cannot complete a handover that is already half done.
3. **A draining lane is not known to the peer.** `Routes::retire` marks a slot
   draining on this end only; the server keeps writing into the channel because
   its own flow entry still names that lane. The client's retire path is what
   the black-hole fix (above) made visible, and it is the same shape: a handover
   has to be agreed by both ends before a lane is allowed to end.

**Confirmed by a run, not by reading (2026-10-11).** One round of
`l3-budget10` / `udp-ladder` / `loss1_buf` (tunnel leg, baseline `490198c`,
binary `1083c5849745eb1b`, `MOLEHILL_L3_STATS=1 MOLEHILL_KCP_STATS=1`) with the
same claim read on both ends:

| | client | server |
|---|---|---|
| aggregate `forwarded` (last stats line) | **61** | **1 050 617** |
| aggregate `no_channel` | **0** | **909 766** |
| busiest slot of the loaded claim (`:2405 member=1`) | ≈0 | 9 039 (first line) and climbing |

So the direction *is* the one the round-3 note guessed — the client's per-slot
counters are four orders of magnitude below the server's on the loaded claim —
and the mechanism is now sharper than "the policy read the wrong direction":

- the client's slots are **live** and its `no_channel` is **zero** the whole
  run, so nothing about the *client's* enqueue path is failing — there is simply
  nothing being offered to it, because the traffic is handed to the lanes at the
  **server** end (it reads the visitor's packets off its own device);
- the丢 is on the server, and it is counted there: `no_channel` climbs to 909 766
  beside 1 050 617 forwarded, i.e. **the server's lane queues are refusing
  ~46 %** of what the device offers them, which is the ladder's 44.5 % inner-UDP
  loss at 2000M seen from the other end of the same path;
- the round-3 policy reads `dropped` from `no_channel` **at the client**, where
  it is 0 — so `drop_pct()` is `None` for every lane, every tick, and the policy
  cannot fire. Not because it was unlucky, but because the counter it reads
  lives on the end that is not dropping anything.

That also names what a fix cannot be: **a receive counter at the client is not
enough**. This workload's loss happens where the packets are *offered* (the
server's hub), and the client has no evidence of it at all — it only ever sees
what arrived. The two ends therefore have to report to each other: the offerer
counts what it offered and what its lane queues refused, the receiver counts
what it actually injected, and the policy compares carriers on the *pair*.

**What this supersedes.** "Add per-slot receive counters and compare delivery
rates" is not the next step: it measures the wrong thing (enqueue, not
delivery), has no lane identity to compare *across* carriers, and would be
driven by a preference switch that the reachability path can silently reverse.
The work is a per-lane identity, both ends' counters for it, and a two-sided
drain — in that order.

### The evidence chain, landed (2026-10-11)

What that ordered list produced, in the order it landed:

- **Lane identity** (`a3d0067`). `protocol::LaneId`, four fixed bytes, minted
  by the client and named in a transparent lane's prologue (8 bytes: service,
  then lane). The server learns the *width* from the registered service before
  it reads — the prologue is the first bytes on a direct channel, so nothing
  on it can say how long it is — and reads the lane half with `read_lane_half`.
  The hub records `LaneKey` (id + generation) per slot, and names the lane in
  the `l3-stats` line.
- **Per-lane counters** (`9fa82a1`). A lane now carries `offered` (counted
  before the answer), `enqueued`, and the two refusals *apart*: `overflow`
  (a full queue — this end's backpressure) and `disconnected` (no lane held
  the slot — a replacement window). Plus `injected`: what the lane delivered
  into this end's kernel, the one "delivery" in the set and the half its peer
  cannot see. The legacy `forwarded`/`no_channel` counters keep their meaning;
  they just no longer have to carry a cause they cannot express.
- **The archive.** A lane's counters are kept when it leaves, mirrored on the
  claim, and kept past the claim's own removal: a handover on a one-lane
  claim retires the only lane and empties the routing table in the same
  instant, so the evidence has to outlive the entry that held it.

Two defects in the *machinery* were fixed first, because a decision built on
top of them could not have been trusted whatever it decided
(`b3b2bba`, `2bad8b9`): a batched control frame was fed to the ARQ and
dropped — so a CLOSE arriving behind data was lost, which is the retired-lane
black hole reachable from an ordinary burst — and a 60 s reachability
re-probe could undo a 120 s quality decision, because `reached_kcp` stored
KCP and cleared the cooldown with it.

**Still missing, and the reason the policy cannot ship yet:** the counters are
local. The ladder's loss happens at the **server** (which offers the packets),
so the client — the only end that decides — still has no evidence of it. The
server has to report its per-lane counters over the existing independent
control channel, and a drain has to be agreed by both ends before a lane is
allowed to end (the client's own flow table cannot see a lane that is only
carrying traffic *towards* it). Both are the next slice.

### The report, and the pair it makes readable (2026-10-11)

The first of those two is landed (`0516d43`): the server attributes its
counters to the lane the client named (`5d79ab4`), and reports them
(`CarrierReport`, tag 5, fixed 17 bytes — service, lane, offered, refused,
sequence) once a second and only for lanes whose counters moved. The client
records each report **against the lane id**, on the hub that owns it, and
prints it beside its own view. Measured on the divergence cell, on the end
that decides:

| client slot | its own `injected` | `peer_offered` | `peer_refused` | refused share |
|---|---|---|---|---|
| `member=3` (lane 4) | 358 632 | 625 008 | 266 376 | **42.6 %** |
| `member=5` (lane 6) | 309 463 | 668 726 | 438 044 | **65.5 %** |

That is the evidence the policy needs, and the client's own counters — 1
forwarded, 0 `no_channel` — could never have produced it: on this workload
the deciding end's local view of those two lanes is empty by construction.
The pair also settles the mechanism the earlier round could only guess at:
the loss is the **peer's lane queues refusing**, not a missing lane
(`disconnected` is 0 throughout) and not the client's own scheduling.

**What is still missing before a policy can act:**

1. **A two-sided drain.** — **Landed (`51e92da`).** The dialer asks
   (`SessionCmd::DrainLane` / `CancelDrain`), the peer stops placing new flows
   in that lane and answers `ControlChannelCmd::LaneDrainAck` (tag 6, fixed 9
   bytes); its per-claim reporter sends "ready" once its own flows in the lane
   reach zero, and the dialer retires the lane only then. A drain nobody answers
   inside the budget is called off, so a lane is never left unusable.

   **The first run found the bug this design exists to prevent, from the other
   side.** The peer's ask set the *local* draining flag, which wakes its own
   lane task — so it tore the connection down before the dialer had its answer,
   and the dialer saw a dead lane instead of a reply: three "the peer answered
   ready but this end holds no such lane" in one run, with the lanes' `live`
   flipping to false between two stats lines. The peer's ask now sets a separate
   `peer_draining` mark that stops placement and nothing else.

   Measured with the diagnostic switch on the divergence cell: three lanes
   retired on the peer's confirmation, zero teardown races, zero unanswered
   drains, and the two lanes the peer still had flows in waited rather than
   being taken.

2. **The rule itself**, on the pair the report makes readable: a lane whose
   peer-side queue refuses a material share of what it is offered is a carrier
   that is not delivering, and the decision is which carrier to prefer for the
   next lane — never a running lane's flows, which must stay where they are.

   **Landed (`9c84a77`), with the arbitration it needed (`97755a2`).** The rule
   is `lanes::carrier_move` (pure, with the pre-registered constants), the
   reading and the action are `LanePool::carrier_step`, and a handover in flight
   now outranks a lend. It fires by itself on the divergence cell — "lane 4
   refused its share for 3 ticks; asked the peer to drain it so its replacement
   rides Tcp" — with no diagnostic hatch.

   **The first A/B of it is a `no claim`, and the reason is worth keeping.**
   Forced KCP (the policy disabled) against the policy live, three runs of
   `l3-budget10` / `udp-ladder` / `loss1_buf`, three rounds plus a warm-up with
   `--aa`, identical fingerprints: every difference was inside the floors. Two
   mechanisms explain it, and both are properties of the design rather than of
   the rule:

   - **An existing flow never moves.** A lane change cannot reorder a flow, so
     the flow that is being refused stays on its lane until it ends; only flows
     that *start* after the switch can ride the replacement. The ladder's cells
     are fresh flows, but the policy needs three ticks of evidence and the
     divergence cells (2000M/5000M) are the last ones — by the time it fires,
     the round is over.
   - **And until this round the drain could not complete at all**: three runs,
     three drains asked, three answered `pending`, none retired. Readiness was
     `flows == 0`, and the placement table remembers a flow for
     `FLOW_IDLE_EVICTION` (a minute) after its last packet, so a flow that
     stopped seconds ago held the lane for a minute — longer than the dialer's
     30 s budget, so every drain was called off. Readiness is now
     `active_flows == 0` (`DRAIN_QUIET`, 3 s) for a *peer's* drain only; the
     local retire and the lending path keep the stricter `flows == 0`.

   **Measured after the fix** (four rounds plus a warm-up, policy live), the
   whole chain in one run, to the millisecond:

   ```
   16:27:20.566773  Data carrier for 10.10.0.254:2400 switched from kcp to tcp
                    (the carrier in use kept refusing this claim's traffic at the peer)
   16:27:20.566801  Data carrier for claim 10.99.0.1:2405: lane 3 refused its share
                    for 3 ticks; asked the peer to drain it so its replacement rides Tcp
   16:27:23.567831  Lane 3 of claim 10.99.0.1:2405: the peer holds no flow in it;
                    retiring it (still carrying this end's flows)
   ```

   The 3.001 s between the ask and the retirement is exactly `DRAIN_QUIET`: the
   quiet window is what unblocked the handover, and the retirement waited for
   the peer's answer rather than taking the lane.

   **The second A/B is also a `no claim`, with a control that is now really
   inert, and it fails for a reason the workload cannot fix.** The first
   comparison's control was not a control at all: the forced-carrier arm still
   asked the peer to drain a lane once per round, because the rule ignored
   `MOLEHILL_DATA_CARRIER` (`prefer` refused the move, the drain went out
   anyway). That is fixed (`2250385`), and the comparison re-run on the `dev`
   profile — whose ladder has five cells of four seconds, so a switch early in a
   round has cells after it — is still a `no claim`: the only reading outside
   the floors is the policy's *cost* (`rss_peak_mib` +14.81 % on the 1000M cell,
   directional).

   The instrumented runs say why, and it is a property of the workload shape
   rather than of the rule. Per round: the policy fires, the carrier switches,
   the drain is asked — and the drain does **not** complete, with **no TCP lane
   ever carrying a packet**. A round is ~20 s; the rule needs three ticks of
   evidence (it fires ~16 s in), the peer then needs `DRAIN_QUIET` (3 s) of
   silence on that lane, and the round is over before the replacement can carry
   anything. The one completion measured so far (the four-round smoke run above)
   landed at the very end of its round, with no cell left to show a difference.

   So the benefit is **not demonstrated, and the current model cannot
   demonstrate it**: what it needs is a workload whose flows *start* after the
   switch, which means a round long enough to contain both the three-tick
   confirmation and several fresh flows afterwards (a 60 s+ cell, or a
   churn-shaped cell). Until that exists and is run, this policy is mechanism
   evidence only — and the honest options are the ones HANDOFF already commits
   to: price it in a release sweep once the model can, or revert it by
   discipline if it cannot.

   **What is still not demonstrated is the policy's *benefit*.** The mechanism
   is measured end to end; a `CLAIM` it is not. The A/B that can show one needs
   a workload whose flows *start* while the policy is already in force — a long
   campaign where round 1 switches and the later rounds' flows ride the
   replacement, or a churn-shaped cell — and that measurement has not been run.
   Until it is, this policy is shipped on mechanism evidence and stays on the
   list of things a release sweep must price.

   **The arbitration it needed is landed** (`97755a2`): `LaneLoad` carries
   `handover` and donor selection goes through `ClaimLoad::lendable()`, so a
   handover in flight is never spent on a lend — repair, then handover, then
   lending, in that order.

3. **L4's version** (a per-tunnel handover), which the report does not cover
   yet: a multiplexed tunnel's streams are not lanes, and the pool's repair
   dialer still pins the carrier a pool was established on. The design it needs,
   so the next round does not re-derive it:

   - **One logical pool, N physical tunnels, a carrier per tunnel.** `PoolEntry`
     gains its own `Carrier` and a `Serving`/`Draining`/`Dialing` state; the
     pool's single carrier becomes a summary the telemetry prints. Placement
     already reasons per tunnel (`order_candidates`, the per-tunnel ceiling, the
     round-robin tie-break), so it only has to skip a draining entry.
   - **The dialer takes a target.** `tunnel_dialer` builds one closure per
     carrier today; it becomes `dial(carrier)` and the repair tick calls it with
     the endpoint's preferred carrier, which is what lets a replacement ride the
     carrier the policy chose rather than the one the pool was born on.
   - **Draining a tunnel is not "streams == 0".** A pre-opened idle TCP channel
     and a UDP worker with no pinned peer can go at once; a tunnel whose streams
     are a visitor's live connections keeps them to the end (a stream cannot move
     without reordering it, exactly as a lane's flows cannot). `PinRegistry`
     already answers the UDP half (`pinned_on`), and `StreamLease` answers the
     rest.
   - **The count never grows.** The server's `max_tunnels_per_client` counts
     tunnels, so a replacement may only be dialed once the drained one has
     released its slot — `PoolEntry::_shutdown` dropping is that moment. A
     drain that cannot complete inside the budget is cancelled and the tunnel
     returns to service, the same rule the lane drain follows.
   - **The failure mode that motivates it, verified by reading the code and
     worth a regression test of its own**: the pool's carrier is part of its
     *identity* (`(session, carrier)`, "two pools wearing one name"), and
     `repair` calls the dialer closure that was built for that carrier at pool
     creation. So a pool established on KCP keeps repairing on KCP — and
     `CarrierChoice::on_kcp_failure` is only reached from the *initial*
     establishment (`establish_tunnels`) and from a direct L3 lane
     (`connect_direct_channel`), never from a failed tunnel repair. The
     consequence: **a path that carries KCP at startup and loses UDP later
     leaves the pool repairing into a dead carrier for the process's life**,
     serving on whatever tunnels survive, instead of following the fallback the
     client already knows how to make. The acceptance case for the slice is
     exactly that sequence: establish on KCP, block UDP, kill a tunnel, and the
     replacement must come up on TCP — with the pool's reported carrier
     following its tunnels rather than contradicting them.
   - **The edit list, with the anchors, so the slice is one pass and not a
     re-derivation** (verified against the tree at `41f152c`):
     1. `src/transport/multiplex.rs:429` — `Dialer` becomes
        `Arc<dyn Fn(Carrier) -> Pin<Box<dyn Future<Output = DialResult> + Send>> + Send + Sync>`:
        a dialer that is *told* which carrier to establish.
     2. `multiplex.rs:391` — `PoolEntry` gains `carrier: Carrier`, and
        `multiplex.rs:360` — `PoolSnapshot::tunnels` carries it per tunnel
        (`Vec<(usize, usize, usize, Carrier)>` or a small struct), because a pool
        whose tunnels ride two carriers must report that rather than the carrier
        it was born on.
     3. `multiplex.rs:655` — `with_dialer` gains
        `preferred: Arc<dyn Fn() -> Carrier + Send + Sync>` (the *decision*, not
        `CarrierChoice`: the pool must not learn about the carrier module), and
        `multiplex.rs:1241` — `grow_locked` dials `dial(self.inner.preferred())`
        and records what came back on the entry. That single line is the fix for
        the verified defect: a repair after a fallback comes up on TCP.
     4. `src/core/client.rs:1756` and `:1781` — the two `Arc::new(move || …)`
        closures must merge into **one** closure that takes `Carrier` and
        branches, reusing both bodies (build `kcp_dialer: Option<Dialer>` and
        `tcp_dialer: Dialer` first, then compose a dispatcher), and
        `client.rs:1709` — the establishment pass's `dials.spawn(async move {
        dial().await })` — must pass the carrier it is establishing, so the cold
        path keeps its current behaviour. **An attempt at steps 1–3 confirmed
        the rest of the list** (the pool compiles against a dialer that takes a
        carrier and a snapshot that carries it per tunnel, with every existing
        `with_dialer` caller unchanged through a delegating `with_preference`);
        it was reverted before step 4 because that merge, plus its call sites
        and tests, did not fit the session's remaining budget — deliberately, so
        the branch stayed green rather than half-migrated.
     5. The four test dialers in `multiplex.rs` (and any in `tests/pool_test.rs`)
        take the parameter and ignore it (`|_| …`), except the one that proves
        the slice: a dialer whose `preferred` flips to `Tcp` after the first
        failure, asserting the replacement is `Tcp` and the snapshot says so.
     6. **Placement skipping a draining entry is the *next* slice**, with the
        per-entry state (`Serving`/`Draining`/`Dialing`) — keep it out of this
        one so the diff stays about the carrier a repair rides.
   - **Evidence — settled, by inventorying what the client can actually see.**
     The local route was "decide from client-observable signals", and it is
     **not viable**, which the inventory shows rather than argues: the client's
     per-tunnel numbers are `streams`, `pending`, `pinned` and `is_alive` —
     *occupancy*, which is exactly what placement exists to equalise. There is no
     per-tunnel delivered-bytes, loss or RTT anywhere on this end (the bench
     model's `retrans_segs` is host-wide, read from `/proc/net/snmp`), so two
     tunnels can be compared only by how full they are, and a carrier judged on
     occupancy is the mistake this cycle already made once with `no_channel`.

     So L4's evidence has to be reported, like L3's: **extend `CarrierReport` to
     name a tunnel instead of a lane** (the report's shape already carries an
     identity, a pair of cumulative counters and a sequence, so the extension is
     a second identity kind rather than a new message). The server can attribute
     what it offered and what a tunnel's stream set refused, because a stream's
     prologue already names the tunnel it arrived on — the same fact the L3 path
     uses per lane. The wire line is unreleased v5, whose commands may be
     extended in place, so this costs no version bump.

     **The first slice's order, therefore**: the per-tunnel carrier and state in
     the pool (`PoolEntry`), the dialer that takes a target, and placement
     skipping a draining entry — all three testable without the report, with the
     UDP-blocked acceptance case above as their proof; then the report extension
     and the rule, in that order, so no rule is ever written against evidence
     that does not exist.

     **The report extension's prerequisite, corrected (round 24).** Round 23
     concluded that the tunnel id has to go into the hello frame — the tunnel id
     is a client-local counter (`next_tunnel_id`, `multiplex.rs:143`) and
     `Hello::DataChannelTunnelHello` carries none — and then found that the hello
     has no length prefix, so the field would have to go into all three variants
     (or the frame would have to learn a length). That is the expensive answer,
     and it is **not needed**: the tunnel id rides in the **stream prologue**,
     which is a separate path from the direct channel's and already exists.

     - The client writes a mux stream's prologue at `client.rs:1278`
       (`run_mux_data_channel`) and the server reads it at `server.rs:1829`
       (`route_tunnel_stream`) — neither touches the hello, the direct channels
       (`client.rs:1115` / `server.rs:822`, `:1948`) or `PACKET_LEN`.
     - The client knows which tunnel a stream belongs to (it dialed it, and
       `TunnelStream` already answers `tunnel_id()` for the pin accounting), so
       the stream can name it the same way a transparent lane names itself: four
       more fixed bytes after the service id, giving the 8-byte shape `LaneId`
       already uses.
     - The server then attributes what it offered, and what that tunnel's streams
       refused, with the identity it read — and `CarrierReport` carries it
       unchanged (an identity, a pair of cumulative counters, a sequence).

     So the second slice's first step is a prologue extension on the mux path
     alone, not a framing change — a far smaller diff, and one that cannot
     disturb the frame every connection is established with.

     **What L4's quality signal is — settled from the code, not by analogy
     (round 26).** L3's evidence is a *refusal* share: the hub's lane queue drops
     when it is full, and the peer can say how much it refused. **L4 has no such
     path.** A tunnel carries one yamux stream per visitor, and the server pairs
     the visitor with a stream through `copy_bidirectional_with_idle`
     (`server.rs:3191`) — a byte-copy loop with **backpressure, no drop**. A slow
     carrier therefore does not lose a tunnel's traffic; it makes the visitors on
     that tunnel slow, which is a *rate* symptom, not a loss percentage.

     So L4's evidence is **delivered bytes per tunnel per window** (optionally
     with write-blocked time), measured at both ends once the id travels with
     the stream: the server counts what it wrote into each tunnel, the client
     counts what it read out of each, and the pair gives the same "both ends
     report" property L3 has — without inventing a drop counter for a path that
     does not drop. `CarrierReport`'s shape still carries it (an identity, two
     cumulative counters, a sequence; the counters are bytes rather than
     packets/refusals, which the variant's own doc comment already frames as
     "what that lane's queue refused" — the L4 variant names bytes instead).

     **…and the both-ends part is not needed for the first cut (round 27).**
     Re-checking why L3 needed a report: there the *loss* happens on the peer's
     offering side, which the deciding end cannot see at all. **L4 has no such
     asymmetry.** A carrier that is slow shows up as a low per-tunnel rate, and
     backpressure makes both ends observe the same rate — so the client, which is
     the deciding end, can measure it locally: `TunnelStream::tunnel_id()` is
     already there at the point the stream is handed to the forwarder
     (`client.rs:1295`), and a counting adapter over that stream (bytes read and
     written, per tunnel) is a self-contained change with no wire and no server
     involvement.

     The confound — a low rate may be the visitor's application and not the
     carrier — is handled the way L3 handles it: compare **across carriers** on
     comparable tunnels, require a material and sustained gap, and never act on
     one window. So **the wire tunnel id is not on L4's critical path**: it stays
     landed (it is what will let the server attribute the same thing per tunnel
     later, and it cost one prologue extension), and the first cut is
     client-local: per-tunnel byte counters → the drain state in `PoolEntry` with
     placement skipping it → the rule.

     **And what the counter says on its first run (round 27).** `l4-mux2`,
     `bulk-1`, clean, `MOLEHILL_TUNNEL_STATS=1`, two lines one second apart:

     ```
     tunnel-stats: … 8:165/4 9:1157103422/0
     tunnel-stats: … 8:165/4 9:1614544702/0
     ```

     Tunnel 9 carried ~457 MB in that second (≈3.9 Gbit/s) — the whole flow.
     Tunnel 8 carried 165 bytes. Tunnels 0/2/3/6/7 carried **exactly one byte**
     each: the pre-opened spare channels. **One flow ⇒ one tunnel**, which is
     correct and is the same pinning a lane has (a flow cannot be split without
     reordering it), but it means **a cross-tunnel rate comparison has nothing to
     compare with on a single-flow workload** — the second tunnel is idle by
     construction, not because its carrier is worse.

     That is the L3 problem reappearing in the one place L4 cannot escape it:
     L3 got out of it by using the *offering* end's refusal counters (a loss
     signal that exists even for a single flow), and L4 has no loss signal — its
     degradation is a rate, and a rate needs a second subject. So the L4 design
     must pick one of:
     1. **comparable subjects**: require (or wait for) several concurrent flows
        spread across tunnels, then compare per-tunnel rates across carriers —
        the honest analogue of the lane rule, but it can only fire on multi-flow
        traffic; or
     2. **a before/after probe on one tunnel**: drain a tunnel to the other
        carrier and compare *its own* rate across the change — self-controlled,
        so it works with any number of flows, and it is what the handover already
        does.

     (2) looks like the one that fits how the pool actually behaves, and it is
     the shape to price next.

     **What the second slice is, then, and what it is not.** The *client* half of
     a handover is landed (`38e4dc2`: a repair rides the preferred carrier, and
     the pool reports what each tunnel actually rides). What is missing is the
     **trigger** — and a trigger needs the evidence above. So the second slice
     is: tunnel id in the hello → per-tunnel report → the drain state in
     `PoolEntry` with placement skipping it → the rule. Writing the drain state
     and the placement filter *first* would be a mechanism with nothing to
     decide it — the shape this cycle has already rejected twice.

     **The drain mechanism landed anyway, with the honest reason attached
     (round 29, `fe46bfb`).** The note above is half right and half wrong, and
     the round's first act was to find out which half by building it. What the
     note got wrong: the pool-level drain is not "a mechanism with nothing to
     decide it" in the sense the lane drain was, because its decision already
     exists — repair dials the preferred carrier, so the handover's *effect*
     is mechanical (old-carrier tunnels stop being replaced by themselves) and
     its *trigger* is the one thing left unwired. What it got right: no rule
     ships with this commit, and no speedup is claimed. What landed is exactly
     the acceptance shape the slice's own note pre-registered, without the
     trigger:

     - `PoolEntry.phase` (`Serving`/`Draining`) — a draining entry keeps its
       live streams (a stream cannot move without reordering it) and receives
       no new opens; `reserve` filters it out of the candidate order, and a
       pool left with one serving tunnel refuses a *new* handover rather than
       draining its last placement target.
     - One handover at a time, two bounded phases (`Draining` waits
       `SWITCH_COOLDOWN` for its streams to end on their own, then the entry
       returns to service; `Dialing` waits `COLD_GROW_WAIT` for the
       replacement, then releases the record so ordinary repair dials the live
       preference) — the lane drain's budget rule, at pool scale: a pool one
       tunnel short for ever is worse than the carrier the move was trying to
       leave. The replacement dial follows the handover's own target even
       before the endpoint's choice has moved (a diagnostic must not spend a
       quality decision's permission), and the record clears when a tunnel of
       the target carrier is in the pool.
     - The diagnostic hatch `MOLEHILL_TUNNEL_SWITCH_AT=<seconds>` fires one
       handover per pool, its `from` read from what the pool's tunnels
       actually ride (after a fallback that may not be the birth carrier), and
       the pool-stats line carries per-tunnel `phase`, a `handover` field and
       `-handover` in the size timeline. Pinned UDP peers block the retirement
       (D30), and a placement refusal inside the window is not reported as
       saturation.
     - Proven by `a_handover_excludes_new_streams_until_existing_lease_ends`:
       a held stream keeps its tunnel alive and un-dialed, new streams land on
       the serving tunnel only, dropping the lease lets the tick retire the
       drained tunnel and dial exactly one TCP replacement, and the pool ends
       at its configured count with a TCP member. Suites at this commit: 254
       lib (1 ignored), 11 pool, 23 integration, clippy ×2, all serial where
       serial is required.

     **What is still open, and what the next round starts from.** The trigger.
     Two honest options were recorded before this round and the mechanism
     keeps both reachable: (1) comparable subjects — require several
     concurrent flows spread across tunnels, compare per-tunnel rates across
     carriers; or (2) a before/after probe on one tunnel — the hatch above is
     the instrument for it, and the comparison is *the slot's* rate across the
     change, not the physical tunnel's (its id and its flow set both change),
     so the rule has to be written against pool-aggregate rates around the
     switch, never one tunnel's lifetime bytes. The human's standing caveat is
     on the record and applies to any rule built here: a policy's benefit is
     conditional on the path having degraded, so the A/B has to cover the
     degrade → switch → fresh-flows window, or it correctly reads "no claim"
     without saying anything about the rule.

     **The hatch's first real run found its own bugs, and the mechanism is now
     measured end to end (round 29, `596b15b`).** Three campaigns on
     `l4-mux8` / `bulk-n` / `loss1_buf` (tunnel leg, 3 rounds + warm-up,
     `--aa`, same binary on both sides, the only difference being
     `MOLEHILL_TUNNEL_SWITCH_AT=6`):

     - **The first run was a method defect, not a result.** The switch env
       never reached the daemons (launched without it) and nothing in the
       results file recorded that — the runner's comment claimed the record
       inherited `MOLEHILL_*` switches and nothing implemented it. Fixed
       alongside: the record now carries `molehill_switches` *beside* the
       method, not inside it — a declared treatment is what a between-file
       comparison contrasts (`--ab-arm`'s two binaries are the same shape),
       and putting it in the method would invalidate every stored method
       record. The L3-era switched pair had identical methods only because
       nothing recorded the env; that hole is closed.
     - **The first *switched* run churned**: the hatch's fired flag was set
       but never read, so the 50 ms tick re-requested a handover every tick —
       136–175 requests per measured round, 167 KCP repairs, zero TCP, all
       masked by the repaired line logging the pool's *birth* carrier. Two
       fixes in one commit: the flag is consumed by its own check (`swap`),
       and the repaired line names `wanted`, the carrier the replacement
       actually dialed.
     - **The corrected run, verified line by line**: six hatch requests per
       pool per round (one per pool — warm-up included), the tunnel marked
       `draining` in the timeline for 2.449 s (`waited_ms` in the drain line),
       retired, and its slot refilled by a repair dialing **`carrier="tcp"`**
       — six per round, zero KCP. No-disruption holds: the streams on the
       drained tunnel ran to the end; placement pressure inside the window did
       not report saturation.
     - **The verdict on effect: `no claim`, correctly.** Pooled over two
       matched pairs (5 clean base rounds each side, one twin each), the
       switched medians sit at +4.07 % (4.809 → 5.004 Gbit/s) — inside the
       control run's own A/A scatter (5.07 % on the off run's twin), so the
       model refused the claim twice, as it should. The structural signature
       is exact and consistent: `fds_peak` 100 → 110 in every switched round
       (the TCP tunnel's sockets), and one pair read `rss_peak_mib` −10.65 %
       (directional; single pair). A +4 % directional lean on a 1 %-loss
       single-flow cell is consistent with the carrier table (KCP ≈ TCP at
       1 Gbit/s, TCP ahead above it) but is not a claim at this profile.
     - **What a real claim needs, stated as method**: the switch's benefit is
       a *fresh-flows* effect — a long round where the switch lands early and
       the cells after it ride the replacement, exactly the shape the L3
       policy's A/B is waiting for. One instrument could serve both; until
       then, `loss1_buf` + `bulk-n` at smoke rounds reads the mechanism, not
       the policy.

     **The fresh-flows window finally ran — and the read is now about the
     policy's *scope*, not its existence (round 31, work dirs
     `bench-20261012-104917` / `-105603`).** With the churn instrument fixed
     (round 30), the A/B the two policies were waiting for is runnable:
     `l3-budget10`, dev profile, `--scenarios udp-ladder,churn-16` (ladder
     first each round so the policy fires; churn after it so its fresh flows
     ride the replacement), `loss1_buf` on the tunnel leg, 4 rounds +
     warm-up, `--aa`, same binary both sides — control pins KCP
     (`MOLEHILL_DATA_CARRIER=kcp`, recorded), candidate leaves the policy
     live. Fingerprint identical; both files carry their switches.

     - **The whole chain fired in every candidate round**: switch → ask →
       retire, one lane per round (lane 3 of the loaded claim `:2405` in
       rounds 1–2, lane 4 in 3–4 — the rule moves the lane the evidence
       names, not a fixed one).
     - **Churn after the switch is instrument-healthy and level**: rates
       305.7–310.0/s (candidate) against 304.9–307.4/s (control), medians
       +1.11 % with the A/A twins at 1.9–2.4 % spread, `connect_errors` 0
       everywhere — the fixed instrument's first policy run produced real
       numbers on both sides.
     - **The ladder's divergence cells stayed level** (2000M: ~40 % loss both
       sides; 5000M: ~62 % both), where the earlier *diagnostic hatch* run
       read +78 %/+194 %. The difference is scope, and it is the finding:
       the hatch moved **every** claim's busiest lane; the automatic policy,
       acting on its own evidence, moved **one lane of one claim** — 1 of
       `l3-budget10`'s 10 lanes — so ~10 % of the path upgraded and the
       ladder could not see it. On the cells where the carriers are level
       (1000M: 0.03–0.34 % loss both sides) nothing moved, which is the
       conservative behaviour the policy was built to have.
     - **What this leaves**: the policy is proven to fire on evidence, move
       exactly what it proved, and cost nothing where the carriers are
       level; its benefit at full scope is a *scale* question — per-claim
       policy reach (every loaded lane, not one per round) or a workload
       narrow enough that one lane is the path (the single-claim shape
       `l3` measures). Neither is a defect: the conservative scope is what
       keeps a wrong decision cheap. The A/B artifacts live outside the
       tree (`~/tmp/churn-policy-{off,on}.json`), as evidence rather than
       published numbers.

     **The single-claim A/B exposed two real defects in the evidence chain,
     and fixing them made the handover complete (round 32, `a9ff0fa`).** The
     same A/B on the `l3` arm (one claim, one lane — the shape where the
     policy's move *is* the whole path) fired on the wrong claim and never
     finished a handover, and both failures were proven by instrumented runs,
     not review:

     - **Carrier reports were matched by lane id alone; every claim mints its
       lane ids from 1 in one shared hub.** With `MOLEHILL_L3_STATS=1` the
       ladder claim's counters sat at zero while an idle sibling read
       `peer_offered=1 000 088 / peer_refused=268 964` — the report for
       :2405's lane 1 landed on :2404's lane 1 (iteration order, not
       attribution), and the policy had been judging evidence attached to the
       wrong claim. Fixed: the client resolves the reporting service's
       claimed endpoint and the hub matches claim first, lane second. The
       regression test `a_report_lands_on_the_claim_that_owns_the_lane`
       pins the collision (two claims, both lane id 1) that the old lookup
       resolved silently.
     - **The lane's own retirement waited for an empty flow table; the table
       holds a flow for 60 s of silence.** The drain ask marked the lane
       draining at 12:00:49 and its counters froze (`injected` stuck at
       731 124 to end of round), while the stats line kept `flows=6` —
       `should_retire` required `entry.flows == 0`, and the ladder's flows
       would not have been evicted until 12:01:49, after the round. The
       replacement never joined; every cell read level because nothing had
       moved. Fixed: the lane's own retire now uses the quiet window the
       peer's drain already answers (`DRAIN_QUIET`, 3 s), which is what
       makes a handover complete inside a round.
     - **Verified end to end after both fixes** (`l3`, `loss1_buf`, ladder
       first, one attributed round): the policy names the loaded claim
       (:2405) every round, the KCP lane retires ~5 s after the ask, and the
       replacement joins riding **TCP** mid-round — lane 2, `carrier=tcp`,
       live reports flowing. The effect on the affected cell: the 5 Gbit/s
       ladder cell, held at **67 % loss / ~1.03 Gbit/s** on KCP across four
       control rounds, read **10.7 % loss / 3.0 Gbit/s** and **5.1 % / 3.2
       Gbit/s** with the handover complete — the top cell finally rides the
       carrier the evidence chose. The 2 Gbit/s cell, which the switch (16 s
       in) had not yet reached, stayed at ~43 % exactly as the held-carrier
       control predicted — the A/B's own internal control.
     - **What this changes about the earlier reads**: the round-31
       `l3-budget10` campaign's level divergence cells now have a mechanism
       (reports landing on the wrong claim) *and* an incomplete handover
       (the same 60 s wait); the round-31 verdict "scope, not existence"
       stands for that run, but its numbers are not evidence of the policy's
       ceiling. The next policy A/B, on the fixed chain, is the first one
       that can read the policy's real effect.

     **The fixed chain's first full policy A/B — and the claim the whole thread
     was building toward (round 33, work dirs `bench-20261012-122119` /
     `-122736`).** `l3`, dev, `udp-ladder,churn-16`, `loss1_buf` on the tunnel
     leg, 4 rounds + warm-up, `--aa`, same binary; control pins KCP (recorded),
     candidate leaves the policy live. The policy fired in **every** candidate
     round on the loaded claim (:2405), and the handover completed inside the
     round — the two defects the round-32 fixes removed are gone.

     The result is the honest shape a progressive handover produces, and the
     A/B's own timestamps prove the mechanism:

     | round | 5 Gbit/s cell done at | lane retired at | cell read |
     |---|---|---|---|
     | 1 | 12:29:54 | 12:29:58 | 65.9 % / 1041 Mbit/s (missed — cell ran first) |
     | **2** | 12:30:36 | **12:30:32** | **13.7 % / 2910 Mbit/s (captured)** |
     | 3 | 12:32:29 | 12:32:33 | 65.1 % / 1052 (missed) |
     | **4** | 12:33:11 | **12:33:07** | **7.4 % / 3080 (captured)** |

     The cell moves **only** in the rounds whose handover completed before the
     cell began — 3x the delivery at a fraction of the loss, in exactly the
     rounds the timestamps name, and untouched where it did not. That is the
     internal control proving the movement is the handover's and nothing
     else's. Churn after the switch: level (304.2-308.6 against
     304.4-308.3/s), the fresh flows pay nothing. One claim, one lane, the
     whole claim moved: **the policy's effect is now measured, not just its
     mechanism.**

     The cost is stated beside it: `syscalls_per_s` +31.37 % on the 5 Gbit/s
     cell is a **claim by the model's own floors** (threshold 30.05, n=[4,4],
     every round agreeing) — TCP's per-datagram work against KCP's batched
     send, paid while delivering 3x. `cpu_s_per_gbit` in the captured rounds
     is actually *better* (0.499/0.527 against 0.574-0.606): carrying 3x on a
     carrier whose per-byte cost is lower. `cpu_cores` +45.66 % is directional
     (its floor was 43.91), the same shape seen at the carrier axis.

     What this closes, and what it opens: the carrier-quality chain — per-lane
     identity, both ends' counters, two-sided drain, the rule, and now a
     measured effect — is done as designed; the L4 pool half follows the same
     pattern and its mechanism is verified, its trigger still unwired. The
     round-31 and round-32 instruments' earlier "no claim" reads are
     superseded by this one; the next release sweep prices the policy at its
     published scope.

     **The L4 pool's acceptance case is now a test, and it forced two hatch
     fixes (round 34, `7063148`).** The design notes' acceptance sequence —
     live flows unharmed while the pool hands a tunnel over — existed only as
     a unit test on pool internals; it is now an integration scenario on a
     real server/client pair (`a_live_pool_hands_a_tunnel_over_without_
     dropping_its_flows`, `#[ignore]`d — see below): a two-tunnel pool
     carrying a held visitor stream, the hatch excluding one tunnel at three
     seconds, and the assertions being the held stream echoing **after** the
     handover and again after the replacement joins (a flow is never moved,
     so it must not notice), a tunnel riding the other carrier with the
     configured count intact (the replacement is dialed only after the
     drained tunnel has released its slot, so the server's valve never sees
     an overlap), and the handover record cleared.

     Writing it forced two product fixes, both found by the test failing in
     the suite while passing alone:

     - **The hatch was read once per process**, not per pool: whichever pool
       was created first consumed `MOLEHILL_TUNNEL_SWITCH_AT` for every pool
       created after it. The switch is documented as "that many seconds after
       the pool starts", so each pool now reads it at its own creation.
       *(Round 36: incomplete — the call moved to pool creation but the
       process-wide `OnceLock` stayed inside the reader; see below.)*
     - **The hatch was spent even when no handover started**: a tick that
       found no eligible candidate (a visitor mid-open, both tunnels busy)
       burned the flag, and the pool never tried again. It is now spent only
       when a handover actually starts; a transient no-candidate tick leaves
       it armed.

     **The scenario is `#[ignore]`d under the timebox rule, and the reason is
     precise**: it passes alone and in pairs, but as the suite's second test
     it timed out four times with the pool healthy (two tunnels, all Kcp,
     `grows: 0`, `handover: None`) — the hatch never fired, though the env
     var is asserted present at the wait. The suspect is suite-context
     interference on the pool's maintenance task, not yet pinned down; the
     next round starts there (a maintenance task that never runs for a pool
     created late in a process would be a real defect beyond the hatch).
     Until then the scenario runs alone:
     `cargo test --test pool_test a_live_pool_hands -- --test-threads=1 --ignored`.
     Suites at this commit: 254 lib, 11 pool + 1 ignored, 23 integration,
     clippy x2, all green.

     **Round 36: the ignore is lifted — the defect was real, and it was not
     suite-context interference.** The failing snapshot (pool healthy,
     `handover: None`, env asserted present) pointed away from the
     maintenance task, and re-reading the round-29 fix showed why: the
     per-pool call had been moved to `with_preference`, but the reader still
     cached its answer in a **process-wide `OnceLock`**
     (`diagnostic_handover_at()`). The suite's first pool is created with
     `MOLEHILL_TUNNEL_SWITCH_AT` unset, so the cache pinned `None` for the
     whole process — the log shows ten pools established before the handover
     scenario's (`service:0:127.0.0.1:41045`, line 48 of the captured run),
     all of them told `hatch_at: None`. Alone, the scenario's pool is the
     process's first, so the hatch always fired: the exact
     alone-vs-suite split, without any interference. It was a product defect,
     not a test artifact — any process that reconfigures the hatch across
     pools (or reads it at all after a first pool) got the first pool's
     answer.

     The fix: the reader parses the environment **per pool** again (the
     cache is gone), and an invalid value is reported through
     `logging::RepeatNotice` — loud once per process, `DEBUG` after —
     instead of a `warn!` that pool creation could repeat. The scenario now
     runs as part of the serial suite: `cargo test --test pool_test --
     --test-threads=1 --include-ignored` is **12 passed, 0 failed** (three
     runs: the failing reproduction, then two greens), the alone run still
     passes (3.26 s), and the doc comment's stale ignore story is replaced
     by the per-pool-read note. Suites at this commit: 254 lib, 12 pool, 23
     integration, clippy x2, all green.

The round-3 policy in `src/transparent/lanes.rs` (still uncommitted) is
**not** the basis for any of this: it reads `no_channel` as carrier loss, has
no lane identity, and cannot see the report. It is superseded by the pair
above and should be rewritten against it rather than adapted.

That policy has since been reverted out of the worktree, so the branch is
clean; it is preserved **outside the repository** as
`~/tmp/p0/pending-strategy-round3.patch` (and the four files as they stood in
`~/tmp/p0/`). Its lending allocator (`plan`) was never part of that diff and
is still in `lanes.rs` at HEAD.

Build gaps fixed on the way (all pre-existing — the baseline does not compile
`transparent` without `multiplex`): the lane allocator, the carrier policy
and the direct-channel carrier reader are gated by the features that define
them. A `noise`-without-`multiplex` combination still fails on an unrelated
`NoiseConfig` import; it fails identically on `490198c` and is not this
cycle's.

**What the new counters say on the divergence cell** (one round,
`l3-budget10` / `udp-ladder` / `loss1_buf`, tunnel leg, `9fa82a1`, binary
`026a526622d8f9a2`; 5000M cell: 61.2 % inner loss, 1055.7 Mbit/s delivered):

| server slot of the loaded claim | `offered` | `enqueued` | `overflow` | `disconnected` |
|---|---|---|---|---|
| `member=3` | 623 296 | 349 366 | **273 930** | **0** |
| `member=4` | 941 626 | 331 224 | **610 402** | **0** |
| `member=1` | 375 127 | 374 922 | 205 | 0 |

So the loss is **this end's lane queues refusing packets**, and it is not a
single even failure: two lanes of one claim refused 44 % and 65 % of what the
device offered them while a third, carrying the same claim, refused 0.05 %.
`disconnected` is 0 throughout, i.e. no lane was missing — the round-3
policy's counter would have reported "the carrier is fine" for every lane
that had a lane in its slot, and "the carrier lost everything" for none.

That is the shape the policy has to react to, and it is why the counters had
to be split before anything could be decided on them: a per-lane queue
overflow under load is a *carrier* property (a lane that cannot take data
fast enough is a lane whose carrier is not delivering), while an empty slot
is this end's own scheduling and says nothing about the path.

Note `lane=-` on the server: the receiving end does not know the lane id its
peer named, because the server's lane prologue is read but not yet handed to
the hub. That is the next slice — the server has to attribute its counters to
the lane it read, and report them, or the deciding end still has no evidence
of the only place where the loss happens.

### KCP, first thread: the datagram-size collapse does not reproduce (2026-10-11)

The open item carried since the MTU work — "`bulk-n` after `bulk-1` collapses the
datagram size from 6.2 KB to 3.4 KB, with a repro command" — is **falsified on
`41f4005`**, on the KCP path as the product now ships it:

```
sudo -n uv run benches/scripts/bench/bench.py run --profile smoke --arms l3-kcp \
  --scenarios bulk-1,bulk-n --condition clean --link-mtu 9000 --tun-mtu 8000 \
  --rounds 1 --warmup-rounds 0
```

Both scenarios share one daemon pair and one arm, so the ordering the finding
described is the ordering this run had. Result: `bulk-1` 6.836 Gbit/s and
`bulk-n` 6.874 Gbit/s (one round each; a smoke run, so the number is a control
point and not a claim), and the per-session adaptation log — read with
`RUST_LOG=debug`, since the once-per-process `INFO` line is the log contract's
limit — shows **exactly five adaptations, all at establishment**
(`1400 -> 8000`, one per lane), and none afterwards: a *decrease* would have
logged, because the probe only reports a change. The datagram size is therefore
stable across the two workloads, and there is nothing to fix here.

What the thread leaves behind is the observation that the size is worth seeing
per run: `MOLEHILL_KCP_STATS=1` carries no size field, so the diagnosis above
needs `RUST_LOG=debug`. If a future size question comes up, add the field there
rather than promoting the per-session line back to `INFO` (the log-budget test
fails a healthy default-level run that repeats one message shape).

### The carrier migration (2026-10-11): KCP first, TCP as its fallback

Landed in one commit because the halves are one change: removing the carrier key
while TCP was still the default would have left KCP unconfigurable, and making
KCP the default without a fallback would have broken every UDP-blocked
deployment. What the work found on the way, in the order it was found:

- **The wire never had to change.** The server serves both carriers per
  connection and only *opens* the KCP listener when a registration declares it,
  so the fallback needs no protocol work: the client declares the carrier it
  will try first and dials either one.
- **The server bound a KCP listener per registration rather than per address,
  and the socket outlived its server.** With KCP as the default this surfaced in
  the integration suite as `Address already in use` — a session holds the shared
  state, which held the acceptor, so the UDP port stayed bound while sessions
  drained. The acceptor is now held **weakly** by that state and strongly only
  by the listener task, so the port closes with the server (`src/core/server.rs`).
- **The suite's fixtures shared a data port** (`data_plane_separate.toml` and
  `per_service_tunnels.toml` both on 2344) — invisible while only TCP-declaring
  clients bound anything, fatal once every server does. One fixture moved.
- **A healthy run's log budget broke**: the KCP path reported its path-MTU
  adaptation once per session at `INFO`, and the budget test fails a run that
  repeats one message shape. It reports once and then at `DEBUG` now.
- **A test that needs a specific carrier can no longer write one**, which is
  what `MOLEHILL_DATA_CARRIER` is for: `pool_test`'s spawned scenarios (the
  operator's valve, the pool's own lines) force TCP.
- **The model's carrier axis followed the key out.** An arm cannot name a
  carrier; the contrast is a *path* condition. `--condition udp_blocked
  --condition-leg tunnel` installs an `iptables -j DROP` for UDP on the leg the
  carrier crosses, so the KCP attempt cannot be established and the run measures
  the fallback — verified end to end (`l4`, `bulk-1`): the client logged
  `KCP could not be established to 10.10.0.254:2400 (KCP establishment
  timed out); using TCP for it and retrying KCP in 60s` and every pool
  established with `carrier="tcp"`. The condition is *refused* on any other leg:
  on the visitor's leg it drops the payload and the carrier comes up as usual,
  which is a run whose method says "fallback" while every packet rode KCP — the
  first attempt at this measurement did exactly that and was caught by the
  pool-stats line, not by the number. Isolating one carrier is now the inherited
  `MOLEHILL_DATA_CARRIER` switch, recorded as instrumentation.

Known and deliberate: with the carrier forced there is **no** fallback — a
failing KCP run fails rather than quietly measuring TCP — and an operator who
wants TCP always pays one bounded attempt (2 s) per endpoint per cooldown before
the fallback engages. Both are properties of the state machine rather than
configuration, and both are stated in `docs/configuration.md`.

**All of L12–L18 is landed and measured**, and the two open questions this
section used to carry are answered: lending is a `CLAIM` at +26.75 % when the
carrier is the bottleneck and indistinguishable on a clean path (above), and the
mux×1 gap is priced as the engine's per-byte work rather than a fixable defect
(above). What is left is follow-up work, in the order it is worth picking up:

1. **The L3 arms in a routine run.** The default `l3` arm holds one lane per
   claim, so it exercises the *product default* and cannot show lending at all;
   anything about lanes should name `l3-budget10-lending` and
   `l3-budget10-nolend` (the pair the model now compares itself). If a future
   sweep should carry the axis rather than an on-demand A/B, that is the pair to
   add — and the cost is real: each condition needs the long-haul leg to say
   anything at all.
2. **The L3 path's own ceiling (M, above).** The measurement said the TUN path
   is per-packet bound and the lever is the packet size (the MTU), not a
   compressor and not more lanes. Nothing there has changed; the L3 acceptance
   harness measures the wire at whatever MTU the operator sets.
3. **The gap's kernel half.** `perf` cannot name kernel symbols in this
   container, so ~35 % of the mux profile is an unattributed number. On a host
   where `/proc/kallsyms` is readable, `gap_profile.sh` would name it — worth one
   run before anyone proposes work on the kernel-side copies.
4. **`docs/benchmarks.md`'s history.** The measurement records still name the
   arms their runs used (`l3-mux`, `l3-raw4`, `l4-mux1`, …). That is deliberate —
   they are published history and the page's note maps them to today's catalog —
   but a future release sweep should add today's names rather than renumber the
   old ones.

### Per-byte or per-datagram: the two-size fit (2026-10-11, round 19)

The split said the send half owns the phase; the next question is what that half
charges *for*, and one run answers it without touching code — the same clean
`bulk-1` cell at a jumbo MTU, against the 1472-byte reading above:

| datagram size | `ms_out_send` per outbound datagram |
|---|---|
| 1472 B | **3.85 µs** |
| 8000 B | **5.25 µs** |

Fitting `cost = a + b·size` over the two points gives **a ≈ 3.5 µs fixed per
datagram** and b ≈ 0.21 ns per byte — so at 1472 bytes the *fixed* term is 92 %
of the cost, and the per-byte term is nearly noise. That is the opposite of a
copy-bound shape and two orders of magnitude above per-datagram syscall
amortisation inside a 32-message `sendmmsg`, so what it prices is the per-message
work below our staging: the kernel's UDP send path per datagram, plus whatever
per-span setup our side does.

**What that closes, and what it leaves.** Three measured attempts now agree that
this path's cost is not where the obvious code-level candidates were: the
inbound copy removal was a cost (RSS), gating the diagnostics was noise, and the
pacer is 1.7 % of the phase. The lever the numbers do point at is **datagram
count**, which is the TUN/link MTU — an operator setting, and one this
repository already measured at +84 % throughput and a third of the CPU per byte
at 8000 ([deployment.md](docs/deployment.md#transparent-services)). The
remaining code-level question — how much of the 3.5 µs is ours and how much is
the kernel's — is answerable by comparing the pump's `ms_out_send` against the
process's own system CPU time; if they match, the answer is "kernel", and no
change on this path will pay.

### And the split says which half (2026-10-11, round 18)

The measurement the note above asked for, taken on the same cell (two
consecutive stat lines one second apart, server side):

| half of the output phase | ms in that second | share |
|---|---|---|
| **`ms_out_send`** (the batched wire send) | **+729.3** | **98 %** |
| `ms_out_pace` (the pacer's per-span decision) | +12.6 | 1.7 % |

with `datagrams_out` +189 396 in the same second — so **≈3.85 µs per outbound
datagram is spent in the send half**, and the pacer costs ~0.07 µs per datagram.
At up to 32 datagrams per batch that is ≈123 µs per `sendmmsg`, one to two
orders of magnitude above what handing 32 iovecs to the kernel should cost.

**The pacer is exonerated, and the search moves into `send_spans` and the
`udp_batch` send path beneath it** — where the remaining candidates are the
per-batch iovec/msghdr construction and whatever the split-span path does with
a header plus a payload reference (a copy there would be ~276 MB/s of memcpy at
this rate, which is real but does not by itself explain 729 ms). The two new
fields are on the stats line (`ms_out_pace`, `ms_out_send`) so the next attempt
starts from this split rather than from the phase total.

## The v0.10.0 release, as it was recorded (archived 2026-10-16)

Moved from [HANDOFF.md](HANDOFF.md) when v0.11.0 was prepared; the
rules at the top of this page apply.

- **v0.10.0 shipped on 2026-10-01**: the release run is green on all thirteen
  jobs (nine platform builds, GitHub Release, GHCR, crates.io), and the
  released artifact is `benches/records/results-soak-v0.10.0.json` — measured
  at `96bda00` with the model the bench model replaced, and gated by the
  sweep's own check with no waiver. That schema is history now: the next
  release publishes `results-bench-vX.Y.Z.json` and gates it with
  `just bench-gate`.
- **The release cost four tag pushes, and the three failures are worth
  remembering**: the emulated cross legs (`native_target` guard missing on a
  new spawn-based test), the musl leg (an assertion pinned to glibc's wording),
  and the `aarch64` cross leg (the release matrix's cache shared across legs
  with different glibc). The first two are in `tests/` — which forced the sweep
  to be re-run on the commit that carries them — and the third is in
  `release.yml`. All three are fixed; the open thread at the bottom of this
  page proposes the check that would catch the next one mechanically.

## The L3 cycle's "next, in order" list, as it closed (archived 2026-10-16)

Moved from [HANDOFF.md](HANDOFF.md) when v0.11.0 was prepared. Every item on
that list is closed — five struck-through ones and the reasoning that closed
them — and the live list is "Open threads for the next cycle" on the front
page. The rules at the top of this page apply.


1. ~~**C — the `[transparent]` run mode and its config model.**~~ **Done**: the
   `[transparent]` block, `[transparent.claims.<name>]`, `RunMode::Transparent`
   with `--transparent`, the `[client.transparent]` and
   `protocol = "transparent"` redirects, and the nine per-key refusals deleted
   (those keys have no home in the schema). The lowering is a front end: the L3
   block becomes the same `ClientConfig` the forwarding path already runs on, so
   `core/client.rs`, the data path and the wire were not touched. Validation
   takes the model as an argument (`ClientModel`) rather than being forked, and
   its messages follow it — a claim is a *claim*, and the block named is
   `[transparent.control]`, not `[client.control]`.
2. ~~**M — measure the L3 path before writing a compressor.**~~ **Done**
   (2026-10-09, `9507a7a`): the harness samples `/proc/net/dev` per arm and
   `benches/scripts/l3/wire_report.py` reports carried packet sizes and a
   header-compression ceiling. Campaign in `docs/benchmarks.md`, "The
   transparent-L3 wire question".
3. ~~**Z — only if M passes.**~~ **Not built: the measurement says no.** The
   ceiling on the workload compression exists for (2000 round trips of 64 B) is
   **17.9 % of the wire** — under the 20 % bar written before the run, and an
   upper bound before exemptions. The same table says why: 196 B of wire per
   116 B carried packet, i.e. ~80 B of tunnel transport per packet (our 2-byte
   length, the multiplexer's frame, the carrier's TCP/IP header and its ACKs)
   against ~35 B of compressible header. **Framing is the lever, not headers**;
   batching several packets per frame is the next candidate and needs its own
   measurement.
4. ~~**V — the verdict, before any `v0.11` tag.**~~ **Recorded above, and it is
   a no**: nothing was added to the wire, so there is nothing to remove and no
   v6 risk.
5. **Done after M: the L3 data path was optimised where the measurement pointed.**
   Batching (drain the device, frame each packet where it is read, hand a run of
   frames over as one write) took 12 % off the bulk arm's wire bytes; the L3
   model's `[transparent.data].default_mode` is now `direct` (a claim has one
   channel, so the multiplexer was pure per-packet cost), worth another 6 % of
   the wire, a third of the CPU per packet and 65 % more small round trips per
   second. Numbers and method in `docs/benchmarks.md`, "The transparent-L3 wire
   question". What the same instrument now says is that **CPU, not the wire, is
   the next lever** (~135 µs of daemon time per carried packet on the small
   arm), and that it **cannot yet answer the many-flow case**: every arm is one
   connection, so parallel TUN queues (`IFF_MULTI_QUEUE`, one reader per queue)
   are a candidate that needs a concurrent-flow arm before it can be claimed
   either way. That is the next measurement, not the next feature.

   **Both of those are now answered, and neither is the next thing to build.**
   The harness gained a multi-flow arm *and a control beside it* (the same flows
   with the tunnel out of the path): at 16 connections the tunnel carries
   19 900 round trips/s against the control's 23 750 (84 % of a ceiling that is
   the probe's own), at 64 connections 27 700 against 30 900 (90 %). So parallel
   TUN queues have no measured need — the device reader is not what limits a
   busy host, and a control is what makes that statement checkable instead of
   plausible. Bulk is the arm where the path *is* the limit (338 Mbit/s against
   the control's 602), and it moved a long way by batching the read side as well
   as the write side (+27 % throughput, −56 % CPU per packet).

   **The remaining levers were then attributed, and none of them is a code
   change.** Two instrument errors had to be fixed first, and both are worth
   remembering: the bulk arm sent one blob and read afterwards, so any run past
   the buffers measured a *deadlock* (two runs of 20 MB and 100 MB were reported
   as throughput when they were five-second stalls), and the harness built
   **debug** while the product ships release — the same arm measures 783 Mbit/s
   in debug and 1986 Mbit/s in release. Both are fixed (`BULK_CHUNK`,
   `PROFILE_BUILD=release`), and the numbers below are release.

   Release, one host, with the no-tunnel control beside each arm: bulk 200 MB in
   65 KB chunks **1986 Mbit/s** against the control's 12 024; a paced single flow
   **10 362 round trips/s**; 16 flows **22 543** against the control's 23 388 —
   **96 % of a ceiling that is python's, not the path's**.

   A `perf` profile of the release build during the bulk arm puts **84 % of the
   CPU in the kernel**, 10 % in molehill, 5 % in libc, with no symbol above 11 %:
   the cost is four syscalls per packet (TUN read, socket write, socket read, TUN
   write), spread thin. That retires the synchronous data path as a plan — it
   would chase the userspace tenth, not the kernel's five sixths — and it names
   the lever that does work: **packet size**. The same bulk bytes in 8000-byte
   packets instead of 1400 measured **3579 Mbit/s** at half the CPU per byte, so
   the TUN MTU (an operator setting, with a link MTU to match) is worth more than
   any remaining code change, and the deployment recipes now say so.
6. **One model, and it owns the sweep too (2026-10-10, unreleased).**
   `benches/scripts/bench/` is now the only runner: the soak sweep, the
   L3-versus-L4 comparison, the memory sampler, the HTTP latency script and the
   peer adapters are all *this* model — its profiles, its scenarios, its arms.
   What it grew in the migration:

   - **peer arms** (`peers.py`): frp, rathole and nps are arms like any other,
     configured per topology, with their release binaries fetched by
     `just bench-peers` and their versions recorded in the results meta. The
     first end-to-end peer run (one condition, `cost`, 60 s) read frp at
     6.08 Gbit/s and molehill at 13.58 Gbit/s, with the p99 and CPU-cost
     metrics beside them — the four-tool comparison, on the model's terms.
   - **conditions and timelines** (`model.CONDITIONS`, `model.TIMELINES`):
     `clean`, `rtt100`, `loss1`, `loss5`, `rate100`, `rate20`, `jitter` and the
     two MTU classes, imposed *in place* on a veth leg (`tc qdisc replace`), so
     the tool keeps running while the path changes. `--scale` shortens a
     timeline's holds without changing its shape, and the scale is part of the
     fingerprint.
   - **the staged scenario** (`timeline`, `soak`, `cost`): a bulk spine
     re-dialed per stage after a drain predicate (queue below a tolerance and
     no send-capable socket, held for two polls, with a budget that is recorded
     when it fires), three continuous series probes (interactive, datagram,
     churn) sliced per stage, a drift sampler (RSS/fds/threads) across the
     whole run, and wedge detection (>5 s of silence).
   - **the capacity ramp** and **reconnect**: one cell per stream level with a
     fresh-connection ping beside it and the SLO verdict per level, and five
     timed cold starts.
   - **`plot.py`**: the six figures (timeline, stages, capacity, UDP, drift,
     cost) and five markdown tables, rendered from the new schema, each figure
     carrying its method in the footer.
   - **`bench.py gate`**: coverage of every declared cell, the endpoint
     invariant, the SLO on the clean stages (the product's arms only — a peer
     that misses it is reported, never blocked), the drift and wedge axes (a
     slope is only judged past five minutes), the capacity ramp, and the
     regression half against a baseline.

   The retired tree (`benches/scripts/soak/`) is deleted; its published records
   moved to `benches/records/` as history, and the release ritual now names
   `results-bench-vX.Y.Z.json` + `assets/bench-vX.Y.Z*.png` and gates with
   `just bench-gate`. The model is self-contained: the iperf3 conventions
   (`iperf.py`), the provenance probes (`hostinfo.py`) and the peer adapters
   all live in it, with no import from the retired sweep.

   Not migrated, deliberately, with the reason: the **slow visitor** probe
   (opt-in head-of-line instrument; the model's UDP and TCP shapes cover the
   load shapes, and a HOL question would come back as a scenario), and
   `udp_stress.py` (it reads molehill's *internal* UDP queue counters to answer
   a molehill-only design question, which the model's externally-observable
   rule forbids in a comparison — it stays as a diagnostic, outside the model).

   What is still open: the sweep has not been run end to end on the migrated
   engine (the pieces are validated: a staged timeline over three arms, the
   capacity ramp with the SLO per level, a peer arm, the drift and wedge
   metrics, the figures from a real staged file), so the first release sweep is
   also its acceptance test. The 5 Gbit/s UDP ladder cell and the loopback
   ceiling remain the two places where a single run cannot order the arms.

7. **The carrier axis and the L3 data path, measured (2026-10-10, unreleased).**
   The hypothesis on the table was "L3 + KCP is better overall, and the
   multiplexer should not cost much". The model answered both, and the answers
   are *no* and *it costs 28 %* — with one exception each way, and with the
   real L3 lever showing up in the same campaign.

   **The carrier is now independent of the mode** (`08e72aa`): a `direct`
   service or claim can ride KCP — one session per channel, no yamux above it —
   where the config used to refuse the pair. The server's KCP listener reads the
   data-channel hello beside the tunnel hello, the client dials the carrier its
   service declared, and the model gained the axis to measure it (`fd4c9aa`):
   arms `l3` / `l3-mux` / `l3-kcp` / `l3-mux-kcp` / `l4-kcp`, the carrier in the
   generated config and in the evidence, and the condition `loss1_rate100` (a
   100 Mbit/s, 20 ms, 1 %-loss leg — the lossy WAN the choice is about).

   What the model measured (`--profile smoke`, two rounds, A/A twin in every
   run, conditions on the **tunnel** leg, tables in
   [docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model)):

   | Condition | control | tcp carrier | kcp carrier |
   |---|---|---|---|
   | clean, `l3` (direct) | 41.8 | 4.17 Gbit/s, 0.855 s/Gbit | 2.19 Gbit/s, 2.554 s/Gbit |
   | clean, `l3-mux` | — | 2.99 Gbit/s, 1.435 | 2.51 Gbit/s, 2.310 |
   | `loss1`, `l4` / `l4-kcp` | 4.43 | 2.51 | 0.40 |
   | `loss1`, `l3` / `l3-kcp` | 4.43 | 2.03 | 0.41 |
   | `loss1_rate100`, bulk | 0.096 | 0.093 (`l4`), 0.089 (`l3`) | 0.059 (`l3-kcp`), **failed 3/3** (`l4-kcp`) |
   | `loss1_rate100`, `rr-16` | 323.2/s | 328.0 (`l4`), 335.3 (`l3`) | **337.8 (`l4-kcp`)**, **338.6 (`l3-kcp`)** |

   Read: the multiplexer costs **28 %** of the bulk throughput and 68 % more CPU
   per byte; KCP costs **48 %** and 3× the CPU per byte *on a clean path*; under
   loss KCP is **five to six times behind TCP** in both architectures; and on a
   rate-limited lossy leg the multiplexed KCP arm does not finish its test at
   all — `KCP session dead link`, which kills the visitor's connection with it.
   The mechanism came out of the carrier's own counters
   (`MOLEHILL_KCP_STATS=1`): **22 % of the datagrams it sent were
   retransmissions**, because with `nc=1` there is no congestion control and the
   pacer's only signal is a PONG that does not arrive within 2.5 s — a
   window-sized burst goes into whatever queue the path has, the standing queue
   delays the acks past the escalating RTO, and the retransmissions enlarge the
   queue until the ARQ declares a live-but-rate-limited peer dead.

   **Both obvious fixes were tried and both are wrong.** `nc=0` (KCP's own
   congestion control) as an A/B against the shipped build on `loss1_rate100`
   measured **0.002 Gbit/s against 0.043** on bulk and 256/s against 344/s on
   `rr-16` — KCP's built-in control collapses the window on a lossy path and
   never recovers. And capping the send window to the path's BDP (2048 → 256
   segments, `set_wndsize`) left bulk where it was (**0.039 against 0.044**) and
   the multiplexed arm still died. The counters say why a knob cannot do it: on
   that leg the sender retransmits **24 % of its output** against a 1 % loss
   rate, so the recovery loop itself is the defect — it needs a controller
   driven by what the path delivers (delivery rate and/or RTT inflation), in the
   adapter, with the `loss1_rate100` A/B as its gate. The engine exposes
   `set_wndsize`/`wait_snd` but not `snd_una`/`rx_srtt`, so such a controller
   starts with two accessors. The one place KCP won is worth keeping in view: on the same leg,
   many short interactions (`rr-16`) were fastest on both KCP arms with the best
   p99 — fast retransmit pays, and it is the bulk path where the missing control
   costs more.

   **The larger finding is about L3 itself.** The same campaigns measured the
   data path against the forwarding path on one host
   ([table](docs/benchmarks.md#the-l3-data-path-what-limits-it-2026-10-10-this-model)):

   | Arm | `bulk-1`, TUN MTU 1400 | `bulk-n` (8 flows) | `bulk-1`, TUN MTU 8000 |
   |---|---|---|---|
   | `l3` | 4.34 Gbit/s | 3.93 | **8.00** |
   | `l4` | 8.99 | 22.33 | 8.00 |

   Two conclusions, both actionable. **L3 does not scale with flows** (eight
   flows move less than one) — the ceiling is a serialized per-packet path, not
   a window, so more connections through a claim cannot raise it. And **packet
   size is the lever**: an 8000-byte TUN MTU (link MTU to match) took the same
   flow to 8.00 Gbit/s (+84 %) at a **third** of the CPU per byte, matching the
   forwarding path at one flow. That is an operator setting, and
   [deployment.md](docs/deployment.md#transparent-services) now carries the
   number.

   At that packet size a second measurement says *what* the remaining ceiling
   is — the forwarding path with its pool capped, so that one arm prices one
   carrier connection:

   | Arm (TUN MTU 8000) | `bulk-1` | `bulk-n` (8 flows) |
   |---|---|---|
   | control | 38.672 | 44.805 |
   | `l3` | 7.136 | 5.847 |
   | `l4` (pool ≤ 4) | 8.286 | 21.604 |
   | `l4-mux1` (one tunnel) | 9.819 | 12.646 |
   | `l4-mux8` | 9.058 | 21.955 |

   One carrier connection tops out near 8–10 Gbit/s here (the one-tunnel arm
   says so at one flow *and* at eight streams over that tunnel), and the
   forwarding path passes it only by spreading streams over several tunnels. A
   claim has exactly one channel, so it sits at the per-connection ceiling by
   construction — that is why its `bulk-n` is below its `bulk-1`. Going past it
   is a **design** decision (per-flow sharding: several channels per claim, one
   per inner flow), not a tuning one, and the model cannot price it before it
   exists.

   Open, in the order the measurements argue for them:

   - **A device path that carries more than one packet per syscall** — chosen
     as the next lever (2026-10-10), and **the kernel half is now verified**
     (spike outside the repo, same host):
     - attaching to an operator-created device with `IFF_VNET_HDR` works, and
       so do `TUNSETVNETHDRSZ` (its argument goes **by pointer**) and
       `TUNSETOFFLOAD` (its argument goes **by value** — mixing the two
       conventions returns `EINVAL` for every mask, which cost an hour of the
       spike);
     - `TUN_F_CSUM|TUN_F_TSO4|TUN_F_TSO6` is accepted, so the device advertises
       TSO/GSO;
     - one 54 918-byte **USO** write (`gso_type = UDP_L4`) was delivered as
       **40 datagrams of 1372 B** to a local socket: one syscall, forty packets,
       segmented by the kernel;
     - one 54 440-byte **TSO** super-packet was accepted and crossed a veth as
       *one* 54 KB skb (the virtual link preserves GSO), so segmentation is
       deferred to wherever it is really needed.

     Two more things the same day's experiments settled, both of them reasons
     the header does **not** land on its own:

     - **The offload flag is two-way, and enabling it alone breaks every
       claim.** With `TUN_F_CSUM` on and the virtio header dropped on the floor,
       no visitor could reach a claim at all (measured: both the framing run and
       the readiness probe failed for every round). The reason is the topology's
       own veth: it hands over packets whose transport checksum is still
       partial, the device advertises that it may do so, and a daemon that
       forwards the packet without its flags forwards something the far end can
       only drop. The header's `flags`/`csum_*` fields have to travel with the
       packet — which is the wire change, not a local one.
     - **The header without the offloads is a pure cost.** Attach with
       `IFF_VNET_HDR` only (offloads off, packets complete, everything still
       working) measured **4–5 % below** the released build on `bulk-1`: three
       rounds, the A/A twin beside it, every round below both old arms
       (4.041 against 4.188/4.255 Gbit/s). The header costs a 10-byte copy on
       read and a two-segment `writev` on write, and buys nothing until the
       writes coalesce. So it lands *with* the GSO write, as one change whose
       A/B has a win to show.

     **What is still unverified** is the half the win depends on: whether the
     kernel *coalesces* on the read side. The spike could not answer it — a UDP
     stream arriving on a veth is fragmented, not GRO-able, and the reads came
     back one packet each with the header all zeroes (10 169 reads of 1406 B,
     no `gso_type`) — so the TCP case has to be measured where TCP flows exist:
     in the bench, with the L3 arm at 1400 and at 8000, watching
     `mean_carried_packet_b` (a frame above the MTU is a coalesced read) and
     `syscalls_per_s`. If the read side never coalesces, the win is only the
     inject side's, and the design has to coalesce a run in userspace — which is
     why the two slices below are ordered that way.

     The design that follows, in two slices:

     * **Slice 1, no wire change.** Both TUNs attach with `IFF_VNET_HDR` and
       enable the offloads; the *inject* side groups consecutive packets of one
       flow (same 5-tuple and direction, consecutive TCP sequence numbers or
       equal-size UDP payloads, no SYN/FIN/RST inside the run, DF set) and
       writes the run as one GSO packet, falling back to one write per packet
       whenever the run does not qualify or the device has no offloads. The
       frames on the wire stay one packet each, so nothing needs a version bump
       and an old peer keeps working.
     * **Slice 2, with a wire change.** The virtio metadata travels in the L3
       frame, so a super-packet read from one TUN is carried as one unit and
       injected as one write at the far end (and the reverse direction too).
       This is a protocol change (v6) and is only worth it after slice 1 has
       been measured.

     Measurement plan, all of it on the model as it stands: the L3 arms
     (`l3`, `l3-deep`) at the default 1400-byte TUN MTU and at 8000, on
     `bulk-1`/`bulk-n`/`rr-16` and on `udp-pace` (`bytes_per_syscall` and
     `syscalls_per_s` are the readings that must move — they are device-I/O
     metrics, so an L3 arm is exactly where they are valid), with `l3-deep`'s
     deeper queue as the control for "is it syscalls or queueing". Success is a
     throughput gain the run's own A/A floor can see, at no cost to `rr-16`'s
     latency or the wire ratio.
   - **The datagram size follows the path — landed, measured, kept.** The
     carrier had been pinned at 1400 bytes with a shrink-only adaptation, so
     every jumbo path paid the per-datagram cost 5.7× more often than it had
     to. `Kcp::set_mtu` (was `shrink_mtu`) now follows the caller in both
     directions, the adapter computes the size from the kernel's path-MTU
     answer up to an 8 KiB ceiling (`datagram_for_path`), re-reads it once a
     second, and every inbound buffer is sized from the *ceiling* rather than
     from the engine's default — the spike's first jumbo attempt arrived as a
     retransmit storm because `DGRAM_BUF` was 2048 against 8000-byte segments.
     A/B against the previous build, two rounds an arm, `l3-kcp`, one bulk
     flow: clean jumbo **2.170 → 3.026 Gbit/s (+39 %)** at −28 % CPU per Gbit
     and a better wire ratio; `loss1` jumbo **0.407 → 1.713 (+4.2×)** at −40 %
     CPU; `loss1` on a 1500-byte link **neutral** (0.406 → 0.434), which is the
     safety property — the probe keeps a normal path at the size it carries;
     `rr-16` on `loss1_rate100` +3.8 %.
   - **The dead link was the rule, and it is fixed.** The engine inherited the
     reference's dead-link rule — twenty retransmissions of one segment ends the
     session — which conflates a peer that is gone with one that is slow. On a
     rate-limited path the acknowledgements queue behind the shaper, so a
     segment collects twenty retransmissions while the peer answers everything
     else, and a live session is closed. The rule now needs the count *and* a
     send window that has not moved for five seconds (`KCP_DEAD_GRACE_MS`, with
     `last_progress` set wherever `snd_una` advances). Measured on
     `loss1_rate100` + a jumbo link: the multiplexed arm failed `bulk-1` in
     **every** round before and completes **both** after (0.058–0.059 Gbit/s,
     17 % fewer wire bytes than the direct arm), and the direct arm's CPU per
     byte fell by a fifth. The one cost: a genuinely vanished peer is now
     detected up to five seconds later, which the session-level watchdogs
     (`FORWARD_IDLE_TIMEOUT`) bound anyway.
   - **The collapse was the pacer, and it is fixed.** The trigger turned out to
     be blunt: in a session's lifetime, the *first* cell of a turn runs at full
     speed and *every later* cell runs at a fraction, whatever the scenario
     (`bulk-n,bulk-n` 6.18 → 1.00; `bulk-n,bulk-1` 5.86 → 1.08; `bulk-1,bulk-n`
     7.1 → 2.0). The state was the adaptive pacer's allowance, visible once a
     gauge for it was added: 12 → 9 → 6.75 → 5.06 → 3.8 Gbit/s across the first
     cell's PONG timeouts, held for the rest of the session because the recovery
     is 5 % per four clean PONGs. A PONG that is late because the peer is busy
     sending is not congestion, so `PaceState::on_ping_timeout` now cuts only
     when the send window has not moved since the PING (`Kcp::snd_una`, sampled
     at PING time). Measured after the fix: `bulk-n,bulk-n` **7.79 then 7.67**,
     `bulk-1,bulk-n` **7.19 then 7.44** — no collapse — while `loss1` jumbo
     stays at 1.67–1.71, `rtt100` is 0.169 → 0.182 and `loss5` is unchanged.
     Two detours are recorded rather than hidden: a **window byte budget** (to
     stop the buffers growing 5.7× at a jumbo MTU) fixed nothing and cost the
     lossy leg two thirds of its throughput (0.45 against 1.67 Gbit/s on
     `loss1`), so it was reverted — a lossy path wants the window *wider* than
     the BDP, not closer to it; and a **1 ms ack-batching window** was −6 % to
     −12 % on the clean path and was reverted. What remains open from the
     original finding: nothing — the collapse is gone, and the numbers that
     replaced it are on the benchmarks page.
   - **L3+KCP matches the TCP carrier on a jumbo single flow and now leads it on
     eight (historical note; see the entry above for the fix).** With the segment size following
     the path and the dead link fixed, one bulk flow through an L3 claim at a
     jumbo TUN MTU (8000) and a jumbo link (9000) measures **7.265 Gbit/s
     against the TCP carrier's 7.086** in the same run (A/A twin 6.948, so both
     are inside the noise of each other) at 2.3× the CPU per byte — the
     user-facing claim "L3+KCP can be as fast as L3+TCP" now has a measurement.
     What does *not* hold is the eight-flow cell, and the trigger is
     reproducible and narrow:
     - `--arms l3-kcp --scenarios bulk-n` (alone): **5.9–6.2 Gbit/s** across
       four runs;
     - `--arms l3-kcp --scenarios bulk-1,bulk-n` (a bulk cell first): **1.99 and
       3.54 Gbit/s** in two runs — and in a 4-arm, 3-round campaign, 2.06 in
       every measured round while the TCP carrier beside it read 5.6–5.9;
     - the engine's own counters are *identical* in both cases (server
       retransmits ~15–18 % of its datagrams either way, client `ms_input`
       1.5–1.9 s, `ms_output` per datagram 7.5 µs), and the difference is the
       **bytes per datagram**: 6.2 KB when it is fast, 3.4 KB when it collapses
       — the sender is producing smaller datagrams, not more work per datagram.
     So the next step is to find why a preceding bulk cell makes the carrier
     emit half-full datagrams: the candidates are the hub's batching (it flushes
     "the moment the device runs dry", and eight flows drain the device
     differently from one) and the KCP writer's chunking downstream of it.
     Reproduce with `sudo -n uv run benches/scripts/bench/bench.py run --profile
     smoke --arms l3-kcp --scenarios bulk-1,bulk-n --condition clean
     --link-mtu 9000 --tun-mtu 8000 --rounds 1 --warmup-rounds 0`.
   - **The whole-workload sweep (`dev`, jumbo path, A/A twin) says where this
     stands**: level with the TCP carrier on one bulk flow (7.136 against 7.554
     and a twin at 7.496) and on churn (6 779 against 7 139, twin 7 209),
     **19 % ahead on eight flows** (7.418 against 6.023, twin 6.243), **behind on
     short round trips** (rr-1 10 177/s against 14 167/s, rr-16 30 264/s against
     32 984/s, p99 equal) and **about twice the inner-UDP loss** at every rung of
     the ladder (1.05 % against 0.60 % at 2 Gbit/s). The UDP loss is the one to
     chase next if quality matters more than throughput: it is the hub's
     endpoint channel dropping when the carrier's writer is slower than the
     ingress, and the loss is unrecoverable for a UDP flow.
   - **The long-haul gap is the next real target, and its mechanism is now
     measured.** `rtt100` (100 ms each way, no configured loss): TCP carrier
     0.461 Gbit/s, `l3-kcp` 0.178 — the widest gap left in the model. The
     carrier's counters put 21 % of the sender's datagrams somewhere on the
     floor and 28 % of its output into recovery, split 4 239 RTO against 2 163
     fast-retransmit (so mostly spurious timeouts). **Kept:** RTO headroom of
     `srtt/8` (`KCP_RTO_HEADROOM_DIVISOR`), worth −42 % RTO resends and a wire
     ratio of 1.20 → 1.09 with throughput flat. **Falsified:** pacing the burst
     to the BDP (`window / srtt`) is 35 % *slower* (0.114 against 0.175 Gbit/s,
     three rounds each, ranges disjoint) and does not change the wire ratio —
     the pacer's bucket-and-rate gate does not remove the losses, it only slows
     the sender. So the next attempt has to find where those datagrams actually
     die (the receiving socket's buffer, the veth queue between the namespaces,
     or the ARQ's own flush cadence) rather than throttle the sender: the
     obvious throttles have all been measured now. The diagnostics this needed
     are on the `MOLEHILL_KCP_STATS` line for good: `resends_rto` vs
     `resends_fast`, `rto_ms`/`srtt_ms`, the window state, the reader/spill
     residency and the pacer's allowance.
     Two more results from chasing it, both landed: **the drain parks what the
     pacer or the kernel refuses** instead of dropping it (worth +12 % on
     `loss1`, 1.824 against 1.627 Gbit/s with disjoint ranges, at an identical
     wire ratio) and the park is bounded by half the RTO so the ARQ does not
     duplicate what the pacer is holding. And one **trade left on the table**:
     with parking in place, capping the send rate at `window / srtt` is +21 % on
     `rtt100` (0.204 against 0.169 Gbit/s) but **+14 % wire bytes** there; it is
     measured, documented on the benchmarks page, and deliberately not carried
     until someone decides that trade. A quick follow-up worth one run: whether
     a *larger* pacer bucket (more burst allowed, less queueing delay) keeps the
     throughput and loses the wire cost.
   - **A rate-aware pacer for the KCP carrier** (above) — **attempted
     2026-10-10, measured, reverted.** The design: sample the peer's
     acknowledged progress every 20 ms, convert it to segments per second, and
     set the send window to `rate × srtt × 1.5`, never below 64 segments. Its
     A/B against the shipped build, same arm, two rounds each:
     clean **neutral** (2.174 against 2.188 Gbit/s), `loss1_rate100` **slightly
     ahead** (0.059 against 0.053 Gbit/s, 9 % fewer wire bytes), and `loss1`
     **twelve times worse** (0.033 against 0.411). The reason is the signal, not
     the intent: `snd_una` measures *contiguous* acknowledgement, so it stalls
     on every gap and the window shrinks exactly when loss needs the opposite —
     and a window too small starves fast retransmit, which is driven by the acks
     of *later* segments; with nothing in flight the sender waits out a 30 ms+
     RTO instead. Two things follow for the next attempt: the rate signal must
     come from acks (or from `KCP_SACKS_SENT`/retransmit counters) rather than
     from contiguous progress, and the window needs a loss-aware reserve with a
     hard floor under fast retransmit's needs. The shipped constant window stays
     until that exists.
   - **The churn scenario reports zeros, and only for L4 arms.** Three runs on
     2026-10-10 (`churn-16`, 2/5/10 rounds) produced four rounds with no samples
     at all and three rounds where the readiness probe could not reach the
     exposed port for its full 30 s — every one of them on an L4 arm or its
     twin, never on L3, and the probe's own log for a zero round *does* contain
     latency samples. Two candidate explanations, neither established: the
     architecture (a fresh visitor connection costs L4 a data channel and a
     pair of ephemeral ports that L3 does not spend) or the probe (which reports
     a rate without saying why connections failed). Until the probe records the
     failure reason, no churn number may be quoted — including the tempting
     reading that L3 beat L4 in both runs' means (6366/7131 against 5638/4932).
     The instrument is the thing to fix first; if the architecture is the cause,
     it is a finding worth having.

     **Closed 2026-10-12: it was the instrument, and the mechanism is now
     named.** The surviving probe logs from the zero rounds read
     `Exception in thread Thread-N: … OSError: [Errno 99] Cannot assign
     requested address` from `Worker._connect` — **ephemeral-port
     exhaustion**, not an architecture effect. `churn-16` opens a fresh
     connection per request; the faster arms (which the L4 ones were, at
     thousands of fresh connections a minute per worker against a ~28 k port
     range with 60 s of TIME_WAIT) exhausted the visitor namespace's source
     ports mid-run, and the resulting `OSError` was raised *outside* the
     worker's try/except, so every thread died uncounted: `ok=0 failed=0
     errors=[]`, a rate of zero, and a cell that looks like "the path carried
     nothing" when it means "the visitor could not open any more sockets".
     L3 read lower rates and never crossed the threshold — which is why the
     zeros were L4-only and the L3-vs-L4 comparison was never evidence.
     Fixed in the probe: a failed connect is now a **counted reading**
     (`connect_errors` in the JSON, in the summary line and the cell's
     `probe` evidence), the worker backs off 50 ms and keeps issuing —
     proven by forcing `_connect` to raise (80 attempts, all counted, no
     thread death) and by a clean 49 183-round-trip local run. **Every churn
     number before this fix is still unquotable** (the count did not exist),
     but new runs can distinguish "slow" from "port-starved", and the
     L3-beats-L4 reading is retracted as an instrument artefact.
   - **What the multiplexer's 28 % actually is.** The earlier per-cell model
     measured 6 % of wire and 33 % of CPU per packet for `direct` over
     `multiplex`; this model measures 28 % of throughput and 68 % of CPU per
     Gbit. The two are not the same reading and the difference is worth
     attributing (yamux frame copies vs the pool's stream machinery) before
     anything is changed for it.

**Pre-registered criteria (kept as written, for the record).** Small packets
(≤128 B payload): wire bytes down ≥ 20 %. Mid (512 B–1 KB): ≥ 5 %. Bulk
(1400 B): throughput down ≤ 2 %. The small-packet and the bulk conditions must
both hold; a difference inside the variance counts as *no difference*, and
either failure removes the feature. The small-packet arm failed on the ceiling
alone, so the other arms were not run for the verdict.
