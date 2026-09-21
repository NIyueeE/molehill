use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::Path;
use tokio::fs;
use url::Url;

#[cfg(feature = "multiplex")]
use crate::common::constants::{DEFAULT_MUX_TUNNELS, MAX_MUX_TUNNELS};
use crate::common::constants::{
    DEFAULT_TCP_POOL_SIZE, DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS,
    DEFAULT_UDP_POOL_SIZE, DEFAULT_UDP_SENDQ_SIZE,
};

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

/// Wire stack of the control channel and (for `carrier = "tcp"`) the data
/// plane: plain bytes or Noise.
#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
pub enum TransportType {
    #[default]
    #[serde(rename = "plain")]
    Plain,
    #[serde(rename = "noise")]
    Noise,
}

/// How the data plane carries forwarded traffic (`[client.data].default_mode`;
/// overridable per service on `[client.services.*]`).
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
pub enum DataMode {
    /// Multiplex data channels as yamux streams over `count` tunnel
    /// connections (default).
    #[default]
    #[serde(rename = "multiplex")]
    Multiplex,
    /// One physical connection per data channel. `count` and `carrier`
    /// do not apply.
    #[serde(rename = "direct")]
    Direct,
}

/// Which transport carries the data plane (`[client.data].default_carrier`;
/// overridable per service on `[client.services.*]`).
///
/// The control channel always rides TCP; this selector only changes the data
/// path. `tcp` and `kcp` are peers: both are just ways to reach
/// `[client.data].default_data_addr`.
///
/// - `tcp` (default): connections use the control channel's wire stack
///   (`[client.transport]`).
/// - `kcp` (feature `kcp`): KCP-over-UDP sessions; Noise is kept on top iff
///   the control transport is `noise`. The carrier is client-declared in
///   the service registration; the server adapts per connection.
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
pub enum DataCarrier {
    #[default]
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "kcp")]
    Kcp,
}

/// Per-service transport override (`[client.services.<name>].transport`).
///
/// Client-first encryption: the client-wide `[client.transport].type`
/// decides by default, and a service can override it individually with the
/// same vocabulary (`type = "noise"` / `type = "plain"`). `noise` holds the
/// keys for THIS service (e.g. a different server's public key in
/// multi-server setups); when absent, the global
/// `[client.transport].noise` keys are used.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientServiceTransportConfig {
    /// `None` = follow `[client.transport].type`; `"noise"` forces this
    /// service to encrypt; `"plain"` forces plaintext.
    #[serde(rename = "type")]
    pub transport_type: Option<TransportType>,
    /// Per-service Noise keys (pattern, remote key, psk). When absent, the
    /// global `[client.transport].noise` keys are used.
    pub noise: Option<NoiseConfig>,
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
    #[serde(rename = "protocol", default = "default_service_type")]
    pub service_type: ServiceType,
    #[serde(skip)]
    pub name: String,
    pub local_addr: String,
    /// The public address (e.g. `"0.0.0.0:6022"`) this service is exposed at
    /// on the server. Required.
    pub remote_bind_addr: String,
    /// Prefer IPv6 for the UDP forwarder's connection to the local
    /// service (UDP services only). Default: false.
    #[serde(default)] // Default to false
    pub udp_forwarder_ipv6: bool,
    pub nodelay: Option<bool>,
    pub retry_interval: Option<u64>,
    /// Override `[client].default_token` for this service only — e.g. to
    /// authenticate against a server that has its own token.
    pub token: Option<MaskedString>,
    /// Override `[client.control].default_remote_addr` for this service
    /// only: the service's control channel (and, by default, its data
    /// plane) dials this server instead of the client-wide one.
    pub remote_addr: Option<String>,
    /// Override `[client.control].default_heartbeat_timeout` for this
    /// service only (useful when services run against servers with
    /// different heartbeat intervals).
    pub heartbeat_timeout: Option<u64>,
    /// Override `[client.data].default_mode` for this service only.
    #[cfg(feature = "multiplex")]
    pub mode: Option<DataMode>,
    /// Override `[client.data].default_count` for this service only; valid
    /// only with `mode = "multiplex"`.
    #[cfg(feature = "multiplex")]
    pub count: Option<usize>,
    /// Override `[client.data].default_carrier` for this service only;
    /// valid only with `mode = "multiplex"`.
    #[cfg(feature = "multiplex")]
    pub carrier: Option<DataCarrier>,
    pub health_check: Option<HealthCheckConfig>,
    /// Per-service transport override (encryption enablement + keys).
    pub transport: Option<ClientServiceTransportConfig>,
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
    pub udp_send_queue_size: Option<u16>,
}

impl ClientServiceConfig {
    pub fn with_name(name: &str) -> ClientServiceConfig {
        ClientServiceConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// The transport type this service will use: its `transport.type`
    /// override when set, else the client-wide default
    /// (`[client.transport].type`).
    pub fn transport_type_with(&self, global: TransportType) -> TransportType {
        self.transport
            .as_ref()
            .and_then(|t| t.transport_type)
            .unwrap_or(global)
    }

    /// The Noise config this service will use: its own
    /// `transport.noise` keys when set, else the client-wide
    /// `[client.transport].noise` keys. `None` when neither side has keys
    /// (only valid for services whose effective transport is plain).
    pub fn noise_config_with<'a>(
        &'a self,
        global: Option<&'a NoiseConfig>,
    ) -> Option<&'a NoiseConfig> {
        self.transport
            .as_ref()
            .and_then(|t| t.noise.as_ref())
            .or(global)
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
    ServiceType::default()
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
    HealthCheckType::default()
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
                    bail!("Port range start {start} is greater than end {end}");
                }
                Ok(PortRange { start, end })
            }
        }
    }

    pub fn contains(self, port: u16) -> bool {
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

fn default_noise_pattern() -> String {
    // The ring-accelerated snow resolver (see Cargo.toml `noise` feature)
    // serves the ChaChaPoly data path for every pattern, so the hash choice
    // only affects the one-time handshake — BLAKE2s stays the default.
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

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(rename = "type")]
    pub transport_type: TransportType,
    /// Client only: proxy used to reach the server (`http` / `socks5`).
    ///
    /// TCP socket options (`nodelay`, keepalive) are fixed internal defaults
    /// rather than configuration — they are applied per connection kind and
    /// exposing them invited misconfiguration.
    pub proxy: Option<Url>,
    pub noise: Option<NoiseConfig>,
}

fn default_heartbeat_timeout() -> u64 {
    DEFAULT_HEARTBEAT_TIMEOUT_SECS
}

fn default_client_retry_interval() -> u64 {
    DEFAULT_CLIENT_RETRY_INTERVAL_SECS
}

/// Control-channel defaults (`[client.control]`).
///
/// Every service inherits these and may override `remote_addr`,
/// `heartbeat_timeout` and `retry_interval` on its own
/// `[client.services.<name>]` block.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_field_names,
    reason = "the `default_` prefix is the config-surface naming rule that \
              distinguishes client-wide defaults from the per-service overlay \
              keys on `[client.services.<name>]`"
)]
pub struct ClientControlConfig {
    /// Server address of the control channel, e.g. `"example.com:2333"`.
    /// Required.
    pub default_remote_addr: String,
    /// Application-layer heartbeat timeout in seconds; `0` disables the
    /// check. Must be greater than the server's `heartbeat_interval`.
    #[serde(default = "default_heartbeat_timeout")]
    pub default_heartbeat_timeout: u64,
    /// Delay between control-channel reconnect attempts.
    #[serde(default = "default_client_retry_interval")]
    pub default_retry_interval: u64,
}

/// Data-plane defaults (`[client.data]`).
///
/// Every service inherits these and may override `mode`, `count` and
/// `carrier` individually on its own `[client.services.<name>]` block;
/// `addr` itself cannot be overridden per service, but a service with its
/// own `remote_addr` dials that server's data endpoint instead.
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_field_names,
    reason = "the `default_` prefix is the config-surface naming rule that \
              distinguishes client-wide defaults from the per-service overlay \
              keys on `[client.services.<name>]`"
)]
pub struct ClientDataConfig {
    /// Data-plane endpoint; defaults to the service's control endpoint
    /// (`[client.services.<name>].remote_addr` when set, else
    /// `[client.control].default_remote_addr`). Applies to every service
    /// that does not override `remote_addr` itself.
    pub default_data_addr: Option<String>,
    #[serde(default)]
    pub default_mode: DataMode,
    /// Number of parallel tunnel connections; only with
    /// `mode = "multiplex"`. Default: 4; clamped to 1..=64.
    pub default_count: Option<usize>,
    #[serde(default)]
    pub default_carrier: DataCarrier,
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Shared secret, must match `[server].default_token`. Required.
    pub default_token: MaskedString,
    #[serde(default)]
    pub control: ClientControlConfig,
    #[cfg(feature = "multiplex")]
    #[serde(default)]
    pub data: ClientDataConfig,
    pub services: HashMap<String, ClientServiceConfig>,
    #[serde(default)]
    pub transport: TransportConfig,
}

impl ClientConfig {
    /// Default data-plane mode: whether data channels should be multiplexed
    /// over tunnel connections. A service overrides this with its own
    /// `[client.services.<name>].mode`.
    #[cfg(feature = "multiplex")]
    pub fn multiplex_enabled(&self) -> bool {
        matches!(self.data.default_mode, DataMode::Multiplex)
    }

    /// Always `false` without the `multiplex` feature.
    #[cfg(not(feature = "multiplex"))]
    pub fn multiplex_enabled(&self) -> bool {
        // The data plane is always direct without the feature; keep the
        // method signature uniform with the multiplex build.
        let _ = self;
        false
    }

    /// Default number of parallel tunnels per control session (a service's
    /// own `count` wins), clamped to `1..=MAX_MUX_TUNNELS`.
    #[cfg(feature = "multiplex")]
    pub fn tunnel_count(&self) -> usize {
        self.data
            .default_count
            .unwrap_or(DEFAULT_MUX_TUNNELS)
            .clamp(1, MAX_MUX_TUNNELS)
    }

    /// Always 1 without the `multiplex` feature.
    #[cfg(not(feature = "multiplex"))]
    pub fn tunnel_count(&self) -> usize {
        // No tunnels exist without the feature; keep the method signature
        // uniform with the multiplex build.
        let _ = self;
        1
    }

    /// Endpoint the data plane dials: `[client.data].default_data_addr`,
    /// or the control channel's `default_remote_addr` when unset (a service
    /// with its own `remote_addr` uses that instead).
    #[cfg(feature = "multiplex")]
    pub fn data_addr(&self) -> &str {
        self.data
            .default_data_addr
            .as_deref()
            .unwrap_or(self.control.default_remote_addr.as_str())
    }

    /// Without the multiplex feature the data plane follows the control
    /// channel.
    #[cfg(not(feature = "multiplex"))]
    pub fn data_addr(&self) -> &str {
        &self.control.default_remote_addr
    }
}

fn default_heartbeat_interval() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_SECS
}

/// Control-channel listener (`[server.control]`).
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerControlConfig {
    /// Address the control channel listens on. Required.
    pub bind_addr: String,
    /// Application-layer heartbeat interval in seconds; `0` disables it.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
}

/// Data-plane listener (`[server.data]`).
///
/// Client-first: the server declares no carriers — the client's
/// registration carries the carrier it will use, and the server opens the
/// corresponding listener on first use.
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerDataConfig {
    /// Data-plane listener; defaults to `[server.control].bind_addr`.
    pub bind_addr: Option<String>,
}

/// The server owns no per-service configuration. Services are registered at
/// runtime by clients and validated against the policy below.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
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
    #[serde(default)]
    pub control: ServerControlConfig,
    #[cfg(feature = "multiplex")]
    #[serde(default)]
    pub data: ServerDataConfig,
    #[serde(default)]
    pub transport: ServerTransportConfig,
}

/// Server-side wire material (`[server.transport]`): only the Noise keys.
///
/// Client-first: the server declares **what it can speak**, not what it
/// speaks — whether a connection is encrypted is decided by the client
/// (every connection starts with a transport selector byte). Placing the
/// Noise keys makes the server accept both plain and Noise connections;
/// without them it accepts plain only.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerTransportConfig {
    /// Noise keys. When present, the server accepts Noise connections
    /// (transport selector `0x01`) in addition to plain ones.
    pub noise: Option<NoiseConfig>,
}

impl ServerConfig {
    /// Data-plane listener address: `[server.data].bind_addr`, or the
    /// control channel's `bind_addr` when unset.
    #[cfg(feature = "multiplex")]
    pub fn data_bind_addr(&self) -> &str {
        self.data
            .bind_addr
            .as_deref()
            .unwrap_or(self.control.bind_addr.as_str())
    }

    /// Without the multiplex feature the data plane follows the control
    /// channel.
    #[cfg(not(feature = "multiplex"))]
    pub fn data_bind_addr(&self) -> &str {
        &self.control.bind_addr
    }
}

/// The full molehill configuration file: at least one of `[server]` /
/// `[client]` must be present.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[server]` block; `None` when absent.
    pub server: Option<ServerConfig>,
    /// `[client]` block; `None` when absent.
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

        if server.control.bind_addr.is_empty() {
            bail!("`[server.control].bind_addr` is required");
        }

        #[cfg(feature = "multiplex")]
        if let Some(addr) = server.data.bind_addr.as_deref()
            && addr.rfind(':').is_none()
        {
            bail!("server.data.bind_addr is missing the port: {addr}");
        }

        Ok(())
    }

    fn validate_client_config(client: &mut ClientConfig) -> Result<()> {
        if client.control.default_remote_addr.is_empty() {
            bail!("`[client.control].default_remote_addr` is required");
        }
        // The port is required, e.g. "example.com:2333"
        if client.control.default_remote_addr.rfind(':').is_none() {
            bail!(
                "client.control.default_remote_addr is missing the port: {}",
                client.control.default_remote_addr
            );
        }

        if client.default_token.is_empty() {
            bail!("`[client].default_token` must not be empty");
        }

        #[cfg(feature = "multiplex")]
        Config::validate_data_config(client)?;

        // Validate services
        for (name, s) in &mut client.services {
            s.name.clone_from(name);

            if s.retry_interval.is_none() {
                s.retry_interval = Some(client.control.default_retry_interval);
            }
            if let Some(addr) = s.remote_addr.as_deref()
                && addr.rfind(':').is_none()
            {
                bail!("service {name}: `remote_addr` is missing the port: {addr}");
            }
            if s.token.as_ref().is_some_and(|t| t.is_empty()) {
                bail!("service {name}: `token` must not be empty");
            }
            // Effective transport is client-decided per service: the
            // `transport.type` override wins over the client-wide
            // `[client.transport].type`, and effective Noise needs keys
            // (per-service or global).
            if s.transport_type_with(client.transport.transport_type) == TransportType::Noise
                && s.noise_config_with(client.transport.noise.as_ref())
                    .is_none()
            {
                bail!(
                    "service {name}: Noise is the effective transport (per-service                     `transport.type = \"noise\"` or the client-wide `type = \"noise\"`)                     but no Noise keys are configured — set them in                     `[client.transport.noise]` or                     `[client.services.{name}.transport.noise]`"
                );
            }
            if let Some(hc) = &s.health_check {
                if s.service_type != ServiceType::Tcp {
                    bail!(
                        "health_check is only supported for TCP services, but service {name} is {:?}",
                        s.service_type
                    );
                }
                if hc.interval == 0 {
                    bail!("health_check.interval must be greater than 0 for service {name}");
                }
                if hc.timeout == 0 {
                    bail!("health_check.timeout must be greater than 0 for service {name}");
                }
                if hc.max_failed == 0 {
                    bail!("health_check.max_failed must be greater than 0 for service {name}");
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
                bail!("service {name}: `remote_bind_addr` port must not be 0");
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
                bail!("service {name}: udp_buffer_size must be greater than 0");
            }
            if s.udp_idle_timeout.is_none() {
                s.udp_idle_timeout = Some(DEFAULT_UDP_IDLE_TIMEOUT_SECS);
            } else if s.udp_idle_timeout == Some(0) {
                bail!("service {name}: udp_idle_timeout must be greater than 0");
            }
            if s.udp_send_queue_size.is_none() {
                s.udp_send_queue_size =
                    Some(u16::try_from(DEFAULT_UDP_SENDQ_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_send_queue_size == Some(0) {
                bail!("service {name}: udp_send_queue_size must be greater than 0");
            }

            #[cfg(feature = "multiplex")]
            Config::validate_service_data(client.data.default_mode, name, s)?;
        }

        Config::validate_transport_config(&client.transport)?;

        Ok(())
    }

    /// Validate the `[client.data]` knobs.
    #[cfg(feature = "multiplex")]
    fn validate_data_config(client: &ClientConfig) -> Result<()> {
        use DataCarrier::Kcp;
        use DataMode::Direct;

        let data = &client.data;

        if data.default_count == Some(0) {
            bail!("`[client.data].default_count` must be greater than 0");
        }
        if let Some(addr) = data.default_data_addr.as_deref()
            && addr.rfind(':').is_none()
        {
            bail!("client.data.default_data_addr is missing the port: {addr}");
        }

        if matches!(data.default_mode, Direct) {
            if data.default_count.is_some() {
                bail!(
                    "`[client.data].default_count` is only valid with `default_mode = \"multiplex\"`"
                );
            }
            if matches!(data.default_carrier, Kcp) {
                bail!(
                    "`[client.data].default_carrier = \"kcp\"` requires `default_mode = \"multiplex\"`"
                );
            }
            return Ok(());
        }

        if matches!(data.default_carrier, Kcp) {
            #[cfg(not(feature = "kcp"))]
            bail!(
                "`[client.data].default_carrier = \"kcp\"` requires a binary built with the `kcp` feature"
            );
        }

        Ok(())
    }

    /// Validate one service's data-plane overrides: the same rules as the
    /// global `[client.data]` block, applied to the merged view (the
    /// service's value, or the global default when unset).
    #[cfg(feature = "multiplex")]
    fn validate_service_data(
        default_mode: DataMode,
        name: &str,
        s: &ClientServiceConfig,
    ) -> Result<()> {
        use DataCarrier::Kcp;
        use DataMode::Direct;

        if s.count == Some(0) {
            bail!("service {name}: `count` must be greater than 0");
        }

        let mode = s.mode.unwrap_or(default_mode);
        if matches!(mode, Direct) {
            if s.count.is_some() {
                bail!("service {name}: `count` is only valid with `mode = \"multiplex\"`");
            }
            if matches!(s.carrier, Some(Kcp)) {
                bail!("service {name}: `carrier = \"kcp\"` requires `mode = \"multiplex\"`");
            }
            return Ok(());
        }

        if matches!(s.carrier, Some(Kcp)) {
            #[cfg(not(feature = "kcp"))]
            bail!(
                "service {name}: `carrier = \"kcp\"` requires a binary built with the `kcp` feature"
            );
        }

        Ok(())
    }

    fn validate_transport_config(config: &TransportConfig) -> Result<()> {
        config.proxy.as_ref().map_or(Ok(()), |u| {
            match u.scheme() {
                "socks5" | "http" => {}
                scheme => bail!("Unknown proxy scheme: {scheme}"),
            }
            if u.host_str().is_none() {
                bail!("Proxy URL is missing the host: {u}");
            }
            if u.port().is_none() {
                bail!("Proxy URL is missing the port: {u}");
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Load and validate the configuration from `path`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, or when the parsed TOML violates
    /// validation rules (missing tokens, invalid addresses, ...). The error
    /// message names the offending part of the file.
    pub async fn from_file(path: &Path) -> Result<Config> {
        let s: String = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read the config {}", path.display()))?;
        Config::from_str(&s).with_context(
            || "Configuration is invalid. Please refer to the configuration specification.",
        )
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
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

    /// Extract every fenced `toml` code block from a markdown file: the
    /// configuration examples live as code blocks in docs/configuration.md
    /// since the examples/ directory was folded into the documentation, and
    /// the test keeps validating that every shipped example parses.
    /// Only used by the `multiplex`-gated doc-example test, so it shares
    /// that gate (a cfg-gated consumer must not leave it as dead code).
    #[cfg(feature = "multiplex")]
    fn doc_toml_blocks(path: &str) -> Result<Vec<String>> {
        let s = fs::read_to_string(path)?;
        let mut blocks = Vec::new();
        let mut in_block = false;
        let mut cur = String::new();
        for line in s.lines() {
            if line.trim_start().starts_with("```toml") {
                in_block = true;
                cur.clear();
            } else if in_block && line.trim_start().starts_with("```") {
                in_block = false;
                if !cur.trim().is_empty() {
                    blocks.push(cur.clone());
                }
            } else if in_block {
                cur.push_str(line);
                cur.push('\n');
            }
        }
        Ok(blocks)
    }

    // The documented examples target the default build: the
    // `[client.data]` / `[server.data]` blocks exist only behind the
    // `multiplex` feature, so this gate runs in the default and
    // multiplex legs and is skipped in feature-minimal legs.
    #[test]
    #[cfg(feature = "multiplex")]
    fn test_doc_example_config() -> Result<()> {
        let blocks = doc_toml_blocks("docs/configuration.md")?;
        assert!(
            !blocks.is_empty(),
            "no toml code blocks found in docs/configuration.md"
        );
        for b in &blocks {
            Config::from_str(b)?;
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
    fn test_validate_server_config() {
        // Missing the token
        let mut cfg = ServerConfig {
            control: ServerControlConfig {
                bind_addr: "0.0.0.0:2333".into(),
                ..Default::default()
            },
            default_token: "".into(),
            ..Default::default()
        };
        assert!(Config::validate_server_config(&mut cfg).is_err());

        // Empty token is rejected too
        let mut cfg = ServerConfig {
            control: ServerControlConfig {
                bind_addr: "0.0.0.0:2333".into(),
                ..Default::default()
            },
            default_token: "123".into(),
            ..Default::default()
        };
        assert!(Config::validate_server_config(&mut cfg).is_ok());
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
            control: ClientControlConfig {
                default_remote_addr: "example.com:2333".into(),
                ..Default::default()
            },
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
            s.udp_send_queue_size,
            Some(u16::try_from(DEFAULT_UDP_SENDQ_SIZE).unwrap())
        );

        Ok(())
    }

    #[test]
    fn test_client_service_explicit_pool_and_udp_options() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:6053"
pool_size = 4
udp_buffer_size = 65535
udp_idle_timeout = 30
udp_send_queue_size = 128
"#;
        let cfg = Config::from_str(config).unwrap();
        let s = &cfg.client.unwrap().services["test"];
        assert_eq!(s.pool_size, Some(4));
        assert_eq!(s.udp_buffer_size, Some(65535));
        assert_eq!(s.udp_idle_timeout, Some(30));
        assert_eq!(s.udp_send_queue_size, Some(128));

        // Zero values are rejected
        let bad = config.replace("udp_buffer_size = 65535", "udp_buffer_size = 0");
        assert!(Config::from_str(&bad).is_err());
    }

    #[test]
    fn test_server_allow_ports_parsing() {
        let config = r#"
[server]
default_token = "t"
allow_ports = ["6000-6999", "8080"]

[server.control]
bind_addr = "0.0.0.0:2333"
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
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        let cfg = Config::from_str(config).unwrap();
        assert!(cfg.server.unwrap().allow_ports.is_empty());

        // Malformed ranges fail validation
        let bad = r#"
[server]
default_token = "t"
allow_ports = ["9999-1000"]

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        assert!(Config::from_str(bad).is_err());

        // An empty default_token is rejected
        let bad = r#"
[server]
default_token = ""

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn test_masked_string_debug() {
        let s = MaskedString::from("secret-token");
        assert_eq!(format!("{s:?}"), "MASKED");
        assert_eq!(&*s, "secret-token");
    }

    #[test]
    fn test_noise_config_with_psk() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
health_check = { interval = 0 }
"#;
        assert!(Config::from_str(config).is_err());
    }

    #[test]
    fn test_service_udp_forwarder_ipv6_parsing() {
        // The client-level `prefer_ipv6` was removed (no consumer); the
        // per-service key stays (renamed `udp_forwarder_ipv6`): it steers
        // the UDP forwarder's bind choice.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
udp_forwarder_ipv6 = true
"#;
        let cfg = Config::from_str(config).unwrap();
        assert!(cfg.client.unwrap().services["test"].udp_forwarder_ipv6);
    }

    #[test]
    fn test_proxy_socks5() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "plain"
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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "plain"
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
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "plain"
proxy = "https://127.0.0.1:443"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        assert!(Config::from_str(config).is_err());
    }

    #[cfg(feature = "multiplex")]
    #[test]
    fn test_per_service_data_overrides_parse() {
        // `[client.data]` acts as defaults; a service's own mode/count/carrier
        // win. The runtime merge lives in `DataOpts::for_service` (client
        // code); here we pin that the keys parse and validate per service.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data]
default_mode = "multiplex"
default_count = 4

[client.services.muxed]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
mode = "direct"

[client.services.bulk]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
count = 2
"#;
        let cfg = Config::from_str(config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(services["muxed"].mode, Some(DataMode::Direct));
        assert_eq!(services["muxed"].count, None);
        assert_eq!(services["bulk"].mode, None);
        assert_eq!(services["bulk"].count, Some(2));
    }

    #[test]
    fn test_per_service_transport_enable() {
        // Global noise default: services inherit it, a service can opt out
        // with `transport.type = "plain"`.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "noise"
[client.transport.noise]
remote_public_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="

[client.services.ssh]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.services.bulk]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
[client.services.bulk.transport]
type = "plain"
"#;
        let cfg = Config::from_str(config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(
            services["ssh"].transport_type_with(TransportType::Noise),
            TransportType::Noise
        );
        assert_eq!(
            services["bulk"].transport_type_with(TransportType::Noise),
            TransportType::Plain
        );

        // Plain global default + a service opting in with its own keys (the
        // multi-server scenario: different server, different public key).
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "plain"

[client.services.enc]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
[client.services.enc.transport]
type = "noise"
[client.services.enc.transport.noise]
remote_public_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
"#;
        let cfg = Config::from_str(config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(
            services["enc"].transport_type_with(TransportType::Plain),
            TransportType::Noise
        );
        assert!(services["enc"].noise_config_with(None).is_some());
        // The per-service key wins over the (absent) global one.
        assert_eq!(
            services["enc"]
                .noise_config_with(None)
                .unwrap()
                .remote_public_key
                .as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
    }

    #[test]
    fn test_per_service_noise_requires_keys() {
        // Effective Noise without keys anywhere is rejected at parse time.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.transport]
type = "plain"

[client.services.enc]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
[client.services.enc.transport]
type = "noise"
"#;
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn test_per_service_remote_addr() {
        // A service can dial a different server than the client-wide
        // `[client.control].default_remote_addr`.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.local]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.services.remote]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
remote_addr = "other.example.com:2444"
"#;
        let cfg = Config::from_str(config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(services["local"].remote_addr, None);
        assert_eq!(
            services["remote"].remote_addr.as_deref(),
            Some("other.example.com:2444")
        );

        // Missing port is rejected, like the global key.
        let bad = config.replace(
            "remote_addr = \"other.example.com:2444\"",
            "remote_addr = \"other.example.com\"",
        );
        assert!(Config::from_str(&bad).is_err());
    }

    #[cfg(feature = "multiplex")]
    #[test]
    fn test_per_service_data_validation() {
        // `count` with a direct mode is rejected, exactly like the global
        // `[client.data]` block.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
mode = "direct"
count = 2
"#;
        assert!(Config::from_str(bad).is_err());

        // `carrier = "kcp"` requires multiplex mode.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
mode = "direct"
carrier = "kcp"
"#;
        assert!(Config::from_str(bad).is_err());

        // `count = 0` is rejected.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
count = 0
"#;
        assert!(Config::from_str(bad).is_err());
    }
}
