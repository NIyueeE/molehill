//! The molehill binary: parses the CLI, installs logging, and runs the
//! configured instance (server, client, or `--genkey`) until shutdown.
use anyhow::Result;
use clap::Parser;
#[cfg(not(feature = "console"))]
use molehill_rathole::logging;
use molehill_rathole::{Cli, run};
use tokio::{signal, sync::broadcast};
use tracing::{debug, info};

/// How many runtime workers a process gets unless the environment says otherwise.
///
/// The data plane is a handful of hot tasks that never block: a worker per core
/// lends them nothing and costs a great deal. Tokio's workers spin briefly for
/// work before they park, so on a wide machine the idle ones burn CPU that the
/// carrier is charged for — measured on a twenty-core bench host, the same
/// workload cost `0.93 s` of CPU per Gbit at twenty workers and `0.64` at four,
/// with the delivered throughput moving by less than a tenth. Four is the
/// widest setting that still ties the TCP carrier's cost; a deployment that
/// wants more sets `TOKIO_WORKER_THREADS`, which is read here and honoured.
const DEFAULT_WORKERS: usize = 4;

/// The workers this process will start.
fn workers() -> usize {
    let cores =
        std::thread::available_parallelism().map_or(DEFAULT_WORKERS, std::num::NonZero::get);
    std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| cores.min(DEFAULT_WORKERS))
}

fn main() -> Result<()> {
    let workers = workers();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    runtime.block_on(entry())
}

async fn entry() -> Result<()> {
    let args = Cli::parse();

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            // Something really weird happened. So just panic
            eprintln!("Failed to listen for the ctrl-c signal: {e:?}");
            std::process::exit(1);
        }

        if let Err(e) = shutdown_tx.send(true) {
            // shutdown signal must be catched and handle properly
            // `rx` must not be dropped
            eprintln!("Failed to send shutdown signal: {e:?}");
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
        // Colored levels + span context; ANSI only on a TTY (respects NO_COLOR)
        logging::init("info");
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
