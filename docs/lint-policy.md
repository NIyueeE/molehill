# Lint policy


All Rust lints live in `Cargo.toml` `[lints]`; the table below is the single
source of truth. The python bench project has its own lint, format and type
gates, configured in `bench/pyproject.toml` (`[tool.ruff]`, `[tool.pyright]`)
and run through its locked environment (`just py-lint`, `just py-typecheck` in
pre-commit). Changing a lint level or a waiver requires updating this page
**in the same commit** — `githooks/check-docs` enforces the mechanical part.

## Declared lints

| Lint | Level | Why |
|------|-------|-----|
| `missing_docs` | warn | the public API (lib surface, config structs, CLI) carries doc comments |
| `unsafe_code` | deny | unsafe is confined to audited modules that opt in per item; every `unsafe` block must carry a `SAFETY` comment |
| `linker_messages` | allow | cross's musl linker wrapper prints trace output to stderr; real linker errors still fail the build |
| `dbg_macro` | deny | no debug leftovers in production code |
| `expect_used` | deny | no hidden panics; handle errors explicitly |
| `panic` | deny | a reverse proxy must not crash on untrusted input |
| `undocumented_unsafe_blocks` | deny | every `unsafe` block carries a `SAFETY` comment |
| `unwrap_used` | deny | no hidden panics; handle errors explicitly |
| `todo` | warn | stubs get removed, not accumulated (AGENTS.md §9) |
| `pedantic` | deny | the whole pedantic clippy group (rust-agents-template parity); members that genuinely do not fit are relaxed per-item in code with a reason, never here |

The lint set matches the rust-agents-template: no declared deviations.
Individual pedantic members that do not fit are relaxed per item in code —
prefer `#[expect(..., reason = "...")]` — under the same waiver discipline as
every other lint.

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
`bench/pyproject.toml`, `githooks/pre-commit`, or any check command. All extra checks
(machete, audit, deny, outdated, docs-sync, secret scan, the python gates)
follow the same discipline.

## Unsafe (the opt-in modules)

`unsafe_code` is **deny**, so an unexpected `unsafe` fails a plain
`cargo build` and not only the `-D warnings` gate. The modules that need it ask
per item, with `#[expect(unsafe_code, reason = "...")]` plus a
`// SAFETY:` comment directly above the item — **none in production** and
5 test-only:

- production: none. The TUN device layer is `tun-rs`; its platform syscalls
  live in the dependency, outside this crate's `unsafe_code` lint. That is a
  dependency-audit boundary, not a licence to ignore it: `cargo deny`, the
  advisory audit and the dependency review cover the crate, and
  `src/transparent/tun.rs` contains no `unsafe`.
- test-only: 5 expectations for edition-2024 environment mutation (3 in
  `src/transport/multiplex.rs`'s tests, 2 in `tests/pool_test.rs`). These are
  serial-suite tests; `std::env::set_var`/`remove_var` are `unsafe` since
  edition 2024 and have no safe replacement. A `temp-env` wrapper works, but
  it adds a dev-dependency and re-resolves the lockfile for five sanctioned
  test-only calls; the trade was tried and declined.
- `src/transport/tcp/mux.rs` carries `#![forbid(unsafe_code)]` outright — `forbid` cannot be
  relaxed by an expectation, which is the point.

`tun-rs` is in because the transparent data path is cross-platform now: one
device API covers Linux, macOS and the Windows server, and the three audited
Linux ioctls (attach, offload declaration, tokio fd registration) left this
crate with it. Its local A/B cost was about 0.6 % on the Linux `screen` L3 arm,
inside the 15 % materiality floor; the record is in
[HANDOFF.md](../HANDOFF.md). The batching site is also safe: `udp_batch`
uses `nix`'s safe `recvmmsg`/`sendmmsg`, at a measured 1.4-1.9 % CPU per Gbit
cost (inside the same floor). The `sysconf(_SC_CLK_TCK)` read is gone, in
favour of `nix::unistd::sysconf` and integer arithmetic.

## Test modules (the one sanctioned module-level waiver)

A `#[cfg(test)] mod tests` may carry a module-level
`#![expect(clippy::unwrap_used, reason = "tests unwrap values they just
constructed")]`, extended with `expect_used`, `panic` or
`assertions_on_constants` only when that test needs it. A test's failure path
*is* a panic, so waiving per call would bury the assertion under `.expect()`
noise.

A shared test-support module may carry a module-level `#![allow(dead_code)]`
(today `tests/common/mod.rs`): each integration binary compiles the module but
calls a subset of the helpers, so `expect` would be unfulfilled in a binary
that uses every helper. This is the second and last module-level exception;
production modules get neither.

## Python (ruff)

The bench project under `bench/src/` is held to the same rule: fix the code
first. The `ignore` list in `bench/pyproject.toml` is reserved for a property
that is true of every tool in scope — today S603, because these tools measure
*external* binaries (iperf3, tc, ss, the peers), where resolving a different
program would measure something other than the documented tool. Every
executable is now resolved to an absolute path
(`shutil.which`/`sys.executable`) before it is launched, so S607 is not
waived. Every other waiver is written inline at its own site (`# noqa: <rule>`) with a one-line reason, the python
equivalent of `#[expect(clippy::...)]` plus its reason comment.
`just py-lint` runs `ruff check` and `ruff format --check`; `just py-fmt`
applies the formatter's fixes.

The selected set is deliberately wider than "the code already passes":
`B` (bugbear, e.g. `zip(strict=)`), `E7` (ambiguous `l`/`O`/`I` names, which
the base rules cannot see), `ARG` (an unused argument is a dead parameter or
a stale signature), `PTH` (these tools build and read paths constantly),
`E501` (the formatter wraps code but not a docstring or a URL, and one had
drifted to 94 characters), `DTZ` (a naive datetime in a run record cannot be
compared to another run's), `TID` (package-internal imports are absolute: the
probes and the `l3` helpers are still executed as scripts and must not depend
on a relative package context), `C901` (cyclomatic complexity, default limit
10 — functions over it were split rather than waived), and the already-clean
`RET`/`C4`/`N`/`FA`/`FLY`/`PERF`/`PIE`/`PGH`/`LOG` families as guards for
future edits. `TRY`/`EM`, `ANN`, `T20` and `COM812` are deliberately **not**
selected, with the reasoning recorded in `bench/pyproject.toml` itself: the
first two fight the house style of actionable error messages and
annotated-where-it-matters signatures, every `print` in these tools is
deliberate output, and `COM812` (trailing commas) is [the one rule ruff's own
formatter warns against][com812] — it rewrites exactly the files `ruff format`
owns, so the two gates would fight over the tree.

[com812]: https://docs.astral.sh/ruff/formatter/#conflicting-lint-rules

## Python (pyright)

`bench/pyproject.toml` sets `typeCheckingMode = "standard"` with `include =
["src"]`, and the project must stay at **0 errors**. Standard (not strict) is
deliberate: strict over the whole model was measured at 4,811 errors, almost
all `reportUnknown*`, and that is an annotation project, not a lint fix.

The **new adapter layer** opts into strict per file with `# pyright: strict`:
`targets/base.py`, `targets/__init__.py`, `targets/molehill.py`,
`targets/control.py`, `targets/peers/base.py` and `paths.py`. New code there
is written to the strict rules; the older model modules keep their inferred
boundaries.

A type error is fixed, not silenced. Where a dynamic boundary genuinely cannot
be typed (a parsed JSON document, a subprocess pipe), narrow it at the boundary
with a local annotation or a runtime check. A `# pyright: ignore[rule]` is the
last resort, at the narrowest line, with a one-line reason, and it is recorded
in this section; `reportUnnecessaryTypeIgnoreComment = true` fails a stale one.
Never disable a file or the whole project to make a gate green.
