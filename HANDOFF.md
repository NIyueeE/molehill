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

1. **One series at a time** (§4.39). The staged cell's p99 is the interactive
   series alone, and the interactive and churn probes are the same code at nearly
   the same rate. Run the timeline three ways — interactive only, churn only, both
   — with everything else equal. If the interactive p99 rises when churn shares the
   host, the tail is inter-probe contention; if it does not, the 22.5 ms is not the
   tail this file has been chasing and needs a different account. **Nothing below
   starts before that is answered.**
2. **One stream, one hop** (the first structural change). A visitor starts on one
   stripe (`initial_stripes()` is 1 in production) and still runs the whole group
   machinery: coordinator task, send loop, receive loop, one reader task per
   stripe, a bounded channel between them, a grace timer and a monitor. A single
   stripe needs none of it. `spawn_single_group` drives both directions from one
   task over the same 10-byte frame and the same `StripeSender`, so the first
   version changes no wire byte. Pass: `timeline[clean]` and `churn-16` move beyond
   the run's own A/A and materiality; rollback: any rr, bulk, capacity, RSS or CPU
   regression.
3. **A lane root that is actually usable** (phase A: implemented, default off).
   The switch stays off until the multi-stripe wedge under `MOLEHILL_MHD_SEAL=1` is
   root-caused with a two-end join on stream, direction and packet — not until the
   design reads well. §4.10's and §4.13's numbers decide its default afterwards.
4. **Native unreliable datagrams** for UDP. This dialect declares no datagram frame
   (`wire.rs` says so), so a UDP service is framed into a reliable stream and pays
   an ARQ, a route map, two worker queues and a Noise record for a packet that must
   not be retransmitted at all. Only after 2 and 3, and never as a second fallback.
5. **A per-visitor spare channel stays refuted** (§4.20). A fixed small reserve
   refilled only when the pool is empty is a *different* experiment and is unrun; it
   is not in this list until 1 says the pool is the constraint.
6. **B (RACK + PTO)**, judged on the shaped cells rather than the clean one:
   `loss1` and `loss5` report 1.06 s and 1.46 s of p99, which is where recovery, not
   setup, decides the number.
7. **C (priority scheduling) is conditional, not scheduled.** `OPEN_RESERVE` buys a
   *write* budget, not a packet-scheduling guarantee, so the question is open — but
   it is worth building only if a `nobulk` run (`streams: 0`) against the same
   timeline shows the bulk spine is what moves the interactive tail.
8. **D (delivery-rate)** stays last: the clean cell never fills the window.
9. **Phase 6 (memory, CPU)** stays last and stays judged on `rss_peak_mib` and
   `cpu_s_per_gbit` against a same-round A/A.

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
