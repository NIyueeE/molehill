# HANDOFF: Working State & Future Work

> **State as of 2026-09-30.** The v0.10.0 theme is implemented on
> `feat/session-and-pool`: **M1** (one control session per endpoint, protocol
> v4), **M2a** (one shared elastic pool per carrier, plus the S1 observation),
> **M6** (the configuration surface) and **M7** (`direct`'s role, measured).
> `main` is at `ab0bf11` (v0.9.0 released, with the withdrawn v0.9.1 cycle
> folded back into development). **Nothing here is merged yet.** PR #4 is open
> and re-green after each push; what is left is the human checklist: the
> repo-settings items and the tag itself.
>
> **This file owns the working state**: what is open, what was decided and why,
> and the release checklist. Per AGENTS.md §3 it is a contributor page —
> user-facing facts belong in the docs pages, and anything released belongs in
> CHANGELOG.md. The cycle's closed records (incidents, superseded sweeps,
> measurement records, retracted diagnoses) live in
> [HANDOFF-archive.md](HANDOFF-archive.md), which this page indexes; the
> archive was split out on 2026-09-30 because the live state had become a third
> of a 1,600-line file.
>
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).

## Where this stands

- **The branch is feature-complete.** Every milestone of the v0.10.0 theme is
  in; the open threads at the freeze are the three at the bottom of this page,
  and none of them blocks the release.
- **The branch is merged and v0.10.0 is ready to re-tag.** PR #4 merged as
  `dffd7d8`. Two tag pushes have failed — the emulated legs, then the musl leg —
  each on a test defect, with nothing published either time; both are fixed, the
  sweep is re-run on the fixed commit (`96bda00`), and the tag is deleted so the
  next push is the deliberate re-tag. See "Release (v0.10.0)" below.
- **The release state is set**: `version = "0.10.0"`, `CHANGELOG.md` carries
  `## [0.10.0] - 2026-10-01` with `[Unreleased]` empty, and the benchmark
  ritual's artifacts are committed (`benches/scripts/soak/results-soak-v0.10.0.json`,
  the chart set in `assets/`, both READMEs refilled).
- **Only docs and assets may follow a sweep.** `githooks/pre-tag` reads the
  results file's recorded revision and fails the tag if `src/`, `tests/`,
  `Cargo.*`, `build.rs` or `benches/scripts/soak/*.py` changed since — the
  changelog date and the README numbers may, code may not (AGENTS.md §10,
  "prove provenance").
- **The repository-settings items are a human's to make** — branch protection
  for `main` and the release secrets. They are tracked as an open thread below;
  nothing in this repository can enable them.

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
   Each failure was fixed, the sweep re-run, the tag deleted and re-pushed —
   which is what AGENTS.md §5 allows, and the reason the rule exists.
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

## Open threads for the next cycle

- **Replacing the `udp_batch` FFI waivers (`quinn-udp`) is an A/B, not a
  cleanup.** This is the decision record behind the one place where
  `docs/lint-policy.md`'s "fix the code first" rule runs into "no equally
  reasonable alternative" — the eight `unsafe_code` expectations in
  `src/transport/udp_batch.rs` (the `recvmmsg`/`sendmmsg` batching FFI):
  - the raw syscalls have no `std` equivalent; `socket2` (already a dependency)
    covers everything *except* the batching calls;
  - **`nix` does not remove them.** Its `MultiHeaders<S>` holds
    `Box<[libc::mmsghdr]>` (raw pointers inside) and is therefore itself
    `!Send`/`!Sync`, so the three `unsafe impl Send`/`Sync` proofs — the part
    that carries the real soundness argument — would still be required, at the
    cost of a new dependency and a per-call `Vec<IoSliceMut>` allocation in the
    hot path. Verified against `nix` 0.31.3's source, not assumed;
  - **`quinn-udp` could remove all eight** (`UdpSocketState` is a plain
    `Send + Sync` struct whose `recv`/`send` take caller-owned slice buffers),
    but it also brings GSO/GRO segmentation, i.e. it changes the send path's
    syscall shape. That is a measurable change to the KCP data path, so it
    belongs in its own `just soak --test=screen` run against the current
    batching rather than in a lint cleanup.
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench/test entries with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).

## Environment notes (this host, re-checked 2026-10-01)

- **Verify `iperf3` before a long run** (`command -v iperf3`). It is installed
  at runtime rather than baked into the image, so a container restart drops it:
  measured on 2026-10-01, when a restart (new hostname, uptime reset) left the
  binary gone and a sweep produced an artifact of typed failures in 14 seconds.
  The bench fails cleanly — every test records `No such file or directory:
  'iperf3'` and the gate refuses the file — but it looks like a finished
  artifact until someone reads it.
- **`/tmp` is periodically wiped.** Keep `--out` and logs under `~/tmp` or the
  repo. The Soak harness's own work directories (`/tmp/molehill-bench.*`) are
  normal residue and are never deleted by the harness.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python
  child, which keeps executing and holds the bench lock.
- **Peers are cached** in `~/tmp/bench-peers` and the interop binary in
  `~/tmp/interop`, so `just soak` and `just interop` normally need no network.
  `just soak-peers` re-resolves each peer to the **latest GitHub release** and
  re-downloads when the cached version is behind; the resolved versions are
  recorded in the results meta beside the numbers.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.
- **This host is `3f8b4508ab91`**: 20 cores, 23 GB RAM, kernel
  `6.12.0-160000.38-default`. `/etc/machine-id` is **absent**, so the Soak
  harness's `host_id` hashes `cpu model + core count` only
  (`host_id_basis.machine_id: false`) — *not* the hostname. The container's
  hostname changes between sessions without moving `host_id`, which is the
  point: the comparable runs in this cycle's files share `host_id
  d764f9da9c7e5b2a`, and a file *without* a `host_id`
  (`results-soak-v0.9.0.json`, `98c48ea3fa68`) falls back to the hostname, so
  the gate refuses it.
- **A baseline-less run is the norm here**: the gate's own self-check
  (completeness, endpoint invariant, absolute SLO) is what a fresh sweep is
  gated on, because every stored baseline either has no `host_id` or predates
  the current method. The v0.9.0 file is still compared where the method keys
  agree, and the gate names what it could not verify rather than assuming it.

## Archive

Everything this cycle closed is in [HANDOFF-archive.md](HANDOFF-archive.md),
in the order it happened, with an index at the top of that page: the incidents,
the superseded sweeps, the measurement records, the retracted diagnoses, and
the closed threads of this cycle. The records that predate the 2026-09-28
freeze were archived into git history earlier and are indexed there too
(`git show f2156de^:HANDOFF.md`).

The rule that keeps the split honest: **a record in the archive says what the
branch's authors believed at the time, and no number in it may be quoted as a
measurement of the current code, compared against a Soak result, or used to
gate anything.** The live numbers are the sweep record under "Release
(v0.10.0)" on this page.
