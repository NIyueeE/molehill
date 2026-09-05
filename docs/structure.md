# Repository structure

> English | [简体中文](structure.zh.md)

What every file and directory in this repository is for. Deeper behavioral
docs: [configuration](configuration.md), [transport](transport.md),
[internals](internals.md), [build guide](build-guide.md).

## Root

| Path | Purpose |
|------|---------|
| `Cargo.toml` | crate manifest; `[lints]` is policy documented in [lint-policy](lint-policy.md) |
| `Cargo.lock` | locked dependency graph (committed; verified with `--locked` in CI builds) |
| `build.rs` | build metadata injection via vergen (git SHA, timestamp, features, target) |
| `justfile` | task runner: `just setup` / `fmt` / `test` / `check` / `powerset` / `container` |
| `rust-toolchain.toml` | `channel = "stable"` + clippy/rustfmt components; never hardcode versions |
| `deny.toml` | cargo-deny policy: licenses, bans, advisories (pre-push + CI) |
| `.editorconfig` | editor defaults (4-space Rust, 2-space YAML/TOML, md keeps trailing spaces) |
| `AGENTS.md` | repository rules for AI agents and humans; entry point for every session |
| `HANDOFF.md` | current working state, decisions, open threads — read after AGENTS.md |
| `CHANGELOG.md` | single source of release notes (Keep a Changelog); gates releases |
| `CONTRIBUTING.md` / `SECURITY.md` | contributor setup; private vulnerability reporting |
| `README.md` / `README.zh.md` | bilingual landing pages; must document edition, channel, `just setup`, `just check`, hooks activation |
| `Containerfile` | scratch container image assembled from musl release artifacts |
| `LICENSE` | Apache-2.0 |
| `assets/` | logo and benchmark charts |

## Automation

| Path | Purpose |
|------|---------|
| `githooks/pre-commit` | fast gates: fmt, secret scan, machete, docs alignment, clippy ×2 |
| `githooks/pre-push` | heavy gates: audit, deny, outdated, tests |
| `githooks/check-secrets` | staged-changes secret scan (`security-scan:allow` marker to waive a line) |
| `githooks/check-docs` | docs ↔ code alignment (hook commands, lints, edition, channel, README index, CI entry) |
| `.github/workflows/ci.yml` | `just check` chain + feature powerset + per-feature test matrix + minimal-size check + 4-platform builds |
| `.github/workflows/release.yml` | tag-driven release: version/changelog gates, 9-target matrix, draft release, GHCR, crates.io |
| `.github/workflows/test-build.yml` | manual per-commit CD test builds (never publishes) |
| `.github/dependabot.yml` | weekly cargo + GitHub Actions updates |
| `.github/ISSUE_TEMPLATE/`, `PULL_REQUEST_TEMPLATE.md` | issue forms (blank issues disabled), PR checklist |

## Source

| Path | Purpose |
|------|---------|
| `src/main.rs` | binary entry point: CLI parsing, signals, logging setup |
| `src/lib.rs` | library root: run-mode detection, main event loop, config-watcher lifecycle |
| `src/cli.rs` | clap-derive CLI definitions |
| `src/protocol.rs` | wire protocol (Hello/Auth/Ack/commands), postcard serialization, protocol version |
| `src/common.rs` + `src/common/` | constants, DNS/keepalive/retry helpers, `MultiMap` |
| `src/config.rs` + `src/config/` | TOML parsing/validation (`Config`, `ClientConfig`, …, `MaskedString`), hot-reload watcher |
| `src/core/client.rs` | client mode: control channel, auth, registration, data-channel requests |
| `src/core/server.rs` | server mode: registration policy, eager binding, connection pools |
| `src/logging.rs` | colored span-aware log formatter |
| `src/transport.rs` + `src/transport/` | `Transport` trait + tcp / native-tls / rustls / noise / websocket / multiplex implementations |

## Tests, benches, examples, docs

| Path | Purpose |
|------|---------|
| `tests/integration_test.rs` | spawns real server+client pairs; TCP/UDP across transports |
| `tests/common/mod.rs` | echo/pingpong hitters and runner helpers |
| `tests/for_tcp/`, `tests/for_udp/`, `tests/config_test/` | transport fixtures and valid/invalid configs |
| `benches/` | HTTP latency (vegeta) and memory-sampling scripts |
| `examples/` | runnable configs: tls, noise_nk, udp, use_proxy, minimal, iperf3, unified, systemd, container, full |
| `docs/` | user docs (configuration, transport, build-guide, internals) + governance docs (checks, lint-policy, release, structure), each bilingual (`*.md` + `*.zh.md`) |
