//! molehill: a secure, stable, high-performance reverse proxy for NAT
//! traversal — a Rust alternative to frp / ngrok. It began as a fork of
//! [rathole](https://github.com/rapiz1/rathole) and is developed
//! independently since.
//!
//! The client runs next to the service behind NAT and keeps a control
//! channel to the server on a public host; visitors hit the server's public
//! endpoint and their traffic is relayed over data channels. See the README
//! and `docs/` for configuration, transports, and the protocol design.

mod cli;
mod common;
mod config;
mod core;
#[cfg(feature = "kcp")]
mod kcp;
pub mod logging;
#[cfg(feature = "multiplex")]
mod mux;
mod protocol;
mod stripe;
#[cfg(all(feature = "transparent", target_os = "linux"))]
mod transparent;
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

/// How many v4 control sessions this process has accepted and authenticated so
/// far (see the integration suite's session-count assertions).
#[cfg(feature = "server")]
pub use core::control_sessions_accepted;

/// The dialect this build speaks, re-exported for the integration suite: the
/// session tests hand-write hellos, so they must name the current version
/// rather than a copy of it that goes stale at the next bump.
#[doc(hidden)]
pub use protocol::CURRENT_PROTO_VERSION;

/// The client's pinned tunnel pool, re-exported for the integration suite:
/// the pool's own state is what the pool tests assert on, instead of parsing
/// the telemetry text (see `tests/integration_test.rs`).
#[cfg(feature = "multiplex")]
#[doc(hidden)]
pub use transport::multiplex::live_pools;
#[cfg(feature = "multiplex")]
#[doc(hidden)]
pub use transport::{Carrier, PoolSnapshot, TunnelPool};

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
    let builder = snow::Builder::new(
        format!(
            "Noise_KK_{}_ChaChaPoly_BLAKE2s",
            get_str_from_keypair_type(curve)
        )
        .parse()?,
    );
    // snow's own message for a curve its resolver cannot do is
    // "initialization error: GetDhImpl", which names neither the curve nor the
    // reason, while the CLI offers the value (--help lists it). Say what is
    // wrong: this build resolves Diffie-Hellman through ring, which provides
    // x25519 alone.
    let keypair = builder.generate_keypair().map_err(|e| {
        anyhow!(
            "cannot generate a {curve:?} keypair: {e}. This build's Noise resolver (ring) \
             provides x25519 only — use `--genkey x25519`."
        )
    })?;

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
    crate::common::helper::feature_not_compile("noise")
}

/// How a running instance ended: `None` when it stopped because the watcher
/// asked it to (a restart is coming), `Some(error)` when it failed.
type InstanceEnd = Option<anyhow::Error>;

/// Turn a running instance's end into this process's result.
///
/// An error is fatal here: with no instance this process has nothing listening,
/// and this is the only place anything looks. It used to be a `JoinHandle`
/// awaited only when the *next* general configuration change arrived, so a
/// failure during startup left the process alive, silent and serving nothing —
/// measured with the control port already bound: four lines of `INFO`
/// ("Running as a server" among them), no error, and no listener.
fn instance_ended(end: Option<InstanceEnd>) -> Result<()> {
    match end {
        Some(None) => Ok(()),
        Some(Some(e)) => Err(anyhow!("the instance stopped: {e:#}")),
        // The reporter owns the sender, so a closed channel cannot happen while
        // an instance runs: report it as the bug it is rather than as a stop.
        None => Err(anyhow!("the instance stopped without reporting why")),
    }
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
/// started, or when an instance stops with an error — a startup failure or a
/// failure that arrives with a restart, both of which are this process's
/// failure, because it has nothing serving afterwards.
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

    // Every instance reports its own end here; the service update channel
    // sender of the running one travels beside it.
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<InstanceEnd>();
    // The service-update sender of the running instance. Only the `notify`
    // feature sends on it, but both builds have to keep it *alive*: the
    // instance selects on the receiving end, and a closed channel would wake it
    // with `None` on every iteration — a spin, not an error.
    let mut service_update: Option<mpsc::Sender<ConfigChange>> = None;
    let mut running = false;

    loop {
        // While an instance runs, its end is an event like any other: this
        // process has to notice that it has nothing serving, whether or not a
        // configuration change ever arrives.
        let event = if running {
            tokio::select! {
                event = cfg_watcher.event_rx.recv() => event,
                end = end_rx.recv() => return instance_ended(end),
            }
        } else {
            cfg_watcher.event_rx.recv().await
        };
        let Some(event) = event else {
            break;
        };

        match event {
            ConfigChange::General(config) => {
                if running {
                    info!("General configuration change detected. Restarting...");
                    shutdown_tx.send(true)?;
                    // The end of the instance just asked to stop orders the
                    // restart, and a failure that arrived with the signal is
                    // still a failure.
                    instance_ended(end_rx.recv().await)?;
                }

                debug!("{:?}", config);

                let (service_update_tx, service_update_rx) = mpsc::channel(1024);
                let end = end_tx.clone();
                let instance_args = args.clone();
                let instance_shutdown = shutdown_tx.subscribe();
                tokio::spawn(async move {
                    let outcome =
                        run_instance(*config, instance_args, instance_shutdown, service_update_rx)
                            .await;
                    // Reported, never swallowed: this is the only place the
                    // process learns that its instance is gone.
                    let _ = end.send(outcome.err());
                });
                service_update = Some(service_update_tx);
                running = true;
            }
            #[cfg(feature = "notify")]
            ev => {
                info!("Service change detected. {:?}", ev);
                if let Some(service_update_tx) = &service_update {
                    let _ = service_update_tx.send(ev).await;
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    // Released with the loop: nothing is left to forward a service update to,
    // and (without the `notify` feature) this is what reads the sender at all.
    drop(service_update);

    Ok(())
}

async fn run_instance(
    config: Config,
    args: Cli,
    shutdown_rx: broadcast::Receiver<bool>,
    service_update: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    match determine_run_mode(&config, &args) {
        RunMode::Undetermine => Err(anyhow!("{}", undetermined_reason(&config))),
        RunMode::Client => {
            info!("Running as a client");
            #[cfg(not(feature = "client"))]
            crate::common::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(config, shutdown_rx, service_update).await
        }
        RunMode::Transparent => {
            info!("Running as a transparent (L3) client");
            #[cfg(not(feature = "client"))]
            crate::common::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(config.into_l3_client()?, shutdown_rx, service_update).await
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
    /// The L3 client: a `[transparent]` block, run through the same client
    /// engine on the config it lowers to.
    Transparent,
    Undetermine,
}

fn determine_run_mode(config: &Config, args: &Cli) -> RunMode {
    // A flag wins over the config, and two flags are never a mode: the same
    // rule as before, now with a third one.
    let flags = [args.server, args.client, args.transparent];
    if flags.iter().filter(|asked| **asked).count() > 1 {
        return RunMode::Undetermine;
    }
    if args.server {
        return RunMode::Server;
    }
    if args.client {
        return RunMode::Client;
    }
    if args.transparent {
        return RunMode::Transparent;
    }

    // Otherwise the file says which mode it is, and it says exactly one: the
    // three blocks are three roles, and a host with two of them is two
    // processes.
    match (
        config.server.is_some(),
        config.client.is_some(),
        config.transparent.is_some(),
    ) {
        (true, false, false) => RunMode::Server,
        (false, true, false) => RunMode::Client,
        (false, false, true) => RunMode::Transparent,
        _ => RunMode::Undetermine,
    }
}

/// Why the mode could not be determined, naming what the file carries.
///
/// "Cannot determine" without the reason is a puzzle; the file's own blocks
/// are the answer to it.
fn undetermined_reason(config: &Config) -> String {
    let mut blocks = Vec::new();
    if config.server.is_some() {
        blocks.push("`[server]`");
    }
    if config.client.is_some() {
        blocks.push("`[client]`");
    }
    if config.transparent.is_some() {
        blocks.push("`[transparent]`");
    }
    if blocks.is_empty() {
        return "The configuration defines none of `[server]`, `[client]` or `[transparent]`"
            .to_string();
    }
    format!(
        "The configuration defines {}, and they are separate run modes: exactly one may be \
         present. A host that is both, or a client that both forwards and claims, runs a \
         second process",
        blocks.join(" and ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::transparent::TransparentClientConfig;
    use crate::config::{ClientConfig, ServerConfig};

    #[test]
    fn test_determine_run_mode() {
        // (config has `[server]`, `[client]`, `[transparent]`;
        //  `--server`, `--client`, `--transparent`)
        let tests: [(bool, bool, bool, bool, bool, bool, RunMode); 13] = [
            (
                false,
                false,
                false,
                false,
                false,
                false,
                RunMode::Undetermine,
            ),
            (true, false, false, false, false, false, RunMode::Server),
            (false, true, false, false, false, false, RunMode::Client),
            // The L3 client is the third mode, and its block alone decides it.
            (
                false,
                false,
                true,
                false,
                false,
                false,
                RunMode::Transparent,
            ),
            (true, true, false, false, false, false, RunMode::Undetermine),
            // Two blocks are two roles: a host that is both is two processes.
            (true, false, true, false, false, false, RunMode::Undetermine),
            (false, true, true, false, false, false, RunMode::Undetermine),
            // A flag decides, and it wins over an ambiguous file.
            (true, true, false, true, false, false, RunMode::Server),
            (true, true, false, false, true, false, RunMode::Client),
            (true, true, false, false, false, true, RunMode::Transparent),
            // Two flags are never a mode.
            (true, true, true, true, true, false, RunMode::Undetermine),
            (true, true, true, true, false, true, RunMode::Undetermine),
            (true, true, true, false, true, true, RunMode::Undetermine),
        ];

        for (cfg_s, cfg_c, cfg_t, arg_s, arg_c, arg_t, run_mode) in tests {
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
                transparent: if cfg_t {
                    Some(TransparentClientConfig::default())
                } else {
                    None
                },
            };

            let args = Cli {
                config_path: Some(std::path::PathBuf::new()),
                server: arg_s,
                client: arg_c,
                transparent: arg_t,
                ..Default::default()
            };

            assert_eq!(determine_run_mode(&config, &args), run_mode);
        }
    }
}
