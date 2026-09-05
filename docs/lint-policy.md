# Lint policy

> English | [简体中文](lint-policy.zh.md)

All lints live in `Cargo.toml` `[lints]`; the table below is the single source
of truth. Changing a lint level requires updating this page (both languages)
**in the same commit** — `githooks/check-docs` enforces the mechanical part.

## Declared lints

| Lint | Level | Why |
|------|-------|-----|
| `missing_docs` | warn | the public API (lib surface, config structs, CLI) carries doc comments |
| `unsafe_code` | warn | unsafe is confined to audited modules that opt in explicitly; every `unsafe` block must carry a `SAFETY` comment |
| `linker_messages` | allow | cross's musl linker wrapper prints trace output to stderr; real linker errors still fail the build |
| `dbg_macro` | deny | no debug leftovers in production code |
| `expect_used` | deny | no hidden panics; handle errors explicitly |
| `panic` | deny | a reverse proxy must not crash on untrusted input |
| `undocumented_unsafe_blocks` | deny | every `unsafe` block carries a `SAFETY` comment |
| `unwrap_used` | deny | no hidden panics; handle errors explicitly |
| `todo` | warn | stubs get removed, not accumulated (AGENTS.md §9) |
| `pedantic` | deny | the whole pedantic clippy group (rust-agents-template parity); members that genuinely do not fit are relaxed per-item in code with a reason, never here |

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

None. The template's `clippy::pedantic` (deny) and `missing_docs` (warn) are
declared above since the lint migration. Individual pedantic members that do
not fit this codebase are relaxed per-item in code — prefer
`#[expect(..., reason = "...")]` — under the same waiver discipline as
every other lint.
