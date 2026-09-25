# Lint policy


All Rust lints live in `Cargo.toml` `[lints]`; the table below is the single
source of truth. The python bench scripts have their own lint *and* format
gate (`ruff.toml`, run as `uvx ruff check` + `uvx ruff format --check` in
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
`ruff.toml`, `githooks/pre-commit`, or any check command. All extra checks
(machete, audit, deny, outdated, docs-sync, secret scan, the python gates)
follow the same discipline.

### Unsafe

`unsafe_code` is **deny**, so an unexpected `unsafe` fails a plain
`cargo build` and not only the `-D warnings` gate. Exactly one module may ask
for it, per item, with `#[expect(unsafe_code, reason = "...")]` plus a
`// SAFETY:` comment directly above the item:

- `src/transport/udp_batch.rs` — the `recvmmsg`/`sendmmsg` batching FFI
  (8 expectations: the zeroed `msghdr`/`sockaddr_storage` templates, the
  kernel-ABI `sockaddr` reinterpretation, the two `mmsg` calls, and three
  `Send`/`Sync` proofs for the reusable descriptor arrays);
- `src/mux.rs` carries `#![forbid(unsafe_code)]` outright — `forbid` cannot be
  relaxed by an expectation, which is the point.

The FFI waivers are the one place where the "fix the code first" rule runs
into "no equally reasonable alternative", so the reasoning is recorded here:

- the raw syscalls have no `std` equivalent; `socket2` (already a dependency)
  covers everything *except* the batching calls;
- **`nix` does not remove them.** Its `MultiHeaders<S>` holds
  `Box<[libc::mmsghdr]>` (raw pointers inside) and is therefore itself
  `!Send`/`!Sync`, so the three `unsafe impl Send`/`Sync` proofs — the part
  that carries the real soundness argument — would still be required, at the
  cost of a new dependency and a per-call `Vec<IoSliceMut>` allocation in the
  hot path. Verified against `nix` 0.31.3's source, not assumed;
- **`quinn-udp` could remove all eight** (`UdpSocketState` is a plain
  `Send + Sync` struct whose `recv`/`send` take caller-owned slice buffers),
  but it also brings GSO/GRO segmentation, i.e. it changes the send path's
  syscall shape. That is a measurable change to the KCP data path, so it
  belongs in its own A/B (`just soak --test=screen`) rather than in a lint
  cleanup. Recorded as an open thread in HANDOFF.md.

### Test modules

A `#[cfg(test)] mod tests` may carry a module-level
`#![expect(clippy::unwrap_used, reason = "tests unwrap values they just
constructed")]`, extended with `expect_used`, `panic` or
`assertions_on_constants` only when that test needs it. This is the one
sanctioned module-level waiver: a test's failure path *is* a panic, so
waiving per call would bury the assertion under `.expect()` noise. Production
modules get no such exception.

### Python (ruff)

The bench/test scripts under `benches/scripts/` are held to the same rule:
fix the code first. `ruff.toml`'s `ignore` list is reserved for a property
that is true of every script in scope — today exactly one, S602/S603/S607,
because these scripts measure *external* binaries reached through `PATH`
(iperf3, tc, ss, vegeta, the peers), where resolving a different program
would measure something other than the documented tool. Every other waiver
is written inline at its own site (`# noqa: <rule>`) with a one-line reason,
the python equivalent of `#[expect(clippy::...)]` plus its reason comment.
`uvx ruff check` and `uvx ruff format --check` are the two gates; `just
py-fmt` applies the formatter's fixes.

## Deviations from the rust-agents-template lint set

None. The template's `clippy::pedantic` (deny) and `missing_docs` (warn) are
declared above since the lint migration. Individual pedantic members that do
not fit this codebase are relaxed per-item in code — prefer
`#[expect(..., reason = "...")]` — under the same waiver discipline as
every other lint.
