//! molehill: a secure, stable, high-performance reverse proxy for NAT
//! traversal — a Rust alternative to frp / ngrok, forked from
//! [rathole](https://github.com/rapiz1/rathole).
//!
//! The client runs next to the service behind NAT and keeps a control
//! channel to the server on a public host; visitors hit the server's public
//! endpoint and their traffic is relayed over data channels. See the README
//! and `docs/` for configuration, transports, and the protocol design.

#![cfg_attr(
    not(any(feature = "client", feature = "server")),
    allow(dead_code, unused_imports, unused_variables, unused_mut)
)]

mod cli;
mod common;
mod config;
mod core;
pub mod logging;
mod protocol;
mod transport;

pub use cli::Cli;
use cli::KeypairType;
pub use common::constants::DEFAULT_UDP_BUFFER_SIZE;
pub use config::Config;

use anyhow::{Result, anyhow};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

#[cfg(feature = "client")]
use core::run_client;

#[cfg(feature = "server")]
use core::run_server;

use crate::config::{ConfigChange, ConfigWatcherHandle};

#[cfg(feature = "noise")]
const DEFAULT_CURVE: KeypairType = KeypairType::X25519;

#[cfg(feature = "noise")]
fn get_str_from_keypair_type(curve: KeypairType) -> &'static str {
    match curve {
        KeypairType::X25519 => "25519",
        KeypairType::X448 => "448",
    }
}

#[cfg(feature = "noise")]
fn genkey(curve: Option<KeypairType>) -> Result<()> {
    use base64::Engine;
    let curve = curve.unwrap_or(DEFAULT_CURVE);
    let builder = snowstorm::Builder::new(
        format!(
            "Noise_KK_{}_ChaChaPoly_BLAKE2s",
            get_str_from_keypair_type(curve)
        )
        .parse()?,
    );
    let keypair = builder.generate_keypair()?;

    println!(
        "Private Key:\n{}\n",
        base64::engine::general_purpose::STANDARD.encode(&keypair.private)
    );
    println!(
        "Public Key:\n{}",
        base64::engine::general_purpose::STANDARD.encode(&keypair.public)
    );
    Ok(())
}

#[cfg(not(feature = "noise"))]
fn genkey(_curve: Option<KeypairType>) -> Result<()> {
    crate::common::helper::feature_not_compile("nosie")
}

/// Run molehill until shutdown.
///
/// Loads the configuration through the config watcher (hot-reload aware),
/// spawns the instance as a server or a client, and restarts the instance
/// on general configuration changes.
///
/// # Errors
///
/// Fails when no config path is given, when the config watcher cannot be
/// started, or when the previous instance errored while a general config
/// change triggers a restart.
pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    if let Some(curve) = args.genkey {
        return genkey(curve);
    }

    // Raise `nofile` limit on linux and mac
    let _ = fdlimit::raise_fd_limit();

    // Spawn a config watcher. The watcher will send a initial signal to start the instance with a config
    let config_path = args
        .config_path
        .as_ref()
        .ok_or_else(|| anyhow!("Missing config path"))?;
    info!("Using config {}", config_path.display());
    let mut cfg_watcher = ConfigWatcherHandle::new(config_path, shutdown_rx).await?;

    // shutdown_tx owns the instance
    let (shutdown_tx, _) = broadcast::channel(1);

    // (The join handle of the last instance, The service update channel sender)
    let mut last_instance: Option<(tokio::task::JoinHandle<_>, mpsc::Sender<ConfigChange>)> = None;

    while let Some(e) = cfg_watcher.event_rx.recv().await {
        match e {
            ConfigChange::General(config) => {
                if let Some((i, _)) = last_instance {
                    info!("General configuration change detected. Restarting...");
                    shutdown_tx.send(true)?;
                    i.await??;
                }

                debug!("{:?}", config);

                let (service_update_tx, service_update_rx) = mpsc::channel(1024);

                last_instance = Some((
                    tokio::spawn(run_instance(
                        *config,
                        args.clone(),
                        shutdown_tx.subscribe(),
                        service_update_rx,
                    )),
                    service_update_tx,
                ));
            }
            #[cfg(feature = "notify")]
            ev => {
                info!("Service change detected. {:?}", ev);
                if let Some((_, service_update_tx)) = &last_instance {
                    let _ = service_update_tx.send(ev).await;
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);

    Ok(())
}

async fn run_instance(
    config: Config,
    args: Cli,
    shutdown_rx: broadcast::Receiver<bool>,
    service_update: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    match determine_run_mode(&config, &args) {
        RunMode::Undetermine => Err(anyhow!("Cannot determine running as a server or a client")),
        RunMode::Client => {
            info!("Running as a client");
            #[cfg(not(feature = "client"))]
            crate::common::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(config, shutdown_rx, service_update).await
        }
        RunMode::Server => {
            info!("Running as a server");
            #[cfg(not(feature = "server"))]
            crate::common::helper::feature_not_compile("server");
            #[cfg(feature = "server")]
            run_server(config, shutdown_rx, service_update).await
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Undetermine,
}

fn determine_run_mode(config: &Config, args: &Cli) -> RunMode {
    if args.client && args.server {
        RunMode::Undetermine
    } else if args.client {
        RunMode::Client
    } else if args.server {
        RunMode::Server
    } else if config.client.is_some() && config.server.is_none() {
        RunMode::Client
    } else if config.server.is_some() && config.client.is_none() {
        RunMode::Server
    } else {
        RunMode::Undetermine
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::config::{ClientConfig, ServerConfig};

    #[test]
    fn test_determine_run_mode() {
        // (config has `[server]`, config has `[client]`, `--server`, `--client`)
        let tests: [(bool, bool, bool, bool, RunMode); 7] = [
            (false, false, false, false, RunMode::Undetermine),
            (true, false, false, false, RunMode::Server),
            (false, true, false, false, RunMode::Client),
            (true, true, false, false, RunMode::Undetermine),
            (true, true, true, false, RunMode::Server),
            (true, true, false, true, RunMode::Client),
            (true, true, true, true, RunMode::Undetermine),
        ];

        for (cfg_s, cfg_c, arg_s, arg_c, run_mode) in tests {
            let config = Config {
                server: if cfg_s {
                    Some(ServerConfig::default())
                } else {
                    None
                },
                client: if cfg_c {
                    Some(ClientConfig::default())
                } else {
                    None
                },
            };

            let args = Cli {
                config_path: Some(std::path::PathBuf::new()),
                server: arg_s,
                client: arg_c,
                ..Default::default()
            };

            assert_eq!(determine_run_mode(&config, &args), run_mode);
        }
    }
}
