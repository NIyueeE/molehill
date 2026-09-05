# Contributing

Thanks for contributing! This repository runs a layered check pipeline; please
make sure it stays green.

## Setup

```bash
just setup        # activate git hooks + install missing check tools
just check        # run the full chain (same as CI)
```

## Check gates

Details and the full gate tables: [docs/checks.md](docs/checks.md) (简体中文:
[docs/checks.zh.md](docs/checks.zh.md)).

- **pre-commit (fast)**: `cargo fmt --check`, secret scan (`githooks/check-secrets`),
  `cargo machete`, docs↔code alignment (`githooks/check-docs`), strict clippy
- **pre-push (heavy)**: `cargo audit`, `cargo deny check`, `cargo outdated`,
  `cargo test` (serial)
- **CI**: the whole chain via `just check`, plus feature powerset, per-feature
  test matrix, minimal-size check, and 4-platform builds

The full discipline — including when a lint waiver is acceptable — lives in
[AGENTS.md](AGENTS.md). In short: fix code first; waivers are code-level,
minimal scope, with a reason comment; never weaken the checks.

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

Feature-combination coverage (TLS backends are mutually exclusive):

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
