# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
