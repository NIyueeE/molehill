use crate::common::constants::FORWARD_IDLE_TIMEOUT;
use crate::common::constants::{DEFAULT_UDP_SENDQ_SIZE, TCP_COPY_BUFFER_SIZE, UDP_ROUTE_TTL_SECS};
use crate::common::forward::copy_bidirectional_with_idle;
use crate::common::helper::write_and_flush;
use crate::config::ConfigChange;
use crate::config::{Config, ServerConfig, ServiceType};
use crate::logging::RepeatNotice;
use crate::protocol::Hello::{ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, Ack, CURRENT_PROTO_VERSION, Carrier, ControlChannelCmd, DataChannelCmd,
    HASH_WIDTH_IN_BYTES, Hello, MAX_UDP_HEADER_LEN, NOISE_SELECTOR, PLAIN_SELECTOR, ServiceId,
    ServiceRegistration, SessionCmd, SessionRegistration, UdpTraffic, read_auth, read_hello,
    read_session_cmd, read_stream_prologue, write_register_result,
};
#[cfg(feature = "noise")]
use crate::transport::noise_resume::NOISE_RESUME_SELECTOR;
#[cfg(feature = "noise")]
use crate::transport::{NoiseKeys, NoiseStream};
use crate::transport::{SocketOpts, TcpTransport, Transport};
use anyhow::{Context, Result, anyhow, bail};
use backon::BackoffBuilder;
use backon::ExponentialBuilder;
use bytes::{Bytes, BytesMut};

use rand::TryRng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use std::time::Instant;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time;
use tracing::{Instrument, Span, debug, error, info, info_span, instrument, warn};

#[cfg(feature = "kcp")]
use crate::transport::kcp::KcpAcceptor;

type Nonce = protocol::Digest; // Also called `session_key`

/// Process-wide rate limit for token rejections: a client that keeps retrying
/// with the wrong token must not be able to fill the log (see `RepeatNotice`).
static AUTH_FAILURES: RepeatNotice = RepeatNotice::new();

/// Control sessions this process has authenticated (v4 only: a v3 connection
/// is one service, not a session).
///
/// The one fact about a session that is invisible from the outside is *how
/// many* there are — a client that opened one connection per service would
/// look identical in every forwarding test. The integration suite therefore
/// counts them through [`control_sessions_accepted`] instead of parsing logs.
static CONTROL_SESSIONS_ACCEPTED: AtomicU64 = AtomicU64::new(0);

/// How many v4 control sessions this process has accepted and authenticated so
/// far.
///
/// Monotone for the life of the process: a caller that wants "how many did
/// *this* scenario open" takes a reading before it starts and subtracts. Only
/// authenticated sessions count, so a refused token or a failed handshake does
/// not move the number.
#[must_use]
pub fn control_sessions_accepted() -> u64 {
    CONTROL_SESSIONS_ACCEPTED.load(Ordering::Relaxed)
}

/// Report a rejected token: once per process at `warn`, then at `debug`.
fn report_auth_failure() {
    AUTH_FAILURES.report(
        || {
            warn!(
                "Rejected a control channel with a wrong token: the client's \
                 `default_token` must match `[server].default_token`. Further \
                 rejections are logged at debug level."
            );
        },
        || debug!("Rejected a control channel with a wrong token"),
    );
}

const CHAN_SIZE: usize = 2048; // The capacity of various chans

/// How long the visitor-pairing loop waits for a data channel before asking
/// for another one.
///
/// It has to be comfortably above a *legitimate* slow open — opening a channel
/// on an empty pool dials a tunnel, and the client's own wait budget for that
/// is 15 s — so that ordinary work is never re-requested. Its job is not to
/// time a healthy open out; it is to stop one unanswerable request from parking
/// the accept loop, which stalls the whole service (see `pair_visitor`).
const PAIR_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// How many `PAIR_WAIT_BUDGET` waits one visitor gets before it is shed.
///
/// Five tries is 25 s of patience for a visitor whose service is under
/// pressure, after which the connection is closed — a failed request, reported
/// at DEBUG, instead of a hang the operator cannot attribute.
const PAIR_ATTEMPTS: usize = 5;
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake

/// Runtime description of a service, as registered by a client.
///
/// The server owns no per-service configuration: everything needed to expose
/// the service arrives in the client's registration and has already been
/// validated against `[server].allow_ports`.
#[derive(Clone, Debug)]
struct RegisteredService {
    name: String,
    service_type: ServiceType,
    bind_addr: SocketAddr,
    /// Receive buffer size for UDP datagrams; ignored for TCP services.
    udp_buffer_size: usize,
    /// The TUN device a transparent service's data path attaches to
    /// (`[server.transparent].tun`); `None` for every other service type.
    #[cfg_attr(
        not(all(feature = "transparent", target_os = "linux")),
        expect(dead_code, reason = "read only by the transparent data path")
    )]
    transparent_tun: Option<String>,
}

// The entrypoint of running a server
pub async fn run_server(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
    update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    let Some(config) = config.server else {
        return Err(anyhow!(
            "Try to run as a server, but the configuration is missing. Please add the `[server]` block"
        ));
    };

    let mut server = Server::from(config)?;
    server.run(shutdown_rx, update_rx).await?;

    Ok(())
}

/// One v4 control session: the services registered on it, beside the session's
/// single writer and its shutdown signal.
struct SessionHandle {
    /// Live services, keyed by the id the client gave each one. Dropping an
    /// entry stops that service's control task, which stops its pool task and
    /// releases its listener and public port.
    services: HashMap<ServiceId, ControlChannelHandle>,
    /// The multiplexed tunnels this client holds right now, shared with the
    /// guards the tunnel tasks own. It is what
    /// `[server.data].max_tunnels_per_client` is checked against. No tunnels
    /// exist without the `multiplex` feature, so neither does the count.
    #[cfg(feature = "multiplex")]
    tunnels: TunnelCount,
    /// The session's one and only writer: every service's commands are queued
    /// here as framed bytes, and a single task drains the queue into the
    /// socket. A session carries N services, so no service may write directly.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Ends the session task. The registry entry holds the sender, so removing
    /// the entry (or dropping the session) stops the session.
    shutdown: broadcast::Sender<bool>,
}

impl SessionHandle {
    /// A session with no services yet: the v4 handshake publishes this before
    /// it serves anything, and the tests build the same shape.
    fn new(
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
        shutdown: broadcast::Sender<bool>,
    ) -> SessionHandle {
        SessionHandle {
            services: HashMap::new(),
            #[cfg(feature = "multiplex")]
            tunnels: TunnelCount::default(),
            write_tx,
            shutdown,
        }
    }
}

/// One session's live tunnel count, and the reservation a tunnel holds.
///
/// The count is taken with a compare-exchange rather than a load-then-add so
/// two tunnels of the same client arriving together cannot both slip past
/// `max_tunnels_per_client`; the guard releases the slot when the tunnel ends,
/// whatever ended it (a clean close, a dead peer, a shutdown).
#[cfg(feature = "multiplex")]
#[derive(Clone, Default)]
struct TunnelCount(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(feature = "multiplex")]
impl TunnelCount {
    /// Reserve one tunnel slot under `cap` (`0` = unlimited). `Err(held)` is
    /// the number of tunnels the client already holds, for the refusal line.
    fn try_reserve(&self, cap: usize) -> std::result::Result<TunnelGuard, usize> {
        use std::sync::atomic::Ordering;
        let mut held = self.0.load(Ordering::Acquire);
        loop {
            if cap != 0 && held >= cap {
                return Err(held);
            }
            match self
                .0
                .compare_exchange_weak(held, held + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(TunnelGuard(self.clone())),
                Err(actual) => held = actual,
            }
        }
    }
}

/// Holds one tunnel's slot in its session's [`TunnelCount`] until it drops.
#[cfg(feature = "multiplex")]
struct TunnelGuard(TunnelCount);

#[cfg(feature = "multiplex")]
impl Drop for TunnelGuard {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// v4 registry: one session per authenticated control connection, keyed by the
/// nonce the server issued — the same value a data channel authenticates with.
type SessionMap = HashMap<Nonce, SessionHandle>;

/// The server's live registrations, one map per dialect.
///
/// v3 stays keyed by *service* (its takeover is by service digest, its data
/// planes by session key); v4 is keyed by *session*, because a session's
/// identity is a nonce and its services are looked up by [`ServiceId`].
struct Registry {
    sessions: Arc<RwLock<SessionMap>>,
}

impl Registry {
    fn new() -> Registry {
        Registry {
            sessions: Arc::new(RwLock::new(SessionMap::new())),
        }
    }
}

/// The service handle a v4 `(session nonce, service id)` names, when both are
/// live.
async fn session_service(
    registry: &Registry,
    nonce: &Nonce,
    service_id: ServiceId,
) -> Option<ControlChannelHandle> {
    registry
        .sessions
        .read()
        .await
        .get(nonce)
        .and_then(|session| session.services.get(&service_id))
        .cloned()
}

/// A connection accepted by the server, after the transport selector byte:
/// plain TCP, or TCP wrapped in the Noise record stream when the client
/// chose encryption. The client decides the transport; the server accepts
/// both on every listener (v3 selector, see protocol.rs).
enum ServerStream {
    Plain(TcpStream),
    #[cfg(feature = "noise")]
    Noise(Box<NoiseStream<TcpStream>>),
}

impl std::fmt::Debug for ServerStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerStream::Plain(s) => f.debug_tuple("Plain").field(s).finish(),
            #[cfg(feature = "noise")]
            ServerStream::Noise(_) => f.debug_tuple("Noise").finish(),
        }
    }
}

impl ServerStream {
    /// Apply socket options to the underlying TCP socket (the Noise wrapper
    /// exposes its inner stream).
    fn hint(&self, opts: SocketOpts) {
        match self {
            ServerStream::Plain(s) => opts.apply(s),
            #[cfg(feature = "noise")]
            ServerStream::Noise(s) => opts.apply(s.get_inner()),
        }
    }
}

impl AsyncRead for ServerStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ServerStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "noise")]
            ServerStream::Noise(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ServerStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ServerStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "noise")]
            ServerStream::Noise(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ServerStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "noise")]
            ServerStream::Noise(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ServerStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "noise")]
            ServerStream::Noise(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Upgrade a freshly accepted TCP connection by its v3 transport selector:
/// `PLAIN_SELECTOR` keeps the raw stream, `NOISE_SELECTOR` runs the Noise
/// responder handshake (requires the server's keys), anything else is
/// rejected.
async fn upgrade_conn(
    mut conn: TcpStream,
    #[cfg(feature = "noise")] noise_keys: Option<&NoiseKeys>,
) -> Result<ServerStream> {
    let selector = conn.read_u8().await?;
    match selector {
        PLAIN_SELECTOR => Ok(ServerStream::Plain(conn)),
        NOISE_SELECTOR => {
            #[cfg(feature = "noise")]
            {
                let keys = noise_keys.ok_or_else(|| {
                    anyhow!("Client requested Noise, but the server has no Noise keys")
                })?;
                Ok(ServerStream::Noise(Box::new(
                    keys.wrap_responder(conn).await?,
                )))
            }
            #[cfg(not(feature = "noise"))]
            {
                let _ = conn;
                bail!(
                    "Client requested Noise, but this binary was built without the `noise` feature"
                )
            }
        }
        #[cfg(feature = "noise")]
        NOISE_RESUME_SELECTOR => {
            let keys = noise_keys.ok_or_else(|| {
                anyhow!("Client requested a noise session resume, but the server has no Noise keys")
            })?;
            // A declined request ends here: the responder already wrote
            // its verdict, and the client falls back to a full handshake
            // on a fresh connection.
            match keys.run_resume(conn).await? {
                Some(stream) => Ok(ServerStream::Noise(Box::new(stream))),
                None => bail!("Declined a noise session resume"),
            }
        }
        other => bail!("Unknown transport selector {other:#04x}"),
    }
}

/// Everything the accept paths need, shared across the server's listener
/// tasks: the TCP transport (bind/accept/hints), the optional Noise keys,
/// the lazily-bound KCP listener state and the shutdown broadcast for
/// tasks spawned after startup.
struct ServerShared {
    tcp: Arc<TcpTransport>,
    #[cfg(feature = "noise")]
    noise_keys: Option<NoiseKeys>,
    #[cfg(feature = "kcp")]
    kcp: Arc<KcpListenerState>,
    #[cfg(feature = "kcp")]
    shutdown_tx: broadcast::Sender<bool>,
}

/// Shared state for the lazily-bound KCP listener: bound on the first
/// registration that declares the `kcp` carrier (client-first), once per
/// server process.
#[cfg(feature = "kcp")]
struct KcpListenerState {
    acceptor: tokio::sync::Mutex<Option<Arc<KcpAcceptor>>>,
}

#[cfg(feature = "kcp")]
impl Default for KcpListenerState {
    fn default() -> Self {
        Self {
            acceptor: tokio::sync::Mutex::new(None),
        }
    }
}

/// Ensure the KCP UDP listener is bound — once, on the first registration
/// that declares the `kcp` carrier. A bind failure is a precise
/// registration rejection, not a startup failure: servers that never see a
/// KCP client never open the UDP socket (client-first, no config-side
/// carrier opt-in).
#[cfg(feature = "kcp")]
async fn ensure_kcp_listener(
    shared: Arc<ServerShared>,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    let mut guard = shared.kcp.acceptor.lock().await;
    if guard.is_some() {
        return Ok(());
    }
    let acceptor = KcpAcceptor::bind(server_config.data_bind_addr())
        .await
        .with_context(|| "Failed to bind the KCP data listener")?;
    let bound = acceptor.local_addr().with_context(|| {
        format!(
            "Failed to read the KCP tunnel socket address at {}",
            server_config.data_bind_addr()
        )
    })?;
    info!("Listening for KCP tunnels at {bound}");
    let acceptor = Arc::new(acceptor);
    tokio::spawn(run_kcp_listener(
        Arc::clone(&acceptor),
        Arc::clone(&registry),
        Arc::clone(&shared),
        shared.shutdown_tx.subscribe(),
        server_config.max_tunnels_per_client(),
    ));
    *guard = Some(acceptor);
    Ok(())
}

// Server holds all states of running a server
struct Server {
    // `[server]` config
    config: Arc<ServerConfig>,

    // Live registrations: v3 control channels and v4 sessions (see `Registry`)
    registry: Arc<Registry>,
    // TCP transport + Noise keys + lazy KCP listener + shutdown broadcast
    // (see `ServerShared`).
    shared: Arc<ServerShared>,
}

impl Server {
    // Create a server from `[server]`
    pub fn from(config: ServerConfig) -> Result<Server> {
        let config = Arc::new(config);
        let registry = Arc::new(Registry::new());
        let tcp = Arc::new(TcpTransport::new(
            &crate::config::TransportConfig::default(),
        )?);
        #[cfg(feature = "kcp")]
        let kcp = Arc::new(KcpListenerState::default());
        #[cfg(feature = "noise")]
        let noise_keys = match &config.transport.noise {
            Some(cfg) => Some(NoiseKeys::from_config(cfg)?),
            None => None,
        };
        let shared = Arc::new(ServerShared {
            tcp,
            #[cfg(feature = "noise")]
            noise_keys,
            #[cfg(feature = "kcp")]
            kcp,
            #[cfg(feature = "kcp")]
            shutdown_tx: broadcast::channel(1).0,
        });
        Ok(Server {
            config,
            registry,
            shared,
        })
    }

    // The entry point of Server
    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        // Control listener: authenticates and registers services, and also
        // accepts data channels when the data plane shares this address.
        let control_bind = self.config.control.bind_addr.clone();
        let control_l = self
            .shared
            .tcp
            .bind(&control_bind)
            .await
            .with_context(|| "Failed to listen at `server.control.bind_addr`")?;
        info!("Listening at {control_bind}");
        tokio::spawn(run_accept_loop(
            Arc::clone(&self.shared),
            control_l,
            self.registry.clone(),
            self.config.clone(),
            shutdown_rx.resubscribe(),
        ));

        // Data-plane listener. When `[server.data].bind_addr` equals the
        // control address (the default) the control listener above already
        // accepts data connections; otherwise a second listener is bound.
        #[cfg(feature = "multiplex")]
        {
            let data_bind = self.config.data_bind_addr().to_owned();
            if data_bind != control_bind {
                let data_l = self.shared.tcp.bind(&data_bind).await.with_context(|| {
                    format!("Failed to listen at `server.data.bind_addr` ({data_bind})")
                })?;
                info!("Listening for data channels at {data_bind}");
                tokio::spawn(run_accept_loop(
                    Arc::clone(&self.shared),
                    data_l,
                    self.registry.clone(),
                    self.config.clone(),
                    shutdown_rx.resubscribe(),
                ));
            }
            // The KCP UDP listener is bound lazily: the first registration
            // that declares the `kcp` carrier triggers it (client-first —
            // the server does not pre-declare carriers in its config).
        }

        // Wait for the shutdown signal; the server owns no service
        // configuration, so there is nothing to hot-reload here.
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("Shutting down gracefully...");
                    break;
                },
                e = update_rx.recv() => {
                    if let Some(e) = e {
                        warn!("Ignored {e:?} since running as a server");
                    }
                }
            }
        }

        // Stop the live connections with the listeners. A session left running
        // would keep its services' listeners bound (and their public ports
        // held) after the server reported a clean shutdown, and — in a test
        // that restarts a server inside one process — would let the *old*
        // server keep answering on the ports the new one is about to bind.
        // Dropping a handle stops that service and its pool; dropping the
        // session handle closes its control connection, which is how the
        // client learns to reconnect.
        self.registry.sessions.write().await.clear();
        // The KCP listener task subscribed to this broadcast when the first
        // `kcp` registration bound it (the TCP accept loops watch the caller's
        // own receiver instead), so without the signal its socket stays bound
        // for the life of the process — and a server restarted in its place
        // cannot bind that port again.
        #[cfg(feature = "kcp")]
        let _ = self.shared.shutdown_tx.send(true);

        info!("Shutdown");

        Ok(())
    }
}

/// Accept loop for one listener: read the v3 transport selector and run
/// the (optional) Noise handshake with a timeout, then dispatch each
/// connection to `handle_connection`.
async fn run_accept_loop(
    shared: Arc<ServerShared>,
    listener: TcpListener,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) {
    // Retry at least every 100ms
    let backoff_builder = ExponentialBuilder::default().with_max_delay(Duration::from_millis(100));
    let mut backoff = backoff_builder.build();
    // A failing `accept` retries every 100ms. The first failure is the
    // operator's (EMFILE means "raise the limit"); a hundred identical lines
    // are not, and they would bury everything else.
    let accept_notice = RepeatNotice::new();

    loop {
        tokio::select! {
            ret = shared.tcp.accept(&listener) => {
                match ret {
                    Err(err) => {
                        if should_retry_accept(&err) {
                            if let Some(d) = backoff.next() {
                                accept_notice.report(
                                    || error!("Failed to accept: {:#}. Retry in {:?}...", err, d),
                                    || debug!("Failed to accept: {:#}. Retry in {:?}...", err, d),
                                );
                                time::sleep(d).await;
                            } else {
                                error!("Too many retries. Aborting...");
                                break;
                            }
                        } else if let Some(e) = err.downcast_ref::<io::Error>() {
                            // Transient connection-level errors (ECONNABORTED,
                            // ECONNRESET, ...) don't affect the listener.
                            debug!("Accept interrupted: {e}");
                        }
                        // Non-IO errors from the transport layer are ignored.
                    }
                    Ok((conn, addr)) => {
                        backoff = backoff_builder.build();
                        accept_notice.clear();

                        // Transport selector + optional Noise handshake,
                        // under the handshake timeout.
                        #[cfg(feature = "noise")]
                        let upgrade = upgrade_conn(conn, shared.noise_keys.as_ref());
                        #[cfg(not(feature = "noise"))]
                        let upgrade = upgrade_conn(conn);
                        match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), upgrade).await
                        {
                            Ok(conn) => {
                                match conn.with_context(|| "Failed to do transport handshake") {
                                    Ok(conn) => {
                                        let registry = registry.clone();
                                        let server_config = server_config.clone();
                                        let shared = Arc::clone(&shared);
                                        tokio::spawn(async move {
                                            if let Err(err) = handle_connection(
                                                conn,
                                                registry,
                                                server_config,
                                                shared,
                                            )
                                            .await
                                            {
                                                // One connection's failure:
                                                // a scanner, a peer that hung
                                                // up, a version mismatch. The
                                                // peer logs its own side.
                                                debug!("{:#}", err);
                                            }
                                        }.instrument(info_span!("connection", %addr)));
                                    }
                                    Err(e) => {
                                        debug!("{:#}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("Transport handshake timeout: {}", e);
                            }
                        }
                    }
                }
            },
            _ = shutdown_rx.recv() => break,
        }
    }
}

// Handle connections accepted on the control or data listener.
async fn handle_connection(
    mut conn: ServerStream,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
    shared: Arc<ServerShared>,
) -> Result<()> {
    // Read hello. `read_hello` has already refused every dialect this server
    // does not serve (protocol v4, and nothing else), so what the hello
    // *variant* says is which kind of connection this is.
    let (_version, hello) = read_hello(&mut conn).await?;
    match hello {
        ControlChannelHello(_, _tag) => {
            // The v4 hello's digest is a client-chosen session tag; the
            // session's identity is the nonce *this* end issues below.
            do_session_handshake(conn, registry, server_config, shared).await?;
        }
        DataChannelHello(_, nonce) => {
            // A direct data channel names its service in the 4 bytes right
            // after the hello.
            let service_id = read_stream_prologue(&mut conn).await?;
            do_v4_data_channel(tcp_data_channel(conn), registry, nonce, service_id).await?;
        }
        #[cfg(feature = "multiplex")]
        Hello::DataChannelTunnelHello(_, nonce) => {
            // A tunnel carries no service of its own: it belongs to the
            // session, and every stream inside it names its own service.
            do_v4_tunnel(
                conn,
                registry,
                nonce,
                server_config.max_tunnels_per_client(),
            )
            .await?;
        }
        #[cfg(not(feature = "multiplex"))]
        Hello::DataChannelTunnelHello(..) => {
            bail!(
                "Peer requested a multiplexed data tunnel, but this binary was built without the `multiplex` feature"
            );
        }
    }
    Ok(())
}

/// A service that passed the server's policy and whose public endpoint is
/// bound, ready for the caller to start its pool and control path.
struct Registered {
    service: RegisteredService,
    bound: BoundEndpoint,
    /// A transparent service's claim on its public endpoint, held for the
    /// service's lifetime so a second client cannot claim the same `ip:port`.
    claim: Option<Claim>,
}

/// The outcome of one registration attempt.
enum Registration {
    /// Bound and ready. The caller starts the service and answers `Ack::Ok`.
    Accepted(Box<Registered>),
    /// The server refused it. The reason is what the client reads in
    /// `Ack::RegisterRejected`; a v3 connection ends after it, while a v4
    /// session rejects only that service and stays up.
    Rejected(String),
}

/// The TUN device this server routes transparent services into, or the policy
/// refusal to answer a registration with.
///
/// `[server.transparent]` **is** the switch. Serving L3 is what asks this
/// process for `CAP_NET_ADMIN` and a device, so it is the operator's decision,
/// taken statically in the server's own configuration — never a remote
/// client's, whose registration arrives at runtime and could otherwise be what
/// makes this host reach for `/dev/net/tun`. A server without the table
/// therefore refuses by policy, before any device is looked at.
#[cfg(all(feature = "transparent", target_os = "linux"))]
fn transparent_tun_for(server_config: &ServerConfig) -> Result<&str, String> {
    match server_config.transparent.as_ref() {
        Some(transparent) => Ok(&transparent.tun),
        None => Err(
            "This server does not serve transparent (L3) services: `[server.transparent]` is \
             not configured"
                .to_string(),
        ),
    }
}

/// Validate one service registration against the server's policy and bind its
/// public endpoint: the body both dialects share.
///
/// Everything here is independent of *who owns the control connection* — the
/// `allow_ports` check, the client-declared carrier check and the eager bind
/// that turns a port conflict into a precise rejection. The caller takes over
/// any previous registration for the service *before* calling this, so
/// `bind_with_retry` absorbs that asynchronous teardown.
async fn register_service(
    reg: &ServiceRegistration,
    server_config: &Arc<ServerConfig>,
    #[cfg_attr(
        not(feature = "kcp"),
        allow(unused_variables, reason = "only read by the kcp carrier-check arm")
    )]
    shared: &Arc<ServerShared>,
    #[cfg_attr(
        not(feature = "kcp"),
        allow(unused_variables, reason = "only read by the kcp carrier-check arm")
    )]
    registry: &Arc<Registry>,
) -> Result<Registration> {
    info!(service = %reg.name, "Registering service at {}", reg.bind_addr);

    // Policy check: `allow_ports` is the master switch for dynamic
    // registration. An empty whitelist rejects everything.
    let port = reg.bind_addr.port();
    if !server_config.allow_ports.iter().any(|r| r.contains(port)) {
        let reason = if server_config.allow_ports.is_empty() {
            format!(
                "Port {port} rejected: dynamic registration is disabled on this server (`allow_ports` is not configured)"
            )
        } else {
            format!("Port {port} rejected: not covered by the server's `allow_ports` whitelist")
        };
        warn!(service = %reg.name, "{reason}");
        return Ok(Registration::Rejected(reason));
    }

    // Client-declared carrier: the server opens the corresponding
    // listener on first use (client-first — no config-side opt-in), and a
    // failure is a precise rejection, like `allow_ports`.
    if reg.carrier == Carrier::Kcp {
        #[cfg(feature = "kcp")]
        let result = ensure_kcp_listener(
            Arc::clone(shared),
            Arc::clone(registry),
            Arc::clone(server_config),
        )
        .await;
        #[cfg(not(feature = "kcp"))]
        let result: Result<()> = Err(anyhow!("This server was built without the `kcp` feature"));
        if let Err(e) = result {
            let reason = format!("{e:#}");
            warn!(service = %reg.name, "Registration failed: {reason}");
            return Ok(Registration::Rejected(reason));
        }
    }

    // A transparent service claims a public address this host never binds, so
    // four things have to hold before it is accepted: this server serves L3 at
    // all (the operator's own `[server.transparent]`, checked first so that a
    // client's registration is never what makes this process reach for a
    // device), the build and platform can carry it, the routing contract is
    // satisfiable (the operator's TUN device exists), and no other service
    // already owns that address.
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    let (claim, transparent_tun) = {
        if reg.service_type == ServiceType::Transparent {
            let tun = match transparent_tun_for(server_config) {
                Ok(tun) => tun,
                Err(reason) => {
                    warn!(service = %reg.name, "{reason}");
                    return Ok(Registration::Rejected(reason));
                }
            };
            if let Err(e) = crate::transparent::check::require_interface(tun) {
                let reason = format!("{e:#}");
                warn!(service = %reg.name, "Registration failed: {reason}");
                return Ok(Registration::Rejected(reason));
            }
            let Some(held) = Claim::acquire(reg.bind_addr) else {
                let reason = format!(
                    "Address {} is already claimed by another transparent service on this server",
                    reg.bind_addr
                );
                warn!(service = %reg.name, "{reason}");
                return Ok(Registration::Rejected(reason));
            };
            (Some(held), Some(tun.to_string()))
        } else {
            (None, None)
        }
    };
    #[cfg(not(all(feature = "transparent", target_os = "linux")))]
    let (claim, transparent_tun): (Option<Claim>, Option<String>) = {
        if reg.service_type == ServiceType::Transparent {
            let reason = "This server cannot serve `protocol = \"transparent\"`: it carries \
                          whole IP packets through a TUN device, which needs a Linux build with \
                          the `transparent` feature. This build does not have it."
                .to_string();
            warn!(service = %reg.name, "{reason}");
            return Ok(Registration::Rejected(reason));
        }
        (None, None)
    };

    let service = RegisteredService {
        name: reg.name.clone(),
        service_type: reg.service_type,
        bind_addr: reg.bind_addr,
        udp_buffer_size: reg.udp_buffer_size as usize,
        transparent_tun,
    };

    // Bind the public endpoint eagerly so that conflicts are reported
    // precisely as a rejection instead of surfacing later as pool errors.
    match bind_with_retry(&service).await {
        Ok(bound) => Ok(Registration::Accepted(Box::new(Registered {
            service,
            bound,
            claim,
        }))),
        Err(e) => {
            let reason = format!("{e:#}");
            warn!(service = %reg.name, "Registration failed: {reason}");
            Ok(Registration::Rejected(reason))
        }
    }
}

/// The v4 control session: authenticate once for the endpoint, then serve N
/// service registrations over the one connection.
///
/// The session owns the connection — a reader loop (this task) consumes
/// `SessionCmd`s and a writer task drains the session's queue — so a service
/// never touches the socket itself; it only queues framed commands.
async fn do_session_handshake(
    mut conn: ServerStream,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
    shared: Arc<ServerShared>,
) -> Result<()> {
    debug!("Handshaking a control session");

    conn.hint(SocketOpts::for_control_channel());

    // The session's identity: 32 random bytes, not derivable from a service
    // name the way a v3 digest is. It is also the data plane's credential.
    let mut nonce = [0u8; HASH_WIDTH_IN_BYTES];
    let mut rng = rand::rngs::SysRng;
    rng.try_fill_bytes(&mut nonce)?;

    let hello = Hello::ControlChannelHello(CURRENT_PROTO_VERSION, nonce);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;

    // Session credential: the endpoint's default token, exactly as v3.
    let mut concat = Vec::from(server_config.default_token.as_bytes());
    concat.extend_from_slice(&nonce);
    let session_key = protocol::digest(&concat);

    let protocol::Auth(d) = read_auth(&mut conn).await?;
    if d != session_key {
        write_and_flush(&mut conn, &postcard::to_stdvec(&Ack::AuthFailed)?).await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        report_auth_failure();
        bail!("Authentication failed");
    }

    // The server *declares* its cadence here (0 = none). `SessionOk` carries a
    // payload, so it travels through the framed helper — never through the
    // fixed-width 1-byte ack path.
    let heartbeat_interval = server_config.control.heartbeat_interval;
    let session_ok = Ack::SessionOk {
        heartbeat_interval_secs: heartbeat_interval,
    };
    write_register_result(&mut conn, &session_ok).await?;
    // One authenticated session per control connection: the integration test
    // for D1 asserts that a client's services share a session, which needs a
    // number the process can be asked for (see `control_sessions_accepted`).
    CONTROL_SESSIONS_ACCEPTED.fetch_add(1, Ordering::Relaxed);

    // Split *after* the socket options are set: these two halves are the
    // session's single reader (below) and its single writer (the task).
    let (mut rd, wr) = tokio::io::split(conn);
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (shutdown_tx, _) = broadcast::channel::<bool>(1);

    let mut writer = tokio::spawn(
        async move {
            let mut wr = wr;
            while let Some(frame) = write_rx.recv().await {
                if let Err(e) = write_and_flush(&mut wr, &frame).await {
                    // The client is gone; the reader below ends with it.
                    debug!("Failed to write a session command: {e:#}");
                    break;
                }
            }
        }
        .instrument(Span::current()),
    );

    // Publish the session before serving it: a data channel for one of its
    // services may arrive while this reader is still registering them. The
    // cadence below and the writer every service queues to are read from the
    // published handle, so the registry entry is what owns them.
    let (heartbeat_tx, mut session_shutdown_rx) = {
        let handle = SessionHandle::new(write_tx, shutdown_tx);
        let heartbeat_tx = handle.write_tx.clone();
        let shutdown_rx = handle.shutdown.subscribe();
        registry.sessions.write().await.insert(nonce, handle);
        (heartbeat_tx, shutdown_rx)
    };

    let ctx = SessionCtx {
        nonce,
        session_key,
        registry: Arc::clone(&registry),
        server_config: Arc::clone(&server_config),
        shared: Arc::clone(&shared),
    };
    let heartbeat = postcard::to_stdvec(&ControlChannelCmd::HeartBeat)?;

    let reason = loop {
        tokio::select! {
            cmd = read_session_cmd(&mut rd) => match cmd {
                Ok(SessionCmd::Register(reg)) => {
                    if let Err(e) = ctx.register(&reg).await {
                        break e;
                    }
                }
                Ok(SessionCmd::Deregister(service_id)) => ctx.deregister(service_id).await,
                Err(e) => break e,
            },
            () = time::sleep(Duration::from_secs(heartbeat_interval)), if heartbeat_interval != 0 => {
                if heartbeat_tx.send(heartbeat.clone()).is_err() {
                    break anyhow!("The session writer is gone");
                }
            }
            // The socket's write half failed: the session goes with it.
            _ = &mut writer => break anyhow!("The session writer stopped"),
            _ = session_shutdown_rx.recv() => break anyhow!("The session was shut down"),
        }
    };
    debug!("Control session ended: {reason:#}");

    // Drop every service handle: each service's control task ends, its pool
    // task stops, and its listener and public port are released. Removing the
    // registry entry drops the last writers, which ends the writer task.
    if let Some(mut session) = registry.sessions.write().await.remove(&nonce) {
        debug!(
            services = session.services.len(),
            "Control session closed, releasing its services"
        );
        // Explicit per-service teardown, for the same reason the takeover
        // sends: a pool must stop even when something still holds a clone.
        for (_, handle) in session.services.drain() {
            handle.shutdown();
        }
        drop(session);
    }
    drop(ctx);
    drop(heartbeat_tx);
    drop(rd);
    Ok(())
}

/// One v4 session's registration path: the session it belongs to, plus the
/// server-side policy every registration is checked against.
struct SessionCtx {
    nonce: Nonce,
    /// The session credential — the server's default token bound to the nonce.
    /// The server owns no per-service token table, so this is also the only
    /// per-service credential it can check.
    session_key: Nonce,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
    shared: Arc<ServerShared>,
}

impl SessionCtx {
    /// The session's one writer, for as long as the session is registered.
    async fn write_tx(&self) -> Result<mpsc::UnboundedSender<Vec<u8>>> {
        self.registry
            .sessions
            .read()
            .await
            .get(&self.nonce)
            .map(|session| session.write_tx.clone())
            .ok_or_else(|| anyhow!("The control session is gone"))
    }

    /// Frame an ack for the session's writer exactly as a connection write
    /// would (`write_register_result`), without touching the socket.
    async fn send_ack(&self, ack: &Ack) -> Result<()> {
        let mut frame = Vec::new();
        write_register_result(&mut frame, ack).await?;
        self.write_tx()
            .await?
            .send(frame)
            .map_err(|_| anyhow!("The control session is gone"))
    }

    /// One `SessionCmd::Register`.
    ///
    /// The service's own credential is checked first; a rejection answers that
    /// service alone and leaves the session and its siblings running.
    async fn register(&self, reg: &SessionRegistration) -> Result<()> {
        let service_id = reg.service_id;

        if reg.auth != self.session_key {
            let reason = "Incorrect token for this service".to_owned();
            debug!(service = %reg.reg.name, "Service {service_id} rejected: {reason}");
            return self.send_ack(&Ack::RegisterRejected(reason)).await;
        }

        // Re-registering the same id replaces the endpoint. The takeover
        // happens before binding, so `bind_with_retry` absorbs the
        // asynchronous teardown — the order the v3 path uses too.
        let replaced = self
            .registry
            .sessions
            .write()
            .await
            .get_mut(&self.nonce)
            .and_then(|session| session.services.remove(&service_id));
        if let Some(previous) = replaced {
            info!(service = %reg.reg.name, "Dropping previous control channel");
            previous.shutdown();
        }

        let registered =
            match register_service(&reg.reg, &self.server_config, &self.shared, &self.registry)
                .await?
            {
                Registration::Rejected(reason) => {
                    return self.send_ack(&Ack::RegisterRejected(reason)).await;
                }
                Registration::Accepted(registered) => registered,
            };
        let Registered {
            service,
            bound,
            claim,
        } = *registered;

        // The verdict precedes any command for this service: everything the
        // per-service task queues goes through the same writer, so answering
        // first keeps `Ok` ahead of the first `CreateDataChannelFor`.
        self.send_ack(&Ack::Ok).await?;

        let handle = ControlChannelHandle::new(
            ControlSink::Session {
                service_id,
                write_tx: self.write_tx().await?,
            },
            &service,
            bound,
            claim,
            // The session owns the cadence (`Ack::SessionOk` declared it), so
            // this service's own heartbeat is off: the per-service task only
            // turns its pool's requests into tagged commands.
            0,
            // v4 registrations carry no `pool_size`: the tunnel pool is a
            // client-side, per-carrier concern sized by the client's own
            // configuration — one request per visitor, or one per stripe for a
            // striped gather (see `pair_striped_group`).
            0,
            stripe_count(&self.server_config),
        );

        // The handle *is* the service's lease: dropping it (Deregister, or the
        // session ending) stops the pool and releases the port.
        let mut guard = self.registry.sessions.write().await;
        let Some(session) = guard.get_mut(&self.nonce) else {
            // The session ended while this registration was being served.
            return Ok(());
        };
        session.services.insert(service_id, handle);
        drop(guard);
        info!(service = %service.name, "Control channel established");
        Ok(())
    }

    /// One `SessionCmd::Deregister`: dropping the handle stops that service's
    /// pool task, which releases its listener and public port. The session's
    /// other services are untouched.
    async fn deregister(&self, service_id: ServiceId) {
        let removed = self
            .registry
            .sessions
            .write()
            .await
            .get_mut(&self.nonce)
            .and_then(|session| session.services.remove(&service_id));
        if let Some(handle) = removed {
            // Ends the service's control task, which stops its pool task and
            // releases the port.
            debug!("Deregistered service {service_id}");
            handle.shutdown();
        } else {
            debug!("Deregister for unknown service {service_id}");
        }
    }
}

/// One end of a forwarded connection, as handed to the connection pool.
///
/// With the `multiplex` feature a data channel is a plain transport stream
/// (no-mux mode), one yamux stream of the client's tunnel, or — with the `kcp`
/// carrier — a KCP session that *is* the channel.
#[cfg(not(feature = "multiplex"))]
type DataChannel = ServerStream;

#[cfg(feature = "multiplex")]
enum DataChannel {
    Raw(ServerStream),
    /// A direct channel over KCP: no yamux above it, the session's own byte
    /// stream is the channel (`carrier = "kcp"` with `mode = "direct"`).
    #[cfg(feature = "kcp")]
    RawKcp(crate::transport::kcp::KcpTunnelStream),
    Mux(crate::transport::MuxStream),
}

/// Wrap a freshly handshaked transport stream as a pool-ready data channel.
#[cfg(not(feature = "multiplex"))]
fn new_data_channel(stream: ServerStream) -> ServerStream {
    stream
}

#[cfg(feature = "multiplex")]
fn new_data_channel(stream: ServerStream) -> DataChannel {
    DataChannel::Raw(stream)
}

#[cfg(feature = "multiplex")]
impl tokio::io::AsyncRead for DataChannel {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "kcp")]
            DataChannel::RawKcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(feature = "multiplex")]
impl tokio::io::AsyncWrite for DataChannel {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "kcp")]
            DataChannel::RawKcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "kcp")]
            DataChannel::RawKcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "kcp")]
            DataChannel::RawKcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// A v4 direct data channel: its prologue already named the service.
///
/// A channel for a service that is not registered (a stale one, or an id the
/// client never registered) is dropped here; the session — and every other
/// service on it — is untouched.
///
/// The carrier decides how the channel was dialed (TCP, or a KCP session when
/// the client declared that carrier for a direct service), not what happens
/// here: once the prologue is read, every direct channel is one byte stream
/// handed to the service's pool.
async fn do_v4_data_channel(
    conn: DataChannel,
    registry: Arc<Registry>,
    nonce: Nonce,
    service_id: ServiceId,
) -> Result<()> {
    debug!("Handshaking a data channel");

    let Some(handle) = session_service(&registry, &nonce, service_id).await else {
        debug!("Data channel names service {service_id}, which is not registered on that session");
        return Ok(());
    };

    handle
        .data_channel
        .send(conn)
        .await
        .map_err(|_| anyhow!("Data channel for a stale control session"))?;
    Ok(())
}

/// A direct data channel accepted on the TCP data listener: apply the
/// per-connection TCP options first — there is a socket to apply them to.
fn tcp_data_channel(conn: ServerStream) -> DataChannel {
    conn.hint(SocketOpts::for_service(None));
    new_data_channel(conn)
}

/// A v4 multiplex tunnel: it belongs to the session, not to a service, and
/// each stream inside it names its service with the 4-byte prologue.
///
/// **This is where `[server.data].max_tunnels_per_client` lives** (the
/// operator's valve): a tunnel that would push the client past its cap is
/// refused with a typed answer and a `debug` line naming the cap, and the
/// session keeps running — never killed, so an over-eager client loses one
/// tunnel, not its services (D14).
#[cfg(feature = "multiplex")]
async fn do_v4_tunnel(
    mut conn: ServerStream,
    registry: Arc<Registry>,
    nonce: Nonce,
    max_tunnels_per_client: usize,
) -> Result<()> {
    debug!("Handshaking a multiplexed data tunnel");

    // The reservation travels into `upgrade_to_tunnel`, which hands it to the
    // task that lives as long as the tunnel: this function itself returns as
    // soon as the tunnel is up.
    let slot = match reserve_tunnel(&registry, &nonce, max_tunnels_per_client).await {
        TunnelSlot::NoSession => {
            debug!("Data tunnel has an incorrect nonce");
            return Ok(());
        }
        TunnelSlot::Held(slot) => slot,
        TunnelSlot::OverCap { held } => {
            // A refusal, not a failure: the client keeps its other tunnels and
            // its session. The cap is named so the operator who set it reads
            // the reason in the log, and the client reads it in the ack.
            debug!(
                "Refused a data tunnel for session {}: it already holds \
                 {held} tunnel(s), and `[server.data].max_tunnels_per_client` is \
                 {max_tunnels_per_client}",
                hex::encode(nonce)
            );
            return refuse_tunnel(&mut conn).await;
        }
    };

    conn.hint(SocketOpts::for_service(None));
    upgrade_to_tunnel(
        conn,
        TunnelOwner {
            sessions: Arc::clone(&registry.sessions),
            nonce,
        },
        Some(slot),
    )
    .await
}

/// The outcome of asking a session for one more tunnel.
#[cfg(feature = "multiplex")]
enum TunnelSlot {
    /// The nonce names no live session; the caller reports that itself.
    NoSession,
    /// Reserved, and held until the guard drops.
    Held(SessionTunnel),
    /// The operator's cap is reached: this many tunnels are already held.
    OverCap { held: usize },
}

/// What a v4 tunnel holds for its whole life: its slot in the session's tunnel
/// count, and the news that the session ended.
///
/// A tunnel belongs to its session, so it must not outlive it: a tunnel whose
/// session is gone has nowhere to route its streams (they are dropped with a
/// "no such service on this session"), and — with the client's pool reused
/// across a reconnect — it would keep a dead connection in the pool. The
/// receiver is the session's own shutdown broadcast, so a clean teardown, a
/// registry drop and a shutdown all end the tunnel the same way.
#[cfg(feature = "multiplex")]
struct SessionTunnel {
    /// The tunnel's reservation in its session's count. Held by the tunnel's
    /// driver task for the tunnel's whole life, so an over-cap client cannot
    /// slip a second tunnel past the valve.
    slot: TunnelGuard,
    /// The session's own shutdown broadcast: the tunnel ends with the session.
    ended: broadcast::Receiver<bool>,
}

/// Reserve one tunnel slot on a live v4 session against the operator's cap
/// (`0` = unlimited). The compare-exchange inside [`TunnelCount`] is what makes
/// concurrent tunnel hellos of one client respect the cap together.
#[cfg(feature = "multiplex")]
async fn reserve_tunnel(registry: &Registry, nonce: &Nonce, cap: usize) -> TunnelSlot {
    let guard = registry.sessions.read().await;
    let Some(session) = guard.get(nonce) else {
        return TunnelSlot::NoSession;
    };
    match session.tunnels.try_reserve(cap) {
        Ok(slot) => TunnelSlot::Held(SessionTunnel {
            slot,
            ended: session.shutdown.subscribe(),
        }),
        Err(held) => TunnelSlot::OverCap { held },
    }
}

/// Answer a tunnel that is over the cap with the typed refusal.
///
/// A tunnel hello is answered on the *fixed-width* ack path — the client reads
/// exactly one byte there — so the refusal is the unit [`Ack::TunnelRefused`]
/// and not a framed `RegisterRejected`: its payload would be misread as the
/// ack itself. The client's tunnel dial reports the variant
/// (`the server refused the multiplexed data tunnel: ...`), and the `debug!`
/// at the TCP and KCP call sites names the cap.
#[cfg(feature = "multiplex")]
async fn refuse_tunnel<S>(conn: &mut S) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    write_and_flush(conn, &postcard::to_stdvec(&Ack::TunnelRefused)?).await
}

/// Process-wide once-only line for a tunnel stream that names a service the
/// session does not know (a stale registration, or an id the client never
/// registered). A broken or hostile client must not be able to fill the log
/// with these; the stream is dropped, never the tunnel or the session (see
/// `RepeatNotice`).
#[cfg(feature = "multiplex")]
static TUNNEL_VIOLATIONS: RepeatNotice = RepeatNotice::new();

/// Report a dropped tunnel stream: once per process at `warn`, then at `debug`.
#[cfg(feature = "multiplex")]
fn report_tunnel_violation(service_id: ServiceId, why: &str) {
    TUNNEL_VIOLATIONS.report(
        || {
            warn!(
                "Dropped a tunnel stream naming service {service_id}: {why}. \
                 Further violations are logged at debug level."
            );
        },
        || {
            debug!("Dropped a tunnel stream naming service {service_id}: {why}");
        },
    );
}

/// Which session a tunnel's streams belong to.
///
/// The tunnel belongs to the session as a whole: each stream's 4-byte prologue
/// names its service, so one tunnel may carry streams of **several** services —
/// which is exactly what a shared pool (`[client.data].shared_pool`) produces,
/// and what a per-service pool produces only by accident.
#[cfg(feature = "multiplex")]
struct TunnelOwner {
    sessions: Arc<RwLock<SessionMap>>,
    nonce: Nonce,
}

/// Finalize a tunnel upgrade on a validated tunnel stream: confirm with the
/// ack (the client waits for it, so a stale nonce surfaces as a clean error
/// there), then run the yamux session and bridge every inbound stream — each
/// one a requested data channel — into the owning service's pool.
///
/// Generic over the stream type so TCP tunnels and KCP tunnels (arm 2) share
/// one implementation.
///
/// `session` is what a v4 tunnel holds for its whole life — its reservation in
/// the session's tunnel count (`[server.data].max_tunnels_per_client`) and the
/// session's shutdown signal — when it has one. This function returns as soon
/// as the tunnel is up, so both have to travel into the task that lives as
/// long as the tunnel; a v3 tunnel belongs to one control channel and takes
/// neither.
#[cfg(feature = "multiplex")]
async fn upgrade_to_tunnel<S>(
    mut conn: S,
    owner: TunnelOwner,
    session: Option<SessionTunnel>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    write_and_flush(&mut conn, &postcard::to_stdvec(&Ack::Ok)?).await?;
    let config = crate::transport::multiplex::mux_config();
    let (bridge_tx, mut bridge_rx) = mpsc::channel::<crate::transport::MuxStream>(64);
    tokio::spawn(async move {
        // Held until this tunnel's IO ends — the slot's term in the session's
        // tunnel count is exactly the tunnel's own lifetime — and ended by the
        // session's own shutdown, so a tunnel never outlives the session it
        // routes for. Both fields have to move into *this* task: destructuring
        // the reservation out and dropping the rest would release the slot (and
        // so the operator's cap) the moment the tunnel came up.
        let (slot, mut ended) = match session {
            Some(SessionTunnel { slot, ended }) => (Some(slot), Some(ended)),
            None => (None, None),
        };
        let _slot = slot;
        let tunnel = crate::transport::multiplex::run_server_tunnel(conn, config, bridge_tx);
        match ended.as_mut() {
            Some(ended) => {
                tokio::select! {
                    () = tunnel => {}
                    _ = ended.recv() => debug!("Data tunnel ended with its session"),
                }
            }
            None => tunnel.await,
        }
        debug!("Multiplexed data tunnel closed");
    });
    tokio::spawn(async move {
        while let Some(mut stream) = bridge_rx.recv().await {
            let Some(queue) = route_tunnel_stream(&mut stream, &owner.sessions, &owner.nonce).await
            else {
                // A stream this session may not carry: dropped here, never the
                // tunnel or the session.
                continue;
            };
            if queue.send(DataChannel::Mux(stream)).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// Resolve one inbound v4 tunnel stream to the queue of the service it names.
///
/// The 4-byte prologue opens the stream — the client writes it before anything
/// else, so reading it here cannot eat payload. Returns `None` when the stream
/// must be dropped: an unreadable prologue, or a service that is not registered
/// on the session. Neither may take the tunnel or the session down.
///
/// Several services may share one tunnel: with `[client.data].shared_pool` the
/// client places a session's streams on one pool, so a tunnel carries whichever
/// service each stream's prologue names. The routing is per *stream*, not per
/// tunnel, and has been since the prologue landed.
#[cfg(feature = "multiplex")]
async fn route_tunnel_stream<S>(
    stream: &mut S,
    sessions: &RwLock<SessionMap>,
    nonce: &Nonce,
) -> Option<mpsc::Sender<DataChannel>>
where
    S: AsyncRead + Unpin,
{
    let Ok(service_id) = read_stream_prologue(stream).await else {
        // The stream ended before naming a service (a client that opened it
        // and hung up): nothing to route it to.
        debug!("Dropped a tunnel stream with no prologue");
        return None;
    };
    let queue = sessions
        .read()
        .await
        .get(nonce)
        .and_then(|session| session.services.get(&service_id))
        .map(|handle| handle.data_channel.clone());
    let Some(queue) = queue else {
        report_tunnel_violation(service_id, "no such service on this session");
        return None;
    };
    Some(queue)
}

/// Accept KCP sessions (arm 2 of the transport comparison) and serve whatever
/// the hello on them turns out to be.
///
/// One listener serves both data-plane shapes, exactly as the TCP data
/// listener does: a **tunnel** hello opens a yamux session (the `multiplex`
/// mode), a **data channel** hello is a direct channel whose carrier happens to
/// be KCP. Which one it is, is the client's own declaration — the same
/// `carrier` it registered the service with — so nothing here needs a
/// configuration of its own.
#[cfg(all(feature = "kcp", feature = "multiplex"))]
async fn run_kcp_listener(
    acceptor: Arc<KcpAcceptor>,
    registry: Arc<Registry>,
    shared: Arc<ServerShared>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    max_tunnels_per_client: usize,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            session = acceptor.accept() => {
                let Some(session) = session else { break };
                let peer = session.peer;
                let registry = registry.clone();
                let shared = Arc::clone(&shared);
                tokio::spawn(
                    async move {
                        if let Err(e) = handle_kcp_session(
                            session,
                            registry,
                            shared,
                            max_tunnels_per_client,
                        )
                        .await
                        {
                            debug!("KCP tunnel session ended: {e:#}");
                        }
                    }
                    .instrument(info_span!("kcp_tunnel", %peer)),
                );
            }
        }
    }
    debug!("KCP tunnel listener stopped");
}

/// Open one accepted KCP session (transport selector, optional Noise
/// handshake), read the hello, and hand it to the path its variant names.
#[cfg(all(feature = "kcp", feature = "multiplex"))]
async fn handle_kcp_session(
    mut session: crate::transport::kcp::AcceptedSession,
    registry: Arc<Registry>,
    shared: Arc<ServerShared>,
    max_tunnels_per_client: usize,
) -> Result<()> {
    use crate::transport::kcp::KcpTunnelStream;

    // Bound the crypto + hello phase: an unresponsive or bogus peer must not
    // park a session task forever (the same role HANDSHAKE_TIMEOUT plays on
    // the TCP accept path).
    let deadline = Duration::from_secs(HANDSHAKE_TIMEOUT * 2);

    // v3 transport selector on the KCP byte stream, same rule as TCP: the
    // client announces plain or Noise, and the server honors it (the crypto
    // stack follows the connection, not a config-side type agreement).
    let selector = tokio::time::timeout(deadline, session.stream.read_u8())
        .await
        .with_context(|| "KCP transport selector timed out")??;
    let mut io = match selector {
        PLAIN_SELECTOR => KcpTunnelStream::Plain(session.stream),
        NOISE_SELECTOR => {
            #[cfg(feature = "noise")]
            {
                let keys = shared.noise_keys.as_ref().ok_or_else(|| {
                    anyhow!("Client requested Noise, but the server has no Noise keys")
                })?;
                KcpTunnelStream::Noise(Box::new(
                    tokio::time::timeout(deadline, keys.wrap_responder(session.stream))
                        .await
                        .with_context(|| "KCP session noise handshake timed out")??,
                ))
            }
            #[cfg(not(feature = "noise"))]
            {
                let _ = (session, shared);
                bail!(
                    "Client requested Noise, but this binary was built without the `noise` feature"
                )
            }
        }
        other => bail!("Unknown transport selector {other:#04x}"),
    };

    // The hello names the shape: a tunnel (multiplex mode, yamux above this
    // stream) or a direct data channel (the session *is* the channel).
    let (_version, hello) = tokio::time::timeout(deadline, read_hello(&mut io))
        .await
        .with_context(|| "KCP hello timed out")??;
    match hello {
        Hello::DataChannelHello(_, nonce) => {
            let service_id = read_stream_prologue(&mut io).await?;
            do_v4_data_channel(DataChannel::RawKcp(io), registry, nonce, service_id).await
        }
        Hello::DataChannelTunnelHello(_, nonce) => {
            let owner = tunnel_owner(&registry, nonce, "KCP").await?;
            serve_kcp_tunnel(io, owner, nonce, registry, max_tunnels_per_client).await
        }
        other @ Hello::ControlChannelHello(..) => {
            bail!("Expected a data-plane hello on the KCP session, got {other:?}")
        }
    }
}

/// Reserve the tunnel's slot on its session and run the shared upgrade.
///
/// A KCP tunnel is one of the client's multiplexed tunnels and takes a slot
/// exactly like a TCP one: the valve must not be bypassable by choosing the
/// other carrier. Shared by the TCP and KCP accept paths so the two cannot
/// disagree about the cap.
#[cfg(all(feature = "kcp", feature = "multiplex"))]
async fn serve_kcp_tunnel(
    mut io: crate::transport::kcp::KcpTunnelStream,
    owner: TunnelOwner,
    nonce: Nonce,
    registry: Arc<Registry>,
    max_tunnels_per_client: usize,
) -> Result<()> {
    let session = match reserve_tunnel(&registry, &nonce, max_tunnels_per_client).await {
        TunnelSlot::Held(slot) => Some(slot),
        TunnelSlot::NoSession => bail!("KCP tunnel hello carried an incorrect nonce"),
        TunnelSlot::OverCap { held } => {
            debug!(
                "Refused a KCP data tunnel for session {}: it already \
                 holds {held} tunnel(s), and `[server.data].max_tunnels_per_client` is \
                 {max_tunnels_per_client}",
                hex::encode(nonce)
            );
            refuse_tunnel(&mut io).await?;
            return Ok(());
        }
    };

    upgrade_to_tunnel(io, owner, session).await
}

/// Resolve the session a validated tunnel hello names.
///
/// A tunnel names its session only; every stream inside it names its own
/// service (see [`TunnelOwner`]). `arm` is the carrier's name, so a stale nonce
/// says which listener saw it.
///
/// The KCP listener is the only caller: the TCP path builds its owner inline
/// (it has no arm name to report).
#[cfg(all(feature = "multiplex", feature = "kcp"))]
async fn tunnel_owner(registry: &Registry, nonce: Nonce, arm: &str) -> Result<TunnelOwner> {
    if !registry.sessions.read().await.contains_key(&nonce) {
        bail!("{arm} tunnel hello carried an incorrect nonce");
    }
    Ok(TunnelOwner {
        sessions: Arc::clone(&registry.sessions),
        nonce,
    })
}

/// A live control channel, kept alive by holding the three channel
/// senders: dropping the handle shuts the control channel down (each is
/// a `Sender`, so the type carries the direction).
pub struct ControlChannelHandle {
    // Shutdown the control channel by dropping it
    shutdown: broadcast::Sender<bool>,
    data_channel: mpsc::Sender<DataChannel>,
    // Keeps the data-channel request channel alive for as long as the handle
    // exists: the control channel loop exits when every sender is gone.
    data_ch_req: mpsc::UnboundedSender<DataChannelRequest>,
}

impl ControlChannelHandle {
    /// Tear this service's control path down now: the pool task stops and its
    /// public port is released.
    ///
    /// Dropping a handle is the usual shutdown signal (each field is a sender),
    /// but a live multiplex tunnel's bridge task holds a *clone* of the handle
    /// for as long as that tunnel lives. A takeover or a `Deregister` that only
    /// dropped the registry's copy would therefore leave the old pool holding
    /// the public port until the tunnel died (observed: a KCP tunnel outlives
    /// its client by tens of seconds, and the re-registration that follows is
    /// rejected with "Port N is already in use"). Sending on the shutdown
    /// broadcast makes the teardown independent of whoever still holds a clone.
    fn shutdown(self) {
        let _ = self.shutdown.send(true);
    }
}

impl Clone for ControlChannelHandle {
    fn clone(&self) -> Self {
        ControlChannelHandle {
            shutdown: self.shutdown.clone(),
            data_channel: self.data_channel.clone(),
            data_ch_req: self.data_ch_req.clone(),
        }
    }
}

/// A public endpoint bound successfully for a registered service.
enum BoundEndpoint {
    Tcp(TcpListener),
    Udp(UdpSocket),
    /// A transparent service binds nothing: the address lives on the client's
    /// TUN device, and the server only routes packets into the tunnel. The
    /// claim (`Registered::claim`) is what keeps the address unique here.
    Transparent,
}

/// Every public endpoint a transparent service has claimed on this server.
///
/// The routing that feeds the tunnel is per address, so a second claimant
/// would silently steal the first one's visitors; a claim makes that a precise
/// rejection instead.
static CLAIMS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<SocketAddr>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// A held claim on one public endpoint, released when the service ends.
struct Claim(SocketAddr);

#[cfg_attr(
    not(all(feature = "transparent", target_os = "linux")),
    expect(dead_code, reason = "transparent registrations are refused here")
)]
impl Claim {
    /// `None` when another service already owns the endpoint.
    fn acquire(addr: SocketAddr) -> Option<Self> {
        let mut claims = CLAIMS.lock().ok()?;
        claims.insert(addr).then_some(Claim(addr))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut claims) = CLAIMS.lock() {
            claims.remove(&self.0);
        }
    }
}

/// Bind the service's public endpoint.
async fn bind_service_endpoint(service: &RegisteredService) -> std::io::Result<BoundEndpoint> {
    match service.service_type {
        ServiceType::Tcp => TcpListener::bind(service.bind_addr)
            .await
            .map(BoundEndpoint::Tcp),
        ServiceType::Udp => UdpSocket::bind(service.bind_addr)
            .await
            .map(BoundEndpoint::Udp),
        // Nothing to bind on this host: the address is the client's.
        ServiceType::Transparent => Ok(BoundEndpoint::Transparent),
    }
}

fn describe_bind_error(service: &RegisteredService, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        format!("Port {} is already in use", service.bind_addr.port())
    } else {
        format!("Failed to bind {}: {}", service.bind_addr, e)
    }
}

/// Bind the service's public endpoint eagerly, retrying briefly on
/// `AddrInUse`.
///
/// When a client re-registers (restart, reconnect), the previous handle is
/// dropped first and its listener sockets close *asynchronously*. Without
/// the retry window such a takeover would race with the teardown and fail
/// spuriously. The bound endpoint is returned only here so that genuine,
/// persistent conflicts surface as precise registration rejections.
async fn bind_with_retry(service: &RegisteredService) -> Result<BoundEndpoint> {
    const MAX_WAIT: Duration = Duration::from_secs(5);
    let mut waited = Duration::ZERO;
    loop {
        match bind_service_endpoint(service).await {
            Ok(bound) => return Ok(bound),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && waited < MAX_WAIT => {
                debug!(
                    service = %service.name,
                    "Bind raced with the previous teardown, retrying: {}",
                    e
                );
                time::sleep(Duration::from_millis(100)).await;
                waited += Duration::from_millis(100);
            }
            Err(e) => return Err(anyhow!("{}", describe_bind_error(service, &e))),
        }
    }
}

/// Effective data channels per visitor connection for one registration.
///
/// `[server.data].stripe_count` with the `multiplex` feature (plus the
/// measurement-only environment override), and `1` without it — a build
/// without the feature can neither produce nor consume the striped
/// command, so the unstriped shape is the only wire it speaks.
fn stripe_count(server_config: &ServerConfig) -> usize {
    #[cfg(feature = "multiplex")]
    {
        server_config.stripe_count()
    }
    #[cfg(not(feature = "multiplex"))]
    {
        let _ = server_config;
        1
    }
}

/// Why a service's pool wants a data channel.
///
/// The pool knows *what* it needs, not *how* to say it: turning a request into
/// a command is the control channel's job (see [`data_channel_cmd`]). The
/// distinction matters for a stripe group — its K requests must reach the
/// client as one group, so it can reserve one tunnel per stripe (D24) — and
/// every other request stays the plain one.
enum DataChannelRequest {
    /// One visitor arrived: an ordinary channel, placed by the client's own
    /// least-loaded rule.
    Plain,
    /// One stripe of a striped visitor connection: the group's id, the stripe's
    /// index and the group's stripe count.
    Stripe {
        group: [u8; 4],
        index: u8,
        count: u8,
    },
}

/// Where a service's control commands go.
///
/// A v3 service owns its control connection; a v4 service shares the
/// session's, so its commands are framed into the session's single writer
/// instead.
enum ControlSink {
    Session {
        service_id: ServiceId,
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
    },
}

impl ControlSink {
    /// Send one already-framed command, queued for the session's writer — the
    /// only task that touches that socket.
    fn send(&mut self, data: &[u8]) -> Result<()> {
        match self {
            ControlSink::Session { write_tx, .. } => write_tx
                .send(data.to_vec())
                .map_err(|_| anyhow!("The control session is gone")),
        }
    }

    /// The service id this sink's commands are tagged with.
    fn service_id(&self) -> ServiceId {
        match self {
            ControlSink::Session { service_id, .. } => *service_id,
        }
    }
}

/// The command one data-channel request becomes.
///
/// One function, so "the group request is the only one that carries a group"
/// stays checkable in a unit test rather than by reading two call sites: every
/// other request keeps the plain four-byte form, which is what a visitor whose
/// service is not striped sends.
fn data_channel_cmd(service_id: ServiceId, request: &DataChannelRequest) -> ControlChannelCmd {
    match request {
        DataChannelRequest::Stripe {
            group,
            index,
            count,
        } => ControlChannelCmd::CreateDataChannelForStripe(service_id, *group, *index, *count),
        DataChannelRequest::Plain => ControlChannelCmd::CreateDataChannelFor(service_id),
    }
}

impl ControlChannelHandle {
    // Create a control channel handle for an already-bound service: spawn
    // the connection pool task and the control channel handling task.
    #[instrument(name = "handle", skip_all, fields(service = %service.name))]
    fn new(
        sink: ControlSink,
        service: &RegisteredService,
        bound: BoundEndpoint,
        claim: Option<Claim>,
        heartbeat_interval: u64,
        pool_size: usize,
        stripe_count: usize,
    ) -> ControlChannelHandle {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Cache some data channels for later use
        for _i in 0..pool_size {
            if let Err(e) = data_ch_req_tx.send(DataChannelRequest::Plain) {
                debug!("Failed to request data channel {}", e);
            }
        }

        // A v4 service's pool death has to reach the client, and the session
        // owns the connection: the wrapper below reports a *failed* pool task
        // through this channel, and the per-service control task turns it into
        // `ServiceDropped`. v3 has no reporter — its control channel dies with
        // the pool anyway.
        let (pool_died_tx, pool_died_rx) = {
            let (tx, rx) = mpsc::unbounded_channel();
            (tx, rx)
        };

        let shutdown_rx_clone = shutdown_tx.subscribe();
        // Socket options for visitor-facing connections: latency-friendly
        // defaults (nodelay + keepalive)
        let sock_opts = SocketOpts::for_service(None);

        // Create the control channel and run it *before* the pool: the pool
        // takes the control task's join handle, because a control channel
        // that ends on its own (client shutdown, a connection reset, a failed
        // heartbeat write) must stop the pool too. The pool owns the bound
        // public listener, so without that the port stays occupied with
        // nothing serving it and the next registration of that service is
        // rejected with "Port N is already in use".
        let ch = ControlChannel {
            sink,
            shutdown_rx,
            data_ch_req_rx,
            heartbeat_interval,
            pool_died_rx,
        };
        let control_task = tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    // The client logs the cause of its control channel ending;
                    // the server's copy is per-session detail.
                    debug!("{:#}", err);
                }
            }
            .instrument(Span::current()),
        );

        match bound {
            BoundEndpoint::Tcp(listener) => {
                info!(service = %service.name, "Listening at {}", service.bind_addr);
                let data_ch_req_tx = data_ch_req_tx.clone();
                tokio::spawn(
                    async move {
                        if let Err(e) = run_tcp_connection_pool::<DataChannel>(
                            listener,
                            sock_opts,
                            data_ch_rx,
                            data_ch_req_tx,
                            shutdown_rx_clone,
                            control_task,
                            stripe_count,
                        )
                        .await
                        .with_context(|| "Failed to run TCP connection pool")
                        {
                            error!("{:#}", e);
                            report_pool_death(&pool_died_tx);
                        }
                    }
                    .instrument(Span::current()),
                );
            }
            BoundEndpoint::Udp(socket) => {
                info!(service = %service.name, "Listening at {}", service.bind_addr);
                let buffer_size = service.udp_buffer_size;
                let data_ch_req_tx = data_ch_req_tx.clone();
                spawn_udp_stats();
                tokio::spawn(
                    async move {
                        if let Err(e) = run_udp_connection_pool::<DataChannel>(
                            Arc::new(socket),
                            buffer_size,
                            data_ch_rx,
                            data_ch_req_tx,
                            shutdown_rx_clone,
                            control_task,
                        )
                        .await
                        .with_context(|| "Failed to run UDP connection pool")
                        {
                            error!("{:#}", e);
                            report_pool_death(&pool_died_tx);
                        }
                    }
                    .instrument(Span::current()),
                );
            }
            // A transparent service binds nothing: its packets arrive on the
            // operator's TUN, and this task moves them to and from the tunnel.
            BoundEndpoint::Transparent => spawn_transparent_service(
                service,
                data_ch_rx,
                data_ch_req_tx.clone(),
                shutdown_rx_clone,
                control_task,
                claim,
                pool_died_tx,
            ),
        }

        ControlChannelHandle {
            shutdown: shutdown_tx,
            data_channel: data_ch_tx,
            data_ch_req: data_ch_req_tx,
        }
    }
}

/// How long to wait before asking for a replacement transparent channel.
///
/// A replacement is requested when the channel for an endpoint ends; without a
/// pause, a peer that refuses the start command instantly would be asked again
/// just as instantly.
#[cfg(all(feature = "transparent", target_os = "linux"))]
const TRANSPARENT_REPLACE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// Serve one transparent service: take the channel the client opened when its
/// registration was accepted, hand the device's packets to it, and inject what
/// comes back.
///
/// One channel at a time. When it ends, a replacement is requested — the
/// client opens on demand, the same path a UDP worker's replacement takes —
/// and the tunnel pool places it, because the service must not give up on an
/// address it holds.
#[cfg(all(feature = "transparent", target_os = "linux"))]
async fn run_transparent_service<C>(
    mut data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    tun: String,
    endpoint: SocketAddr,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_task: tokio::task::JoinHandle<()>,
    claim: Option<Claim>,
) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use crate::transparent::hub::{TunHub, forward_transparent};
    use crate::transparent::{Endpoint, Stats};

    let stats = Arc::new(Stats::default());
    crate::transparent::spawn_stats_reporter("server", Arc::clone(&stats));
    let hub = TunHub::get_or_spawn(
        &tun,
        Arc::clone(&stats),
        crate::transparent::Direction::Destination,
    )?;
    let endpoint = Endpoint::new(endpoint.ip(), endpoint.port());
    // Held for the service's lifetime: the address stays claimed until this
    // task ends, and the claim is what a second client's registration hits.
    let _claim = claim;

    let start_cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardTransparent)?;
    let mut first_channel = true;
    loop {
        let channel = tokio::select! {
            channel = data_ch_rx.recv() => channel,
            _ = shutdown_rx.recv() => return Ok(()),
            _ = &mut control_task => return Ok(()),
        };
        let Some(mut channel) = channel else {
            return Ok(());
        };

        if !first_channel {
            // The previous channel ended. Pace the replacement: a client that
            // cannot serve this command (an older build, a broken data path of
            // its own) must not be asked for a new channel in a tight loop.
            tokio::time::sleep(TRANSPARENT_REPLACE_BACKOFF).await;
        }
        first_channel = false;

        if let Err(e) = write_and_flush(&mut channel, &start_cmd).await {
            debug!("Transparent channel for {endpoint} died before starting: {e:#}");
            let _ = data_ch_req_tx.send(DataChannelRequest::Plain);
            continue;
        }
        if let Err(e) =
            forward_transparent(channel, Arc::clone(&hub), endpoint, Arc::clone(&stats)).await
        {
            debug!("Transparent channel for {endpoint} ended: {e:#}");
        }
        // No channel for this endpoint any more: ask for one, and keep holding
        // the address while the client opens it.
        let _ = data_ch_req_tx.send(DataChannelRequest::Plain);
    }
}

/// Start a transparent service's data path.
///
/// Not a pool runner: there is no listener to accept on. It waits for the
/// channel the client opened when its registration was accepted, then moves
/// packets between the operator's TUN device and that channel until either
/// end stops, asking for a replacement channel when one does.
fn spawn_transparent_service(
    service: &RegisteredService,
    data_ch_rx: mpsc::Receiver<DataChannel>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    shutdown_rx: broadcast::Receiver<bool>,
    control_task: tokio::task::JoinHandle<()>,
    claim: Option<Claim>,
    pool_died_tx: mpsc::UnboundedSender<()>,
) {
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    {
        let tun = service.transparent_tun.clone().unwrap_or_default();
        let endpoint = service.bind_addr;
        tokio::spawn(
            async move {
                if let Err(e) = run_transparent_service(
                    data_ch_rx,
                    data_ch_req_tx,
                    tun,
                    endpoint,
                    shutdown_rx,
                    control_task,
                    claim,
                )
                .await
                {
                    error!("{:#}", e);
                    report_pool_death(&pool_died_tx);
                }
            }
            .instrument(Span::current()),
        );
    }
    #[cfg(not(all(feature = "transparent", target_os = "linux")))]
    {
        // A registration is refused long before this on such a build; the
        // parameters are consumed so the signature stays one shape.
        let _ = (
            service,
            data_ch_rx,
            data_ch_req_tx,
            shutdown_rx,
            control_task,
            claim,
            pool_died_tx,
        );
    }
}

/// Report that a service's pool task could not serve its listener any more./// Report that a service's pool task could not serve its listener any more.
///
/// Only the *abnormal* end is reported: a pool that returns `Ok` ended
/// orderly (shutdown, or the control channel gone), and must never tell the
/// client its service was dropped. A no-op in v3, where the pool's end already
/// stops that service's control channel.
fn report_pool_death(tx: &mpsc::UnboundedSender<()>) {
    let _ = tx.send(());
}

/// Wait for a service's pool task to report that it died. Never resolves when
/// the service has no reporter (v3) or while its pool is healthy.
async fn recv_pool_death(rx: &mut mpsc::UnboundedReceiver<()>) -> Option<()> {
    rx.recv().await
}

/// One service's control path.
///
/// v3: the service owns the connection, so this task writes its commands
/// directly and runs the per-service heartbeat. v4: the session owns the
/// connection, so this task only translates the pool's requests into
/// service-tagged commands for the session's writer.
struct ControlChannel {
    sink: ControlSink,                      // Where the commands go
    shutdown_rx: broadcast::Receiver<bool>, // Receives the shutdown signal
    data_ch_req_rx: mpsc::UnboundedReceiver<DataChannelRequest>, // Receives visitor connections
    heartbeat_interval: u64,                // Application-layer heartbeat interval in secs
    pool_died_rx: mpsc::UnboundedReceiver<()>, // the pool's death notice
}

impl ControlChannel {
    // Run a control channel
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        let service_id = self.sink.service_id();
        // The ordinary request's command, framed once: it is the same bytes for
        // every visitor. The striped one cannot be cached like this — its
        // payload differs per stripe — so it is framed per request below.
        let create_ch_cmd =
            postcard::to_stdvec(&ControlChannelCmd::CreateDataChannelFor(service_id))?;
        let heartbeat = postcard::to_stdvec(&ControlChannelCmd::HeartBeat)?;
        let dropped_cmd = Some(postcard::to_stdvec(&ControlChannelCmd::ServiceDropped(
            service_id,
        ))?);

        // Wait for data channel requests and the shutdown signal
        loop {
            tokio::select! {
                val = self.data_ch_req_rx.recv() => {
                    match val {
                        Some(request) => {
                            // The plain request is the common case and its frame
                            // is byte-identical every time, so it is cached; the
                            // striped one carries its group and is framed per
                            // request.
                            let striped;
                            let framed: &[u8] = match data_channel_cmd(service_id, &request) {
                                ControlChannelCmd::CreateDataChannelFor(_) => &create_ch_cmd,
                                cmd => {
                                    striped = postcard::to_stdvec(&cmd)?;
                                    &striped
                                }
                            };
                            if let Err(e) = self.sink.send(framed) {
                                // The client is gone: one session's end. Its
                                // own log says why.
                                debug!("{:#}", e);
                                break;
                            }
                        }
                        None => {
                            break;
                        }
                    }
                },
                // v4 only: the pool task ended abnormally — its listener could
                // not be served any more — so the client is told the service is
                // no longer exposed. The session stays up for its siblings.
                Some(()) = recv_pool_death(&mut self.pool_died_rx) => {
                    match dropped_cmd.as_deref() {
                        Some(cmd) => {
                            if let Err(e) = self.sink.send(cmd) {
                                debug!("{:#}", e);
                            }
                        }
                        None => break,
                    }
                    break;
                }
                () = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
                            if let Err(e) = self.sink.send(&heartbeat) {
                                debug!("{:#}", e);
                                break;
                            }
                }
                // Wait for the shutdown signal
                _ = self.shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        Ok(())
    }
}

// Accept visitors on the pre-bound listener and pair each of them with a
// data channel from the pool.
//
// `stripe_count` data channels are paired per visitor: `1` is the classic
// one-channel shape; a higher count spreads the visitor connection over
// that many parallel channels (a stripe group, see `crate::stripe`), which
// multiplies its ceiling and window. The unstriped path (`stripe_count`
// 1) is the default and stays the single-variable control for the striped
// one.

/// What the pairing wait decided.
enum PairOutcome<C> {
    /// A data channel to hand this visitor.
    Channel(C),
    /// No channel arrived within the visitor's allowance: shed it.
    Shed,
    /// The pool itself must stop (shutdown, or the control channel ended).
    Stop,
}

/// Wait for a data channel for one visitor, asking for more when none arrives.
///
/// This used to be the accept loop's critical section — as long as it waited
/// for one visitor the service accepted no other. A request the client cannot
/// answer — the pool is at its placement ceiling and refuses the open, which
/// the client reports to nobody — would otherwise park the whole service for
/// the rest of the session. Measured: a saturated pool left the service unable
/// to serve a *fresh* visitor even with every stream released
/// (`tests/pool_test.rs`, `a_saturated_pool_still_serves_the_next_visitor`).
///
/// The wait is now per visitor, so its bound protects one visitor instead of
/// the service, and `MAX_CONCURRENT_VISITORS` bounds how many can wait at once.
/// The wait itself is still a budget, and its expiry re-requests rather than
/// giving up: capacity usually comes back (a stream retires, a tunnel grows)
/// and at most this one visitor is waiting, so a re-request is cheap. Only a
/// visitor the client refuses `PAIR_ATTEMPTS` times in a row is shed — a typed
/// failure for that visitor and nothing for the service.
async fn pair_visitor<C>(
    data_ch_rx: &SharedChannels<C>,
    data_ch_req_tx: &mpsc::UnboundedSender<DataChannelRequest>,
    shutdown_rx: &mut broadcast::Receiver<bool>,
    control_alive: &mut tokio::sync::watch::Receiver<bool>,
) -> PairOutcome<C>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut attempts = 0usize;
    loop {
        // A visitor can be waiting for a data channel that will never arrive
        // once its control channel is gone, so this loop watches both signals
        // too.
        let next = tokio::select! {
            _ = shutdown_rx.recv() => None,
            _ = control_alive.changed() => None,
            ch = take_channel(data_ch_rx) => ch,
            () = time::sleep(PAIR_WAIT_BUDGET) => {
                // Nothing arrived in the budget: ask again, unless this
                // visitor has waited out its whole allowance.
                attempts += 1;
                if attempts >= PAIR_ATTEMPTS {
                    debug!("No data channel after {attempts} requests; dropping the visitor");
                    return PairOutcome::Shed;
                }
                if data_ch_req_tx.send(DataChannelRequest::Plain).is_err() {
                    return PairOutcome::Stop;
                }
                continue;
            }
        };
        let Some(ch) = next else {
            return PairOutcome::Stop;
        };
        return PairOutcome::Channel(ch);
    }
}

/// The visitor pairings one TCP service pool allows at once.
///
/// The pairing wait is the accept loop's critical section only while pairing is
/// serial: a visitor whose channel request the client cannot answer used to
/// hold the accept loop for the whole wait, and every visitor behind it queued
/// in the kernel backlog (measured: a saturated pool left a *fresh* visitor
/// waiting even after the pool had drained). Pairing is per visitor now, so this
/// bound is what keeps a wedged service from spawning unbounded tasks — each
/// in-flight pairing owns one visitor socket and one data channel, and the
/// backlog keeps the rest.
const MAX_CONCURRENT_VISITORS: usize = 128;

/// The data channels a service pool hands out, shared by every visitor pairing
/// in flight.
///
/// A channel is interchangeable between the service's visitors — the queue
/// belongs to one service and every channel on it carries that service's
/// prologue — so the mutex serializes only the *take*, never the wait.
type SharedChannels<C> = std::sync::Arc<tokio::sync::Mutex<mpsc::Receiver<C>>>;

/// Take one data channel from the shared queue, or `None` when the pool's
/// channel source ended (the control channel did).
///
/// The guard is dropped with the future, so a pairing that loses the race to its
/// own timeout budget releases the queue for the other visitors.
async fn take_channel<C>(rx: &SharedChannels<C>) -> Option<C>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    rx.lock().await.recv().await
}

#[instrument(skip_all)]
async fn run_tcp_connection_pool<C>(
    l: TcpListener,
    sock_opts: SocketOpts,
    data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_task: tokio::task::JoinHandle<()>,
    stripe_count: usize,
) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    info!("Listening at {}", l.local_addr()?);

    // Retry at least every 1s
    let backoff_builder = ExponentialBuilder::default().with_max_delay(Duration::from_secs(1));
    let mut backoff = backoff_builder.build();
    let listen_notice = RepeatNotice::new();

    let data_ch_rx: SharedChannels<C> = std::sync::Arc::new(tokio::sync::Mutex::new(data_ch_rx));
    // A striped gather is atomic by construction: K channels consumed by one
    // visitor, all or none. Concurrent unstriped pairings take channels one by
    // one from the shared queue and cannot interfere with each other, but a
    // gather in flight must not steal the channel a waiting unstriped visitor
    // was promised — so the striped path holds this lock for the whole group.
    let stripe_gather = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let pool = VisitorPool {
        data_ch_rx,
        data_ch_req_tx: data_ch_req_tx.clone(),
        cmd: std::sync::Arc::new(postcard::to_stdvec(&DataChannelCmd::StartForwardTcp)?),
        stripe_gather,
        stripe_count,
    };
    let visitor_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_VISITORS));
    // The control task's liveness as a shareable signal. A `JoinHandle` cannot
    // be cloned and polling one from several tasks is not allowed, so one
    // watcher task owns it and every consumer — the accept loop and each
    // pairing in flight — reads the same channel: the value flips, or the
    // sender is dropped with the watcher, and both end the wait.
    let (control_alive_tx, control_alive_rx) = tokio::sync::watch::channel(true);
    tokio::spawn(async move {
        let _ = (&mut control_task).await;
        let _ = control_alive_tx.send(false);
    });
    // The accept loop's own view of that signal, bound before the loop: the
    // loop re-arms `changed()` on every iteration.
    let mut control_alive = control_alive_rx.clone();

    'pool: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            // The control channel ended without a replacement registration:
            // release the listener instead of holding the port for a service
            // nobody drives any more.
            _ = control_alive.changed() => break,
            // Take a pairing slot before accepting: the bound is on pairings
            // in flight, not on connections, so a service whose client cannot
            // answer stops *inside* the bound instead of parking the accept
            // loop behind one visitor.
            permit = visitor_slots.clone().acquire_owned() => {
                let Ok(permit) = permit else { break };
                // Accept under the same two stop signals as the loop itself: a
                // shutdown that arrives while the listener is idle must release
                // the listener, not wait for the next connection.
                let mut control_alive = control_alive_rx.clone();
                let accepted = tokio::select! {
                    _ = shutdown_rx.recv() => None,
                    _ = control_alive.changed() => None,
                    val = l.accept() => Some(val),
                };
                match accepted {
                    // Shutdown, or the control channel ended.
                    None => break 'pool,
                    Some(Err(e)) => {
                    // Give the slot back before sleeping: a listener error is
                    // not this visitor's doing, and the retry must not hold a
                    // pairing slot while it waits.
                    drop(permit);
                    // `l` is a TCP listener so this must be an IO error —
                    // possibly EMFILE, which is the operator's problem and
                    // therefore loud once, then debug while it retries.
                    listen_notice.report(
                        || error!("{e}. Sleep for a while"),
                        || debug!("{e}. Sleep for a while"),
                    );
                    if let Some(d) = backoff.next() {
                        time::sleep(d).await;
                    } else {
                        // This branch will never be reached for current backoff
                        // policy. It is an abnormal end all the same: the
                        // listener could not be served any more, which the
                        // caller reports as a failed pool task.
                        error!("Too many retries. Aborting...");
                        bail!("Too many retries. Aborting...");
                    }
                }
                    Some(Ok((incoming, addr))) => {
                        listen_notice.clear();
                        debug!("New visitor from {}", addr);
                        // The visitor socket gets the same latency-friendly
                        // defaults as the rest of the forwarding path.
                        sock_opts.apply(&incoming);
                        backoff = backoff_builder.build();
                        tokio::spawn(
                            serve_tcp_visitor(
                                incoming,
                                pool.clone(),
                                shutdown_rx.resubscribe(),
                                control_alive_rx.clone(),
                                permit,
                            )
                            .instrument(Span::current()),
                        );
                    }
            }
            }
        }
    }

    info!("Shutdown");
    Ok(())
}

/// Everything a visitor pairing needs from its service pool, shared by every
/// visitor in flight (the arcs clone per visitor; only the arriving socket and
/// the stop signals are per-visitor).
struct VisitorPool<C> {
    /// Channels to pair visitors with, one take at a time.
    data_ch_rx: SharedChannels<C>,
    /// How to ask the client for one more (or K more) data channels, named as
    /// a group when they are a visitor's stripes.
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    /// The `StartForwardTcp` command, serialized once for the pool.
    cmd: std::sync::Arc<Vec<u8>>,
    /// Held for the whole gather, so a striped group's K channels stay atomic.
    stripe_gather: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Channels per visitor: 1 is the classic shape, K is a stripe group.
    stripe_count: usize,
}

impl<C> Clone for VisitorPool<C> {
    fn clone(&self) -> Self {
        Self {
            data_ch_rx: std::sync::Arc::clone(&self.data_ch_rx),
            data_ch_req_tx: self.data_ch_req_tx.clone(),
            cmd: std::sync::Arc::clone(&self.cmd),
            stripe_gather: std::sync::Arc::clone(&self.stripe_gather),
            stripe_count: self.stripe_count,
        }
    }
}

/// Pair one accepted visitor with its data channels and start the forward.
///
/// Everything that used to run inside the accept loop runs per visitor here, so
/// one visitor's pairing wait — bounded by `PAIR_ATTEMPTS × PAIR_WAIT_BUDGET`
/// — costs that visitor and nothing else. The visitor slot (`permit`) lives for
/// exactly the pairing: the copy task it spawns ends with the connection, and
/// holding a slot for it would turn a long-lived connection into a missing
/// slot.
async fn serve_tcp_visitor<C>(
    mut incoming: TcpStream,
    pool: VisitorPool<C>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_alive: tokio::sync::watch::Receiver<bool>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    if pool.stripe_count <= 1 {
        // A channel that dies before it carries the forward command is a stale
        // lease, not this visitor's fault — the client dials the local service
        // *after* it receives that command, so a channel whose backend leg was
        // already gone fails here and nowhere else. Dropping the visitor then
        // is what the shaped-path measurement saw as `control socket has
        // closed unexpectedly`: an iperf3 control connection was paired with a
        // dying channel, the server closed the visitor's socket, and every
        // later dial of the stage inherited the failure. Re-request instead,
        // under the same allowance a *missing* channel gets.
        let mut dead_channels = 0usize;
        loop {
            if pool
                .data_ch_req_tx
                .send(DataChannelRequest::Plain)
                .with_context(|| "Failed to send data chan create request")
                .is_err()
            {
                // An error indicates the control channel is broken.
                return;
            }
            match pair_visitor(
                &pool.data_ch_rx,
                &pool.data_ch_req_tx,
                &mut shutdown_rx,
                &mut control_alive,
            )
            .await
            {
                PairOutcome::Channel(mut ch) => {
                    if write_and_flush(&mut ch, &pool.cmd).await.is_ok() {
                        tokio::spawn(
                            async move {
                                // A stalled forward is closed by the watchdog: a
                                // wedged visitor must not hold a tunnel stream
                                // for the session's life (see
                                // `FORWARD_IDLE_TIMEOUT`).
                                if let Err(e) = copy_bidirectional_with_idle(
                                    &mut ch,
                                    &mut incoming,
                                    TCP_COPY_BUFFER_SIZE,
                                    FORWARD_IDLE_TIMEOUT,
                                )
                                .await
                                {
                                    debug!("Data channel closed: {e}");
                                }
                            }
                            .instrument(Span::current()),
                        );
                        return;
                    }
                    dead_channels += 1;
                    if dead_channels >= PAIR_ATTEMPTS {
                        debug!(
                            "A data channel died before the forward command \
                             {dead_channels} times; dropping the visitor"
                        );
                        return;
                    }
                }
                // Both ends leave nothing to release: `Shed` failed this
                // visitor's request (the service keeps serving), and `Stop`
                // means the pool's owning task has already ended. Dropping
                // `incoming` closes the visitor's socket, which is the refusal
                // it sees.
                PairOutcome::Shed | PairOutcome::Stop => return,
            }
        }
    }

    // The gather is atomic: hold the group lock for the whole attempt so
    // concurrent visitors cannot interleave their K channels.
    let _gather = pool.stripe_gather.lock().await;
    // The boolean told the accept loop to stop; for one visitor both
    // outcomes are "nothing left to do here".
    if let Err(e) = pair_striped_group(
        incoming,
        pool.stripe_count,
        &pool.data_ch_rx,
        &pool.data_ch_req_tx,
        &mut shutdown_rx,
        &mut control_alive,
    )
    .await
    {
        debug!("Striped pairing failed: {e:#}");
    }
}

/// Pair one visitor connection with a stripe group: gather `stripe_count`
/// healthy data channels, announce each one as a stripe of the group, and
/// spawn the group's forwarding.
///
/// The caller holds the stripe-gather lock for the whole call, which is what
/// keeps a gather atomic: concurrent unstriped pairings take channels one by
/// one and cannot interfere with each other, but two gathers in flight would
/// interleave their K channels and produce two broken groups.
///
/// The requests name the group (see [`DataChannelRequest::Stripe`]), so the
/// client can place the K channels on K distinct tunnels. The index a request
/// carries is the slot the stripe will take *when it arrives*: the gather
/// labels channels in arrival order, and the client learns its real index from
/// `StartForwardStripedTcp` — nothing about the pairing depends on a request's
/// index coming back with it.
///
/// Returns `Ok(true)` when the control channel ended mid-gather (the pool must
/// stop) and `Ok(false)` once the group is forwarding. A broken pooled
/// channel discards the whole attempt — the client already parked the
/// group's stripes, and a retry under a fresh group id is the only way to
/// keep the indices consistent.
#[instrument(skip_all, fields(stripes = stripe_count))]
async fn pair_striped_group<C>(
    incoming: TcpStream,
    stripe_count: usize,
    data_ch_rx: &SharedChannels<C>,
    data_ch_req_tx: &mpsc::UnboundedSender<DataChannelRequest>,
    shutdown_rx: &mut broadcast::Receiver<bool>,
    control_alive: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let stripes = u8::try_from(stripe_count).with_context(|| "stripe count exceeds u8")?;
    // Each iteration is one gather attempt under a fresh group id.
    'gather: loop {
        let group = GROUP_IDS.fetch_add(1, Ordering::Relaxed);
        let group_bytes = group.to_be_bytes();
        let cmds = stripe_cmds(group_bytes, stripes)?;
        // One striped request per stripe, before the first wait: the
        // unstriped path asks for its channel this way, and a striped gather
        // that does not ask simply waits for channels nobody was told to
        // open — the shape of the original defect (the client used to
        // pre-open nothing, so the gather hung until the visitor's own
        // read timed out). A request the client refuses is answered with
        // nothing at all, which is why the wait below re-asks.
        for index in 0..stripes {
            if data_ch_req_tx
                .send(stripe_request(group_bytes, index, stripes))
                .is_err()
            {
                return Ok(true);
            }
        }
        let mut gathered: Vec<C> = Vec::with_capacity(stripe_count);
        let mut attempts = 0usize;
        loop {
            let next = tokio::select! {
                _ = shutdown_rx.recv() => None,
                _ = control_alive.changed() => None,
                ch = take_channel(data_ch_rx) => ch,
                () = time::sleep(PAIR_WAIT_BUDGET) => {
                    // Nothing arrived inside the budget. Only the channels this
                    // group still lacks are asked for again: the ones that
                    // arrived are paired already, and re-asking for those would
                    // open channels no visitor needs.
                    attempts += 1;
                    if attempts >= PAIR_ATTEMPTS {
                        debug!("No data channel after {attempts} requests; dropping the visitor");
                        // Nothing to release: a request the client refused left
                        // nothing behind, and the channels that did arrive are
                        // parked in the client's registry until its TTL reaps
                        // them there.
                        return Ok(false);
                    }
                    // The stripes still missing are the slots from the next
                    // arrival on: the channels already gathered took the slots
                    // before them.
                    let next = u8::try_from(gathered.len())
                        .with_context(|| "stripe index exceeds u8")?
                        .min(stripes);
                    for index in next..stripes {
                        if data_ch_req_tx
                            .send(stripe_request(group_bytes, index, stripes))
                            .is_err()
                        {
                            return Ok(true);
                        }
                    }
                    continue;
                }
            };
            let Some(mut ch) = next else {
                return Ok(true);
            };
            if write_and_flush(&mut ch, &cmds[gathered.len()])
                .await
                .is_ok()
            {
                gathered.push(ch);
                if gathered.len() == stripe_count {
                    break;
                }
            } else {
                // A broken pooled channel: drop the attempt (the client's
                // registry reaps its parked stripes) and gather a fresh one.
                // The next iteration's own requests replace the channels that
                // died with it, so nothing is re-asked here.
                drop(gathered);
                continue 'gather;
            }
        }
        debug!("Visitor paired with a {stripe_count}-stripe group {group}");
        let (read, write) = incoming.into_split();
        crate::stripe::spawn_group(read, write, gathered);
        return Ok(false);
    }
}

/// Group ids for striped visitor connections: wrapping is fine — a group is
/// transient, and the client prunes abandoned ones by age.
static GROUP_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The striped request for one stripe slot of a gather.
///
/// The index and the count are `u8`s on the wire (`DataChannelCmd` fixes the
/// same width for the group's own `StartForwardStripedTcp`), and `count` is the
/// gather's real stripe count, so the client's growth target and the group's
/// shape cannot disagree.
fn stripe_request(group: [u8; 4], index: u8, count: u8) -> DataChannelRequest {
    DataChannelRequest::Stripe {
        group,
        index,
        count,
    }
}

/// The per-stripe `StartForwardStripedTcp` commands of one gather attempt,
/// in arrival order. Each is 7 bytes (tag + fixed-width group id + index +
/// count), so `write_and_flush` emits it as a single frame.
fn stripe_cmds(group: [u8; 4], stripes: u8) -> Result<Vec<Vec<u8>>> {
    (0..stripes)
        .map(|index| {
            let cmd = DataChannelCmd::StartForwardStripedTcp(group, index, stripes);
            let bytes = postcard::to_stdvec(&cmd)?;
            Ok::<Vec<u8>, anyhow::Error>(bytes)
        })
        .collect()
}

/// Visitor-bound datagram queue into one data-channel worker.
type UdpWorkerQueue = mpsc::Sender<(SocketAddr, Bytes)>;
/// Live data-channel workers, keyed by a monotonically increasing id.
type UdpWorkerMap = Mutex<HashMap<usize, UdpWorkerQueue>>;
/// Session-affinity table: remote peer -> assigned data channel.
type UdpRouteMap = Mutex<HashMap<SocketAddr, UdpRoute>>;

/// Session-affinity entry: the data channel (`worker`) a remote peer's
/// datagrams are routed to, and when the peer was last seen (for TTL
/// eviction).
struct UdpRoute {
    worker: usize,
    last_seen: Instant,
}

/// The affinity state one UDP pool reports to the `MOLEHILL_UDP_STATS` line.
///
/// `affinity` is the table's live size and `evictions` its cumulative expiry
/// count: the two numbers that say whether the TTL is doing its job (a table
/// that only grows means address churn is winning; one that never evicts means
/// the TTL is long enough not to matter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UdpPoolStats {
    /// The service's public endpoint, the line's identity.
    pub(crate) bind_addr: String,
    /// Live affinity entries.
    pub(crate) affinity: usize,
    /// Entries expired by the TTL sweep since the pool started.
    pub(crate) evictions: u64,
    /// Live data-channel workers.
    pub(crate) workers: usize,
    /// Per-worker `(worker id, peers pinned to it)`, ascending by id.
    pub(crate) pinned: Vec<(usize, usize)>,
    /// The visitor-datagram drops this pool counted.
    pub(crate) drops_queue_full: u64,
    pub(crate) drops_no_worker: u64,
}

/// The live state of one UDP pool, shared with the stats task.
struct UdpPoolShared {
    /// The pool's worker table and affinity table, for the snapshot.
    workers: Arc<UdpWorkerMap>,
    routes: Arc<UdpRouteMap>,
    /// Cumulative TTL evictions.
    evictions: AtomicU64,
    bind_addr: String,
}

impl UdpPoolShared {
    /// A consistent view of the pool, for the periodic line and for tests.
    fn snapshot(&self) -> UdpPoolStats {
        let workers = self.workers.lock().unwrap_or_else(PoisonError::into_inner);
        let routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        // A peer is *pinned* to the worker its affinity entry points at: this
        // is the server's own count (D30), and it is what says a data channel
        // is still serving a live UDP session.
        let mut pinned: HashMap<usize, usize> = HashMap::new();
        for route in routes.values() {
            *pinned.entry(route.worker).or_insert(0) += 1;
        }
        let mut pinned: Vec<(usize, usize)> = pinned.into_iter().collect();
        pinned.sort_unstable();
        UdpPoolStats {
            bind_addr: self.bind_addr.clone(),
            affinity: routes.len(),
            evictions: self.evictions.load(Ordering::Relaxed),
            workers: workers.len(),
            pinned,
            drops_queue_full: UDP_DROPS_QUEUE_FULL.load(Ordering::Relaxed),
            drops_no_worker: UDP_DROPS_NO_WORKER.load(Ordering::Relaxed),
        }
    }
}

/// Every live UDP pool of this process, for the `MOLEHILL_UDP_STATS` line.
///
/// `Once`-guarded and weak: the reporter reports each pool exactly while it
/// lives, instead of one line per pool *creation* per process.
static UDP_POOLS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<UdpPoolShared>>>> =
    std::sync::OnceLock::new();

fn udp_pools() -> &'static std::sync::Mutex<Vec<std::sync::Weak<UdpPoolShared>>> {
    UDP_POOLS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Register one UDP pool with the telemetry, when `MOLEHILL_UDP_STATS` is on.
fn register_udp_pool(shared: &Arc<UdpPoolShared>) {
    if std::env::var_os("MOLEHILL_UDP_STATS").is_none() {
        return;
    }
    udp_pools()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(shared));
}

/// Cleans up after a UDP worker task exits: removes its queue from the
/// routing table and asks the control channel for a replacement data channel
/// so the pool keeps its size. Runs on normal exits and on panics (the guard
/// drops on unwind).
struct UdpWorkerGuard {
    id: usize,
    workers: Arc<UdpWorkerMap>,
    req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    shutting_down: Arc<AtomicBool>,
}

impl Drop for UdpWorkerGuard {
    fn drop(&mut self) {
        let removed = self
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id)
            .is_some();
        if removed && !self.shutting_down.load(Ordering::Relaxed) {
            debug!(
                "UDP data channel {} exited, requesting a replacement",
                self.id
            );
            // Fails only when the control channel is gone; the pool loop
            // breaks on its own in that case.
            let _ = self.req_tx.send(DataChannelRequest::Plain);
        }
    }
}

/// The UDP pool's worker set: the live workers plus the state a spawn and a
/// route both need.
///
/// Bundled for one reason — the spawn below takes the whole set, and passing
/// its pieces one at a time made a six-argument helper whose arguments were
/// the pool's own state in every slot.
struct UdpWorkerSet {
    /// Live data channels by worker id. Workers remove their own entry on
    /// exit (via the guard) and request a replacement, so the pool keeps its
    /// size for the session's lifetime.
    workers: Arc<UdpWorkerMap>,
    /// Asks the control channel for a replacement channel when a worker exits.
    req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    /// Set while the pool shuts down, so an exit asks for nothing.
    shutting_down: Arc<AtomicBool>,
    /// The next worker id, and the round-robin cursor a route starts from.
    next: usize,
}

impl UdpWorkerSet {
    /// Start one data-channel worker and register its queue.
    ///
    /// The worker set is exactly what the client opened for the service's
    /// configured count (D31): this is the *only* place a worker is added, and
    /// it is reached only for a data channel the client actually sent. A new
    /// visitor source never creates one; [`route_udp_datagram`] drops its
    /// datagram instead.
    fn spawn_worker<C>(
        &mut self,
        l: &Arc<UdpSocket>,
        conn: C,
        buffer_size: usize,
        shutdown_rx: &broadcast::Receiver<bool>,
    ) where
        C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(DEFAULT_UDP_SENDQ_SIZE);
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let guard = UdpWorkerGuard {
            id,
            workers: Arc::clone(&self.workers),
            req_tx: self.req_tx.clone(),
            shutting_down: Arc::clone(&self.shutting_down),
        };
        tokio::spawn(udp_forward_worker(
            Arc::clone(l),
            conn,
            rx,
            shutdown_rx.resubscribe(),
            buffer_size,
            guard,
        ));
    }
}

/// Accept visitors on the pre-bound UDP socket and route every peer's
/// datagrams to the data channel assigned to it.
///
/// Session affinity is the point: with a plain "every worker reads the
/// socket" pool, the kernel hands each datagram to an arbitrary worker, so
/// one peer's packets traverse different channels and leave the proxy client
/// through different local sockets. Stateful UDP (`RakNet`, QUIC,
/// `WireGuard`, ...) pins sessions to the `(ip, port)` tuple and breaks apart
/// when the proxy splits a peer across source ports.
#[instrument(skip_all)]
async fn run_udp_connection_pool<C>(
    l: Arc<UdpSocket>,
    buffer_size: usize,
    mut data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_task: tokio::task::JoinHandle<()>,
) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    info!("Listening at {}", l.local_addr()?);

    let cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardUdp)?;

    let worker_set = UdpWorkerSet {
        workers: Arc::new(Mutex::new(HashMap::new())),
        req_tx: data_ch_req_tx.clone(),
        shutting_down: Arc::new(AtomicBool::new(false)),
        next: 0,
    };
    let workers = Arc::clone(&worker_set.workers);
    let shutting_down = Arc::clone(&worker_set.shutting_down);
    // The affinity table: peer address -> assigned data channel.
    let routes: Arc<UdpRouteMap> = Arc::new(Mutex::new(HashMap::new()));
    // The worker ids and the round-robin cursor. Shared with the telemetry so
    // the periodic line reports live state instead of a spawn-time snapshot.
    let shared = Arc::new(UdpPoolShared {
        workers: Arc::clone(&workers),
        routes: Arc::clone(&routes),
        evictions: AtomicU64::new(0),
        bind_addr: l.local_addr()?.to_string(),
    });
    register_udp_pool(&shared);
    let mut worker_set = worker_set;
    // One socket reader: `recv_from` is the single entry point for all
    // visitors, and the affinity table below decides the channel. A single
    // reader also means one slow worker can never stall other peers.
    //
    // The read buffer is a whole datagram wide, and `udp_buffer_size` is applied
    // to what was read rather than to the buffer it was read into. Reading
    // straight into a `buffer_size` buffer would save ~63 KiB per service, but
    // that kernel-level truncation only happens on POSIX: Windows fills the
    // buffer with the datagram's prefix and *fails* the read with
    // `WSAEMSGSIZE`, and a failed `recv_from` is also where the visitor's
    // address is lost — so the datagram could neither be truncated to the
    // service's limit nor routed to its peer. One full-size buffer per UDP
    // service buys one documented behaviour on every platform, and a datagram
    // over the limit costs only the copy of its own bytes (the truncation
    // below), not a lost datagram or a dead pool.
    let mut buf = vec![0u8; usize::from(u16::MAX)];

    let mut sweep = time::interval(Duration::from_secs(UDP_ROUTE_TTL_SECS));
    // The first tick of an interval completes immediately; consume it.
    sweep.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                shutting_down.store(true, Ordering::Relaxed);
                break;
            }
            // The control channel ended without a replacement registration:
            // release the visitor-facing socket (see the TCP pool).
            _ = &mut control_task => {
                shutting_down.store(true, Ordering::Relaxed);
                break;
            }
            maybe_chan = data_ch_rx.recv() => {
                let Some(mut conn) = maybe_chan else {
                    shutting_down.store(true, Ordering::Relaxed);
                    break;
                };
                if let Err(e) = write_and_flush(&mut conn, &cmd).await {
                    debug!("Failed to init UDP channel: {:#}", e);
                    continue;
                }
                worker_set.spawn_worker(&l, conn, buffer_size, &shutdown_rx);
            }
            recv = l.recv_from(&mut buf) => match recv {
                Ok((n, from)) => {
                    // The service's registered `udp_buffer_size`: a datagram
                    // longer than it arrives as its prefix (see the buffer
                    // comment above).
                    let n = n.min(buffer_size);
                    match route_udp_datagram(
                        &workers,
                        &routes,
                        &mut worker_set.next,
                        from,
                        Bytes::copy_from_slice(&buf[..n]),
                    ) {
                        UdpRouteOutcome::Enqueued => {}
                        UdpRouteOutcome::DroppedQueueFull => {
                            UDP_DROPS_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
                        }
                        UdpRouteOutcome::DroppedNoWorker => {
                            UDP_DROPS_NO_WORKER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // Linux surfaces a stale ICMP error (the recipient of an
                // earlier datagram has gone) as ECONNREFUSED on the next
                // recv; it is transient and must not tear down the pool.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    debug!("Transient UDP recv error: {e}");
                }
                Err(e) => {
                    // The listener is gone for good: an abnormal end, reported
                    // by the failed pool task (see `ControlChannelHandle::new`).
                    shutting_down.store(true, Ordering::Relaxed);
                    return Err(e).with_context(|| "UDP service socket failed");
                }
            },
            _ = sweep.tick() => {
                // Evict idle affinity entries so address churn (e.g. scans)
                // cannot grow the table unboundedly. Expiry only re-shards a
                // peer onto another channel; the client-side hub keeps the
                // peer's local outbound socket, so its source port is
                // unaffected.
                let mut routes = routes.lock().unwrap_or_else(PoisonError::into_inner);
                let before = routes.len();
                routes.retain(|_, route| {
                    route.last_seen.elapsed() < Duration::from_secs(UDP_ROUTE_TTL_SECS)
                });
                let evicted = before - routes.len();
                drop(routes);
                if evicted > 0 {
                    shared.evictions.fetch_add(evicted as u64, Ordering::Relaxed);
                }
            }
        }
    }

    // Drop the socket immediately so the port is released before a
    // replacement pool tries to bind. Worker tasks exit on the shutdown
    // signal (or when their channel dies) and release their own clones.
    drop(l);
    debug!("UDP pool dropped");
    Ok(())
}

// --- visitor-datagram drop counters (opt-in line, MOLEHILL_UDP_STATS) -------
//
// The single socket reader never blocks: a full worker queue drops the
// datagram, exactly what UDP peers tolerate, instead of head-of-line blocking
// every other visitor. That design choice had no number attached to it — the
// drop was visible only as a `debug!` line — so "is the queue depth right?"
// could not be answered with evidence. These counters are that number.
//
// They count *datagrams the server refused to enqueue*, not datagrams lost in
// the network or by the peer: `queue_full` is the drop the design accepts,
// `no_worker` is a datagram that arrived while no data channel was ready (the
// registration/reconnect window), which is a different failure and is
// separated for that reason.

/// Drops because the assigned worker's queue was full.
static UDP_DROPS_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
/// Drops because no data channel was ready at all (registration window).
static UDP_DROPS_NO_WORKER: AtomicU64 = AtomicU64::new(0);
/// Guards the one-time spawn of the periodic line.
static UDP_STATS_SPAWNED: std::sync::Once = std::sync::Once::new();

/// The two drop counters, for the periodic line and for tests.
#[cfg(test)]
pub(crate) fn udp_drop_stats() -> (u64, u64) {
    (
        UDP_DROPS_QUEUE_FULL.load(Ordering::Relaxed),
        UDP_DROPS_NO_WORKER.load(Ordering::Relaxed),
    )
}

/// Spawn the periodic UDP line, once per process, when
/// `MOLEHILL_UDP_STATS` is set. Cumulative counters, like the KCP ones: a
/// reader that knows the window (or takes the first and last line of a run)
/// gets a drop *rate*, which is the number a queue-depth decision needs.
///
/// One line per **live pool** per second, with the pool's identity, its
/// affinity table (live entries and cumulative TTL evictions), its worker
/// count, and each worker's pinned peers (D30). The per-worker pinned counts
/// belong to this line rather than to a new one: they *are* the affinity
/// table, cut by the channel each peer is pinned to.
///
/// Deliberately independent of the KCP stats task: the default carrier is
/// TCP, so a run can exercise the UDP path without any KCP session existing.
fn spawn_udp_stats() {
    if std::env::var_os("MOLEHILL_UDP_STATS").is_none() {
        return;
    }
    UDP_STATS_SPAWNED.call_once(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let pools: Vec<Arc<UdpPoolShared>> = {
                    let mut guard = udp_pools().lock().unwrap_or_else(PoisonError::into_inner);
                    guard.retain(|w| w.strong_count() > 0);
                    guard.iter().filter_map(std::sync::Weak::upgrade).collect()
                };
                for pool in pools {
                    let s = pool.snapshot();
                    let pinned: Vec<String> = s
                        .pinned
                        .iter()
                        .map(|(worker, peers)| format!("{worker}:{peers}"))
                        .collect();
                    info!(
                        bind_addr = %s.bind_addr,
                        affinity = s.affinity,
                        evictions = s.evictions,
                        workers = s.workers,
                        pinned = %if pinned.is_empty() { "-".to_owned() } else { pinned.join(",") },
                        queue_full = s.drops_queue_full,
                        no_worker = s.drops_no_worker,
                        "udp-stats: affinity table and per-worker pinned peers"
                    );
                }
            }
        });
    });
}

/// What routing one visitor datagram did.
///
/// Returned rather than only counted, so the decision is testable without
/// touching the process-global counters: a counter that can only be asserted
/// on racily (several tests routing through the same statics in parallel) is a
/// counter nobody can prove works. The caller counts the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UdpRouteOutcome {
    /// Handed to the peer's data channel (sticky or freshly assigned).
    Enqueued,
    /// The assigned channel's queue was full — the loss this design accepts.
    DroppedQueueFull,
    /// No data channel was ready (the registration/reconnect window).
    DroppedNoWorker,
}

/// Send one visitor datagram to the data channel assigned to its source
/// address, assigning (or re-assigning after a worker died) on the fly.
///
/// The single socket reader never blocks: a full worker queue drops the
/// datagram — exactly what UDP peers already tolerate — instead of
/// head-of-line blocking every other visitor.
fn route_udp_datagram(
    workers: &UdpWorkerMap,
    routes: &UdpRouteMap,
    next_worker: &mut usize,
    from: SocketAddr,
    mut data: Bytes,
) -> UdpRouteOutcome {
    let now = Instant::now();
    let mut routes = routes.lock().unwrap_or_else(PoisonError::into_inner);
    let workers = workers.lock().unwrap_or_else(PoisonError::into_inner);

    // Sticky path: this peer already has a live data channel assigned.
    if let Some(route) = routes.get_mut(&from)
        && let Some(tx) = workers.get(&route.worker)
    {
        match tx.try_send((from, data)) {
            Ok(()) => {
                route.last_seen = now;
                return UdpRouteOutcome::Enqueued;
            }
            Err(TrySendError::Full(_)) => {
                debug!("UDP worker queue full, dropping a datagram from {from}");
                return UdpRouteOutcome::DroppedQueueFull;
            }
            Err(TrySendError::Closed((_, back))) => {
                // The assigned worker died; re-assign below.
                data = back;
            }
        }
    }

    // (Re-)assign the peer to a worker, round-robin over the live ones.
    if workers.is_empty() {
        debug!("No UDP data channel is ready, dropping a datagram from {from}");
        return UdpRouteOutcome::DroppedNoWorker;
    }
    let idx = *next_worker % workers.len();
    *next_worker = next_worker.wrapping_add(1);
    let Some((id, tx)) = workers.iter().nth(idx).map(|(id, tx)| (*id, tx.clone())) else {
        return UdpRouteOutcome::DroppedNoWorker; // Unreachable: the map is non-empty.
    };
    debug!("UDP peer {from} assigned to data channel {id}");
    routes.insert(
        from,
        UdpRoute {
            worker: id,
            last_seen: now,
        },
    );
    match tx.try_send((from, data)) {
        Ok(()) => UdpRouteOutcome::Enqueued,
        Err(e) => {
            // The freshly assigned channel refused it: same class as a full
            // queue (a closed one loses the race with the worker's death).
            debug!("Dropped a datagram from {from}: {e}");
            UdpRouteOutcome::DroppedQueueFull
        }
    }
}

/// One data channel serving the peers assigned to it: visitor-bound
/// datagrams arrive through the routed queue, replies are read from the
/// channel and sent from the shared service socket.
async fn udp_forward_worker<C>(
    l: Arc<UdpSocket>,
    mut conn: C,
    mut rx: mpsc::Receiver<(SocketAddr, Bytes)>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    buffer_size: usize,
    _guard: UdpWorkerGuard,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Scratch buffers reused across datagrams so the hot path allocates
    // nothing.
    let mut tx_scratch = BytesMut::with_capacity(MAX_UDP_HEADER_LEN + buffer_size);
    let mut rx_scratch = BytesMut::with_capacity(MAX_UDP_HEADER_LEN + buffer_size);
    loop {
        tokio::select! {
            // Visitor-bound datagrams routed to this channel
            item = rx.recv() => {
                let Some((from, data)) = item else { break };
                if let Err(e) =
                    UdpTraffic::write_frame(&mut conn, &mut tx_scratch, from, &data).await
                {
                    debug!("Failed to forward UDP traffic to the client: {e:#}");
                    break;
                }
            }
            // Replies from the local service, back to the visitor
            hdr_len = conn.read_u8() => {
                let hdr_len = match hdr_len {
                    Ok(len) => len,
                    Err(e) => {
                        debug!("UDP data channel closed: {e:#}");
                        break;
                    }
                };
                // `Ok(None)` means an oversized packet was dropped; the
                // stream stays in sync, so just keep going.
                match UdpTraffic::read_slice(&mut conn, hdr_len, &mut rx_scratch, buffer_size)
                    .await
                {
                    Ok(Some((from, len))) => {
                        if let Err(e) = l.send_to(&rx_scratch[..len], from).await {
                            // Transient send failures must not kill the
                            // worker (and churn a replacement channel).
                            debug!("Failed to send a UDP datagram to {from}: {e:#}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!("UDP data channel closed: {e:#}");
                        break;
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                break;
            }
        }
    }
}

/// Returns `true` if the error is a transient resource exhaustion error (EMFILE, ENFILE, ENOMEM, ENOBUFS)
/// that warrants sleeping before retrying the accept loop.
fn should_retry_accept(err: &anyhow::Error) -> bool {
    let Some(io_err) = err.downcast_ref::<io::Error>() else {
        return false;
    };
    if cfg!(unix) {
        matches!(
            io_err.raw_os_error(),
            Some(24 | 23 | 12 | 105) // EMFILE, ENFILE, ENOMEM, ENOBUFS
        )
    } else {
        // On non-Unix, treat all IO errors as potentially transient
        io_err.kind() == io::ErrorKind::OutOfMemory || io_err.kind() == io::ErrorKind::StorageFull
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Serving L3 is the *server operator's* decision: without the table a
    /// transparent registration is refused by policy — with a reason that names
    /// the missing switch, not a missing device — and with it the device named
    /// there is what the data path gets. `[server.transparent]` is an `Option`
    /// for exactly this reason: a defaulted table would leave this process
    /// armed to attach a device the operator never asked for.
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    #[test]
    fn the_server_switch_governs_transparent_registrations() {
        let mut server = ServerConfig::default();
        let refusal = transparent_tun_for(&server).unwrap_err();
        assert!(
            refusal.contains("[server.transparent]"),
            "the refusal must name the missing switch, got: {refusal}"
        );

        server.transparent = Some(crate::config::parsing::TransparentConfig {
            tun: "l3test0".to_string(),
        });
        assert_eq!(
            transparent_tun_for(&server).unwrap(),
            "l3test0",
            "the device comes from the table the operator wrote"
        );
    }

    /// The operator's valve, at the level the tunnel path reads it: a cap of
    /// `N` admits N live tunnels and refuses the next, and a refused tunnel
    /// leaves the count alone. `0` is unlimited.
    #[cfg(feature = "multiplex")]
    #[test]
    fn a_tunnel_slot_is_reserved_under_the_cap_and_released_on_drop() {
        let count = TunnelCount::default();
        assert_eq!(count.0.load(Ordering::Acquire), 0);

        let first = count.try_reserve(2).unwrap();
        let second = count.try_reserve(2).unwrap();
        assert_eq!(
            count.try_reserve(2).err(),
            Some(2),
            "a cap of 2 must refuse the third tunnel, reporting what is held"
        );
        drop(first);
        let third = count.try_reserve(2).unwrap();
        assert_eq!(count.0.load(Ordering::Acquire), 2);
        drop((second, third));
        assert_eq!(count.0.load(Ordering::Acquire), 0);

        // 0 means "no valve": every reservation is admitted.
        let unlimited = TunnelCount::default();
        let slots: Vec<TunnelGuard> = (0..8).map(|_| unlimited.try_reserve(0).unwrap()).collect();
        assert_eq!(unlimited.0.load(Ordering::Acquire), 8);
        drop(slots);
        assert_eq!(unlimited.0.load(Ordering::Acquire), 0);
    }

    type UdpWorkerRxs = Vec<mpsc::Receiver<(SocketAddr, Bytes)>>;

    /// A routing table with `n` live workers; returns the maps and the
    /// receiving ends of every worker queue.
    fn setup(n: usize) -> (Arc<UdpWorkerMap>, Arc<UdpRouteMap>, UdpWorkerRxs, usize) {
        let workers = Arc::new(Mutex::new(HashMap::new()));
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let mut rxs = Vec::new();
        for id in 0..n {
            let (tx, rx) = mpsc::channel(DEFAULT_UDP_SENDQ_SIZE);
            workers.lock().unwrap().insert(id, tx);
            rxs.push(rx);
        }
        (workers, routes, rxs, 0)
    }

    fn route(
        workers: &Arc<UdpWorkerMap>,
        routes: &Arc<UdpRouteMap>,
        next_worker: &mut usize,
        from: SocketAddr,
    ) {
        route_udp_datagram(workers, routes, next_worker, from, Bytes::from_static(b"x"));
    }

    #[test]
    fn same_peer_always_routed_to_one_worker() {
        let (workers, routes, mut rxs, mut next) = setup(2);

        for _ in 0..16 {
            route(&workers, &routes, &mut next, peer(1000));
        }

        // All datagrams landed on exactly one worker...
        let total: usize = rxs
            .iter_mut()
            .map(|rx| {
                let mut n = 0;
                while rx.try_recv().is_ok() {
                    n += 1;
                }
                n
            })
            .sum();
        assert_eq!(total, 16);
        // ...and the affinity entry points at a single, stable channel.
        let table = routes.lock().unwrap();
        assert_eq!(table.len(), 1);
        assert!(table.contains_key(&peer(1000)));
    }

    /// Build a session holding one service per id, and the queue receivers
    /// those services would be served from.
    #[cfg(feature = "multiplex")]
    fn session_with_services(
        nonce: Nonce,
        ids: &[u32],
    ) -> (
        RwLock<SessionMap>,
        Vec<mpsc::Receiver<DataChannel>>,
        Vec<mpsc::Sender<DataChannel>>,
    ) {
        let mut services = HashMap::new();
        let mut receivers = Vec::new();
        let mut senders = Vec::new();
        for id in ids {
            let (data_ch_tx, data_ch_rx) = mpsc::channel(4);
            let (request_tx, _request_rx) = mpsc::unbounded_channel();
            let (shutdown_tx, _) = broadcast::channel(1);
            services.insert(
                ServiceId::new(*id),
                ControlChannelHandle {
                    shutdown: shutdown_tx,
                    data_channel: data_ch_tx.clone(),
                    data_ch_req: request_tx,
                },
            );
            receivers.push(data_ch_rx);
            senders.push(data_ch_tx);
        }
        let (write_tx, _write_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut map = SessionMap::new();
        map.insert(nonce, {
            let mut session = SessionHandle::new(write_tx, shutdown_tx);
            session.services = services;
            session
        });
        (RwLock::new(map), receivers, senders)
    }

    /// Write a stream's 4-byte service prologue.
    #[cfg(feature = "multiplex")]
    async fn write_prologue(tx: &mut tokio::io::DuplexStream, id: u32) {
        tx.write_all(&id.to_be_bytes()).await.unwrap();
        tx.flush().await.unwrap();
    }

    /// A loopback TCP stream wrapped the way a data channel can carry one, so
    /// a routed queue can be shown to deliver.
    #[cfg(feature = "multiplex")]
    async fn probe_data_channel() -> DataChannel {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (client, _) = tokio::join!(TcpStream::connect(addr), l.accept());
        DataChannel::Raw(ServerStream::Plain(client.unwrap()))
    }

    /// A v4 tunnel's streams are routed by their 4-byte prologue: to the queue
    /// of the service they name, or nowhere at all. A violation drops that one
    /// stream — the session and its services stay untouched, which is what the
    /// assertions on the session map at the end pin down.
    #[cfg(feature = "multiplex")]
    // The module-level waiver cannot carry `expect_used`: the only test that
    // uses `.expect()` is multiplex-gated, so a feature-minimal build would
    // leave the module's expectation unfulfilled.
    #[expect(
        clippy::expect_used,
        reason = "the test asserts on a routing decision it just made"
    )]
    #[tokio::test]
    async fn tunnel_streams_route_by_their_prologue() {
        let nonce = [7u8; HASH_WIDTH_IN_BYTES];
        let (sessions, mut queues, senders) = session_with_services(nonce, &[1, 2]);
        let queue_a = senders[0].clone();
        let queue_b = senders[1].clone();

        // A registered service's stream lands on *that* service's queue.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        write_prologue(&mut tx, 1).await;
        let routed = route_tunnel_stream(&mut rx, &sessions, &nonce)
            .await
            .expect("a registered service must route");
        assert!(routed.same_channel(&queue_a));
        assert!(!routed.same_channel(&queue_b));

        // An unregistered id is dropped.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        write_prologue(&mut tx, 9).await;
        assert!(
            route_tunnel_stream(&mut rx, &sessions, &nonce)
                .await
                .is_none()
        );

        // A stream that ends before naming a service is dropped too.
        let (tx, mut rx) = tokio::io::duplex(64);
        drop(tx);
        assert!(
            route_tunnel_stream(&mut rx, &sessions, &nonce)
                .await
                .is_none()
        );

        // A *sibling* service is routable on the same session, which is what a
        // shared pool (`[client.data].shared_pool`) needs: one tunnel carries
        // streams of several services, each routed by its own prologue.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        write_prologue(&mut tx, 2).await;
        let routed = route_tunnel_stream(&mut rx, &sessions, &nonce)
            .await
            .expect("the sibling service must route too");
        assert!(routed.same_channel(&queue_b));
        assert!(!routed.same_channel(&queue_a));

        // A stream that routed lands in *that* service's pool queue and in no
        // other, which is what the pool pairing later consumes.
        routed.send(probe_data_channel().await).await.unwrap();
        assert!(queues[1].try_recv().is_ok());
        assert!(
            queues[0].try_recv().is_err(),
            "the stream must not reach another service's queue"
        );

        // Nothing above ended a session or released a service.
        let guard = sessions.read().await;
        let session = guard.get(&nonce).expect("the session is still registered");
        assert_eq!(session.services.len(), 2);
        drop(guard);
        assert!(!queues[0].is_closed(), "service 1 is still served");
        assert!(!queues[1].is_closed(), "service 2 is still served");
    }

    /// Routing reports *what it did*; the caller counts it. Asserting the
    /// outcome keeps this deterministic — the counters are process-global and
    /// several tests route through them in parallel, so the outcome, not the
    /// static, is what a unit test can prove.
    #[test]
    fn routing_reports_each_drop_reason() {
        // A worker with capacity 1: the second datagram for the same peer is
        // refused by the queue.
        let workers = Arc::new(Mutex::new(HashMap::new()));
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel(1);
        workers.lock().unwrap().insert(0, tx);
        let mut next = 0;

        assert_eq!(
            route_udp_datagram(
                &workers,
                &routes,
                &mut next,
                peer(4000),
                Bytes::from_static(b"x")
            ),
            UdpRouteOutcome::Enqueued
        );
        assert_eq!(
            route_udp_datagram(
                &workers,
                &routes,
                &mut next,
                peer(4000),
                Bytes::from_static(b"x")
            ),
            UdpRouteOutcome::DroppedQueueFull,
            "a full queue must say so"
        );
        // The queued datagram is still there: the drop decision lost nothing.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());

        // No live channel at all is a different reason, and says so.
        let empty: Arc<UdpWorkerMap> = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(
            route_udp_datagram(
                &empty,
                &routes,
                &mut next,
                peer(4100),
                Bytes::from_static(b"x")
            ),
            UdpRouteOutcome::DroppedNoWorker
        );
    }

    /// The opt-in line exists so the design's accepted loss has a number; the
    /// counters it reads must therefore be wired to the outcomes above.
    #[test]
    fn drop_counters_track_their_reason() {
        let (full0, none0) = udp_drop_stats();
        UDP_DROPS_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
        assert_eq!(udp_drop_stats().0, full0 + 1);
        assert_eq!(udp_drop_stats().1, none0);
        UDP_DROPS_NO_WORKER.fetch_add(1, Ordering::Relaxed);
        assert_eq!(udp_drop_stats().1, none0 + 1);
    }

    #[test]
    fn distinct_peers_spread_across_workers() {
        let (workers, routes, mut rxs, mut next) = setup(2);

        for port in 2000..2010 {
            route(&workers, &routes, &mut next, peer(port));
        }

        // Round-robin at assignment time: both channels carry traffic.
        assert!(rxs[0].try_recv().is_ok(), "worker 0 got no peers");
        assert!(rxs[1].try_recv().is_ok(), "worker 1 got no peers");
        assert_eq!(routes.lock().unwrap().len(), 10);
    }

    #[test]
    fn dead_worker_is_bypassed() {
        let (workers, routes, mut rxs, mut next) = setup(2);

        route(&workers, &routes, &mut next, peer(3000));
        let first = routes.lock().unwrap()[&peer(3000)].worker;
        rxs[first].try_recv().unwrap();

        // Simulate the worker exiting (what `UdpWorkerGuard` does): its
        // queue disappears from the table.
        workers.lock().unwrap().remove(&first);

        // The peer's next datagram must be re-assigned to a live channel.
        route(&workers, &routes, &mut next, peer(3000));
        let reassigned = routes.lock().unwrap()[&peer(3000)].worker;
        assert_ne!(reassigned, first, "peer stayed on the dead worker");
        rxs[reassigned].try_recv().unwrap();
    }

    #[test]
    fn full_queue_drops_without_blocking() {
        // Capacity 1, filled before the call: the router must drop instead
        // of stalling the single socket reader.
        let workers = Arc::new(Mutex::new(HashMap::new()));
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel(1);
        workers.lock().unwrap().insert(0, tx.clone());
        tx.try_send((peer(4000), Bytes::from_static(b"fill")))
            .unwrap();
        let mut next = 0;

        route(&workers, &routes, &mut next, peer(4000));

        // Only the pre-filled datagram is in the queue.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn no_workers_drops_quietly() {
        let (workers, routes, _rxs, mut next) = setup(0);
        route(&workers, &routes, &mut next, peer(5000));
        assert!(routes.lock().unwrap().is_empty());
    }

    /// A group request is the *only* one that carries a group, and an ordinary
    /// visitor keeps the plain four-byte command. It matters because the
    /// client's tag dispatch is what makes a mistake fatal rather than merely
    /// unhelpful: an unknown tag closes the session, and a plain request that
    /// grew a group would be a frame no peer expects.
    #[test]
    fn only_a_group_request_names_a_group() {
        let service = ServiceId::new(0xdead_beef);
        let stripe = DataChannelRequest::Stripe {
            group: [0x01, 0x02, 0x03, 0x04],
            index: 2,
            count: 4,
        };
        assert!(
            matches!(
                data_channel_cmd(service, &stripe),
                ControlChannelCmd::CreateDataChannelForStripe(id, group, 2, 4)
                    if id == service && group == [0x01, 0x02, 0x03, 0x04]
            ),
            "a stripe request must name its group, index and count"
        );
        assert!(matches!(
            data_channel_cmd(service, &DataChannelRequest::Plain),
            ControlChannelCmd::CreateDataChannelFor(id) if id == service
        ));
    }
}
