# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
