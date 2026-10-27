use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::Path;
use tokio::fs;

#[cfg(feature = "carrier")]
use crate::common::constants::MAX_TRANSPARENT_LANES;
use crate::common::constants::{
    DEFAULT_MAX_UDP_WORKERS, DEFAULT_UDP_BUFFER_SIZE, MAX_UDP_WORKERS_CAP,
};
#[cfg(feature = "multiplex")]
use crate::common::constants::{DEFAULT_MUX_TUNNELS, MAX_MUX_TUNNELS_CAP};
use crate::config::transparent::TransparentClientConfig;

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

/// Wire stack of the control channel: plain bytes or Noise.
///
/// Only the control channel, which is always TCP. The data plane's own carrier
/// is chosen at runtime (mhd, falling back to TCP) and is not a key.
#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
#[doc(hidden)]
pub enum TransportType {
    #[default]
    #[serde(rename = "plain")]
    Plain,
    #[serde(rename = "noise")]
    Noise,
}

/// Per-service Noise overrides (`[client.services.<name>.noise]`).
///
/// The same three keys as `[client.noise]`, each optional: an unset key
/// inherits the client-wide value. Keys override the *session's* identity
/// (a service may therefore dial another server with its own pair), while
/// `data_enable` only selects whether this service's data channels are
/// encrypted.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceNoiseConfig {
    /// The peer's public key, base64 32 bytes. Unset inherits `[client.noise]`.
    pub public_key: Option<String>,
    /// This host's private key, base64 32 bytes. Unset inherits
    /// `[client.noise]`.
    pub private_key: Option<MaskedString>,
    /// Whether this service's data channels are encrypted. Unset inherits
    /// `[client.noise].data_enable`.
    pub data_enable: Option<bool>,
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
    /// This service's cap on the reconnect backoff. Unset inherits
    /// `[client.control].retry_backoff_max`; when that is unset too, the cap
    /// is the adaptive default of [`crate::common::constants::backoff_cap`].
    pub retry_backoff_max: Option<u64>,
    /// Override `[client].default_token` for this service only — e.g. to
    /// authenticate against a server that has its own token.
    pub token: Option<MaskedString>,
    /// Override `[client.control].default_remote_addr` for this service
    /// only: the service's control channel (and, by default, its data
    /// plane) dials this server instead of the client-wide one.
    pub remote_addr: Option<String>,
    /// Per-service Noise overrides (keys + data-channel encryption).
    pub noise: Option<ServiceNoiseConfig>,
    /// The ceiling on this UDP service's worker set: the client starts one
    /// data channel and opens another only when the server asks for one.
    /// Default: 2; clamped to 1..=64. UDP services only — a TCP service opens
    /// one data channel per visitor, on demand, so the key is an error there.
    pub max_udp_workers: Option<u16>,
    /// Receive buffer size for UDP datagrams in bytes. Default: 2048,
    /// maximum 65535 (bounded by the wire format's `u16` length).
    pub udp_buffer_size: Option<u16>,
    /// The TUN device this service attaches to, filled for a claim by
    /// [`crate::config::transparent::TransparentClientConfig::lower`]. Empty
    /// for every forwarding service: only a claim touches a device.
    #[serde(skip)]
    pub transparent_tun: String,
    /// How many carrier connections this claim holds — its **lanes** — filled
    /// by the L3 model's lowering from the carrier's budget
    /// (`[transparent.data].tunnels`, divided equally among the claims
    /// that draw on it). `0` for every forwarding service, which never reads it.
    #[serde(skip)]
    pub transparent_lanes: u16,
    /// Whether this claim's lanes may be lent to a sibling claim carrying more
    /// flows than lanes (`[transparent.data].lend`). Derived, never configured
    /// per service: the key is the client's, and every claim of a client states
    /// the same value.
    #[serde(skip)]
    pub transparent_lend: bool,
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

    /// The effective KK key pair this service's session uses: its own
    /// `noise` overrides applied to the client-wide pair.
    ///
    /// A service that names another server's keys therefore gets a session of
    /// its own (the session key is the endpoint plus this pair), while a
    /// service that only flips `data_enable` keeps sharing one.
    pub fn noise_keys_with(&self, global: &NoiseConfig) -> NoiseConfig {
        let Some(noise) = &self.noise else {
            return global.clone();
        };
        NoiseConfig {
            public_key: noise
                .public_key
                .clone()
                .unwrap_or_else(|| global.public_key.clone()),
            private_key: noise
                .private_key
                .clone()
                .unwrap_or_else(|| global.private_key.clone()),
        }
    }

    /// Whether this service's data channels are encrypted: its own override,
    /// else the client-wide default.
    pub fn data_enabled_with(&self, global: bool) -> bool {
        self.noise
            .as_ref()
            .and_then(|noise| noise.data_enable)
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
    /// address and the server holds no connection state. Linux and macOS run
    /// both ends; Windows can serve but not claim. The privilege is
    /// `CAP_NET_ADMIN` (Linux), root (macOS), or Administrator + `wintun.dll`
    /// (Windows server); see `docs/configuration.md`.
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

/// Noise key material: the fixed KK handshake's two static keys.
///
/// Both keys are required and base64-encoded 32-byte Curve25519 values. The
/// pattern is not configurable: every session is
/// `Noise_KK_25519_ChaChaPoly_BLAKE2s`, so both ends already know each
/// other's static key and no anonymous suite can be selected by a config.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoiseConfig {
    /// The peer's public key, base64 32 bytes.
    pub public_key: String,
    /// This host's private key, base64 32 bytes.
    pub private_key: MaskedString,
}

impl Default for NoiseConfig {
    /// An empty pair: only a default-constructed value that never reaches a
    /// handshake. Validation refuses it before a config can start.
    fn default() -> Self {
        Self {
            public_key: String::new(),
            private_key: MaskedString::default(),
        }
    }
}

/// Client-wide Noise settings (`[client.noise]`).
///
/// The control channel always speaks KK with these keys. `data_enable`
/// decides only whether data channels (a forwarding client's tunnels and
/// lanes) are wrapped in the same session's record stream; the control
/// channel is never plain.
#[derive(Debug, Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientNoiseConfig {
    #[serde(flatten)]
    pub keys: NoiseConfig,
    /// Whether data channels are encrypted. Default: `true` for a forwarding
    /// client, `false` for a transparent one (computed by its lowering).
    pub data_enable: Option<bool>,
}

/// An internal wire-stack selection, built by the runtime from the client
/// settings. It is deliberately **not** a configuration table: the control
/// channel is always Noise and `data_enable` is the only switch, so this
/// type exists to keep one call shape in `transport.rs`.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
#[doc(hidden)]
pub struct TransportConfig {
    pub transport_type: TransportType,
    pub noise: Option<NoiseConfig>,
}

/// The optional status API (`[client.status]`).
///
/// A read-only HTTP endpoint that answers "what is this client carrying":
/// its services, their tunnel pools with per-tunnel bytes and ARQ health,
/// and the transparent claims. One GET path, one bearer token, no frontend —
/// the panel is someone else's program. Absent means the whole feature is
/// off: no listener, no port, nothing to probe.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Address the status API listens on, e.g. `"127.0.0.1:9090"`. The
    /// sensible default is the loopback: this endpoint states the machine's
    /// own topology, and binding it beyond the host is the operator's
    /// explicit decision, not the daemon's.
    pub bind_addr: String,
    /// The bearer token every request must present
    /// (`Authorization: Bearer <token>`). Required and non-empty: an
    /// unauthenticated topology view on the machine that is itself how the
    /// traffic gets in would hand an attacker the map of what to attack.
    pub token: MaskedString,
}

/// Control-channel defaults (`[client.control]`).
///
/// Every service inherits these and may override `remote_addr` and
/// `retry_backoff_max` on its own `[client.services.<name>]` block.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
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
    /// Cap on the control-channel reconnect backoff, in seconds. Unset means
    /// the adaptive default of [`crate::common::constants::backoff_cap`],
    /// computed from the number of consecutive failures.
    #[serde(default)]
    pub retry_backoff_max: Option<u64>,
}

/// Data-plane defaults (`[client.data]`).
///
/// Every service of one control session shares one tunnel pool; `addr` itself
/// cannot be overridden per service, but a service with its own `remote_addr`
/// dials that server's data endpoint instead.
///
/// Neither the *shape* of the data plane nor its *carrier* is a key: a
/// forwarding service always multiplexes ([`ClientServiceConfig::uses_pool`]), a
/// transparent claim never does, and the carrier is chosen at runtime (mhd,
/// falling back to TCP).
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientDataConfig {
    /// Data-plane endpoint; defaults to the service's control endpoint
    /// (`[client.services.<name>].remote_addr` when set, else
    /// `[client.control].default_remote_addr`). Applies to every service
    /// that does not override `remote_addr` itself.
    pub default_data_addr: Option<String>,
    /// The pool's width: how many carrier connections this session holds for
    /// every service it carries. Unset means the default
    /// ([`DEFAULT_MUX_TUNNELS`]). Must be `>= 1`; values above
    /// [`MAX_MUX_TUNNELS_CAP`] are clamped.
    ///
    /// A transparent client writes the same key for a different quantity — a
    /// claim's **lane budget** — because a claim never multiplexes.
    pub tunnels: Option<u16>,
}
/// The TUN device a transparent (L3) service attaches to.
///
/// This table names the device. It installs no route, rule or firewall rule
/// ([deployment.md](../../docs/deployment.md) owns the recipes). Linux creates
/// a missing persistent device and attaches to an existing one untouched;
/// macOS and Windows create the ephemeral utun/Wintun adapter, and macOS also
/// assigns the claimed `/32`s. Both ends need a device — the server routes the
/// claimed address into its own, the client carries the address on its own.
///
/// On the **server** the table carries a second meaning: it is the operator's
/// own switch for the feature, because serving L3 is what asks this process for
/// a device and the platform privilege. `[server]` therefore holds this as an
/// `Option`, and a server without the table refuses every transparent
/// registration by policy instead of being steered into a device by a client.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransparentConfig {
    /// Interface name. On Linux the daemon creates a missing device and uses
    /// an existing one as it is; on macOS `auto` chooses a free `utunN`; on
    /// Windows it names the Wintun adapter. Default: `molehill0` (Linux and
    /// Windows), `auto` (macOS).
    #[serde(default = "default_tun_name")]
    pub tun: String,
}

pub(crate) fn default_tun_name() -> String {
    if cfg!(mh_macos) {
        // macOS utun devices are created on demand and their unit number is
        // only known after creation; `auto` lets the daemon choose a free one.
        "auto".to_owned()
    } else {
        "molehill0".to_owned()
    }
}

/// Whether this build and platform can run a transparent **client**.
///
/// Windows is deliberately absent: it has no source-address policy routing,
/// so the client's replies would not return to the TUN. It can still serve as
/// a transparent server.
pub(crate) const fn transparent_client_supported() -> bool {
    cfg!(mh_l3_client)
}

/// Whether this build and platform can run a transparent **server**.
pub(crate) const fn transparent_server_supported() -> bool {
    cfg!(mh_l3_server)
}

/// The platform-specific half of a transparent refusal message.
fn transparent_platform_reason(client: bool) -> &'static str {
    if !cfg!(feature = "transparent") {
        "this build was compiled without the `transparent` feature"
    } else if !cfg!(feature = "carrier") {
        "this build was compiled without the `carrier` feature (transparent requires it)"
    } else if client && !cfg!(feature = "client") {
        "this build was compiled without the `client` feature"
    } else if !client && !cfg!(feature = "server") {
        "this build was compiled without the `server` feature"
    } else if client && cfg!(mh_windows) {
        "Windows has no source-address policy routing; run the transparent client under WSL2 \
         (Linux), or on Linux/macOS"
    } else {
        "this platform is not supported"
    }
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
    /// The optional read-only status API (`[client.status]`). Absent = off.
    #[serde(default)]
    pub status: Option<StatusConfig>,
    /// The fixed KK key pair and whether data channels are encrypted.
    pub noise: ClientNoiseConfig,
}

impl ClientConfig {
    /// The session's single pool width, clamped into `1..=MAX_MUX_TUNNELS_CAP`.
    ///
    /// One control session holds one pool, so this is the client-wide count
    /// every service of the session draws on; a service cannot resize it.
    #[cfg(feature = "multiplex")]
    pub fn tunnels(&self) -> usize {
        usize::from(self.data.tunnels.unwrap_or(DEFAULT_MUX_TUNNELS))
            .clamp(1, usize::from(MAX_MUX_TUNNELS_CAP))
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

/// Control-channel listener (`[server.control]`).
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerControlConfig {
    /// Address the control channel listens on. Required.
    pub bind_addr: String,
    /// Application-layer heartbeat interval in seconds; `0` disables it.
    /// Unset means the adaptive tier of
    /// [`crate::common::constants::heartbeat_interval_for`], chosen from the
    /// session count when a session is authenticated.
    pub heartbeat_interval: Option<u64>,
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
    /// The server's **global** valve on live carrier connections: every
    /// multiplexed tunnel and every transparent lane of every client counts
    /// against it. `0` is unlimited; unset derives the cap from the process's
    /// open-file limit (`[crate::common::constants::max_tunnels_for]`). A
    /// connection over the cap is refused with a typed answer and a `debug!`
    /// line naming the cap; the session itself is never touched (D14).
    pub max_tunnels: Option<u32>,
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
    /// The fixed KK key pair. Required: the control channel is never plain.
    pub noise: NoiseConfig,
    /// `[server.transparent]`: **the presence of this table is the switch**
    /// that lets this server serve transparent (L3) services. Without it a
    /// registration of that type is refused by policy before any device is
    /// looked at, so this process never touches `/dev/net/tun` on a
    /// client's say-so (see [`TransparentConfig`]).
    pub transparent: Option<TransparentConfig>,
}

impl ServerConfig {
    /// The control-channel heartbeat cadence to declare, in seconds.
    ///
    /// The operator's value when written (including `0`, which disables it),
    /// else the adaptive tier of the current session count.
    pub fn heartbeat_interval(&self, sessions: usize) -> u64 {
        self.control
            .heartbeat_interval
            .unwrap_or_else(|| crate::common::constants::heartbeat_interval_for(sessions))
    }

    /// Data-plane listener address: `[server.data].bind_addr`, or the
    /// control channel's `bind_addr` when unset.
    #[cfg(feature = "multiplex")]
    pub fn data_bind_addr(&self) -> &str {
        self.data
            .bind_addr
            .as_deref()
            .unwrap_or(self.control.bind_addr.as_str())
    }

    /// The global tunnel cap, resolved once at server start: the written
    /// value, or the adaptive default derived from the process's open-file
    /// limit. `0` is unlimited.
    #[cfg(feature = "multiplex")]
    pub fn max_tunnels(&self) -> usize {
        match self.data.max_tunnels {
            Some(written) => usize::try_from(written).unwrap_or(usize::MAX),
            None => {
                crate::common::constants::max_tunnels_for(crate::common::constants::soft_nofile())
            }
        }
    }

    /// Without the `multiplex` feature there is no `[server.data]` table and
    /// no tunnel pool to bound, so the v3 clamp this feeds is a no-op; the
    /// method keeps one signature across the two builds.
    #[cfg(not(feature = "multiplex"))]
    pub fn max_tunnels(&self) -> usize {
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
         `[client.data].tunnels`, established at service start and kept",
    ),
    (
        "client.services.*.count",
        "v0.10.0",
        "a pool belongs to the session rather than to one service; its width is \
         `[client.data].tunnels`",
    ),
    (
        "client.data.tcp",
        "the next release",
        "the data plane's carrier is chosen at runtime now (mhd, falling back to TCP), so it is not \
         a row to write: move its `tunnels` up into `[client.data].tunnels`",
    ),
    (
        "client.data.kcp",
        "the next release",
        "the data plane's carrier is chosen at runtime now (mhd, falling back to TCP), so it is not \
         a row to write: move its `tunnels` up into `[client.data].tunnels`",
    ),
    (
        "client.data.default_carrier",
        "the next release",
        "the carrier is chosen at runtime (mhd, falling back to TCP); remove the key",
    ),
    (
        "client.services.*.carrier",
        "the next release",
        "the carrier is chosen at runtime (mhd, falling back to TCP); a service's own \
         `tunnels` is the data-plane key it may still write",
    ),
    (
        "client.data.idle_timeout",
        "the next release",
        "the pool is pinned: its tunnels are established at service start and kept, so there is no \
         shrink clock to write; remove the key",
    ),
    (
        "client.services.*.pool_size",
        "v0.10.0",
        "write `max_udp_workers` for a UDP service: it is the ceiling on the worker set; a TCP service opens one channel per visitor",
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
        "write `[server.data].max_tunnels`: it is the tunnel cap one client may hold \
         (0 = unlimited), where the old key bounded a v3 client's requested channel count",
    ),
    (
        "server.data.max_tunnels_per_client",
        "v0.11.2",
        "the valve is global now: write `[server.data].max_tunnels` (unset = \
         derived from the open-file limit, 0 = unlimited)",
    ),
    (
        "server.data.stripe_count",
        "v0.11.2",
        "striping is dynamic now: it starts at one data channel per visitor and grows under backpressure; remove the key",
    ),
    (
        "client.services.*.udp_workers",
        "v0.11.2",
        "write `max_udp_workers`: it is the ceiling on the service worker set, not the count it starts with (one channel, grown on the server request)",
    ),
    (
        "client.services.*.udp_send_queue_size",
        "v0.11.2",
        "the queue is internal now (CoDel); remove the key",
    ),
    (
        "client.services.*.udp_idle_timeout",
        "v0.11.2",
        "a peer mapping lifetime adapts to the peer traffic now; remove the key",
    ),
    (
        "client.services.*.udp_forwarder_ipv6",
        "v0.11.2",
        "the forwarder follows the address family of local_addr; remove the key",
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
         multiplexes and `[client.data].tunnels` says how wide its pool is, so there is no mode to \
         write; remove the key",
    ),
    (
        "client.services.*.tunnels",
        "v0.11.2",
        "one control session holds one tunnel pool now: write `[client.data].tunnels` for the \
         session's total width, and remove this key",
    ),
    (
        "client.data.shared_pool",
        "v0.11.2",
        "a session always has one tunnel pool now; remove the key (its width is \
         `[client.data].tunnels`)",
    ),
    (
        "transparent.claims.*.tunnels",
        "v0.11.2",
        "a claim's lane count is its share of the client's lane budget: write \
         `[transparent.data].tunnels` for the budget, and remove this key",
    ),
    (
        "client.services.*.mode",
        "the next release",
        "a service's shape follows its protocol — a forwarding service multiplexes and a claim \
         does not — so there is no mode to write; remove the key",
    ),
    (
        "client.services.*.nodelay",
        "v0.11.2",
        "every forwarded connection has TCP_NODELAY on now; remove the key",
    ),
    (
        "client.control.default_retry_interval",
        "v0.11.2",
        "the key names a cap, not an interval: write `[client.control].retry_backoff_max` \
         (unset = the adaptive default)",
    ),
    (
        "client.services.*.retry_interval",
        "v0.11.2",
        "the key names a cap, not an interval: write this service's `retry_backoff_max` \
         (unset = inherit `[client.control].retry_backoff_max`)",
    ),
    (
        "transparent.control.default_retry_interval",
        "v0.11.2",
        "the key names a cap, not an interval: write `[transparent.control].retry_backoff_max` \
         (unset = the adaptive default)",
    ),
    (
        "transparent.claims.*.retry_interval",
        "v0.11.2",
        "the key names a cap, not an interval: write this claim's `retry_backoff_max` \
         (unset = inherit `[transparent.control].retry_backoff_max`)",
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
const MOVED_TABLES: &[(&str, &str)] = &[
    (
        "client.transparent",
        "a transparent (L3) client is its own run mode now: name the device in \
         `[transparent].tun`, write every claimed address as a `[transparent.claims.<name>]` \
         entry, and start it with `molehill <config> --transparent`",
    ),
    (
        "client.transport",
        "the wire tables are flat now: write `[client.noise]` with `public_key`, \
         `private_key` and `data_enable`. The handshake is fixed KK, so `pattern`, \
         `psk`, `psk_location`, `resume` and `proxy` have no replacement",
    ),
    (
        "server.transport",
        "the wire table is flat now: write `[server.noise]` with this server's \
         `private_key` and the client's `public_key`. The handshake is fixed KK, so \
         `pattern`, `psk`, `psk_location` and `resume` have no replacement",
    ),
    (
        "transparent.transport",
        "the wire table is flat now: write `[transparent.noise]` with `public_key`, \
         `private_key` and `data_enable` (default false). The control channel is always \
         KK; `proxy` is gone",
    ),
    (
        "client.services.*.transport",
        "a service's wire settings are `[client.services.<name>.noise]` now: write \
         `public_key` / `private_key` (to use another server's pair) and `data_enable`. \
         There is no `type`: the control channel is always Noise, and data_enable only \
         selects the data channels",
    ),
];

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
         own, and `[transparent.data].tunnels` is their budget; remove the key",
    ),
    (
        "transparent.claims.*.mode",
        "a transparent claim never multiplexes: its data channels are carrier connections of its \
         own; remove the key",
    ),
    (
        "transparent.data.default_members",
        "a claim's lanes come from its carrier's budget now: write \
         `[transparent.data].tunnels`, divided equally among the claims that draw on that \
         carrier, and every claim keeps at least one; remove the key",
    ),
    (
        "transparent.claims.*.members",
        "a claim's lanes come from its carrier's budget now: write \
         `[transparent.data].tunnels`, divided equally among the claims that draw on that \
         carrier; remove the key",
    ),
    (
        "transparent.data.tcp",
        "a transparent client's carrier is chosen at runtime (mhd, falling back to TCP), so the \
         lane budget is not a row per carrier: move its `tunnels` up into `[transparent.data]`",
    ),
    (
        "transparent.data.kcp",
        "a transparent client's carrier is chosen at runtime (mhd, falling back to TCP), so the \
         lane budget is not a row per carrier: move its `tunnels` up into `[transparent.data]`",
    ),
    (
        "transparent.data.default_carrier",
        "the carrier is chosen at runtime (mhd, falling back to TCP); remove the key",
    ),
    (
        "transparent.claims.*.carrier",
        "the carrier is chosen at runtime (mhd, falling back to TCP); a claim's own `tunnels` is \
         the key it may still write",
    ),
    (
        "transparent.data.shared_pool",
        "a transparent client has no pool to share: every claim's channels are connections of its \
         own, drawn from the carrier's lane budget; remove the key",
    ),
    (
        "transparent.data.idle_timeout",
        "a claim's lanes are pinned: they are established when the claim starts and kept, so there \
         is no shrink clock to write; remove the key",
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

        // The control channel's transport rule belongs to loading, not to
        // running: a file that cannot serve is not a loaded configuration, and
        // putting the check here is what makes every consumer of `from_str`
        // agree with the daemon — the fixtures, the doc examples, and a future
        // reader that parses a config to describe it.
        #[cfg(feature = "noise")]
        Config::validate_noise_material(&config)?;

        if config.transparent.is_some() && !transparent_client_supported() {
            bail!(
                "`[transparent]` carries whole IP packets through a TUN device, and {}",
                transparent_platform_reason(true)
            );
        }
        if let Some(server) = config.server.as_ref()
            && server.transparent.is_some()
            && !transparent_server_supported()
        {
            bail!(
                "`[server.transparent]` carries whole IP packets through a TUN device, and {}",
                transparent_platform_reason(false)
            );
        }

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
            #[cfg(feature = "carrier")]
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

    #[cfg(feature = "noise")]
    /// Validate every Noise key the configuration carries.
    ///
    /// The handshake is fixed KK, so each side needs its own private key and
    /// the peer's public key, both base64 32-byte Curve25519 values. Checking
    /// here — rather than at the first handshake — is what turns a typo into a
    /// start-up refusal that names the block; the control channel is always
    /// encrypted, so a file without usable keys could never serve anyway.
    pub(crate) fn validate_noise_material(config: &Config) -> Result<()> {
        if let Some(server) = &config.server {
            Self::validate_noise_block("[server.noise]", &server.noise)?;
        }
        if let Some(client) = &config.client {
            Self::validate_noise_block("[client.noise]", &client.noise.keys)?;
            for (name, service) in &client.services {
                if let Some(noise) = &service.noise {
                    Self::validate_noise_overrides(
                        &format!("[client.services.{name}.noise]"),
                        noise,
                    )?;
                }
            }
        }
        if let Some(transparent) = &config.transparent {
            Self::validate_noise_block("[transparent.noise]", &transparent.noise.keys)?;
            for (name, claim) in &transparent.claims {
                if let Some(noise) = &claim.noise {
                    Self::validate_noise_overrides(
                        &format!("[transparent.claims.{name}.noise]"),
                        noise,
                    )?;
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "noise")]
    fn validate_noise_block(block: &str, keys: &NoiseConfig) -> Result<()> {
        Self::validate_noise_key(block, "public_key", &keys.public_key)?;
        Self::validate_noise_key(block, "private_key", &keys.private_key)
    }

    #[cfg(feature = "noise")]
    fn validate_noise_overrides(block: &str, noise: &ServiceNoiseConfig) -> Result<()> {
        if let Some(public_key) = noise.public_key.as_deref() {
            Self::validate_noise_key(block, "public_key", public_key)?;
        }
        if let Some(private_key) = &noise.private_key {
            Self::validate_noise_key(block, "private_key", private_key)?;
        }
        Ok(())
    }

    #[cfg(feature = "noise")]
    fn validate_noise_key(block: &str, key: &str, encoded: &str) -> Result<()> {
        #[cfg(feature = "noise")]
        {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .with_context(|| format!("{block}: {key} is not valid base64"))?;
            anyhow::ensure!(
                bytes.len() == 32,
                "{block}: {key} must decode to a 32-byte Curve25519 key, got {} bytes",
                bytes.len()
            );
        }
        #[cfg(not(feature = "noise"))]
        {
            let _ = (block, key, encoded);
        }
        Ok(())
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

        // The status API is off unless asked for; asked for, it is a
        // credential-bearing listener, and half a credential is worse than
        // none.
        if let Some(status) = &client.status {
            if status.token.is_empty() {
                bail!(
                    "`{}.status.token` must not be empty: the status API \
                     answers without one for nobody",
                    model.block()
                );
            }
            if status.bind_addr.rfind(':').is_none() {
                bail!(
                    "`{}.status.bind_addr` is missing the port: {}",
                    model.block(),
                    status.bind_addr
                );
            }
        }

        #[cfg(feature = "multiplex")]
        Config::validate_data_config(client, model)?;

        // Validate the entries: services in a `[client]` block, claims in a
        // `[transparent]` one. The word follows the model, because that is
        // the table the reader has in front of them.
        let entry = model.entry();
        for (name, s) in &mut client.services {
            s.name.clone_from(name);

            if let Some(addr) = s.remote_addr.as_deref()
                && addr.rfind(':').is_none()
            {
                bail!("{entry} {name}: `remote_addr` is missing the port: {addr}");
            }
            if s.token.as_ref().is_some_and(|t| t.is_empty()) {
                bail!("{entry} {name}: `token` must not be empty");
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
                match s.max_udp_workers {
                    None => s.max_udp_workers = Some(DEFAULT_MAX_UDP_WORKERS),
                    Some(0) => bail!("{entry} {name}: max_udp_workers must be at least 1"),
                    Some(n) => {
                        s.max_udp_workers = Some(n.clamp(1, MAX_UDP_WORKERS_CAP));
                    }
                }
            }
            if s.udp_buffer_size.is_none() {
                s.udp_buffer_size =
                    Some(u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap_or(u16::MAX));
            } else if s.udp_buffer_size == Some(0) {
                bail!("{entry} {name}: udp_buffer_size must be greater than 0");
            }
        }

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
                if !transparent_client_supported() {
                    bail!(
                        "{entry} {name}: `[transparent]` carries whole IP packets through a \
                         TUN device, and {}",
                        transparent_platform_reason(true)
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
                ("max_udp_workers", s.max_udp_workers.is_some()),
                ("udp_buffer_size", s.udp_buffer_size.is_some()),
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
        let data = &client.data;
        let block = model.data_block();

        // The pool's tunnel count. `0` would mean "a pool with no tunnel"; the
        // value is validated rather than clamped so a typo is refused with the
        // block's name in it. A count above the cap is clamped by
        // [`ClientConfig::tunnels`] when the pool is established.
        if data.tunnels == Some(0) {
            bail!("`{block}.tunnels` must be at least 1");
        }
        if let Some(addr) = data.default_data_addr.as_deref()
            && addr.rfind(':').is_none()
        {
            bail!("{block}.default_data_addr is missing the port: {addr}");
        }
        let _ = client;
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
    #[cfg(feature = "carrier")]
    fn validate_claim_lanes(transparent: &TransparentClientConfig) -> Result<()> {
        {
            let claims = transparent.claims();
            if claims == 0 {
                return Ok(());
            }
            let block = "[transparent.data]";
            let Some(written) = transparent.written_budget() else {
                return Ok(());
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
                     {claims} claim(s) draw on it, and a claim's lane is a connection of its own. \
                     Write `{block}.tunnels = {claims}` or more"
                );
            }
        }
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

/// The KK key pairs the configs these tests build use.
///
/// The control channel is always KK, so a config a test builds by hand needs
/// both sides' keys: each host's private key and the peer's public key. These
/// are test keys, generated once and published here because a test has to be
/// reproducible; a deployment generates its own with molehill --genkey.
/// The client pair was generated with openssl genpkey -algorithm X25519.
#[cfg(test)]
pub(crate) const TEST_SERVER_PRIVATE: &str = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=";
#[cfg(test)]
pub(crate) const TEST_SERVER_PUBLIC: &str = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=";
#[cfg(test)]
pub(crate) const TEST_CLIENT_PRIVATE: &str = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=";
#[cfg(test)]
pub(crate) const TEST_CLIENT_PUBLIC: &str = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=";

/// config with the KK blocks appended.
///
/// Which block is added follows the config's own shape: [server.noise] when
/// it carries a [server], [client.noise] for a [client], and
/// [transparent.noise] for the L3 table. A test that asserts on the key
/// rules itself does not call this.
#[cfg(test)]
pub(crate) fn with_noise(config: impl AsRef<str>) -> String {
    let config = config.as_ref();
    let mut out = config.trim_end().to_owned();
    if config.contains("[server]") && !config.contains("[server.noise]") {
        out.push_str("\n\n[server.noise]\npublic_key = \"");
        out.push_str(TEST_CLIENT_PUBLIC);
        out.push_str("\"\nprivate_key = \"");
        out.push_str(TEST_SERVER_PRIVATE);
        out.push_str("\"\n");
    }
    if config.contains("[client]") && !config.contains("[client.noise]") {
        out.push_str("\n\n[client.noise]\npublic_key = \"");
        out.push_str(TEST_SERVER_PUBLIC);
        out.push_str("\"\nprivate_key = \"");
        out.push_str(TEST_CLIENT_PRIVATE);
        out.push_str("\"\n");
    }
    if config.contains("[transparent]") && !config.contains("[transparent.noise]") {
        out.push_str("\n\n[transparent.noise]\npublic_key = \"");
        out.push_str(TEST_SERVER_PUBLIC);
        out.push_str("\"\nprivate_key = \"");
        out.push_str(TEST_CLIENT_PRIVATE);
        out.push_str("\"\n");
    }
    out
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
    use crate::transport::{DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_SECS};
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
                let available = cfg!(feature = "transparent");
                if !available {
                    println!("  skip {name}: needs the `transparent` feature");
                }
                available
            }
            "transparent-client" => {
                let available = transparent_client_supported();
                if !available {
                    println!(
                        "  skip {name}: needs a Linux/macOS build with the `transparent` feature"
                    );
                }
                available
            }
            "transparent-server" => {
                let available = transparent_server_supported();
                if !available {
                    println!(
                        "  skip {name}: needs a Linux/macOS/Windows build with the `transparent` feature"
                    );
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
    /// Stand in for the `<…>` placeholders a prose example uses for an
    /// address the reader has to supply.
    ///
    /// Deployment's L3 recipe writes `<public-ip>` and `<server-address>`,
    /// which is right for a reader and unparsable for the loader. A block is
    /// only an example if the reader's own values work, so the check fills
    /// them in rather than skipping the block — a config that stopped
    /// *parsing* is exactly the drift this test exists to catch, and a
    /// placeholder is the one part of it that is not.
    #[cfg(feature = "multiplex")]
    fn doc_placeholders(block: &str) -> String {
        block
            .replace("<server-address>", "203.0.113.5")
            .replace("<public-ip>", "198.51.100.7")
    }

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
                    blocks.push(doc_placeholders(&cur));
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
        const DOC_FILES: [&str; 6] = [
            "docs/configuration.md",
            "docs/configuration.zh.md",
            "docs/deployment.md",
            "docs/deployment.zh.md",
            "README.md",
            "README.zh.md",
        ];
        let mut total = 0;
        for path in DOC_FILES {
            let blocks = doc_toml_blocks(path)?;
            assert!(!blocks.is_empty(), "no toml code blocks found in {path}");
            for b in &blocks {
                // An L3 client example can only be validated where the
                // client is supported. Elsewhere the refusal is the contract,
                // so the block is skipped rather than read as drift — every
                // other block on the page is still checked.
                if (!transparent_client_supported() && b.contains("[transparent]"))
                    || (!transparent_server_supported() && b.contains("[server.transparent]"))
                {
                    continue;
                }
                Config::from_str(b).map_err(|e| anyhow::anyhow!("{path}: {e:#}"))?;
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
            crate::common::constants::heartbeat_interval_for(0),
            30,
            "server heartbeats"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_MAX_UDP_WORKERS),
            2,
            "max_udp_workers"
        );
        assert_eq!(
            std::hint::black_box(MAX_UDP_WORKERS_CAP),
            64,
            "the max_udp_workers clamp"
        );
        assert_eq!(
            std::hint::black_box(DEFAULT_UDP_BUFFER_SIZE),
            2048,
            "udp_buffer_size"
        );
        assert_eq!(
            std::hint::black_box(crate::common::constants::UDP_QUEUE_SIZE),
            1024,
            "the UDP queue depth"
        );
        assert_eq!(
            crate::common::constants::udp_lifetime(std::time::Duration::ZERO),
            std::time::Duration::from_secs(30),
            "the UDP mapping lifetime floor"
        );
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
    fn test_max_tunnels_defaults_to_the_fd_derived_cap() {
        let config = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        let cfg = Config::from_str(&with_noise(config)).unwrap();
        assert_eq!(
            cfg.server.unwrap().max_tunnels(),
            crate::common::constants::max_tunnels_for(crate::common::constants::soft_nofile()),
            "an unwritten valve follows the open-file limit"
        );

        for (written, expected) in [("0", 0usize), ("6", 6usize)] {
            let config = format!(
                r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"

[server.data]
max_tunnels = {written}
"#
            );
            let cfg = Config::from_str(&with_noise(config)).unwrap();
            assert_eq!(
                cfg.server.unwrap().max_tunnels(),
                expected,
                "a written valve is the cap, including 0 = unlimited"
            );
        }
    }

    /// `[server.transparent]` is opt-in, and the *table's presence* is the
    /// switch: a server that never mentions it parses to `None` rather than to
    /// a default device it would then be willing to attach. Inside the table
    /// the device name keeps its default, because writing the table already is
    /// the operator's decision.
    #[cfg(feature = "transparent")]
    #[test]
    fn test_server_transparent_is_opt_in() {
        let without = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"
"#;
        let cfg = Config::from_str(&with_noise(without)).unwrap();
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
        let cfg = Config::from_str(&with_noise(with)).unwrap();
        assert_eq!(
            cfg.server.unwrap().transparent.map(|t| t.tun).as_deref(),
            Some(default_tun_name().as_str()),
            "writing the table is the decision; the device name still defaults"
        );
    }

    /// Windows can serve transparent clients but cannot be one: the client's
    /// replies need source-address policy routing, which Windows does not
    /// expose. The refusal names the supported alternative rather than saying
    /// "unsupported platform".
    #[cfg(mh_windows)]
    #[test]
    fn a_windows_transparent_client_is_refused_with_the_wsl2_path() {
        let config = r#"
[transparent]
default_token = "t"
tun = "utun0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#;
        let err = Config::from_str(&with_noise(config)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("WSL2"), "{msg}");
        assert!(msg.contains("source-address policy routing"), "{msg}");
    }

    #[cfg(mh_windows)]
    #[test]
    fn a_windows_server_can_serve_transparent_clients() {
        let config = r#"
[server]
default_token = "t"

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transparent]
tun = "molehill0"
"#;
        let cfg = Config::from_str(&with_noise(config)).unwrap();
        assert_eq!(
            cfg.server.unwrap().transparent.map(|t| t.tun).as_deref(),
            Some("molehill0")
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
        assert_eq!(s.max_udp_workers, Some(DEFAULT_MAX_UDP_WORKERS));
        assert_eq!(
            s.udp_buffer_size,
            Some(u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap())
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
max_udp_workers = 4
udp_buffer_size = 65535
"#;
        let cfg = Config::from_str(&with_noise(config)).unwrap();
        let s = &cfg.client.unwrap().services["test"];
        assert_eq!(s.max_udp_workers, Some(4));
        assert_eq!(s.udp_buffer_size, Some(65535));

        // Zero values are rejected
        let bad = config.replace("udp_buffer_size = 65535", "udp_buffer_size = 0");
        assert!(Config::from_str(&bad).is_err());

        // So is a ceiling of zero, and one above the cap is clamped.
        let bad = config.replace("max_udp_workers = 4", "max_udp_workers = 0");
        assert!(Config::from_str(&bad).is_err());
        let clamped = config.replace("max_udp_workers = 4", "max_udp_workers = 1000");
        let cfg = Config::from_str(&with_noise(clamped)).unwrap();
        assert_eq!(
            cfg.client.unwrap().services["test"].max_udp_workers,
            Some(MAX_UDP_WORKERS_CAP)
        );
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
        for key in ["max_udp_workers = 4", "udp_buffer_size = 2048"] {
            let bad = config.replace("{key}", key);
            let err = Config::from_str(&with_noise(&bad)).unwrap_err();
            let msg = format!("{err:#}");
            let name = key.split(' ').next().unwrap();
            assert!(
                msg.contains(name) && msg.contains("UDP"),
                "{key} must be refused by name and protocol, got: {msg}"
            );
        }

        // The same keys on a UDP service stay valid.
        let ok = config
            .replace("{key}", "protocol = \"udp\"\nmax_udp_workers = 4")
            .replace("127.0.0.1:80", "127.0.0.1:53");
        if let Err(e) = Config::from_str(&with_noise(ok)) {
            panic!("the UDP equivalents must parse: {e:#}");
        }
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
        let cfg = Config::from_str(&with_noise(config)).unwrap();
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
        let cfg = Config::from_str(&with_noise(config)).unwrap();
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
    fn test_the_noise_pattern_is_fixed() {
        // The handshake is fixed KK now: pattern, psk and resume have no home
        // in the noise table, and the strict parse says so by name.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let err = format!("{:#}", Config::from_str(config).unwrap_err());
        assert!(
            err.contains("unknown field") && err.contains("pattern"),
            "the refusal must name the key that has no home: {err}"
        );
    }

    #[cfg(feature = "noise")]
    #[test]
    fn test_noise_keys_must_be_32_bytes() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.noise]
public_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
private_key = "dG9vLXNob3J0"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let err = format!("{:#}", Config::from_str(config).unwrap_err());
        assert!(
            err.contains("[client.noise]")
                && err.contains("private_key")
                && err.contains("32-byte"),
            "the refusal must name the block, the key and the length: {err}"
        );
    }

    #[test]
    fn test_client_noise_is_required() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#;
        let err = format!("{:#}", Config::from_str(config).unwrap_err());
        assert!(
            err.contains("noise"),
            "a client without a key pair cannot serve: {err}"
        );
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
        let err = Config::from_str(&with_noise(config))
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
default_retry_interval = 1

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
nodelay = true
retry_interval = 1

[server]
default_token = "t"
max_pool_size = 16

[server.control]
bind_addr = "0.0.0.0:2333"
"#,
        );
        // The `[client.data]` clause only applies where the section exists;
        // without it the key is not even an unknown field.
        let err = Config::from_str(&with_noise(config))
            .expect_err("a config full of removed keys must not start")
            .to_string();
        for pattern in [
            "client.data.default_count",
            "client.services.*.count",
            "client.services.*.pool_size",
            "client.services.*.heartbeat_timeout",
            "client.services.*.health_check",
            "client.services.*.nodelay",
            "client.control.default_retry_interval",
            "client.services.*.retry_interval",
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
                && err.contains("max_tunnels"),
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
        assert!(Config::from_str(&with_noise(config)).is_ok());
    }

    /// One control session holds one tunnel pool: `[client.data].tunnels` is
    /// its total width, there is no per-service width, and `shared_pool` is
    /// gone because a session is the only pool key.
    #[cfg(feature = "multiplex")]
    #[test]
    fn test_data_tunnels_is_one_session_pool() {
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data]
tunnels = 6

[client.services.a]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.services.b]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
"#;
        let cfg = Config::from_str(&with_noise(config))
            .unwrap()
            .client
            .unwrap();
        assert_eq!(cfg.tunnels(), 6, "the written width is the session's pool");
        assert_eq!(cfg.services.len(), 2, "both services draw on that one pool");

        // Unwritten, the pool is the documented four.
        let unset = config.replace(
            "tunnels = 6
",
            "",
        );
        let cfg = Config::from_str(&with_noise(unset))
            .unwrap()
            .client
            .unwrap();
        assert_eq!(cfg.tunnels(), 4);

        // A per-service width is a removed key, refused with the key to write
        // instead rather than left to `deny_unknown_fields`.
        let per_service = config.replace(
            "[client.services.b]\nlocal_addr = \"127.0.0.1:81\"",
            "[client.services.b]\nlocal_addr = \"127.0.0.1:81\"\ntunnels = 2",
        );
        let err = format!(
            "{:#}",
            Config::from_str(&with_noise(per_service)).unwrap_err()
        );
        assert!(
            err.contains("client.services.*.tunnels") && err.contains("[client.data].tunnels"),
            "the refusal must name the removed key and its replacement: {err}"
        );

        // `shared_pool` is gone the same way: one pool per session is the only
        // shape.
        let shared = config.replace(
            "[client.data]\ntunnels = 6",
            "[client.data]\ntunnels = 6\nshared_pool = true",
        );
        let err = format!("{:#}", Config::from_str(&with_noise(shared)).unwrap_err());
        assert!(
            err.contains("client.data.shared_pool") && err.contains("[client.data].tunnels"),
            "the refusal must name the removed key and its replacement: {err}"
        );
    }

    #[test]
    fn test_per_service_data_enable_and_keys() {
        // The client-wide switch is data_enable; a service may flip it
        // without changing the session it shares.
        let config = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.ssh]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"

[client.services.bulk]
local_addr = "127.0.0.1:81"
remote_bind_addr = "0.0.0.0:6081"
[client.services.bulk.noise]
data_enable = false
"#;
        let cfg = Config::from_str(&with_noise(config)).unwrap();
        let client = cfg.client.unwrap();
        let global = client.noise.data_enable.unwrap_or(true);
        assert!(
            client.services["ssh"].data_enabled_with(global),
            "an unset per-service switch follows the client-wide one"
        );
        assert!(
            !client.services["bulk"].data_enabled_with(global),
            "a service may make its data channels plain"
        );
        assert_eq!(
            client.services["bulk"].noise_keys_with(&client.noise.keys),
            client.noise.keys,
            "flipping data_enable does not change the session's key pair"
        );

        // A service may name another server's pair; the session key follows
        // it, which is what keeps one client on two servers with two keys.
        let per_service_keys = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.enc]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
[client.services.enc.noise]
public_key = "aoRns7OwLrdZUTd8A7dKG/g257V6tp7U7yD8GBSwHhM="
private_key = "SJEjSnPAXtkDx8oP0M5ksDypP8hc92/n1bvh+Wqpi3E="
"#;
        let cfg = Config::from_str(&with_noise(per_service_keys)).unwrap();
        let client = cfg.client.unwrap();
        let keys = client.services["enc"].noise_keys_with(&client.noise.keys);
        assert_eq!(
            keys.public_key, "aoRns7OwLrdZUTd8A7dKG/g257V6tp7U7yD8GBSwHhM=",
            "the per-service public key wins"
        );
        assert_eq!(
            &*keys.private_key, "SJEjSnPAXtkDx8oP0M5ksDypP8hc92/n1bvh+Wqpi3E=",
            "the per-service private key wins"
        );
    }

    #[cfg(feature = "noise")]
    #[test]
    fn test_per_service_noise_key_is_validated() {
        let bad = r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.services.enc]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
[client.services.enc.noise]
public_key = "not-base64"
"#;
        let err = format!("{:#}", Config::from_str(&with_noise(bad)).unwrap_err());
        assert!(
            err.contains("[client.services.enc.noise]") && err.contains("public_key"),
            "the refusal must name the block and the key: {err}"
        );
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
        let cfg = Config::from_str(&with_noise(config)).unwrap();
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

    /// `0` is refused with the key's block named, and a value above the pool
    /// cap is clamped rather than obeyed.
    #[cfg(feature = "multiplex")]
    #[test]
    fn test_data_tunnels_is_validated_and_clamped() {
        let config = |tunnels: u16| {
            format!(
                r#"
[client]
default_token = "t"

[client.control]
default_remote_addr = "example.com:2333"

[client.data]
tunnels = {tunnels}

[client.services.test]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:6080"
"#
            )
        };

        let err = format!(
            "{:#}",
            Config::from_str(&with_noise(config(0))).unwrap_err()
        );
        assert!(
            err.contains("`[client.data].tunnels`") && err.contains("at least 1"),
            "the refusal must name the key: {err}"
        );

        let cfg = Config::from_str(&with_noise(config(u16::MAX))).unwrap();
        assert_eq!(
            cfg.client.unwrap().tunnels(),
            usize::from(MAX_MUX_TUNNELS_CAP),
            "a width above the cap is clamped to it, never dialed"
        );
    }

    /// A lane budget is a count like any other: above the resource guard it is
    /// refused with the bound named, never clamped. Clamping is the *pool*
    /// ceiling's behaviour, and a claim's lanes are connections rather than
    /// streams, so the two are deliberately different keys with different
    /// bounds.
    #[cfg(all(feature = "carrier", feature = "transparent", not(mh_windows)))]
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

[transparent.data]
tunnels = {tunnels}

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#
            )
        };

        let over = format!(
            "{:#}",
            Config::from_str(&with_noise(config(MAX_TRANSPARENT_LANES + 1))).unwrap_err()
        );
        assert!(
            over.contains("[transparent.data].tunnels")
                && over.contains(&MAX_TRANSPARENT_LANES.to_string()),
            "the refusal must name the key and the bound: {over}"
        );

        // The guard itself is a budget this client may hold.
        assert!(Config::from_str(&with_noise(config(MAX_TRANSPARENT_LANES))).is_ok());
    }
}
