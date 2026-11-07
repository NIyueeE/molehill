# HANDOFF: the current state, and what to do next

This is the working document for whoever picks this repository up: what the tree
is, what has been measured **on it**, what is still open, and which designs are
decided. It is deliberately short. The research record — every round of
investigation, every superseded conclusion, every negative result with its
reasoning — lives in [`HANDOFF-archive.md`](HANDOFF-archive.md), and nothing in
that file describes the current tree unless this one says so.

Read `AGENTS.md` first for the rules; read this for the state.

---

## 1. Current state

| fact | value |
|---|---|
| branch | `feat/v1.0.0` (never tagged; no release has been cut) |
| version | `Cargo.toml` `1.0.0` |
| wire dialect | **6.0**, unreleased; `SUPPORTED_PROTO_VERSIONS` is 6.0 alone |
| carrier default | `mhd`, with `Carrier::{Tcp, Mhd}` and the `"mhd"` wire tag unchanged |
| gates | `just check`, `just bench gate`, `just interop`, `just l3-accept` (§7 has the evidence and dates) |

The 1.0.0 line exists to make **mhd the carrier a user should want by default**,
not merely one that passes a gate. Priorities are fixed:
**latency > throughput > memory > processor**. Nothing about the configuration
model moves: no key is added, removed or renamed, and no default changes.

**Where the plan lives.** This file is the durable copy of the plan (§4 open
problems, §5 decisions and order). The session's goal objective carries the same
order in one paragraph so that it survives a long context; §5 says how the two are
kept in step. If they ever disagree, this file wins and the objective is corrected.

---

## 2. Decision and invariants

The goal, restated once: a new visitor connection over mhd must cost what a TCP
tunnel costs. Today it does not — see §3 and §4.1.

Binding constraints, all still in force:

- **Configuration is frozen**: no key added, removed or renamed; no default or
  semantic change. `src/config/` may only take wording fixes that match `mhd`.
- **Wire changes go only into dialect 6.0**, which is unreleased; `just interop`
  proves a 6.0 peer and an older dialect refuse each other in both directions.
  No two tags are compatible; a dialect is `major.minor`.
- `Carrier::{Tcp, Mhd}`, the `"mhd"` wire tag, the `unsafe` rules, the permission
  boundary, TCP fallback, the L3/UDP forwarding semantics, the log budget
  (`tests/log_budget_test.rs`) and the serial test convention are all fixed.
- **Never create or push a release tag.** Only a human asks for that; a release
  record is written only when every gate in §7 passes.
- A change that does not beat its own gate is **reverted whole**, with its
  measurement kept. More complexity is not more advanced.

---

## 3. Measured baseline

All numbers below are one run of the `sweep` profile on a freshly built binary,
one round per arm, five arms, 83 samples — `just bench gate` **PASS** (§7). Treat
them as the baseline for any structural change, not as a promise about a
different host.

### 3.1 The clean stages, against the peers (p99, lower is better)

| arm | `timeline[clean]` | `timeline[clean#2]` | what it is |
|---|---|---|---|
| **molehill (mhd)** | **22.505 ms** | **23.08 ms** | the subject |
| rathole | 2.063 ms | — | TCP tunnel, same model, upstream lineage |
| frp | 2.377 ms | — | TCP tunnel |
| l3 | 3.475 ms | — | userspace L3 tunnel, same repository |
| nps | 101.388 ms | — | TCP tunnel (also fails capacity, §3.3) |

molehill's own row: **7.57 Gbit/s, 0.0 % errors** on `clean`; `clean#2` 23.08 ms
with the same zero errors. Both stages are inside the 50 ms SLO **on their own**
— neither is averaged, and no warm-up stage was added.

What the number *is*: the **interactive series'** p99, where each sample is a fresh
TCP connection to the tool's exposed port carrying one 64-byte echo (`probes/series.py`'s
`ping()`). The same stage window also carries a churn series and a datagram series —
they are separate metrics (`churn_per_s`, `udp_rtt_p99_ms`) and are never pooled into
this one (see §4.39 for the correction that cost this file a wrong conclusion).

The gap this document exists for: a TCP tunnel opens a visitor in **2–3.5 ms**;
mhd takes **22.5 ms**. Roughly ten times, on the cheapest path there is.

### 3.2 The shaped stages (molehill, p99)

| stage | p99 |
|---|---|
| `rtt100` | 402.9 ms |
| `loss1` | 1058.1 ms |
| `loss5` | 1464.3 ms |
| `rate100` | 103.1 ms |
| `rate20` | 184.0 ms |
| `jitter` | 109.9 ms |

Each stage is judged alone. `loss5` and `rate20` are the two the earlier cycle
spent longest on; both are far from the clean floor and neither is improvable by
touching that floor.

### 3.3 Capacity

`molehill sustained 8 of 8 streams`; L1–L8 read 3.1–24.5 ms and the ramp reaches
eight. The same run gives l3, frp and rathole 8 of 8, and **nps 0 of 8** — the
peer fails its own capacity, which is worth knowing before quoting it as a
comparison.

### 3.4 What has *not* been measured on this tree

- **bulk-n against a baseline.** The gate only judges the regression half when it
  is given `--baseline`; that has not been run on this tree. The objective's
  "within 15 %" is therefore **unproven**, not met.
- **`churn-16` and `rr-16` across the reconnect/reuse paths** — the two scenarios
  that would show what a reused connection costs, as opposed to a new one.
- **`MOLEHILL_DATA_CARRIER=tcp` as a matched A/B arm.** The peer arms above are
  TCP tunnels from other projects; the in-tree TCP carrier under the *same*
  binary, same method, same round has not been run.

---

## 4. Open problems

Each is written as symptom → what is unknown → the next experiment → the
pre-registered pass and rollback. **The pass and rollback criteria are fixed
before the experiment runs**; a run that changes them afterwards is not evidence.

### 4.1 A visitor pays for a Noise handshake that TCP does not

- **Symptom.** Clean p99 22.5 ms against 2–3.5 ms for the TCP peers (§3.1), and
  the earlier cycle measured `open_us` p90 45.8 ms / max 101.5 ms on individual
  opens — a per-stream tail, not a throughput problem.
- **Where it is, measured (A0 done).** The client already splits an open into
  `started_us` (spawn) / `open_us` (the carrier's stream open) / `ready_us`
  (prologue) / `cmd_us` (command), and the carrier splits the open again into
  `hs_us` (handshake) and `ack_us` (hello acknowledgement). In the failing
  samples **all of the wait is `hs_us`**:

  ```
  started_us=2..16   open_us=8005..21924   ready_us=open_us+2..4   cmd_us=ready_us+3..17
  mhd slow stream open: lane=0 us=21924 hs_us=21820 ack_us=0
  ```

  So the whole clean tail is the **per-visitor Noise handshake**, and a stream
  open costs 5–22 ms where a loopback round trip costs 0.1.
- **Two hypotheses already refuted, both one-constant A/Bs on the `dev` profile**
  (`churn-16`, A/A in the same run, noise ±0.5 ms): the small-flight RTO clamp
  (`MIN_RTO_SMALL_FLIGHT` 20 → 2 ms) and the unmeasured small-flight timer
  (`SMALL_FLIGHT_INITIAL_RTO` 20 → 2 ms). `churn-16` read **10.2 ms** at the
  baseline, **9.95 ms** and **9.5–10.7 ms** after — no change, both reverted. The
  20 ms is therefore **not** a retransmission timer, even though the code contains
  two constants of exactly that value on the same path. (A number matching a
  constant is a reason to look, not a finding.)
- **A proviso on that second A/B.** I rebuilt the binary *while its run was still
  going*, so only that run's first four rounds belong to the 2 ms build (9.720,
  10.472, 7.373, 10.733 ms); the rest measured a mixed binary and are not used.
  The conclusion does not change — the first four are the whole of the evidence —
  but the rule is worth restating: **build, then run, then read; never rebuild
  under a measurement.**
- **Unknown.** Where inside `open_channel_carrying` the time goes: the first
  Noise message's write, the pump's post of it, or the wait for the reply. The
  instrument that answers it is three timestamps inside that call.
- **A0b/A0c, done — and they reframe the question.** Two instruments and three
  runs (`churn-16` and `rr-16`, ~2 000 opens, same-run A/A) say:

  1. an idle write reaches the wire inside five milliseconds — `mhd idle write
     waited for the pump` fired **zero** times;
  2. the server's own path is microseconds — `accept_p50 6 µs`, `post_p50 12 µs`;
  3. in the churn scenario **no open exceeded 5 ms at all** — the
     `mhd open round trip` line never fired.

  So the 21.8 ms `hs_us` that A0 found is **not** the clean path's cost: it is an
  open measured **under contention** (during bulk), which is phase C's problem,
  not the handshake's.

  What the plan's completion condition compares against TCP is the bench's
  `churn-16` cell: `kind="rr"`, sixteen connections each carrying four hundred
  requests, every connection fresh, and the cell is **the round trip of the first
  request on each new connection** — its own claim says so, and that the connect
  itself is `setup_p50_ms` because the server accepts before it pairs. That cell
  reads **10.4 ms** here against 2.1–3.5 ms for the TCP peers.

  The carrier's share of it is now bounded from both ends: the channel open never
  reached 5 ms in these runs (point 3), and the server's accept and post are
  microseconds (point 2). So the residual ~10 ms is in the **forward and echo
  path** — the pair, the backend round trip, the response leg — and that is where
  the next instrument goes. (The probe now states the same connect/echo split on
  its churn line that it already stated for the interactive one; `churn-16` itself
  is an `rr` scenario and does not use that probe.)

- **Unknown.** Whether RACK-style loss detection plus a separate PTO — the split
  MsQuic makes in `core/loss_detection.c` — removes both the age gate and the
  full-flight retransmission storm without reintroducing spurious repairs.
- **Next experiment.** Phase B below.
- **Pass.** A single lost opening recovers at low latency; bulk never re-sends a
  healthy flight; `churn-16`, `rr-16`, both clean stages and `loss1/loss5/rtt100`
  each judged alone; retransmission volume and CPU compared in the same round.
- **Rollback.** Any healthy full-flight retransmission storm, or a first-stage
  throughput drop.

### 4.3 A new stream queues behind bulk

- **Symptom.** `conn.rs` schedules one packet per stream round robin, so a
  44-packet GSO run can belong entirely to mature bulk streams while an opening
  waits its turn.
- **Unknown.** Whether bounded per-class quanta (control/repair/opening,
  new-stream, bulk by byte deficit) move the clean tail without costing single
  stream throughput.
- **Next experiment.** Phase C below.
- **Rollback.** p99 improves while throughput falls; or fairness arguments end up
  starving new streams under an adversary that keeps opening them.

### 4.4 The window only grows, and it grows from a proxy

- **Symptom.** `cwnd` never decreases on loss and slow start never returns; the
  flight total is measuring *received* bytes rather than *delivered* ones, which
  this cycle showed is load-bearing: freeing it on what the peer holds made the
  transfer four to five times worse.
- **Unknown.** Whether a delivery-rate/BDP controller (a simplified BBR: startup,
  drain, probe-BW, app-limited sampling) makes the clean stage stable and the
  shaped stages recoverable, and whether its pacing avoids adding a new
  sub-millisecond wait.
- **Next experiment.** Phase D below.
- **Rollback.** Less stable than today's cwnd on a clean host; keep the recovery
  and scheduler work, revert the controller alone.

### 4.5 The runner's summary prints dashes over real data

- **Symptom.** The sweep summary shows `run: -` and `L1..L8: -` for every arm
  while the results file holds those samples; `just bench gate` reads them fine.
- **Unknown.** Which metric the summary selects per cell — a display bug, not a
  data one.
- **Next experiment.** Fix the summary to name the metric it prints, so a future
  reader cannot mistake "no metric chosen" for "no measurement".

### 4.6 Evidence has been lost to its own clean-up

- **Symptom.** A clean-up glob `m[0-9]*-*` matched the result files as well as the
  scratch directories and deleted `~/tmp/m2*.json`, including the aggregate of the
  gate-passing sweep (§7). The raw per-sample artefacts of that run survive under
  `~/tmp/bench-20261105-230248/`, and the gate output is quoted here.
- **Rule for next time.** Clean up by **directory type** (`bench-<date>-<time>/`,
  `m<round>-<topic>/`), list what will be deleted and count it first, and keep
  result JSONs in a directory that no clean-up pattern covers.

### 4.72 A datagram crosses the wire and is counted, still without a producer (2026-11-07)

The second increment of phase 3, and it is again deliberately smaller than the feature.

`Conn` now has a bounded **outgoing datagram queue** (`DATAGRAM_QUEUE = 64`, oldest dropped when
full — "the newest is the one the application still cares about"), a **transmit arm placed before
the streams** ("a datagram is the latency-sensitive class: it is already on the wire from the
application's point of view, and a queue behind a bulk stream is exactly the delay this frame exists
to remove, but one per packet so a burst cannot crowd out a stream for a whole run"), and a receive
path that **counts** what arrives instead of dropping it silently. The window line prints
`sent=/received=` once a second.

**The three properties are pinned by a test that crosses a real datagram between two `Conn`s**
(`a_datagram_crosses_without_touching_the_flight`):

- the peer's `datagrams_in` goes to 1 and its `datagrams_out` stays 0;
- the sender's `recovery.last_number()` **does not move** — the datagram carries the number it is
  given and consumes none, so the next stream packet still takes the next number;
- `recovery.table_stats().0` stays at **1**, the one stream packet that was in flight: nothing was
  added to the table, so there is nothing to retire and nothing to repair.

The test also had to learn something the code already knew: `poll_transmit` may first owe its peer a
credit update or a bare acknowledgement, so the test **polls until the pass that carries the
datagram** rather than assuming the first call does. Asserting on call order was the first version
and it failed, which is the instrument behaving correctly.

**What is still missing, stated so it is not mistaken for the feature.** Nothing above the carrier
*queues* a datagram: `queue_datagram` is called only by the test, and carries a narrow
`#[cfg_attr(not(test), expect(dead_code, reason = ...))]` naming the next increment as its producer.
The UDP data plane is therefore **still the framed-into-a-reliable-stream path of §4.58**, and every
measurement in this file still describes that path. What exists now is the wire class and a carrier
that can carry one; the service layer is the increment that makes it a feature.

**Next increment, pre-registered.** The producer and the consumer: the UDP service layer queues a
datagram with its per-visitor `id` instead of writing a `UdpTraffic` frame into a reliable stream,
and the receive path hands the payload to the visitor's socket instead of counting it.

- **Pass**: an `udp-pace` run carries its datagrams on the new class — `datagrams_in` grows at the
  probe's offered rate while the lane's `flight`, `retransmits` and credit stay at their idle values.
  If any of the three moves, a datagram is still paying for reliability somewhere.
- **Guard**: the TCP visitor's `clean` p50/p99 must not move beyond the run's own A/A (the two
  services share a lane and its pump); `just bench gate`'s endpoint invariant still holds.
- **Rollback**: the service layer cannot express its affinity without the reliable stream's ordering,
  or a `clean` stage moves — then the class cannot share a lane with the reliable one as designed and
  this increment is reverted whole.

### 4.71 Phase 3 begins: the datagram frame class exists on the wire (2026-11-07)

The first increment of the unreliable-datagram work is in, and it is deliberately only the wire.

**`Kind::Datagram = 4`** with `Frame::Datagram { id, data }`: a fixed-width `u32` layer id
followed by whatever bytes are left. Three properties are chosen rather than inherited, and each
one is there because a datagram is not a stream:

- **it takes no packet number** (`FLAG_UNNUMBERED`). A lost datagram that held a number would leave
  a hole in the peer's cumulative frontier, and the sender's flight would then never retire — the
  next datagram would pay for a repair the application has already given up on;
- **it never enters the recovery table and never touches connection credit**. That is what the
  §4.58 measurement says is worth having: the socket already delivers what is posted, so the cost
  the UDP path pays today is the *bookkeeping* around each datagram (ARQ record, credit, stream
  offset), not the wire;
- **there is no length field.** A truncated datagram is a shorter datagram; nothing above it
  reassembles anything, so there is nothing a length would protect.

`a_datagram_frame_is_unnumbered` pins the first property from the encoded bytes, and the existing
`every_frame_round_trips` now carries a datagram case with a non-trivial layer id and payload.

**What this commit does not do, stated so it is not mistaken for the feature.** The receive path
recognises a datagram frame and **drops it** — `Conn::apply_datagram` returns before touching a
stream, a credit or the recovery table. Nothing produces one yet, and nothing above the carrier can
deliver one, so **the UDP data plane is still the framed-into-a-reliable-stream path of §4.58** and
every measurement in this file still describes that path. The class exists so the service layer can
be built on it; it is inert until then.

**Next increment, pre-registered.** Deliver a datagram to the layer above: a per-lane `id ->|
channel` table filled by the service layer, a `Frame::Datagram` produced by `poll_transmit` when one
is queued, and a receive path that hands the payload to the table's channel instead of dropping it.

- **Pass**: a paced UDP session (`udp-pace`) carried natively reports **fewer** `rx_datagrams` of
  bookkeeping than the framed path at the same offered rate — measured as retransmits and credit on
  the lane staying at their idle values while the datagrams flow. If either moves, the frame is
  still paying for reliability somewhere and the increment is not done.
- **Guard**: the TCP visitor's `clean` p50/p99 must not move (a datagram service shares the lane and
  its pump); `just bench gate`'s endpoint invariant must still hold.
- **Rollback**: a datagram path that needs a retransmit, or a `clean` stage that moves beyond the
  run's own A/A, means the class cannot share a lane with the reliable one as designed and the
  increment is reverted whole.

### 4.70 Sealing costs 3.13× per packet, so the AEAD is the cost and the switch stays off (2026-11-07)

§4.69 re-measured both arms on one shape and **kept both logs** (`pp-on-server.log`,
`pp-off-server.log` beside their results in `~/tmp/results-series/`). Busy lane-seconds, straight
from the `cycle=` line so nothing is assumed:

| arm | `clean` throughput | packets/pass | `flush`/pass | **`flush`/packet** |
|---|---|---|---|---|
| seal **off** | **8.77 Gbit/s** | 27.6 | 80 µs | **2.91 µs** |
| seal **on** | 3.75 Gbit/s | 35.5 | 324 µs | **9.12 µs** |

**The pre-registered pass was "sealed at most 2× per packet"; it is 3.13×.** So the second branch is
the one that happened, in its own words: **the AEAD is the cost on this host, sealing stays off, and
these numbers go into the release record as the reason.**

Two things worth keeping from it:

- **The per-pass ratio (4.05×) overstates the per-packet ratio (3.13×)** because the sealed arm
  actually runs *more* packets a pass (35.5 against 27.6) — so part of the per-pass figure is a
  consequence, not a cause. §4.69's "between 3.4× and 3.6×" is corrected by this table.
- **9.12 − 2.91 = 6.2 µs per packet for one ChaCha20-Poly1305 call on 1 387 bytes**, with no key
  derivation and no allocation on that path (§4.69's code read). That is roughly **an order of
  magnitude above** what the cipher costs in software on a modern core. It is not this cycle's
  question any more — sealing stays off either way — but it is recorded as a measured anomaly rather
  than smoothed away, because if the AEAD is ever wanted for another reason (a hostile middlebox, a
  policy requirement) this number is where that work starts, and it should start from "the call costs
  6.2 µs, which is too much for what it does" rather than from the cipher's reputation.

**What this closes.** Phase 2's original question — "can the per-visitor handshake go away" — has a
measured answer now: **the handshake goes, the latency halves, and the crypto that replaces it costs
three times the build.** On the priority order 延迟 > 吞吐 that is a favourable trade only if the
throughput loss is acceptable, and the pre-registration said it is not. The switch stays off, the
`DERIVED_SELECTOR` path stays tested-and-inert, and the release record cites 3.13×.

### 4.69 Two corrections to §4.68: a denominator, and evidence this round destroyed (2026-11-07)

Both corrections are mine, found while trying to act on §4.68's conclusion, and both are the kind
this file already has rules against.

**1. The per-packet cost was a per-pass number divided by an assumed packet count.** §4.68 turned
`flush` per pass (ON 273 µs, OFF 73–81) into "+4.5 µs per packet" by assuming 44 packets a pass —
the `SEND_RUN` cap. Measured from a surviving log: a busy lane-second actually runs **29.6 packets a
pass**, so OFF's cost is **81 / 29.6 = 2.74 µs a packet**, not the 1.73–1.76 §4.66 reports at a
different load, and not derivable from the per-pass figure at 44. This is the sixth unit-or-denominator
error of the cycle and the same shape as the fifth: a quotient read without its divisor.

**2. Two of the four arms' logs no longer exist, because this round's clean-up deleted them.** The
`~/tmp` sweep kept only the newest `bench-*` directory, which removed `on2`'s and `off1`'s logs — the
only evidence of what the sealed arm's `packets` and `passes` actually were. So the *ON* per-packet
cost **cannot be recomputed at all** from what survives: `flush_per_pass` for ON was quoted while the
line existed, but its companion `packets` was not, and the arithmetic needs both. The rule "list what
will be deleted and count it first" was followed for the directory *count* and not for its *content*:
no check asked "does a conclusion in HANDOFF depend on this?"

**What survives, stated precisely.** OFF: `flush` **2.74 µs per packet** at 29.6 packets a pass on a
busy lane-second. ON: `flush` **273 µs per pass** (quoted while the log existed). **Sealing costs the
build somewhere between 3.4× and 3.6× per pass, and the per-packet attribution is open.**

**And a code fact that narrows where it can be.** `StreamCipher::seal` is one `encrypt_inout_detached`
with a nonce from the packet number and a stack `[u8; TAG]` return — **no per-packet key derivation
and no allocation on that path**. The one allocation `seal_stream_payload` does make is `to_vec()` on
the 9–12-byte header, which is tens of nanoseconds, not microseconds. So §4.68's guess ("the suspect
is the per-packet derivation") is **wrong**: there is no derivation per packet. Whatever the 3.4–3.6×
is, it is inside the AEAD call itself or in how it is invoked — which makes §4.68's "if the AEAD is
most of it, the cipher is the cost on this host" branch the likely one, and that branch's answer is
"sealing stays off, and the trade is recorded".

**Next experiment, pre-registered before it runs.** Re-measure **both** arms on the same shape, and
**keep both logs** by writing them to `~/tmp/results-series/` before any clean-up: per arm, the busy
lane-second's `flush per pass`, `packets per pass` and therefore `flush per packet`, plus `flush` as a
share of the second.

- **Pass**: the sealed arm's `flush per packet` is **at most 2×** the unsealed arm's. Then the 3.6×
  per-pass figure is mostly *fewer packets a pass* (a consequence, not a cause), sealing's throughput
  cost is a pacing artefact rather than a crypto cost, and the next question is why the sealed build
  produces fewer packets a pass.
- **If sealed is 3× or more per packet**: the AEAD is the cost on this host, sealing stays off, and
  §4.68's numbers plus this one go into the release record as the reason.
- **Guard**: nothing but the two runs; `--scale 0.15 --rounds 1` each, ~14 minutes. **Artefacts are
  copied out of the work directory before it is removed** — that is the deliverable, after this round
  lost them.

### 4.68 Sealing passes the latency test and fails the throughput one, and the cost is in the build: 273 µs a pass against 73 (2026-11-07)

§4.67's pre-registration: **ON · OFF · ON · OFF**, same shape, pass if the open falls ≥ 30 %, `clean`
p99 falls ≥ 30 %, and throughput is no worse than the spread *between* the two ON runs and the two
OFF runs. Four runs, and one of them died at the readiness probe:

| arm | open µs | `clean` p99 | `clean` thr | `loss1` thr | max reader park | cells |
|---|---|---|---|---|---|---|
| `on1` | 566 | — | — | — | **2 001 691 µs** | 1 (failed) |
| `off1` | 1 826 | 24.08 | 6.26 | 4.33 | 2 003 270 | 8 |
| `on2` | **549** | **23.22** | 4.13 | 1.86 | 2 002 012 | 8 |
| `off2` | 1 394 | 59.62 | 7.93 | 5.10 | **2 385 739** | 8 |

**The latency criterion passes with room**: the open is **557 µs against 1 610** (−65 %), and `on2`'s
`clean` p99 is lower than *both* OFF runs (23.22 against 24.08 and 59.62).

**The throughput criterion fails.** `clean` 4.13 against 6.26 and 7.93, `loss1` 1.86 against 4.33 and
5.10 — below both OFF runs by more than the OFF pair's own spread (6.26–7.93, i.e. 1.67 Gbit/s wide).
So under its own rule **the switch stays off**.

**And the cost was located, then that location was withdrawn within a round — see §4.69.** Per-pass
`flush` on busy lane-seconds read **ON 273 µs (p50), OFF 73–81 µs**, which seals the build by about
3.6×. What this section then wrote — "at ~44 packets a pass that is +4.5 µs per packet" — **divided a
per-pass figure by an assumed packet count**, and §4.69 measured the real figure: a busy lane-second
runs **29.6 packets a pass, not 44**, so OFF's cost is **2.74 µs a packet**. The ON arm's per-packet
figure is **unrecoverable**, because this round deleted its logs. The claim "+4.5 µs per packet" is
withdrawn: the per-pass numbers stand, the per-packet conversion does not.

**One thing the pairs settle, and it is not sealing's fault.** The **2-second reader park appears in
all four arms** — 1 999–2 386 ms — including both OFF runs, so it is the ambient stall this file has
recorded before and not a property of sealing. `on1` died because the readiness probe's 3-second
connect landed inside it; the same stall in an OFF arm would have done the same. **Saying "sealing
wedged" would have been wrong**, and the 6/6 integration result of §4.67 stands.

**Next experiment, pre-registered before it runs.** Split the sealed path's per-packet cost, with the
same instrument shape §4.64–§4.66 used: time `seal_stream_payload` as a whole, and inside it the
**key/nonce derivation** and the **AEAD call** separately.

- **Pass**: the derivation is ≥ half of the 4.5 µs. Then it is the defect named above, the fix is a
  counter instead of a fresh derivation, and sealing becomes a **latency win at no throughput cost** —
  which would be the first change in this cycle to win on the first-priority axis for free.
- **If the AEAD call itself is most of it**: the cipher is the cost on this host and the trade stands
  as measured; sealing stays off and the numbers above go into the release record as the reason.
- **Guard**: the benchmarked arms are unchanged (this is an instrument); `flush_per_pass` must move
  by the amount the derivation accounts for, and the four-arm spread is the yardstick.
- **Rollback**: the instrumentation's own cost exceeds 5 % of `flush_per_pass` against an
  uninstrumented arm.

### 4.67 The sealed wedge is gone, and sealing is now the cycle's first real latency win (2026-11-07)

Phase 2's precondition was "reproduce the multi-stripe wedge under `MOLEHILL_MHD_SEAL=1`, then fix
only a proved root cause". **It does not reproduce.**

`tests/integration_test.rs::striped_data_channels` with `MOLEHILL_MHD_SEAL=1`, six consecutive
runs: **6 passed, 0 failed.** §4.13 recorded the opposite — "sealed 3 of 6 runs, unsealed 6 of 6,
same test, same host" — and the tree has changed enough since (the 30-second reorder deadline of
`4ce0f24`'s neighbourhood, the abandon hold of §4.50, the window-ceiling fix) that the stall it
described no longer happens. A six-of-six is not proof of absence, but it is the opposite of the
recorded rate and the right amount of evidence to stop treating the wedge as a blocker.

**And the property the derivation depends on holds, measured rather than assumed.** Both ends
print the handshake transcript per lane; across four lanes the client reports
`79fa8db1 / b75a9db3 / 481f14f3 / 80f078de` and the server reports the **same four values**
(one per stream, all on stream 1). So the two ends agree on one root per lane, and the four roots
are distinct — which is what `(lane handshake hash, direction, stream id)` needs to keep keys and
nonces from colliding across lanes.

**Then the measurement, and it is the most promising latency result of this cycle.** Two runs of
the same shape, seal on and seal off, back to back:

| | seal **on** | seal **off** |
|---|---|---|
| `paired_p50` (accept → channel taken, **the open**) | **513 µs** (n=377) | **1 494 µs** (n=325) |
| `clean` p50 / p99 | **1.64 / 16.00 ms** | 10.85 / 125.01 ms |
| `clean#2` p50 / p99 | 1.80 / 24.48 ms | 2.46 / 7.60 ms |
| throughput `clean` | 3.98 Gbit/s | 6.06 |
| throughput `loss1` | **1.38** | **7.68** |
| throughput `jitter` | **0.90** | **4.75** |

**Sealing cuts the open to a third and the release cell's tail by an order of magnitude** — and
it costs throughput on `loss1`, `jitter` and `clean#2` far beyond what the run-to-run swing can
explain. That is exactly the trade the objective's order ranks: **latency above throughput**. But
it is **two single runs**, and §4.66's own rule says a 36 % swing lives on this cell at one round,
so the throughput column cannot be read from one pair and neither can the latency be *claimed*
from it. What can be said now is directional and stated as such: the open and the clean tail both
move in the wanted direction by margins far larger than any noise this file has measured, and the
throughput loss is real on at least two stages.

**Next experiment, pre-registered before it runs.** Interleaved pairs, **ON · OFF · ON · OFF**, the
same shape (one arm, `--scale 0.2 --rounds 1`, stats on), which gives two ON and two OFF runs with
each pair adjacent in time — the cheapest form that respects §4.66's 36 % swing.

- **Pass**: the open's `paired_p50` falls ≥ 30 % (it is 3× at one pair), **and** `clean`'s p99
  falls ≥ 30 %, **and** throughput on `clean` and `loss1` is no worse than the spread *between the
  two ON runs and the two OFF runs* — measured, not assumed from a single pair.
- **If throughput holds**: sealing becomes the default and the release cell is re-measured on it,
  because a 3× faster open and a 10× tighter clean tail are the two numbers the objective's
  latency clause is about.
- **If throughput does not hold**: the trade is recorded, the switch stays off, and the finding
  stands as the reason it is off — a latency win that costs a quarter of the throughput is a
  decision for the operator, not for this file.
- **Guard**: `just interop` (6.0 refuses older dialects both ways) and the 6/6 stripe test re-run
  on the final tree; `rss_peak_mib` recorded (sealing keeps a `TransportState` per stream).

### 4.66 The cursor walk is free too: four candidates excluded, 82 % is a dozen small costs (2026-11-07)

§4.65's last instrument, on the same shape — busy lane-seconds of a clean stage:

| part | µs per packet | share of `flush` |
|---|---|---|
| stream pick (`next_stream_range`) | **0.000** | **0.0 %** |
| cursor advance (`advance_cursor`) | **0.000** | **0.0 %** |
| recovery record (`Sent` insert) | 0.305 | **17.7 %** |
| payload copy | 0.006 | 0.3 % |
| **everything else** | **1.413** | **81.9 %** |

**Both `BTreeMap` walks are free** — a range over a few dozen streams, done twice per packet,
costs less than a nanosecond each. So the pre-registered "if it is free too" branch has fired, and
it is the branch that licenses a real statement rather than an inference from absence:

> **Building a packet costs 1.73–1.76 µs, and 82 % of it is not any one thing.** Four candidates
> have now been measured and excluded — the pick, the cursor advance, the payload copy and (mostly)
> the recovery record — and what is left is the sum of a dozen small costs: the header encode, the
> `acks.packet()` bitmap, the seal check, and roughly fifteen counter updates.

That number goes into the release record as a **measured** cost of the design, not a gap. And it
closes the build-path question this cycle opened at §4.59: the throughput ceiling is
`packets per second × 1 387 B` with a fixed per-packet cost that no single structural change
moves.

**A method problem surfaced in the same run and must travel with that conclusion.** The two
consecutive one-round runs differ in stage throughput by **8.49 against 5.46 Gbit/s** — 36 % —
while their per-packet cost is nearly identical (1.76 and 1.725 µs). The instrumentation is
therefore not what moved (its own cost is in the per-packet figure, which held); the rank is.
But it means the guard the last two pre-registrations asked for — "the uninstrumented arm within
noise" — **cannot be settled at one round on this cell**: its own floor at eight rounds is 6.7 %
(§4.47), and at one round the swing observed here is 36 %. Any future claim that an instrument is
free has to come from the two-arm, eight-round form, or it is not a claim.

**So the built-path line closes here, and the plan moves on.** §5's item 1 is done: the ceiling is
`SEND_RUN × 44 packets at ~1.75 µs`, `SEND_RUN` is ruled out by the latency order, and the yield
is priced and un-reclaimable. The next item is the carrier work that has been waiting behind it —
phase 2, the lane root that is implemented and switched off.

### 4.65 The pick is free, the recovery record is 11 %, and 88 % is still unmeasured (2026-11-07)

§4.64 pre-registered a three-way split of the remaining 1.78 µs per packet, timed at both
`BTreeMap` sites. The pass criterion was "one of the two operations is at least 40 % of the
packet's cost". Measured on half a million packets of a clean stage:

| part | µs per packet | share of `flush` |
|---|---|---|
| stream pick (`next_stream_range`, a `BTreeMap::range` walk) | **0.001** | **0.0 %** |
| recovery record (`Sent` insert) | **0.198** | **11.3 %** |
| payload copy (§4.64) | 0.007 | 0.4 % |
| **everything else** | **1.555** | **88.3 %** |

**The criterion fails**: the pick is free — a range walk over a few dozen streams costs a
nanosecond — and the recovery record is a fifth of a microsecond. So the pre-registered
"if both are small" branch applies to *them*, and it does **not** license the conclusion that
attached to it ("1.8 µs is this design's cost"), because 88.3 % of the packet has now been
excluded from three candidate explanations without being explained itself. **Naming a number
as "the design's cost" is only honest when the remainder has been measured, and here it has
not.**

**What the remainder can be, listed so the next instrument is chosen rather than guessed.**
Per packet, still untimed: `advance_cursor()` — **a second cursor walk per packet**, which is
the single most suspicious item precisely because its sibling turned out free;
`acks.packet()` (the bitmap and the stream-frontier fields); the header encode
(`wire::encode`, behind a 1-in-16 sample); `seal_stream_payload`'s check; and roughly fifteen
`saturating_add` counter updates. Six `Instant::now()` calls per packet are also inside the
figure now — the instrumentation pays for itself, at ~25 ns each, and the throughput at this
shape (8.49 Gbit/s) matches the previous stats-on run (8.51) inside run-to-run noise.

**Next experiment, pre-registered before it runs.** Time **`advance_cursor`** the same way, and
nothing else. It is one site, one pair of clock reads, and it either takes the 88.3 % or it does
not; if it does, the fix is a cursor that already knows where the next eligible stream is,
which is exactly the structural change §4.64 hoped for. If `advance_cursor` is also free, then
the remainder is the sum of a dozen small costs, **that** is when "1.76 µs per packet is this
design's cost" becomes a measured statement rather than an inference from absence — and it goes
into the release record as one.

- **Pass**: `advance_cursor` is at least 30 % of `flush`. Then it is the lever, and it does not
  cost latency.
- **If it is free**: accept the spread, write the cost down, and stop optimising the build path;
  the next item is the phase-2 carrier work.
- **Guard**: same as §4.64 — the uninstrumented arm of the *same commit* must be within noise. The
  8.49 against 8.51 above is that check at one round each, and it is weak; a two-arm run is owed
  before any conclusion leans on the instrument being free.

### 4.64 A packet's microsecond is 99.5 % machinery: the copy costs 8 nanoseconds (2026-11-07)

§4.63 left one throughput lever — the 41.6 % of a busy lane-second spent in `flush` — and
pre-registered a split of its 1 µs per packet into lock, pick, copy and bookkeeping. Timing the
**payload copy alone** (one pair of clock reads per packet, off unless the switch is on) answers
the data-path half of it, and the answer is lopsided to the point of being a verdict:

```
busy lane-seconds (71 of 96): passes=494 661  packets=18 383 586   stage 8.51 Gbit/s
flush = 46.1 %   gap = 20.0 %   yield = 19.3 %   copy = 0.2 %
flush per packet = 1.785 us      copy per packet = 0.008 us  (8 ns, 1 349 B)
copy share of flush = 0.5 %      everything else = 45.9 % of the lane-second
```

**The data path is 0.5 % of the build cost.** Copying 349 MB/s of payload into the run buffer
costs eight nanoseconds a packet; the other 1.777 µs is the machinery around it — taking the
connection lock, walking the stream range, encoding the header, inserting the recovery record,
and updating a dozen counters. The pre-registered "if the cost is spread evenly" branch does not
apply: the cost is **not** spread, it is almost entirely in the bookkeeping, and the copy is not
worth optimising at all.

**What that rules out, concretely.** Any plan to reduce the build cost by touching the data
path — a cheaper buffer, fewer copies, `Bytes` instead of a copy, a bigger `STRIPE_CHUNK_SIZE` —
can win at most half a percent of `flush`, which is 0.2 % of the lane's second. §4.63's finding
that `SEND_RUN` is the second ceiling and §4.60's 1 µs per packet therefore point at the same
place: the per-packet **machinery**, not the bytes.

**Next experiment, pre-registered before it runs.** Split the remaining 1.777 µs three ways
inside `build_packet`, with two clock reads a packet as before: **the stream pick**
(`next_stream_range`, a `BTreeMap` range walk per packet), **the recovery record** (the
`Sent` insert per packet), and **everything else** (header encode, counters, the seal check).

- **Pass**: one of the two `BTreeMap` operations is at least 40 % of the packet's cost. Then the
  change is structural and specific — a cursor that remembers where the last packet came from,
  or a recovery ring instead of a map — and it is the first throughput lever this cycle has found
  that does not cost latency.
- **If both are small**: the microsecond is spread across many small costs, and the honest
  answer for the release record is that **1.8 µs per packet is this design's cost** and the
  throughput clause is decided by it.
- **Guard**: the same cell with stats **off** in the same run must match the instrumented arm
  within the run's A/A — this is now two clock reads per packet on the hot path, twice what
  §4.63's split used, and it has to pay for itself or be withdrawn.
- **Rollback**: throughput falls more than 5 % against the uninstrumented arm.

### 4.63 The gap is the yield: 27 µs a pass, 19.8 % of a busy lane-second (2026-11-07)

§4.62 pre-registered the gap's tail and a split either side of the `yield_now`. On busy
lane-seconds (71 of 96 in a clean stage) the answer is neither of the two branches it wrote,
and it is more useful than both:

```
passes=519 703   flush=41.6%   gap=20.4%   yield=19.8%   gap_max=57 512us
gap per pass = 28us   yield per pass = 27us   => the non-yield gap is 1us
slow gaps (>=200us) = 1 809 of 519 703 passes = 0.3%
```

**The gap is 97 % the yield.** 27 of its 28 microseconds per pass are spent *inside*
`yield_now`, and the time between the yield returning and the next pass starting is **1 µs**.
So §4.62's "the gap is 26 % of a busy second, tail-dominated" is right about the size and wrong
about the shape: it is not a tail at all, it is every pass, and the slow-gap count (0.3 %)
accounts for 1 809 × ~200 µs ≈ 0.36 s out of the 20.4 % — a rounding error beside the 19.8 %
the yield costs.

**That corrects §4.60, and the correction matters more than the number.** §4.60 read "gap p50
5–7 µs ⇒ the give is not the cost" and closed the yield as a lever. Those 5–7 µs were a median
over **every** pass including the empty ones, which return in about a microsecond. On a lane
carrying bulk, `yield_now` costs **27 µs per pass** — 9 100 passes a second is 246 ms, i.e.
**19.8 % of the lane's whole second**. The give is one of the two largest terms in the cycle.

**And it is not obviously reclaimable, which is the part that matters for the plan.** §4.46
tried both ways out of it and both were worse: removing the yield entirely took throughput to
**0.74 Gbit/s** with a 20-second listener gap, and `consume_budget` (which yields once per ~128
polls rather than every pass) read 6.17 / 6.42 / 5.21 / 7.32 against a baseline of 7.11 / 7.22 /
9.18 / 7.05. So the 19.8 % is **the price of the dispatcher being able to run**, not waste — and
the two cheapest alternatives have already been measured and lost.

**So the cycle now reads, on a busy lane-second:**

| term | share | what it is |
|---|---|---|
| `flush` | **41.6 %** | building 44 packets: lock, pick, encode, 1 387-byte copy, recovery bookkeeping — **1 µs per packet** |
| `yield_now` | **19.8 %** | giving the worker back so the listener's dispatcher can feed this lane its acknowledgements |
| between yield and next pass | 1.2 % | nothing |
| parked in `wait()` | ~37 % | waiting for the writer to produce the next run |

**Which leaves exactly one throughput lever**: the 41.6 % in `flush`. The yield is priced and
already has two failed alternatives; the parked share is the writer's, not the pump's.

**Next experiment, pre-registered before it runs.** Split the 1 µs per packet inside `flush`
into its parts, per lane per second: the **lock wait** (`Shared::with` acquired per packet),
the **pick** (`next_stream_range`), the **copy** (the payload into the run buffer) and the
**bookkeeping** (recovery record, credits, counters) — the last three as a single "inside
`build_packet`" figure to keep the clock reads to two per packet.

- **Pass**: one of the three parts is at least half the packet's cost. Then the next change is
  in that part and it is a real lever, not a tuning.
- **If the cost is spread evenly**: no single structural change buys much, and the honest
  answer is that `SEND_RUN × 44 packets at 1 µs` is the design's throughput and the objective's
  throughput clause is met or not by that number — which is worth knowing for the release
  record.
- **Guard**: two clock reads per packet is the perturbation this must survive; measure the
  same cell with the split **off** in the same run (an arm of its own) and require throughput
  within the run's A/A. **Rollback**: the split costs more than 5 % of throughput.

### 4.62 The flush fork closes: both readings were right, for different seconds (2026-11-07)

§4.61 left two instruments 27× apart. Printing **raw** microseconds beside the normalised ones
(`flush_raw`, `flush_per_pass`) and splitting the stage's lane-seconds by load settles it:

| lane-seconds | n | passes/s | packets/s | flush per pass | flush share | gap share |
|---|---|---|---|---|---|---|
| idle (54 of 98… see below) | — | ~650 | ~280 (0.4/pass) | **1 µs** | 0.1 % | 0 % |
| **busy (>20 k packets)** | 72 | 8 700–9 500 | 383 000–408 000 | **43–48 µs** | **40–45 %** | **26 %** |

So the "2.2 %" line of §4.61 and the "74 µs" of §4.60 were both measuring real seconds:
**a second in which the pump mostly runs empty passes costs 1 µs a pass; a second in which it
runs full 44-packet runs costs 43–48 µs a pass.** The stage is an interleaving of the two, which
is why a single line and a single histogram disagreed. Aggregated over the *busy* lane-seconds:

```
passes=509 890  packets=18 247 401  flush_raw=31 184 190us of 72 134 302us lane-time
=> 61 µs per pass, flush 43.2 % of the second, gap 25.6 % -- 69 % of the lane's time in the loop
=> 253 000 packets/s per lane, against a stage of 8.27 Gbit/s
```

**Three statements this fixes, and one it leaves open.**

- **The pump is neither parked nor saturated**: 43 % flush + 26 % gap leaves ~31 % parked in
  `wait()`. So §4.61's "parked 96 %" was the low-load seconds and §4.60's "build cost is the
  constraint" is closer, but neither is the whole picture.
- **1 µs per packet stands** (43–48 µs for ~44 packets), which matches §4.60's 1.5 µs estimate
  closely enough that the number is now measured twice by different routes.
- **The `PUMP_FLUSH` p50 of 20 µs understates a busy lane** by 2–2.4×, because the histogram's
  median is taken over every pass including the empty ones. A p50 over *full* runs is the number
  that means anything, and it is not what the histogram reports.
- **Open**: the gap is 26 % of a busy second in aggregate but 5–7 µs at p50 — so its distribution
  is tail-dominated, and nothing has looked at that tail. It is the second-largest term and it
  has never been described.

**Next experiment, pre-registered before it runs.** Give `gap` a p99 and a max per lane per
second (it is already sampled per pass; only the aggregation is missing), and split the gap into
its two halves by stamping **before and after the `yield_now`**.

- **Pass**: on busy lane-seconds the gap's p50 is under 20 µs while its p99 is above 200 µs —
  then the tail is a scheduling delay on a small fraction of passes and the fix is where the
  pump is descheduled, not in the loop.
- **If the gap's p50 is above 100 µs on busy seconds**: the 26 % is uniform and the pump is
  giving up the worker every pass; that is the §4.46 tension again, now with a number attached.
- **Guard**: instrument-only, one line a second; `clean` p50/p99 inside §4.54's two-arm spread.
- **Rollback**: the yield split cannot be done without two clock reads per pass on a hot path —
  in which case only the p99/max are added and the split is dropped as too expensive.

### 4.61 The cycle is now per lane, and it says the pump is parked most of the second (2026-11-07)

§4.60's instrument, made per lane: `Conn` carries its own `passes`, `pass_packets`, `flush_us`,
`gap_us` and a **separate** `pass_at` clock, and the window line prints
`cycle=n=… packets=… flush=…us/1000 gap=…us/1000 of …us`. It is filled once per flush and once
per gap, so two adds per pass and nothing per packet.

**Two instrument bugs were fixed in building it, both caught by reading output rather than
by review.** The first version reused `posted_at` for the interval, which `posted_rate` — a
few lines earlier in the same report — had just set to `now`, so every line printed
`cycle=n/a` for a whole run: two independent samples need two independent clocks. And
`report_window` went over its line budget twice before the cycle rendering and the Mbit/s
conversion were each lifted into a named helper.

**What the lines say, one second of one lane, verbatim:**

```
cycle=n=523 packets=15524 flush=22us/1000 gap=14us/1000 of 1000019us
```

That is **flush 2.2 % and gap 1.4 % of the second** — 3.6 % together. If that sample is
typical, then **~96 % of the lane's second is spent in `wait()`, i.e. the pump is parked**
with nothing to send, and the throughput is set by whoever *fills* it — the stripe send
loop — not by the pump's build cost. That would overturn §4.60's reading ("throughput is
packets per microsecond of build") the same way §4.60 overturned §4.59's.

**The reconciliation was done, and the packet accounting closes exactly — after two of my own
arithmetic errors were found and fixed.** Filtered to the stage window (the first pass summed
the whole log, all eight stages, into each stage's figure) and parsed from the whole line
(`cycle=`'s value contains spaces, so splitting the line on spaces shredded it into separate
keys), the clean stage reads:

```
lane 0: passes=191 032 packets=7 579 147  pkts/s=315 187  pk/pass=39.7
lane 1: passes= 16 572 packets=    6 598  pkts/s=    274  pk/pass= 0.4
lane 2: passes=199 840 packets=7 830 904  pkts/s=325 750  pk/pass=39.2
lane 3: passes=135 088 packets=5 104 475  pkts/s=212 391  pk/pass=37.8
SUM 853 349 pkts/s × 1 387 B/packet = 9.47 Gbit/s = the stage, exactly
```

Three things that closes, each of which corrects something:

- **The payload is 1 387 bytes per packet, not 1 348** — that is `MAX_DATAGRAM` (1 400) minus
  the 21-byte head, i.e. the pump is filling its datagrams. `packets × 1 348` was my guess and
  it was wrong by exactly the amount that made the numbers look irreconcilable.
- **The lanes are nothing like symmetric**: 315 187 / 274 / 325 750 / 212 391 packets per
  second. Dividing a process total by four — which §4.59 and §4.60 both did — is wrong here by
  up to three orders of magnitude on the idle lane.
- **The 1.6× of §4.60 is therefore closed as an artefact**, not a mystery: it came from that
  division, plus the payload guess.

**But a new fork opened, and it has to close before anything is built.** The per-lane instrument
says flush is **22 µs per 1 000 µs (2.2 %)**; the process-wide `PUMP_FLUSH` histogram in the
same runs says **p50 74 µs per pass**. With 7 960 passes/s on lane 0, 74 µs a pass would be
**59 % busy**, and 2.2 % would be **2.8 µs a pass**. The two differ by 27×, and they point in
opposite directions: if the second is right, the pump's build cost is the constraint (as §4.60
concluded); if the first is right, the pump is parked ~96 % of the second and the constraint is
whoever fills it (§4.61's candidate).

**A second slip of the same family is in this section's own parsing**, recorded so it is not
repeated: `format_cycle` prints **per-1 000-µs normalised** values, so summing them and dividing
by a raw interval divides by 1 000 twice. The shares above are read off single lines
(`22us/1000`), not recomputed from sums.

**Next experiment, pre-registered before it runs.** One number, two ways, and they must agree:
**the microseconds of one flush**. Both instruments are already in the tree — `PUMP_FLUSH`
(process-wide, called from `flush`) and the per-lane `flush_us` (called from the same place via
`note_pass`) — so this is not a build, it is a check that the two are measuring the same span.
Sample the *raw* per-flush microseconds per lane, sum them over a second, and compare with
`passes × PUMP_FLUSH_p50` for the same second on the same lane.

- **Pass**: the two agree within 20 %. Then the shares stand, and the fork closes on whichever
  one they agreed on — with the consequence stated in advance: **agreement at ~2.2 % means the
  pump is parked and the stripe send loop is the subject (item 2); agreement at ~59 % means the
  build cost is the constraint and §4.60 stands.**
- **If they do not agree**: one of the two is measuring a different span than it claims, and the
  wrong one is fixed or removed before any throughput work is done. That is the cheapest half
  day this investigation can spend, given how many readings a wrong flush share would explain.
- **Guard**: instrument-only, one line a second, gated; `clean` p50/p99 inside §4.54's two-arm
  spread. **Rollback**: the raw per-flush sample cannot be produced per lane without adding a
  clock read per pass — which is one read per 44 packets and is affordable, so a refusal here
  would itself be the finding.

### 4.60 The gap is microseconds, so the yield is not the cost — and one number does not close (2026-11-07)

§4.59's instrument, measured on the same shape:

```
how each pump pass ended  passes=19 513  built_p50=44  built_p99=44  built_max=44
                          stopped=3 837   gap_n=15 676
                          gap_p50_us=5  gap_p99_us=121  gap_max_us=27 826
```

**The gap's p50 is 5–7 µs and its p90 is 13 µs.** The pre-registered pass was "above 100 µs
means the cycle is waiting", and the pre-registered failure was "below 20 µs means the pump
is already posting back-to-back and `SEND_RUN` is the only lever left". **The failure branch
is the one that happened**: after posting a full run the pump is polled again five
microseconds later. The `yield_now` costs nothing measurable here — which also explains why
§4.46's two alternatives to it were both worse: the give was never the expense, and removing
it gave up the dispatcher for nothing.

**So the cycle is `flush`, not waiting — and `flush` is 63–74 µs at p50 for a 44-packet
run.** That is 1.5 µs per packet: the lock, the pick, the encode, the 1 348-byte copy and
the recovery bookkeeping, 44 times. On this cell the throughput is therefore
`packets per µs of build`, and `SEND_RUN` is bounded by the latency it costs a small packet
(§6 row 7), so **the remaining lever is the cost of building a packet, not the shape of the
loop**.

**One number does not close, and it is recorded rather than smoothed.** Four lanes at
19 207 flushes/s would be 4 802 per lane, i.e. a 208 µs cadence — while `flush` (74) + gap
(6) is 80 µs. About 128 µs per cycle is unaccounted. Two candidate explanations, neither
measured: the process-wide `passes` counter cannot be split by lane, so dividing by four
assumes the lanes are symmetric, which the per-lane `posted_mbit_per_s` already shows they
are not; or `send_run`'s `socket.writable()` wait is inside `flush` and sits in the tail
(`flush` p99 427–537 µs) rather than the median. **A per-lane flush count would settle it**,
and that is the next instrument rather than a conclusion drawn over a factor of 1.6.

**Next experiment, pre-registered before it runs.** Make `passes` and `flush` per lane
rather than process-wide — the same defect §4.57 fixed for `posted_datagrams_per_s`, in the
two counters that are still shared — and report, per lane and per second, flushes, the flush
p50 and the gap p50.

- **Pass**: per-lane flushes × flush p50 accounts for the second within 25 %. Then the cycle
  is understood and the next move is inside `flush` (the per-packet build cost).
- **If it does not**: the missing time is a wait this loop's own timers cannot see, and the
  next instrument is on `send_run`'s writability await, which `note_post` already brackets
  but only for the *posting* half.
- **Guard/rollback**: instrument-only, gated, one line a second; revert if the counters
  cannot be made per lane without touching a hot path (they are incremented once per pass,
  not per packet, so this should not be possible).

### 4.59 The pump is not window-limited and not starved: throughput is passes × 44 packets (2026-11-07)

§4.58 asked why the pump holds 59 KB against a 262 144 window with 13.5 MB queued. The
answer is in the pass instrument, and it reframes the whole question:

```
how each pump pass ended  passes=19 281  built_p50=44  built_p99=44  built_max=44
                          stopped=2 803..4 369   reasons="no eligible stream"=2803..4369
```

**Every pass builds exactly `SEND_RUN` (44) packets.** The p50, p99 and max are all 44 —
there is no distribution, so the pump is never short of work: it fills the run, posts it,
and goes round again. Only ~15–20 % of passes end on a reason, and that reason is
`no eligible stream` — the walk found nothing *because the previous 44 packets covered
everything queued*.

**And then the arithmetic closes, which is what makes this the answer rather than another
lead.** 19 281 passes a second across four lanes is ~4 820 per lane; × 44 packets
× 1 348 payload bytes = 285 MB/s per lane ≈ **2.3 Gbit/s**, × 4 = **9.1 Gbit/s** — the stage
reads 7.1–9.7. So:

> **throughput = passes per second × SEND_RUN × payload**, and on this cell it is
> exactly that product.

**`flight ≈ 59 KB` is one pass, not a shortfall.** 44 × 1 348 = 59 312 bytes. The lane never
accumulates more than one run in flight because the acknowledgements return before the next
pass is built. Which means the *real* round trip is far below the 1 000 µs the estimator
reports — that number is `recovery::GRANULARITY`, the estimator's own floor (§4.43), and
59 KB per 50 µs is 9.4 Gbit/s, which is the cell. **The lane's window is 4.4 passes wide and
is never filled because the path drains faster than the pump cycles.** §4.55's "the window
is full" and §4.58's "the pump does not fill its window" were both describing this: the
window is irrelevant here, and the pass cycle is everything.

**So the throughput ceiling is `SEND_RUN`, and it is a latency trade — already measured.**
A pass is ~207 µs apart (4 820/s) while its own `flush` is 18–21 µs, so ~185 µs goes to the
`yield_now` and to being scheduled. Raising `SEND_RUN` would raise throughput in proportion
(96 would be ~2.2×), and §6 row 7 already tried it: **`SEND_RUN` 8 or 96 produced 5 530 and
7 853 slow opens, with stage medians 2.5 → 13 ms.** That is the trade this objective's own
priority order forbids — latency above throughput — so it is not a candidate.

**What is a candidate, and it is not a constant.** The product above is bounded by two
things that can be changed *independently*: how many packets a pass may build, and **how
long a pass waits before building the next one**. If the ~185 µs is the yield rather than
work, then a pass that yields *after* posting but resumes as soon as the socket is writable
would raise the product without delaying a small packet — because there is no queue to jump,
`flight` being one pass. That distinction is measurable with the instrument this round
built, so it is the next experiment rather than another guess.

**Next experiment, pre-registered before it runs.** Per pass, the interval from "run posted"
to "next pass started" — reported beside `built_p50` — so the ~207 µs cycle is split into
`flush` (18–21 µs, measured) and *the gap after it*.

- **Pass**: the gap's p50 is above 100 µs on a `clean` stage. Then the cycle is waiting, not
  working, and the fix is where it waits — the `yield_now` after a full run, which §4.46
  measured as load-bearing for the *dispatcher* but never split from scheduling delay.
- **If it fails** (gap below 20 µs): the cycle is `flush` plus nothing, the pump is already
  posting back-to-back, and `SEND_RUN` is the only remaining lever — which the priority order
  rules out, making the throughput at this cell a property of the design and not a defect.
- **Guard**: instrument-only, one line a second, `MOLEHILL_MHD_STATS` gated; `clean` p50/p99
  inside §4.54's two-arm spread.
- **Rollback**: the gap cannot be measured (the pass has no single "posted" point after a
  full run — `flush` is one call), in which case the split is done inside `flush` instead.

### 4.58 The socket posts exactly what the stage delivers, so the constraint is upstream of it (2026-11-07)

§4.57's instrument, measured, with the `1_000_000` removed. **Summed over the four lanes,
per second**, `posted_mbit_per_s` reproduces the stage's own `throughput_gbps` in **32 of
32 cells, ratio 0.91–1.05** — well inside the pre-registered 15 %:

| stage | arm·round | stage (Mbit/s) | Σ posted | ratio |
|---|---|---|---|---|
| `clean` | molehill·0 | 7 130 | 7 291 | **1.02** |
| `clean` | molehill~aa·0 | 9 554 | 9 698 | **1.02** |
| `rtt100` | molehill·0 | 2 395 | 2 287 | 0.95 |
| `loss1` | molehill·0 | 2 255 | 2 305 | 1.02 |
| `loss5` | molehill~aa·1 | 304 | 306 | 1.01 |
| `rate100` | molehill·1 | 94 | 93 | 0.99 |
| `rate20` | four cells | 18–19 | 17 | 0.93–0.94 |
| `jitter` | molehill~aa·0 | 4 581 | 4 801 | 1.05 |

**And the per-lane column is what made it work, after a fourth denominator error.** The
first read gave ratios of 0.25–0.35 on most cells and 1.0 on a few, which looked like two
populations and suggested a real split; **0.25 is a quarter, and a quarter is one lane of
four.** A single iperf3 `-P 4` run opens four connections that land on four lanes, so a
per-lane number compared with an aggregate reads exactly one quarter low — the same mistake
as §4.50 (Gbit/s read as "near zero") and §4.55 (per-lane read as global), now with a
denominator instead of a unit. The cells that read 1.0 were the ones where one lane carried
everything (`flight = 0`, the other three idle), which is why the split looked meaningful.

**What the pass means, stated narrowly.** Every byte this end posted arrived at the
visitor's `iperf3` — nothing is lost or stalled between the socket and the receiver, and
the plateau in every stage is set by *how fast the pump posts*, not by the peer and not by
a full send buffer. That is a measurement, not an inference: the two numbers are computed
from different instruments on different ends of the run.

**So the question sharpens to one line, and it is not the window and not the socket.**
With `flight ≈ 59 KB` at p50 against a 262 144 window, the pump never has more than a
quarter of its window in flight — while `worst_unsent` reads 13.5 MB. **The pump has data
queued and does not fill its own window.** That is upstream of the socket (which posts
whatever it is given) and downstream of `write` (which admits), so it is inside
`flush`'s build loop: `SEND_RUN` packets per pass, then a `yield_now`, and the run ends
when `poll_transmit` returns `None` — which it does as soon as the per-stream picks run
out, not when the window is full.

**Pre-registered, before it runs.** Instrument what one *build* iteration's refusal
actually is, per second: for each pass, how many packets were built before
`poll_transmit` answered `None`, and **which** `none_reason` stopped it (`NoneSnapshot`
already carries `reason`, `queued`, `credit`, `window` and `cursor`). Grouped by reason.

- **Pass**: on a `clean` stage where `flight` is far below `cwnd`, the dominant
  `none_reason` is a **budget** (`credit`/`window`/`buffer` at zero) — which would mean the
  pump stops for a reason a controller could address — or `no eligible stream` while
  streams hold unsent bytes, which would mean the scheduler's walk is the limit.
- **Guard**: instrument-only, gated on `MOLEHILL_MHD_STATS`, once a second per lane; the
  run's `clean` p50/p99 must stay inside the two-arm spread of §4.54's baseline.
- **Rollback**: the instrument charges for itself, or the reasons come back flat — a flat
  distribution is a metric without contrast (AGENTS §10) and says nothing.

### 4.57 The lane's socket rate, per lane: the instrument, and two unit errors it caught (2026-11-07)

§4.56's next step was to join *bytes leaving this lane's socket* to `flight` and
`buffered`, because that number decides whether the send path or a backlog is the
constraint. Building it produced two findings before it produced any number.

**First, the existing per-lane line was reporting a process-wide counter.** The window line
printed `posted_datagrams_per_s` from `stats::take_datagrams()`, which `swap`s the
*process* total — so the first lane to report each second consumed every lane's datagrams
and the rest read zero. The logs made it visible: `728 624` on lane 1 and `1 937` on lane 3
in the same second. `Conn::posted_rate` now answers `(bytes, datagrams, µs)` from the
connection's own counters, and the window line prints that instead.

**Second, I made the unit error again, in the same round I wrote the rule about it.**
`posted_mbit_per_s` divided by a further `1_000_000` after `bytes × 8 / µs` — which is
already Mbit/s, because a byte per microsecond *is* 8 Mbit/s. Every lane printed `0` for a
whole run, and the run's own log is the evidence: `posted_mbit_per_s=0` in 1 308 lines, not
one non-zero. The fix is the divisor and a comment in place; the measurement is being
re-taken. This is the third unit error in three rounds (§4.50 Gbit/s read as "near zero",
§4.55 the per-lane number read as global), and all three share one shape: **a number was
read without its denominator or its scale, and a mechanism was attached before the
arithmetic was checked.**

### 4.56 The rate estimator failed its own pre-registration, and the window is not what limits this lane (2026-11-07)

§4.55 pre-registered one thing: an estimator built from the acknowledgements the lane was
already applying, and a test that it reproduces the stage's own measured throughput within
**15 %** before any controller could be built on it. It was built and measured on the same
shape as the baseline; **it fails**, and it fails by a factor rather than by a margin.

`delivered` counted every byte an acknowledgement moved — both a retired range and a byte
prefix, because the prefix is the main path on a healthy lane. Per lane, once a second,
`delivered_mbit_per_s = delivered × 8 / µs`. Against the stage's own `throughput_gbps`:

| stage | arm · round | stage (Mbit/s) | estimate p50 | ratio |
|---|---|---|---|---|
| `clean` | molehill · 0 | 9 204 | 3 273 | **0.36** |
| `clean` | molehill~aa · 1 | 8 821 | 3 169 | 0.36 |
| `clean` | molehill · 1 | 7 392 | 1 540 | 0.21 |
| `jitter` | molehill~aa · 1 | 6 840 | 1 617 | 0.24 |
| `rate100` | all four | 94 | 22–25 | 0.23–0.27 |
| `rate20` | all four | 19 | 4–7 | 0.21–0.37 |

**Every lane, every stage, 0.18–0.37.** A systematic factor of 3–5 is not noise and not a
tuning problem: the number is measuring something else, or measuring it in the wrong unit.
The one thing that would explain the whole column is that `delivered` follows the
**frontier** while a large `buffered` backlog sits behind it, so the acknowledgement stream
lags the send stream by the backlog's growth — which is a hypothesis, stated as one, and
not measured.

**What the same lines did establish, and it corrects §4.55.** The window line reports
`cwnd = 262 144` (the ceiling), **`flight = 59 356`** and `buffered = 13.5 MB` on a lane
carrying bulk. So the congestion window is *not* full: 59 KB is in flight against a 256 KB
window, and 13.5 MB is queued in front of a pump that cannot carry it. §4.55's arithmetic —
"`W / RTT` is 2.1 Gbit/s per lane, exactly what four lanes carry, so the window is full in
both modes" — reproduces the *rate* from `ceiling / srtt` but the lane is not window-limited
while it does so; it is **pump- and backlog-limited**, and `worst_unsent = 13 549 191` says
the same thing on the stream that is furthest behind. The coincidence of numbers was real;
the mechanism attached to it was not.

**Both halves are recorded and the code is reverted**, per the pre-registration's own
rollback clause: an estimator that does not reproduce an independent measurement may not
feed a controller, and `just bench` must not pay for it either. What survives is the
measurement — including the correction, which matters more than the estimator would have:
**the release cell's carrier is not window-limited, it is limited by what the pump can
hand to the socket while a 13 MB backlog waits.**

**Next experiment, pre-registered before it runs.** The instrument that would settle which
of the two is the constraint, with no control loop: per lane, once a second, **`posted`
datagrams and their bytes** beside **`flight`** and **`buffered`**, and the pump's own
`flush` duration in that same second (`PUMP_FLUSH` already exists and already reports
per second — it is read, never joined to the window line). The question it answers is
whether bytes are leaving the socket at the rate the acknowledgement frontier implies.

- **Pass**: on a `clean` stage, the mean of `posted_bytes / elapsed` per lane lands within
  15 % of the stage's `throughput_gbps`. If it does, the *send* path is not the constraint
  and the backlog is on the receive side; if it does not, the socket hand-off is.
- **Guard**: instrument-only, gated, one line a second; an instrumented run's `clean`
  p50/p99 must not move beyond the A/A of the run it is compared with.
- **Rollback**: the instrument charges for itself, or the number cannot be joined to a
  stage's window (which is what made this round's reading possible at all — every line now
  carries `lane`, `role` and the second's own boundary).

### 4.55 The clean stage's p99 tracks a lane-level mode: both carrier round trips scale together (2026-11-07)

§4.54 left one question — why one binary reads 20.1 and 141.7 ms at p99 on the same stage
in different arm-rounds. The `info!` histograms already in every run answer the *shape* of
it, and this is the first time they can be read on stages that both carry load:

| arm | rnd | stage p99 | `paired` p50 (accept → channel taken) | `echo` p50 (request read → reply written) |
|---|---|---|---|---|
| molehill | 0 | **141.7** | **4 519 µs** | **1 606 µs** |
| molehill~aa | 0 | 28.2 | 1 573 | 673 |
| molehill~aa | 1 | 20.1 | 2 021 | 932 |
| molehill | 1 | 37.7 | 1 840 | 735 |

**Both of the carrier's round trips are 2.4–2.9× longer in the worst round, by almost the
same factor**, and the visitor's own first-byte delay is unchanged. That is the same
signature §4.44 measured between a good and a slow `cost` cell, now on the release cell
and with the leak fixed: the stage's p99 is not one bad visitor, it is a **lane-level
mode** in which every one of the carrier's round trips is ~2.5× slower.

**And the arithmetic of that mode looked like the window — it is not, and §4.56 corrects
this.** `ceiling = 262 144` with `srtt ≈ 1 004 µs` gives `W / RTT` = 2.1 Gbit/s per lane,
which is exactly what four lanes carry; that coincidence held, but the mechanism attached
to it does not: the same window lines report **`flight ≈ 59 KB` against a 256 KB window**
while 13.5 MB sits in `buffered`. The lane is not window-limited in that mode — it is
limited by what the pump hands to the socket, with a backlog in front of it.

**So the two levers left are named, and neither is a constant.** Either the window stops
being the binding constraint (which §4.43–§4.44 measured as a latency/throughput trade
that failed three times), or the path stops queueing — which is pacing, and pacing needs a
rate estimate this carrier does not keep. That is §5.D, and it is now the item the
evidence points at rather than a design someone liked.

**Pre-registered, before it runs.** A **rate estimator from the acknowledgements already
arriving**, reported only (no control loop yet): per lane, `delivered_bytes / interval`
over one-second windows, and the same in the window line beside `cwnd`, `srtt` and
`flight`. Pass is not a performance number — it is that the estimator reproduces the
independent measurement: its median over a `clean` stage must land within 15 % of that
stage's own `throughput_gbps`, on all four arm-rounds of §4.54's baseline. If it does not,
the estimator is wrong and no controller may be built on it.

- **Guard**: instrument-only, `MOLEHILL_MHD_STATS` gated, one line a second; `clean` and
  `clean#2` p50/p99 must not move beyond the run's own A/A between this run and §4.54's.
- **Rollback**: the estimator's 15 % agreement fails, or an instrumented run's numbers
  move — in which case the instrument is charging for itself and is reverted.
- **Then**: the controller (§5.D) is built on the estimator and judged on the clean
  stage's *p99 distribution across arm-rounds*, which is the quantity §4.54 showed varies
  7× and the only one the release cell is gated on.

### 4.54 The stricter method's first baseline: `clean#2` carries in three arm-rounds of four, and one is refused (2026-11-07)

The re-baseline §5's first item asked for: current tree (`c8ac062`, fingerprint
`1e7f4ed13553b4e8`), one arm plus its A/A twin, two rounds, `--scale 0.2`, stats on.
36 samples; **one cell refused** by §4.53's rule, and that refusal is the point.

| stage | arm | rnd | throughput | p50 | p99 |
|---|---|---|---|---|---|
| `clean` | molehill | 0 | 7.94 | 8.81 | **141.71** |
| `clean` | molehill | 1 | 4.01 | 2.80 | 37.69 |
| `clean` | molehill~aa | 0 | 8.27 | 2.53 | 28.20 |
| `clean` | molehill~aa | 1 | 7.78 | 3.41 | 20.06 |
| `rtt100` | all four | — | 0.38–3.32 | 402.4–402.8 | 410.6–2 090 |
| `loss1` | all four | — | 5.33–6.38 | 42.8–45.3 | 1 057.9–1 093.8 |
| `loss5` | all four | — | 0.24–1.10 | 401.7–402.7 | 1 417.2–3 108.2 |
| `rate100` | all four | — | 0.062–0.094 | 177.4–183.0 | 229.0–262.1 |
| `rate20` | all four | — | 0.0182–0.0187 | 360.8–393.6 | 495.1–744.2 |
| `jitter` | all four | — | 4.85–7.92 | 86.2–89.3 | 138.9–216.9 |
| `clean#2` | molehill | 0 | **0.0 — refused** | 2.55 | 22.27 |
| `clean#2` | the other three | — | **5.05 / 5.25 / 7.00** | 2.27–14.15 | 5.69–91.77 |

**Three things this establishes, each measured rather than argued.**

1. **The abandon fix shows up in the baseline itself.** `clean#2` now carries bulk in
   three of four arm-rounds at 5.05–7.00 Gbit/s. Before the fix (§4.52's pre-fix arm) it
   carried zero in three stages of one round and in five of six cells across two earlier
   runs. The last stage is a measurement again, which is what makes the "first stage
   against last" comparison meaningful at all.
2. **The rule bites exactly once, on the cell that deserves it.** `molehill` r0's
   `clean#2` moved zero bytes and was refused with its reason; the other three carried
   and were judged. A single rule with no threshold, and the run separates the two
   populations on its own.
3. **The first stage's p99 still spans 20–142 ms across four arm-rounds of one binary.**
   Same 7× spread §4.48 measured (47–133), on a different day, with a different
   fingerprint. §3's 22.505 ms sits inside that spread, so it is neither reproducible nor
   contradicted by any single run — it is one draw from a distribution this file has now
   sampled three times (47–133, 20–142, and the 133 in §4.48's table).

**The shaped stages are stable, and that is worth saying because it bounds the harness.**
`loss1` reads 1 057.9–1 093.8 ms at p99 across four arm-rounds (**3 % spread**),
`rate100` 229.0–262.1 (**14 %**), `rate20` 495.1–744.2, `jitter` 138.9–216.9. A harness
whose shaped stages repeat to 3 % and whose clean stage varies 7× is telling us the clean
stage's variance is in the clean stage — not in the instrument.

**Two cells moved the wrong way and are recorded, not explained.** `rtt100` carried
0.38 Gbit/s in one arm-round against 1.94–3.32 in the others, and `loss5`'s p99 reaches
3 108 ms in one. Both are single samples of the harshest conditions; neither has a
mechanism yet, and attaching one now would be the same mistake §4.50 made with units.

**This is the baseline the next comparison uses.** Fingerprint `1e7f4ed13553b4e8`,
`--scale 0.2`, two rounds with the A/A inside. Any claim about a carrier change is read
against *these* numbers, and against the run's own A/A — never against §3's full-scale
sweep, which was produced by a method that would now refuse three of its own cells.

### 4.53 The gate can no longer pass a stage that carried nothing (2026-11-06)

§4.49's hole is closed: a staged cell that dialed bulk and moved **zero** bytes is now
`ok=False` with its own reason, so the gate refuses it as an unmeasured round instead of
judging its (excellent) latency.

The rule is exact, not a threshold — `_carried_nothing` is `carried <= 0.0` and nothing
else, because a **rate-shaped** stage legitimately reads small: `rate20` carrying
18.4 Mbit/s against its 20 Mbit/s shaper must not be mistaken for a dead stage, and the
smallest honest value in a staged run is a shaper's own rate, orders of magnitude above
zero. A missing metric is not this case either; it is a typed absence and already carries
its own reason. Five selfcheck cases pin the boundary (0.0 fails; 0.001, 0.0197, `streams
= 0` and absent do not), and the rejection lives in `_iperf_cell` — the cell that knows
how many streams were dialed is the cell that fails — rather than at the call site,
which also kept `_measure_stage` inside its statement budget.

**Verified end to end** with the pre-fix binary, which is the build that produced these
cells: `rate100` in **both** rounds and `clean#2` in round 0 came out `ok=False` with

> the bulk spine carried no bytes in this stage: 4 stream(s) were dialed and the sender
> reported zero, so this stage's latency is an idle path's

while `rate20` at 18.4 Mbit/s was left alone. And the gate now fails that run:

```
gate: 2 FAILURE(S)
  - coverage: molehill / timeline / rate100 produced no measured round (staged)
  - SLO: molehill / timeline[clean] p99 130.30245 ms > 50.0 ms on a clean stage
```

The first line is this section's claim, measured: a dead stage is now an *unmeasured*
round, not a fast one.

**A limit worth stating.** Coverage is satisfied by **any one** round, so a stage that
dies in some rounds but not others still passes — `clean#2` died in one of its two rounds
here and the gate did not complain about it. That is deliberate (a retried stage is
normal) but it means a 50 %-dead stage is still a 50 %-dead measurement. If that starts
to matter, the rule to consider is a floor on the *share* of rounds a cell must measure,
not one on a single round.

**And the method moved, which is the last bench debt.** `workloads.py` is a method module,
so this change moves the fingerprint: §3's numbers were produced by a method that would
now refuse those cells. The next sweep must record its baseline from this stricter
method — and the difference is not cosmetic, since three of the cells in §4.52's own
guard run are now failures rather than readings.

### 4.52 The abandon hold passes its guard: no cost on `clean`/`rtt100`, and the leak's signature is gone (2026-11-06)

The two-arm guard §4.51 owed: the patched binary against the pre-fix build (`git show
99ca30a:src/transport/mhd/conn.rs`, built and kept at `~/tmp/bin/molehill-nofix`), one
scaled timeline, two rounds each, in one run.

| stage | patched (median) | pre-fix (median) | change |
|---|---|---|---|
| `clean` | **7.95 Gbit/s** | 7.47 | +6.4 % |
| `rtt100` | **3.400** | 2.505 | +36 % |
| `clean#2` | **3.64** | 2.10 | +73 % |
| `throughput_gbps`, all cells | — | — | **+72.7 %** (directional, n=[2,2]) |

And the leak's own signature: the **pre-fix** arm carried **zero bytes** in `jitter`,
`rate100` and `rate20` in round 1, which the patched arm did in neither round. The
pre-registered pass was "the patched arm at or above the pre-fix arm on `clean` and
`rtt100`" and its rollback was the opposite; **the guard passes and the rollback did not
fire**.

**The latency numbers go the other way, and that is the point, not a regression.**
`clean#2`'s `rtt_p99_ms` is **+293 %** for the patched arm and `rtt_max_ms` **+475 %**.
Those stages are now *carrying bulk* where they carried nothing: a loaded stage has a
longer tail than an empty one, and comparing them is the comparison §4.49 was about. The
honest statement is that the fix trades "an empty stage that looked fast" for "a stage
that measures". Anyone reading §3.1's `clean#2` 23.08 ms against a future run must expect
that number to be larger and to mean more.

**One bench defect came out of this run and is fixed with it.** The report crashed with
`StatisticsError: no median for empty data`, because `verdict` guarded against an *empty
arm* but not against two arms whose samples share **no round**: `_aligned` then returned
nothing and `statistics.median([])` took the whole report down. A comparison that cannot
be paired is now an `unavailable` verdict whose reason names both arms' rounds, which is
the shape every reader of this file already knows.

### 4.51 The bounded abandon works: `buffered` stops being monotone and the last stage carries 6.28 Gbit/s again (2026-11-06)

§4.50's fix was built and measured in the same round: `StreamState.abandoned_at`, a
30-second `ABANDON_HOLD`, and `Conn::reap_abandoned` called from `Conn::on_timeout` — the
timer the pump already drives, so no new clock read on a hot path and no new task. A
stream abandoned with unacknowledged bytes is retired after the hold with a reset, and its
bytes come out of `buffered`. One unit test pins both halves (`an_abandoned_stream_
releases_its_buffer_after_the_hold`: unchanged at one second, released exactly at the
hold).

Same scaled timeline, before and after:

| stage | what the condition admits | carried before | carried after | `buffered` after |
|---|---|---|---|---|
| `clean` | the path | 10 260 Mbit/s | 9 334 | 8.3 MB |
| `rtt100` | a 100 ms path | 3 327 | 1 678 | 65 KB |
| `loss1` | 1 % loss, 10 ms | 7 715 | 6 524 | **5.3 MB** |
| `loss5` | 5 % loss, 100 ms | 218 | 19.4 | **150 KB** |
| `rate100` | 100 Mbit/s | **94** | **93** | **2.9 KB** |
| `rate20` | 20 Mbit/s | **0 (zero bytes)** | **19.7** | **2.9 KB** |
| `jitter` | 20 ± 10 ms | 4 763 | 3 635 | **2.1 MB** |
| `clean#2` | the path | **0 (zero bytes)** | **6 273** | 15.0 MB |

Eight reaps logged. **The leak is gone** — `buffered` returns to kilobytes after every
shaped stage instead of accumulating to the ceiling — and the two stages that had carried
**zero bytes** now carry: `rate20` at 19.7 Mbit/s against its 20 Mbit/s shaper, and
`clean#2` at 6 273 Mbit/s against no shaper at all. `clean#2` becomes a measurement again,
which was the pre-registered test that could actually be met.

**And the criterion that could not.** The pass as written asked for `loss5`, `rate100`,
`rate20` and `clean#2` all above 4 Gbit/s — which no 20 Mbit/s or 100 Mbit/s shaper can
satisfy, so that half of it was a specification error rather than a result. Against the
right question — "does each stage carry what its condition admits" — `rate20` (19.7 of 20),
`rate100` (93 of 100) and `clean#2` (6 273, unshaped) do; `loss5` reads 19.4 against 218 in
the same stage before the fix, and one run each cannot say whether that is the fix or the
stage's own variability, which is 5 % loss at 100 ms and the most brutal condition here.

**What is deliberately not claimed.** The pre-registered guard — `clean` and `rtt100`
unchanged within the run's own A/A — was not measured: this run had one arm and one
round. `clean` read 9 334 against 10 260 before, and `rtt100` 1 678 against 3 327, and at
this cell's known floor neither difference is interpretable. A fix that only *releases*
budget cannot reduce throughput by construction, but construction is not a measurement.

**Next experiment, pre-registered before it runs.** The guard this round owed: two arms,
the patched binary against the previous commit's, one scaled timeline, **four rounds**,
each arm's stages compared in the same run.

- **Pass**: the patched arm's `clean` and `rtt100` throughput medians are at or above the
  unpatched arm's (the fix releases budget and must not cost any), and its `clean#2`
  throughput is above the unpatched arm's by more than the run's A/A spread.
- **Rollback**: `clean` or `rtt100` below the unpatched arm by more than that spread —
  which would mean the hold is reaping streams that were still delivering.
- **Then, as its own question, with the right denominator**: whether each shaped stage
  carries what its condition admits. Before the fix `rate20` carried 0 bytes against a
  20 Mbit/s shaper and `clean#2` 0 against none; after it they read 19.7 and 6 273. What
  is left unexplained is `loss5`: 218 Mbit/s before the fix and 19.4 after, in one run
  each, on the harshest condition here. That needs rounds, not a rewording — and it is
  the one shaped cell where the fix may have cost something.

### 4.50 Abandoned streams never give their buffer back, and two stages carried zero bytes (2026-11-06)

§4.49 asked which stream was reset and why the pool did not help. The answer is worse
and simpler: **nothing was reset, nothing died, and the connection's buffer budget leaked
until it was full.** `buffered` per stage, one scaled timeline, server lanes; the third
column is what the stage's own shaper admits, because **a rate class is supposed to read
its own rate** and a table in Gbit/s hides that:

| stage | what the condition admits | carried | `buffered` p50 |
|---|---|---|---|
| `clean` | the path | 10 260 Mbit/s | 5.4 MB |
| `rtt100` | a 100 ms path | 3 327 Mbit/s | 65 KB |
| `loss1` | 1 % loss, 10 ms | 7 715 Mbit/s | **17.0 MB** |
| `loss5` | 5 % loss, 100 ms | 218 Mbit/s | **31.7 MB** |
| `rate100` | 100 Mbit/s | **94 Mbit/s** | 31.7 MB |
| `rate20` | 20 Mbit/s | **0 (zero bytes)** | 31.7 MB |
| `jitter` | 20 ± 10 ms | 4 763 Mbit/s | **33.5 MB** (the ceiling) |
| `clean#2` | the path | **0 (zero bytes)** | 33.5 MB |

So the leak starves exactly the stages that must carry **unshaped volume** — `rate20`, whose
shaper admits 20 Mbit/s and which carried nothing at all, and `clean#2`, which has no
shaper — while a rate class that admits little hides the defect behind its own ceiling.

**And the correction, because the first version of this section got it wrong.** It said
"the run is over at its third stage and every throughput number after it is on a dead
spine". That is false: the shaped stages carried their shapers' rates throughout, and
§4.2's shaped-cell judgements were measured on a spine that was carrying. What is true and
was measured: `buffered` climbed to the ceiling, two stages carried **zero bytes**, and
after the fix (§4.51) both carry their expected rates.

**The mechanism is in `Conn::settle`.** A stream retires only when
`abandoned && fin_sent && send.unacked() == 0`, and `fin_sent` itself requires
`unacked == 0`, so a stream whose peer never acknowledges cannot retire — and its
`send.held()` bytes stay in `Conn::buffered` for the lane's life. `Conn::write` computes
its third bound from exactly that number:

```rust
let held = MAX_BUFFERED.saturating_sub(self.buffered);
```

so one stream that cannot be retired refuses **every later stream on the lane**:
`held == 0`, no room, and a new bulk stream's first write is refused. The logs agree
from the other end: the four groups of the bulk spine are the only ones that never
finish (1 156 started, 1 152 finished), each read 16–40 KB from its visitor and then
stopped — and iperf3 sees `max_snd_wnd: 0`, because a group that cannot write to the
tunnel stops reading its visitor.

**And every stage transition creates one.** A stage ends by killing `iperf3`'s client;
the tool's group sees EOF, its `StreamIo` drops, `close_stream` runs, and the stream is
marked `abandoned` while some of its bytes are still unacknowledged. On a clean path
those acks arrive and the stream retires; **after a lossy stage they do not**, and the
lane's budget is gone for good. Four lanes, eight stages, one ceiling.

**Next experiment, pre-registered before it runs.** Bound the wait instead of trusting
it: a stream that has been `abandoned` with unacknowledged bytes for longer than
`ABANDON_HOLD` (30 s, the same order as `STRIPE_REORDER_DEADLINE`, and longer than
`recovery`'s `LOST_HOLD` of 3 s and `DEAD_AFTER` of 20 s) is retired with a reset, and its
`send.held()` is subtracted from `buffered`. Swept from the pump's existing once-a-second
pass, so no new task and no clock in `settle`.

- **Pass**: one scaled timeline in which `buffered` does not grow monotonically — the
  per-stage series returns toward its starting level — **and** `loss5`, `rate100`,
  `rate20` and `clean#2` each carry more than 4 Gbit/s (today: 0.21, 0.09, 0.0, 0.0).
  The shaped stages' *latency* is expected to get worse and that is the point: it is
  measured with load again.
- **Guard**: `clean` and `rtt100` unchanged within the run's own A/A (the clean cell is
  not near this ceiling), `rss_peak_mib` not higher (the hold releases memory earlier,
  never later), and no truncation symptom: a stream that retires with `written > acked`
  must say so on its `mhd stream closed` line, and that count must stay zero for streams
  that were **not** abandoned.
- **Rollback**: any clean-cell regression, a non-zero truncated-stream count, or
  `buffered` still monotone.

**What this changes in the plan.** §5's RACK+PTO item is judged on the shaped cells, and
the shaped cells have been empty: until this leak is fixed, no shaped-cell number in this
file describes the carrier. That item moves behind this one, and so does any capacity
claim past the first stage.

### 4.49 The last clean stage carries no bulk in five of six runs, and the gate passes it (2026-11-06)

§4.48's state diff turned into the round's most important finding, and it corrects the
section above it: **`clean#2` is fast because it is not carrying anything.**

Per-second connection state, same run, server lanes, first clean stage against last:

| | `clean` (first) | `clean#2` (last) |
|---|---|---|
| `cwnd` / `ceiling` | 262 144 / 262 144 | 262 144 / 262 144 |
| `srtt_us` p50 / p90 / max | **1 004 / 19 614 / 83 234** | **1 000 / 1 000 / 1 028** |
| `buffered` p50 / max | 5.4 MB / 16.3 MB | **33.5 MB / 33.5 MB** (the ceiling) |
| `flight` p50 / max | 118 KB / 9.6 MB | **0 / 51 B** |
| `throughput_gbps` | **10.26** | **0.0** |

And the bulk cell's own artifact says the same in iperf3's words: `end.streams[0].sender`
reads `bytes: 0, bits_per_second: 0, max_snd_wnd: 0`, and every interval reads
`snd_wnd: 0`. The visitor's TCP connection had a **zero receive window** for the whole
measured window: the tool accepted it and never read from it.

**Five of six `clean#2` cells across two runs read `0.0` Gbit/s**, all with `ok=True`, no
`iperf_failure`, and no typed absence — so the cell reports a pass:

| run | `clean` | `clean#2` |
|---|---|---|
| `state` | 10.26 | **0.0** |
| `release-aa` (4 arm-rounds) | 7.57 / 9.07 / 9.44 / 9.47 | **0.0 / 0.0 / 0.0 / 2.69** |

Two consequences, and the first is a gate defect rather than a carrier one:

1. **A stage that carried no bulk passes the SLO.** `_breaks_slo` judges `rtt_p99_ms`
   and `rtt_error_rate_pct` only, so a tool that stops forwarding after the first stage
   scores its best latency of the run on its emptiest stage — and `rtt_samples` is 639,
   because the interactive probe kept working. The gate has no check for "the bulk spine
   carried approximately nothing in a stage that asked for four streams". §10 of
   AGENTS.md already says a stage with no reading must say so with a reason; here it has
   a reading (`0.0`) and no reason.
2. **The wedge is real, reproducible, and after the shaped stages.** The server's send
   buffer is pinned at `MAX_BUFFERED` with **nothing in flight**, no lane death is logged
   (`mhd connection closed`, `mhd pump stopped`, `lane declared dead` all zero), and
   `mhd stream ended` fires 108 times — the shape of a stream that was **reset** while
   32 MB of its bytes were still queued, after which the visitor's channel is dead and
   the group's send loop parks, which is exactly the zero window iperf3 measured.

**So the two most-quoted numbers in this file need their caveat re-stated.** §3.1's
`clean#2` 23.08 ms was measured in a full sweep where the bulk presumably did run (its
throughput is not quoted per stage), and §4.28's "cold start: the last stage is at the
peers' level" now has an alternative explanation that fits every scaled run here: the
last stage is at the peers' level because it is empty. Until a sweep says otherwise, the
**only load-bearing stage is the first**, which is also the one with the 47–133 ms
spread.

**Next experiment, pre-registered before it runs.** Reproduce the wedge in one scaled
timeline with both ends' logs joined on the group id, and answer three questions in
order: (a) which stream is reset, and by which end (`mhd peer reset a stream`, the
client's `close_stream` on drop, or the group's abort); (b) what the client's group was
doing immediately before it (its `stripe source ended` / `write failed` lines); (c) why
the pool did not replace the lane — the lane never died, so the *stream* is the unit that
failed and nothing rebuilds a channel mid-group.

- **Pass**: one run names (a), (b) and (c) from the logs, with absolute stamps that agree
  on both ends for the same group. No code change is made before that.
- **A separate, bench-side fix, to be pre-registered and measured on its own**: a staged
  cell whose `streams > 0` and whose throughput is zero must fail (`ok=False`) with the
  instrument's own reason, so the gate cannot pass an empty stage. That changes the
  bench's method and its fingerprint, and it is the first bench change this cycle that
  closes a hole rather than adding a diagnostic.

### 4.48 The release timeline, measured with an A/A: one binary reads 47–133 ms on the first stage, and the last stage is empty (2026-11-06)

The scaled sweep (`--scale 0.2`), two identical arms, two rounds, stats on, **0.0 %
errors in all eight stages** — the first healthy full timeline this cycle has measured.
36 samples, 432 s per arm-round.

| stage | arm | rnd | n | p50 | p99 |
|---|---|---|---|---|---|
| `clean` | molehill | 0 / 1 | 607 / 638 | 6.19 / **3.04** | **132.95** / **47.32** |
| `clean` | molehill~aa | 0 / 1 | 628 / 618 | 10.52 / 11.95 | **80.53** / **96.79** |
| `clean#2` | molehill | 0 / 1 | 639 / 639 | 2.14 / 2.02 | **3.20** / **4.03** |
| `clean#2` | molehill~aa | 0 / 1 | 640 / 639 | 2.48 / 1.99 | 23.43 / **2.65** |

**The first stage is still there, and one binary produces both ends of it**: p99 47.3,
80.5, 96.8 and 133.0 ms across four arm-rounds of the *same* build — a 2.8× spread with
no configuration difference at all. `clean#2` reads 2.65–4.03 ms in three arms and 23.4
in the fourth — **and §4.49 shows why that is not a recovery: five of six of those cells
carried no bulk at all** (0.0 Gbit/s, a zero receive window at the visitor). So this
cell's own floor is far larger than every effect §4.41–§4.47 tried to measure, and the
~22.5 ms §3 quotes is indistinguishable from "the first stage, at some point in its
47–133 ms range" — which is also the only stage that carries load.

**And the difference between the two clean stages is a state, not a warm-up.** Per-second
interactive p50/p99, first six seconds and last four, in the same run:

```
clean   r0   5/24 10/28 4/61 5/65 4/83 8/101 … 7/59 7/33 7/39 2/2     (slow all through)
clean~aa r1 20/102 20/66 7/74 16/78 10/52 17/191 … 9/84 9/38 12/38 16/16
clean#2 r1   2/3 2/2 2/3 2/3 2/3 2/2 … 2/3 2/23 2/2 2/2                 (fast all through)
clean#2~aa r1  2/2 2/2 2/2 2/2 2/3 2/2 … 2/3 2/22 2/3 2/3
```

The first stage is slow **for its whole 30 s** and the last is fast for its whole 30 s.
It is not a cold start inside the stage: something about the *first* clean stage's state
persists and something about the run's end-state does not have it. The shaped stages in
the same run are stable to 2–25 % between arms (`rtt100` 403–511, `rate100` 102.0–103.4,
`jitter` 103.1–107.2, `rate20` 167.7–313.9, `loss1` 267.8–1 098.5, `loss5` 1 422–2 478),
so the variability is specific to the clean stages and not a property of the harness.

**Next experiment, pre-registered before it runs.** The same shape, `RUST_LOG=info,
molehill=debug` (the A/A run above had stats on but not debug, so it has no `mhd window`
lines), and read the per-second `cwnd`, `ceiling`, `srtt_us`, `buffered` and `flight` for
the first clean stage against the last, per lane.

- **Decision rule, fixed now**: whichever of those five differs between the two stages by
  more than its own **within-stage** spread is the next target; if none does, the
  difference is not in the connection's state and the next instrument is the pool's
  (lane age, placement, how many lanes a visitor's channel is opened on).
- **Guard**: nothing changes in this run — it is an instrument-only measurement, and its
  cost is one scaled timeline with one arm (≈7 minutes).
- **No release record, no gate, no tag**: still a diagnostic on a scaled timeline.

### 4.47 One stream, one hop: a 24 % throughput win and a latency null the cell cannot resolve (2026-11-06)

§5's first item was built: `spawn_single` drives a one-stripe group's two directions
from **one** task (`spawn_inner` dispatches to it when `streams.len() == 1`), over the
same 10-byte frame and the same `StripeSender`, with no reorder buffer, no reader task,
no bounded channel and no coordinator. Four rounds of the same cell, debug logging,
against the same-day baseline (`wake.json`):

| | throughput (Gbit/s) | p50 (ms) | p99 (ms) | idle-wait events |
|---|---|---|---|---|
| baseline | 7.11 / 7.22 / 9.18 / 7.05 | 2.52 / 2.81 / 21.80 / 2.50 | 22.7 / 35.1 / 99.1 / 19.5 | 830–5 458 |
| one hop | **9.96 / 8.16 / 8.01 / 9.60** | 2.73 / 3.15 / 21.85 / 11.83 | 22.4 / 57.9 / 83.5 / 81.0 | 807–4 333 |

**Throughput is a clean win and the latency is a null.** Every one-hop round is above
8.0 Gbit/s against a baseline where one round in four is; the medians are 8.9 against
7.2, **+24 %**, from deleting four tasks per visitor and the channel between two of
them. Latency is *not* separable at this n: the baseline's own p99 spans 19.5–99.1 ms
across four rounds, and the one-hop p99 (22.4–83.5) sits inside that span. Three rounds
above the pre-registered 25 ms is a fail against the criterion as written, and the
criterion cannot certify a win or a loss here — it can only refuse.

**So the change is held out of the tree, not rejected.** The patch is kept at
`~/tmp/patches/single-hop.patch` (323 lines, applies cleanly to `0315df9`), the
measurement is above, and the question is left open with its own pre-registration
rather than answered with four samples of a bimodal cell.

**The powered run answered it, and the answer is no.** Three arms, eight rounds each,
in one run: the baseline twice (the A/A twin is the floor) and the patched binary.

| arm | throughput median | range | p99 median | range |
|---|---|---|---|---|
| `molehill` (baseline) | 8.42 | 6.95–9.44 | 39.5 | 14.7–127.2 |
| `molehill~aa` (same binary) | **7.86** | 6.74–9.56 | **23.9** | 11.1–84.4 |
| `molehill~b` (one hop) | 7.46 | 6.29–8.68 | 33.1 | 22.6–73.9 |

**Two runs of one binary differ by 6.7 % of throughput and 65 % of p99.** The one-hop
arm sits inside that band and below both baseline arms on throughput, so the +24 % of
the four-round reading was an artefact of sampling this cell four times, and the p99
question is not resolvable at eight rounds either. The run's own resolution report says
so in its own words: `cost|run rtt_samples: the cell is too noisy for rounds to fix
(longer workloads, not more of them)`.

Two conclusions, both worth more than the patch:

1. **The four tasks per visitor are not on the critical path.** Deleting the
   coordinator, the reader task, the bounded channel and the grace timer changes
   nothing measurable — which is the same answer §4.37 gave from the spawn side, now
   from the other end.
2. **This cell cannot certify a change smaller than ~7 % of throughput or ~30 ms of
   p99 at eight rounds**, whatever the change. Every four-round reading in §4.41–§4.46
   has to be read against that, including the ones that looked like wins and the ones
   that looked like failures: §4.43's −25 % throughput and §4.46's 0.74 Gbit/s are
   large enough to survive it, but the medians in between are not.

### 4.46 The wake is exonerated, and the reserve does not cover the buffer ceiling (2026-11-06)

§4.45's instrument was built and run: the interval from `wake_pump()` to the pass
that follows, sampled only while the pump is parked (`Shared::park` / `Parked::end`,
`mhd-stats: how long a parked pump waited after its wake`). Over four rounds it
produced **zero samples** — and the same rounds logged **846** `idle write waited for
the pump` events on the server, so the two facts together are the answer rather than a
broken instrument: **the pump was not waiting when those writes arrived.** The wake
path cannot be blamed for a park that never happened.

Where it was instead is in the same logs:

```
one pump pass  stage="pass"   n=1 782-3 382   p50 0 us   p99 29-38 us   max 380-4 380 us
one pump pass  stage="flush"  n=12 259-16 622 p50 76 us   p99 451-971 us  max 1.0-1.7 ms
stream byte waited in the send buffer   138 samples >= 20 ms, values 39 969-45 852 us
```

Twelve to sixteen thousand flushes a second — 44 packets each, so ~530 000 packets/s —
with about **five flushes per pass**, i.e. the `sent == SEND_RUN → continue` loop. The
server's pump never parks while the bulk has data, and a byte that is *in* the send
buffer waits 40–46 ms for its turn. So the shortage is the pump's attention, and the
846 events are writes that were **refused**, not writes that waited for a wake.

**And the refusal is not an opening's.** The reserve hypothesis was written down,
built and measured in the same round — one line, an opening may overdraw the buffer
ceiling by `OPEN_RESERVE` — and **refuted by its own logs**:

```
writes refused, by the bound that refused   credit=123..886  window=3 596..11 970  aged=0  opening=0
```

`opening=0` in every round: **no opening was ever refused.** The refusals are bulk
writes hitting the congestion window, 3 600–12 000 times a second, which is what
`window = cwnd − flight = 0` looks like when the flight is 9.9 MB against a 262 144
window. The reserve was given to `credit` and to `window` and correctly withheld from
`credit` (overrunning a peer's advertised window is a protocol violation, not a
policy); the third bound was never the problem. The change was reverted, four rounds
of it kept: p99 78.8 / 26.3 / 121.0 / 39.3 against a 25 ms criterion, throughput
9.00 / 7.11 / 7.68 / **4.80**, and `idle write waited` unchanged at 582–2 989 per round
against the baseline's 830–5 458.

**So what is left is the pump's own attention, and the loop says where to look.**
Twelve to sixteen thousand flushes a second at **about five per pass** means the
`sent == SEND_RUN → continue` path runs four or five times between two `wait()` calls,
and each of those iterations **yields** the task:

```rust
if sent == SEND_RUN {
    tokio::task::yield_now().await;
    continue;
}
```

A `yield_now` on a saturated runtime returns the task to the back of a worker's queue.
At 846–5 458 writes a round waiting 5–8 ms, with ~5 yields per pass, **a millisecond per
yield is the arithmetic that fits**, and it is the only interval in the pump's own loop
this file has never measured.

**Next experiment, pre-registered before it runs.** Bound the give instead of removing
it: replace `yield_now()` with `tokio::task::coop::consume_budget().await` — the
cooperative budget yields only once a task has spent it (~128 polls), so the pump keeps
filling while the peer has budget to spare and gives up the worker on a cadence rather
than on every full run.

**Both variants have now been measured and reverted, so the give policy is a closed
lever.** Three pumps, same cell, four rounds each:

| give | p99 (ms) | throughput (Gbit/s) | idle-wait events | listener `max_gap_us` |
|---|---|---|---|---|
| `yield_now` (baseline) | 22.7 / 35.1 / 99.1 / 19.5 | 7.11 / 7.22 / 9.18 / 7.05 | 830–5 458 | 0.27–0.90 s |
| none | 355.8 / 135.3 / 26.8 / 278.7 | 3.31 / 4.97 / 6.36 / **0.74** | 1 588–3 653 | **20.3 s** |
| `consume_budget` | 36.0 / **184.1** / 48.7 / 62.9 | 6.17 / 6.42 / 5.21 / 7.32 | 2 881–4 543 | 1.56 s |

The baseline wins on both axes, and the two "cheaper" policies are worse in both
directions at once: less giving starves the dispatcher that feeds the pump its
acknowledgements, and the starvation costs more throughput than the yield ever cost
latency. A pump whose inner loop has no await point cannot be preempted at all — tokio's
scheduler is cooperative — which is why the no-yield run measures a 20-second gap and
not a millisecond one.

So the 5 ms figure is **not** reachable from this loop, and the honest reading of
§4.42–§4.46 together is that the access-latency question has moved: three bounds of the
send queue and three give policies have each been measured, and the cell's spread
survives them all. §5's next item is therefore the **structural** one — one stream, one
hop — which removes work rather than re-tuning it.

### 4.45 The visitor's request rides the busy direction, and three window bounds each trade latency for throughput (2026-11-06)

§4.44 left one question: why the median lands at 6 ms whenever the queue is bounded,
when the queue is what the bound was meant to remove. Three measurements answer it.

**Where the 6 ms is spent.** Joining the two ends' absolute stamps on the group id
(2 236 matched pairings per round, `bufcap` run, good round against slow):

| | control (ask → client start) | client open (→ finished) | transit (→ delivered at the pairing) |
|---|---|---|---|
| r2, cell p50 2.45 ms | 0 ms | **1 ms** | **0 ms** |
| r1, cell p50 7.41 ms | 0 ms | **2 ms** | **2 ms** |

The control hop is free in both, and the two legs that **are** a carrier round trip
double. `paired` = open + transit = 1 ms against 4 ms, and the echo adds the same
again. So the cell's latency is `2 × lane RTT + the visitor's own connect`, and the
lane's RTT is the queue — the visitor's request travels in the **same direction as
the bulk**, so it waits behind whatever that direction holds.

**What the queue is not.** Two candidates are excluded with the same logs:

- **Socket blocking.** `mhd pump waited on the socket` fired **zero** times in every
  round of all three binaries, so the tail never came from a full socket buffer.
- **Delayed acknowledgement.** The server attaches an ack to essentially every
  packet (`acks_attached_per_s ≈ numbers_built_per_s`, 102 000–285 000/s) and the
  receiver acks on the order of once per batch — 3 458–8 476 acks/s × ~32 datagrams
  ≈ the send rate, i.e. exactly self-consistent. There is no ack gap to close.

**What the queue really is, and why three bounds all failed the same way.** At the
baseline the 32 MiB send buffer shows up as *bulk backpressure*, not as an interactive
stall: `mhd stream writer was parked` fired **1 100–1 460 times per round with a p50
of 33–43 ms**. The pump is then always busy, so a small packet is served within a
pass — median 2.6 ms — while the deep flight occasionally costs the p99 its 100 ms.
Bound the buffer, and the same cell flips: no deep flight (p99 15–25 ms), but the pump
now has nothing to send between acknowledgements, so a small packet waits for a wake,
and the median becomes 5–7 ms. Both readings are the *same* queue seen from either
side of the bound.

So the trade is structural, and no window size escapes it: `rate = W / RTT` holds at
every `W`, so a window that is kept full **is** the queue, and the only way to have
both is to drain the queue without emptying the pipe — which needs the small packet
served *first* rather than the queue made smaller.

**Next experiment, pre-registered before it runs.** Not a scheduling class — that
idea is withdrawn before it was built, for a reason worth writing down: a packet the
scheduler builds earlier still enters the **same socket buffer**, behind everything
already handed to the kernel, so `next_stream_range` cannot reorder the wire. What the
three bounds actually exposed is the *wake*, and the baseline's own logs say what it
costs: **1 300–1 600 `idle write waited for the pump` events per round at a p50 of
6.4–6.6 ms**, against a cell median of 2.6 ms — a population large enough to be the
median of any build whose queue is shallow, and it is exactly what attempts A and B
walked into.

So the first step is an instrument, not a change: histogram the interval from
`Shared::wake_pump()` to the pump's next pass start, bounded-sampled, reported once a
second. That number decides between "the wake is slow" (fix the wake path) and "the
wake is fast and the wait is elsewhere" (the stamp pair is the same one §4.30 used to
clear the wake of suspicion on a *busy* lane — this is the same question on an idle
one, which was never asked).

- **Decision rule, fixed now**: if the wake latency's p50 is above 1 ms, the next
  change targets the wake path (candidates to be read from the code once the
  distribution exists: the dispatcher's per-batch wake ordering, the pump's
  `select!` re-arming a `sleep_until` every pass, and whether the parked pump's
  worker is the one the dispatcher occupies). If it is below 100 µs, the wait is
  downstream and the next target is the socket hand-off, not the runtime.
- **Guard for whatever follows**: the four-round shape used today, bulk inside the
  same-round A/A, no shaped-cell regression.

### 4.44 The window is the queue, and both bounds now show it (2026-11-06)

The second attempt — `held = 2 × max(cwnd, WINDOW_FLOOR) − buffered`, i.e. size the
send buffer by the window instead of the flat 32 MiB — was measured against its own
pre-registration and **also reverted**. One number passed, one failed.

| | baseline (15 rounds) | attempt A (admission) | attempt B (bounded buffer) |
|---|---|---|---|
| throughput, median | 7.87 Gbit/s | **5.86** ✗ | **8.99** ✓ |
| interactive p99 | 13–133 | 14.9–24.6 | 12.9–27.5 |
| interactive p50 | 2.5–11.8, bimodal | 2.5–11.3 | **5.4–7.4 in 3 of 4** |
| slow seconds | 29 % | 69 % ✗ | **70 %** ✗ |
| `buffered` peak | 33.5 MB | 251 352 | 513 547 |

So bounding the buffer *by the window* keeps the throughput (better than baseline,
because the queue no longer competes with the ACK path) and keeps the p99 tight —
and still lands the median at 6 ms in most rounds. The pre-registered share is ≤ 5 %
and it read 70 %, so the change goes, and the interesting part is why.

**The decomposition says the carrier's two round trips lengthened together, not one
stage.** Server histograms, same cell, good round against slow round:

| round | cell p50 | `paired` p50 (accept → channel taken) | `echo` p50 (request read → reply written) | `first` p50 |
|---|---|---|---|---|
| `bufcap` r2 | 2.45 | **1 486 µs** | **593 µs** | 25 µs |
| `bufcap` r1 | 7.41 | **4 931 µs** | **2 531 µs** | 26 µs |

Both intervals scale by 3–4× while the visitor's own first-byte delay does not move
(25 µs). `paired` is the client's channel open — spawn, lane open, prologue,
delivery — and `echo` is a request and its reply across the carrier. **One
round trip each.** So the mode is a property of the path's round-trip time, not of a
stage, and the round trip is the standing queue.

**Which puts the tradeoff on the table with numbers.** Throughput tracks the window
(256 KiB → 5.9 Gbit/s, 512 KiB → 9.0) and latency tracks the window too, because a
window that is kept full *is* a queue: `RTT ≈ min_rtt + W / rate`, and
`rate = W / RTT` is satisfied at every W, so no window size is both. The way out is
not a third window size — it is **pacing**: keep the window large enough to cover the
path and bound what is *in flight at any instant* to `rate × (min_rtt + a small
budget)`, so the queue never forms. On this cell the budget is the whole 4–6 ms.

**Next experiment, pre-registered before it runs.** A delay-bounded ceiling, no new
constant beyond the queue budget: the window may grow only while
`smoothed_rtt ≤ min_rtt + 1 ms`; the ceiling is `rate_estimate × (min_rtt + 1 ms)`
with `rate_estimate = cwnd / srtt`. At its fixed point that is `srtt = min_rtt +
1 ms`, so the cell should read a median near the path plus one millisecond, and the
rate is unchanged because the window still covers `rate × srtt`.

- **Pass**: `timeline[clean]`-shaped cell p50 ≤ 3.5 ms and p99 ≤ 15 ms over four
  rounds, **and** throughput ≥ 7.5 Gbit/s with an A/A twin in the same run.
- **Guard**: `rr-1`, `rr-16`, `bulk-1`, `bulk-pair` inside that A/A; `rss_peak_mib`
  lower; the shaped cells (`loss1`, `loss5`, `rtt100`) not worse — a delay-based
  ceiling on a path whose *own* RTT is 100 ms must not become a lock-out, so the
  budget is added to `min_rtt` and never replaces it.
- **Rollback**: throughput below the A/A floor, a shaped-cell regression, or a
  ceiling that collapses to the floor (visible as `ceiling` = 262 144 in the window
  line while a shaped stage is running).

### 4.43 The pre-registered admission fix failed its own guards, and it separates the two defects (2026-11-06)

§4.42's one-line change — `Conn::write` subtracting `self.buffered` instead of
`self.recovery.flight_bytes()` — was measured against its pre-registration and
**reverted whole**. Both rollback conditions fired.

| | before (15 rounds) | after (4 rounds) |
|---|---|---|
| interactive p50 | 2.5–11.8, bimodal | 2.48, 11.28, 10.79, 5.36 |
| interactive p99 | 13–133, **median ≈ 25** | **14.9 / 23.2 / 23.4 / 24.6** |
| slow seconds (> 5 ms p50) | 275/936 = **29 %** | 173/251 = **69 %** |
| throughput | 6.36–9.71 Gbit/s (median 7.87) | **4.49–7.21** (median 5.86) |

**The instrument check passed and is the part worth keeping.** `buffered` fell from
4–15 MB (peak 33.5 MB) to **200–251 KB, peak 251 352** — the window became the bound
on admitted data, exactly as claimed — and `srtt_max` fell from 44–123 ms to
1–8.5 ms. So §4.42's mechanism is confirmed from the other direction: **the 32 MB of
admitted-but-unpacketised data was the p99 tail**, and removing it took the worst
round from 133 ms to 25 ms.

**So the failure is not the idea, it is that the window cannot carry the path.**
With the buffer no longer hiding it, `cwnd` is the binding constraint and it is
calibrated on the wrong quantity: `window_ceiling` is `WINDOW_FLOOR × (min_rtt /
1 ms)`, and `min_rtt` **cannot go below the estimator's own granularity of 1 ms**
(`recovery::GRANULARITY`). On this cell the lane therefore gets
256 KiB × 1 = **256 KiB**, while the smoothed RTT the same logs report is 1–4 ms and
the flight spikes to 8 ms. A quarter of the throughput goes with it, and the median
rises too — the sender is now paced so tightly that the pump spends time parked, and
a small write that lands while it is parked pays the wake (the interval §4.30
measured at 5–8 ms). That is a hypothesis about the median, not a measurement; what
is measured is that both moved in the wrong direction together.

**Both defects are now named and they have one owner.** The window is sized from a
number pinned at the estimator's floor, and admission is bounded by a number that
excludes what has not been packetised. Fixing either alone makes the other worse:
raising the ceiling without bounding admission gives the buffer more room (more
tail), and bounding admission without raising the ceiling gives a quarter of the
throughput away.

**And the ceiling's own term turns out to be inert, which rules out the obvious
pair.** The first draft of the next experiment was "scale the ceiling from
`max(min_rtt, smoothed_rtt)` as well" — it cannot work, and the arithmetic is worth
writing down because it saves a run. If the ceiling were `256 KiB × hops` with
`hops = smoothed / 1 ms`, and the queue is `q = W / rate`, then `hops = q / 1 ms`,
so at a fixed point `W = 256 KiB × W / (rate × 1 ms)`, i.e. `rate = 256 KiB / 1 ms =
2.1 Gbit/s per lane` and `hops = 0`. **The estimate cannot leave the floor unless the
queue already exceeds the floor**, so on a fast path the smoothed term is inert by
construction: it is the same 256 KiB, and the throughput comes back no higher.

**Next experiment, pre-registered before it runs.** One line in `Conn::write`: keep
the old admission test (`flight`) and bound the *buffer ceiling* by the path instead
of the flat 32 MiB — `held = 2 × max(cwnd, WINDOW_FLOOR) − buffered`. The buffer
exists to hold what the window cannot yet carry, so its size belongs to the window
and not to a constant; on this cell that is **512 KiB instead of 33.5 MB, and a
standing queue of ~0.5 ms instead of 34 ms**, while still holding two windows' worth
so the pump never runs dry waiting for an acknowledgement (which is what cost the
previous attempt its 25 %).

- **Pass**: slow-second share ≤ 5 % (baseline 29 %) **and** throughput ≥ 7.5 Gbit/s
  (the pre-change median is 7.87; the failed attempt read 5.86).
- **Guard**: an A/A twin in the same run for both numbers; `rr-1`/`rr-16` and
  `bulk-1`/`bulk-pair` inside it; `rss_peak_mib` lower, not higher (a smaller buffer
  can only help, so a rise means something else moved).
- **Rollback**: throughput below the A/A floor, or the share above 10 %, or any
  shaped-cell regression — `MAX_BUFFERED`'s flat value also protects a frontier that
  stalls, and the dynamic one must keep that protection (`2 × cwnd` is still a hard
  ceiling per write, never a target).

### 4.42 The window bounds packetised bytes, not admitted ones, so 32 MiB queues behind a 256 KiB window (2026-11-06)

§4.41 left two candidate explanations for a `flight` of 3.4–12 MB against a `cwnd`
of 262 144: a real queue, or a counter inflated by re-sent ranges. A third reading
turns out to be the right one, and it is a defect rather than an accounting quirk.

The window line now carries the streams' own unacknowledged bytes (`buffered`) and
the recovery table's length beside `flight`. Four rounds, all healthy this time
(p50 2.5–4.9 ms), server lanes:

```
rnd lane  cwnd   flight_med  flight_max  buffered_med  buffered_max  pkts_max
 0    1  262144      196 356   8 752 760    13 335 310    33 543 640     7 159
 0    2  262144       59 356   6 766 584     9 801 850    16 518 560     5 016
 1    1  262144      121 410   2 611 664    13 678 516    33 543 640     1 936
 2    2  262144      182 115   8 130 690    14 491 336    33 543 640     6 848
 3    2  262144      151 088   2 975 894    15 718 132    33 543 640     2 736
```

**`buffered` sits at 4.4–15.7 MB and peaks at 33.5 MB — `MAX_BUFFERED` — while the
window is 262 144.** The difference is bytes `write()` accepted and no packet has
carried yet, and it exists because the admission test is

```rust
let window = self.cwnd.saturating_sub(self.recovery.flight_bytes());
```

`flight_bytes` counts only bytes **already handed to a packet**: it grows in
`Recovery::on_sent`, which the pump calls. A writer in a tight loop is therefore
tested against a number the pump has not updated yet, and it keeps admitting while
the answer stays "60–200 KiB free" — until the memory ceiling `MAX_BUFFERED`
(32 MiB) refuses it. **The bound on admitted data is the buffer ceiling, not the
congestion window.**

The numbers agree with that reading rather than with a real queue:
`cwnd / RTT × lanes` = 262 144 B / 1 ms × 4 = 262 MB/s ≈ **8.4 Gbit/s**, which is
the throughput this cell actually carries (7.57 Gbit/s). So the 30 MB of buffered
slack buys no throughput at all; it is standing work in front of every packet the
pump has not reached, and 32 MiB at 940 MB/s is **34 ms** — the same order as the
bad mode's per-second medians (§4.41) and of the 100 ms p99.

Why it is intermittent: how much gets buffered depends on how far the writer runs
ahead of the pump, which depends on how the two are scheduled against each other.
The pump and the writer are separate tasks on a shared runtime, so the same build
alternates between "the window is the bound" (2.5 ms flat) and "the buffer ceiling
is" (5–28 ms oscillating) — §4.41's two modes, with a mechanism.

**The fix this names, and the one it rules out.** Admission must be bounded by the
*unacknowledged bytes*, not by the packetised ones — one line in `Conn::write` —
and it should cost no throughput, because the window is already what carries the
cell. It also rules out "give the visitor's packets a priority class" as the first
move: a scheduler reorders work that should not be queued in the first place.

**Next experiment, pre-registered before it runs.** `Conn::write` subtracts
`self.buffered` (unacknowledged bytes) instead of `self.recovery.flight_bytes()`
(packetised ones), leaving `MAX_BUFFERED` as the memory ceiling. The measurement is
the per-second *mode*, not the cell's median, because §4.41 showed the median swings
2–15× between rounds:

- **Baseline, measured over 15 rounds of this cell**: **275 of 936 seconds (29 %) sit
  above a 5 ms per-second p50**, and 4 of the 15 rounds are entirely in the slow
  mode. (`interactive.json` 97 %, `modes` r0–r2 100/76/84 %, `all` r1 84 %.)
- **Pass**: over four rounds of the same cell, that share falls to **≤ 5 %**, and
  `buffered`'s per-second peak reads at most `cwnd` plus one `SEND_RUN` — the
  instrument's own check that the bound is the window again rather than the buffer.
- **Guard**: `bulk-1` and `bulk-pair` throughput inside the same round's A/A
  spread, `rr-1`/`rr-16` not worse, RSS not higher (the buffer ceiling is
  untouched, so it must not move).
- **Rollback**: any guard breached, or the share unchanged.

### 4.41 The clean cell is bimodal, and the probe set is below its own noise (2026-11-06)

§5's first item asked whether the staged tail is inter-probe contention. It is not:
**the cell has two modes, and the spread between them dwarfs any probe effect.**

One cell, four probe sets, the same binary, `cost` = one `clean` stage of 60 s with
four bulk streams, `MOLEHILL_MHD_STATS=1`, host otherwise idle:

| probe set | round | interactive n | p50 | p99 | max | err | churn p50 | churn p99 |
|---|---|---|---|---|---|---|---|---|
| interactive | 0 | 1 188 | **9.60** | **104.5** | 198.7 | 0 % | — | — |
| all three | 0 | 1 235 | 2.96 | 45.5 | 152.1 | 0 % | 3.08 | 47.5 |
| all three | 1 | 1 192 | **9.43** | **109.7** | 212.3 | 0 % | 8.71 | 121.1 |
| interactive + churn | 0 | 1 238 | 2.61 | 14.7 | 70.1 | 0 % | 2.69 | 23.9 |
| interactive + churn | 1 | 1 227 | 2.81 | 39.2 | 306.8 | 0 % | 2.90 | 39.6 |
| churn | 0 | — | — | — | — | — | 8.09 | **111.9** |
| churn | 1 | — | — | — | — | — | 2.63 | **7.5** |
| all three (repeat) | 0, 1 | 1 237 / 1 234 | 2.60 / 2.53 | 20.2 / 24.2 | — | 0 % | — | — |

**One configuration read p99 7.5 ms in one round and 111.9 in the next.** The best
cell in the table is `interactive + churn` at 14.7 and the worst is `all` at 109.7 —
so a probe-set effect, if it exists, is far inside the round-to-round scatter. The
honest reading is that **this cell cannot compare two configurations in one round
each**, which is how the last twenty rounds compared everything.

**The per-second shape separates the modes**, and it is in every cell's evidence
(`interactive_per_s`):

```
all r0      p50 3.1 3.2 2.8 3.0 3.0 3.2 … 3.0 3.7 3.0 2.8 62 s   (flat)
ichurn r0   p50 3.0 2.8 2.7 2.6 2.7 2.7 … 2.7 2.6 2.5 2.5 63 s   (flat)
all r1      p50 10.0 6.0 5.1 7.4 5.1 4.9 13.5 11.3 27.9 21.6 …   (oscillating)
interactive p50 8.2 11.5 4.9 5.0 6.4 5.5 12.5 13.4 5.2 13.4 …    (oscillating)
```

A flat ≈2.6–3.0 ms mode and an oscillating 5–28 ms mode, ten times apart at p99.

**What is excluded, and how.** Host load does not explain it: the runs recorded
`loadavg` at start, and the **quietest** run (0.86) is the 9.60 ms one while the
**busiest** (7.58) read 2.61 and 2.81. The probe set does not explain it (both modes
appear with one probe and with three). And the window ceiling is not latched: in the
healthy rounds every lane reads `ceiling = 262 144` (the floor, `srtt_us = 1000`)
with `cwnd` at that ceiling and a total bulk flight of ~400 KB, so the
`window_ceiling` feedback §4.4 describes is behaving in that mode. **The bad mode has
no window line in any existing log** — it is a `debug!` line and every run so far
logged at `info!` — so the same measurement on a bad round is the next step, with
`RUST_LOG=info,molehill=debug`.

**And the artifact §4.39 quoted is not a latency reading at all.** Counting the
surviving files of that run by kind:

```
series-molehill-interactive.ndjson   44 226 error   3 036 rtt_ms      (93.5 % failed)
series-molehill-churn.ndjson         35 758 error  38 525 setup_ms   (92.8 % failed)
series-molehill-udp.ndjson              502 loss    2 220 rtt_ms      (18.4 % lost)
```

So its "interactive p99 3.52 ms" was a percentile over the 6.5 % of attempts that
finished, in a run whose tool stopped answering after its first stage. The same
applies to §4.39's caveat, which called the rest of the window "clean#2 reading
42–1097 ms": that window was a wedge, not a shaper that had not settled.

**What follows for the plan.** The comparison unit has to be *several rounds with a
recorded per-round mode*, not one cell per configuration, and any claim about the
release cell must state the mode it was measured in. This is the same lesson as
`bulk-n`'s two-fold A/A (§4.36), now on the number the release is gated on.

### 4.40 The staged cell can be measured one probe at a time, and its absences are typed (2026-11-06)

§4.39's next step needed a switch that did not exist: the staged runner always
started all three probes. It is now a method parameter.

- **`--series interactive`** starts one probe; `--series interactive,churn` starts
  two; the default is all three (`model.STAGED_SERIES`, declared once). The set
  travels in the fingerprint like `--scale` does, because three probes running at
  once and one running alone are different measurements of the same tool.
- **An unknown name and an empty list are refused before a namespace exists.**
  The empty list is the dangerous one: it would measure nothing while the record
  said so, and every metric would read absent.
- **Every absence carries its reason** — the series was not started, it started and
  produced no sample, or every attempt failed. The middle one did not exist until
  the selfcheck asked for it: with all three series enabled and an empty window,
  every metric was absent with no explanation at all, which is the shape a dead
  probe, a lost route and an analysis bug produce equally.
- **The churn series reports its own shape** (`churn_setup`, `churn_connect`,
  `churn_echo`, `churn_errors`) as evidence, not as a metric: `churn_per_s` is a
  throughput and says nothing about the tail of what a visitor pays to arrive,
  while the probe already states all three numbers per attempt.

Two regressions in `just bench selfcheck` now pin the arithmetic: a churn or
datagram sample **cannot** reach a staged `rtt_p99_ms`, and an empty window has no
error rate, no percentile and no SLO verdict (a `0.0` in any of them turns
"nothing arrived" into "everything was fine"). 725 checks.

This is the instrument §5's first item needs; it does not yet say what the
measurement will show.

### 4.39 The staged cell's p99 is the interactive series alone, and it is not an aggregate (2026-11-06)

Every round of this cycle has optimised "the cell": `timeline[clean]`, whose p99 the gate records as
22.5 ms. The bench writes one ndjson per series per arm — `interactive`, `churn`, `udp` — and **that
breakdown had never been read.** Cutting the first clean stage by series, from a surviving artifact:

```
clean, first 7.5 s
  interactive   n=150   p50 2.52 ms   p99  3.52 ms   max   4.94
  churn         n=120   p50 2.55 ms   p99 35.62 ms   max  38.82
  udp           n=371   p50 0.77 ms   p99 24.45 ms   max  49.73
  AGGREGATE     n=641   p50 2.00 ms   p99 24.22 ms   max  49.73     <- the cell
```

**That last row is not the cell, and the correction matters more than the finding.** The staged
cell's `rtt_p99_ms` is built from the **interactive series alone**: `workloads.py`'s
`_stage_metrics` takes `rtt_p99_ms` from `interaction` (`rtt_ms`), puts churn only into
`churn_per_s`, and reports UDP as its own `udp_rtt_p99_ms` / `udp_loss_pct`. Pooling the three logs
by hand produces the 24.22 above, and its agreeing with the gate's 22.5 to a tenth is **a
coincidence of a hand-built pool, not a reproduction**. The gate never computes that number.

**So the reading is narrower, and stranger, than "the tail is churn".** The interactive and churn
series are the *same probe function*: `probes/series.py`'s `ping()` opens a fresh TCP connection,
sends 64 bytes, waits for the echo and closes — for both modes. They differ in offered rate
(interactive every 50 ms ≈ 20/s, churn 16/s) and in the metric name each writes (`rtt_ms` against
`setup_ms`). Two probes running the same code at comparable rates in the same window, reporting
3.52 and 35.62 ms at p99, is therefore **not** a statement about two paths; it is a statement about
the measurement. Either the two probe *processes* interfere, or churn's own cadence
(`next_at += period` with a catch-up sleep) turns its slow samples into a different shape.

**The cheap discriminator, before any architecture moves.** Run the staged timeline with **one
series at a time** — same binary, same topology, same window, only the probe set changed. If the
interactive p99 rises to churn's level when both run, the tail is inter-probe contention and no
carrier change removes it. If it stays near 3.5 ms alone *and* with both, then the release cell's
number was never the tail this file has been chasing, and the 22.5 ms needs a different account
entirely.

`churn-16` is unaffected by that question: its scenario states `fresh: True` with `connections=16,
requests=400`, so it really is 6 400 fresh connections, and §4.7's wording is right.

Two caveats that must travel with this:

- the window is `scale 0.05`, where a stage is 7.5 s; at that scale `clean#2` reads 42–1097 ms because
  it follows `jitter` and `rate20` too closely for the shaper to settle, so **only the first stage is
  usable at this scale**;
- the same run reports ~13 700 `interactive` and ~11 100 `churn` error events, against the gate's
  0.0 % — an artifact of the shortened stages and series startup, to be re-checked at full scale
  before anything is concluded from it.

**What this changes.** The comparison to a TCP peer must be per series: our interactive p99 is 3.52 ms
against frp's *aggregate* 2.38, which is not the same quantity. The honest question is "what does
frp's interactive series report on this cell", and answering it needs `just bench peers fetch` (its
binaries are missing) and one run.

**And the methodological lesson, corrected.** The first version of this section said a metric that
aggregates three workloads had been optimised for twenty rounds. It had not: the published metric
aggregates nothing. The error was the opposite one — reading three series as if they were one
because they share a stage, and then treating a hand-built pool as the gate's own arithmetic. Both
mistakes have one root, and it is the one to carry forward: **a number whose definition was never
read in the code that computes it.**

### 4.38 The live-task instrument was reverted, and the cheaper shape is named (2026-11-06)

The obvious next step from §4.37 was a *live* task count: an atomic taken at a task's first statement
and released when its future drops. It was written and then reverted whole, for two reasons worth
recording rather than hiding.

**Ownership.** Putting a guard inside each spawned future means moving `activity` and `frames` into
the async blocks, which changes three call sites' clones and — in the code that followed — pushed two
functions past the hundred-line lint (the rule this file already carries, and which this round then
broke three times).

**The lever was larger than the measurement.** The count I want is `groups × 4`, and groups are
already known: `spawn_inner` is entered once per group and returns when it ends. Two atomic
operations at those two points — no guard, no ownership change, no line added to any loop — give the
same number. That is the shape to write next, in a round with budget for it.

The revert is recorded per this objective's own rule: an experiment that does not land cleanly is
reverted whole and its measurement kept, not left half-applied because it was expensive to write.

### 4.37 The task count is a number now, and it weakens the premise (2026-11-06)

A0ak's verdict (§4.36) rested on a premise this file had only ever asserted: that this carrier puts
"several thousand tasks" on eight workers. A counter on every group task spawned — reader, send,
receive, monitor — makes it a measurement:

```
group tasks spawned   p50 52/s   p99 144/s   max 160/s      (timeline[clean], scale 0.05)
```

Four tasks per group, so about thirteen groups a second at the median. A group's own life is about a
millisecond (turnaround 0.5 ms, §4.33), which puts the **live** population in the *tens*, not the
thousands. **The premise is wrong**, and with it the simplest reading of §4.34: doubling the workers
did not help because the task table was overflowing.

Two readings survive, and they are distinguishable:

1. **the long-lived tasks** — the bulk spine's groups and the churn series' visitors live for a whole
   stage, not a millisecond, and they are the ones that compete with the pump when an interactive
   probe arrives. A spawn *rate* cannot see them; a live *count* would.
2. **not a task-count effect at all** — sixteen workers also change how the *harness's own* series
   progress, and the first stage's p99 is measured while bulk, UDP and churn all run.

One change was kept from this round because it is monotone and cannot affect any instrumented run:
the group monitor is diagnostics, and with `MOLEHILL_MHD_STATS` off a group now spawns three tasks
instead of four. Every cell in this file is measured with the switch **on**, so this neither
improves nor invalidates any number here — and saying so is the point: an unmeasured change is
recorded as unmeasured.

The next step is **A0al**: live task count, not spawn rate — an atomic incremented on spawn and
decremented on drop (a guard inside each loop), reported per second. If the live count is in the
hundreds, reading 1 stands and the fix is fewer long-lived tasks; if it is in the tens, the
sixteen-worker win has another cause and the next experiment is the same cell at 12 workers, to see
whether the effect is monotone in the worker count or a threshold.

### 4.36 A0ak verdict: the pre-registration did its job — revert the global default (2026-11-06)

The four cells at `TOKIO_WORKER_THREADS=16`, two rounds with the A/A arm:

| cell | 16 workers (median, range) | 8 workers, this session |
|---|---|---|
| `churn-16` | 10.6 ms (10.3–15.6) | 10.4 ms (10.17–11.88) |
| `rr-1` | 0.164 ms (0.154–0.174) | 0.157 ms (0.151–0.163) |
| `rr-16` | ~23 100 1/s (21 489–23 459) | 23 500 1/s (23 382–23 754) |
| **`bulk-n`** | **4.5 Gbit/s (3.3–6.5)** | **8.6 Gbit/s (6.55–10.73)** |

Three of the four are inside noise. **`bulk-n` is not**: its sixteen-worker samples sit below every
eight-worker sample taken this session, and the drop is about half. The pre-registered rule said
revert on a material throughput regression, so **`DEFAULT_WORKERS` stays 8** — the latency win at
§4.34 is real and is **not** shipped as a global default, because it costs throughput and the rule
was written before the numbers.

Two things make this a good outcome rather than a failed experiment:

1. **The rule bound the decision.** Without it, −71 % on the release cell is exactly the number that
   would have justified a one-line global change; the throughput cost would have surfaced later, on
   someone else's benchmark.
2. **It localises the fix.** Ten workers per visitor's tasks is a *task-count* problem, not a
   *worker-count* one. Eight workers were enough until this carrier put two tasks per stripe group on
   top of the pump and the stream tasks. The answer is fewer tasks (or a scheduling budget reserved
   for the pump), not more threads for everything.

`bulk-n`'s own A/A spread is two-fold (3.3–6.5 within one run), which is worth recording on its own:
that cell cannot currently resolve a 20 % difference, so any future claim about bulk needs more
rounds than a latency cell does.

### 4.35 A0ak, pre-registered before the numbers (2026-11-06)

`DEFAULT_WORKERS` is 8 in `src/main.rs`, and `min(cores, 8)` is what any machine gets. §4.34 shows
that doubling it takes 71 % off the release cell's p99 and halves both medians — but that was measured
through `TOKIO_WORKER_THREADS`, on **one** cell, and a default is a global act: it applies to every
carrier and both roles, and it spends threads and memory, which the objective ranks *below* latency
and therefore permits only if nothing else moves.

**Written before the measurement, so the numbers cannot choose the rule:**

- **Pass** — with `TOKIO_WORKER_THREADS=16` in the same round, `churn-16` p50 and p99, `rr-1`, `rr-16`
  and `bulk-n` all stay inside the run's own A/A spread (or inside 5 % of the 8-worker baseline
  where that spread is tighter), **and** the release cell's echo p99 improves by at least 30 %
  (measured: −39 %).
- **Revert** — any of those four cells regresses materially, or `rss_peak_mib` grows by more than a
  third. A latency win paid for with a throughput or memory regression is not a win under this
  objective's own ordering.

The four-cell measurement follows in the same round.

### 4.34 A0aj confirmed: the tail is runtime saturation, and the cell moves 71 % (2026-11-06)

The pre-registered test: the same cell, twice, with the only difference the runtime's worker count
(`TOKIO_WORKER_THREADS`, which both subject processes inherit). Every second's histogram averaged
over the run, so the comparison is a distribution and not one cold-start sample:

| metric | default (`min(cores, 8)` = 8) | 16 workers | change |
|---|---|---|---|
| `clean` p99, the cell | **142.97 ms** | **41.40 ms** | **−71 %** |
| echo p50 | 1 268 µs | 613 µs | −52 % |
| echo p99 | 8 128 µs | 4 989 µs | **−39 %** (the criterion was 30 %) |
| paired p50 | 2 789 µs | 1 430 µs | −49 % |
| paired p99 | 9 612 µs | 4 542 µs | −53 % |

**Confirmed on every metric and by a wider margin than the criterion asked for.** The six
milliseconds §4.33 could not place are the runtime not polling the pump while several thousand tasks
compete for eight workers; doubling the workers halves the medians and removes 71 % of the cell's
p99.

One number to keep honest: the same cell measured 24.183 ms in §4.27's paired run and 142.97 ms
here, on the same tree. **The first stage's p99 swings by six times between runs at eight workers**
— which is itself a symptom of saturation, and the reason every claim in this file now quotes a
distribution rather than a single cell when the tail is the subject.

The next step is **A0ak**: make the worker count a deliberate decision. `DEFAULT_WORKERS` is 8 in
`src/main.rs` and is not a configuration key, so raising its cap is a code change with a measured
effect — but it is a **global** default (every carrier, every role), it costs threads and memory
(phase 6's budget), and the objective puts latency above both. Measured as a change: the cell at
16 and at `min(cores, 16)`, `churn-16`, `rr-1`, `rr-16` and `bulk-n` in the same round, with the
revert rule stated before the numbers are read.

### 4.33 The pump's own work is microseconds, so the six milliseconds are the runtime (2026-11-06)

The last two candidates in the pump loop, measured on the cell that matters:

```
pass   (wait() returns -> next wait())   p50 0 µs   p99 2-3 µs   max 3-7 µs
flush  (inside that pass)                p50 18-21 µs p99 50-59   max 57-67 µs
```

and on `churn-16`, for contrast: pass p50 0 µs (p99 2, max 234), flush p50 13 µs (p99 30).

**Neither is milliseconds, so the pump is not busy and its socket is not blocking.** The write path
is one synchronous `poll_write` — queue the bytes, then `wake_pump()` — so the notify follows the
queue by a few instructions. That leaves one place: **the pump task is awake and not polled.**

Which is a statement about the runtime, not the carrier's logic, and it fits everything measured:

- the cell is slow only at its first stage, which is the busiest (§4.28);
- a stream's serving task takes 30–51 ms in the same run (`task_us`), the same starvation seen from
  another task;
- `churn-16`, with sixteen visitors, never sees it — pass p99 2 µs, flush p99 30 µs, no tail.

The carrier spawns **two tasks per stripe group** (a send loop and a receive loop) on top of the
pump and the stream-serving tasks, so four thousand concurrent visitors is a several-thousand-task
load on a runtime sized by `min(cores, 8)`. §4.9's worker experiment raised that on `churn-16`, where
it could not matter; it has never been raised on this cell.

The next step is **A0aj**, and it is cheap and decisive: run the same cell with
`TOKIO_WORKER_THREADS=16` and compare the `echo_p99` and `clean` p99. If the tail shrinks, the
mechanism is runtime saturation by task count and the structural answer is fewer tasks per visitor
rather than any change to the carrier's protocol.

### 4.32 One pass: the pump is signalled and then not polled for six milliseconds (2026-11-06)

Counting passes rather than milliseconds, as §4.31 planned — how many `poll_transmit` calls find the
idle write still queued before one carries it:

```
churn-16        p50 = 1   p99 = 1   max = 1
timeline[clean] p50 = 1   p99 = 1   max = 1      (~100 a second, every second)
```

**One, everywhere.** So the first pass that looks at that lane carries its data, and the five to eight
and a half milliseconds are **not spent inside the pump's pass**: they are spent between the data
queueing and that pass *beginning*.

Both halves of the earlier reasoning are now excluded, and what is left is precise:

- the wake arrives (signal, 100 %, §4.31);
- the first pass that sees the queue carries it (one pass, here);
- and yet the queue sits for five to eight and a half milliseconds first.

That leaves exactly one shape: **the pump task is runnable and not polled for that long** — either the
runtime is not scheduling it, or it is inside one of the two awaits in its loop that are not `wait()`
and takes milliseconds to come back. The loop has three: `flush(&socket, …)` around every send,
`yield_now()`, and `wait()`. A `flush` on a socket whose send buffer is full blocks until the socket is
writable, which under four thousand visitors is exactly the kind of thing that takes milliseconds.

The next step is **A0ai**: put a histogram on the pump loop's own awaits — how long each `flush` takes,
and how long the task spends between leaving `wait()` and arriving at the next one. If `flush` owns
five to eight milliseconds, the tail is a socket-writable stall and the answer is in the send path,
not in the carrier's scheduling at all.

### 4.31 The wake is not missed; the pump is woken and still does not serve the lane (2026-11-06)

Counting where every pump pass came from, but only when a write was already queued on an idle
connection:

```
signal=104 timer=0      signal=105 timer=0      signal=101 timer=0   (every second of the run)
```

**A hundred per second, and not one from the timer.** So the hypothesis §4.30 left open is settled on
the other side: on this cell the pump is woken by its signal every time, exactly as `churn-16` showed
(§4.9). The wake is not being missed.

Which means the pump is **woken and still does not carry the lane's data for five to eight and a half
milliseconds.** That leaves two places for the delay, and they are distinguishable:

1. **the pump's own pass** — it is awake and running passes, but `poll_transmit` does not build a
   packet for that lane for several passes (other lanes, other streams, or the run's batching order
   come first). This is the scheduling question §5's phase C was written about, arriving now from a
   measurement rather than from a design;
2. **the runtime** — the task is awake but not scheduled. `churn-16` refuted this for eight workers,
   but this cell runs four lanes' pumps plus four thousand visitors' streams, which is a different
   load.

The next step is **A0ah**: count *passes* rather than milliseconds — how many `poll_transmit` calls
elapse between a write queueing on an idle lane and the pass that carries it. One means the pump
itself was slow to run (runtime); two or more means the pass ran and chose not to serve that lane
(pass ordering).

### 4.30 The tail has a mechanism, and the logs name it: the pump does not run (2026-11-06)

Reading the per-second lines at a spike (§4.29's method) found two lines repeating through the whole
run, both from instruments that were already there:

```
mhd idle write waited for the pump  lane=3 stream=5 waited_us=6929
mhd idle write waited for the pump  lane=3 stream=5 waited_us=6568
mhd idle write waited for the pump  lane=0 stream=73 waited_us=8488     (5 393-8 514 µs, all run)
mhd stream delivered group=212 ... task_us=51224
mhd stream delivered group=258 ... task_us=30187                        (30-51 ms)
```

`idle_queued` is set when a stream's data is queued on a lane and cleared when the pump builds a
packet carrying it (`conn.rs`'s `poll_transmit`), so that line says, in its own words: **data was
queued, and no pump pass ran for five to eight and a half milliseconds.**

That is the echo tail's mechanism. §4.29 measured an echo p99 of 21–64 ms against a median of
0.66 ms; three to eight such pump waits is the whole of it. The `paired` tail's 80 ms is the other
line: a stream's serving task taking 30–51 ms (`task_us`), which is the same starvation seen from
the server's side.

So the cell's tail is **pump starvation on a lane that has nothing else to do** — the exact opposite
of the contention hypothesis this file started with, and consistent with §4.9's observation that the
pump is woken by its signal every time on `churn-16`, where it is never idle.

The next step is **A0ag**: on this cell, count where each pass comes from — `Wake::Signal` or
`Wake::Timer` — whenever an idle write was queued. If the passes that carry queued idle data are
timer-driven, the wake is being missed on idle lanes; if they are signal-driven, the pump is being
starved by something ahead of it in its own task.

### 4.29 The bracket already existed, and the cell's p99 is a tail in two places (2026-11-06)

A0ae was going to add a bracket from the visitor's accept to the group's write-back. The bracket was
already there — `FirstByte` records `accept → paired` and `paired → first byte`, `EchoStamp` records
`first byte → reply`, and both feed un-gated histograms that print every ten seconds. The data was
in the log of the round before:

```
visitor arrival, split at the pairing   n=36   paired_p50 1 501 µs   paired_p99 80 231 µs
                                              first_p50    16 µs   first_p99     34 µs
a visitor's echo after its own request  n=35   echo_p50   661 µs   echo_p99  21 656-63 778 µs
```

**Every median on the release cell is 0.6–1.5 ms. The p99 of the cell is a tail, and the tail is in
two places at once**: the pairing's own completion (80 ms at p99, 1.5 ms at p50) and the echo
(22–64 ms at p99, 0.66 ms at p50).

That rules out what §4.28 was about to test and sharpens the question. The cell's 99 ms is not a
constant cost spread over every visitor — it is a small number of visitors that each lose tens of
milliseconds, and they lose them in the carrier rather than in the probe, because both halves are
ours and both have the tail.

This is the shape the cycle opened with: an occasional multi-ten-millisecond stall in this carrier
that a median never shows. §4.6's reorder-deadline bug was one of these; §4.10's sealing deadlock was
another. The next step is **A0af**: find what the slow visitors have in common — the same lane, the
same group, a pump pass that ran long, or a channel that arrived from the pool bell — by reading the
per-second lines at the timestamps where `paired_p99` or `echo_p99` spikes.

### 4.28 The gap is cold start, and it is not in any interval we instrument (2026-11-06)

Two more candidates fell in one run. `accept_us`, un-gated and validated on `churn-16` first (p50
18 µs, p99 34, against the known 17–41), reads **25–29 µs, p99 38–49** on the release cell. The
pairing reads 1.25–1.30 ms and the turnaround 608–627 µs. So candidates ① and ② are both excluded,
and on that cell every interval this carrier owns adds to about two milliseconds.

And then the cell's own stages said something one stage-wide number had hidden (this is a
per-*stage* split of the interactive series, not a pool of several series — see §4.39):

```
clean      p50  11.31 ms   p99  99.35 ms    n = 425     (the first stage)
clean#2    p50   2.10 ms   p99   4.51 ms    n = 440     (the last, after recovery)
```

**The last stage is at the TCP peers' level** (frp 2.377, rathole 2.063) and the first is
twenty-two times worse. This is a **cold start**, and the objective's rules forbid deleting the
first stage or adding a warm-up — correctly, because the first stage is what a real deployment
looks like when traffic arrives.

The coldness is not in the three intervals either: in the first four seconds of that stage the same
histograms read turnaround 629–1 888 µs, pairing 1 257–3 570 µs, accept 18–42 µs.

So the next instrument is not another segment but a **bracket**: from the visitor's `accepted_at` to
the last byte the group writes back to it, per visitor. If that reads ~99 ms on the first stage, the
time is inside this process and the segments between are the ones nobody named; if it reads 3 ms,
the time is on the visitor's socket or in the probe, both of which are outside every stamp here.

The next step is **A0ae**: that bracket, on the first `clean` stage.

### 4.27 A0ac answered: the control arm reports 3.05 ms, so the 22.5 is ours (2026-11-06)

`--scale 0.05 --warmup-rounds 0 --no-aa` makes the release cell affordable: 6 minutes an arm instead
of 17. Both arms in one run:

```
control    timeline[clean]   3.052 ms
molehill   timeline[clean]  24.183 ms
```

**The probe's floor is three milliseconds**, and the peers sit at 2.06–2.38 on the same cell, which
is the same floor. So **candidate ① of A0ac is excluded**: the twenty-two milliseconds are ours to
explain, not an artefact of the harness. That is worth stating plainly because the last four rounds
of this file have all pointed at the harness.

The instruments on that same cell, from a molehill-only run:

```
turnaround (request read -> reply written)   p50   534-697 µs   p90 831   p99 915
pairing    (accept -> first channel taken)   p50 1 442-1 460 µs p90 1695  p99 1862
pre-flow   (group created -> first byte)     p50 679 µs       p90 7 769 p99 12 849
```

`pre-flow`'s tail is **the probe's own think-time**, not ours: it is the interval between the
probe's connect and the request it sends afterwards, and the group exists throughout it. That is
why it was 9 µs on the dev-profile run and 7.8 ms here — different probe behaviour, same code.

So on the release cell our request path is under a millisecond and the pairing is one and a half,
against a twenty-four millisecond cell. **The remaining twenty milliseconds are between the
visitor's connection being accepted and the pairing beginning** — candidate ② of A0ac, `accept_us`,
which on `churn-16` reads 17–41 µs and has never been histogrammed on this cell.

### 4.26 The release cell costs 17 minutes per arm-round, and that is arithmetic (2026-11-06)

§4.25 said a round cannot hold this cell. The schedule says why, exactly: `TIMELINES["sweep"]` is
**eight stages of 150 and 120 seconds** — 1 020 s, seventeen minutes, for one arm and one round,
and the runner's warm-up doubles it.

So a three-arm run (`control`, `molehill`, the A/A arm) is **about a hundred minutes**. But be
precise about the two attempts that looked slower than that: the cause was found afterwards and it
was **not** the schedule. A `stage`-profile run from an earlier round was still alive, competing for
the same host, so the sweep ran at half speed — host load was 2.5 with two bench runs at once. Killed
by PID and re-run alone, the same scenario finished its first arm in six minutes at `--scale 0.05`.

**The rule that costs nothing to follow: before any long measurement, list what is running and kill
by PID.** A stray run does not fail loudly, it just makes every number slower than the one before
it — and this file has already recorded that lesson in §7 without applying it here.

**The consequence is a rule, not an apology: the release cell is measured once per release, with the
sweep that the release ritual already prescribes, and never inside an optimization round.** Every
measurement a round can act on comes from the three-minute cells (`churn-16`, `rr-1`, `bulk-*`), and
any claim about `timeline` must be carried by a sweep that was started deliberately and collected
when it lands.

The control-versus-molehill sweep started last round is still running and will be collected; the
number it produces is the one A0ac needs, and it is now known to cost a hundred minutes rather than
being "slow".

### 4.25 The release cell cannot be measured in a round; and `cost` is not a latency cell (2026-11-06)

A0ac needs one number — what a control arm (no tool in the path) reports on the same `timeline`
cell — and getting it turned out to be its own finding.

**The `timeline` scenario is a sweep-class run, not a round instrument.** One arm, seven stages,
warm-up plus one measured round: a run started with two arms had not finished its **warm-up** after
twenty minutes, and a control-only run had not either. Every other cell this file uses (`churn-16`,
`rr-1`, `bulk-n`) finishes in three minutes because it is one scenario; `timeline` is seven. So
A0ac is not something a round can contain, and pretending otherwise costs a round to learn nothing.
It is now a long background run, collected when it lands.

**The cheap profile with a control arm measures something else.** `stage` runs the `cost` scenario
with arms `control, molehill, l3` in about a minute a round, and its metric is **CPU seconds per
Gbit**, not latency:

```
molehill   0.966 / 1.027 s/Gbit     (its own A/A arm: 1.088)
l3         0.713 s/Gbit
```

so this carrier spends about **forty percent more processor per gigabit** than the L3 tunnel in the
same round, and its own A/A spread is six percent. That belongs to phase 6 and is recorded here so
phase 6 does not start from a guess; it does not answer the latency question, and reading it as if
it did would be the same category error as reading `cost` as a latency cell.

### 4.24 On the release cell every interval we own is milliseconds, and the cell is not (2026-11-06)

Two more distributions on the timeline cell, both un-gated so the median can be read (the pairing
line's five-millisecond gate is what cost §4.19's dozen rounds):

```
turnaround (request read -> reply written)     p50   542 µs   p90 618   p99  730
pre-flow   (group created -> first byte)       p50     9 µs   p90  13   p99   20
pairing    (accept -> first channel taken)     p50 1 183 µs  p90 1298  p99 1503
```

**Together under two milliseconds**, against a cell whose p99 the release gate reads as 22.5. The
pre-flow number also settles a design question: a group reads its first byte nine microseconds after
it exists, so the visitor's request is never waiting on this carrier once the group is up.

So on this cell **every interval we can measure is 1–2 ms and the cell is not**, which means the
22.5 ms is spent somewhere none of these stamps covers. Three candidates remain, and they are
distinguishable next round rather than by inference: the probe's own connect-and-ask path (which a
TCP peer pays too, and which reports 2–3.5 ms), a queue in front of the accept, or a small
population of connections that the medians here cannot see.

What this rules out, and it is the useful half: the carrier's own request path is **not** where the
release cell's p99 comes from. Every structural phase in §5 that would optimise that path — A's key
schedule, C's scheduling, D's controller — is optimising under two milliseconds of a twenty-two
millisecond number.

Both instruments stay: un-gated histograms behind the stats switch, nothing on the hot path when
it is off.

### 4.23 The release cell is not the request's transit, it is the wait before it (2026-11-06)

A new instrument, the first in this file that is **per request** and not per group: a stripe group
sets an anchor when it reads the visitor's bytes and releases it when the answer is written back.
That is the interval a `timeline` visitor's latency is made of inside the carrier, and it is
readable on a long-lived cell where every per-group leg spans the whole connection (§4.22).

```
churn-16         turnaround   p50 4.3-5.3 ms   p90 ~6.2   p99 7.5-8.1     (matches the echo)
timeline[clean]  turnaround   p50 807-924 µs   p90 ~1 000 p99 949-1 022    (one 20.7 ms outlier)
```

**On the release cell a request crosses this carrier in nine tenths of a millisecond**, and the cell
reports a p99 of 22.5.

So the 22.5 ms is **not** the request's transit through a stripe group. It is the time **before** a
request can flow at all: no turnaround sample exists while a visitor is still waiting for its
channel, and that wait is what the cell is timing. This is the same constraint §4.18 found from the
other direction — the pool holds nothing, and a channel is opened on demand — arriving, at last, on
the cell that decides the release.

The instrument is kept: it is four stamps and a histogram, it costs nothing when the stats switch is
off, and it is the first number in this file that can be compared with a `timeline` cell directly.

The next step is **A0ab**: the same anchor on the *pre-flow* interval — the visitor's accept to the
first byte the group reads — which is where the 21 ms must be.

### 4.22 The release cell cannot be read with the group instruments (2026-11-06)

The release criterion is `timeline[clean]`, whose p99 is 22.5 ms against a TCP tunnel's 2–3.5, and
that cell had never been decomposed. It was attempted this round and did not get there, for a reason
worth writing down.

**The timeline workload is not the workload the instruments were built for.** The legs this file
uses are per **group**: `up` is "the group started → its first frame was handed to the carrier".
Under `churn`/`rr` a group lives for one connection's whole request and the leg is microseconds.
Under `timeline` a group can live for the entire stage, so the same code reports:

```
leg="up"  n=12 994  p50 = 32 031 000 µs   (32 seconds)
```

That is not a bug: it is the interval the definition names, and 32 s is how long those groups had
been open before their first byte was handed over. It is also why `note_first_roundtrip` cannot be
read on that cell either — the same number goes into it.

So the decomposition needs a **per-request anchor** rather than a per-group one: a stamp at the
visitor's read of this request, carried to the carrier hand-off, the far side's echo and the reply.
That is a different instrument from every one this file has, and it is the next round's whole job.

Also measured, and worth keeping: one `timeline` arm takes **six minutes** (seven stages), so a
two-arm A/A run is about thirty-five minutes. The full-profile route to this cell is a
`--scenarios timeline` run with **one** arm plus the A/A the runner adds, not with a control arm
alongside.

### 4.21 The control arm gives the zero point, and the cell's overhead finally closes (2026-11-06)

Every comparison in this file has been tool-against-tool. The bench has a **control arm with no tool
in the path at all**, and it was measured in the same round:

| arm | `churn-16` (fresh connections) | `rr-1` (reuse) |
|---|---|---|
| **control** (no tool) | **6.228 ms** | **0.045 ms** |
| molehill | 10.102 ms | 0.151 ms |
| **molehill's own cost** | **+3.87 ms** | **+0.106 ms** |

**Sixty percent of the cell is the harness.** Our carrier adds 3.9 ms to a fresh connection and a
tenth of a millisecond to a reused one.

And the 3.9 ms closes against everything measured inside it:

| term | value | source |
|---|---|---|
| pairing (accept to channel delivered) | 2.0 ms | §4.19, four-stamp join |
| channel delivered to first reply written | 4.0 ms | §4.21, six-thousand-group join |
| — of which the back-end echo, which the control also pays | ~4.8 ms | §4.12 |
| carrier hops | ~0.4 ms | §4.12 |

so our own share of that 4.0 ms is a little over a millisecond: the group's two task spawns, the
request's hop and the reply's. **2.0 + 0.4 + ~1.5 = 3.9**, which is the control delta.

Two things follow. The first is that **absolute cell numbers were overstating our cost by a factor of
about 1.6**, and every "we are N ms slower" statement in this file should be read as a difference of
two overheads, not as a property of the carrier. The second is that the remaining work on this cell is
worth at most 3.9 ms, of which the open is 1.0–1.7 ms and two attempts to remove it have lost (§4.10,
§4.20).

`frp` was **not** measured in this round: its binaries are missing from `~/tmp/bench-peers/frp`. The
peer comparison is re-run after `just bench peers fetch` before any claim that uses it.

### 4.20 A spare channel per visitor loses everywhere (2026-11-06)

With the pairing measured at 2.0 ms — all of it a channel opened *after* the visitor arrived — the
obvious design answer is to keep one ready. It was implemented behind `MOLEHILL_MHD_SPARE` (one
`DataChannelRequest::Plain` per paired visitor) and measured in the same round, four samples a cell:

| cell | spare on | this session's baseline |
|---|---|---|
| `churn-16` | 10.73–13.18 ms | 10.17–11.88 ms |
| `rr-1` | **0.208–0.210 ms** | 0.151–0.163 ms |
| `rr-16` | **20 095–20 290 1/s** | 23 382–23 754 1/s |

**It loses on every cell**: a third slower on reuse and fourteen percent slower at sixteen
connections, while the cell it was meant to help does not improve. One spare per visitor doubles the
channels the client opens, and opening a channel costs a handshake — so the belt is fed faster than
it is consumed and the cost lands on the data path.

By the rule this objective was started with — a change that does not win is reverted whole, with its
measurement kept — the code is reverted and the numbers stay here. The idea is not refuted in
principle; *one spare per visitor* is. A reserve of a fixed small depth, refilled only when the pool
is empty, is a different experiment and has not been run.

### 4.19 The five milliseconds were the tail of a gated log; the median pairing is 2.0 ms (2026-11-06)

`report_pairing` opens with `if facts.paired_us < 5_000 { return }`. **Every pairing number this
file has quoted since §4.13 is the tail of a threshold-gated sample**, not a median: 907 lines out of six
thousand four hundred pairings in one run — the slow 14 % — and 117 in an earlier one.

Measured on all of them instead, by joining four absolute stamps on the group id:

```
client open                    p50 1.0 ms
open end -> channel delivered  p50 0.0 ms   (mean 0.31, max 3)
deliver -> pairing takes it    p50 0.0 ms   (99 % in the same millisecond)
client start -> paired         p50 2.0 ms
```

**The pairing waits two milliseconds, not five.** The wake after delivery is instantaneous, the
delivery follows the client's open immediately, and the whole of it is one channel open.

So the cell's decomposition is corrected too:

| term | value | shared? |
|---|---|---|
| visitor accepted and paired | **~2.0 ms** (not 5.4) | ours |
| the first request's back-end echo | ~4.8 ms | every arm pays it |
| the request and reply's carrier hops | ~0.4 ms | ours |

and against a TCP peer's 7.2 ms the difference is **~3 ms of per-visitor setup**, of which half is
the open (§4.9) — not the six-millisecond wait this file chased for a dozen rounds.

This is the eighth time in this cycle that a threshold-gated log produced a false number, and the
worst: it cost more rounds than any other error here. The rule already exists in §7 and was
violated by the instrument that prints at five milliseconds. **A line that prints only the slow
cases may never be used to characterise a wait** — the four-stamp join, printed for every group,
is what the same question needed.

### 4.18 Superseded: "the pool is a rotating belt" was a missing join key

Deliveries were stamped and takes were not, and this section read the asymmetry as every visitor
taking somebody else's channel. The take stamp was on `try_recv` while the pairing leaves through
the `select!` arm; stamped at all three exits, deliver→take is 0.0 ms (§4.19). The "empty pool"
idea that survived is §4.20's, and it lost. Original text archived.

### 4.17 Superseded: the 3.4 ms was attributed to a wake-up that does not exist

With four absolute stamps the gap appeared to sit between delivery and the pairing's take; measured
properly (§4.19) the wake is instantaneous — 99 % of takes land in the same millisecond as the
delivery. Original text archived.

### 4.16 Superseded: the legs did not add up, because one of them was gated

The five legs summed to 2.4–2.9 ms against a reported 5.4, and this section concluded the
discrepancy was in a definition. It was: the 5.4 was a gated sample. See §4.19. Original text
archived.

### 4.15 Superseded: the pairing wait, before it was measured without a gate

This section built a decomposition of a 5.4 ms pairing wait that turned out to be the tail of a
log line that prints only past five milliseconds. §4.19 has the median over every pairing, and
§4.23–§4.24 have the release cell. Original text archived; kept for the record of how the gated
line misled a dozen rounds.

### 4.14 Superseded: the pairing request stamp was on the retry path

A stamp on `ask_missing` never fired, and this section read that as "the pairing is not a request
and a response". `ask_missing` is only the **retry**; the first request is sent directly a few lines
earlier, so the conclusion was an instrument placement error. §4.19 re-measured the interval on all
6 400 pairings. Original text archived.

### 4.13 Sealing is a net loss, and the cell's own arithmetic closes (2026-11-06)

Sealing was measured properly this time — two rounds, eight samples a cell, the A/A arm inside
the same invocation:

| cell | sealed | unsealed (this session) |
|---|---|---|
| `churn-16` | 10.17–10.43 ms | 10.27–11.88 ms |
| `rr-1` | **0.168–0.175 ms** | **0.151–0.163 ms** |
| `rr-16` | **22 591–22 718 1/s** | **23 382–23 754 1/s** |

So it is neutral on fresh connections (inside their spread) and **costs eight percent on reuse
and four on the sixteen-connection rate** — the per-packet AEAD. With §4.10's finding that it
does not move the cell either, the switch stays **off**: it is faster where nothing needs it and
slower where requests are small.

And the cell's arithmetic now closes, on this build:

```
pairing wait   5.0-6.4 ms   (server: visitor accepted -> a channel exists)
backend echo   4.8 ms       (shared with every arm, TCP tunnels included)
carrier hops   0.5 ms       (0.2 + 0.3, §4.12)
                ---------
               ~10.3 ms     = the cell's 10.2 ms
```

**The two terms that matter are the pairing wait and a backend every tool pays.** The visitor's
own connect is 0.08 ms and the transport is half a millisecond.

So the next question is entirely inside that 5–6 ms: the visitor is accepted, the server asks the
client for a channel, the client opens one (1.7 ms, §4.9), and the channel arrives. One and a
half of those five milliseconds are therefore *not* the open, and no instrument yet says where
they go — the control hop measured 0.1–1.2 ms and the placement under one.

The next step is **A0u**: split the pairing wait on the server into "request written to the
control session" and "channel delivered", against the client's own stamps for the same group —
now joinable, because both ends finally number a group the same way (§4.12).

### 4.12 The five-times disagreement was the join key; joined properly, the carrier's hops are 0.2 ms (2026-11-06)

The previous entry's two ends could not be compared because their group ids were **different
counters**: both ends called `spawn_group` without a tag, so the layer numbered its groups
locally. The protocol already carries a group id — the server names it and sends it to the client
— and the server was already passing it as the tag; the client was not. One line fixed it, and
then both ends' absolute stamps join on the group.

Six thousand four hundred groups, every one matched on both ends:

```
hop1  (request handed to the carrier -> arrives at the far side)   p50 0.0 ms  mean 0.2  max 5
echo  (far side: arrives -> echo read from the backend)            p50 5.0 ms  mean 4.8  max 9
hop2  (echo handed back -> reply arrives at the near side)         p50 0.0 ms  mean 0.3  max 5
```

**The carrier's two hops are a fifth of a millisecond each.** Everything the plan has been
chasing in the transport — the handshake, the timers, the scheduler, the acknowledgement — is
0.2 ms against a five-millisecond backend echo that every arm pays, TCP tunnels included.

The cell's ten milliseconds therefore decompose as: **~4.8 ms of backend echo (shared), ~1.7 ms
of per-visitor open (ours, and removable by sealing as §4.10 measured), ~0.2 + 0.3 ms of carrier
hops, and the rest in the visitor's own connect and pairing.** Against a TCP peer's 7.2 ms the
3.2 ms difference is almost exactly our open plus our pairing — not our transport.

That is the most complete accounting of this cell so far, and it is also the least flattering to
the plan: the transport was never the bottleneck at this cell.

### 4.11 Superseded: the data path's two ends disagreed by five times

The server's `wire` leg and the client's `echo` leg spanned the same wall-clock interval and
differed five-fold. The cause was the **join key**, not the path: both ends numbered their groups
with their own counter, so nothing could be matched event for event. Fixed in §4.12, which is the
first place in this file where two ends' intervals are comparable. Original text archived.

### 4.9 Lost wake-ups are excluded, and the shape is "two waits where the peer has one" (2026-11-06)

The tail's most plausible mechanism left was a **lost pump wake**: a write signals the
pump, but if that signal does not reach it the packet waits for the timer, and the pause is
the timer's width — exactly a wide tail over a healthy median. A counter on the pump's timer
passes answers it: a pass that begins from the timer **with bytes queued** is a lost wake.

Over a `churn-16` run: **one timer pass in the whole run, and it found nothing to send.**
The pump is woken by its signal every time. Excluded.

With that, the whole set of exclusions for the fresh-connection tail is:

| candidate | how it was excluded |
|---|---|
| the pool growing | never grows during the scenario (§4.7) |
| the connection lock | 2 µs at p99 (§4.7) |
| the open's own preface | 1 µs (§4.7) |
| the driver hop | mhd implements `PoolLane` inline; the hop is yamux's (§4.7) |
| worker threads | 16 workers read the same as 8 (§4.7) |
| the carrier's own round trip | 0.1 ms on an idle lane (§4.7) |
| a lost wake | one timer pass per run, finding nothing (§4.9) |

**Then the corrected instrument changed the picture once more, and for the better.**
`ack_at` was only ever assigned in the plain branch, so for a *carrying* open it still
held the function's entry stamp — and `ack_us` therefore measured the whole handshake.
That is why the two phases read identical. Stamped in both branches:

```
preface       p50 1 µs
open_channel  p50 1693–1743 µs
hello_ack     p50 0 µs
```

So an open is **one wait, not two**: `open_channel` — the Noise handshake, with the
hello and its acknowledgement carried inside it, which is the shape that was
already optimised. `hello_ack` is zero because the reply arrives with the
handshake, exactly as designed.

The interesting number is the ratio: **the carrier's own round trip on an idle lane
is 0.1 ms, and the handshake's is 1.7 ms.** Sixteen times, on loopback, with every
hop measured in microseconds and both wake-ups accounted for. The difference is the
per-visitor handshake itself — two X25519 exchanges and the two task wake-ups that
carry them — and that is precisely what phase **A** (lane-root keys, one handshake
per lane instead of per visitor) removes.

So the A0 series ends where the plan began, but now with a number instead of an
assumption: **1.7 ms of every fresh connection is a handshake that a lane root
would not need to run at all.**

### 4.8 Bulk has no published baseline, and is not behind where it was measured (2026-11-06)

The objective asks that `bulk-n` stay within fifteen percent of "the baseline". There is no such
baseline in the record: `benches/records/results-bench-v0.11.2.json` holds **only timeline and
capacity cells** — the `sweep` profile does not include bulk at all — and its method fingerprint
(`257343fa3fd158d3`) differs from the current one (`1421e66cb2be72f0`), because this cycle changed
the bench probes. So the criterion has never had a number to be measured against, and a
comparison against that record would be refused by the gate's own comparability rule.

What can be measured is a same-round comparison, and it says the opposite of "behind":

| cell | molehill | frp (a TCP tunnel) |
|---|---|---|
| `bulk-1` | 5.35 / 5.37 Gbit/s (one A/A sample read **1.75**) | 4.58 / 4.78 |
| `bulk-n`, eight streams | 6.55 / 6.69 / 9.18 / **10.73** | 6.14 / 6.01 |
| `bulk-pair` | 8.47 / 8.32 / 5.57 | 5.96 / 5.90 |

The medians are at or above the TCP tunnel's; the **spread is several times as wide** — the same
shape as the latency cells, where the median is at parity and the tail is ours.

This run becomes the bulk baseline for this tree: `~/tmp/baseline-dev-bulk-111.json`, dev profile,
two arms, A/A arm included. A future release compares against it (or against a freshly measured
one), not against a published record that has no bulk cells.

### 4.7 The clean cell, measured end to end (A0i–A0k)

One connection's path, from three histogram instruments on both ends of a group
(1 400–2 200 groups a second, `churn-16`, stats switch on):

| leg | what it spans | p50 | p90 |
|---|---|---|---|
| server `up` | visitor read → frame handed to the carrier | 0 µs | 0 µs |
| server `wire` | handoff → the reply frame arrives | **1 ms** (5 ms in another run) | 6 ms |
| server `down` | reply frame → written to the visitor | 0 µs | 0 µs |
| client `from_src` / `to_dest` | the far side's two hops | 0 µs | 0 µs |
| setup (A0h) | pool placement + the lane's open | 1.4 ms | — |

So neither end is slow in its own two hops, and the whole cost is the trip in
between — the carrier's posts, the client's stripe layer, the backend's echo, and
back — at four to six task hops of roughly a millisecond each.

**The cell is not that round trip.** `churn-16` is sixteen connections each making
four hundred *fresh* requests; its run took 2.87 s, so 7.2 ms per request per
connection, and the cell prints 10.4 ms. Against the 2.4 ms an uncontended
connection costs, that is a **three-to-four-fold penalty from sixteen-way
concurrency** — and the TCP peers answer the same cell in 2.1–3.5 ms, about their
uncontended cost.

**A0l priced it, and it corrected the paragraph above.** Same round, same binary,
with the A/A arm in it:

| cell | molehill | frp (a TCP peer) |
|---|---|---|
| `rr-1` — one connection, reused | **0.155 ms** | 0.162 ms |
| `rr-16` — sixteen connections, reused | 23 382–23 754 1/s | 25 552–25 714 1/s |
| `churn-16` — sixteen connections, **fresh** | **10.40–12.12 ms** | **7.20–7.35 ms** |

Two corrections follow. First, **on reuse we are at parity**: 0.155 against
0.162 ms, and 8 % behind on the sixteen-connection rate. Second, the gap on fresh
connections is **1.44×, not the three-to-four times claimed above** — that claim
compared our `churn-16` against the peers' *timeline* clean stage, which is a
different cell. Against the same cell, the peer pays 7.2 ms where we pay 10.4.

So both tunnels pay a large contention penalty for fresh connections — roughly
four times their own uncontended cost — and ours is 44 % worse than a plain TCP
tunnel's. Our own setup (1.4 ms, A0h) is a large part of that difference; the rest
is the extra hop the carrier adds per connection.

**A0m is done, and it moves the target to the tail.** The same run, per-metric:

| `churn-16` | molehill | frp |
|---|---|---|
| `rtt_p50_ms` | **6.89 / 7.32** | **6.54 / 6.55** |
| `rtt_p99_ms` | **10.40 / 12.12** | **7.20 / 7.35** |
| `setup_p50_ms` | 0.085 / 0.093 | 0.075 / 0.076 |

So the **median is at parity** — within five percent of a plain TCP tunnel — and the
`1.44×` above is entirely a **p99** difference. The connect to the tunnel, which
neither tool can avoid, is eighty microseconds for both.

The distribution is also ten times as variable: our A/A pair reads 10.40 and 12.12
for p99 against the peer's 7.20 and 7.35.

**So what remains is the tail of a fresh connection, not its typical cost** — and
that is exactly what the objective asks for ("new-connection p99 close to TCP's").
A tail that is wide only when sixteen visitors arrive at once points at
something that happens *once per burst* rather than per connection: the pool
growing (dialling a tunnel when the burst exceeds what it holds), a lane being
created, or the first group's stripe plan.

**A0n is done, and the tail is the channel open's own.** Two instruments:

1. **the pool never grew** during a `churn-16` run — not once, so growth is not
   the tail (the config sizes it up front and sixteen visitors stay inside it);
2. **every** channel open now goes into a histogram, not only the slow ones:

```
open_p50 = 1.23–1.26 ms      open_p99 = 4.23–4.56 ms
```

The median matches A0h's 1.4 ms, and the **p99 is 4.2–4.6 ms** — a 3.4 ms tail
against a 1.2 ms body. The cell's gap to the TCP peer is 3.2–4.7 ms. **The gap
is the open's tail.**

And the tail is not where the obvious suspects are: the pool's reservation never
reached a millisecond and the lane's own open never reached one either, in the same
run. So the 4.2 ms at p99 is inside the stretch between them — the stripe
placement's bookkeeping — which has only ever been measured by a threshold line
that read microseconds in a different run.

**A0o is done, and it puts the whole open in one call.** The four steps as
histograms, 2 200 opens a second:

```
entry  p50 0 µs      p99 0
used   p50 0 µs      p99 0
open   p50 1192 µs   p99 4270 µs
record p50 0 µs      p99 0
```

`open` is `tunnel.open_stream_on_distinct(...)` and it holds both the median and
the tail; the placement map and the bookkeeping are zero at every percentile.

Two more numbers pin it down. The pool's reservation (`place_us`) never reached a
millisecond even at its p99 — because it is taken **before** `complete().await` —
and `complete`'s own measurement, which this repository already had, reads
**mean 1.4 ms, max 6.9 ms** with `MOLEHILL_PLACEMENT_STATS=1`. The lane's internal
open (`hs_us`) stays under a millisecond throughout.

So the cost is **inside `complete()`'s `tunnel.open_stream()`**, between the pool's
reservation and the pump's own open, and that is the last stretch of a visitor's
setup that has never been split.

**A0p closes the chain without needing another instrument: the open is a task
hop.** `ClientTunnel::open_stream` does not open anything itself:

```rust
let (tx, rx) = oneshot::channel();
self.open_tx.send(tx).await?;   // hand the request to the driver task
rx.await?                        // wait for it to hand a stream back
```

So a fresh connection's channel open is **a request to a driver task and its
reply** — two wake-ups — while the carrier's own open inside that driver
(`hs_us`) stays under a millisecond, and the pool's reservation, the placement map
and the stripe bookkeeping are all microseconds.

That is the answer to the whole A0 series, and it explains the shape of every
number this cycle: the median of a fresh connection costs 1.4 ms because two task
hops cost about that under load, and the p99 costs 4–7 ms because a driver serving
sixteen concurrent opens takes that long to get to some of them. It is not the
handshake, not the timer, not the pool, and not the acknowledgement — each of those
was measured and excluded.

**A0q corrects that in two ways, and finds an instrument bug on the way.**

1. **mhd does not take the driver hop.** `MhdLane` implements `PoolLane` itself
   (`pump.rs:1040`), so `complete()` calls it inline; the request/reply through
   `ClientTunnel` is the **yamux** path only. The paragraph above describes the
   other carrier.
2. **`ack_us` has always read zero, by construction.** The stamp was taken
   immediately beside the measurement:

   ```rust
   let ack_at = Instant::now();
   let ack_us = ack_at.elapsed();   // zero, every time
   ```

   so the hello's round trip was never measured, and every `ack_us=0` in this
   file's history — including one read as "the acknowledgement is free" — meant
   nothing. The stamp is now taken after the hello is flushed and before the
   answer is read.

With it fixed, both phases go into histograms, and the open turns out to be
**two serial waits of the same size**:

```
open_channel  p50 1235–1246 µs   p99 4.3–4.9 ms
hello_ack     p50 1235–1244 µs   p99 4.3–4.7 ms
```

Two independent ~1.2 ms waits inside one open, where the carrier's own round trip
on an idle lane is a tenth of a millisecond. That is what a fresh connection pays
twice, and it is why its median sits at 1.4 ms above the peer's.

**A0r narrows it once more and then stops at a wall of equal numbers.** Five
phases now, and two of them are exonerated:

```
preface       p50 1 µs      p99 5 µs     (hello serialisation, closed check)
lock_wait     p50 2 µs      p99 3 µs     (the connection lock, for `open()`)
open_channel  p50 1227 µs   p99 4363 µs
hello_ack     p50 1226 µs   p99 4362 µs
channel_call  p50 22.8 ms   (one sample: the first open on a fresh lane)
```

`open_channel_with_id` is synchronous, its preface is a microsecond, its lock is
two — and `open_channel` still reads 1.2 ms. `hello_ack` is a second, independent
1.2 ms, which is also what an idle lane's round trip would not cost.

**Two unrelated waits landing on the same number is the clue**: that is the shape
of two "let the runtime run something else" hops, not of two pieces of work. The
measurement cannot separate them further with stamps on the caller's side, because
both waits are inside the callee and both are dominated by time this task is simply
not running.

**4.8 is a separate front and it closed too**: bulk has no published baseline
(the record holds only timeline and capacity), and where it was measured it is at
or above a TCP tunnel's median with a much wider spread. That baseline is now
stored as `~/tmp/baseline-dev-bulk-111.json`.

**A0s rules out the runtime as well.** The binary runs a multi-threaded runtime
with `min(cores, 8)` workers, and sixteen visitors against four to eight workers is
exactly where queueing would live. Raising it to sixteen:

```
churn-16  10.28–10.39 ms   (8 workers: 10.32–10.50)
rr-1       0.151–0.161 ms  (8 workers: 0.153–0.158)
```

**Identical.** So it is not thread starvation, not the connection lock (2 µs), not
the open's preface (1 µs), not pool growth (never happens), not the driver hop (mhd
does not take it), and not the carrier's own round trip (0.1 ms idle).

What is left is the honest state of this investigation: **a fresh connection pays
two waits of about 1.2 ms each, both inside calls this task makes, and every
internal stamp says those calls are microseconds.** Either the wait is time this
task is not scheduled — which the worker experiment argues against — or the
instrument's own clock reads are not where the cost is.

Before spending more rounds on it, the cheap experiment is a **clock sanity check**:
time a `sleep(0)`-free no-op loop and a second `Instant::now()` pair inside the same
function during the churn, to confirm the timestamps themselves behave.

The alternative was taken in the same round: §4.8 measured `bulk-n` against a TCP
tunnel in one round — parity or better at the median, several times the spread — and
found that the objective's "within fifteen percent of the baseline" has **no
published baseline to compare with**, because the record has no bulk cells.
Closing that gap is a documentation-and-ritual question, not a carrier one, and it
is now recorded with a stored baseline to compare against next time.

---

## 5. Active architecture decisions

Approved and ordered. **A is implemented and switched off** (`MOLEHILL_MHD_SEAL`,
see §4.10 and §4.13); **B, C and D are not implemented at all.** Each is a separate
commit with its own A/B and its own revert.

**A. Lane-root key derivation** (removes the per-visitor handshake). One Noise
session per lane at establishment; every mhd stream derives its own key and nonce
from `(lane handshake hash, direction, stream id)`. No second record layer, no
yamux on top, no per-stream handshake. Keys and nonces are never reused across
streams or directions; a rebuilt lane gets a fresh root and rejects replays.

**B. RACK + PTO** (splits recovery from congestion). A RACK deadline from
packets with later acknowledgement evidence, a PTO deadline from the newest
ack-eliciting packet with variance and backoff, and a dead-peer decision from
actual silence. Delete the global age gate; keep a bounded probe exemption for
openings.

**C. Priority scheduling** (openings do not queue behind bulk). Bounded quanta
per class, byte-deficit round robin inside bulk, and GSO only over packets
already decided.

**D. Delivery-rate controller** (a window that can shrink). Simplified BBR with
app-limited sampling; loss is repaired, not treated as proof of capacity loss.

**E. One stream, one hop** (a visitor's first channel stops paying for a
multiplexer it does not use). A stripe group of one is driven by a single task that
polls both directions, over the same 10-byte stripe frame and the same
`StripeSender`, so the wire does not move. The group path stays for two or more
stripes, where the reorder buffer and per-stripe readers are what the layer exists
for. The decision is *structural*, and its pass criterion is the release cell's own
percentiles, not the task count.

Memory and CPU work is **phase 6**: it may not preempt A–E, and it is judged on
`rss_peak_mib` and `cpu_s_per_gbit` against a same-round A/A.

**Order, as of this commit.** §4 carries the findings; this is the sequence only.
Rewritten on 2026-11-06 after §4.39's correction: the previous list opened with a
live-task count whose premise §4.37 had already refuted, and behind it a 12-worker
curve and a RACK rewrite that no measurement had asked for yet.

1. **Make the datagram class a feature: the UDP service layer as its producer** (§4.72,
   pre-registered there). The carrier can now carry one — a bounded queue, a transmit arm ahead
   of the streams, a receive path that counts, and a test pinning the three properties (peer
   counts it, sender's packet number does not move, recovery table unchanged) — but **nothing
   above queues one**, so the UDP data plane is still the framed-into-a-reliable-stream path and
   every measurement in this file describes that. Next: the service layer queues a datagram with
   its per-visitor `id` instead of writing a `UdpTraffic` frame into a reliable stream, and the
   receive path hands the payload to that visitor's socket. **Pass**: an `udp-pace` run carries its
   datagrams on the new class — `datagrams_in` grows at the offered rate while the lane's
   `flight`, `retransmits` and credit stay at their idle values. **Guard**: the TCP visitor's
   `clean` p50/p99 must not move (the services share a lane and its pump). **Rollback**: if the
   service layer cannot express affinity without the stream's ordering, the class cannot share a
   lane with the reliable one as designed and the increment goes.
2. **A per-visitor spare channel stays refuted** (§4.20). A fixed small reserve
   refilled only when the pool is empty is a *different* experiment and is unrun; it
   is not in this list until 1 says the pool is the constraint.
3. **B (RACK + PTO)**, judged on the shaped cells rather than the clean one:
   `loss1` and `loss5` report 1.06 s and 1.46 s of p99, which is where recovery, not
   setup, decides the number.
4. **C (priority scheduling) is conditional, not scheduled.** `OPEN_RESERVE` buys a
   *write* budget, not a packet-scheduling guarantee, so the question is open — but
   it is worth building only if a `nobulk` run (`streams: 0`) against the same
   timeline shows the bulk spine is what moves the interactive tail.
5. **D (delivery-rate) waits, and now for a measured reason.** Two estimators were built:
   one from the acknowledgement frontier (read 0.18–0.37 of the stage, reverted under its
   own pre-registration) and one from posted bytes (**0.91–1.05, §4.58, and it is per lane
   and only joins when the lanes are summed**). So a *rate is measurable* — but the pass
   says the socket already delivers what is posted, which means a pacer placed at the socket
   would be pacing a link that is not the constraint. Item 1 decides whether a controller is
   wanted at all; if the pump stops for a budget reason, the fix is in the budget, not in a
   rate.
6. **Phase 6 (memory, CPU)** stays last and stays judged on `rss_peak_mib` and
   `cpu_s_per_gbit` against a same-round A/A.

Settled and off the list: **one stream, one hop** (§4.47). Built, measured at eight
rounds with the A/A inside the run, and it does not move either axis — so the group
machinery is not on the critical path, and the patch stays in `~/tmp/patches/` rather
than in the tree.

What is **not** on this list, and why: a live-task count (the premise is refuted by
§4.37, and §4.38's cheaper shape answers a question nobody has shown to matter), a
12-worker curve and a higher global worker default (the one attempt cost `bulk-n`
48 %, §4.36), more timer constants (three refuted), yamux over mhd, and a static
spare channel per visitor (measured, loses).

**Cost of this cell, for planning.** A full `timeline` arm-round is 17 minutes and a
three-arm run about a hundred. Optimization rounds use `--scale 0.05
--warmup-rounds 0 --no-aa` (six minutes an arm); the release cell is measured once per
release with the full sweep. Before any long measurement, list what is running and
kill by PID — a stray run doubles every number without failing (§4.26).

This section is the **durable copy** of the order; the session's goal objective is
the copy that is re-read every round, and a stale objective re-asserts a plan that has already
been refuted. **Every round ends by updating both** — the objective with what finished, what is
next and any change of order (the operator asked for this explicitly), and this section with the
sequence alone. Not only when the order changes: a round that changed nothing still moves the
"next step" line, and a stale "next step" is how a plan drifts.

**Reading list for B–D.** Four implementations are checked out under
`~/tmp/mhd-references/`; each is cited where the plan uses it:

| project | file | what to take |
|---|---|---|
| msquic | `src/core/loss_detection.c` | the RACK / PTO split, and what each is allowed to declare lost |
| msquic | `src/core/bbr.c` | delivery-rate filter, app-limited sampling |
| quic-go | `internal/ackhandler/received_packet_tracker.go` | which packets force an immediate acknowledgement |
| quinn | `quinn-proto/src/connection/pacing.rs` | token-bucket pacing with a bounded burst: capacity is `window × 2 ms / rtt`, clamped to 10 ms of traffic — the shape §5.D needs |
| quiche | `quiche/src/ack.rs` | range-based acknowledgement instead of a fixed-width bitmap |

---

## 6. Negative results: do not retry these blind

Every row was measured on this repository and reverted. The reasoning lives in
`HANDOFF-archive.md`; this is the index a future experiment should check before
spending a day.

| # | attempt | measured result |
|---|---|---|
| 1 | Remove the pinned cumulative watermark (make number zero arrive) | mechanism fixed and *proved* (watermark advances to thousands); **3/12** integration runs against a 13/24 baseline — worse |
| 2 | Repair the frontier range on its own clock | 4/12; gated to the closed-window state: **0/12**, the worst result recorded |
| 3 | Widen the acknowledgement bitmap 64 → 512 numbers | 5/12, plus 56 bytes on every packet — reverted |
| 4 | Separate "held" from "delivered" so the peer can free flight | **1/12**; the flight total is load-bearing backpressure |
| 5 | Declare stale records lost on the acknowledgement path | neutral (7/12); the premise was wrong — in a deep flight every record is "old" |
| 6 | Post a lane's first data packet twice | with the condition fixed, 3/12 |
| 7 | `SEND_RUN` 8 or 96 instead of 44 | 5 530 and 7 853 slow opens, stage medians 2.5 → 13 ms |
| 8 | `AckSet::AHEAD` 16 384 | 4/8 against 5/8 |
| 9 | Round-robin or laggard-named byte prefix | 2/8 and 6/12 against 5/8 and 7/12 |
| 10 | Decoupled echo in the test harness | 6/12 — the harness was never the cause |
| 11 | Per-visitor Noise record layer, mux-over-mhd, more lanes / window / threads, static warm pool | rejected earlier; reasoning in the archive |
| 12 | `MIN_RTO_SMALL_FLIGHT` 20 → 2 ms | `churn-16` 10.2 → 9.95 ms, inside the ±0.5 ms A/A noise — the 20 ms open is not this clamp |
| 13 | `SMALL_FLIGHT_INITIAL_RTO` 20 → 2 ms | `churn-16` 10.2 → 9.5–10.7 ms, no change — not the unmeasured small-flight timer either |
| 14 | One spare data channel per visitor (`MOLEHILL_MHD_SPARE`) | loses on every cell — reuse −30 %, `rr-16` −14 %, `churn-16` unchanged; reverted (§4.20) |

One row is **not** in this table because it worked: the striped group's reorder
deadline was **2 s**, shorter than an 11.9 s carrier stall, so the reader failed
the group before the transport could finish. Thirty seconds gives **11/12**
integration runs against 13/24. That is the only structural change this cycle
that moved a number, and it moved the *bulk* stall, not the clean tail.

---

## 7. Environment and reproduction

**Tools and entry points**

- `just check` — the full chain (fmt, machete, ruff, pyright, bench selfcheck,
  clippy twice, serial tests, docs, secrets).
- `just bench doctor` — **run this first after any container restart.** The
  release binary survives in `target/` while source mtimes do not, so a stale
  build silently measures the previous tree: one `cargo build --release` turned a
  91.98 ms "failure" into a 22.50 ms pass.
- `sudo -n env MOLEHILL_MHD_STATS=1 just bench run --profile sweep --out ~/tmp/<name>.json --force`
  — the full sweep, five arms, roughly two hours at scale 1. `just bench gate
  <file>` judges it; add `--baseline <record>` for the regression half.
- `just interop`, `just l3-accept` — dialect refusal and the L3 acceptance run.

**Gate evidence on this tree**

| gate | result |
|---|---|
| `just bench gate ~/tmp/m24-sweep.json` | **PASS** — coverage 83 triples, endpoint 57 samples, SLO 4 clean cells, drowned 0, drift checked, capacity 8/8 (molehill, l3, frp, rathole) and 0/8 (nps). Revision `v0.11.2-592-gff0bae7`, fingerprint `1421e66cb2be72f0`. **That aggregate file was deleted by the §4.6 clean-up**; the run's raw per-sample artefacts survive under `~/tmp/bench-20261105-230248/` (48 series files, 146 raw iperf3 artefacts), and the gate output above is the one this document quotes. |
| `just check` | **green** at `83eb22e` (0 errors; the serial suite, both clippy passes, audit, deny, outdated, ruff, pyright and the bench selfcheck's 708 checks all pass) |
| `just interop` | **3 of 3** at `83eb22e` — new server refuses an old client and keeps serving, old server refuses a new client and says so, old server rejects an unknown dialect and survives |
| `just l3-accept` | **16 PASS, 0 FAIL** at `83eb22e` (run as root: the recipe skips with exit 77 without namespaces; `sudo -n bash bench/src/bench/utils/l3/run.sh`) |

All three were re-run on this tree rather than carried over from an ancestor: this cycle changed
the stripe layer, the stats instruments and the server's stream setup, and a gate result from
before those changes would have described a different binary.

**Host and harness traps** (each cost real time this cycle)

- **Fixed ports.** The integration test binds 2333/2334. A killed test leaves its
  binary alive holding them, and every later run then fails at startup with
  "connection refused", which reads like a product failure. Check
  `ss -ltn | grep :2333` and `pgrep -af 'deps/integration_test-'` before a batch.
- **`pkill` self-match.** A pattern that appears in the loop's own command line
  kills the loop. Match the child only (`target/debug/deps/integration_test-<hash>`).
- **Network namespaces.** A bench that dies leaves `mhbvis`/`mhbsrv`/`mhbcli`
  behind; delete them before the next run, and never delete one while a bench is
  still running — that is how one sweep was destroyed.
- **Container restarts.** The container is recreated between sessions: `/tmp` is
  wiped, load averages reset, and a sweep running under `/tmp` dies with it.
  Artifacts and evidence go to `~/tmp`.
- **Measurement discipline.** A twelve-run binary comparison resolves about 45
  points; eight runs resolve nothing. Per-round `A/A` gives the noise floor, and
  a difference inside it is not a result. Two of this cycle's three "losses" were
  population mistakes (window versus lifetime counters, one direction subtracted
  from its opposite) — name the convention in the field name, as the window line
  now does (`*_per_s` versus `*_total`).

---

## 8. Archive index

`HANDOFF-archive.md` holds the research record. Enter it by topic, not by date.

| topic | what is there |
|---|---|
| The transparent-L3 cycle | closed records from the L3 work, including the three fixes measurement refused |
| Carrier evolution | the mhd carrier's design history: lanes, pool, striping, sealing, the stride metric |
| Measurement accidents | every instrument that lied: direction subtraction, window-versus-total, stale binary, kbuild-queue, port poisoning |
| Refused experiments | per-experiment reasoning, with the numbers, for every row in §6 |
| The pre-consolidation body | the full 5,460-line `HANDOFF.md` as of `6c50265`, verbatim |

Entry point for a specific question: `grep '^## ' HANDOFF-archive.md`, then read
that section's first paragraphs — each opens with what was asked and what the
measurement answered.
