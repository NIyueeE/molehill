# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- CI skips the code chain for a change limited to markdown, `docs/` or
  `assets/`: those paths cannot move the Rust gates, so `ci.yml` filters them
  out and a new `docs.yml` runs the docs-alignment check instead — the one
  gate such a change can break. It needs no toolchain and finishes in
  seconds. A commit mixing docs and code paths runs both workflows, and the
  tag-driven release workflow is unaffected.

## [0.8.0] - 2026-09-11

> **The benchmark figures quoted per entry were measured when that change
> landed**, on the host then in use. The 2026-09-10/11 method revision
> (rate-cell shaping, throughput window convention, UDP capacity ladder)
> then re-measured the whole matrix against the committed defaults, so the
> figures are not comparable across entries. The authoritative, mutually
> comparable set is the README Benchmarks chapter, regenerated from
> `benches/scripts/bench/results-v0.8.0.json`; HANDOFF.md records that
> baseline's scope and its regression-gate waiver.

### Added

- The KCP adapter's SACK gap notification tightens from 50 ms / 32
  segments to 10 ms / 16: the cooldown now sits below the nodelay RTO
  floor, so a SACK beats the RTO backoff instead of firing after it
  (measured +9% on the loss1_rtt10 cell, flat elsewhere, no jitter-cell
  regression).

- The KCP data plane batches UDP datagram IO on Linux (`recvmmsg` /
  `sendmmsg`, up to 32 datagrams per syscall — the same amortization
  QUIC stacks get from UDP GSO): loopback throughput improves +32%
  (1-stream) and +8% (8-stream) on the kcp4 arm, with the weak cells
  flat; the wire format is untouched (datagrams are byte-identical).

- `just test-fast` and `just bench-fast`: a quick verification loop (lib +
  core integration subset) and a ~2-minute molehill-only smoke matrix into
  `results-dev.json`; the bench runner's default `--out` derives from
  `Cargo.toml`'s version, so dev runs never merge into a release baseline.

- v0.8.0 benchmark re-baseline: `results-v0.8.0.json` and the README
  chapter/charts now describe the merged defaults (count=4, ring-
  accelerated noise) with three new cells (rate-limited r100/20 and
  r20/40, jittery j20/10) and six new metrics (CPU%, connection churn,
  sustained UDP capacity, a 64-stream scale point, a mixed bulk +
  interactive workload, per-rep min/max). The matrix self-throttles
  (nice 10, load-aware cooldown) and every probe socket is bounded.
  The v0.7.2 baseline ran a different container and single-tunnel
  default, so the old regression gate does not carry over;
  `results-v0.8.0.json` is the new baseline.

- Parallel multiplex tunnels by default (`[client.data].default_count = N`,
  default 4): data channels are spread round-robin over N tunnel connections
  per service, isolating head-of-line blocking (rtt10 HoL max 101 -> 81 ms)
  and aggregating beyond a single TCP flow (loopback 8-stream 4.3 -> 12 Gbps,
  1% loss 3.7 -> 7.1 Gbps in the bench matrix); a dead tunnel is skipped
  transparently. `1` reproduces the single-tunnel behavior.
- Per-service servers and auth: `[client.services.<name>].remote_addr`
  overrides `[client.control].default_remote_addr` for that service — its
  control channel and, by default, its data plane dial the given server —
  and `token` overrides `[client].default_token`, so one client can spread
  services across several molehill servers (region replicas, per-tenant
  servers, rolling migrations) even when the servers do not share a token;
  each server covers the registered ports in its own `allow_ports`.
- Per-service data-plane and control overrides: `[client.services.<name>]`
  accepts `mode`/`count`/`carrier` (defaults in `[client.data]`),
  `remote_addr`/`heartbeat_timeout`/`retry_interval`/`token` (defaults in
  `[client.control]` / `[client]`), so one client can mix data paths per
  service (e.g. a multiplexed interactive service next to a `direct` bulk
  service) and per-service heartbeat timeouts against servers with
  different heartbeat intervals. The pools were already per service; the
  server needs no changes — it adapts per connection and lazily opens its
  KCP listener on the first `kcp` registration.
- Optional KCP data tunnels (`[client.data].default_carrier = "kcp"`,
  feature `kcp`):
  multiplexed tunnels ride KCP-over-UDP sessions (fixed, recorded protocol
  parameters) behind a thin in-repo tokio adapter, with Noise kept on top
  when the control transport is `noise`; the server lazily binds its UDP
  listener on the first registration that declares the `kcp` carrier (see
  HANDOFF.md); idle-tunnel keepalive is a known limitation.
- Explicit data-plane endpoints: `[client.data].default_data_addr` (defaults to
  the service's control endpoint) and `[server.data].bind_addr` (defaults
  to the control listener) let operators move data channels onto a separate
  port or interface. The server binds a dedicated listener only when the
  two addresses differ; otherwise the control listener keeps accepting data
  connections exactly as before.
- Release-grade benchmark matrix (`just bench`): peer comparison against frp,
  rathole (upstream) and bore across loopback and weak-network cells
  (rtt/loss via netem, with a userspace delay-proxy fallback when
  `CAP_NET_ADMIN` is unavailable), chart + table rendering (`just bench-plot`)
  and a per-tag regression gate (`just bench-check`) required before tagging.

### Changed

- Benchmark measurement method revision (2026-09-10). Rate cells shape `lo`
  with a `limit 2000`-packet queue (recorded as `netem_rate_limit`): the old
  shallow queue tail-dropped GSO segments and cost ~80% of the shaped rate,
  so a rate cell measured the shaper rather than the tool. Throughput
  sampling is isolated per repetition (fresh iperf3 server after a stall or a
  `server is busy` answer, a client bound never tighter than the historical
  `secs + 20`), keeps raw per-rep JSON under `iperf-raw/<arm> <cell>/`, uses
  one stated window convention (bytes over the measured window, with the
  receiver's own window beside it, and the receiver's count used when the
  sender's accounting is provably degenerate), and records a typed reason for
  every `null`. `audit_results.py` gates completeness (holes, nested fields,
  sampler output) and the endpoint invariant below; `docs/release.md` and
  AGENTS.md §10 state the comparability rules.

- The UDP capacity probe is now a ladder rather than a fixed two-point
  sample: 500 pps to the configured burst rate in 2000-datagram bursts with a
  1.5 s drain, reporting the highest step delivered within
  `max(2%, cell loss + 2pp)` plus the knee. It finds real knees (10 ms cell
  12 000 pps / 10.9 Mbit/s, 100 ms cell 1 000-2 000 pps / 0.7-1.4 Mbit/s) and
  a lower bound on unshaped loopback (27.2 Mbit/s, the ladder top), where the
  previous design only reported its own pacing back.

- The v0.8.0 benchmark baseline was re-measured on 2026-09-10/11 against the
  revised method (52 arms on host `0b073ddbf222`; the audit reports zero
  holes, zero arm errors and no endpoint violation), and the README
  chapter/charts were regenerated from it. Because the revision changed the
  shaping model, the throughput window/accounting and the host, **every
  number in the chapter is a fresh same-host measurement — the previous
  v0.8.0 rows and the v0.7.2 regression baseline are not comparable.** The
  re-derived conclusions from this run: Noise retains ~58%/76% of plain
  throughput, `count = 4` aggregates at 8 streams (19.5 vs 9.2 Gbit/s on
  loopback, 12.3 vs 4.5 at 1% loss), and the KCP carrier stays far behind TCP
  wherever the path is not the bottleneck (1.1 vs 14.9 Gbit/s at 8 streams,
  ~3x RSS) with UDP-only paths and rtt100 session quality as its uses. A
  UDP-under-load weakness seen in two earlier runs (100% paced-pinger loss in
  the 10 ms cell) did **not** reproduce in this one (2%), so it is recorded as
  variance rather than a finding.

- Fixed a measurement regression that invalidated an intermediate baseline:
  the throughput sampler dialed the iperf3 backend instead of the tool's
  exposed port, so it reported the loopback ceiling with the tunnel bypassed.
  Entries now record the endpoint, the sampler raises when the two ports are
  equal, and the audit fails such a run. With the endpoint fixed, the
  weak-cell limits are visible again and honest: the 20 Mbit/s cell measures
  1-stream (0.009 Gbit/s) with its 8-stream slot `null` and a recorded
  timeout reason, and every `null` in the file carries its reason.


- The logo was replaced with the mole + volcano + data-stream design
  (`assets/molehill.svg`), and per-run bench artifacts
  (`results-arms-*.json`) are no longer tracked — only the release
  snapshots (`results-vX.Y.Z.json`) and charts stay committed.

- Docs now present molehill as an independent project: the rathole fork is
  kept as provenance (README / README.zh provenance note, AGENTS.md §7,
  `docs/release.md` versioning, the crate doc comment and the Cargo
  description) instead of as the project's identity. The fork point and the
  continued version line stay documented and unchanged.

- Benchmark peer binaries are cached under `~/tmp/bench-peers` instead of
  `/tmp/bench-peers`, which session cleanup wipes (forcing a re-download
  through the rate-limited GitHub API); `PEER_DIR` still overrides it.

- **BREAKING (protocol v3)**: every connection between client and server
  starts with a one-byte transport selector (`0x00` plain / `0x01` noise,
  on TCP connections and KCP sessions alike), and the service registration
  carries the data-plane carrier the client will use (`tcp`/`kcp`). The
  server accepts whatever the client speaks on one listener — its
  `[server.transport]` block is keys-only — and opens its KCP UDP listener
  lazily on the first `kcp` registration, rejecting with a precise reason
  when the bind fails or the binary lacks the `kcp` feature. Per-service
  encryption: `[client.services.<name>].transport` accepts `type`
  (`"noise"` / `"plain"`, unset = follow `[client.transport].type`)
  plus per-service `noise` keys — one client can run plain and encrypted
  services side by side (e.g. a service dialing a different server with its
  own public key), and the client is no longer generic over one transport
  (`ClientStream`/`ClientTransport` enums). Both ends
  must upgrade together (protocol version mismatch is a hard error).
- `snow` is upgraded to 0.10 (MSRV 1.85, edition 2024): the u16-framed
  record stream wrapper is vendored in-repo (`src/transport/noise_stream.rs`,
  ported from snowstorm 0.4.0 — unmaintained and pinned to snow 0.9, so a
  real upgrade requires owning the wrapper; Apache-2.0 attribution in the
  file header) and `pin-project` joins the `noise` feature. No behavior or
  wire change (measured throughput identical to the snow 0.9.6 build); the
  wrapper is now ours, which unblocks the leaner-record-stream work
  (HANDOFF). `Builder::local_private_key`/`remote_public_key`/`psk` now
  return `Result` and validate key/PSK lengths at build time — a wrong-size
  key surfaces at handshake setup instead of mid-handshake.
- KCP data tunnels: the ARQ send/receive windows are doubled (2048/4096
  segments, ~2.8/5.7 MiB in flight at MTU 1400 — the old 1024/2048 capped
  throughput at BDP/RTT, e.g. ~0.28 Gbps at 40 ms tunnel RTT). Measured
  with the bench matrix: +29% single-stream on the 1%-loss/10 ms cell
  (0.29 -> 0.38 Gbps) and +20% on the 5%-loss/100 ms cell, loopback
  unchanged. A 5 ms flush interval was tried and rejected: it regressed
  loopback 8-stream ~3x (busier update timer under saturation). The
  adapter-level PING/PONG keepalive (2 s, NAT warm + RTT probe) is now
  documented as present; dead peers are still only confirmed on the next
  write (~20 RTOs). KCP's measured role: UDP-only paths (TCP blocked by
  firewall/NAT) and latency-first interactive traffic on high-loss,
  high-RTT links (5%-loss/100 ms cell: udp p50 601 vs 813 ms and hol max
  gap 1444 vs 2963 ms, kcp4 vs the noise-TCP arm).
- The Noise data path now runs ring's hardware-dispatched
  ChaCha20-Poly1305 (`snow`'s ring-accelerated resolver, part of the
  default feature set; `snow` is now a direct dependency, `snowstorm`
  keeps the stream wrapper): measured +29% loopback single-stream
  (3.81 -> 4.92 Gbps) and +25% 8-stream (11.80 -> 14.75 Gbps) end-to-end
  on x86-64, and +7%/+16% on the 1%-loss/10 ms cell. No wire change —
  every pattern gets the accelerated cipher; the pattern hash (BLAKE2s
  default) only runs once per connection in the handshake.
- **BREAKING (transport)**: the `tls` and `websocket` transports are removed.
  `[client.transport]` / `[server.transport]` accept only `"plain"` (default)
  and `"noise"`, and the `[client.transport.tls]`,
  `[client.transport.websocket]`, `[server.transport.tls]` and
  `[server.transport.websocket]` blocks, the `TlsConfig`/`WebsocketConfig`
  types, and the `native-tls` / `rustls` / `websocket-*` cargo features with
  their dependencies are gone. Migrate to `noise` (docs/transport.md); if TLS
  termination is still required, run nginx or a CDN in front of molehill.
  Old configs naming the removed values or blocks fail loudly at startup.
- **BREAKING (configuration)**: the client/server layout is now three blocks
  per side. `[client]` keeps `default_token`; the client-level `prefer_ipv6`
  was removed (it had no effect in the running code — only the per-service
  `prefer_ipv6`, which steers the UDP forwarder's bind choice, is live); the
  control channel moved to `[client.control]` and the data plane to
  `[client.data]`. The client-wide defaults are `default_`-prefixed so they
  read distinctly from the per-service overlay keys:
  `[client.control]` (`default_remote_addr`, `default_heartbeat_timeout`,
  `default_retry_interval`) and `[client.data]` (`default_mode`,
  `default_count`, `default_carrier`, `default_data_addr` — replacing `mux`,
  `mux_tunnels`, `tunnel`, `tunnel_addr`), while `[client.services.<name>]`
  carries the overlays (`remote_addr`, `heartbeat_timeout`,
  `retry_interval`, `token`, `mode`, `count`, `carrier`, `protocol`,
  `transport`, `udp_forwarder_ipv6`, `udp_send_queue_size`). The service
  key `type` was renamed to `protocol` (it selects tcp/udp and collided
  with the transport `type`), `prefer_ipv6` to `udp_forwarder_ipv6` (it
  only steers the UDP forwarder's bind choice), `udp_sendq_size` to
  `udp_send_queue_size`, and the per-service transport toggle is
  `transport.type` (`"noise"`/`"plain"`). On the server,
  `bind_addr`/`heartbeat_interval` moved to `[server.control]`,
  `kcp_bind_addr` became `[server.data].bind_addr`, and
  `[server.data].carriers` is **gone** — the registration carries the
  client-declared carrier (v3) and the server opens the listener on first
  use. `[server.transport]` lost its `type` key: it holds only the Noise
  keys, and the server accepts both plain and Noise connections (v3
  transport selector byte); a client that decides to encrypt (noise-vs-
  plain benchmark in the README is the price list) needs no server-side
  `type` agreement. The transport value `"tcp"` is now `"plain"`,
  `[client.transport.tcp].proxy` moved to `[client.transport].proxy`, and the
  dead transport-level `nodelay`/`keepalive_secs`/`keepalive_interval` keys
  were removed (fixed internal defaults; the per-service `nodelay` remains).
  Old keys are rejected by `deny_unknown_fields`, never silently ignored;
  the full migration table is in docs/configuration.md.
- Bounded per-tunnel yamux buffering: the receive window is fixed at 64 MiB
  per tunnel with 32 streams (was: yamux's own 1 GiB / 512, and configurable).
  With the old defaults a lossy link accumulated unbounded backlog — measured
  211 MiB avg / 620 MiB peak RSS in the client process at 1% loss (10 ms
  RTT); the new defaults keep every cell of the bench matrix at its previous
  throughput (the auto-tuner needs allocatable credit headroom,
  ~window/2RTT per stream; 16 MiB / 32 streams measured 30-65% slower on
  delayed links) while bounding the loss backlog at ~35 MiB avg / ~69 MiB
  peak. The values are internal constants, no longer config keys — yamux
  couples them (`window >= 256 KiB * streams`), so exposing both invited
  misconfiguration. Applies to TCP and KCP tunnels alike.
- The client tunnel driver is now event-driven: announcing a freshly opened
  stream is a polled future instead of an awaited socket flush inside the
  driver loop, so a backpressured tunnel can no longer stall inbound frame
  processing (window updates, data) while an open is in flight.
- The transport-arm feature `kcp` is part of the default feature set, so the
  standard `cargo build --release` binary covers the tcp/kcp arms. Slim
  builds (`embedded`, the `minimal` profile, the container recipe) keep
  their explicit feature lists and are unaffected.
- Benchmark entries are PEP 723 python scripts run via `uv run` (no shell test
  entries); peer tools (frp, rathole, bore) are fetched as the latest
  GitHub release binaries — never built from source. The matrix measures per
  tool and cell: through-tunnel TCP
  throughput (1/8 streams), connection-path RTT, data-path RTT, UDP session
  quality (RTT/loss/jitter/max gap), a head-of-line probe and RSS (schema v3).
  Runs are resumable and continue on error: each completed arm is printed and
  checkpointed to the results file immediately, `--tools/--cells/--variants`
  select subsets, and results merge unless `--fresh` is given. Molehill runs
  mux / mux-off / mux1 / noise / kcp4 variants (mux-off on the loopback cell
  only).
- Benchmark charts split into a plain-TCP peers chart (molehill default mux
  vs frp / rathole / bore) and a molehill-family chart isolating the costs of
  multiplexing (mux vs mux-off) and encryption (mux vs noise); the
  encrypted chisel peer was removed — its SSH tunnel is not comparable on the
  plain-TCP axis.
- The python bench/test entries are now linted by ruff in the pre-commit gate
  (`ruff.toml`, waivers documented there), and the uv/PEP 723 convention is
  part of AGENTS.md.
- The multiplexing-cost comparison covers the loopback cell only (mux-off
  runs loopback by design; the weak-cell multiplexing behavior is told by
  the tunnel-count axis — count = 4 vs count = 1 — in the count chart). The
  README benchmark section is reorganized around a configuration-selection
  guide (when to keep or turn off mux, plain vs noise) and a methodology
  section documenting the test discipline and known limits.
- Documentation policy: governance and contributor docs are English-only —
  the four `*.zh.md` governance mirrors were dropped; user-facing docs
  (README, configuration, transport) keep Chinese mirrors, and the Chinese
  configuration (`docs/configuration.zh.md`) and transport
  (`docs/transport.zh.md`) pages were added. AGENTS.md §2/§3 and the
  docs-alignment check were updated to match.
- Releases are published **directly** — the GitHub Release is created public
  from `CHANGELOG.md` notes as soon as the build matrix finishes; the draft
  stage is gone and the human checkpoint is the tag push itself.
- New `githooks/pre-tag` release review (light, seconds): tag↔version match
  (incl. `Cargo.lock`), dated non-empty changelog section, committed bench
  results/chart, container-job greps, plus an advisory audit checklist
  (CHANGELOG & docs, container build, benchmark gate, deliberate-release
  confirm). git has no native tag hook, so it fires via `just tag` and again
  inside pre-push on every `v*` tag push (before the heavy gates); the
  pre-commit / pre-push / pre-tag responsibilities are now separated in
  docs/checks.md.
- Benchmark data hygiene: the missing/failed cells of the v0.8.0 baseline
  are filled from targeted same-host re-runs — the peer tunnels at the
  rate-limited cells (frp/rathole/bore 1-stream now measured, previously
  null; the 8-stream slots stay `null` by design — see the known
  limitation below), the loss2b25 steady-RTT probes, churn at rate20,
  and every HoL probe that previously recorded fake zero RTTs now records
  `null` with its reason. The benchmark backends now run **per arm** (a
  wedged iperf3 server can no longer poison every later arm of a cell;
  EADDRINUSE retries + SIGKILL cleanup), the 8-stream test runs after the
  cheap probes so a wedge cannot contaminate them, and every throughput
  failure carries the iperf3 stderr in `partial_metrics` instead of a bare
  null. The charts mark missing data with an 'x' and the README tables are
  emitted mechanically from the raw results (full per-cell count/carrier
  tables, adaptive decimals). Known limitation, recorded in the
  methodology: on the low-rate rate20 cell, eight parallel iperf3 streams
  wedge iperf3's single-test server on the shaped loopback path
  (GSO-sized segments × the netem packet limit buffer seconds of data), so
  its 8-stream slot is `null` with the timeout reason in `partial_metrics`
  — consistently across all tools (rate100 measures both).
- Configuration docs restructured around the measured data: a decision
  tree (mermaid) with the v0.8.0 benchmark costs now guides the
  `mode`/`count`/`carrier`/transport choices before the specification, in
  both languages. The `examples/` directory is gone — every example config
  (minimal, full, noise, udp, unified, proxy, iperf3) and the
  systemd/container deployment files now live as code blocks in
  docs/configuration.md, and the config test suite parses those blocks
  directly (it previously read the `examples/` files), so the shipped
  examples keep being validated.

### Fixed

- The container image (and the musl release binaries it is built from) now
  include the `kcp` feature: the explicit feature lists in `release.yml`
  and `just container` omitted it while the default set — and the docs —
  include KCP, so `default_carrier = "kcp"` failed with a config error on
  the GHCR image. Verified with a `x86_64-unknown-linux-musl` check of the
  exact feature set.

- The KCP protocol engine is now maintained in-repo as molehill's own
  module (`src/kcp/`), re-implemented from the reference C implementation
  by skywind3000 instead of the `third_party/kcp` path dependency: cargo
  strips path dependencies when publishing, so `cargo publish` used to
  compile against the unpatched registry version and fail verification.
  `cargo package` now verifies cleanly. Aligned with the reference: fast
  retransmit is ack-timestamp-gated (`IKCP_FASTACK_CONSERVE` semantics —
  the old dep feature of the same name was inverted and shipped the
  aggressive variant), the timeout ssthresh halves the flush-entry cwnd,
  window probing starts after 5 s, and stream-mode `send` reports partial
  progress; the unused accessors, the `tokio`-feature code and the
  conv-adoption hook are trimmed.

- The committed charts are regenerated with the canonical cell order and
  the canonical footer (targeted re-runs no longer shuffle the axes or the
  footer cell list).

- Benchmark chart values: a measured zero is labelled `0` (a zero-height
  bar was indistinguishable from an absent slot, e.g. the UDP-loss panel
  showed empty-looking cells), and sub-1 cost metrics keep three significant
  digits (`kcp4`'s mixed-bulk 0.029 Gbit/s used to print as `0.0`).

- Benchmark charts only draw rows and cells that actually take part in a
  comparison: the molehill-vs-peers per-cell panels skip the cells the
  plain-TCP peers do not run (those molehill-only stories live in the
  count/carrier charts), the mux chart is a single loopback row (mux-off is
  loopback-only by design), and the cost chart's 64-stream panel omits
  `mux1` instead of drawing an empty slot. The HoL probe now records a
  connected pinger that completes zero or one round trip as a stall (the
  elapsed wait) instead of `null`, which used to vanish from the chart
  (frp/rathole at rate20_rtt40).

- Benchmark charts and runner: an absent measurement now renders as a grey
  `x` instead of a fake `0.0` bar (the cost chart drew `mux1`'s unmeasured
  64-stream and mixed-bulk slots as zero), the runner skips by design the
  probe it would otherwise wedge — the 64-stream scale point above an
  arm's `count × 32` yamux ceiling —
  recording the reason in `partial_metrics`, and a failed iperf3 run
  reports iperf3's own error/exit status instead of a bare
  `KeyError: 'sum_received'`. `mux1`'s mixed-bulk probe is re-measured at
  9.0 Gbit/s now that the over-ceiling scale test no longer leaves the
  shared iperf3 server wedged.

- Benchmark probes: the steady-ping probe reconnects on stall or EOF (no
  more probe-wide `TimeoutError` on burst loss; the EOF recv-spin is gone),
  the churn probe no longer `IndexError`s when every connection fails
  (reports zeros with `success_pct`), the HoL pinger records `null` instead
  of fake 0 ms when it gets no samples (single-sample gaps are `null` too),
  and a failed mixed-bulk transfer records its reason alongside the echo
  half of the metric.

- Benchmark throughput now measures **through the tunnel** (iperf3 dials the
  tool's exposed port); previously it dialed the backend directly, so every
  tool reported the loopback iperf3 ceiling (~46 Gbit/s) regardless of tool or
  network cell. Historical results files and charts are not comparable to the
  new schema.
- Benchmark runner hardening: a global lock refuses concurrent runs (they
  used to reap each other's live processes through the stale-pid sweep, which
  now also never touches processes owned by a running runner); SIGTERM takes
  the Ctrl-C cleanup path (arms killed, netem qdisc removed, full meta
  checkpointed); a killed `--fresh` run can no longer destroy the previous
  results file (it is backed up first); mid-run checkpoints carry the full
  meta instead of only the schema; per-metric failures record `null` plus a
  `partial_metrics` list instead of faking `0.0` or discarding the arm; a
  backend bind failure records that cell's arms as errors instead of aborting
  the whole matrix; retransmit counts belong to the median throughput rep;
  leaked iperf3 backends are now reaped by the sweep (workdir in cmdline).

## [0.7.2] - 2026-09-05

### Changed

- Adopted the rust-agents-template lint set in full: `pedantic` (deny) and
  `missing_docs` (warn) are declared in `Cargo.toml` and the whole codebase
  passes them; the public API and config structs are now documented. The
  unused `lazy_static` dependency was replaced by `std::sync::LazyLock` and
  dropped.

### Fixed

- UDP forwarding keeps **session affinity** for every remote peer: the
  server routes all datagrams of one peer address through a single data
  channel (previously the per-datagram pool load balancing split a burst
  across channels), and the proxy client keeps one local outbound socket per
  peer for its whole session, surviving channel re-sharding and channel
  loss. Stateful UDP sessions — e.g. Minecraft Bedrock (RakNet), QUIC,
  WireGuard — no longer tear in half with `pool_size > 1`; the pool now
  shards distinct peers, not packets. The server also replaces dead UDP
  data channels to keep the pool at its configured size.

## [0.7.1] - 2026-09-05

Project base rebuilt on [rust-agents-template](https://github.com/NIyueeE/rust-agents-template).
No forwarding-behavior changes; the git history was also rebuilt in this
release (upstream rathole history intact; the fork's development commits
squashed into one release commit per version).

### Added

- Layered git hooks under `githooks/`: pre-commit fast gates (fmt, staged-
  changes secret scan, unused-dependency check, docs↔code alignment, strict
  clippy) and pre-push heavy gates (audit, deny, outdated, tests); activate
  with `just setup`
- `githooks/check-docs`: verifies the governance docs still describe the code
  (hook commands, lint tables, edition, toolchain channel, README doc index,
  CI/release wiring)
- `githooks/check-secrets`: blocks commits that stage credential-shaped
  lines (`security-scan:allow` marker to waive a documented line)
- Modular bilingual governance docs: `docs/checks.md`, `docs/lint-policy.md`,
  `docs/release.md`, `docs/structure.md` with `*.zh.md` counterparts
- Dependency policy via `cargo-deny` (`deny.toml`: licenses, bans,
  advisories, yanked crates) wired into pre-push and CI
- `just` recipes: `setup`, `fmt`, `test`, `check`, `powerset` (cargo-hack
  feature powerset), `container` (scratch image)
- `.github/workflows/test-build.yml`: manual per-commit, per-platform CD
  test builds that never publish
- `CONTRIBUTING.md`, `SECURITY.md` (private vulnerability reporting with
  molehill-specific scope notes), `.editorconfig`, Dependabot config for
  cargo and GitHub Actions, PR template, issue-template config
- `rust-toolchain.toml` now declares `clippy` and `rustfmt` components
- All GitHub Actions pinned to commit SHAs (Dependabot tracks the `# vX`
  comments)

### Changed

- CI restructured: the full check chain runs via `just check`; the
  feature-powerset, per-feature test matrix, minimal-size check, and
  cross-platform builds remain dedicated jobs
- Release workflow enforces the changelog-driven policy: a missing
  `## [x.y.z]` section in `CHANGELOG.md` fails the release before anything
  builds; notes are only ever extracted from the changelog; releases are
  created as drafts
- `AGENTS.md` rewritten as repository rules (self-check, waiver discipline,
  docs↔code alignment, commit convention, tag-push policy, provenance from
  rathole and the upstream-anchored versioning); architecture details moved
  to `docs/structure.md` and `docs/internals.md`
- READMEs gained a Development section (hooks activation, `just setup` /
  `just check`) and a split documentation index; the stale `rust-1.95.0+`
  badge now reflects the `stable` channel

### Removed

- Legacy `.githooks/pre-commit` (superseded by the layered `githooks/` chain)

## [0.7.0] - 2026-08-26

Major release: the server no longer needs per-service configuration — clients
declare what to expose and the server enforces policy. **Protocol bumped to
v2: upgrade both ends together** (mismatched peers fail loudly with a
"please update" message; no compatibility with 0.6.x, by design).

### ⚠ Breaking changes & migration

1. **`[server.services.*]` removed.** The client is now authoritative.
   - Move each service's server-side `bind_addr` into the client's
     `remote_bind_addr`.
   - Delete all per-service `token = ...` lines; both sides now use one
     required `default_token`.
   - The server now requires `allow_ports` (see below).
2. **Protocol v2**: hello/auth/registration framing changed; the fixed-size
   ack gained framed variants (`RegisterRejected(reason)` replaces
   `ServiceNotExist`).
3. Auth is anchored to `default_token` on each side instead of per-service
   token tables.

### Added

- **Dynamic service registration**: clients send a framed `RegisterService`
  (name, type, `remote_bind_addr`, pool size) right after authentication;
  the server validates against its policy, binds the endpoint eagerly, and
  acknowledges — port conflicts surface as precise rejections delivered to
  the client.
- **Server-side policy knobs**: mandatory-for-registration `allow_ports`
  whitelist (empty/missing rejects *all* registrations), explicit listing
  required for privileged ports (<1024), optional `max_pool_size` clamp.
  Rejections are terminal for that service run: the client logs the server's
  reason once and stops retrying until config/restart.
- **Per-service tuning**: `pool_size` (default 8 TCP / 2 UDP),
  `udp_buffer_size` (default 2048, up to 65535 — wire-compatible),
  `udp_idle_timeout` (default 60s), `udp_sendq_size` (default 1024).
- **Multiplexing**: one tunnel connection per service carries every data
  channel as a yamux stream. The `multiplex` feature is part of the default
  feature set and `mux = true` is the default. rust-yamux auto-tunes stream
  receive windows towards the bandwidth-delay product; tunable via
  `mux_receive_window` / `mux_max_streams`. The initial end-to-end stall was
  traced to yamux's lazy outbound-stream SYN (a read-only pooled stream never
  emitted its first frame); the client driver now sends a zero-length SYN
  kick, with a read-first regression test and a full
  `{tcp,tls,noise,websocket} × {mux±}` integration matrix. `mux = false`
  keeps the one-connection-per-channel path available.
- **Container image parity**: the release musl builds and the scratch image
  now include multiplexing, and the publish workflow smoke-tests the pushed
  image (`--help` plus manifest inspection).
- **Colored, span-aware logging**: level-coded colors (ERROR red, WARN
  yellow, INFO green, DEBUG cyan, TRACE purple), visible span context
  (`handle{service=ssh}:`) on every line, ANSI only on TTYs (`NO_COLOR`
  respected), source target appended at debug/trace.

### Performance

- Loopback benchmark vs v0.6.4 and frp 0.71.0 (plain TCP, same machine):
  throughput is loopback-saturated and statistically identical across all
  tools (~64 Gbit/s aggregate); the differentiator is connection-path
  latency — echo RTT p50 **0.249 ms** with mux enabled vs **0.380 ms** for
  frp (~35% lower), p99 **0.319 ms** vs **0.594 ms** (~46% lower). Chart in
  the README; raw data and reproducible scripts in `benches/scripts/bench/`.
- With `multiplex` enabled (default), concurrent visitors no longer pay a
  TCP(+TLS/Noise) handshake per data channel, and steady-state file
  descriptors drop from one-connection-per-channel to a single tunnel.
- Memory comparison added: average RSS (server + client) sampled under
  loopback iperf3 load — molehill 0.7.0 mux **16.9 MiB** (35.8% of frp),
  mux=off 16.8 MiB (35.7%), v0.6.4 16.5 MiB (35.0%), frp 47.1 MiB.
  `run_bench.sh` now records RSS and the chart includes the memory panel.

### Changed

- Service endpoints bind eagerly at registration time so conflicts surface
  immediately as rejections instead of pool retry loops.
- Hot reload of client services registers/unregisters over existing control
  channels instead of tearing down physical connections.
- UDP oversized-datagram drop threshold follows the receiver's configured
  `udp_buffer_size` instead of a compile-time constant.
- CI now also checks the no-hot-reload `server,client`-only feature
  combination (clippy + lib tests) and runs integration tests serially to
  avoid timing flake between the TCP and UDP suites.
- CI minimal-size job now looks for `target/minimal/molehill` (the custom
  Cargo profile's actual output path), fixing the size step failure.

## [0.6.4] - 2026-08-25

### Added

- Client-side health check (`health_check`) for TCP services: the client probes the local service and, after `max_failed` consecutive failures, drops the service's control channel so the server stops serving it and visitors fail fast; the service is re-registered automatically once it recovers. Supports `tcp` and `http` probe types with configurable `interval`, `timeout`, `max_failed`, and `http_path`
- `HANDOFF.md` now holds all planned work and future design documents (data-channel multiplexing, configurable pool sizes, QUIC transport, buffer pooling, single control channel, and the former README planning items); the README planning section was removed in favor of it

### Changed

- TCP_NODELAY is now enabled by default on every leg of a forwarded service (both ends of data channels, visitor-facing sockets, and the client's connection to the local service) instead of only on the outer transport connections; the per-service `nodelay` option still allows opting out
- TCP keepalive (20s/8s) is enabled by default on data channels and visitor sockets, so pooled idle channels that were silently dropped by NATs/middleboxes are detected instead of being handed to visitors
- The bidirectional TCP copy buffer is raised from tokio's 8 KiB default to 32 KiB per direction (`copy_bidirectional_with_sizes`)
- UDP datagrams are now framed into a reused buffer and emitted with a single write (one TLS/Noise record per packet) instead of three writes with per-packet heap allocations; the server's receive path no longer allocates per packet
- UDP packets larger than the 2048-byte buffer are now dropped in-stream (the channel stays usable) instead of tearing down the whole data channel

### Fixed

- The UDP connection pool logged "Failed to run TCP connection pool" as its error context
- Upgraded `h2` to 0.4.16 to fix RUSTSEC-2026-0258 (unbounded empty DATA frames; `h2` is pulled in by the optional console feature's hyper/tonic chain)

## [0.6.3] - 2026-08-05

### Added

- Strict lint rules in `Cargo.toml`: `unwrap_used`, `expect_used`, `panic`, `dbg_macro`, and `undocumented_unsafe_blocks` are denied; `unsafe_code` is warned
- Complete configuration example covering every option in `examples/full/`
- Container deployment examples (Docker/Podman Compose and Podman Quadlet) in `examples/container/`
- Startup log banner with version, git describe, and target triple; timestamps on log lines
- Detailed usage guide, usage notes, and troubleshooting section in `docs/configuration.md`

### Changed

- Container image is now assembled from the release build's static musl binaries on a `scratch` base (~8 MiB) instead of compiling inside the builder stage
- x86_64/aarch64 musl release artifacts now build with the full rustls feature set (static, OpenSSL-free)
- CI updated to `actions/checkout@v7` and the `stable` toolchain
- Client logs show the service name and remote address instead of a hex digest
- `docs/transport.md` and `docs/internals.md` rewritten to match the current implementation (Noise PSK, connection pooling, heartbeat, hot reload)
- README restructured with a Deployment section and a documentation index; `README.zh.md` synced to the new structure
- Updated dependencies (anyhow, vergen, tokio, clap, openssl, etc.)

### Fixed

- Config validation now rejects proxies without host/port and `remote_addr` without a port at startup instead of panicking at runtime
- UDP packet headers declaring a length above the receive buffer are rejected instead of causing oversized allocations
- TCP and UDP integration tests no longer share exposed ports, eliminating flaky parallel test failures
- Release packages now include the Chinese README (`README.zh.md` instead of the non-existent `README-zh.md`)
- Expired TLS test certificates regenerated
- Removed regenerable TLS key files from the repository (kept in `.gitignore`)
- Log message typos (`Shutting down gracefully`, `identity`)

## [0.6.2] - 2026-05-02

### Added

- Added Noise PSK (pre-shared key) support with configurable `psk` and `psk_location` fields
- Added UDP pool load balancing: each data channel gets its own worker task sharing the same socket via `JoinSet`
- Added unit tests for protocol message serialization/deserialization (`Hello`, `Auth`, `Ack`, `ControlChannelCmd`, `DataChannelCmd`, `UdpTraffic`)
- Added cargo test step for non-macOS ARM targets in CI pipeline
- Added `prefer_ipv6` config option at both client-level and per-service
- Added benchmark documentation section in `CLAUDE.md`
- Added `MaskedString` and Proxy Support documentation in `CLAUDE.md`

### Changed

- Pinned CI Rust toolchain to `1.95.0` via `dtolnay/rust-toolchain@master`
- Updated `README.md` and `README.zh.md` with Noise PSK, `prefer_ipv6`, and WebSocket transport documentation
- Expanded `CLAUDE.md` with source structure details, connection pooling, and build profile descriptions
- Fixed typo in TLS config example: `pkcs12 = "identify.pfx"` → `pkcs12 = "identity.pfx"`

### Fixed

- Resolved deferred TODO items: Noise PSK support and UDP pool load balancing are now implemented

## [0.6.1] - 2026-05-01

### Added

- Added `src/common/`, `src/config/`, `src/core/` submodule structure with module re-exports
- Added embedded (noise-only) test run and binary smoke test (`--help`) to CI pipeline
- Added `molehills.service` (server systemd unit file)
- Added `should_retry_accept()` helper for transient resource exhaustion errors (EMFILE, ENFILE, ENOMEM, ENOBUFS)
- Added safety documentation for `MultiMap` unsafe code blocks

### Changed

- Reorganized source tree into `src/common/`, `src/config/`, `src/core/` submodules
- Upgraded `toml` from 0.5 to 1.0, `sha2` from 0.10 to 0.11, `rand` from 0.8 to 0.10, `async-socks5` from 0.5.1 to 0.6.0
- Upgraded `vergen` from 8 to 10.0.0-beta.8 with separate `vergen-gitcl` crate
- Migrated `base64` API from top-level functions to engine-based API (`base64::engine::general_purpose::STANDARD`)
- Updated `build.rs` for vergen 10 API
- Upgraded GitHub Actions from node20 to node24 (`actions/checkout@v6`, `upload-artifact@v7`, `download-artifact@v8`)
- Moved `panic = "abort"` from release profile to dev profile
- Switched nonce generation from `rand::thread_rng().fill_bytes()` to `rand::rngs::SysRng::try_fill_bytes()`
- Renamed systemd example files from `rathole*` to `molehill*` with corrected service descriptions and mode flags
- Updated `CLAUDE.md` with improved command examples, source structure documentation, and build profiles

### Fixed

- Fixed CI toolchain alignment with project and multi-arch Docker build (removed `--locked` flag)
- Fixed `cargo publish` with `--allow-dirty` for Cargo.lock drift in CI
- Fixed systemd service flag assignments (server/client mode flags were inverted)
- Fixed `fdlimit::raise_fd_limit()` ignored return value warning

### Removed

- Removed stale FIXME comment in UDP data channel code
- Removed `async-trait` dependency (resolved upstream in `async-socks5` 0.6.0)
- Removed old `ratholes@.service` systemd file (replaced by `molehills.service`)


## [0.6.0] - 2026-04-23

### Added

- New project branding: renamed from `rathole` to `molehill` with new logo and updated documentation
- Added `justfile` with `just check` command chain (cargo check, clippy, fmt, audit, machete)
- Added `CHANGELOG.md`
- Added `rust-toolchain.toml` pinning Rust 1.95.0
- Added `CLAUDE.md` project documentation for contributors
- New CI workflow `ci.yml` with cargo audit, cargo machete, and cargo-hack feature-powerset checks
- Enhanced `release.yml` with GHCR Docker publishing and automatic changelog extraction

### Changed

- Upgraded Rust edition from 2021 to 2024
- Upgraded Rust toolchain from 1.71.0 to 1.95.0
- Upgraded `clap` from 3.x to 4.x with derive features and `ValueEnum`
- Replaced `backoff` with `backon` 1.6 for retry logic
- Replaced `bincode` with `postcard` for serialization
- Upgraded `tokio-rustls` from 0.24 to 0.26
- Upgraded `rustls-native-certs` to 0.8
- Upgraded `vergen` from 7 to 8 with `gitcl` backend
- Updated `build.rs` for vergen 8 API
- Updated author and description metadata in `Cargo.toml`
- Reformatted `README.md` and `README-zh.md` with centered layout, badges, and fork attribution
- Updated all internal references from `rathole` to `molehill`

### Removed

- Removed `.rustfmt.toml` (nightly-only `imports_granularity` incompatible with stable)
- Removed outdated documentation: `docs/benchmark.md`, `docs/out-of-scope.md`, and `docs/img/` directory
- Removed old `rust-toolchain` file (replaced by `rust-toolchain.toml`)
- Removed old CI workflow `.github/workflows/rust.yml` (replaced by `ci.yml`)
- Removed `atty` dependency (unmaintained)
- Removed `rustls-pemfile` dependency (functionality merged into rustls)
