//! Build script: emits build metadata (version, features, git info) via vergen.
use anyhow::Result;
use vergen::{Build, Cargo};
use vergen_gitcl::{Emitter, Gitcl};

/// Emit `native_target` when this build's target can execute its own artifacts.
///
/// `tests/log_budget_test.rs` drives the freshly built binary as a subprocess —
/// that is what makes it measure what an operator sees — and that only works
/// when the target runs natively. Under `cross` (the release workflow tests
/// every target it publishes) a child process dies with `Exec format error`
/// while every in-process test passes, which failed a v0.9.1 release build.
/// Exposing the fact here keeps the affected targets honest: they report
/// `0 tests` rather than a log budget that silently measured nothing.
///
/// The test is the *architecture*, not the whole triple: an `x86_64` host runs
/// an `x86_64-musl` binary directly, and only a different architecture needs an
/// emulator whose child-process behaviour is the unreliable part.
fn emit_native_target_cfg() {
    println!("cargo::rustc-check-cfg=cfg(native_target)");
    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let arch = |triple: &str| triple.split('-').next().unwrap_or_default().to_string();
    if !host.is_empty() && arch(&host) == arch(&target) {
        println!("cargo::rustc-cfg=native_target");
    }
}

fn main() -> Result<()> {
    emit_native_target_cfg();
    Emitter::default()
        .add_instructions(&Build::builder().build_timestamp(true).build())?
        .add_instructions(&Cargo::builder().features(true).target_triple(true).build())?
        .add_instructions(
            &Gitcl::builder()
                // `--version` prints a `Commit SHA` line, so the SHA has to be
                // emitted; a dirty tree is marked because a binary built from
                // uncommitted code must not report a clean revision (AGENTS.md
                // §10, "prove provenance").
                .sha(true)
                .commit_date(true)
                .commit_timestamp(true)
                .describe(true, true, None)
                .build(),
        )?
        .emit()?;
    Ok(())
}
