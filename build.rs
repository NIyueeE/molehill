//! Build script: emits build metadata (version, features, git info) via vergen.
use anyhow::Result;
use vergen::{Build, Cargo};
use vergen_gitcl::{Emitter, Gitcl};

fn main() -> Result<()> {
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
