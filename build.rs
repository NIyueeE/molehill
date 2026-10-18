//! Build script: emits build metadata (version, features, git info) via vergen,
//! plus the platform cfgs the source tree gates on.
use anyhow::Result;
use vergen::{Build, Cargo};
use vergen_gitcl::{Emitter, Gitcl};

/// The architecture prefix of a target triple (`x86_64-...` → `x86_64`).
fn arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or_default()
}

/// The OS family of a target triple.
///
/// `linux` covers gnu and musl on purpose: an `x86_64-unknown-linux-musl`
/// binary runs on an `x86_64-unknown-linux-gnu` host, while a same-architecture
/// Windows or macOS target does not run there at all.
fn os_family(triple: &str) -> &'static str {
    if triple.contains("-linux-") {
        "linux"
    } else if triple.ends_with("-apple-darwin") {
        "darwin"
    } else if triple.contains("-windows-") {
        "windows"
    } else {
        ""
    }
}

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
/// The test is the *architecture and OS family*, not the whole triple: an
/// `x86_64` Linux host runs an `x86_64` musl binary directly, but not an
/// `x86_64` Windows or macOS one — matching only the architecture marked
/// cross-OS binaries as executable.
fn emit_native_target_cfg() {
    println!("cargo::rustc-check-cfg=cfg(native_target)");
    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let host_os = os_family(&host);
    let target_os = os_family(&target);
    if !host.is_empty()
        && !target.is_empty()
        && !host_os.is_empty()
        && arch(&host) == arch(&target)
        && host_os == target_os
    {
        println!("cargo::rustc-cfg=native_target");
    }
}

/// Whether Cargo enabled `feature` for this build.
fn cargo_feature(feature: &str) -> bool {
    let key = format!("CARGO_FEATURE_{}", feature.to_ascii_uppercase());
    std::env::var_os(key).is_some()
}

/// Emit the platform cfgs the L3 source gates on.
///
/// The mapping lives here once, instead of being repeated as
/// `all(feature = …, any(target_os = …))` at every call site:
///
/// - `mh_linux` / `mh_macos` / `mh_windows`: the target OS;
/// - `mh_l3`: the transparent module can exist (feature, its carrier
///   choice, and a supported OS);
/// - `mh_l3_client` / `mh_l3_server`: the L3 data path is compiled for that
///   end (its run-mode feature is on, and the OS supports that end).
///
/// `build.rs` sees the feature set in `CARGO_FEATURE_*` and the target in
/// `TARGET` (not `HOST`: a cross build must emit the target's cfgs).
fn emit_platform_cfgs() {
    for cfg in [
        "mh_linux",
        "mh_macos",
        "mh_windows",
        "mh_l3",
        "mh_l3_client",
        "mh_l3_server",
    ] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    let linux = target.contains("-linux-");
    let macos = target.ends_with("-apple-darwin");
    let windows = target.contains("-windows-");

    let transparent = cargo_feature("transparent");
    let carrier = cargo_feature("carrier");
    let client = cargo_feature("client");
    let server = cargo_feature("server");

    let l3 = transparent && carrier && (linux || macos || windows);
    let l3_client = l3 && client && (linux || macos);
    let l3_server = l3 && server && (linux || macos || windows);

    if linux {
        println!("cargo::rustc-cfg=mh_linux");
    }
    if macos {
        println!("cargo::rustc-cfg=mh_macos");
    }
    if windows {
        println!("cargo::rustc-cfg=mh_windows");
    }
    if l3 {
        println!("cargo::rustc-cfg=mh_l3");
    }
    if l3_client {
        println!("cargo::rustc-cfg=mh_l3_client");
    }
    if l3_server {
        println!("cargo::rustc-cfg=mh_l3_server");
    }
}

fn main() -> Result<()> {
    emit_platform_cfgs();
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
