use crate::common::helper::{datagram_len, udp_connect};
#[cfg(feature = "notify")]
use crate::config::ClientServiceChange;
use crate::config::ConfigChange;
#[cfg(all(feature = "multiplex", feature = "kcp", feature = "noise"))]
use crate::config::NoiseConfig;
use crate::config::{
    ClientConfig, ClientServiceConfig, Config, MaskedString, ServiceType, TransportConfig,
    TransportType,
};
#[cfg(feature = "multiplex")]
use crate::config::{DataCarrier, DataMode};
use crate::logging::RepeatNotice;
use crate::protocol::Hello::{self, ControlChannelHello};
use crate::protocol::{
    self, Ack, Auth, CURRENT_PROTO_VERSION, ControlChannelCmd, DataChannelCmd, HASH_WIDTH_IN_BYTES,
    MAX_UDP_HEADER_LEN, SUPPORTED_PROTO_VERSIONS, ServiceId, ServiceRegistration, SessionCmd,
    SessionRegistration, UdpTraffic, read_ack, read_control_cmd, read_data_cmd, read_hello,
    read_register_result, write_session_cmd, write_stream_prologue,
};
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
use crate::transport::multiplex::{Carrier, ClientTunnel, Dialer, StreamLease, TunnelPool};

use crate::common::constants::{
    DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS, DEFAULT_UDP_SENDQ_SIZE,
    DEFAULT_UDP_WORKERS, FORWARD_IDLE_TIMEOUT, TCP_COPY_BUFFER_SIZE, run_control_chan_backoff,
};
use crate::common::forward::copy_bidirectional_with_idle;

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
/// service's own `mode`/`carrier` overrides.
#[derive(Clone, Debug, Default)]
struct DataOpts {
    /// Whether the service multiplexes its data plane onto tunnels. Only the
    /// `multiplex` build has tunnels, so the field does not exist without it.
    #[cfg(feature = "multiplex")]
    enabled: bool,
    /// Endpoint the data plane dials: the service's own `remote_addr` when
    /// set, else `[client.data].default_data_addr`, else the control
    /// channel's
    /// `remote_addr`.
    addr: String,
    /// Which carrier carries the data plane.
    #[cfg(feature = "multiplex")]
    carrier: DataCarrier,
    /// The cap the pool may grow to for this service's carrier
    /// (`[client.data.tcp|kcp].max_tunnels`). The pool starts cold and grows
    /// up to it on demand.
    #[cfg(feature = "multiplex")]
    max_tunnels: usize,
    /// Noise key config, `Some` iff the control transport is `noise`; KCP
    /// tunnels wrap it on top (the crypto stack is kept).
    #[cfg(all(feature = "multiplex", feature = "kcp", feature = "noise"))]
    noise: Option<NoiseConfig>,
}

/// Placeholder so the channel-opening code keeps one shape without the
/// `multiplex` feature (the data plane is always direct there, and no tunnel
/// can exist).
#[cfg(not(feature = "multiplex"))]
struct Tunnels;

/// A data-channel open the elastic pool refused. Reported once per process,
/// then DEBUG: the visitor's failure is per-connection, the condition behind it
/// is the operator's to see (the pool at its ceiling, or a growth the server's
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
enum ClientStream {
    Plain(TcpStream),
    #[cfg(feature = "noise")]
    Noise(Box<crate::transport::NoiseStream<TcpStream>>),
}

impl std::fmt::Debug for ClientStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientStream::Plain(s) => f.debug_tuple("Plain").field(s).finish(),
            #[cfg(feature = "noise")]
            ClientStream::Noise(_) => f.debug_tuple("Noise").finish(),
        }
    }
}

impl ClientStream {
    /// Apply socket options to the underlying TCP socket.
    fn hint(&self, opts: SocketOpts) {
        match self {
            ClientStream::Plain(s) => opts.apply(s),
            #[cfg(feature = "noise")]
            ClientStream::Noise(s) => opts.apply(s.get_inner()),
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
    /// The effective wire config of one service: the client-wide
    /// `[client.transport]` with the service's own `transport` overlay
    /// applied. It is also the session's identity input — two services share
    /// a control connection exactly when their effective config is equal.
    fn effective_config(client: &ClientConfig, service: &ClientServiceConfig) -> TransportConfig {
        let mut cfg = client.transport.clone();
        if let Some(st) = &service.transport {
            if let Some(t) = st.transport_type {
                cfg.transport_type = t;
            }
            if let Some(noise) = &st.noise {
                cfg.noise = Some(noise.clone());
            }
        }
        cfg
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
    /// Resolve the data-plane knobs for one service: `[client.data]` as
    /// defaults, overridden by the service's own `mode`/`carrier`.
    /// The data endpoint follows the service's own server: its `remote_addr`
    /// override when set, else `[client.data].default_data_addr` (or the
    /// client-wide
    /// control endpoint when that is unset either).
    fn for_service(c: &ClientConfig, s: &ClientServiceConfig) -> DataOpts {
        DataOpts {
            #[cfg(feature = "multiplex")]
            enabled: s
                .mode
                .map_or_else(|| c.multiplex_enabled(), |m| m == DataMode::Multiplex),
            addr: s.endpoint_with(c.data_addr()).to_owned(),
            #[cfg(feature = "multiplex")]
            carrier: s.carrier.unwrap_or(c.data.default_carrier),
            #[cfg(feature = "multiplex")]
            max_tunnels: c.max_tunnels(s.carrier.unwrap_or(c.data.default_carrier)),
            #[cfg(all(feature = "multiplex", feature = "kcp", feature = "noise"))]
            noise: match s.transport_type_with(c.transport.transport_type) {
                TransportType::Noise => s.noise_config_with(c.transport.noise.as_ref()).cloned(),
                TransportType::Plain => None,
            },
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
                // Client-declared data-plane carrier; the server validates it
                // against its own capabilities and lazily opens listeners.
                carrier: {
                    #[cfg(feature = "multiplex")]
                    {
                        protocol::Carrier::from_data_carrier(self.data.carrier)
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
    /// The effective `[client.transport]` config (service overlay applied).
    transport: TransportConfig,
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
}

impl Client {
    // Create a Client from `[client]` config block. The transport is
    // resolved per service (see `ClientTransport`), so there is nothing
    // to build here.
    fn from(config: ClientConfig) -> Client {
        Client {
            config,
            sessions: Vec::new(),
            services: HashMap::new(),
        }
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
                        #[cfg(feature = "notify")]
                        self.handle_hot_reload(e);
                        // Without the `notify` feature the config never
                        // changes at runtime; nothing can arrive here.
                        #[cfg(not(feature = "notify"))]
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

        Ok(())
    }

    /// The endpoint and wire stack one service dials: its own `remote_addr`
    /// when it declares one, else the client-wide control endpoint.
    fn session_key(&self, cfg: &ClientServiceConfig) -> SessionKey {
        SessionKey {
            addr: cfg
                .endpoint_with(&self.config.control.default_remote_addr)
                .to_owned(),
            transport: ClientTransport::effective_config(&self.config, cfg),
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
        let transport = Arc::new(ClientTransport::from_config(&key.transport)?);
        let token = cfg
            .token
            .clone()
            .unwrap_or_else(|| self.config.default_token.clone());
        let data = DataOpts::for_service(&self.config, &cfg);
        // The client opens its configured channels itself: a v4 registration
        // asks the server for none. UDP opens its worker set (they are the
        // sharding targets and the pool's UDP floor); TCP opens none, because
        // the server asks for one channel per visitor; a transparent service
        // opens one, long-lived, because every packet for its claimed endpoint
        // rides it and the server asks for a replacement when it ends.
        let channels = match cfg.service_type {
            ServiceType::Tcp => 0,
            ServiceType::Udp => usize::from(cfg.udp_workers.unwrap_or(DEFAULT_UDP_WORKERS)),
            ServiceType::Transparent => 1,
        };
        let retry_interval = cfg.retry_interval.unwrap_or(1);

        // A transparent service's prerequisites are the operator's to satisfy
        // and this process's to verify: it must already own the address it
        // claims, and the kernel must be willing to accept injected packets.
        if cfg.service_type == ServiceType::Transparent {
            check_transparent_client(&cfg)?;
        }

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

        let idx = self.session_index(key.clone(), transport.clone(), retry_interval);
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
        retry_interval: u64,
    ) -> usize {
        if let Some(i) = self.sessions.iter().position(|s| s.key == key) {
            return i;
        }
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let session = ClientSession {
            remote_addr: key.addr.clone(),
            transport,
            token: self.config.default_token.clone(),
            default_heartbeat_timeout: self.config.control.default_heartbeat_timeout,
            req_rx,
            shutdown_rx,
            services: HashMap::new(),
            pending_drops: Vec::new(),
            #[cfg(feature = "multiplex")]
            pools: HashMap::new(),
            #[cfg(feature = "multiplex")]
            shared_pool: self.config.shared_pool(),
            #[cfg(feature = "multiplex")]
            idle_timeout: self.config.pool_idle_timeout(),
            #[cfg(feature = "multiplex")]
            pins: Arc::new(crate::transport::multiplex::PinRegistry::new()),
        };
        tokio::spawn(
            async move {
                session.drive(retry_interval).await;
            }
            .instrument(Span::current()),
        );
        self.sessions.push(SessionEntry {
            key,
            handle: ClientSessionHandle {
                req_tx,
                shutdown_tx,
            },
            next_id: 0,
        });
        self.sessions.len() - 1
    }

    /// Apply one client-service config change (add or remove a service).
    #[cfg(feature = "notify")]
    fn handle_hot_reload(&mut self, e: ConfigChange) {
        match e {
            ConfigChange::ClientChange(client_change) => match *client_change {
                ClientServiceChange::Add(cfg) => {
                    let name = cfg.name.clone();
                    if let Err(e) = self.add_service(*cfg) {
                        warn!("Failed to start service {name}: {e:#}");
                    }
                }
                ClientServiceChange::Delete(s) => {
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
    socket_opts: SocketOpts,
    service: ClientServiceConfig,
    /// Mints this service session's data-channel ids: the key the UDP pin
    /// accounting uses to name the tunnel a peer's route ends on.
    #[cfg(feature = "multiplex")]
    channels: std::sync::atomic::AtomicU64,
    /// Shared UDP hub for the service; `Some` iff this is a UDP service.
    udp: Option<Arc<UdpHub>>,
    /// Stripe-group registry of the service session: stripes of one
    /// visitor connection arrive as independent data channels and park
    /// here until the group is complete (see [`crate::stripe`]).
    #[cfg(feature = "multiplex")]
    stripes: Arc<crate::stripe::StripeGroups<ClientDataChannel>>,
    /// Placement state of this service's stripe groups, by group id (D24).
    #[cfg(feature = "multiplex")]
    stripe_placements: StripePlacements,
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
/// hands the list to the pool, which grows to the group's K and avoids them —
/// so the group's K data channels land on K distinct tunnels, a cold pool
/// included. Without it, K requests were K indistinguishable opens and a cold
/// pool (whose default state is *zero* tunnels) put the whole group on one.
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

/// The service session's stripe-group registry type (a no-op stand-in
/// without the `multiplex` feature, which can never receive a striped
/// command).
#[cfg(feature = "multiplex")]
type StripeRegistry = crate::stripe::StripeGroups<ClientDataChannel>;
#[cfg(not(feature = "multiplex"))]
type StripeRegistry = ();

impl RunDataChannelArgs {
    /// The session's stripe-group registry. Without the `multiplex` feature
    /// a striped command can never arrive, and the registry degenerates to
    /// a unit reference the (unreachable) striped arm ignores.
    #[cfg(feature = "multiplex")]
    fn stripes(&self) -> &StripeRegistry {
        &self.stripes
    }

    #[cfg(not(feature = "multiplex"))]
    #[cfg_attr(
        not(feature = "multiplex"),
        allow(
            clippy::unused_self,
            reason = "the unit stand-in has no field to read; the multiplex arm keeps the method shape"
        )
    )]
    fn stripes(&self) -> &StripeRegistry {
        const UNIT: () = ();
        &UNIT
    }
}

async fn do_data_channel_handshake(args: Arc<RunDataChannelArgs>) -> Result<ClientStream> {
    // Retry at least every 100ms, at most for 10 seconds
    let backoff = ExponentialBuilder::default()
        .with_max_delay(Duration::from_millis(100))
        .with_total_delay(Some(Duration::from_secs(10)));

    // Connect to remote_addr
    let mut conn: ClientStream = (|| async {
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

    conn.hint(args.socket_opts);

    // Send nonce, then the v4 service prologue: a session carries N services,
    // so the channel names the one it was opened for and the server reads it
    // before anything else can follow.
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, args.session_nonce);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;
    write_stream_prologue(&mut conn, args.service_id).await?;

    Ok(conn)
}

async fn run_data_channel(args: Arc<RunDataChannelArgs>) -> Result<()> {
    // Do the handshake
    let conn = do_data_channel_handshake(args.clone()).await?;

    // Forward
    forward_data_channel(
        ClientDataChannel::Raw(conn),
        &args.service,
        args.udp.clone(),
        args.stripes(),
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
    /// Open the next data channel: a stream from the elastic pool, placed
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
    /// service, setting its UDP floor, or reading its state.
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
    // service it carries: the server reads these four bytes before its
    // forwarding command. Several services may share the tunnel — that is
    // what a shared pool produces — so the routing is per stream.
    write_stream_prologue(&mut stream, args.service_id).await?;
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

impl tokio::io::AsyncRead for ClientDataChannel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientDataChannel::Raw(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "multiplex")]
            ClientDataChannel::Mux(s) => std::pin::Pin::new(s).poll_read(cx, buf),
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
            ClientDataChannel::Mux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
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
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            unused_variables,
            reason = "only the multiplex build can receive striped commands"
        )
    )]
    stripes: &StripeRegistry,
) -> Result<()> {
    let sock_opts = SocketOpts::from_client_cfg(service);
    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardTcp => {
            if service.service_type != ServiceType::Tcp {
                bail!("Expect TCP traffic. Please check the configuration.")
            }
            run_data_channel_for_tcp(conn, &service.local_addr, sock_opts).await?;
        }
        DataChannelCmd::StartForwardUdp => {
            if service.service_type != ServiceType::Udp {
                bail!("Expect UDP traffic. Please check the configuration.")
            }
            let hub = udp_hub
                .ok_or_else(|| anyhow!("Service {} has no UDP forwarding hub", service.name))?;
            run_data_channel_for_udp(conn, hub).await?;
        }
        #[cfg(feature = "multiplex")]
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
                crate::stripe::spawn_group(read, write, done.streams);
            }
        }
        #[cfg(not(feature = "multiplex"))]
        DataChannelCmd::StartForwardStripedTcp(..) => {
            bail!(
                "This binary was built without the `multiplex` feature, so it cannot forward striped data channels"
            );
        }
        DataChannelCmd::StartForwardTransparent => {
            if service.service_type != ServiceType::Transparent {
                bail!("Expect transparent traffic. Please check the configuration.")
            }
            #[cfg(all(feature = "transparent", target_os = "linux"))]
            run_transparent_channel(conn, service).await?;
            #[cfg(not(all(feature = "transparent", target_os = "linux")))]
            {
                let _ = (conn, service);
                bail!(
                    "This build cannot carry a transparent service: it needs Linux and the \
                     `transparent` feature"
                );
            }
        }
    }
    Ok(())
}

/// Inject what the tunnel sends for this service's claimed endpoint, and hand
/// the kernel's return traffic back.
#[cfg(all(feature = "transparent", target_os = "linux"))]
async fn run_transparent_channel(
    conn: ClientDataChannel,
    service: &ClientServiceConfig,
) -> Result<()> {
    use crate::transparent::hub::{TunHub, forward_transparent};
    use crate::transparent::{Endpoint, Stats};

    let addr: SocketAddr = service.remote_bind_addr.parse().with_context(|| {
        format!(
            "service {}: invalid `remote_bind_addr`: {:?}",
            service.name, service.remote_bind_addr
        )
    })?;
    let stats = Arc::new(Stats::default());
    crate::transparent::spawn_stats_reporter("client", Arc::clone(&stats));
    let hub = TunHub::get_or_spawn(
        &service.transparent_tun,
        Arc::clone(&stats),
        crate::transparent::Direction::Source,
    )?;
    forward_transparent(
        conn,
        hub,
        Endpoint::new(addr.ip(), addr.port()),
        Arc::clone(&stats),
    )
    .await
}

/// The client's side of a transparent service's contract, checked before the
/// service registers rather than after its first packet disappears.
#[cfg(all(feature = "transparent", target_os = "linux"))]
fn check_transparent_client(cfg: &ClientServiceConfig) -> Result<()> {
    let addr: SocketAddr = cfg.remote_bind_addr.parse().with_context(|| {
        format!(
            "service {}: invalid `remote_bind_addr`: {:?}",
            cfg.name, cfg.remote_bind_addr
        )
    })?;
    crate::transparent::check::check_client(&cfg.transparent_tun, &[addr.ip()]).with_context(|| {
        format!(
            "service {}: transparent prerequisites are not met",
            cfg.name
        )
    })
}

#[cfg(not(all(feature = "transparent", target_os = "linux")))]
fn check_transparent_client(_cfg: &ClientServiceConfig) -> Result<()> {
    bail!(
        "This build cannot carry a transparent service: it needs Linux and the `transparent` feature"
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
    Ok((tunnel, shutdown_tx))
}

/// Establish one tunnel's pool: **cold**, plus the dialer every growth uses.
///
/// There is no initial size any more (`default_count` / `count` are gone): the
/// pool starts at zero and its first open grows it synchronously
/// (`GrowReason::Cold`), so the client pays one tunnel setup on the first
/// visitor after an idle period instead of keeping N connections warm for
/// every configured service.
///
/// The carrier follows `[client.data].default_carrier`: `tcp` dials the data
/// endpoint with the control-channel wire stack, `kcp` opens KCP-over-UDP
/// sessions (optionally Noise-wrapped). The pool's identity is `key`:
/// `session` for a shared pool, `service:<name>` otherwise.
///
/// The returned senders shut the tunnel drivers down when dropped; a tunnel
/// the pool *grows* carries its own sender inside the pool, so a shrink can
/// release it.
#[cfg(feature = "multiplex")]
async fn establish_tunnels(
    transport: &Arc<ClientTransport>,
    data_addr: &AddrMaybeCached,
    session_nonce: Nonce,
    opts: &DataOpts,
    key: &str,
    idle_timeout: Duration,
    pins: Arc<crate::transport::multiplex::PinRegistry>,
) -> Result<Tunnels> {
    let carrier = match opts.carrier {
        DataCarrier::Tcp => Carrier::Tcp,
        DataCarrier::Kcp => Carrier::Kcp,
    };
    let dialer = tunnel_dialer(transport, data_addr, session_nonce, opts).await?;
    // Cold by construction: resolve the dialer (a KCP growth must not block on
    // DNS later) and leave the pool empty until an open asks for a tunnel.
    let initial = Vec::new();
    debug!(
        pool = %key,
        carrier = carrier.as_str(),
        max_tunnels = opts.max_tunnels,
        "Multiplexed data tunnel pool established cold"
    );
    Ok(Tunnels::Yamux(TunnelPool::with_dialer(
        carrier,
        key.to_owned(),
        initial,
        opts.max_tunnels,
        idle_timeout,
        Some(dialer),
        pins,
    )))
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
) -> Result<Dialer> {
    match opts.carrier {
        DataCarrier::Tcp => {
            let transport = Arc::clone(transport);
            let data_addr = data_addr.clone();
            Ok(Arc::new(move || {
                let transport = Arc::clone(&transport);
                let data_addr = data_addr.clone();
                Box::pin(async move {
                    establish_tunnel(&transport, &data_addr, session_nonce, "pool")
                        .await
                        .map_err(|e| format!("{e:#}"))
                })
            }))
        }
        #[cfg(feature = "kcp")]
        DataCarrier::Kcp => {
            // Resolved once, at pool creation: a KCP growth must not block on
            // DNS inside the placement path.
            let remote_addr = {
                let mut probe = AddrMaybeCached::new(&opts.addr);
                probe.resolve().await.with_context(|| {
                    format!("Failed to resolve the KCP data endpoint {}", opts.addr)
                })?;
                probe.socket_addr.ok_or_else(|| {
                    anyhow!("The KCP data endpoint {} resolved to nothing", opts.addr)
                })?
            };
            let opts = opts.clone();
            Ok(Arc::new(move || {
                let opts = opts.clone();
                Box::pin(async move {
                    tokio::time::timeout(
                        KCP_ESTABLISH_TIMEOUT,
                        establish_one_kcp_tunnel(remote_addr, session_nonce, &opts, "pool"),
                    )
                    .await
                    .map_err(|_| "KCP tunnel establishment timed out".to_owned())?
                    .map_err(|e| format!("{e:#}"))
                })
            }))
        }
        // Config validation rejects `carrier = "kcp"` without the feature,
        // so this arm is unreachable in a correctly loaded configuration.
        #[cfg(not(feature = "kcp"))]
        DataCarrier::Kcp => bail!("This binary was built without the `kcp` feature"),
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
    use crate::transport::kcp::{self, KcpTunnelStream};
    use rand::TryRng;

    // Random per-session conversation id; together with the ephemeral client
    // port it keeps the server's session map collision-free across restarts.
    let mut rng = rand::rngs::SysRng;
    let mut conv_bytes = [0u8; 4];
    rng.try_fill_bytes(&mut conv_bytes)
        .with_context(|| "Failed to generate a KCP conversation id")?;
    let conv = u32::from_le_bytes(conv_bytes);

    let stream = kcp::connect(remote_addr, conv).await?;
    let mut io = {
        #[cfg(feature = "noise")]
        if let Some(cfg) = &opts.noise {
            let keys = crate::transport::NoiseKeys::from_config(cfg)?;
            // Full handshake + v3 selector byte (the wrapper owns the
            // selector): KCP tunnels establish once per control session,
            // so a resume attempt has nothing cached yet and would only
            // add a round trip.
            KcpTunnelStream::Noise(Box::new(keys.wrap_initiator_full(stream).await?))
        } else {
            let mut s = stream;
            s.write_all(&[crate::protocol::PLAIN_SELECTOR]).await?;
            KcpTunnelStream::Plain(s)
        }
        #[cfg(not(feature = "noise"))]
        KcpTunnelStream::Plain(stream)
    };

    let hello = Hello::DataChannelTunnelHello(CURRENT_PROTO_VERSION, session_nonce);
    io.write_all(&postcard::to_stdvec(&hello)?).await?;
    io.flush().await?;
    match read_ack(&mut io).await? {
        Ack::Ok => {}
        v => bail!("Service {service_name}: the server refused the KCP data tunnel: {v}"),
    }

    let config = crate::transport::multiplex::mux_config();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    Ok((ClientTunnel::start(io, config, shutdown_rx), shutdown_tx))
}

// Runtime-resolved per-service UDP options. Validation fills the defaults;
// the fallbacks here only guard against a missed validation.
fn udp_buffer_size(s: &ClientServiceConfig) -> usize {
    s.udp_buffer_size
        .map_or(DEFAULT_UDP_BUFFER_SIZE, |v| v as usize)
}

fn udp_idle_timeout_secs(s: &ClientServiceConfig) -> u64 {
    s.udp_idle_timeout.unwrap_or(DEFAULT_UDP_IDLE_TIMEOUT_SECS)
}

fn udp_send_queue_size(s: &ClientServiceConfig) -> usize {
    s.udp_send_queue_size
        .map_or(DEFAULT_UDP_SENDQ_SIZE, |v| v as usize)
}

// Simply copying back and forth for TCP
#[instrument(skip_all)]
async fn run_data_channel_for_tcp<S>(
    mut conn: S,
    local_addr: &str,
    sock_opts: SocketOpts,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    debug!("New data channel starts forwarding");

    let mut local = TcpStream::connect(local_addr)
        .await
        .with_context(|| format!("Failed to connect to {local_addr}"))?;
    // The leg towards the local service needs explicit socket options;
    // without them Nagle stays enabled and interactive traffic stalls.
    sock_opts.apply(&local);
    // A stalled forward is closed by the watchdog rather than left to hold a
    // tunnel stream for the session's lifetime (see `FORWARD_IDLE_TIMEOUT`).
    if let Err(e) = copy_bidirectional_with_idle(
        &mut conn,
        &mut local,
        TCP_COPY_BUFFER_SIZE,
        FORWARD_IDLE_TIMEOUT,
    )
    .await
    {
        // A reaped connection is a failed request, not a lifecycle event:
        // DEBUG, like every other per-connection failure.
        debug!("Data channel closed: {e}");
    }
    Ok(())
}

/// Creation-time parameters for the per-service UDP hub, resolved once from
/// the service configuration per control-channel session.
struct UdpForwardParams {
    local_addr: String,
    udp_forwarder_ipv6: bool,
    buffer_size: usize,
    idle_timeout_secs: u64,
    sendq_size: usize,
}

/// One remote peer's forwarding state.
struct UdpVisitorRoute {
    /// Queue into the peer's forwarder task (datagrams bound for the local
    /// service).
    inbound: mpsc::Sender<Bytes>,
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
                if let Err(e) = route.inbound.try_send(data) {
                    debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
                }
                return;
            }
        }
        // First datagram from this peer: bind its dedicated local socket
        // outside the lock, then insert the route. Another channel may have
        // created the peer in the meantime; reuse that forwarder.
        let socket = match udp_connect(&me.params.local_addr, me.params.udp_forwarder_ipv6).await {
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
            if let Err(e) = route.inbound.try_send(data) {
                debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
            }
        } else {
            let (inbound_tx, inbound_rx) = mpsc::channel(me.params.sendq_size);
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
            if let Err(e) = inbound_tx.try_send(data) {
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
    let (wr_tx, mut wr_rx) = mpsc::channel::<UdpTraffic>(hub.params.sendq_size);
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
    mut inbound_rx: mpsc::Receiver<Bytes>,
    hub: Arc<UdpHub>,
    from: SocketAddr,
    my_inbound: mpsc::Sender<Bytes>,
    channel_id: u64,
) {
    debug!("Forwarder created");
    let mut buf = BytesMut::zeroed(hub.params.buffer_size);

    loop {
        tokio::select! {
            // Receive from the server
            data = inbound_rx.recv() => {
                match data {
                    Some(data) => {
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

                let t = UdpTraffic{
                    from,
                    data: Bytes::copy_from_slice(&buf[..len])
                };

                hub.send_outbound(from, t, channel_id).await;
            },

            // No traffic for the duration of the idle timeout, clean up the state
            () = time::sleep(Duration::from_secs(hub.params.idle_timeout_secs)) => {
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
                udp_forwarder_ipv6: service.udp_forwarder_ipv6.unwrap_or(false),
                buffer_size: udp_buffer_size(service),
                idle_timeout_secs: udp_idle_timeout_secs(service),
                sendq_size: udp_send_queue_size(service),
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
    /// the configured channel pool warm.
    Active(Box<ActiveService>),
    /// The server refused the registration. Terminal for this service: only a
    /// human can fix the config or the server's `allow_ports`.
    Rejected,
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
    /// The client's shutdown signal.
    shutdown_rx: oneshot::Receiver<u8>,
    /// Services this session carries, by the id the client allocated.
    services: HashMap<ServiceId, ServiceEntry>,
    /// Services the server reported dropped *while a registration was in
    /// flight*. They are re-registered once that registration is done rather
    /// than from inside it, which would nest one verdict wait in another.
    pending_drops: Vec<ServiceId>,
    /// This session's tunnel pools, by key.
    ///
    /// With `[client.data].shared_pool` the key is `session` per carrier, so
    /// every service of the session shares one pool; without it the key is
    /// `service:<id>` per carrier, so each service keeps its own. One code
    /// path, two keys — and the session owns them, because a shared pool must
    /// outlive any single service.
    #[cfg(feature = "multiplex")]
    pools: HashMap<String, Tunnels>,
    /// `[client.data].shared_pool`.
    #[cfg(feature = "multiplex")]
    shared_pool: bool,
    /// `[client.data].idle_timeout`, applied to every pool of this session.
    #[cfg(feature = "multiplex")]
    idle_timeout: Duration,
    /// The client's UDP pin accounting, shared by every hub of this session's
    /// services: a tunnel with pinned peers is never shrunk (D30).
    #[cfg(feature = "multiplex")]
    pins: Arc<crate::transport::multiplex::PinRegistry>,
}

impl ClientSession {
    /// Drive the session until the client shuts it down, reconnecting with the
    /// usual backoff after a *retryable* failure.
    ///
    /// A [`SessionFatal`] error ends the task instead: no retry can fix a
    /// dialect mismatch or a refused credential, and the operator has to act.
    async fn drive(mut self, retry_interval: u64) {
        let backoff_builder = run_control_chan_backoff(retry_interval);
        let mut start = Instant::now();
        let mut retry_backoff = backoff_builder.build();
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
                        // The session ran for at least 3 secs and then dropped
                        retry_backoff = backoff_builder.build();
                        // It was up, so the next failure is news again.
                        retry_notice.clear();
                    }

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
        // A reconnect starts from nothing: the server has forgotten every
        // service of this session, so they all go back to `Registering`. A
        // service the server once refused stays refused.
        //
        // The tunnel pools go with the connection they were built on: their
        // tunnels carry the *previous* session nonce and died with it, so a
        // pool kept across a reconnect can only refuse streams — and its
        // growth would be rejected as a stale nonce. Each service builds a
        // fresh pool when it re-activates below.
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
        for id in ids {
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
                    ControlChannelCmd::HeartBeat => (),
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
                    // The client is gone.
                    None => break,
                },
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
                    self.services.remove(&id);
                }
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
                ControlChannelCmd::HeartBeat => (),
                ControlChannelCmd::ServiceDropped(id) => self.pending_drops.push(id),
                ControlChannelCmd::CreateDataChannel => bail!(
                    "Protocol violation: the server sent the v3-only command `CreateDataChannel` \
                     on a v{CURRENT_PROTO_VERSION} control session"
                ),
            }
        }
    }

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

    /// The tunnel pool this service must use, creating it when the service is
    /// the first of that key to become active.
    ///
    /// The key is `session` for a shared pool (`[client.data].shared_pool`),
    /// so the *second* service of the session finds the pool here and does not
    /// dial a second one; without the flag it is `service:<id>`, so every
    /// service keeps the pool it had. The key also carries the data endpoint
    /// and the carrier: two services cannot share a physical tunnel to
    /// different endpoints, and a KCP pool is not a TCP pool.
    #[cfg(feature = "multiplex")]
    async fn service_pool(
        &mut self,
        id: ServiceId,
        name: &str,
        data_addr: &AddrMaybeCached,
        slot: &ServiceSlot,
        nonce: &Nonce,
    ) -> Result<(Tunnels, String)> {
        let owner = if self.shared_pool {
            "session".to_owned()
        } else {
            format!("service:{id}")
        };
        // The key carries the data endpoint and the carrier as well as the
        // owner: two services cannot share a physical tunnel to different
        // endpoints, and a KCP pool is not a TCP pool.
        let key = format!("{owner}/{}:{}", slot.data.carrier.as_str(), data_addr);
        if let Some(pool) = self.pools.get(&key) {
            debug!(service = %name, pool = %key, "Reusing the session's tunnel pool");
            return Ok((pool.clone(), key));
        }
        let tunnel = establish_tunnels(
            &slot.transport,
            data_addr,
            *nonce,
            &slot.data,
            &key,
            self.idle_timeout,
            Arc::clone(&self.pins),
        )
        .await
        .with_context(|| format!("service {name}: failed to establish its tunnel pool"))?;
        self.pools.insert(key.clone(), tunnel.clone());
        Ok((tunnel, key))
    }

    /// Turn a registered service into a live one: resolve its data endpoint,
    /// open (or join) its tunnel pool when it multiplexes, pre-open its
    /// configured channels (D5), and refresh the pools' UDP-derived floors.
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
        let active = ActiveService {
            args: Arc::new(RunDataChannelArgs {
                session_nonce: *nonce,
                service_id: id,
                remote_addr: data_addr,
                connector: slot.transport.clone(),
                socket_opts: SocketOpts::from_client_cfg(&slot.service),
                service: slot.service.clone(),
                udp: build_udp_hub(&slot.service, pins),
                #[cfg(feature = "multiplex")]
                channels: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "multiplex")]
                stripes: Arc::new(crate::stripe::StripeGroups::new()),
                #[cfg(feature = "multiplex")]
                stripe_placements: StripePlacements::default(),
            }),
            #[cfg(feature = "multiplex")]
            tunnel,
            #[cfg(feature = "multiplex")]
            pool_key,
        };
        open_channels(&active, slot.channels);
        self.services.insert(
            id,
            ServiceEntry {
                slot,
                state: ServiceState::Active(Box::new(active)),
            },
        );
        // A newly active (or newly gone) UDP service changes what the pool
        // must be able to carry (D7).
        #[cfg(feature = "multiplex")]
        self.refresh_pool_floors();
        Ok(())
    }

    /// Recompute every pool's UDP-derived floor (D7) from the services that
    /// are active right now, and hand it to the pool.
    ///
    /// The floor is `ceil(channels / streams-per-tunnel)` of the deepest
    /// active UDP service, maintained across a tunnel's death: the pool may
    /// lose an idle tunnel to a failure without the floor moving, because the
    /// floor describes what the *services* need, not how many tunnels exist.
    #[cfg(feature = "multiplex")]
    fn refresh_pool_floors(&self) {
        let cap = crate::transport::multiplex::stream_cap();
        let mut floors: HashMap<&str, usize> = HashMap::new();
        for entry in self.services.values() {
            let ServiceState::Active(active) = &entry.state else {
                continue;
            };
            let Some(key) = active.pool_key.as_deref() else {
                continue;
            };
            if !matches!(entry.slot.service.service_type, ServiceType::Udp) {
                continue;
            }
            let floor = crate::transport::pool::udp_floor_capped(
                [entry.slot.channels],
                cap,
                entry.slot.data.max_tunnels,
            );
            let slot = floors.entry(key).or_insert(0);
            *slot = (*slot).max(floor);
        }
        for (key, pool) in &self.pools {
            let floor = floors.get(key.as_str()).copied().unwrap_or(0);
            pool.pool().set_udp_floor(floor);
        }
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

    /// Open one data channel for a service the server asked about.
    fn open_placed_channel(&self, id: ServiceId, stripe: Option<StripeOpen>) {
        let Some(entry) = self.services.get(&id) else {
            debug!("The server asked for a data channel for unknown service {id}");
            return;
        };
        match &entry.state {
            ServiceState::Active(active) => open_data_channel(active, stripe),
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
        // The service's data plane is gone with its registration, so its
        // demand on the pools goes with it.
        #[cfg(feature = "multiplex")]
        self.refresh_pool_floors();
        self.register(id, nonce, rd, wr).await
    }

    /// Drop one service: the server releases its public endpoint and the
    /// session keeps serving its siblings.
    async fn deregister<W: AsyncWrite + Unpin>(&mut self, id: ServiceId, wr: &mut W) -> Result<()> {
        if self.services.remove(&id).is_some() {
            debug!("Deregistering service {id}");
            write_session_cmd(wr, &SessionCmd::Deregister(id)).await?;
            // A service that is gone asks for nothing: the pools it drew from
            // may shrink now that its channels are no longer a floor (D7).
            #[cfg(feature = "multiplex")]
            self.refresh_pool_floors();
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
            udp_send_queue_size: Some(2),
            ..Default::default()
        };
        // The session's pin registry: a real one with tunnels, the unit handle
        // without them. `Default` is the one spelling that fits both builds
        // (and the one the `unit_arg` lint accepts).
        let pins: Arc<Pins> = Arc::default();
        let hub = build_udp_hub(&service, pins).expect("a UDP service must build a hub");
        assert_eq!(
            hub.params.sendq_size, 2,
            "the configured queue size must reach the runtime"
        );

        // The queue `run_data_channel_for_udp` creates for one channel's
        // writer, with nobody draining it: a writer parked on a closed window.
        let (tx, mut rx) = mpsc::channel::<UdpTraffic>(hub.params.sendq_size);
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
            "exactly sendq_size datagrams are queued (in order); the rest are dropped"
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
