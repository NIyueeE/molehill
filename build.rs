use anyhow::Result;

fn main() -> Result<()> {
    vergen::EmitBuilder::builder()
        .build_timestamp()
        .cargo_features()
        .cargo_target_triple()
        .git_sha(true)
        .emit()?;
    Ok(())
}
