# Repository structure


What every file and directory in this repository is for, and which page owns
which topic (see "Documentation responsibilities" below — the ownership
contract, not just a list). Deeper docs: [configuration](configuration.md),
[deployment](deployment.md), [transport](transport.md),
[benchmarks](benchmarks.md), [internals](internals.md),
[build guide](build-guide.md).

## Root

| Path | Purpose |
|------|---------|
| `Cargo.toml` | crate manifest; `[lints]` is policy documented in [lint-policy](lint-policy.md) |
| `Cargo.lock` | locked dependency graph (committed; verified with `--locked` in CI builds) |
| `build.rs` | build metadata injection via vergen (git SHA, timestamp, features, target) |
| `justfile` | task runner: `just setup` / `fmt` / `py-fmt` / `test` / `test-fast` / `check` / `interop` / `l3-accept` / `tag` / `tag-check` / `powerset` / `py-lint` / `bench-deps` / `soak` / `soak-peers` / `soak-plot` / `soak-check` / `container` |
| `rust-toolchain.toml` | `channel = "stable"` + clippy/rustfmt components; never hardcode versions |
| `deny.toml` | cargo-deny policy: licenses, bans, advisories (pre-push + CI) |
| `ruff.toml` | ruff configuration for the python bench/test entries: the lint set and the repo-wide waivers (see [lint-policy](lint-policy.md)) |
| `.editorconfig` | editor defaults (4-space Rust, 2-space YAML/TOML, md keeps trailing spaces) |
| `AGENTS.md` | repository rules for AI agents and humans; entry point for every session |
| `HANDOFF.md` | current working state, decisions, open threads — read after AGENTS.md |
| `HANDOFF-archive.md` | the closed half: incidents, superseded sweeps, measurement records and retracted diagnoses, indexed at the top |
| `CHANGELOG.md` | single source of release notes (Keep a Changelog); gates releases |
| `CONTRIBUTING.md` / `SECURITY.md` | contributor setup; private vulnerability reporting |
| `README.md` / `README.zh.md` | bilingual landing pages; must document edition, channel, `just setup`, `just check`, hooks activation |
| `Containerfile` | scratch container image assembled from musl release artifacts |
| `LICENSE` | Apache-2.0 |
| `assets/` | logo and benchmark charts |

## Automation

| Path | Purpose |
|------|---------|
| `githooks/pre-commit` | fast gates: fmt, secret scan, machete, docs alignment, ruff lint + format, clippy ×2 |
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
| `src/protocol.rs` | wire protocol (Hello/Auth/Ack/commands, the session commands and per-service prologue), postcard serialization, protocol version |
| `src/common.rs` + `src/common/` | constants, DNS/keepalive/retry helpers, `MultiMap`, the `AsyncWriteOwned` owned-write capability boundary (`owned_write.rs`), the idle-reaping forward wrapper (`forward.rs`) |
| `src/config.rs` + `src/config/` | TOML parsing/validation (`Config`, `ClientConfig`, …, `MaskedString`), the L3 client's own block (`transparent.rs`: `TransparentClientConfig`, which lowers into the `ClientConfig` the client engine runs on), the hot-reload watcher |
| `src/core/client.rs` | client mode: one control session per endpoint (auth, per-service registration, per-service state), data-channel requests, the UDP hub |
| `src/core/server.rs` | server mode: registration policy, eager binding, the session registry, connection pools, UDP affinity |
| `src/logging.rs` | colored span-aware log formatter |
| `src/transport.rs` + `src/transport/` | `Transport` trait + tcp (plain) / noise (the `noise_stream.rs` record wrapper — ported from snowstorm and since extended in-repo: one-sweep record reads, direct decrypt into the caller's buffer, owned-record writes, opt-in session resume) / multiplex (the tunnel pool, placement, growth and shrink) / `pool.rs` (that policy's arithmetic, kept pure and separately testable) / kcp implementations |
| `src/kcp.rs` + `src/kcp/` | internal KCP (ARQ) protocol engine — self-maintained, algorithm aligned with the reference C implementation by skywind3000, plus the SACK extensions the adapter needs; kept in-repo so nothing external needs patching and the module follows molehill's own rules. The tokio adapter around it (pump task, channels, send batching / receive coalescing, pacer, keepalive) is `src/transport/kcp.rs` |
| `src/mux.rs` + `src/mux/` | the yamux framing engine — vendored from rust-yamux 0.14 and maintained in-repo (like the KCP engine), wire-identical with the yamux specification and tokio-native (tokio IO traits, no compat shim); the `multiplex` transport integrates it through `src/transport/multiplex.rs` |
| `src/stripe.rs` | the stripe group: spreads one visitor connection over K data channels with numbered 32 KiB chunks (`[server.data] stripe_count`, default 1 = off) — the frame a chunk is read into crosses to the stripe by ownership, and the receiver reassembles by sequence number; design in docs/internals.md, "Data-channel striping" |
| `src/transparent/` | the transparent (L3) data path — attaching to and reading/writing a TUN device (`tun.rs`, the one audited `TUNSETIFF` ioctl), the header-only packet parser (`ip.rs`), the per-device hub with its endpoint table and per-service queues (`hub.rs`), the endpoint table, drop counters and stats line (`mod.rs`), and the startup prerequisite checks (`check.rs`). Linux-only, behind the `transparent` feature; design in docs/internals.md, "Transparent (L3) services" |

## Tests, benches, examples, docs

| Path | Purpose |
|------|---------|
| `tests/integration_test.rs` | spawns real server+client pairs; TCP/UDP across transports and the session contract |
| `tests/pool_test.rs` | the tunnel pool end to end: a shared pool serving two services, the per-service default, UDP source-port stickiness across a grow/shrink, the first visitor after the pool shrank, a burst spreading over tunnels while it is placed, an unanswerable visitor not parking the service, the valve refusing growth without killing the tunnel, and the opt-in telemetry lines from a real binary |
| `tests/session_test.rs` | the session server contract driven by a hand-written client (one session, N services, per-service rejection, deregistration, the tunnel prologue) |
| `tests/log_budget_test.rs` | drives the real binary and counts what an operator sees: a healthy run must emit no WARN/ERROR, bounded INFO, and no message shape more than three times |
| `tests/interop_test.rs` | this build against the previous release's binary: each side refuses the other's dialect on that connection alone (v0.10 serves v4 only), an unknown dialect is refused the same way, and a refusing process keeps serving a peer of its own version (`#[ignore]`d; `just interop` sets `MOLEHILL_OLD_BIN`) |
| `tests/hot_reload_test.rs` | the config watcher against a real binary: a changed file reaches the running service |
| `tests/noise_keys_test.rs` | `--genkey` and the Noise key/PSK paths end to end, including the refusal when a key does not match |
| `tests/startup_failure_test.rs` | a process that cannot serve its instance ends instead of running deaf |
| `tests/common/mod.rs` | echo/pingpong hitters and runner helpers |
| `tests/for_tcp/`, `tests/for_udp/`, `tests/config_test/` | integration fixtures: transport variants, the session cases, the control-channel teardown case, valid/invalid configs |
| `benches/` | Soak benchmark model (`scripts/soak/`: uv/PEP 723 python — `soak.py` runner, `lib.py` shared primitives, `soak_check.py` gate, `soak_plot.py` charts, `fetch_peers.py` peer fetcher) with its committed results (`scripts/soak/results-soak-vX.Y.Z.json`) and charts (`assets/soak-vX.Y.Z*.png`); side probes: mux e2e smoke (`scripts/mux/`), HTTP latency (`scripts/http/`), memory sampling (`scripts/mem/`), the interop fetcher (`scripts/interop/fetch_old.py`), the UDP stress probe (`scripts/udp_stress.py`) |
| `benches/scripts/l3/` | the transparent-L3 acceptance harness: `run.sh` builds the real binaries and drives three network namespaces (visitor, server, client) to prove the client owns the public `ip:port` and the server holds no connection state for the flow; `visitor.py` and `echo_service.py` are its PEP 723 peers. Linux-only and root-only, outside the check chain (`just l3-accept`; see docs/checks.md) |
| `docs/configuration.md` | every setting: meaning, default, allowed values, logging, tuning, troubleshooting, and the decision tree |
| `docs/deployment.md` (+ `.zh.md`) | ready-to-run configurations for common scenarios, systemd units, container/compose/Quadlet recipes, network requirements, deployment security |
| `docs/benchmarks.md` (+ `.zh.md`) | how the published numbers are produced, read and reproduced — the home of the benchmark method |
| `docs/` | documentation set — one owner per topic, everything else links (see below) |

### Documentation responsibilities

**One topic, one home.** Every fact is written once, in the page that owns it,
and every other page links to it. A fact written twice is a fact that will go
stale in one of the two places — this table is the contract. AGENTS.md §3
routes by it, and carries the two rules that follow from it (what a user page
must never explain, and who a user page may link to).

| Doc | Audience | Owns | Does **not** own |
|-----|----------|------|------------------|
| `README.md` + `.zh.md` | users | the landing page: what molehill is, quick start, the published numbers, the docs index | method, release mechanics, configuration reference |
| `configuration.md` + `.zh.md` | users | every setting: meaning, default, allowed values, logging, tuning, troubleshooting | measured costs (→ benchmarks), release history, the recipes (→ deployment) |
| `deployment.md` + `.zh.md` | users | how to stand it up: worked examples, systemd units, container/compose/Quadlet recipes, network requirements, deployment security | what a setting means (→ configuration), measured costs (→ benchmarks) |
| `transport.md` + `.zh.md` | users | Noise transport setup: keys, patterns, resume | configuration reference, measured costs |
| `benchmarks.md` + `.zh.md` | users | the benchmark method: what is measured, how to read the charts, the stage schedule, the SLO, the test types, per-decision measurements, comparability, how to reproduce a run (including the two-build screen) | the release ritual and its gate (→ release), configuration reference |
| `build-guide.md` | contributors | building from source, feature flags, minimal binaries | gate tables, release mechanics |
| `internals.md` | contributors | wire protocol and forwarding design (registration, muxing, UDP affinity, striping) | measured comparisons (→ benchmarks) |
| `checks.md` | contributors | gate tables: every command a hook runs and how to handle a block | lint levels, release mechanics |
| `lint-policy.md` | contributors | declared lints, waiver discipline (Rust and python) | what the gates run |
| `release.md` | contributors | release mechanics, versioning, CD test builds, the per-tag benchmark *ritual* and its gate | the measurement method (→ benchmarks) |
| `structure.md` | contributors | this map — what every file is for, and which page owns which topic (the routing table AGENTS.md §3 points to) | any topic's content |
| `HANDOFF.md` | contributors | working state: decisions, open threads, the release checklist | anything a user needs, and the closed records (→ HANDOFF-archive.md) |
| `HANDOFF-archive.md` | contributors | the closed records: incidents, superseded sweeps, measurement records, retracted diagnoses | the current state (→ HANDOFF.md) |
| `AGENTS.md` | contributors | the rules that bind future changes (§2 lint, §5 release, §10 measurement) | topic content |
| `CHANGELOG.md` | users | what changed, per release | design rationale |
| `*.zh.md` | users | Chinese mirrors of the user-facing docs (`README`, `configuration`, `deployment`, `transport`, `benchmarks`) — governance and contributor docs are English-only by decision | — |
