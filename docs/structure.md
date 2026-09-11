# Repository structure


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
| `githooks/pre-push` | heavy gates: audit, deny, outdated, tests; runs the release review (pre-tag) on `v*` tag pushes |
| `githooks/pre-tag` | light release review (via `just tag` + on tag pushes): tag↔version, changelog section, bench assets, container-job greps, advisory checklist |
| `githooks/check-secrets` | staged-changes secret scan (`security-scan:allow` marker to waive a line) |
| `githooks/check-docs` | docs ↔ code alignment (hook commands, lints, edition, channel, README index, CI entry) |
| `.github/workflows/ci.yml` | `just check` chain + feature powerset + per-feature test matrix + minimal-size check + musl check + 4-platform builds (skipped for docs-only changes) |
| `.github/workflows/docs.yml` | the docs-only half: `githooks/check-docs` for changes limited to markdown, `docs/`, `assets/` |
| `.github/workflows/release.yml` | tag-driven release: version/changelog gates, 9-target matrix, direct release, GHCR, crates.io |
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
| `src/transport.rs` + `src/transport/` | `Transport` trait + tcp (plain) / noise (+ vendored `noise_stream.rs` record wrapper, ported from snowstorm) / multiplex / kcp implementations |
| `src/kcp/` | internal KCP (ARQ) protocol engine — self-maintained, algorithm aligned with the reference C implementation by skywind3000, plus the adapter's SACK extensions; kept in-repo so nothing external needs patching and the module follows molehill's own rules |

## Tests, benches, examples, docs

| Path | Purpose |
|------|---------|
| `tests/integration_test.rs` | spawns real server+client pairs; TCP/UDP across transports |
| `tests/common/mod.rs` | echo/pingpong hitters and runner helpers |
| `tests/for_tcp/`, `tests/for_udp/`, `tests/config_test/` | transport fixtures and valid/invalid configs |
| `benches/` | Peer-comparison benchmark matrix (`bench/`: uv/PEP 723 python — runner, peer fetch, chart, regression gate), mux e2e smoke (`mux/repro_e2e.py`), HTTP latency (vegeta) and memory-sampling scripts |
| `docs/configuration.md` (Complete examples / Deployment) | ready-to-run configs and systemd/container deployment files, as code blocks (previously the `examples/` directory) |
| `docs/` | documentation set — one owner per topic, everything else links (see below) |

### Documentation responsibilities

One topic, one home; the other pages link instead of repeating.

| Doc | Owns |
|-----|------|
| `configuration.md` | full config reference, logging, tuning, troubleshooting |
| `transport.md` | Noise transport setup: keys, patterns |
| `build-guide.md` | building from source, feature flags, minimal binaries |
| `internals.md` | wire protocol and forwarding design (registration, muxing, UDP affinity) |
| `checks.md` | gate tables: every command and how to handle a block |
| `lint-policy.md` | declared lints and waiver discipline |
| `release.md` | release mechanics, versioning, CD test builds |
| `structure.md` | this map |
| `*.zh.md` | Chinese mirrors of the user-facing docs (README, configuration, transport) — governance and contributor docs are English-only |
