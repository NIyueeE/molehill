# HANDOFF: Working State & Future Work

> **State as of 2026-09-26.** The v0.10.0 theme is implemented on
> `feat/session-and-pool`: **M1** (one control session per endpoint, protocol
> v4), **M2a** (one shared elastic pool per carrier, plus the S1 observation),
> **M6** (the configuration surface) and **M7** (`direct`'s role, measured) are
> in, and the measurements this section records were taken on that branch.
> `main` is at `ab0bf11` (v0.9.0 released, with the withdrawn v0.9.1 cycle
> folded back into development). **Nothing here is merged yet**; the release
> sweep, the PR and the tag are the remaining steps (see "Release (v0.10.0)").
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).
>
> **This file owns the working state**: what is open, what was decided and why,
> and the measurement records of this cycle. Per AGENTS.md §3 it is a
> contributor page — user-facing facts belong in the docs pages, and anything
> released belongs in CHANGELOG.md.

## Open: striping with the elastic pool

**A stripe group's bulk path deadlocks when its channels are opened while the
pool is still growing**, so a v4 session is served **unstriped**: the server
logs one warning naming `[server.data].stripe_count` and uses one channel per
visitor. `tests/integration_test.rs::striped_data_channels` is `#[ignore]`d with
a pointer to this section, so a plain run reports `19 passed; 1 ignored`.

What is known, from a falsification matrix on this branch (each row three runs of
`cargo test --test integration_test -- --test-threads=1 striped_data_channels`):

| Configuration | Result |
|---|---|
| the branch before this milestone (pre-opened channels) | 3/3 pass |
| cold pool, `stripe_count = 1` | 3/3 pass |
| one warm tunnel, no pre-opened channel | 3/3 pass |
| cold pool, the losing cold-grow opens staggered by 2 ms | 3/3 pass |
| cold pool as it ships | **0/3 — "striped bulk echo read timed out"** |

The trace: the visitor is paired with a complete 4-stripe group and both sides
log "stripe group started"; the group's receive direction ends after **one**
frame, the stripe logs "stripe group finished" milliseconds later, and the
visitor's read then waits for a sequence a broken channel will never carry. The
mux framing counters freeze with it. Nothing in the log names a channel the
*client* closed — which is where the next attempt should look first (the
client's `StripeGroups` registration and the `open_stream` burst it sits on),
because the only variable the matrix isolates is simultaneity: a burst of K
opens differs from the same opens 2 ms apart.

Fixed on the way there and kept: a concurrent first open waits for the growth in
flight (`await_growth`, bounded by `GROW_WAIT_BUDGET`) instead of racing past it
and failing with "the pool has no tunnel"; a refused growth holds growth off for
`GROW_FAILURE_COOLDOWN`, and a tunnel's death or a shrink releases the hold; a
server shutdown really ends its sessions now (`registry.v3`/`v4` cleared,
`MultiMap::clear`, the shared shutdown stops the KCP listener), where an
in-process "restart" used to keep serving through a ghost session; and a
reconnect drops its stale tunnel pools.

**What would falsify the quarantine**: a green `striped_data_channels` on the
cold pool in three consecutive runs, with the `#[ignore]` removed. A fix has to
explain the simultaneity difference — that is the one variable the matrix
isolates, and a fix that only re-orders the burst (rather than removing the
channel that dies with it) will not survive the third run.

A second, related open thread: the group's K channels are requested one at a
time (`CreateDataChannelFor` per channel), so the client cannot reserve K
**distinct** tunnels for one group (D24/D29). A `CreateStripedGroupFor(group,
count)` command — the server names the group once, the client reserves K tunnels
and answers with K prologues carrying `StartForwardStripedTcp(group, i, K)` —
would make the guarantee structural and is the natural companion to whatever
fixes the deadlock. It needs the stream prologue to carry the command, not just
the service id, because a pooled stream has to say which group and index it is.

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
| M1 | One control session per endpoint (protocol v4) | **landed** | below, "M1" |
| M2a | Shared elastic pool + S1 observation | **landed** | below, "M2a" and "S1" |
| M2b | S2 placement + D28 spare selection | **not landed, on purpose** | the S1 spread is zero — "S1" below |
| M2c | UDP shortest-queue assignment (D27) | not landed | gated on the drop counters, which stayed at zero |
| M3 | Transparent visibility (health check deleted) | landed (merged) | `CHANGELOG.md`, the dead-backend test |
| M4 | IPv6 path MTU | landed (merged) | the `#[ignore]`d netns test |
| M5 | Log model | landed (merged) | `tests/log_budget_test.rs` |
| M6 | Configuration surface | **landed** | below, "M6" |
| M7 | `direct`'s role | **measured** | below, "M7" |

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
| D24 | A stripe group's K streams must land on K distinct tunnels — achieved today by back-to-back reservation; the wire cannot express a group yet (see the open thread) |
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

### M1 — one control session per endpoint (protocol v4)

Commits `a68df08` (server half) and `81bd60a` (client half). One authenticated
control session per `(remote_addr, effective transport)` carries every service
that dials it; each registration carries its own credential
(`digest(service_token ‖ nonce)`), so a rejected service is rejected alone. The
server declares its heartbeat cadence in `Ack::SessionOk`; the client derives its
timeout and refuses a config below the derived floor as soon as the cadence is
known (not earlier: the cadence is the server's to declare). Every client-origin
data channel and tunnel stream names its service with a four-byte prologue; a
stream naming an unregistered service, or a second service on a tunnel, is
dropped alone (warned once, then DEBUG).

**Verified**: 140 lib, 20 integration, 7 `session_test`, 2 log-budget, and
`just interop` against the released v0.9.0 binary — all three cases: the old
client still forwards through the new server, the old server refuses the new
client and the client reports `protocol v4` instead of retrying, and an unknown
dialect is refused on that connection alone.

**Two defects found while building it, both pinned by tests**: the server's v3
branch answered with `CURRENT_PROTO_VERSION`, so flipping the constant would
have told a v0.9.0 client the server speaks v4 (fixed to `PROTO_V3_VERSION`,
`the_server_still_answers_a_v3_hello_in_v3`); and a session's writer is one queue
for every service, so a sibling's command can land between a registration and its
verdict — reading that as a framed ack misparsed 5 bytes as 512 and stalled the
session. The verdict reader dispatches on the first byte instead, which is only
sound while an ack stays under 256 bytes, so the writer shortens an oversized
rejection reason (`MAX_REJECTION_REASON_LEN`, pinning test).

**Landmine worth remembering**: `read_control_cmd` and `read_ack` are
fixed-width readers. Any new command variant with a payload must be
tag-dispatched or carry a fixed-width id, or the reader desyncs.

### M2a — one shared elastic pool per carrier

Commit `d10e566` (+ its fixups). `shared_pool = false` keeps exactly today's
per-service pool (asserted, not assumed); `true` serves every service of a
session from one pool per carrier. Placement is least-loaded with the
reservation charged before the first `await`, so back-to-back opens land on
distinct tunnels while the pool has them. Growth: cold, load above 80 % of the
pool's stream capacity, an open that waited too long, or the UDP floor. Shrink
only when the whole pool has no streams, no pending opens and no pinned peers
and has been idle past `idle_timeout`, with a warm hold and a cooldown. A refused
growth holds growth off (D14); a tunnel's death or a shrink releases the hold.

**First visitor after the pool shrank**: 2.15 / 2.01 / 2.08 ms (three runs;
earlier three 2.07 / 3.23 / 1.99 ms), debug build, loopback, one service, no
pre-opened channel — i.e. the cold path: visitor accepted, one channel
requested, one stream opened on the surviving tunnel. Five single measurements
quoted as a range, not a distribution.

**Tests**: `tests/pool_test.rs` (6: shared pool serves two services, the
per-service default keeps two pools, the UDP source port across a grow/shrink,
a cold pool grows under load and shrinks when idle, the telemetry is opt-in from
a real binary, the server valve refuses growth without killing the session),
plus pool unit tests for distinct placement, reuse when the pool is smaller than
the demand, the pinned-tunnel shrink gate and the failed-growth hold.

### M6 — the configuration surface

Commit `82e741e`. Six keys removed with a warning that names the replacement
(`REMOVED_KEYS` now carries the version that removed each key, so `health_check`
reports v0.10.0 rather than the v0.9.1 tag that never shipped), two added
(`udp_workers`, `max_tunnels_per_client`), and the UDP-only keys on a TCP service
are errors instead of being silently ignored.

**Four defects the cold pool exposed, all fixed**: a tunnel refusal could not be
reported on the fixed-width ack path (new unit `Ack::TunnelRefused`, pinned by a
protocol test and proven end to end); a server shutdown did not end its sessions
(clearing both registries and `MultiMap::clear` — an in-process "restart" used to
keep serving through a ghost session, which the pre-opened channels had hidden);
a reconnect reused pools whose tunnels carried the previous session's nonce; and
a concurrent first open raced the growth it needed.

**Evidence**: 19 integration (1 ignored, the striping defect), 6 pool, 7
session, 2 log budget, 140 lib; both clippy passes; the docs gate.

### M7 — what `direct` is for

One interleaved run, `--ab-variants direct,shared` on **one binary**
(`workload_version` 2, `SOAK_SLOW_VISITOR_BPS=2000000`, clean loopback, 8 load
steps, the slow visitor completed 16/16 arms): the question is whether one slow
visitor's stream delays the interactive stream that shares its pool, and what
`direct` costs or saves.

| step | direct p99 (ms) | shared p99 (ms) | direct Gbit/s | shared Gbit/s |
|---|---|---|---|---|
| 1 | 1.93 | 3.57 | 17.95 | 8.64 |
| 2 | 3.77 | 4.62 | 24.54 | 18.77 |
| 3 | 3.63 | 5.32 | 21.93 | 17.50 |
| 4 | 4.91 | 8.35 | 23.25 | 19.30 |
| 5 | 9.75 | 8.21 | 29.79 | 19.44 |
| 6 | 14.14 | 14.75 | 23.02 | 22.83 |
| 7 | 17.59 | 14.49 | 19.66 | 23.17 |
| 8 | 22.42 | 15.54 | 17.02 | 18.92 |
| median | **7.33** | **8.28** | — | — |

**Reading.** The interactive p99 medians differ by ~1 ms inside a step-to-step
spread of 1.9–22.4 ms, so nothing here is claimable either way: `direct` is
lower on the four lightest steps and the shared pool is lower on the two
heaviest. Throughput is **directional, not a claim** (`direct` ahead on 5 of 8
steps, `shared` on 1, past the 15 % threshold only where the shared arm's cold
pool dominates step 1: 17.95 vs 8.64 Gbit/s). So the shared pool does **not**
lose in any claimable way, `direct` keeps only its documented role (sparse
visitors and the measurement control arm), and no configuration guidance
changes. The step-1 gap is the cold pool, not the pool's design: the shared arm
pays one tunnel establishment the first visitor would pay on a fresh service.

### S1 — the placement observation (and why S2/D28 do not land)

The pre-registered kill criterion: run the mixed workload (interactive, 20 bulk
streams, churn, UDP) over `clean`, `loss1`, `rtt100` with
`MOLEHILL_PLACEMENT_STATS=1` and `MOLEHILL_POOL_STATS=1`; if the spread between
candidates is inside the noise, S2 and D28 do not land.

**The spread is not merely inside the noise — it is zero.** 2687 placements
across 183 one-second intervals: mean best-candidate load 0.437 stream slots,
mean worst-candidate load 0.437, **mean spread 0.000**, fallbacks 0, weighted
mean open latency 36 µs (max 6907 µs — the cold-pool dial).

**Why**, from the same run's pool timeline: the pool stayed at **one tunnel**
carrying up to 21 streams; it grew to two only for the UDP floor
(`+udp_floor:1->2`). The growth rule fires above 80 % of the pool's stream
capacity — 51 streams at size 1 — and the mixed workload peaks at 21, so
candidate choice never had a lever: every candidate a placement could pick was
equally loaded by construction. Placement policy is therefore not the axis that
would improve anything here; *pool size* is, and the elastic rules own that.

**Instrument defect found by taking the measurement**: `best` was overwritten
with the chosen tunnel's load *after* the open, so `worst − best` came out
negative on a single-tunnel pool (mean −1.0 slots on the first run). The
candidate snapshot is what the spread must be measured against; fixed before the
recorded run (the numbers above are from the corrected instrument).

### The v0.10.0 release sweep

To be recorded here after the sweep: the run's provenance (`revision`,
`tree_clean`, `stale`, the binary fingerprint), `just soak-check`'s verdict, the
self-check's SLO numbers, and the comparability against v0.9.1 (same host) and
v0.9.0 (different host — the interleaved `--ab` screen is the evidence that
travels). The withdrawn v0.9.1 file stays in the tree until then, because it was
measured on this host and from this source, which makes it the only same-host
baseline the delta can be read against.

## Release (v0.10.0)

1. Freeze: `chore(release): prepare v0.10.0` — `version = "0.10.0"`, the
   `[Unreleased]` content moved under `## [0.10.0] - <date>`, and the withdrawn
   `results-soak-v0.9.1.json` + `assets/soak-v0.9.1*.png` deleted.
2. Fresh sweep on the frozen commit, quiet machine, `cargo build --release`
   first: `just soak --test=rrul --tools molehill,frp,rathole,nps --out
   benches/scripts/soak/results-soak-v0.10.0.json` (~80 min).
3. `just soak-plot` → `assets/soak-v0.10.0*.png`; README (+zh) numbers;
   `just soak-check`.
4. `just check`, `just interop`, then push the branch and open the PR.
5. CI green → merge (merge commit) → on `main`: `just tag` → push the tag →
   the release workflow publishes.

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
(local and remote) were deleted, crates.io never received the version, and the
branch went back into development. The mistake was not judging M1 large; it was
turning that judgement into a *release* without asking the person whose plan it
is — a release is a deliberate act (AGENTS.md §5), and "continue the plan, then
tag" is not a licence to redefine what the plan contains. The GHCR images were
removed as part of the withdrawal (see the note above); the only trace left is
this paragraph and the deleted-file history.

## Open threads for the next cycle

- **Striping** — the deadlock above, and the group command that would make D24
  structural (first item in this file).
- **M2b/M2c (S2, D28, D27)** — do not land on this data: the spread is zero and
  the UDP drop counters stayed at zero. Re-open with a *pool-size* question
  (does growing earlier than 80 % help a mixed workload?) rather than a
  placement question.
- **The v3 server path** — kept only for old clients, and now dead weight: it
  must not acquire features, and removing it is a future cycle's work. It owns
  the only remaining `pool_size` on the wire (`ServiceRegistration`).
- **The method revision** (recorded, not fixed): the host key is the container
  hostname, so two runs on the same hardware never compare; a single sample per
  stage cannot resolve a 25 % change when the within-run spread is 40-70 %; and
  the 64-stream scale point is single-rep, which makes that cell structurally
  undecidable.
- **The config-test gaps** still open from the v0.9.0 audit: `allow_ports`
  rejection end to end, per-service `token` resolution, the UDP knobs'
  documented effects, a PSK handshake, hot-reload add/delete/modify, and
  `--genkey` curve behaviour.
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench/test entries with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).
- **`MOLEHILL_TCP_BUFFER_BYTES`** is referenced by a doc comment in
  `src/stripe.rs` but implemented nowhere — a dangling reference to a switch
  that no longer exists.

## Environment notes (this host, re-checked 2026-09-26)

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
- **This host is `16b4dc8db68b`**, the same host as the withdrawn v0.9.1 sweep,
  and the source has not moved since it (`git diff 0072098..HEAD -- src/
  build.rs` was empty at the branch point) — which is what makes that run a
  same-host, same-source baseline for this cycle's delta.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.

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
