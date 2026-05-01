use anyhow::Result;
use vergen::{Build, Cargo};
use vergen_gitcl::{Emitter, Gitcl};

fn main() -> Result<()> {
    Emitter::default()
        .add_instructions(&Build::builder().build_timestamp(true).build())?
        .add_instructions(&Cargo::builder().features(true).target_triple(true).build())?
        .add_instructions(
            &Gitcl::builder()
                .sha(false)
                .commit_date(true)
                .commit_timestamp(true)
                .describe(true, false, None)
                .build(),
        )?
        .emit()?;
    Ok(())
}
