# HANDOFF: Working State & Future Work

> **State as of 2026-09-29.** The v0.10.0 theme is implemented on
> `feat/session-and-pool`: **M1** (one control session per endpoint, protocol
> v4), **M2a** (one shared elastic pool per carrier, plus the S1 observation),
> **M6** (the configuration surface) and **M7** (`direct`'s role, measured) are
> in, and the measurements this section records were taken on that branch.
> `main` is at `ab0bf11` (v0.9.0 released, with the withdrawn v0.9.1 cycle
> folded back into development). **Nothing here is merged yet.** The freeze and
> PR #4 are done; the release audit found three gaps (a config-docs
> contradiction, the sweep's provenance, and a completeness gate that could not
> see a dead stage spine) and all three are closed on the branch. The gate then
> did its job: it failed the shipped sweep's dead `rate20` cell, that cell was
> traced to three harness defects over 2026-09-29, and the re-sweep on the fixed
> harness is **green with no waiver** — see "Release sweep (2026-09-29)". What
> is left is the human checklist: the repo-settings items and the tag itself.
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).
>
> **This file owns the working state**: what is open, what was decided and why,
> and the measurement records of this cycle. Per AGENTS.md §3 it is a
> contributor page — user-facing facts belong in the docs pages, and anything
> released belongs in CHANGELOG.md.
>
> **Update 2026-09-29.** The dead bulk spine is fixed: the drain was reading
> `tc`'s backlog wrong, the spine had a single dial, and the drain budget was
> shorter than the 106 s flush it waits for. All three are in "The dead bulk
> spine: three defects, and what the third one is not", and the sweep they
> produced carries all 32 tool-stages. Two earlier diagnoses recorded here —
> tunnel liveness, and the backend leg — were **falsified by measurement** and
> are retracted in place.

## Fixed: striping with the elastic pool

**The stripe livelock had two causes, both fixed.** The quarantine above
(a v4 session served unstriped, `striped_data_channels` `#[ignore]`d) is
lifted: the test runs in the suite now and the server serves
`stripe_count > 1`.

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


## The v0.10.0 theme

One control session per endpoint, one shared elastic pool per carrier,
transparent visibility and a quiet log. Four of the milestones were independent
and are already merged on `main` (unreleased, so they are part of this release);
the rest ship as **v0.10.0**, because the wire protocol and the configuration
surface both change.

### Milestones, status and evidence

| # | Milestone | Status | Evidence |
|---|---|---|---|
| M0 | Interop matrix | landed (merged) | `tests/interop_test.rs`, `just interop` |
| M1 | One control session per endpoint (protocol v4) | **landed** | archived record, "M1" |
| M2a | Shared elastic pool + S1 observation | **landed** | archived records, "M2a" and "S1" |
| M2b | S2 placement + D28 spare selection | **not landed, on purpose** | the S1 spread is zero (archived, "S1") |
| M2c | UDP shortest-queue assignment (D27) | not landed | gated on the drop counters, which stayed at zero |
| M3 | Transparent visibility (health check deleted) | landed (merged) | `CHANGELOG.md`, the dead-backend test |
| M4 | IPv6 path MTU | landed (merged) | the `#[ignore]`d netns test |
| M5 | Log model | landed (merged) | `tests/log_budget_test.rs` |
| M6 | Configuration surface | **landed** | archived record, "M6" |
| M7 | `direct`'s role | **measured** | archived record, "M7" |

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
| D27 | UDP assigns a *new* peer to the shortest worker queue — gated on the drop counter, which has stayed at zero under every measured load |
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

## Measurement records (v0.10.0 cycle)

**Archived.** Every measurement record this cycle produced — M1 (protocol v4),
the stream-cap leak investigation, M2a, M6, M7, the S1 placement observation,
both post-review rounds, the cycle's four sweeps and the CI-verification
incident — lives in git history in the revision *before* the one that archived
it (`f2156de chore(release): re-sweep v0.10.0 on the release commit`):

```
git show f2156de^:HANDOFF.md
```

Named by revision rather than by the `v0.10.0` tag on purpose: the tag lands on
a commit that already carries this shortened page, so `git show
v0.10.0:HANDOFF.md` returns the index, not the records.

Per this file's own rule (kept verbatim below the historical-records index) those
records say what the branch's authors believed at the time and why a decision
was taken; **no number in them may be quoted as a measurement of the current
code, compared against a Soak result, or used to gate anything.** The live
numbers are the sweep record for the release commit, in "Release (v0.10.0)"
below. What each archived record settled, so it can be navigated:

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

## Release (v0.10.0)

1. ~~Freeze~~ **done (2026-09-28)** — `b305394 chore(release): prepare
   v0.10.0`: `version = "0.10.0"` set, the `[Unreleased]` content moved under
   `## [0.10.0] - 2026-09-28`, `[Unreleased]` left empty, the withdrawn
   `results-soak-v0.9.1.json` + `assets/soak-v0.9.1*.png` deleted.
2. ~~Re-sweep~~ **done (2026-09-30, at `c265af3`)** — the sweep the artifact
   and both READMEs in this commit were measured by: four tools, 8/8 stages
   each plus the capacity ramp, `--test=rrul,capacity`, the bounded rate-class
   window, `just soak-check` green with no waiver. It supersedes the
   2026-09-29 sweep at `8ed32dd`, which described code two commits back.
   `githooks/pre-tag` reads the results file's recorded revision and passes for
   this commit: docs, assets and the artifact itself may follow a sweep, code
   may not. The measurement record is the subsection below.
3. Before the tag: the `[0.10.0]` changelog date is the tag day, and
   `just tag-check` must be run on the release commit (it now passes — the
   artifact is measured at `c265af3` and only docs/assets followed).
4. `just check`, `just interop`, then push the branch and open the PR. (The PR
   exists and is re-green after each push.)
5. CI green → merge (merge commit) → on `main`: `just tag` → push the tag →
   the release workflow publishes.

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

## Open threads for the next cycle

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
  a placement rule nor a larger cap. D27's UDP half stays gated on the drop
  counters, which are still zero.
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
- **The host calibration certifies CPU state, not the loopback path** (found
  2026-09-30, numbers in "Release sweep (2026-09-30)"). Two container instances
  of the same `host_id` differed by 25-39 % on the clean bulk cells of the two
  arms that reach the loopback ceiling, while frp and nps were flat and the
  calibration probe read 414.0 against 421.2 MiB/s — 1.7 % apart, inside the
  25 % the gate allows. Stable and blind at once: it catches a busy or throttled
  CPU, not the machine property the fast cells are bounded by. Options, none
  measured yet: a loopback-ceiling probe measured *outside* the tools (the
  128 MiB socket pair drifts 18.7 % across median-of-five readings, which may be
  too noisy to be a key); gating on the run's own first `clean` reading against
  the baseline's (circular for a cross-tool claim, sound for cross-run
  comparability); or stating that only same-instance runs compare and dropping
  the calibration's claim to more than CPU state. Decide before the next sweep:
  this one's numbers describe one instance.
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
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench/test entries with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).
- ~~**`MOLEHILL_TCP_BUFFER_BYTES`**~~ — **closed, and the note was stale.** The
  switch never existed; its doc-comment reference went with `d405360`, and the
  `diag_env` illustration the note still pointed at is gone too (the list there
  is `MUX_STATS`/`KCP_STATS`/`POOL_STATS`/`PLACEMENT_STATS`/`UDP_STATS`/
  `STRIPE_COUNT`). Nothing survives anywhere in the tree.
- **An instance that fails to start is silent** (found 2026-09-30 while writing
  the config tests, **not fixed**). `src/lib.rs::run` spawns `run_instance` and
  only ever observes its `Result` when a *later* general config change triggers
  a restart, so a failure at startup leaves the process running with nothing
  listening and **not one line of output**. Reproduced twice: a
  `local_private_key` that is not valid base64, and an `allow_ports` range whose
  start is above its end. The documented contract is the opposite ("refused
  before the start"), so this is a code bug, not a doc one; the fix (observe the
  first instance's early exit, log it at `ERROR`, exit non-zero) touches the
  hot-reload loop, which is why it is filed rather than folded into a test
  change.

## Environment notes (this host, re-checked 2026-09-28)

- **Verify `iperf3` before a long run.** The container's apt layer has dropped
  the package mid-session before; the bench then fails cleanly (every test
  records a typed error) but spends an hour producing nothing.
- **`/tmp` is periodically wiped.** Keep `--out` and logs under `~/tmp` or the
  repo. The Soak harness's own work directories (`/tmp/molehill-bench.*`) are
  normal residue and are never deleted by the harness.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python
  child, which keeps executing and holds the bench lock.
- **Peers are cached** in `~/tmp/bench-peers` (frp 0.71.0, rathole 0.5.0,
  nps 0.26.10 — all still the latest releases) and the interop binary in
  `~/tmp/interop/v0.9.0`, so `just soak` and `just interop` need no network.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.
- **This host is `a093c5fbe0dc`** (re-checked 2026-09-28): 20 cores, 23 GB RAM,
  kernel `6.12.0-160000.38-default`. `/etc/machine-id` is **absent**, so the
  Soak harness's `host_id` hashes `cpu model + core count` only
  (`host_id_basis.machine_id: false`) — *not* the hostname. That is why the
  `16b4dc8db68b` → `a093c5fbe0dc` rename did not break comparability: the
  frozen-commit sweep and the earlier v0.10.0 file share `host_id
  d764f9da9c7e5b2a` and are comparable, which is what the same-host delta table
  in "The release sweep on the frozen commit" rests on. The files that are
  skipped are the ones *without* a `host_id` — `results-soak-v0.9.0.json`
  (`98c48ea3fa68`) — where the gate falls back to the hostname and refuses.
- **A baseline-less run is the norm here**: the gate's own self-check
  (completeness, endpoint invariant, absolute SLO) is what a fresh sweep is
  gated on, because every stored baseline either has no `host_id` or predates
  the current method.

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
