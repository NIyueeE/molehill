# Contributing

Thanks for contributing! This repository runs a layered check pipeline; please
make sure it stays green.

## Setup

```bash
just setup        # activate git hooks + install missing check tools
just check        # run the full chain (same as CI)
```

`just setup` installs the four cargo check tools (machete, audit, outdated,
deny) and reports `uvx` / `cargo-hack` when they are missing. The python
gates (ruff lint + format) additionally need `uv`/`uvx` on PATH, installed
with `curl -LsSf https://astral.sh/uv/install.sh | sh` (AGENTS.md §1).

## Check gates

The authoritative gate tables — every command, what it does, and what to do
when a gate blocks you — live in [docs/checks.md](docs/checks.md). In short:
fast gates run on every commit, heavy gates on every push, and CI runs the
same chain via `just check` plus the extra jobs `just check` cannot cover
(feature powerset, the per-feature test matrix, the minimal-size check, the
musl static build, the platform builds).

The full discipline — including when a lint waiver is acceptable — lives in
[AGENTS.md](AGENTS.md) §2 and [docs/lint-policy.md](docs/lint-policy.md). In
short: fix code first; waivers are code-level, minimal scope, with a reason
comment; never weaken the checks.

## Commit messages

Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`,
`test:`, `ci:`, `perf:`), English, imperative subject ≤ 72 chars. Breaking
changes append `!` and a `BREAKING CHANGE:` footer. See AGENTS.md §4.

## Tests

The integration suite spawns real server/client pairs on fixed ports and runs
**serially**:

```bash
cargo test -- --test-threads=1
```

Feature-combination coverage:

```bash
just powerset     # cargo hack feature powerset (what CI's features job runs)
```

## Releases

Releases are tag-driven and changelog-gated — contributors never publish
directly. Record user-visible changes under `## [Unreleased]` in
[CHANGELOG.md](CHANGELOG.md) in the same commit as the change. The mechanics
are documented in [docs/release.md](docs/release.md).

## Reporting vulnerabilities

Do **not** open a public issue — see [SECURITY.md](SECURITY.md).
