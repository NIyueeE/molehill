# Lint policy

> English | [简体中文](lint-policy.zh.md)

All lints live in `Cargo.toml` `[lints]`; the table below is the single source
of truth. Changing a lint level requires updating this page (both languages)
**in the same commit** — `githooks/check-docs` enforces the mechanical part.

## Declared lints

| Lint | Level | Why |
|------|-------|-----|
| `unsafe_code` | warn | unsafe is confined to audited modules that opt in explicitly; every `unsafe` block must carry a `SAFETY` comment |
| `linker_messages` | allow | cross's musl linker wrapper prints trace output to stderr; real linker errors still fail the build |
| `dbg_macro` | deny | no debug leftovers in production code |
| `expect_used` | deny | no hidden panics; handle errors explicitly |
| `panic` | deny | a reverse proxy must not crash on untrusted input |
| `undocumented_unsafe_blocks` | deny | every `unsafe` block carries a `SAFETY` comment |
| `unwrap_used` | deny | no hidden panics; handle errors explicitly |
| `todo` | warn | stubs get removed, not accumulated (AGENTS.md §9) |

## Waiver discipline

Fix the code first; a waiver is the last resort, and only code-level:

- prefer `#[expect(clippy::lint_name)]` (it starts producing a compile warning
  once the lint stops firing, preventing stale allows), fall back to
  `#[allow(clippy::lint_name)]`;
- minimal scope: a single statement or one function; never function groups,
  module-level `#![allow(...)]`, or crate-level relaxation;
- a one-line reason comment at the waiver point is mandatory (plus a linked
  issue, if any).

Only two legitimate scenarios:

1. **genuinely unavoidable** — the business need demands it and no equally
   reasonable alternative exists;
2. **upstream problems** — false positives, macro/derive-generated code, or
   audit noise from dependencies themselves.

Never "make errors disappear" by editing `Cargo.toml` `[lints]`,
`githooks/pre-commit`, or any check command. All extra checks (machete, audit,
deny, outdated, docs-sync, secret scan) follow the same discipline.

## Deviations from the rust-agents-template lint set

The template denies `clippy::pedantic` and warns on `missing_docs`. molehill
is a fork of a mature codebase (rathole) with a large public API; adopting
both is a dedicated migration, tracked in [HANDOFF.md](../HANDOFF.md). The
deny set above already covers the safety-critical lints the template targets.
