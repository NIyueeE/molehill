use crate::common::helper::{datagram_len, udp_connect};
#[cfg(feature = "hot-reload")]
use crate::config::ClientServiceChange;
use crate::config::ConfigChange;
use crate::config::{
    ClientConfig, ClientServiceConfig, Config, MaskedString, NoiseConfig, ServiceType,
    TransportConfig, TransportType,
};
use crate::logging::RepeatNotice;
use crate::protocol::Hello::{self, ControlChannelHello};
use crate::protocol::{
    self, Ack, Auth, CURRENT_PROTO_VERSION, ControlChannelCmd, DataChannelCmd, HASH_WIDTH_IN_BYTES,
    LaneId, MAX_UDP_HEADER_LEN, SUPPORTED_PROTO_VERSIONS, ServiceId, ServiceRegistration,
    SessionCmd, SessionRegistration, UdpTraffic, read_ack, read_control_cmd, read_data_cmd,
    read_hello, read_register_result, write_session_cmd, write_stream_prologue,
};
// The mux prologue is written only where multiplexed tunnels exist.
#[cfg(feature = "multiplex")]
use crate::protocol::write_tunnel_prologue;
// The lane *prologue* is what only a transparent build writes; the
// `LaneId` type itself is imported above for every build, because a data
// channel's signature carries `Option<LaneId>` wherever it is compiled.
#[cfg(mh_l3_client)]
use crate::protocol::write_lane_prologue;
#[cfg(mh_l3_client)]
use crate::transparent::Direction;
#[cfg(mh_l3_client)]
use crate::transparent::Endpoint;
#[cfg(mh_l3_client)]
use crate::transparent::hub::TunHub;
#[cfg(mh_l3_client)]
use crate::transparent::lanes::{self, ClaimLoad, LaneLoad};
#[cfg(any(feature = "multiplex", mh_l3_client))]
use crate::transport::carrier::{Carrier, CarrierChoice};
#[cfg(feature = "multiplex")]
use crate::transport::carrier::{KCP_ATTEMPT_BUDGET, declared_carrier};
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
use anyhow::{Context, Result, anyhow, bail};
use backon::BackoffBuilder;
use backon::ExponentialBuilder;
use backon::Retryable;
use bytes::{Bytes, BytesMut};
use rand::TryRng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{
    self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc::error::TrySendError;
#[cfg(feature = "multiplex")]
use tokio::sync::watch;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::time::{self, Duration, Instant};
use tracing::{Instrument, Span, debug, error, info, instrument, trace, warn};

#[cfg(feature = "multiplex")]
use crate::transport::multiplex::{ClientTunnel, Dialer, StreamLease, TunnelPool};

use crate::common::constants::{
    DEFAULT_MAX_UDP_WORKERS, DEFAULT_UDP_BUFFER_SIZE, UDP_CODEL_TARGET, UDP_QUEUE_SIZE,
    backoff_cap, run_control_chan_backoff, udp_lifetime,
};

// The entrypoint of running a client
pub async fn run_client(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
    update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    let config = config.client.ok_or_else(|| {
        anyhow!(
        "Try to run as a client, but the configuration is missing. Please add the `[client]` block"
    )
    })?;

    let mut client = Client::from(config);
    client.run(shutdown_rx, update_rx).await
}

type Nonce = protocol::Digest;

/// Data-plane knobs for one service, resolved from `[client.data]` with the
/// service's own `tunnels` override.
#[derive(Clone, Debug, Default)]
struct DataOpts {
    /// Whether the service multiplexes its data plane onto tunnels. Derived
    /// from the service type rather than configured
    /// ([`ClientServiceConfig::uses_pool`]); only the `multiplex` build has
    /// tunnels, so the field does not exist without it.
    #[cfg(feature = "multiplex")]
    enabled: bool,
    /// Endpoint the data plane dials: the service's own `remote_addr` when
    /// set, else `[client.data].default_data_addr`, else the control
    /// channel's
    /// `remote_addr`.
    addr: String,
    /// The session's single pool width (`[client.data].tunnels`). Every
    /// service of the session draws on this one pool.
    #[cfg(feature = "multiplex")]
    tunnels: usize,
    /// Noise key config, `Some` iff the control transport is `noise`; KCP
    /// tunnels wrap it on top (the crypto stack is kept).
    #[cfg(all(feature = "multiplex", feature = "noise"))]
    noise: Option<NoiseConfig>,
    /// The service's Noise key **pair**, always present when the deployment has
    /// one: the UDP carrier's own handshake authenticates the data port whether or
    /// not the record layer above it is on.
    #[cfg(all(feature = "quic", feature = "noise"))]
    identity: NoiseConfig,
}

/// Placeholder so the channel-opening code keeps one shape without the
/// `multiplex` feature (the data plane is always direct there, and no tunnel
/// can exist).
#[cfg(not(feature = "multiplex"))]
struct Tunnels;

/// A data-channel open the tunnel pool refused. Reported once per process,
/// then DEBUG: the visitor's failure is per-connection, the condition behind it
/// is the operator's to see (the pool at its ceiling, or a repair the server's
/// valve refuses).
#[cfg(feature = "multiplex")]
static OPEN_REFUSED: RepeatNotice = RepeatNotice::new();

/// The client's UDP pin accounting, as the hub sees it. Without tunnels there
/// is nothing to pin a peer to, so the handle is a unit: the plumbing keeps
/// one shape and the feature only decides whether it does anything.
#[cfg(feature = "multiplex")]
type Pins = crate::transport::multiplex::PinRegistry;
#[cfg(not(feature = "multiplex"))]
type Pins = ();

/// One connection dialed by the client, after its v3 transport selector
/// byte: plain TCP, or TCP wrapped in the Noise record stream. The
/// transport is a per-service decision (see `ClientTransport`).
///
/// A `Kcp` variant is the carrier's own byte stream. A KCP *tunnel* is a yamux
/// session whose channels are `TunnelStream`s, so this variant exists for the
/// **direct** data channels, where the carrier's session is the channel.
enum ClientStream {
    Plain(TcpStream),
    #[cfg(feature = "noise")]
    Noise(Box<crate::transport::NoiseStream<TcpStream>>),
    #[cfg(feature = "kcp")]
    Kcp(crate::transport::kcp::KcpTunnelStream),
    /// A direct channel on the UDP carrier: one QUIC connection, one stream,
    /// and the endpoint that carries them (see `transport::quic::channel`).
    #[cfg(feature = "quic")]
    Udp(crate::transport::quic::channel::UdpChannel),
}

impl std::fmt::Debug for ClientStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientStream::Plain(s) => f.debug_tuple("Plain").field(s).finish(),
            #[cfg(feature = "noise")]
            ClientStream::Noise(_) => f.debug_tuple("Noise").finish(),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(s) => f.debug_tuple("Kcp").field(s).finish(),
            #[cfg(feature = "quic")]
            ClientStream::Udp(_) => f.debug_tuple("Udp").finish(),
        }
    }
}

impl ClientStream {
    /// Apply socket options to the underlying TCP socket.
    ///
    /// A KCP stream has no socket of its own: its session's UDP socket is tuned
    /// where it is created (`transport::kcp::tune_socket_buffers`), and the
    /// per-connection TCP options have nothing to land on.
    fn hint(&self, opts: SocketOpts) {
        match self {
            ClientStream::Plain(s) => opts.apply(s),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => opts.apply(s.get_inner()),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(_) => {}
            #[cfg(feature = "quic")]
            ClientStream::Udp(_) => {}
        }
    }
}

impl tokio::io::AsyncRead for ClientStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "quic")]
            ClientStream::Udp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ClientStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "quic")]
            ClientStream::Udp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "quic")]
            ClientStream::Udp(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "kcp")]
            ClientStream::Kcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "quic")]
            ClientStream::Udp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// The client's wire stack, resolved **per service**: the service's
/// `transport.type` override wins over `[client.transport].type`, and the
/// service's `transport.noise` keys win over the global ones.
enum ClientTransport {
    Plain(TcpTransport),
    #[cfg(feature = "noise")]
    // Boxed: the NoiseTransport state dwarfs the plain variant; the enum
    // lives in per-service Arcs and on the stack of dial paths.
    Noise(Box<crate::transport::NoiseTransport>),
}

impl ClientTransport {
    /// The KK pair one service's session authenticates with: the client-wide
    /// pair with the service's own key overrides applied.
    ///
    /// It is the session's identity input beside the endpoint: two services
    /// share a control connection exactly when both match. A service that
    /// only flips `data_enable` keeps the same pair and therefore the same
    /// session — the switch changes its data channels, never the control leg.
    fn noise_keys(client: &ClientConfig, service: &ClientServiceConfig) -> NoiseConfig {
        service.noise_keys_with(&client.noise.keys)
    }

    /// The control channel's stack: always Noise with the effective pair.
    fn control_config(client: &ClientConfig, service: &ClientServiceConfig) -> TransportConfig {
        TransportConfig {
            transport_type: TransportType::Noise,
            noise: Some(Self::noise_keys(client, service)),
        }
    }

    /// The data plane's stack: Noise with the same pair when the service's
    /// effective `data_enable` is true, plain otherwise.
    fn data_config(client: &ClientConfig, service: &ClientServiceConfig) -> TransportConfig {
        let enabled = service.data_enabled_with(client.noise.data_enable.unwrap_or(true));
        TransportConfig {
            transport_type: if enabled {
                TransportType::Noise
            } else {
                TransportType::Plain
            },
            noise: enabled.then(|| Self::noise_keys(client, service)),
        }
    }

    /// Build a wire stack from an effective transport config.
    fn from_config(cfg: &TransportConfig) -> Result<ClientTransport> {
        match cfg.transport_type {
            TransportType::Plain => Ok(ClientTransport::Plain(TcpTransport::new(cfg)?)),
            TransportType::Noise => {
                #[cfg(feature = "noise")]
                {
                    Ok(ClientTransport::Noise(Box::new(
                        crate::transport::NoiseTransport::new(cfg)?,
                    )))
                }
                #[cfg(not(feature = "noise"))]
                {
                    let _ = cfg;
                    Err(anyhow!("This binary was built without the `noise` feature"))
                }
            }
        }
    }

    /// Dial a connection with this transport's selector byte and optional
    /// Noise handshake.
    async fn connect(&self, addr: &AddrMaybeCached) -> Result<ClientStream> {
        match self {
            ClientTransport::Plain(t) => t.connect(addr).await.map(ClientStream::Plain),
            #[cfg(feature = "noise")]
            ClientTransport::Noise(n) => n
                .connect(addr)
                .await
                .map(|s| ClientStream::Noise(Box::new(s))),
        }
    }
}

impl DataOpts {
    /// Resolve the data-plane knobs for one service: `[client.data]` is the
    /// session's pool, and the data endpoint follows the service's own server:
    /// its `remote_addr` override when set, else
    /// `[client.data].default_data_addr` (or the client-wide control endpoint
    /// when that is unset either).
    fn for_service(c: &ClientConfig, s: &ClientServiceConfig) -> DataOpts {
        DataOpts {
            #[cfg(feature = "multiplex")]
            enabled: s.uses_pool(),
            addr: s.endpoint_with(c.data_addr()).to_owned(),
            #[cfg(feature = "multiplex")]
            tunnels: c.tunnels(),
            #[cfg(all(feature = "multiplex", feature = "noise"))]
            noise: s
                .data_enabled_with(c.noise.data_enable.unwrap_or(true))
                .then(|| ClientTransport::noise_keys(c, s)),
            #[cfg(all(feature = "quic", feature = "noise"))]
            identity: ClientTransport::noise_keys(c, s),
        }
    }
}

/// One service placed on one session: the wire form of everything the
/// session needs to register it, keep it warm and serve its visitors.
struct ServiceSlot {
    /// The id this client allocated for the service on this session. It is
    /// kept for as long as the service lives there, so hot reload can name it
    /// in `SessionCmd::Deregister`.
    id: ServiceId,
    service: ClientServiceConfig,
    /// The service's credential: its own `token` when set, else
    /// `[client].default_token`.
    token: MaskedString,
    /// The wire stack this service's data plane dials with. The control
    /// connection's stack belongs to the session (see [`SessionKey`]).
    transport: Arc<ClientTransport>,
    /// `[client.data]` knobs with the service's own overrides applied.
    data: DataOpts,
    /// Data channels opened before a visitor arrives. A UDP service's
    /// `udp_workers` (its worker set, default 2) are opened as soon as the
    /// registration is accepted — the server shards distinct visitors across
    /// them. A TCP service opens none: one channel per visitor, on demand.
    channels: usize,
    /// What this client's transparent device can take. `None` for every
    /// forwarding service.
    transparent_capabilities: Option<crate::protocol::TransparentCapabilities>,
}

impl ServiceSlot {
    /// The wire form of this service's registration: the registration the
    /// server validates, plus the credential that authorizes it
    /// (`digest(service token ‖ nonce)`).
    ///
    /// The session proved the endpoint's default token; the service proves its
    /// own, which is what keeps one refused service from taking its siblings
    /// down (D2).
    fn registration(&self, nonce: Nonce) -> Result<SessionRegistration> {
        let bind_addr: SocketAddr = self.service.remote_bind_addr.parse().map_err(|_| {
            anyhow!(
                "service {}: invalid `remote_bind_addr`: {:?}",
                self.service.name,
                self.service.remote_bind_addr
            )
        })?;
        let mut concat = Vec::from(self.token.as_bytes());
        concat.extend_from_slice(&nonce);
        Ok(SessionRegistration {
            service_id: self.id,
            auth: protocol::digest(&concat),
            reg: ServiceRegistration {
                name: self.service.name.clone(),
                service_type: self.service.service_type,
                bind_addr,
                // The carrier the client **can** use (KCP where the feature is
                // built in, TCP otherwise), which is what the server needs to
                // know: it opens the listener that carrier needs and validates
                // nothing further. It is deliberately *not* the dial order —
                // the client dials TCP first and falls back to KCP — because a
                // server that opened its UDP listener only for clients that
                // declared it would leave a TCP-first client no fallback.
                carrier: {
                    #[cfg(feature = "multiplex")]
                    {
                        protocol::Carrier::from_data_carrier(declared_carrier())
                    }
                    #[cfg(not(feature = "multiplex"))]
                    {
                        protocol::Carrier::Tcp
                    }
                },
                udp_buffer_size: self
                    .service
                    .udp_buffer_size
                    .unwrap_or_else(|| u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap_or(u16::MAX)),
                max_udp_workers: match self.service.service_type {
                    ServiceType::Udp => self
                        .service
                        .max_udp_workers
                        .unwrap_or(DEFAULT_MAX_UDP_WORKERS),
                    ServiceType::Tcp | ServiceType::Transparent => 0,
                },
                transparent_capabilities: self.transparent_capabilities,
            },
        })
    }
}

/// What the client's other code asks a session to do.
enum SessionRequest {
    /// Register (or re-register) a service on this session.
    Register(Box<ServiceSlot>),
    /// Drop a service: the server releases its public endpoint while the
    /// session keeps serving its siblings.
    Deregister(ServiceId),
    /// Ask the peer to stop using one of a claim's lanes, or to take it back.
    ///
    /// The allocator decides (it is the only thing that reads the evidence) but
    /// does not own the writer, so the ask travels through the same channel a
    /// registration does.
    #[cfg(mh_l3_client)]
    DrainLane {
        service: ServiceId,
        lane: [u8; 4],
        cancel: bool,
    },
}

/// What every tunnel of this process has carried so far, as
/// `(delivered, written)` pairs in pool order.
///
/// **Delivered** is read out of the tunnel; **written** is handed into it. This
/// is the L4 quality signal: a carrier that is slow shows up as a low
/// per-tunnel rate, and backpressure makes both ends observe the same rate — so
/// the deciding end can measure it here, with no peer report. (L3 could not:
/// there the *loss* is on the peer's offering side, which this end never sees.)
///
/// The counters live in each tunnel's own
/// [`TunnelCounters`](crate::transport::multiplex::TunnelCounters) — two
/// relaxed adds per IO poll, no lock — and the pool snapshot carries them, so
/// this reads the same registry every pool is already in. The predecessor was
/// a process-wide `Mutex<HashMap>` the *same* IO poll took on every read and
/// write, which serialized every multiplexed byte behind one lock whether or
/// not anyone had asked for the diagnostic.
///
/// Cumulative for a tunnel's life: a policy compares *windows*, so it takes
/// two snapshots and differences them, exactly as the lane policy does.
#[cfg(feature = "multiplex")]
pub(crate) fn tunnel_bytes() -> Vec<(u64, u64)> {
    crate::transport::multiplex::live_pools()
        .into_iter()
        .flat_map(|pool| pool.bytes)
        .collect()
}

/// Print each tunnel's cumulative bytes once a second, while
/// `MOLEHILL_TUNNEL_STATS` is set.
///
/// The instrument has to be readable in a run to be worth having: it exists to
/// answer one question the L4 work is falsifiable on — **do tunnels on different
/// carriers carry materially different rates?** — which is the same shape the
/// lane counters had (S1 measured before S2 decided).
#[cfg(feature = "multiplex")]
fn spawn_tunnel_stats() {
    if std::env::var_os("MOLEHILL_TUNNEL_STATS").is_none() {
        return;
    }
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let mut counts = tunnel_bytes();
            if counts.is_empty() {
                continue;
            }
            // Pool order, numbered: the line has no tunnel ids (they live in
            // the pool-stats line beside it), but the position is stable
            // between lines of the same run, so a reader can difference two.
            let line: Vec<String> = counts
                .drain(..)
                .enumerate()
                .map(|(i, (delivered, written))| format!("{i}:{delivered}/{written}"))
                .collect();
            tracing::info!(
                "tunnel-stats: delivered/written bytes per tunnel {}",
                line.join(" ")
            );
        }
    });
}

/// One lane's cumulative counters as of the previous tick.
///
/// `(peer offered, peer refused, this end injected)`: the peer's two are what its
/// report carries, and the third is this end's own view of the same lane. A
/// window is the difference between two of these.
#[cfg(mh_l3_client)]
type LaneSample = (u64, u64, u64);

/// What the server said about the drains this client asked for, by lane.
///
/// The dialer cannot see the flows the peer placed on a lane, so its own
/// emptiness is only half the question; this is the other half, and a lane is
/// only retired once it says [`crate::protocol::LANE_DRAIN_READY`].
#[cfg(mh_l3_client)]
#[derive(Debug, Default)]
struct LaneDrainState {
    states: std::sync::Mutex<HashMap<(ServiceId, LaneId), u8>>,
}

#[cfg(mh_l3_client)]
impl LaneDrainState {
    fn set(&self, service: ServiceId, lane: LaneId, state: u8) {
        if let Ok(mut states) = self.states.lock() {
            states.insert((service, lane), state);
        }
    }

    fn take(&self, service: ServiceId, lane: LaneId) -> Option<u8> {
        self.states
            .lock()
            .ok()
            .and_then(|mut states| states.remove(&(service, lane)))
    }
}

/// The identity of one control session: the endpoint and the wire stack it is
/// dialed with.
///
/// Two services share a session exactly when both match, so the normal case —
/// every service of a client on one endpoint with one transport — is a single
/// control connection for the whole client (D1). A service that declares its
/// own transport keeps a connection of its own: one connection cannot be both
/// plain and Noise, and quietly downgrading an encrypted service would be
/// worse than one extra connection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionKey {
    /// `[client.control].default_remote_addr`, or the service's own
    /// `remote_addr`.
    addr: String,
    /// The effective KK pair (the service's key overrides applied). The
    /// control channel is always Noise, so the pair — not a transport type —
    /// is what distinguishes one session from another.
    noise: NoiseConfig,
}

/// A live session, as the client's other code sees it.
struct SessionEntry {
    key: SessionKey,
    handle: ClientSessionHandle,
    /// Source of the service ids handed out on this session.
    next_id: u32,
}

/// Where one service was placed, so hot reload can deregister it.
struct ServiceLocation {
    session: SessionKey,
    id: ServiceId,
}

// Holds the state of a client
struct Client {
    config: ClientConfig,
    /// One session per `(endpoint, transport)`.
    sessions: Vec<SessionEntry>,
    /// Every service this client runs, by name.
    services: HashMap<String, ServiceLocation>,
    /// The status API's ask channels, by the endpoint a session dials: one
    /// sender per session, consumed by the status task's report. An endpoint
    /// without a `[client.status]` block has no entry, and nothing reads the
    /// map.
    status_asks: HashMap<String, mpsc::UnboundedSender<crate::status::SessionAsk>>,
    /// The configured half of the status answer, kept current across hot
    /// reloads. The listener clones a receiver per request; a receiver that
    /// subscribes after a reload still sees the latest list.
    status_configured: tokio::sync::watch::Sender<Vec<crate::status::ConfiguredService>>,
}

impl Client {
    // Create a Client from `[client]` config block. The transport is
    // resolved per service (see `ClientTransport`), so there is nothing
    // to build here.
    fn from(config: ClientConfig) -> Client {
        let (status_configured, _) = tokio::sync::watch::channel(Vec::new());
        Client {
            config,
            sessions: Vec::new(),
            services: HashMap::new(),
            status_asks: HashMap::new(),
            status_configured,
        }
    }

    /// Keep the status listener's configured half aligned with the live service
    /// table. Called after startup and after every hot-reload add or delete;
    /// the watch retains the latest list for a listener that subscribes later.
    fn sync_status_configured(&self) {
        let configured: Vec<crate::status::ConfiguredService> = self
            .config
            .services
            .values()
            .map(|service| crate::status::ConfiguredService {
                name: service.name.clone(),
                kind: crate::status::kind_name(service.service_type),
                remote_addr: service.remote_bind_addr.clone(),
                local_addr: service.local_addr.clone(),
                lanes: (service.service_type == ServiceType::Transparent)
                    .then_some(service.transparent_lanes),
            })
            .collect();
        self.status_configured.send_replace(configured);
    }

    // The entrypoint of Client
    async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        // Every service dials its endpoint on a session the client shares with
        // the others; the session reconnects and re-registers on its own.
        let services: Vec<ClientServiceConfig> = self.config.services.values().cloned().collect();
        for config in services {
            self.add_service(config)?;
        }

        self.sync_status_configured();

        // The optional status API: one read-only listener the sessions answer.
        // Off unless `[client.status]` exists; the asks map is empty without
        // it, so spawning the task anyway would only cost a dead select arm —
        // and one condition here reads clearer than one feature flag there.
        let status_task = self.config.status.clone().map(|status| {
            let asks = self.status_asks.clone();
            let shutdown = shutdown_rx.resubscribe();
            let configured = self.status_configured.subscribe();
            tokio::spawn(async move {
                if let Err(e) = crate::status::run(status, shutdown, asks, configured).await {
                    error!("status API stopped: {e:#}");
                }
            })
        });

        // Wait for the shutdown signal
        loop {
            tokio::select! {
                val = shutdown_rx.recv() => {
                    match val {
                        Ok(_) => {}
                        Err(err) => {
                            error!("Unable to listen for shutdown signal: {}", err);
                        }
                    }
                    break;
                },
                e = update_rx.recv() => {
                    if let Some(e) = e {
                        #[cfg(feature = "hot-reload")]
                        self.handle_hot_reload(e);
                        // Without the `notify` feature the config never
                        // changes at runtime; nothing can arrive here.
                        #[cfg(not(feature = "hot-reload"))]
                        warn!("Ignored {e:?} since running as a client");
                    }
                }
            }
        }

        // Shutdown every session: dropping the control connection releases all
        // of that endpoint's public ports on the server.
        for entry in self.sessions.drain(..) {
            entry.handle.shutdown();
        }

        // The status task ends on its own (its select carries the same
        // shutdown signal); keep the handle so the join is not silently
        // detached work at exit.
        if let Some(task) = status_task {
            let _ = task.await;
        }

        Ok(())
    }

    /// The endpoint and wire stack one service dials: its own `remote_addr`
    /// when it declares one, else the client-wide control endpoint.
    fn session_key(&self, cfg: &ClientServiceConfig) -> SessionKey {
        SessionKey {
            addr: cfg
                .endpoint_with(&self.config.control.default_remote_addr)
                .to_owned(),
            noise: ClientTransport::noise_keys(&self.config, cfg),
        }
    }

    /// Start one service: place it on the session that owns its endpoint.
    ///
    /// A service that already runs is re-registered: on the same session it
    /// keeps its id and the server takes the previous endpoint over, on
    /// another endpoint the old registration is dropped first so its public
    /// port is released.
    fn add_service(&mut self, cfg: ClientServiceConfig) -> Result<()> {
        let name = cfg.name.clone();
        let key = self.session_key(&cfg);
        // Two stacks per service: the session's control channel always speaks
        // KK, while its data channels follow the service's effective
        // `data_enable`. They share the pair, never the type.
        let control_transport = Arc::new(ClientTransport::from_config(
            &ClientTransport::control_config(&self.config, &cfg),
        )?);
        let transport = Arc::new(ClientTransport::from_config(
            &ClientTransport::data_config(&self.config, &cfg),
        )?);
        let token = cfg
            .token
            .clone()
            .unwrap_or_else(|| self.config.default_token.clone());
        let data = DataOpts::for_service(&self.config, &cfg);
        // The client opens its configured channels itself: a v4 registration
        // asks the server for none. A UDP service starts one worker — its
        // sharding targets are streams inside the pool, so the pool's width
        // does not have to cover them — and the server asks for more up to
        // `max_udp_workers`; TCP opens none, because the server asks for one
        // channel per visitor; a transparent service opens its **lanes** — one
        // carrier connection per lane, from the carrier's budget — and keeps
        // them, because the claim's inner flows are spread across them and the
        // server asks for a replacement for each lane that ends.
        let channels = match cfg.service_type {
            ServiceType::Tcp => 0,
            // UDP starts one worker; the server asks for another when its
            // queue/affinity state needs one, up to max_udp_workers.
            ServiceType::Udp => 1,
            ServiceType::Transparent => cfg.lanes(),
        };
        // The cap belongs to the session (one reconnect loop), so the service
        // that starts it decides: its own key, else the client-wide one, else
        // the adaptive default (resolved per failure inside the session).
        let retry_backoff_max = cfg
            .retry_backoff_max
            .or(self.config.control.retry_backoff_max);

        // A transparent service's prerequisites are the operator's to satisfy
        // and this process's to verify: on Linux it must already own the
        // address it claims and the kernel must accept injected packets; on
        // macOS the daemon creates the utun and assigns the claimed address. The
        // same call reports what the device can take, which is what the
        // registration declares to the server.
        let transparent_capabilities = if cfg.service_type == ServiceType::Transparent {
            Some(check_transparent_client(&cfg)?)
        } else {
            None
        };

        let previous = self.services.remove(&name);
        if let Some(loc) = &previous
            && loc.session != key
        {
            // The service moved to another server: its public endpoint on the
            // old one has to go.
            if let Some(old) = self.sessions.iter().find(|s| s.key == loc.session) {
                old.handle.deregister(loc.id);
            }
        }

        let idx = self.session_index(key.clone(), control_transport, retry_backoff_max);
        let entry = &mut self.sessions[idx];
        let id = match &previous {
            Some(loc) if loc.session == key => loc.id,
            _ => {
                let id = ServiceId::new(entry.next_id);
                entry.next_id += 1;
                id
            }
        };
        info!("Starting service {name}");
        entry.handle.register(Box::new(ServiceSlot {
            id,
            service: cfg,
            token,
            transport,
            data,
            channels,
            transparent_capabilities,
        }));
        self.services
            .insert(name, ServiceLocation { session: key, id });
        // A service that moved away may have left its old session empty.
        self.reap_idle_sessions();
        Ok(())
    }

    /// Stop every session no service dials any more.
    ///
    /// A session outlives its services by design (its siblings use it), but a
    /// service that hot-reloads to another endpoint, or is deleted, must not
    /// leave an authenticated connection — and its server-side registrations —
    /// behind for the client's whole lifetime.
    fn reap_idle_sessions(&mut self) {
        let live: Vec<SessionKey> = self.services.values().map(|l| l.session.clone()).collect();
        let mut i = 0;
        while i < self.sessions.len() {
            if live.contains(&self.sessions[i].key) {
                i += 1;
                continue;
            }
            let entry = self.sessions.remove(i);
            debug!(
                "Session {} carries no services any more, stopping it",
                entry.key.addr
            );
            entry.handle.shutdown();
        }
    }

    /// The index of the session that owns `key`, starting it on first use.
    fn session_index(
        &mut self,
        key: SessionKey,
        transport: Arc<ClientTransport>,
        retry_backoff_max: Option<u64>,
    ) -> usize {
        if let Some(i) = self.sessions.iter().position(|s| s.key == key) {
            return i;
        }
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (ask_tx, ask_rx) = mpsc::unbounded_channel::<crate::status::SessionAsk>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let session = ClientSession {
            remote_addr: key.addr.clone(),
            transport,
            token: self.config.default_token.clone(),
            default_heartbeat_timeout: self.config.control.default_heartbeat_timeout,
            req_rx,
            ask_rx,
            shutdown_rx,
            services: HashMap::new(),
            pending_drops: Vec::new(),
            #[cfg(feature = "multiplex")]
            pools: HashMap::new(),
            #[cfg(feature = "multiplex")]
            pins: Arc::new(crate::transport::multiplex::PinRegistry::new()),
            #[cfg(mh_l3_client)]
            lane_pool: None,
            #[cfg(mh_l3_client)]
            req_tx: req_tx.clone(),
            #[cfg(mh_l3_client)]
            lane_drains: Arc::new(LaneDrainState::default()),
            #[cfg(any(feature = "multiplex", mh_l3_client))]
            carriers: HashMap::new(),
        };
        tokio::spawn(
            async move {
                session.drive(retry_backoff_max).await;
            }
            .instrument(Span::current()),
        );
        let endpoint = key.addr.clone();
        self.sessions.push(SessionEntry {
            key,
            handle: ClientSessionHandle {
                req_tx,
                shutdown_tx,
            },
            next_id: 0,
        });
        // The status task asks this session for its view by endpoint; a
        // session that never answers (reconnecting) is a gap in one report,
        // not an error.
        self.status_asks.insert(endpoint, ask_tx);
        self.sessions.len() - 1
    }

    /// Apply one client-service config change (add or remove a service).
    #[cfg(feature = "hot-reload")]
    fn handle_hot_reload(&mut self, e: ConfigChange) {
        match e {
            ConfigChange::ClientChange(client_change) => match *client_change {
                ClientServiceChange::Add(cfg) => {
                    let name = cfg.name.clone();
                    self.config.services.insert(name.clone(), (*cfg).clone());
                    if let Err(e) = self.add_service(*cfg) {
                        warn!("Failed to start service {name}: {e:#}");
                    }
                }
                ClientServiceChange::Delete(s) => {
                    self.config.services.remove(&s);
                    // Deregister from the session that owns the service: the
                    // server releases its public port, the session and the
                    // siblings carry on.
                    if let Some(loc) = self.services.remove(&s)
                        && let Some(session) = self.sessions.iter().find(|e| e.key == loc.session)
                    {
                        session.handle.deregister(loc.id);
                    }
                    self.reap_idle_sessions();
                }
            },
            ignored @ ConfigChange::General(_) => {
                warn!("Ignored {ignored:?} since running as a client");
            }
        }
        self.sync_status_configured();
    }
}

struct RunDataChannelArgs {
    /// The nonce the server issued for the control session: a v4 data channel
    /// authenticates with it, not with the session's auth digest.
    session_nonce: Nonce,
    /// The service this channel is for. Every v4 channel announces it with the
    /// four-byte prologue before the server's forwarding command arrives.
    service_id: ServiceId,
    remote_addr: AddrMaybeCached,
    connector: Arc<ClientTransport>,
    /// The carrier decision for this service's data endpoint, shared with every
    /// other channel that dials it: a **direct** channel asks it which carrier
    /// to use, and reports back what happened (the multiplex path makes the same
    /// decision once, when it establishes its pool).
    #[cfg(any(feature = "multiplex", mh_l3_client))]
    carrier: Arc<CarrierChoice>,
    socket_opts: SocketOpts,
    service: ClientServiceConfig,
    /// Mints this service session's data-channel ids: the key the UDP pin
    /// accounting uses to name the tunnel a peer's route ends on.
    #[cfg(feature = "multiplex")]
    channels: std::sync::atomic::AtomicU64,
    /// Shared UDP hub for the service; `Some` iff this is a UDP service.
    udp: Option<Arc<UdpHub>>,
    /// The service's Noise keys, when its control transport is `noise`: a KCP
    /// data channel carries the same record stream a TCP one does, so it is
    /// wrapped under exactly the same rule.
    #[cfg(all(feature = "multiplex", feature = "noise"))]
    noise: Option<NoiseConfig>,
    /// The service's key **pair**, always present when the deployment has one.
    ///
    /// Not the same thing as `noise` above: that is the record-layer switch, and
    /// the UDP carrier's own handshake runs whatever it says. The identity is
    /// what gives the data port a name — a connection that cannot complete KK
    /// never becomes a channel — so it is read from the control stack, which is
    /// Noise by construction.
    #[cfg(all(feature = "quic", feature = "noise"))]
    identity: Option<NoiseConfig>,
    /// Stripe-group registry of the service session: stripes of one
    /// visitor connection arrive as independent data channels and park
    /// here until the group is complete (see [`crate::stripe`]).
    stripes: Arc<crate::stripe::StripeGroups<ClientDataChannel>>,
    /// Placement state of this service's stripe groups, by group id (D24).
    #[cfg(feature = "multiplex")]
    stripe_placements: StripePlacements,
    /// Mints the *lane* ids this service's transparent claims dial.
    ///
    /// A lane is one physical carrier connection, and it is the unit a carrier
    /// can be compared per — not the hub slot it happens to hold (a slot is
    /// reused by whatever takes it next). The id is named in the lane's
    /// prologue so the server knows which connection it is reading, which is
    /// what will let it report what it received from a lane the client has
    /// already retired.
    #[cfg(mh_l3_client)]
    lane_ids: std::sync::atomic::AtomicU32,
}

#[cfg(mh_l3_client)]
impl RunDataChannelArgs {
    /// The next lane id: unique per service session, and never reused inside
    /// one (a retired lane's id must not be read as its replacement's).
    fn next_lane_id(&self) -> u32 {
        self.lane_ids
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// What the server's `CreateDataChannelForStripe` said about one data channel:
/// which group it belongs to and how many stripes the group has.
///
/// The stripe *index* is deliberately not kept. The gather labels a channel
/// when it arrives (`StartForwardStripedTcp`), so the index a request carries
/// is the slot the sender expects, not a fact about this channel — and the
/// client's placement needs only the group and its K.
#[cfg(feature = "multiplex")]
#[derive(Clone, Copy)]
struct StripeOpen {
    /// The wire's four big-endian group bytes, read back as the `u32` the
    /// server's own group counter and the stripe commands use.
    group: u32,
    /// The group's stripe count, from the request.
    count: u8,
}

/// Placeholder so the open path keeps one shape without the `multiplex`
/// feature: there is no pool for a group's identity to steer, and the request
/// simply opens the channel it asks for.
#[cfg(not(feature = "multiplex"))]
type StripeOpen = ();

/// The client-side bound on tracked stripe groups per service.
///
/// A group's entry lives only while its stripes are being placed (it is dropped
/// as soon as the last one is), so the ordinary case is one entry per *live*
/// gather. The bound is a leak guard for the abnormal one: a gather that dies
/// server-side never places its last stripe, and a service may serve striped
/// visitors for the whole session.
#[cfg(feature = "multiplex")]
const MAX_TRACKED_STRIPE_GROUPS: usize = 64;

/// The placement state of one service's stripe groups (D24 structural).
///
/// The server names the group in every `CreateDataChannelForStripe`, so the
/// client knows what the placement rule alone never could: which of its opens
/// belong together. It remembers the tunnels a group's stripes already took and
/// hands the list to the pool, which avoids them — so the group's K data
/// channels land on K distinct tunnels while the pool has that many and share
/// them when it does not. Without it, K requests were K indistinguishable opens
/// and the whole group stacked on one tunnel.
///
/// The map is per service (`RunDataChannelArgs` is one service's), so the group
/// id alone is the key: a group belongs to one service of one session.
#[cfg(feature = "multiplex")]
#[derive(Default)]
struct StripePlacements {
    groups: std::sync::Mutex<HashMap<u32, Arc<StripePlacement>>>,
}

#[cfg(feature = "multiplex")]
impl StripePlacements {
    /// The state of one group, created on its first stripe request.
    fn entry(&self, group: u32, count: u8) -> Arc<StripePlacement> {
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(known) = groups.get(&group) {
            return Arc::clone(known);
        }
        // Bounded: the oldest entry is the one least likely to still be placing
        // stripes, and evicting it only narrows a later stripe's choice (it
        // falls back to the ordinary least-loaded rule) — never correctness.
        if groups.len() >= MAX_TRACKED_STRIPE_GROUPS
            && let Some(oldest) = groups
                .iter()
                .min_by_key(|(_, place)| place.started)
                .map(|(id, _)| *id)
        {
            groups.remove(&oldest);
        }
        let place = Arc::new(StripePlacement::new(count));
        groups.insert(group, Arc::clone(&place));
        place
    }

    /// Forget one group whose stripes are all placed, so the map tracks only
    /// live gathers.
    ///
    /// Only the entry this placement came from is removed: a request that
    /// arrived after an eviction created a fresh one, and dropping *that* would
    /// throw away the tunnels its stripes already took.
    fn finish(&self, group: u32, placed: &Arc<StripePlacement>) {
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if groups
            .get(&group)
            .is_some_and(|known| Arc::ptr_eq(known, placed))
        {
            groups.remove(&group);
        }
    }
}

/// One group's placement state: the tunnels its stripes took, and how many
/// stripes are placed.
#[cfg(feature = "multiplex")]
struct StripePlacement {
    /// The group's stripe count, from the server's request.
    count: u8,
    /// The tunnels this group's stripes were placed on, in placement order.
    used: std::sync::Mutex<Vec<usize>>,
    /// Stripes placed so far.
    placed: std::sync::atomic::AtomicU8,
    /// When the entry was created: the eviction order of the bounded map.
    started: Instant,
}

#[cfg(feature = "multiplex")]
impl StripePlacement {
    fn new(count: u8) -> Self {
        Self {
            count,
            used: std::sync::Mutex::new(Vec::new()),
            placed: std::sync::atomic::AtomicU8::new(0),
            started: Instant::now(),
        }
    }

    /// The tunnels the group already occupies.
    fn used(&self) -> Vec<usize> {
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record one placed stripe and report whether the group is now complete.
    ///
    /// The read above and this append are not one atomic step, and they do not
    /// have to be: the group's stripes are placed concurrently, and the pool's
    /// reservation is charged before its first `await`, so a stripe that missed
    /// a sibling's tunnel here is still steered onto a free one by the
    /// placement rule. This list is what makes the exclusion explicit and
    /// carries it to the stripes that arrive later.
    fn record(&self, tunnel: usize) -> bool {
        {
            let mut used = self
                .used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !used.contains(&tunnel) {
                used.push(tunnel);
            }
        }
        self.placed.fetch_add(1, Ordering::Relaxed) + 1 >= self.count
    }
}

/// The service session's stripe-group registry type. The registry is
/// transport-agnostic: a stripe may be a yamux stream or a direct data
/// channel, so the type exists in every client build.
type StripeRegistry = crate::stripe::StripeGroups<ClientDataChannel>;

impl RunDataChannelArgs {
    /// The session's stripe-group registry.
    fn stripes(&self) -> &StripeRegistry {
        &self.stripes
    }
}

/// Dial the carrier a **direct** data channel rides.
///
/// TCP goes through the service's transport (`ClientTransport`, so Noise is
/// applied where the control channel's wire stack says it should be) with the
/// usual retry: a visitor may arrive while the server is briefly unreachable.
/// KCP opens a session on the data endpoint directly — the carrier owns its own
/// socket stack — and its reliability makes the retry loop meaningless: a lost
/// datagram is KCP's business, and there is no connect error to retry on.
async fn connect_direct_channel(args: &RunDataChannelArgs) -> Result<ClientStream> {
    // KCP when this endpoint's choice names it — after a TCP failure put it
    // there, or when its re-probe window has opened: an attempt is bounded, and
    // a path that cannot bring a session up in that budget is one where UDP is
    // going nowhere, so the fallback turns that into a delay on this channel
    // rather than a data plane that never comes up. TCP, the first carrier,
    // never enters this block.
    // The UDP carrier. A lane is one connection carrying one stream, and the
    // handshake that names the data port is the same KK pair the control channel
    // uses: data_enable decides whether the stream carries the record layer, never
    // whether the connection is authenticated. A build with quic takes this
    // branch; a build without it keeps the KCP implementation in the same carrier
    // slot, so the two never race for it.
    #[cfg(all(feature = "quic", feature = "noise"))]
    if args.carrier.preferred() == Carrier::Kcp
        && let Some(stream) = connect_udp_carrier(args).await?
    {
        return Ok(stream);
    }

    #[cfg(all(feature = "multiplex", feature = "kcp", not(feature = "quic")))]
    if args.carrier.preferred() == Carrier::Kcp {
        let remote_addr = {
            let mut probe = args.remote_addr.clone();
            probe
                .resolve()
                .await
                .with_context(|| format!("Failed to resolve the data endpoint {}", probe.addr))?;
            probe
                .socket_addr
                .ok_or_else(|| anyhow!("The data endpoint {} resolved to nothing", probe.addr))?
        };
        #[cfg(feature = "noise")]
        let noise = args.noise.as_ref();
        #[cfg(not(feature = "noise"))]
        let noise = None;
        match tokio::time::timeout(
            KCP_ATTEMPT_BUDGET,
            crate::transport::kcp::connect_stream(remote_addr, noise),
        )
        .await
        {
            Ok(Ok(stream)) => {
                args.carrier.succeeded(Carrier::Kcp);
                return Ok(ClientStream::Kcp(stream));
            }
            Ok(Err(e)) => {
                if args
                    .carrier
                    .failed(Carrier::Kcp, &format!("{e:#}"))
                    .is_none()
                {
                    return Err(e).context("the KCP session failed and the carrier is forced");
                }
            }
            Err(_) => {
                if args
                    .carrier
                    .failed(Carrier::Kcp, "the KCP session did not come up in time")
                    .is_none()
                {
                    bail!("the KCP session did not come up in time, and the carrier is forced");
                }
            }
        }
    }

    // Retry at least every 100ms, at most for 10 seconds
    let backoff = ExponentialBuilder::default()
        .with_max_delay(Duration::from_millis(100))
        .with_total_delay(Some(Duration::from_secs(10)));

    let conn = (|| async {
        args.connector
            .connect(&args.remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", args.remote_addr))
    })
    .retry(backoff)
    .notify(|e: &anyhow::Error, duration| {
        // Per data channel: a visitor arrived while the server was briefly
        // unreachable. The control channel reports the outage that matters.
        debug!("{:#}. Retry in {:?}", e, duration);
    })
    .await?;
    // TCP carried it: a re-probe that came up here is the news the choice
    // keeps, and a fallback's cooldown is over (see `CarrierChoice::succeeded`).
    // The choice only exists where a second carrier could: a build with neither
    // `multiplex` nor the L3 client has one carrier and no decision to record.
    #[cfg(any(feature = "multiplex", mh_l3_client))]
    args.carrier.succeeded(Carrier::Tcp);
    Ok(conn)
}

/// Dial one direct channel through the UDP carrier.
///
/// Split out of connect_direct_channel so each carrier keeps its own
/// bookkeeping and both bodies fit the lint room.
#[cfg(all(feature = "quic", feature = "noise"))]
async fn connect_udp_carrier(args: &RunDataChannelArgs) -> Result<Option<ClientStream>> {
    let remote_addr = {
        let mut probe = args.remote_addr.clone();
        probe
            .resolve()
            .await
            .with_context(|| format!("Failed to resolve the data endpoint {}", probe.addr))?;
        probe
            .socket_addr
            .ok_or_else(|| anyhow!("The data endpoint {} resolved to nothing", probe.addr))?
    };
    let identity = args
        .identity
        .as_ref()
        .ok_or_else(|| anyhow!("The UDP carrier has no Noise identity for this service"))?;
    let keys = crate::transport::NoiseKeys::from_config(identity)?;
    match tokio::time::timeout(
        KCP_ATTEMPT_BUDGET,
        crate::transport::quic::channel::connect(remote_addr, keys, args.noise.is_some()),
    )
    .await
    {
        Ok(Ok(channel)) => {
            args.carrier.succeeded(Carrier::Kcp);
            Ok(Some(ClientStream::Udp(channel)))
        }
        Ok(Err(e)) => {
            if args
                .carrier
                .failed(Carrier::Kcp, &format!("{e:#}"))
                .is_none()
            {
                return Err(e).context("the UDP carrier failed and the carrier is forced");
            }
            Ok(None)
        }
        Err(_) => {
            if args
                .carrier
                .failed(Carrier::Kcp, "the UDP carrier did not come up in time")
                .is_none()
            {
                bail!("the UDP carrier did not come up in time, and the carrier is forced");
            }
            Ok(None)
        }
    }
}

async fn do_data_channel_handshake(
    args: Arc<RunDataChannelArgs>,
) -> Result<(ClientStream, Option<LaneId>)> {
    let mut conn = connect_direct_channel(&args).await?;

    conn.hint(args.socket_opts);

    // Send nonce, then the v4 service prologue: a session carries N services,
    // so the channel names the one it was opened for and the server reads it
    // before anything else can follow.
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, args.session_nonce);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;
    // A transparent lane names itself, so the two ends can talk about the
    // *connection* rather than about a slot: a slot is one end's routing index
    // and is reused by whatever takes it next, which is what made a half-done
    // handover invisible to the policy. The id is kept here as well as sent:
    // the server reports what it offered *this* lane, and a report the dialer
    // cannot match to a lane is a number about nothing. Everything else keeps
    // the 4-byte prologue it has always written.
    #[cfg(mh_l3_client)]
    if args.service.service_type == ServiceType::Transparent {
        let lane = LaneId::new(args.next_lane_id());
        write_lane_prologue(&mut conn, args.service_id, lane).await?;
        return Ok((conn, Some(lane)));
    }
    write_stream_prologue(&mut conn, args.service_id).await?;

    Ok((conn, None))
}

async fn run_data_channel(args: Arc<RunDataChannelArgs>) -> Result<()> {
    // Do the handshake
    let (conn, lane) = do_data_channel_handshake(args.clone()).await?;

    // Forward
    forward_data_channel(
        ClientDataChannel::Raw(conn),
        &args.service,
        args.udp.clone(),
        args.stripes(),
        lane,
    )
    .await
}

/// The established tunnel(s) of one control session, per `[client].tunnel`:
/// a yamux tunnel pool over TCP (arms 0/1) or KCP (arm 2).
#[cfg(feature = "multiplex")]
#[derive(Clone)]
enum Tunnels {
    Yamux(crate::transport::multiplex::TunnelPool),
}

#[cfg(feature = "multiplex")]
impl Tunnels {
    /// Open the next data channel: a stream from the tunnel pool, placed
    /// least-loaded-first over the physical tunnels.
    async fn open_stream(&self) -> Result<TunnelStream> {
        match self {
            Tunnels::Yamux(pool) => pool
                .open_stream()
                .await
                .map(TunnelStream::Yamux)
                .map_err(|e| self.refused(&e)),
        }
    }

    /// Open one stripe's data channel: a stream on a tunnel the group does not
    /// already occupy, growing the pool to the group's K first when it has
    /// fewer (see [`TunnelPool::open_stream_on_distinct`]).
    async fn open_stream_on_distinct(
        &self,
        used: &[usize],
        stripes: usize,
    ) -> Result<TunnelStream> {
        match self {
            Tunnels::Yamux(pool) => pool
                .open_stream_on_distinct(used, stripes)
                .await
                .map(TunnelStream::Yamux)
                .map_err(|e| self.refused(&e)),
        }
    }

    /// The visitor whose open this was fails either way — and a failed request
    /// is DEBUG the way any per-connection failure is. The *condition* behind
    /// it is what an operator must be able to see, though: a pool at its
    /// ceiling, or a growth the server's valve refuses, is a state of the
    /// deployment and not a property of one visitor. So it is reported once per
    /// process and then demoted to the per-connection DEBUG (AGENTS.md §9's log
    /// contract, and the same shape the pool's own refused-growth line uses).
    fn refused(&self, e: &crate::transport::multiplex::OpenError) -> anyhow::Error {
        OPEN_REFUSED.report(
            || info!(pool = %self.pool().key(), "the pool refused a data channel: {e}"),
            || debug!(pool = %self.pool().key(), "the pool refused a data channel: {e}"),
        );
        anyhow!("Failed to open a multiplexed data channel: {e}")
    }

    /// The pool itself, for the session that owns it: sharing it with another
    /// service, or reading its state.
    fn pool(&self) -> &TunnelPool {
        match self {
            Tunnels::Yamux(pool) => pool,
        }
    }
}

/// One opened data channel over the arm's tunnel transport.
///
/// The stream is the pool's [`StreamLease`], so the tunnel's stream count is
/// charged for exactly as long as the channel lives.
#[cfg(feature = "multiplex")]
enum TunnelStream {
    Yamux(StreamLease),
}

#[cfg(feature = "multiplex")]
impl TunnelStream {
    /// The tunnel this channel's stream is charged to: the UDP pin
    /// accounting's key (D30).
    fn tunnel_id(&self) -> usize {
        match self {
            TunnelStream::Yamux(lease) => lease.tunnel_id(),
        }
    }

    /// Record what one IO poll moved across this channel's tunnel (see
    /// [`StreamLease::count_bytes`]): the counters are the tunnel's own, so
    /// the poll pays two relaxed adds and no lock.
    fn count_bytes(&self, delivered: u64, written: u64) {
        match self {
            TunnelStream::Yamux(lease) => lease.count_bytes(delivered, written),
        }
    }
}

#[cfg(feature = "multiplex")]
impl tokio::io::AsyncRead for TunnelStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Yamux(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(feature = "multiplex")]
impl tokio::io::AsyncWrite for TunnelStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TunnelStream::Yamux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Yamux(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Yamux(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Run a data channel as one stream of the multiplexed tunnels.
#[cfg(feature = "multiplex")]
async fn run_mux_data_channel(
    args: &Arc<RunDataChannelArgs>,
    tunnel: &Tunnels,
    stripe: Option<StripeOpen>,
) -> Result<()> {
    let mut stream = match stripe {
        Some(request) => open_stripe_stream(args, tunnel, request).await?,
        None => tunnel.open_stream().await?,
    };
    trace!("Multiplexed data channel opened");
    // A v4 tunnel belongs to the session, so each of its streams names the
    // service it carries: the server reads these bytes before its forwarding
    // command. Several services may share the tunnel — that is what a shared
    // pool produces — so the routing is per stream. The stream also names the
    // **tunnel** it rides, which is what lets the server say what it offered
    // that connection (see `TunnelId`).
    let tunnel =
        crate::protocol::TunnelId::new(u32::try_from(stream.tunnel_id()).unwrap_or(u32::MAX));
    write_tunnel_prologue(&mut stream, args.service_id, tunnel).await?;
    // The channel belongs to one tunnel now: the client's UDP pin accounting
    // keys on the channel, so a peer's route can move a tunnel's pinned count.
    let channel = args
        .channels
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(udp) = &args.udp {
        udp.bind_channel(channel, stream.tunnel_id());
    }
    let result = forward_data_channel(
        ClientDataChannel::Mux(stream),
        &args.service,
        args.udp.clone(),
        args.stripes(),
        // A stream of a multiplexed tunnel is not a lane: a claim never
        // multiplexes, so nothing here has an id to report or to match.
        None,
    )
    .await;
    if let Some(udp) = &args.udp {
        udp.unbind_channel(channel);
    }
    result
}

/// Open one stripe's stream, on a tunnel the group does not already occupy
/// (D24 structural).
///
/// The group's placement state is taken from this service's map, its tunnel
/// list handed to the pool, and the tunnel the pool chose recorded back — so
/// the group's next stripe (which the server has usually already requested)
/// avoids it. The record's return value is what lets an abandoned entry be
/// dropped instead of tracked for the session's life.
#[cfg(feature = "multiplex")]
async fn open_stripe_stream(
    args: &Arc<RunDataChannelArgs>,
    tunnel: &Tunnels,
    stripe: StripeOpen,
) -> Result<TunnelStream> {
    let placement = args.stripe_placements.entry(stripe.group, stripe.count);
    let used = placement.used();
    let stream = tunnel
        .open_stream_on_distinct(&used, usize::from(stripe.count))
        .await?;
    if placement.record(stream.tunnel_id()) {
        args.stripe_placements.finish(stripe.group, &placement);
    }
    Ok(stream)
}

/// One opened data channel, as one concrete type so a stripe group's
/// registry can park stripes that arrive on either path.
enum ClientDataChannel {
    Raw(ClientStream),
    #[cfg(feature = "multiplex")]
    Mux(TunnelStream),
}

#[cfg(any(feature = "multiplex", mh_l3_client))]
impl ClientDataChannel {
    /// The carrier this channel rides, when the channel *is* the carrier.
    ///
    /// A direct channel is one carrier connection — a plain TCP socket, a Noise
    /// stream over one, or a KCP session — so its own type is the answer, and
    /// that is what a transparent lane needs: the hub records it per slot so the
    /// allocator's carrier policy compares lanes by what they actually ride
    /// rather than by the choice in force when they were dialed. A multiplexed
    /// channel rides a tunnel whose carrier was decided when the pool was
    /// established, which the pool knows and this type does not.
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            clippy::unnecessary_wraps,
            reason = "a direct channel is always a carrier; the multiplex arm can answer None, \
                      so the shared call site keeps one Option shape"
        )
    )]
    fn carrier(&self) -> Option<Carrier> {
        match self {
            // The KCP arm is the feature's: without `kcp` the variant does not
            // exist and every raw channel is a TCP connection, which is the
            // only carrier this build has.
            #[cfg(feature = "kcp")]
            Self::Raw(ClientStream::Kcp(_)) => Some(Carrier::Kcp),
            Self::Raw(_) => Some(Carrier::Tcp),
            #[cfg(feature = "multiplex")]
            Self::Mux(_) => None,
        }
    }
}

impl tokio::io::AsyncRead for ClientDataChannel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientDataChannel::Raw(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "multiplex")]
            ClientDataChannel::Mux(s) => {
                let before = buf.filled().len();
                let ready = std::pin::Pin::new(&mut *s).poll_read(cx, buf);
                if let std::task::Poll::Ready(Ok(())) = ready {
                    let got = buf.filled().len().saturating_sub(before);
                    if got > 0 {
                        s.count_bytes(u64::try_from(got).unwrap_or(u64::MAX), 0);
                    }
                }
                ready
            }
        }
    }
}

impl tokio::io::AsyncWrite for ClientDataChannel {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ClientDataChannel::Raw(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "multiplex")]
            ClientDataChannel::Mux(s) => {
                let ready = std::pin::Pin::new(&mut *s).poll_write(cx, buf);
                if let std::task::Poll::Ready(Ok(n)) = ready
                    && n > 0
                {
                    s.count_bytes(0, u64::try_from(n).unwrap_or(u64::MAX));
                }
                ready
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientDataChannel::Raw(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "multiplex")]
            ClientDataChannel::Mux(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientDataChannel::Raw(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "multiplex")]
            ClientDataChannel::Mux(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Wait for the server's forwarding command and start copying traffic.
///
/// `stripes` is the service session's stripe-group registry: a command that
/// announces a striped channel registers the stream in its group here, and
/// the registrar of the group's last stripe dials the local service once
/// and forwards the whole group (see [`crate::stripe`]).
async fn forward_data_channel(
    mut conn: ClientDataChannel,
    service: &ClientServiceConfig,
    udp_hub: Option<Arc<UdpHub>>,
    stripes: &StripeRegistry,
    #[cfg_attr(
        not(mh_l3_client),
        expect(
            unused_variables,
            reason = "only a transparent claim's lane names itself in its prologue"
        )
    )]
    lane: Option<LaneId>,
) -> Result<()> {
    let sock_opts = SocketOpts::for_service();
    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardUdp => {
            if service.service_type != ServiceType::Udp {
                bail!("Expect UDP traffic. Please check the configuration.")
            }
            let hub = udp_hub
                .ok_or_else(|| anyhow!("Service {} has no UDP forwarding hub", service.name))?;
            run_data_channel_for_udp(conn, hub).await?;
        }
        DataChannelCmd::StartForwardStripedTcp(group_bytes, index, count) => {
            if service.service_type != ServiceType::Tcp {
                bail!("Expect TCP traffic. Please check the configuration.")
            }
            let group = u32::from_be_bytes(group_bytes);
            if let Some(done) =
                stripes.register(group, index, count, conn, &service.local_addr, sock_opts)?
            {
                // The group is complete: one local connection, shared by
                // every stripe of the group.
                debug!("Stripe group {group} complete with {count} stripes");
                let local = TcpStream::connect(&done.local_addr)
                    .await
                    .with_context(|| format!("Failed to connect to {}", done.local_addr))?;
                done.sock_opts.apply(&local);
                let (read, write) = local.into_split();
                // The client does not size groups: no plan, so a stall here
                // reports to nobody.
                crate::stripe::spawn_group(read, write, done.streams, None);
            }
        }
        DataChannelCmd::StartForwardTransparent(wire) => {
            if service.service_type != ServiceType::Transparent {
                bail!("Expect transparent traffic. Please check the configuration.")
            }
            #[cfg(mh_l3_client)]
            run_transparent_channel(conn, service, lane, wire).await?;
            #[cfg(not(mh_l3_client))]
            {
                let _ = (conn, service, wire);
                bail!(
                    "This build cannot carry a transparent service: it needs the `transparent` \
                     feature (which implies `carrier`) and Linux or macOS"
                );
            }
        }
    }
    Ok(())
}

/// Inject what the tunnel sends for this service's claimed endpoint, and hand
/// the kernel's return traffic back.
#[cfg(mh_l3_client)]
async fn run_transparent_channel(
    conn: ClientDataChannel,
    service: &ClientServiceConfig,
    lane: Option<LaneId>,
    wire: crate::protocol::FrameFormat,
) -> Result<()> {
    use crate::transparent::hub::{TunHub, forward_transparent};
    use crate::transparent::{Direction, Endpoint};

    let addr: SocketAddr = service.remote_bind_addr.parse().with_context(|| {
        format!(
            "service {}: invalid `remote_bind_addr`: {:?}",
            service.name, service.remote_bind_addr
        )
    })?;
    // The hub owns the data path's counters, so every channel of the claim —
    // its member set — counts into the same line. This channel becomes one
    // member of that set for as long as it runs; the set's width is what the
    // hub actually sees join, so a replacement inherits its predecessor's slot.
    let hub = TunHub::get_or_spawn(&service.transparent_tun, Direction::Source)?;
    let endpoint = Endpoint::new(addr.ip(), addr.port());
    if wire == crate::protocol::FrameFormat::Vnet && hub.format() != wire {
        bail!(
            "The server selected vnet frames for {}, but this device produces {:?} frames",
            endpoint,
            hub.format()
        );
    }
    hub.set_wire_format(endpoint, wire)?;
    // This end dialed the lane, so it knows which carrier it rides — the fact
    // the allocator's carrier policy compares instead of trusting the choice it
    // made earlier (a lane dialed before a switch rides the old carrier). The
    // lane id it named is handed to the hub for the same reason: the server
    // reports what it offered *that* connection, and a report this end cannot
    // match to a lane is a number about nothing.
    let carrier = conn.carrier();
    forward_transparent(conn, hub, endpoint, lane, carrier, wire).await
}

/// The client's side of a transparent service's contract, checked before the
/// service registers rather than after its first packet disappears.
#[cfg(mh_l3_client)]
fn check_transparent_client(
    cfg: &ClientServiceConfig,
) -> Result<crate::protocol::TransparentCapabilities> {
    use crate::transparent::{Direction, hub::TunHub};

    let addr: SocketAddr = cfg.remote_bind_addr.parse().with_context(|| {
        format!(
            "service {}: invalid `remote_bind_addr`: {:?}",
            cfg.name, cfg.remote_bind_addr
        )
    })?;
    // The device and the claimed address are the part the daemon owns on every
    // platform: Linux creates a missing device and assigns a missing address,
    // while macOS does both through the hub below. Everything around them —
    // the routes, the policy rule and the forwarding switches — stays the
    // operator's.
    #[cfg(mh_linux)]
    {
        crate::transparent::net::prepare_device(&cfg.transparent_tun)
            .with_context(|| format!("service {}: failed to prepare the TUN device", cfg.name))?;
        crate::transparent::net::claim_client(&cfg.transparent_tun, addr).with_context(|| {
            format!(
                "service {}: failed to assign {addr} to the device",
                cfg.name
            )
        })?;
    }
    let hub = TunHub::get_or_spawn(&cfg.transparent_tun, Direction::Source).with_context(|| {
        format!(
            "service {}: transparent prerequisites are not met",
            cfg.name
        )
    })?;
    #[cfg(mh_linux)]
    crate::transparent::check::check_client(&cfg.transparent_tun, &[addr.ip()]).with_context(
        || {
            format!(
                "service {}: transparent prerequisites are not met",
                cfg.name
            )
        },
    )?;
    #[cfg(mh_macos)]
    hub.ensure_claimed(addr.ip())
        .with_context(|| format!("service {}: failed to assign the claimed address", cfg.name))?;
    Ok(crate::protocol::TransparentCapabilities {
        vnet_frames: hub.format() == crate::protocol::FrameFormat::Vnet
            && !crate::transparent::force_ip_wire(),
    })
}

#[cfg(not(mh_l3_client))]
fn check_transparent_client(
    _cfg: &ClientServiceConfig,
) -> Result<crate::protocol::TransparentCapabilities> {
    bail!(
        "This build cannot carry a transparent service: it needs the `transparent` \
         feature (which implies `carrier`) and Linux or macOS"
    );
}

/// Dial an extra connection and upgrade it into a yamux data tunnel.
///
/// The returned sender shuts the driver down when dropped.
#[cfg(feature = "multiplex")]
async fn establish_tunnel(
    transport: &Arc<ClientTransport>,
    remote_addr: &AddrMaybeCached,
    session_nonce: Nonce,
    service_name: &str,
) -> Result<(ClientTunnel, watch::Sender<bool>)> {
    let mut conn = transport
        .connect(remote_addr)
        .await
        .with_context(|| format!("Failed to connect the data tunnel to {remote_addr}"))?;
    conn.hint(SocketOpts::for_control_channel());

    let hello = Hello::DataChannelTunnelHello(CURRENT_PROTO_VERSION, session_nonce);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;

    match read_ack(&mut conn).await? {
        Ack::Ok => {}
        v => bail!("Service {service_name}: the server refused the multiplexed data tunnel: {v}"),
    }

    let config = crate::transport::multiplex::mux_config();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tunnel = ClientTunnel::start(conn, config, shutdown_rx);
    // The per-tunnel byte counters start with the first tunnel, like the KCP
    // adapter's own stats task: no entry point has to know about it, and a
    // run that never multiplexes pays nothing.
    spawn_tunnel_stats();
    Ok((tunnel, shutdown_tx))
}

/// Establish one service's tunnel pool: its configured count, plus the dialer
/// every repair uses.
///
/// All `tunnels` connections are dialed here, at service start, and kept for
/// the service's lifetime; a tunnel that dies is replaced by the pool's repair
/// tick, which is the only establishment after this point. The count is the
/// operator's `[client.data].tunnels` (or the default, `4`): every service of
/// the session draws on it, and no service raises it.
///
/// The carrier is chosen at runtime, not written: TCP first, KCP when a TCP
/// data channel cannot be established, with the decision remembered per data
/// endpoint and re-probed after a cooldown, and the quality rule able to move a
/// KCP-carrying pool back (see [`crate::transport::carrier`]). The pool's
/// identity is `key`: `session` for a shared pool, `service:<name>` otherwise.
///
/// The returned senders shut the tunnel drivers down when dropped; a tunnel
/// the pool *grows* carries its own sender inside the pool, so a shrink can
/// release it.
#[cfg(feature = "multiplex")]
async fn establish_tunnels(
    dial: PoolDial<'_>,
    key: &str,
    pins: Arc<crate::transport::multiplex::PinRegistry>,
    choice: &Arc<CarrierChoice>,
) -> Result<Tunnels> {
    let opts = dial.opts;
    // The pool's tunnels are established here, together, and kept for the
    // session's lifetime: a deployment's capacity is what its configuration
    // says it is, not what it happened to be doing a minute ago. The dialer
    // stays with the pool for repair — a tunnel that dies is replaced so the
    // count survives — which is the only runtime establishment left.
    let target = opts.tunnels;
    // The carrier is decided once, here, and the choice remembers it for the
    // endpoints's other channels: a pool that established on TCP keeps dialing
    // TCP (its repair included), because a pool whose tunnels rode two carriers
    // would be two pools wearing one name.
    let mut carrier = choice.preferred();
    // One fallback, not a rotation: `failed` always answers the other carrier,
    // so a loop that trusted it alone would pay both attempts for ever when
    // neither can be established.
    let mut fell_back = false;
    let (dialer, initial) = loop {
        let attempt = match carrier {
            Carrier::Kcp => establish_kcp_pool(dial, target, key).await,
            Carrier::Tcp => tcp_pool_start(dial, target, key).await,
        };
        let failure = match attempt {
            Ok(started) => {
                // Reached: the choice keeps this carrier (a re-probe that came
                // up is the news; a held quality switch is not overridden —
                // see `CarrierChoice::succeeded`).
                choice.succeeded(carrier);
                break started;
            }
            Err(e) => e,
        };
        // Recorded for the endpoint and answered with the carrier to try
        // instead; `None` means there is nothing left (a forced carrier, or a
        // build with one), and the failure is the pool's.
        match choice.failed(carrier, &format!("{failure:#}")) {
            Some(next) if !fell_back => {
                fell_back = true;
                carrier = next;
            }
            _ => {
                return Err(failure).with_context(|| {
                    format!(
                        "the {} data plane could not be established",
                        carrier.as_str()
                    )
                });
            }
        }
    };
    let established = initial.len();
    info!(
        pool = %key,
        carrier = carrier.as_str(),
        tunnels = target,
        established,
        "pool-stats: tunnel pool established"
    );
    // The pool is told how to ask which carrier to establish *now*: a repair
    // after a fallback comes up on the carrier the endpoint prefers, instead of
    // dialing the one the pool was born on forever.
    let preferred: crate::transport::multiplex::PreferredCarrier = {
        let choice = Arc::clone(choice);
        Arc::new(move || choice.carrier())
    };
    Ok(Tunnels::Yamux(TunnelPool::with_choice(
        carrier,
        key.to_owned(),
        initial,
        target,
        Some(dialer),
        preferred,
        pins,
        Some(Arc::clone(choice)),
    )))
}

/// Dial the pool's tunnels, concurrently, and hand back the ones that came up.
///
/// A partial result is not a failure: the pool repairs what is missing (its
/// dialer is the same closure), and a service that cannot reach *any* tunnel
/// says so on its first open — which is a better answer than refusing to start
/// a service whose server is briefly unreachable.
#[cfg(feature = "multiplex")]
async fn establish_initial(
    dialer: &crate::transport::multiplex::Dialer,
    target: usize,
    key: &str,
    carrier: Carrier,
) -> Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)> {
    let mut dials = tokio::task::JoinSet::new();
    for _ in 0..target {
        let dial = Arc::clone(dialer);
        dials.spawn(async move { dial(carrier).await });
    }
    let mut initial = Vec::with_capacity(target);
    let mut failures = 0usize;
    while let Some(joined) = dials.join_next().await {
        match joined {
            Ok(Ok(tunnel)) => initial.push(tunnel),
            Ok(Err(e)) => {
                failures += 1;
                debug!(pool = %key, carrier = carrier.as_str(), "tunnel establishment failed: {e}");
            }
            Err(e) => {
                failures += 1;
                debug!(pool = %key, carrier = carrier.as_str(), "tunnel establishment panicked: {e}");
            }
        }
    }
    if failures > 0 {
        debug!(
            pool = %key,
            carrier = carrier.as_str(),
            failures,
            "some of the pool's tunnels did not come up; the pool will repair them"
        );
    }
    initial
}

/// The closure a pool calls to add a tunnel.
///
/// KCP tunnels are established by dialing the resolved endpoint directly (the
/// carrier owns its own socket stack), TCP tunnels by the service's transport
/// — so a Noise-wrapped pool keeps its crypto, exactly as the initial
/// establishment did.
#[cfg(feature = "multiplex")]
async fn tunnel_dialer(
    transport: &Arc<ClientTransport>,
    data_addr: &AddrMaybeCached,
    session_nonce: Nonce,
    opts: &DataOpts,
    choice: &Arc<CarrierChoice>,
) -> Result<Dialer> {
    // The KCP arm is the only reader of these; a TCP-only build still keeps
    // the same dialer signature.
    #[cfg(not(feature = "kcp"))]
    let _ = (opts, choice);
    // Both carriers are built, and the pool says which one to use **per call**:
    // a repair asks for the carrier the endpoint prefers *now*, which is what
    // lets a pool that came up on KCP bring its replacement up on TCP after UDP
    // stops working.
    /// One carrier's dial, before the choice of which to call: the composite
    /// below is what the pool holds.
    type OneDialer = std::sync::Arc<
        dyn Fn() -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = crate::transport::multiplex::DialResult>
                        + Send,
                >,
            > + Send
            + Sync,
    >;
    let tcp: OneDialer = {
        {
            let transport = Arc::clone(transport);
            let data_addr = data_addr.clone();
            Arc::new(move || {
                let transport = Arc::clone(&transport);
                let data_addr = data_addr.clone();
                Box::pin(async move {
                    establish_tunnel(&transport, &data_addr, session_nonce, "pool")
                        .await
                        .map_err(|e| format!("{e:#}"))
                })
            })
        }
    };
    #[cfg(feature = "kcp")]
    let kcp: OneDialer = {
        // Resolved once, at pool creation: a KCP growth must not block on
        // DNS inside the placement path.
        let remote_addr = {
            let mut probe = AddrMaybeCached::new(&opts.addr);
            probe.resolve().await.with_context(|| {
                format!("Failed to resolve the KCP data endpoint {}", opts.addr)
            })?;
            probe
                .socket_addr
                .ok_or_else(|| anyhow!("The KCP data endpoint {} resolved to nothing", opts.addr))?
        };
        let opts = opts.clone();
        let choice = Arc::clone(choice);
        Arc::new(move || {
            let opts = opts.clone();
            let choice = Arc::clone(&choice);
            Box::pin(async move {
                let attempt = tokio::time::timeout(
                    KCP_ESTABLISH_TIMEOUT,
                    establish_one_kcp_tunnel(remote_addr, session_nonce, &opts, "pool"),
                )
                .await;
                let result = attempt
                    .map_err(|_| "KCP tunnel establishment timed out".to_owned())?
                    .map_err(|e| format!("{e:#}"));
                if let Err(e) = &result {
                    // A repair that cannot bring KCP up is the same news the
                    // *initial* establishment reports, and it is the only
                    // news the endpoint's choice would otherwise never get:
                    // the pool's dialer is fixed to the carrier the pool was
                    // born on, so without this the client keeps believing a
                    // path that has since stopped carrying UDP is fine —
                    // and every other pool, lane and future session that
                    // dials that endpoint keeps paying for the attempt.
                    choice.failed(Carrier::Kcp, e);
                }
                result
            })
        })
    };
    Ok(Arc::new(move |wanted: Carrier| match wanted {
        Carrier::Tcp => tcp(),
        // A build without the feature has no way to be asked for KCP
        // (`CarrierChoice::failed` has nowhere to fall back to), and TCP is
        // then the only carrier there is.
        #[cfg(feature = "kcp")]
        Carrier::Kcp => kcp(),
        #[cfg(not(feature = "kcp"))]
        Carrier::Kcp => tcp(),
    }))
}

/// Everything one pool's dialing needs, so the functions that build a dialer,
/// start a pool and fall back to TCP pass one thing rather than four.
#[cfg(feature = "multiplex")]
#[derive(Clone, Copy)]
struct PoolDial<'a> {
    transport: &'a Arc<ClientTransport>,
    data_addr: &'a AddrMaybeCached,
    session_nonce: Nonce,
    opts: &'a DataOpts,
    /// The endpoint's carrier decision, so a dialer's own failures are reported
    /// to the one place that knows what the path is doing (see the KCP arm of
    /// [`tunnel_dialer`]).
    choice: &'a Arc<CarrierChoice>,
}

/// A pool's start on KCP: its dialer plus the tunnels that came up inside the
/// attempt budget.
///
/// A session that does not come up in [`KCP_ATTEMPT_BUDGET`] is a path where UDP
/// is going nowhere, so the caller falls back instead of waiting: the budget is
/// what keeps a blocked path a delay on the endpoint rather than a hung service.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
async fn establish_kcp_pool(
    dial: PoolDial<'_>,
    target: usize,
    key: &str,
) -> Result<(
    Dialer,
    Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
)> {
    let dialer = dial.dialer().await?;
    match tokio::time::timeout(
        KCP_ATTEMPT_BUDGET,
        establish_initial(&dialer, target, key, Carrier::Kcp),
    )
    .await
    {
        Ok(established) if !established.is_empty() => Ok((dialer, established)),
        Ok(_) => bail!("no KCP session came up"),
        Err(_) => bail!("KCP establishment timed out"),
    }
}

/// The same start in a build without `kcp`: the endpoint's choice can never
/// name KCP there ([`crate::transport::carrier::first_carrier`] is TCP and
/// [`CarrierChoice::failed`] has nowhere to fall back to), so this arm only
/// keeps the match whole and reports the build rather than pretending.
#[cfg(all(feature = "multiplex", not(feature = "kcp")))]
async fn establish_kcp_pool(
    dial: PoolDial<'_>,
    target: usize,
    key: &str,
) -> Result<(
    Dialer,
    Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
)> {
    let _ = (dial, target, key);
    bail!("This build cannot carry the data plane over KCP (the `kcp` feature is off)")
}

/// A pool's start on TCP: its dialer (which repair keeps using) and the tunnels
/// that came up. The first carrier's half, and the only half a build without the
/// `kcp` feature ever runs.
#[cfg(feature = "multiplex")]
async fn tcp_pool_start(
    dial: PoolDial<'_>,
    target: usize,
    key: &str,
) -> Result<(
    Dialer,
    Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
)> {
    let dialer = dial.dialer().await?;
    let initial = establish_initial(&dialer, target, key, Carrier::Tcp).await;
    Ok((dialer, initial))
}

#[cfg(feature = "multiplex")]
impl PoolDial<'_> {
    /// The dialer one carrier's tunnels are established with.
    async fn dialer(&self) -> Result<Dialer> {
        tunnel_dialer(
            self.transport,
            self.data_addr,
            self.session_nonce,
            self.opts,
            self.choice,
        )
        .await
    }
}

/// Timeout for one KCP tunnel establishment (session dial + optional Noise
/// handshake + hello/ack). UDP gives no connect error, so a blackholed
/// endpoint must not hang the control channel forever; on timeout the usual
/// control-channel retry loop rebuilds the pool.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
const KCP_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Open one KCP session, optionally wrap it in Noise, exchange the tunnel
/// hello/ack and start the yamux driver.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
async fn establish_one_kcp_tunnel(
    remote_addr: SocketAddr,
    session_nonce: Nonce,
    opts: &DataOpts,
    service_name: &str,
) -> Result<(ClientTunnel, watch::Sender<bool>)> {
    // Full Noise handshake + v3 selector byte (the wrapper owns the selector):
    // KCP tunnels establish once per control session, so a resume attempt has
    // nothing cached yet and would only add a round trip.
    #[cfg(feature = "noise")]
    let noise = opts.noise.as_ref();
    #[cfg(not(feature = "noise"))]
    let noise = None;
    let mut io = crate::transport::kcp::connect_stream(remote_addr, noise).await?;
    // The session's ARQ counters travel with the tunnel: the pool reads this
    // tunnel's retransmit share for the carrier rule (the KCP-side signal; a
    // TCP tunnel has none, which is why the rule is one-sided by construction).
    let health = io.health();

    let hello = Hello::DataChannelTunnelHello(CURRENT_PROTO_VERSION, session_nonce);
    io.write_all(&postcard::to_stdvec(&hello)?).await?;
    io.flush().await?;
    match read_ack(&mut io).await? {
        Ack::Ok => {}
        v => bail!("Service {service_name}: the server refused the KCP data tunnel: {v}"),
    }

    let config = crate::transport::multiplex::mux_config();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    Ok((
        ClientTunnel::start_with_health(io, Some(health), config, shutdown_rx),
        shutdown_tx,
    ))
}

// Runtime-resolved per-service UDP options. Validation fills the defaults;
// the fallbacks here only guard against a missed validation.
fn udp_buffer_size(s: &ClientServiceConfig) -> usize {
    s.udp_buffer_size
        .map_or(DEFAULT_UDP_BUFFER_SIZE, |v| v as usize)
}

/// Creation-time parameters for the per-service UDP hub, resolved once from
/// the service configuration per control-channel session.
struct UdpForwardParams {
    local_addr: String,
    buffer_size: usize,
}

/// One remote peer's forwarding state.
struct UdpVisitorRoute {
    /// Queue into the peer's forwarder task (datagrams bound for the local
    /// service), carrying the sojourn timestamp `CoDel` reads.
    inbound: mpsc::Sender<(Instant, Bytes)>,
    /// Data-channel writer currently carrying the peer's traffic.
    outbound: mpsc::Sender<UdpTraffic>,
    /// That channel's id, so moving the peer to another channel moves the
    /// tunnel's pinned count with it (D30).
    #[cfg(feature = "multiplex")]
    channel: u64,
}

/// UDP state shared by every data channel of one service session.
///
/// Peers are keyed by their source address: each peer gets exactly one local
/// forwarder socket for its whole session, and its outbound datagrams are
/// pinned to the data channel its inbound traffic arrives on. This keeps the
/// `(ip, port)` tuple the local service sees stable. Stateful UDP sessions
/// (`RakNet`, QUIC, `WireGuard`, ...) break when a proxy splits one peer
/// across several source ports — a per-data-channel peer map does exactly
/// that whenever the server re-shards the peer onto another channel.
struct UdpHub {
    params: UdpForwardParams,
    routes: RwLock<HashMap<SocketAddr, UdpVisitorRoute>>,
    /// Writers of the currently live data channels, used as the outbound
    /// fallback when a pinned channel died.
    channels: RwLock<Vec<mpsc::Sender<UdpTraffic>>>,
    next_channel: AtomicUsize,
    /// Source of this service session's data-channel ids: the UDP pin
    /// accounting's keys. Deliberately *not* `next_channel`, which is the
    /// writer round-robin cursor.
    #[cfg(feature = "multiplex")]
    next_channel_id: std::sync::atomic::AtomicU64,
    /// The session's pin accounting: which tunnel each channel's stream is on.
    /// In the direct build the handle is the unit type, so the field is never
    /// read there.
    #[cfg_attr(
        not(feature = "multiplex"),
        allow(dead_code, reason = "the direct build has no tunnels to pin a peer to")
    )]
    pins: Option<Arc<Pins>>,
}

impl UdpHub {
    fn new(params: UdpForwardParams, pins: Arc<Pins>) -> Self {
        UdpHub {
            params,
            routes: RwLock::new(HashMap::new()),
            channels: RwLock::new(Vec::new()),
            next_channel: AtomicUsize::new(0),
            #[cfg(feature = "multiplex")]
            next_channel_id: std::sync::atomic::AtomicU64::new(0),
            pins: Some(pins),
        }
    }

    /// Mint this hub's next data-channel id.
    #[cfg(feature = "multiplex")]
    fn next_channel_id(&self) -> u64 {
        self.next_channel_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Record that this data channel is a stream of `tunnel`, so a peer
    /// pinned to the channel pins that tunnel too (D30).
    #[cfg(feature = "multiplex")]
    fn bind_channel(&self, channel: u64, tunnel: usize) {
        if let Some(pins) = &self.pins {
            pins.bind(channel, tunnel);
        }
    }

    /// Forget a data channel that ended.
    #[cfg(feature = "multiplex")]
    fn unbind_channel(&self, channel: u64) {
        if let Some(pins) = &self.pins {
            pins.unbind(channel);
        }
    }

    async fn register_channel(&self, tx: mpsc::Sender<UdpTraffic>) {
        self.channels.write().await.push(tx);
    }

    async fn unregister_channel(&self, tx: &mpsc::Sender<UdpTraffic>) {
        self.channels.write().await.retain(|c| !c.same_channel(tx));
    }

    /// Forward one datagram from the server to the peer's local socket,
    /// creating the forwarder (and the route) on first sight, and pinning
    /// the delivering channel as the peer's outbound path.
    async fn deliver(
        me: Arc<UdpHub>,
        from: SocketAddr,
        data: Bytes,
        channel: mpsc::Sender<UdpTraffic>,
        channel_id: u64,
    ) {
        {
            let mut routes = me.routes.write().await;
            if let Some(route) = routes.get_mut(&from) {
                #[cfg(feature = "multiplex")]
                {
                    repin(me.pins.as_deref(), route, channel, channel_id);
                }
                #[cfg(not(feature = "multiplex"))]
                {
                    route.outbound = channel;
                }
                if let Err(e) = route.inbound.try_send((Instant::now(), data)) {
                    debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
                }
                return;
            }
        }
        // First datagram from this peer: bind its dedicated local socket
        // outside the lock, then insert the route. Another channel may have
        // created the peer in the meantime; reuse that forwarder.
        let socket = match udp_connect(&me.params.local_addr).await {
            Ok(s) => s,
            Err(e) => {
                debug!("Failed to connect to the local UDP service: {e:#}");
                return;
            }
        };
        let mut routes = me.routes.write().await;
        if let Some(route) = routes.get_mut(&from) {
            // Lost the race; drop our freshly bound socket and reuse the
            // existing forwarder.
            drop(socket);
            #[cfg(feature = "multiplex")]
            {
                repin(me.pins.as_deref(), route, channel, channel_id);
            }
            #[cfg(not(feature = "multiplex"))]
            {
                route.outbound = channel;
            }
            if let Err(e) = route.inbound.try_send((Instant::now(), data)) {
                debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
            }
        } else {
            let (inbound_tx, inbound_rx) = mpsc::channel(UDP_QUEUE_SIZE);
            debug!("New UDP peer {from}, binding a local forwarder socket");
            tokio::spawn(run_udp_forwarder(
                socket,
                inbound_rx,
                Arc::clone(&me),
                from,
                inbound_tx.clone(),
                channel_id,
            ));
            routes.insert(
                from,
                UdpVisitorRoute {
                    inbound: inbound_tx.clone(),
                    outbound: channel,
                    #[cfg(feature = "multiplex")]
                    channel: channel_id,
                },
            );
            #[cfg(feature = "multiplex")]
            if let Some(pins) = me.pins.as_deref() {
                pins.update(channel_id, true);
            }
            if let Err(e) = inbound_tx.try_send((Instant::now(), data)) {
                debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
            }
        }
    }

    /// Push one datagram from the local service towards the peer: onto the
    /// pinned channel when it lives, otherwise onto any live channel.
    ///
    /// Queues are written with `try_send` only: a slow path drops datagrams
    /// instead of stalling this peer's socket — and with it every other peer
    /// sharing the channel.
    async fn send_outbound(
        &self,
        from: SocketAddr,
        mut t: UdpTraffic,
        #[cfg_attr(
            not(feature = "multiplex"),
            expect(
                unused_variables,
                reason = "direct data channels have no tunnel to pin a peer to"
            )
        )]
        channel_id: u64,
    ) {
        let pinned = self
            .routes
            .read()
            .await
            .get(&from)
            .map(|r| r.outbound.clone());
        if let Some(tx) = pinned {
            match tx.try_send(t) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    debug!("UDP outbound queue full for {from}, dropping a datagram");
                    return;
                }
                Err(TrySendError::Closed(back)) => {
                    // Pinned channel died; fall back below.
                    t = back;
                }
            }
        }

        let fallback = {
            let channels = self.channels.read().await;
            if channels.is_empty() {
                debug!("No live UDP data channel, dropping a datagram for {from}");
                return;
            }
            let idx = self.next_channel.fetch_add(1, Ordering::Relaxed) % channels.len();
            channels[idx].clone()
        };
        // Re-pin so subsequent datagrams stop hitting the dead channel.
        let mut routes = self.routes.write().await;
        if let Some(route) = routes.get_mut(&from) {
            #[cfg(feature = "multiplex")]
            {
                repin(self.pins.as_deref(), route, fallback.clone(), channel_id);
            }
            #[cfg(not(feature = "multiplex"))]
            {
                route.outbound = fallback.clone();
            }
        }
        drop(routes);
        if let Err(e) = fallback.try_send(t) {
            debug!("Dropped a UDP datagram for {from}: {e}");
        }
    }
}

/// Move a peer's route onto `channel`: the tunnel it was pinned to loses one
/// pinned peer, the new one gains it (D30).
///
/// Without the `multiplex` feature there are no tunnels to pin, so the route
/// pointer moves and nothing else.
#[cfg(feature = "multiplex")]
fn repin(
    pins: Option<&crate::transport::multiplex::PinRegistry>,
    route: &mut UdpVisitorRoute,
    channel: mpsc::Sender<UdpTraffic>,
    channel_id: u64,
) {
    if route.channel != channel_id {
        if let Some(pins) = pins {
            pins.update(route.channel, false);
            pins.update(channel_id, true);
        }
        route.channel = channel_id;
    }
    route.outbound = channel;
}

#[instrument(skip_all)]
async fn run_data_channel_for_udp<S>(conn: S, hub: Arc<UdpHub>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    debug!("New data channel starts forwarding");
    // The channel's id, so a peer routed onto it pins the tunnel its stream
    // lives on. Direct data channels have no tunnel, hence the zero.
    #[cfg(feature = "multiplex")]
    let channel_id = hub.next_channel_id();
    #[cfg(not(feature = "multiplex"))]
    let channel_id = 0u64;
    let (wr_tx, mut wr_rx) = mpsc::channel::<UdpTraffic>(UDP_QUEUE_SIZE);
    hub.register_channel(wr_tx.clone()).await;

    let (mut rd, mut wr) = io::split(conn);

    // Keep sending datagrams the hub routes to this channel to the server.
    // The scratch buffer is reused across packets: each datagram is framed
    // into it once and emitted with a single write (single Noise record).
    let writer_hub = Arc::clone(&hub);
    tokio::spawn(async move {
        let mut scratch =
            BytesMut::with_capacity(MAX_UDP_HEADER_LEN + writer_hub.params.buffer_size);
        while let Some(t) = wr_rx.recv().await {
            trace!("outbound {:?}", t);
            if let Err(e) = UdpTraffic::write_frame(&mut wr, &mut scratch, t.from, &t.data)
                .await
                .with_context(|| "Failed to forward UDP traffic to the server")
            {
                debug!("{:?}", e);
                break;
            }
        }
    });

    let res = udp_read_loop(&mut rd, &hub, &wr_tx, channel_id).await;

    // Leave the channel registry (and drop our queue sender) whether the
    // read loop failed or the stream simply ended.
    hub.unregister_channel(&wr_tx).await;
    res
}

/// The read side of one UDP data channel: frame datagrams coming from the
/// server and hand each to its peer's forwarder through the hub.
async fn udp_read_loop<S>(
    rd: &mut S,
    hub: &Arc<UdpHub>,
    wr_tx: &mpsc::Sender<UdpTraffic>,
    channel_id: u64,
) -> Result<()>
where
    S: tokio::io::AsyncRead + Unpin,
{
    loop {
        // Read a packet from the server. `Ok(None)` means an oversized packet
        // was dropped; the stream stays in sync, so just keep going.
        let hdr_len = rd.read_u8().await?;
        let Some(packet) = UdpTraffic::read(rd, hdr_len, hub.params.buffer_size)
            .await
            .with_context(|| "Failed to read UDPTraffic from the server")?
        else {
            continue;
        };
        UdpHub::deliver(
            Arc::clone(hub),
            packet.from,
            packet.data,
            wr_tx.clone(),
            channel_id,
        )
        .await;
    }
}

/// Run the local socket of one remote peer.
///
/// Datagrams from the server are sent to the local service through this
/// socket — one socket per peer for the peer's whole session, so the local
/// service observes a stable source port. Replies are tagged with the
/// peer's address and pushed onto the pinned data channel, falling back to
/// any live channel when that one died.
#[instrument(skip_all, fields(from))]
async fn run_udp_forwarder(
    s: UdpSocket,
    mut inbound_rx: mpsc::Receiver<(Instant, Bytes)>,
    hub: Arc<UdpHub>,
    from: SocketAddr,
    my_inbound: mpsc::Sender<(Instant, Bytes)>,
    channel_id: u64,
) {
    debug!("Forwarder created");
    let mut buf = BytesMut::zeroed(hub.params.buffer_size);
    // The peer's own pace: the largest gap between two datagrams observed so
    // far. The mapping lifetime is a multiple of it (see udp_lifetime), so a
    // steady game session keeps its source port and a scanner does not.
    let mut last_seen = Instant::now();
    let mut max_gap = Duration::ZERO;

    loop {
        tokio::select! {
            // Receive from the server
            data = inbound_rx.recv() => {
                match data {
                    Some((enqueued, data)) => {
                        let now = Instant::now();
                        max_gap = max_gap.max(now.duration_since(last_seen));
                        last_seen = now;
                        if enqueued.elapsed() > UDP_CODEL_TARGET {
                            // CoDel: a datagram that waited past the sojourn
                            // target is dropped, not delivered late.
                            trace!("UDP CoDel drop for {from}: {:?}", enqueued.elapsed());
                            continue;
                        }
                        if let Err(e) = s.send(&data).await {
                            debug!("Failed to send to the local UDP service: {e:#}");
                            break;
                        }
                    }
                    None => break,
                }
            },

            // Receive from the service
            val = s.recv(&mut buf) => {
                // A reply longer than `udp_buffer_size` is delivered as its
                // prefix on both platforms; on Windows the kernel reports that
                // as `WSAEMSGSIZE` instead of a short read (see `datagram_len`).
                // The socket is connected, so no address is lost by reading the
                // error that way.
                let Ok(len) = datagram_len(val, buf.len()) else {
                    break;
                };

                let now = Instant::now();
                max_gap = max_gap.max(now.duration_since(last_seen));
                last_seen = now;
                let t = UdpTraffic{
                    from,
                    data: Bytes::copy_from_slice(&buf[..len])
                };

                hub.send_outbound(from, t, channel_id).await;
            },

            // No traffic for this peer's adaptive lifetime: give the mapping
            // (and the local source port) back.
            () = time::sleep(udp_lifetime(max_gap)) => {
                break;
            }
        }
    }

    // Remove the route only if it still points at this forwarder: another
    // forwarder may have taken over while we were exiting.
    let mut routes = hub.routes.write().await;
    if routes
        .get(&from)
        .is_some_and(|r| r.inbound.same_channel(&my_inbound))
    {
        routes.remove(&from);
    }

    debug!("Forwarder dropped");
}

/// Build the per-service UDP hub for a UDP service session: every data
/// channel registers with it, and every remote peer keeps one local
/// forwarder socket regardless of which channel carries it (session
/// affinity, see `UdpHub`). TCP services get `None`.
fn build_udp_hub(
    service: &ClientServiceConfig,
    #[cfg_attr(
        not(feature = "multiplex"),
        allow(
            unused_variables,
            reason = "the unit handle is carried for shape; nothing pins without tunnels"
        )
    )]
    pins: Arc<Pins>,
) -> Option<Arc<UdpHub>> {
    match service.service_type {
        ServiceType::Udp => Some(Arc::new(UdpHub::new(
            UdpForwardParams {
                local_addr: service.local_addr.clone(),
                buffer_size: udp_buffer_size(service),
            },
            pins,
        ))),
        ServiceType::Tcp | ServiceType::Transparent => None,
    }
}

/// Spawn one requested data channel: a stream off the tunnel pool when the
/// service multiplexes its data plane, a fresh transport connection
/// otherwise. `stripe` is `Some` only for a channel the server asked for as one
/// stripe of a named group (`CreateDataChannelForStripe`); each channel
/// then joins its stripe group when the server labels it with
/// `StartForwardStripedTcp` (see [`crate::stripe`]).
fn spawn_data_channel(
    args: Arc<RunDataChannelArgs>,
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            unused_variables,
            reason = "without the multiplex feature the data plane is always direct"
        )
    )]
    tunnel: Option<&Tunnels>,
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            unused_variables,
            reason = "without the multiplex feature no pool exists to place a stripe on"
        )
    )]
    stripe: Option<StripeOpen>,
) {
    #[cfg(feature = "multiplex")]
    let tunnel = tunnel.cloned();
    tokio::spawn(
        async move {
            let res = {
                #[cfg(feature = "multiplex")]
                match tunnel {
                    Some(t) => run_mux_data_channel(&args, &t, stripe).await,
                    None => run_data_channel(args).await,
                }
                #[cfg(not(feature = "multiplex"))]
                run_data_channel(args).await
            };
            if let Err(e) = res.with_context(|| "Failed to run the data channel") {
                // One visitor connection's life, and its end: the log line for
                // a dead local service is this one, so the level has to be the
                // per-connection one. A *failed request* is visible to the
                // visitor; the operator needs the cause only when debugging.
                debug!("{:#}", e);
            }
        }
        .instrument(Span::current()),
    );
}

/// Handle of a control session: the client's only way to reach one endpoint's
/// connection.
///
/// Dropping it does not stop the session — [`Self::shutdown`] does — so a
/// service that hot-reloads away cannot take the connection down by accident.
struct ClientSessionHandle {
    req_tx: mpsc::UnboundedSender<SessionRequest>,
    shutdown_tx: oneshot::Sender<u8>,
}

impl ClientSessionHandle {
    /// Ask the session to register (or re-register) one service.
    fn register(&self, slot: Box<ServiceSlot>) {
        // A send failure means the session stopped: a fatal error already
        // told the operator why, and there is nothing left to register on.
        let _ = self.req_tx.send(SessionRequest::Register(slot));
    }

    /// Ask the session to drop one service.
    fn deregister(&self, id: ServiceId) {
        let _ = self.req_tx.send(SessionRequest::Deregister(id));
    }

    fn shutdown(self) {
        // A send failure shows that the session has already stopped.
        let _ = self.shutdown_tx.send(0u8);
    }
}

/// An error no retry can fix: the two ends speak different dialects, or the
/// server refused a credential. Retrying would only hide it, so the session
/// stops and says so instead (a human has to act).
#[derive(Debug)]
struct SessionFatal(String);

impl std::fmt::Display for SessionFatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionFatal {}

/// The shortest heartbeat timeout that survives a server declaring
/// `interval`: two missed beats plus slack, never below 10 s. `None` when the
/// server declares no cadence (`interval == 0`), which means there is nothing
/// to time out (D11).
fn derived_heartbeat_floor(interval: u64) -> Option<Duration> {
    (interval != 0).then(|| {
        Duration::from_secs(interval.saturating_mul(2).saturating_add(5))
            .max(Duration::from_secs(10))
    })
}

/// Resolve one session's heartbeat timeout from the cadence the server
/// declared and the client-wide constraint.
///
/// The floor is `max(10 s, 2 × interval + 5 s)` — two missed beats plus slack
/// — and a configured timeout *below* it would declare a healthy server dead,
/// so it is refused with both numbers instead of obeyed (D11). A configured
/// timeout at or above the floor wins (it is the operator's explicit choice),
/// and `Some(0)` disables the check outright. Unset means "derive it".
fn resolve_heartbeat_timeout(interval: u64, default: Option<u64>) -> Result<Option<Duration>> {
    // An explicit 0 disables the check, whatever the server declares.
    if default == Some(0) {
        return Ok(None);
    }
    let floor = derived_heartbeat_floor(interval);
    let Some(configured) = default.map(Duration::from_secs) else {
        return Ok(floor);
    };
    if let Some(floor) = floor
        && configured < floor
    {
        return Err(SessionFatal(format!(
            "the server declares a {interval} s heartbeat, which needs at least {} s; \
             you configured {} s",
            floor.as_secs(),
            configured.as_secs()
        ))
        .into());
    }
    Ok(Some(configured))
}

/// The fatal error for a server that did not answer the hello at all.
///
/// This is what an older server does to this client: it reads the version
/// byte, fails its own check and closes without a reply. Naming the dialect
/// and the likely cause is the whole point — the alternative is a client that
/// looks healthy while none of its services are reachable. The interop matrix
/// greps the client's log for exactly this statement.
fn unanswered_hello_error(remote_addr: &str, cause: &anyhow::Error) -> anyhow::Error {
    SessionFatal(format!(
        "{cause:#}. The server at {remote_addr} closed the connection before answering the hello: \
         this client speaks protocol v{CURRENT_PROTO_VERSION}, so the server is likely older than \
         this client. Upgrade the server first, or run a client of the server's version."
    ))
    .into()
}

/// Sleep for the session's heartbeat timeout, or forever when it has none.
async fn wait_for_heartbeat(timeout: Option<Duration>) {
    match timeout {
        Some(t) => time::sleep(t).await,
        None => std::future::pending().await,
    }
}

/// One service as its session holds it.
struct ServiceEntry {
    slot: Box<ServiceSlot>,
    state: ServiceState,
}

/// The lifecycle of one service inside its session (D2): a service the server
/// refuses stops on its own, and the session and its siblings carry on.
enum ServiceState {
    /// Handed to the session, not yet answered by the server.
    Registering,
    /// Registered: the server bound the public endpoint and this side keeps
    /// its configured data channels open.
    Active(Box<ActiveService>),
    /// The server refused the registration. Terminal for this service: only a
    /// human can fix the config or the server's `allow_ports`.
    Rejected,
}

/// The lanes one carrier's claims hold, and the allocator that moves them.
///
/// A lane is a claim's carrier connection — one per lane, because a claim never
/// multiplexes. The configuration gives every claim of a carrier an equal share
/// of the operator's budget, which is the right *starting* point and the wrong
/// steady state: a claim carrying ten flows on one connection and a claim
/// carrying nothing have no use for the same number of connections. Once a
/// second this pool looks at what each claim's lanes are carrying and moves at
/// most one idle lane from a claim that can spare one to a claim carrying more
/// flows than it has lanes ([`crate::transparent::lanes`] owns the rule; this is
/// the runtime that reads the hubs and dials).
///
/// Two properties make it safe:
///
/// - a lane is asked to end **only when it holds no flow at all**, which is the
///   condition the hub re-checks under its own lock, and no flow ever moves —
///   so lending cannot reorder anything;
/// - the receiver's lane is dialed only after the donor's lane has actually
///   ended, so the pair never holds more carrier connections than the budget at
///   any instant: the server's `max_tunnels` valve, and the
///   operator's bill, both see a swap rather than a growth.
///
/// The allocator redistributes what the configuration gave these claims; it does
/// not invent lanes the configuration never allocated. That keeps the operator's
/// `tunnels` the number that decides how many connections exist, with the
/// *division* of them left to what the traffic turns out to be.
#[cfg(mh_l3_client)]
struct LanePool {
    /// Whether lending is on for this client (`[transparent.data].lend`).
    ///
    /// Off means the equal split stands: the pool still tracks its claims (the
    /// replacement bookkeeping is the same), it just never moves a lane.
    lend: bool,
    /// When this pool started, for the diagnostic switch below.
    started: std::time::Instant,
    /// Whether the one diagnostic switch has been made.
    switched: std::sync::atomic::AtomicBool,
    /// Drains this pool has asked for, with when it asked: a lane whose drain
    /// never becomes ready is called off rather than left unusable.
    draining: std::sync::Mutex<HashMap<(ServiceId, LaneId), std::time::Instant>>,
    /// The previous tick's per-lane cumulative counters, so a tick is a window
    /// rather than a lifetime: `(peer offered, peer refused, this end injected)`.
    carrier_sample: std::sync::Mutex<HashMap<(Endpoint, LaneId), LaneSample>>,
    /// Consecutive ticks each claim has been judged to be losing traffic on the
    /// carrier it rides. One second of loss is a burst; the rule waits for
    /// [`CARRIER_CONFIRM_TICKS`] of them before it moves a lane.
    carrier_streak: std::sync::Mutex<HashMap<Endpoint, u32>>,
    /// Where an ask goes (the session owns the writer), and where its answer
    /// arrives (the peer's half of the agreement).
    req_tx: mpsc::UnboundedSender<SessionRequest>,
    drain_state: Arc<LaneDrainState>,
    /// The claims of this pool, by claimed endpoint.
    claims: std::sync::Mutex<HashMap<Endpoint, ClaimEntry>>,
}

/// How long a lane drain may wait for the peer's answer before it is called
/// off. The peer answers as soon as its own flows in the lane are gone, and a
/// flow that outlives this budget is one the drain must not wait for: a lane
/// nobody may use is worse than the carrier the drain was trying to leave.
#[cfg(mh_l3_client)]
const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// How many consecutive ticks a claim must be judged to be losing traffic on
/// the carrier it rides before a lane is handed over.
///
/// One second of loss is a burst — a visitor's blast, a queue that filled for a
/// moment — and moving a lane for it would trade a transient for a drain. Three
/// ticks is the order the lending allocator's own tick works at, and the move
/// itself is rate-limited again by
/// [`crate::transport::carrier::SWITCH_COOLDOWN`].
#[cfg(mh_l3_client)]
const CARRIER_CONFIRM_TICKS: u32 = 3;

/// One claim, as the allocator knows it.
#[cfg(mh_l3_client)]
struct ClaimEntry {
    /// The device the claim's hub reads (`[transparent].tun`).
    tun: String,
    /// What one data channel of the claim needs, so another lane can be dialed
    /// for it without going back to the session.
    args: Arc<RunDataChannelArgs>,
    /// The lanes the configuration gave this claim (its equal share), which is
    /// what the pool's budget is the sum of — a move redistributes these, it
    /// does not create more.
    share: usize,
    /// The lanes the claim is wanted to hold right now: the share, adjusted by
    /// every move. It is also what tells a replacement the server asks for
    /// apart: a lane that *failed* leaves the claim short of its target, so the
    /// replacement is dialed; a lane retired on purpose lowered the target
    /// first, so that request is refused.
    target: usize,
}

#[cfg(mh_l3_client)]
impl LanePool {
    fn new(
        lend: bool,
        req_tx: mpsc::UnboundedSender<SessionRequest>,
        drain_state: Arc<LaneDrainState>,
    ) -> Self {
        Self {
            lend,
            started: std::time::Instant::now(),
            switched: std::sync::atomic::AtomicBool::new(false),
            draining: std::sync::Mutex::new(HashMap::new()),
            carrier_sample: std::sync::Mutex::new(HashMap::new()),
            carrier_streak: std::sync::Mutex::new(HashMap::new()),
            req_tx,
            drain_state,
            claims: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Add a claim to the pool, or update the one already there (a hot reload
    /// re-registers the same claim, possibly with a new share).
    fn register(
        &self,
        endpoint: Endpoint,
        tun: String,
        args: Arc<RunDataChannelArgs>,
        share: usize,
    ) {
        if let Ok(mut claims) = self.claims.lock() {
            let share = share.max(1);
            claims.insert(
                endpoint,
                ClaimEntry {
                    tun,
                    args,
                    share,
                    target: share,
                },
            );
        }
    }

    /// A service is gone: its claim stops counting against the budget.
    fn unregister(&self, endpoint: &Endpoint) {
        if let Ok(mut claims) = self.claims.lock() {
            claims.remove(endpoint);
        }
    }

    /// Whether a data channel the server asked for is one this claim wants.
    ///
    /// A lane that failed leaves the claim short of its target, and the hub keeps
    /// the dead lane's slot for exactly that replacement. A lane that was retired
    /// on purpose lowered the target first, so this answers no — otherwise the
    /// allocator would undo its own move on every request.
    fn wants_another_lane(&self, endpoint: &Endpoint) -> bool {
        let Some((tun, target)) = self.claims.lock().ok().and_then(|claims| {
            claims
                .get(endpoint)
                .map(|entry| (entry.tun.clone(), entry.target))
        }) else {
            return true;
        };
        match TunHub::get_or_spawn(&tun, Direction::Source) {
            Ok(hub) => {
                hub.member_stats()
                    .iter()
                    .filter(|lane| lane.endpoint == *endpoint && lane.live)
                    .count()
                    < target
            }
            // The device is not up (yet): a claim with no hub has no lanes
            // either, so the channel is what it is waiting for.
            Err(_) => true,
        }
    }

    /// Start the allocator's tick for this pool.
    ///
    /// It runs for the session's lifetime rather than for its claims': a pool
    /// that emptied and filled again must not need a second start, and one wake
    /// per second with nothing to do costs nothing worth the bookkeeping.
    fn spawn_ticker(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(lanes::LANE_TICK);
            loop {
                tick.tick().await;
                pool.step();
            }
        });
    }

    /// What the policy sees: every claim's live lanes and what they carry, and
    /// how many lanes that is in total.
    fn snapshot(&self) -> (Vec<ClaimLoad>, usize) {
        let Ok(claims) = self.claims.lock() else {
            return (Vec::new(), 0);
        };
        // Lanes whose handover is in flight, as (service, lane) — read once, so
        // the loop below does not take the drain lock per lane.
        let handing_over: Vec<(ServiceId, LaneId)> = self
            .draining
            .lock()
            .map(|draining| draining.keys().copied().collect())
            .unwrap_or_default();
        let mut loads = Vec::with_capacity(claims.len());
        let mut used = 0;
        for (endpoint, entry) in claims.iter() {
            let Ok(hub) = TunHub::get_or_spawn(&entry.tun, Direction::Source) else {
                continue;
            };
            let service = entry.args.service_id;
            let lanes: Vec<LaneLoad> = hub
                .member_stats()
                .iter()
                .filter(|lane| lane.endpoint == *endpoint && lane.live)
                .map(|lane| LaneLoad {
                    slot: lane.slot,
                    flows: lane.flows,
                    // A lane is under a handover when this end asked the peer to
                    // drain it: the ask is what makes it unavailable to lending
                    // (a handover in flight outranks a lend).
                    handover: lane
                        .lane
                        .is_some_and(|key| handing_over.contains(&(service, key.id))),
                })
                .collect();
            used += lanes.len();
            loads.push(ClaimLoad {
                endpoint: *endpoint,
                lanes,
            });
        }
        (loads, used)
    }

    /// The budget these claims share: the lanes the configuration gave them.
    ///
    /// The *shares*, not the targets — a move must not raise the budget it moves
    /// within, or an allocator that dialed once could keep finding room to dial
    /// again.
    fn budget(&self) -> usize {
        self.claims
            .lock()
            .map_or(0, |claims| claims.values().map(|entry| entry.share).sum())
    }

    /// The hub of one claim, or `None` when its device is not up.
    fn hub(&self, endpoint: &Endpoint) -> Option<Arc<TunHub>> {
        let tun = self
            .claims
            .lock()
            .ok()
            .and_then(|claims| claims.get(endpoint).map(|entry| entry.tun.clone()))?;
        TunHub::get_or_spawn(&tun, Direction::Source).ok()
    }

    /// Ask the peer to stop using a lane, or to take it back.
    ///
    /// Recorded here so the ask is made once (a ticker that re-sent it every
    /// second would fill the session's writer with duplicates), and the deadline
    /// is what stops a lane being unusable forever when the answer never comes.
    fn ask_drain(&self, service: ServiceId, lane: LaneId, cancel: bool) {
        if let Ok(mut draining) = self.draining.lock() {
            if cancel {
                if draining.remove(&(service, lane)).is_none() {
                    return;
                }
            } else if draining
                .insert((service, lane), std::time::Instant::now())
                .is_some()
            {
                return;
            }
        }
        let _ = self.req_tx.send(SessionRequest::DrainLane {
            service,
            lane: lane.0,
            cancel,
        });
    }

    /// Act on the answers the server gave about the drains this pool asked for.
    ///
    /// A lane is only marked for retirement once the **peer** says it holds no
    /// flow in it: the dialer cannot see the flows the peer placed there, and
    /// retiring on this end's word alone is the retired-lane black hole. Once
    /// both halves hold, the existing retire path does the rest — the lane takes
    /// no new flows and ends when the ones it has are done.
    ///
    /// A drain that is never answered is called off: a lane nobody may use is
    /// worse than the carrier the drain was trying to leave.
    fn drain_step(&self) {
        use crate::protocol::{LANE_DRAIN_CANCELLED, LANE_DRAIN_READY, LANE_DRAIN_UNKNOWN};
        let asked: Vec<(ServiceId, LaneId)> = self
            .draining
            .lock()
            .map(|draining| draining.keys().copied().collect())
            .unwrap_or_default();
        for (service, lane) in asked {
            let Some(state) = self.drain_state.take(service, lane) else {
                // No answer yet: give up on a drain that has been waiting too
                // long rather than leaving the lane half-retired.
                let stale = self.draining.lock().is_ok_and(|draining| {
                    draining
                        .get(&(service, lane))
                        .is_some_and(|at| at.elapsed() >= DRAIN_BUDGET)
                });
                if stale {
                    info!(
                        "Lane {lane} of service {service}: no drain answer within {DRAIN_BUDGET:?}; \
                         calling the drain off"
                    );
                    self.ask_drain(service, lane, true);
                }
                continue;
            };
            if state != LANE_DRAIN_READY {
                if matches!(state, LANE_DRAIN_UNKNOWN | LANE_DRAIN_CANCELLED)
                    && let Ok(mut draining) = self.draining.lock()
                {
                    draining.remove(&(service, lane));
                }
                debug!("Lane {lane} of service {service}: drain answer {state}, not retiring it");
                continue;
            }
            // The peer holds nothing in it. Find the claim that owns the lane
            // and mark it: from here the lane takes no new flows and ends when
            // the ones it has are done.
            let claim = self.claims.lock().ok().and_then(|claims| {
                claims
                    .iter()
                    .find(|(_, entry)| entry.args.service_id == service)
                    .map(|(endpoint, entry)| (*endpoint, entry.tun.clone()))
            });
            let Some((endpoint, tun)) = claim else {
                debug!("Lane {lane} of service {service}: no claim of this session dials it");
                continue;
            };
            let Ok(hub) = TunHub::get_or_spawn(&tun, Direction::Source) else {
                debug!("Lane {lane} of service {service}: its device is not up");
                continue;
            };
            let Some(slot) = hub
                .member_stats()
                .into_iter()
                .find(|member| {
                    member.endpoint == endpoint && member.lane.is_some_and(|l| l.id == lane)
                })
                .map(|member| member.slot)
            else {
                // The peer answered about a lane this end does not hold — the
                // one inconsistency worth a line: the two ends disagree about
                // which connections exist, and a retirement decided on it would
                // be about nothing.
                info!(
                    "Lane {lane} of claim {endpoint}: the peer answered ready but this end holds \
                     no such lane; not retiring anything"
                );
                if let Ok(mut draining) = self.draining.lock() {
                    draining.remove(&(service, lane));
                }
                continue;
            };
            let idle = hub.retire_lane(&endpoint, slot);
            if let Ok(mut draining) = self.draining.lock() {
                draining.remove(&(service, lane));
            }
            info!(
                "Lane {lane} of claim {endpoint}: the peer holds no flow in it; retiring it \
                 ({}) so its replacement rides the carrier now preferred",
                if idle {
                    "idle"
                } else {
                    "still carrying this end's flows"
                }
            );
        }
    }

    /// One look: move at most one lane, if the policy says to.
    ///
    /// The carrier rule runs first: a lane being handed over is unavailable to
    /// lending (`ClaimLoad::lendable` skips it), and a handover a lend would
    /// cancel is worse than a claim waiting one more tick for a lane. Then the
    /// diagnostic switch (a measurement hatch, not policy), then lending.
    fn step(&self) {
        self.drain_step();
        self.carrier_step();
        self.diagnostic_switch();
        if !self.lend {
            return;
        }
        let (loads, used) = self.snapshot();
        // Nothing to move between, and no move without a reason.
        if loads.len() < 2 {
            return;
        }
        let Some(plan) = lanes::plan(&loads, self.budget(), used) else {
            return;
        };
        match plan {
            lanes::Plan::Add { to } => {
                // Room under the budget means a claim is holding fewer lanes than
                // the configuration gave it — a lane died and its replacement has
                // not arrived — so this hands the room to the claim that needs it
                // rather than waiting for the dead one.
                info!(
                    "Lane allocator: claim {to} has more flows than lanes and the lane budget has \
                     room; dialing a lane"
                );
                self.dial(&to);
            }
            lanes::Plan::Lend {
                from,
                from_slot,
                to,
            } => {
                let Some(hub) = self.hub(&from) else {
                    return;
                };
                if !hub.retire_lane(&from, from_slot) {
                    // The policy only names a lane that was carrying nothing, so
                    // this is the race with the packets that arrived since its
                    // snapshot: the lane is draining now, ends on its own, and
                    // the next tick can lend it.
                    debug!(
                        "Lane {from_slot} of claim {from} is not idle; not lending it this tick"
                    );
                    return;
                }
                if let Ok(mut claims) = self.claims.lock()
                    && let Some(entry) = claims.get_mut(&from)
                {
                    entry.target = entry.target.saturating_sub(1).max(1);
                }
                info!("Lane allocator: lent lane {from_slot} of claim {from} to claim {to}");
                // Only now, with the donor's lane on its way out: a swap, never
                // a growth.
                self.dial(&to);
            }
        }
    }

    /// The measurement hatch for carrier switching: after
    /// `MOLEHILL_CARRIER_SWITCH_AT` seconds, move every claim of this pool to the
    /// other carrier and drain one lane so the replacement rides it.
    ///
    /// It exists so the *effect* of a switch can be measured before the policy
    /// that decides one is written — whether the lanes that follow it capture
    /// what the A/B says is available, and what a switch costs the flows that
    /// are on the lane being drained. It is a diagnostic switch, not a
    /// deployment knob: the production behaviour is a measured decision inside
    /// [`crate::transport::carrier::CarrierChoice`], and this is how the
    /// measurement that justifies it gets its data. Read once per pool; a value
    /// that is not a whole number of seconds is reported and ignored.
    fn diagnostic_switch(&self) {
        use std::sync::atomic::Ordering;
        static SWITCH_AT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let Some(at) = *SWITCH_AT.get_or_init(|| {
            let Ok(raw) = std::env::var("MOLEHILL_CARRIER_SWITCH_AT") else {
                return None;
            };
            if let Ok(secs) = raw.trim().parse::<u64>() {
                Some(secs)
            } else {
                warn!(
                    "MOLEHILL_CARRIER_SWITCH_AT={raw:?} is not a whole number of seconds; \
                     ignoring it (the carrier is chosen at runtime)"
                );
                None
            }
        }) else {
            return;
        };
        if self.started.elapsed() < std::time::Duration::from_secs(at)
            || self.switched.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let targets: Vec<(Endpoint, Arc<RunDataChannelArgs>, Option<LaneId>)> = {
            let Ok(claims) = self.claims.lock() else {
                return;
            };
            claims
                .iter()
                .map(|(endpoint, entry)| {
                    // The busiest live lane is the one whose replacement shows
                    // the other carrier's worth: an idle lane would move without
                    // carrying anything.
                    let busiest = TunHub::get_or_spawn(&entry.tun, Direction::Source)
                        .ok()
                        .and_then(|hub| {
                            hub.member_stats()
                                .into_iter()
                                .filter(|lane| lane.endpoint == *endpoint && lane.live)
                                .max_by_key(|lane| lane.flows)
                                .and_then(|lane| lane.lane)
                                .map(|lane| lane.id)
                        });
                    (*endpoint, Arc::clone(&entry.args), busiest)
                })
                .collect()
        };
        for (endpoint, args, lane) in targets {
            let choice = &args.carrier;
            let other = match choice.carrier() {
                Carrier::Kcp => Carrier::Tcp,
                Carrier::Tcp => Carrier::Kcp,
            };
            // `prefer` is per *endpoint* and the choice is shared by every claim
            // dialing it, so the first claim moves it and the rest find it
            // already there — the drain below is what each claim owes the switch
            // regardless, or the ones that find the choice moved would keep every
            // lane on the carrier being left.
            choice.prefer(other, "MOLEHILL_CARRIER_SWITCH_AT: diagnostic switch");
            if let Some(lane) = lane {
                // The lane is *asked* for, not taken: its dialer cannot see the
                // flows the peer placed on it, so it is retired only once the
                // peer answers that it holds none (see [`Self::drain_step`]).
                // That is the whole difference between this and the black hole
                // the earlier version of this switch could produce.
                self.ask_drain(args.service_id, lane, false);
                info!(
                    "Data carrier switched for claim {endpoint}: asked the peer to drain lane \
                     {lane} so its replacement rides the other carrier"
                );
            }
        }
    }

    // The carrier policy was here, and its rule is gone with it. It read
    // `forwarded`/`no_channel` per slot and called it carrier loss: those are
    // *local enqueue* results (a full queue and a missing lane share the second
    // one), and on a transparent claim the loss that matters happens at the
    // **peer**, which offers the packets. Measured on the divergence cell, this
    // end's counters read 61 forwarded and 0 `no_channel` for a whole round
    // while the peer refused hundreds of thousands — so the rule could not fire,
    // and would have been judging the wrong end if it had.
    //
    // The replacement is built on the pair the report makes readable: what the
    // peer offered each lane and what that lane's queue refused there, against
    // what this end injected off it, both keyed by the lane id the dialer named
    // (`LaneId`; `peer_offered`/`peer_refused` in the `l3-stats` line). It is
    // deliberately not written yet: a lane may only be retired once **both**
    // ends agree it is empty, and that agreement does not exist (HANDOFF.md).

    // The carrier rule reads the pair the report made readable — what the peer
    // offered each lane and what that lane's queue refused there, against what
    // this end injected off it — and acts through the two-sided drain. It is not
    // the predecessor that read `no_channel` (see the note above where that one
    // was): this one samples the *peer's* counters, which is the only place the
    // loss on a transparent claim is visible at all.

    /// One window of evidence for one claim: what the peer offered each of its
    /// lanes and refused there, against what this end injected off it, all as
    /// differences from the previous tick.
    ///
    /// Every counter involved is cumulative for a lane's life, so a tick is only
    /// a window once there is a previous sample: a lane seen for the first time
    /// contributes nothing, which is the safe direction (no evidence, no move).
    fn carrier_evidence(&self, endpoint: &Endpoint, hub: &Arc<TunHub>) -> Vec<lanes::LaneEvidence> {
        let mut evidence = Vec::new();
        let mut fresh = Vec::new();
        {
            let Ok(mut last) = self.carrier_sample.lock() else {
                return evidence;
            };
            for member in hub
                .member_stats()
                .into_iter()
                .filter(|member| member.endpoint == *endpoint && member.live)
            {
                let Some(lane) = member.lane else {
                    continue;
                };
                let now = (
                    member.remote.map_or(0, |remote| u64::from(remote.offered)),
                    member.remote.map_or(0, |remote| u64::from(remote.refused)),
                    member.counters.injected_packets,
                );
                let previous = last.get(&(*endpoint, lane.id)).copied();
                fresh.push(((*endpoint, lane.id), now));
                let Some(previous) = previous else {
                    continue;
                };
                evidence.push(lanes::LaneEvidence {
                    slot: member.slot,
                    lane: lane.id,
                    carrier: member.carrier,
                    offered: now.0.saturating_sub(previous.0),
                    refused: now.1.saturating_sub(previous.1),
                    injected: now.2.saturating_sub(previous.2),
                });
            }
            for (key, value) in fresh {
                last.insert(key, value);
            }
        }
        evidence
    }

    /// Move a claim's lane to the other carrier when the one it rides is
    /// measurably losing its traffic.
    ///
    /// One tick is a rate: each lane's counters are differenced against the
    /// previous tick's, and the rule ([`lanes::carrier_move`]) sees a window.
    /// A verdict is held for [`CARRIER_CONFIRM_TICKS`] ticks before anything
    /// moves — one second of loss is a burst — and the move itself is the drain
    /// both ends have to agree on, never a lane retired on this end's word.
    fn carrier_step(&self) {
        // A forced carrier is the run's answer, not a preference to be argued
        // with: `MOLEHILL_DATA_CARRIER` exists so a measurement can pin one
        // carrier, `prefer` refuses to move it, and asking the peer to drain
        // lanes anyway would take them out of service for a switch that cannot
        // happen. (Measured: the "forced KCP" control of the first A/B asked for
        // four drains, one per round, and moved lanes while claiming to be the
        // arm where nothing moves.)
        if crate::transport::carrier::forced_carrier().is_some() {
            return;
        }
        let Ok(claims) = self.claims.lock() else {
            return;
        };
        let claims: Vec<(Endpoint, String, ServiceId, Arc<RunDataChannelArgs>)> = claims
            .iter()
            .map(|(endpoint, entry)| {
                (
                    *endpoint,
                    entry.tun.clone(),
                    entry.args.service_id,
                    Arc::clone(&entry.args),
                )
            })
            .collect();
        for (endpoint, tun, service, args) in claims {
            let Ok(hub) = TunHub::get_or_spawn(&tun, Direction::Source) else {
                continue;
            };
            let evidence = self.carrier_evidence(&endpoint, &hub);
            // Only lanes that are not already being handed over are candidates:
            // a second drain for the same lane would be about a connection that
            // is already on its way out.
            let handing_over: Vec<LaneId> = self
                .draining
                .lock()
                .map(|draining| {
                    draining
                        .keys()
                        .filter(|(asked, _)| *asked == service)
                        .map(|(_, lane)| *lane)
                        .collect()
                })
                .unwrap_or_default();
            let evidence: Vec<lanes::LaneEvidence> = evidence
                .into_iter()
                .filter(|lane| !handing_over.contains(&lane.lane))
                .collect();
            let verdict = lanes::carrier_move(&evidence, args.carrier.carrier());
            let Ok(mut streaks) = self.carrier_streak.lock() else {
                return;
            };
            let streak = streaks.entry(endpoint).or_insert(0);
            let Some(moved) = verdict else {
                *streak = 0;
                continue;
            };
            *streak += 1;
            if *streak < CARRIER_CONFIRM_TICKS {
                continue;
            }
            *streak = 0;
            drop(streaks);
            // The evidence is what justifies the change, so it is what the log
            // line carries (once per switch: `prefer` is the notice).
            args.carrier.prefer(
                moved.prefer,
                "the carrier in use kept refusing this claim's traffic at the peer",
            );
            if args.carrier.carrier() != moved.prefer {
                // The cooldown from an earlier switch refused it: the decision
                // stands, the move waits, and no lane is taken out of service
                // for a carrier that cannot change yet.
                debug!(
                    "Claim {endpoint}: carrier move to {:?} deferred by the switch cooldown",
                    moved.prefer
                );
                continue;
            }
            self.ask_drain(service, moved.lane, false);
            info!(
                "Data carrier for claim {endpoint}: lane {} refused its share for \
                 {CARRIER_CONFIRM_TICKS} ticks; asked the peer to drain it so its replacement \
                 rides {:?}",
                moved.lane, moved.prefer
            );
        }
    }

    /// Dial one more lane for a claim, and count it as wanted.
    fn dial(&self, endpoint: &Endpoint) {
        let args = self.claims.lock().ok().and_then(|mut claims| {
            claims.get_mut(endpoint).map(|entry| {
                entry.target += 1;
                Arc::clone(&entry.args)
            })
        });
        if let Some(args) = args {
            spawn_data_channel(args, None, None);
        }
    }
}

/// A registered service's live data plane.
struct ActiveService {
    /// Everything one data channel of this service needs.
    args: Arc<RunDataChannelArgs>,
    /// The multiplexed tunnel pool, when the service multiplexes its data
    /// plane. `None` means one connection per channel.
    ///
    /// The pool is *owned* by the session (see `ClientSession::pools`), not by
    /// this service: with `[client.data].shared_pool` several services hold the
    /// same pool, so dropping one service must not tear the tunnels down.
    #[cfg(feature = "multiplex")]
    tunnel: Option<Tunnels>,
    /// The pool's key in the session, so the service can be counted against it.
    #[cfg(feature = "multiplex")]
    pool_key: Option<String>,
    /// The claimed endpoint this service's lanes carry, when it is a
    /// transparent service: the allocator names a claim by it.
    ///
    /// Gated with `mh_l3_client`: the lane allocator and carrier choice require
    /// `carrier`, not yamux, so a claim keeps its lanes on a no-multiplex build.
    #[cfg(mh_l3_client)]
    lane: Option<Endpoint>,
}

/// Open `count` data channels for a registered service.
///
/// The channels are the service's own: a v4 registration carries no pool (D5),
/// so the client opens what it needs and one more for every
/// `CreateDataChannelFor` the server sends. A UDP service's `udp_workers` are
/// opened when it becomes active; a TCP service opens none here (the server
/// asks for one channel per visitor), and the tunnel pool behind them starts
/// cold and grows on the first open.
fn open_channels(active: &ActiveService, count: usize) {
    // One request per channel; each takes the least-loaded tunnel, and the
    // reservation is charged before the first await, so back-to-back requests
    // land on distinct tunnels while the pool has them. A *stripe group* is
    // asked for one `CreateDataChannelForStripe` per stripe instead, which is
    // what makes that spread structural rather than a property of the pool's
    // current size (D24).
    for _ in 0..count {
        open_data_channel(active, None);
    }
}

/// Open one data channel for a service, as a stripe of a named group when the
/// server asked for one.
fn open_data_channel(active: &ActiveService, stripe: Option<StripeOpen>) {
    #[cfg(feature = "multiplex")]
    let tunnel = active.tunnel.as_ref();
    #[cfg(not(feature = "multiplex"))]
    let tunnel = None;
    spawn_data_channel(active.args.clone(), tunnel, stripe);
}

/// One endpoint's control session: the connection, its command loop, and every
/// service that dials that endpoint (D1).
///
/// The session owns the connection — one `tokio::io::split` gives it a reader
/// half for the server's commands and a writer half for its own
/// [`SessionCmd`]s. A service never touches the socket: it hands over a
/// [`ServiceSlot`] and is answered with service-tagged commands.
struct ClientSession {
    /// The endpoint this session dials (`[client.control].default_remote_addr`,
    /// or a service's own `remote_addr`) — the session's identity in logs.
    remote_addr: String,
    /// The wire stack the session dials with, shared with the data plane of
    /// every service that joined it.
    transport: Arc<ClientTransport>,
    /// The session credential: `[client].default_token`, exactly as in v3.
    token: MaskedString,
    /// `[client.control].default_heartbeat_timeout`: an explicit constraint,
    /// or `None` to derive the timeout from the server's declared cadence.
    default_heartbeat_timeout: Option<u64>,
    /// Registrations the client asks for.
    req_rx: mpsc::UnboundedReceiver<SessionRequest>,
    /// The status API's asks, answered from this session's own state. A
    /// session without a status listener still holds the receiver: an ask
    /// channel nobody closes is one fewer conditional in the session loop.
    ask_rx: mpsc::UnboundedReceiver<crate::status::SessionAsk>,
    /// The client's shutdown signal.
    shutdown_rx: oneshot::Receiver<u8>,
    /// Services this session carries, by the id the client allocated.
    services: HashMap<ServiceId, ServiceEntry>,
    /// Services the server reported dropped *while a registration was in
    /// flight*. They are re-registered once that registration is done rather
    /// than from inside it, which would nest one verdict wait in another.
    pending_drops: Vec<ServiceId>,
    /// The session's one tunnel pool, by key (`session:<data endpoint>`).
    ///
    /// The session owns it, not a service: every service of the session draws
    /// on the same connections, and dropping one service must not tear them
    /// down.
    #[cfg(feature = "multiplex")]
    pools: HashMap<String, Tunnels>,
    /// The client's UDP pin accounting, shared by every hub of this session's
    /// services: a tunnel with pinned peers is never shrunk (D30).
    #[cfg(feature = "multiplex")]
    pins: Arc<crate::transport::multiplex::PinRegistry>,
    /// The lane allocator this session's transparent claims share.
    ///
    /// One per session, not one per claim: the lane budget is the client's, so
    /// the claims that share it are the claims the allocator can move a lane
    /// between — and they are exactly the claims that dial this session.
    #[cfg(mh_l3_client)]
    lane_pool: Option<Arc<LanePool>>,
    /// A second handle on this session's own request channel, so the lane
    /// allocator can ask for a drain without owning the writer.
    #[cfg(mh_l3_client)]
    req_tx: mpsc::UnboundedSender<SessionRequest>,
    /// What the server said about those drains, shared with the allocator.
    #[cfg(mh_l3_client)]
    lane_drains: Arc<LaneDrainState>,
    /// One carrier decision per data endpoint this session's services dial.
    ///
    /// The scope is the endpoint because the network path is: services dialing
    /// the same address share the decision (and the one bounded attempt it
    /// costs), while a service with its own `remote_addr` makes its own.
    #[cfg(any(feature = "multiplex", mh_l3_client))]
    carriers: HashMap<String, Arc<crate::transport::carrier::CarrierChoice>>,
}

impl ClientSession {
    /// Drive the session until the client shuts it down, reconnecting with the
    /// usual backoff after a *retryable* failure.
    ///
    /// A [`SessionFatal`] error ends the task instead: no retry can fix a
    /// dialect mismatch or a refused credential, and the operator has to act.
    async fn drive(mut self, retry_backoff_max: Option<u64>) {
        let mut start = Instant::now();
        // Consecutive failures in the current outage: what the adaptive cap
        // reads. A session that stayed up for more than a few seconds starts
        // over, so the count describes one outage.
        let mut failures: u32 = 0;
        let mut logged_cap: Option<u64> = None;
        // A session that starts before its server retries every second. The
        // first line tells the operator what is happening; the hundredth only
        // fills the log, so it is a `debug`.
        let retry_notice = RepeatNotice::new();

        loop {
            match self
                .run()
                .await
                .with_context(|| "Failed to run the control session")
            {
                Ok(()) => {
                    // `run` returns `Ok` only after the shutdown signal broke
                    // its loop, so there is nothing left to reconnect for.
                    return;
                }
                Err(err) => {
                    if self.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                        return;
                    }

                    // A dialect mismatch or a refused token: a human must act,
                    // and a retry loop would only hide the reason.
                    if err.downcast_ref::<SessionFatal>().is_some() {
                        error!("{:#}", err);
                        return;
                    }

                    if start.elapsed() > Duration::from_secs(3) {
                        // The session ran for at least 3 secs and then dropped:
                        // this is a new outage.
                        failures = 0;
                        // It was up, so the next failure is news again.
                        retry_notice.clear();
                    }

                    failures = failures.saturating_add(1);
                    let cap = retry_backoff_max.unwrap_or_else(|| backoff_cap(failures));
                    if logged_cap != Some(cap) {
                        debug!(
                            failures,
                            cap_secs = cap,
                            explicit = retry_backoff_max.is_some(),
                            "control-channel retry backoff cap"
                        );
                        logged_cap = Some(cap);
                    }
                    let mut retry_backoff = run_control_chan_backoff(cap).build();
                    if let Some(duration) = retry_backoff.next() {
                        retry_notice.report(
                            || info!("{:#}. Retry in {:?}...", err, duration),
                            || debug!("{:#}. Retry in {:?}...", err, duration),
                        );
                        time::sleep(duration).await;
                    } else {
                        // Should never be reached with the current backoff
                        // policy, but keep the session alive instead of
                        // panicking.
                        warn!("{:#}. Backoff exhausted, retrying in 1s", err);
                        time::sleep(Duration::from_secs(1)).await;
                    }

                    start = Instant::now();
                }
            }
        }
    }

    /// One connection's life: connect, authenticate once, then serve every
    /// service until the connection or the shutdown ends it.
    async fn run(&mut self) -> Result<()> {
        let mut control_addr = AddrMaybeCached::new(&self.remote_addr);
        control_addr.resolve().await?;

        let conn = self
            .transport
            .connect(&control_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", self.remote_addr))?;
        conn.hint(SocketOpts::for_control_channel());
        // Buffered so the handshake can peek at the ack's first byte and read
        // the shape the server actually sent (see `handshake`).
        let mut conn = BufReader::new(conn);

        let (nonce, interval) = self.handshake(&mut conn).await?;
        let heartbeat_timeout = self.heartbeat_timeout(interval)?;

        // One split, two jobs: this task reads the server's commands from `rd`
        // and writes registrations to `wr`. Nothing else writes on a control
        // session — a service's data channels are their own connections.
        let (rd, mut wr) = tokio::io::split(conn);
        // A second buffer, around the read half only: `tokio::io::split`
        // hands out a plain `AsyncRead`, and the registration path has to
        // *peek* at the next frame's first byte to tell a verdict from a
        // sibling's command (`await_verdict`). The inner buffer is empty once
        // the handshake has read its frame, so nothing is copied twice.
        let mut rd = BufReader::new(rd);

        info!("Control session established, remote {}", self.remote_addr);

        // Apply whatever the client asked for while this session was
        // connecting *before* re-registering: a service deleted in the
        // meantime must not come back for one round.
        self.apply_pending_requests();
        for id in self.services_to_reregister() {
            self.register(id, &nonce, &mut rd, &mut wr).await?;
            self.drain_dropped(&nonce, &mut rd, &mut wr).await?;
        }

        loop {
            tokio::select! {
                cmd = read_control_cmd(&mut rd) => match cmd? {
                    // A visitor needs a channel for that service.
                    ControlChannelCmd::CreateDataChannelFor(id) => self.open_channel(id),
                    // A visitor's stripe group: the group is known, so the
                    // channel can be placed on a tunnel of its own (D24).
                    ControlChannelCmd::CreateDataChannelForStripe(id, group, index, count) => {
                        self.open_stripe_channel(id, group, index, count);
                    }
                    // The server will never forward that group: the stripes
                    // parked for it are capacity nobody can use.
                    ControlChannelCmd::ReleaseStripedGroup(id, group) => {
                        self.release_stripe_group(id, group);
                    }
                    ControlChannelCmd::HeartBeat => (),
                    // What the server offered one of this claim's lanes, and
                    // what that lane's queue refused there. Recorded against
                    // the lane, on the hub that owns it: the loss on a
                    // transparent claim happens where the packets are offered
                    // (the server), so this is the only view of it this end
                    // will ever have.
                    ControlChannelCmd::CarrierReport(id, lane, offered, refused, seq) => {
                        self.note_lane_report(id, lane, offered, refused, seq);
                    }
                    // The peer's half of a tunnel's ARQ health: what the
                    // server's session for that tunnel emitted and re-sent.
                    // Without it the carrier rule sees only what this end sent,
                    // which on a downstream workload is nothing (HANDOFF.md).
                    #[cfg(feature = "multiplex")]
                    ControlChannelCmd::TunnelHealth(id, tunnel, out, resent, handed, seq) => {
                        self.note_tunnel_report(id, tunnel, out, resent, handed, seq);
                    }
                    // No tunnels exist without `multiplex`, so no report can
                    // arrive: the arm exists for exhaustiveness alone.
                    #[cfg(not(feature = "multiplex"))]
                    ControlChannelCmd::TunnelHealth(..) => {}
                    // The peer's half of a lane retirement: it holds no flow in
                    // the lane, or it does not know the lane at all. Recorded
                    // for the allocator, which is what may now retire it.
                    ControlChannelCmd::LaneDrainAck(id, lane, state) => {
                        self.note_lane_drain(id, lane, state);
                    }
                    // The server lost that service's listener; re-register it.
                    ControlChannelCmd::ServiceDropped(id) => {
                        self.pending_drops.push(id);
                        self.drain_dropped(&nonce, &mut rd, &mut wr).await?;
                    }
                    // Tag 0 is the v3 command: a server that answers in that
                    // dialect is a mismatch this client must report, not
                    // mis-read as an ack frame.
                    ControlChannelCmd::CreateDataChannel => {
                        bail!(
                            "Protocol violation: the server sent the v3-only command \
                             `CreateDataChannel` on a v{CURRENT_PROTO_VERSION} control session"
                        );
                    }
                },
                req = self.req_rx.recv() => match req {
                    Some(SessionRequest::Register(slot)) => {
                        let id = slot.id;
                        self.services.insert(id, ServiceEntry { slot, state: ServiceState::Registering });
                        self.register(id, &nonce, &mut rd, &mut wr).await?;
                        self.drain_dropped(&nonce, &mut rd, &mut wr).await?;
                    }
                    Some(SessionRequest::Deregister(id)) => {
                        self.deregister(id, &mut wr).await?;
                    }
                    #[cfg(mh_l3_client)]
                    Some(SessionRequest::DrainLane { service, lane, cancel }) => {
                        let cmd = if cancel {
                            SessionCmd::CancelDrain(service, lane)
                        } else {
                            SessionCmd::DrainLane(service, lane)
                        };
                        write_session_cmd(&mut wr, &cmd).await?;
                    }
                    // The client is gone.
                    None => break,
                },
                ask = self.ask_rx.recv() => self.answer_ask(ask),
                () = wait_for_heartbeat(heartbeat_timeout) => {
                    bail!(
                        "Heartbeat timed out after {} seconds",
                        heartbeat_timeout.unwrap_or_default().as_secs()
                    );
                }
                _ = &mut self.shutdown_rx => break,
            }
        }

        info!("Control session shutdown");
        Ok(())
    }

    /// A reconnect starts from nothing: the server has forgotten every service
    /// of this session, so they all go back to `Registering` — a service the
    /// server once refused stays refused — and the ids to re-register come
    /// back in order.
    ///
    /// The tunnel pools go with the connection they were built on: their
    /// tunnels carry the *previous* session nonce and died with it, so a pool
    /// kept across a reconnect can only refuse streams, and its growth would be
    /// rejected as a stale nonce. Each service builds a fresh pool when it
    /// re-activates.
    fn services_to_reregister(&mut self) -> Vec<ServiceId> {
        #[cfg(feature = "multiplex")]
        self.pools.clear();
        for entry in self.services.values_mut() {
            if !matches!(entry.state, ServiceState::Rejected) {
                entry.state = ServiceState::Registering;
            }
        }
        let mut ids: Vec<ServiceId> = self
            .services
            .iter()
            .filter(|(_, e)| matches!(e.state, ServiceState::Registering))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The session handshake: the hello that names the session, the server's
    /// v4 answer, the endpoint's credential, and the ack that declares the
    /// server's heartbeat cadence.
    ///
    /// Returns the nonce the server issued — the data plane's credential — and
    /// the cadence it declared.
    async fn handshake(&mut self, conn: &mut BufReader<ClientStream>) -> Result<(Nonce, u64)> {
        // A random session tag: the session's identity is no longer derivable
        // from a service name the way a v3 digest was.
        let mut tag = [0u8; HASH_WIDTH_IN_BYTES];
        rand::rngs::SysRng.try_fill_bytes(&mut tag)?;
        debug!("Sending hello");
        let hello_send = ControlChannelHello(CURRENT_PROTO_VERSION, tag);
        conn.write_all(&postcard::to_stdvec(&hello_send)?).await?;
        conn.flush().await?;

        // Read hello. A server older than this client fails its version check
        // and closes without a reply, which surfaces here as a read error; the
        // version is checked as well, because a peer that answers in another
        // dialect would otherwise be discovered halfway through the session, or
        // not at all.
        debug!("Reading hello");
        let (version, hello) = read_hello(conn)
            .await
            .map_err(|e| unanswered_hello_error(&self.remote_addr, &e))?;
        if !SUPPORTED_PROTO_VERSIONS.contains(&version) {
            return Err(SessionFatal(format!(
                "The server answered in protocol v{version}, which this client does not accept: \
                 this build speaks protocol v{CURRENT_PROTO_VERSION} and accepts \
                 {SUPPORTED_PROTO_VERSIONS:?}. Upgrade the server to this version, or run a client \
                 that matches it."
            ))
            .into());
        }
        let ControlChannelHello(_, nonce) = hello else {
            bail!("Unexpected type of hello");
        };

        // Send auth: the endpoint's default token bound to the nonce.
        debug!("Sending auth");
        let mut concat = Vec::from(self.token.as_bytes());
        concat.extend_from_slice(&nonce);
        conn.write_all(&postcard::to_stdvec(&Auth(protocol::digest(&concat)))?)
            .await?;
        conn.flush().await?;

        // The server answers success through the framed path (`Ack::SessionOk`
        // carries the declared cadence) and a refusal as the bare one-byte
        // `Ack::AuthFailed` — peek one byte to read the shape that arrived.
        debug!("Reading ack");
        let framed = match conn
            .fill_buf()
            .await
            .with_context(|| "Failed to read the session ack")?
            .first()
        {
            // A framed ack is u16-length-prefixed, and every payload the
            // server sends fits in 255 bytes, so its first byte is zero.
            Some(0) => true,
            Some(_) => false,
            None => {
                return Err(SessionFatal(format!(
                    "The server at {} closed the connection instead of answering the session \
                     authentication. Check the server's log; the credential is \
                     `[client].default_token`.",
                    self.remote_addr
                ))
                .into());
            }
        };
        let ack = if framed {
            read_register_result(conn).await?
        } else {
            read_ack(conn).await?
        };
        match ack {
            Ack::SessionOk {
                heartbeat_interval_secs,
            } => Ok((nonce, heartbeat_interval_secs)),
            Ack::AuthFailed => Err(SessionFatal(format!(
                "Authentication failed: the server at {} rejected `[client].default_token`. \
                 It must match the server's `[server].default_token`.",
                self.remote_addr
            ))
            .into()),
            v => bail!("Unexpected session ack: {v}"),
        }
    }

    /// The session's heartbeat timeout, from the cadence the server declared
    /// and the client-wide `[client.control].default_heartbeat_timeout` (see
    /// [`resolve_heartbeat_timeout`]).
    fn heartbeat_timeout(&self, interval: u64) -> Result<Option<Duration>> {
        resolve_heartbeat_timeout(interval, self.default_heartbeat_timeout)
    }

    /// Answer one status ask, or drop the channel when the asker is gone.
    fn answer_ask(&self, ask: Option<crate::status::SessionAsk>) {
        match ask {
            Some(crate::status::SessionAsk::Snapshot(tx)) => {
                let _ = tx.send(self.status_view());
            }
            // The status task is gone: nothing asks again.
            None => {}
        }
    }

    /// This session's own answer to a status ask: its services and its
    /// transparent claims.
    ///
    /// Built from the same maps the session loop mutates, under no lock that
    /// matters — the ask arrives on the session's own select branch, so the
    /// read cannot race the write. The tunnel pools are *not* answered here:
    /// they are a process registry (`live_pools`) the status task reads
    /// directly, and one pool per session key means a session-keyed answer
    /// would only duplicate it.
    fn status_view(&self) -> crate::status::SessionView {
        let services = self
            .services
            .values()
            .map(|entry| {
                let service = &entry.slot.service;
                crate::status::ServiceSnapshot {
                    name: service.name.clone(),
                    kind: crate::status::kind_name(service.service_type),
                    remote_addr: service.remote_bind_addr.clone(),
                    local_addr: service.local_addr.clone(),
                    state: match entry.state {
                        ServiceState::Registering => "registering",
                        ServiceState::Active(_) => "active",
                        ServiceState::Rejected => "rejected",
                    },
                }
            })
            .collect();

        #[cfg(mh_l3_client)]
        let claims = self
            .lane_pool
            .as_ref()
            .map(|pool| {
                let claims: HashMap<String, (String, u16)> = self
                    .services
                    .values()
                    .map(|entry| {
                        (
                            entry.slot.service.remote_bind_addr.clone(),
                            (
                                entry.slot.service.name.clone(),
                                entry.slot.service.transparent_lanes,
                            ),
                        )
                    })
                    .collect();
                pool.snapshot()
                    .0
                    .into_iter()
                    .map(|load| {
                        let endpoint = load.endpoint.to_string();
                        let (name, configured_lanes) = claims
                            .get(&endpoint)
                            .cloned()
                            .unwrap_or_else(|| (endpoint.clone(), 0));
                        crate::status::ClaimSnapshot {
                            name,
                            endpoint,
                            configured_lanes: Some(configured_lanes),
                            lanes: load
                                .lanes
                                .into_iter()
                                .map(|lane| crate::status::LaneStatus {
                                    slot: lane.slot,
                                    flows: lane.flows,
                                    handover: lane.handover,
                                })
                                .collect(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        #[cfg(not(mh_l3_client))]
        let claims = Vec::new();

        #[cfg(feature = "multiplex")]
        let pool_key = self.pools.keys().next().cloned();
        #[cfg(not(feature = "multiplex"))]
        let pool_key = None;

        crate::status::SessionView {
            services,
            claims,
            pool_key,
        }
    }

    /// Apply every request already queued, without waiting for one.
    ///
    /// Called once a connection is up, before the re-registration sweep: a
    /// service the client deleted while this session was connecting must not
    /// be re-registered for a round.
    fn apply_pending_requests(&mut self) {
        while let Ok(req) = self.req_rx.try_recv() {
            match req {
                SessionRequest::Register(slot) => {
                    let id = slot.id;
                    self.services.insert(
                        id,
                        ServiceEntry {
                            slot,
                            state: ServiceState::Registering,
                        },
                    );
                }
                SessionRequest::Deregister(id) => {
                    #[cfg(mh_l3_client)]
                    self.forget_claim(id);
                    self.services.remove(&id);
                }
                // A drain ask that arrives while the session is (re)connecting
                // is about lanes of the *previous* connection: they are gone
                // with it, and the allocator re-asks on its next tick if it
                // still wants one.
                #[cfg(mh_l3_client)]
                SessionRequest::DrainLane { .. } => {}
            }
        }
    }

    /// Send one `SessionCmd::Register` and wait for the server's verdict.
    ///
    /// The verdict is that service's own outcome: a rejection marks the
    /// service and leaves the session and its siblings running (D2).
    async fn register<R, W>(
        &mut self,
        id: ServiceId,
        nonce: &Nonce,
        rd: &mut R,
        wr: &mut W,
    ) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Some(entry) = self.services.get(&id) else {
            return Ok(());
        };
        let name = entry.slot.service.name.clone();
        let reg = entry.slot.registration(*nonce)?;
        write_session_cmd(wr, &SessionCmd::Register(reg)).await?;

        debug!(service = %name, "Waiting for the registration result");
        match self.await_verdict(rd).await? {
            Ack::Ok => {}
            Ack::RegisterRejected(reason) => {
                // Per-service and terminal: the session and the siblings are
                // untouched, and a retry would only spam the server.
                // The reason is the server's own sentence and names what it
                // refused for (`allow_ports`, the missing `[server.transparent]`
                // switch, a claimed address, ...), so the advice stays generic:
                // this end cannot know which policy answered.
                error!(
                    "Server rejected service {name}: {reason}. Giving up on that service; fix \
                     what the reason names — in the client config or in the server's policy — \
                     then reload."
                );
                if let Some(entry) = self.services.get_mut(&id) {
                    entry.state = ServiceState::Rejected;
                }
                return Ok(());
            }
            v @ Ack::AuthFailed => {
                bail!("Unexpected registration result: {v}");
            }
            v @ Ack::SessionOk { .. } => bail!(
                "Protocol violation: the server answered a service registration with a session \
                 ack ({v})"
            ),
            v @ Ack::TunnelRefused => bail!(
                "Protocol violation: the server answered a service registration with a tunnel \
                 refusal ({v})"
            ),
        }

        self.activate(id, nonce).await
    }

    /// Wait for the verdict of the registration in flight.
    ///
    /// The verdict is not necessarily the next thing on the wire: the
    /// session's writer is a single queue shared by every service, so a
    /// sibling's `CreateDataChannelFor` can be queued between a `Register` and
    /// its ack — the server only orders the verdict against *that service's*
    /// own commands. Reading the next frame as an ack would then misparse a
    /// command, so the shapes are told apart first: a framed ack starts with
    /// its length prefix's high byte, which is zero for every ack the server
    /// sends, while a v4 command's tag is 1..=3.
    ///
    /// A `ServiceDropped` is queued rather than served here: re-registering
    /// from inside a registration would nest one verdict wait in another.
    async fn await_verdict<R: AsyncBufRead + Unpin>(&mut self, rd: &mut R) -> Result<Ack> {
        loop {
            let framed = {
                let peek = rd
                    .fill_buf()
                    .await
                    .with_context(|| "Failed to read the registration result")?;
                match peek.first() {
                    Some(0) => true,
                    Some(_) => false,
                    None => bail!(
                        "The server closed the control session while a registration was in flight"
                    ),
                }
            };
            if framed {
                return read_register_result(rd).await;
            }
            match read_control_cmd(rd).await? {
                ControlChannelCmd::CreateDataChannelFor(id) => self.open_channel(id),
                ControlChannelCmd::CreateDataChannelForStripe(id, group, index, count) => {
                    self.open_stripe_channel(id, group, index, count);
                }
                ControlChannelCmd::ReleaseStripedGroup(id, group) => {
                    self.release_stripe_group(id, group);
                }
                ControlChannelCmd::HeartBeat => (),
                ControlChannelCmd::ServiceDropped(id) => self.pending_drops.push(id),
                // A report that arrives while a registration is in flight is
                // still the server's view of a live lane: recorded the same
                // way, so a claim does not lose its evidence to a reload.
                ControlChannelCmd::CarrierReport(id, lane, offered, refused, seq) => {
                    self.note_lane_report(id, lane, offered, refused, seq);
                }
                // See the other control loop: the peer's half of a tunnel's ARQ
                // health, routed to the pool that owns the tunnel.
                #[cfg(feature = "multiplex")]
                ControlChannelCmd::TunnelHealth(id, tunnel, out, resent, handed, seq) => {
                    self.note_tunnel_report(id, tunnel, out, resent, handed, seq);
                }
                #[cfg(not(feature = "multiplex"))]
                ControlChannelCmd::TunnelHealth(..) => {}
                // A drain answer that arrives while a registration is in flight
                // is still the peer's half of a retirement: recorded the same
                // way, so a claim does not lose the answer to a reload.
                ControlChannelCmd::LaneDrainAck(id, lane, state) => {
                    self.note_lane_drain(id, lane, state);
                }
                ControlChannelCmd::CreateDataChannel => bail!(
                    "Protocol violation: the server sent the v3-only command `CreateDataChannel` \
                     on a v{CURRENT_PROTO_VERSION} control session"
                ),
            }
        }
    }

    /// Record the peer's health report for one tunnel of one of this session's
    /// pools: what the server's session for that tunnel emitted and re-sent.
    ///
    /// Routed by the tunnel id the client itself assigned (`next_tunnel_id`), so
    /// no per-service ambiguity can arise: the same id is what the tunnel's
    /// stream prologue carries, which is how the server learned to attribute the
    /// report in the first place.
    #[cfg(feature = "multiplex")]
    fn note_tunnel_report(
        &self,
        id: ServiceId,
        tunnel: [u8; 4],
        out: [u8; 4],
        resent: [u8; 4],
        handed: [u8; 4],
        seq: u8,
    ) {
        // Every step that can drop a report says so at debug: a peer's evidence
        // is what the carrier rule acts on, and "the rule never fires" has to be
        // distinguishable from "the evidence never landed" without a rebuild.
        let Some(entry) = self.services.get(&id) else {
            debug!(service = %hex::encode(id.0), "tunnel report for an unknown service");
            return;
        };
        let ServiceState::Active(active) = &entry.state else {
            debug!(service = %hex::encode(id.0), "tunnel report for a service that is not active");
            return;
        };
        let Some(key) = active.pool_key.as_ref() else {
            debug!(service = %hex::encode(id.0), "tunnel report for a service with no pool");
            return;
        };
        let Some(Tunnels::Yamux(pool)) = self.pools.get(key) else {
            debug!(pool = %key, "tunnel report for a pool this session does not hold");
            return;
        };
        let recorded = pool.note_peer_health(
            crate::protocol::TunnelId(tunnel),
            (
                u64::from(u32::from_be_bytes(out)),
                u64::from(u32::from_be_bytes(resent)),
            ),
            u64::from(u32::from_be_bytes(handed)),
            seq,
        );
        // A heartbeat rather than one line per report (they arrive every 50 ms):
        // every twentieth frame shows the summed counters the rule reads, which
        // is what makes "the rule never fires" distinguishable from "the
        // evidence never reached the counters it reads".
        if seq.is_multiple_of(20) {
            debug!(
                tunnel = %hex::encode(tunnel),
                pool = %key,
                summed = ?recorded,
                "tunnel health recorded"
            );
        }
    }

    /// Record the server's view of one lane: what it offered it, and what that
    /// lane's queue refused there.
    ///
    /// The hub is the destination, not a map beside it: the hub is what knows
    /// which slot the lane id belongs to *now*, and a report matched by slot
    /// would land on whatever took the slot next — the exact confusion the lane
    /// id exists to remove. A report for a service that is not (or no longer) a
    /// transparent claim of this session is dropped: it is not this end's
    /// business, and inventing a place for it would make stale evidence
    /// readable.
    #[cfg(mh_l3_client)]
    fn note_lane_report(
        &self,
        id: ServiceId,
        lane: [u8; 4],
        offered: [u8; 4],
        refused: [u8; 4],
        seq: u8,
    ) {
        let Some(entry) = self.services.get(&id) else {
            return;
        };
        if entry.slot.service.service_type != ServiceType::Transparent {
            return;
        }
        let tun = &entry.slot.service.transparent_tun;
        let Ok(hub) = TunHub::get_or_spawn(tun, Direction::Source) else {
            return;
        };
        // The report is matched to the **claim** the service owns — its claimed
        // endpoint — before the lane id is consulted: every claim mints its lane
        // ids from 1 in one shared hub, so a report keyed by lane id alone
        // landed on the first claim holding that number (measured: the ladder's
        // claim's report read as an idle sibling's `peer_offered`), and the
        // carrier policy was reading evidence about nothing.
        let Some(claimed) = entry
            .slot
            .service
            .remote_bind_addr
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|addr| Endpoint::new(addr.ip(), addr.port()))
        else {
            return;
        };
        hub.note_remote(
            claimed,
            LaneId(lane),
            u32::from_be_bytes(offered),
            u32::from_be_bytes(refused),
            seq,
        );
    }

    /// A build without the transparent data path has no lane to report on.
    #[cfg(not(mh_l3_client))]
    #[expect(
        clippy::unused_self,
        reason = "the two shapes of this method are one call site: the build that has lanes \
                  reads them off itself, and the one that does not keeps the same signature"
    )]
    fn note_lane_report(
        &self,
        _id: ServiceId,
        _lane: [u8; 4],
        _offered: [u8; 4],
        _refused: [u8; 4],
        _seq: u8,
    ) {
    }

    /// Record the peer's half of a lane retirement, for the allocator to act on
    /// (see [`LanePool::drain_step`]).
    #[cfg(mh_l3_client)]
    fn note_lane_drain(&self, id: ServiceId, lane: [u8; 4], state: u8) {
        self.lane_drains.set(id, LaneId(lane), state);
    }

    /// A build without the transparent data path has no lane to retire.
    #[cfg(not(mh_l3_client))]
    #[expect(
        clippy::unused_self,
        reason = "the two shapes of this method are one call site: the build that has lanes \
                  records them on itself, and the one that does not keeps the signature"
    )]
    fn note_lane_drain(&self, _id: ServiceId, _lane: [u8; 4], _state: u8) {}

    /// Re-register every service the server reported dropped while this
    /// session was busy answering a registration.
    async fn drain_dropped<R, W>(&mut self, nonce: &Nonce, rd: &mut R, wr: &mut W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        while let Some(id) = self.pending_drops.pop() {
            self.re_register(id, nonce, rd, wr).await?;
        }
        Ok(())
    }

    /// The session's one tunnel pool, created when its first service becomes
    /// active.
    ///
    /// The key is always `session:<data endpoint>`: one control session holds
    /// one pool, so the *second* service of the session finds it here and does
    /// not dial a second one. The key carries the data endpoint because two
    /// services cannot share a physical tunnel to different endpoints.
    #[cfg(feature = "multiplex")]
    async fn service_pool(
        &mut self,
        _id: ServiceId,
        name: &str,
        data_addr: &AddrMaybeCached,
        slot: &ServiceSlot,
        nonce: &Nonce,
    ) -> Result<(Tunnels, String)> {
        // The key carries the data endpoint: two services cannot share a
        // physical tunnel to different endpoints. The carrier is *not* part of
        // it — a pool has one carrier for its lifetime, chosen when it is
        // established, so there is never a second pool on the same key to tell
        // apart.
        let key = format!("session:{data_addr}");
        if let Some(pool) = self.pools.get(&key) {
            debug!(service = %name, pool = %key, "Reusing the session's tunnel pool");
            return Ok((pool.clone(), key));
        }
        let choice = self.carrier_for(data_addr);
        let dial = PoolDial {
            transport: &slot.transport,
            data_addr,
            session_nonce: *nonce,
            opts: &slot.data,
            choice: &choice,
        };
        let tunnel = establish_tunnels(dial, &key, Arc::clone(&self.pins), &choice)
            .await
            .with_context(|| format!("service {name}: failed to establish its tunnel pool"))?;
        self.pools.insert(key.clone(), tunnel.clone());
        Ok((tunnel, key))
    }

    /// The carrier decision for one data endpoint, created on first use.
    ///
    /// Per endpoint rather than per service: the decision is about a network
    /// path, and services that dial the same address share it — including the
    /// one bounded KCP attempt it costs when the path cannot carry KCP.
    #[cfg(any(feature = "multiplex", mh_l3_client))]
    fn carrier_for(&mut self, data_addr: &AddrMaybeCached) -> Arc<CarrierChoice> {
        let key = data_addr.addr.clone();
        Arc::clone(self.carriers.entry(key.clone()).or_insert_with(|| {
            let choice = CarrierChoice::new(key.clone());
            debug!(
                endpoint = %key,
                carrier = choice.carrier().as_str(),
                "data endpoint will try this carrier first"
            );
            choice
        }))
    }

    /// Join one claim to the session's lane allocator, starting it on first use.
    ///
    /// One allocator per session (the lane budget is the client's, so the claims
    /// that share it are the claims a lane can move between), and it is what
    /// lets an idle lane of one claim become a lane of a congested sibling.
    #[cfg(mh_l3_client)]
    fn join_lane_pool(&mut self, endpoint: Endpoint, slot: &ServiceSlot, active: &ActiveService) {
        let share = slot.channels.max(1);
        let lend = slot.service.transparent_lend;
        let pool = Arc::clone(self.lane_pool.get_or_insert_with(|| {
            let pool = Arc::new(LanePool::new(
                lend,
                self.req_tx.clone(),
                Arc::clone(&self.lane_drains),
            ));
            pool.spawn_ticker();
            pool
        }));
        pool.register(
            endpoint,
            slot.service.transparent_tun.clone(),
            Arc::clone(&active.args),
            share,
        );
    }

    /// Turn a registered service into a live one: resolve its data endpoint,
    /// open (or join) its tunnel pool when it multiplexes, and pre-open its
    /// configured channels (D5).
    async fn activate(&mut self, id: ServiceId, nonce: &Nonce) -> Result<()> {
        let Some(entry) = self.services.remove(&id) else {
            return Ok(());
        };
        let ServiceEntry { slot, .. } = entry;
        let name = slot.service.name.clone();
        info!(
            service = %name,
            "Registered, exposed at {}",
            slot.service.remote_bind_addr
        );

        let mut data_addr = AddrMaybeCached::new(&slot.data.addr);
        data_addr.resolve().await.with_context(|| {
            format!(
                "service {name}: failed to resolve the data endpoint {}",
                slot.data.addr
            )
        })?;

        #[cfg(feature = "multiplex")]
        let (tunnel, pool_key) = if slot.data.enabled {
            let (tunnel, key) = self
                .service_pool(id, &name, &data_addr, &slot, nonce)
                .await?;
            (Some(tunnel), Some(key))
        } else {
            (None, None)
        };

        #[cfg(feature = "multiplex")]
        let pins = Arc::clone(&self.pins);
        #[cfg(not(feature = "multiplex"))]
        let pins = Arc::new(());
        // A transparent service's lanes are the claim's carrier connections,
        // and the allocator names the claim by the endpoint it carries. The
        // registration already validated this address (`check_transparent_client`),
        // so a parse that fails here means the config changed under us: the
        // service still runs, it just has no claim to lend lanes to or from.
        #[cfg(mh_l3_client)]
        let lane = if slot.service.service_type == ServiceType::Transparent {
            slot.service
                .remote_bind_addr
                .parse::<SocketAddr>()
                .ok()
                .map(|addr| Endpoint::new(addr.ip(), addr.port()))
        } else {
            None
        };

        // The carrier decision for this service's data endpoint (the same one
        // every service dialing it shares).
        #[cfg(any(feature = "multiplex", mh_l3_client))]
        let choice = self.carrier_for(&data_addr);
        let active = ActiveService {
            args: Arc::new(RunDataChannelArgs {
                session_nonce: *nonce,
                service_id: id,
                remote_addr: data_addr,
                connector: slot.transport.clone(),
                #[cfg(any(feature = "multiplex", mh_l3_client))]
                carrier: Arc::clone(&choice),
                socket_opts: SocketOpts::for_service(),
                service: slot.service.clone(),
                udp: build_udp_hub(&slot.service, pins),
                #[cfg(all(feature = "multiplex", feature = "noise"))]
                noise: slot.data.noise.clone(),
                #[cfg(all(feature = "quic", feature = "noise"))]
                identity: Some(slot.data.identity.clone()),
                #[cfg(feature = "multiplex")]
                channels: std::sync::atomic::AtomicU64::new(0),
                stripes: Arc::new(crate::stripe::StripeGroups::new()),
                #[cfg(feature = "multiplex")]
                stripe_placements: StripePlacements::default(),
                #[cfg(mh_l3_client)]
                lane_ids: std::sync::atomic::AtomicU32::new(1),
            }),
            #[cfg(feature = "multiplex")]
            tunnel,
            #[cfg(feature = "multiplex")]
            pool_key,
            #[cfg(mh_l3_client)]
            lane,
        };
        open_channels(&active, slot.channels);
        // The claim joins the session's allocator, which is what lets an idle
        // lane of one claim become a lane of a congested sibling.
        #[cfg(mh_l3_client)]
        if let Some(endpoint) = lane {
            self.join_lane_pool(endpoint, &slot, &active);
        }
        self.services.insert(
            id,
            ServiceEntry {
                slot,
                state: ServiceState::Active(Box::new(active)),
            },
        );
        Ok(())
    }

    /// One visitor arrived: hand the server one more channel for that service.
    fn open_channel(&self, id: ServiceId) {
        self.open_placed_channel(id, None);
    }

    /// The server asked for one stripe of a named group
    /// (`CreateDataChannelForStripe`): open a channel, reserving a tunnel
    /// this group does not already occupy (D24).
    ///
    /// A command whose metadata contradicts itself is a wire error, not a
    /// channel to open: it is refused here, where nothing has been placed yet.
    fn open_stripe_channel(&self, id: ServiceId, group: [u8; 4], index: u8, count: u8) {
        if count == 0 || index >= count {
            debug!(
                "Ignoring a stripe request for group {group:02x?} that claims stripe {index} of \
                 {count}"
            );
            return;
        }
        // Without the `multiplex` feature the request still opens a channel;
        // the group's identity has no pool to steer there (see `StripeOpen`).
        #[cfg(feature = "multiplex")]
        let stripe = Some(StripeOpen {
            group: u32::from_be_bytes(group),
            count,
        });
        #[cfg(not(feature = "multiplex"))]
        let stripe = None;
        self.open_placed_channel(id, stripe);
    }

    /// The server gave up on a stripe group before forwarding it
    /// (`ReleaseStripedGroup`): drop the stripes it parked here.
    ///
    /// Those channels are pool capacity the server will never use, so they go
    /// back now rather than at the registry's TTL. The group id is tombstoned
    /// on the way out: a stripe the client was already opening when the
    /// release was written must not park after it.
    fn release_stripe_group(&self, id: ServiceId, group: [u8; 4]) {
        if let Some(entry) = self.services.get(&id)
            && let ServiceState::Active(active) = &entry.state
        {
            let released = active.args.stripes.release(u32::from_be_bytes(group));
            debug!("Service {id} released stripe group {group:02x?} ({released} parked stripes)");
        }
    }

    /// Open one data channel for a service the server asked about.
    fn open_placed_channel(&self, id: ServiceId, stripe: Option<StripeOpen>) {
        let Some(entry) = self.services.get(&id) else {
            debug!("The server asked for a data channel for unknown service {id}");
            return;
        };
        match &entry.state {
            ServiceState::Active(active) => {
                // A transparent claim's lanes are allotted, not asked for: the
                // server asks for a replacement whenever one ends, and a lane
                // the allocator retired on purpose is one the claim should not
                // get back (the allocator lowered its target first). A lane that
                // *failed* leaves the claim short, and this dials its
                // replacement.
                #[cfg(mh_l3_client)]
                if let Some(endpoint) = active.lane
                    && let Some(pool) = self.lane_pool.as_ref()
                    && !pool.wants_another_lane(&endpoint)
                {
                    debug!(
                        "Ignoring a data channel request for {id}: the claim holds the lanes it \
                         was allotted"
                    );
                    return;
                }
                open_data_channel(active, stripe);
            }
            ServiceState::Rejected => {
                debug!("Ignoring a data channel request for rejected service {id}");
            }
            ServiceState::Registering => {
                debug!("Ignoring a data channel request for service {id}, still registering");
            }
        }
    }

    /// The server lost that service's listener: register it again so its
    /// public endpoint comes back.
    ///
    /// Only that service is touched: the session and its siblings keep
    /// running (D2).
    async fn re_register<R, W>(
        &mut self,
        id: ServiceId,
        nonce: &Nonce,
        rd: &mut R,
        wr: &mut W,
    ) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Some(entry) = self.services.get_mut(&id) else {
            debug!("The server dropped unknown service {id}");
            return Ok(());
        };
        if matches!(entry.state, ServiceState::Rejected) {
            return Ok(());
        }
        // Drop the old data plane with the old registration: its tunnels are
        // pinned to a listener that no longer exists, and re-registering
        // rebuilds both.
        debug!(service = %entry.slot.service.name, "Service dropped by the server, re-registering");
        entry.state = ServiceState::Registering;
        self.register(id, nonce, rd, wr).await
    }

    /// Tell the lane allocator a service is gone, so its claim stops counting
    /// against its carrier's budget.
    #[cfg(mh_l3_client)]
    fn forget_claim(&self, id: ServiceId) {
        let Some(entry) = self.services.get(&id) else {
            return;
        };
        let ServiceState::Active(active) = &entry.state else {
            return;
        };
        let Some(endpoint) = active.lane else {
            return;
        };
        if let Some(pool) = self.lane_pool.as_ref() {
            pool.unregister(&endpoint);
        }
    }

    /// Drop one service: the server releases its public endpoint and the
    /// session keeps serving its siblings.
    async fn deregister<W: AsyncWrite + Unpin>(&mut self, id: ServiceId, wr: &mut W) -> Result<()> {
        #[cfg(mh_l3_client)]
        self.forget_claim(id);
        if self.services.remove(&id).is_some() {
            debug!("Deregistering service {id}");
            write_session_cmd(wr, &SessionCmd::Deregister(id)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // The failure these tests assert on *is* the assertion; a per-call
    // `.expect()` would bury it under noise.
    #![expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "tests unwrap and expect on values they just constructed"
    )]
    use super::*;

    /// A tunnel's byte counts accumulate per tunnel, and the snapshot is cumulative
    /// so a policy can difference two of them into a window.
    ///
    /// The accumulator itself is covered by the multiplex module's tests (the
    /// counters live on the pool's tunnels there); the *wiring* (counting every
    /// read and write of a mux channel) is exercised by the mux end-to-end
    /// suites, which carry traffic through it.
    #[cfg(all(feature = "multiplex", test))]
    #[test]
    fn tunnel_stats_thread_starts_without_env() {
        // The spawn call is what the old registry's `STARTED.once` did; a run
        // with `MOLEHILL_TUNNEL_STATS` unset must start no thread and never
        // panic — the reporter is strictly opt-in.
        spawn_tunnel_stats();
    }

    /// The literal the interop matrix greps for: a client that meets a server
    /// too old to answer has to *say* which dialect it speaks, and stop.
    #[test]
    fn a_server_that_never_answers_is_reported_as_a_protocol_mismatch() {
        let cause = anyhow!("Failed to read hello: failed to fill whole buffer");
        let err = unanswered_hello_error("127.0.0.1:2333", &cause);
        assert!(
            err.downcast_ref::<SessionFatal>().is_some(),
            "a silent server must be terminal for the session, not retried"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("protocol v{CURRENT_PROTO_VERSION}")),
            "the dialect must be named: {msg}"
        );
        assert!(
            msg.contains("older"),
            "the likely cause must be named: {msg}"
        );
    }

    /// The client-side stripe bookkeeping (D24): a group's tunnels are
    /// remembered while its stripes are placed, the entry goes as soon as the
    /// last stripe is placed, and a group whose gather died server-side cannot
    /// grow the map without bound.
    #[cfg(feature = "multiplex")]
    #[test]
    fn a_stripe_groups_placement_state_lives_exactly_as_long_as_its_stripes() {
        let placements = StripePlacements::default();
        let place = placements.entry(7, 2);
        assert!(
            !place.record(10),
            "the first of two stripes is not the last"
        );
        assert_eq!(place.used(), vec![10], "the tunnel is remembered");
        // A repeated tunnel is not a second entry: `used` is what the pool
        // avoids, and it is a set of tunnels, not a count of stripes.
        assert!(place.record(10), "the second stripe completes the group");
        assert_eq!(place.used(), vec![10]);
        placements.finish(7, &place);
        assert!(
            placements.groups.lock().unwrap().is_empty(),
            "a completed group must be forgotten"
        );

        // An evicted entry does not take a replacement's state with it.
        let fresh = placements.entry(7, 2);
        assert!(fresh.used().is_empty(), "a fresh group starts over");
        placements.finish(7, &place);
        assert!(
            placements.groups.lock().unwrap().contains_key(&7),
            "finishing an old entry must not remove the live one"
        );

        // The bound: the oldest entry gives way, so an abandoned group (whose
        // last stripe is never placed) cannot leak for the session's life.
        let bounded = StripePlacements::default();
        let oldest = bounded.entry(0, 4);
        std::thread::sleep(Duration::from_millis(2));
        for id in 1..u32::try_from(MAX_TRACKED_STRIPE_GROUPS).unwrap() {
            bounded.entry(id, 4);
        }
        assert_eq!(
            bounded.groups.lock().unwrap().len(),
            MAX_TRACKED_STRIPE_GROUPS
        );
        bounded.entry(999, 4);
        let groups = bounded.groups.lock().unwrap();
        assert_eq!(groups.len(), MAX_TRACKED_STRIPE_GROUPS, "the cap holds");
        assert!(
            !groups.contains_key(&0),
            "the oldest entry is the one evicted"
        );
        assert!(groups.contains_key(&999), "the new group is tracked");
        assert!(
            oldest.used().is_empty(),
            "the evicted entry keeps its own (empty) state, it does not alias the new one"
        );
    }

    /// `udp_send_queue_size` is the bound the peer's outbound queue actually
    /// enforces, and a full queue *drops*: the local service's replies go to
    /// the visitor through a data channel whose writer can park (a mux
    /// stream's window closes on a slow path), and a parked writer must cost
    /// datagrams rather than stall the peer's socket — which is why
    /// `send_outbound` only ever uses `try_send`.
    ///
    /// This is a unit test on purpose. The queue-full state is not reachable
    /// end to end on a loopback pair: the server's UDP worker reads the data
    /// channel continuously, so a real run never keeps the writer parked long
    /// enough to overflow the queue, and a test that tried would assert on
    /// scheduling rather than on the bound. Here the bound is driven where it
    /// lives — nothing drains the queue the hub writes to — so the number of
    /// datagrams that fit is exact.
    #[tokio::test]
    async fn the_udp_send_queue_holds_exactly_its_configured_size() {
        let service = ClientServiceConfig {
            name: "game".to_owned(),
            service_type: ServiceType::Udp,
            local_addr: "127.0.0.1:8103".to_owned(),
            remote_bind_addr: "127.0.0.1:2354".to_owned(),
            ..Default::default()
        };
        // The session's pin registry: a real one with tunnels, the unit handle
        // without them. `Default` is the one spelling that fits both builds
        // (and the one the `unit_arg` lint accepts).
        let pins: Arc<Pins> = Arc::default();
        let hub = build_udp_hub(&service, pins).expect("a UDP service must build a hub");

        // The queue `run_data_channel_for_udp` creates for one channel's
        // writer, with nobody draining it: a writer parked on a closed window.
        let (tx, mut rx) = mpsc::channel::<UdpTraffic>(2);
        hub.register_channel(tx).await;

        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let traffic = |i: u8| UdpTraffic {
            from: peer,
            data: Bytes::from(vec![i]),
        };
        // Four sends into a queue of two. A blocking send would park here for
        // ever, so the timeout is the assertion that the path never waits.
        tokio::time::timeout(Duration::from_secs(5), async {
            for i in 0..4 {
                hub.send_outbound(peer, traffic(i), 0).await;
            }
        })
        .await
        .expect("a full UDP send queue must drop, never park the sender");

        let queued: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|t| t.data[0])
            .collect();
        assert_eq!(
            queued,
            vec![0, 1],
            "exactly the queue capacity is queued (in order); the rest are dropped"
        );
    }

    #[test]
    fn heartbeat_floor_is_two_beats_plus_slack() {
        // A 30 s cadence (the documented default) needs 65 s to survive two
        // missed beats; a fast one never drops below 10 s.
        assert_eq!(derived_heartbeat_floor(30), Some(Duration::from_secs(65)));
        assert_eq!(derived_heartbeat_floor(5), Some(Duration::from_secs(15)));
        assert_eq!(derived_heartbeat_floor(1), Some(Duration::from_secs(10)));
        assert_eq!(derived_heartbeat_floor(2), Some(Duration::from_secs(10)));
        // No declared cadence means nothing to time out (D11).
        assert_eq!(derived_heartbeat_floor(0), None);
    }

    #[test]
    fn an_unset_timeout_is_derived_from_the_server() {
        assert_eq!(
            resolve_heartbeat_timeout(30, None).unwrap(),
            Some(Duration::from_secs(65))
        );
        // A server that declares no cadence: no timeout either way.
        assert_eq!(resolve_heartbeat_timeout(0, None).unwrap(), None);
        // An explicit 0 disables the check, whatever the server declares.
        assert_eq!(resolve_heartbeat_timeout(30, Some(0)).unwrap(), None);
        // Above the floor is the operator's choice and is honored.
        assert_eq!(
            resolve_heartbeat_timeout(30, Some(90)).unwrap(),
            Some(Duration::from_secs(90))
        );
    }

    /// The contract the configuration page states: a value below the derived
    /// floor is refused with both numbers, not obeyed into a reconnect loop.
    #[test]
    fn a_timeout_below_the_floor_names_both_numbers() {
        for (interval, configured, floor) in [(30u64, 20u64, 65u64), (1, 5, 10), (5, 14, 15)] {
            let err = resolve_heartbeat_timeout(interval, Some(configured)).unwrap_err();
            assert!(
                err.downcast_ref::<SessionFatal>().is_some(),
                "the refusal must be terminal, got {err:#}"
            );
            let msg = format!("{err:#}");
            for needle in [
                format!("{interval} s heartbeat"),
                format!("at least {floor} s"),
                format!("you configured {configured} s"),
            ] {
                assert!(msg.contains(&needle), "{needle:?} missing from {msg:?}");
            }
        }
    }
}
