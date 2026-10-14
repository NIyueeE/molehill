# AGENTS.md — Repository Rules

This file governs AI coding agents (and, equally, human contributors) working
in this repository. Read it fully before making any change; when resuming an
interrupted session, treat it as a fresh entry and redo the §1 self-check.
HANDOFF.md records the current working state (decisions, open threads) — read
it right after this file. If this file contradicts the actual code, the code
wins — and §3 requires fixing the docs in the same change.

## 1. Entering the repository: routine self-check (every time)

Before touching anything, verify three things:

1. **pre-commit is enabled** — `git config core.hooksPath` must print
   `githooks`. If empty, run (prefer `just setup`, which also installs missing
   tools):

   ```bash
   git config core.hooksPath githooks
   ```

2. **hook dependencies are installed** — four external tools must be on PATH:

   ```bash
   command -v cargo-machete cargo-audit cargo-outdated cargo-deny
   ```

   Install whatever is missing (with `--locked`):

   ```bash
   cargo install cargo-machete cargo-audit cargo-outdated cargo-deny --locked
   ```

   `uv`/`uvx` must also be on PATH (the python bench gates — ruff lint *and*
   format check — run in the pre-commit chain; install:
   `curl -LsSf https://astral.sh/uv/install.sh | sh`).

   Note: `cargo fmt` and `cargo clippy` are guaranteed by the components
   declared in `rust-toolchain.toml`; rustup installs them with the toolchain.

3. **toolchain** — `rust-toolchain.toml` declares `channel = "stable"`; rustup
   resolves the latest stable automatically. Never hardcode a version number
   and never bypass this file.

When in doubt about environment health, run `githooks/pre-commit` end to end
as a smoke test (the first run of `cargo audit` fetches the RustSec database;
slowness is normal).

## 2. Lint errors: waiver discipline

Principle: **fix the code first; a waiver is the last resort, and only
code-level.**

- Never "make errors disappear" by editing `Cargo.toml` `[lints]`,
  `ruff.toml`, `githooks/pre-commit`, or any check command. Never delete,
  comment out, or bypass a check.
- Every check follows the **same discipline** — the Rust lints, the two ruff
  gates, and all the audits and extra checks (machete, audit, deny, outdated,
  docs-sync, secret scan, and anything added later): fix if fixable, waive
  only when genuinely unfixable, in code, at the narrowest scope, with a
  reason.
- The waiver *forms* — which attribute, what scope counts as minimal, the one
  sanctioned module-level exception, the `unsafe` rule and the python policy —
  and the declared lint set are owned by
  [docs/lint-policy.md](docs/lint-policy.md): read it before writing a waiver.
  What each gate runs, and what to do when one blocks you, is
  [docs/checks.md](docs/checks.md).
- "The checks" are the two hook layers plus the tag review: fast gates
  (`githooks/pre-commit`) on commit, heavy gates (`githooks/pre-push`) on push,
  the light release review (`githooks/pre-tag`) on a `v*` tag — §5, §8.

## 3. Before every commit: docs ↔ code alignment (every commit)

- Verify the docs still tell the truth about the code:
  - lint tables in docs/lint-policy.md ↔ `[lints]` in `Cargo.toml`;
  - gate tables in docs/checks.md ↔ the actual commands in the hooks
    (`githooks/pre-commit`, `githooks/pre-push`, `githooks/pre-tag`);
  - README.md / README.zh.md as landing pages: quick-start commands, docs
    index links, and feature claims still hold;
  - toolchain description ↔ `rust-toolchain.toml`; layout ↔
    docs/structure.md; command examples; version numbers;
  - source doc comments (`//!` / `///`) ↔ actual behavior.
- User-facing docs (README, configuration, transport, benchmarks, deployment)
  keep Chinese mirrors (`*.zh.md`) and must change together; never update one
  language only. Governance and contributor docs (checks, lint-policy,
  release, structure, internals, build-guide, AGENTS.md, HANDOFF.md) are
  English-only by decision — do not create `*.zh.md` for them. When touching
  any page, at minimum keep it truthful.
- **One topic, one home.** Every fact is written once, in the page that owns
  it; every other page links to it. Putting a fact in two places guarantees
  that one of them goes stale. **The routing table is
  [docs/structure.md](docs/structure.md), "Documentation responsibilities"** —
  it names the owning page for every topic, the audience for every page, and
  what each page does *not* own. It is the single copy of that contract: route
  by it before you write, and add a row to it in the same commit as a new
  page.
- Two consequences worth stating, because both have been violated:
  - **A user-facing page never explains the repository's own history.** "What
    replaced the old tables", "the previous harness measured X", why a model
    or a design changed, and incident post-mortems belong in `CHANGELOG.md`
    (what a release changed) or `HANDOFF.md` (why, and what it cost). A
    landing page states what is true now and links onward. The one thing that
    *does* belong on a user page is an **upgrade instruction**: "this key was
    renamed, write that instead" is something a reader has to act on, so the
    configuration page keeps its old→new migration callouts. The instruction
    stays; the story does not.
  - **Guidance about *using* the tool never sends a reader to a contributor
    page.** `HANDOFF.md`, `AGENTS.md` and `docs/checks.md` are for people
    changing the repository; a user who wants to configure, measure or deploy
    is sent to `configuration`, `transport`, `benchmarks` or `release`. The one
    exception is the landing page's own "for people changing it" index group,
    which is how a contributor finds those pages in the first place — keep that
    group clearly labelled and keep it out of the usage sections. Contributor
    pages link each other freely.
- Changing lint config or the check chain requires syncing the affected docs
  pages, both READMEs, and this file **in the same commit**.
- The mechanical part is automated in `githooks/check-docs`, wired into the
  pre-commit chain. It only covers greppable invariants (hook commands ↔
  docs/checks, lint names ↔ docs/lint-policy, edition, channel, just recipes,
  README docs index, CI entry, CHANGELOG extraction, test-build entry,
  secret-scan gate).
  **Semantic alignment** (outdated prose, runnable examples, consistent tone)
  cannot be mechanized — it stays with the agent or a human reviewer.

## 4. Commit message convention

- **English only**, regardless of the author's language.
- Conventional Commits prefixes: `feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`, `test:`, `ci:`, `perf:`.
- Subject line: imperative mood ("add", not "added"), ≤ 72 characters, no
  trailing period.
- Body (optional): explain **why**, wrap long lines; breaking changes append
  `!` to the type and carry a `BREAKING CHANGE:` footer.
- Every commit must pass the pre-commit gate — it runs automatically; do not
  use `--no-verify`.

## 5. Releases: tag-driven, automated

- **Releases are tag-driven.** The only trigger of a release is pushing a
  `v*` tag; `.github/workflows/release.yml` owns the whole flow and no other
  path publishes a release.
- **Versioning continues from the fork point**: the version line started at
  v0.6.0, where molehill forked from
  [rathole](https://github.com/rapiz1/rathole) (upstream's last release was
  v0.5.0), and has been numbered independently since. Never renumber.
- **The wire protocol version moves with the release line, and no two tags are
  compatible.** A dialect is defined by a release: the number changes when the
  wire changes, it changes *with* the tag that introduces it, and
  `SUPPORTED_PROTO_VERSIONS` is the current dialect alone — a peer from another
  tag is refused on the connection it happens on, in both directions, and
  upgrading means upgrading both ends. Two releases share a number when their
  wire is the same (0.8.x and 0.9.x are both v3). While a dialect's release is
  still in development it is still being defined, so commands may be extended
  **in place** under that number; once the tag exists, the next change to the
  wire is the next number. The line so far: 0.6 v1, 0.7 v2, 0.8–0.9 v3, 0.10 v4
  — each read off its tag's `CURRENT_PROTO_VERSION`. The unreleased line on top
  of v0.10.0 is v5; it has no tag yet, so its commands may still be extended in
  place.
- `CHANGELOG.md` is the **single source of release notes**, maintained in
  [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format and
  following [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
  During development, record notable changes under `## [Unreleased]`; before
  tagging, that content moves into exactly one dated
  `## [x.y.z] - YYYY-MM-DD` section (`v` prefix on the git tag) and
  `[Unreleased]` is left empty. A missing, empty or duplicated section
  **fails the release**; never hand-edit release notes on GitHub — the
  changelog is the source.
- **Tag-push policy: no casual release pushes.** Commits are always allowed —
  the fast gates guard them and they trigger nothing public. Pushing a `v*`
  tag is a deliberate release act; **all** of the following must hold before
  pushing one:
  1. an explicit human request (agents must never create release tags on
     their own initiative);
  2. `version` in `Cargo.toml` equals the tag version;
  3. a dated `## [x.y.z] - YYYY-MM-DD` section exists in `CHANGELOG.md`, is the
     only section for that version, contains prose, and `[Unreleased]` is
     empty (every extractor takes the first match, so a stray duplicate in
     front of the real section would be published as the notes);
  4. `just check` is green on the tagged commit;
  5. the benchmark ritual is done (docs/release.md):
     `results-soak-vX.Y.Z.json`, the new chart, and the README benchmark
     table are refreshed in the release commit, and `just soak-check` is
     green against the previous tag — capacity, SLO and drift must not
     regress.
  Re-tagging is allowed only to fix a failed release (delete the tag, fix,
  re-push). For verifying a commit without releasing, use CD test builds (§6).
- **Mechanics live in [docs/release.md](docs/release.md)**: the exact changelog
  steps, what `githooks/pre-tag` verifies (before the tag exists via
  `just tag`, and again on a tag push, since git has no native tag hook), and
  what `.github/workflows/release.yml` builds and publishes.

## 6. CD test builds: per-commit, per-platform artifacts

`.github/workflows/test-build.yml` builds **test artifacts** from any commit
without ever creating a release — dispatch it manually from the Actions tab
with a `ref` (commit SHA, branch, or tag) and `targets` (`linux`, `macos`,
`windows`). Artifacts are ephemeral (7-day retention): never hand out
release links for them, and never reference them in the changelog. Typical
uses: verifying that a specific commit compiles on all platforms before
tagging (§5), and reproducing platform-specific issues on an exact commit.
Details: [docs/release.md](docs/release.md).

## 7. Provenance: independent project, rathole lineage

- molehill is an independent project that began as a fork of
  [rathole](https://github.com/rapiz1/rathole) (Apache-2.0). Upstream history
  is preserved intact below the v0.6.0 fork commit and the version line
  continues from there. Development has long since diverged — a different
  configuration model, protocol and feature set — so upstream is history,
  not the reference point for the design.
- When porting an upstream fix, credit it in the commit body
  (`Ported from rathole <sha>.`) and add a CHANGELOG entry in the same
  commit.
- Do not re-sync wholesale with upstream: v0.7.0 replaced the configuration
  model and the wire protocol (v2). Cherry-pick consciously; note any
  conflict with the dynamic-registration design in HANDOFF.md.

## 8. Day-to-day operations

- commit → fast gates; push to a branch → heavy gates; **push of a `v*` tag →
  release (§5, deliberate)**; PR (any branch) or push to `main`/`dev` → CI
  runs the identical chain when the change touches code. A docs-only change
  (markdown, `docs/`, `assets/`) skips that chain and runs `docs.yml`'s
  docs-alignment check instead — the one gate such a change can break. **The
  hooks mirror that split** (`githooks/docs-only` classifies the staged paths
  or the pushed range): pre-commit keeps only the secret scan and the docs
  check, pre-push keeps only the docs check, and anything ambiguous — an empty
  change set, a new branch, a tag push, a commit mixing docs with code — falls
  back to the full chain. Deleting the classifier restores the full chain,
  which is the safe direction. `main`
  is not branch-protected today — the
  `full check chain` check and the no-force-push rule are enforced by
  convention (CI red on main is the top priority; the one sanctioned
  exception to history rules: a coordinated history rebuild, explicitly
  requested and backed up first). Enabling branch protection is a repo
  settings change for a human to make.
- Formatting: `just fmt` (Rust) and `just py-fmt` (python bench scripts)
  auto-fix; `just check` rehearses the whole chain before committing.
- Dependencies: add or remove them only through cargo — `cargo add` (add
  `--dev` for dev-dependencies) and `cargo remove`. Never hand-edit the
  `[dependencies]` / `[dev-dependencies]` tables in `Cargo.toml`: `cargo add`
  resolves a compatible version requirement and updates `Cargo.lock` in the
  same step, avoiding hand-written specs that drift from the lock or trip the
  dependency gates.
- Maintenance: Dependabot opens weekly updates for GitHub Actions and cargo
  dependencies; they merge only with CI green.
- Security reports go through GitHub's private vulnerability reporting
  (SECURITY.md), never public issues.

## 9. Working discipline (daily rules)

- **Stage with eyes open.** Review `git status` and stage selectively
  (`git add -p`); never blanket `git add -A` while the worktree holds
  unrelated changes. One commit = one logical change: features, refactors,
  and fixes do not share a commit.
- **main stays releasable.** Direct pushes to main are allowed, so CI red on
  main is the top priority — fix it before starting new work; experiments go
  to a branch.
- **No drive-by dependency upgrades.** Upgrades are Dependabot's job (or a
  dedicated commit); never bundle them into feature work — keep bisect clean.
- **CHANGELOG as you go.** A user-visible change and its `## [Unreleased]`
  entry land in the same commit; never backfill at release time (§5).
- **Prove it, don't assume it.** Every "it works" claim must be backed by
  real command output from this session; no output, no claim. Measurements follow §10.
- **Shell hygiene for commits and bulk edits.**
  - Commit messages containing backticks, quotes or parentheses (TOML keys,
    markdown) go through a quoted heredoc or a file (`git commit -F - <<'EOF'`)
    — never inline them in `-m "..."`: backticks trigger shell command
    substitution and parentheses break the parser mid-message.
  - Bulk-edit scripts: `assert old in s` before every `s.replace(old, new)` —
    a silently non-matching anchor leaves a half-edited file; after the
    sweep, grep for the old names to prove the rename is complete. Keep
    triple quotes out of heredoc-python that already uses triple quotes —
    use single-quoted strings or `<<'PYEOF'`.
  - `pkill` self-match: use bracket patterns (`[s]erver`) and never combine
    a pkill and a process launch in one bash call — the outer shell matches
    its own command line.
- **No corpses.** Commented-out code and `todo!()` stubs get removed, not
  accumulated (the `todo` lint already watches).
- **End-of-session ritual.** A session ends with `just fmt` + `just check`,
  everything committed and pushed — never a dirty tree, never unpushed
  commits.
- **Timebox rabbit holes.** Three failed attempts on the same problem: stop,
  write the findings into HANDOFF.md, and ask the human.
- **Log levels follow the contract, not the mood.** The definitions live in
  `docs/configuration.md`, "What each level means": `ERROR` = a human must act,
  `WARN` = handled and worth one line, `INFO` = lifecycle, `DEBUG` = one
  connection's business. A failed request is therefore `DEBUG` however alarming
  it looks, and a condition that can repeat is reported once
  (`logging::RepeatNotice`) and then at `DEBUG`. `tests/log_budget_test.rs`
  fails a healthy run that emits a single `WARN` or `ERROR`, so this is
  enforced rather than intended — never relax it by adding a loud line to a hot
  path.
- **Clear → act; ambiguous or irreversible → ask.** Renames, deletions,
  settings changes, and anything touching releases need the human's go.
- **Secrets never enter the repository.** Tokens, keys, and credentials live
  in repo settings / environment only — never in code, docs, or commits.
  Enforced mechanically by `githooks/check-secrets` in the pre-commit chain;
  a line that must carry a secret-shaped string takes a
  `security-scan:allow` marker with a reason.

## 10. Measurement discipline (benchmarks, probes, results)

Every performance number in this repository is a claim about the *path under
test*. These rules come from real incidents (the 2026-09-10 measurement
revision) and are as binding as the lint discipline in §2.

- **Measure the path you claim.** The benchmark client must dial the
  endpoint under test (the tool's exposed port), never the backend it
  forwards to. The pre-schema-v3 results dialed the backend and reported the
  loopback iperf3 ceiling (~46 Gbit/s) for every tool; the same error
  reappeared when a sampler was refactored to reuse the backend port. Record
  the endpoint in the data (the port the probe dials vs the backend's) and
  assert the invariant: the Soak runner raises when a throughput sample
  dials the backend instead of the tool's exposed port.
- **Instrument parameters are part of the method.** Queue depth, pacing
  rate, client timeout and the window convention change the result, so they
  are recorded in the results meta (the stage order, the settle window, the
  sample rates) and stated in the README. A constant buried in a function
  is a method nobody can audit: a
  hardcoded shallow queue shaped rate cells at ~30% of the nominal rate for
  a whole baseline.
- **One failure must not poison the next sample.** A wedged iperf3 server
  (single-test by design) turned every later repetition into a `null` and
  made the 8-stream slots at the rate cells look unmeasurable. Restart the
  external tool after a failed sample, scale client timeouts with the test
  length, and record the restart.
- **Every failure leaves evidence.** Keep raw per-sample artifacts (exact
  command, stdout, stderr, exit status) and a typed reason. A bare `null` is
  a guess; the raw artifacts (the run's `iperf-raw/<test> <streams>/`
  directory) are what let a 0-byte sample be diagnosed and the backend-dial
  bug be caught, and the gate re-checks the recorded endpoint invariant so the
  same mistake cannot pass unnoticed.
- **State one convention and apply it everywhere.** Define the denominator
  once (here: bytes over the measured window, with the receiver's own window
  beside it), apply it to every tool, and never take `max()` of two sides to
  look better. When one side's accounting is provably degenerate (a fast
  sender into a slow shaper), document the fallback and flag it rather than
  hiding it.
- **Variance is data; do not conclude across it.** Record min/max, quote the
  spread, and refuse to build a claim on a difference inside it. State the
  comparability boundary of every baseline: same schema, same method, same
  host.
- **A metric without contrast is not a measurement.** If every arm returns
  the same value, or the probe only ever reaches its own ceiling, remove the
  chart panel and the claim — and say why in the README and HANDOFF — rather
  than drawing a degenerate panel.
- **Re-check every consumer after reshaping data.** Renaming one field
  silently nulled `mixed_bulk_latency.bulk_gbps` on every loopback arm, and
  the plot still read a removed key and rendered placeholders. Grep for
  every reader of a key you touch, and keep a completeness gate in the
  ritual (`soak_check.py`: the completeness of every test's series, the
  endpoint invariant, the SLO verdicts).
- **Prove provenance.** A run must correspond to a committed revision and a
  freshly built binary; check the binary's reported version/hash before
  trusting its numbers (a binary two commits behind HEAD was caught that
  way, and its numbers would have described code that no longer existed).
- **Docs move with the data.** The method's public home is
  `docs/benchmarks.md` (+ `.zh.md`): it owns what is measured, how to read a
  chart, the stage schedule, the test types, the per-decision measurements and
  how to reproduce a run. A method or number change updates that page, the
  numbers in `README.md` / `README.zh.md`, and the configuration guidance;
  `docs/release.md` owns the ritual and says what is no longer comparable, and
  `HANDOFF.md` keeps the measurement record. A re-numbered table with stale
  conclusions is worse than no table. This section (§10) owns the *rules* that
  bind future measurements, not the description of any one run.

## 11. Project facts (appendix)

Details that agents need constantly — the ones with a single home elsewhere are
pointers, not copies:

- **What it is**: a secure, stable, high-performance reverse proxy for NAT
  traversal (a Rust alternative to frp / ngrok). Server runs on a public
  host, client behind NAT; a control channel carries commands, data channels
  carry forwarded traffic.
- **Crate**: `molehill-rathole`, binary `molehill`, edition 2024,
  Apache-2.0. Feature-gated: `server` / `client` modes; `noise`;
  `hot-reload`; `multiplex` (yamux, in the default set); `kcp` (optional
  KCP-over-UDP data tunnels, in the default set); `transparent` (a
  client-owned public `ip:port` carried as whole IP packets over a TUN device,
  Linux only, in the default set); `embedded` (minimal).
  Clippy runs twice in the pre-commit gate: the second pass covers the
  minimal no-default-features `server,client` build that the default-feature
  pass never compiles.
- **Tests are serial** (`--test-threads=1`): integration tests spawn real
  server/client pairs on fixed ports. `cargo run -- server.toml|client.toml`;
  `cargo run -- --genkey` (noise keypair).
- **Protocol**: v5, one control session per endpoint; the rule and the
  release line are §5, the wire and the forwarding design are
  [docs/internals.md](docs/internals.md).
- **Build profiles** (`release` with lto/strip/panic=abort, `minimal` at
  `opt-level = "z"`, `bench`) and the scratch container image are
  [docs/build-guide.md](docs/build-guide.md).
- **Unsafe** is denied crate-wide; the modules that opt in per item, the
  `src/mux.rs` forbid and the single surviving `#[allow]` are
  [docs/lint-policy.md](docs/lint-policy.md) ("Unsafe").
- **Bench/test entries are PEP 723 python scripts run via `uv run`**; their
  lint and format gates and the waiver rule are
  [docs/lint-policy.md](docs/lint-policy.md) ("Python (ruff)").
- **Where everything lives**: [docs/structure.md](docs/structure.md) — the
  file map and the topic-ownership contract (§3).

## 12. One-line summary

> Self-check the environment on entry; when a check blocks you, fix the code —
> waive only as a last resort, locally, with a named reason; keep docs and
> code in the same commit; write commit messages in english; commits are free,
> release tags are deliberate; let releases speak through CHANGELOG.md; count
> versions from the fork point; prove every claim with real output and
> measure the path you claim (§10); end
> sessions clean; secrets never enter the repo.
