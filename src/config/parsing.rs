use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::Path;
use tokio::fs;
use url::Url;

use crate::common::constants::{
    DEFAULT_TCP_POOL_SIZE, DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS,
    DEFAULT_UDP_POOL_SIZE, DEFAULT_UDP_SENDQ_SIZE,
};
use crate::transport::{DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_SECS, DEFAULT_NODELAY};

/// Application-layer heartbeat interval in secs
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 40;

/// Client
const DEFAULT_CLIENT_RETRY_INTERVAL_SECS: u64 = 1;

/// String with Debug implementation that emits "MASKED"
/// Used to mask sensitive strings when logging
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
pub struct MaskedString(String);

impl Debug for MaskedString {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.write_str("MASKED")
    }
}

impl Deref for MaskedString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&str> for MaskedString {
    fn from(s: &str) -> MaskedString {
        MaskedString(String::from(s))
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
pub enum TransportType {
    #[default]
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "tls")]
    Tls,
    #[serde(rename = "noise")]
    Noise,
    #[serde(rename = "websocket")]
    Websocket,
}

/// Per service config (client side).
///
/// The client is authoritative: each service declares the public address it
/// wants to be exposed at (`remote_bind_addr`) and the server validates the
/// request against its policy. All `Option`s are optional in configuration
/// but must be `Some` at runtime (validation fills in the defaults).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientServiceConfig {
    #[serde(rename = "type", default = "default_service_type")]
    pub service_type: ServiceType,
    #[serde(skip)]
    pub name: String,
    pub local_addr: String,
    /// The public address (e.g. `"0.0.0.0:6022"`) this service is exposed at
    /// on the server. Required.
    pub remote_bind_addr: String,
    #[serde(default)] // Default to false
    pub prefer_ipv6: bool,
    pub nodelay: Option<bool>,
    pub retry_interval: Option<u64>,
    pub health_check: Option<HealthCheckConfig>,
    /// Requested number of pre-established data channels.
    /// Defaults: 8 for TCP, 2 for UDP. The server clamps it to
    /// `[server].max_pool_size`.
    pub pool_size: Option<u16>,
    /// Receive buffer size for UDP datagrams in bytes. Default: 2048,
    /// maximum 65535 (bounded by the wire format's `u16` length).
    pub udp_buffer_size: Option<u16>,
    /// Seconds of inactivity after which a UDP peer mapping is cleaned up on
    /// the client side. Default: 60.
    pub udp_idle_timeout: Option<u64>,
    /// Queue size for outbound UDP datagrams per data channel. Default: 1024.
    pub udp_sendq_size: Option<u16>,
}

impl ClientServiceConfig {
    pub fn with_name(name: &str) -> ClientServiceConfig {
        ClientServiceConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[serde(rename = "tcp")]
    #[default]
    Tcp,
    #[serde(rename = "udp")]
    Udp,
}

fn default_service_type() -> ServiceType {
    Default::default()
}

/// How the client probes the local service of a TCP service
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum HealthCheckType {
    /// Establish a TCP connection to the service
    #[default]
    #[serde(rename = "tcp")]
    Tcp,
    /// Send an HTTP GET request and accept any 2xx/3xx response
    #[serde(rename = "http")]
    Http,
}

const DEFAULT_HEALTH_CHECK_INTERVAL_SECS: u64 = 10;
const DEFAULT_HEALTH_CHECK_TIMEOUT_SECS: u64 = 3;
const DEFAULT_HEALTH_CHECK_MAX_FAILED: u32 = 1;
const DEFAULT_HEALTH_CHECK_HTTP_PATH: &str = "/";

fn default_health_check_type() -> HealthCheckType {
    Default::default()
}

fn default_health_check_interval() -> u64 {
    DEFAULT_HEALTH_CHECK_INTERVAL_SECS
}

fn default_health_check_timeout() -> u64 {
    DEFAULT_HEALTH_CHECK_TIMEOUT_SECS
}

fn default_health_check_max_failed() -> u32 {
    DEFAULT_HEALTH_CHECK_MAX_FAILED
}

fn default_health_check_http_path() -> String {
    DEFAULT_HEALTH_CHECK_HTTP_PATH.to_string()
}

/// Health check of a client-side service (TCP services only).
///
/// The client probes `local_addr` every `interval` seconds with a timeout of
/// `timeout` seconds. After `max_failed` consecutive failed probes the service
/// is declared unhealthy and its control channel is dropped, which removes the
/// service from the server (visitors then fail fast instead of being forwarded
/// to a dead local service). Once a probe succeeds again the client
/// re-registers the service automatically.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckConfig {
    #[serde(rename = "type", default = "default_health_check_type")]
    pub check_type: HealthCheckType,
    /// Probe interval in seconds. Default: 10
    #[serde(default = "default_health_check_interval")]
    pub interval: u64,
    /// Probe timeout in seconds. Default: 3
    #[serde(default = "default_health_check_timeout")]
    pub timeout: u64,
    /// Consecutive failures before the service is declared unhealthy.
    /// Default: 1
    #[serde(default = "default_health_check_max_failed")]
    pub max_failed: u32,
    /// Path for `http` probes. Default: "/"
    #[serde(default = "default_health_check_http_path")]
    pub http_path: String,
}

/// A closed port range parsed from a config string, either `"8080"` or
/// `"6000-6999"`. Used by `[server].allow_ports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    fn parse(s: &str) -> Result<PortRange> {
        let s = s.trim();
        if s.is_empty() {
            bail!("Empty port range");
        }
        match s.split_once('-') {
            None => {
                let start = s
                    .parse::<u16>()
                    .with_context(|| format!("Invalid port: {s}"))?;
                Ok(PortRange { start, end: start })
            }
            Some((a, b)) => {
                let start = a
                    .trim()
                    .parse::<u16>()
                    .with_context(|| format!("Invalid port: {a}"))?;
                let end = b
                    .trim()
                    .parse::<u16>()
                    .with_context(|| format!("Invalid port: {b}"))?;
                if start > end {
                    bail!("Port range start {} is greater than end {}", start, end);
                }
                Ok(PortRange { start, end })
            }
        }
    }

    pub fn contains(&self, port: u16) -> bool {
        self.start <= port && port <= self.end
    }
}

impl std::fmt::Display for PortRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

impl<'de> Deserialize<'de> for PortRange {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PortRange::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for PortRange {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub hostname: Option<String>,
    pub trusted_root: Option<String>,
    pub pkcs12: Option<String>,
    pub pkcs12_password: Option<MaskedString>,
}

fn default_noise_pattern() -> String {
    String::from("Noise_NK_25519_ChaChaPoly_BLAKE2s")
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoiseConfig {
    #[serde(default = "default_noise_pattern")]
    pub pattern: String,
    pub local_private_key: Option<MaskedString>,
    pub remote_public_key: Option<String>,
    pub psk: Option<MaskedString>,
    #[serde(default)]
    pub psk_location: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebsocketConfig {
    pub tls: bool,
}

fn default_nodelay() -> bool {
    DEFAULT_NODELAY
}

fn default_keepalive_secs() -> u64 {
    DEFAULT_KEEPALIVE_SECS
}

fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TcpConfig {
    #[serde(default = "default_nodelay")]
    pub nodelay: bool,
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
    pub proxy: Option<Url>,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            nodelay: default_nodelay(),
            keepalive_secs: default_keepalive_secs(),
            keepalive_interval: default_keepalive_interval(),
            proxy: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(rename = "type")]
    pub transport_type: TransportType,
    #[serde(default)]
    pub tcp: TcpConfig,
    pub tls: Option<TlsConfig>,
    pub noise: Option<NoiseConfig>,
    pub websocket: Option<WebsocketConfig>,
}

fn default_heartbeat_timeout() -> u64 {
    DEFAULT_HEARTBEAT_TIMEOUT_SECS
}

fn default_client_retry_interval() -> u64 {
    DEFAULT_CLIENT_RETRY_INTERVAL_SECS
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub remote_addr: String,
    /// Shared secret, must match `[server].default_token`. Required.
    pub default_token: MaskedString,
    pub prefer_ipv6: Option<bool>,
    pub services: HashMap<String, ClientServiceConfig>,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    #[serde(default = "default_client_retry_interval")]
    pub retry_interval: u64,
    /// Multiplex every data channel of this client over one physical
    /// connection (yamux). Requires the `multiplex` feature. Default: true.
    #[cfg(feature = "multiplex")]
    #[serde(default = "default_mux_enabled")]
    pub mux: bool,
    /// Upper bound for the total yamux receive window across all streams of a
    /// tunnel connection, in bytes. Only meaningful with `multiplex`.
    #[cfg(feature = "multiplex")]
    pub mux_receive_window: Option<usize>,
    /// Maximum number of concurrent streams per tunnel connection. Only
    /// meaningful with `multiplex`.
    #[cfg(feature = "multiplex")]
    pub mux_max_streams: Option<usize>,
}

#[cfg(feature = "multiplex")]
fn default_mux_enabled() -> bool {
    true
}

impl ClientConfig {
    /// Whether data channels should be multiplexed over one connection.
    /// Always `false` without the `multiplex` feature.
    #[allow(unreachable_code)]
    pub fn mux_enabled(&self) -> bool {
        #[cfg(feature = "multiplex")]
        return self.mux;
        #[cfg(not(feature = "multiplex"))]
        let _ = self;
        false
    }

    /// Total yamux receive window per tunnel connection.
    #[allow(unreachable_code)]
    pub fn mux_receive_window(&self) -> Option<usize> {
        #[cfg(feature = "multiplex")]
        return self.mux_receive_window;
        #[cfg(not(feature = "multiplex"))]
        None
    }

    /// Maximum concurrent streams per tunnel connection.
    #[allow(unreachable_code)]
    pub fn mux_max_streams(&self) -> Option<usize> {
        #[cfg(feature = "multiplex")]
        return self.mux_max_streams;
        #[cfg(not(feature = "multiplex"))]
        None
    }
}

impl ServerConfig {
    /// Server-side yamux receive window for tunnel connections.
    #[allow(unreachable_code)]
    pub fn mux_receive_window(&self) -> Option<usize> {
        #[cfg(feature = "multiplex")]
        return self.mux_receive_window;
        #[cfg(not(feature = "multiplex"))]
        None
    }

    /// Server-side maximum concurrent streams per tunnel connection.
    #[allow(unreachable_code)]
    pub fn mux_max_streams(&self) -> Option<usize> {
        #[cfg(feature = "multiplex")]
        return self.mux_max_streams;
        #[cfg(not(feature = "multiplex"))]
        None
    }
}

fn default_heartbeat_interval() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_SECS
}

/// The server owns no per-service configuration. Services are registered at
/// runtime by clients and validated against the policy below.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind_addr: String,
    /// Shared secret used to authenticate control channels. Required.
    pub default_token: MaskedString,
    /// Port ranges a client may claim for its services, e.g.
    /// `["6000-6999", "8080"]`. This is the master switch for dynamic
    /// registration: when empty or missing, **all** registrations are
    /// rejected. Privileged ports (<1024) must be listed explicitly.
    #[serde(default)]
    pub allow_ports: Vec<PortRange>,
    /// Upper bound applied to every service's requested `pool_size`.
    pub max_pool_size: Option<u16>,
    /// Upper bound for the total yamux receive window the server advertises
    /// per tunnel connection, in bytes. Only meaningful with `multiplex`.
    #[cfg(feature = "multiplex")]
    pub mux_receive_window: Option<usize>,
    /// Maximum number of concurrent streams per tunnel connection. Only
    /// meaningful with `multiplex`.
    #[cfg(feature = "multiplex")]
    pub mux_max_streams: Option<usize>,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Option<ServerConfig>,
    pub client: Option<ClientConfig>,
}

impl Config {
    fn from_str(s: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(s).with_context(|| "Failed to parse the config")?;

        if let Some(server) = config.server.as_mut() {
            Config::validate_server_config(server)?;
        }

        if let Some(client) = config.client.as_mut() {
            Config::validate_client_config(client)?;
        }

        if config.server.is_none() && config.client.is_none() {
            Err(anyhow!("Neither of `[server]` or `[client]` is defined"))
        } else {
            Ok(config)
        }
    }

    fn validate_server_config(server: &mut ServerConfig) -> Result<()> {
        if server.default_token.is_empty() {
            bail!("`[server].default_token` must not be empty");
        }

        Config::validate_transport_config(&server.transport, true)?;

        Ok(())
    }

    fn validate_client_config(client: &mut ClientConfig) -> Result<()> {
        // The port is required, e.g. "example.com:2333"
        if client.remote_addr.rfind(':').is_none() {
            bail!(
                "client.remote_addr is missing the port: {}",
                client.remote_addr
            );
        }

        if client.default_token.is_empty() {
            bail!("`[client].default_token` must not be empty");
        }

        // Validate services
        for (name, s) in &mut client.services {
            s.name = name.clone();

            if s.retry_interval.is_none() {
                s.retry_interval = Some(client.retry_interval);
            }
            if let Some(hc) = &s.health_check {
                if s.service_type != ServiceType::Tcp {
                    bail!(
                        "health_check is only supported for TCP services, but service {} is {:?}",
                        name,
                        s.service_type
                    );
                }
                if hc.interval == 0 {
                    bail!(
                        "health_check.interval must be greater than 0 for service {}",
                        name
                    );
                }
                if hc.timeout == 0 {
                    bail!(
                        "health_check.timeout must be greater than 0 for service {}",
                        name
                    );
                }
                if hc.max_failed == 0 {
                    bail!(
                        "health_check.max_failed must be greater than 0 for service {}",
                        name
                    );
                }
            }

            // The public endpoint is client-declared and required.
            let bind: SocketAddr = s.remote_bind_addr.parse().with_context(|| {
                format!(
                    "service {}: invalid `remote_bind_addr`: {:?}. It must be a socket address like \"0.0.0.0:6022\"",
                    name, s.remote_bind_addr
                )
            })?;
            if bind.port() == 0 {
                bail!("service {}: `remote_bind_addr` port must not be 0", name);
            }

            // Fill in runtime defaults.
            if s.pool_size.is_none() {
                s.pool_size = Some(match s.service_type {
                    ServiceType::Tcp => DEFAULT_TCP_POOL_SIZE,
                    ServiceType::Udp => DEFAULT_UDP_POOL_SIZE,
                });
            }
            if s.udp_buffer_size.is_none() {
                s.udp_buffer_size =
                    Some(u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_buffer_size == Some(0) {
                bail!("service {}: udp_buffer_size must be greater than 0", name);
            }
            if s.udp_idle_timeout.is_none() {
                s.udp_idle_timeout = Some(DEFAULT_UDP_IDLE_TIMEOUT_SECS);
            } else if s.udp_idle_timeout == Some(0) {
                bail!("service {}: udp_idle_timeout must be greater than 0", name);
            }
            if s.udp_sendq_size.is_none() {
                s.udp_sendq_size = Some(u16::try_from(DEFAULT_UDP_SENDQ_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_sendq_size == Some(0) {
                bail!("service {}: udp_sendq_size must be greater than 0", name);
            }
        }

        Config::validate_transport_config(&client.transport, false)?;

        Ok(())
    }

    fn validate_transport_config(config: &TransportConfig, is_server: bool) -> Result<()> {
        config.tcp.proxy.as_ref().map_or(Ok(()), |u| {
            match u.scheme() {
                "socks5" | "http" => {}
                scheme => bail!("Unknown proxy scheme: {}", scheme),
            }
            if u.host_str().is_none() {
                bail!("Proxy URL is missing the host: {}", u);
            }
            if u.port().is_none() {
                bail!("Proxy URL is missing the port: {}", u);
            }
            Ok(())
        })?;
        match config.transport_type {
            TransportType::Tcp => Ok(()),
            TransportType::Tls => {
                let tls_config = config
                    .tls
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing TLS configuration"))?;
                if is_server {
                    tls_config
                        .pkcs12
                        .as_ref()
                        .and(tls_config.pkcs12_password.as_ref())
                        .ok_or_else(|| anyhow!("Missing `pkcs12` or `pkcs12_password`"))?;
                }
                Ok(())
            }
            TransportType::Noise => {
                // The check is done in transport
                Ok(())
            }
            TransportType::Websocket => Ok(()),
        }
    }

    pub async fn from_file(path: &Path) -> Result<Config> {
        let s: String = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read the config {:?}", path))?;
        Config::from_str(&s).with_context(
            || "Configuration is invalid. Please refer to the configuration specification.",
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    fn list_config_files<T: AsRef<Path>>(root: T) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                files.append(&mut list_config_files(path)?);
            }
        }
        Ok(files)
    }

    fn get_all_example_config() -> Result<Vec<PathBuf>> {
        Ok(list_config_files("./examples")?
            .into_iter()
            .filter(|x| x.ends_with(".toml"))
            .collect())
    }

    #[test]
    fn test_example_config() -> Result<()> {
        let paths = get_all_example_config()?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            Config::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            Config::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            assert!(Config::from_str(&s).is_err());
        }
        Ok(())
    }

    #[test]
    fn test_validate_server_config() -> Result<()> {
        // Missing the token
        let mut cfg = ServerConfig {
            bind_addr: "0.0.0.0:2333".into(),
            default_token: "".into(),
            ..Default::default()
        };
        assert!(Config::validate_server_config(&mut cfg).is_err());

        // Empty token is rejected too
        let mut cfg = ServerConfig {
            bind_addr: "0.0.0.0:2333".into(),
            default_token: "123".into(),
            ..Default::default()
        };
        assert!(Config::validate_server_config(&mut cfg).is_ok());

        Ok(())
    }

    #[test]
    fn test_port_range() {
        assert!(PortRange::parse("8080").is_ok_and(|r| r.contains(8080) && !r.contains(8081)));
        let r = PortRange::parse("6000 - 6999").unwrap();
        assert!(r.contains(6000) && r.contains(6999) && !r.contains(5999) && !r.contains(7000));
        assert!(PortRange::parse("").is_err());
        assert!(PortRange::parse("6999-6000").is_err());
        assert!(PortRange::parse("a-b").is_err());
        assert_eq!(PortRange { start: 80, end: 80 }.to_string(), "80");
        assert_eq!(
            PortRange {
                start: 6000,
                end: 6999
            }
            .to_string(),
            "6000-6999"
        );
    }

    #[test]
    fn test_validate_client_config() -> Result<()> {
        let mut cfg = ClientConfig {
            remote_addr: "example.com:2333".into(),
            default_token: "123".into(),
            ..Default::default()
        };

        let svc = |remote_bind_addr: &str| ClientServiceConfig {
            service_type: ServiceType::Udp,
            name: "foo1".into(),
            local_addr: "127.0.0.1:80".into(),
            remote_bind_addr: remote_bind_addr.to_string(),
            ..Default::default()
        };

        // Missing remote_bind_addr (empty string does not parse)
        cfg.services.insert("foo1".into(), svc(""));
        assert!(Config::validate_client_config(&mut cfg).is_err());

        // Invalid remote_bind_addr (missing port)
        cfg.services.insert("foo1".into(), svc("0.0.0.0"));
        assert!(Config::validate_client_config(&mut cfg).is_err());

        // Port 0 is rejected
        cfg.services.insert("foo1".into(), svc("0.0.0.0:0"));
        assert!(Config::validate_client_config(&mut cfg).is_err());

        // A valid config passes and gets its runtime defaults filled in
        cfg.services.insert("foo1".into(), svc("0.0.0.0:6081"));
        Config::validate_client_config(&mut cfg)?;
        let s = cfg.services.get("foo1").unwrap();
        assert_eq!(s.pool_size, Some(DEFAULT_UDP_POOL_SIZE));
        assert_eq!(
            s.udp_buffer_size,
            Some(u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap())
        );
        assert_eq!(s.udp_idle_timeout, Some(DEFAULT_UDP_IDLE_TIMEOUT_SECS));
        assert_eq!(
            s.udp_sendq_size,
            Some(u16::try_from(DEFAULT_UDP_SENDQ_SIZE).unwrap())
        );

        Ok(())
    }

    #[test]
    fn test_client_service_explicit_pool_and_udp_options() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
type = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:6053"
pool_size = 4
udp_buffer_size = 65535
udp_idle_timeout = 30
udp_sendq_size = 128
"#;
        let cfg = Config::from_str(config).unwrap();
        let s = &cfg.client.unwrap().services["test"];
        assert_eq!(s.pool_size, Some(4));
        assert_eq!(s.udp_buffer_size, Some(65535));
        assert_eq!(s.udp_idle_timeout, Some(30));
        assert_eq!(s.udp_sendq_size, Some(128));

        // Zero values are rejected
        let bad = config.replace("udp_buffer_size = 65535", "udp_buffer_size = 0");
        assert!(Config::from_str(&bad).is_err());
    }

    #[test]
    fn test_server_allow_ports_parsing() {
        let config = r#"
[server]
bind_addr = "0.0.0.0:2333"
default_token = "t"
allow_ports = ["6000-6999", "8080"]
"#;
        let cfg = Config::from_str(config).unwrap();
        let server = cfg.server.unwrap();
        assert_eq!(
            server.allow_ports,
            vec![
                PortRange {
                    start: 6000,
                    end: 6999
                },
                PortRange {
                    start: 8080,
                    end: 8080
                },
            ]
        );

        // allow_ports defaults to empty (= dynamic registration disabled)
        let config = r#"
[server]
bind_addr = "0.0.0.0:2333"
default_token = "t"
"#;
        let cfg = Config::from_str(config).unwrap();
        assert!(cfg.server.unwrap().allow_ports.is_empty());

        // Malformed ranges fail validation
        let bad = r#"
[server]
bind_addr = "0.0.0.0:2333"
default_token = "t"
allow_ports = ["9999-1000"]
"#;
        assert!(Config::from_str(bad).is_err());

        // An empty default_token is rejected
        let bad = r#"
[server]
bind_addr = "0.0.0.0:2333"
default_token = ""
"#;
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn test_masked_string_debug() {
        let s = MaskedString::from("secret-token");
        assert_eq!(format!("{:?}", s), "MASKED");
        assert_eq!(&*s, "secret-token");
    }

    #[test]
    fn test_noise_config_with_psk() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.transport]
type = "noise"

[client.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
psk = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
psk_location = 0
"#;
        assert!(Config::from_str(config).is_ok());
    }

    #[test]
    fn test_noise_config_with_default_psk_location() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.transport]
type = "noise"

[client.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
psk = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
"#;
        assert!(Config::from_str(config).is_ok());
    }

    #[test]
    fn test_health_check_config() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
health_check = { type = "http", interval = 5, timeout = 2, max_failed = 3, http_path = "/healthz" }
"#;
        let cfg = Config::from_str(config).unwrap();
        let hc = cfg
            .client
            .as_ref()
            .unwrap()
            .services
            .get("test")
            .unwrap()
            .health_check
            .as_ref()
            .unwrap();
        assert_eq!(hc.check_type, HealthCheckType::Http);
        assert_eq!(hc.interval, 5);
        assert_eq!(hc.timeout, 2);
        assert_eq!(hc.max_failed, 3);
        assert_eq!(hc.http_path, "/healthz");
    }

    #[test]
    fn test_health_check_defaults() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
health_check = {}
"#;
        let cfg = Config::from_str(config).unwrap();
        let hc = cfg
            .client
            .as_ref()
            .unwrap()
            .services
            .get("test")
            .unwrap()
            .health_check
            .as_ref()
            .unwrap();
        assert_eq!(hc.check_type, HealthCheckType::Tcp);
        assert_eq!(hc.interval, DEFAULT_HEALTH_CHECK_INTERVAL_SECS);
        assert_eq!(hc.timeout, DEFAULT_HEALTH_CHECK_TIMEOUT_SECS);
        assert_eq!(hc.max_failed, DEFAULT_HEALTH_CHECK_MAX_FAILED);
        assert_eq!(hc.http_path, DEFAULT_HEALTH_CHECK_HTTP_PATH);
    }

    #[test]
    fn test_health_check_rejected_on_udp_service() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
type = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:6053"
health_check = { interval = 5 }
"#;
        assert!(Config::from_str(config).is_err());
    }

    #[test]
    fn test_health_check_rejects_zero_values() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
health_check = { interval = 0 }
"#;
        assert!(Config::from_str(config).is_err());
    }

    #[test]
    fn test_prefer_ipv6_config() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"
prefer_ipv6 = true

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
prefer_ipv6 = true
"#;
        let cfg = Config::from_str(config).unwrap();
        let client = cfg.client.unwrap();
        assert!(client.prefer_ipv6.unwrap());
        assert!(client.services["test"].prefer_ipv6);
    }

    #[test]
    fn test_prefer_ipv6_defaults_false() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let cfg = Config::from_str(config).unwrap();
        let client = cfg.client.unwrap();
        assert!(!client.prefer_ipv6.unwrap_or(false));
        assert!(!client.services["test"].prefer_ipv6);
    }

    #[test]
    fn test_proxy_socks5() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.transport]
type = "tcp"

[client.transport.tcp]
proxy = "socks5://127.0.0.1:1080"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        Config::from_str(config).unwrap();
    }

    #[test]
    fn test_proxy_http() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.transport]
type = "tcp"

[client.transport.tcp]
proxy = "http://user:pass@proxy.example.com:8080"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        Config::from_str(config).unwrap();
    }

    #[test]
    fn test_proxy_invalid_scheme() {
        let config = r#"
[client]
remote_addr = "example.com:2333"
default_token = "t"

[client.transport]
type = "tcp"

[client.transport.tcp]
proxy = "https://127.0.0.1:443"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        assert!(Config::from_str(config).is_err());
    }
}
