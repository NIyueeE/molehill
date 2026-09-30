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
use crate::common::constants::{
    DEFAULT_MAX_TUNNELS, DEFAULT_POOL_IDLE_TIMEOUT_SECS, MAX_MUX_TUNNELS_CAP,
};
use crate::common::constants::{
    DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS, DEFAULT_UDP_SENDQ_SIZE,
    DEFAULT_UDP_WORKERS,
};

/// Application-layer heartbeat interval in secs
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;

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

#[cfg(feature = "multiplex")]
impl DataCarrier {
    /// The carrier's name, as the configuration writes it: the pool key and
    /// the telemetry use it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Kcp => "kcp",
        }
    }
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
    /// service (UDP services only). `None` = the key was not written.
    pub udp_forwarder_ipv6: Option<bool>,
    pub nodelay: Option<bool>,
    pub retry_interval: Option<u64>,
    /// Override `[client].default_token` for this service only — e.g. to
    /// authenticate against a server that has its own token.
    pub token: Option<MaskedString>,
    /// Override `[client.control].default_remote_addr` for this service
    /// only: the service's control channel (and, by default, its data
    /// plane) dials this server instead of the client-wide one.
    pub remote_addr: Option<String>,
    /// Override `[client.data].default_mode` for this service only.
    #[cfg(feature = "multiplex")]
    pub mode: Option<DataMode>,
    /// Override `[client.data].default_carrier` for this service only;
    /// valid only with `mode = "multiplex"`.
    #[cfg(feature = "multiplex")]
    pub carrier: Option<DataCarrier>,
    /// Per-service transport override (encryption enablement + keys).
    pub transport: Option<ClientServiceTransportConfig>,
    /// How many data channels this UDP service's worker set uses. The server
    /// shards distinct visitors across the workers (session affinity) and the
    /// tunnel pool keeps at least the tunnels they need. Default: 2. UDP
    /// services only — a TCP service opens one data channel per visitor, on
    /// demand, so the key is an error there.
    pub udp_workers: Option<u16>,
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

    /// The server this service dials: its own `remote_addr` when it declares
    /// one, else `default`. The one place the override is resolved — the
    /// control endpoint and the data endpoint both go through it, with their
    /// own defaults, so a service can never end up on two servers.
    pub fn endpoint_with<'a>(&'a self, default: &'a str) -> &'a str {
        self.remote_addr.as_deref().unwrap_or(default)
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
    /// Noise session resume: a reconnect that proves possession of the
    /// previous session's handshake hash with a MAC instead of repeating
    /// the handshake's key exchanges. Opt-in (default off) because a
    /// resumed session's keys derive without a fresh DH — see
    /// docs/transport.md, "Noise session resume".
    #[serde(default)]
    pub resume: Option<bool>,
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

fn default_client_retry_interval() -> u64 {
    DEFAULT_CLIENT_RETRY_INTERVAL_SECS
}

/// Control-channel defaults (`[client.control]`).
///
/// Every service inherits these and may override `remote_addr` and
/// `retry_interval` on its own `[client.services.<name>]` block.
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
    /// Application-layer heartbeat timeout in seconds. Unset means *derive*
    /// it from the cadence the server declares in the session ack
    /// (`max(10 s, 2 × interval + 5 s)`); `0` disables the check. A value
    /// below the derived floor is refused, because it would time out a
    /// healthy server.
    #[serde(default)]
    pub default_heartbeat_timeout: Option<u64>,
    /// Delay between control-channel reconnect attempts.
    #[serde(default = "default_client_retry_interval")]
    pub default_retry_interval: u64,
}

/// Data-plane defaults (`[client.data]`).
///
/// Every service inherits these and may override `mode` and `carrier`
/// individually on its own `[client.services.<name>]` block;
/// `addr` itself cannot be overridden per service, but a service with its
/// own `remote_addr` dials that server's data endpoint instead.
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientDataConfig {
    /// Data-plane endpoint; defaults to the service's control endpoint
    /// (`[client.services.<name>].remote_addr` when set, else
    /// `[client.control].default_remote_addr`). Applies to every service
    /// that does not override `remote_addr` itself.
    pub default_data_addr: Option<String>,
    #[serde(default)]
    pub default_mode: DataMode,
    #[serde(default)]
    pub default_carrier: DataCarrier,
    /// Serve every service of one control session from **one** shared tunnel
    /// pool per carrier, instead of one pool per service. Default: `false`
    /// (one pool per service, the classic shape). Both modes are one code
    /// path; they differ only in the pool's key.
    #[serde(default)]
    pub shared_pool: bool,
    /// Seconds a tunnel pool with no streams, no pending opens and no pinned
    /// UDP peers must stay idle before the pool removes one tunnel.
    /// Default: 60. The pool never shrinks to zero while a service is
    /// registered, and never below the UDP-derived floor.
    pub idle_timeout: Option<u64>,
    /// `[client.data.tcp]`: the TCP carrier's tunnel ceiling.
    #[serde(default)]
    pub tcp: DataCarrierLimits,
    /// `[client.data.kcp]`: the KCP carrier's tunnel ceiling.
    #[serde(default)]
    pub kcp: DataCarrierLimits,
}

/// One carrier's elastic-pool limits (`[client.data.tcp]` / `[client.data.kcp]`).
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct DataCarrierLimits {
    /// The cap the pool may grow to for this carrier. The pool starts cold
    /// and grows on demand up to it. Must be `>= 1`; values above
    /// [`MAX_MUX_TUNNELS_CAP`] are clamped.
    pub max_tunnels: Option<u16>,
}

#[cfg(feature = "multiplex")]
impl Default for DataCarrierLimits {
    fn default() -> Self {
        Self {
            max_tunnels: Some(DEFAULT_MAX_TUNNELS),
        }
    }
}

#[cfg(feature = "multiplex")]
impl DataCarrierLimits {
    /// The effective cap, clamped into `1..=MAX_MUX_TUNNELS_CAP`.
    pub fn max_tunnels(&self) -> usize {
        usize::from(self.max_tunnels.unwrap_or(DEFAULT_MAX_TUNNELS))
            .clamp(1, usize::from(MAX_MUX_TUNNELS_CAP))
    }
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

    /// Whether one control session's services share one tunnel pool per
    /// carrier (`[client.data].shared_pool`). `false` is the classic shape:
    /// one pool per service.
    #[cfg(feature = "multiplex")]
    pub fn shared_pool(&self) -> bool {
        self.data.shared_pool
    }

    /// `[client.data].idle_timeout`, the elastic pool's shrink clock.
    #[cfg(feature = "multiplex")]
    pub fn pool_idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.data
                .idle_timeout
                .unwrap_or(DEFAULT_POOL_IDLE_TIMEOUT_SECS),
        )
    }

    /// The elastic pool's ceiling for one carrier: the carrier's
    /// `max_tunnels`, clamped into `1..=MAX_MUX_TUNNELS_CAP`.
    #[cfg(feature = "multiplex")]
    pub fn max_tunnels(&self, carrier: DataCarrier) -> usize {
        match carrier {
            DataCarrier::Tcp => self.data.tcp.max_tunnels(),
            DataCarrier::Kcp => self.data.kcp.max_tunnels(),
        }
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
    /// Data channels per visitor connection.
    ///
    /// `1` (the default) is the classic shape: one data channel per
    /// visitor. A higher count spreads every visitor connection over that
    /// many parallel data channels ("stripes"), which multiplies its
    /// throughput ceiling and in-flight window; both ends must be able to
    /// speak the striped data-channel framing (a molehill new enough to
    /// know the `StartForwardStripedTcp` command on both sides). See
    /// `docs/internals.md` ("Data-channel striping").
    pub stripe_count: Option<u16>,
    /// The operator's valve on the elastic tunnel pool: how many multiplexed
    /// data tunnels **one client** may hold across every service of its
    /// session. `0` (the default) is unlimited. A tunnel over the cap is
    /// refused with a typed answer and a `debug` line naming the cap; the
    /// session itself is never touched (D14). A v3 client, whose registration
    /// carries a channel count of its own, has that count clamped to this
    /// value — one valve for both dialects.
    pub max_tunnels_per_client: Option<u16>,
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
    /// rejected. An entry admits every port it contains, privileged ports
    /// (<1024) included — binding one still needs the server's own OS
    /// privilege, so list those literally when the server has it.
    #[serde(default)]
    pub allow_ports: Vec<PortRange>,
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

    /// Effective data channels per visitor connection: the configured
    /// `[server.data].stripe_count` clamped to `1..=MAX_STRIPES` (an
    /// environment override for measurements wins when set — see
    /// `crate::stripe::stripe_count`).
    #[cfg(feature = "multiplex")]
    pub fn stripe_count(&self) -> usize {
        crate::stripe::stripe_count(self.data.stripe_count)
    }

    /// The operator's tunnel valve, `[server.data].max_tunnels_per_client`:
    /// the number of multiplexed data tunnels one client may hold. `0`
    /// (including an absent key) means unlimited. Read by the two places that
    /// accept a v4 tunnel and by the v3 registration path, which clamps the
    /// channel count its dialect still asks for.
    #[cfg(feature = "multiplex")]
    pub fn max_tunnels_per_client(&self) -> usize {
        usize::from(self.data.max_tunnels_per_client.unwrap_or(0))
    }

    /// Without the `multiplex` feature there is no `[server.data]` table and
    /// no tunnel pool to bound, so the v3 clamp this feeds is a no-op; the
    /// method keeps one signature across the two builds.
    #[cfg(not(feature = "multiplex"))]
    pub fn max_tunnels_per_client(&self) -> usize {
        let _ = self;
        0
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

/// Keys a release removed, the version that removed each one, and what to
/// write instead.
///
/// A removed key is not left to `deny_unknown_fields`: that says *what* is
/// wrong but not what to do about it, and a config whose owner believes a
/// behaviour is still configured is worse off than one that fails to start.
/// For one release the key is therefore accepted and warned about; the entry is
/// then deleted, and `deny_unknown_fields` rejects it from that release on.
/// Paths are `client.services.*.health_check`-shaped, `*` matching any table
/// key. The advice names the replacement the reader has to write, so the
/// warning is an upgrade instruction rather than a complaint.
const REMOVED_KEYS: &[(&str, &str, &str)] = &[
    (
        "client.data.default_count",
        "v0.10.0",
        "the tunnel pool now starts cold and grows on demand, so a service has no initial tunnel \
         count to write; `[client.data.tcp].max_tunnels` (or `[client.data.kcp].max_tunnels`) is \
         the cap it grows to",
    ),
    (
        "client.services.*.count",
        "v0.10.0",
        "the tunnel pool now starts cold and grows on demand, and a pool belongs to the session \
         and carrier rather than to one service; write `[client.data.tcp].max_tunnels` (or \
         `[client.data.kcp].max_tunnels`) for the cap",
    ),
    (
        "client.services.*.pool_size",
        "v0.10.0",
        "write `udp_workers` for a UDP service: it is the number of data channels the service's \
         worker set uses; a TCP service's channels are opened on demand, one per visitor",
    ),
    (
        "client.services.*.heartbeat_timeout",
        "v0.10.0",
        "the server declares its heartbeat cadence in the session ack and the client derives the \
         timeout from it; `[client.control].default_heartbeat_timeout` remains as an optional \
         floor",
    ),
    (
        "server.max_pool_size",
        "v0.10.0",
        "write `[server.data].max_tunnels_per_client`: it is the tunnel cap one client may hold \
         (0 = unlimited), where the old key bounded a v3 client's requested channel count",
    ),
    (
        "client.services.*.health_check",
        "v0.10.0",
        "a service stays registered for as long as its client runs: a request that cannot be \
         forwarded fails for that visitor, and the reason goes to the log",
    ),
];

/// Refuse a config that still carries a key from [`REMOVED_KEYS`], naming
/// every one it found and what to write instead.
///
/// It looks at the document rather than the typed config because the typed
/// config is deliberately strict — `deny_unknown_fields` would reject the file
/// with a bare "unknown field", which tells an operator *that* something is
/// wrong without telling them what to write. One start, one message: an
/// operator fixing the file has to run the binary once, not once per key.
fn reject_removed_keys(doc: &mut toml::Value) -> Result<()> {
    let mut found: Vec<String> = Vec::new();
    for (pattern, version, advice) in REMOVED_KEYS {
        let segments: Vec<&str> = pattern.split('.').collect();
        let mut hits = 0;
        count_at(doc, &segments, &mut hits);
        if hits > 0 {
            found.push(format!(
                "  `{pattern}` (removed in {version}, {hits}x): {advice}"
            ));
        }
    }
    anyhow::ensure!(
        found.is_empty(),
        "this config still carries keys this version does not know:\n{}\n\
         Remove them, then start again.",
        found.join("\n")
    );
    Ok(())
}

/// Walk `value` along `segments`, counting the leaves that are present. `*`
/// descends into every value of a table.
fn count_at(value: &toml::Value, segments: &[&str], hits: &mut usize) {
    match segments {
        [] => {}
        [last] => {
            if let Some(table) = value.as_table()
                && table.contains_key(*last)
            {
                *hits += 1;
            }
        }
        [head, rest @ ..] => {
            let Some(table) = value.as_table() else {
                return;
            };
            if *head == "*" {
                for child in table.values() {
                    count_at(child, rest, hits);
                }
            } else if let Some(child) = table.get(*head) {
                count_at(child, rest, hits);
            }
        }
    }
}

impl Config {
    fn from_str(s: &str) -> Result<Config> {
        // Parse to a document first: a removed key has to be seen (and taken
        // out) before the strict struct parse, which rejects unknown fields.
        let mut doc: toml::Value =
            toml::from_str(s).with_context(|| "Failed to parse the config")?;
        reject_removed_keys(&mut doc)?;
        let mut config: Config =
            Config::deserialize(doc).with_context(|| "Failed to parse the config")?;

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

            // UDP-only keys on a TCP service are refused, not ignored: they
            // used to be accepted and silently dropped, which let a config's
            // owner believe a buffer or a worker count was in effect when
            // nothing read it. The message names the key and the protocol.
            if matches!(s.service_type, ServiceType::Tcp) {
                for (key, written) in [
                    ("udp_workers", s.udp_workers.is_some()),
                    ("udp_buffer_size", s.udp_buffer_size.is_some()),
                    ("udp_idle_timeout", s.udp_idle_timeout.is_some()),
                    ("udp_send_queue_size", s.udp_send_queue_size.is_some()),
                    ("udp_forwarder_ipv6", s.udp_forwarder_ipv6.is_some()),
                ] {
                    if written {
                        bail!(
                            "service {name}: `{key}` is only valid for a UDP service \
                             (`protocol = \"udp\"`), but this service is TCP. Remove the key, or \
                             declare the service as UDP"
                        );
                    }
                }
            }

            // Fill in runtime defaults.
            if matches!(s.service_type, ServiceType::Udp) {
                match s.udp_workers {
                    None => s.udp_workers = Some(DEFAULT_UDP_WORKERS),
                    Some(0) => bail!("service {name}: udp_workers must be at least 1"),
                    Some(_) => {}
                }
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

        if data.idle_timeout == Some(0) {
            bail!("`[client.data].idle_timeout` must be greater than 0");
        }
        // The elastic pool's per-carrier ceiling. `0` would mean "a pool that
        // may never have a tunnel"; the value is validated rather than clamped
        // so a typo is refused with the carrier's name in it.
        for (carrier, limits) in [("tcp", &data.tcp), ("kcp", &data.kcp)] {
            if limits.max_tunnels == Some(0) {
                bail!("`[client.data.{carrier}].max_tunnels` must be at least 1");
            }
        }
        if let Some(addr) = data.default_data_addr.as_deref()
            && addr.rfind(':').is_none()
        {
            bail!("client.data.default_data_addr is missing the port: {addr}");
        }

        if matches!(data.default_mode, Direct) {
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

        let mode = s.mode.unwrap_or(default_mode);
        if matches!(mode, Direct) {
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
    /// message names the offending part of the file: the context line below
    /// points at the docs, and the cause is what the reader has to act on —
    /// which is why it is not replaced by a generic "invalid configuration".
    pub async fn from_file(path: &Path) -> Result<Config> {
        let s: String = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read the config {}", path.display()))?;
        Config::from_str(&s).map_err(|e| e.context(format!("Failed to load {}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap values they just constructed, and expect on the \
                  construction's result"
    )]
    // A fixture that names a feature the directive does not know is a typo,
    // and retiring a fixture silently is the failure mode the directive exists
    // to prevent — the test's failure path is a panic (AGENTS.md §2).
    #![expect(
        clippy::panic,
        reason = "a malformed `# requires:` directive must fail the test"
    )]
    use super::*;
    // Only the defaults-pinning test needs these (and the tunnels/streams
    // group exists only with the multiplex feature); at file scope the
    // feature-minimal build warns about unused imports.
    #[cfg(feature = "multiplex")]
    use crate::common::constants::DEFAULT_MUX_MAX_STREAMS;
    use crate::transport::{DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_SECS, DEFAULT_NODELAY};
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    /// Whether a fixture's declared feature is compiled in.
    ///
    /// A fixture exercises the schema of the build it is run in, and parts of
    /// that schema are feature-gated: `[client.data]` and `[server.data]` need
    /// `multiplex`, so a fixture carrying them is meaningless — and parses as
    /// *unknown field* — in the `--no-default-features --features server,client`
    /// leg CI runs (AGENTS.md §12). Such a fixture says so on a leading comment
    /// line, the way an invalid fixture declares `# expect:`:
    ///
    /// ```toml
    /// # requires: multiplex
    /// ```
    ///
    /// The skip is reported rather than silent, and an unknown feature name is
    /// a hard failure: a typo in the directive must not quietly retire a
    /// fixture.
    fn fixture_is_available(text: &str, name: &str) -> bool {
        let Some(required) = text
            .lines()
            .take_while(|l| l.starts_with('#'))
            .find_map(|l| l.strip_prefix("# requires: "))
        else {
            return true;
        };
        match required.trim() {
            "multiplex" => {
                let available = cfg!(feature = "multiplex");
                if !available {
                    println!("  skip {name}: needs the `multiplex` feature");
                }
                available
            }
            other => panic!("{name}: unknown feature {other:?} in `# requires:`"),
        }
    }

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
    /// configuration examples live as code blocks in the docs and the
    /// READMEs since the examples/ directory was folded into the
    /// documentation, and the test keeps validating that every shipped
    /// example parses.
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
    //
    // Every markdown file that ships a config example is checked — the
    // two READMEs and both configuration pages. Covering only the English
    // page let the Chinese mirror and the quick-start examples drift
    // silently (a renamed key would ship unparsed); the zh mirrors are
    // translated copies, so the same parse contract applies to them.
    #[test]
    #[cfg(feature = "multiplex")]
    fn test_doc_example_config() -> Result<()> {
        const DOC_FILES: [&str; 4] = [
            "docs/configuration.md",
            "docs/configuration.zh.md",
            "README.md",
            "README.zh.md",
        ];
        let mut total = 0;
        for path in DOC_FILES {
            let blocks = doc_toml_blocks(path)?;
            assert!(!blocks.is_empty(), "no toml code blocks found in {path}");
            for b in &blocks {
                Config::from_str(b).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
                total += 1;
            }
        }
        assert!(total >= DOC_FILES.len(), "expected several examples");
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        for p in paths {
            let name = p.display().to_string();
            let s = fs::read_to_string(&p)?;
            if !fixture_is_available(&s, &name) {
                continue;
            }
            Config::from_str(&s)
                .with_context(|| format!("{name} is a valid fixture but was rejected"))?;
        }
        Ok(())
    }

    /// Every fixture under `tests/config_test/invalid_config` must fail for
    /// the reason it names. A fixture that only asserts `is_err()` passes for
    /// the wrong reason the moment one of its fields becomes required — two
    /// of them did exactly that (both failed on a missing `remote_bind_addr`
    /// instead of the proxy/host-port defect they document). The fixture
    /// declares its expectation on its first line as `# expect: <substring>`,
    /// and the error has to contain it.
    /// The documented defaults are part of the user-facing contract: a change
    /// to one of these constants silently changes what every existing config
    /// means, so each is pinned here against the value the documentation
    /// states. If a default has to move, this test moves with it and the
    /// configuration pages change in the same commit (AGENTS.md §3).
    #[test]
    // A literal `assert_eq!(CONST, 30)` is folded to a constant and
    // `assertions_on_constants` rightly flags it. Pinning the *documented*
    // number is the point here, so the constant is passed through
    // `black_box`: the assertion still compares the real value at run time,
    // it is simply no longer a compile-time tautology.
    fn test_documented_defaults_are_pinned() {
        assert_eq!(
            std::hint::black_box(DEFAULT_HEARTBEAT_INTERVAL_SECS),
            30,
            "server heartbeats"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_CLIENT_RETRY_INTERVAL_SECS),
            1,
            "retry interval"
        );
        assert_eq!(std::hint::black_box(DEFAULT_UDP_WORKERS), 2, "udp_workers");
        assert_eq!(
            std::hint::black_box(DEFAULT_UDP_BUFFER_SIZE),
            2048,
            "udp_buffer_size"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_UDP_IDLE_TIMEOUT_SECS),
            60,
            "udp_idle_timeout"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_UDP_SENDQ_SIZE),
            1024,
            "udp_send_queue_size"
        );
        assert!(std::hint::black_box(DEFAULT_NODELAY), "nodelay");
        assert_eq!(
            std::hint::black_box(DEFAULT_KEEPALIVE_SECS),
            20,
            "tcp keepalive"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_KEEPALIVE_INTERVAL),
            8,
            "tcp keepalive interval"
        );
        #[cfg(feature = "multiplex")]
        {
            assert_eq!(
                std::hint::black_box(DEFAULT_MUX_MAX_STREAMS),
                64,
                "streams per tunnel"
            );
            assert_eq!(
                std::hint::black_box(DEFAULT_MAX_TUNNELS),
                4,
                "carrier max_tunnels"
            );
            assert_eq!(
                std::hint::black_box(MAX_MUX_TUNNELS_CAP),
                64,
                "max_tunnels clamp"
            );
            assert_eq!(
                std::hint::black_box(DEFAULT_POOL_IDLE_TIMEOUT_SECS),
                60,
                "pool idle_timeout"
            );
        }
    }

    /// The operator's valve defaults to "no cap": `0` and an absent key mean
    /// the same thing, and neither refuses a tunnel.
    #[cfg(feature = "multiplex")]
    #[test]
    fn test_max_tunnels_per_client_defaults_to_unlimited() {
        let config = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        let cfg = Config::from_str(config).unwrap();
        assert_eq!(cfg.server.unwrap().max_tunnels_per_client(), 0);

        let config = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"

[server.data]
max_tunnels_per_client = 6
"#;
        let cfg = Config::from_str(config).unwrap();
        assert_eq!(cfg.server.unwrap().max_tunnels_per_client(), 6);
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        assert!(!paths.is_empty(), "no invalid-config fixtures found");
        for p in paths {
            let name = p.display();
            let s = fs::read_to_string(&p)?;
            if !fixture_is_available(&s, &name.to_string()) {
                continue;
            }
            let expected = s.lines().find_map(|l| l.strip_prefix("# expect: "));
            let Err(err) = Config::from_str(&s) else {
                anyhow::bail!("{name} parsed, but it is an invalid fixture");
            };
            if let Some(needle) = expected {
                let msg = format!("{err:#}");
                assert!(
                    msg.contains(needle),
                    "{name}: expected an error containing {needle:?}, got: {msg}"
                );
            }
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
        assert_eq!(s.udp_workers, Some(DEFAULT_UDP_WORKERS));
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
    fn test_client_service_explicit_udp_options() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:6053"
udp_workers = 4
udp_buffer_size = 65535
udp_idle_timeout = 30
udp_send_queue_size = 128
"#;
        let cfg = Config::from_str(config).unwrap();
        let s = &cfg.client.unwrap().services["test"];
        assert_eq!(s.udp_workers, Some(4));
        assert_eq!(s.udp_buffer_size, Some(65535));
        assert_eq!(s.udp_idle_timeout, Some(30));
        assert_eq!(s.udp_send_queue_size, Some(128));

        // Zero values are rejected
        let bad = config.replace("udp_buffer_size = 65535", "udp_buffer_size = 0");
        assert!(Config::from_str(&bad).is_err());

        // So is a worker set with no workers: the pool's UDP floor counts the
        // channels, and a count of zero would ask for none.
        let bad = config.replace("udp_workers = 4", "udp_workers = 0");
        assert!(Config::from_str(&bad).is_err());
    }

    /// The UDP-only keys are refused on a TCP service instead of being
    /// silently ignored (the defect M6 closes); the empty-value case matters
    /// too, because `udp_forwarder_ipv6 = false` is a key a reader wrote
    /// believing it configured something.
    #[test]
    fn test_udp_only_keys_are_refused_on_a_tcp_service() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
{key}
"#;
        for key in [
            "udp_workers = 4",
            "udp_buffer_size = 2048",
            "udp_idle_timeout = 30",
            "udp_send_queue_size = 128",
            "udp_forwarder_ipv6 = false",
        ] {
            let bad = config.replace("{key}", key);
            let err = Config::from_str(&bad).unwrap_err();
            let msg = format!("{err:#}");
            let name = key.split(' ').next().unwrap();
            assert!(
                msg.contains(name) && msg.contains("UDP"),
                "{key} must be refused by name and protocol, got: {msg}"
            );
        }

        // The same keys on a UDP service stay valid.
        let ok = config
            .replace("{key}", "protocol = \"udp\"\nudp_workers = 4")
            .replace("127.0.0.1:80", "127.0.0.1:53");
        assert!(Config::from_str(&ok).is_ok());
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
    fn test_a_removed_key_is_refused_and_names_its_replacement() {
        // `health_check` was removed in a withdrawn release and reports
        // v0.10.0 now (the release that actually removes it). The config does
        // not start: refusing beats obeying it silently, and the message has
        // to name what to write instead — an error that says only "unknown
        // field" leaves the reader guessing.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
health_check = { type = "http", interval = 5, timeout = 2, max_failed = 3 }
"#;
        let err = Config::from_str(config)
            .expect_err("a removed key must not start")
            .to_string();
        assert!(
            err.contains("`client.services.*.health_check`") && err.contains("registered"),
            "the refusal must name the key and its replacement: {err}"
        );
    }

    /// Every key the v0.10.0 surface removed is refused, wherever it lived, and
    /// the refusal names each one with its replacement in a single message.
    #[test]
    fn test_every_removed_key_is_refused() {
        // Assembled by concatenation, not `format!`: the TOML carries literal
        // braces (`health_check = { ... }`) that a format string would read as
        // placeholders. `[client.data]` is included only where it exists —
        // without `multiplex` the section is an unknown field — and the removal
        // assertions below are feature-independent either way.
        let mut config = String::from(
            r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

"#,
        );
        if cfg!(feature = "multiplex") {
            config.push_str("[client.data]\ndefault_count = 4\n");
        }
        config.push_str(
            r#"
[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
count = 2
pool_size = 8
heartbeat_timeout = 90
health_check = { type = "tcp", interval = 5 }

[server]
default_token = "t"
max_pool_size = 16

[server.control]
bind_addr = "0.0.0.0:2333"
"#,
        );
        // The `[client.data]` clause only applies where the section exists;
        // without it the key is not even an unknown field.
        let err = Config::from_str(&config)
            .expect_err("a config full of removed keys must not start")
            .to_string();
        for pattern in [
            "client.data.default_count",
            "client.services.*.count",
            "client.services.*.pool_size",
            "client.services.*.heartbeat_timeout",
            "client.services.*.health_check",
            "server.max_pool_size",
        ] {
            if pattern == "client.data.default_count" && !cfg!(feature = "multiplex") {
                continue;
            }
            assert!(
                err.contains(&format!("`{pattern}`")),
                "the refusal must name `{pattern}`: {err}"
            );
        }
        assert!(
            err.contains("max_tunnels")
                && err.contains("udp_workers")
                && err.contains("max_tunnels_per_client"),
            "each removed key's replacement must be named: {err}"
        );
    }

    #[test]
    fn test_a_config_without_removed_keys_is_untouched() {
        // The strip must not report (or remove) anything from a current config
        // — otherwise every reload would log a warning that is not true.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let mut doc: toml::Value = toml::from_str(config).unwrap();
        reject_removed_keys(&mut doc).expect("a current config must pass untouched");
        assert!(Config::from_str(config).is_ok());
    }

    #[test]
    fn test_service_udp_forwarder_ipv6_parsing() {
        // The client-level `prefer_ipv6` was removed (no consumer); the
        // per-service key stays (renamed `udp_forwarder_ipv6`): it steers
        // the UDP forwarder's bind choice. It is UDP-only, so the service
        // declares its protocol.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:6080"
udp_forwarder_ipv6 = true
"#;
        let cfg = Config::from_str(config).unwrap();
        assert_eq!(
            cfg.client.unwrap().services["test"].udp_forwarder_ipv6,
            Some(true)
        );
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
        // `[client.data]` acts as defaults; a service's own mode/carrier win.
        // The runtime merge lives in `DataOpts::for_service` (client code);
        // here we pin that the keys parse and validate per service.
        // `default_carrier = "kcp"` (and `[client.data.kcp]`) need the `kcp`
        // feature; the CI legs build this test with and without it, so the
        // override is exercised with whatever carrier the build has — the
        // property under test is that the service's own value wins.
        let mut config = String::from(
            r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data]
default_mode = "multiplex"
"#,
        );
        config.push_str(if cfg!(feature = "kcp") {
            "default_carrier = \"kcp\"\n\n[client.data.kcp]\nmax_tunnels = 6\n"
        } else {
            "default_carrier = \"tcp\"\n"
        });
        config.push_str(
            r#"
[client.services.muxed]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
mode = "direct"

[client.services.bulk]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
carrier = "tcp"
"#,
        );
        let cfg = Config::from_str(&config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(services["muxed"].mode, Some(DataMode::Direct));
        assert_eq!(services["muxed"].carrier, None);
        assert_eq!(services["bulk"].mode, None);
        assert_eq!(services["bulk"].carrier, Some(DataCarrier::Tcp));
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

        // `max_tunnels = 0` is rejected, with the carrier named.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data.tcp]
max_tunnels = 0

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let err = Config::from_str(bad).unwrap_err();
        assert!(
            format!("{err:#}").contains("max_tunnels"),
            "the refusal must name the key: {err:#}"
        );
    }
}
