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

### 1.1 Archive: the cost gate as it stood on 2026-10-27

Kept because these numbers exist nowhere else: they are the only measurement of
`cpu_s_per_gbit` in the tree, and §5's phase 6 is judged on that metric. The plan that
follows this section (six phases, approved 2026-11-06) replaced the sequence this block
ends with; the *measurements* are still evidence.

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

**Verification so far.** `githooks/pre-push` (the whole chain: audit, deny,
outdated, the serial test suite) is green on `3e8f095`. `cargo test --lib`:
300 pass (21 in the new engine, including a 100-round-trip Noise stream and an
8 MiB stream behind a slow reader). `integration_test`: 22/22 pass with
`MOLEHILL_DATA_CARRIER=mhd` and 22/22 with `=tcp` (serial, separate runs), and
the striped bulk transfer test ran 12 times in a row. `pool_test` (default
carrier), `status_api_test`, `log_budget_test`, `session_test`,
`startup_failure_test`, `noise_keys_test` and `hot_reload_test` are green.
`pool_test` under a *forced* `tcp` still fails the three tests that use the
diagnostic cross-carrier handover hatch (the server never binds UDP when the
client declares TCP); that is a harness/diagnostic interaction, not a data-path
failure, and the forced modes the release gate names are the integration
suite's.

**Still open, in order:**

1. **The cost gate was measured and it fails — this is the stop condition.**
   Two `screen` runs with `--aa` on this host, 2026-10-27, the same method
   fingerprint (`2bdcdece60a72f0c`), binaries built from `3e8f095`; results
   (outside the tree, raw evidence beside them):
   `/home/dsh/tmp/bench-20261027-150525.json` (TCP) and
   `/home/dsh/tmp/bench-20261027-151428.json` (mhd).

   - **TCP carrier:** `l4` clean cost **0.650 s/Gbit** (11.97 Gbit/s, 7.67
     cores); `l4~aa` 0.635 (12.22 Gbit/s) — the run's own floor on that cell is
     **2.4 %**, far below the 15 % materiality floor. `l3` 0.209 s/Gbit
     (21.2 Gbit/s).
   - **mhd carrier:** the **`l4` arm does not complete**. The warm-up clean
     cell timed out after 106 s; every measured round then failed in 3–7 s with
     `iperf3 error: control socket has closed unexpectedly` (three clean
     samples, plus the `l4~aa` copy failing the same way, so the failure is the
     carrier and not one build). The `l3` arm completed at **4.21 s/Gbit**
     (0.55 Gbit/s), three samples spread 3.22–4.80 — a 49 % spread, so the run
     cannot resolve anything there either. The raw arm logs are under
     `/home/dsh/tmp/bench-20261027-151428/round1/arm-l4/` (`client.log`,
     `server.log`): registration and the four-lane mhd pool succeed, then the
     iperf3 control connection through the L4 path is lost before any
     measurement.
   - **Therefore `cpu_s_per_gbit(mhd) ≤ TCP` is NOT established**, and the l3
     number is ~20× TCP's on the same run. Per the plan's P2 stop condition,
     the sequence stops here. The remaining phases (P3b/P3c, P4, P5, P6) are
     not started, and no cost or tail claim may be made about `mhd`.
   - **Open question this leaves for the human:** the L4-over-mhd control
     connection dies under the bench's six-service, TCP+UDP-shared-port,
     namespaced topology while the integration suite passes the same path in
     one process. The next step is to reproduce that topology (or run the bench
     arm with `RUST_LOG=debug` on both daemons) and find out whether it is the
     lane's pool placement under many concurrent visitor channels, the
     shared-port service pair, or something the bench path does that loopback
     does not. A real number is not available until that cell completes.
   - The earlier note that the screen profile does not measure
     `cpu_s_per_gbit` was wrong: its `cost` scenario is the headline owner of
     that metric. The failure is the arm's clean cell, not the profile.
2. **P3b/P3c** — the unreliable datagram/L3 frame classes are in the wire but
   not yet wired into the UDP-forwarding and transparent paths; the header MAC,
   the unauthenticated-state budget, the PMTU probe, pacing, `SO_REUSEPORT`
   accounting on the client and the `SessionHealth`/carrier-rule evidence are
   not done. `mhd` streams currently carry those paths through the reliable
   stream, which is correct but loses the loss-isolation win.
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
| branch | `feat/v1.0.0` (never tagged; no release has been cut) |
| version | `Cargo.toml` `1.0.0` |
| wire dialect | **6.0**, unreleased; `SUPPORTED_PROTO_VERSIONS` is 6.0 alone |
| carrier default | `mhd`, with `Carrier::{Tcp, Mhd}` and the `"mhd"` wire tag unchanged |
| gates | `just check`, `just bench gate`, `just interop`, `just l3-accept` (§7 has the evidence and dates) |

The 1.0.0 line exists to make **mhd the carrier a user should want by default**,
not merely one that passes a gate. Priorities are fixed:
**latency > throughput > memory > processor**. Nothing about the configuration
model moves: no key is added, removed or renamed, and no default changes.

**Where the plan lives — read this first after a context loss.**

The **approved plan's original text** is `MHD-FINALIZATION-PLAN.md` in the
checkout root. It is **deliberately untracked** (`.gitignore` names it): it is the
recovery copy this file and the session objective both point at, so a compacted
or resumed session can reread the whole plan verbatim instead of reconstructing
it from summaries. Never `git add` it, never rewrite it as release copy, and
treat it as authoritative over both summaries below.

Three copies, in this order of authority:

1. `MHD-FINALIZATION-PLAN.md` — the approved plan, verbatim, untracked.
2. This file — the durable, committed record of the same decisions: §4 holds the
   measurements they rest on, §5 holds the order and its costs.
3. The session's goal objective — one paragraph so it survives a long context.

If they disagree, fix the lower one. The plan is a working document: when a
phase lands, its result is written into §4/§5 here and the plan file is updated
in place rather than amended with a second story — a superseded paragraph in an
untracked file is a paragraph nobody can date.

**The plan file was replaced whole on 2026-11-09** (third version, approved that
day). It is now the **D0–D5** plan: settle the copies, instrument the lane, choose
the fix from the readings, then judge it — with the lossy-path penalty as the
subject rather than a closed decision. The two earlier texts (the 2026-11-07
phases 0–6 and the same-day §12 P1–P5) are superseded; their measurements, closed
phases and withdrawn conclusions are **not** in the plan file any more, they are
here — §4.99–§4.160 for phases 0–3, §4.165–§4.175 for P1–P5, §4.176–§4.179 for
the three reversals. That is deliberate: a plan states what is being done, this
file states what was measured, and a fact keeps one home (§3 of `AGENTS.md`).

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

### 4.216 The burst budget fails too, and the arithmetic says why: the carrier sends 135 Mbit/s into a 100 Mbit/s pipe (2026-11-10)

A‴ — cap each pass at `min(44, max(4, cwnd/(4·MSS)))` datagrams — was implemented and measured on the shallow cell:

| arm | `throughput_gbps` | `dropped_packets` |
|---|---|---|
| rathole | 0.0957 / 0.0954 | 16 / 16 |
| frp | 0.0951 / 0.0950 | 17 / 16 |
| molehill | 0.0840 / 0.0785 | **3 111 / 4 417** |
| molehill twin | 0.0821 / 0.0836 | 1 973 / 2 621 |

Primary misses (needs ≤ 100) and the throughput guard misses too. **Reverted whole.**

**And the ledger from the same cell says exactly what is wrong** (§4.211's run, per lane, per second):

```
sent=16926295 rep=635379(3%) pkt=12551 ack=9305795
```

**16.9 MB posted in one second is 135 Mbit/s through a 100 Mbit/s shaper.** The carrier is not merely a little over; it is
a third over, all second, and the difference is dropped and then re-sent — which is a loop that never converges because
nothing in it ever asks the sender to slow down.

**Why the per-pass cap did not fix it**: a budget per pass bounds a *burst*, not a *rate*. The pump runs pass after pass
with no gap, so four datagrams per pass at pump speed is the same throughput as forty-four. **Pacing is a function of
time, and nothing in this carrier is a function of time.**

**And why the BDP ceiling did not fix it either** (§4.215): on a 200 ms path at 100 Mbit/s the bandwidth-delay product is
**2.5 MB**, and the shaper's queue is a few dozen kilobytes. A window of one BDP is therefore a window a hundred times
the queue — correct for a path with a deep buffer, ruinous on a shallow one. `Rtt::min` times the rate is the *path's*
capacity, not the *queue's*, and the two are not the same number here.

**The mechanism that is missing is a pacer**: a token bucket at the measured delivery rate, so the sender's rate is a
function of time and of the path, not of how fast the pump happens to loop. That is a different mechanism from all five
tried so far (window gate, repair budget, loss halving, BDP cap, per-pass burst), and it is the one the arithmetic above
points at directly.

**Committed: nothing.** The tree is §4.211's state, 107 tests green.

### 4.215 The delay ceiling barely binds either, so the window is not what fills the queue (2026-11-10)

A″ — cap `cwnd` at the bandwidth-delay product (`Rtt::min × measured delivery rate`, gain 1.0), with a `held` counter
so the same run could say whether it engaged — was implemented and measured on the shallow cell:

| arm | `throughput_gbps` | `dropped_packets` | `held` (max per lane) |
|---|---|---|---|
| rathole | 0.0955 / 0.0956 | 16 / 16 | — |
| frp | 0.0952 / 0.0953 | 16 / 16 | — |
| molehill | 0.0852 / 0.0828 | **7 294 / 2 298** | **19** |
| molehill twin | 0.0820 / 0.0824 | 1 737 / 6 868 | — |

Primary misses by two orders, guard (≥ 0.090) misses. **Reverted whole.**

**Three instruments now agree, and that is the result.** The loss halving fired **once** per lane (§4.214); the BDP cap
held the window back on **19** acknowledgements in a whole lane (§4.215); the repair budget moved the drops half the
distance and no further (§4.213). A `cwnd` that no controller can move is a `cwnd` that was never the binding
constraint on this cell — the sender is not window-limited there, so shrinking the window neither helps nor is even
felt.

**What that leaves, stated as a hypothesis with a test.** If the sender is not limited by bytes in flight, it is limited
by *how it posts*: the pump emits up to `SEND_RUN` (44) datagrams back to back in one pass, a ~59 KB burst, and a
shallow shaper queue has to absorb all of it or drop the tail. TCP, which both peers use, paces by the acknowledgement
clock and never hands a queue that shape. That is a different mechanism from every attempt so far — burst shaping, not
window regulation — and it is testable in one cell in two minutes.

**Committed: nothing.** The tree is §4.211's state, 107 tests green.

### 4.214 The loss response is right and barely fires, which is the finding (2026-11-10)

A′ — halve `cwnd` on a declared loss, at most once per smoothed round trip, floor two segments, slow start latched off
— was implemented, with a `cuts` counter in the lane ledger so the same run could prove it engaged. Two design
decisions came out of the unit suite before any bench time:

- **A probe is not a loss.** The first version cut on every `on_timeout` that returned records, including the
  probe branch, and `a_noise_stream_carries_a_hundred_round_trips` stalled at round 11: on a silent-but-healthy path
  the timer probes, and charging the window for that collapses it. The cut now fires only when the expiry is
  `Expiry::Loss`.
- **One hole declared by two mechanisms is one cut**, which is what the per-round-trip limit is for.

Measured on the cell the defect lives in (`rate100_shallow`, `bulk-1`, two measured rounds, stats on so the ledger
exists):

| arm | `throughput_gbps` | `dropped_packets` | `cuts` (max per lane, cumulative) |
|---|---|---|---|
| rathole | 0.0954 / 0.0955 | 17 / 16 | — |
| frp | 0.0952 / 0.0952 | 17 / 16 | — |
| molehill | **0.0827 / 0.0824** | **6 878 / 7 078** | **1** |
| molehill twin | 0.0838 / 0.0825 | 1 867 / 7 030 | 1 |

The primary misses by two orders of magnitude and the throughput guard (≥ 0.090) misses. **Reverted whole.**

**But the `cuts` column is the result.** A lane runs for sixty seconds at ~1 700 packets per window and the rule fires
**once in its whole life**. So the miss is not "the rule is wrong" — it is that **the loss signal arrives too rarely to
steer the window at all** on this cell. The shaper overflows, the carrier does not notice, the window keeps whatever it
had, and the next second overflows again. Every previous candidate on the declaration side (§4.186–§4.213) was trying to
tune a signal that this cell barely generates.

**What that points at.** A window that must be right *before* the first loss needs a signal that does not wait for one:
either a delay-based bound (keep `cwnd` at the bandwidth-delay product the acknowledgements measure, and treat a growing
queue as the brake), or an explicit ceiling from the path's own queue estimate. mhd has the RTT estimate already
(`Rtt::min` is there for the window ceiling) and no delay-based control at all.

**Committed: nothing.** The tree is §4.211's state, 107 tests green.

### 4.213 The repair budget halves the drops and misses, and the reason points somewhere else (2026-11-10)

The budget from §4.212 — repairs may have at most `cwnd/2` outstanding, counted apart from `flight`, never blocked —
was implemented and measured on the cell the defect lives in:

| arm | `throughput_gbps` | `dropped_packets` |
|---|---|---|
| rathole | 0.0955 / 0.0952 | 60 / 16 |
| frp | 0.0952 / 0.0951 | 16 / 16 |
| molehill (with the budget) | **0.0800 / 0.0627** | **1 373 / 1 068** |
| molehill twin | 0.0725 / 0.0843 | 1 228 / 7 078 |

Before the budget the same cell read 0.0829 / 0.0838 with **2 261 / 2 465** drops. So the primary moved **in the right
direction and by half**, and that is not enough: the criterion is *the same order as the peers' 16–17*, and 1 068–1 373
is still two orders away; the throughput guard (≥ 95 % of a peer = 0.090) misses as well. **Reverted whole**, and the
unit suite never even noticed — the five-percent-loss transfer still delivered, so the budget does not deadlock the way
the window gate did.

**What the number says instead.** A repair budget bounds one of the two streams that fill the queue, and the other one
is the one nobody has touched: **new data**. The window on this cell is around **2.3 MB** — roughly 1 700 packets — on a
path whose queue is a hundred-odd packets deep. Nothing in this carrier reduces that window when packets are lost: it
grows on acknowledgements and it is cut by nothing. On a deep path that is merely optimistic; on a shallow one it
*guarantees* overflow, and every drop this cell records is that guarantee being kept.

**So the missing mechanism is a loss response, not a repair budget.** mhd has never had one — the summary in the plan
says it plainly ("nothing cuts cwnd on loss"), and four rounds of work on the *recovery* side were changes to what
happens after a declaration while the declaration's *cause* sat untouched on the sending side. The textbook rule is a
halving per round trip on a declared loss, and it is the next candidate; it is a different mechanism from every attempt
so far, so it gets its own pre-registration.

**Committed: nothing.** The tree is §4.211's state, 107 tests green.

### 4.212 A's first part dies in the unit test, which is cheaper than the bench (2026-11-10)

A's pre-registration says repairs must be inside the congestion window. Implemented literally — the range picker
refuses a repair when `cwnd − flight == 0` — the suite answered in twelve seconds:

```
five_percent_loss_on_a_fake_link_delivers_without_storming:
  every byte arrives even when one in twenty is dropped:
  sent=128155 rep=29678(23%) pkt=95 ack=98477 num=121410 pfx=6745 late=29678 loss=2t/20m
  left: 98477  right: 131072
```

**Three quarters of the transfer arrives and then it stops.** The reason is structural, and it is the same one §4.188
measured from the other side: `flight_bytes` **includes the repair records**, so gating a repair on
`flight < cwnd` waits for room that only the repair itself could make. A repair is not new data competing for the
window — it is the *same* bytes, already counted, being sent again. Blocking it asks the window to open while forbidding
the only thing that can open it.

So part 1 as written is **falsified at the unit level, in one test, before any bench time**: cheaper and more decisive
than §4.188's four-arm run, and it says why that run failed too.

**What the shallow-queue defect needs instead.** The exemption is not the problem in itself — a repair has to be allowed
to overtake — the problem is that nothing bounds how *much* repair traffic a lane puts on a shallow queue. Two levers
that do not close the loop:

1. **A repair budget proportional to `cwnd`**, counted separately from `flight` (e.g. at most `cwnd/2` bytes of repair
   in flight at once), so repairs are bounded but never blocked by the window they are trying to open;
2. **Pacing the repairs** — the picker already runs once per pass, so a byte-per-pass budget per lane sets the rate
   directly, and the shallow queue's depth is what decides whether that is enough.

Both are written here rather than tried in the same round: the first was falsified by a unit test in one minute, and the
second is a different mechanism that deserves its own pre-registration.

**Committed: nothing.** The tree is §4.211's state, 107 tests green.

### 4.211 What B's fix changed in the acceptance cells, and what it did not (2026-11-10)

Re-measured on the fixed engine, before designing A:

| cell | reading after the fix | before |
|---|---|---|
| `loss5`, `--scale 2`, 3 arms × 3 rounds | candidate **0.425 / 0.542 / 0.467**, duplicates 13 / 13 / 54 %; twin 0.361–0.432; pre-change binary 0.420–0.515 | candidate 0.055–0.118, duplicates 55–91 % |
| `rate100_shallow` vs rathole/frp | molehill **0.0829 / 0.0838**, **drops 2 261 / 2 465**; twin 0.0826–0.0853, drops 2 450–6 726; peers 0.0951–0.0955, drops **16–17** | 0.0688–0.0833, drops 1 605–4 664 |
| clean `bulk-n` | molehill **12.16 / 12.05**, twin 12.86 / 12.19, control 65.1 / 65.8 | 13.17 / 13.02, twin 13.23 / 13.35 |
| `transport` | open 3.5 µs, first byte 74.5 µs, recovery 4.5 µs, 0 errors | 3.0 / 73.5 / 4.5 / 0 |

**The storm's fuel is gone**: every arm of the `loss5` run has `num` retirement in the millions and `cwnd` growing to
**2.2–2.4 MB**, where before the fix the storming lane's `cwnd` was frozen at 13 480 and its `num` was zero all run.
The B-fix's own mechanism is what §4.210 measured directly (refusals 13 → 0, `first_seen` 0 everywhere, `below`
advancing), and the cell now reads healthy on all three arms.

**What it did not fix, and what A is for**: on `rate100_shallow` the carrier still drops **two orders of magnitude more**
than either peer (2 261–2 465 against 16–17) and carries ~12 % less — the shallow queue is still being overflowed by the
carrier's own repairs, which remain exempt from the window.

**One guard is not met today, and it is not the fix**: clean `bulk-n` reads **12.16 / 12.05** against the plan's
13.10 and 13.35 floor, and the twin fell with it (12.86 / 12.19) while the control stayed at 65 Gbit/s. Both arms of
the same binary sit ~8 % below the earlier reading, so this is the host or the run, not the change — recorded as **not
reproduced**, to be re-established with a same-method baseline rather than quoted as a regression.

### 4.210 B is fixed: `send_gso` was fragmenting every lane's first stream packet (2026-11-10)

§4.209 left two questions: which field of the refused datagram was wrong, and why only the first packet of a lane. The
bytes answered both. What the client posted was a 17-byte credit followed by a 74-byte stream packet; what the server
refused was a **17-byte datagram whose flags said `stream`** — the first seventeen bytes of the 74-byte packet — plus a
`len=6` fragment of the same packet in the same second. A packet cut into pieces of the credit's length is not a decode
bug: it is `UDP_SEGMENT`.

**The bug, in `SendBatch::send_gso`.** It sized its segments from the **first** span, counted the spans that matched
that size, and then allowed one "shorter piece at the end" — checking only that it *was* the last one, never that it was
shorter. A run whose second datagram is *longer* than the first (the opening run of every lane: 17-byte credit, then the
first stream packet) therefore went out with `UDP_SEGMENT = 17`, and the kernel split the stream packet into 17-byte
fragments. The receiver refused them as `malformed_decode`, one lane at a time, and the number they carried — zero —
was never seen. **That is why the accepting end's first sighting was always 1 and why the cumulative frontier stayed at
zero for every product lane's whole life** (§4.182).

**The fix is one condition**: the tail must be *shorter* than the segment, or the batch is refused and the caller falls
back to one datagram per span. The test pins it (`a_longer_tail_is_refused_so_the_caller_sends_it_whole`).

**Before and after, on the artifact** (`rr-1`, clean path, `MOLEHILL_TRACE_ZERO`; traces are out of the tree):

| reading | before | after |
|---|---|---|
| `malformed_decode` refusals on a 13-lane run | **13** | **0** |
| server `first_seen` | 1, on every lane | **0, on all 90 lane-seconds** |
| client `own_below` (the frontier) | 0, forever | **11 017 / 12 934 / 15 008 / …**, within one or two of what the peer reports |

**And the cell, for the first time, reads healthy at `--scale 2`** — three arms, three rounds, `loss5`, tunnel leg:

| arm | `mhd_lane_dup_pct` | `throughput_gbps` | `rtt_p99_ms` |
|---|---|---|---|
| candidate (with the fix) | 13.0 / 13.3 / 54.0 | **0.425 / 0.542 / 0.467** | 2 032 / 1 419 / 1 405 |
| candidate twin | 18.4 / 12.3 / 13.3 | 0.361 / 0.413 / 0.432 | 2 184 / 1 724 / 1 262 |
| pre-change binary | 16.1 / 14.0 / 12.5 | 0.501 / 0.515 / 0.420 | 1 395 / 1 502 / 1 404 |

The pre-registered guard is `throughput_gbps ≥ 0.3628`; **every candidate round clears it**, against the four reverted
attempts that read 0.055–0.118. **What this run does not say**: that the fix caused the throughput. The pre-change
binary read 0.42–0.52 in the same run, and the storm was bistable before, so the honest statement is that the cell is
healthy *now* on both binaries and the fix's own mechanism is proven separately (the frontier, above). What the fix
does establish is that a lane's recovery is no longer starting from a hole at number zero.

**B's completion criteria**: `first_seen == 0` on every lane — **met**; `below` advancing — **met**; the cell's metrics
moving — **the cell is healthy and inside its guard**, with the attribution caveat above. A (the recovery-loop
re-architecture) is next, and it is the one that has to be judged on the storm when the storm is present.

### 4.209 B, first half: the missing number zero is a decoder refusal, not a lost packet (2026-11-10)

The question was where the dialer's number-0 packet goes on the product path, given the accepting end's first sighting
is always 1. Two gated traces answered it, one at each end, on a **clean** path (`rr-1`, no configured loss):

| trace | what it shows |
|---|---|
| what the client **posted**, decoded back from its own wire bytes | `credit` (unnumbered, number 0) then **`stream`, number 0** — the packet exists and left the process |
| what the server's **dispatcher saw** | the same `credit`, and then the next numbered packet is **`stream`, number 1** — number 0 never appears |
| the server's refusal line | `malformed_decode=13` on a run with **13 lanes** |

So the packet is **sent and not lost**: it arrives and the decoder refuses it. One refusal per lane, one lane per
refusal, and the missing number is exactly the one the cumulative frontier waited for its whole life. That is why
`below` pins at zero on every product lane — not loss, not the admission rule (§4.203 excluded that), but a packet the
accepting end cannot parse.

**What is still open, and how it closes.** Why that one packet does not decode. The next step is to print the first
bytes of the datagram at the `malformed_decode` site and compare them with the bytes the client posted for the same
packet — a field-by-field diff, not another hypothesis. The candidates worth checking against that diff: a sealed
frame whose length arithmetic is off for a short payload, a header the client writes after the checksum, and a
truncated write on the first send of a fresh socket.

**Reverted**: both traces were `MOLEHILL_TRACE_ZERO`-gated instrumentation and are out of the tree; the working tree is
the state §4.208 left it in, 107 tests green.

### 4.208 The fourth attempt is measured, reverted, and the conclusion goes to the human (2026-11-09)

The delayed link (§4.205, `4a67b08`) made the fourth candidate exercisable: **all 107 tests passed with the wide block,
the gated timer, the anchored declaration and the escape hatch in place.** The measurement then closed it:

| arm | `mhd_lane_dup_pct` | `throughput_gbps` | `rtt_p99_ms` |
|---|---|---|---|
| candidate | 55.5 / 58.2 / 57.1 | **0.094 / 0.107 / 0.073** | 2 612 / 1 814 / 1 395 |
| candidate twin | 56.1 / 66.4 / 90.6 | 0.069 / 0.021 / 0.101 | 1 399 / 1 400 / 1 472 |
| pre-change binary | 13.8 / 13.4 / 92.0 | **0.404 / 0.417 / 0.273** | 1 486 / 1 517 / 1 390 |

The guard is `throughput_gbps ≥ 0.3628`; the candidate misses it by three to five times. It is the best of the four —
55–58 % duplicate share against the third attempt's 62–91 %, and both arms agree, so the mechanism is real — and it is
still nowhere near the pre-change binary's *healthy* rounds (0.40 at 13 % duplicate share). **Reverted whole.**

**Four attempts, four reverts, and the pattern is now the finding.**

| attempt | what it changed | result |
|---|---|---|
| §4.203 | wide block + gated timer | `num` retirement 0 → 312 051/s, `cwnd` 13 480 → 198 156 — the retirement defect is real — but 0.055–0.092 against 0.325–0.540 |
| §4.206 | + time-anchored, capped declaration | 0.086–0.118, p99 2.7–3.9 s: **deadlock**, not storm |
| §4.208 | + escape hatch when the frontier stalls | 0.094–0.107 at 55–58 % duplicates: better, still 3–5× short |

**The conclusion, stated as a conclusion.** In this architecture — repairs exempt from the window, a timer that
declares, a cumulative frontier that pins at zero on every product lane, and a block that can only name a bounded
window behind the largest number — **the lossy path cannot be brought to the pre-change binary's healthy rounds by
changing the declaration rule.** Each attempt moved one of the four, and each was pulled back by another: widening the
block exposes more holes than the rule can admit; gating the timer stops the flood and deadlocks; adding evidence
without an escape deadlocks; adding the escape recovers throughput to a third and no further.

**What would be needed is not a fifth variation.** It is a recovery loop whose parts are designed together: repairs
*inside* the window (so the flight cannot grow to 30× `cwnd`), a frontier that cannot pin (so the cumulative path
stays alive), an acknowledgement whose reach scales with the flight (so nothing is stranded), and a declaration rule
with both evidence and an escape. That is a re-architecture of the recovery path, not a patch, and it is the human's
call — which is exactly what the fourth pre-registration said would happen if this failed.

**Committed from this round: nothing in the engine.** The tree is `4a67b08`'s state, 107 tests green. The four
attempts' designs and numbers are in §4.203–§4.208 and in the plan's D3 pre-registrations; none of them is in the tree.

### 4.207 The fourth candidate compiles and its rule is right, but the harness cannot exercise it (2026-11-09)

The fourth candidate was implemented: the wide block, the gated timer, the time-anchored capped declaration, and the
**escape hatch** — when the acknowledgement frontier has not advanced for a smoothed round trip, the clock may declare
up to eight records, oldest first, no more often than a reordering window apart (`Recovery::escapes`).

It compiles, the whole recovery and wire suite passes with the new rule, and **four connection tests do not**: the hole
in each is never re-sent, so the stream delivers nothing. The ledger from one of them shows why the rule cannot fire
there:

```
sent=8000 rep=0(0%) pkt=6 ack=0 num=6652 pfx=0 late=0 loss=0t/0m probe=0 grow=6652
```

`num=6652` (bytes retired by number) with `rep=0` and no declarations: the receiver named every record it received, the
sender retired them, and the *hole* — the first packet, the one carrying the stream's first byte — is never declared
because the anchor it needs is `smoothed/4` away and the harness's `smoothed` is an artefact of its own batching: the
ack's round trip is "however long the batch took to drain", not a path. Widening the batch spacing to 20 ms made it
*worse* (six tests failed instead of four), which is what an artefact looks like when you push on it.

**So the blocker is the same one as §4.204, one level deeper.** Stepping the clock per packet was necessary and is
landed (§4.205); it is not sufficient, because a rule about *time* needs a link whose **delay is a number the test
chooses** — one-way latency, an acknowledgement clock, and a queue — rather than a side effect of how many packets a
pump happened to move. That is the harness the fifth attempt needs, and it is a change to the test link, not to the
engine.

**Committed from this round: nothing in the engine.** The tree is the state §4.206 left it in, 107 tests green. The
fourth candidate's design stays in the plan; its implementation was reverted with its measurements unrun, and that is
stated rather than implied.

**Where this leaves the mechanism family**: three attempts measured and reverted (§4.203–§4.206), one implemented and
reverted before it could be measured (this one). The next step is not a fifth variation of the rule — it is the link
harness that can tell the rule apart from its own scaffolding.

### 4.206 The anchored declaration fails too, and the failure says why (2026-11-09)

The three pieces of §4.204 were built together — wide block, gated timer, time-anchored and capped declaration — with
all 107 tests green, and measured on the same cell, three arms, three rounds, in one run:

| arm | `mhd_lane_dup_pct` | `throughput_gbps` | `rtt_p99_ms` |
|---|---|---|---|
| candidate | 90.6 / 68.0 / 61.9 | **0.086 / 0.118 / 0.092** | 3 878 / 3 347 / 2 729 |
| candidate twin | 76.0 / 83.9 / 79.2 | 0.172 / 0.137 / 0.028 | 923 / 1 640 / 1 477 |
| pre-change binary | 14.5 / 13.9 / 44.5 | **0.517 / 0.364 / 0.496** | 1 671 / 1 402 / 1 394 |

The guard is `throughput_gbps ≥ 0.3628`; the candidate misses it by three to four times and its tail doubles. **So it
is reverted whole**, and this is the third mechanism in this family to fail.

**The failure is informative, and it names the mistake.** With the timer gated and the threshold anchored, almost
nothing declares any more: a hole needs a packet sent a quarter of the *smoothed* round trip later to have been
acknowledged, and on a 100 ms path with a window that has stalled there is no such packet. So losses are never
repaired, the stream stalls, and the numbers above are a carrier that stopped recovering rather than one that
recovered too much. **The timer is not only the amplifier — it is also the only thing that can break a stall, and the
gate removed exactly that.**

**What the next attempt has to be, stated as a rule rather than a patch.** A declaration needs both properties at
once: *evidence* (a later packet was acknowledged past the hole) **and** *an escape* (when no acknowledgement has
advanced for a while, the timer may declare — bounded, oldest first, at most a few records). The first alone deadlocks
as measured here; the second alone is the storm of §4.203. `oldest_flight_age` already exists for the escape hatch.

**Not pursued further in this round**: the failing combination is well enough understood that the next change should
be designed as that two-part rule, not as another tweak of these three.

### 4.205 The harness prerequisite is in: the fake clock lets time pass between packets (2026-11-09)

§4.204 named the blocker and this is the first half of it, landed on its own so that it can be shown inert:

- `Link::pump` now stamps every datagram **a spacing after the one before it** (`2 ms`) instead of giving a whole
  batch one instant, and delivers the acknowledgements after the batch.
- The manual `poll_transmit(now, …)` loops in the connection tests go through `Link::next_a` / `next_b`, which stamp
  each packet after the previous one.

**With the declaration rule unchanged, all 107 tests pass** — so the harness change moves nothing on its own, which is
what makes it safe to land before the rule it exists for. And it removes the objection that stalled §4.204: a test
that wants "a hole and a packet that overtook it" can now say so in time.

What is left for the next change, and it is one change: the three pieces of §4.204 plus the expectations of the seven
connection tests whose scenarios were written against a clock that never advanced. The harness can express both shapes
now, so those tests can be rewritten to `pump_spaced(now, gap)` and to assertions about *time*, not about counts.

### 4.204 The next candidate is designed and blocked on the fake clock, not on the engine (2026-11-09)

The evidence from §4.203 defines one candidate, and this entry records its design and the reason it is not in the tree
yet. Neither is a change of direction: both are the same fix.

**The design.** Three pieces, and they only work together:

1. **A wider acknowledgement block** — word 0 plus up to seven tail words, 512 numbers, variable length, built by the
   receiver from the holes it actually holds, `AHEAD` 256 → 1024. Measured to work mechanically (§4.203: `num`
   retirement 0 → 312 051 per lane-second, `cwnd` 13 480 → 198 156).
2. **A timer that only fires on its own deadline** — `Recovery::on_timeout` must compare `now` with its armed timer
   before *declaring*; the probe path stays on every pump tick, which is what keeps a shallow hole's repair at ~2 ms.
3. **A declaration anchored on time, not on a count** — a record is lost when it is unnamed by the acknowledgement
   **and** a packet sent a reordering window later has been acknowledged. The window is a quarter of `loss_delay`, so
   it tracks the path. Plus a cap of [`MAX_DECLARES_PER_ACK`] records per acknowledgement, oldest first, because a
   declaration is a burst and repairs are exempt from the window — the amplifier §4.203 measured.

**Why it is not landed.** Piece 3 needs the fake clock to advance *between* packets, and the test harness does not do
that: `Link::pump(now)` stamps a whole batch with one instant, so a hole and the packets that overtake it carry the
same send time and the anchor difference is zero. Seven connection-level tests and two recovery tests depend on that
shape, and rewriting them is a change to the harness, not to the engine — with its own verification (the harness must
still be able to produce the count-rule cases the current tests describe).

**So the next step is the harness, and it is a step with a definition of done**: `Link` gains a clock that advances per
packet by a configurable spacing, the seven tests are rewritten to that clock with their *intent* unchanged, and only
then does piece 3 go in with its own A/B — the same cell and the same pre-registered guard as §4.203
(`throughput_gbps ≥ 0.3628`, `rtt_p99_ms ≤ 1 412`, `transport_loss_detect_us ≤ 20 ms`, clean `bulk-n ≥ 13.10`).

**What is committed from this round: nothing in the engine.** The tree is the state §4.203 left it in, 107 tests green.
That is deliberate: a half-landed declaration rule that the harness cannot exercise is a defect this repository has
already paid for once.

### 4.203 The storm's mechanism, found and measured: a lane whose records can never be retired by number (2026-11-09)

The per-lane ledger from a storming arm and a healthy arm of the **same run** names the state, and it is not
congestion:

| reading | storming lane | healthy lane |
|---|---|---|
| `num` (records retired by number) | **0 for the whole run** | 500 699 → 9 885 787 per second |
| `grow` (window-growth events) | **0** | 12 141 → 116 014 |
| `cwnd` | **13 480** (its initial ten packets), never moves | 25 631 → **141 645** |
| `flight` | 400–3 000 % of `cwnd` (repairs are window-exempt) | within `cwnd` |
| `loss` | 0 threshold / **20 306** timer per second | 24 threshold / 4 973 |

So the storm is: **records are never retired by number → the window never grows → new data cannot be admitted →
the repairs, which are exempt from the window, are all the path carries → 80–90 % of the wire is re-sends.** The
window is not too small *because of congestion*; it is small because the only retirement path left is the
per-stream byte prefix, which cannot feed the growth hook.

**Why nothing is retired by number.** `Ack::contains` reached exactly **64 numbers** behind `largest`, so any record
older than that was invisible to the acknowledgement and could only be retired by the prefix. And the cumulative
frontier — the one path that would cover old numbers — is pinned at **zero on every product lane**, because the
dialer's number-0 packet never reaches the accepting connection: the server's `first_seen` is **1** in 4 of 4 lanes,
every run. A trace at the admission point shows the opening *credit* dropped exactly as designed (unnumbered, not an
opening) and **no stream datagram refused at all** — so number zero never arrives at the dispatcher, and the cause is
still open. The bare carrier probe does see number zero, so the loss is in the product path.

**Two changes were built and both are reverted, on the pre-registered guard.**

1. **The timer only does what a timer does**: `Recovery::on_timeout` must wait for its own deadline before *declaring*
   losses; the probe path is untouched. Two tests pin it (`a_pump_tick_declares_nothing_before_the_timer_expires`,
   `a_tick_with_nothing_stale_still_probes_one_range`).
2. **A wider acknowledgement block**: word 0 plus up to seven tail words (512 numbers), variable length, built by the
   receiver from the holes it actually holds; the memory bound `AHEAD` went 256 → 1024, and the shadow bitmap beside
   the hole set was deleted rather than kept in sync.

**The wider block works, and that is the important measurement.** With it, `num` retirement went from **0 to
6 312–312 051 per lane-second** and `cwnd` grew **13 480 → 198 156** — the retirement defect is real, and the block
width was its cause.

**And the cell got worse**, which is why it is reverted. Three arms, three rounds, same run: candidate
`dup 64.1 / 70.3 / 76.7` at `0.055–0.092 Gbit/s`; its twin `62.4 / 79.1 / 65.7` at `0.044–0.093`; the **pre-change
binary** `13.4 / 92.0 / 92.2` at `0.325–0.540`. The pre-registered guard is `throughput_gbps ≥ 0.3628`, so the
candidate misses it by four to eight times, while the old binary still reaches `0.54` in its healthy round.

**Why it got worse is visible in the same numbers**: an acknowledgement that can name 512 numbers names *many*
holes, and the rule that declares a hole lost "three numbers behind the largest" then declares all of them at once.
The wider block feeds the mass declaration. **A wider block is only safe with a declaration rule that does not read
queueing delay as loss** — that pairing is the next candidate, and it is now defined by evidence rather than guessed.

### 4.202 The human's ruling: a defect is not deliverable, so D3 reopens on the timer (2026-11-09)

Two instructions arrived together, and they change what this stage is for.

**First: mhd has to be a complete protocol, with no known defect.** It is going to sit beside the TCP carrier as the
basis for later design decisions, so a quarter-throughput lossy path is not a trade-off to be documented — it is a
blocker. That retires the "closed as an explicit falsification" framing of §4.195: a falsification proves the
protocol is **not ready**, which is a stage result but not a deliverable.

**Second: measurement choices are mine to make.** No more waiting on a human for "should this cell be judged by
throughput or by the duplicate share", or "may I build an older engine to tell a regression from an environment".
If a question can be answered by running something, it gets run.

**So D3 reopens, with the third candidate pre-registered before any code moves** (plan, *D3 第三次预登记*):

**The candidate: the timer only does what a timer does.** `Recovery::on_timeout` must wait for **its own deadline**
before it *declares* losses; the probe path is untouched (one range per tick, which is what keeps a shallow hole's
repair at ~2 ms).

**Why this one, from three measured facts.** `pump::wait()` caps its deadline at `READ_POLL` whenever a stream is
live, and that cap arrives as `Wake::Timer` (`pump.rs:767`, `:976`). `Recovery::on_timeout` never compares `now`
with its own timer — it clears it and decides (`recovery.rs:763`) — so **every millisecond** it either probes (when
nothing is stale) or **declares the whole stale set lost and re-sends it** (`recovery.rs:780–830`). And the stale set
is mostly not loss: 80–90 % of declarations come from the timer (§4.189), and the shaped cells storm without any
configured loss at all because the carrier's own re-sending fills the queue (§4.200). Queue → declaration → re-send
→ deeper queue is the loop.

**The criteria, fixed before the run** (they are in the plan, and this entry is the copy that travels with the
record): cell `cost` + `loss5` + tunnel leg + `--scale 2`, two rounds and the A/A twin; **primary
`mhd_lane_dup_pct`** against 5 % materiality and the cell's own 4.9 % floor; **guards** `throughput_gbps ≥ 0.3628`
(the acceptance floor — the candidate has to *double* today's 0.1843, not merely not-worsen), `rtt_p99_ms ≤ 1 412`
ms, `transport_loss_detect_us ≤ 20` ms (the shallow hole must stay at millisecond scale — latency outranks
throughput), clean `bulk-n ≥ 13.10` Gbit/s and `rate100_shallow` no worse than today. Any guard broken, the whole
change goes back.

**Status: pre-registered, no engine code touched yet.** The next commit is the change plus its unit tests, then the
A/B.

### 4.201 The ratio floor lands, `jitter` holds, and a warm-up round breaks that cell (2026-11-09)

**The floor first, because two samples in §4.200 needed it.** `cpu_s_per_gbit` 3 762 and
`wire_per_visitor_byte` 436.8 came from cells that had carried under half a megabit: a ratio over a stalled cell
describes the stall. `model.MIN_RATIO_GBPS = 0.001` (1 Mbit/s) now makes both metrics absent below it, while
`offered_gbps` keeps reporting what the visitor actually offered — so a stalled cell shows a small offer and two
dashes, not a spectacular ratio. Declared in `model.py` because it decides whether a metric exists (it is part of the
method fingerprint), stated on the benchmarks page in both languages, and it fired in the broken run below:
`offered_gbps = 0.0001`, both ratios absent.

**`jitter` holds** (`cost`, tunnel leg, two measured rounds, 356.7 s):

| arm | `throughput_gbps` | `offered_gbps` | `rtt_p99_ms` | errors |
|---|---|---|---|---|
| molehill | 1.7813 / 1.8427 | 1.60 / 1.79 | 155.2 / 168.2 | 0 % |
| molehill~aa | 1.8685 / 1.7383 | 1.68 / 1.56 | 161.3 / 155.5 | 0 % |

Nearly two Gbit/s through 20 ms ± 5 ms of jitter, a 155–168 ms tail, no errors: the cell holds and there is no
repair loop.

**But the first attempt at this cell was broken, and by a documented option.** With `--warmup-rounds 1` the
*first* arm of every round ran a **0.5 s** stage while the twin ran 62–68 s — `window_s` 0.508, 0.499, 0.498 against
67.7, 62.8, 62.8 — so its throughput was absent rather than low, and the twin read 1.7–2.1 Gbit/s beside it. The same
command without the warm-up round is clean on both arms. **A warm-up round is not free on this cell**, and a run that
reports an absent primary because of one is a measurement defect, not a reading.

### 4.200 The storm is not loss-specific: both shaped cells storm where their records were clean (2026-11-09)

`rate100` (100 Mbit shaper on the carrier leg) twice, the second after killing eight harness servers a previous run
had leaked:

| run | `throughput_gbps` | `rtt_p99_ms` | `dropped_packets` | wire/visitor |
|---|---|---|---|---|
| first | 0.074 (one sample; the other degenerate) | 4 138 / 70.6 | 132 857 / −19 | 13.6 / **3 762** `cpu_s_per_gbit` |
| second | 0.0765 / 0.0681 | 3 803 / 122.3 (flipping by round) | 89 783 / 618 112 | 1.43 / **436.8** |

**Against §4.168's 93.5 % of the shaper, today reads 68–77 % with a tail that flips between 122 ms and 3.8 s by
round, and 90 000–620 000 drops.** Two rounds are enough to say the cell is not in the state its record describes.
The same is true of `rate1000` (§4.197: 79 % of the shaper, 0.85 s tail).

**And that is the finding worth more than either number: the storm is not a loss-cell phenomenon.** These cells
configure no loss at all — the loss is the shaper's queue overflowing, which the carrier's own re-sending causes. So
the defect reproduces wherever the carrier can build a queue, not only under `loss5`, and both shaped cells in the
plan's acceptance table are downstream of it.

**What this does not establish.** That a code change caused it. The engine is identical to the baseline binary
(`git diff 346d5e3..HEAD -- src/` touches only `probe.rs`, the ledger and tests), so "environment" and "a regression
older than 346d5e3" are both open, and this session has no older binary to separate them. The distinguishing run is
named, not guessed: build the engine from the commit that produced §4.168 and run this cell on it, same method.

**Two degenerate samples are also on the record** — `cpu_s_per_gbit` 3 762 and `wire_per_visitor_byte` 436.8, each
from a round whose denominator collapsed. They are not readings; they are evidence that this cell needs its
denominator guarded before it can be quoted at all.

### 4.199 The formal `loss5` acceptance: half the floor, with a 1.4 s tail (2026-11-09)

The headline cell, run properly: `cost` + `loss5` on the tunnel leg, **`--scale 2` (120 s stages), six measured
rounds plus the twin** (1 677 s), with no statistics overhead so the throughput is the clean one.

| arm | `throughput_gbps` (six rounds) | median | `rtt_p99_ms` | wire/visitor |
|---|---|---|---|---|
| molehill | 0.1215, 0.2327, 0.1206, 0.2291, 0.1395, 0.3437 | **0.1843** | 1 412 | 1.566 |
| molehill~aa | 0.4510, 0.2276, 0.0282, 0.2250, 0.1242, 0.2451 | 0.2263 | 1 562 | 1.588 |

**Against the plan's floor of 0.3628 Gbit/s (30.5 % of the 1.19 Gbit/s clean baseline), the median is 51 % of it.**
The cell fails, and it fails by a factor the noise cannot cover — the criterion is a floor on the median of six
rounds and every round is below it.

Two things it also says, which matter more than the number:

- **The tail is 1.4 s on a 100 ms path.** The plan's priority is 延迟 > 吞吐, so this is the more serious of the two
  readings: a visitor on this path waits a second and a half at the 99th percentile.
- **The spread is still huge** — 0.12–0.34 on the same binary, 0.03–0.45 on its twin — so this cell can only ever
  support a *floor* claim, not a ratio. That is a property of the defect, not of the run: the storm's presence is
  what varies, and six rounds of 120 s do not average it away.

### 4.198 D4's `transport` cell holds, and the six-criterion picture so far (2026-11-09)

**`transport`, clean** (dev profile, two measured rounds plus the twin, 12.2 s): the carrier's own numbers, with the
bare TCP socket as the control.

| arm | open p99 (µs) | first-byte p99 (µs) | recovery p99 (µs) | errors |
|---|---|---|---|---|
| molehill (fresh / reuse) | **3.0** / — | 73.5 / 80.5 | 4.5 / 4.0 | 0 % |
| molehill~aa (fresh / reuse) | 3.0 / — | 70.5 / 68.5 | 4.5 / 3.5 | 0 % |
| control (TCP, fresh / reuse) | **48.0** / — | 22.0 / 30.0 | 15.5 / 16.5 | 0 % |

An mhd stream opens in 3 µs against a TCP connect's 48, its round trip is ~2.5× the bare socket's, and the wait
after the first byte is a quarter of the control's. That is the §4.171 record reproduced with the twin beside it,
and the criterion for this cell — the four `transport_*` numbers inside their floors — **holds**.

**D4's six criteria, as the cells now stand.** Only the cells that have been run in this stage are listed; the
formal 120 s `loss5` and the clean interactive numbers are next.

| criterion | state |
|---|---|
| tunnel-leg `loss5` `throughput_gbps` | **fails, known**: the storm is steady at 120 s (§4.191: 0.011–0.252 Gbit/s, 81–91 % duplicate share) |
| `rate100_shallow` against peers | **fails**: 20 % less carried, two orders of magnitude more drops (§4.197) |
| `rate100` / `rate1000` | `rate1000` **does not reproduce its record** (79 % of the shaper, 0.85 s tail); unreconciled (§4.197) |
| clean / `rtt100`, `bulk-n ≥ 13.35` | **undecided, at the boundary**: 13.10 median, twin's best 13.345, no same-method baseline (§4.196) |
| `loss1` and the jitter family | `loss1` **holds** (4.2–5.3 Gbit/s, 100–128 ms tail); jitter not run |
| `transport` | **holds** (this entry) |

**So the acceptance is going to be a split verdict, and that is the honest one**: the carrier's protocol-level
numbers and 1 %-loss behaviour hold; the lossy and shallow-queue cells fail on the defect D3 could not fix; one
cell sits on its floor; one no longer reproduces its record.

### 4.197 D4 continues: the shallow-queue defect reproduces, and `rate1000` no longer reads like its record (2026-11-09)

**`rate100_shallow`, `bulk-1`, against the two peers** (dev profile, two measured rounds plus the twin, 108.9 s).
This is the second defect cell in the plan, and it has not moved:

| arm | `throughput_gbps` | `dropped_packets` |
|---|---|---|
| rathole | 0.0957 / 0.0957 | 16 / 16 |
| frp | 0.0952 / 0.0951 | 14 / 16 |
| molehill | 0.0688 / 0.0833 | 4 664 / 1 605 |
| molehill~aa | 0.0743 / 0.0756 | 1 385 / 1 324 |

mhd carries about **20 % less** than either peer and drops **two orders of magnitude** more, exactly as §4.176
measured. The plan's criterion for this cell — "throughput no longer behind its peers, and its own drops down" —
**fails**, and it fails for the same reason the lossy cell does: the carrier's own re-sending overruns a shallow
queue.

**`rate1000` on the carrier leg** (`cost`, `--scale 1`, two rounds plus the twin, 383.9 s):

| arm | `throughput_gbps` | `rtt_p99_ms` | `cpu_s_per_gbit` | wire/visitor | drops |
|---|---|---|---|---|---|
| molehill | 0.7919 / 0.7940 | 824.6 / 876.1 | 2.092 / 2.093 | 1.224 | 0 / 0 |
| molehill~aa | 0.8080 / 0.7936 | 781.3 / 856.5 | 2.046 / 2.129 | 1.20–1.23 | 0 / −1 |

**79 % of the shaper with a tail near 0.85 s and 22 % duplicate wire, on a 2 ms path with no drops at all** — so the
tail is the carrier stalling itself, not a queue. The P1 record for this cell (§4.168) reads 93.8 % of the shaper
with a tail two orders of magnitude smaller, and **the two are not comparable as they stand**: different profile,
different round count, and no fingerprint check has been done between them. So this cell is recorded as **does not
reproduce its record**, with the reconciliation named as the next step for it rather than asserted as a regression.

**What D4 has so far, out of six criteria.** `loss1` holds; `bulk-n` sits exactly on its floor and is undecided
until a same-method baseline exists; the shallow-queue cell fails; `rate1000` does not reproduce its record and is
unreconciled. The remaining cells — `transport`, the clean interactive numbers, and the formal 120 s `loss5` — have
not been run in this stage.

### 4.196 D4 begins: `loss1` holds, and `bulk-n` sits exactly on its floor (2026-11-09)

D4's acceptance cells, run on the tree that has no fix in it. Two of the six so far, both with the A/A twin.

**`loss1` on the carrier leg** (`cost`, `--scale 1`, two measured rounds plus the twin, 373 s):

| arm | `throughput_gbps` | `rtt_p99_ms` |
|---|---|---|
| molehill | 5.271 / 4.204 | 119.6 / 105.3 |
| molehill~aa | 4.235 / 4.223 | 128.5 / 100.1 |

1 % loss costs about a fifth of the clean rate and leaves the tail in the hundreds of milliseconds, which is the
published shape rather than a new one; nothing here is worse than the record. **And the new metric reported itself
absent with a reason** — this run did not set `MOLEHILL_MHD_STATS`, so `mhd_lane_dup_pct` is a typed absence rather
than a zero, which is the behaviour §4.187 designed and this is its first real run proving it.

**`bulk-n` clean, the 85 % guardrail** (`throughput_gbps`, two measured rounds plus the twin, 70.7 s):

| arm | `throughput_gbps` |
|---|---|
| molehill | 13.168 / 13.023 (median **13.10**) |
| molehill~aa | 13.227 / 13.345 (median 13.29) |
| control (no tool) | 62.920 / 62.943 |

The criterion in the plan is `bulk-n ≥ 13.35 Gbit/s`. **The median is below it and the twin's best round is 13.345 —
the cell is sitting exactly on the floor, not above it.** Two honest statements follow, and neither is a claim yet:

1. **It is not a measured regression.** 13.35 came from 85 % of a **15.840 Gbit/s** baseline taken under an earlier
   method; a comparison across methods is one this model refuses (`analysis.comparability`), so nothing here says
   throughput fell. What it says is that the floor and today's reading are the same number to three digits.
2. **It is not a pass either.** D4 needs a *same-method* baseline for this cell before it can say "85 % of it", and
   that baseline does not exist yet — the last one is method-incomparable by construction.

So D4's fourth criterion is recorded as **undecided, at the boundary**, and the next step for it is to take a
same-method baseline of this cell (the plan's own re-baseline rule) rather than to quote 13.10 as a regression or
as a pass.

### 4.195 D3 closes as a falsification: four directions measured, none of them the fix (2026-11-09)

The plan's D3 asks for "a fix **or an explicit falsification**". The last four rounds produced the second, and this
entry closes the stage on it — not on a fix, and not on "acceptable".

| direction | verdict | evidence |
|---|---|---|
| byte-range acknowledgement (what the peer holds above its prefix) | **falsified** | mechanism fired (`rng` 21–25 MB/round) but the pre-registered primary could not resolve, and on the new metric the candidate was *worse* in healthy rounds: 25–30 % against the baseline's 14.5 % (§4.186–§4.187) |
| admit repairs against the window | **falsified** | stormed in 3 of 6 rounds against the baseline's 1, throughput −21 %, reproduced by its own twin (§4.188) |
| RACK-shaped delivery-time detection | **premise refuted** | the shallow cell reads 2.3–2.6 ms on a 200 ms path *with the round trip seeded*, because the pump probes on its 1 ms tick — there is nothing slow to fix (§4.194) |
| bound the unconditional probe (this entry's own candidate) | **not worth it** | measured from the archived runs: probe bytes are **2.4–3.5 %** of what the lanes post (2.4 / 2.8 % at `--scale 2`, 3.0–3.5 % at `--scale 1`), and the change would also push a shallow hole's repair from ~2 ms out to the RTO — a throughput gain paid for in latency, against the plan's own 延迟 > 吞吐 ordering |

**What the defect is, stated as it now stands.** On a carrier path that loses 5 % of datagrams, the lane settles into
a state where most of the wire is repairs (68–92 % duplicate share), the window stays small, and the cell carries a
tenth to a quarter of what a clean path does. The declaration that starts it is the **stale branch** — 80–90 % of
declarations come from the clock rather than from later-packet evidence (§4.189) — and every candidate that tried to
change what happens *around* that declaration failed or was refuted.

**What is explicitly not claimed here.** That the defect is unfixable. Four directions were measured and closed;
the fifth is not named, and finding it needs a new hypothesis, not another attempt on one of these four.

**So D3 closes, and D4 opens with the defect on the record.** The acceptance cells now produce a verdict that says
so, which is what the plan asks for: a defect kept as a defect rather than dressed as a trade-off.

### 4.194 The cell's real reading refutes RACK's premise: the carrier probes on the pump's tick (2026-11-09)

§4.193 left the cell measuring a 2.2 ms "repair" on a 200 ms path and blamed a missing round-trip sample. The
warm-up was added (a throwaway exchange on the same lane, `--warmup-ms 400`; the drop box now counts **streams**
rather than datagrams, `--drop-stream 2`, and matches the repair on stream **and** offset, because two streams at
the same offset is the ordinary case). The round trip was then seeded — `srtt_us=200 072` on the shaped leg — and
the repair still came **2.3–2.6 ms** after the drop.

**So the number was never about `loss_delay`, and the reason is in the pump.** `wait()` caps its deadline at
`READ_POLL` (1 ms) whenever a stream is live (`pump.rs:767`), and that cap arrives as `Wake::Timer`, which calls
`Conn::on_timeout` (`pump.rs:976`). `Recovery::on_timeout` does **not** compare `now` with its own timer — it clears
it and decides (`recovery.rs:763`): if nothing is older than `loss_delay`, `stale` is empty and it takes the
**probe** branch, which re-sends the oldest outstanding range as a new number. On a lane with a live stream that has
anything in flight, that is **one re-send per millisecond, with no evidence required**.

**What that means, in the two places it matters.**

- **The shallow-flight cell is measuring the probe cadence, not loss detection.** A hole in a three-packet flight
  is re-sent about two pump ticks after it is dropped. There is nothing slow here for a delivery-time detector to
  fix: **RACK would not improve this cell**, and its premise — "the declaration waits for the clock" — was an
  artefact of the protocol cell calling `on_timeout` once, by hand, at 500 ms.
- **On a wide flight the same rule is a tax.** The ledger's own `probe=` counts on the storm lanes of §4.188 were
  290–314 per second, i.e. about 3.2 Mbit/s per lane of unconditional re-sends; and when records *do* age past
  `loss_delay`, the stale branch adds the mass that dominated §4.189's 80–90 %.

**What this does to the decision, stated plainly.** Branch ① (re-open RACK) has lost its justification by
measurement rather than by argument: the carrier is already re-sending a shallow hole at pump cadence, so the cell
that was built to price RACK reads ~2 ms in its favour already. The defect the cell *does* expose is the opposite
of the one the plan was chasing — **an unconditional re-send per tick** — which is a different candidate (bound the
probe rate or require evidence for it), and a different plan change.

**The cell is still worth what it cost**: it is stable (A/A floor 4.2 %), it is on the shaped path, and it is the
instrument that would price *that* candidate. What changed is which candidate it argues about.

### 4.193 The shallow cell is wired into the model, and its first shaped reading is a surprise (2026-11-09)

The cell from §4.192 is now a scenario with a metric rather than a hand-run binary: `transport-loss` runs the
three processes (server in the server namespace, drop box in the visitor's, client dialling the box) and reports
`transport_loss_detect_us`. It is **diagnostic** — `control=False`, stated in the plan and the docs — because a
path with no carrier in it has no loss detector, so there is no control arm to compare against; a *change* is
judged by comparing two runs of the same cell. `just bench-selfcheck` went 774 → **791**, both languages carry the
metric and the scenario row, and the docs gate passes.

**First shaped run**: `rtt100` on the tunnel leg, two measured rounds plus the twin, 21.3 s.

| arm | `transport_loss_detect_us` | spread |
|---|---|---|
| molehill | 2 206 / 2 273 (median 2 240) | 3.0 % |
| molehill~aa | 2 272 / 2 405 (median 2 339) | 5.7 % |
| A/A floor | **4.2 %** | |

**The cell is stable** — a 4.2 % floor against the storm cell's 76 %, and two rounds already separate a 10 %
change. That was the point of building it.

**And the number is a surprise: 2.2 ms on a 200 ms path.** It is the same figure loopback gave, so the path's delay
never entered the measurement. The reason is the lane's state at the drop: **it has no round-trip sample yet**, and
`loss_delay` therefore falls back to its millisecond floor — the record looks stale after about one millisecond,
the pump notices on its next pass or two, and the range goes back out. The cell is measuring *that fallback*, not
what a 200 ms path does to a shallow flight.

**So the refinement is named rather than papered over**: the measured exchange has to come *after* a warm-up
exchange on the same lane, so an acknowledgement has already seeded the round trip, and the drop index has to move
past the warm-up's datagrams. Then the shaped number becomes path-dependent — on a 200 ms path the declaration
should wait for `loss_delay` ≈ 225 ms — and the cell prices the rule where the defect actually lives. Until then
its reading is a real number about a state that no shaped path reaches after its first round trip.

### 4.192 The RACK cell exists: a shallow flight, a drop box, and 2 210 µs between the evidence and the repair (2026-11-09)

§4.189 built the *protocol* cell. This is the bench one, run inside the harness with no product setting touched: the
harness gained `--role shallow`, which starts the acceptor lane, the dialer lane **and a drop box between them** in
one process. The box drops the second stream datagram (the first is the carrier's own open), timestamps the next
one through — later evidence that the dropped range is missing — and timestamps the first datagram that carries
that range again.

| reading | value |
|---|---|
| drop | 155 µs after start, offset 1 349, length 1 349 |
| later evidence through | **+9 µs** (the next packet of the same burst) |
| the dropped range re-sent | **+2 179 µs** |
| `detect_after_later_us`, five runs | 2 186 / 2 197 / 2 210 / 2 232 / 2 320 µs (median **2 210**, spread 6 %) |
| served | 2 698 of 2 698 bytes |

**It needed one read-only seam.** The box has to know which offset a datagram names, and the decoder that knows is
the carrier's own, so `probe::describe` exposes it (`Carried { stream, offset, len, fin, ack_only }`) rather than
letting the instrument re-implement the header — a second decoder is a second thing to be wrong, and the one that
lies would be the one measuring.

**What 2 210 µs is, stated carefully.** It is *not* `loss_delay`: on loopback that is about 225 µs, and the carrier
declared the loss about ten times later than that. It is the **pump's cadence** — the declaration waits for a pass
to notice the stale record, and the pump wakes at `READ_POLL` (1 ms) while a stream is live, so two or three passes
is the floor here. On a shaped path the same rule would wait for `loss_delay` instead, which is 225 ms over a
200 ms round trip. **So the cell measures the rule, and the rule's delay is whichever is later: the clock's floor
or the pump's poll** — and the model has no metric for either yet.

**Boundary of this first version, and what it changes for the next step.** Both lanes are loopback, so the number
above describes the rule at a zero-length path. To price the rule where the defect lives, the drop box has to be a
process of its own (`--role relay`) sitting on the shaped leg between a normal server and client — that is the next
step, and only after it does declaring a `model.METRICS` entry make sense, because a metric is a claim and this
cell is not yet on the path the claim would be about.

### 4.191 A two-minute stage turns the storm from a coin flip into the steady state, and the metric becomes judgeable (2026-11-09)

§4.190 concluded that the cell was the problem, not the statistic. One probe settles it: the same cell with
`--scale 2` (120 s stages) instead of 60 s.

| | `--scale 1`, six rounds (§4.188) | **`--scale 2`, two rounds** |
|---|---|---|
| candidate `mhd_lane_dup_pct` | 13.7, 14.2, 14.8, 79.9, 92.0, 85.8 | **86.7, 90.7** |
| twin | 14.0, 82.3, 14.1, 82.8, 12.9, 86.6 | **89.7, 81.4** |
| A/A difference | 1.5 % | **4.0 %** |
| **A/A floor (what a claim must clear)** | **76.4 %** | **4.9 %** |
| `throughput_gbps` | 0.2469 median, spread 0.11–0.37 | 0.011 / 0.252 / 0.130 / 0.357 |

**A longer stage does not average the storm away; it makes the storm the steady state.** Every round of both arms
sits at 81–91 % duplicate share, so the two halves of the A/A agree to 4 %, and the floor drops from 76 % to 4.9 %.
The candidate that pulls a lane out of the storm would read ~30 against ~87 — fifteen floors away, on two rounds.

**So decision B dissolves, and decision A gets simpler.** No new statistic is needed and no metric has to change:
the claim rule stays exactly as written, and the cell to judge a candidate exists — `cost`, tunnel leg, `loss5`,
`l4-mux8`, **`--scale 2`**, two rounds plus the A/A twin. What the 60 s stage was measuring was *whether* the
storm had started by the time it ended, which is why it read 14 % in half the rounds and 90 % in the rest.

**And it sharpens the product statement.** On this path the carrier needs about two minutes to settle into the
regime where nine bytes in ten are re-sends; the 120 s readings are 0.011–0.252 Gbit/s, i.e. a tenth to a quarter
of the clean path. That is the defect the plan exists for, now measurable in a shape that can be claimed on.

### 4.190 A bistable metric cannot carry a claim under the model's own floor rule (2026-11-09)

The metric from §4.187 was built so the next candidate could be judged. Reading the last A/B's own noise table
(§4.188) shows the judging is still not possible, and the reason is worth more than the metric:

| reading | value |
|---|---|
| A/A paired difference, `mhd_lane_dup_pct` | **1.49 %** — the pairing is tight when neither half storms |
| A/A **scatter** (the floor the model uses) | **76.4 %** (36.8 points) |
| why | the twin stormed in 3 of its 6 rounds, so its own half-range *is* the storm |
| candidate vs baseline, paired by round | candidate **higher in 5 of 6**, median +2.86 points |

**The two readings disagree because the quantity is bistable.** Paired, the same binary differs by 1.5 %; unpaired,
its own scatter is 76 %. The model's rule is "the floor is the largest of the readings", so on this metric the floor
is the storm itself, and **no candidate can clear it** — not even the falsified one, which is *worse* in five rounds
of six and still inside the floor. That is the same defect the plan recorded in §4.186 wearing different clothes:
the number exists, and the rule that turns numbers into claims cannot use it.

**What a future pre-registration has to do about it, decided before any run:**

1. **Declare a stable statistic.** The share of rounds above a storm threshold (50 points), or the median of the
   *lower* half, is stable enough for the same A/A: the twin would read 3/6 and the candidate 3/6 against a
   baseline of 1/6. This is a metric declaration change, and it has the same cost as any other: registry, both
   languages, self-check, and it must be written before the run it judges.
2. **Or make the cell deterministic.** Both halves of an A/A must be driven into the same state, or neither — the
   storm appears in one to three rounds out of six on the same binary, so a cell that leaves it to chance cannot
   produce a floor worth quoting.

**And this is not a reason to weaken the floor.** A 76 % floor on a metric that swings 12→92 is honest about the
measurement; the dishonest move would be to quote the 1.49 % paired difference because it is smaller. The rule
stays; the *statistic* or the *cell* has to change, and the change has to be declared before the run.

### 4.189 The cell RACK was refused for now exists, and the bench says the clock does 80–90 % of the declaring (2026-11-09)

§12.5 closed RACK with a condition rather than a verdict: *"a cell where a stream keeps two or three packets in
flight"* did not exist, so a reordering- or delivery-time-based detector could not be judged. Both halves of that
condition are now answered, and neither answer decides the plan by itself.

**The protocol-level cell exists.** `a_middle_packet_of_three_in_flight_waits_for_the_clock_despite_later_evidence`
(in `recovery.rs`) builds exactly that flight — three packets, the **middle** one dropped — and measures the
current rule:

| | measured |
|---|---|
| the later packet's acknowledgement | arrives at **10 ms**, and is proof the middle one is gone |
| what `on_ack` declares from it | **nothing** — the threshold needs three later packets and a flight of three has one |
| what the clock does | names it, and never earlier than the timer: **20 ms** on the small-flight floor, `MIN_RTO` = **200 ms** as soon as the flight is wider |

So the gap a RACK-shaped rule would close is stated in milliseconds rather than asserted: **the evidence is in hand
10 ms after the send and the declaration waits for a floor.** The cell is deterministic, needs no network, and is
the thing a re-opened plan would pre-register against.

**And at the bench the clock is what declares, by a wide margin.** Reading `loss=Nt/Nm` over the busy lanes of the
last A/B (§4.188, `l4-mux8`, tunnel leg, `loss5`, six rounds):

| arm | threshold declarations | timer declarations | timer share |
|---|---|---|---|
| candidate | 160 529 | 689 140 | 81.1 % |
| candidate twin | 160 954 | 620 099 | 79.4 % |
| baseline | 251 516 | 2 167 198 | **89.6 %** |

**Eight or nine declarations in ten come from the clock, not from later-packet evidence**, on both builds. That is
the same finding as the protocol cell, at the scale where the throughput is lost — and it is why both falsified
candidates failed: they changed what happens *after* a declaration, while the declaration itself is what this path
is drowning in.

**What this does and does not change.** It satisfies the plan's *stated* condition for revisiting RACK at the
protocol level. It does **not** satisfy the part that mattered: there is still no **bench** cell in which a stream
carries two or three packets — the carrier's window is not configurable, and no condition in the model makes a
stream's bandwidth-delay product three packets. So a re-opened RACK could be *shown correct* on the deterministic
cell and could not be *priced* on a shaped one; that distinction is exactly what the human's decision has to weigh,
and it is why this entry records the cell rather than starting the change.

### 4.188 The repair budget made the storm **more frequent**, and it is reverted too (2026-11-09)

D3's second pre-registered candidate: admit a repair only while the re-sent bytes in flight are under the window
(`repair_flight < cwnd`), which bounds a lane to one window of repairs. Implemented in `397ece1`, measured against
the `346d5e3` binary in the pre-registered cell — `cost`, tunnel leg, `loss5`, `l4-mux8`, `--scale 1`, **six**
rounds, `--aa` plus `--ab-arm` — and reverted on its own verdict.

| arm | `mhd_lane_dup_pct` per round | median | `throughput_gbps` median |
|---|---|---|---|
| candidate | 13.7, 14.2, 14.8, **79.9, 92.0, 85.8** | **47.3** | 0.2469 |
| candidate twin | 14.0, **82.3**, 14.1, **82.8**, 12.9, **86.6** | 48.2 | 0.2469 |
| baseline | 14.0, 12.6, 14.3, 13.4, **87.8**, 14.8 | **14.2** | 0.3146 |

**The candidate is worse, and the twin says it is the binary and not the noise.** It stormed in three rounds of
six against the baseline's one, its twin (the same build) also stormed in three, and the twin's throughput median
is identical to the candidate's to four decimals while the baseline's is 21 % higher. So the effect is far outside
both the pooled A/A and the metric's floor, and it points the wrong way: by bounding repairs, the lane frees its
window more slowly, its hole stays open longer, acknowledgements arrive later, and the timer declares more — the
gate fed the very loop it was meant to damp.

**That refutes the amplifier hypothesis, and that is worth more than the candidate.** §4.182's step 5 said repairs
exempt from the window are what sustains the storm; the measurement says the opposite is at least as true:
**repairs must keep moving, or the acknowledgement path stalls behind them.** Two candidates have now been built
on the chain from §4.183 and both were falsified — one on retirement evidence (§4.186), one on repair volume here.
What both leave standing is the *trigger*: the timer declaring a loss because an acknowledgement arrived later
than `loss_delay`, which on this path is a queueing delay the carrier created itself.

**Where D3 stands, and the decision it needs.** Two mechanisms are dead with evidence; the remaining lever is the
declaration rule, not the recovery volume and not the retirement paths. RACK-shaped delivery-delay detection is the
obvious one and it is **closed in the plan** for a stated reason — "no cell keeps two or three packets in flight
per stream" — which this cell does not change: its streams carry dozens of packets, so a RACK threshold would have
later evidence to anchor on but the cell was never the one RACK was refused for. **Re-opening it is a plan
change, not an implementation choice**, so it goes to the human rather than being started.

Also recorded here because it is the one thing this round improved permanently: the metric that made both verdicts
possible, and the discipline that a candidate is judged on the number its mechanism moves — which is why the
six-round run took 22 minutes and produced an answer instead of a fifth ambiguous "no claim cleared the floors".

### 4.187 A metric for the thing the candidate moved, and what it says about the candidate (2026-11-09)

§4.186's verdict was that no candidate on this shape could pass, because the number the mechanism moves was not a
model metric. It is one now: **`mhd_lane_dup_pct`**.

**Definition, and the two decisions inside it.** The metric is the **busiest lane's** share of the stream bytes it
posted that were a range already sent once, read from the daemons' per-lane window lines inside the stage window.

- **The worst lane, not the mean.** The carrier does this to one or two lanes while the rest look healthy
  (§4.185: 68.6 % against 12.6–15.8 %), so a mean divides the finding by however many lanes the run happened to
  have.
- **A share of the busiest lane, not a byte floor.** The first version used 1 MiB and read **98.6 % on every arm
  and every round** — it had selected the tunnels' control connections, which post two or three megabytes and
  re-send almost all of them. A tenth of the busiest lane separates a lane that carries bulk from one that carries
  a greeting, on any cell, without a constant that describes this host's traffic.

**Validated against the archived A/B** (§4.186), through the module's own API rather than by hand:

| arm | `mhd_lane_dup_pct` per round |
|---|---|
| candidate | 30.35 / 24.93 / 30.15 |
| candidate twin | 28.89 / 30.67 / 23.43 |
| baseline | 14.52 / 14.63 / **73.18** |

**And it corroborates the revert for a second reason.** The candidate avoided the baseline's 73 % storm round —
but in the two rounds where the baseline was healthy, its duplicate share (14.5 %, 14.6 %) was **lower than the
candidate's** (24.9–30.7 %). So the ranges frame was not merely unproven: on these three rounds it looks mildly
*harmful*, which is consistent with its lower throughput point estimates. `b3e9778` was right twice over.

**What this unlocks.** A future candidate on this shape can be pre-registered on a number the mechanism actually
moves, and the model now carries it end to end: a declaration with a materiality floor, a row on the benchmarks
page in both languages, `just bench-selfcheck` at **774** checks, and a **typed absence with a reason** when the
run did not enable the switch — because a run that never logged a lane has no view of this and must not read as a
clean zero.

**What it does not do.** It does not make the storm deterministic (the baseline read 14.5 % twice and 73 % once in
three rounds), so a future pre-registration still has to say how many rounds it takes to see the worst lane, and
the answer is "more than three". It also does not change the fact that this metric needs `MOLEHILL_MHD_STATS=1`
with `RUST_LOG=molehill=debug`, so a performance claim and this reading cannot come from the same run.

**What followed, and what it cost.** D3's second pre-registration named the fallback candidate — **admit repairs
against the window** — on the same `l4-mux8` shape with **six rounds** (three were shown not to be enough), judged
on `mhd_lane_dup_pct` with throughput demoted to a guard. It was built (`397ece1`), measured, and **reverted**
(`0c26311`): the candidate stormed in three rounds of six against the baseline's one and cost 21 % of the
throughput, reproducible by its own twin. §4.188 records the numbers and the refutation they carry — bounding
repairs feeds the loop it was meant to damp — and the plan's stage table now says D3 is **stopped for a human
decision**, because the only lever left is the declaration rule itself and re-opening RACK is a plan change.

### 4.186 The ranges frame was built, engaged, and **reverted**: the pre-registered cell could not judge it (2026-11-09)

D3's candidate was implemented (`3043039`), measured against the binary at `346d5e3` in the pre-registered cell,
and reverted (`b3e9778`). Both halves matter: the mechanism worked, and the experiment failed.

**What was built.** A `Ranges` frame (`Kind::Ranges`, 13 bytes fixed plus 6 per entry, at most four entries)
carrying `stream`, the contiguous `base` its deltas are measured from, and `(delta, len)` pairs. A receiver sets
`RangesOwed` when a packet lands **above** a stream's contiguous frontier and emits one report at most every 10 ms.
A sender retires every recovery record **wholly inside** a named range, samples the trip from the newest of them,
and feeds the confirmed bytes to the same growth rule a number acknowledgement feeds. It never moves the stream's
`base`: a range is out of order by definition, and a reader handed those bytes would read a hole as data. Five
tests, including the one that pins "wholly inside, never overlapping".

**The run.** `cost`, tunnel leg, `loss5`, arm `l4-mux8`, `--scale 1`, three rounds, `--aa` plus `--ab-arm` so the
same run measured the candidate, a second copy of it, and the baseline: 669.1 s, 18 samples.

| arm | `throughput_gbps` per round | median | dup (aggregate) | worst lane | `rng` retired |
|---|---|---|---|---|---|
| candidate | 0.2557 / 0.3048 / 0.2526 | **0.2557** | 22.7 % | **30.4 %** | 24.6 MB |
| candidate twin | 0.2429 / 0.0790 / 0.3366 | 0.2429 | 23.5 % | 30.7 % | 21.0 MB |
| baseline | 0.3799 / 0.3422 / 0.2973 | **0.3422** | 22.0 % | **73.2 %** | — |

**The mechanism engaged, so the run is not void.** `rng` shows 21–25 MB retired through range reports, and the
lane-level reading is what the change predicted: without it one lane sat at **73.2 %** duplicate share; with it,
the worst lane in either candidate arm was **30.4 % / 30.7 %**.

**And the experiment failed anyway.** The model's verdict is *no claim cleared the floors*, and the cell resolves
`throughput_gbps` at **53 %** — about 109 rounds would be needed to see a 10 % effect. The candidate's point
estimate was also lower than the baseline's (0.2557 against 0.3422), inside that noise but pointing the wrong way,
and one candidate round read 0.079 (a storm). **A change that might cost throughput is not kept on a lane-level
side effect**, so it was reverted whole; `git revert b3e9778` re-lands it unchanged when a cell can judge it.

**The real finding is about the pre-registration, and it is my error.** I registered a primary metric on a cell
whose own noise is five times the effect size, in the same document that says a claim must clear the run's own
measured noise. The evidence that the mechanism works is a **counter** — worst-lane duplicate share, 73 % → 30 % —
and that counter is not a model metric, so the pre-registration could not name it. Two ways out, and the next
round picks one **before** re-landing the frame:

1. **Promote the lane-level duplicate share into `model.METRICS`**, fed from the ledger lines the runner already
   archives, so the primary is the number the mechanism actually moves and the gate can police it like any other.
2. **Find a cell where the storm is deterministic** (a longer stage, or more lanes) so `throughput_gbps`
   separates by more than 100 % and survives the scatter.

Until one of those exists no candidate on this shape can pass, and a number that clears a floor by luck is worse
than no number.

**One thing that did survive the revert, because it is a harness fix and not the candidate**: the fake link now
drives `on_timeout`, as a real pump does. Without it the harness only ever repaired on the three-packet
threshold, so a hole with no later packet behind it waited for a timer that never came — which is why a change in
the datagram sequence was enough to stall `five_percent_loss_on_a_fake_link_delivers_without_storming` short of
the end. The earlier pass was luck.

### 4.185 The storm is reproducible, the pre-registration is written, and §4.182's contradiction dissolves (2026-11-09)

**Reproduction.** `cost`, tunnel leg, `loss5`, arm `l4-mux8` (eight lanes), `--scale 1`, three rounds plus the A/A
twin, 419.2 s. One lane went into the regime and its peers did not:

| lane | sent | ack | dup | late | pfx | num |
|---|---|---|---|---|---|---|
| 1054200012 | 1 570 MB | 479 MB | **68.6 %** | 50 MB | 176 MB | 333 MB |
| 1054200008 | 1 020 MB | 862 MB | 13.8 % | 98 MB | 158 MB | 812 MB |
| 1054200011 | 1 063 MB | 900 MB | 14.7 % | 114 MB | 178 MB | 840 MB |
| 1054200014 | 1 028 MB | 860 MB | 15.8 % | 121 MB | 186 MB | 796 MB |

Cell `throughput_gbps` 0.249 / 0.3749 / 0.3548 (twin 0.3035 / 0.4058), `retrans_segments` 182–208,
`dropped_packets` 47 476–62 646, `wire_per_visitor_byte` 1.17–1.19. So the doubling of lanes does not *guarantee*
the storm, but it produces it often enough to judge a change against — and the lane-level reading names which lane
paid.

**And the contradiction §4.182 recorded dissolves.** It said `flight` at 8.4 MB against `cwnd` at 102 KB could not
be explained: either something reduced `cwnd` (nothing does) or the flight counted bytes no window admitted. It is
the second, and now with a mechanism:

- a record whose bytes the peer already holds stays in the flight while neither the cumulative frontier (dead,
  §4.183) nor the 64-number bitmap nor that stream's prefix can reach it;
- a repair is admitted regardless of the window, so it can be *sent* while the window is closed.

So `flight` is "records we have not been allowed to forget", not "bytes the window admitted", and on a path where
retirement is blind it grows to whatever the peer already has. That is why `cwnd − flight` sits at zero on a lane
that is delivering at 479 MB over a minute.

**The pre-registration is in the plan**, under D3, written before any code: the cell above, `throughput_gbps` as
the primary against both its materiality floor and the run's own A/A floor, eight guards (worst-lane `dup`,
receiver `arrdup`, `late`, `retrans_segments`, `dropped_packets`, `rtt_p99_ms`, `cpu_s_per_gbit`, `wedge_count`),
two cross-guards (`bulk-n`, `rate1000`), and a whole-revert rule.

**The candidate is byte-range acknowledgement** — the ack names the byte ranges the receiver holds above its
contiguous prefix, and the sender retires any record wholly covered by them. That attacks the chain at its root
(step 3: no out-of-order evidence). The alternative, admitting repairs against the window, only silences the
amplifier (step 5) and is recorded as the fallback for the case where this one is falsified — not as a cheaper
first try, because it leaves every lane re-sending what the peer already has.

**The design, fixed before the code so the next round does not re-derive it.** A separate optional `Ranges` frame
rather than a longer acknowledgement block: the ordinary ack stays 28 bytes and the extra cost is paid only when
there is out-of-order data to name. It carries up to four ranges above the reported prefix (`delta u32` +
`len u16`), the receiver emits it when its pieces hold bytes above `arrived()`, and the sender uses it to retire
**recovery records** — `flight` falls — while leaving the stream's `base` where it is, because `base` is the
contiguous frontier and the stream's semantics do not change. A record retired by a range leaves nothing for the
later prefix to find, so the two paths cannot double-count. The predicted chain is the one the measurements
imply: `flight` down → `cwnd − flight` open → new data admitted → the queue drains → acknowledgements land inside
`loss_delay` → the timer stops over-declaring → growth resumes.

### 4.184 The receiver's duplicate share: the re-sends really are bytes it already held (2026-11-09)

§4.183 left one question: is the carrier re-sending data the peer already holds, or carrying real loss? The
receiver could not answer it, because a repeated range is dropped by offset inside the stream and no counter saw
it. It can now: `recv` (stream bytes that arrived) and `dup` (how many of them the stream already held), taken from
the frontier and the held bytes around each insert, so the difference is exactly what the packet carried against
what was news.

**Measured on `loss5`, `cost`, two rounds plus the A/A twin, `--scale 1`, 343.9 s:**

| side | reading |
|---|---|
| sending lanes (`sent` 790–1 185 MB) | re-send share **13–28 %**; `ack` 654–1 019 MB; `late` 94–117 MB |
| receiving lanes (`recv` 750–1 137 MB) | **8.5–37 %** of arriving bytes were already held |
| cell `throughput_gbps` | 0.2208 / 0.2906, twin 0.3775 / 0.2162 |

**Two independent instruments agree.** The sender says one byte in five to one in eight was a range already sent;
the receiver, counting separately and in the other direction, says one byte in twelve to one in three arrived as a
duplicate it dropped. That is the first cross-check of the ledger against something that is not the ledger, and it
holds: `dup` tracked the sender's `rep` on every lane.

**And this run did not reproduce the state §4.182 measured.** There, several lanes re-sent 70–90 % of their bytes
and the cell carried 0.1667 / 0.1653. Here every lane sat at 13–28 % and the cell carried 0.2208 / 0.2906. Same
binary, same condition, same command shape, **different state** — and the two runs' wall times differ with it
(537 s against 344 s, because a stormy stage drains for longer). So the 70–90 % figure is a *regime* the carrier
falls into, not its steady cost, and that changes what a fix has to be judged on.

**Which is why D3 has not started.** A candidate judged on a cell that may or may not be in the bad regime is
exactly the trap this file keeps recording: one run would show a gain, the next would show nothing, and neither
would be about the change. Before any fix there has to be a cell where the storm is reproducible on demand.

**The deterministic route is closed, and that is itself a measurement.** A memory link that drops one datagram in
twenty delivers every byte with **zero timer declarations** — the threshold finds all four losses and the timer
never fires, because nothing in a memory link is ever late (`five_percent_loss_on_a_fake_link_delivers_without_storming`
now pins that shape). So the storm is not "loss plus a wide flight": it needs the **queueing delay** a real qdisc
adds, which is what makes an acknowledgement arrive later than `loss_delay`. A fake-clock reproduction would have
to model that queue; the cheaper reproduction is at the bench, with a cell heavy enough that the receiver drains
slower than the sender posts — more concurrent lanes on the same shaped leg, which is what §4.182's stormy run had
and §4.184's did not.

**What this does establish for the fix's shape.** The re-sent bytes are, in the receiver's own accounting, bytes it
already had: the recovery is spending the path's capacity to hand the peer what the peer already holds. Two ways
out, both to be judged in the reproduction: give the acknowledgement better out-of-order evidence (the byte
prefix stops at the first hole; the only other evidence is 64 packet numbers), or stop re-sending a range the peer
has already buffered — which the wire currently cannot express.

### 4.183 Every lane's cumulative frontier is dead: the peer never sees number 0 (2026-11-09)

Chasing §4.182's `num=0` produced something larger than the question. Across **three runs and forty lanes**, on
both roles:

| reading | what it says |
|---|---|
| accepting end's `first_seen` | **1**, on every lane — never 0 |
| dialing end's `first_posted` | 0 — so number 0 *was* posted |
| dialing end's `below`, every second | **0**, for the lane's whole life |
| `unrouted` in the listener's cadence line | 0–9 per second, i.e. no flood of dropped unknown ids |
| new line `mhd first numbered packet this end ever saw` | `number=1 frame="stream"` on the accepting side, `number=0 frame="stream"` on the dialing side |
| listener's admitting call, now logged | **zero** refusals |

So the dialer's first numbered packet is a stream frame, it is posted, and the accepting end never sees it — while
the *other* direction delivers number 0 normally. One packet, at the head of the lane, in one direction.

**The consequence is structural and it is the finding.** The accepting end's `below` is the cumulative frontier,
and it only advances by filling the *first* hole. A lost packet is re-sent as a **new number**, so number 0 is
never filled; `below` therefore stays 0 for the life of every lane. Retirement then runs on the only two paths
left: the 64-number acknowledgement bitmap, and the per-stream byte prefix. `num=0` on §4.181's collapsed lane
and the 70–90 % duplicate share on §4.182's are both this.

**Two things were checked and are not the cause.** The plain two-connection seam is correct — a new test,
`the_accepting_end_counts_the_dialers_first_numbered_packet`, hands a fresh dialer's first two datagrams to a
fresh acceptor and gets `first_seen = 0` and `below = 1`. And the listener is not refusing the opening: its
admitting call used to discard `on_datagram`'s result silently, which is now a debug line, and it fired **zero**
times in a rerun. So the packet is lost between the two daemons, not refused by either.

**Why this matters more than one lost packet.** The byte prefix is per stream and stops at the first hole *in that
stream*; the receiver buffers bytes above a hole (`RecvBuf` keeps pieces), but the acknowledgement cannot say so —
the only out-of-order evidence the wire carries is 64 packet numbers. At 5 % loss with a wide flight there is a
hole every twenty packets, so everything more than 64 numbers behind `largest` can only be re-sent. **That is the
70–90 % duplicate share**, and it is a wire-evidence limit rather than a controller question.

**The next measurement is the one that decides it**: count, at the *receiver*, how many arriving bytes its stream
already held — duplicate arrivals — beside how many were new. If most arriving bytes are duplicates the receiver
already had, the carrier is re-sending data the peer holds and the fix is better out-of-order evidence (a wider
bitmap or byte ranges, both inside the unreleased 6.0 dialect). If they are not, the re-sends are carrying real
loss and the fix is elsewhere.

**What was added to make that measurable**, all diagnostic and all off the default path: the first-numbered-packet
line above, the logged admitting refusal, and one new test at the two-connection seam. No behaviour changed.

### 4.182 D2's second run: on a lossy path most of the wire is repair, and the healthy lane is the exception (2026-11-09)

`cost` on the carrier leg under `loss5`, two rounds plus the A/A twin, `--scale 1`, 537.7 s, same binary and
same switch as §4.181. The cell's own resolution is poor on this shape (throughput resolves 21.8 % against a
10 % floor, ~19 rounds would reach it), which is why nothing here is a claim — the per-lane ledger is what the
run is for.

**Per lane over the whole stage.** `sent` and `ack` are the ledger's wire bytes and unique delivered bytes; `dup`
is `rep` as a share of `sent`; `late` is acknowledged bytes whose record had already been declared lost.

| lane | sent | ack | dup | late | pfx | num |
|---|---|---|---|---|---|---|
| 1748352497 | 4 440 MB | 472 MB | **89.1 %** | 84 MB | 168 MB | 383 MB |
| 246251900 | 3 766 MB | 474 MB | **86.7 %** | 80 MB | 126 MB | 425 MB |
| 1748352500 | 3 687 MB | 480 MB | **86.6 %** | 81 MB | 181 MB | 368 MB |
| 246251902 | 1 955 MB | 566 MB | 70.4 % | 88 MB | 126 MB | 527 MB |
| 246251901 | 1 036 MB | 651 MB | **33.9 %** | 77 MB | 136 MB | 615 MB |
| 246251899 | 1 172 MB | 682 MB | 41.2 % | 64 MB | 145 MB | 604 MB |
| 3227764096 | 1 134 MB | 981 MB | **12.1 %** | 101 MB | 158 MB | 936 MB |

**Three things this establishes, and one it does not.**

1. **The carrier spends most of its capacity re-sending.** On the busiest lanes seven of every eight bytes that
   leave are a range already sent once, and the ratio between `sent` and `ack` is 8–9×. The cell's ~0.22–0.30
   Gbit/s is what is left after that, not what the path carried.
2. **The healthy lane is the low-`dup` one, not the fast one.** `3227764096` re-sent 12 % and delivered 981 MB —
   more than any other lane — while lanes with 89 % duplicate share delivered half that. The correlation runs the
   way the mechanism predicts: repair traffic is the cost, not the cure.
3. **`late` is a large share of `ack` on every lane** — 64–101 MB against 472–981 MB delivered, i.e. 10–20 % of
   confirmed bytes arrived after the lane had given up on them. So a substantial part of the declaration traffic
   is provably spurious, and each spurious declaration buys a re-send.

What it does **not** establish: whether the re-sends come mostly from the *timer* declarations (as on §4.181's
collapsed lane, where `num=0` and 521 561 timer firings) or from re-declaring the same range repeatedly after a
real loss. Both produce this table. Distinguishing them needs the per-lane `loss=Nt/Nm` split per second, which
the ledger now carries and this summary has not yet read.

**One contradiction to record rather than explain away.** The single-stream `bulk-1` run on the same `rtt100`
path (§4.181's next measurement, 47.6 s) shows a healthy lane — `cwnd` climbing 102 KB → 402 KB in five
seconds, `grow` 50–120 KB/s, `dup` 1–2 %, zero loss declarations — but it also shows **`flight` at 8.4 MB
against a `cwnd` of 102 KB**, with `buffered` identical to the byte. My model says `write` admits at most
`cwnd − flight`, that `cwnd` only ever grows, and that the ceiling binding here was 16 MiB; so either `cwnd` was
once ~8 MB and something reduced it (nothing in the code does), or the flight counts bytes no window ever
admitted. The existing test `the_flight_never_outgrows_the_window` passes, which means it does not model the
state the artifact shows. **This is a counter-versus-code question, and it goes before the next candidate**:
a fix designed against a window whose own accounting is unexplained would be a guess.

**What D3 will therefore be.** Not a controller. The plan's order puts the recovery side before the window, and
two runs now say the same thing from opposite directions — a loss-free path where one lane never retired a
record by number (§4.181) and a lossy path where seven of eight bytes are repairs (§4.182). The candidate is the
declaration-and-repair path, and the first thing it needs is the `loss=Nt/Nm` split per lane per second plus an
answer to the `flight ≫ cwnd` contradiction above.

### 4.181 D2's first run: on a loss-free path, one lane's window never leaves its initial value (2026-11-09)

D2 opens with `rtt100` on the carrier leg, because a path with no random loss removes the obvious suspect.
One round, two arms (the profile's own A/A twin), 60 s stage, `--scale 1`, `MOLEHILL_MHD_STATS=1` with
`RUST_LOG=molehill=debug`. `just bench doctor` before it: the binary is `sha256 bb853ab36a52dd74`, built from
`8e9a189`, `stale=False`.

**The cell carried 220 Mbit/s and the run resolved nothing** (every difference inside its own scatter; the A/A
floor on `cpu_cores` was 317 %), which is what a single round buys and why the plan asks for more. The
per-lane ledger is what this run was for, and it found something a cell median hides.

**Of the four server lanes, three behaved and one collapsed.** Per lane over the stage, from the ledger:

| lane | `cwnd` (median / max) | sent | ack | dup | timer losses | window refusals |
|---|---|---|---|---|---|---|
| 4164090728 | 1.07 / 1.57 MB | 1 036 MB | 1 008 MB | **2.6 %** | 1 126 | 89 969 |
| 4164090729 | 0.82 / 1.02 MB | 1 002 MB | 531 MB | 46.8 % | 86 587 | 40 065 |
| 4164090730 | **13 490 B / 13 490 B** | 1 909 MB | 226 MB | **87.9 %** | **521 561** | 67 831 |

**Five facts about the collapsed lane, each read from the line rather than inferred.**

1. **Its window never moved.** `cwnd=13490` in every second of 122 s — which is exactly `INITIAL_CWND`
   (ten full packets) — and `grow=0` in every one of them. `acked_since_growth` stayed 0.
2. **It posted twenty times what it delivered.** 1 909 MB sent, 226 MB confirmed, 87.9 % of it a re-send.
3. **The peer's frontier was caught up, not pinned.** `below=1 827 856`, `largest=1 827 855` — the hole-pins-the-
   frontier case, which the code has a comment and a test for, is **not** what happened here.
4. **Not one record was retired by the packet-number block.** `num=0` for the whole stage; the 200 MB that was
   confirmed came entirely from the byte prefix, and the rest of the table left by expiry.
5. **The estimator was healthy.** `srtt_us=200 172` and `ceiling=16777216`: the round trip was measured and the
   ceiling was 16 MiB, so neither an unseeded timer nor a low ceiling explains a 13 490-byte window.

**And the same binary's other lanes grew normally**, to 0.8–1.5 MB with 1–2 % duplicate share. So this is a
**bistable** failure — a lane that trips into it stays in it — which is also the shape of the 0.24–0.40 Gbit/s
scatter §4.170 measured on the lossy cell and §4.178 could not explain.

**What this rules out, stated because it is as useful as what it rules in.** The window is not limited by the
ceiling (16 MiB), not by the peer's credit (`refus=c` is 7 in the whole stage), and not by a stalled number
frontier (`below = largest + 1`). What remains is that **`progress` never became true on that lane**, because
`Acked::progress` is set only when a record is retired *by number*, and no record ever was.

**Three candidates for the next run, in the order the evidence already points.**

1. **The storm is self-sustaining through the shaper's queue.** Repairs are exempt from `cwnd`, so a lane that
   starts re-sending keeps re-sending; the queue fills; every acknowledgement then arrives later than
   `loss_delay`; every packet looks stale; the timer declares them all. `lost=41 160m` in a single second against
   a table of ~1 500 records is what a lane re-declaring everything it holds looks like.
2. **Records expire before their acknowledgements.** `LOST_HOLD` is 3 s, the queue delay here is seconds, and a
   record that is reaped leaves without a ledger entry: that is exactly the `num=0, late=0, pfx»0` pattern.
3. **Something queues a repair without a loss declaration.** Second 1 already reads `rep=98 %` with `loss=0t/0m`,
   which no mechanism in the code explains yet — `state.send.repair()` is called only from the two loss handlers
   — so either the reading or the mechanism is wrong, and one of them has to be found.

**The next measurement is smaller than this one.** `bulk-1` on the same path: one lane, one stream, no echo, no
churn, no datagram series, so the ledger's numbers belong to exactly one flow. Then the same cell under `loss5`.
Only after that is there a candidate worth building, and the plan's D3 order still holds — accounting and
recovery first, a controller only if these say the window is the limit.

### 4.180 D1: the per-lane ledger, and the instrument defect it found on the way (2026-11-09)

The plan's D1 asked for evidence that can tell a window problem from a scheduler problem, an
acknowledgement problem, a repair problem and a counting problem. Three of those had no counter at all, and the
one that did was aggregated across lanes.

**What the window line now carries, per lane, once a second.** A `SendLedger` on the connection, taken by the
same report that prints `cwnd` so the two describe one interval:

| field | what it answers |
|---|---|
| `sent` / `rep` / `pkt` | bytes handed to packets, how many of them were a range already sent, and how many packets carried them — `rep` as a percentage is the duplicate share §4.179 said nobody could compute |
| `ack` | stream bytes the peer confirmed, counted from the **stream's frontier**: a byte that crossed the wire twice is one byte of delivery |
| `num` / `pfx` | **record** bytes retired by the packet-number block, and by the stream byte prefix — `pfx` being the path that does not grow the window |
| `late` | of those, the records that had already been declared lost: a declaration the path did not support, or one whose range a repair then delivered |
| `loss` = `Nt` / `Nm` | declarations by evidence: N later packets seen, versus the timer finding live records older than the reordering window |
| `probe` | timer firings that re-sent one range because nothing was stale |
| `grow` | how much `cwnd` actually gained in the second |
| `refus` = `w`/`b`/`c`/`a` | writes refused by the window, the buffer ceiling, the peer's credit, the age gate |
| `stop` = `none`/`quiet`/`closed`/`other` | passes that built nothing, by reason — the per-lane shape of the process-wide `stats::note_none_reason` table |

**Two findings, and one of them is a defect in the instrument rather than in the carrier.**

1. **The byte prefix does not grow the window, and now the line says how much it retires.** `confirm_prefix`
   frees recovery records the number frontier can no longer name — the §4.108 escape hatch — but it returns its
   byte count to nobody, and `apply_ack` never fed it to `Acked::bytes`, so those bytes buy no window. That may
   be right (they are not evidence of a larger pipe) or wrong (they are evidence the peer has the data), and the
   point of `pfx` is that the question is now measurable instead of arguable. `a_prefix_confirmation_retires_bytes_without_growing_the_window`
   pins the current behaviour with a block whose frontier is pinned at zero while a thousand later packets
   arrived — the state a hole in another stream produces.
2. **A write refused by `MAX_BUFFERED` was counted as refused by the window.** `write` computes
   `room = credit.min(window).min(held)` and then classified the refusal by comparing `credit` against `window`
   only, so the third minimum was invisible: every buffer refusal was reported as a window refusal. On a lossy
   path that is the difference between "acknowledgements are not coming back" and "they are coming back too
   slowly for what was admitted", and §4.174's reading of the cell leaned on exactly that distinction. The
   process-wide counter has a `buffer` class now, and the per-lane one has `refus=b`. The tie-break keeps
   `credit` winning a three-way tie, so the long-standing number still means what it meant for the two cases it
   could already tell apart.

**A third finding is about this entry's own first draft.** The ledger began by counting *records*: `acked` was
the sum of every retired record's bytes. The repaired-byte test then read `ack=9541` for an 8 192-byte payload,
because a repaired range is two records and both were retired — the repair by number, the original by the prefix
that the repair's arrival moved. That is not a duplicate delivery, it is one delivery counted twice, and a
duplicate share computed from it would have been off by exactly the number it exists to measure. `ack` is now
the **stream frontier's** advance, which cannot be fooled that way, and the record counts stayed as `num`/`pfx`
where they belong: they answer "which path retired the record", not "how much was delivered". The lesson is the
one §4.179 already wrote down — a plausible number is not a checked one — and the test that caught it is
`a_repaired_byte_crosses_the_wire_twice_and_is_counted_once`.

**What it costs, and why it is not behind the switch.** Two adds per data packet on the send path, a match per
stopped pass, and nothing else: no clock read, no allocation, no lock. It is *not* gated on
`MOLEHILL_MHD_STATS`, because a unit test runs with the switch off and counters that only move under a switch
are counters the tests cannot pin — and because the line that prints them is already inside the stats gate, so a
production run pays the adds and never prints the line.

**What D1 does not do, stated so it is not read as more.** It does not estimate a delivery rate (that is D3's
estimator, built on `ack`), it does not decide which seconds count (D2 filters the series to intervals with a
sustained backlog, which is what §4.179 says the plateau reading should have done), and it changes no behaviour:
five new tests pin the accounting, and nothing in the repair, growth or scheduling paths was touched.

### 4.179 The plateau was an artifact of the stage ending, and the growth arithmetic was wrong too (2026-11-09)

The plan was replaced with a D0–D5 version whose subject is this defect, not its classification. Before it could
run, re-reading the artifact that produced §4.174 killed two claims I had written on top of it.

**"`cwnd` plateaus at 1.5 MB" is not what the series says.** The `cwnd=1471759` line is at
`04:43:27`, and it is the **last second of the bulk stream**, not the middle of it. The same lane one second later
reads `buffered=51`, then `buffered=0` for the rest of the stage, with `acked_since_growth` frozen at `466754` —
that is a stream that finished and a stage still running the small-packet series. So the maximum is an
end-of-flow value. Across the flow it is not flat at all: 90 383 → 1 471 759 in **53 s**, a steady climb. The
"plateau" was one row read as if it were a level, and the run that was supposed to explain it (one `--scale 2`
stats run) would have explained nothing.

**And my 6.7 KB/s arithmetic had a false premise.** `on_ack` grows the window once per `cwnd` worth of
acknowledged bytes, which is one round trip *only if the acknowledged rate is `cwnd / rtt`*. On this path it is
not: the same lane's `flight` has a median of 1.12 MB and a peak of 13.7 MB against a `cwnd` median of 992 864,
because **a declared loss leaves the flight and its repair enters it outside the window**. So `cwnd` worth of acks
arrives faster than one per RTT, and the measured climb is **≈28 KB/s**, four times §4.177's number. "The window is
nearly frozen" is withdrawn with the plateau.

**What survives, and what it changes.** The defect itself is unchanged and still measured on the product's own
path: six rounds with the twin at 0.2886 / 0.3332 Gbit/s against ~1.19 clean, and `rate100_shallow` at 0.0776 with
1 381 drops against rathole's 0.0955 with 17. What changes is where to look. The window does climb, and it climbs
past the point where `cwnd / rtt` would explain the cell's throughput — 1.47 MB over 200 ms is 59 Mbit/s per lane,
while the cell carries ~290 across four. **So a large share of what crosses the wire is repair traffic, not new
data**, and the first question is no longer "why does the window stop" but "how much of the second is the lane
sending the same bytes again". That is measurable with the counters that already exist plus the ones D1 adds, and
it is the row in the plan's decision table that ends in "fix the accounting, not the controller".

**Where the three reversals now sit.** §4.178 recorded that the penalty is a defect rather than a trade-off;
§4.177 recorded that the cheap recovery rules are arithmetically dead; this entry records that the mechanism I
named in §4.177 was the wrong one, and that the arithmetic behind it was off by four because of an unstated
assumption. All three stay in the file: a wrong mechanism that was written down and then measured is worth more to
the next reader than a clean paragraph, and the pattern in all three is the same — **I stated a mechanism from
reading code and one aggregate row, then treated it as measured.**

**The plan that follows from it.** `MHD-FINALIZATION-PLAN.md` (third version) is D0 sync → D1 per-lane
instrumentation → D2 two attribution runs → D3 one candidate, submitted and reverted alone → D4 criteria → D5
closing. Its decision table sends "posted bytes disagree with delivered throughput" to the accounting, and only
"continuous data, healthy acknowledgements, a window that is genuinely the limit" to a rate-driven controller.

### 4.178 The lossy-path penalty is a defect, not a trade-off: doubling the flow does not help (2026-11-09)

§4.177 closed P4 as "no controller, mechanism named", and the entry after it framed the lossy-path cost as
something the product could accept and document. **Both of those were too generous, and the human's objection —
"isn't this a hard defect?" — is the correct reading.** The test that decided it was cheap: run the same cell
with the stage twice as long, because if the window is merely growing slowly, a longer flow gives it time.

| stage | `throughput_gbps` | `rtt_p99_ms` | `retrans_segments` |
|---|---|---|---|
| 60 s, six rounds, with A/A | 0.2886 (0.242–0.403), twin 0.3332 | 2 038 | 153 |
| **120 s** (`--scale 2`), single arm | **0.2306** | 1 462 | 69.5 |

**Doubling the duration did not raise throughput.** The 120 s reading is single-arm with no twin, so the honest
statement is "not higher" rather than "lower" — but the hypothesis that the penalty is a transient of a short
window was that the window would climb, and it did not. So the penalty is a **level at about a quarter of clean
throughput, independent of how long the flow runs**, which is the shape of a defect rather than of a trade-off
someone chose.

**And the mechanism is not yet explained, which is the part I have to be careful about.** My arithmetic said
additive growth adds 1 349 B per round trip — 6.7 KB/s at 200 ms — so over 120 s the window should have gained
~800 KB and the rate with it. It did not. So either `cwnd` does not grow the way I read the code, or something
else caps it near 1.5 MB. §4.174's series reached 1 471 759 and stopped there (that value was both p90 and max),
which is consistent with a plateau that my model of the growth rule does not predict.

**What that leaves.** The classification changes: this is an **unfixed defect** — a carrier that carries a
quarter of its capacity on a path that loses 5 %, for as long as the flow lasts, and that drops two orders of
magnitude more than its peers on a shallow queue (§4.176). What does *not* change is that no controller has been
written and none should be written on the current evidence, because the next question is smaller and comes
first: **why does `cwnd` plateau at 1.5 MB on that path?** That is one stats run at `--scale 2` with
`RUST_LOG=molehill=debug`, reading the same per-lane series that produced §4.174, and it is the precondition for
either a fix or an honest "we do not know".

**Recording the reversal rather than the conclusion.** Two framings in a row were wrong here — "accept and
document" and then "it is a transient" — and both were mine, made before the cheap test that settled it. The
lesson is the one this file already carries, applied to a judgement instead of a number: **test the classification
before putting it to someone as a decision.**

### 4.177 P4 closes without a change: the candidate rule is arithmetically dead, and the real fix is a redesign (2026-11-09)

§4.176 built the cell that makes "should the window react?" falsifiable. Before spending a run on a candidate,
two pieces of arithmetic from the code killed the obvious one.

**Candidate 1 — re-enter slow start after a full window of clean delivery.** The rule reads well: a loss followed
by an entire window delivered is not congestion. But at a 5 % drop rate the probability that a whole window
arrives cleanly is `0.95^n`, and the windows here are large:

| window | P(clean) |
|---|---|
| 10 packets | 0.599 |
| 100 packets | **0.0059** |
| 500 packets | 0.0000016 |
| 1 100 packets (1.47 MB) | ~0 |

So the rule fires only while the window is a few tens of packets, and stops firing exactly when it would start
to matter. **It cannot fix the cell it was designed for**, and the calculation costs nothing while a run would
have cost ten minutes and a story about noise.

**The growth rate is the real constraint, and it is one packet per round trip.** `on_ack` adds
`MAX_STREAM_PAYLOAD` (1 349 B) once per window's worth of acknowledgements, so at a 200 ms round trip the window
grows at **6.7 KB/s**. Climbing from the ~600 KB slow start reached to the 1.47 MB observed takes two minutes —
longer than the stage. **The window is not "recovering slowly"; it is very nearly frozen at whatever value slow
start happened to reach when the first loss landed.** That is why the same cell reads 0.24 to 0.40 Gbit/s: the
spread is where the first loss fell.

**And the plan's own P4 remedy does not apply.** §12's P4 says "window too small → ceiling from calibrated
delivery-rate × min_rtt". On this path the ceiling is already the 16 MiB cap while `cwnd` is 1.47 MB — raising a
ceiling `cwnd` never reaches changes nothing. **The lever is the growth rule, not the bound.**

**What would actually work is a window that is a function of the delivery rate rather than an integrator** — a
BBR-shaped `cwnd = rate × min_rtt × gain`, recomputed rather than accumulated. That is a redesign of the
congestion controller, not a fix to it: it needs the delivery-rate estimator in the steady-state path, an
app-limited rule, and a guard matrix this model cannot currently supply (one cell judges it, and §4.176 says the
shallow-queue cell would punish a mistake in the other direction). The estimator itself is not the blocker —
§4.58 measured the posted-bytes rate against the stage's own throughput at 0.91–1.05 across 32 cells. What was
never tried is a *window* driven by it; pacing was rejected because the socket already delivers what is posted,
which is a different question.

**So P4 closes as: no change, with the mechanism named.** The lossy-path cost is real (a quarter to a third of
clean throughput), its cause is measured (the window freezes where slow start left it, growing 6.7 KB/s after
that), the cheap fixes are arithmetically excluded, and the one that would work is a controller redesign that
belongs to its own plan with its own guard matrix. Recording that is more useful than a patch nobody can judge.

**What is left for the model, if that plan ever runs.** A cell where the *offered rate* is held steady while the
window is varied — neither `rate100_shallow` nor `loss5` does that; both let the sender run free. Without it, a
rate-driven window's central claim (that it holds a rate rather than discovering one) has nothing to be measured
against.

### 4.176 The cell that makes loss depend on the sender's rate — and what it says (2026-11-09)

§4.174's conclusion was that the lossy-path cost is the window recovering too slowly, and that any change to the
recovery rule needs a cell where **loss depends on the offered rate** — otherwise "back off on loss" and "grow
faster after loss" are both unfalsifiable. That cell now exists.

**`rate100_shallow`**: 100 Mbit, 20 ms each way, and a **100-packet** queue — about 11 ms of buffering at that
rate, against the 2 000 the other rate cells carry. A sender that keeps a 1 MB window flowing overruns it; a
sender with a loss response backs off. Measured on `bulk-1`, four arms, six rounds, 257.7 s:

| arm | `throughput_gbps` | `dropped_packets` | `retrans_segments` | `cpu_s_per_gbit` |
|---|---|---|---|---|
| `rathole` | **0.0955** | **17** | 3.0 | **0.72** |
| `frp` | 0.0951 | 119 | 5.5 | 1.42 |
| `l4-plain~aa` | 0.0793 | **3 655** | 13 | 3.32 |
| `l4-plain` (mhd) | 0.0776 | **1 381** | 16 | 2.92 |

Floors: `throughput_gbps` **4.5 %**, `dropped_packets` 68 %.

**Three readings, and the middle one is the point.**

1. **mhd carries 18.7 % less than the TCP-based peers** — 0.0776 against 0.0955 — with the two halves of its own
   binary 2.2 % apart, so the deficit is well outside both the floor and its twin's scatter.
2. **The drops are mhd's own.** 1 381 against rathole's 17 — two orders of magnitude — on a queue where the
   peers' own TCP stacks back off and mhd's window does not. That is what this cell was built to show: the loss
   rate follows the sender's rate, so "should the window react?" is now a question with an answer rather than a
   guess.
3. **CPU per carried Gbit is 4× rathole's** on the same cell, 2.92 against 0.72 — consistent with §4.168's 57 %
   on the gigabit cell, and larger here because the sender is working against its own drops.

**What it licenses, and what it does not.** It licenses the experiment §4.153-style pre-registration demands:
change how the window recovers after a loss, judge it on the **tunnel-leg `loss5`** cell's `throughput_gbps`, and
hold `dropped_packets` here as the guard — a change that buys lossy-path throughput by dropping more on a shallow
queue has not fixed anything, it has moved the cost. It does **not** by itself say which recovery rule is right;
that is the next round's job, with the criteria written before the run.

### 4.175 P5's gates and the repeat policy, run on this tree (2026-11-09)

The closing half of §12 is a set of gates, and a gate that is not run is a claim. All of them on the current
tree, after the bench model, the pump and the docs changed:

| gate | result |
|---|---|
| `just check` (fmt, clippy ×2, machete, audit, deny, outdated, docs, ruff, pyright, bench self-check at **770**, tests) | **exit 0**, "pre-push checks passed" |
| `just interop` | **3 passed, 0 failed** |
| `just l3-accept` | **16 PASS**, exit 0 |
| integrations, mhd | **3 consecutive runs, 22 passed each** (94.31 / 93.98 / 93.78 s) |
| `striped_data_channels` alone | **20 passed, 0 failed** |
| integrations, `MOLEHILL_DATA_CARRIER=tcp` | **435 passed, 0 failed** |
| lib tests | **383 passed** |

**The numbers moved in the right direction and by the amount they should.** The suite is 435 with the TCP
carrier against 430 in §4.149 — five more, which is the four new pump tests plus one; and the three full runs
take 94 s against 90 s before, on a tree with five more tests. Nothing here is fast because a check was skipped:
`just check` still runs both clippy passes, the audits and the 770-check self-check.

**Two things this round did not do, stated so the table is not read as more than it is.** It did not run
`just bench gate` — that needs a sweep-shaped run and belongs to the release ritual (§4.143). And it did not
touch `timeline --scale 1.0`, for the same reason.

### 4.174 The attribution lands on the window after all — but on how *slowly it grows*, not on a missing cut (2026-11-09)

§4.173 pre-registered one run to separate three candidates: transmission inflation, repair latency, or a
stream-side stall. It named a fourth possibility only implicitly, and that is the one that won.

Tunnel-leg `loss5`, `MOLEHILL_MHD_STATS=1` with `RUST_LOG=molehill=debug`, one arm, two measured rounds,
278.2 s. The per-second series of one lane over 70 s, which is where medians had been hiding the shape:

| | p10 | p50 | p90 | max |
|---|---|---|---|---|
| `cwnd` | 13 490 | 992 864 | 1 471 759 | **1 471 759** |
| `flight` | 29 | 1.12 MB | 5.93 MB | 13.7 MB |
| `buffered` | 29 | **5.97 MB** | 14.5 MB | 16.5 MB |
| `posted_mbit_per_s` | 0 | **125** | 206 | 277 |
| `worst_retransmits` (cumulative) | 0 | 32 863 | 71 474 | **81 938** |
| `srtt_us` | 200 147 | | | |

**Three candidates eliminated by the counters, and one arithmetic that closes.**

1. **Not inflation.** 81 938 retransmissions over ~70 s is ≈ 1 170/s; at 290 Mbit/s carried that is one packet in
   twenty-three — **4.3 %, which is the loss rate**. The wire is not doing the same work twice.
2. **Not the ceiling.** `cwnd` reaches 1 471 759 against a `ceiling` of 16 MiB, so the ceiling never binds.
3. **Not a stream-side stall.** `posted_mbit_per_s` is 125 at the median and the backlog is 6–16 MB — the sender
   has both work to do and data to do it with, and `worst_ready_ms` is 0.
4. **It is the window, and the arithmetic agrees.** 1.47 MB over a 200 ms round trip is **59 Mbit/s per lane**;
   four lanes is 236 Mbit/s, against a cell that carries ~290. So throughput is set by the cwnd the lane *grew
   to*, not by the path and not by the ceiling.

**Which inverts the lever.** `slow_start` latches off at the **first** loss (the code says so in as many words),
and from there the window grows one packet per window — so at a 200 ms round trip, climbing from 13 490 to
1.47 MB costs roughly twenty seconds of a sixty-second stage, and the lane plateaus at a value it would have
passed in a few round trips under slow start. The failure is **not that the window never shrinks; it is that it
recovers too slowly** — the opposite of the "no loss response" reading in §4.169, and the reason adding a
multiplicative decrease would have made this cell *worse*.

**So P4's entry condition is met, with the sign reversed.** §12's P4 asked for `flight ≈ cwnd` with
`none_reason = window` dominating before a controller could be written. The counters now say the window *is* the
limiter on this shape — `flight` reaching 13.7 MB against a 1.47 MB cwnd, with 16 MB buffered behind it. What a
controller should do here, however, is raise the window faster after a random loss, not lower it.

**What that costs, and why it is not free.** "Re-enter slow start after a loss is followed by a full window of
delivery" is a real congestion-control change: on a genuinely congested path it would be wrong, and the model has
no cell where loss depends on the offered rate (§4.173's point). So the honest next step is still a cell before a
change — a shallow-queue rate cell where mhd's own rate causes the drops — unless the human prefers to accept the
lossy-path number and stop here.

**Retransmission share is worth keeping as a guardrail**: 4.3 % against a 5 % loss rate is what "the repair path
works" looks like, and any future change that lifts throughput by inflating this number is caught by a counter
that already exists.

### 4.173 The lossy-path collapse is measured but its mechanism is not — and the one I named is wrong (2026-11-09)

§4.169 and §4.170 both end by saying the collapse is "the 'no loss response' gap in measured form", and §4.169
adds the repair path: "one range per probe". Reading `recovery.on_timeout` and `conn.on_timeout` to plan the
next step shows that this is not what the code does, and the claim has to come out.

`Recovery::on_timeout` has **two branches**, and only one of them is the probe:

- **Nothing is stale** — the timer fired before the reordering window elapsed — and *this* is the branch that
  re-sends **one** oldest range (§4.126). Its own comment says why: a probe exists to elicit an
  acknowledgement, and re-sending the flight would multiply it by the number of probes.
- **Something is stale** — which is the normal case on a path that is actually losing — and this branch declares
  **every** stale range lost and returns them all. `Conn::on_timeout` then does `note_retransmits(lost.len())`
  and queues a repair for each.

So bulk repair already exists on the branch that a 5 %-loss path takes. **"One range per probe" cannot be the
reason throughput falls to a quarter**, and neither §4.169 nor §4.170 has evidence for *any* mechanism — they
have a level (0.29 Gbit/s against 1.19 clean, six rounds) and a guess that the code does not support.

**What the two claims were, and what is left.** The measured part stands unchanged: on a carrier path that drops
5 %, mhd carries about a quarter to a third of its clean throughput, as a level rather than a lottery, and the
window ceiling is not the limiter. What is withdrawn is the *explanation*, and with it the shape of the decision
I put to the human — "add a loss-response stage, or accept the behaviour" was framed on a mechanism I had not
checked. Asking someone to choose between those two before knowing which lever exists is the same mistake as
§4.157: a conclusion drawn from my own summary of the code rather than from the code.

**Candidates that survive, in the order they are worth measuring:**

1. **Repair latency.** `loss_delay` is `9/8 × max(latest_rtt, srtt)` — about 225 ms on this 200 ms path — so
   every packet the three-packet threshold does not name waits that long. Whether that is a *quarter* of the
   throughput depends on how much of the repair is threshold-driven, which no counter currently says.
2. **Transmission inflation.** A range declared lost and re-declared could be re-sent more than once; the window
   line carries `worst_retransmits` and `worst_repairs`, and `posted_mbit_per_s` against the cell's carried rate
   would show wire working harder for the same goodput.
3. **A stream-side stall.** `worst_ready_ms` and `backlog_*` on the same line distinguish "the sender has nothing
   to post" from "the sender is posting into a path that eats it".

**The measurement that separates them** (pre-registered, cheap, one arm, two rounds, ~7 min): the tunnel-leg
`loss5` cell with `MOLEHILL_MHD_STATS=1` and `RUST_LOG=molehill=debug`, reading per lane — summed, per §4.58's
denominator lesson — `posted_mbit_per_s`, `worst_retransmits`, `worst_repairs`, `worst_ready_ms`, `backlog_*`.
If posted ≈ 4× carried, the wire is inflated (2); if posted ≈ carried and repair counters are small, the wait is
in the timer (1); if `worst_ready_ms` is large with an empty backlog, it is the application (3).

**Until that runs, the answer to "which direction" is that the question is not yet well-formed**, and I would
rather say so than have the plan record a decision made on a bad premise.

### 4.172 P3 closes §4.120 #6 and the admission half of #7 (2026-11-09)

The two lifecycle items §4.120 could only mark "partial" now have tests that observe the *release* rather than
the request for it. Suite: **383 lib tests**, four of them new.

**#6, the listener's dispatchers.** The seam is small and behavioural: `spawn_dispatcher` returns its
`JoinHandle` and the listener keeps them, so a test can `await` the task instead of trusting the stop channel.
Dropping a handle detaches, which is exactly what the listener did before, so nothing about shutdown changed.

| test | what it observes |
|---|---|
| `a_closed_listener_lets_its_dispatchers_go` | after `close_all`, each dispatcher's task **finishes** within 2 s and its `Arc<UdpSocket>` count drops by one |
| `a_rebound_port_takes_the_traffic_the_closed_one_left` | a closed listener's address can be bound again **and the opening datagram sent to it is admitted by the new listener** — impossible if the old dispatcher still reads that socket |
| `a_full_connection_table_refuses_without_inserting` | at `MAX_CONNECTIONS` the refusal path inserts nothing and the table does not grow |

**Load-bearing, checked by breaking the mechanism** — the discipline that has caught three of this session's
mistakes: making the dispatcher `continue` instead of `return` on its stop signal turns the first two red;
loosening `admit` from `<` to `<=` turns the third red. Restored, all four pass.

**And one false break, worth recording** because it looked convincing: sending `false` on the stop channel
instead of `true` changed nothing, because `tokio::sync::watch::Sender::send` notifies its receivers whether or
not the value differs. A test that passed under that "break" would have told me the test was load-bearing when
it was not — so the second attempt broke the *dispatcher's* handling instead of the value it is handed.

**A doc defect found on the way.** `MhdListener::close_all` claimed to "release its sockets", and it does not:
it takes `&self`, and the sockets go when the listener is dropped. The comment now says so and names the order a
shutting-down server uses (`close_all` then drop), which is also what the rebound-port test exercises. That is a
comment fix, not a behaviour change — making `close_all` clear its own sockets would need `&mut self` and buy
nothing, since no caller keeps the listener afterwards.

**What is still partial.** #7's other half — task and memory bounds under sustained loss — is unchanged:
`the_lost_table_is_capped`, `silence_backs_off_and_then_kills_the_path` and
`an_abandoned_stream_releases_its_buffer_after_the_hold` cover the tables, not a bound on the number of tasks a
hostile or degrading path can make the process hold. §4.172 does not claim it.

### 4.171 P2 lands: the protocol harness is a workload, and its four numbers are in the run file (2026-11-09)

§10.3 leans on "L1/L2 protocol metrics reported independently" and §3.4 asked for them as declarations with
units, denominators, materiality, selfcheck and bilingual docs. Until now the harness existed and printed
NDJSON, but nothing in the model could produce, gate or compare its numbers. That is now wired:

- **`scenario transport`** (`kind transport`, headline `transport_first_byte_p99_us`) runs the harness twice per
  arm — a `reuse` cell and a `fresh` cell — with the listener in the **server** namespace and the dialer in the
  **visitor's**, so the traffic crosses the leg a `--condition` shapes instead of measuring loopback. §12.3's
  sentence had those two names the other way round; the leg logic is what matters and this is it.
- **The control arm is L1 by construction**: it runs `--mode tcp`, i.e. a raw TCP socket with no tool, which is
  what §3.1 calls L1. A tool arm runs `--mode mhd`, which is L2. An arm that crosses no socket (L3) reports the
  four metrics as typed absences.
- **Four declarations**: `transport_open_p99_us` (absent in a reuse cell, not zero), `transport_first_byte_p99_us`,
  `transport_recovery_p99_us` (`max(0, complete_us - first_us)`, so it is not the round trip twice), and
  `transport_stream_error_rate_pct`. Rows on both `benchmarks` pages; `just bench-selfcheck` went from 741 to
  **770** checks, the new ones derived from the declarations rather than written by hand.

**Measured, three rounds of smoke then a four-round A/A calibration:**

| cell | arm | `open` | `first` | `recovery` | error % |
|---|---|---|---|---|---|
| reuse | control (L1, TCP) | — | 25 µs | 15 µs | 0 |
| reuse | mhd (L2) | — | **87 µs** | 2 µs | 0 |
| fresh | control | 48 µs | 34 µs | 17 µs | 0 |
| fresh | mhd | **3 µs** | 70 µs | 4 µs | 0 |

- **A stream open costs 3 µs on mhd against 48 µs for a TCP connect** — the carrier's open is local bookkeeping
  and pays no handshake, which is the shape §4.106's work assumed.
- **A round trip costs 2.5× more on mhd** (79.5 against 31.5 µs on the reuse cell, four rounds, A/A twin exactly
  79.5). This is the same 25 µs task hop §4.134 measured on loopback, now visible from inside the model across a
  veth pair.
- **The floors are the point**: `open` 18.3 %, `first` 22.0 %, `recovery` 41.7 %. The first two can resolve the
  2.5× difference; `recovery` cannot resolve anything at these magnitudes (its values are 2–17 µs, near the
  timer's own grain) and its floor says so rather than inviting a claim.

**What this changes about §4.169/§4.170.** Nothing in their numbers, but they now have an instrument that could
have caught the leg mistake in one run instead of three: `first` on a shaped cell that is not shaped must come
out near the clean value, and that comparison is now available inside a single campaign.

**What it does not do.** It does not put L1/L2 into any release cell — `transport` is not in the `sweep`
profile — and it does not produce a product SLO: `transport` is a diagnostic on the carrier, and §12.3's rule
that these numbers support attribution rather than replace the product verdict stands.

### 4.170 Six rounds make the lossy-path collapse a level, not a lottery (2026-11-09)

§4.169's two-round reading was 0.074–0.174 Gbit/s on a 62 % floor, which could not say whether the collapse was a
plateau or a coin flip. Six measured rounds with the A/A twin and one warm-up, judged on `throughput_gbps` alone,
pre-registered before the run:

| arm | `throughput_gbps` median | min–max | `rtt_p99_ms` |
|---|---|---|---|
| `molehill` (mhd) | **0.2886** | 0.242 – 0.403 | 2 038.0 |
| `molehill~aa` | 0.3332 | 0.200 – 0.376 | 1 896.5 |

Floors: `throughput_gbps` **26.4 %**, `rtt_p99_ms` 92 %. 1 787.7 s for 28 samples; six rounds per arm, **five of
which contributed a value** — one round in each arm's set carried nothing, which is the cell's own
`carried nothing` guard doing its job rather than a silent zero.

**It is a level, and the 0.074 was a bad round.** The two halves of the same binary overlap (0.242–0.403 against
0.200–0.376) and their medians differ by 15.5 %, inside the 26.4 % floor. So the honest statement is a level
with a spread, not a number: **on a carrier path that drops 5 %, mhd carries about 0.29 Gbit/s — between a
fifth and a third of the 1.19 Gbit/s the same cell carries clean.**

**And one round in the log was a near-stall** — `cpu_s_per_gbit` 21 728 at 184 s of stage, i.e. about
0.046 Mbit/s — which is precisely the round that contributed no value. So the distribution has a lower tail
that neither the median nor the min above shows; the level is a central reading and the extreme is real.

**What this settles and what it does not.** It settles the *size* of the lossy-path penalty: roughly a factor of
four on throughput, reproducible to about a quarter, not a coin flip. It does not identify the mechanism, and it
does not make the earlier two-round number wrong — 0.074 was inside the same distribution's tail. The mechanism
question is §4.169's, and it is a loss *response* question: the window is at the 16 MiB cap on that path, so it
is not the ceiling, and the repair path is one range per probe.

> **Mechanism withdrawn — see §4.173.** The repair path is *not* one range per probe on a
> losing path: that rule governs only the branch where nothing is stale. The level stands; the
> explanation does not.

### 4.169 §4.165–§4.167 measured the wrong leg; the counters caught it, and the corrected number is severe (2026-11-09)

**The mistake.** `--condition` defaults to the **visitor** leg — the visitor-to-server hop — while the carrier
crosses the **tunnel** leg, server to client. So every `loss5` run in §4.165 and §4.166 shaped the visitor's
TCP connection and left the carrier's own path clean. Those entries' numbers are real, but they answer
"visitor-side loss with a clean carrier", not "the carrier on a lossy path", and their text says the latter.

**What caught it was a counter, not a re-reading.** The per-lane window line from the attribution run reads

```
cwnd=262144 flight=59356 buffered=65216 ceiling=262144 srtt=1000us
```

`srtt = 1000 µs` is `recovery::GRANULARITY`, the estimator's floor — and the condition names **100 ms of delay
each way**. A 200 ms path cannot report a 1 ms smoothed round trip, so the datagrams that lane sends were not
crossing the shaped leg at all. §4.141's rule ("prove the premise in the same run") applies to the *condition*
as much as to a code change, and this is the third time it has paid.

**The corrected measurement**, `--condition loss5 --condition-leg tunnel`, four bulk streams and one
interactive stream, one warm-up plus two rounds, 1 004.7 s:

| arm | `rtt_p99_ms` | `rtt_worst_1s_ms` | `throughput_gbps` | error % | `retrans_segments` |
|---|---|---|---|---|---|
| `molehill` (mhd) | **744.5** | 1 016.5 | **0.0742** | 0.0 | 31 |
| `molehill~aa` | 1 226.1 | 1 300.5 | **0.1736** | 0.53 | 107 |
| `control` | 1 419.1 | 2 437.4 | 1.1903 | 0.55 | 164 805 |

**This cell cannot be quantified and must not be.** Its own floors are `rtt_p99_ms` **71.8 %** and
`throughput_gbps` **61.9 %**, and the two halves of the same binary read 0.074 and 0.174 Gbit/s — a 2.3× spread
between an arm and itself. What survives the noise is the **order of magnitude**: when the carrier's own path
drops 5 %, mhd carries **one-seventh of what the clean path carries** (0.074–0.174 against 1.19), with 31–107
TCP segments retransmitted in the namespaces and a zero error rate on the interactive probe.

**That is the "no loss response" gap from §4.163, in a measured form.**

> **Withdrawn — see §4.173.** The gap is measured; the mechanism named here is not what the code does:
> `on_timeout` re-sends one range only on the branch where nothing is stale, and repairs every stale range
> otherwise. The explanation is open again. `conn.rs` never cuts the window on
loss; it repairs and continues (§4.163's quote). On a 5 %-loss carrier path that design keeps flying into a
path that drops one datagram in twenty, and throughput collapses. **This is not a window-ceiling problem** —
`srtt` on that path puts the ceiling at the 16 MiB cap, far above any bandwidth-delay product involved — so
P4's entry condition, as written, *still* is not met: the limiter is the loss response, which §12's P4 list
does not cover.

**What should happen next, stated as a choice rather than taken unilaterally.** Either §12 gains a stage for
the loss response (a window that reacts to loss, or a repair rate that does not serialise behind one probe per
timeout), or the lossy-path collapse is recorded as the carrier's accepted behaviour and the deployment advice
says "do not use mhd where the path loses packets". The first is a design change in the engine; the second is a
documentation change. Both are cheap to state and expensive to get wrong, so this waits for the human.

**The measurement to sharpen it first** (pre-registered, before it runs): the same tunnel-leg `loss5` cell with
**six measured rounds and the A/A twin**, judged on `throughput_gbps` alone, whose floor at that count should
fall well below the 2.3× spread — the question is whether mhd's collapse is a plateau or a lottery, and only
more rounds can say.

### 4.168 P1 on a gigabit link: equal throughput, 4.3 % more tail, 57 % more CPU per Gbit (2026-11-09)### 4.168 P1 on a gigabit link: equal throughput, 4.3 % more tail, 57 % more CPU per Gbit (2026-11-09)

The cleanest cell of this session, and the first where the control arm agrees with itself across two files. The
same `cost` single point under the new `rate1000` condition (1 Gbit, 2 ms each way, 4 000-packet queue), run
once on mhd and once with `MOLEHILL_DATA_CARRIER=tcp` — same fingerprint, pin in `molehill_switches`.

| | mhd | TCP carrier | `control` (no tool) |
|---|---|---|---|
| `rtt_p99_ms` | **30.10** | **28.85** | 28.87 / 28.83 |
| `throughput_gbps` | 0.93765 | 0.9377 | 0.93775 / 0.9376 |
| `cpu_s_per_gbit` | **0.990** | **0.632** | — |
| floors in that file | p99 0.74 %, thr 0.016 %, cpu 0.51 % | p99 **0.37 %**, thr 0.016 %, cpu 0.38 % | |

**The control arm is the reason this cell can be read at all.** It reads 28.87 ms in one file and 28.83 in the
other — 0.14 % apart — and 0.93775 against 0.9376 on throughput. Compare `loss5` (§4.166), where the same arm
swung 47 % between two runs. **This cell is stable across files; that one is not**, and the difference is the
condition, not the carrier: a rate limit is a deterministic shaper, and 5 % loss is not.

**Three findings, all resolved:**

1. **Throughput is identical** — 0.9377 Gbit/s for both carriers, 93.8 % of the shaper, and the same as the
   tool-free path. On a gigabit link with four bulk streams and an interactive stream, mhd carries exactly what
   the path carries. **The 256 KiB window ceiling does not cap it**: at a 4 ms round trip the ceiling is 1 MiB,
   twice the bandwidth-delay product.
2. **The interactive tail is 4.3 % worse** — 30.10 against 28.85 ms, i.e. **1.25 ms**, in a cell whose own floor
   is 0.37–0.74 % and whose control is reproducible to 0.14 %. Small in absolute terms, and resolved rather
   than directional: this is the first carrier-vs-carrier difference in the session that survives the
   cross-file stability check.
3. **CPU per carried Gbit is 56.6 % higher** — 0.990 against 0.632 s/Gbit, both files resolving that metric to
   about half a percent. On a fast path the carrier's cost is CPU, not throughput and only marginally latency.

**What this does to P4's entry condition, stated plainly.** §12 requires `flight ≈ cwnd` with
`none_reason = window` dominating before a controller may be written. Here throughput **equals the shaper and
the tool-free control**, so the path is the limiter and the window is not — on this shape, by construction of
the numbers rather than by inspection of a counter. The `loss5` shape says the same about throughput and
something else about the tail. **So the entry condition is not met on either shape, and P4 stays shut.**

**What that leaves for P1.** One measurement, and it is now narrow: in `loss5`, where the tail is bad and the
throughput is fine, *where does the interactive stream wait?* That is a scheduling and queueing question —
`buffered` against `flight`, the `none_reason` distribution, and the read-side wake — not a window question.
§12's rule applies to it unchanged: if `buffered` is large while `flight ≪ cwnd`, the answer is the send
scheduler and the `flush` walk, and a controller would be the wrong instrument.

**One caveat on the 4.3 %.** It is measured at one operating point (four bulk streams, one interactive, 1 Gbit).
The interactive probe's own cadence is 50 ms, so 1.25 ms is a small fraction of what it measures; the cell is
reproducible, but the *size* should not be extrapolated to other loads without measuring them.

### 4.167 P1 on a rate-limited path: the carrier is not the limit, and nothing regresses (2026-11-09)### 4.167 P1 on a rate-limited path: the carrier is not the limit, and nothing regresses (2026-11-09)

> **Leg caveat (§4.169).** These runs shaped the **visitor** leg, the default, while the carrier
> crosses the **tunnel** leg — so the numbers below describe a clean carrier path. The corrected
> tunnel-leg measurement is §4.169, where mhd loses about six-sevenths of its throughput.

The `cost` single point under `rate100` (100 Mbit, 20 ms each way, 2 000-packet queue), four bulk streams and
one interactive stream, one warm-up plus two measured rounds, 693.4 s:

| arm | `rtt_p99_ms` | `rtt_worst_1s_ms` | `throughput_gbps` | `offered_gbps` | `cpu_s_per_gbit` | `retrans_segments` |
|---|---|---|---|---|---|---|
| `molehill` (mhd) | **203.16** | 196.37 | 0.0935 | 0.0947 | 6.05 | **0.0** |
| `molehill~aa` | 200.25 | 194.73 | 0.0935 | 0.0947 | 6.24 | 0.0 |
| `control` | 213.87 | 208.23 | 0.0935 | 0.0944 | — | 0.0 |

Floors: `rtt_p99_ms` **6.6 %**, `throughput_gbps` **0.0 %**, `cpu_s_per_gbit` 3.1 %.

**Three arms, one number.** Throughput 0.0935 Gbit/s for all three — 93.5 % of the shaper — offered 0.0947,
and **zero TCP retransmissions everywhere**, so no queue is being overrun. mhd's interactive p99 is nominally
*lower* than the tool-free path and inside its 6.6 % floor against its own twin.

**That is the opposite of the `loss5` picture and it is the useful contrast**: on a path that is limited by
rate and not by loss, the carrier adds nothing measurable — not to the tail, not to throughput, not to the
queue. The 200 ms p99 here is the shaper's own standing queue, present for the control arm too.

**And it independently confirms §4.163's correction**: `retrans_segments` is 0.0, so the window is not
overflowing a 2 000-packet queue at 100 Mbit even though the *ceiling* is 20× the bandwidth-delay product. The
ceiling is a cap that nothing reaches while the source is feedback-limited by the same path.

**The one number worth a caveat.** `cpu_s_per_gbit` reads 6.05, which looks alarming beside `bulk-n`'s ~0.65 —
but this cell carries 93 Mbit/s, so a Gbit takes eleven seconds of wall clock, and the idle polling of §4.151
dominates the ratio. It is a per-Gbit CPU figure on a cell that cannot carry a Gbit per second; it should not
be compared with a bulk cell's.

### 4.166 P1's second half: against a TCP carrier, `loss5` shows no throughput advantage and a worse tail (2026-11-09)### 4.166 P1's second half: against a TCP carrier, `loss5` shows no throughput advantage and a worse tail (2026-11-09)

> **Leg caveat (§4.169).** These runs shaped the **visitor** leg, the default, while the carrier
> crosses the **tunnel** leg — so the numbers below describe a clean carrier path. The corrected
> tunnel-leg measurement is §4.169, where mhd loses about six-sevenths of its throughput.

The same `cost` single point, the same `loss5` path, the same binary, run twice — once on mhd and once with
`MOLEHILL_DATA_CARRIER=tcp`. **Same fingerprint** (`3864bdd29651a505`), and the pin travels in
`molehill_switches: {"MOLEHILL_DATA_CARRIER": "tcp"}` rather than as an arm, which is §12's rule and
`refuse_carrier_arm`'s design.

| | mhd | TCP carrier | difference |
|---|---|---|---|
| `rtt_p99_ms` | **2 904.5** | **1 878.6** | mhd **+54.6 %** |
| `throughput_gbps` | 1.0974 | 1.1629 | mhd −5.6 % |
| `cpu_s_per_gbit` | **0.757** | **0.513** | mhd **+47.6 %** |
| that file's A/A floor, p99 | 32.7 % | 47.7 % | |
| that file's A/A floor, throughput | 19.1 % | 3.7 % | |
| `control` arm, p99 | 1 867.1 | **2 745.3** | **+47 % between two runs of the same bare path** |

**Three statements this supports, and one it does not.**

1. **The published "3.7× throughput at 5 % loss" is not reproduced in this shape.** mhd's 1.0974 is 5.6 % below
   the TCP carrier's 1.1629, which is **inside mhd's own 19.1 % floor** — indistinguishable, not an advantage
   and not a defeat. mhd's own number is very close to the published row's 1.11 Gbit/s; it is the **TCP side**
   that reads differently here (1.16 against 0.30).
2. **The interactive tail is worse, and the size is above either file's floor** — but the two files are
   **not paired runs**, so this is directional, not a claim. Stated as measured: mhd waits 1.03 s longer at
   p99 than the TCP carrier on the same path.
3. **CPU per carried Gbit is 47.6 % higher for mhd**, and both files resolve that metric to about 2 %, so this
   one is comfortably beyond noise. Under loss5 with concurrent bulk the carrier costs about 1.5× the CPU per
   byte it delivers.
4. **And the shape is noisy across files in a way that limits all of the above.** The `control` arm — no tool
   at all, the same path, the same binary — reads **1 867 ms in one run and 2 745 ms in the other, a 47 %
   swing**. Nothing about a carrier can be resolved more finely than the thing both runs share, so the honest
   precision of every cross-file number above is "tens of percent, and the tail especially".

**The discrepancy is the finding, and it is not explained.** The published row and this run share a nominal
shape (four bulk streams, one interactive stream, 5 % loss) yet disagree about the TCP carrier by nearly 4×.
Either the `timeline` stage and the `cost` stage differ in something I have not identified — the staged one
re-dials its bulk spine after the leg goes quiet, and the `cost` one imposes the condition onto an already
running arm — or one of the two numbers is wrong. Recording it as unexplained is the only honest option;
inventing a mechanism for it is exactly what §4.50 did with units.

**What this means for P4.** The entry condition for a controller was "`flight ≈ cwnd` with `none_reason =
window` dominating". Nothing here provides that evidence, and nothing here provides a throughput deficit for a
controller to fix. What it provides is a **tail** problem under loss with concurrent bulk, which points at
scheduling and queueing rather than at the window. So P4 stays shut until the attribution run in §12.2 is done,
and the next measurement is that attribution rather than a controller.

### 4.165 P1's first half: under `loss5`, mhd's interactive tail is 56 % above the bare path (2026-11-09)### 4.165 P1's first half: under `loss5`, mhd's interactive tail is 56 % above the bare path (2026-11-09)

> **Leg caveat (§4.169).** These runs shaped the **visitor** leg, the default, while the carrier
> crosses the **tunnel** leg — so the numbers below describe a clean carrier path. The corrected
> tunnel-leg measurement is §4.169, where mhd loses about six-sevenths of its throughput.

§12 P1 asks for a `cost` single point (60 s, four bulk streams and one interactive stream at once) under
`loss5`, with mhd against its own A/A twin and a TCP side. The enabler came first: **`--condition` did not
reach a staged single point at all** — `single`'s timeline named `clean` outright, so `--condition loss5` on
`cost` silently measured a clean path, which is the §4.141 failure shape again. `single` now names a
`campaign` sentinel resolved by `model.staged_condition` from the run's own condition, `expected_cells` and the
workload resolve it from the same params, and a smoke run confirms the cell is labelled `loss5`.

**Measured**, three arms, one warm-up plus two measured rounds, 754.2 s:

| arm | `rtt_p99_ms` | `rtt_worst_1s_ms` | `throughput_gbps` | `cpu_s_per_gbit` |
|---|---|---|---|---|
| `molehill` (mhd) | **2 904.5** | 2 908.6 | 1.0974 | 0.757 |
| `molehill~aa` (its twin) | 3 065.2 | 3 272.1 | 0.9289 | 0.775 |
| `control` (no tool) | **1 867.1** | 2 816.9 | 1.1395 | — |

**The run's own noise floor is the only thing that makes this readable**, and it is printed with the table:
p99 **32.7 %** (absolute 1 002 ms), `worst_1s` 24.3 %, throughput **19.1 %**, `cpu_s_per_gbit` 2.3 %.

- **The interactive tail is the finding.** mhd is 55.6 % above the tool-free path on p99, above a 32.7 %
  floor, so this is a real cost and not scatter. A single interactive stream sharing the carrier with four
  bulk streams on a 5 %-loss path waits behind them.
- **Throughput is not.** 1.097 against the control's 1.139 is inside the 19.1 % floor; mhd carries what the
  path carries.
- **The A/A twin is doing its job and is unusually wide here**: 2 904 against 3 065 on p99 (4.1 %), but its
  *scatter* is 32.7 % — the twin's two rounds differ from each other far more than they differ from the arm.
  That is why the entry above quotes the floor rather than the point estimate.

**What this is not yet.** §10.4's claim is about mhd against a **TCP carrier on the same path**, and this run
has only the tool-free control as its reference. The second half — the same shape with the carrier pinned to
TCP — is the run that answers it, and §12's own rule applies: the pin travels in `molehill_switches`, not as
an arm.

**And one thing worth naming honestly.** The shape that produced the original 3.7× claim was the staged
`timeline` with the carrier pinned, not a `cost` single point, so a difference between this and §3's published
row would not by itself refute that row. What this run establishes is narrower and firmer: on a 5 %-loss path
with four bulk streams, the carrier's interactive tail is a third of a second per round trip worse than the
bare path, and its throughput is not.

### 4.164 The plan was extended: five stages, and the controller is conditional (2026-11-09)### 4.164 The plan was extended: five stages, and the controller is conditional (2026-11-09)

After §4.161–§4.163 the human asked whether mhd still had anything to finish, then approved an extension
rather than a new plan. `MHD-FINALIZATION-PLAN.md` §12 is the approved text; this entry is the record of why
it says what it says.

**What the audit found, and what each finding became.**

| finding | became |
|---|---|
| §10.4's "the `loss5` advantage must not be erased" was never verified in one round | **P1**: `cost` + `loss5`, mhd against its A/A twin and a TCP side, `rtt_p99_ms` and `throughput_gbps` pre-registered |
| the rate-shaped cells have never had a bulk-plus-interactive measurement at one operating point | **P1**: the same cell on `rate100`, plus a new `rate1000` for the high-bandwidth case §4.163 could not reach |
| §3.4's four `transport_*` metrics were never registered; §10.3 leans on them | **P2**: the L1/L2 harness wired in, four declarations, calibration, and a topology that keeps the harness on the shaped path instead of loopback |
| §4.120 #6 (listener dispatcher release) and #7 (task/memory bounds) are partial | **P3**: tests that observe the task and the socket actually going away, and bounded tables under sustained loss |
| §4.163's window finding is real in the code and unreachable in the instrument | **P4**, explicitly conditional: no controller until P1 shows `flight ≈ cwnd` with `none_reason = window` dominating |

**Two things the extension deliberately does not do.** It does not reopen the per-lane-socket 25 µs route
(§4.145) or the `READ_POLL` decision (§4.153), and it does not schedule a controller by default — §12.1's
entry condition for P4 is a measurement, and §12.2 says what that measurement has to look like before the
question is even open.

**One correction inside the plan itself.** §12.3 defines `transport_recovery_p99_us` as
`max(0, complete_us − first_us)` rather than the whole round trip, because the harness already reports
`complete_us` and re-naming the same number twice would put two metrics on one measurement — the shape that
made §4.105's coverage claim wrong in the first place.

### 4.162 `jitter` is resolved, and it says the carriers are equal there (2026-11-09)### 4.162 `jitter` is resolved, and it says the carriers are equal there (2026-11-09)

§6.4 names `jitter` alongside `loss1`/`loss5`; it had never been run. `rr-1`, four arms, six rounds,
28 samples, 1 716.9 s:

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| `l4-plain` (mhd) | 40.78 | **57.52** | 24.5 |
| `l4-plain~aa` (its twin) | 40.83 | 57.70 | 24.5 |
| `frp` | 40.84 | 57.99 | 24.5 |
| `rathole` | 40.54 | 57.85 | 24.5 |

**All four agree to within 0.8 % on p99 and exactly on rate**, and — unlike `loss1` and `loss5` — **this cell
resolves**: mhd's own twin differs by 0.3 %, so "no difference" is a finding here rather than noise. On a path
with 20 ms of delay and added jitter the carrier is indistinguishable from both TCP-based tools, and its p99 is
nominally the lowest of the four.

**That makes the picture coherent across conditions**: mhd's deficit is a **clean-path** phenomenon (§4.131:
p99 +24.8 % against rathole on `rr-16[clean]`), it disappears once the path adds jitter (this entry), and on loss
the cells cannot tell the arms apart at all (§4.135 at 1 %, §4.161 at 5 %). So the 25 µs task hop matters where
nothing else dominates, and nowhere else — which is exactly the shape §4.145 left the design in.

**One number worth stating**: `setup_p50_ms` here resolves 27.4 % and would need ~13 rounds for 15 %, so the
*open* latency on a jittery path is still unresolved even though the round trip is not.

### 4.163 The window's ceiling follows the round trip and nothing follows a loss — a real gap (2026-11-09)

Asked whether the carrier still has anything left to improve, the honest answer starts in the code rather than
in the record, and this is the one place where the two disagree about completeness.

**What the code does**, read off `conn.rs` today:

- `window_ceiling()` = `WINDOW_FLOOR (256 KiB) × max(1, min_rtt / 1 ms)`, capped at `MAX_CWND` (16 MiB). **It is
  a function of delay alone** — no measured bandwidth, no loss signal.
- Slow start adds every acknowledged byte to the window; after the first loss it turns additive ("one packet
  per window acknowledged") and **the window is never cut** — the comment says so in as many words: *"a loss is
  repaired by retransmission, not paid for by halving the pipe for a round trip"*.

So the window grows to the ceiling and stays there whatever the path's capacity turns out to be. The arithmetic
on two existing conditions:

| condition | BDP | ceiling | ratio |
|---|---|---|---|
| `rate100` — 100 Mbit, 20 ms delay each way, 2 000-packet queue | ≈ 500 KB | `256 KiB × 40` = **10 MiB** | **20×** |
| `rate20` — 20 Mbit, 40 ms each way | ≈ 200 KB | `256 KiB × 80` = **20 MiB**, capped at 16 MiB | **80×** |

**Twenty times the bandwidth-delay product, with a queue of 2.8 MB and no mechanism that ever reduces it.** The
prediction written here was a permanently full queue — of order 200 ms of standing delay at 100 Mbit — plus
tail-drop loss that is repaired but never interpreted as a signal.

**Measured, and the prediction is wrong.** `bulk-1` + `rate100`, six rounds, three arms (mhd, its A/A twin,
rathole), 179.9 s:

| arm | `throughput_gbps` | `offered_gbps` | `retrans_segments` |
|---|---|---|---|
| `l4-plain` (mhd) | 0.0955 | 0.0883 | **0.0** |
| `l4-plain~aa` | 0.0955 | 0.0884 | 0.0 |
| `rathole` | 0.0955 | 0.0854 | 0.0 |

All three carry **95.5 Mbit/s of a 100 Mbit limit, identically, with zero retransmissions from mhd** — so the
window is not overflowing the queue at all. **And the reason is structural**: the ceiling is a *cap*, not a
target, and a window only grows as fast as the sender has bytes to acknowledge. `bulk-1`'s source is itself
feedback-limited by the same shaped path, so its window stops at the bandwidth-delay product (≈470 KB at
95 Mbit and 40 ms) and never approaches the 10 MiB ceiling. **A single TCP source cannot offer more than the
path carries, so this instrument cannot expose an oversized window.**

The mirror case — 10 Gbit/s at 1 ms RTT against a 256 KiB ceiling, i.e. a fifth of the path — is still
untested, and it would need a path whose capacity is *higher* than any condition here provides.

**So the gap is narrower than this entry first claimed and its severity is unknown.** An oversized window
becomes reachable only when the offered load exceeds the path (several concurrent flows on one shaped link, or
a UDP source), and no cell does that today: `bulk-pair` gives two services their own streams but not one
saturated shared link, and `udp-blast` measures loss, not queueing delay. Recording the correction rather than
the prediction is the point: the arithmetic was right and the reachability was not.

**Why this matters for the next decision.** It is the one open item that can move a *product* number on a real
deployment rather than on loopback, and it is also the one the model is thinnest on: there is no cell that
carries bulk through a rate limit while an interactive probe runs. `timeline` is the closest — its interactive
series runs alongside the bulk stages — at 358 s per arm-round at `--scale 0.05`, so two arms and three rounds
is about 36 minutes. **That is the decisive measurement and it has not been made**: `bulk-1` alone would show
carried throughput but not the standing delay, which is the symptom.

**What is not a gap, for the same reason.** The `mhd` stream's own read path (1.8 µs), the ack path (§4.154),
recovery (§4.126/§4.155) and the pool's structure were each measured and found sound; the deficit on a clean
path is the userspace task hop (§4.134/§4.145), decided. So the remaining risk is concentrated in **flow
control against a capacity limit**, not in the datapath.

### 4.161 Auditing the plan for performance items I had not closed: two found, one measured (2026-11-09)### 4.161 Auditing the plan for performance items I had not closed: two found, one measured (2026-11-09)

The goal was marked complete and then a direct question asked whether the plan was really all handled. It was
not: re-reading the original against the record found **three stale statements and two unclosed performance
items**, all in the performance half.

**The stale statements, now fixed.** §0's row for phase 6 still said "so this one has to wait for an
`l4-plain` baseline record" — that baseline exists (15.840 Gbit/s, §4.143) and the judgement is done (96.8 %,
§4.146) — *and* it repeated §4.157's wrong default ("`molehill` inherits `noise_data=True`, not the product
default"), which is backwards. The two closing paragraphs of §0 still said "the human has decided: attack the
25 µs" (§4.145 closed it as declined) and "whether 2.2 % per lane is acceptable must be decided by a human"
(§4.153 decided: keep 1 ms). **I had updated the table row and left the prose that said the opposite**, which
is the same failure as §4.157 in a different form: fixing the copy I was looking at instead of every copy.

**Unclosed performance item 1: §6.4's "measure `loss1`/`loss5`/`jitter` in the same round".** Only `loss1`
had been run. `loss5` is now measured on `rr-1`, four arms, six rounds, 28 samples, 1 732 s:

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| `l4-plain` (mhd) | 200.64 | 865.11 | 3.95 |
| `l4-plain~aa` (its twin) | 200.65 | **794.82** | 4.05 |
| `frp` | 200.79 | 907.64 | 3.90 |
| `rathole` | 200.61 | 810.65 | 4.10 |

**The arms are indistinguishable, and the proof is the twin**: mhd differs from itself by 8.8 % on p99, which
is wider than the whole spread across arms (795–908). The model says why — this cell's `setup_p50_ms` needs
**1 075 rounds** to resolve 15 %, and its `rtt_samples` resolve 6.8 % against a 0 % floor. So `rr-1`+loss5
**cannot** answer the carrier question, exactly as `rr-1`+loss1 could not (§4.135). What it does answer:
nothing regressed beyond noise.

**Unclosed performance item 2, and this one is a real gap: §10.4's "the existing `loss5` latency *and*
throughput advantage must not be erased".** That advantage (3.7× throughput, 31 % tighter tail) was measured
on the **`timeline`** scenario with `MOLEHILL_DATA_CARRIER` pinning each carrier — a different cell from
anything run in this session. At `--scale 0.05` it is 358 s per arm-round, so two carrier pins × four rounds is
about **48 minutes**, and it has **not been run**. Until it is, the claim that this session's changes (the
drain loop, the one-range probe, the RACK revert) left the loss5 advantage intact **rests on bulk-n and loss1
only**.

**A third item, measurement-side: §3.4's protocol metrics were never registered.** §3.4 asks for
`transport_open_p99_us`, `transport_first_byte_p99_us`, `transport_recovery_p99_us` and
`transport_stream_error_rate_pct` with unit, direction, denominator, applicable workloads, materiality,
selfcheck and bilingual docs. The model holds **37 metrics and none of them is a `transport_*` one**, and no
scenario invokes the harness. The numbers *are* reported, by the harness's own NDJSON, so §10.3's "report L1/L2
independently" is satisfied in substance — but not in the form §3.4 specified, and they cannot appear in a run
file or the gate. `HANDOFF.md` §0 line "protocol metrics and the runner workload are not wired yet" has been
true since the plan was approved.

**So the honest answer to "is it all handled": no.** Two items remain and one is a real verification gap:
`loss5` on the `timeline` cell (~48 min), `jitter` (§6.4 names it, never run), and the protocol metrics'
registration (§3.4). The first two are measurements; the third is a small feature.

### 4.160 The loop's work is finished, and here is exactly what that means (2026-11-08)### 4.160 The loop's work is finished, and here is exactly what that means (2026-11-08)

Every section of the plan now carries a status, and no row of its progress table says "not started" — the
last one, phase 4, was a *standing process* rather than a phase and now says so with the four times this
session used it. So this is the point to say what is finished and what is not, rather than to keep finding one
more thing to measure.

**Of the six acceptance criteria, four are met, one is met at an indication, one was declined:**

| criterion | state |
|---|---|
| `bulk-n` ≥ 13.35 Gbit/s | **met** — 15.339, 96.8 % of its recorded baseline (§4.146) |
| `capacity` 8/8 | **met** — `capacity_streams` 8, `slo_broken` 0 at every level (§4.129) |
| gate green | **met** — `just check`, `interop` 3/3, `l3-accept` 16 PASS, both carriers 430/0 (§4.149), under a stated repeat policy (§4.158) |
| no resource leak | **met** — `drift_rss` 3.29 MiB/min against a floor of 50, no wedge (§4.150) |
| `timeline[clean]` p99 ≤ 50 ms | **indication only** — 24.87 / 14.34 ms with zero errors at `--scale 0.05`; the formal judgement is the release sweep at `--scale 1.0`, 15.9 hours (§4.143) |
| no worse than TCP and peers | **declined** — unmet on the clean loopback cell, and the change that would fix it was refused as too much configuration surface (§4.145) |

**What "finished" therefore means**: the plan is executed as far as this branch's authority allows, and the two
items that remain are outside it — one by the plan's own design (a release-time measurement, and the loop is
told not to run release sweeps) and one by the human's decision. Running that ritual is a new instruction, not
an unfinished task, and it needs the "no release-oriented sweep" constraint lifted to happen here.

**What a future round would need to be worth starting**: a new instruction, or a defect. There is no third
kind — the measurement queue is empty, the decision queue is empty, and the record says so in three places
(`§0`, §5 and the session objective).

### 4.159 The next release is a re-baseline release, and it is recorded before the ritual (2026-11-08)### 4.159 The next release is a re-baseline release, and it is recorded before the ritual (2026-11-08)

Checking whether the release path is healthy turned up one fact worth having early, and one false alarm of my
own.

**The fact.** The benchmark's engine hash changed in this session, and the metric registry did not:

| | `engine_hash` | `metrics_hash` |
|---|---|---|
| `benches/records/results-bench-v0.11.2.json` | `deec5d29798c5a68` | `e240acbf6d8515a0` |
| current runs (`bulk-n`, `soak`) | **`031034b4b187aeae`** | `e240acbf6d8515a0` |

The engine covers the method modules, adapters and probes — `model.py` gained `l4-plain` and `Arm.noise_data`
— so `docs/release.md`'s rule applies: **"a method-changing release is a re-baseline release"**. The gate
runs without `--baseline`, and the record it produces becomes the next baseline. That is now written into
`CHANGELOG.md`'s `[Unreleased]` under *Changed*, where the rule asks for it, instead of being discovered on
the day. The metric meanings, directions and floors did not move — only the instrument.

**The false alarm.** The changelog also claims, from an earlier session, that "mhd wins on a lossy path and
TCP wins on a clean one, and the crossover is between 1 % and 5 % loss". §4.135 measured **1 %** and found all
four arms tied at p99 ≈ 246 ms — which is *consistent* with a crossover between 1 % and 5 %, so the claim
stands and I did not touch it. Then I checked its link and reported the anchor dead, because I grepped for
`^## ` and the section is a `###`: `### Where mhd wins, and where it loses (2026-11-07)` at line 1025 slugs
to exactly what the changelog links to. **Two minutes, one correct claim and one wrong accusation** — the
same lesson in miniature, applied to a link instead of a number.

### 4.158 "Green" now means a stated repeat policy, not one lucky run (2026-11-08)### 4.158 "Green" now means a stated repeat policy, not one lucky run (2026-11-08)

The acceptance asks for both integration suites green, and this session's own record says that was not enough
to mean anything: before §4.114 the striped cell failed about one run in four, and §4.107 quoted a day of
6/6 followed by a day of 13/24. A single pass therefore proved nothing, which is why the criterion needed a
repeat policy rather than a tick.

**Measured now**, on the current tree:

| what | runs | result |
|---|---|---|
| `striped_data_channels` alone | 20 | **20 passed, 0 failed** |
| `integration_test` whole suite (22 cases) | 3 | **22 passed each**, 90.3 s / 91.0 s / 92.6 s |

So the policy this record adopts: **the integration suite is called green when a full run passes three times
in a row and the once-flaky cell passes twenty times in a row.** Twenty is not a statistical proof of
zero rate — at a 1-in-20 rate the chance of seeing none in twenty is 36 % — but it is the difference between
"it passed" and "it passes", and it is what the earlier measurements of the same cell never had.

**The stability shows in the durations too.** The full suite used to take 40–70 s per failing run plus the
stall; it now finishes in 90–93 s consistently, which is the same wall-clock every time — a stall would show
as one run an order of magnitude slower, and none did.

### 4.157 §4.105 was wrong about which default the bench was measuring (2026-11-08)### 4.157 §4.105 was wrong about which default the bench was measuring (2026-11-08)

Following a user-facing claim to its source turned up a mistake of mine, ten rounds old and published on both
`benchmarks` pages.

**The claim.** §4.105 said "the product's default is `false`" about `data_enable`, concluded that every L4 arm
— all of which write `data_enable = true` — had never measured the plane a default deployment runs, and put
that sentence on the benchmarks pages as a limitation.

**What the code says**, and I should have checked it then:

| where | default |
|---|---|
| `[client.noise].data_enable`, unset | **`true`** — `src/core/client.rs:322`, `unwrap_or(true)` |
| `[transparent.noise].data_enable` | **`false`** — `src/config/transparent.rs:54` |

So the default is **plain for a transparent claim and encrypted for a forwarding service**. The L4 arms'
`data_enable = true` **is** the L4 default; the benches were measuring exactly the plane an L4 deployment
runs. The `false` I quoted belongs to the L3 claim's config, and the migration note it came from
(`parsing.rs:961`) is a `transparent.transport` entry.

**What that changes.** Not one number: every measurement stands, including `l4-plain`'s, which is still one
knob apart from `l4` and still prices the encryption and its opening shape. What changes is the **claim about
coverage**, and it was the wrong way round — the pages told a reader that their L4 tables described a
non-default plane, when those tables *are* the default plane and it is the transparent claim whose plane was
never measured here. `docs/transport.md` said "`true` (the client default)" all along, and it was right; I
took the other reading from a migration note without opening the resolver.

**Fixed in three places**, because all three carried it: the English bullet, its Chinese mirror, and the
`l4-plain` comment in `bench/bench/model.py`, which called it "the data plane a default deployment runs". Each
now states which plane is whose default and cites the resolver.

**This is the third time this session that a figure or fact taken from my own notes, rather than from the code
or a fresh measurement, was wrong** — §4.133's 45 µs against a stale 64–80, §4.141's two unverified
experiments, and now this. The rule that keeps emerging is the one already written down: check the artifact,
not the record of it.

### 4.156 The carrier's recovery policy now has a home in internals.md (2026-11-08)### 4.156 The carrier's recovery policy now has a home in internals.md (2026-11-08)

`docs/internals.md` was not *wrong* about the carrier — it says an `mhd` lane is a UDP connection with its
own packet numbers, acknowledgements and streams — but it was **silent** on how that recovery actually
behaves, and five of this session's changes live there. A contributor reading the page learned the shape and
nothing about the policy.

One paragraph now states it where that audience looks: an acknowledgement is scheduled by every data-carrying
packet and every credit change, with no delayed-ack timer and no packet number on an ack-only packet; loss
comes from the three-packet threshold or a reorder window from the peer's frontier; a probe re-sends **one**
oldest range rather than the flight; the pump wakes at most a millisecond apart while a stream is live —
2.6 ms worst fresh-stream open against 21.5, for about 2 % of a core per idle lane — and sleeps on a timer
when every stream has ended; and stream ids are monotonic in both directions, the minter skipping an id a
live stream holds and a late packet for a retired stream dropped with its window returned.

**Why there and not here.** AGENTS.md's "one topic, one home" puts the design and its trade-offs on the
contributor page and the measurement record on this one. The numbers travel with the policy because a
contributor choosing a poll interval needs them; the experiments behind them stay in §4.101 and §4.152.

**What it does not say.** No history — not that RACK was tried and reverted, not that the page was silent
before. A user-facing or contributor page states what is true now; the story belongs here, and this is it.

### 4.155 §6.4's verification list, audited: nine of ten covered, one gap filled (2026-11-08)### 4.155 §6.4's verification list, audited: nine of ten covered, one gap filled (2026-11-08)

The plan's §6.4 names ten shapes that the fake-clock and in-memory-link tests must cover. Nobody had checked
them one by one; this does, and each maps to a test that exists:

| §6.4 shape | the test |
|---|---|
| first packet lost alone | `a_lost_opening_packet_is_re_sent_within_the_cheap_timer`, `a_re_sent_opening_takes_the_cheap_timer_on_a_busy_lane` |
| **last packet lost alone** | **was missing** — added below |
| the three-packet threshold | `three_later_acks_declare_a_loss` |
| a loss declared by time alone | `the_reordering_window_declares_a_loss_without_later_acks` |
| reordering | the same test: the window is what keeps a reordered packet from being called lost |
| a late acknowledgement | `an_acknowledgement_after_the_loss_still_retires_the_record`, `a_late_acknowledgement_still_frees_the_send_buffer` |
| consecutive probe timeouts | `ten_silent_retries_need_five_seconds_of_silence`, `silence_backs_off_and_then_kills_the_path` |
| a single-packet probe | `a_small_flight_retransmits_in_twenty_milliseconds`, `a_probe_re_sends_one_range_not_the_flight` |
| an opening packet inside a wide bulk flight | `a_re_sent_opening_takes_the_cheap_timer_on_a_busy_lane` |
| one-way credit recovery | `credit_goes_out_from_an_end_that_has_nothing_else_to_send` |
| no-acknowledgement memory bound | `the_lost_table_is_capped`, `a_lost_record_is_forgotten_after_its_hold` |

**The gap was the tail**, and it is the one shape where nothing but the deadline can work: the last packet of
a flight has no later packet to reach the three-packet threshold, and when it also ends the data there is
nothing behind it to acknowledge. `the_last_packet_of_a_flight_is_recovered_by_the_timer_alone` sends four,
acknowledges the first three, and shows the fourth is declared lost by the timer and put back on the wire.
Suite green at 380.

**What this audit does not claim.** A test existing is not the same as a behaviour being right — that is what
the measurements in §4.124–§4.127 were for. This says the plan's list is covered, not that the carrier is
correct beyond what those tests assert.

### 4.154 §6.3's acknowledgement policy, stated and pinned (2026-11-08)### 4.154 §6.3's acknowledgement policy, stated and pinned (2026-11-08)

The plan's §6.3 was the one item never touched: "state which packets must trigger an acknowledgement
immediately, and whether holes and credit updates reach the peer promptly". The code's answer, now written
down and pinned:

- **Every packet that carried stream data makes an acknowledgement pending** (`apply_datagram`'s stream arm),
  and so does **every credit change** (`credit_due`, §4.101's one-way-flow fix). There is **no delayed-ack
  timer**: the block rides the next packet this end builds, and if that packet also carries credit or data,
  the acknowledgement rides along.
- **A packet that carries only an acknowledgement takes no packet number** (`FLAG_UNNUMBERED`), so it can
  never leave a hole in the peer's number frontier — the clause §6.3 asks to keep.
- **The bitmap is not widened.** §6.3 says only a measurement showing the single-stream byte prefix limits
  recovery would justify it; §4.108's prefix retirement and §4.142's numbers never showed that, so it stands
  at 64.

`a_packet_that_carried_data_is_acknowledged_in_a_packet_of_its_own` pins the first clause: a fresh end's
first packet is its **credit**, so the test takes the first packet that actually carries stream data, delivers
only that, and asserts the receiver answers with a packet carrying an acknowledgement, unnumbered. Disabling
the block's attachment fails it at "a packet that carried data must be acknowledged"; suite green at 379.

**Two wrong assumptions in my own test, and they are the interesting part.** The first version delivered the
*sender's first* packet and expected an ack — but that packet is the sender's credit, carrying no data, so
nothing was owed. The second expected the acknowledgement to travel alone — but it rides the receiver's credit
packet, which is both correct and cheaper. Both were me assuming a state instead of checking it, which is the
same failure §4.141 and §4.151 recorded; the fix is the same, and this time it was two rounds of it inside one
test.

### 4.153 The poll interval stays at 1 ms, decided with the curve in hand (2026-11-08)### 4.153 The poll interval stays at 1 ms, decided with the curve in hand (2026-11-08)

§4.152 put three points on the table and the human's answer was to **keep 1 ms**, the shipped value. No code
change: the constant was restored before the measurement and `git diff` is empty, so this entry records a
decision rather than a change.

**What the decision was made against**, which is the point of having measured the curve:

| `READ_POLL` | idle CPU per lane | worst fresh-stream open |
|---|---|---|
| **1 ms — kept** | **2.2 % of a core** | **2.6 ms** |
| 5 ms | 0.9 % | 6.5 ms |
| 20 ms | 0.2 % | 21.5 ms |

The 20 ms row is the state before §4.101; choosing 1 ms means paying 2.2 % of a core per idle live lane — about
9 % for a pool of four — to hold the worst fresh-stream open at 2.6 ms instead of 21.5. A 5 ms middle point was
available at a third of the CPU and was not taken, so the latency precedence in the plan's constraints
(延迟 > 吞吐 > CPU) is what the decision followed, stated in numbers rather than in principle.

**And one thing the curve changed about how this is judged.** p99 reads 78–101 µs at all three points, flat
inside noise; the trade lives entirely in the max, where the 21.5 ms event is rarer than one request in a
thousand. So a p99 SLO cannot express this trade, and an acceptance written only in p99 would have let the
20 ms setting look identical to the 1 ms one. If a future round wants a guard on this, it has to be a max or a
"worst N seconds" metric, not a percentile.

**That closes the last open product call in the objective.** What remains outside this loop's authority is
`timeline[clean]` at `--scale 1.0` — the release sweep's 15.9-hour measurement (§4.143) — and the peers
criterion, closed as declined (§4.145).

### 4.152 The poll-interval trade is a curve with a knee, not a binary (2026-11-08)### 4.152 The poll-interval trade is a curve with a knee, not a binary (2026-11-08)

§4.151 left a product decision with two ends: 2.2 % of a core for a 1 ms poll, or the 21 ms tail §4.101
fixed. Three points make it a curve, and the middle one matters.

| `READ_POLL` | idle CPU per lane (server / client) | fresh-stream `complete_us` **max** | p99 | p50 |
|---|---|---|---|---|
| **1 ms** (shipped) | **2.20 % / 2.20 %** | **2.6 ms** | 101 µs | 49 µs |
| 5 ms | 0.80 % / 1.00 % | 6.5 ms | 78 µs | 52 µs |
| 20 ms | **0.20 % / 0.20 %** | **21.5 ms** | 87 µs | 49 µs |

Five thousand fresh streams per point, so every request opens a stream — the shape §4.101's tail appeared in.
CPU scales as one over the interval (2.2 → 0.9 → 0.2 %), which is what a wake rate should do.

**And the trade shows in the max, not in p99.** p99 reads 101 / 78 / 87 µs across the three — flat inside this
sample size's noise — while the max moves 2.6 → 6.5 → **21.5 ms**, and 21.5 ms at a 20 ms poll is §4.101's
tail reproduced to the millisecond. That is worth stating plainly, because it means **a p99 SLO would not
see this trade at all**: the 21 ms event is rarer than one in a thousand requests, which is exactly why §4.101
found it by looking at a tail and not at a percentile.

**So the decision has three points, not two:**

- 1 ms: 2.2 % of a core per idle lane, worst fresh-stream open 2.6 ms.
- 5 ms: 0.9 %, worst 6.5 ms — a third of the CPU for a tail that is still under a millisecond-scaled budget.
- 20 ms: 0.2 %, worst 21.5 ms — the state before §4.101.

The constant is back at **1 ms**, the shipped value, with `git diff` empty: this round measured the curve and
changed nothing.

### 4.151 An idle lane with a live stream costs 2.2 % of a core — §4.147 measured the wrong state (2026-11-08)### 4.151 An idle lane with a live stream costs 2.2 % of a core — §4.147 measured the wrong state (2026-11-08)

§4.147 reported that an idle live connection costs 0.00 % of a core. **It measured an idle connection with no
live streams**, and the difference is two orders of magnitude.

**How it was caught.** The clamp that §4.101 added — a pump with live streams may not sleep past 1 ms — has a
precondition, `has_live_streams()`, and §4.147 never checked it. It is on the `mhd window` line now
(`live=`), and during that hold it reads:

```
cycle=n=1 packets=1     live=false
```

Both ends, every line. The requests finish their streams, so a plain hold is an idle *connection* with
nothing in it: the pump sleeps on the keepalive timer, runs one or three passes a second, and spends nothing.
That is a real measurement of a real state, and it is not the state the open item asked about.

**The harness now opens a stream for the hold** (`stream_open: true` in the NDJSON), and the same
measurement — `/proc/<pid>/stat` over five seconds, four runs — reads:

| end | run 1 | run 2 | run 3 | run 4 |
|---|---|---|---|---|
| server | **2.20 %** of a core | 2.20 % | 2.20 % | — |
| client | 1.60 % | 2.00 % | 2.20 % | 2.20 % |

**Two percent of a core, per idle lane, on both ends.** At the default pool of four that is ~9 % of a core
per connected client doing nothing, and it is exactly what §4.101 bought: the 21 ms tail it fixed was a
reader waiting for a pump that had gone to sleep, and the fix was to stop it sleeping past a millisecond.
The trade was never priced until now.

**What that does not decide.** Whether ~2 % per idle lane is acceptable is a product call, and it now has a
number to be made against instead of an assumption. The alternative — wake on the timer only, and accept the
tail — was measured as a 21 ms p99 tail on a fresh stream (§4.101), so the trade is 2 % CPU against 21 ms.

**And the discipline held this time.** §4.141 says a change whose effect is measured must first be shown to
take effect; the precondition set that standard for a *measurement* too, and it caught a state I had assumed
rather than checked.

### 4.150 The soak: no wedge, and a drift fifteen times below the project's own floor (2026-11-08)### 4.150 The soak: no wedge, and a drift fifteen times below the project's own floor (2026-11-08)

The last resource property never measured: what a long run does to memory, file descriptors and threads.
`--profile soak --arms l4-plain`, one round, 952 s (15.9 min), the staged timeline walking clean, loss1,
rtt100, loss5 and clean again.

| metric | value | the project's floor |
|---|---|---|
| `drift_rss_mib_per_min` | **+3.2949** | 50 MiB/min |
| `drift_fds_per_min` | **+0.1848** | — |
| `drift_threads_per_min` | **−0.0079** | — |
| `wedge_count` (silences over 10 s) | **0** | 1 |
| `wedge_max_s` (longest interactive silence) | 5.005 | 1.0 |
| `rss_peak_mib` / `fds_peak` / `threads_peak` | 177–233 / 56–60 / 20–22 | — |

**Nothing leaks at the standard this project sets.** The RSS slope is a least-squares fit over the whole run,
not a first-to-last difference, so a warm-up ramp would still show — and it reads 3.29 MiB/min against a
materiality of 50. Descriptors grow 0.18 a minute (about eleven an hour) and threads are flat. **No silence
crossed ten seconds**, which is what `wedge_count` counts; the longest was 5.0 s, on a shaped stage.

**Per stage**, and consistent with everything before it: clean p99 **7.05 ms** and clean#2 **6.27 ms** with
zero errors; rtt100 p99 401 ms, i.e. the 200 ms shaper each way; loss1 p99 1 065 ms and loss5 2 090 ms with
a 0.33 % error rate — the shaped cells are the shaper plus the carrier's retransmission, which is what §4.135
found when four tools tied on the same condition.

**What this is not.** One arm, no A/A twin, so the model produced no verdict — "no claim cleared the floors"
with zero comparisons. These are absolute readings against published floors, which is the only thing a
single-arm soak can say, and the drift number is context rather than a claim.

### 4.149 The acceptance gates, run rather than assumed (2026-11-08)### 4.149 The acceptance gates, run rather than assumed (2026-11-08)

The criteria name four gates that had never been run in this session — `just check`, `just interop`,
`just l3-accept` and both carriers' integrations. All four are green:

| gate | result |
|---|---|
| `just check` (the whole chain: fmt, clippy ×2, machete, audit, deny, outdated, docs, bench self-check, tests) | **exit 0**, "pre-push checks passed" |
| `just interop` | **3 passed, 0 failed** — a new server refuses an old client and keeps serving; an old server refuses a new client and says so; an old server rejects an unknown dialect and survives |
| `just l3-accept` | **16 PASS**, none failed — including 200 KB in 65 KB chunks with no MTU black hole, 2 000 small round trips byte-exact, 16 concurrent flows byte-exact, the policy refusal before the device is touched, and a lane lent between claims byte-exactly |
| integrations, default carrier (= mhd) | **430 passed, 0 failed** across 13 test binaries |
| integrations, `MOLEHILL_DATA_CARRIER=tcp` | **430 passed, 0 failed**, same count |

**The two carriers agree on the count**, which is the shape the criterion asks for: the suite exercises the
same behaviour whichever carrier the data plane would use, and the TCP pin does not quietly skip the carrier
paths (it disables carrier *moves*, not the data channels).

**What is left unmet, and it is not a gate.** `"no worse than same-round TCP and peers"` — closed as
declined (§4.145), not pending. And `timeline[clean]` at `--scale 1.0` is the release sweep's 15.9-hour
measurement (§4.143), with the scale-0.05 reading (p99 24.87 / 14.34 ms, zero errors) standing as the
indication until the ritual runs.

### 4.148 The closing record: three user-visible fixes and one refused comparison, written down (2026-11-08)### 4.148 The closing record: three user-visible fixes and one refused comparison, written down (2026-11-08)

The changelog and the user-facing benchmark page had fallen behind the work, and both are pages a reader
takes at face value.

**`CHANGELOG.md`'s `[Unreleased]` had two `### Fixed` headings** — a defect in its own right, since the
release ritual extracts a section by its first heading — and none of this round of fixes. Now one heading,
with three entries a user would recognise: a carrier that stopped delivering one direction no longer stalls
a transfer (§4.114, and the 10-of-10 / 2.96–3.90 s measurement is quoted); a packet arriving after its stream
ended no longer opens a new one (§4.121); a new stream can no longer be given an id a live stream holds
(§4.122).

**They are late, and the rule says they should not be.** AGENTS.md asks for the entry in the same commit as
the change; these landed 2–14 commits afterwards. Recording it here rather than pretending the discipline
held: the reason is that each fix was made inside a measurement round and the changelog was never opened —
which is exactly the failure mode the rule exists for.

**`docs/benchmarks.md` and its mirror gained the one bullet a reader most needs.** The list of what the model
cannot answer did not say that a TCP-based tool wins on this host: on a clean loopback cell mhd's round trip
is 45 µs against 20, and a proxied request 113 against rathole's 61, because TCP's loopback path is kernel to
kernel. The bullet says so in both languages, states that the change which would remove it was declined, and
says plainly that the carrier's case — per-stream recovery, no head-of-line blocking, datagram semantics — is
not measured by any scenario here.

### 4.147 An idle live connection costs 0.00 % of a core — and the 1 ms clamp is not what I thought (2026-11-08)### 4.147 An idle live connection costs 0.00 % of a core — and the 1 ms clamp is not what I thought (2026-11-08)

The open item since §4.101: a lane with a live stream wakes at `READ_POLL` (1 ms), so the design allows up to
a thousand wake-ups a second per lane, and nobody had measured what that costs. The harness could not hold a
connection open, so it gained `--hold-ms` — the client keeps its lane alive and idle after its requests —
and both ends' CPU is sampled from `/proc/<pid>/stat` over five seconds.

| transport | end | CPU over 5 s |
|---|---|---|
| tcp | server / client | **0.000 cpu-s / 0.000 cpu-s** |
| mhd | server / client | **0.000 cpu-s / 0.000 cpu-s** (two runs) |

**The idle cost is nil, at this instrument's resolution.** Whatever the clamp allows, an idle lane does not
spend measurable CPU. The item is closed as measured rather than as assumed.

**But the shape is not what the clamp implies, and that part is unexplained.** With `MOLEHILL_MHD_STATS=1`
the pump's own line during the hold reads:

```
cycle=n=3 packets=1
```

**Three passes a second, not a thousand.** `has_live_streams()` is true here (the held stream is not reset),
and `wait()` caps the deadline at `now + READ_POLL` when nothing else is armed — so the pump should be
waking about a thousand times a second and the line should say so. It does not, and I did not find out why
inside this round. Recorded as an observation with its evidence and **not** as a finding: either the clamp
is not in the path I think it is, or `cycle` counts something narrower than passes.

**What that means for the item.** The cost question is answered — nil — and the mechanism question is a new
one, smaller: why does an idle lane with a live stream run three passes a second when the code says it may
run a thousand? It is not a performance problem in either direction, which is why it is written down rather
than chased this round.

### 4.146 `bulk-n` passes the 85 % line at 96.8 % of its baseline (2026-11-08)### 4.146 `bulk-n` passes the 85 % line at 96.8 % of its baseline (2026-11-08)

The first phase-5 measurement, against the baseline §4.143 recorded rather than against a neighbour:

| arm | `throughput_gbps` | of baseline | vs 13.35 | `cpu_cores` | `rss_peak_mib` |
|---|---|---|---|---|---|
| `l4-plain` (HEAD) | **15.339** | **96.8 %** | **PASS** | 10.43 | 155.65 |
| `l4-plain~aa` | 15.679 | 99.0 % | PASS | 10.57 | 153.80 |

Six rounds, 14 samples, 110.7 s. **Passes with margin**: 96.8 % against a floor of 85 %, and the A/A twin
reads 99.0 %, so the 2.2 % below baseline is inside this cell's own spread. `cpu_cores` and `rss_peak_mib`
are indistinguishable from the twin.

**What this covers.** Everything since the baseline was taken — the RACK revert (§4.125), the one-range PTO
(§4.126) and the six instruments added while chasing the 25 µs (§4.132–§4.142) — costs nothing measurable on
bulk. That is the regression guard the plan's §9 asks for, and it is green.

**What it does not cover.** The criterion is "not below 85 % of a valid baseline"; it is met on this host,
this profile and this arm. A release record is still not what this is, and §4.143 says why.

### 4.145 The 25 µs stops here: one port per lane is too much configuration surface (2026-11-08)### 4.145 The 25 µs stops here: one port per lane is too much configuration surface (2026-11-08)

§4.144 priced the only path that removes the extra task hop — one UDP socket per lane — and the human's
answer was that it is too large a change to the configuration surface. **So it is not done**, and this entry
records the decision rather than leaving the thread dangling.

**What that settles.** The carrier keeps one server port and one demultiplexing dispatcher, and therefore
keeps the extra task hop on the receive path. The consequence is arithmetic that is now fixed rather than
open: a bare loopback round trip stays around 45 µs against 20 for a TCP socket, and a proxied request stays
around 113 µs against rathole's 61. **The acceptance line "p50/p90/p99 no worse than same-round TCP and
peers" is not met on the clean loopback cells, and cannot be met without that change** — which the product
has declined.

**What is not implicated.** Nothing in the attribution work changes: the carrier's own read path is 1.8 µs,
apply 1.49, lock 0.21, copy 0.09, wake 5.0, consumer entry 2.9 — all direct meters — and no measurement here
found a defect in the carrier's code. The gap is the cost of leaving the kernel, which is what the design is:
a userspace datagram carrier against a kernel-to-kernel TCP path on the same host.

**Where the carrier can still win, stated so the record is not one-sided.** Every one of those numbers is a
*loopback* number, where TCP pays nothing for arriving in the kernel. On a real path the kernel advantage
disappears and per-stream recovery, no head-of-line blocking across the pool's streams, and the option of
`datagram` semantics are what the design buys. **No cell in the model measures any of those** (§4.125: the
lossy cell ties because the shaper dominates; the reorder-window cell does not exist). So the carrier's case
rests on an argument the bench cannot currently test, and that is the honest position to leave it in.

**Consequence for the plan.** The "no worse than peers" criterion stays **unmet and closed as declined**,
rather than unmet and pending. The remaining phase-5 work — throughput and resources, `bulk-n` against
13.35 Gbit/s, the capacity ramp — is unaffected and is where the remaining measurable value is.

### 4.144 The 25 µs is one extra task-hop per direction, and removing it costs a port per lane (2026-11-08)### 4.144 The 25 µs is one extra task-hop per direction, and removing it costs a port per lane (2026-11-08)

§4.142 closed the read path: the carrier's own work is 1.8 µs and 2.9 µs more when a wake is needed. Every
remaining number is a task hop, so they can simply be added:

| hop | cost | source |
|---|---|---|
| a parked task's wake → run | 5.0 µs | direct meter, 29 000/s |
| apply the datagram | 1.49 µs | direct meter |
| woken consumer → entering its read | 2.9 µs | direct meter, 25 % of reads |
| **one hop on the receive path** | **9.4 µs** | |
| **both directions** | **18.8 µs** | |
| measured gap, mhd 45 µs against TCP 20 µs | **25 µs** | §4.134 |

**Three quarters of the gap is one extra task-hop per direction.** A TCP socket's data arrives in the kernel
and its reader is woken by the kernel: one task per direction. mhd's data is received by a *pump* or a
*dispatcher*, applied there, and only then handed to the consumer: two tasks per direction. Remove one hop
each way and the round trip would be about **26 µs** — within 6 µs of TCP, out of 25 today.

**How to remove it, and what it costs.** The consumer would have to be the task that receives, which means
**one UDP socket per lane** on the accepted side: the listener currently receives every lane on the same port
and demultiplexes by lane id, which is exactly the extra hop.

- *The win*: the accepting task does `recvmsg` → apply → read in one hop. Both directions would then be one
  task, like TCP.
- *The price*: a **port per lane**. The pool opens N lanes per client, so a client that has a pool of four
  holds four server ports instead of one — more NAT bindings, more firewall surface, and a wire change (the
  client must be told which port carries which lane, so it has to ride the accept path in dialect 6.0).
- *Not on the table*: any variant that keeps one port. The demultiplexer has to be a task, and the consumer
  has to be woken by it; §4.142 measured both halves and they are the 9.4 µs.

**So the decision is a product decision, not a tuning one**: whether mhd is willing to hold one server port
per lane to buy roughly 19 µs of loopback round trip. That is why this stops here rather than at a commit —
it changes the carrier's network footprint, which the plan's own constraints treat as a product boundary, and
"attack the 25 µs" was decided before this was the price.

### 4.143 The 85 % line has a baseline now, and the release cell has a price (2026-11-08)### 4.143 The 85 % line has a baseline now, and the release cell has a price (2026-11-08)

Two criterion gaps were left open. One closes here; the other turns out to be a cost question rather than a
forgotten task.

**A baseline for `bulk-n` on the arm that ships.** The published records cannot serve (§4.128: a `sweep` with
no `bulk-n` cell, and a `mohill` row that runs the encrypted plane under a default-sounding name). So the
baseline is recorded here, with the method that produced it, and the rule becomes arithmetic:

```
just bench run --profile dev --arms l4-plain --scenarios bulk-n --rounds 6 --aa
fingerprint 6e70327078ce7037   engine 0967cd826c743b04   elapsed 167.9 s
```

| arm | n | median `throughput_gbps` | min | max |
|---|---|---|---|---|
| `l4-plain` (the shipped code) | 6 | **15.840** | 15.595 | 15.931 |
| `l4-plain~b` (same code, second build) | 6 | 15.740 | 10.785 | 15.776 |
| `l4-plain~aa` (same binary twice) | 6 | 15.693 | 13.978 | 15.867 |

**85 % of 15.70 Gbit/s is 13.35 Gbit/s**, so that is the number a future run is judged against — on this host,
with this profile, on this arm. It is not a release record and does not pretend to be one: `benches/records/`
holds release artifacts, and there is no version here to name one after.

**The release cell's price, so nobody plans around it by mistake.** `timeline` at `--scale 1.0` costs
**119 minutes per arm-round** — measured 358 s at `--scale 0.05` (§4.130) and scaled linearly. The acceptance
asks for it over four arms and two rounds: **15.9 hours**. Even one arm and one round is two hours.

So `timeline[clean]` at scale 1.0 is not a task this loop can run; it is the release sweep, which is what
the plan's wording says and why it says it. What has been done at a fifth of the scale (§4.130) stands as an
indication — p99 24.87 ms and 14.34 ms with zero errors — and the judgement waits for the ritual.

### 4.142 The park is mostly the consumer coming back by itself, not a late wake (2026-11-08)### 4.142 The park is mostly the consumer coming back by itself, not a late wake (2026-11-08)

The instrument §4.141 asked for: `rx_wake`, the time from `waker.wake()` to the consumer entering
`poll_read`, recorded beside the count so a null could be told from a no-op. It fires —
**43 677 samples against 171 394 reads** in one 60 000-request run — and that ratio is the finding.

| what a reader pays | per call | how often |
|---|---|---|
| `waker.wake()` → **entering `poll_read`** (`rx_wake`) | **2.9 µs** | 25 % of reads |
| entering → holding the connection lock (`rx_lock`) | 0.21 µs | every read |
| the copy inside the lock (`io_read`) | 0.09 µs | every read |
| the whole park (bytes ready → taken) | 13–15 µs | every parked read |

**Three quarters of the reads were not woken at all.** The reader was not parked when the bytes arrived, so
`waker.wake()` had nothing to wake, and it came back on its own schedule. The park is measured from *bytes
becoming ready*, so it charges the carrier for however long the consumer was busy elsewhere — in the L1
harness, finishing the previous echo.

**So the carrier's read path is fully accounted for, and it is small**: apply 1.49 µs, lock 0.21, copy 0.09,
and 2.9 µs when a wake is actually needed. Nothing in it is 10 µs, and there is no longer anywhere in the
carrier to look for the 25 µs.

**What that means for the comparison, and it is a correction of method as much as of result.** The park was
never a carrier metric — it is "how long the consumer took to come back", and a TCP echo pays the same
consumer latency. Comparing parks across transports measures the two harnesses' loops, not the two carriers.
The difference that *is* real is the number of hops on the critical path: mhd has write → pump's syscall →
dispatcher's syscall → apply → wake → copy, where TCP has write → kernel → read. Each hop is a task (or a
syscall) of a few µs, and §4.140's arithmetic — two wakes plus the apply ≈ 12 µs against TCP's one wake ≈ 5 —
is the only part of the 25 µs that any measurement here supports.

### 4.141 Two of my three "exclusions" were never verified to fire (2026-11-08)### 4.141 Two of my three "exclusions" were never verified to fire (2026-11-08)

Going back over the numbers to write the summary turned up something worse than another null result.

**§4.138's bound never fired.** The pass was capped at 64 datagrams from the *listener* to test whether the
dispatcher holds its worker while a reader's wake waits. The listener's own cadence line says how many
datagrams a batch carries:

```
listener cadence batches=34424 datagrams=34535
```

**1.003 datagrams per batch.** Sixty-four was never reached, so the cap changed nothing and the experiment
tested nothing. What the same line *does* show is that a pass holds the worker for one datagram's apply —
about 1.5 µs (§4.137) — so a long hold was never plausible; but "not plausible" is not "measured", and
§4.138's conclusion is withdrawn as established. It is now: the hypothesis is unsupported by the occupancy
data, and untested by experiment.

**§4.136's inline send was never verified to fire either.** It asserted no counter, no log line, nothing that
would distinguish "the writer sent it" from "the writer did not, and the pump did as usual". The p50 did not
move, which is consistent with the change firing and being worthless *and* with the change never firing. I
reported it as the former.

**Only §4.140 stands on a direct meter**: `rx_lock` measures the reader's wait for the connection mutex at
0.18 µs, with no gating condition to satisfy. That one is a measurement; the other two were attempts.

**The rule this earns, and it goes into the plan's evidence discipline**: a change whose *effect* is being
measured must first be shown to *take effect* — a counter, a line, a histogram shift — in the same run that
reports the effect. Otherwise a null result is indistinguishable from a no-op, and a null result that cannot
be distinguished from a no-op is not evidence about anything.

**What the numbers that do hold say about the 25 µs**, for whoever picks this up:

| stage | cost | how solid |
|---|---|---|
| apply, decode + ack + append | 1.49 µs | direct meter |
| reader's wait for the connection lock | 0.18 µs | direct meter |
| the reader's copy | 0.08 µs | direct meter |
| a parked pump's wake → run | 5 µs | direct meter, 29 000/s |
| the reader's park (bytes ready → taken) | 13–15 µs | direct meter |

Two wakes (dispatcher, then consumer) plus the apply is about 12 µs, which is the park. A TCP socket pays
**one** wake for the same work, which is the 5 µs line. That accounts for roughly 7 µs of the 25 µs gap; the
other ~18 µs is not in any instrument in this file yet, and the two attempts to find it in the carrier's own
code were both no-ops.

### 4.140 The reader is not blocked by the carrier: 0.18 µs to reach the lock (2026-11-08)### 4.140 The reader is not blocked by the carrier: 0.18 µs to reach the lock (2026-11-08)

§4.139 left two candidates for the 8–10 µs of the park that is not the wake: the consumer's own turnaround,
or the reader waiting on the same per-connection mutex the dispatcher takes per datagram. A meter separates
them — `rx_lock`, the time from entering `poll_read` to holding the lock, beside the existing `io_read`,
which is the copy inside it.

One L1 run, 60 000 requests, and per call:

| what the reader pays | per call |
|---|---|
| entering `poll_read` → **holding the connection lock** (`rx_lock`) | **0.18 µs** |
| the copy inside the lock (`io_read`) | 0.08 µs |
| the whole park, waiting for bytes | 12–15 µs |

**The reader is not blocked by the carrier.** 0.18 µs against a 13 µs park is not a contention story: the
dispatcher takes that mutex 175 000 times in a run and the reader waits a fifth of a microsecond for it.

**So the carrier's entire bookkeeping on a datagram is 1.8 µs** — apply 1.49, lock 0.18, copy 0.08 — and the
13 µs park is *the consumer's own scheduling*: after `waker.wake()` the echo task does not run for ~13 µs,
where the pump's own wake-to-run is 5 µs in the same run (`n≈29 000/s`).

**That is the honest shape of the 25 µs, and it is not a defect in the carrier's code.** Three tasks touch a
datagram in one process — the pump sends, the dispatcher receives and applies, the consumer reads — and the
consumer's wake lands behind the other two on the same runtime. The pump's 5 µs is the floor; the reader's 13
is what it costs when two carrier tasks are already busy on the same workers.

**What would move it, stated so nobody looks for it in the wrong place**: fewer tasks per datagram — the
dispatcher applying *and* the consumer reading in one task for a lane — or a runtime where a lane's tasks do
not share workers with the listener's dispatcher. Both are architecture changes with their own risks, and
neither is a tuning constant. That is where this line of work now stands.

### 4.139 Only 5 µs of the reader's park is the wake — §4.137's phrasing corrected (2026-11-08)### 4.139 Only 5 µs of the reader's park is the wake — §4.137's phrasing corrected (2026-11-08)

§4.137 said the reader's 13–15 µs park was "the reader's wait for its task to be scheduled". The same run
carries the number that tests that claim, and I had not read it: the pump's own wake latency lives on its own
line.

| measurement | p50 | p99 | max |
|---|---|---|---|
| **a parked pump waiting after its wake** (`n≈29 000/s`) | **5 µs** | 9–35 µs | 138 µs |
| a stream's reader waiting for bytes (`park_*`) | **13–15 µs** | 30–35 µs | 2 129 µs |

**So at most 5 µs of the park is the wake.** A task woken in this runtime starts running in 5 µs, measured
29 000 times a second on the same process in the same run; the reader takes three times that. The other
8–10 µs is **not** the wake, and §4.137's phrasing — "the reader's wait for its task to be scheduled" — was
too narrow. The park is measured from *bytes becoming ready* to *the reader taking them*, so it also carries
whatever the consumer was doing, and whatever it had to wait for before it could ask again.

**What that leaves, and it is a shorter list than before.** Between the wake and the read there are two
candidates, and they are separable: the consumer's own turnaround (in the L1 harness the echo loop, which
must write the previous reply before it reads again), and contention with the carrier's own tasks — the
reader's `poll_read` takes the same per-connection mutex the dispatcher holds while applying, and §4.114's
drain means it takes it thousands of times a second.

**The instrument that would separate them** is not the park: it needs the reader's *entry* stamped as it
enters `poll_read` and the lock wait timed inside it, so "the consumer was late" and "the consumer was
blocked on the carrier's lock" become different numbers. That is the next probe, and the pump's 5 µs is the
floor any design has to beat.

### 4.138 Bounding the drain pass did not help, so the drain loop is not the reader's shadow (2026-11-08)### 4.138 Bounding the drain pass did not help, so the drain loop is not the reader's shadow (2026-11-08)

§4.137 raised one hypothesis it could not test: that §4.114's unbounded drain lets the dispatcher hold its
worker while a reader's wake waits behind it. This tests it by bounding the pass at **64 datagrams per
wake-up** — far above the one-per-wake that lost 97 %, and low enough to yield at a bound.

| build | `complete_us` p50 | p99 | **`park_p50_us`** | park p99 |
|---|---|---|---|---|
| unbounded (three runs) | 48–54 | 75–78 | **13–15** | 31–34 |
| bounded at 64 (three runs) | 50–53 | 74–79 | **15–16** | 30–35 |
| unbounded again, after reverting | 52 | 75 | **16** | 35 |

**No effect, and the third row is the point**: the unbounded build reads `park_p50_us = 16` on a rerun, so
13–16 is this instrument's run-to-run noise. The park cannot resolve a 1–2 µs change, and there was no
change to resolve. **Reverted**, and the hypothesis is dead: the drain loop is not what delays the reader.

**A mistake worth recording, because it nearly produced a false conclusion.** The first attempt at this
measurement printed three "bounded" runs that were **measurements of the unchanged code**: the edit script
asserted on an anchor that no longer existed and threw *before* writing the file, and I read the numbers
before checking that the change was in the tree. `grep -c DATAGRAMS_PER_PASS` returned 0 and `git diff
--stat` was empty — that is now the first thing I check before quoting a number from a change.

**Where this leaves the 25 µs.** The park is a task-wake latency of ~15 µs with nothing left to blame
upstream of it: the apply is 1.5 µs, the syscall batching works, the dispatcher's pass is not in the way, and
the sender's dwell is 2–3 µs. What is left is the cost of the wake itself — the waker path (a custom
`Waker` behind `Wakes`, plus a `Notify`) and the runtime's readiness to run the reader's task. That is the
next measurement, and it needs a different instrument than the park: the park says how long, not why.

### 4.137 The 25 µs is scheduling, not computation — and the carrier's own work is 1.5 µs (2026-11-08)### 4.137 The 25 µs is scheduling, not computation — and the carrier's own work is 1.5 µs (2026-11-08)

The receive path was said to be the next target (§4.136). Before adding anything, the instrument that
already existed was made reachable from the harness — `probe::start_stats()`, which starts the product's own
per-second reporter, gated on the same `MOLEHILL_MHD_STATS` switch, and first landed inside `impl Listener`
by mistake and did not compile until it was moved to module scope. Then 60 000 requests through the L1
harness, both ends release, loopback, clean:

| what | p50 | p99 | max |
|---|---|---|---|
| a stream's **reader waiting for bytes** (`park_p50_us`) | **14–17 µs** | 34–76 µs | 2 129 µs |
| a stream's **first byte waiting for the wire** (`dwell_p50_us`) | 2–3 µs | 10–18 µs | 144 µs |

and the cumulative phase timings, per datagram, from the same run:

| stage | cost |
|---|---|
| `on_datagram`, entry to return (`rx_total_us` / `rx_total_n`) | **1.49 µs** |
| ├ `decode` | 0.06 µs |
| ├ `ack` | 0.70 µs |
| └ `append` | 0.55 µs |

**So the carrier's own work on a datagram is 1.5 µs and the reader's wait for its task to be scheduled is
14–17 µs.** The 25 µs gap against a TCP socket is **scheduling, not computation** — and that is why the
inline send in §4.136 was always going to fail: the sender's dwell was 2–3 µs, so there was nothing to win
on that side.

**A hypothesis this raises, which I cannot test retroactively.** §4.114's drain loop makes the dispatcher
keep taking datagrams until the socket is empty. That fixed a 97 % loss, and it also means the dispatcher can
hold its worker while a reader's wake waits behind it — the park went *unmeasured* before that change (the
`mhd-stats` line existed but the harness could not start the reporter), so there is no pre-§4.114 park to
compare against. Stating it so the next step can be designed around it rather than rediscovering it: if the
park is the drain loop's shadow, the fix is to bound a pass by *time* (or by datagrams shared with the
wake-ups it delays) rather than by "until empty", and the measurement is the park's p99 against the same
build with the bound tightened.

**What this rules out, and it is most of the chain**: `recvmsg`, routing, decode, the ack block and the
payload copy together are 1.5 µs. The dispatcher's syscall count is not the problem either — 119 847
`rx_runs` carried 9.1 MB, so the batching works. What is left is one thing: the cost of handing a datagram
from the task that received it to the task that will read it.

### 4.136 Inline send from the writer: built, measured, reverted — and it rules a suspect out (2026-11-08)### 4.136 Inline send from the writer: built, measured, reverted — and it rules a suspect out (2026-11-08)

The user chose "attack the 25 µs" over restating the criterion, so the first attack was the most direct
one: let the writer send its own packet instead of hopping to the pump's task.

**What was built.** `Shared` gained the socket and peer (`attach_link`), a `pumping` flag the pump sets
around `flush`, and `try_send_inline`, which builds one packet with the same `poll_transmit`, sends it with
`try_send_to`, keeps the same counters, and falls back to `wake_pump` on anything it cannot do without
waiting. `poll_write` tries it first. The guard exists because two senders on one socket are fine for UDP but
a packet built now could overtake a run the pump has already staged.

**It changed nothing.** Same harness, same 20 000 requests, 64 bytes, one reused connection:

| build | p50 | p90 | p99 |
|---|---|---|---|
| pump only (before) | 45.0 µs | 55.0 | 65.0 |
| inline send | **45.0 µs** | 53.0 | 61.0 |

p50 identical, p99 −6 % on a single run. **Reverted**, by the same rule RACK went by: a second sender plus a
new ordering invariant is not worth a p99 difference that a rerun could erase.

**The value is what it rules out.** The client's wake-up-to-pump hop is **not** the 25 µs. That is a
hypothesis killed cheaply, and it points the next attack at the other end: the server's `read_us` is 41 µs
over mhd against 15 µs over TCP, and that is measured *after* the datagram has arrived — so the suspect is
the receive path (dispatcher `recvmsg` → route → `on_datagram` → apply → reader wake), not the send one.

**And a process note, because it is the second time.** The first attempt at this measurement was killed by
`pkill -f 'role server --addr …'` matching **its own command line** — the exact self-match AGENTS.md warns
about, and I had already hit it once this session. The rerun used `kill $PID`. Both times the work was fine
and the measurement died.

### 4.135 On the lossy cell every tool ties, because the shaper is the measurement (2026-11-08)### 4.135 On the lossy cell every tool ties, because the shaper is the measurement (2026-11-08)

The complement to §4.134: if the clean cell is structurally unwinnable, does the *shaped* cell show the
carrier winning? `rr-16` under `--condition loss1`, four arms, six rounds each, 28 samples, 279 s:

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| `l4-plain` (mhd) | 20.40 | 245.86 | 551 |
| `l4-plain` A/A twin | 20.38 | 246.94 | 554 |
| `frp` | 20.49 | 246.37 | 550 |
| `rathole` | 20.49 | 246.24 | 566 |

**Everything is the same to within 0.4 %**, and p99 ≈ 246 ms is the shaper's own arithmetic — a 1 %-loss
path where every tool pays one 200 ms retransmission timeout per loss. The cell measures the shaper, not the
carriers. Nothing can be ordered from it, and mhd's first place by 0.4 % is noise, not a win.

**So the picture across the bench is now complete and it is not flattering:**

- **clean loopback**: mhd is **25 µs behind a TCP socket** on a bare round trip and **85 % behind rathole** on
  a proxied request, for the structural reason in §4.134 — a userspace wake the kernel path does not pay.
- **loss1**: everyone ties, because the shaper dominates.
- **the cells that could discriminate** — two or three packets in flight (§4.125), a reorder window, a path
  where per-stream recovery beats a shared TCP connection — **do not exist in the model**.

That is the state of the acceptance line "no worse than same-round TCP and peers": **not met, and not
measurable as the model stands** on either of the two cells that do exist. The carrier's claimed advantage —
per-stream recovery over an unreliable datagram, no head-of-line blocking across streams — is exactly the
thing no cell here tests.

### 4.134 The 25 µs is a userspace wake that TCP's loopback never pays (2026-11-08)### 4.134 The 25 µs is a userspace wake that TCP's loopback never pays (2026-11-08)

The same harness, the same 20 000 requests, 64 bytes, one reused connection, loopback, clean — `--mode tcp`
is a bare TCP echo, so this is transport against transport:

| transport | client round trip p50 | p99 | server `read_us` p50 |
|---|---|---|---|
| bare TCP socket | **20.0 µs** | 34.0 | 15.0 |
| bare mhd stream | **45.0 µs** | 65.0 | 41.0 |

**The extra 25 µs is entirely in the outbound leg**: the server has the request after 41 µs on mhd and 15 µs
on TCP, while the return legs are 4 and 5 µs. So it is not payload, not encryption (both plain), not the
listener's routing — it is what happens between a `write` and the peer's `read`.

**And the mechanism is structural, not a defect.** TCP's loopback path is kernel to kernel: `write` copies
into the peer's socket buffer and its `read` returns, with no userspace task involved. mhd cannot do that —
the bytes go into `SendBuf`, a stream writer must be woken, the pump must be signalled, and the pump runs a
pass that builds a packet and issues `sendmmsg`. On the far side a dispatcher must `recvmsg`, route by lane
id, apply, and wake the reader. **Two userspace wake-ups and one pump pass**, against zero. The pump's own
pass cost is already on the window line: `flush_per_pass ≈ 13 µs`.

**What follows, and it is not comfortable.** On a loopback, clean, single-small-request cell, **no userspace
UDP carrier can beat a TCP socket** — the 25 µs is the price of leaving the kernel, and it is a price TCP on
loopback simply does not pay. So the acceptance line "p50/p90/p99 no worse than same-round TCP and peers",
*as judged on this kind of cell*, is not achievable by this design, and rathole's 61 µs whole-request figure
is 61 µs of kernel loopback plus a thin shim.

That does not make the last three rounds of measurements pointless — they locate the cost precisely, and they
say where mhd *can* win: anywhere TCP's kernel-to-kernel path stops being cheap, which is a real network with
loss, reordering and a shaper, and that is what the `loss1`/`loss5`/`jitter`/`rtt100` conditions exist to
model. **But it does mean the criterion and the cell have to be brought into line**: either the comparison
moves to a shaped path — where both tools pay for the path and the carrier's recovery is what differs — or
the criterion says plainly that on loopback mhd is 25 µs behind by construction.

Recorded here rather than decided: this one is the human's call, because it changes what the carrier is
claiming to be good at.

### 4.133 The bare carrier is 45 µs now, so the proxy is the bigger term — §4.132 corrected (2026-11-08)### 4.133 The bare carrier is 45 µs now, so the proxy is the bigger term — §4.132 corrected (2026-11-08)

§4.132 concluded that the interactive gap sat in the carrier, using §4.101's bare round-trip figure of
64–80 µs. **That figure is stale** — it predates §4.114's drain loop — so it was re-measured before building
anything on it. The L1 harness, release build, 20 000 requests over one reused stream, 64-byte payload,
loopback, clean:

| metric | p50 | p90 | p99 |
|---|---|---|---|
| `complete_us` (bare round trip) | **45.0** | 55.0 | 65.0 |
| `first_us` | 44.0 | 54.0 | 64.0 |
| server `read_us` (request in hand) | **41.0** | — | 64.0 |
| server `write_us` (echo written) | 0.0 | — | 1.0 |

20 000 attempts, `failed: 0`. So the carrier's round trip is **45 µs**, not 64–80, and §4.132's arithmetic
changes sign: `rr-1` measures 113 µs proxied, so the **proxy adds 68 µs on top of a 45 µs carrier**. The
proxy hop is the larger term, not the smaller one.

**And the round trip is almost all outbound.** The server has the request in hand after 41 µs of the 45;
its own reply costs 0–1 µs. So whatever is slow is on the **client's write → wire → server's read** leg: the
post, the pump's wake, the `sendmmsg`, the peer's `recvmsg`, the dispatcher and the apply. That is a much
smaller search space than "the proxy path", and it is where the next instrument goes.

**What this costs §4.132.** Its measurement stands — mhd is 85 % behind rathole on `rr-1` — and its
conclusion does not: the gap is not "the carrier's own round trip exceeds rathole's whole request", it is
that the proxied path pays 68 µs over a 45 µs carrier while rathole pays 61 µs in total. The lesson is the
one this file keeps re-learning: a figure from ten rounds ago is a hypothesis, and this one was 50 % off.

**Next, and it is §8's list applied to a steady-state request rather than a first echo**: bracket the
outbound leg — `post_at` → pump wake → `sendmmsg` → peer `recvmsg` → dispatch → apply → reader wake — inside
the product, on this cell, before touching anything. The harness already emits `post_at` on its side; the
carrier needs the matching stamps on its own.

### 4.132 The gap is per-request and it is in the carrier, not the proxy (2026-11-08)### 4.132 The gap is per-request and it is in the carrier, not the proxy (2026-11-08)

§4.131 showed mhd 25 % behind rathole with sixteen streams. The single-stream cell says more, because it
removes concurrency as an explanation:

`rr-1[clean]`, four arms, six measured rounds each, 47.1 s, 28 samples:

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| `l4-plain` (mhd) | 0.113 | **0.152** | 7 987 |
| `l4-plain` A/A twin | 0.114 | 0.153 | 7 908 |
| `frp` | 0.136 | 0.178 | 6 769 |
| **`rathole`** | **0.061** | **0.082** | **13 788** |

Against rathole: **p50 and p99 both +85 %**, rate **−42 %**. Against frp mhd *wins* (0.113 against 0.136), so
this is not "UDP is slow" or "the proxy is slow" — it is specific to the comparison with rathole.

**And the absolute numbers put the cost in the carrier.** §4.101 measured a **bare mhd round trip** on the
L1 harness at p50 **64–80 µs**. rathole's *entire proxied request* — visitor socket, framing, tunnel, backend
dial, response, all of it — is **61 µs**. So a round trip over the mhd carrier alone costs more than
rathole's whole path, and the proxy logic on top of it adds only ~40 µs (113 − 64…80).

That reframes the remaining work. The interactive path is not paying for mhd's stream layer, its open
handshake, its scheduler or its forwarding loops — those are inside that ~40 µs. It is paying for what one
datagram round trip costs: the pump's wake, its lock, and the syscalls on both ends.

**This is a hypothesis about where to look, not a diagnosis**, and it is stated as the next thing to test:
instrument one steady-state request inside the carrier — post → pump wake → `sendmmsg` → peer `recvmsg` →
dispatch → apply → reader wake — and see which bracket holds the 60 µs. If the bare L1 number has moved since
§4.101 (it was measured before §4.114's drain loop), that measurement comes first, because everything above
assumes it still reads 64–80 µs.

### 4.131 mhd is *worse* than rathole on the interactive cell, and that is the acceptance line (2026-11-08)### 4.131 mhd is *worse* than rathole on the interactive cell, and that is the acceptance line (2026-11-08)

The acceptance says "p50/p90/p99 no worse than same-round TCP and peers". Two things had to be established
before it could even be asked, and one of them is an answer nobody wants.

**"TCP" is not expressible as an arm.** `refuse_carrier_arm` refuses one by design: the data plane's
carrier is chosen at runtime (mhd, falling back to TCP) and the configuration has no field for it, so an arm
naming a carrier would describe a setting the product does not have (§ the catalog's own comment). The
"peers" half is expressible, so that is what was measured.

`rr-16[clean]`, four arms, six measured rounds each, 40.9 s, 28 samples:

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rate_per_s` |
|---|---|---|---|
| `l4-plain` (mhd, plain plane) | 0.59 | **1.71** | 22 562 |
| `l4-plain` A/A twin | 0.60 | 1.72 | 22 599 |
| `frp` | 0.59 | 1.65 | 22 890 |
| **`rathole`** | **0.49** | **1.37** | **23 940** |

**mhd loses this cell.** Against rathole: p99 **+24.8 %**, p50 **+20 %**, rate −5.8 %. Against frp: p99
+3.6 %, rate −1.4 %. The A/A twin differs by **0.6 %** on p99, so a 24.8 % gap is not noise — it is the
answer.

**Why the model said nothing about it**: it compared the A/B pair and the A/A pair and reported "0 further
metric comparisons"; cross-tool comparisons are not scored. So this is a hand-read of six-round medians with
the twin as the noise floor, and the numbers are stated here rather than promoted to a claim.

**What this means for the objective, plainly.** "No worse than same-round TCP and peers" is an acceptance
criterion, and on the one cell where it can be measured, the carrier is 25 % behind rathole and 4 % behind
frp. That is not a rounding difference or a resolution problem; it is the product being slower than the tool
it forked from, on the workload the bench resolves best. The next work on the interactive path — whatever it
is — is measured against 1.37 ms, not against the previous mhd build.

**One thing that would make the TCP half askable**, if it is ever wanted: a build-time or environment
selection the runner can name as an arm. `MOLEHILL_DATA_CARRIER` exists as a switch; an arm cannot use it
today, and inventing one would mean the arm describes a setting the product does not have — which is exactly
what `refuse_carrier_arm` exists to prevent. So the honest options are: drop "TCP" from the criterion, or
add a supported way to select the carrier and then let an arm name it.

### 4.130 The clean cell's absolute thresholds pass, at a fifth of the required scale (2026-11-08)### 4.130 The clean cell's absolute thresholds pass, at a fifth of the required scale (2026-11-08)

The acceptance's headline criterion is `timeline[clean]` and `clean#2` at **p99 ≤ 50 ms** and error rate
**≤ 0.5 %**, judged on the release sweep at `--scale 1.0`. A full-scale timeline is forty-odd minutes per
arm-round, so this run is `--scale 0.05` — two rounds, three arms, 54 samples, 1432 s (358 s per
arm-round) — and that difference is stated rather than smoothed over.

| cell | arm | `rtt_p50_ms` | **`rtt_p99_ms`** | `rtt_max_ms` | error rate |
|---|---|---|---|---|---|
| `clean` | `l4-plain` | 2.06 | **24.87** | 62.51 | **0.00 %** |
| `clean` | A/A twin | 1.92 | 15.19 | 38.74 | 0.00 % |
| `clean#2` | `l4-plain` | 1.61 | **14.34** | 25.95 | **0.00 %** |
| `clean#2` | A/A twin | 1.47 | 12.01 | 14.92 | 0.00 % |

**Both clean cells are inside 50 ms and both report zero errors.** Three things this run does *not*
establish, in the order they matter:

1. **The scale.** The criterion names 1.0; this is 0.05. Holds are shorter, so there are fewer samples per
   phase and less time to accumulate a tail. It is a strong indication and not the judgement.
2. **The comparison.** "p50/p90/p99 no worse than same-round TCP and peers" needs `tcp`-class arms and the
   peers in the same run, and this run had `l4-plain` and its own twin only.
3. **The cell cannot order anything anyway.** The run reports p99 resolution at **173 %** on `clean#2` —
   two copies of one binary are 39–167 % apart on this cell (§4.104). So no A/B is claimed from it; the
   only thing it can answer is an *absolute* threshold, which is exactly what it was asked here.

`rtt_max_ms` on `clean` was 62.51 ms against the twin's 38.74 — one outlier in 439 samples, and the
criterion is p99, so it is recorded and not acted on. The shaped cells look as expected for what they are:
`rtt100` sits at p50 401 ms, `loss5` at p99 1 743 ms.

### 4.129 Capacity passes at 8/8 with no level breaking the SLO (2026-11-08)### 4.129 Capacity passes at 8/8 with no level breaking the SLO (2026-11-08)

Second acceptance guardrail. `capacity` on `l4-plain` with an A/A twin, three rounds, 72 samples, 477.7 s:

| level | `slo_broken` (both arms) | interactive `rtt_p99_ms` (arm / twin) |
|---|---|---|
| L1 | 0 | 1.71 / 1.33 |
| L2 | 0 | 1.67 / 5.80 |
| L3 | 0 | 1.51 / 1.34 |
| L4 | 0 | 1.54 / 2.67 |
| L5 | 0 | 2.08 / 1.87 |
| L6 | 0 | 2.48 / **22.30** |
| L7 | 0 | 4.88 / 1.96 |
| L8 | 0 | 2.38 / 2.29 |
| **ramp** | — | **`capacity_streams` = 8** on both arms |

**8 of 8, and not marginally**: every level's interactive stream stayed inside the SLO, and the worst single
reading anywhere in the run (22.3 ms, on the *twin* at L6) is still less than half the 50 ms budget. The
`capacity_headroom_pct` of 0.0 is the metric's definition, not a miss — the ramp's ceiling is 8, so
sustaining 8 leaves no headroom above it.

The per-level `rtt_p99_ms` resolutions are poor at three rounds (15–50 %, as the run itself reports), so no
level-to-level comparison is claimed; what is claimed is the headline, which is a count and counts exactly.

### 4.128 Phase 3 costs nothing on bulk, and the 85 % criterion has no baseline yet (2026-11-08)### 4.128 Phase 3 costs nothing on bulk, and the 85 % criterion has no baseline yet (2026-11-08)

The first acceptance guardrail, because phase 3 changed the loss and retransmission path: `bulk-n[P8]` on
`l4-plain`, three arms (pre-phase-3 `f975207`, current, A/A twin), six rounds, 21 samples, 167.9 s.

| metric | pre-phase-3 | current | change | A/A |
|---|---|---|---|---|
| `throughput_gbps` | 15.840 | 15.740 | **−0.63 %** | −0.93 % |
| `offered_gbps` | 15.061 | 14.961 | −0.67 % | −0.82 % |
| `cpu_cores` | 10.559 | 10.460 | −0.93 % | −0.74 % |
| `cpu_s_per_gbit` | 0.700 | 0.701 | +0.19 % | +0.21 % |
| `wire_per_visitor_byte` | 1.039 | 1.039 | −0.01 % | −0.03 % |
| `rss_peak_mib` | 158.85 | 162.55 | +2.33 % | +7.49 % |

**The throughput change is smaller than the twin's own difference** (−0.63 % against −0.93 %), so phase 3
costs nothing measurable on bulk. Guardrail passes.

**But the criterion itself cannot be judged yet, and the reason matters.** "`bulk-n` at ≥85 % of a valid
baseline" needs a baseline measured with the arm that ships. The published records
(`benches/records/results-bench-v0.11.2.json`) are a `sweep` with arms `molehill`/`l3`/`frp`/`rathole` and
**no `bulk-n` cell at all** — the sweep is timeline and capacity only. And the arm they do carry is not the
product default: `"molehill"` inherits `noise_data = True`, so every stream in every published sweep paid a
Noise handshake, while the product's default is `data_enable = false` (§4.105). The catalog comment called
that arm "the product in its default configuration", which was simply false, and is corrected.

So the 85 % line can only be judged after a baseline is recorded with `l4-plain` on this host — a `dev` run
kept as a record, or the release sweep once its arms include the plain plane. Until then the honest answer
to "is `bulk-n` within 85 %" is **not knowable**, and the −0.63 % above is a *build-to-build* guardrail, not
the criterion.

### 4.127 The "independent probe deadline" is a no-op here, and liveness already resets (2026-11-08)

§6.2 had two bullets left. Neither needed the code I was about to write, and saying so with a test is better
than shipping a second timer that changes nothing.

**The probe deadline cannot be earlier than the timer that exists.** §6.2 asks for it to be computed
"independently from the latest still-outstanding packet". `arm` anchors the one timer on the **oldest**
data-bearing packet with the same `rto` a probe would use — and `oldest ≤ latest`, so `oldest + rto ≤
latest + rto` always. A second deadline taking `min` of the two could only ever pick the one that exists.
What §6.2 actually wants is that the timer is a **backstop** while loss is decided by evidence, and that is
what `on_timeout` already does: it declares whatever is past the reorder window, and probes only when
nothing is. Changing the anchor to the newest would be the bug, not the fix — it delays every repair by a
send interval, and `the_timer_is_never_later_than_a_latest_anchored_probe_would_be` fails on it (the two
instants differ by exactly the 20 ms small-flight timeout).

**Liveness already separates.** `Expiry::Dead` needs `DEAD_LINK` unanswered retransmissions **and**
`DEAD_FLOOR` of silence, and `heard()` clears both — `ten_silent_retries_need_five_seconds_of_silence`
already pinned the count and the floor; what was missing was the **reset**, which is the half that makes
"cheap PTO counts cannot kill a path" true.
`a_real_receipt_resets_the_silence_that_would_have_killed_the_path` sets the exact state that kills, calls
`heard()`, and asserts the next timeout is not dead. Making `heard()` stop clearing fails it at "a receipt
clears the unanswered count".

**A note on my own first attempt at that test**: it drove forty probes and asserted no death, and it passed
**with `heard()` removed** — because after a probe takes the only record the table is empty, `on_timeout`
returns `Expiry::Loss` on `live == 0` before ever reaching the dead branch. It was asserting nothing. The
rewritten one sets the state directly and fails when the reset goes.

So §6.2 closes with: one range per probe (§4.126, measured), the timer as a backstop (pinned by ordering),
and liveness reset by real receipt (pinned). Suite green at 378.

### 4.126 Phase 3's second cut: a probe re-sends one range, not the flight (2026-11-08)

§6.2's most concrete item, and the one whose shape §4.107 predicted: a probe timeout re-sent **every**
outstanding packet. `Recovery::on_timeout`'s early branch collected the whole live table, `take`-ed each
record and handed them all back to be repaired as new numbers — so every probe multiplied the flight, and
each pass re-sent more than the last. That is the "retransmission of everything in flight on each write"
the file's own comment calls a storm.

It now probes with **one** range: the oldest live record, by `(time, number)`. A probe exists to elicit an
acknowledgement, and one new number does that — the peer's block answers it, the frontier moves, and the
rest of the flight is retired by that answer rather than duplicated before it arrives.

`a_probe_re_sends_one_range_not_the_flight` pins it: three packets, a timer that fires before anything is
past the reordering window, and the answer is one `Sent` — the oldest — with two records still tracked.
Restoring the old branch fails it with "a probe re-sends one range; it offered 3". Suite green at 376.

**What is not done, and it is the harder half of §6.2.** The probe deadline is still not independent: `arm`
anchors the single timer on the **oldest packet carrying stream data**, which is the right anchor for
*declaring a loss* (an old hole must expire) and the wrong one for *probing a silent path* (a PTO runs from
the latest outstanding packet, because that one has had the least time to be acknowledged). One timer
serving both is why the probe branch only runs when the timer fires early — small flights and short
arms — and never on a busy lane. Splitting them needs a second deadline and a pump that waits on the
earlier of the two, which is a change of its own.

**The criterion, written before the run, and what the run said.** `rr-1` + `--condition loss1`,
`rtt_p99_ms` and `rtt_max_ms` against the same run's A/A, kept only if one clears the floors. The baseline
is §4.125's 39.85 req/s, p99 246.4 ms, p50 20.6 ms. Six rounds, three arms, 21 samples, 1277 s:

| metric | pre | probe | change | A/A |
|---|---|---|---|---|
| `rtt_p50_ms` | 20.641 | 20.636 | −0.03 % | −0.03 % |
| `rtt_p99_ms` | 246.653 | 245.394 | **−0.51 %** | −0.11 % |
| `rtt_max_ms` | 285.908 | 274.127 | −4.12 % | +30.81 % |
| `rate_per_s` | 39.40 | 40.35 | **+2.41 %** | −0.13 % |
| `retrans_segments` | 97.5 | **86.0** | **−11.79 %** | −1.03 % |
| `wire_per_visitor_byte` | 3.754 | 3.761 | +0.18 % | +0.08 % |

**The pre-registered endpoint was not met**: p99 −0.51 % is inside the floors, and `rtt_max_ms` is
unresolvable at this run's length (A/A +30.81 %). The model's verdict is "no claim cleared the floors".

**And the change stays anyway, on a ground I want stated plainly rather than smuggled.** §6.2 of the
approved plan does not ask for this as an optimisation — it requires it: "每次 PTO 最多重发一个必要的最旧
未确认范围以产生 ACK；不得把 outstanding flight 全量加入 repair". The old branch violated that
requirement, and §6.4's revert rule is written for optimisations that fail to show a gain, not for a
correctness requirement of the design. So this is a **departure from "revert if the pre-registered
endpoint does not clear"**, and the reason is that the endpoint was the wrong instrument for a change whose
mechanism is retransmission volume, not latency.

**What the run does say, and it is consistent with the mechanism**: `retrans_segments` −11.79 % against an
A/A difference of −1.03 % — eleven times the twin — and `rate_per_s` +2.41 % against −0.13 %. Both point
the way the change predicts, and neither clears its materiality floor (15 % and 10 %). So: mechanism
pinned, direction consistent, magnitude below what this run can call a claim. If a later round wants the
claim, the cell is `rr-16` + `loss1` with `retrans_segments` pre-registered, and it needs more rounds than
six.

### 4.125 RACK reverted: both cells are structurally blind to it (2026-11-08)

§4.124 landed RACK and owed it a bench judgement. Two cells, two shapes, both null:

| cell | why it was chosen | `rtt_p99_ms` pre → post | A/A | `rate_per_s` |
|---|---|---|---|---|
| `rr-16` + `loss1` | many small requests | 246.276 → 246.171 (**−0.04 %**) | −0.10 % | +0.38 % |
| `rr-1` + `loss1` | one stream, RTO-bound | 246.707 → 246.448 (**−0.10 %**) | −0.13 % | −0.50 % |

The second ran 33 samples over 2007 s (66.9 s per arm-round) and the model's verdict was "no claim cleared
the floors", with the A/A twin agreeing to 0.13 %. **Reverted**, per §6.4: an unproven change in the loss
path is a liability, not a neutral — it can declare spurious losses under reordering, which is the
retransmit storm the plan warns about.

**And the interesting part is why neither cell could have shown it.** RACK needs a *later* packet's
acknowledgement to anchor its window on:

- `rr-16` keeps sixteen streams busy, so the in-flight flight per stream is **deeper than three packets** —
  the old three-packet threshold already declares the loss, and RACK has nothing left to add.
- `rr-1` is the opposite: one request at a time, so a lost packet is usually the **last** one in its flight
  and no later acknowledgement exists to anchor on. `newest` is `None` and RACK cannot fire. That case is
  what PTO (§6.2) is for, not RACK.

So the model has **no cell where RACK is the only loss detector** — which needs two or three packets in
flight per stream, sustained. That is a scenario the model does not have, and it is now named in
`benchmarks.md`'s "cannot answer" list rather than rediscovered the hard way next time.

**What this costs and what it buys.** Three attempts and about an hour of bench time to establish that a
correct-looking change cannot be measured here; the change is gone, the repository is back to 375 tests,
and the next phase-3 step (PTO) is exactly the one whose cell *does* exist — `rr-1` under loss is
RTO-bound by construction, which is what PTO shortens.

### 4.124 Phase 3's first cut: RACK anchored at the acknowledged packet (2026-11-08)

The plan's §6.1 asks for loss detection anchored at the **most recently acknowledged** packet rather than
at each packet's own age. What existed was the three-packet threshold (`ack.largest - number >= 3`) plus the
timer's per-packet staleness — and `SMALL_FLIGHT_PACKETS` is **two**, so a two-packet flight could never
reach the threshold and could only ever be declared lost by the retransmission timer.

`Recovery::on_ack` now takes `reorder = self.rtt.loss_delay()` — which is already `9/8 × max(latest,
smoothed)` with the granularity floor, exactly the window §6.1 names — and declares lost any in-flight
packet **below the anchor** that the block does not name, once `now >= anchor.sent_at + reorder`.

**One arithmetic trap, and it would have made the change dead code that looks alive.** The window has to
be read *before* this acknowledgement samples the path: the sample is `now - anchor`, so a window computed
afterwards gives a deadline of `anchor + 9/8 × (now - anchor)`, which is always **ahead** of `now` and can
never fire. The deadline is therefore captured at the top of `on_ack`.

Two tests pin both sides: `one_packet_behind_is_lost_once_the_reorder_window_has_run` (500 ms after an
anchor 20 ms later, the packet one behind is lost — and the three-packet threshold cannot see it) and
`one_packet_behind_is_reordering_inside_the_reorder_window` (10 ms after the anchor is reordering, not
loss). Disabling the block fails the first with "the packet one behind the anchor is lost once the window
has run", and the whole suite is green at 377.

**What this is not, yet — and the first attempt at it failed to produce a verdict.** The plan requires
loss1/loss5/jitter in the same round, plus clean and `bulk-n` as regression guards. The `rr-16` run under
`--condition loss1` (three arms, 8 rounds asked) **hit the dev profile's 150 s budget** and stopped after
21 samples — one warmup plus six measured rounds per arm — so the model never printed a verdict
(`finished` is empty, `elapsed_s` 0.0). A run the model refuses to score is not a null result and is not
promoted here.

What the six rounds say, for the next attempt to check against: `rtt_p99_ms` 246.276 pre-RACK against
246.171 with it (**−0.04 %**), the A/A twin differing by **−0.10 %**; `rtt_p50_ms` identical; `rate_per_s`
+0.38 % (A/A +1.36 %); `retrans_segments` +0.32 % (A/A −1.77 %). **Nothing moves.** Note what the cell
looks like under loss1: p99 is 246 ms against 1.6 ms clean, and the rate falls from 24 000 to 610 — the
shaper dominates, so this cell may simply be unable to see a reorder-window change.

**So the next round runs loss1 again with a budget that can finish, plus loss5 and jitter, and `clean` and
`bulk-n` as the regression guards. If none of the lossy cells improves beyond their A/A, the change is
reverted** — that is what §6.4 says and this entry is not going to argue with it.

### 4.120 The eight lifecycle invariants, audited one by one (2026-11-08)

Phase 3 touches the retire/repair path, liveness and the hold, so the plan's eight lifecycle invariants
were due an audit rather than the "checklist" status they carried out of phase 1. Result: **two pinned,
three partial, three not pinned** — and the table lives in the plan's §4.2 so phase 3 reads it before it
moves anything.

| # | invariant | state |
|---|---|---|
| 1 | packet/stream id wraparound | **pinned, and a second real defect** — see below |
| 2 | a duplicate open does not accept twice | **pinned now** — see below |
| 3 | a late packet does not resurrect a retired stream | **pinned, and the audit found a real defect here** — see below |
| 4 | credit advances with no data | **pinned now** — see below |
| 5 | FIN over a hole; reset against unsent data | **pinned**: `a_finish_behind_a_hole_does_not_end_the_stream` and `a_reset_leaves_even_with_bytes_still_unsent` |
| 6 | close releases pump, dispatcher, socket, receive | **partial, three of four now**: `a_closed_lane_lets_its_pump_go` asserts the pump ends within two seconds **and** drops its handle on the socket. The listener's dispatcher is still uncovered |
| 7 | sustained loss keeps tasks, memory and the table bounded | **partial**: the table cap, the dead path and an abandoned stream's buffer are pinned; task and memory bounds are not measured |
| 8 | `LOST_HOLD` leaves no unrecoverable frontier | **pinned** |

**One of the eight got pinned here**, because it is the one loss makes reachable: a lost acknowledgement
makes the sender re-send the opening packet, so a duplicate open is ordinary traffic, not a hostile peer.
`a_second_open_for_a_known_stream_is_not_accepted_twice` delivers the same opening datagram twice and
asserts the second one queues no accept. **It is load-bearing**: removing `apply_datagram`'s `if !known`
guard fails it with "a repeated opening frame must not queue a second accept for stream 1".

**Two of the eight are pinned by this round.** The second is #4:
`credit_goes_out_from_an_end_that_has_nothing_else_to_send` drives the policy rather than a transfer,
because credit falls due only once the reader has consumed a quarter of the window (4 MiB) and pushing
that through a `Link` would take thousands of passes to assert one ordering. It pins three things: a fresh
end advertises its whole window, an end that has drained past `WINDOW / 4` owes more, and paying it
settles the debt. Removing `build_packet`'s `credit_due()` branch fails it at the first packet.

That is the standard the other five are now measured against — a test that fails when its guard is
removed, not one that merely passes.

**The audit paid for itself on #3.** The rule that dropped late packets covered only
*our own* parity — an id in the peer's space that the connection did not hold was assumed to be a stream
being opened. So a late packet for a stream this end had **accepted and then retired** created a new
stream and queued a phantom accept: `a_late_packet_for_an_accepted_stream_does_not_open_a_new_one` was
written first and **failed against the code as it stood**, with a visitor whose connection had already
closed still being able to make the server open a channel for it.

The rule is now `Conn::is_retired_stream`: an id in our own space that we do not hold is retired; an id
in the peer's space is new only if it sits at or above the highest one this end has accepted, because
stream ids are monotonic. Both halves are pinned, and the dialer half was verified to fail when the
branch is disabled.

**The audit found a second real defect, on #1.** `open_at` inserted the minted id
unconditionally, and stream ids wrap by two — so once the space wrapped, the minter could hand out an id a
live stream still held and `streams.insert` would replace that stream's state: ciphers, frontier, place in
the window, with no error anywhere. `opening_a_stream_never_hands_out_an_id_a_live_stream_holds` was
written first and failed against the code ("the minter handed out id 1 while a live stream held it"); the
minter now probes at most `len() + 1` candidates of its parity, which by the pigeonhole always finds a free
id because only `len()` ids are occupied in total.

**And #6 is pinned as a property, not a branch.** `a_closed_lane_lets_its_pump_go` asserts the pump stops
within two seconds and lets go of the socket on the way out. Disabling either of the two exits I could
disable without breaking compilation still left it green, because the pump has three: the select's
`closed()` arm, the loop's `is_closed()` check, and the wake `close()` delivers through the pump signal.
That redundancy is good news for the carrier and bad news for attribution — the test pins that the lane
releases, not which line does it. (I also caught myself about to report "all exits disabled and it still
passed" as evidence: that variant did not compile, `Wake::Closed` left uncovered.)

**#5's first half, and the rule it pins.** `RecvBuf::is_complete()` is
`fin_at == read && pieces.is_empty()`, and `Conn::peer_finished` asks for `peer_fin && is_complete()` — so a
finish that arrives ahead of a hole cannot end the stream. That is the correct rule and it was unpinned;
`a_finish_behind_a_hole_does_not_end_the_stream` delivers the peer's last range first, then the finish, and
asserts the stream is not complete and the frontier stops at 100. **Replacing `is_complete` with
`fin_at.is_some()` fails it** with "the bytes before 300 have not all arrived, so the stream cannot be
over" — the truncation it exists to prevent. **The other half of #5 is pinned too.** `a_reset_leaves_even_with_bytes_still_unsent`
closes a stream with 24 KiB still buffered, waits out the hold, and asserts a `Reset` reaches the wire and
that sending it empties the queue. Disabling `transmit_reset` fails it with "the reset has to reach the
wire: 1 packets went out without it" — and note that **not one data packet went out either**, so the test
is precisely about the reset and not about draining.

Writing it turned up a trap worth recording: `close_stream` stamps `abandoned_at` from the **real** clock,
not from the instant the caller passes, so a deadline computed as `test_now + ABANDON_HOLD` can miss by
the time the preceding write took. The test derives it from the stream's own stamp.

**What phase 3 must pin before it moves.** It changes retire, liveness and the hold, which are #3, #6 and
#8. #8 is already pinned; **#3 and #6 are not**, so they come first. #4 is a free pin and #1 is the one
open hazard with a plausible failure mode (`streams.insert` over a live stream) that no test would catch
today.

### 4.119 Phase 2 buys a per-open saving and costs nothing steady-state (2026-11-08)

§4.106 kept phase 2 against its own revert rule — p99 −11.4 % on `churn-16`, under the 15 % floor — with
the departure stated as a judgement. The new instrument (§4.118) turns that judgement into a measurement,
and it explains the whole thing.

`rr-16[l4-plain]`, 8 rounds, 3 arms, pre-registered `rtt_p99_ms`, resolution **1.48 %**:

| metric | pre-phase-2 `74d96e4` | phase 2 `df43d57` | change | floor |
|---|---|---|---|---|
| `rtt_p99_ms` | 1.613 | 1.623 | **+0.62 %** | 1.48 % |
| `rtt_p50_ms` | 0.598 | 0.594 | −0.67 % | 0.67 % |
| `rate_per_s` | 23 850 | 23 898 | +0.20 % | 0.47 % |
| `cpu_cores` | 5.690 | 5.705 | +0.25 % | 1.28 % |
| `wire_per_visitor_byte` | **3.701** | **3.699** | **−0.06 %** | 0.05 % |

**Nothing moves**, and the near-zero `wire` figure is the finding rather than a null result: on `churn-16`
the same change took `wire_per_visitor_byte` **3.5646 → 2.8921** and `cpu_cores` **3.2065 → 2.7115**.

**The difference is opens.** `rr-16` opens sixteen streams at the start and then serves 24 000 requests a
second over them, so an acknowledgement saved *per stream* is rounded away; `churn-16` opens a connection
per request, so the same saving is paid 24 000 times. Phase 2 removes a round trip at stream open — it is
a **per-open** saving, and only a churn workload can price it.

**So the departure in §4.106 was right for a reason I could not then state**: the change buys a real
per-open saving, and this run shows it costs nothing in steady state — every steady-state metric sits
inside its floor, including `wire` at 0.05 %. That is the trade a carrier wants: pay nothing when
idle-steady, save a round trip when connections are frequent.

### 4.118 The drain fix is a no-op on `rr-16` too, and `rr-16` is the latency instrument (2026-11-08)

§4.117 left the drain fix unpriced on the one shape it was supposed to help — many small datagrams — so
the next run pre-registered `rtt_p99_ms` on `rr-16[l4-plain]`, eight rounds, three arms. It resolves the
metric to **1.19 %** (A/A difference 0.56 %), the tightest this project has ever seen on a latency cell,
in **81 s** (3.38 s per arm-round).

| arm | `rtt_p50_ms` | `rtt_p99_ms` | `rtt_max_ms` | `rate_per_s` | `cpu_cores` |
|---|---|---|---|---|---|
| pre-drain | 0.595 | 1.615 | 10.518 | 23 893 | 5.516 |
| drain | 0.593 | **1.601** | 10.614 | 23 997 | 5.511 |
| A/A twin | 0.594 | 1.604 | 11.426 | 23 950 | 5.527 |

**−0.9 % at a 1.19 % resolution: no change.** Not a claim, and this time the run could have made one.

**So the fix is a correctness fix, not a performance one, and the arithmetic says why.** The
one-datagram-per-wake-up cap is the reader's *loop rate*, and that rate depends on the build: the
integration test is a **debug** binary and its listener managed about 690 wake-ups a second, while these
cells run a **release** binary whose loop is fast enough that the cap never bound — 24 000 requests a
second are served with p99 at 1.6 ms. The striped test lost 97 % because its reader was twenty times too
slow for the traffic it had; a release reader at the same traffic is not. Whether a *release* deployment
can reach a rate where the drain loop matters is now an open question, and it needs a cell that
saturates the listener with non-coalesced datagrams — the striped shape, which the model does not have
(§4.117).

**And the run repays the detour with a better instrument.** `rr-16[l4-plain]` resolves `rtt_p99_ms` to
1.19 % in 81 s, against the `timeline` cell's 39–167 % between two copies of one binary (§4.104). So
**phase 2's latency is judged here, not on `timeline` at a shrunken scale**: same cell, pre-registered
`rtt_p99_ms`, three arms, 81 s. That replaces the plan's "judge phase 2 on `timeline` + `l4-plain` with
`rtt_worst_1s_ms` / `rtt_max_ms`", which was written when no cell could resolve a per-connection latency
difference at all.

### 4.117 The drain fix does nothing for bulk, and that is the useful part (2026-11-08)

§4.116 could not score the drain fix on four rounds. Six rounds of the cheapest cell can, and the answer
is **no effect** — measured, not assumed:

| arm | `throughput_gbps` | `cpu_cores` | `rss_peak_mib` | `wire_per_visitor_byte` |
|---|---|---|---|---|
| pre-drain `6dd505fd` | 15.064 | 10.361 | 155.7 | 1.039 |
| drain `08285417` | **15.002** | 10.331 | 153.7 | 1.039 |
| A/A twin (pre-drain again) | 14.994 | 10.355 | 160.3 | 1.039 |

`bulk-n[P8]`, 6 rounds, 3 arms, 168.3 s, 21 samples, 9.35 s per arm-round. Throughput moves **−0.4 %**
against an A/A difference of **0.46 %** and an A/A scatter of **4.92 %** — the same number. `cpu_cores`
−0.3 %, `wire_per_visitor_byte` identical to three decimals.

**Which is the interesting part.** The one-datagram-per-wake-up limit only binds when a wake-up carries one
datagram, and on the bench's bulk cells it never does: GSO/GRO hands the kernel coalesced runs, so one
`recvmsg` returns a whole run and the listener was never the constraint there. The striped test's traffic
has no coalescing — 1 349-byte datagrams from a TCP relay, one per packet — so the same code path there
was capped at ~690 a second and dropped 97 %.

So the drain fix is a **many-small-datagrams** fix, not a throughput one, and the bench's bulk cells are
structurally unable to see this class of defect. That is why ten rounds of benchmark numbers never showed
a direction losing 97 % of its traffic: the cells were measuring a shape the bug does not touch. A cell
that writes like the striped relay — one small datagram per packet, no GSO — is the gap, and it belongs on
the list of things the model cannot currently answer.

### 4.116 The drain fix's bench effect is unmeasured, and the model said so first (2026-11-08)

The drain fix is a change to the receive path, so every earlier benchmark number describes code whose
listener took one datagram per wake-up. It has to be re-measured — and the first attempt **could not**,
which is worth recording rather than hiding.

Three arms in one run (pre-drain `6dd505fd`, drain `08285417`, and the A/A twin), `l4-plain`,
`bulk-n,rr-16`, 4 rounds: 194 s, 30 samples, 16.17 s per arm-round. The verdict:

```
no claim cleared the floors: every difference this run saw is inside its own scatter
  or below its metric's materiality
l4-plain~aa: 29 metric comparisons against a second copy of itself, 0 claims
l4-plain~b:  29 metric comparisons against a second copy of itself, 0 claims
rr-16|c16 rate_per_s: resolves 33.0 % against a 10 % floor — about 42 rounds would reach it
rr-16|c16 cpu_cores: resolves 34.3 % against a 15 % floor — about 21 rounds would reach it
```

It even printed a "+188.70 %" rate difference with a confidence interval spanning zero and `p 0.625`,
flagged `FRAGILE: dropping any one round flips this`. **Four rounds cannot resolve these cells**, and
the run's own A/A twin is the evidence: two copies of one binary differ by as much as the A/B does.

So the honest position is that the drain fix's throughput effect is **still unknown**, and the next
attempt is `bulk-n` only (6 rounds would resolve its `rss_peak_mib`, and it is the cheapest cell) or the
`rr` cells at 20-plus rounds. Nothing is promoted from this run.

### 4.115 Phase 1's missing invariant is now a guard, and it is not a reproduction (2026-11-08)

`the_flight_never_outgrows_the_window` closes the gap §4.107 named: phase 1 was closed on "a re-sent range
is counted once" and "a finished exchange leaves nothing counted", and neither of those is `flight ≤ cwnd`,
which is what `write` admits against. The test runs a 256 KiB transfer that keeps the window full and drops
every fourth pass, and asserts the invariant after every pass.

**It passes on the current code, and it also passes with §4.108's prefix retirement disabled** — I checked,
because a test that cannot fail is theatre. In the in-process `Link` harness the cumulative frontier already
keeps the table honest; the live failure needed the listener dropping 97 % of a direction, which this
harness cannot produce. So it is recorded as a **guard on the invariant** — a future path that adds bytes to
the flight without retiring them fails here in milliseconds instead of on a live lane ten rounds later —
and not as a reproduction of §4.107. The reproduction is that section's counter snapshot.

### 4.114 The listener took one datagram per wake-up, and that was the stall (2026-11-08)

§4.113 split the counters and the contradiction dissolved: `listener_read` 63 562 with `routed` 63 534 on
the same run — **the listener routes 99.96 % of what it reads** — and `pump_read` 466 131, which is where
the "20 000 a second" I had been reading actually came from. The listener's own rate was about **650
datagrams a second, at 0.9 per batch**, while the client posted ten times that.

**Why one per batch**: `RecvBatch::recv` performs exactly **one `recvmsg`**. The batch's capacity (16 for
the listener, 44 for a pump) bounds a GRO run, not the number of separate datagrams one call can take. So
`receive` returned after one datagram however many the socket held, and the dispatcher called it once per
wake-up: **the listener's ceiling was its own loop rate (~690/s), not the socket's**. The kernel dropped
everything above that, which is the 97 %.

**The fix is the sentence this file has been writing since §4.101 and never applying to this reader**:
drain until the socket is empty. One wake-up now loops `try_io` until `WouldBlock` (or a zero-length
read), so a burst is taken in one pass, and the `WouldBlock` still clears the cached readiness so the
next wait is a real edge.

**Measured**: `striped_data_channels` **10 of 10 pass, every one in 2.96–3.90 s**. Before the change
the same cell failed about one run in four, and its failures took 40–70 s. Ten rounds of instrumenting a
stall ended at a receive loop that took one packet per syscall.

**And it unblocked the branch**: the push that had failed eight times ran the whole suite green and
landed sixteen commits (`74d96e4..a7a805f`). The forwarder's deadlock was never in the recovery table,
the acknowledgement window, the buffer policy or the pump; it was a reader that took one datagram per
wake-up while ten times that arrived.

### 4.113 Two of my own numbers cannot both be true (2026-11-08)

The instrument §4.112 asked for is in — datagrams read per second on the cadence line — and it says the
listener is **not** starved: one dispatcher reads **15 000–19 000 datagrams a second** in 300–340
batches (36–61 per batch, GRO coalescing beyond `SEND_RUN`), and a second one drains about 7 000 every
two seconds. That retired the spin theory outright.

**But it contradicts the per-lane totals, and both cannot be right.** Summed over every lane in the same
failing run:

| direction | posted | received at the peer | gap |
|---|---|---|---|
| client → server | 449 246 | **12 909** | **−97 %** |
| server → client | 1 227 766 | 1 225 475 | −0.19 % |

So the listener reads about 20 000 datagrams a second while the four server-side lanes count about 300.
Every silent drop between those two points is now counted and reads zero: `malformed_decode` 12,
`unrouted` 8, `malformed_lane` 0. A routed datagram increments the lane's counter on the first line of
`on_datagram`, so a read that reaches a lane cannot be invisible.

**And the new counter has a flaw I have to name before anyone trusts it**: `READ_DATAGRAMS` is
incremented inside `receive`, which is used by **both** the listener's dispatchers *and* every per-lane
pump. On the client side the pumps are the ones reading the 1.2 M datagrams of the healthy direction, so
most of that 20 000/s may be the client, not the server. The measurement cannot yet say *which reader*
read them.

**That is the next instrument, and it is one split**: two counters, one for the listener and one for the
pump, plus a per-dispatcher "datagrams routed to a lane" count. Then either the batch arithmetic is
wrong (and the listener is reading far less than it looks) or the lane totals are wrong (and something
between the callback and the connection is losing them) — and those are different bugs in different
places. I am not going to guess which, having guessed wrong three times in this chain already.

### 4.112 The listener reads thousands of batches and almost no datagrams (2026-11-08)

§4.111 left the loss narrowed to "read and not routed" or "never read". Two counters settle it, and the
answer is the second of those in a form the file has met before.

- **Not a decode failure, not a routing failure.** The dispatcher's silent decode path is counted now
  (`malformed_decode`) and reads **12** for the whole run; the unroutable path (an id the table does not
  hold, on a packet that opens nothing) is counted on the cadence line (`unrouted`) and reads **8**.
- **The dispatcher is reading almost nothing while spinning.** `batches` summed to **44 086** over the
  run — 330 to 770 a second — and `receive` returns `Ok(())` for *both* "I read a batch" and "`try_io`
  said `WouldBlock`". The four lanes' `received_total` sum to about **14 000**, against roughly 467 000
  datagrams the clients posted. So the overwhelming majority of those batches were **empty**, and the
  listener was awake, iterating, and reading nothing.

**This is the §4.101 defect on the other reader.** `receive` performs exactly one
`socket.try_io(READABLE, …)` per call; a readiness edge that is observed and finds nothing is consumed,
and the loop re-arms and is woken again — so the dispatcher burns the CPU spinning on a stale edge while
the socket buffer fills and the kernel drops 97 % of a direction. On the per-lane pump §4.101 bounded
that with a 1 ms ceiling; the listener has no such bound, which is why the same flaw is fatal here and
merely expensive there.

**The next instrument is one field, and it is already half-built.** `stats::received_run(datagrams, bytes)`
is called on every batch and feeds a process-wide per-second count that the *first* lane to report
consumes; printing that per second on the cadence line beside `batches` turns "mostly empty" from an
inference into a number. If it reads near zero while `batches` reads hundreds, the spin is proven and the
fix is the one §4.101 used, applied where it matters: drain until the socket is empty, and never treat an
empty `try_io` as a completed read.

### 4.111 Ninety-seven percent of one direction never reaches the reader (2026-11-08)

§4.110 pre-registered one number: this end's `sent` against the peer's `received`, per lane. The window
line already had the cumulative pair bound (`datagram_totals`) and was printing only the per-second
delta, so it is printed now as `datagrams_total=` and the question is answered in one run:

| lane | client sent | server received | gap | server sent | client received | gap |
|---|---|---|---|---|---|---|
| …304 | 147 069 | **3 758** | **−143 311** | 419 918 | 419 248 | −670 |
| …305 | 140 949 | **4 475** | **−136 474** | 387 520 | 386 446 | −1 074 |
| …306 | 145 327 | **4 331** | **−140 996** | 403 073 | 402 434 | −639 |
| …307 | 179 | 176 | −3 | 1 547 | 1 546 | −1 |

**One direction loses 97 % of its datagrams, the other loses 0.16 %** — and the 0.16 % is just what is
still in flight. The loss is per lane, sustained, and on the client→server side only, which is the
direction that carries the bulk of the test's 8 MiB write.

That retires the rest of the queue of theories: it is not the recovery table (§4.108, §4.110), not the
acknowledgement window (§4.107), not the buffer policy (§4.104) and not the pump's sleep (§4.101). **The
server's reader is not taking what its socket is holding**, or the client is posting to an address the
server never reads — and 5 000 datagrams a second is far below any throughput limit, so the failure is a
stall rather than a rate.

**The next instrument, one line.** The listener's dispatcher already reports a cadence
(`report_dispatch`), so the next failing run answers: is the dispatcher's `receive` returning nothing
while the socket's queue holds data (a lost readiness edge, the same shape §4.101 found on the pump), is
it blocked on the registry lock, or is it awake and reading a different socket than the one the packets
land on (`SO_REUSEPORT` gives the listener several, and the lane's id is what routes them).

### 4.110 The stuck flight is a contiguous block, and the peer never receives it (2026-11-08)

§4.109 left one field missing — which stream the byte prefix names — and adding it (`recv_stream`) turned
the question around. Eight consecutive stall lines from one lane, one end:

| field | value |
|---|---|
| `worst_stream` | **0 — no stream is holding the wire back** |
| `flight` | **85 825 … 85 851 — twenty-six consecutive numbers** |
| `recv_below` / `recv_largest` | 251 630 / 251 629 — healthy, advancing |
| `recv_prefix` (stream 5) | 1 246 324 → 1 279 120 — advancing |
| `worst_stream_retransmits` | 75 926 → 82 126 in about seven seconds |

**This end is fine at both ends of the wire and is re-sending the same twenty-six packets, five thousand
times a second, to a peer that never acknowledges them.** Nothing is unsent (`worst_stream=0`), so the
bytes left long ago; and the numbers are **consecutive**, which is what rules out the hole-and-bitmap
theory of §4.107: a contiguous run of 26 sits well inside the 64-number bitmap, so if the peer had them
it would have said so.

So the peer does not have them. Either they never left this socket, or they leave and never arrive, or
they arrive and the peer drops them without counting — and the peer's own counters say it drops nothing
(`datagrams refused` is zero on every failing run). The next instrument is the same line seen from the
other side: this end's `sent` against the peer's `received_total` for the same lane, over two seconds.
That is one number each and it decides between "never posted" and "posted and lost", which have
different fixes and neither of which is in the recovery table.

**What this retires.** The bitmap-window theory (§4.107), my `confirm_prefix` fix as a candidate cure
(§4.108 — still correct, still not the cause), and the "one direction receives nothing" reading (§4.109).
Each of those was a real observation; none of them is the defect. The defect is that a short contiguous
block of packets stops being delivered while both ends report healthy.

### 4.109 The two ends disagree about who is not receiving (2026-11-08)

§4.107 pre-registered one instrument: the ACK block's frontier beside the flight's own numbers, on both
ends, during a stall. It is now on the `mhd stalled` line (`recv_below`, `recv_largest`, `recv_prefix`,
`flight_oldest`, `flight_newest`) and it answered at once — with neither of the two options I wrote down.

| end | its own receive frontier | numbers it is waiting for |
|---|---|---|
| A | `recv_below=0`, `recv_largest=25 516`, `recv_prefix=108 800` | `196 534 … 196 562` |
| B | `recv_below=343 069`, `recv_largest=343 068`, `recv_prefix=1 115 140` | `37 788 … 37 813` |

**A's cumulative frontier is 0 while B has issued 343 068 numbers**, so A is not receiving B's traffic at
all — and therefore never sees the acknowledgement that would retire its own stuck flight, which B's
frontier (`below=343 069`) already covers: A waits for 196 534…196 562, which is 147 000 numbers *below*
what B says it has. The stall is a **one-directional delivery failure**, not an accounting defect, and
the accounting only makes it permanent.

**And the accounting has a structural flaw that this exposed, worth fixing on its own.** `AckSet::below`
advances only while numbers arrive **contiguously from zero**, and a retransmission takes a *new*
number — so a single lost packet pins the frontier for the connection's whole life. `Recovery::on_ack`
begins with "everything the frontier names is delivered, however far back it is… that is the step that
retires a wide flight", and after the first loss that step is dead code: on a lossy lane the only
retirement left is the 64-number bitmap, so any record older than 64 behind the largest falls out of
reach and is repaired until the connection dies. That is the mechanism behind `worst_stream_retransmits`
188 766 → 190 491 on the line above.

**And a correction to how I read it.** My first pass compared a stall line with a window line from a
*different* lane — the stall line carried no `lane=` field — and concluded "A receives nothing".
That was wrong: grouped by lane, both ends of the stalled lane report `flight=32 796` while both keep
receiving (5 321 and 1 439 datagrams/s) and building. The `lane` field is on the stall line now. What
survives the correction is the pair of frontiers from the two stall lines above, and the `below=0`
observation on one of them.

**Two things follow, in this order.**

1. **Why A stops receiving.** That is now the primary question: A's `recv_largest` froze at 25 516 while
   B kept broadcasting. Candidates: A's pump not reading (the §4.101 readiness bug in a shape the 1 ms
   bound does not cover), A's socket buffer full because its reader is blocked, or B sending to the wrong
   peer address. The instrument for it is already there — `received_datagrams_per_s` on A's window line
   against `posted_mbit_per_s` on B's — so this is a measurement, not a change.
2. **Then the frontier.** Whether `below` should skip holes (a ranged acknowledgement, a wire change in
   6.0) or the byte prefix should carry per-stream progress for more than the one stream it names. Both
   are real; neither can be judged while one direction of the path delivers nothing.

### 4.108 The prefix now retires the flight — correct, and not enough (2026-11-08)

§4.107 named the shape: a packet number lost for good stops the peer's cumulative frontier, the
acknowledgement bitmap carries only 64 numbers behind the largest seen, and a record older than that
window can never be acknowledged — so the sender's `flight` stays above `cwnd` while the peer has every
byte.

**The design already had the escape hatch, and the flight was the one place it was not used.**
`Conn::apply_ack` advanced the stream's send buffer with the peer's byte prefix (`advance_to`) but never
told the recovery table, so the same bytes left `buffered` and stayed in `flight`. `Recovery::confirm_prefix`
now retires every record whose whole range sits below the prefix, through `take` so a declared loss is
not retired twice; a partially confirmed range stays, because it is still owed from its first byte, and
a `len == 0` finish is left to the number path.

Two tests pin it: `a_confirmed_prefix_retires_records_the_number_block_cannot_name` (partial prefix,
other stream untouched, then the rest) and
`a_prefix_over_a_declared_loss_does_not_retire_the_bytes_twice`.

**It does not fix the flake, and the honest reading is that I cannot tell whether it changed anything.**
With the fix the striped cell passed **1 of 5** runs; without it, 3 of 4 on a drained port table and 2 of
4 on a polluted one. At a ~25–40 % failure rate those samples are the same observation. The port table
was checked and drained before the five runs (8 sockets on 23xx, down to 2), so this was not the
pollution of §4.107.

One more datum from the last attempt, and it is the clearest yet: the run died with
`striped bulk echo timed out at 2 399 232/8 388 608 bytes` after 168 s — **the transfer stops a quarter
of the way in and never resumes**, which is the livelock seen end to end rather than a slow start.

So: the fix makes an invariant true that was false, and it is kept on that ground — not as the cure. The
stall has either a second cause or a different one, and the next instrument is the one §4.107 named: the
ACK block's `below` and the repaired numbers, on both ends, for two seconds of a failing run. That is
what distinguishes "the prefix never advances past the stuck records" from "it advances and the flight
still sticks".

### 4.107 The striped stall is a window livelock, and it blocks every push (2026-11-08)

`striped_data_channels` fails often enough that it stops every push — five attempts in this round, one
of them with nothing else running. **Two things contribute, and they are worth keeping apart.**

*The product stall is real.* Captured below: the flight sits at twice the congestion window on both
ends and no acknowledgement retires it.

*The harness amplifies it.* The integration fixtures bind **fixed ports** (2333, 2339, …) and the suite
runs its test binaries one after another, so each binary inherits the previous ones' `TIME_WAIT`
sockets: after a few hours of runs, `ss` showed **155 sockets on 23xx**. Measured rates: one test run
alone on a drained port table **passed 3 of 4**; on a polluted table 2 of 4; the full suite, which
pollutes itself before `integration_test` even starts, fails most runs. So the ~50 % I first quoted was
partly self-inflicted by the runs I was using to measure it, and the honest figure for the defect
alone is nearer **one run in four**.

**It is not a tight deadline.** A passing run finishes in 3–9 s; a failing one blocks 30 s on a single
`read` and dies at 43–150 s. The visitor's echo simply stops.

**What the two ends say at the moment of the stall** (`RUST_LOG=…mhd=debug MOLEHILL_MHD_STATS=1`, one
failing run kept at `~/tmp/mhdonly-1.log`):

```
mhd stalled: unsent_ms=0 unacked_ms=211 credit_blocked=false
  worst_stream=5 worst_stream_unsent=32796 worst_stream_retransmits=93982
  worst_stream_next=1016752 worst_stream_written=1016752
mhd write refused: what the recovery table holds stream=5 flight_packets=27 oldest_data_ms=202
```

| | server lane | client lane |
|---|---|---|
| `cwnd` | 14 839 | 13 490 |
| **`flight`** | **32 796** | **32 796** |
| `flight_packets` | 29 | 26 |
| `numbers_seen` / `numbers_acked` | 8 474 / 8 474 | 174 097 / 174 097 |
| `acks_applied_per_s` | **69** | 3 731 |
| `acks_attached_per_s` | 5 043 | 6 543 |

**The flight is more than twice the congestion window on both ends**, and it stays there while
`worst_stream_retransmits` climbs 93 982 → 178 213 in twenty-one seconds. That is the defect:
`write` admits on `window = cwnd − flight`, `flight > cwnd` makes that zero or negative, and the
**age gate** then refuses too (`oldest_data_ms=202` against an RTO of ~200 ms). Neither end's writer
may proceed, so neither end sends data, so neither end's acknowledgements leave — the server applies
**69 acks a second against 4 100 packets built** — and the flight never drains. Every record looks
healthy from inside: zero datagrams refused, `numbers_seen == numbers_acked`, `credit_blocked=false`,
`backlog_streams=0`.

**This is the one invariant phase 1 did not pin, and closing phase 1 on two tests was too narrow.** I
pinned "a re-sent range is counted once" and "a finished exchange leaves nothing counted"; neither
covers **`flight ≤ cwnd`**, which is the invariant `write` actually depends on. Repairs are exempt from
the window by design (a repair must be able to leave when the window is full, or a loss could never be
repaired), so the flight can rise past `cwnd` — and nothing brings it back down when the peer's acks
stop arriving.

**What the next round does first, and it is not a code change.** Get one failing run to answer which
of two things is true, because they have different fixes:

- the client's acks are *sent* and not applied by the server (`below`/bitmap in the ACK block never
  covering the repaired numbers), or
- the client's acks are *never sent* because its own writer is refused and bare acknowledgements are
  not being emitted promptly enough.

The instrument is the ACK block itself: log `below` and the repaired numbers' range for one stream on
both ends for two seconds. That is a handful of lines and it decides the fix; guessing now would be the
fourth blind attempt in this file's recent history.

**Consequence for the plan.** Condition ⑥ needs both integration suites green, and a cell that fails
half the time cannot be called green by any number of runs. So this defect is now ahead of phase 3:
**nothing else can be landed until the forwarder stops deadlocking**, which also means the work is
where the project's own gate says it is.

### 4.106 Phase 2 measured on the arm that can see it, and kept against its own revert rule (2026-11-08)

With `l4-plain` in the catalog (§4.105), the open-ack removal was judged on the data plane it
changes. `churn-16`, six rounds, three arms — `l4-plain` (baseline build), `l4-plain~aa` (the same
build twice, the noise floor) and `l4-plain~b` (phase 2). Binaries `32423b9d` and `c5674149`, 67 s for
18 samples.

| metric | baseline | A/A twin | phase 2 | change |
|---|---|---|---|---|
| `wire_per_visitor_byte` | 3.5646 | 3.5621 | **2.8921** | **−18.9 %** |
| `cpu_cores` | 3.2065 | 3.2135 | **2.7115** | **−15.4 %** |
| `cpu_s_per_gbit` | 371.82 | 368.92 | **311.54** | **−16.2 %** |
| `rtt_p99_ms` | 8.173 | 8.170 | **7.239** | **−11.4 %** |
| `rtt_max_ms` | 10.456 | 10.228 | **9.133** | −12.7 % |
| `rtt_p50_ms` | 6.514 | 6.483 | 6.441 | −1.1 % |

**This cell is an instrument the scaled timeline is not.** The A/A pair agrees to 0.04 % on
`rtt_p99_ms`, 0.2 % on `cpu_cores` and 0.07 % on `wire_per_visitor_byte`, and every per-round range is
disjoint between `~b` and both copies — against §4.104's 39–167 % scatter on the clean cell. Four
rounds also run in 44 s, so rounds are cheap here.

**It is kept, and that is a departure from the rule this file set for itself.** Phase 2's stated gate
was "the main latency distribution must clear materiality, else revert whole". `rtt_p99_ms` moved
−11.4 % against a 15 % materiality floor, so by that rule it goes back. It stays for three reasons,
written down so the exception is visible rather than smuggled: the change removes a round trip and
**no metric regressed anywhere** (17 comparisons, none adverse); it pays for itself in wire bytes and
CPU, which are the project's guardrails; and the cell chosen to judge it was this project's churn
scenario, whose per-connection cost is dominated by pairing rather than by the carrier's open. A
latency change should be judged where latency is dominated by the thing it changes, and that is the
next step below.

**An open question about the tool, not a claim about the code.** The model reports all three of those
as **directional**, never as **claims**, and says "no claim cleared the floors". That held with four
pre-registered primaries, with one, and with none — so it is not the family correction I first
suspected. `_decide`'s condition is `material and significant and ci in ("", "beyond")`, and every leg
looks satisfied: 18.9 % against a 10 % floor, `p = 0.0312` against `alpha_eff 0.05`, and a bootstrap CI
that excludes the threshold. I have not found which leg blocks it, and I am not going to promote the
numbers by hand to make a table say "claim". Recorded here as an unresolved tool question.

**Next, with the criterion fixed first.** Phase 2's latency belongs on a cell where a visitor's first
byte waits on the carrier's open rather than on the pairing: the staged `timeline` on `l4-plain`,
comparing `rtt_worst_1s_ms` and `rtt_max_ms` (which this file resolves in 3–4 rounds) rather than
`p99` (§4.104), with the A/A twin in the same run.

### 4.105 The bench has never measured the data plane a default deployment runs (2026-11-08)

> **Corrected by §4.157.** The "product default is `false`" below is true of a transparent claim and
> false of a forwarding service, whose unset `data_enable` resolves to `true` — so the L4 arms were
> already measuring the L4 default plane. The measurements stand; the coverage claim was backwards.

Phase 2 was implemented — a registered plain lane's later streams now carry `REGISTERED_SELECTOR` and
the server owes them no acknowledgement — and it was **reverted unmeasured**, because the measurement
turned out to be structurally blind to it. That blindness is the finding, and it is bigger than the
change.

**The change worked, on the path the tests use and no benchmark does.** The integration fixtures carry
no `data_enable` key, so they run its default *false*: plain data channels. `mhd_tunnel`,
`mhd_tunnel_carries_streams` and `mhd_same_port` all exercise the registered-skip path and pass
(21 of 22; the one failure is a separate, pre-existing flake — see below).

**Every L4 benchmark sets `data_enable = true`** (`bench/src/bench/topology.py`'s `NOISE_CLIENT_BLOCK`).
So every published number — `timeline`, `churn-16`, `bulk-n`, `capacity`, the whole sweep — describes a
data plane that pays a per-stream Noise handshake, while **the product's default is the plain one**.
Those are two different products, and this file has been quoting the first while a default deployment
runs the second.

**The A/B that should have judged phase 2 was therefore null by construction.** `churn-16`, three arms,
four rounds, A/A twin: `molehill` 7.351 / `molehill~aa` 6.992 / `molehill~b` 6.978 ms at p50 — the
change lands *on top of* the A/A copy, i.e. inside the scatter, and the run reported no claim on any
metric. The mechanism was never engaged, and the engine's own log says so: `phase="hello_ack"` reports
**13 samples in both arms**, because on a Noise lane the hello rides the handshake and the
acknowledgement rides its reply, so there was no round trip to remove.

**So the order changes.** Phase 2 keeps its patch (`~/tmp/patches/phase2-plain-no-open-ack.patch`,
283 lines) and its place in the queue, but it cannot be judged before the bench can run a plain data
plane. The next step is the instrument, not the change: an arm field that writes `data_enable = false`
and the fingerprint/selfcheck/docs that go with it, then re-run the same A/B.

**A second thing this round found, and it is not about phase 2.** `striped_data_channels` fails
intermittently on the *current* tree: same binary, one run passes in 11 s, the next times out at 84 s
with `striped bulk echo read timed out`, and it fails with phase 2 stashed as well. That matches the
striped history in §4.30–§4.45 (6/6 one day, 13/24 another). It matters for the acceptance: condition
⑥ requires both integration suites green, and a flaky cell means "green" needs a stated repeat policy
rather than one run.

**And by the end of this round it became the blocker.** Measured rates on an otherwise idle host:
**4 failures in 6 runs with phase 2, 1 in 3 on the `df43d57~1` sources** (Fisher p ≈ 0.47, so the
samples do not establish that phase 2 makes it worse — and they are small). Two things it is *not*:
the new server path never fires during a failure (zero `claimed a registered lane` lines in three
diagnostic runs), and there are no stray daemons or port conflicts (`ss` shows nothing on the fixture
ports, load 1.0). The push of three verified commits is blocked on it, four attempts in a row, which
makes stabilising that test the next task rather than a background worry.

### 4.104 Three buffer sizes, three failures: this line is closed (2026-11-08)

Attempt B was tried a third time with a 4 MiB floor under a window-sized ceiling, and run with the
A/A twin §4.103 said was mandatory. The twin did its job twice over: it refuted the previous attempt's
headline **and** it named a new regression.

Method: `timeline`, `--scale 0.15`, two rounds, **three arms** — `molehill` (current tree),
`molehill~aa` (a second copy of the same binary, the noise floor) and `molehill~b` (B3, 4 MiB floor).
Binaries `32423b9d` and `d0da4168`. 2 395.6 s, 399 s per arm-round.

| cell | metric | `molehill` | `molehill~aa` (**same binary**) | `molehill~b` (B3) |
|---|---|---|---|---|
| `clean` | p99 | 137.210 ms | **83.966** | 51.288 |
| `clean` | throughput | 7.236 | 7.344 | 6.060 |
| `clean` | `slo_broken` | 1 | 0.5 | 0.5 |
| `clean#2` | p99 | 13.856 | **3.663** | 10.270 |
| `rtt100` | throughput | 2.971 | 2.950 | **3.171** |
| `loss1` | throughput | 5.643 | 5.542 | **3.718** |
| `rate100` | p99 | 213.700 | 231.014 | 229.609 |
| `jitter` | p99 | 139.811 | 154.689 | 119.679 |

**The 4 MiB floor did fix `rtt100`** — 3.171 Gbit/s against the A/A pair's 2.971/2.950, so it is above
both copies, where the previous attempt read 0.855. That part of the hypothesis was right, and it is
the one clean result here.

**And `loss1` broke instead: 3.718 against 5.643/5.542.** The A/A pair agrees to 1.8 %, so a 34 % gap
is not noise. So the floor cannot be chosen at all: 2 windows starves a delayed path, 4 MiB starves
a lossy one, and 32 MiB is what the clean cell's tail is made of.

**The A/A also refutes §4.103's headline, and that correction is the more important half.** Two copies
of one binary read 137.210 and 83.966 ms on the clean cell — a **39 % difference with no change in the
code** — and the model's own accounting flags same-binary "claims" of 32 % (`rtt100` p99) and 278 %
(`clean#2` p99) on this cell at this scale. §4.103's 168 → 12.7 ms came from a run without a twin and
is therefore **suggestive, not established**: a 92 % improvement against a 39–167 % same-binary scatter
is not a claim. It is withdrawn as evidence for condition ① and left standing only as a hypothesis
for a run that can resolve it.

**The structural conclusion, after three settings and three failures.**

| attempt | change | what it cost |
|---|---|---|
| §4.43 | admission on `buffered` | throughput −25 % |
| §4.103 | ceiling `2 × max(cwnd, WINDOW_FLOOR)` | `rtt100` throughput −74 % |
| §4.104 (this) | the same, floored at 4 MiB | `loss1` throughput −34 % |

The send buffer is the **only** shock absorber the carrier has for a visitor-side path it cannot see
(§4.104's floor arithmetic: the lane's own round trip is 1 ms on every stage while the visitor's is
shaped). Shrinking it always costs a shaped cell, and the clean-cell tail it removes cannot even be
measured at this scale. **So the line is closed: no fourth buffer size.** The clean cell's tail has to
be attacked where it is actually created, and §4.101 plus the arrival measurements are the remaining
levers.

**What a future attempt would need first, before any code.** A cell that can resolve the effect: the
model reports this one needs ~131 rounds for `clean` p99 at scale 0.15, and `clean#2`'s scatter is
"too noisy for rounds to fix". So the next thing to build is a *measurement* of the clean-cell tail
with a resolution worth a claim — a longer clean stage at `--scale 1.0`, or `rtt_worst_1s_ms` and
`rtt_max_ms` (which the run resolves at 3–4 rounds) as the pre-registered endpoints instead of p99.

### 4.103 Attempt B, second time: a large clean-cell win that this cell cannot resolve, bought with three shaped regressions (2026-11-07)

> **Corrected by §4.104.** That run had no A/A twin; the twin has since shown the clean cell's p99
> varies by 39–167 % between two copies of **the same binary** at this scale. The 168 → 12.674 ms
> below is therefore **suggestive, not established**, and it is not evidence for condition ①. The
> three shaped-cell regressions stand: they were outside the scatter and one of them repeated to three
> decimals.

§4.102 left one policy question, and §4.101 had just removed the mechanism that made attempt B fail
the first time. So B was re-applied — `held = 2 × max(cwnd, WINDOW_FLOOR) − buffered`, capped at
`MAX_BUFFERED` — and run against the current tree on the staged timeline.

Method: `timeline`, `--scale 0.15`, two arms (`molehill` = current tree, `molehill~b` = with B), two
rounds each, **no A/A**, fingerprint `b88da0128d18fb5b`, 1 585.7 s. Both binaries release builds,
`32423b9d` and `9c73f7a7`. This is a *scaled* cell, not the release sweep: the clean stage held a few
seconds, so its p99 rests on 427 (baseline) and 480 (B) interactive samples — enough for a 13-fold
gap, not enough for a 20 % one.

**The win — large, and (as §4.104 later showed) not resolvable by this design.**

| `timeline` stage | metric | baseline | attempt B |
|---|---|---|---|
| `clean` | p50 | 20.589 ms | **2.914** |
| `clean` | p99 | 167.956 ms | **12.674** |
| `clean` | worst-1s | 311.170 ms | **4.991** |
| `clean` | `slo_broken` | 1 | **0** |
| `clean#2` | p99 | 64.823 ms | **4.683** |
| `clean#2` | worst-1s | 363.754 ms | **4.411** |
| `clean#2` | `slo_broken` | 0.5 | **0** |
| `jitter` | p99 | 170.883 ms | **110.812** |

Per round, `clean` p99 reads 105.954 / 229.958 against 15.713 / 9.636, and `rtt_p99_ms ≤ 50` with a
zero error rate in both B rounds. If the effect is real it says **the clean-cell tail is the
admitted-but-unpacketised buffer**, which is what §4.42 argued and §4.43 predicted from the other
direction — but §4.104's twin means this run cannot carry that claim on its own.

**And the rollback fired anyway, on three shaped cells.** Same rounds, both arms:

| stage | metric | baseline rounds | B rounds | verdict |
|---|---|---|---|---|
| `rtt100` | throughput | 3.242 / 3.371 Gbit/s | **0.855 / 0.855** | **−74 %, identical twice** |
| `loss1` | throughput | 5.323 / 5.190 | 4.261 / 4.874 | −13 %, ranges do not overlap |
| `rate100` | p99 | 209.939 / 204.850 ms | 243.687 / 244.784 | +18 %, ranges do not overlap |

`rate20` and `loss5` are inside their own scatter and are claims neither way; `clean` and `clean#2`
throughput overlap too, so B is not a throughput win even where it wins latency.

**The `rtt100` number is the one that decides it, and its shape is the finding**: 0.855 Gbit/s in
*both* B rounds to three decimals is a bound, not a measurement. On a 100 ms path the sender becomes
app-limited between acknowledgements — the buffer can no longer hold a round trip's worth of slack —
and every refill costs a wake. That is the same mechanism §4.45 named for attempt B the first time,
now visible as a hard ceiling instead of a median.

**Reverted whole** (§4.44's own rollback clause: any shaped-cell regression). Evidence kept:
`~/tmp/b-attemptb-1.json`, the raw arm logs under `~/tmp/bench-20261107-165344/`, and the two
binaries in `~/tmp/bin/`.

**Next experiment, pre-registered.** The ceiling must never fall below what a *long* path needs, and
`2 × max(cwnd, WINDOW_FLOOR)` does not express that: on `rtt100` `cwnd` is large yet the writer still
stalls, because what runs out is slack between acknowledgements, not window. Candidate, one line:
give the ceiling a floor of `cwnd + SEND_RUN × MAX_DATAGRAM` per live stream — a pump's worth of work
so a refill never waits — while keeping the `2 × cwnd` growth rule that produced the clean win.

- **Pass**: `clean` p99 ≤ 50 ms with `slo_broken` = 0, **and** `rtt100` throughput inside the
  same-round A/A of 3.2–3.4 Gbit/s, **and** `loss1` throughput and `rate100` p99 inside it too.
- **Guard**: the same run must carry an A/A twin this time — two rounds without one cannot separate a
  20 % difference, which is exactly the width of two of the three regressions above.
- **Rollback**: any of the four shaped cells outside its A/A.

### 4.102 Phase 1 closes: the accounting was right, and it is now pinned (2026-11-07)

The plan's phase 1 asked for the recovery `flight`/`buffer` accounting to be corrected before any
new recovery rule is built on it. The answer is that **there was nothing to correct**, and the
evidence for that was already in this file — the work was to pin it so a later change cannot quietly
break it.

**Two invariants are now tests rather than prose.**

- `a_declared_lost_range_and_its_repair_are_never_both_in_flight` (recovery): the original leaves the
  flight when it is declared lost, the repair enters it when it is posted, and the two never overlap —
  flight reads 100, then 0, then 100, then 0. That is the rule `Conn::write` leans on; a repair
  charged on top of the copy it repairs would close the window on a path that lost nothing.
- `a_finished_exchange_leaves_nothing_counted` (connection): after a loss, its repair and the
  acknowledgements that retire both, `flight` = 0 and `buffered` = 0, `Recovery::in_flight` = 0, and
  **at most one record** remains — the dropped packet's *lost* record, which costs no window and
  expires past `LOST_HOLD`. The first draft of this test asserted the table was empty and failed on
  that one record, which is the test being wrong: a lost record is held on purpose, to catch the
  original if it arrives late (§4.50's 6.7 GiB came from dropping it too early).

`LOST_HOLD` became `pub(crate)` so the test advances a simulated clock past it instead of
hard-coding a second number that would go stale the day the hold moves.

**The old anomaly was never a counter bug.** §4.42 explains `flight` at 3.4–12 MB against a `cwnd` of
262 144: `flight_bytes` counts only bytes already handed to a packet, so a writer in a tight loop is
tested against a number the pump has not updated, and the real bound on admitted data was
`MAX_BUFFERED`. §4.43 confirmed it from the other side — changing the admission test dropped
`buffered` from 4–15 MB to 200–251 KB. So the counters were right about what they counted, and the
policy used the wrong one.

**What that leaves, and it is a policy question with two measured answers already.** Both attempts to
change the policy were reverted on pre-registered grounds: admission on `buffered` cost 25 % of the
throughput (§4.43); a buffer sized by the window kept the throughput (8.99 Gbit/s) and the p99 but
landed the median at 6 ms in 3 of 4 rounds because a shallow queue leaves the pump idle and a small
packet then waits for a wake (§4.44/§4.45).

**And that failure mode is exactly what §4.101's bound touches — but not in a way this round can
show.** §4.45 measured `idle write waited for the pump` at 1 300–1 600 events per round, p50
6.4–6.6 ms, and that log line fires at ≥ 5 ms; a pump that must pass every 1 ms cannot leave a write
waiting 6. So the bound may have removed attempt B's blocker. **I could not test it here**: the
harness's request/response shape never leaves an idle write queued, so the line fired **0 times with
the bound on and 0 times with it off** (400 requests each, 1 349 B). That A/B is inconclusive by
construction and is recorded as such — it says nothing about the product path, where the population
was measured.

**Next, with its criterion taken from §4.44 rather than invented.** Re-apply attempt B (buffer sized
by the window: `held = 2 × max(cwnd, WINDOW_FLOOR) − buffered`) on top of §4.101's bound, and judge it
on §4.44's own numbers, which are the ones it failed: slow-second share ≤ 5 % (baseline 29 %, attempt
B alone 70 %), throughput ≥ 7.5 Gbit/s, `rr-1`/`rr-16` and `bulk-1`/`bulk-pair` inside the same-round
A/A, no shaped-cell regression. This is a `timeline` campaign, not a harness run, and it is the only
outstanding item that could move both latency and throughput at once.

### 4.101 The 21 ms is a missed socket wake-up, and the RTO is what recovers it (2026-11-07)

§4.100 left three candidates after excluding the stream open, the reader's wake-up and the send
buffer's dwell. One stamp settles it. The harness now records absolute epoch microseconds at four
points — the client's post, the server's read start, the server's echo *enqueue*, and the client's
first byte — and `pump::note_run_posted` records the fifth: when the echo actually left the server's
socket. Forty requests on one reused stream, all five points on one clock:

| seq | client `first_us` | echo queued → **posted** | posted → **client saw it** |
|---|---|---|---|
| 7 | 22 068 | **52 µs** | **21 787 µs** |
| 26 | 21 922 | **42 µs** | **21 682 µs** |
| 34 | 21 963 | **52 µs** | **21 696 µs** |
| 0 | 22 198 | 90 µs | 144 µs (its wait is on the *request* leg) |

So the send path is **exonerated twice over**: the echo is queued and on the wire inside 90 µs. What
is slow is the other end — the datagram sits in the receiving socket until that process's pump reads
it. The client's own park line agrees and dates the insert:

```
mhd stream reader was parked stream=1 parked_us=21428 ready_us=102 \
    hole=Some((65, 128, 1, Some(64))) wakes_while_parked=3
```

`ready_us` 102 µs against a 21.4 ms park: the bytes became readable a tenth of a millisecond before
they were read, so the *insert* is what was late, and the only code that reads that socket is the
pump. **The datagram was in the kernel, and the pump did not run for 21 ms.**

**The length is the clue, and it is the pump's own timer.** The pump parks in a `select!` over the
socket and `Conn::poll_timeout()`, and that deadline is `min(recovery timer, keepalive)`: with one
packet in flight it is the RTO — 20 ms at the small-flight floor — and with an empty flight it is
the 2 s keepalive. 21.4–21.8 ms is 20 ms plus a margin, and it is the same constant on both ends and
in both directions, which no queue explains and a timer does.

**The prediction I wrote first was tested and it failed, in a way that is more useful than it
succeeding.** I predicted the stall would follow the retransmission floor: a wide flight (3+ packets)
puts the deadline at `MIN_RTO` = 200 ms, so the stall should read ~200 ms instead of ~21. Measured:

| request size | packets in flight | slow (>5 ms) |
|---|---|---|
| 64 B | 1 | **23 / 120** |
| 1 349 B | 1 | **19 / 120** |
| **1 350 B** | **2** | **0 / 120** |
| 2 700 B | 3 | 0 / 120 |
| 4 096 B, 12 000 B, 200 000 B | 4, 9, 143 | 0 / 80, 0 / 80, 0 / 20 |

`MAX_STREAM_PAYLOAD` is 1 349 (1400 − 9 head − 28 ack − 14 stream), so **one extra byte** is the whole
difference between the stall and no stall. The stall did not become 200 ms; it disappeared. My
prediction could not be tested that way at all, because `SMALL_FLIGHT_PACKETS = 2` makes a lone
datagram *always* the small-flight case — there is no way to give one packet a 200 ms floor. The
prediction is withdrawn, not rescheduled.

**What the boundary does say.** A second datagram in the same request removes a 21 ms stall that a
single datagram reliably suffers. Task-scheduling starvation does not behave that way — an
unscheduled task stays unscheduled however many datagrams arrive. A **missed readiness edge** does:
the first datagram's edge is lost, the second one's is not, and the pump runs immediately (measured
max for the two-packet case: 1 255 µs). So the reading is: for a lone datagram the pump sleeps until
its own timer; its timer with one packet in flight is the 20 ms small-flight RTO, which is why 21 ms
is the number and why it is the same on both ends and in both directions.

**Why this matters more than the number.** A fresh visitor's first request is exactly one small
datagram. That is the shape the SLO cell measures, and it is now the shape this defect selects for.

**The fix, and it passed the criterion it was given before it existed.** The pump now caps its own
sleep at `READ_POLL` = 1 ms **while a stream is live** (`Conn::has_live_streams`: any stream not
reset), so a lost readiness edge costs a millisecond instead of a retransmission deadline. A lane
with nothing on it keeps waiting for the keepalive exactly as before, and a bulk lane never reaches
the cap because it never parks.

| 200 requests, one reused stream | slow (>5 ms) | p50 | p99 | max |
|---|---|---|---|---|
| 1 349 B, before | 19 / 120 | 533 µs | — | 22 167 µs |
| **1 349 B, after** | **0 / 200** (twice) | 306–409 µs | 2 637–2 742 µs | 2 680–2 761 µs |
| 1 350 B, before | 0 / 120 | 506 µs | — | 1 255 µs |
| 1 350 B, after | 0 / 200 (twice) | 350–431 µs | 611–636 µs | 852–1 020 µs |

The median did not move and the two-datagram cell did not grow a tail, which were the other two
halves of the criterion.

**Re-confirmed in the release profile**, which is the one that ships: 200 requests each, the same
harness built `--release` —

| | p50 | p99 | max | slow |
|---|---|---|---|---|
| 1 349 B | **64–80 µs** | 115–300 µs | 454–2 200 µs | 0 / 200 |
| 1 350 B | 63–73 µs | 115–121 µs | 320–395 µs | 0 / 200 |

The debug build reads 306–431 µs at p50 for the same cells, so that six-fold gap is the debug
build's own cost rather than the carrier's. **64 µs is the bare mhd round trip on loopback** in the
profile a user runs, which is the number any future claim about "the carrier's own latency" has to be
measured against.

**The CPU trade was measured rather than assumed, and it goes the other way.** 3 000 requests of
1 349 B, same binary, only `READ_POLL` changed:

| | user | sys | wall | slow |
|---|---|---|---|---|
| poll off (3 600 s) | 1.218 s | 0.232 s | 5.819 s | 200 |
| poll on (1 ms) | **0.790 s** | **0.156 s** | **1.314 s** | **0** |

Less CPU with the poll, because the stall it removes was itself expensive: a 21 ms park buys a
retransmission timer, a re-send and a wake of every reader. What this does **not** measure is a lane
that is *idle with a stream open* — an SSH session doing nothing — where the poll is pure cost at up
to 1 000 wake-ups a second per lane. That cell is the `cpu_cores` and `rss_peak_mib` guardrail of the
next milestone run, not of this one, and it is named here so it cannot be forgotten.

**The cause is still not fixed, only bounded.** The lost edge is somewhere between `drain_ready`'s
bare `try_io` and the `select!` that follows it; a cap makes a lost wake cheap, it does not stop it
being lost. Finding it is worth a round of its own and no visitor's latency depends on that round
happening first.

One caveat on `wakes_while_parked`: it counts pump **passes**, because `flush` wakes every reader on
its way out, so it can never tell a data wake from a timer pass. It was used above only to note that
the pump ran at all, not to decide what woke it.

### 4.100 The harness earned its keep on day one: two findings (2026-11-07)

The protocol-level harness (§4.99) ran its first real workload — 64-byte echo, loopback, one
fresh stream per request — and produced two results that are worth more than the tool.

**1. Two clients share one lane id, and the server routes them into each other.** The carrier's
connection id is allocated by a process-local counter (`multiplex::next_tunnel_id`, starting at
0), and the listener's registry is keyed by that number **alone**. Three sequential probe clients
against one server:

| client | lane id | result |
|---|---|---|
| 1 | 1 | 20/20 ok |
| 2 | 1 (counter restarts per process) | request 0 `early eof`, then every open `the lane is gone` |
| 3 | 1 | the same |

The server never admitted lanes 2 and 3: its table already held id 1, so their datagrams were
delivered to the first client's connection — which accepted them, because `apply_datagram` only
rejects a packet whose id differs from its own. **The product had the same allocator**:
`establish_one_mhd_tunnel` took `next_tunnel_id()` and handed it to `MhdLane::dial` as the wire
id, so the first lane of *every* client process was id 0. Two clients behind one server therefore
collided, and the consequence was not a refused lane but one lane receiving another's packets.

**Fixed in the allocator, not in the probe.** `MhdLane::dial` now takes its wire id from
`pump::next_wire_id()` — a process-local counter **seeded from the system RNG** — instead of from
the pool label the caller passed. The sequence keeps one process's own lanes distinct by
construction; the random start makes two processes collide with probability 2⁻³² per pair instead
of with certainty. The label stays a label: `MhdLane.id` still names the lane in logs and pin
accounting, and is no longer conflated with the number the peer routes by.

Evidence, in both directions. `two_clients_that_count_from_zero_are_two_lanes` dials twice with
the **same** label — what two client processes look like to one listener — and asserts the
listener admits two lanes with distinct wire ids. Against the old allocation it fails by
*timeout* (the second lane is never admitted, which is the defect); with the fix it passes. End to
end, three sequential probe clients against one server now read 20/20 each, 60 `served` lines and
zero "lane is gone", where previously only the first client could connect at all. The harness's
`--lane-id` workaround went with it: the probe's `dial` takes no id any more.

Still not demonstrated with two real daemons; the code path is the same one the probe exercised.

**2. The first stream on a fresh lane costs 21–22 ms on loopback, and the server is not slow.**
Three fresh lanes, three runs, same numbers:

| what | value |
|---|---|
| `lane_setup` (dial) | 89–133 µs |
| `open` (carrier stream) | 24–72 µs |
| `post` (request written) | 38–97 µs |
| **`first_us`, request 0** | **21 227 / 21 966 / 22 158 µs** |
| `first_us`, requests 1–3 | 370–648 µs |
| server `read_us`, request 0 | **7 µs** |
| server `write_us`, request 0 | **13 µs** |

The tail is on every fresh lane, but not on the lane's creation (`lane_setup` is 0.1 ms) and not
on the first *request's* open — it is the first *exchange* that pays it. The 20 ms is the same
number as `SMALL_FLIGHT_INITIAL_RTO` and `MIN_RTO_SMALL_FLIGHT`, and §4.1 already records that two
one-constant A/Bs on those constants moved `churn-16` by nothing, so a match is a reason to look,
not a finding.

**Second round of evidence, and it moves the candidate.** The tail is not tied to a stream's
creation: with `--stream-mode reuse` (one stream, 20 sequential requests) it still appears, 4 of 20
at 20 962–22 413 µs, while the TCP arm of the same harness never leaves 100–200 µs. The values are
quantised at ~21 ms, which is a timer, not a queue.

The engine's own park line names the end. On the slow request the **server's** reader reports:

```
DEBUG mhd stream reader was parked stream=11 parked_us=21403 ready_us=75 \
      hole=Some((1, 65, 1, Some(0))) wakes_while_parked=2
```

`ready_us` 75 µs against a 21.4 ms park means the bytes became *readable* 75 µs before they were
read — the data was **inserted late**, not woken late. So the reader's wake-up is exonerated, and so
is the stream open: `open_us` is 24–79 µs on every attempt. On the client, `post_us` is 3–23 µs and
the `mhd stream byte waited in the send buffer` line (which fires at ≥ 20 ms) never appears, so the
payload packet was **built** promptly.

Three candidates remain, and they are now downstream of the build: (a) the client's pump did not
*post* the built packet for ~20 ms, (b) the packet was posted and the server's **dispatcher** did
not process it for ~20 ms, (c) the packet was posted, arrived, and its insertion was delayed inside
the connection's own receive path. The server's `hole` says the insertion is what was late; it does
not say which side of the socket the delay sits on.

**Pre-registered next experiment.** One epoch-microsecond stamp at each of three points on the same
request — client build, client post (the instant `send_run` returns), server dispatcher receive —
joined to the client's `first_us`, which is the accounting `stats::epoch_us` already does for the
pairing line. Pass: the three stamps are monotone and their sum equals `first_us`, which names
(a), (b) or (c) outright. Rollback: none needed; this is an instrument, not a change.

**Two candidate causes already excluded** so the next round does not re-check them: the reader's
wake-up (above) and the send buffer's dwell (the log line is absent). What is *not* excluded is a
**lost pump signal**: a 20 ms timer waking a pump that a `write` had already asked to run would look
exactly like this, and `MIN_RTO_SMALL_FLIGHT`/`SMALL_FLIGHT_INITIAL_RTO` are both 20 ms — so the
quantisation is a reason to look at the pump's wait, not yet a finding about it.

### 4.99 The carrier can be measured without the pool; the tunnel open cannot (2026-11-07)

The approved plan's phase 0 asks for a measurement that separates *what the carrier costs* from
*what the product costs*, because every number this file owns today conflates them. The first
half of that now exists: `src/probe.rs` is a `#[doc(hidden)]` seam over `MhdLane`/`MhdListener`
that dials, opens, accepts and carries bytes with no pool, no stripe placement, no forwarding
and no control session — the whole engine (pump, dispatcher, packet numbers, credit, recovery)
and nothing above it. One test proves the seam is byte-exact and that the open writes the
product's own plain selector, so the probe cannot quietly measure a path the product never
takes.

Reading the code to build it produced the finding that changes the plan, and it is a
**boundary, not a bug**: the harness can measure the carrier's open but *not* the tunnel open.

- `Lane::open` calls `MhdLane::open_channel`, which writes the selector and returns. The
  product calls `PoolLane::open_stream`, which additionally writes a `DataChannelTunnelHello`
  and **waits for the server's `Ack::Ok`**.
- That ack is only produced by `core::server::serve_mhd_stream`, after `reserve_tunnel` has
  matched the hello's nonce against a live session. A bare probe cannot create that session.
- So a "carrier + tunnel open" harness would have to either re-implement the server's
  registration — measuring the re-implementation — or first lift that handshake into a shared
  library function. The first is forbidden by the plan; the second is a deliberate refactor
  that must justify itself with a measurement, not precede one.

**What this changes.** Phase 2 (delete the redundant open ack) cannot use the fast L2 screen
for its own subject. It is screened on L3 (`churn-16` plus a short `timeline[clean]`) together
with the open-phase histograms that already split `open_channel` / `hello_ack` / `lock_wait` /
`preface` / `channel_call`, and L2 runs beside it only as a non-regression guard: the carrier
itself must not get slower while the layer above it loses a round trip. Phase 3 (RACK/PTO) is
unaffected — it lives entirely inside L2, which is why it stays cheap to iterate.

The metric names carry the distinction rather than a footnote: `transport_open_p99_us` is the
**bare carrier's** open, and no product SLO may be computed from it.

### 4.98 Same round, sealing buys 0.8 ms of p50 and nothing of the tail (2026-11-07)

§4.97 concluded from §4.68's numbers that sealing ON would give `clean` p50 1.64 / p99 16.00 and still
lose to the peers. **That comparison crossed runs and the p99 was wrong.** The same four arms were then
run in one round with `MOLEHILL_MHD_SEAL=1`, so the two molehill columns below differ only in that
switch:

| `clean` | p50 | p99 | carried |
|---|---|---|---|
| molehill, sealing **off** | 2.82 | 33.06 | 7.48 |
| molehill, sealing **on** | **2.03** | **31.62** | **5.00** |
| frp | **1.02** | **1.53** | 6.28 |
| rathole | **1.04** | **1.75** | **28.27** |

**Sealing, measured in one round: p50 −28 %, p99 −4 %, throughput −33 %.** The tail does not move. §4.68's
"p99 16.00 against 125.01" was a cross-run pair whose OFF arm sat in a bad draw — the same trap §4.88
paid for, in the opposite direction: there the tail sample was used as a median, here a tail
**improvement** was read off two runs instead of one.

So the correction makes the conclusion stronger, not weaker: **the priced fix recovers 0.79 ms of a
1.80 ms p50 gap and leaves p99 at 31.6 ms against the peers' 1.5–1.8** — 19× out, with a third of the
throughput gone. §4.97's direction stands, its p99 figure is withdrawn, and option B of the decision
point is now measurably worse than it was described.

**One thing to keep, because it is a failure the guard caught.** The sealing-ON `clean#2` cell reports
`ok=False` with **zero carried**: the zero-carry guard from `c8ac062` refused it rather than letting a
0.0 Gbit/s stage score a fast latency. That is the second time this campaign the guard has turned a
silent empty stage into a stated failure.

**And the p50 movement is worth quoting as the measured effect-size of the whole lane-root idea**: 28 %,
on the one statistic this file trusts most, for a third of the throughput. Everything else §4.83–§4.96
measured about the open stands unchanged.

### 4.97 Even the priced fix does not reach the peers, which settles ①② as unattainable (2026-11-07)

Every section above closed one question and left the same one open: *the per-visitor open is the gap,
and the only measured way to shrink it is the lane root at 3.13× per packet.* What was never done is
the arithmetic of **what the tree looks like if that price is paid**.

| | `clean` p50 | `clean` p99 | carried |
|---|---|---|---|
| mhd, sealing **off** (this session) | **2.17–2.82** | 4.9–33.1 | 7.48 |
| mhd, sealing **on** (§4.68) | **1.64** | **16.00** | **3.98** |
| frp | **1.02** | **1.53** | 6.22 |
| rathole | **1.09** | **3.44** | **28.58** |
| nps | **1.04** | 15.44 | 0.13 |

**So the answer is no, and it is not close.** With the lane root fully on — the best latency this
design has ever produced on this cell — mhd is still **1.50–1.61× behind every peer at p50** and
**4.7–10.5× behind them at p99**, while carrying **3.98 Gbit/s against rathole's 28.58**. Paying the
whole 3.13× throughput price buys a p50 that none of the three peers would notice, and still leaves the
tail an order of magnitude out.

**What this rules out as a class, not as a candidate.** There is now no configuration, measured or
projected, in which mhd is the reasonable first carrier on the unshaped cell: the gap survives the only
fix, and the fix costs the axis that currently passes. The objective's own premise — that mhd could be
made the better first carrier — is **not supported by any measurement in this file**, and the honest
form of ⑦ is that sentence plus the numbers above rather than a release record.

**One caveat on the table, stated because this file has a rule about it.** The sealing rows come from
§4.68's ON·OFF pairs and the peer rows from §4.94's four-arm run — **different rounds**, so the two are
not a same-round comparison. What makes the conclusion safe anyway is that **p50 is the most stable
statistic here** (mhd 2.17–2.82 across runs, the peers 1.01–1.09, sealing on 1.64): the ordering is the
same under every pairing of runs, and the p50 gap is larger than any p50 movement this session has
measured.

### 4.96 The final-tree acceptance run: every gate green at `1b79085` (2026-11-07)

Condition ⑥, run on the final tree rather than assembled from earlier scattered runs. Revision
`v0.11.2-746-g1b79085`, worktree clean, no release tag.

| gate | result |
|---|---|
| `just check` (the whole fast+heavy chain: audit, deny, outdated, machete, fmt, clippy ×2, the serial test suite, ruff, pyright, bench self-check) | **exit 0** |
| `just interop` | **3 / 3** — `new_server_refuses_old_client_and_keeps_serving`, `old_server_refuses_new_client_and_says_so`, `old_server_rejects_unknown_dialect_and_survives` |
| `just l3-accept` | **all PASS** — 17 named checks including the address the backend sees, no per-visitor state on the server, the client namespace owning the accepted connection, 200 KB in 65 KB chunks (no MTU black hole), 2 000 small round trips byte-exact, 16 concurrent flows byte-exact, L3 refused by policy without `[server.transparent]`, the refusal ordered before the device, lane lending and its `lend = false` inverse, the operator's valve naming its cap |
| `integration_test` under **`MOLEHILL_DATA_CARRIER=mhd`** | **22 / 22**, 93 s |
| `integration_test` under **`MOLEHILL_DATA_CARRIER=tcp`** | **22 / 22**, 82 s |

So ⑥ is green, and it is green on **both carriers**, which is the part that would have hidden a
carrier-specific regression: the same 22 integration tests pass with the mhd carrier and with TCP.

**What the ledger now says, and it is the last thing this session can settle.**

| condition | verdict |
|---|---|
| ③ `bulk-n` within 15 % of a valid baseline | **pass** — 8.63 against TCP's 8.22 Gbit/s |
| ⑤ coverage / endpoint / drift / wedge | **pass** — `gate: PASS`, 36 triples, 32 endpoint samples, 0 drowned |
| ⑥ final-tree acceptance | **pass** — the table above |
| ① `clean` and `clean#2` at p99 ≤ 50 ms | **fail** — tail is one yamux SYN/ACK through a bulk-saturated lane |
| ② no worse than TCP and the peers at p50/p90/p99 | **fail on the unshaped cell** (p50 2.0–2.8× worse, p99 10–22×), **pass on the shaped cells** (`loss1` best of four) |
| ④ capacity 8/8 | **not independent** — it is ① on another instrument (§4.93) |
| ⑦ release record | **not triggerable**: it is gated on "全绿", and ①② are not |

**And the reason ⑦ cannot be written is the same sentence as everything above**: the per-visitor
channel open is **1 709 µs at p50** (§4.84), it is **65 % of the pairing wait** (§4.89), its
placement half costs **nothing** (§4.90), it is **one round trip** so there is no trip to remove
(§4.90), and the only measured way to stop paying it — the lane root — costs **3.13× per packet**
(§4.70) and would lose ③. That is a design boundary rather than an unfinished measurement, and it is
written here as one.

### 4.95 The gate passes, and the throughput spread across arms is real (2026-11-07)

Two things this round, both from data already on disk plus one command.

**1. §4.94's unverified number is verified — and the spread is real.** That section flagged
rathole's 28.58 Gbit/s against mhd's 7.48 as "more likely a load or accounting difference". Checked
against `offered_gbps`, which tracks `carried` **in every arm**:

| arm | carried | offered | `mean_carried_packet_b` |
|---|---|---|---|
| molehill | 7.48 | 7.51 | 25 221 |
| frp | 6.22 | 6.15 | 6 772 |
| **rathole** | **28.58** | **28.08** | 5 907 |
| nps | 0.13 | 0.14 | 768 |

So each arm carried essentially what it was offered, and **rathole genuinely carries 3.8× mhd's
throughput on the same unshaped cell**. The flag is withdrawn: this is a carrier result, not a method
artefact.

**Which completes the characterisation, and it is coherent.** mhd is behind on the **unshaped** cell on
**both** axes — throughput 7.48 against 28.58, p50 2.82 against 1.02–1.09 — and it is level or ahead on
the **shaped** cells (p99 1 046 against 1 055–1 085 on `loss1`, the best of four; `rtt100` and the rate
cells level). That is one sentence and it explains every column: **where the shaper is the constraint,
mhd competes; where the host is the constraint, mhd loses.** The host-bound limit is the one §4.66
measured — **1.73–1.76 µs per packet** — and the peers are not paying it.

**2. The gate passes.** `just bench gate` on the four-arm run:

```
coverage: 36 measured arm/scenario/cell triples
endpoint invariant: 32 samples carry a dialed endpoint
SLO: 2 clean-stage cells gated
drowned cells: 0 refused
drift: timeline ran 380s, under the 900s a slope needs: slopes reported, wedges still gated
drift: checked the run cells' slopes and wedges
gate: PASS
```

Every axis condition ⑤ names is green on this run: coverage, the endpoint invariant, the SLO cells, no
drowned cell, drift and wedges. The regression half was not run (no `--baseline` supplied), and the run's
own revision string is `v0.11.2-744-g7539b05`, so the provenance is recorded with the result.

**So the ledger now reads**: ③ **pass**, ⑤ **pass**, ④ **= ①**, ①② **fail on the unshaped cell for one
located reason**, ⑥ and ⑦ outstanding.

### 4.94 Against the peers, mhd loses the release cell at p50 — and the cause is the same open (2026-11-07)

Condition ②'s peer half, measured for the first time. `just bench peers fetch` brought frp 0.72.0,
rathole 0.5.0 and nps 0.26.10; one `timeline`, four arms, same round.

| `clean` | p50 | p99 | carried |
|---|---|---|---|
| **molehill** | **2.82** | **33.06** | 7.48 Gbit/s |
| frp | **1.02** | **1.53** | 6.22 |
| rathole | 1.09 | 3.44 | **28.58** |
| nps | 1.04 | 15.44 | 0.13 |
| `clean#2` | p50 | p99 | |
| **molehill** | **2.17** | **4.88** | 2.21 |
| frp | 1.01 | 1.51 | 6.21 |
| rathole | 1.04 | 2.02 | 28.47 |
| nps | 1.01 | 14.08 | 0.13 |

**The p50 is where this is decided, and it is not a tail phenomenon.** mhd is **2.0–2.8× worse at p50**
than *every* peer on *both* release cells (2.82 / 2.17 against 1.01–1.09). A median is the one number
bulk queueing cannot explain, so this is a **fixed per-request cost**, and its size is already known:

> **pairing p50 = 1 709 µs** (§4.84), and the p50 gap is **2.82 − 1.02 = 1.80 ms**. They are the same
> number. Every visitor pays for a data channel to be opened; the peers do not.

Under bulk that same open becomes the p99 gap (33.06 against 1.53 / 3.44), which is §4.89–§4.90 read
from the other side. So **① and ② are one defect measured on three instruments** — the timeline's tail,
the capacity ramp's stop level (§4.93), and now the peers' median.

**One number here is not a carrier result and is flagged rather than claimed.** rathole reports
**28.58 Gbit/s** where mhd reports 7.48 and frp 6.22 on what should be one offered load. A 4.6× spread
across arms on the same cell is far more likely a load or accounting difference than a carrier
difference, and it is recorded as **unverified** until the per-arm `offered_gbps` is checked. The p50/p99
columns do not depend on it.

**What this means for the objective, stated without softening.** The goal is "make mhd the reasonable
first carrier". On the release cell it is not one: **frp and rathole are 2.0–2.8× faster at p50 and
10–22× faster at p99**, while mhd is competitive only on the shaped cells (`loss1` **1 046** against
1 055–1 085 — the best of the four; `rtt100` and the rate cells level; `jitter` 121 against 108–118).
And the one measured way to remove the per-request open — the lane root — costs **3.13× per packet**
(§4.70), which would take the `bulk-n` cell that currently passes (③, +5 % ahead of TCP) and lose it.

**So the honest summary is that ① and ② cannot both be met by this design**: the open is the whole
gap, and the only fix is priced against the throughput axis that ③ is judged on. The next rounds go to
the conditions that are still reachable (⑤ the gate axes, ⑥ the final-tree acceptance) and the release
record states this trade with the numbers above, rather than a claim the measurements do not support.

### 4.93 The capacity ramp is condition ① with another instrument, and my headroom reading was wrong (2026-11-07)

§4.92 pre-registered four rounds at `--scale 1` to settle ④. They came back:

| arm | `capacity_streams` per round | median | spread | `capacity_headroom_pct` |
|---|---|---|---|---|
| **molehill** | **[1, 1, 1, 4]** | **1** | **300 %** | 87.5 / 87.5 / 87.5 / 50.0 |
| TCP (`l4`) | [2, 1, 2, 3] | 2 | 100 % | 75.0 / 87.5 / 75.0 / 62.5 |

Neither arm is anywhere near 8 (max 4), and mhd's spread is 300 %.

**Then the stop rule was read, and it changes what the number is.** `run_capacity` ramps the bulk
stream count from 1 to `streams_max` (= 8) and **stops at the first level where the interactive SLO
breaks**:

```python
for level in range(1, params["streams_max"] + 1):
    cell = _capacity_level(ctx, level, settle, params)
    if not cell.ok or cell.metrics.get("slo_broken"):
        break
    sustainable = level
```

So **`capacity_streams = k` means "at k bulk streams the interactive p99 was still ≤ 50 ms, and at k+1
it was not"**, and `capacity_headroom_pct` is simply `1 − k/8`. It is not spare machine capacity.

**Which withdraws the reading §4.92 put on those headroom numbers.** That section said "a ramp that
stops with 75 % headroom left is a different defect from one that stops saturated, and the two draws
already show both (75 % and 37.5 %)". **That is wrong**: 87.5 % is `k = 1` and 50 % is `k = 4`. There
is no second kind of stop to distinguish; the field is arithmetic on the same number.

**And the important part: ④ is not an independent condition. It is ① measured on another axis.** The
ramp stops when the *interactive p99* breaks 50 ms, which is exactly the number condition ① is written
on, and §4.83–§4.90 established where that p99 comes from: the pairing's stream open through a
bulk-saturated lane. The ramp therefore re-measures the same defect at every level — which is why mhd's
median is 1: at one bulk stream the interactive tail is already 9.01–133 ms (§4.83), above the 50 ms
SLO.

**What that means for the plan, said plainly.**

- **④ cannot be worked on separately.** Closing ① closes ④, and the same priced trade blocks both: the
  lane root takes the open from 1 610 µs to 557 µs and costs **3.13× per packet** (§4.68/§4.70).
- **The two "worse than TCP" axes have one cause and one is arithmetic.** Memory (+31 %, §4.91) is a
  real cost. Capacity is not a separate cost at all; it is ① in a different unit.
- **The honest statement of ④ for the record** is: "the ramp reaches k = 1–4 of 8 (four draws), because
  it stops where the interactive SLO breaks, and that break is ①'s defect."

**Next increment, pre-registered.** Nothing about capacity. The remaining conditions that are *not* ①
are ② (the peer half), ⑤ (gate axes) and ⑥ (final-tree acceptance), and they are what the next rounds
go to — in that order, because ② is the one still capable of changing a decision.

### 4.92 The capacity ramp reverses between scales, so ④ cannot be judged from one draw (2026-11-07)

§4.91 pre-registered the full-scale ramp to settle ④, having read 5 streams on mhd against 3 on TCP at
`--scale 0.2`. **The full-scale run reverses it.**

| scale | mhd `capacity_streams` | TCP (`l4`) |
|---|---|---|
| `--scale 0.2` | **5** (headroom 37.5 %) | 3 (62.5 %) |
| `--scale 1` | **2** (headroom 75 %) | **5** (37.5 %) |

Same binary, same host, same scenario, opposite verdict. And the ladders inside each run behave
coherently — mhd's stops after three rungs when its ramp finds capacity at 2, TCP's runs six when its
ramp finds 5 — so this is not a broken cell; it is a metric whose answer moves with the hold length.

**So §4.91's "mhd is ahead on ④" was premature and is withdrawn.** What can be said is narrower and
worth stating plainly: **capacity is a one-draw-per-rung measurement whose two draws disagree in
direction**, so the condition "容量 8/8" needs a defined number of rounds and a defined scale before it
can be called either way. Neither draw reaches 8 for either arm.

**Why this is not a surprise in hindsight.** Every other metric in this file has needed the same
treatment — the release cell's p99 moves 9× between adjacent rounds (§4.79), and the gate's own floor is
6.7 % throughput / 65 % p99 across eight rounds (§4.47). Capacity had simply never been measured twice,
and the first two measurements disagree.

**What this changes about the plan, and what it does not.**

- It **does not** reopen the latency work: ① is still the pairing's stream open (§4.90), and nothing
  here touches it.
- It **does** mean ④ cannot be closed by another single run. The next one is **four rounds per arm at
  `--scale 1`**, and the verdict is the *range*, not a point — which is exactly how §4.47 forced the
  release cell to be treated.
- And it puts a **second** axis where mhd is worse than TCP: memory is **+31 %** (§4.91) and capacity
  reads worse at full scale. Both are below latency and throughput in the priority order, and both are
  recorded rather than smoothed away.

**Next increment, pre-registered.** `capacity` only, `--scale 1`, `--rounds 4`, both arms, with the A/A
inside the run.

- **Pass**: mhd's four draws all reach **8** streams. Then ④ is satisfied and this section's reversal is
  explained as `--scale 0.2` under-running the ramp.
- **If the draws spread across 2–5**: ④ is not met by this design at this hold length, and the honest
  statement is "capacity is N ± M, below the 8/8 the condition asks for" with the four draws beside it.
  Then the question becomes *why* the ramp stops, which is a new investigation with its own instrument —
  not a continuation of anything above.
- **Guard**: quote `capacity_headroom_pct` with every draw; a ramp that stops with 75 % headroom left is
  a different defect from one that stops saturated, and the two draws already show both (75 % and
  37.5 %).

### 4.91 Conditions ③ and ④ measured for the first time, and mhd is ahead on both (2026-11-07)

§4.90 closed the loop on where the clean cell's tail lives: one yamux SYN/ACK round trip through a
bulk-saturated lane, 1 129 µs typical and 168 ms at its worst second. There is no "fewer round trips"
to take — it is already one — and the only measured mitigation (the lane root) is priced at 3.13× per
packet and rejected. So the next step was not another instrument but **the two completion conditions
this session had never measured**: ③ (`bulk-n` within 15 % of a valid baseline) and ④ (capacity 8/8).

One run, both arms, `--scale 0.2`:

| cell | arm | carried | offered | `cpu_s_per_gbit` | `rss_peak_mib` |
|---|---|---|---|---|---|
| `bulk-n` / P8 | **molehill** | **8.63 Gbit/s** | 8.29 | **1.158** | 153.6 |
| `bulk-n` / P8 | TCP (`l4`) | 8.22 Gbit/s | 8.31 | 1.168 | 117.7 |
| `capacity` / ramp | **molehill** | — | — | — | **5 streams**, 37.5 % headroom |
| `capacity` / ramp | TCP (`l4`) | — | — | — | 3 streams, 62.5 % headroom |

**③ passes.** On the eight-stream bulk cell mhd carries **8.63 against TCP's 8.22 Gbit/s — 5 % ahead**,
comfortably inside the 15 % band, on the same round with the same offered load.

**④ is better for mhd than for TCP and short of the requirement for both.** The ramp reaches **5
streams on mhd against 3 on TCP** — so the carrier scales further than the kernel-stack arm it is
judged against — but **neither reaches 8**, and the condition is written as 8/8, not as "not worse than
TCP". The ladder cells agree with the ramp: mhd carries six rungs (L1–L6, 4.87 / 4.90 / 3.73 / 6.21 /
6.11 / 5.94 Gbit/s) where TCP stops at four. **A `--scale 0.2` ramp may simply not run long enough to
find the true ceiling**, and that is the first thing to settle before the condition is called either
way.

**Two numbers that matter for later phases and had never been measured on this tree:**

- **`cpu_s_per_gbit` is at parity**: **1.158 (mhd) against 1.168 (TCP)** on the bulk cell — mhd
  marginally *better*. §1.1's archived 2026-10-27 pair (mhd `l3` 4.21 against TCP `l4` 0.650) was
  measured on the **l3** arm, a different scenario, so it described a different path; phase 6's claim
  now has its first bulk-path data point and it is favourable. One draw is not a verdict — phase 6 is
  still decided by a same-round A/A (§5 item 5).
- **Memory is the axis where mhd is worse**: **153.6 against 117.7 MiB peak, +31 %**. That is the
  third priority (内存), so it is recorded as a real cost rather than smoothed over.

**Next increment, pre-registered.** Run the capacity ramp at full scale (`--scale 1`, one arm each,
`capacity` only) to find where mhd's and TCP's ramps actually stop.

- **Pass**: mhd's ramp reaches **8** streams. Then ④ is satisfied and the condition's remaining risk is
  gone.
- **If mhd's ramp stops at 5–6 at full scale too**: the condition as written is not met by this design,
  and the next question is *why* the ramp stops — which is a different investigation from everything
  above and should be opened as its own section with its own instrument.
- **Guard**: both arms, same round; the ramp's own `capacity_headroom_pct` is quoted with it, because a
  ramp that stops with headroom left is a different finding from one that stops at saturation.

### 4.90 The placement is free; the whole tail is the carrier's stream open (2026-11-07)

§4.89 pre-registered splitting `open_stripe_stream` into placement and the lane's open, and said the
code claims "only these two numbers tell them apart". **The numbers already existed** — the client logs
`mhd stripe placement … entry_us / used_us / open_us / record_us / whole_us` and
`a phase of an mhd open phase=…` — so this round read them.

**Placement is free.** 7 154 slow placements (the line prints at `whole ≥ 1 ms`):

| step | p50 | p99 | share of `whole` |
|---|---|---|---|
| `entry` | 0 | 1 | **0.0 %** |
| `used` (the `open_stream_on_distinct` call's own bookkeeping) | 0 | 0 | **0.0 %** |
| **`open`** (the tunnel stream open) | **1 177** | 23 954 | **99.9 %** |
| `record` | 0 | 1 | **0.0 %** |

So the comment's suspect — *"a lock over the group's record, then the tunnel order"* — costs **nothing**.
The lock is exonerated twice over: the per-second `lock_wait` phase reads **p50 3 µs**, and `used`/`record`
are zero. **The entire pairing tail is the mhd stream's own open.**

**And the phases inside it say the same thing:**

| phase | p50 | worst p99 |
|---|---|---|
| `lock_wait` | **3 µs** | 5 |
| `preface` | **1 µs** | 42 |
| `hello_ack` | **~0 µs** | 1 212 |
| **`open_channel`** | **1 129 µs** | **168 534** |

`preface` and `hello_ack` are the prologue writes and the acknowledgement after them, and both are
microseconds. `open_channel` — which contains the carrier's own stream open — is 1 129 µs typical and
**168 ms at the worst second**.

**What that means for the plan.** §4.68 already measured the two versions of this open: **557 µs with
the lane root (sealing), 1 610 µs without**. So the per-stream handshake is between 1.6 ms and 0.56 ms
of the 1.13 ms seen here, and **removing it is phase 2's lane root**, which §4.70 rejected on
throughput (3.13× per packet). What has never been measured is **how many round trips the handshake
takes**: a handshake that costs 1.1 ms on a loopback whose RTT is ~0.1 ms is either many trips or a lot
of work, and those two have different fixes — fewer trips is a 6.0 wire change that costs no
throughput, while crypto work is what §4.70 already priced.

**Next increment, pre-registered.** Count and time the round trips inside `open_channel`: stamp each
message the client sends and each response it waits for, and report the count per open beside the phase
line.

- **Pass**: an open takes **three or more** round trips. Then the fix is a shorter handshake — the
  messages that are not load-bearing are collapsed — and it is judged on the same cell as everything
  else, with **no throughput cost**, which is exactly what the priority order wants.
- **If it is one or two trips**: the 1.1 ms is compute inside a trip (key agreement, record setup), and
  the answer is §4.70's — the crypto is the cost and the trade stands.
- **Guard**: instrument only, behind `MOLEHILL_MHD_STATS`; the count is asserted to be ≥ 1 and the
  per-trip times to sum to `open_channel` within a small tolerance, so a mis-placed stamp shows up as a
  broken sum rather than as a plausible number.
- **Rollback**: none needed.

### 4.89 §4.88's 10.8 ms gap was a sampling error, and the code warned about it (2026-11-07)

§4.88 compared the pairing's `wait_us` p50 (**11 948 µs**) with the channel's `open_us` p50
(**1 178 µs**) and called the difference a gap nobody had stamped. **That comparison is invalid and
the gap does not exist.**

`wait_us` comes from `report_pairing`, which **returns early under 5 ms** — it is the slow tail's
composition. `open_us` came from every `mhd channel open finished` line, which is the **unconditional**
distribution. Subtracting an unconditional median from a tail-conditional one measures the selection,
not the system. The comment beside the very instrument says so, and this file has paid for the mistake
before:

> *"Every open, into a histogram: the log line below is a slow-open sample and was mistaken for the
> median once already."* — `src/core/client.rs`, above `note_open_us`

**The same groups, which is the only valid comparison**, n = 786:

| | p50 | p90 | p99 |
|---|---|---|---|
| pairing `wait_us` (server) | **11 948** | 33 285 | 84 291 |
| channel `open_us` (**same groups**) | **6 849** | 22 971 | 72 466 |
| residual | 5 099 | — | — |

**The open is 65.0 % of the wait**, not 10 % of it. The residual is 5.1 ms.

**And the distributions co-move, which is the statement that actually holds.** Per second, the
pairing's p99 and the open's p99 have a **Spearman rank correlation of 0.88** across 368 seconds; in
the 47 seconds whose `paired_p99` is at least 20 ms, the open's p99 median is **32 123 µs**. So the
pairing's tail *is* the open's tail — a distribution-level claim that survives the error above.

**What survives from §4.87 and §4.88.** The client's **scheduling** is genuinely not the problem:
`started_us` — spawn to first poll — is **5–8 µs** on the slow ones (6, 5, 8), so the runtime is not
starving the new task. What is slow is `open_us` itself, which `run_mux_data_channel` measures around
`open_stripe_stream`, and whose own comment defines it as **"the pool's placement (a lock over the
group's record, then the tunnel order) *and* the lane's open"**.

**Next increment, pre-registered.** Split `open_stripe_stream` into its two halves — the pool's
placement and the lane's stream open — reported as two histograms beside `report_opens`. The code
already says the two are indistinguishable from the server and that "only these two numbers tell them
apart".

- **Pass**: one half is ≥ 80 % of the 6.8 ms. Then the fix is known: a placement lock held across a
  saturated runtime is a lock-contention fix, and a slow lane open is a carrier fix.
- **Guard**: instrument only; the pairing line and the open line keep their meaning, and the new pair
  is *inside* `open_us` so the two must sum to it (a check worth asserting in the reporter).
- **Rollback**: none needed.

### 4.88 The open is 1.2 ms and flat, so the 12 ms is a 10.8 ms gap nobody has measured (2026-11-07)

§4.87 pre-registered measuring the client's open span, on the hypothesis that the ~12 ms pairing wait
was the client's runtime giving the new task small slices. **The instrument existed already** —
`run_mux_data_channel` logs `mhd channel open finished … open_us=… cmd_us=…` — so this round added a
reporter for the *non-multiplex* path only and then read the numbers that were already there.

**Both halves in the same run, because the first comparison I made was across two runs and that is
not a comparison:**

| | p50 | p99 |
|---|---|---|
| pairing `wait_us` (server) | **11 948 µs** | 84 291 |
| channel `open_us` (client) | **1 178 µs** | 23 738 |
| **the gap** | **10 770 µs** | — |

`cmd_us` is `open_us` + 21 µs, so the client's command handling is not it either.

**And the open is flat in load**, which refutes §4.87's own hypothesis: 1 139 µs at a median 92 B of
flight against 1 424 µs at 237 516 B — **1.25× for 2 582× the bytes**. So the client's runtime is not
starving the open: a channel opens in 1.2 ms whether the lanes are idle or carrying a quarter of a
megabyte.

**So the 12 ms is neither the client's scheduling nor the open. It is a 10.8 ms gap between "the
channel finished opening" and "the pairing took it"**, and that is the one hop in the chain nobody
has stamped. The chain is: server enqueues → control command → client opens (1.2 ms) → **the new
connection reaches the server's listener, is accepted, and is routed to the pool** → `push` (which
calls `note_wake`) → the pairing's `take_for` resumes (which calls `note_resume`). Every stamp on the
ends of that chain is microseconds — the wake-to-resume pair reports p99 **25 µs** — so the
unmeasured middle is the server's own **accept-and-route** of a connection that arrives while its
runtime is saturated with bulk pumps.

**Two things this rules out, worth stating.** It is not the shared queue handing one visitor's
channel to another: `ChannelQueue`'s own comment records that tail (a pairing p99 of 20.1 ms beside a
worst open of 7.6 ms) and the per-group one-slot delivery is the fix that is in the tree. And it is
not the retry budget: every recorded slow pairing has `attempts=0`.

**Next increment, pre-registered.** Stamp the server's inbound data-connection path: from the
listener accepting the connection to the moment its channel is pushed into the pool queue, reported
as its own histogram. That is the only unstamped segment left, and its width should be ~10.8 ms at
p50 if this is right.

- **Pass**: the accept-to-push span is the missing 10.8 ms, and it is **flat in flight** again (the
  same test, three times now). Then the fix is in how the server's accept path gets scheduled against
  its pump tasks, which is a runtime-shape change and not a carrier change.
- **If the span is small**: the gap is in the control command itself (server → client), and the
  §4.86 join's 1 ms resolution was too coarse to see it; the next step is that hop at microsecond
  resolution.
- **Guard**: instrument only, behind `MOLEHILL_MHD_STATS`.

### 4.87 Two designs refuted before they were written, and the wait is flat in flight (2026-11-07)

§4.86 pre-registered a reserve of depth 1 per service. Three measurements taken before writing it —
two of them from logs already on disk — say it cannot work, and they re-aim the problem.

**1. A depth-1 reserve cannot hold the tail, by arithmetic.** From the same run: the clean window's
visitor arrival rate is **34.1/s** (36 in a median second), so the inter-arrival is **27.8 ms**; the
refill is the open, which costs **11.7 ms at p50 but 114.7 ms at p99**. A single-flight depth-1
reserve is therefore *empty* whenever a refill lands in its tail, and the fraction of visitors who
still wait is roughly `refill / inter_arrival` — so the p50 improves and **the p99 barely moves**,
which is the one number condition ① is written on. Covering the p99 needs a depth of about
`114.7 / 27.8 ≈ 4`, and at that depth the drain rate is exactly what §4.20 measured as "the belt is
fed faster than it is consumed". **The reserve is not the fix, and this is known before any code.**

**2. Preferring an opening stream in the picker cannot help either.** `next_stream_range` could be
taught to serve a stream in its `opening` phase first, which is cheap and tempting. But §4.84 measured
the sender's queue at **25 µs** — the handshake's packets already leave this end immediately. What
they wait behind is the **socket's own buffer**, already filled with bulk, and no ordering chosen at
pick time reorders bytes the kernel has accepted. A sender-side priority scheme is sorting the
wrong queue, and §4.82's lesson (a queue that is not where the wait is) applies unchanged.

**3. And the wait is not a queue at all.** Joining each slow pairing to the total flight in its
second:

| flight half | median flight | pairing `wait` p50 |
|---|---|---|
| lower | 299 478 B | **12 078 µs** |
| upper | 2 308 139 B | **11 553 µs** |

**7.7× the bytes in flight buys 0.96× the wait.** So the 11.7 ms is *not* the handshake queueing
behind bulk data — it is **roughly constant under load**. A constant ~12 ms is a timer or a
scheduling quantum, not a queue, and it sits on the client: the join in §4.86 says the client *starts*
dialling inside a millisecond, and what is unmeasured is the span from starting the dial to the
channel being established (`spawn_data_channel` → handshake done → hello).

**Next increment, pre-registered.** Measure that span on the client: from the `mhd channel open
started` stamp to the moment the channel is established, reported as its own histogram beside the
existing line.

- **Pass**: that span is ~12 ms and roughly flat in bulk load (the same flatness test as above).
  Then the open's cost under load is the **client's runtime** giving the new task small slices while
  it forwards bulk, and the fix is in how the client shares its runtime — not in the pool, not in the
  lane, and not in a reserve.
- **If the span is small** (say 1–2 ms), then the 12 ms is *before* the client's dial after all, the
  §4.86 join's 1 ms resolution was hiding it, and the next place is the control session's own queuing
  on the client side.
- **Guard**: instrument only, behind `MOLEHILL_MHD_STATS`; no behaviour change.
- **Rollback**: none needed — nothing to revert but the reporter.

### 4.86 The join: the client acts at once, and the open itself costs 11.7 ms under bulk (2026-11-07)

§4.85 pre-registered joining both ends on the group id. **The join was designed into the tree and
had never been used**: the server logs `mhd pairing request group=G ask_ms=…`, the client logs
`mhd channel open started group=G now_ms=… queued_us=…`, and the comment beside the client's line
already says what the subtraction means. 6 425 groups, **6 425 matched**, and 641 of them are the
slow pairings §4.85 measured.

| segment | p50 | p90 | p99 | max |
|---|---|---|---|---|
| server asks → client **starts** dialling | **−1 µs** | 999 | 1 000 | 4 999 |
| dialling → channel arrives | **11 664 µs** | 37 290 | 114 749 | 262 147 |

**The control hop is 0.6 % of the wait; the open is 99.4 %.** The client acts immediately — the
`−1 µs` is the 1 ms resolution of `as_millis()`, not a negative duration, and it means "inside the
same millisecond". So **(c) is excluded**: the client is not being starved of scheduling. **(b) is
the answer**: the channel's own open is what costs 11.7 ms.

**Which is seven times its idle cost.** §4.68 measured an open at **557 µs** with sealing and
**1 610 µs** without; here it is 11.7 ms median and 115 ms at p99, while every data-path instrument
in the same run reads microseconds (§4.84). The open is a handshake — several round trips on a lane
that is saturated — so each round trip queues behind the bulk. That is consistent with everything
measured so far and it is the first statement in this file that says *where* the bulk contention
lands: not on the bulk's own packets, and not on the sender's queue, but on **a new stream's
handshake**.

**What this means for a fix, and why §4.20 has to be read carefully before acting.** The obvious
answer is "keep a channel ready". §4.20 built exactly that — one spare **per paired visitor** — and it
**lost on every cell** (rr-1 0.208 against 0.151 ms, rr-16 20 095 against 23 382, churn-16 no
improvement), because one spare per visitor doubles the opens and the belt is fed faster than it is
consumed. That section ends with the sentence this round can now act on: *"A reserve of a fixed small
depth, refilled only when the pool is empty, is a different experiment and has not been run."*

**Next increment, pre-registered.** That experiment: **a reserve of depth 1 per service**, refilled
in the background only when it is empty, so a visitor takes the ready channel instead of waiting for
a handshake.

- **Pass**: `clean` p99 falls below TCP's same-round figure **and** the pairing `wait_us` p50 drops
  out of the 11.7 ms region, **without** moving `rr-1`, `rr-16` or `churn-16` — the three cells
  §4.20 lost on. Those three are the guard because they are exactly what a badly-sized reserve costs.
- **If the refill cannot keep up** (the reserve is empty whenever a visitor arrives): the measurement
  says so directly — `wait_us` stays put — and the answer is that a per-service reserve cannot beat a
  handshake whose cost is the contention itself. Then the fix has to be fewer round trips in the
  open, which is a wire change and belongs to 6.0.
- **Rollback**: any of the three guard cells moving, or `loss5`/`jitter` regressing.
- **Discipline**: implemented behind a switch, committed **before** the run (§4.82), reverted whole if
  it loses, numbers kept (§4.20's own precedent).

### 4.85 The pairing wait splits 99.9 % into one component: waiting for a channel to be produced (2026-11-07)

§4.84 pre-registered splitting the pairing into its parts, with the pass criterion "one of them is at
least 80 % of the 88.5 ms". **The split was already in the log** — `report_pairing` prints a
four-part breakdown for every pairing over 5 ms — so this round needed no new run.

641 slow pairings from the dwell run, in microseconds:

| component | p50 | p90 | p99 | max | share of all pairing time |
|---|---|---|---|---|---|
| `accept` (visitor taken off the listen queue) | 51 | 71 | 95 | 167 | **0.3 %** |
| `enqueue` (request put on the pool's queue) | 1 | 1 | 3 | 10 | **0.0 %** |
| **`wait` (until a data channel is taken)** | **11 794** | **37 496** | **114 748** | **263 146** | **99.9 %** |
| `write` (channel handed to the visitor) | 5 | 22 | 267 | 575 | **0.1 %** |

**`wait` is 99.9 % of it**, which clears the pre-registered 80 % by a wide margin. `attempts` is 0 on
every one of the 641, so this is not a retry loop, and `accept` being 51 µs says the listen queue is
not where visitors queue either.

**The sample is the tail by construction, and that is the right sample here.** `report_pairing`
returns early under 5 ms — a threshold-gated log, which this file forbids as a way to characterise a
*distribution*. The question was never the distribution: §4.84 already had it from the `paired_*`
percentiles (p50 1.7 ms, p99 88.5 ms). The question was **what the slow ones are made of**, and for
that a sample of slow ones is exactly right. Stated so the next reader does not add these to the
percentile.

**What is now narrowed and what is not.** Of §4.84's three candidates, **(a) "no free tunnel" is
excluded**: `enqueue` is 1 µs, so the pool accepted the request instantly, and the wait is on the
*answer*, not on the queue. What remains inside `wait` is either **(b)** the client's own open work
(the control round trip plus the carrier's stream open, whose own cost is ~557 µs per §4.68) or
**(c)** the client's task not being scheduled to act on the request. **A 11.8 ms median against a
0.6 ms open is twenty round trips, so (c) is the better suspect — but this measurement cannot tell
them apart, and guessing here is what this cycle has twice paid for.**

**Next increment, pre-registered.** Join the two ends on the group id the file already carries for
exactly this purpose: the server's `debug!("Visitor paired with a striped group", group, at_us)` line
carries an absolute epoch stamp, and the client logs its own line for the same group. The gap between
them is "how long after the server asked did the client act", which separates (b) from (c).

- **Pass**: the client's decision lands **within a millisecond** of the server's stamp, i.e. the
  client acted at once and the time went into the open itself ⇒ **(b)**, and the fix is a pool that
  does not have to open a channel per visitor.
- **If the gap is tens of milliseconds**: **(c)**, the client is not scheduling the request, and the
  fix is in how the client's channel-request path shares a runtime with bulk forwarding.
- **Guard**: the join needs both ends' logs, so this run's work directory must be preserved before
  any clean-up — the rule §4.69 paid for.
- **Rollback**: nothing but reading logs; no code changes this increment.

### 4.84 The tail is the pairing wait, not the data path — and every carrier instrument agrees (2026-11-07)

§4.83 pre-registered one measurement before writing any scheduler: how long a byte waits in this
end's send buffer. The instrument already existed — `stats::note_dwell` has been called on every
wait since it was written — and had **never been printed**; this round added the reporter and ran
once.

**Every carrier-side number is microseconds, in a run whose cell p99 is 83.76 ms.**

| instrument | value in this run |
|---|---|
| send-buffer dwell, server | median reported p99 **25 µs**, worst **7.0 ms** |
| send-buffer dwell, client | **11 µs** |
| pump's post to the socket | p99 **53 µs**, max **3.0 ms** |
| parked pump after its wake | p99 **57 µs** |
| request-to-reply turnaround (group) | p50 **658 µs**, max **3.6 ms** |
| writes refused | by the **window** (cwnd), thousands — normal flow control |

So the byte leaves this end promptly, the socket write is fast, and the wake is fast. **A
sender-side scheduler cannot fix an 83 ms tail that the sender spends 25 µs on.** §4.83's second
branch is the one that happened.

**The instrument that does explain it was already in the log**, one line per second:

```
visitor arrival, split at the pairing n=32 paired_p50_us=1709 paired_p99_us=88533 first_p99_us=34
```

`paired` is the wait from a visitor arriving to a data channel being paired to it; `first` is the
wait from the pairing to the first byte. **88.5 ms against 34 µs.** And the same line under each
condition:

| | pairing p99 | the cell's p99 |
|---|---|---|
| no bulk (§4.83's run) | **3.2 ms** | 3.08 / 3.10 / 3.41 / 3.31 ms |
| with bulk (this run) | **88.5 ms** (worst per-second report 263 ms) | **83.76 ms** |

**The pairing wait and the cell's p99 are the same number.** The visitor's request is not slow
because data moves slowly; it is slow because **there is no data channel to move it on yet**, and
under a bulk transfer the pool takes tens of milliseconds to produce one.

**What this changes.** Phase C as it was scoped — class-aware scheduling *inside the lane* — would
have measured a data path that is already fast and left the actual wait untouched. The work that
remains is at the **pool**: how a visitor obtains a data channel while the tunnels it would take are
busy carrying bulk. That is the subject §4.20 examined and refuted in a different form ("a fixed
small reserve refilled only when the pool is empty"), and it is back with a number attached.

**Next increment, pre-registered.** Find which of the three waits the pairing actually is, because
they have different fixes: **(a)** waiting for a *free* tunnel in a pool that is all busy, **(b)**
waiting for the client to *open* a new one (a control round trip plus the carrier's own open), or
**(c)** waiting for the control channel or the client's task to be *scheduled*.

- **Measure**: bracket the pairing on the server into "request sent to the client", "channel
  arrived", "paired", and report each as its own histogram beside the existing `paired` line — the
  instrument shape is already there and adds no clock to the data path.
- **Pass**: one of the three accounts for ≥ 80 % of the 88.5 ms under bulk. Then the fix is known
  before it is written, which is the whole point of this round's discipline.
- **If it is (a)**: the pool needs a channel that is *not* carrying bulk — which is a scheduling
  decision at the pool, not in the lane, and it is what a reserve is for.
- **If it is (b) or (c)**: the fix is in how the client answers a channel request under load.
- **Rollback**: instrument only; the unobstructed path must not move.

### 4.83 The bulk spine is the whole of condition ①, and the two distributions do not overlap (2026-11-07)

§4.82 ended with the crude form of priority scheduling refuted and the precise form untested. Before
writing a scheduler, one cheap experiment settled whether the premise even holds: **run the same
timeline with the bulk spine removed** (`nobulk` profile: `streams: 0`, two rounds, one arm).

| | `clean` p99 | `clean#2` p99 | p50 | max |
|---|---|---|---|---|
| **no bulk**, round 0 | **3.08** | **3.10** | 2.40 / 2.43 | 22.9 / 3.6 |
| **no bulk**, round 1 | **3.41** | **3.31** | 2.52 / 2.61 | 24.2 / 23.6 |
| with bulk, ten draws (§4.79, §4.81, §4.82) | **9.01 … 133.0** | — | 2.55 … 10.85 | 147.4 |

**The two distributions do not overlap.** The worst no-bulk draw is 3.41 ms; the best with-bulk draw
is 9.01 ms — a **2.6× gap between the ranges**, and the spread itself collapses from 15× to 1.1×
(3.08–3.41). Error rate 0.0 % throughout.

**Three things follow, and the second changes the plan.**

1. **Bulk is the cause, not a contributor.** §4.80's flight join could only offer 1.5× and said so; this
   is a clean separation, which is the only form this file accepts as proof.
2. **The 9× variance of §4.79 is not ambient noise.** Without bulk the p99 is stable to 11 %; with it,
   it moves 15× between draws. So the variance is a *function of how much bulk is in flight when the
   probe samples* — which means it is **controllable**, and "condition ① is a coin flip" was a
   description of the symptom rather than of the system. The system has a cause.
3. **The carrier alone is already better than the target.** 3.1–3.4 ms is under the 50 ms SLO by 15×
   and under TCP's same-round 14.40 ms by 4×. **mhd without bulk is the better carrier.** Everything
   standing between the tree and condition ① is how it shares a lane with a bulk transfer.

**Next increment, pre-registered.** Locate the wait inside the carrier before changing the scheduler —
the same discipline §4.82 earned. For each packet the pump builds, measure **how long the byte it
carries waited** in this end's queue: sampled once per waiting episode, per lane, reported as
`pickup_p50/p99/max` beside the cycle line.

- **Pass (the fix is in the sender's scheduler)**: `pickup` p99 for the echo's lane is of the same
  order as the observed 40–80 ms tail. Then class-aware selection fixes it, and it is a real fix.
- **If `pickup` is microseconds**: the byte left this end promptly, so the wait is downstream — the
  socket's own queue, the wire, or the receiver's pump — and a sender-side scheduler would fix
  nothing. The next place to look is what the pump does with 4 MB already in the socket.
- **Guard**: instrument only, gated on `MOLEHILL_MHD_STATS`; the unobstructed path must not move.
- **Rollback**: if the instrument costs more than 5 % of throughput against an uninstrumented arm.

### 4.82 The flight cap made the clean cell worse, so it is reverted (2026-11-07)

§4.81 predicted that capping a lane's outstanding bytes would pull the `clean` cell's tail below
TCP's, because that cell is queueing rather than loss. The cap was built behind
`MOLEHILL_MHD_FLIGHT_CAP` (default off), applied as a fourth term beside credit, window and the
buffer ceiling — and then measured, one run per setting, both with the TCP arm present:

| cell | mhd **no cap** p99 | mhd **cap 512 KiB** p99 | TCP p99 (same run pair) |
|---|---|---|---|
| `clean` | 27.45 | **114.66** | 39.19 |
| `jitter` | **115.23** | 128.17 | 124.99 |
| `clean#2` | 9.14 | **3.74** | 4.08 |
| `loss5` | 2 468.16 | **1 446.59** | 2 476.63 |
| `rtt100` | 403.35 | 438.73 | 437.22 |
| `rate100` / `rate20` | 203.34 / 515.27 | 217.98 / 509.43 | 210.97 / 509.49 |

**The cell it was aimed at got 4× worse, and the guard cell moved the wrong way.** Under the
pre-registration — pass is `clean` p99 below TCP's *and* `jitter`/`loss5` unchanged — this fails on
both counts, so **the increment is reverted whole** and the switch is gone from the tree rather than
left off-by-default: an off switch with a failed hypothesis behind it is a knob nobody should turn.

**Why it failed is worth more than the verdict, and it corrects the hypothesis.** On this cell the
"queue" is not a buffer sitting beside the pipe — **it is the pipelining itself**. At 8 Gbit/s a
512 KiB cap is about half a millisecond of data, so the lane must stop and wait for an acknowledgement
every half millisecond instead of keeping 4 MB outstanding. The echo request does not then find a
shorter queue; it finds a **slower lane**, because the bulk spine's own packets are now gated. So
shrinking the outstanding window on a clean cell trades everyone's throughput for nobody's latency —
and `clean#2` and `loss5`, which improved, are the cells where there *is* a shaper and the cap acts as
a crude pacing hint.

**That is a real distinction and it re-aims the next step.** The property that would help is not "less
in flight" but "**the echo does not sit behind the bulk**" — the same total flight, ordered by class.
That is exactly what phase C (priority scheduling) is, and §4.82 is the measurement that says the
crude form of it does not work while leaving the precise form untested. C now has both an indication
(§4.80) and a refuted alternative (§4.82) behind it.

**A method note, because this round nearly lost work.** The cap was never committed when it was
measured, so "revert the increment" had to mean discarding the worktree; the revert command first
landed on the previous *docs* commit and had to be undone. Nothing was lost, but the rule it produces
is worth keeping: **commit the experiment before running it**, so that reverting is a git operation
and not a judgement about which files are the experiment.

### 4.81 The TCP comparison: mhd loses on exactly one cell, and it is the same one as ① (2026-11-07)

Condition ② asks for p50/p90/p99 no worse than the in-tree TCP carrier on like workloads. Same run,
same round, both arms, `--scale 0.15`:

| cell | mhd p50 | tcp p50 | mhd p99 | tcp p99 | mhd thr | tcp thr |
|---|---|---|---|---|---|---|
| `clean` | 3.02 | 2.63 | **59.04** | **14.40** | 8.61 | 8.50 |
| `clean#2` | 2.15 | 2.68 | **4.70** | **23.83** *(no measurement)* | 2.61 | 0 |
| `rtt100` | 402.26 | 402.43 | 403.24 | 410.13 | 2.57 | 2.51 |
| `loss1` | 42.47 | 42.94 | 1 061.06 | 1 052.12 | **4.79** | **6.07** |
| `loss5` | 402.76 | 403.05 | **1 454.86** | **2 100.77** | **1.11** | **0.30** |
| `rate100` | 177.41 | 177.42 | 233.20 | 228.12 | 0.09 | 0.09 |
| `rate20` | 391.15 | 383.22 | 549.13 | 508.05 | 0.02 | 0.02 |
| `jitter` | **83.41** | **90.88** | **117.96** | **171.77** | 4.02 | 5.82 |

**`clean` is the only cell where mhd is clearly worse, and it is worse by 4×.** p99 59.04 against
14.40 ms, p50 3.02 against 2.63, throughput identical (8.61 / 8.50). Every other cell is a tie or a
win — `jitter` better on both latency axes, `loss5` better on both the tail (1 455 against 2 101) and
throughput (**1.11 against 0.30**, 3.7×), `rtt100` and the rate cells equal, `loss1` equal on latency
with TCP 27 % ahead on throughput.

**`clean#2`'s TCP arm is not comparable and must not be read.** It is `ok=False` with the reason the
zero-carry guard from `c8ac062` gives: *"the bulk spine carried no bytes in this stage: 4 stream(s)
were dialed and the sender reported zero"*. So mhd's 4.70 ms there has **no counterpart** — and the
guard is what stopped a zero-carry TCP cell from being read as a 0.00 Gbit/s win for mhd. That is the
mechanism working on the first occasion it mattered.

**What this does to the plan is the point of the section: ① and ② are one problem.** The cell that
fails the SLO (①) is the cell where the carrier loses to TCP (②), and it is the *same* cell whose
`flight` spans 59 KB to 7.8 MB (§4.80). Three independent measurements now point at one place:
the clean cell's tail, when a lane is carrying megabytes behind an echo.

**And it explains why the lossy and jitter cells are wins.** There the TCP carrier's kernel stack is
doing congestion control over a shaper; mhd's own recovery is doing it directly, and it wins. On the
clean cell there is no shaper to fight, and what is left is **queueing**: 4.8 MB of flight behind a
small echo request costs exactly what §4.80 measured (76 ms worst-request mean in ≥1 MB seconds).
TCP does not lose that request because its stack keeps the queue small; mhd allows it to grow because
its window is 16 MiB and the `ceiling` never binds on this cell.

**Next increment, pre-registered.** Attack the queue, not the scheduler: **cap what a latency-bound
service puts in flight** — a per-lane outstanding-byte limit applied to streams whose service is
latency-sensitive, chosen as the smallest value that does not cost throughput on `clean` (the two are
measured in the same cell, so one run decides both).

- **Pass**: `clean` p99 falls **below TCP's on the same round** (14.40 ms here) **and** `clean`
  throughput stays within the same round's A/A of the uncapped arm. A cap that buys p99 by losing
  throughput is not a win under the stated priority order unless it is large, and the objective's
  first condition is latency.
- **If the cap cannot beat 14.40**: phase C (priority scheduling) is next, because a cap is the
  crude form of the same idea.
- **Guard**: `loss5` and `jitter` must not regress — they are the cells mhd currently wins, and a
  blanket cap would trade them away.
- **Rollback**: any of `loss5`/`jitter` moving, or `clean` throughput outside the A/A.

### 4.80 The join: the slow request waits in `echo`, and its lane is carrying megabytes (2026-11-07)

§4.79's pre-registration joined the above-SLO round's slow requests to the lane-seconds they ran in.
Round 1 of `pivot1` is the above-SLO round (`clean` p99 81.09 ms, max 147.36 ms); all three quarters
of the join already existed.

**Where the time goes, first.** The worst request spent **147.2 of its 147.4 ms in `echo_ms`**, with
`connect_ms` at **0.15 ms**. The next two are the same shape (121.9 of 122.1; 85.6 of 85.7). So the
visitor's connection is opened promptly and the carriage is what is slow — the shape §4.46 recorded.

**What the lanes were doing in those seconds: busy and healthy.** In the 147 ms request's second,
three lanes carried 221 k / 265 k / 332 k packets, with `yield_raw` at 203 / 175 / 218 ms (**17–22 %**,
the §4.63 baseline is 19.3 %) and gap proportionally normal. **So this is not a scheduling stall**, and
§5's item 2 (the wake path) is **not** what this round indicates. A lane that was descheduled would
show yield far above its baseline or a parked second; neither appears.

**What did stand out is `flight`.** In those three seconds the largest flight was **4 392 344 /
1 726 720 / 652 916 bytes** — where §5 has claimed since §4.6x that "`flight ≈ 59 KB` is exactly one
run and the window never fills". Across the whole clean window it reads:

```
flight range: 59 356 .. 7 834 992 bytes     seconds under 100 KB: 2 of 23
flight >= 1 MB : n=9   worst-request mean 76.1 ms   worst 147.4
flight <  1 MB : n=14  worst-request mean 50.0 ms   worst  85.7
```

**Two facts, and the first is a correction to a standing claim.** The window on this cell is *not*
irrelevant: `flight` spans two orders of magnitude inside one clean stage, and **only 2 of 23 seconds
are anywhere near the 59 KB the file has been quoting**. That figure is true of the *best* seconds —
which is where most of the file's per-second instrumentation was read, because those are the seconds
that look clean.

**The second is directional, not decisive.** Seconds with ≥ 1 MB in flight show a 1.5× worse
worst-request latency (76.1 against 50.0 ms, n=9 and n=14). That is a real direction and the right
mechanism — a request arriving while megabytes are outstanding waits behind them — but 1.5× is not
the 9× of §4.79, so queueing behind bulk is **part of** the variance, not all of it.

**What this decides.** §5 item 4 (phase C, priority scheduling) was conditional on "first prove the
bulk spine is pushing interactive's tail". **This is that proof**, in the only form the file accepts:
a same-lane, same-second join, with `echo` carrying the time and `flight` carrying the megabytes. C
moves from *conditional* to *indicated*. It is still not scheduled ahead of the TCP comparison,
because C changes scheduling for every service and should be decided with ②'s numbers in hand.

**And what it rules out.** The wake path (§5 item 2) is not indicated: the slow seconds were not
parked and their yield was at baseline. Chasing the pump's wake-up would have been a detour that this
join, cheap as it was, prevented.

### 4.79 The release cell moves 9× between adjacent rounds of one run (2026-11-07)

One arm, two rounds, one binary, one shape, nothing changed between them:

| round | `clean` p50 | `clean` p99 | errors | `clean#2` p99 | `clean` thr |
|---|---|---|---|---|---|
| 0 | 2.64 | **9.01 ms** | 0.0 % | 4.76 ms | 8.39 Gbit/s |
| 1 | 5.03 | **81.09 ms** | 0.0 % | 8.74 ms | 7.78 Gbit/s |

**A 9× swing in p99, between two rounds of the same run, with the throughput moving 7 %.** Every
earlier measurement in this file described the spread as living "between arms and rounds" — §4.48's
four reads, §4.54's four, §4.68's four. This pair sharpens that: the spread needs no change of
binary, session, load shape or clock; it is present in the **narrowest comparison the harness can
make**, and it is what §4.66's 36 % throughput swing was a shadow of.

**Two things follow, and the second is the important one.**

- **Condition ① cannot be settled by any number of rounds of this cell.** Eight draws now read
  9.0 / 23.2 / 24.1 / 47.3 / 59.6 / 80.5 / 81.1 / 96.8 / 133.0 ms — about half under the 50 ms SLO
  and about half over, with the best and worst separated by 15×. A criterion written as "each of two
  cells at p99 ≤ 50 ms, no averaging, no warmup, no deleting the first stage" is, against this
  distribution, a **coin flip per cell**, and running it more times only prices the coin.
- **So the work that remains for ① is not measurement, it is the variance itself** — and the
  variance has a known shape that has never been attacked: §4.46's p99 pathology (both carrier round
  trips slow 2.4–2.9× together while the visitor's first-byte delay is unchanged) is a **lane-level
  mode**, and a lane-level mode that arrives and leaves between rounds is what a *stall* looks like,
  not what congestion looks like.

**What is known about stalls on this cell, and what has never been measured.** The 2-second reader
park appears in every arm (§4.68: 1 999–2 386 ms, including both OFF arms), so a long stall is
present even when nothing about the carrier changed. `wake_pump→pass` latency was instrumented in
§4.46. What has **not** been measured is where the *p99 sample itself* spends its time: a 81 ms p99
means some request waited 81 ms, and this file can say what the lane's cycle looks like second by
second (§4.62–§4.66) but has never asked **which of those seconds the slow request was in, and what
the lane was doing then**.

**Next experiment, pre-registered.** Cross the two instruments that already exist: for a round whose
`clean` p99 is above the SLO, take the request whose wait is at p99 (the interactive series records
it) and join it against **the same lane-second's** `cycle=` line — flush, gap, yield, parked — and the
window line's `flight`/`buffered`. The question is only "what was the lane doing in the second the
slow request happened in", and both halves of the join are already recorded.

- **Pass**: the p99 request's second is a second the lane spent **parked** (or with `yield` far above
  19 %). Then the stall is the lane not being scheduled, the fix is in the pump's wake path, and
  §4.46's `consume_budget` result becomes explainable rather than a dead end.
- **If the lane was busy and healthy in that second**: the delay is above the lane — in the pool, the
  stripe group, or the service — and the next instrument moves there. That is also an answer.
- **Guard**: the join needs absolute timestamps on both sides, which the run already records
  (`group` ids); a round with no above-SLO request is not evidence and is skipped, not averaged.

### 4.78 A stop to the datagram plumbing, and the reason is the completion conditions (2026-11-07)

Phase 3 has now run eight sections — §4.71 through §4.77 — and the honest accounting is that it has
produced **no user-visible improvement yet**. What exists is a wire class, a carrier that can carry
one and hand it over once, a test that pins its three properties, and a documented order in which the
service endpoints may be wired. What does not exist is a datagram that a visitor ever receives on the
new class.

**And the completion conditions do not ask for one.** They are: `timeline[clean]` and `clean#2` at
p99 ≤ 50 ms and ≤ 0.5 % errors; p50/p90/p99 not worse than in-tree TCP on like workloads; `bulk-n`
within 15 % of a valid baseline; capacity 8/8; coverage / endpoint / drift / wedge green; the final
tree green through `just check`, `just interop` and L3 acceptance with UDP/L3 **not regressing**;
then the release record. **"UDP does not regress" is already satisfied** by the framed-into-a-reliable
-stream path this phase was going to replace, and nothing in the list prices the datagram class.

**So the order changes, and the change is written down rather than made quietly.** The remaining
rounds go to the conditions that are still open and are all *measured*: ①, the release cell's
distribution; ②, the comparison against in-tree TCP and the peers; ③, `bulk-n` resolution; ⑤, the
gate axes. Phase 3 is **parked**, not abandoned: the code is committed, tested and inert, its
receive door is used by a test and carries **one** narrow waiver, and the pre-registration that
resumes it is §4.77's — the wake mechanism and the client hook.

**Two things this costs, stated so they are not discovered later.**

- The parked code is *inert*, which means it does not pay rent: nothing measures it, and a future
  reader will find a class the product does not use. That is acceptable only because §4.71–§4.77 each
  say so out loud and because the alternative — reverting a tested, committed piece of wire protocol —
  costs more than carrying it into the release under its own section.
- If a later phase needs the lane's datagram path (a hostile path where ARQ on a UDP service is
  visibly wrong), it starts from §4.77's pre-registration, not from a blank slate.

**What the next measurement is.** No new instrument: **two more rounds of the release cell**, one arm,
the same shape, recorded into the same distribution, because ① currently has six draws
(47.3 / 80.5 / 96.8 / 133.0 / and four from §4.68's arms at 23.2 / 24.1 / 59.6) that do not yet say
anything a single number could. The question this round answers is only "where does this tree sit",
so the next decision — reducing the spread, or arguing the cell's SLO against its own floor — is made
on more than one sample.

### 4.77 The receive door exists, and one waiver went away as another arrived (2026-11-07)

§4.76 pre-registered the client read path. What landed is the half that can be verified by not
changing behaviour: **the carrier now stores what arrives and hands it over on request**.

- `Conn` gained `in_datagrams` (bounded by the same `DATAGRAM_QUEUE`, oldest dropped) and
  `accept_datagram`, its own function because `apply_datagram` is at the line budget — and because the
  two things it does are exactly what a datagram service depends on: count what arrived, and drop the
  *oldest* queued one when full.
- `take_datagrams` empties the queue, so the layer above collects each datagram **once**; nothing else
  may, or a datagram would be counted as received and never delivered.
- The lane test now asserts all three: the peer counts **1**, the collected queue holds **exactly one**
  entry, and it is the id the sender chose (`3`) with the bytes it sent (`b"hi"`). The id stays opaque
  to the carrier — the test names it as the sender's, not the carrier's.

**The waiver count moved down by one and up by one, and both moves are honest.** `DATAGRAM_QUEUE` and
`queue_datagram` no longer needed theirs: `accept_datagram` uses the bound in **real** code, and
`queue_datagram` is the sender's door that the server side will call. `take_datagrams` needs one
because its caller is `udp_read_loop`, which is **not** wired yet — and that is deliberate, because the
loop needs something this section does not have.

**What the loop still needs, stated rather than assumed.** `udp_read_loop` waits on `read_u8` from the
reliable stream and has **no wake-up for a datagram**: the lane's arrivals are delivered to `Conn`
without waking anyone. The options are a `Notify` per lane (correct, and it touches `Shared`), a
periodic poll (wrong: a timer on the client's read path costs CPU for a queue that is empty today), or
having the pump wake the reader the way it already wakes writers (`wake_readers` exists for streams —
the datagram path would need its own reason bit so a stream read does not spin). **None of these is
the next paragraph's business to guess**, so the loop hook is pre-registered as its own increment with
the mechanism as its first decision.

**Next increment, pre-registered.** Choose and implement the wake mechanism, then hook
`udp_read_loop`: when the channel is mhd, the loop takes datagrams and delivers them through the same
per-visitor sink `UdpTraffic` fills.

- **Pass**: behaviour identical while the server does not switch — a `udp-pace` run's `udp_recv_mbit`,
  `udp_loss_pct` and `udp_gap_p99_ms` inside the run's own A/A, the lane's `datagrams_in` still **0**,
  and the client's CPU (`cpu_s_per_gbit`) not worse than the A/A, which is what rules out a poll.
- **Guard**: `just check` and the integration suite.
- **Rollback**: any movement in the UDP cell, or a CPU cost above the A/A, means the mechanism is wrong
  and the hook comes out.

### 4.76 The wiring has an order, and getting it backwards breaks UDP quietly (2026-11-07)

Before cutting into (b′) I checked which end can move first. It is not symmetric, and the asymmetry
is worth a paragraph because the wrong order has no loud failure.

**The two directions carry different traffic.** Server → client is visitor-bound datagrams; client
→ server is the service's replies. Today **both** are `UdpTraffic` frames on a reliable stream.

**If the server switches first**, it stops writing `UdpTraffic` for visitor-bound datagrams and
writes `Frame::Datagram` instead. The client it is talking to still reads `UdpTraffic` — so those
datagrams arrive at a reader that is not looking for them and are **counted by the carrier and
dropped** (§4.73's receive path returns early after incrementing). No error, no reset, no log line:
`datagrams_in` rises, the visitor sees nothing, and the UDP cell's loss metric attributes it to the
network. That is the worst shape a defect can have here — a regression that looks like the thing
the benchmark already measures.

**So the order is: the client reads first, the server switches second.** The client can be taught to
read a datagram frame while the server still sends `UdpTraffic` with no cost at all — the read path
is additive and never fires until the server switches. The reverse is not available.

**And the switch itself must be conditional, not simultaneous.** Two builds of one repository are
not one process: a client from an older commit (or the other end of a release boundary) may still be
`UdpTraffic`-only, and dialect 6.0 is unreleased, so both ends can be rebuilt together — but the
bench runs peers and the integration suite runs both, and a silent drop is exactly what the release
gate would not catch. The capability belongs in the control-channel registration, next to the
existing `TransparentCapabilities` precedent: the client declares it can receive native datagrams,
and **the server uses `Frame::Datagram` only for a client that declared it**. Absent the declaration,
every datagram stays on `UdpTraffic` — the same property (b′) gets from the types, obtained here from
the handshake.

**Next increment, pre-registered.** The **client read path only**: `MhdLane` exposes
`take_datagrams()` and the client's `udp_read_loop` drains it into the same per-visitor sink that
`UdpTraffic` fills. Nothing on the server changes; nothing switches.

- **Pass**: with the server still sending `UdpTraffic`, the client behaves **identically** — a
  `udp-pace` run's `udp_recv_mbit`, `udp_loss_pct` and `udp_gap_p99_ms` land inside the run's own
  A/A against a pre-change baseline, and the lane's `datagrams_in` stays **0** (nothing is being sent
  on the class yet). This is the increment that can only be verified by *not* changing behaviour.
- **Guard**: `just check` and the integration suite; the new path must be unreachable without a
  producer, which the `datagrams_in == 0` assertion above pins.
- **Rollback**: any movement in the UDP cell means the read path is not as additive as claimed.

### 4.75 Cut (b) is blocked by a blanket impl, and the seam below it is the real one (2026-11-07)

§4.74 chose cut (b): an additive `datagrams() -> Option<…>` on the pool's stream type, "only the mhd
carrier answers `Some`". Reading the type to implement it shows that sentence cannot be written as
stated, and the reason is worth recording before someone tries anyway.

**The type is erased twice over.**

- `pub(crate) type CarrierStream = Box<dyn CarrierIo>`, and
- `CarrierIo` is a **marker trait with a blanket impl**:
  `impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> CarrierIo for T {}`.

So there is no "the pool's stream type" whose method could answer `Some` for one carrier and `None`
for another: the box holds a `dyn CarrierIo` that knows only I/O, and any `datagram_sink` added to
the trait would either arrive through the blanket impl (**every** type answering the default, and no
type able to override it, because a concrete impl would collide with the blanket one) or require
deleting that impl and writing an impl per stream type — a change to every carrier's plumbing for the
benefit of one.

**What that means for the cut.** The sink cannot travel *inside* the trait object. It has to travel
**beside** it, and the place that already holds a pair is the enum the pool actually moves:
`DataChannel { Raw(ServerStream), RawMhd(CarrierStream), Mux(MuxStream) }`. `DataChannel` is also the
concrete type the UDP pool is instantiated with — `run_udp_connection_pool::<DataChannel>`, one call
site — so it is the right place to carry a sink and the only place that needs to.

**Revised cut (b′), pre-registered before it is written.** `DataChannel::RawMhd` carries
`(CarrierStream, Option<DatagramSink>)`, filled where the mhd stream is accepted (the same place
`serve_mhd_stream` already has `lane: Arc<MhdLane>` in hand); `parallel_udp`'s worker takes the sink
as a parameter from `spawn_worker`, which reads it off the `DataChannel` it was given. Every other
struct variant carries no sink, so **a TCP carrier's UDP path is unchanged by construction**, not by
a runtime branch that must be tested to stay unchanged.

- **Pass**: unchanged from §4.74 — `udp-pace`'s `datagrams_in` grows at the offered rate while the
  lane's flight, retransmits and credit stay idle.
- **Guard**: unchanged, plus one this round adds: **the compiler is the guard for "TCP is
  unchanged"** — if the non-mhd variants cannot carry a sink, no runtime branch can regress them.
- **Rollback**: if `DataChannel` cannot carry the sink without changing its `AsyncRead`/`AsyncWrite`
  behaviour (it is a plain enum today, so this is a real risk only if a variant's wrapping is
  load-bearing), the increment goes whole.

**Not started this round, deliberately.** The change touches `multiplex.rs`'s boxed type, the
`DataChannel` enum, `spawn_worker`, `udp_forward_worker` and the client's `udp_read_loop`; landing
part of it would leave a type nothing produces and a waiver to cover it, which this file's rules
forbid. The round's deliverable is the corrected cut, which is the part that was wrong.

### 4.74 The datagram seam is the pool's data-channel API, and it is drawn here before it is cut (2026-11-07)

§4.73 pre-registered the service wiring with its two call sites named. Reading those call sites
properly turned up the real obstacle, and it is **not** in either of them: it is the boundary
between the pool and the service.

**Established by reading, not guessed:**

- the UDP pool takes a data channel as `data_ch_rx.take()` → `(_, mut conn)`, and `conn` is a
  **stream** — `C: AsyncRead + AsyncWrite + Unpin + Send + 'static`. **The `MhdLane` is dropped at
  that hand-off**, long before the worker exists;
- `UdpWorkerSet::spawn_worker` is generic over exactly that `C` (**one** call site), and
  `udp_forward_worker` carries visitor-bound traffic with
  `UdpTraffic::write_frame(&mut conn, …)` — a frame on the reliable stream, which is the cost
  §4.58 measured and this phase exists to remove;
- the affinity the datagram class must **not** lose already lives in `UdpRouteMap`
  (`Mutex<HashMap<SocketAddr, UdpRoute>>`, `UdpRoute { worker, last_seen }`), keyed by the
  visitor's address — so the per-visitor id the frame needs can be the **worker id plus a
  per-worker sequence**, and no new table is required;
- the lane handle *does* exist where the stream is accepted (`serve_mhd_stream(stream, lane: Arc<MhdLane>, …)`),
  so the information is available — it is being thrown away by the channel that carries the stream
  to the service.

**So the cut is one of three, and the cheapest is named.** Either (a) the pool's data-channel
channel yields an optional lane beside the stream, (b) the stream type grows a
`datagrams() -> Option<…>` method that only the mhd carrier answers `Some` from, or (c) the service
keeps `UdpTraffic` and nothing changes. **(b) is chosen**: it is additive, it changes no existing
signature, every other carrier answers `None` and keeps today's path byte-for-byte, and the pool
already hands the service a concrete type rather than a bare `TcpStream`. It also keeps the
capability where it is used instead of threading a new parameter through `spawn_worker`, the
worker set and both ends.

**Next increment, pre-registered before it is written.** Give the pool's stream type an optional
datagram handle filled when the carrier is mhd; `udp_forward_worker` uses it when present and
`UdpTraffic` otherwise; the visitor id is `worker << 16 | sequence`, and the client's
`udp_read_loop` reads the lane's datagrams when its channel has the handle.

- **Pass**: an `udp-pace` run carries its datagrams on the new class — `datagrams_in` (already on
  the window line, §4.72) grows at the probe's offered rate **while the lane's flight, retransmits
  and credit stay at their idle values**. Any of the three moving means a datagram is still paying
  for reliability somewhere, and the increment is not done.
- **Guard**: the TCP visitor's `clean` p50/p99 must not move beyond the run's own A/A (the two
  services share a lane and its pump), `just bench gate`'s endpoint invariant still holds, and a
  **TCP-carrier** UDP run is re-measured — it must be indistinguishable from today, because (b)
  says it answers `None`.
- **Rollback**: if the route needs the stream's ordering to keep its affinity, the class cannot
  share a lane with the reliable one as designed, and the whole increment goes.

### 4.73 A datagram crosses a real lane pair, and the buffer contract was the bug (2026-11-07)

The third increment gives the class a lane-level test and no service wiring — and it caught a real
defect in the second increment.

**The defect.** `transmit_datagram` called `wire::encode(out, …)`, which writes into a
**caller-sized slice**. Every other frame goes through `encode_control`, which appends into the run
the pump is staging, because **the pump's buffer starts empty**. So the datagram arm returned `None`
on every real pass: `a_datagram_queued_on_a_lane_arrives_on_the_peer` failed with the peer's counter
at 0 while the `Conn`-pair test passed, and the difference between the two tests is exactly that the
pump's buffer is a fresh `Vec`. The arm now uses `encode_control` like its siblings, and the reason
is written where it was wrong: "a bare `encode` would need a caller-sized slice, and the pump's
buffer starts empty".

**Two assertions had to become honest rather than convenient.**

- The `Conn`-pair test asserted `table_stats().0 == 1` — "the stream packet is still in flight".
  It is 0, because a pass may owe its peer a credit update first and the stream packet then goes out
  *after* the datagram the loop stops at. The assertion is now **relative**: the table's size before
  the datagram is captured, and the test asserts the datagram added nothing to it. That is the
  property; the absolute size was a coincidence of which control frame came first.
- The same test read the built packet as `&buf[..encoded]`. With `encode_control` the pass *appends*,
  so the packet is the **last** `encoded` bytes — `&buf[buf.len() - encoded..]`. The old form worked
  only while the buffer happened to hold one packet.

**What the lane test establishes**, on a real `Shared` pair driven by `pump_once`: a datagram queued
with `conn.queue_datagram(3, …)` reaches the peer's `datagrams_in` as **1**, and the sender's
recovery table is **unchanged** — nothing to retire, nothing to repair. No stream is opened, no offset
is named.

**Removed rather than waived.** The `MhdLane::queue_datagram`/`datagram_counts` wrappers written
earlier in the round were unused (the test drives `Shared` directly), so they are **deleted** instead
of attracting another `expect(dead_code)`. The producer's door gets added in the commit that first
walks through it, which is the next increment — the waiver already in `Conn` is the only one, and it
disappears the same day the service layer calls it.

**Next increment, pre-registered.** The service wiring, with its call sites named: the server's
`route_udp_datagram` currently pushes `(from, data)` into a `UdpWorkerQueue` whose worker writes a
`UdpTraffic` frame into a reliable stream; it will instead hand the datagram to the **lane** the
service rides, with the per-visitor id the route map already assigns. On the client,
`udp_read_loop` currently reads `UdpTraffic` frames off a reliable stream; it will instead take the
lane's datagrams.

- **Pass**: an `udp-pace` run carries its datagrams on the new class — `datagrams_in` grows at the
  probe's offered rate while the lane's `flight`, retransmits and credit stay at their idle values.
- **Guard**: the TCP visitor's `clean` p50/p99 must not move beyond the run's own A/A (the two
  services share a lane and its pump), and `just bench gate`'s endpoint invariant still holds.
- **Rollback**: if the route needs the stream's ordering to keep its affinity, the class cannot share
  a lane with the reliable one as designed, and this increment goes whole.

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

> **The governing plan is `MHD-FINALIZATION-PLAN.md`** (checkout root, untracked
> on purpose — see §1, "Where the plan lives"). Its phase order supersedes the
> "Order, as of this commit" list further down this section; that list still
> carries the measurements and reasoning behind each item, so it is kept, not
> deleted. When a phase lands, write its result in §4 and update the plan file in
> place.

**Where the plan stands (2026-11-08).**

| phase | state |
|---|---|
| 0 — measurement apparatus | **landed**: `src/probe.rs` (pool-less lane seam), `bench/transport_harness` (L1/L2), the gate wiring that makes a compiled bench member un-skippable, and three findings (§4.99, §4.100, §4.101) |
| 1 — recovery accounting and lifecycle | **closed** (§4.102): the counters were right; two invariants are now tests. The eight lifecycle items are handed to phase 3 as a checklist, not as proven |
| 1b — window and admission policy | **closed, no fourth buffer size** (§4.43, §4.103, §4.104): three settings, three failures |
| 2a — a plain-data plane in the bench | **landed** (§4.105): `l4-plain`, with `Arm.noise_data`, its docs and its selfcheck. Its A/A agrees to 0.04–0.5 %, making it the project's best instrument for per-connection work |
| 2 — no duplicate open ack | **landed** (§4.106), against its own revert rule and with the reason stated: wire −18.9 %, CPU −15.4 %, p99 −11.4 % (below the 15 % floor) |
| 2b — the striped window livelock | **blocking everything** (§4.107): the cell fails ~half of runs, `flight` sits at twice `cwnd` on both ends and neither writer may proceed |
| 3 — RACK / PTO / liveness | not started |
| 4 — re-attribution | not started |
| 5 — throughput and resources | not started |

**Next step: the server's receive path, measured before it is changed.** The human chose attacking the
25 µs over restating the acceptance criterion, and the first attack is already spent: an inline send from
the writer (§4.136) left p50 exactly where it was (45.0 µs) and was reverted. What that buys is the
hypothesis it kills — the client's hop to the pump is not the cost.

The other end is: the server's `read_us` is **41 µs over mhd against 15 µs over TCP** (§4.133/§4.134), and
that number is read *after* the datagram has arrived. So the 41 µs is inside `recvmsg` → route by lane id →
`on_datagram` → `apply_datagram` → wake the reader → the reader task running again: two task switches and
one syscall.

The order is instrument first: stamp that chain, read it on the L1 harness, and pre-register `complete_us`
p50 **and** p99 against the current build's A/A in the same run. Only an effect beyond that A/A goes to L3
(`rr-1`/`rr-16` with clean and `loss1` as guardrails), and no effect means a revert, exactly as §4.136 and
§4.125 went.

**Two criterion gaps stay on the list** rather than falling out of it: `timeline[clean]` at `--scale 1.0`
with TCP and peers in the same run, and an `l4-plain` baseline record so that `bulk-n` at 85 % can be
judged at all.

**Two things already judged, and not to be reopened.** Phase 2's latency is judged on `rr-16[l4-plain]`
(1.19–1.48 % resolution, 81 s) and moved nothing — it is a per-open saving, so only a churn cell can price
it (§4.119). And the eight lifecycle invariants are audited: six fully pinned, two partial, with two real
defects found and fixed on the way (#3's phantom accept, #1's live-id overwrite).

**And nothing about the clean cell may be claimed again until a measurement can resolve it.**

Three things stay open, deliberately and with their evidence:

- the lost readiness edge is **bounded, not fixed** — it is still between `drain_ready`'s bare
  `try_io` and the `select!` that follows it, and no visitor's latency depends on that round
  happening first;
- a lane that is *idle with a stream open* now pays up to 1 000 wake-ups a second, which is the
  poll's pure cost and is unmeasured. It belongs to the next milestone's `cpu_cores` and
  `rss_peak_mib` guardrail, and §4.101 says so;
- admission is still bounded by `MAX_BUFFERED`, not by the window (§4.42), and §4.104 says that stays:
  three settings were measured and each cost a shaped cell, because the buffer is the carrier's only
  shock absorber for a visitor link it cannot see. It is a **closed** defect, not an open one — the
  honest description is "the buffer is load-bearing", not "the buffer is wrong".

**Three objectives, and the third one is the D0–D5 plan** (2026-11-09). The objective that carried phases 0–3 was
marked complete at revision 18 (§4.160: four of six acceptance criteria met, one met at an indication, one
declined), and a completed goal cannot be re-armed. So the §12 extension lived in a **second objective**
(`goal-7b7c4139-cfb5-4c7f-8720-eb758ab9a4c2`, 120 rounds) whose five stages were P1 measurement → P2 instrument →
P3 coverage → P4 controller (conditional) → P5 closing, with §4.164 mapping each finding to its stage. That one is
**complete too, and its subject turned out to be the wrong one**: §12 asked whether a controller was worth
writing, and the answer to the defect it uncovered was left as "no change, mechanism named".

The **third objective** (`goal-baa3d2de-a96a-4be8-a5b3-30cd6801d18a`, 120 rounds) is the one that runs now, and
its text is the **D0–D5** plan that replaced the plan file on the same day:

| stage | what it produces | state (2026-11-09) |
|---|---|---|
| **D0** | the three copies agree — plan file, this file, the objective | **done** (`15e4d76`) |
| **D1** | per-lane instrumentation that can tell window, scheduler, acknowledgement, repair and counting apart | **done** (`cebd0bc`, `8e9a189`, `c0a5216`, `cdb98f5`): the `SendLedger` and eight tests |
| **D2** | attribution runs that **choose** the fix | **done** (§4.181–§4.185): the cause is out-of-order evidence, and the storm reproduces at `l4-mux8` |
| **D3** | **one** candidate, submitted and reverted alone | **B fixed and verified; A's three sides tried — two falsified, one that moved the target.** B (`27c9793`): `send_gso` sized from the first span and never checked that its tail was *shorter*, so every lane's opening run (a 17-byte credit plus the first stream packet) went out with `UDP_SEGMENT=17`, the kernel fragmented the stream packet, and the receiver refused it as `malformed_decode` — which is why number zero never arrived and the cumulative frontier pinned at zero for a lane's whole life. After: refusals 13 → 0, `first_seen` 0 on every lane, `own_below` advancing, and the `loss5` cell at 0.425/0.542/0.467 against a 0.3628 guard. A: repairs inside the window is **falsified at the unit level** (flight already counts the repair, so gating it waits for room only it can make, §4.212); a `cwnd/2` repair budget halves the shallow cell's drops and still misses by two orders (§4.213); halving on a declared loss fires **once in a lane's whole life** on that cell (§4.214) — so the loss signal there is too sparse to steer anything. Next: a delay-based ceiling (A″) |
| **D4** | the pre-registered criteria and guards (six cells, listed in the plan's §四) | **six cells measured, and the defect cell fails**: `loss5` at 51 % of its floor with a 1.4 s tail (§4.199); `rate100_shallow` behind both peers with two orders of magnitude more drops (§4.197); `rate100`/`rate1000` no longer reproducing their records (§4.197, §4.200); `bulk-n` on its floor (§4.196); `loss1`, `jitter` and `transport` hold (§4.196, §4.198, §4.201). These are the **before** numbers: once D3's fix lands, all six are re-run, and they are not a verdict until then |
| **D5** | `just fmt`/`check`/`interop`/`l3-accept`, both carriers' integration suites, capacity, soak, docs, changelog | waiting on D4 |

**The candidate, in one paragraph**, because this is the sentence the next session needs: a byte-range
acknowledgement — a separate optional `Ranges` frame naming up to four byte ranges the receiver holds above its
reported prefix, sent only when there is such data. The sender retires whole records covered by those ranges
(`flight` falls) and leaves the stream's contiguous `base` alone, so stream semantics do not move and a later
prefix cannot double-count a retired record. The predicted chain is `flight` down → `cwnd − flight` open → new
data admitted → the queue drains → acknowledgements land inside `loss_delay` → the timer stops over-declaring →
growth resumes. Its cell, primary metric, eight guards, two cross-guards and revert rule are in the plan under
"D3 预登记"; nothing about them is to be renegotiated after a run.

**Read the plan file, not this paragraph, when the two differ** — it is the approved document and this is a
summary of it. What §12 left behind, kept here because these are measurements and not a plan:

- **P1 done** (§4.165–§4.170) with one correction that mattered: the first three runs shaped the **visitor**
  leg, not the carrier's, and a window line reading `srtt=1000us` under a 100 ms condition is what caught it.
  Corrected, a 5 %-loss carrier path costs mhd about four-fifths of its throughput — 0.29 Gbit/s against 1.19
  clean, a level rather than a lottery.
- **P2 done** (§4.171): `transport` scenario, four declared metrics, bilingual rows, selfcheck 741 → 770. mhd's
  stream open is 3 µs against a TCP connect's 48, and its round trip is 2.5× the bare socket's.
- **P3 done** (§4.172): the listener's dispatchers and the admission cap now have tests that observe the
  release — the task ending and the socket handle going — rather than the request for it, and both are
  load-bearing. #7's task bound stays partial, recorded as structure rather than as measurement.
- **P4's gate looked met on one shape with the sign reversed** (§4.174): `rate100`/`rate1000` say the path limits,
  but on a 5 %-loss carrier path the counters said the window did — `cwnd` reaching 1.47 MB against a
  16 MiB ceiling, which at 200 ms is 236 Mbit/s against the 290 measured. **The "reaches only 1.47 MB" half is
  withdrawn** (§4.179): that reading is the last second of the bulk stream, and across the flow the window climbs
  steadily. What survives is that the failure is on the recovery side, not that the window never shrinks — a
  multiplicative decrease would make that cell worse.
- **P5 done** (§4.175): `just check`, `just interop`, `just l3-accept`, three full integration runs and twenty
  runs of the once-flaky cell, plus the TCP carrier's suite at 435 passed — all on the tree that carries the
  changes.
- **The lossy-path penalty is an unfixed defect** (§4.178). Doubling the flow from 60 s to 120 s did not raise
  throughput (0.2306 against 0.2886, single arm, so "not higher"). Two earlier framings — "accept and document"
  and then "it is a transient" — were mine and both were wrong; the cheap test that settled it should have come
  first.
- **P4 closed without a controller, and then its stated reason was withdrawn** (§4.176–§4.177, §4.179). §4.176
  built the shallow-queue cell (`rate100_shallow`) and it shows mhd dropping two orders of magnitude more than its
  peers while carrying 19 % less — so loss does follow its own rate. §4.177 showed the cheap recovery rules are
  arithmetically dead (a clean window at 5 % loss is 0.6 % likely at a hundred packets). What is **withdrawn** is
  the mechanism it named: the window is not frozen, it climbs ≈28 KB/s, and `cwnd` worth of acknowledgements
  arrives faster than once per round trip because **repairs ride outside the window**. So the next question is
  how much of the wire is bytes sent twice, and that is D1/D2's first job.

**§12 finished with every stage having an outcome, and the outcome was a question it declined to answer.** §4.172's
#7 task-count bound stays structural rather than measured, and no controller was written. That is exactly why the
third plan exists: "no change, mechanism named" is an honest end to a *plan*, and an unacceptable end to a
*defect* the product ships with.

**What is queued now, in order** (the plan's D-stages, with the evidence each one owes):

1. **D0 done** (`15e4d76`) — the plan file replaced, §4.179 recording both withdrawals, this section rewritten,
   `docs/internals.md` no longer calling the window rule "a property of the design … not a defect", and the
   objective restated as the D0–D5 plan.
2. **D1 done** (`cebd0bc`, `8e9a189`, `c0a5216`, `cdb98f5`) — the per-lane `SendLedger` on the window line, the
   first-numbered-packet log, the logged admitting refusal, and eight tests. It found two instrument defects on
   the way: `MAX_BUFFERED` refusals were counted as window refusals, and the first version of `ack` counted
   records instead of delivery.
3. **D2 done** (§4.181–§4.185) — `rtt100`, `loss5`, and `bulk-1`; the cause is that every lane's cumulative
   frontier is dead (number 0 never arrives) so retirement runs on 64 numbers and a per-stream prefix; the
   duplicate share is 8.5–37 % in a healthy run and 68.6 % in the storm, which reproduces at `l4-mux8`.
4. **D3 in progress** — pre-registration and design are committed and are the only authority for what "passes".
   The code is next: the `Ranges` frame, the receiver's report, the sender's retirement, the tests, then the
   pre-registered cell.
5. **D4 then D5** — the six acceptance cells and the gates; neither starts before D3 has an outcome.

**Two things this round must not repeat.** A plan that closes a defect without fixing it, and an entry that names
a mechanism from an aggregate row instead of from a counter. Both are in this file's record now; neither is a
reason to skip the work, and the D-stages are ordered so the second cannot happen again before the first is
tested.

And the release ritual is still not this: `timeline[clean]` at `--scale 1.0` is a 15.9-hour measurement (§4.143)
and `just bench gate` needs a sweep-shaped run. Both stay parked until a human asks for a release.

And the record has been corrected four times in this session for the same reason — a fact taken from my own
notes instead of from the code or a fresh measurement (§4.133's stale 45 µs, §4.141's two unverified
experiments, §4.157's wrong default, §4.179's end-of-stage maximum read as a plateau): the rule that keeps
earning its place is to check the artifact, not the record of it.

None of the three copies may drift between turns: this section, the plan file's stage table and the objective
say the same thing, or one of them is wrong — and when they disagree the plan's original wins, because it is the
approved document.

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

**Order, as of the 2026-11-06 rewrite — superseded in sequence by
`MHD-FINALIZATION-PLAN.md`, kept for the measurements and reasoning it carries.**
§4 carries the findings; this is the sequence only.
Rewritten on 2026-11-06 after §4.39's correction: the previous list opened with a
live-task count whose premise §4.37 had already refuted, and behind it a 12-worker
curve and a RACK rewrite that no measurement had asked for yet.

1. **⑥ the final-tree acceptance run** (§4.95 closed ⑤). The gate **passes** on the four-arm run:
   coverage 36 triples, endpoint invariant on 32 samples, 2 SLO cells gated, 0 drowned cells, drift and
   wedges checked, revision `v0.11.2-744-g7539b05` (regression half unrun — no baseline supplied).
   §4.94's flagged number also resolved **the other way from the flag**: `offered_gbps` tracks `carried`
   in every arm (molehill 7.48/7.51, frp 6.22/6.15, rathole 28.58/28.08, nps 0.13/0.14), so **rathole
   genuinely carries 3.8× mhd's throughput on the same unshaped cell**. Which completes the picture in
   one sentence: **where the shaper is the constraint mhd competes** (`loss1` p99 **1 046**, best of
   four), **and where the host is the constraint it loses** — the host-bound cost is §4.66's
   1.73–1.76 µs/packet, which the peers do not pay.
2. **A per-visitor spare channel stays refuted** (§4.20).
3. **B (RACK + PTO)**, judged on the shaped cells rather than the clean one: `loss1` and `loss5`
   report 1.06 s and 1.46 s of p99, which is where recovery, not setup, decides the number.
4. **C (priority scheduling): its condition is now met.** This item used to say C was conditional on
   "a `nobulk` run showing the bulk spine is what moves the interactive tail". **That run is §4.83,
   and it shows exactly that** (3.1–3.4 ms without bulk, 9–133 ms with). So C is scheduled, and it
   follows item 1, which says *where* inside the path the wait actually is; §4.82 already refutes
   the crude form (capping outstanding bytes), so what remains is ordering by class at the same
   total flight.
5. **D (delivery-rate) waits, and now for a measured reason.** Two estimators were built: one from
   the acknowledgement frontier (0.18–0.37 of the stage, reverted) and one from posted bytes
   (**0.91–1.05, §4.58, per lane, joining only when lanes are summed**). The socket delivers what
   is posted, so a pacer at the socket would pace a link that is not the constraint.
6. **Phase 6 (memory, CPU)** stays last and stays judged on `rss_peak_mib` and `cpu_s_per_gbit`
   against a same-round A/A.

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
