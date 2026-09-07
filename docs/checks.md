# Checks


Fast gates run before every commit, heavyweight gates before every push, and CI
runs the whole chain on every push / pull request via `just check`.

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

Note the template difference: molehill's TLS backends (`native-tls` vs
`rustls`) and their websocket variants are **mutually exclusive**, so clippy
runs twice (default features, then `server,client` only) instead of once with
`--all-features`. `just check` runs the identical chain.

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

## One-shot run

```bash
just check   # identical to hooks + CI
```

Other recipes: `just fmt` (auto-fix), `just test`, `just powerset` (feature
powerset via cargo-hack, CI's `features` job), `just container` (scratch image).

## When a gate blocks you

Fix the code first. A waiver is the last resort: code-level only
(`#[expect(...)]` preferred over `#[allow]`), minimal scope, with a reason
comment. Never weaken `[lints]`, the hooks, or CI. See
[Lint policy](lint-policy.md) and [AGENTS.md](../AGENTS.md).
