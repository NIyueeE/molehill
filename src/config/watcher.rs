use crate::Config;
#[cfg(feature = "notify")]
use crate::config::{ClientConfig, ClientServiceConfig};
#[cfg(feature = "notify")]
use anyhow::Context;
use anyhow::{Result, anyhow};
#[cfg(feature = "notify")]
use std::collections::HashMap;
#[cfg(feature = "notify")]
use std::env;
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc};
#[cfg(feature = "notify")]
use tracing::{error, info, instrument};

#[cfg(feature = "notify")]
use notify::{EventKind, RecursiveMode, Watcher};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ConfigChange {
    General(Box<Config>), // Trigger a full restart
    #[cfg(feature = "notify")]
    ClientChange(ClientServiceChange),
}

#[cfg(feature = "notify")]
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ClientServiceChange {
    Add(ClientServiceConfig),
    Delete(String),
}

#[cfg(feature = "notify")]
trait InstanceConfig: Clone {
    type ServiceConfig: PartialEq + Eq + Clone;
    fn equal_without_service(&self, rhs: &Self) -> bool;
    fn service_delete_change(s: String) -> ConfigChange;
    fn service_add_change(cfg: Self::ServiceConfig) -> ConfigChange;
    fn get_services(&self) -> &HashMap<String, Self::ServiceConfig>;
}

#[cfg(feature = "notify")]
impl InstanceConfig for ClientConfig {
    type ServiceConfig = ClientServiceConfig;
    fn equal_without_service(&self, rhs: &Self) -> bool {
        let left = ClientConfig {
            services: Default::default(),
            ..self.clone()
        };

        let right = ClientConfig {
            services: Default::default(),
            ..rhs.clone()
        };

        left == right
    }
    fn service_delete_change(s: String) -> ConfigChange {
        ConfigChange::ClientChange(ClientServiceChange::Delete(s))
    }
    fn service_add_change(cfg: Self::ServiceConfig) -> ConfigChange {
        ConfigChange::ClientChange(ClientServiceChange::Add(cfg))
    }
    fn get_services(&self) -> &HashMap<String, Self::ServiceConfig> {
        &self.services
    }
}

pub struct ConfigWatcherHandle {
    pub event_rx: mpsc::UnboundedReceiver<ConfigChange>,
}

impl ConfigWatcherHandle {
    pub async fn new(path: &Path, shutdown_rx: broadcast::Receiver<bool>) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let origin_cfg = Config::from_file(path).await?;

        // Initial start
        event_tx
            .send(ConfigChange::General(Box::new(origin_cfg.clone())))
            .map_err(|e| anyhow!("Failed to send the initial config event: {}", e))?;

        tokio::spawn(config_watcher(
            path.to_owned(),
            shutdown_rx,
            event_tx,
            origin_cfg,
        ));

        Ok(ConfigWatcherHandle { event_rx })
    }
}

// Fake config watcher when compiling without `notify`
#[cfg(not(feature = "notify"))]
async fn config_watcher(
    _path: PathBuf,
    mut shutdown_rx: broadcast::Receiver<bool>,
    _event_tx: mpsc::UnboundedSender<ConfigChange>,
    _old: Config,
) -> Result<()> {
    // Do nothing except waiting for ctrl-c
    let _ = shutdown_rx.recv().await;
    Ok(())
}

#[cfg(feature = "notify")]
#[instrument(skip(shutdown_rx, event_tx, old))]
async fn config_watcher(
    path: PathBuf,
    mut shutdown_rx: broadcast::Receiver<bool>,
    event_tx: mpsc::UnboundedSender<ConfigChange>,
    mut old: Config,
) -> Result<()> {
    let (fevent_tx, mut fevent_rx) = mpsc::unbounded_channel();
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()?.join(path)
    };
    let parent_path = path
        .parent()
        .ok_or_else(|| anyhow!("Config file has no parent directory: {}", path.display()))?;
    let path_clone = path.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, _>| match res {
            Ok(e) => {
                if matches!(e.kind, EventKind::Modify(_))
                    && e.paths
                        .iter()
                        .map(|x| x.file_name())
                        .any(|x| x == path_clone.file_name())
                {
                    let _ = fevent_tx.send(true);
                }
            }
            Err(e) => error!("watch error: {:#}", e),
        })?;

    watcher.watch(parent_path, RecursiveMode::NonRecursive)?;
    info!("Start watching the config");

    loop {
        tokio::select! {
          e = fevent_rx.recv() => {
            match e {
              Some(_) => {
                    info!("Rescan the configuration");
                    let new = match Config::from_file(&path).await.with_context(|| "The changed configuration is invalid. Ignored") {
                      Ok(v) => v,
                      Err(e) => {
                        error!("{:#}", e);
                        // If the config is invalid, just ignore it
                        continue;
                      }
                    };

                    let events = calculate_events(&old, &new).into_iter().flatten();
                    for event in events {
                        event_tx.send(event)?;
                    }

                    old = new;
              },
              None => break
            }
          },
          _ = shutdown_rx.recv() => break
        }
    }

    info!("Config watcher exiting");

    Ok(())
}

#[cfg(feature = "notify")]
fn calculate_events(old: &Config, new: &Config) -> Option<Vec<ConfigChange>> {
    if old == new {
        return None;
    }

    if (old.server.is_some() != new.server.is_some())
        || (old.client.is_some() != new.client.is_some())
    {
        return Some(vec![ConfigChange::General(Box::new(new.clone()))]);
    }

    let mut ret = vec![];

    if old.client != new.client {
        match old.client.as_ref().zip(new.client.as_ref()) {
            Some((old_c, new_c)) => match calculate_instance_config_events(old_c, new_c) {
                Some(mut v) => ret.append(&mut v),
                None => return Some(vec![ConfigChange::General(Box::new(new.clone()))]),
            },
            None => return Some(vec![ConfigChange::General(Box::new(new.clone()))]),
        }
    }

    Some(ret)
}

#[cfg(feature = "notify")]
// None indicates a General change needed
fn calculate_instance_config_events<T: InstanceConfig>(
    old: &T,
    new: &T,
) -> Option<Vec<ConfigChange>> {
    if !old.equal_without_service(new) {
        return None;
    }

    let old = old.get_services();
    let new = new.get_services();

    let deletions = old
        .keys()
        .filter(|&name| new.get(name).is_none())
        .map(|x| T::service_delete_change(x.to_owned()));

    let addition = new
        .iter()
        .filter(|(name, c)| old.get(*name) != Some(*c))
        .map(|(_, c)| T::service_add_change(c.clone()));

    Some(deletions.chain(addition).collect())
}

#[cfg(all(test, feature = "notify"))]
mod test {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_calculate_events() {
        // Server-block-only changes always trigger a full restart (the server
        // has no per-service config anymore); client service changes are
        // forwarded as incremental events.
        let old = Config {
            server: Some(Default::default()),
            client: None,
        };
        let new = Config {
            server: Some(Default::default()),
            client: Some(Default::default()),
        };
        assert_eq!(
            calculate_events(&old, &new),
            Some(vec![ConfigChange::General(Box::new(new.clone()))])
        );

        let server_a = Config {
            server: Some(Default::default()),
            client: None,
        };
        assert_eq!(calculate_events(&server_a, &server_a), None);
    }
}
