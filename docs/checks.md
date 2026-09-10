# Checks

Three layered gates guard the repository, split by moment and weight:

| Moment | Gate | Weight | What it guards |
|--------|------|--------|----------------|
| every commit | `githooks/pre-commit` | fast (~1 min) | code quality: fmt, secrets, machete, docs sync, ruff, clippy ×2 |
| every push | `githooks/pre-push` | heavy (minutes) | security & dependency policy & freshness & tests (audit, deny, outdated, test) |
| every release tag | `githooks/pre-tag` | light (seconds) | release state: tag↔version, changelog section, bench assets, container-job greps + advisory checklist |

The split is deliberate: pre-commit is the fast per-commit quality loop,
pre-push is the heavyweight loop every push pays (tag pushes included —
releasing is a deliberate act), and pre-tag is a light release-readiness
review. git has no native tag hook, so pre-tag fires at the two tag moments:
`just tag` runs it before creating the tag, and pre-push runs it for every
`v*` tag in a push (before the heavy gates, so a violation fails fast). It is
not part of `just check` — it needs a release state (dated changelog section,
committed bench results) and would fail on ordinary development commits.
release.yml re-enforces the version and changelog invariants remotely; CI runs
pre-commit + pre-push via `just check` on every pull request and on pushes to
`main`/`dev`.

## Tools

The cargo gates use four external tools; `just setup` installs any that are
missing (and activates the git hooks):

```bash
cargo install cargo-machete cargo-audit cargo-outdated cargo-deny --locked
```

`cargo fmt` and `cargo clippy` come with the toolchain declared in
`rust-toolchain.toml` (`channel = "stable"` + clippy/rustfmt components).
The python bench/test entries and the ruff gate run through `uv` / `uvx`
(PEP 723 scripts, see docs/release.md); install uv with
`curl -LsSf https://astral.sh/uv/install.sh | sh`.

## On every commit — `githooks/pre-commit`

| # | Gate | Command | Purpose |
|---|------|---------|---------|
| 1 | fmt | `cargo fmt --all -- --check` | code style |
| 2 | secrets | `githooks/check-secrets` | secret scan on staged changes |
| 3 | machete | `cargo machete` | unused dependencies |
| 4 | docs | `githooks/check-docs` | docs ↔ code alignment |
| 5 | python lint | `uvx ruff check benches/scripts/` | python bench/test entries (ruff.toml) |
| 6 | clippy | `cargo clippy --all-targets -- -D warnings` | strict lints, default features |
| 7 | clippy (gates) | `cargo clippy --all-targets --no-default-features --features server,client -- -D warnings` | feature-gated code paths |

Note the template difference: clippy runs twice (default features, then
`server,client` only) instead of once with `--all-features`, because the
second pass covers the minimal no-default-features build that the
default-feature pass never compiles. `just check` runs the identical chain.

Lines that must carry a secret-shaped string (e.g. key-format documentation)
take a `security-scan:allow` marker with a reason; `check-secrets` skips them.

## On every push — `githooks/pre-push`

| # | Gate | Command | Purpose |
|---|------|---------|---------|
| 8 | audit | `cargo audit` | RustSec security advisories |
| 9 | deny | `cargo deny check` | licenses / bans / advisories policy (deny.toml) |
| 10 | outdated | `cargo outdated --root-deps-only` | outdated direct dependencies |
| 11 | test | `cargo test --quiet -- --test-threads=1` | test suite (serial by design) |

Tests run **serially** (`--test-threads=1`): the integration suite spawns real
server/client pairs on fixed ports; parallel execution races on them.

A push that carries `v*` tags additionally runs the release review
(`githooks/pre-tag`, below) before these heavy gates — releasing is a
deliberate act, and a tag review violation fails the whole push.

## On every tag — `githooks/pre-tag`

A light release review for the next `v*` tag (AGENTS.md §5). Fires at tag
creation (`just tag`) and again on tag push (inside pre-push, evaluated
against the tag's commit — so re-pushing an old tag reviews that tag's state,
not the working tree). All checks are seconds-fast greps against the reviewed
commit; nothing here is a heavy gate.

| # | Check | Purpose |
|---|-------|---------|
| 12 | tag name ↔ `Cargo.toml` version; `Cargo.lock` in sync | release identity |
| 13 | dated, non-empty `## [x.y.z] - YYYY-MM-DD` in `CHANGELOG.md` | release notes single source |
| 14 | `results-vX.Y.Z.json` + `assets/benchmark-vX.Y.Z.png` committed | benchmark ritual deliverables |
| 15 | `Containerfile` + release.yml GHCR job / image tags / `--help` smoke test | container build review (mechanical part) |
| 16 | advisory checklist: CHANGELOG & docs audit, container review, benchmark gate, deliberate-release confirm | human/agent review items |

At tag creation (local mode only) it additionally requires a clean working
tree and that the tag does not exist yet. On a tag push (evaluated against a
commit), missing bench assets are a note instead of a failure — a historical
tag legitimately predates the ritual, and re-pushing one to fix a failed
release must stay possible. The advisory checklist is printed but not
enforced mechanically — no gate can review prose.

## One-shot run

```bash
just check   # identical to hooks + CI (pre-commit + pre-push)
just tag     # release review (githooks/pre-tag) + create the local v* tag
just tag-check   # run only the release review, without tagging
```

Other recipes: `just fmt` (auto-fix), `just test`, `just powerset` (feature
powerset via cargo-hack, CI's `features` job), `just container` (scratch image).

## When a gate blocks you

Fix the code first. A waiver is the last resort: code-level only
(`#[expect(...)]` preferred over `#[allow]`), minimal scope, with a reason
comment. Never weaken `[lints]`, the hooks, or CI. See
[Lint policy](lint-policy.md) and [AGENTS.md](../AGENTS.md).
