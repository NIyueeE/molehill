# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
