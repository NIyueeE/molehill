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
use crate::common::constants::{DEFAULT_MUX_TUNNELS, MAX_MUX_TUNNELS_CAP, MAX_TRANSPARENT_LANES};
use crate::common::constants::{
    DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS, DEFAULT_UDP_SENDQ_SIZE,
    DEFAULT_UDP_WORKERS,
};
use crate::config::transparent::TransparentClientConfig;

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
#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Hash, Default)]
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
    /// The local service's address, e.g. `"127.0.0.1:6022"`. Required by
    /// `tcp`/`udp`, and refused by `transparent` — there the local service
    /// binds the claimed public address itself, and nothing dials anything.
    #[serde(default)]
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
    /// Override `[client.data].default_carrier` for this service only.
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
    /// The TUN device this service attaches to, filled for a claim by
    /// [`crate::config::transparent::TransparentClientConfig::lower`]. Empty
    /// for every forwarding service: only a claim touches a device.
    #[serde(skip)]
    pub transparent_tun: String,
    /// How many carrier connections this claim holds — its **lanes** — filled
    /// by the L3 model's lowering from the carrier's budget
    /// (`[transparent.data.tcp|kcp].tunnels`, divided equally among the claims
    /// that draw on it). `0` for every forwarding service, which never reads it.
    #[serde(skip)]
    pub transparent_lanes: u16,
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

    /// The lane count of a claim's data plane, at least one: a claim always has
    /// one carrier connection, whatever the budget division rounds to.
    ///
    /// Only a claim's value is ever read; a forwarding service answers 1.
    pub fn lanes(&self) -> usize {
        usize::from(self.transparent_lanes.max(1))
    }

    /// Whether this service's data channels ride the multiplexed pool.
    ///
    /// Derived, never configured. A forwarding service multiplexes: its channels
    /// are per-visitor and unbounded, so a bounded pool of connections is what
    /// keeps the client's FD and NAT footprint flat. A transparent claim never
    /// does: its channels *are* its carrier connections, and its throughput is
    /// the sum of connections rather than of streams, so a multiplexer over them
    /// would be framing for nothing (measured: 28 % of the bulk throughput and
    /// 68 % more CPU per byte; see HANDOFF.md, "The data plane is derived").
    /// Without the `multiplex` feature no pool exists and every channel is a
    /// connection of its own — the same answer.
    pub fn uses_pool(&self) -> bool {
        cfg!(feature = "multiplex") && self.service_type != ServiceType::Transparent
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
    /// The client *owns* the public `ip:port` instead of the server binding it:
    /// the client's host carries the address on its own TUN device and its
    /// kernel answers the visitor, so the backend sees the visitor's real
    /// address and the server holds no connection state. Linux only, and it
    /// needs `CAP_NET_ADMIN` on both ends; see `docs/configuration.md`.
    #[serde(rename = "transparent")]
    Transparent,
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
/// Every service inherits these and may override `carrier` individually on its
/// own `[client.services.<name>]` block; `addr` itself cannot be overridden per
/// service, but a service with its own `remote_addr` dials that server's data
/// endpoint instead.
///
/// The *shape* of the data plane is not a key: a forwarding service always
/// multiplexes ([`ClientServiceConfig::uses_pool`]) and a transparent claim
/// never does.
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
    pub default_carrier: DataCarrier,
    /// Serve every service of one control session from **one** shared tunnel
    /// pool per carrier, instead of one pool per service. Default: `false`
    /// (one pool per service, the classic shape). Both shapes are one code
    /// path; they differ only in the pool's key.
    #[serde(default)]
    pub shared_pool: bool,
    /// `[client.data.tcp]`: the TCP carrier's tunnel ceiling.
    #[serde(default)]
    pub tcp: DataCarrierLimits,
    /// `[client.data.kcp]`: the KCP carrier's tunnel ceiling.
    #[serde(default)]
    pub kcp: DataCarrierLimits,
}

/// One carrier's tunnel count (`[client.data.tcp]` / `[client.data.kcp]`).
///
/// The pool established at service start, held for the service's lifetime and
/// never resized: a deployment's capacity is a function of its configuration,
/// not of what it happened to be doing a minute ago. Failure repair is the one
/// exception — a dead tunnel is replaced so the count survives — and it is not
/// growth.
#[cfg(feature = "multiplex")]
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct DataCarrierLimits {
    /// How many tunnels this carrier's pool establishes at service start.
    /// Unset: the default ([`DEFAULT_MUX_TUNNELS`]), raised by the UDP-derived
    /// floor of the services that share the pool. Must be `>= 1`; values above
    /// [`MAX_MUX_TUNNELS_CAP`] are clamped.
    pub tunnels: Option<u16>,
}

#[cfg(feature = "multiplex")]
impl DataCarrierLimits {
    /// The count the operator asked for, clamped into `1..=MAX_MUX_TUNNELS_CAP`,
    /// or `None` for "the default, raised by the floor".
    pub fn tunnels(&self) -> Option<usize> {
        self.tunnels
            .map(|n| usize::from(n).clamp(1, usize::from(MAX_MUX_TUNNELS_CAP)))
    }

    /// The count to establish, given the UDP-derived floor of the services
    /// that share this pool: the operator's number when they wrote one, else
    /// the default raised to the floor.
    ///
    /// A floor *above* an explicit count is a configuration the pool refuses to
    /// reconcile silently (the services need more tunnels than the operator
    /// allows them), so the caller validates it first; this function only has
    /// to be total.
    pub fn resolved_tunnels(&self, floor: usize) -> usize {
        match self.tunnels() {
            Some(n) => n,
            None => floor.max(usize::from(DEFAULT_MUX_TUNNELS)),
        }
    }

    /// The count the operator wrote for this carrier, unclamped, or `None`.
    ///
    /// The L3 model reads this one rather than [`Self::tunnels`]: a
    /// transparent client's `tunnels` is a *lane budget* (one lane is one
    /// connection, [`MAX_TRANSPARENT_LANES`]), not the multiplexed pool's
    /// ceiling, so a value above that ceiling is a refusal with a number to
    /// write rather than a silent clamp.
    pub fn written(&self) -> Option<usize> {
        self.tunnels.map(usize::from)
    }
}
/// The TUN device a transparent (L3) service attaches to.
///
/// Only the *name* is configuration: the device itself, its addresses and its
/// routes belong to the operator, and this daemon deliberately never installs
/// one ([deployment.md](../../docs/deployment.md) owns the recipes). Both ends
/// may need a device — the server routes the claimed address into its own, the
/// client carries the address on its own.
///
/// On the **server** the table carries a second meaning: it is the operator's
/// own switch for the feature, because serving L3 is what asks this process for
/// `CAP_NET_ADMIN` and a device. `[server]` therefore holds this as an
/// `Option`, and a server without the table refuses every transparent
/// registration by policy instead of being steered into a device by a client.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransparentConfig {
    /// Interface name, created and configured by the operator before the
    /// daemon starts. Default: `molehill0`.
    #[serde(default = "default_tun_name")]
    pub tun: String,
}

pub(crate) fn default_tun_name() -> String {
    "molehill0".to_owned()
}

impl Default for TransparentConfig {
    fn default() -> Self {
        Self {
            tun: default_tun_name(),
        }
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
    /// Whether one control session's services share one tunnel pool per
    /// carrier (`[client.data].shared_pool`). `false` is the classic shape:
    /// one pool per service.
    #[cfg(feature = "multiplex")]
    pub fn shared_pool(&self) -> bool {
        self.data.shared_pool
    }

    /// One carrier's configured tunnel count, clamped into
    /// `1..=MAX_MUX_TUNNELS_CAP`, or `None` when the operator left it to the
    /// default (which the UDP-derived floor raises).
    #[cfg(feature = "multiplex")]
    pub fn tunnels(&self, carrier: DataCarrier) -> Option<usize> {
        match carrier {
            DataCarrier::Tcp => self.data.tcp.tunnels(),
            DataCarrier::Kcp => self.data.kcp.tunnels(),
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
    /// The operator's valve on the client's tunnel pools: how many multiplexed
    /// data tunnels **one client** may hold across every service of its
    /// session. `0` (the default) is unlimited. A tunnel over the cap is
    /// refused with a typed answer and a `debug` line naming the cap; the
    /// session itself is never touched (D14). It bounds every establishment a
    /// client's pools make — at service start and on repair alike.
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
    /// `[server.transparent]`: **the presence of this table is the switch**
    /// that lets this server serve transparent (L3) services. Without it a
    /// registration of that type is refused by policy before any device is
    /// looked at, so this process never touches `/dev/net/tun` on a
    /// client's say-so (see [`TransparentConfig`]).
    pub transparent: Option<TransparentConfig>,
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
/// `[client]` / `[transparent]` must be present.
///
/// The three blocks are three **run modes**, so a file normally carries
/// exactly one: a host that is both a forwarding client and an L3 one is two
/// processes, with their own capabilities and their own restarts.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[server]` block; `None` when absent.
    pub server: Option<ServerConfig>,
    /// `[client]` block; `None` when absent.
    pub client: Option<ClientConfig>,
    /// `[transparent]` block: the L3 client's own model; `None` when absent.
    pub transparent: Option<TransparentClientConfig>,
}

/// Which configuration model a client block was written in.
///
/// The two models share one validator because they share everything the
/// validator checks — addresses, tokens, retries, data-plane defaults. What
/// they do not share is which service types are *legal*: a `[client]` service
/// forwards and may never be transparent, while every service of an L3 block
/// is a claim. Naming the model keeps that difference in one argument instead
/// of forking the rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClientModel {
    /// `[client]`: services forward to a `local_addr`.
    Forwarding,
    /// `[transparent]`: services own their public address.
    Claiming,
}

impl ClientModel {
    /// What this model calls one entry, for messages about it. An L3 client
    /// writes `[transparent.claims.<name>]`, so calling its entries "services"
    /// would send a reader looking for a table they never wrote.
    const fn entry(self) -> &'static str {
        match self {
            Self::Forwarding => "service",
            Self::Claiming => "claim",
        }
    }

    /// The block this model's client-wide keys live in.
    const fn block(self) -> &'static str {
        match self {
            Self::Forwarding => "[client]",
            Self::Claiming => "[transparent]",
        }
    }

    /// The same, for the control-channel sub-table.
    const fn control_block(self) -> &'static str {
        match self {
            Self::Forwarding => "[client.control]",
            Self::Claiming => "[transparent.control]",
        }
    }

    /// The same, for the data-plane sub-table. Only the multiplex build has
    /// one to validate, so it is gated with its caller.
    #[cfg(feature = "multiplex")]
    const fn data_block(self) -> &'static str {
        match self {
            Self::Forwarding => "[client.data]",
            Self::Claiming => "[transparent.data]",
        }
    }

    /// The table one carrier's limits live in, e.g. `[client.data.tcp]`.
    #[cfg(feature = "multiplex")]
    fn carrier_block(self, carrier: &str) -> String {
        let base = match self {
            Self::Forwarding => "[client.data",
            Self::Claiming => "[transparent.data",
        };
        format!("{base}.{carrier}]")
    }
}

/// What asked for a pool's width floor: which knob wants one tunnel per unit.
///
/// The refusal names the knob the operator wrote, so it carries both the source
/// and the count it stood for (see `Config::validate_tunnel_floor`).
#[cfg(feature = "multiplex")]
enum FloorCause {
    /// A UDP service's worker set (`udp_workers`): its shards must stay apart.
    UdpWorkers(u16),
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
        "a service has no initial tunnel count to write; the pool's width is \
         `[client.data.tcp].tunnels` (or `[client.data.kcp].tunnels`), established at service \
         start and kept",
    ),
    (
        "client.services.*.count",
        "v0.10.0",
        "a pool belongs to the session and carrier rather than to one service; its width is \
         `[client.data.tcp].tunnels` (or `[client.data.kcp].tunnels`)",
    ),
    (
        "client.data.tcp.max_tunnels",
        "the next release",
        "the pool is pinned now: write `[client.data.tcp].tunnels`, the number of connections that \
         carrier's pool establishes at service start and keeps; the old key was the cap an elastic \
         pool grew to",
    ),
    (
        "client.data.kcp.max_tunnels",
        "the next release",
        "the pool is pinned now: write `[client.data.kcp].tunnels`, the number of connections that \
         carrier's pool establishes at service start and keeps; the old key was the cap an elastic \
         pool grew to",
    ),
    (
        "client.data.idle_timeout",
        "the next release",
        "the pool is pinned: its tunnels are established at service start and kept, so there is no \
         shrink clock to write; remove the key",
    ),
    (
        "transparent.data.tcp.max_tunnels",
        "the next release",
        "the pool is pinned now: write `[transparent.data.tcp].tunnels`, the number of connections \
         that carrier's pool establishes at service start and keeps; the old key was the cap an \
         elastic pool grew to",
    ),
    (
        "transparent.data.kcp.max_tunnels",
        "the next release",
        "the pool is pinned now: write `[transparent.data.kcp].tunnels`, the number of connections \
         that carrier's pool establishes at service start and keeps; the old key was the cap an \
         elastic pool grew to",
    ),
    (
        "transparent.data.idle_timeout",
        "the next release",
        "the pool is pinned: its tunnels are established at service start and kept, so there is no \
         shrink clock to write; remove the key",
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
    (
        "client.data.default_mode",
        "the next release",
        "the data plane's shape is derived from the service type now: a forwarding service always \
         multiplexes and `[client.data.tcp|kcp].tunnels` says how wide its pool is, so there is no \
         mode to write; remove the key",
    ),
    (
        "client.services.*.mode",
        "the next release",
        "a service's shape follows its protocol — a forwarding service multiplexes and a claim \
         does not — so there is no mode to write; remove the key",
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

/// Tables this version moved, and what to write instead.
///
/// The same reasoning as [`REMOVED_KEYS`], for the one place a *table* rather
/// than a key changed home: the strict parse would answer "unknown field
/// `transparent`", which says that something is wrong without saying what to
/// write. No version is named here because nothing of this surface was ever
/// released — the transparent model is being shaped before its first tag — so
/// there is no release for an operator to have upgraded from.
const MOVED_TABLES: &[(&str, &str)] = &[(
    "client.transparent",
    "a transparent (L3) client is its own run mode now: name the device in \
     `[transparent].tun`, write every claimed address as a `[transparent.claims.<name>]` \
     entry, and start it with `molehill <config> --transparent`",
)];

/// Refuse a config that still carries a table this version moved, naming it
/// and what to write instead.
fn reject_moved_tables(doc: &toml::Value) -> Result<()> {
    let mut found: Vec<String> = Vec::new();
    for (pattern, advice) in MOVED_TABLES {
        let segments: Vec<&str> = pattern.split('.').collect();
        let mut hits = 0;
        count_at(doc, &segments, &mut hits);
        if hits > 0 {
            found.push(format!("  `[{pattern}]`: {advice}"));
        }
    }
    anyhow::ensure!(
        found.is_empty(),
        "this config still carries configuration this version moved:\n{}\n\
         Move it, then start again.",
        found.join("\n")
    );
    Ok(())
}

/// Keys the unreleased L3 model carried, and what to write instead.
///
/// The same reasoning as [`MOVED_TABLES`], for keys rather than tables: nothing
/// of this surface was ever released (the transparent model is being shaped
/// before its first tag), so an upgrade instruction cannot name a release, but
/// the key still has to be refused by name rather than left to
/// `deny_unknown_fields`, which says *what* is wrong without saying what to do.
const UNRELEASED_KEYS: &[(&str, &str)] = &[
    (
        "transparent.data.default_mode",
        "a transparent claim never multiplexes: its data channels are carrier connections of its \
         own, and `[transparent.data.tcp|kcp].tunnels` is their budget; remove the key",
    ),
    (
        "transparent.claims.*.mode",
        "a transparent claim never multiplexes: its data channels are carrier connections of its \
         own; remove the key",
    ),
    (
        "transparent.data.default_members",
        "a claim's lanes come from its carrier's budget now: write \
         `[transparent.data.tcp|kcp].tunnels`, divided equally among the claims that draw on that \
         carrier, and every claim keeps at least one; remove the key",
    ),
    (
        "transparent.claims.*.members",
        "a claim's lanes come from its carrier's budget now: write \
         `[transparent.data.tcp|kcp].tunnels`, divided equally among the claims that draw on that \
         carrier; remove the key",
    ),
    (
        "transparent.data.shared_pool",
        "a transparent client has no pool to share: every claim's channels are connections of its \
         own, drawn from the carrier's lane budget; remove the key",
    ),
];

/// Refuse a config that still carries a key of the unreleased L3 model this
/// version does not know, naming it and what to write instead.
fn reject_unreleased_keys(doc: &toml::Value) -> Result<()> {
    let mut found: Vec<String> = Vec::new();
    for (pattern, advice) in UNRELEASED_KEYS {
        let segments: Vec<&str> = pattern.split('.').collect();
        let mut hits = 0;
        count_at(doc, &segments, &mut hits);
        if hits > 0 {
            found.push(format!("  `{pattern}` ({hits}x): {advice}"));
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
    /// Parse and validate a config document. Crate-visible because the model
    /// modules' own tests go through it: it is the only thing that validates.
    pub(crate) fn from_str(s: &str) -> Result<Config> {
        // Parse to a document first: a removed key has to be seen (and taken
        // out) before the strict struct parse, which rejects unknown fields.
        let mut doc: toml::Value =
            toml::from_str(s).with_context(|| "Failed to parse the config")?;
        reject_removed_keys(&mut doc)?;
        reject_moved_tables(&doc)?;
        reject_unreleased_keys(&doc)?;
        let mut config: Config =
            Config::deserialize(doc).with_context(|| "Failed to parse the config")?;

        if let Some(server) = config.server.as_mut() {
            Config::validate_server_config(server)?;
        }

        if let Some(client) = config.client.as_mut() {
            Config::validate_client_config(client, ClientModel::Forwarding)?;
        }

        // An L3 block is validated in its *lowered* shape, so both models go
        // through one set of rules about addresses, tokens, retries and data
        // defaults; what differs between them is which service types are
        // legal, and that is the model argument.
        if let Some(transparent) = config.transparent.as_mut() {
            #[cfg(feature = "multiplex")]
            Config::validate_claim_lanes(transparent)?;
            let mut lowered = transparent.lower();
            Config::validate_client_config(&mut lowered, ClientModel::Claiming)?;
            transparent.lowered = Some(Box::new(lowered));
        }

        if config.server.is_none() && config.client.is_none() && config.transparent.is_none() {
            Err(anyhow!(
                "Neither of `[server]`, `[client]` or `[transparent]` is defined"
            ))
        } else {
            Ok(config)
        }
    }

    /// The client block an L3 run executes on.
    ///
    /// The transparent block has no runtime of its own: it becomes the same
    /// `ClientConfig` a forwarding client uses (see
    /// [`TransparentClientConfig::lower`]), so this is the seam between "which
    /// model was written" and "which engine runs it".
    ///
    /// # Errors
    ///
    /// Fails when the config carries no `[transparent]` block, or when it was
    /// never validated — neither can happen through
    /// [`Config::from_str`], which fills the lowering in.
    pub fn into_l3_client(self) -> Result<Config> {
        let Some(transparent) = self.transparent else {
            return Err(anyhow!(
                "Try to run as a transparent (L3) client, but the configuration is missing. \
                 Please add the `[transparent]` block"
            ));
        };
        let client = transparent.lowered.ok_or_else(|| {
            anyhow!("the `[transparent]` block was not validated before it was used")
        })?;
        Ok(Config {
            server: None,
            client: Some(*client),
            transparent: None,
        })
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

    fn validate_client_config(client: &mut ClientConfig, model: ClientModel) -> Result<()> {
        if client.control.default_remote_addr.is_empty() {
            bail!(
                "`{}.default_remote_addr` is required",
                model.control_block()
            );
        }
        // The port is required, e.g. "example.com:2333"
        if client.control.default_remote_addr.rfind(':').is_none() {
            bail!(
                "{}.default_remote_addr is missing the port: {}",
                model.control_block(),
                client.control.default_remote_addr
            );
        }

        if client.default_token.is_empty() {
            bail!("`{}.default_token` must not be empty", model.block());
        }

        #[cfg(feature = "multiplex")]
        Config::validate_data_config(client, model)?;

        // Validate the entries: services in a `[client]` block, claims in a
        // `[transparent]` one. The word follows the model, because that is
        // the table the reader has in front of them.
        let entry = model.entry();
        for (name, s) in &mut client.services {
            s.name.clone_from(name);

            if s.retry_interval.is_none() {
                s.retry_interval = Some(client.control.default_retry_interval);
            }
            if let Some(addr) = s.remote_addr.as_deref()
                && addr.rfind(':').is_none()
            {
                bail!("{entry} {name}: `remote_addr` is missing the port: {addr}");
            }
            if s.token.as_ref().is_some_and(|t| t.is_empty()) {
                bail!("{entry} {name}: `token` must not be empty");
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
                    "{entry} {name}: Noise is the effective transport (per-service                     `transport.type = \"noise\"` or the client-wide `type = \"noise\"`)                     but no Noise keys are configured — set them in                     `[client.transport.noise]` or                     `[client.services.{name}.transport.noise]`"
                );
            }

            // The public endpoint is client-declared and required.
            let bind: SocketAddr = s.remote_bind_addr.parse().with_context(|| {
                format!(
                    "{entry} {}: invalid `remote_bind_addr`: {:?}. It must be a socket address like \"0.0.0.0:6022\"",
                    name, s.remote_bind_addr
                )
            })?;
            if bind.port() == 0 {
                bail!("{entry} {name}: `remote_bind_addr` port must not be 0");
            }

            Config::validate_service_protocol(name, s, model)?;

            // Fill in runtime defaults.
            if matches!(s.service_type, ServiceType::Udp) {
                match s.udp_workers {
                    None => s.udp_workers = Some(DEFAULT_UDP_WORKERS),
                    Some(0) => bail!("{entry} {name}: udp_workers must be at least 1"),
                    Some(_) => {}
                }
            }
            if s.udp_buffer_size.is_none() {
                s.udp_buffer_size =
                    Some(u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_buffer_size == Some(0) {
                bail!("{entry} {name}: udp_buffer_size must be greater than 0");
            }
            if s.udp_idle_timeout.is_none() {
                s.udp_idle_timeout = Some(DEFAULT_UDP_IDLE_TIMEOUT_SECS);
            } else if s.udp_idle_timeout == Some(0) {
                bail!("{entry} {name}: udp_idle_timeout must be greater than 0");
            }
            if s.udp_send_queue_size.is_none() {
                s.udp_send_queue_size =
                    Some(u16::try_from(DEFAULT_UDP_SENDQ_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_send_queue_size == Some(0) {
                bail!("{entry} {name}: udp_send_queue_size must be greater than 0");
            }

            #[cfg(all(feature = "multiplex", not(feature = "kcp")))]
            Config::refuse_service_kcp_without_feature(name, s, entry)?;
        }

        Config::validate_transport_config(&client.transport)?;

        Ok(())
    }

    /// Per-protocol service keys: what each protocol requires, and what it
    /// refuses because nothing would read it.
    ///
    /// The refusals are deliberate rather than lenient — a key that is
    /// accepted and ignored lets its writer believe a buffer or a worker count
    /// is in effect when nothing reads it.
    fn validate_service_protocol(
        name: &str,
        s: &mut ClientServiceConfig,
        model: ClientModel,
    ) -> Result<()> {
        let entry = model.entry();
        // Which service types a model may express is the whole of the
        // difference between them, and it is structural rather than a list of
        // refusals: a forwarding service may never own its public address (an
        // L3 client is a different run mode, with a different device and a
        // different table), and a claim has nothing to forward to.
        match (model, s.service_type) {
            (ClientModel::Forwarding, ServiceType::Transparent) => bail!(
                "{entry} {name}: `protocol = \"transparent\"` is not a forwarding protocol. An \
                 L3 client is its own run mode: move this service to a \
                 `[transparent.claims.{name}]` entry, and start the process with \
                 `molehill <config> --transparent`"
            ),
            (ClientModel::Claiming, ServiceType::Tcp | ServiceType::Udp) => bail!(
                "{entry} {name}: a `[transparent]` client claims public addresses; it has no \
                 forwarding services. Move this one to a `[client]` block in a config of its \
                 own, started as a client"
            ),
            (ClientModel::Claiming, ServiceType::Transparent) => {
                if !cfg!(all(feature = "transparent", target_os = "linux")) {
                    let why = if cfg!(feature = "transparent") {
                        "this platform is not Linux"
                    } else {
                        "this build was compiled without the `transparent` feature"
                    };
                    bail!(
                        "{entry} {name}: `[transparent]` carries whole IP packets through a \
                         TUN device, and {why}"
                    );
                }
            }
            (ClientModel::Forwarding, ServiceType::Tcp | ServiceType::Udp) => {
                if s.local_addr.is_empty() {
                    let protocol = if s.service_type == ServiceType::Tcp {
                        "tcp"
                    } else {
                        "udp"
                    };
                    bail!("{entry} {name}: `local_addr` is required for a {protocol} service");
                }
            }
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
                        "{entry} {name}: `{key}` is only valid for a UDP service \
                         (`protocol = \"udp\"`), but this service is TCP. Remove the \
                         key, or declare the service as UDP"
                    );
                }
            }
        }

        Ok(())
    }

    /// Validate the data-plane knobs of whichever block the reader wrote.
    #[cfg(feature = "multiplex")]
    fn validate_data_config(client: &ClientConfig, model: ClientModel) -> Result<()> {
        use DataCarrier::Kcp;

        let data = &client.data;
        let block = model.data_block();

        // The pool's per-carrier tunnel count. `0` would mean "a pool with no
        // tunnel"; the value is validated rather than clamped so a typo is
        // refused with the carrier's name in it, and a count *below* what the
        // services need is refused below (the pool no longer grows to fit).
        for (carrier, limits) in [("tcp", &data.tcp), ("kcp", &data.kcp)] {
            if limits.tunnels == Some(0) {
                bail!(
                    "`{}.tunnels` must be at least 1",
                    model.carrier_block(carrier)
                );
            }
        }
        Self::validate_tunnel_floor(client, model)?;
        if let Some(addr) = data.default_data_addr.as_deref()
            && addr.rfind(':').is_none()
        {
            bail!("{block}.default_data_addr is missing the port: {addr}");
        }

        // The carrier is orthogonal to the mode, and has been since a direct
        // channel could ride a KCP session: `kcp` needs a build with the
        // feature, and that is the whole rule.
        if matches!(data.default_carrier, Kcp) {
            #[cfg(not(feature = "kcp"))]
            bail!(
                "`{block}.default_carrier = \"kcp\"` requires a binary built with the `kcp` feature"
            );
        }

        Ok(())
    }

    /// Refuse a lane budget that cannot give every claim a connection of its
    /// own.
    ///
    /// A claim's lane **is** a carrier connection — a transparent client never
    /// multiplexes — so a budget below the number of claims drawing on that
    /// carrier cannot be honoured: some claim would have no carrier at all. One
    /// lane per claim is therefore both the default (when the key is unwritten)
    /// and the floor, and the message names the count to write instead. The
    /// upper bound is a resource guard, not a pool ceiling: `tunnels` here is
    /// [`MAX_TRANSPARENT_LANES`], not [`MAX_MUX_TUNNELS_CAP`].
    ///
    /// Run on the L3 model's own block, before it is lowered: the key the
    /// message names is the one the reader wrote.
    #[cfg(feature = "multiplex")]
    fn validate_claim_lanes(transparent: &TransparentClientConfig) -> Result<()> {
        for carrier in [DataCarrier::Tcp, DataCarrier::Kcp] {
            let claims = transparent.claims_on(carrier);
            if claims == 0 {
                continue;
            }
            let block = format!("[transparent.data.{}]", carrier.as_str());
            let Some(written) = transparent.written_budget(carrier) else {
                continue;
            };
            let cap = usize::from(MAX_TRANSPARENT_LANES);
            if written > cap {
                bail!(
                    "`{block}.tunnels = {written}` is above the {cap} carrier connections a \
                     transparent client may hold on one carrier; lower it, or split the claims \
                     across clients"
                );
            }
            if written < claims {
                bail!(
                    "`{block}.tunnels = {written}` is below what this client's claims need: \
                     {claims} claim(s) draw on the {} carrier, and a claim's lane is a connection \
                     of its own. Write `{block}.tunnels = {claims}` or more",
                    carrier.as_str()
                );
            }
        }
        Ok(())
    }

    /// Refuse a tunnel count smaller than what the services of that carrier
    /// need.
    ///
    /// One thing asks for one tunnel per unit of demand: a UDP service's worker
    /// set shards across tunnels. A pinned pool keeps one tunnel per worker so
    /// those shards stay on distinct tunnels — that is the floor, and a pool that
    /// cannot grow cannot meet it later either, so a count below it is a
    /// configuration that would silently degrade the service it was written
    /// for. The message names the count to write instead, because an operator
    /// who has just been told "no" needs the number, not the rule.
    ///
    /// A transparent claim is not part of this floor: it never draws from a
    /// pool, and what bounds its own carrier connections is validated on the L3
    /// block itself.
    #[cfg(feature = "multiplex")]
    fn validate_tunnel_floor(client: &ClientConfig, model: ClientModel) -> Result<()> {
        for (carrier, limits) in [
            (DataCarrier::Tcp, &client.data.tcp),
            (DataCarrier::Kcp, &client.data.kcp),
        ] {
            let Some(written) = limits.tunnels() else {
                continue;
            };
            let Some((needed, name, cause)) = Self::deepest_floor(client, carrier) else {
                continue;
            };
            if needed <= written {
                continue;
            }
            let block = model.carrier_block(carrier.as_str());
            let why = match cause {
                FloorCause::UdpWorkers(workers) => format!(
                    "service `{name}` declares {workers} UDP workers, and a pool keeps one tunnel \
                     per worker so their shards stay on distinct tunnels. Write `{block}.tunnels \
                     = {needed}` or more, or lower that service's `udp_workers`."
                ),
            };
            bail!(
                "`{block}.tunnels = {written}` is below what the services of that carrier need: \
                 {why}"
            );
        }
        Ok(())
    }

    /// The deepest floor among the services that would share a pool on this
    /// carrier, the service that owns it, and what asked for it.
    ///
    /// "Deepest" rather than "summed": every pool is keyed per service (or, with
    /// `shared_pool`, per session), and a service's own pool only ever has to
    /// carry that service's workers at once, because what a floor buys is
    /// distinctness *within* one set of channels. The floor of the largest
    /// demand is therefore what every pool on that carrier must be able to hold:
    /// the conservative reading, and the one that cannot under-provision a
    /// service.
    ///
    /// Only a service that draws from a pool counts. A transparent claim never
    /// does: its channels are carrier connections of its own (it never
    /// multiplexes), so its count is no reason to refuse a tunnel count.
    #[cfg(feature = "multiplex")]
    fn deepest_floor(
        client: &ClientConfig,
        carrier: DataCarrier,
    ) -> Option<(usize, String, FloorCause)> {
        let stream_cap = crate::transport::multiplex::stream_cap();
        let mut deepest: Option<(usize, String, FloorCause)> = None;
        for (name, service) in &client.services {
            if service.carrier.unwrap_or(client.data.default_carrier) != carrier {
                continue;
            }
            let (demand, cause) = match service.service_type {
                ServiceType::Udp => {
                    let workers = service.udp_workers.unwrap_or(DEFAULT_UDP_WORKERS);
                    (usize::from(workers), FloorCause::UdpWorkers(workers))
                }
                ServiceType::Transparent | ServiceType::Tcp => continue,
            };
            let floor = crate::transport::pool::udp_floor([demand], stream_cap);
            if deepest
                .as_ref()
                .is_none_or(|(widest, _, _)| floor > *widest)
            {
                deepest = Some((floor, name.clone(), cause));
            }
        }
        deepest
    }

    /// Refuse a service that asks for the KCP carrier in a binary without the
    /// feature.
    ///
    /// `mode` and `carrier` are independent — a direct channel may ride a KCP
    /// session — so this is the only per-service data-plane rule left, and it
    /// exists only in the build that can fail it. The message names the
    /// service, its block and the feature.
    #[cfg(all(feature = "multiplex", not(feature = "kcp")))]
    fn refuse_service_kcp_without_feature(
        name: &str,
        s: &ClientServiceConfig,
        entry: &str,
    ) -> Result<()> {
        if matches!(s.carrier, Some(DataCarrier::Kcp)) {
            bail!(
                "{entry} {name}: `carrier = \"kcp\"` requires a binary built with the `kcp` feature"
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
    /// leg CI runs (AGENTS.md §11). Such a fixture says so on a leading comment
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
            "transparent" => {
                let available = cfg!(all(feature = "transparent", target_os = "linux"));
                if !available {
                    println!("  skip {name}: needs a Linux build with the `transparent` feature");
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
                std::hint::black_box(DEFAULT_MUX_TUNNELS),
                4,
                "the carrier's tunnel count"
            );
            assert_eq!(
                std::hint::black_box(MAX_MUX_TUNNELS_CAP),
                64,
                "the tunnel count clamp"
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

    /// `[server.transparent]` is opt-in, and the *table's presence* is the
    /// switch: a server that never mentions it parses to `None` rather than to
    /// a default device it would then be willing to attach. Inside the table
    /// the device name keeps its default, because writing the table already is
    /// the operator's decision.
    #[test]
    fn test_server_transparent_is_opt_in() {
        let without = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        let cfg = Config::from_str(without).unwrap();
        assert!(
            cfg.server.unwrap().transparent.is_none(),
            "a server that does not mention transparent must not be armed for it"
        );

        let with = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transparent]
"#;
        let cfg = Config::from_str(with).unwrap();
        assert_eq!(
            cfg.server.unwrap().transparent.map(|t| t.tun).as_deref(),
            Some("molehill0"),
            "writing the table is the decision; the device name still defaults"
        );
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
        assert!(Config::validate_client_config(&mut cfg, ClientModel::Forwarding).is_err());

        // Invalid remote_bind_addr (missing port)
        cfg.services.insert("foo1".into(), svc("0.0.0.0"));
        assert!(Config::validate_client_config(&mut cfg, ClientModel::Forwarding).is_err());

        // Port 0 is rejected
        cfg.services.insert("foo1".into(), svc("0.0.0.0:0"));
        assert!(Config::validate_client_config(&mut cfg, ClientModel::Forwarding).is_err());

        // A valid config passes and gets its runtime defaults filled in
        cfg.services.insert("foo1".into(), svc("0.0.0.0:6081"));
        Config::validate_client_config(&mut cfg, ClientModel::Forwarding)?;
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
        assert!(
            cfg.server.unwrap().allow_ports.is_empty(),
            "a config that declares no allow_ports must parse with an empty whitelist"
        );

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
        // `[client.data]` acts as defaults; a service's own carrier wins. The
        // runtime merge lives in `DataOpts::for_service` (client code); here we
        // pin that the key parses and validates per service.
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
"#,
        );
        config.push_str(if cfg!(feature = "kcp") {
            "default_carrier = \"kcp\"\n\n[client.data.kcp]\ntunnels = 6\n"
        } else {
            "default_carrier = \"tcp\"\n"
        });
        // One service overrides the carrier back to TCP: the per-service key is
        // independent of the block.

        config.push_str(
            r#"
[client.services.muxed]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.services.bulk]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
carrier = "tcp"
"#,
        );
        let cfg = Config::from_str(&config).unwrap();
        let services = &cfg.client.unwrap().services;
        assert_eq!(services["muxed"].carrier, None);
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
        // A forwarding service multiplexes whatever carrier it names: a KCP
        // service rides KCP *tunnels* (the carrier's session is the pool's
        // connection, with the multiplexer above it), and that must load.
        let kcp_service = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
carrier = "kcp"
"#;
        if cfg!(feature = "kcp") {
            let cfg = Config::from_str(kcp_service).unwrap();
            let s = &cfg.client.unwrap().services["test"];
            assert_eq!(s.carrier, Some(DataCarrier::Kcp));
        } else {
            // Without the feature the carrier is refused, and the message names
            // the feature rather than a mode the pair never needed.
            let err = format!("{:#}", Config::from_str(kcp_service).unwrap_err());
            assert!(
                err.contains("kcp"),
                "the refusal must name the feature: {err}"
            );
        }

        // `tunnels = 0` is rejected, with the carrier named.
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data.tcp]
tunnels = 0

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let err = Config::from_str(bad).unwrap_err();
        assert!(
            format!("{err:#}").contains("tunnels"),
            "the refusal must name the key: {err:#}"
        );
    }

    /// An explicit tunnel count below the UDP-derived floor is refused: a
    /// pinned pool cannot grow to meet it, so the configuration would silently
    /// under-provision the workers the service declared.
    #[cfg(feature = "multiplex")]
    #[test]
    fn test_a_tunnel_count_below_the_udp_floor_is_refused() {
        let config = |tunnels: u16| {
            format!(
                r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data.tcp]
tunnels = {tunnels}

[client.services.dns]
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:5353"
udp_workers = 4
"#
            )
        };

        // Four workers ask for four paths; two tunnels cannot hold them, and
        // the refusal names the number to write instead.
        let err = format!("{:#}", Config::from_str(&config(2)).unwrap_err());
        assert!(
            err.contains("`[client.data.tcp].tunnels = 2`")
                && err.contains("`[client.data.tcp].tunnels = 4`")
                && err.contains("udp_workers"),
            "the refusal must name the key, the count to write and the knob: {err}"
        );

        // The floor itself (and anything above it) is accepted: the operator's
        // number is the pool.
        for tunnels in [4u16, 8] {
            let cfg = Config::from_str(&config(tunnels)).unwrap();
            assert_eq!(
                cfg.client.unwrap().tunnels(DataCarrier::Tcp),
                Some(usize::from(tunnels))
            );
        }

        // Leaving it unset is always fine: the default is raised to the floor
        // at establishment time.
        let unset = config(4).replace("tunnels = 4\n", "");
        assert!(Config::from_str(&unset).is_ok());
    }

    /// A lane budget is a count like any other: above the resource guard it is
    /// refused with the bound named, never clamped. Clamping is the *pool*
    /// ceiling's behaviour, and a claim's lanes are connections rather than
    /// streams, so the two are deliberately different keys with different
    /// bounds.
    #[cfg(feature = "multiplex")]
    #[test]
    fn test_a_lane_budget_above_the_guard_is_refused() {
        let config = |tunnels: u16| {
            format!(
                r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.data.tcp]
tunnels = {tunnels}

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#
            )
        };

        let over = format!(
            "{:#}",
            Config::from_str(&config(MAX_TRANSPARENT_LANES + 1)).unwrap_err()
        );
        assert!(
            over.contains("[transparent.data.tcp].tunnels")
                && over.contains(&MAX_TRANSPARENT_LANES.to_string()),
            "the refusal must name the key and the bound: {over}"
        );

        // The guard itself is a budget this client may hold.
        assert!(Config::from_str(&config(MAX_TRANSPARENT_LANES)).is_ok());
    }
}
