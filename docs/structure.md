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
| `justfile` | task runner: `just setup` / `fmt` / `py-fmt` / `py-lint` / `py-typecheck` / `test` / `test-fast` / `check` / `interop` / `l3-accept` / `tag` / `tag-check` / `powerset` / `bench-deps` / `bench` / `bench-selfcheck` / `container`; every other bench tool is a `just bench <verb>` |
| `rust-toolchain.toml` | `channel = "stable"` + clippy/rustfmt components; never hardcode versions |
| `deny.toml` | cargo-deny policy: licenses, bans, advisories (pre-push + CI) |
| `bench/` | the performance model's own uv project: `pyproject.toml` (dependencies, console script, ruff + pyright configuration), `uv.lock` (committed), and the package under `src/bench/` |
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
| `githooks/pre-commit` | fast gates: fmt, secret scan, machete, docs alignment, ruff lint + format, pyright, bench-model self-check, clippy ×2 (benches-only changes take a compile-free path via `githooks/benches-only`) |
| `githooks/pre-push` | heavy gates: audit, deny, outdated, tests; runs the release review (pre-tag) on `v*` tag pushes |
| `githooks/pre-tag` | light release review (via `just tag` + on tag pushes): tag↔version, changelog section, bench assets, container-job greps, advisory checklist |
| `githooks/docs-only` | change-set classifier: all docs paths → pre-commit/pre-push take the two-gate docs-only path |
| `githooks/benches-only` | change-set classifier: all paths under `bench/` or `benches/records/` → pre-commit skips the cargo gates, pre-push skips the heavy gates (runtime tooling, never compiled) |
| `githooks/check-secrets` | staged-changes secret scan (`security-scan:allow` marker to waive a line) |
| `githooks/check-docs` | docs ↔ code alignment (hook commands, lints, feature-name registry, docs-only paths ↔ the workflows, edition, channel, README index, CI entry) |
| `githooks/changelog-notes` | the one extractor for a version's `CHANGELOG.md` section (single dated section, prose, empty `[Unreleased]`); called by `githooks/pre-tag` and `release.yml` |
| `.github/workflows/ci.yml` | `just check` chain + feature powerset (plus test-target compilation for the transparent set) + per-feature test matrix + minimal-size check + musl checks (full set on x86_64 and native aarch64; `embedded` via cross) + native musl test jobs (x86_64 and aarch64) + 5-target builds/tests, including both macOS architectures on native runners (skipped for docs-only changes) |
| `.github/workflows/docs.yml` | the docs-only half: `githooks/check-docs` for changes limited to markdown, `docs/`, `assets/` |
| `.github/workflows/release.yml` | tag-driven release: version/changelog gates, 10-target matrix (each leg native on its architecture), direct release, GHCR, crates.io |
| `.github/workflows/test-build.yml` | manual per-commit CD test builds (never publishes) |
| `.github/dependabot.yml` | weekly cargo + GitHub Actions + uv (`/bench`) updates |
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
| `src/status.rs` | the client's read-only status API: one hand-written `GET /status` on its own loopback listener, JSON only, behind a required bearer token — services from the session and pools from the registry, with no protocol parser on the control connection; the block and its answer are in [configuration](configuration.md), "Status API" |
| `src/logging.rs` | colored span-aware log formatter |
| `src/transport.rs` + `src/transport/` | `Transport` trait + tcp (plain) / noise (the `noise_stream.rs` record wrapper — ported from snowstorm and since extended in-repo: one-sweep record reads, direct decrypt into the caller's buffer, owned-record writes, opt-in session resume) / multiplex (the tunnel pool: establishment, placement and repair) / `pool.rs` (that policy's arithmetic, kept pure and separately testable) / kcp implementations |
| `src/transport/quic/` | the UDP carrier's engine adapter, behind the `quic` feature (not in the default set while the migration is in progress): `crypto.rs` implements the engine's `PacketKey`/`HeaderKey` seam as a pass-through (the payload's confidentiality is the Noise record layer's job, `data_enable` its switch), `session.rs` drives the same Noise KK handshake through the engine's `Session`/`ClientConfig`/`ServerConfig` traits and carries the transport parameters as the handshake payload. The engine is `quinn-proto` (sans-IO, TLS never compiled); design and the phase gate are in HANDOFF |
| `src/kcp.rs` + `src/kcp/` | internal KCP (ARQ) protocol engine — self-maintained, algorithm aligned with the reference C implementation by skywind3000, plus the SACK extensions the adapter needs; kept in-repo so nothing external needs patching and the module follows molehill's own rules. The tokio adapter around it (pump task, channels, send batching / receive coalescing, pacer, keepalive) is `src/transport/kcp.rs` |
| `src/mux.rs` + `src/mux/` | the yamux framing engine — vendored from rust-yamux 0.14 and maintained in-repo (like the KCP engine), wire-identical with the yamux specification and tokio-native (tokio IO traits, no compat shim); the `multiplex` transport integrates it through `src/transport/multiplex.rs` |
| `src/stripe.rs` | the stripe group: spreads one visitor connection over a growing number of data channels with numbered 32 KiB chunks (starts at 1, grows under backpressure) — the frame a chunk is read into crosses to the stripe by ownership, and the receiver reassembles by sequence number; design in docs/internals.md, "Data-channel striping" |
| `src/transparent/` | the transparent (L3) data path — the `tun-rs` device wrapper (`tun.rs`), the wire/device frame conversion (`frame.rs`: Linux vnet/GSO ↔ bare IPv4 or IPv6), the header-only packet parser (`ip.rs`), the per-device hub with its endpoint table and per-claim lane sets (`hub.rs`), the flow placement that spreads a claim's inner flows over its lanes (`flow.rs`), the endpoint table, drop counters and stats line (`mod.rs`), and the platform-specific startup prerequisite checks (`check.rs`). Linux and macOS run both ends; Windows can serve; behind the `transparent` feature (which implies `carrier` and `noise`). Design in docs/internals.md, "Transparent (L3) services" |

## Tests, benches, examples, docs

| Path | Purpose |
|------|---------|
| `tests/integration_test.rs` | spawns real server+client pairs; TCP/UDP across transports and the session contract |
| `tests/pool_test.rs` | the tunnel pool end to end: one pool serving every service of a session, the pool established at its configured count and held there, UDP source-port stickiness across a tunnel's death and repair, a burst spreading over the tunnels that exist, an unanswerable visitor not parking the service, the server valve refusing an establishment without killing the tunnel, and the opt-in telemetry lines from a real binary |
| `tests/session_test.rs` | the session server contract driven by a hand-written client (one session, N services, per-service rejection, deregistration, the tunnel prologue) |
| `tests/log_budget_test.rs` | drives the real binary and counts what an operator sees: a healthy run must emit no WARN/ERROR, bounded INFO, and no message shape more than three times |
| `tests/interop_test.rs` | this build against the previous release's binary: each side refuses the other's dialect on that connection alone, an unknown dialect is refused the same way, and a refusing process keeps serving a peer of its own version (`#[ignore]`d; `just interop` sets `MOLEHILL_OLD_BIN`) |
| `tests/hot_reload_test.rs` | the config watcher against a real binary: a changed file reaches the running service |
| `tests/noise_keys_test.rs` | `--genkey` and the Noise key/PSK paths end to end, including the refusal when a key does not match |
| `tests/startup_failure_test.rs` | a process that cannot serve its instance ends instead of running deaf |
| `tests/status_api_test.rs` | the status API: `GET /status` answers only with its token and only on its path, and the answer is real (services from the session, pools from the registry); the parser's refusals live in `tests/config_test/*/status_*.toml` |
| `tests/common/mod.rs` | echo/pingpong hitters and runner helpers |
| `tests/for_tcp/`, `tests/for_udp/`, `tests/config_test/` | integration fixtures: transport variants, the session cases, the control-channel teardown case, valid/invalid configs |
| `bench/src/bench/` | the measurement model — the repository's one runner and its standard: `model.py` declares every metric, condition, timeline, arm, scenario and profile; `topology.py` builds the one topology and imposes conditions in place; `workloads.py` runs the workloads (bulk, round trips, UDP, the staged sweep, the capacity ramp, reconnect); `runner.py` runs a campaign; `analysis.py` aggregates, measures the run's own noise floor, decides verdicts and gates a release run; `plot.py` renders a results file's charts and tables; `cli.py` is the single entry point; `paths.py` locates the project, the checkout and the user's home; `iperf.py`, `instruments.py` and `hostinfo.py` carry the iperf3 conventions, the instruments and the provenance probes. Linux-only, root-only, outside the check chain except its self-check (`just bench-selfcheck`; see docs/checks.md) |
| `bench/src/bench/targets/` | the tool-adapter layer: `base.py` is the `ToolAdapter` contract (`BinarySet`, `Ports`, `Launch`, `Endpoint`, `Deployment`, `DeploymentContext`); `molehill.py` starts the subject; `control.py` is the tool-free arm; `peers/base.py` is the shared peer contract and `peers/{frp,rathole,nps}.py` adapt the reference tools, registered in `peers/registry.py`; `peers/fetch.py` installs their newest prebuilt releases. A new peer is an adapter plus a registry row, never a branch in the runner |
| `bench/src/bench/probes/` | the traffic generators the engine spawns in the visitor namespace, each a standalone PEP 723 script and blind to the tunnel: `rr.py` (round-trip rate and latency; `--fresh` measures the setup cost), `series.py` (the staged run's interactive, UDP and churn streams as NDJSON), `udp.py` (paced loss and spacing, and the blast ceiling) |
| `bench/src/bench/utils/interop.py` | fetches the **previous release's** binary for the interop matrix — the asset users download, never a build from source; resolves the newest tag and prints the path `just interop` exports as `MOLEHILL_OLD_BIN` (`just bench interop`) |
| `bench/src/bench/utils/udp_stress.py` | the UDP worker-queue overflow instrument (D27's missing evidence): offers load a worker's 1024-datagram queue can actually feel, with a receive-only sink reported beside the offered rate and a control arm; an unreached step is called inconclusive, not "no drops" (`just bench udp-stress`) |
| `bench/src/bench/utils/mux/gap_profile.sh` | the multiplexing *attribution* tool: builds a symbol-carrying bench-profile binary, runs one iperf3 flow through one molehill pair on loopback and reports where the CPU went (self time per symbol and per daemon), which is what turns "multiplexing costs +87 % CPU per byte" into a list of functions. Not a measurement — the cost of an arm is the model's job; this answers *where*, not *how much*. Outside the check chain (`just bench mux`) |
| `bench/src/bench/utils/l3/` | the transparent-L3 *acceptance* harness: `run.sh` builds the real binaries and drives three network namespaces (visitor, server, client) to prove the client owns the public `ip:port` and the server holds no connection state for the flow; `visitor.py` and `echo_service.py` are its PEP 723 peers, and `wire_report.py` turns the per-arm `/proc/net/dev` and CPU samples into carried packet sizes, wire bytes per carried packet and the ceiling a header compressor could reach; `tcp_tso_census.py` is the GSO census behind the L3 path's offload decision. `run_ipv6.sh` is the focused IPv6 acceptance companion (`just l3-accept-ipv6`). These are assertion tools, not measurements. Linux-only and root-only, outside the check chain (see docs/checks.md) |
| `benches/records/` | the published results of the release sweeps (`results-soak-vX.Y.Z.json` from the retired soak model, `results-bench-vX.Y.Z.json` going forward), with their charts in `assets/`. The only thing left under `benches/`; a results file is evidence for a published number, not scratch space |
| `docs/configuration.md` | every setting: meaning, default, allowed values, logging, tuning, troubleshooting, and the decision tree |
| `docs/deployment.md` (+ `.zh.md`) | ready-to-run configurations for common scenarios, systemd units, container/compose/Quadlet recipes, network requirements, deployment security |
| `docs/benchmarks.md` (+ `.zh.md`) | how the published numbers are produced, read and reproduced — the home of the benchmark method |
| `docs/` | documentation set — one owner per topic, everything else links (see below) |

### Documentation responsibilities

**One topic, one home.** Every fact is written once, in the page that owns it,
and every other page links to it. A fact written twice is a fact that will go
stale in one of the two places — this table is the contract.

**One audience, one voice.** The Audience column is the voice contract:
**users** read task-oriented pages that state what is true now, **contributors**
read instructional pages that may explain design, **agents** read binding rules
and working state, and a **vulnerability reporter** reads `SECURITY.md`.
AGENTS.md §3, "Audience and voice", carries the rules; this table is the
roster. Route by it before you write, and add a row in the same commit as a new
page.

| Doc | Audience | Owns | Does **not** own |
|-----|----------|------|------------------|
| `README.md` + `.zh.md` | users | the landing page: what molehill is, quick start, the published numbers, the docs index | method, release mechanics, configuration reference |
| `configuration.md` + `.zh.md` | users | every setting: meaning, default, allowed values, logging, tuning, troubleshooting | measured costs (→ benchmarks), release history, the recipes (→ deployment) |
| `deployment.md` + `.zh.md` | users | how to stand it up: worked examples, systemd units, container/compose/Quadlet recipes, network requirements, deployment security | what a setting means (→ configuration), measured costs (→ benchmarks) |
| `transport.md` + `.zh.md` | users | Noise transport setup: keys, patterns, resume | configuration reference, measured costs |
| `benchmarks.md` + `.zh.md` | users | the benchmark method: what is measured, how to read the charts, the stage schedule, the SLO, the test types, per-decision measurements, comparability, how to reproduce a run (including the two-build screen) — and the bench model's standard: its metric registry, scenarios, profiles, verdict rules and how to run, report and compare a campaign | the release ritual and its gate (→ release), configuration reference, what each gate runs (→ checks) |
| `CHANGELOG.md` | users | what changed, per release (past tense, one change per entry) | design rationale |
| `CONTRIBUTING.md` | contributors | setup, the gate summary, the commit convention, test and release pointers | the full gate tables (→ checks), the waiver forms (→ lint-policy) |
| `build-guide.md` | contributors | building from source, feature flags, minimal binaries | gate tables, release mechanics |
| `internals.md` | contributors | wire protocol and forwarding design (registration, muxing, UDP affinity, striping) | measured comparisons (→ benchmarks) |
| `checks.md` | contributors | gate tables: every command a hook runs and how to handle a block | lint levels, release mechanics |
| `lint-policy.md` | contributors | declared lints, waiver discipline (Rust and python) | what the gates run |
| `release.md` | contributors | release mechanics, versioning, CD test builds, the per-tag benchmark *ritual* and its gate | the measurement method (→ benchmarks) |
| `structure.md` | contributors | this map — what every file is for, and which page owns which topic (the routing table AGENTS.md §3 points to) | any topic's content |
| `HANDOFF.md` | agents | working state: decisions, open threads, the release checklist | anything a user needs, and the closed records (→ HANDOFF-archive.md) |
| `HANDOFF-archive.md` | agents | the closed records: incidents, superseded sweeps, measurement records, retracted diagnoses, and each cycle's investigation | the current state (→ HANDOFF.md) |
| `AGENTS.md` | agents | the rules that bind future changes (§2 lint, §5 release, §10 measurement) | topic content |
| `SECURITY.md` | vulnerability reporters | supported versions, private reporting, scope notes | release mechanics, contributor workflow |
| `*.zh.md` | users | Chinese mirrors of the user-facing docs (`README`, `configuration`, `deployment`, `transport`, `benchmarks`) — governance and contributor docs are English-only by decision | — |
