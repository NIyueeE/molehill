# HANDOFF: Working State & Future Work

> **State as of 2026-10-01.** **v0.10.0 is released.** PR #4 merged into
> `main` (merge commit `dffd7d8`), the tag `v0.10.0` published a GitHub
> Release, the GHCR image and crates.io, and the branch that carried the work
> (`feat/session-and-pool`) is deleted — `main` is the only line now. The theme
> is **M1** (one control session per endpoint, protocol v4), **M2a** (one
> shared elastic pool per carrier, plus the S1 observation), **M6** (the
> configuration surface) and **M7** (`direct`'s role, measured).
>
> What is left is not code: the repository-settings items a human has to make,
> and the three open threads at the bottom of this page.
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

- **An unreleased cycle is in flight** on `feat/transparent-l3`: transparent
  (L3) services, wire **v5**, based on `main` at `cf01091`. Nothing of it is in
  `v0.10.0` (verified: the tag's tree has no transparent code at all), so its
  configuration surface is still being shaped in place. The decision record,
  the model it is being reshaped into, and the header-compression question are
  in "The transparent-L3 cycle" below.

- **v0.10.0 shipped on 2026-10-01**: the release run is green on all thirteen
  jobs (nine platform builds, GitHub Release, GHCR, crates.io), and the
  released artifact is `benches/scripts/soak/results-soak-v0.10.0.json` —
  measured at `96bda00`, gated by `just soak-check` with no waiver.
- **The release cost four tag pushes, and the three failures are worth
  remembering**: the emulated cross legs (`native_target` guard missing on a
  new spawn-based test), the musl leg (an assertion pinned to glibc's wording),
  and the `aarch64` cross leg (the release matrix's cache shared across legs
  with different glibc). The first two are in `tests/` — which forced the sweep
  to be re-run on the commit that carries them — and the third is in
  `release.yml`. All three are fixed; the open thread at the bottom of this
  page proposes the check that would catch the next one mechanically.
- **The repository state**: `version = "0.10.0"`, `CHANGELOG.md` carries
  `## [0.10.0] - 2026-10-01` with `[Unreleased]` empty, and the release tag
  `v0.10.0` names `104626f`.
- **Only docs and assets may follow a sweep.** `githooks/pre-tag` reads the
  results file's recorded revision and fails the tag if `src/`, `tests/`,
  `Cargo.*`, `build.rs` or `benches/scripts/soak/*.py` changed since — the
  changelog date and the README numbers may, code may not (AGENTS.md §10,
  "prove provenance").
- **The repository-settings items are a human's to make** — branch protection
  for `main` and the release secrets. They are tracked as an open thread below;
  nothing in this repository can enable them.

## The transparent-L3 cycle (unreleased, branch `feat/transparent-l3`)

**State as of 2026-10-09.** The feature is committed and green
(`45adb6e` the feature, `edffa53` never-encrypted, `b1ce03b` the striping
note): the transparent (L3) client makes itself the owner of a public
`ip:port`, carried as whole IP packets over a TUN device on both ends, with the
wire at **v5** and `just l3-accept` (root-only, outside the check chain)
proving the transparency end to end. Because none of it is released, the
configuration surface is free to change — and this cycle changes it.

### Decisions

| # | Decision |
|---|---|
| L1 | L3 is a **run mode**, not a service type: touching a NIC is a process-level capability, so an entry-level key must not be what opens it |
| L2 | The mode is chosen by its own top-level table (`[transparent]`), exactly as `[server]`/`[client]` are; a CLI flag only overrides |
| L3 | `[client]` and `[transparent]` in one file is `Undetermine`: two roles are two processes (capability and restart isolation) |
| L4 | The server gets **no** mode. It is a policy host: one config serves encrypted L4 clients and plaintext L3 ones |
| L5 | `[server.transparent]` **is** the server's switch: absent, a registration is refused by policy *before* any device is looked at, so a client can never be what makes the server reach for `/dev/net/tun` (shipped first in this cycle) |
| L6 | **No** `allow_addresses`: one shared `default_token` is one trust domain, so a global address list cannot discriminate between clients; the address universe is already expressed by the operator's own routing |
| L7 | Noise and L3 are **not** mutually exclusive — the Noise keys simply have no effect on an L3 client. The documented consequence: enabling L3 means that server accepts plaintext connections from it |
| L8 | `allow_ports` keeps gating L3 too: it is documented as the master switch for dynamic registration, and an L3 claim is a registration |
| L9 | A claim's address key stays `remote_bind_addr`, and `proxy` stays in `[transparent.transport]` (the only key there — no encryption key has a home in that table) |
| L10 | Header compression is an **intermediate state**: keep it when the measurement shows benefit, remove it when it does not, against criteria written down *before* the run |
| L11 | No startup INFO for the switch; the plaintext consequence lives in the docs instead |

### Next, in order

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

   What is left is one number, measured and characterized: the **paced single
   flow**, at ~142 µs of daemon CPU per carried packet, which is the wakeup
   chain (a read wakeup, a channel hop and a write, per direction, per side).
   Nothing above moves it — it is the async task-per-hop shape, not a batch
   that is missing — so the only lever left is a synchronous data path (a
   blocking thread per direction, WireGuard's queue-thread shape, `nix` for the
   readiness instead of tokio tasks). It is a rewrite of this path's concurrency
   model, and the case for it is bounded: a single paced flow already carries
   4 000 round trips/s, and the workloads L3 exists for sit two orders of
   magnitude below that per flow; the flows that are busy queue, and queued
   flows batch. Measure it before building it if it is built at all.
6. **Open: the v0.10.0 architecture comparison.** The L4 baseline arm (a
   worktree build at the `v0.10.0` tag running the same echo backend over the
   same topology, same host, same run) is not in the harness yet. The two arms
   measured above are internal to HEAD and answer the compression question
   only; the L3-versus-L4 cost is still unmeasured.

**Pre-registered criteria (kept as written, for the record).** Small packets
(≤128 B payload): wire bytes down ≥ 20 %. Mid (512 B–1 KB): ≥ 5 %. Bulk
(1400 B): throughput down ≤ 2 %. The small-packet and the bulk conditions must
both hold; a difference inside the variance counts as *no difference*, and
either failure removes the feature. The small-packet arm failed on the ceiling
alone, so the other arms were not run for the verdict.

### Open threads

- **The soak bench has no namespace support** — every arm runs on host loopback
  — so an L3 arm cannot be expressed as a config variant there. The L3 numbers
  come from the l3 harness instead; folding L3 into the soak is a separate
  decision that needs netns plumbing in `soak/lib.py`.
- The benchmark comparison against v0.10.0 must build that state (worktree at
  the tag): there is no released asset that speaks v5, and no two tags are
  compatible, so each side runs as a complete pair.

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
