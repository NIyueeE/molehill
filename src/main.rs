use anyhow::Result;
use clap::Parser;
use molehill_rathole::{Cli, run};
#[cfg(not(feature = "console"))]
use std::io::IsTerminal;
use tokio::{signal, sync::broadcast};
use tracing::{debug, info};
#[cfg(not(feature = "console"))]
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            // Something really weird happened. So just panic
            eprintln!("Failed to listen for the ctrl-c signal: {:?}", e);
            std::process::exit(1);
        }

        if let Err(e) = shutdown_tx.send(true) {
            // shutdown signal must be catched and handle properly
            // `rx` must not be dropped
            eprintln!("Failed to send shutdown signal: {:?}", e);
            std::process::exit(1);
        }
    });

    #[cfg(feature = "console")]
    {
        console_subscriber::init();

        tracing::info!("console_subscriber enabled");
    }
    #[cfg(not(feature = "console"))]
    {
        let is_atty = std::io::stdout().is_terminal();

        let level = "info"; // if RUST_LOG not present, use `info` level
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
            )
            .with_timer(tracing_subscriber::fmt::time::SystemTime)
            .with_ansi(is_atty)
            .init();
    }

    info!(
        "molehill v{} ({}, {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_DESCRIBE").unwrap_or("no-git-info"),
        option_env!("VERGEN_CARGO_TARGET_TRIPLE").unwrap_or("unknown-target"),
    );
    debug!(
        "Built with features: {}",
        option_env!("VERGEN_CARGO_FEATURES").unwrap_or("none")
    );

    run(args, shutdown_rx).await
}
