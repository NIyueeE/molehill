use crate::common::constants::{UDP_QUEUE_SIZE, UDP_ROUTE_TTL_SECS};
use crate::common::helper::write_and_flush;
use crate::config::ConfigChange;
#[cfg(mh_l3_server)]
use crate::config::parsing::TransparentConfig;
use crate::config::{Config, ServerConfig, ServiceType};
use crate::logging::RepeatNotice;
use crate::protocol::Hello::{ControlChannelHello, DataChannelHello};
// Read only where multiplexed tunnels exist (the mux stream path).
#[cfg(feature = "multiplex")]
use crate::protocol::read_tunnel_prologue;
use crate::protocol::{
    self, Ack, CURRENT_PROTO_VERSION, Carrier, ControlChannelCmd, DataChannelCmd, FrameFormat,
    HASH_WIDTH_IN_BYTES, Hello, MAX_UDP_HEADER_LEN, NOISE_SELECTOR, PLAIN_SELECTOR, ServiceId,
    ServiceRegistration, SessionCmd, SessionRegistration, UdpTraffic, read_auth, read_hello,
    read_session_cmd, read_stream_prologue, write_register_result,
};
#[cfg(feature = "noise")]
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

#[cfg(feature = "mhd")]
use crate::transport::mhd::MhdListener;

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
/// the accept loop, which stalls the whole service (see `serve_tcp_visitor`).
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
    /// The client's ceiling on this UDP service's worker set (0 for non-UDP).
    max_udp_workers: usize,
    /// The TUN device a transparent service's data path attaches to
    /// (`[server.transparent].tun`); `None` for every other service type.
    #[cfg_attr(
        not(mh_l3_server),
        expect(dead_code, reason = "read only by the transparent data path")
    )]
    transparent_tun: Option<String>,
    /// The frame shape this service's transparent channels carry. `Ip` for
    /// every forwarding service, and for a transparent one that did not
    /// negotiate the Linux vnet fast path.
    #[cfg(mh_l3_server)]
    wire_format: FrameFormat,
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
            write_tx,
            shutdown,
        }
    }
}

/// One session's live tunnel count, and the reservation a tunnel holds.
///
/// The count is taken with a compare-exchange rather than a load-then-add so
/// two tunnels of the same client arriving together cannot both slip past
/// `max_tunnels`; the guard releases the slot when the tunnel ends,
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

/// The session counter a transparent service charges its lanes to, when this
/// build has one.
///
/// `[server.data].max_tunnels` was checked on the tunnel hello, so it
/// counted multiplexed tunnels and nothing else — and a transparent claim's lane
/// is a plain data channel of its own, which made a claim's carrier connections
/// the one kind of establishment no server-side policy bounded. The counter is
/// the same one the tunnel valve uses. A build without the transparent data
/// path has no lane to charge, so the valve is zero-sized and does not pretend
/// otherwise.
#[cfg(feature = "multiplex")]
type LaneCounter = Option<TunnelCount>;
#[cfg(not(feature = "multiplex"))]
type LaneCounter = ();

/// What a transparent service charges its lanes to: the session's counter and
/// the operator's cap. Without `multiplex` there is no tunnel counter, so the
/// valve is zero-sized and only the lane's own lifetime governs it.
#[cfg(all(mh_l3_server, feature = "multiplex"))]
#[derive(Clone, Default)]
struct LaneValve {
    counter: LaneCounter,
    /// `[server.data].max_tunnels`; `0` is unlimited.
    cap: usize,
}

#[cfg(all(mh_l3_server, not(feature = "multiplex")))]
#[derive(Clone, Default)]
struct LaneValve;

/// A build without the transparent data path has no lane to charge. The type
/// stays, so the plumbing that carries it reads the same everywhere, and it is
/// zero-sized.
#[cfg(not(mh_l3_server))]
#[derive(Clone, Default)]
struct LaneValve;

impl LaneValve {
    /// The valve one session's services charge their lanes to.
    #[cfg(all(mh_l3_server, feature = "multiplex"))]
    fn new(counter: LaneCounter, cap: usize) -> Self {
        Self { counter, cap }
    }

    #[cfg(all(mh_l3_server, not(feature = "multiplex")))]
    fn new(_counter: LaneCounter, _cap: usize) -> Self {
        Self
    }

    #[cfg(not(mh_l3_server))]
    fn new(_counter: LaneCounter, _cap: usize) -> Self {
        Self
    }

    /// Reserve one of a client's carrier connections for a lane.
    ///
    /// `Err(held)` is the number of connections the client already holds, for
    /// the refusal line — the same contract as the tunnel valve's.
    #[cfg(all(mh_l3_server, feature = "multiplex"))]
    fn reserve(&self) -> std::result::Result<Option<TunnelGuard>, usize> {
        match &self.counter {
            Some(count) => count.try_reserve(self.cap).map(Some),
            None => Ok(None),
        }
    }
}

/// What a transparent service carries in from its registration: the address
/// claim it holds for its lifetime, and the valve its lanes are charged to.
///
/// One value rather than two parameters because the two travel together and
/// only the transparent data path reads them — without that path there is no
/// lane to charge, and the type is just the claim.
struct TransparentContext {
    /// Held for the service's lifetime: the address stays claimed until this
    /// task ends. Read only by the transparent data path.
    #[cfg(mh_l3_server)]
    claim: Option<Claim>,
    /// The session's lane valve.
    #[cfg(mh_l3_server)]
    lane_valve: LaneValve,
    /// Where the claim's per-lane counters are reported to its client.
    ///
    /// The deciding end is the client, and the loss that matters on a
    /// transparent claim happens here (this end reads the visitor's traffic off
    /// its own device and offers it to the lanes), so the client cannot see it
    /// without being told.
    #[cfg(mh_l3_server)]
    sink: ControlSink,
}

impl TransparentContext {
    /// What a registration hands the service it just accepted.
    #[cfg(mh_l3_server)]
    fn new(claim: Option<Claim>, lane_valve: LaneValve, sink: ControlSink) -> Self {
        Self {
            claim,
            lane_valve,
            sink,
        }
    }

    /// A build without the transparent data path has no lane to charge and no
    /// claim to hold, so there is nothing to carry.
    #[cfg(not(mh_l3_server))]
    fn new(_claim: Option<Claim>, _lane_valve: LaneValve, _sink: ControlSink) -> Self {
        Self {}
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
    /// Every live tunnel and lane of every session: the global valve's count.
    #[cfg(feature = "multiplex")]
    tunnels: TunnelCount,
}

impl Registry {
    fn new() -> Registry {
        Registry {
            sessions: Arc::new(RwLock::new(SessionMap::new())),
            #[cfg(feature = "multiplex")]
            tunnels: TunnelCount::default(),
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
        other => bail!("Unknown transport selector {other:#04x}"),
    }
}

/// Everything the accept paths need, shared across the server's listener
/// tasks: the TCP transport (bind/accept/hints), the optional Noise keys,
/// the lazily-bound mhd listener state and the shutdown broadcast for
/// tasks spawned after startup.
struct ServerShared {
    tcp: Arc<TcpTransport>,
    /// The resolved global carrier-connection cap (`0` = unlimited).
    max_tunnels: usize,
    /// The server-wide stripe target: starts at 1 and grows on path
    /// backpressure, never shrinks.
    stripes: Arc<crate::stripe::StripePlan>,
    #[cfg(feature = "noise")]
    noise_keys: Option<NoiseKeys>,
    #[cfg(feature = "mhd")]
    mhd: Arc<MhdListenerState>,
    #[cfg(feature = "mhd")]
    shutdown_tx: broadcast::Sender<bool>,
}

/// Shared state for the lazily-bound mhd listener: bound on the first
/// registration that declares the `mhd` carrier (client-first), once per
/// server process.
///
/// The listener is held **weakly** here, and the listener task holds the only
/// strong reference: the UDP sockets must close when the server stops, and a mhd
/// *connection* outlives the accept loop (it keeps this shared state to reach
/// the registry and the valve). A strong reference here would therefore keep the
/// data port bound for as long as any lane drained — which, in-process, is
/// how a restarted server ends up refused with "address already in use".
#[cfg(feature = "mhd")]
struct MhdListenerState {
    listener: tokio::sync::Mutex<Option<std::sync::Weak<tokio::sync::Mutex<MhdListener>>>>,
}

#[cfg(feature = "mhd")]
impl Default for MhdListenerState {
    fn default() -> Self {
        Self {
            listener: tokio::sync::Mutex::new(None),
        }
    }
}

/// Ensure the mhd UDP listener is bound — once, on the first registration
/// that declares the `mhd` carrier. A bind failure is a precise
/// registration rejection, not a startup failure: servers that never see a
/// mhd client never open the UDP socket (client-first, no config-side
/// carrier opt-in).
#[cfg(feature = "mhd")]
async fn ensure_mhd_listener(
    shared: Arc<ServerShared>,
    registry: Arc<Registry>,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    let mut guard = shared.mhd.listener.lock().await;
    if guard.as_ref().is_some_and(|weak| weak.strong_count() > 0) {
        return Ok(());
    }
    let addr = tokio::net::lookup_host(server_config.data_bind_addr())
        .await
        .with_context(|| {
            format!(
                "Failed to resolve the mhd data listener address {}",
                server_config.data_bind_addr()
            )
        })?
        .next()
        .ok_or_else(|| {
            anyhow!(
                "The mhd data listener address {} resolved to nothing",
                server_config.data_bind_addr()
            )
        })?;
    let listener = MhdListener::bind(addr)
        .with_context(|| format!("Failed to bind the mhd data listener at {addr}"))?;
    let bound = listener.local_addr();
    info!("Listening for mhd tunnels at {bound}");
    let listener = Arc::new(tokio::sync::Mutex::new(listener));
    *guard = Some(Arc::downgrade(&listener));
    tokio::spawn(run_mhd_listener(
        listener,
        Arc::clone(&registry),
        Arc::clone(&shared),
        shared.shutdown_tx.subscribe(),
        shared.max_tunnels,
    ));
    Ok(())
}

// Server holds all states of running a server
struct Server {
    // `[server]` config
    config: Arc<ServerConfig>,

    // Live registrations: v3 control channels and v4 sessions (see `Registry`)
    registry: Arc<Registry>,
    // TCP transport + Noise keys + lazy mhd listener + shutdown broadcast
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
        #[cfg(feature = "mhd")]
        let mhd = Arc::new(MhdListenerState::default());
        #[cfg(feature = "noise")]
        let noise_keys = Some(NoiseKeys::from_config(&config.noise)?);
        let max_tunnels = config.max_tunnels();
        let shared = Arc::new(ServerShared {
            tcp,
            max_tunnels,
            stripes: Arc::new(crate::stripe::StripePlan::new(
                crate::stripe::initial_stripes(),
            )),
            #[cfg(feature = "noise")]
            noise_keys,
            #[cfg(feature = "mhd")]
            mhd,
            #[cfg(feature = "mhd")]
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
            // The mhd UDP listener is bound lazily: the first registration
            // that declares the `mhd` carrier triggers it (client-first —
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
        // The mhd listener task subscribed to this broadcast when the first
        // `mhd` registration bound it (the TCP accept loops watch the caller's
        // own receiver instead), so without the signal its socket stays bound
        // for the life of the process — and a server restarted in its place
        // cannot bind that port again.
        #[cfg(feature = "mhd")]
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
            // The control channel is never plain: the session nonce it
            // publishes is the credential every data channel authenticates
            // with, so a plaintext control leg would hand that credential to
            // everyone on the path. Data channels may be plain (data_enable
            // is the client's choice); the control leg may not.
            #[cfg(feature = "noise")]
            if matches!(conn, ServerStream::Plain(_)) {
                bail!(
                    "Refused a plaintext control channel: the control channel must speak Noise                      (Noise_KK_25519_ChaChaPoly_BLAKE2s)"
                );
            }
            // The v4 hello's digest is a client-chosen session tag; the
            // session's identity is the nonce *this* end issues below.
            do_session_handshake(conn, registry, server_config, shared).await?;
        }
        DataChannelHello(_, nonce) => {
            // A direct data channel names its service right after the hello;
            // a transparent lane names itself beside it (see
            // `read_channel_prologue`).
            let service_id = read_stream_prologue(&mut conn).await?;
            do_v4_data_channel(tcp_data_channel(conn), registry, nonce, service_id).await?;
        }
        #[cfg(feature = "multiplex")]
        Hello::DataChannelTunnelHello(_, nonce) => {
            // A tunnel carries no service of its own: it belongs to the
            // session, and every stream inside it names its own service.
            do_v4_tunnel(conn, registry, nonce, shared.max_tunnels).await?;
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
#[cfg(mh_l3_server)]
fn transparent_for(server_config: &ServerConfig) -> Result<&TransparentConfig, String> {
    match server_config.transparent.as_ref() {
        Some(transparent) => Ok(transparent),
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
#[expect(
    clippy::too_many_lines,
    reason = "registration is one policy sequence: allow_ports, carrier, transparent \
              prerequisites and the claim, then bind"
)]
async fn register_service(
    reg: &ServiceRegistration,
    server_config: &Arc<ServerConfig>,
    #[cfg_attr(
        not(feature = "mhd"),
        allow(unused_variables, reason = "only read by the mhd carrier-check arm")
    )]
    shared: &Arc<ServerShared>,
    #[cfg_attr(
        not(feature = "mhd"),
        allow(unused_variables, reason = "only read by the mhd carrier-check arm")
    )]
    registry: &Arc<Registry>,
) -> Result<Registration> {
    info!(service = %reg.name, "Registering service at {}", reg.bind_addr);

    if reg.service_type != ServiceType::Transparent && reg.transparent_capabilities.is_some() {
        let reason = format!(
            "Service {} declared transparent capabilities but is not a transparent service",
            reg.name
        );
        warn!(service = %reg.name, "{reason}");
        return Ok(Registration::Rejected(reason));
    }

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
    if reg.carrier == Carrier::Mhd {
        #[cfg(feature = "mhd")]
        let result = ensure_mhd_listener(
            Arc::clone(shared),
            Arc::clone(registry),
            Arc::clone(server_config),
        )
        .await;
        #[cfg(not(feature = "mhd"))]
        let result: Result<()> = Err(anyhow!("This server was built without the `mhd` feature"));
        if let Err(e) = result {
            let reason = format!("{e:#}");
            warn!(service = %reg.name, "Registration failed: {reason}");
            return Ok(Registration::Rejected(reason));
        }
    }

    // A transparent service claims a public address this host never binds, so
    // four things have to hold before it is accepted: this server serves L3 at
    // all (the operator's own `[server.transparent]`, checked first so that a
    // client's registration is never what enables L3), the build and platform
    // can carry it, the device can be opened (Linux creates a missing TUN;
    // macOS/Windows create theirs), and no other service already owns that
    // address.
    #[cfg(mh_l3_server)]
    let (claim, transparent_tun, wire_format) = {
        if reg.service_type == ServiceType::Transparent {
            let transparent = match transparent_for(server_config) {
                Ok(transparent) => transparent,
                Err(reason) => {
                    warn!(service = %reg.name, "{reason}");
                    return Ok(Registration::Rejected(reason));
                }
            };
            let tun = transparent.tun.as_str();
            // The daemon owns the device step on every platform: Linux creates
            // a missing one and uses an existing one as the operator prepared
            // it, while macOS and Windows create theirs. Every route, policy
            // rule and forwarding switch stays the operator's.
            #[cfg(mh_linux)]
            if let Err(e) = crate::transparent::net::prepare_device(tun) {
                let reason = format!("Failed to prepare the TUN device: {e:#}");
                warn!(service = %reg.name, "Registration failed: {reason}");
                return Ok(Registration::Rejected(reason));
            }
            if let Err(e) = crate::transparent::check::require_interface(tun) {
                let reason = format!("{e:#}");
                warn!(service = %reg.name, "Registration failed: {reason}");
                return Ok(Registration::Rejected(reason));
            }
            // The kernel must forward the claimed addresses into the device;
            // off, every claim's packets die in the kernel and the operator
            // sees silence. Checked here (the first transparent registration)
            // rather than at startup: serving L3 is this table's switch, and
            // a server that never serves it never needs the sysctl.
            if let Err(e) = crate::transparent::check::check_server(reg.bind_addr.ip()) {
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
            let hub = match crate::transparent::hub::TunHub::get_or_spawn(
                tun,
                crate::transparent::Direction::Destination,
            ) {
                Ok(hub) => hub,
                Err(e) => {
                    let reason = format!("{e:#}");
                    warn!(service = %reg.name, "Registration failed: {reason}");
                    return Ok(Registration::Rejected(reason));
                }
            };
            let client_vnet = reg
                .transparent_capabilities
                .is_some_and(|capabilities| capabilities.vnet_frames);
            let wire = if client_vnet
                && hub.format() == FrameFormat::Vnet
                && !crate::transparent::force_ip_wire()
            {
                FrameFormat::Vnet
            } else {
                FrameFormat::Ip
            };
            (Some(held), Some(tun.to_string()), wire)
        } else {
            (None, None, FrameFormat::Ip)
        }
    };
    #[cfg(not(mh_l3_server))]
    let (claim, transparent_tun, wire_format): (Option<Claim>, Option<String>, FrameFormat) = {
        if reg.service_type == ServiceType::Transparent {
            let reason = "This server cannot serve `protocol = \"transparent\"`: it carries \
                          whole IP packets through a TUN device, which needs a `transparent` \
                          build on Linux, macOS or Windows. This build does not have it."
                .to_string();
            warn!(service = %reg.name, "{reason}");
            return Ok(Registration::Rejected(reason));
        }
        (None, None, FrameFormat::Ip)
    };
    #[cfg(not(mh_l3_server))]
    let _ = wire_format;

    let service = RegisteredService {
        name: reg.name.clone(),
        service_type: reg.service_type,
        bind_addr: reg.bind_addr,
        udp_buffer_size: reg.udp_buffer_size as usize,
        max_udp_workers: reg.max_udp_workers as usize,
        transparent_tun,
        #[cfg(mh_l3_server)]
        wire_format,
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
    // The declared cadence: the operator's value when written, else the
    // adaptive tier of the current session count. The ack is the one place the
    // client learns it, and the session loop below uses the same number.
    let sessions = registry.sessions.read().await.len();
    let heartbeat_interval = server_config.heartbeat_interval(sessions);
    let explicit = server_config.control.heartbeat_interval.is_some();
    debug!(
        sessions,
        heartbeat_interval_secs = heartbeat_interval,
        explicit,
        "declared heartbeat cadence"
    );
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
            cmd = read_session_cmd(&mut rd) => {
                if let Some(reason) = handle_session_cmd(&ctx, &heartbeat_tx, cmd).await? {
                    break reason;
                }
            }
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

/// Serve one command from a session's client; `Some(reason)` ends the session.
///
/// Split out of the session loop so the loop is the wiring and this is the
/// policy: what each command does, and which failure is the session's end.
async fn handle_session_cmd(
    ctx: &SessionCtx,
    writer: &mpsc::UnboundedSender<Vec<u8>>,
    cmd: Result<SessionCmd>,
) -> Result<Option<anyhow::Error>> {
    match cmd {
        Ok(SessionCmd::Register(reg)) => {
            if let Err(e) = ctx.register(&reg).await {
                return Ok(Some(e));
            }
        }
        Ok(SessionCmd::Deregister(service_id)) => ctx.deregister(service_id).await,
        // A claim's lane is being retired, and its dialer needs this end's half
        // of the agreement: the client cannot see the flows *this* end placed on
        // the lane, and ending it on the client's word alone is how a peer kept
        // writing into a connection nothing read (HANDOFF.md, the retired-lane
        // black hole).
        Ok(SessionCmd::DrainLane(id, lane)) => {
            if !answer_lane_drain(ctx, writer, id, lane, true).await? {
                return Ok(Some(anyhow!("The session writer is gone")));
            }
        }
        Ok(SessionCmd::CancelDrain(id, lane)) => {
            if !answer_lane_drain(ctx, writer, id, lane, false).await? {
                return Ok(Some(anyhow!("The session writer is gone")));
            }
        }
        Err(e) => return Ok(Some(e)),
    }
    Ok(None)
}

/// Answer one lane-drain command, and say whether the session writer took it.
///
/// The answer is this end's half of a two-sided retirement, and it is the only
/// thing the client's dialer can learn about the flows *this* end placed on the
/// lane.
async fn answer_lane_drain(
    ctx: &SessionCtx,
    writer: &mpsc::UnboundedSender<Vec<u8>>,
    service_id: ServiceId,
    lane: [u8; 4],
    drain: bool,
) -> Result<bool> {
    let ack = ctx.lane_drain(service_id, lane, drain).await;
    let sent = writer.send(postcard::to_stdvec(&ack)?).is_ok();
    if sent {
        // A lane's retirement is a lifecycle event, not one connection's
        // business: the operator sees the same fact the client does (once per
        // drain, and a drain is rate-limited by the policy that asks for it).
        info!(
            "Lane {lane:?} of service {service_id}: drain {} answered {ack:?}",
            if drain { "requested" } else { "cancelled" }
        );
    }
    Ok(sent)
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

    /// Start or call off the drain of one of a claim's lanes, and build the
    /// answer the client reads.
    ///
    /// This end's half of a two-sided retirement: it stops placing **new**
    /// flows in the lane, and says whether it still holds any of its own. The
    /// flows a drain has to wait for are the ones *this* end placed — the
    /// dialer cannot see them, which is why the agreement exists at all.
    ///
    /// A lane this end does not hold (already ended, or never its business)
    /// answers `LANE_DRAIN_UNKNOWN` rather than pretending to have drained
    /// something: the client then keeps the lane instead of retiring a
    /// connection on an answer about nothing.
    #[cfg(mh_l3_server)]
    async fn lane_drain(
        &self,
        service_id: ServiceId,
        lane: [u8; 4],
        drain: bool,
    ) -> ControlChannelCmd {
        use crate::protocol::{
            LANE_DRAIN_CANCELLED, LANE_DRAIN_PENDING, LANE_DRAIN_READY, LANE_DRAIN_UNKNOWN, LaneId,
        };
        use crate::transparent::hub::TunHub;
        use crate::transparent::{Direction, Endpoint};

        let ack = |state| ControlChannelCmd::LaneDrainAck(service_id, lane, state);
        let Some(handle) = self
            .registry
            .sessions
            .read()
            .await
            .get(&self.nonce)
            .and_then(|session| session.services.get(&service_id))
            .cloned()
        else {
            return ack(LANE_DRAIN_UNKNOWN);
        };
        // The transparent device is a server-wide setting: the service's data
        // path is a hub keyed by that device and the endpoint it claimed.
        let Ok(transparent) = transparent_for(&self.server_config) else {
            return ack(LANE_DRAIN_UNKNOWN);
        };
        let Ok(hub) = TunHub::get_or_spawn(&transparent.tun, Direction::Destination) else {
            return ack(LANE_DRAIN_UNKNOWN);
        };
        let endpoint = Endpoint::new(handle.endpoint.ip(), handle.endpoint.port());
        let lane = LaneId(lane);
        if !drain {
            return match hub.cancel_drain(&endpoint, lane) {
                Some(()) => ack(LANE_DRAIN_CANCELLED),
                None => ack(LANE_DRAIN_UNKNOWN),
            };
        }
        match hub.begin_drain(&endpoint, lane) {
            Some(true) => ack(LANE_DRAIN_READY),
            Some(false) => ack(LANE_DRAIN_PENDING),
            None => ack(LANE_DRAIN_UNKNOWN),
        }
    }

    /// A build without the transparent data path has no lane to drain; the
    /// answer is the same one an unknown lane gets, so the client's handling is
    /// one shape.
    #[cfg(not(mh_l3_server))]
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the two shapes of this method are one call site: the build that has lanes \
                  reaches the hub, and the one that does not answers from here"
    )]
    async fn lane_drain(
        &self,
        service_id: ServiceId,
        lane: [u8; 4],
        _drain: bool,
    ) -> ControlChannelCmd {
        ControlChannelCmd::LaneDrainAck(service_id, lane, crate::protocol::LANE_DRAIN_UNKNOWN)
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

        // A claim's lanes are charged to the session's valve as they arrive
        // (see `LaneValve`). The counter is cloned here, so the transparent
        // supervisor never has to reach back into the registry on the lane path.
        #[cfg(feature = "multiplex")]
        let lane_counter = Some(self.registry.tunnels.clone());
        #[cfg(not(feature = "multiplex"))]
        let lane_counter = ();
        let lane_valve = LaneValve::new(lane_counter, self.shared.max_tunnels);

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
            // The valve a transparent service charges its lanes to. A v4
            // registration carries no `pool_size`: the tunnel pool is a
            // client-side, per-carrier concern sized by the client's own
            // configuration — one request per visitor, or one per stripe for a
            // striped gather (see `pair_striped_group`).
            lane_valve,
            Arc::clone(&self.shared.stripes),
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
/// (no-mux mode), one yamux stream of the client's tunnel, or — with the `mhd`
/// carrier — an mhd session that *is* the channel.
#[cfg(not(feature = "multiplex"))]
type DataChannel = ServerStream;

#[cfg(feature = "multiplex")]
enum DataChannel {
    Raw(ServerStream),
    /// A direct channel over mhd: no yamux above it, one stream of the
    /// carrier's own connection *is* the channel. With the data plane's shape
    /// derived from the service type, this is also a transparent claim's lane:
    /// an mhd carrier with no multiplexer over it.
    #[cfg(feature = "mhd")]
    RawMhd(crate::transport::multiplex::CarrierStream),
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
            #[cfg(feature = "mhd")]
            DataChannel::RawMhd(s) => std::pin::Pin::new(s).poll_read(cx, buf),
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
            #[cfg(feature = "mhd")]
            DataChannel::RawMhd(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "mhd")]
            DataChannel::RawMhd(s) => std::pin::Pin::new(s).poll_flush(cx),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "mhd")]
            DataChannel::RawMhd(s) => std::pin::Pin::new(s).poll_shutdown(cx),
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
/// The carrier decides how the channel was dialed (TCP, or an mhd session when
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
    conn.hint(SocketOpts::for_service());
    new_data_channel(conn)
}

/// A v4 multiplex tunnel: it belongs to the session, not to a service, and
/// each stream inside it names its service with the 4-byte prologue.
///
/// **This is where `[server.data].max_tunnels` lives** (the
/// operator's valve): a tunnel that would push the client past its cap is
/// refused with a typed answer and a `debug` line naming the cap, and the
/// session keeps running — never killed, so an over-eager client loses one
/// tunnel, not its services (D14).
#[cfg(feature = "multiplex")]
async fn do_v4_tunnel(
    mut conn: ServerStream,
    registry: Arc<Registry>,
    nonce: Nonce,
    max_tunnels: usize,
) -> Result<()> {
    debug!("Handshaking a multiplexed data tunnel");

    // The reservation travels into `upgrade_to_tunnel`, which hands it to the
    // task that lives as long as the tunnel: this function itself returns as
    // soon as the tunnel is up.
    let slot = match reserve_tunnel(&registry, &nonce, max_tunnels).await {
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
                 {held} tunnel(s), and `[server.data].max_tunnels` is \
                 {max_tunnels}",
                hex::encode(nonce)
            );
            return refuse_tunnel(&mut conn).await;
        }
    };

    conn.hint(SocketOpts::for_service());
    upgrade_to_tunnel(
        conn,
        TunnelOwner {
            sessions: Arc::clone(&registry.sessions),
            nonce,
        },
        Some(slot),
        // A TCP tunnel has no userspace ARQ to report: its retransmit state is
        // the kernel's, which is also why the carrier rule is one-sided.
        None,
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
    // The valve is global: every tunnel and lane of every session charges the
    // same counter, while the session lookup is still what proves the nonce.
    match registry.tunnels.try_reserve(cap) {
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
/// at the TCP and mhd call sites names the cap.
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

/// Report a transparent lane the valve refused: once per process at `info` —
/// the operator wrote the cap, so this is it biting — then at `debug`.
#[cfg(all(mh_l3_server, feature = "multiplex"))]
fn report_lane_refusal(held: usize, cap: usize) {
    static LANE_REFUSALS: RepeatNotice = RepeatNotice::new();
    LANE_REFUSALS.report(
        || {
            info!(
                "Refused a transparent lane: this client already holds {held} carrier \
                 connections and `[server.data].max_tunnels` is {cap}. Further \
                 refusals are logged at debug level."
            );
        },
        || debug!("Refused a transparent lane over the cap of {cap} (client holds {held})"),
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
/// Generic over the stream type so TCP tunnels and mhd tunnels (arm 2) share
/// one implementation.
///
/// `session` is what a v4 tunnel holds for its whole life — its reservation in
/// the session's tunnel count (`[server.data].max_tunnels`) and the
/// session's shutdown signal — when it has one. This function returns as soon
/// as the tunnel is up, so both have to travel into the task that lives as
/// long as the tunnel; a v3 tunnel belongs to one control channel and takes
/// neither.
#[cfg(feature = "multiplex")]
async fn upgrade_to_tunnel<S>(
    mut conn: S,
    owner: TunnelOwner,
    session: Option<SessionTunnel>,
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            unused_variables,
            reason = "without multiplex there are no tunnels to report on"
        )
    )]
    health: Option<Arc<crate::transport::health::SessionHealth>>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    write_and_flush(&mut conn, &postcard::to_stdvec(&Ack::Ok)?).await?;
    let config = crate::transport::multiplex::mux_config();
    let (bridge_tx, mut bridge_rx) = mpsc::channel::<crate::transport::MuxStream>(64);
    // What this end hands the tunnel, counted above the wire: the delivery ratio
    // needs the offered bytes, and a policer drops them in flight, so counting
    // lower down would count what arrived instead (see `CountingTunnel`).
    #[cfg(feature = "multiplex")]
    let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
    #[cfg(feature = "multiplex")]
    let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // The reporter outlives this scope and the driver task owns the adapter's
    // reference, so the count is shared by clone rather than moved twice.
    #[cfg(feature = "multiplex")]
    let reported_written = Arc::clone(&written);
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
        let tunnel = crate::transport::multiplex::run_server_tunnel(
            CountingTunnel {
                inner: conn,
                written: Arc::clone(&written),
                read: Arc::clone(&read),
            },
            config,
            bridge_tx,
        );
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
    // The identity a health report is attributed by: the client's own name for
    // this connection, which its stream prologue carries. The mux driver cannot
    // read it (it owns the connection but not the streams) and the dispatch
    // cannot read the health (it owns the streams but not the connection), so the
    // two meet in this slot, and the reporter below waits on it.
    #[cfg(feature = "multiplex")]
    let tunnel_id: Arc<std::sync::OnceLock<(ServiceId, crate::protocol::TunnelId)>> =
        Arc::new(std::sync::OnceLock::new());
    #[cfg(feature = "multiplex")]
    spawn_tunnel_health_reporter(
        health,
        reported_written,
        Arc::clone(&tunnel_id),
        Arc::clone(&owner.sessions),
        owner.nonce,
    );
    tokio::spawn(async move {
        while let Some(mut stream) = bridge_rx.recv().await {
            let Some(queue) = route_tunnel_stream(
                &mut stream,
                &owner.sessions,
                &owner.nonce,
                #[cfg(feature = "multiplex")]
                &tunnel_id,
            )
            .await
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

/// A tunnel connection that counts what passes through it.
///
/// The **written** count is what this end handed the tunnel, and it is the
/// numerator the delivery ratio does not have: a policer drops those bytes in
/// flight, so counting them anywhere below the wire would count what arrived
/// instead of what was offered — which is the whole distinction the carrier rule
/// needs (HANDOFF.md, round 6). The read count rides along for symmetry and for
/// the pool-stats line's sake.
#[cfg(feature = "multiplex")]
struct CountingTunnel<S> {
    inner: S,
    written: Arc<std::sync::atomic::AtomicU64>,
    read: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(feature = "multiplex")]
impl<S: AsyncRead + Unpin> AsyncRead for CountingTunnel<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            let moved = buf.filled().len().saturating_sub(before);
            self.read
                .fetch_add(u64::try_from(moved).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
        polled
    }
}

#[cfg(feature = "multiplex")]
impl<S: AsyncWrite + Unpin> AsyncWrite for CountingTunnel<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &polled {
            self.written
                .fetch_add(u64::try_from(*n).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
        polled
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Report one tunnel's mhd health back to the end that dialed it.
///
/// The deciding end chooses a tunnel's carrier, and on a path that polices UDP
/// the evidence it needs is here: the packets this server sends into a policed
/// carrier never arrive, so this session's ARQ re-sends them, and the client —
/// which only ever sees what arrived — reads nothing of it. Measured on the
/// `udp_throttled` condition: this end re-sent **22.9 %** of what it emitted
/// while the client's own counters read 0.04 %. One frame per change, on the
/// session's own control writer, so nothing new is opened for it.
///
/// The report starts once the tunnel's identity is known — its first stream's
/// prologue carries it — and the task ends with the session it reports on: the
/// `Weak` is all it holds of the health, so a dead tunnel takes its reporter
/// with it rather than leaving one polling a number nobody owns.
#[cfg(feature = "multiplex")]
fn spawn_tunnel_health_reporter(
    health: Option<Arc<crate::transport::health::SessionHealth>>,
    handed: Arc<std::sync::atomic::AtomicU64>,
    tunnel_id: Arc<std::sync::OnceLock<(ServiceId, crate::protocol::TunnelId)>>,
    sessions: Arc<RwLock<SessionMap>>,
    nonce: Nonce,
) {
    const SAMPLE: std::time::Duration = std::time::Duration::from_millis(50);
    let Some(health) = health.map(|h| Arc::downgrade(&h)) else {
        // A TCP tunnel: the kernel holds its retransmit state and there is
        // nothing here to report.
        return;
    };
    tokio::spawn(async move {
        let mut sent: Option<(u64, u64, u64)> = None;
        let mut seq: u8 = 0;
        let mut announced = false;
        loop {
            tokio::time::sleep(SAMPLE).await;
            let Some(health) = health.upgrade() else {
                // The session is gone: so is the tunnel this reported on.
                return;
            };
            let counters = health.counts();
            let handed = handed.load(Ordering::Relaxed);
            if sent == Some((counters.0, counters.1, handed)) {
                continue;
            }
            sent = Some((counters.0, counters.1, handed));
            let Some((service_id, tunnel)) = tunnel_id.get().copied() else {
                // No stream has named this tunnel yet: there is no identity to
                // attribute a report to, and no traffic to have lost either.
                continue;
            };
            seq = seq.wrapping_add(1);
            let cmd = ControlChannelCmd::TunnelHealth(
                service_id,
                tunnel.0,
                u32::try_from(counters.0).unwrap_or(u32::MAX).to_be_bytes(),
                u32::try_from(counters.1).unwrap_or(u32::MAX).to_be_bytes(),
                u32::try_from(handed).unwrap_or(u32::MAX).to_be_bytes(),
                seq,
            );
            let Ok(frame) = postcard::to_stdvec(&cmd) else {
                return;
            };
            let write_tx = sessions
                .read()
                .await
                .get(&nonce)
                .map(|session| session.write_tx.clone());
            let Some(write_tx) = write_tx else {
                return;
            };
            if !announced {
                // Once per tunnel: the report is a fact about this connection,
                // and its first frame is what proves the path end to end.
                announced = true;
                debug!(
                    service = %hex::encode(service_id.0),
                    tunnel = tunnel.0.iter().fold(String::new(), |mut acc, b| {
                        use std::fmt::Write as _;
                        let _ = write!(acc, "{b:02x}");
                        acc
                    }),
                    emitted = counters.0,
                    resent = counters.1,
                    "Reporting a tunnel's session health to its dialing end"
                );
            }
            if write_tx.send(frame).is_err() {
                return;
            }
        }
    });
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
    #[cfg_attr(
        not(feature = "multiplex"),
        expect(
            unused_variables,
            reason = "without multiplex there are no tunnels to report on"
        )
    )]
    tunnel_id: &std::sync::OnceLock<(ServiceId, crate::protocol::TunnelId)>,
) -> Option<mpsc::Sender<DataChannel>>
where
    S: AsyncRead + Unpin,
{
    let Ok((service_id, tunnel)) = read_tunnel_prologue(stream).await else {
        // The stream ended before naming a service (a client that opened it
        // and hung up): nothing to route it to.
        debug!("Dropped a tunnel stream with no prologue");
        return None;
    };
    // The tunnel a stream arrived on is the connection the *client's* pool
    // holds, and it is the key a per-tunnel report is attributed by — the
    // identity that lets this end say what it offered that connection rather
    // than the pool it belongs to.
    // The tunnel's identity, recorded once and never overwritten: a tunnel's
    // first stream names it, and every later stream of the same connection names
    // the same pair.
    let _ = tunnel_id.set((service_id, tunnel));
    tracing::trace!("Stream for service {service_id} arrived on tunnel {tunnel}");
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

/// Accept mhd lanes and serve every stream they carry.
///
/// One **lane** is one carrier connection: the client's pool holds several, and
/// each carries as many streams as visitors need. A stream is opened by the
/// client and prepared exactly like a TCP data channel — transport selector,
/// optional Noise handshake, hello — so the crypto and the service routing keep
/// their per-channel meaning; everything below that is the carrier's own
/// multiplexing, with no yamux between the visitor and the wire.
///
/// A **tunnel** stream (a pool lane) reserves the lane's slot on its first
/// hello; a **data channel** stream is a direct channel and takes no slot,
/// exactly as the old single-session mhd path did.
#[cfg(feature = "mhd")]
async fn run_mhd_listener(
    listener: Arc<tokio::sync::Mutex<MhdListener>>,
    registry: Arc<Registry>,
    shared: Arc<ServerShared>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    max_tunnels: usize,
) {
    loop {
        let accepted = {
            let mut guard = listener.lock().await;
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                lane = guard.accept() => lane,
            }
        };
        let Some(lane) = accepted else { break };
        let id = lane.id();
        let registry = Arc::clone(&registry);
        let shared = Arc::clone(&shared);
        tokio::spawn(
            async move {
                serve_mhd_lane(lane, registry, shared, max_tunnels).await;
            }
            .instrument(info_span!("mhd_lane", id)),
        );
    }
    // Close every admitted connection, so no lane's sender keeps a listening
    // socket bound after this server stops (and a restarted server cannot share
    // the port with a connection table nobody reads).
    listener.lock().await.close_all();
    debug!("mhd listener stopped");
}

/// What one lane's streams share: the session its first tunnel hello named.
#[cfg(feature = "mhd")]
#[derive(Default)]
struct MhdLaneState {
    registered: tokio::sync::Mutex<Option<MhdLaneRegistration>>,
}

/// The session one lane belongs to, and the slot it holds in the valve.
///
/// The guard lives here for the lane's whole life: a lane that the operator's
/// `[server.data].max_tunnels` already counted must not release its slot until
/// the connection ends.
#[cfg(feature = "mhd")]
struct MhdLaneRegistration {
    nonce: Nonce,
    _session: SessionTunnel,
}

/// Serve one accepted lane: accept its streams until the connection ends.
#[cfg(feature = "mhd")]
async fn serve_mhd_lane(
    lane: Arc<crate::transport::mhd::MhdLane>,
    registry: Arc<Registry>,
    shared: Arc<ServerShared>,
    max_tunnels: usize,
) {
    let state = Arc::new(MhdLaneState::default());
    while let Some(stream) = lane.wait_stream().await {
        let registry = Arc::clone(&registry);
        let shared = Arc::clone(&shared);
        let state = Arc::clone(&state);
        let lane = Arc::clone(&lane);
        tokio::spawn(async move {
            if let Err(e) =
                serve_mhd_stream(stream, lane, state, registry, shared, max_tunnels).await
            {
                // A stream that ends early is one visitor's channel, or a
                // probing peer: DEBUG, never the lane's death.
                debug!("mhd stream ended: {e:#}");
            }
        });
    }
    debug!("mhd lane ended");
}

/// Prepare and route one stream the peer opened.
///
/// The opening is the same three acts a TCP data channel performs; the only
/// difference is that the bytes arrive on a fresh stream of an existing
/// connection rather than on a fresh socket.
#[cfg(feature = "mhd")]
async fn serve_mhd_stream(
    mut stream: crate::transport::mhd::lane::StreamIo,
    lane: Arc<crate::transport::mhd::MhdLane>,
    state: Arc<MhdLaneState>,
    registry: Arc<Registry>,
    shared: Arc<ServerShared>,
    max_tunnels: usize,
) -> Result<()> {
    use crate::transport::multiplex::PoolLane;

    // Bound the crypto + hello phase: an unresponsive or bogus peer must not
    // park this stream's task forever (the same role HANDSHAKE_TIMEOUT plays on
    // the TCP accept path).
    let deadline = Duration::from_secs(HANDSHAKE_TIMEOUT * 2);
    let selector = tokio::time::timeout(deadline, stream.read_u8())
        .await
        .with_context(|| "mhd transport selector timed out")??;
    let stream: crate::transport::multiplex::CarrierStream = Box::new(stream);
    let mut io: crate::transport::multiplex::CarrierStream = match selector {
        PLAIN_SELECTOR => stream,
        NOISE_SELECTOR => {
            #[cfg(feature = "noise")]
            {
                let keys = shared.noise_keys.as_ref().ok_or_else(|| {
                    anyhow!("Client requested Noise, but the server has no Noise keys")
                })?;
                let wrapped = tokio::time::timeout(deadline, keys.wrap_responder(stream))
                    .await
                    .with_context(|| "mhd stream noise handshake timed out")??;
                Box::new(wrapped)
            }
            #[cfg(not(feature = "noise"))]
            {
                let _ = (stream, shared);
                bail!(
                    "Client requested Noise, but this binary was built without the `noise` feature"
                )
            }
        }
        other => bail!("Unknown transport selector {other:#04x}"),
    };

    let (_version, hello) = tokio::time::timeout(deadline, read_hello(&mut io))
        .await
        .with_context(|| "mhd hello timed out")??;
    match hello {
        Hello::DataChannelHello(_, nonce) => {
            let service_id = read_stream_prologue(&mut io).await?;
            do_v4_data_channel(DataChannel::RawMhd(io), registry, nonce, service_id).await
        }
        Hello::DataChannelTunnelHello(_, nonce) => {
            // Reserve the lane's slot once. Every later stream of the same lane
            // names the same session; a different one is a client mistake or a
            // hijack attempt, and either way it is refused.
            let mut registered = state.registered.lock().await;
            if let Some(existing) = registered.as_ref() {
                if existing.nonce != nonce {
                    bail!("mhd lane carried streams for two different sessions");
                }
            } else {
                match reserve_tunnel(&registry, &nonce, max_tunnels).await {
                    TunnelSlot::Held(session) => {
                        *registered = Some(MhdLaneRegistration {
                            nonce,
                            _session: session,
                        });
                    }
                    TunnelSlot::NoSession => {
                        bail!("mhd tunnel hello carried an incorrect nonce");
                    }
                    TunnelSlot::OverCap { held } => {
                        drop(registered);
                        debug!(
                            "Refused an mhd lane for session {}: it already holds {held} \
                             tunnel(s), and `[server.data].max_tunnels` is {max_tunnels}",
                            hex::encode(nonce)
                        );
                        refuse_tunnel(&mut io).await?;
                        PoolLane::mark_dead(&*lane);
                        return Ok(());
                    }
                }
            }
            drop(registered);
            // The client's open waits for this before it names its service.
            write_and_flush(&mut io, &postcard::to_stdvec(&Ack::Ok)?).await?;
            let (service_id, _tunnel) = read_tunnel_prologue(&mut io).await?;
            debug!(%service_id, "mhd tunnel stream routed");
            route_mhd_stream(io, &registry, &nonce, service_id).await
        }
        other @ Hello::ControlChannelHello(..) => {
            bail!("Expected a data-plane hello on the mhd stream, got {other:?}")
        }
    }
}

/// Hand a prepared tunnel stream to the service its prologue named.
///
/// A stream naming a service this session does not know is dropped here, never
/// the lane or the session: the visitor loses one request, not every other
/// visitor on the same lane.
#[cfg(feature = "mhd")]
async fn route_mhd_stream(
    io: crate::transport::multiplex::CarrierStream,
    registry: &Registry,
    nonce: &Nonce,
    service_id: ServiceId,
) -> Result<()> {
    let queue = registry
        .sessions
        .read()
        .await
        .get(nonce)
        .and_then(|session| session.services.get(&service_id))
        .map(|handle| handle.data_channel.clone());
    let Some(queue) = queue else {
        report_tunnel_violation(service_id, "no such service on this session");
        return Ok(());
    };
    queue
        .send(DataChannel::RawMhd(io))
        .await
        .map_err(|_| anyhow!("Data channel for a stale control session"))
}

/// A live control channel, kept alive by holding the three channel
/// senders: dropping the handle shuts the control channel down (each is
/// a `Sender`, so the type carries the direction).
pub struct ControlChannelHandle {
    /// The public endpoint this service serves.
    ///
    /// Carried here because a session-level command that concerns one service
    /// (a lane drain) has to reach that service's *data path*, and the hub is
    /// keyed by the claimed endpoint — the registration itself is not retained
    /// anywhere the session can read.
    endpoint: SocketAddr,
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
    /// the public port until the tunnel died (observed: an mhd tunnel outlives
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
            endpoint: self.endpoint,
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
    not(mh_l3_server),
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
    /// The server gave up on a stripe group before forwarding it: the stripes
    /// the client parked for it must be dropped, and a stripe that is still in
    /// flight must not park at all. Without this the client would hold those
    /// channels until its registry's TTL — pool capacity nobody can use.
    ReleaseGroup([u8; 4]),
}

/// Where a service's control commands go.
///
/// A v3 service owns its control connection; a v4 service shares the
/// session's, so its commands are framed into the session's single writer
/// instead. `Clone` because a service's data path may need to report something
/// of its own (a transparent claim's per-lane counters) without going through
/// the request queue that exists for channels.
#[derive(Clone)]
enum ControlSink {
    Session {
        service_id: ServiceId,
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
    },
}

impl ControlSink {
    /// Send one already-framed command, queued for the session's writer — the
    /// only task that touches that socket.
    fn send(&self, data: &[u8]) -> Result<()> {
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
        DataChannelRequest::ReleaseGroup(group) => {
            ControlChannelCmd::ReleaseStripedGroup(service_id, *group)
        }
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
        lane_valve: LaneValve,
        plan: Arc<crate::stripe::StripePlan>,
    ) -> ControlChannelHandle {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

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
        let sock_opts = SocketOpts::for_service();

        // Create the control channel and run it *before* the pool: the pool
        // takes the control task's join handle, because a control channel
        // that ends on its own (client shutdown, a connection reset, a failed
        // heartbeat write) must stop the pool too. The pool owns the bound
        // public listener, so without that the port stays occupied with
        // nothing serving it and the next registration of that service is
        // rejected with "Port N is already in use".
        // A transparent claim's data path reports its per-lane counters through
        // the same sink the control channel uses: the session's writer is the
        // only task that touches the control socket, so nothing else may write
        // to it.
        let report_sink = sink.clone();
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
                            plan,
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
                let max_workers = service.max_udp_workers;
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
                            max_workers,
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
                TransparentContext::new(claim, lane_valve, report_sink),
                pool_died_tx,
            ),
        }

        ControlChannelHandle {
            endpoint: service.bind_addr,
            shutdown: shutdown_tx,
            data_channel: data_ch_tx,
            data_ch_req: data_ch_req_tx,
        }
    }
}

/// How long to wait before answering a replacement transparent channel with the
/// start command.
///
/// A replacement is requested when a member of the claim ends; without a pause,
/// a peer that refuses the start command instantly would be asked again just as
/// instantly.
#[cfg(mh_l3_server)]
const TRANSPARENT_REPLACE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a transparent claim reports its per-lane counters to its client.
///
/// One second, the same cadence as the `l3-stats` line and the allocator's own
/// tick: a policy compares deltas between consecutive samples, so a slower
/// report would make every window a guess, and a faster one would spend the
/// session's single writer on telemetry. Only lanes whose counters *moved* are
/// sent, so a quiet claim costs nothing.
#[cfg(mh_l3_server)]
const LANE_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Aborts a claim's lane reporter when the claim's task ends.
///
/// A guard rather than an explicit abort at each exit: `run_transparent_service`
/// returns from several arms (shutdown, a dead control task, the channel queue
/// closing), and a reporter that outlived its claim would keep a session writer
/// alive and report lanes nobody holds.
#[cfg(mh_l3_server)]
struct LaneReporter(tokio::task::JoinHandle<()>);

#[cfg(mh_l3_server)]
impl Drop for LaneReporter {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Report one claim's per-lane counters to its client, until dropped.
///
/// This is the half of the evidence the client cannot produce: on a transparent
/// claim the loss that matters happens *here*, where the visitor's packets are
/// read off the device and offered to the lanes, and the client only ever sees
/// what arrived. A lane whose queue refuses a large share of what it is offered
/// is a carrier that is not delivering, and without this report the end that
/// decides which carrier to use has no way to know.
///
/// The same task carries the other thing the client cannot know: when a lane it
/// asked to drain has become empty **on this end**. The dialer sees only its own
/// flows, so a lane may only be retired once this answer arrives — the retired
/// lane black hole is exactly a peer that kept writing into a connection the
/// other end had let go.
///
/// Only lanes whose counters moved since the last sample are sent: a claim that
/// is quiet (or whose lanes are carrying everything) costs no control traffic,
/// and the client's own staleness rule is what bounds a lane that stops
/// reporting.
#[cfg(mh_l3_server)]
fn spawn_lane_reporter(
    hub: std::sync::Arc<crate::transparent::hub::TunHub>,
    endpoint: crate::transparent::Endpoint,
    sink: ControlSink,
) -> LaneReporter {
    use crate::protocol::{ControlChannelCmd, LANE_DRAIN_READY};
    let service_id = sink.service_id();
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(LANE_REPORT_INTERVAL);
        // Per lane: the sequence the client uses to tell a fresh report from a
        // late one, and the last counters sent so a quiet lane is skipped.
        let mut seq: std::collections::HashMap<crate::protocol::LaneId, u8> =
            std::collections::HashMap::new();
        let mut sent: std::collections::HashMap<crate::protocol::LaneId, (u64, u64)> =
            std::collections::HashMap::new();
        // Lanes already answered "ready": the answer is re-sent only when the
        // lane is drained again, not every second for a lane that stays empty.
        let mut ready_sent: std::collections::HashSet<crate::protocol::LaneId> =
            std::collections::HashSet::new();
        loop {
            tick.tick().await;
            for member in hub.member_stats() {
                if member.endpoint != endpoint {
                    continue;
                }
                let Some(lane) = member.lane else {
                    continue;
                };
                // The peer's half of a retirement: this end holds no flow in a
                // lane the *peer* asked to drain. The local `draining` mark is
                // deliberately not the trigger — that one is this end retiring
                // a lane of its own, and answering "ready" for it would tell the
                // peer about a decision it never asked about.
                if member.peer_draining && member.active_flows == 0 {
                    if ready_sent.insert(lane.id) {
                        let ack = ControlChannelCmd::LaneDrainAck(
                            service_id,
                            lane.id.0,
                            LANE_DRAIN_READY,
                        );
                        let Ok(frame) = postcard::to_stdvec(&ack) else {
                            return;
                        };
                        if sink.send(&frame).is_err() {
                            return;
                        }
                    }
                } else if !member.peer_draining {
                    ready_sent.remove(&lane.id);
                }
                let counters = (
                    member.counters.offered_packets,
                    member.counters.overflow_packets,
                );
                if sent.get(&lane.id) == Some(&counters) {
                    continue;
                }
                sent.insert(lane.id, counters);
                let n = seq.entry(lane.id).or_insert(0);
                *n = n.wrapping_add(1);
                let cmd = ControlChannelCmd::CarrierReport(
                    service_id,
                    lane.id.0,
                    // The counters are cumulative for the lane's life and
                    // saturate: a lane that wrapped a `u32` would have carried
                    // 4 billion packets, and the client's window is seconds.
                    u32::try_from(counters.0).unwrap_or(u32::MAX).to_be_bytes(),
                    u32::try_from(counters.1).unwrap_or(u32::MAX).to_be_bytes(),
                    *n,
                );
                let Ok(frame) = postcard::to_stdvec(&cmd) else {
                    return;
                };
                if sink.send(&frame).is_err() {
                    // The session is gone: nothing left to report to.
                    return;
                }
            }
        }
    });
    LaneReporter(task)
}

/// The member set as its supervisor sees it: how many members have ended.
///
/// A member's end is the claim's only event that asks for something — one more
/// channel — and it is also what makes an arriving channel a *replacement*
/// rather than part of the set the client opened in one go. Both halves of that
/// rule live here so they can be read (and tested) without a TUN device: the
/// loop below is plumbing, this is the policy.
#[cfg(mh_l3_server)]
#[derive(Default)]
struct MemberSupervisor {
    ended: usize,
}

#[cfg(mh_l3_server)]
impl MemberSupervisor {
    /// Whether a channel arriving now is a replacement, and therefore paced.
    ///
    /// The claim's own channels start at once; only a channel that follows a
    /// member's death waits out [`TRANSPARENT_REPLACE_BACKOFF`], so a client
    /// that cannot serve the start command is not polled in a tight loop while
    /// a fresh claim still comes up at full width.
    fn paces_arrivals(&self) -> bool {
        self.ended > 0
    }

    /// One member ended: the claim is short exactly one channel, so this is the
    /// request that goes to the client — one per end, never one per two.
    fn member_ended(&mut self) -> DataChannelRequest {
        self.ended += 1;
        DataChannelRequest::Plain
    }
}

/// Serve one transparent service: take the channels the client opened when its
/// registration was accepted, hand the device's packets to them, and inject what
/// comes back.
///
/// The claim is carried by a **member set**, and this task is its supervisor:
/// every channel that arrives becomes one member, runs for its own lifetime,
/// and its end asks for a replacement — the client opens on demand, the same
/// path a UDP worker's replacement takes, and the tunnel pool places it.
///
/// The server is not told how many members the client configured (the
/// registration carries no channel count) and does not need to be: the channels
/// the client opens *are* the set, and the hub discovers its width from the
/// members that actually join. A member's end does not narrow it — the slot is
/// kept, so the surviving flows stay where they are — and the replacement
/// inherits the slot its predecessor held.
#[cfg(mh_l3_server)]
#[expect(
    clippy::too_many_arguments,
    reason = "the supervisor owns the receiver, requester, device, endpoint, wire format, \
              shutdown, control task and claim context; grouping them would only move the \
              same fields into one struct"
)]
async fn run_transparent_service<C>(
    mut data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    tun: String,
    endpoint: SocketAddr,
    wire: FrameFormat,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_task: tokio::task::JoinHandle<()>,
    transparent: TransparentContext,
) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use crate::transparent::hub::{TunHub, forward_transparent};
    use crate::transparent::{Direction, Endpoint};

    // The hub owns the data path's counters, so every member of a claim counts
    // into the same line.
    let hub = TunHub::get_or_spawn(&tun, Direction::Destination)?;
    let endpoint = Endpoint::new(endpoint.ip(), endpoint.port());
    hub.set_wire_format(endpoint, wire)?;
    let TransparentContext {
        claim: _claim,
        lane_valve,
        sink,
    } = transparent;
    #[cfg(not(feature = "multiplex"))]
    let _ = lane_valve;

    // The claim's lanes are reported to its client for as long as the claim
    // lives: the client decides which carrier a lane should ride, and the loss
    // that decides it happens here, where the device's packets are offered to
    // the lanes. The guard aborts the reporter on every exit from this function.
    let _reporter = spawn_lane_reporter(Arc::clone(&hub), endpoint, sink);

    let start_cmd = Arc::new(postcard::to_stdvec(
        &DataChannelCmd::StartForwardTransparent(wire),
    )?);
    // What the claim has lost, and what that makes of the channels arriving.
    let mut lost_members = MemberSupervisor::default();
    // Dropping the set aborts every member still running: a claim that goes
    // away (shutdown, a dead control session, a deregistration) takes its
    // channels with it.
    let mut members = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            channel = data_ch_rx.recv() => {
                // The lane the client named rides with the channel: the hub
                // attributes its counters to that connection, so the end that
                // decides can be told about the lane that actually carried the
                // traffic (a slot index is this end's own bookkeeping and is
                // reused by a replacement).
                let Some(mut channel) = channel else {
                    return Ok(());
                };
                // The lane the client named rides in the prologue's second
                // half, read *here* rather than where the service id was: the
                // lane is a claim's business, and this is the only place that
                // knows the channel is one. A channel that cannot name its lane
                // is dropped — reading nothing would leave its bytes to be
                // parsed as frames, and a lane nobody can attribute is exactly
                // what this is here to end.
                #[cfg(mh_l3_server)]
                let named_lane = match crate::protocol::read_lane_half(&mut channel).await {
                    Ok(lane) => Some(lane),
                    Err(e) => {
                        debug!("Transparent channel for {endpoint} named no lane: {e:#}");
                        continue;
                    }
                };
                #[cfg(not(mh_l3_server))]
                let named_lane = None;
                // The operator's valve counts these lanes too: a claim's channel
                // is a carrier connection of its own, and before this it was the
                // one kind of establishment nothing bounded. The reservation is
                // held for as long as the lane runs (moved into the member task
                // below), so a lane that ends gives its slot back.
                #[cfg(feature = "multiplex")]
                let valve_slot = match lane_valve.reserve() {
                    Ok(lane) => lane,
                    Err(held) => {
                        report_lane_refusal(held, lane_valve.cap);
                        continue;
                    }
                };
                let hub = Arc::clone(&hub);
                let start_cmd = Arc::clone(&start_cmd);
                // The first channels of a fresh claim start at once — they are
                // the member set the client just opened, not replacements —
                // while a channel that arrives after a member ended is paced:
                // a client that cannot serve the start command (an older build,
                // a broken data path of its own) must not be polled in a tight
                // loop.
                let paced = lost_members.paces_arrivals();
                members.spawn(async move {
                    // Held for the lane's whole life: dropping it (the lane
                    // ending, however it ends) releases the session slot.
                    #[cfg(feature = "multiplex")]
                    let _valve_slot = valve_slot;
                    if paced {
                        tokio::time::sleep(TRANSPARENT_REPLACE_BACKOFF).await;
                    }
                    if let Err(e) = write_and_flush(&mut channel, &start_cmd).await {
                        debug!("Transparent channel for {endpoint} died before starting: {e:#}");
                        return;
                    }
                    // The receiving end does not choose the carrier: a lane
                    // arrives on whichever listener the client reached, and the
                    // policy that reads this runs where the choice is made. It
                    // *does* record which lane it was — the client named it in
                    // the prologue — so this end's counters can be attributed
                    // to the connection that carried the traffic.
                    if let Err(e) =
                        forward_transparent(channel, hub, endpoint, named_lane, None, wire).await
                    {
                        debug!("Transparent channel for {endpoint} ended: {e:#}");
                    }
                });
            }
            joined = members.join_next(), if !members.is_empty() => {
                if let Some(Err(e)) = joined {
                    error!("A transparent member task for {endpoint} failed: {e}");
                }
                // A member ended: the claim is short one channel, so ask for a
                // replacement and keep holding the address while the client
                // opens it. The slot it held stays in the claim's set, so the
                // flows placed in it resume on the replacement.
                let _ = data_ch_req_tx.send(lost_members.member_ended());
            }
            _ = shutdown_rx.recv() => return Ok(()),
            _ = &mut control_task => return Ok(()),
        }
    }
}

/// Start a transparent service's data path.
///
/// Not a pool runner: there is no listener to accept on. It supervises the
/// **member set** of the endpoint the client claimed — the channels the client
/// opened when its registration was accepted — moving packets between the
/// operator's TUN device and whoever holds a slot, and asking for a replacement
/// whenever one ends.
#[cfg_attr(
    not(mh_l3_server),
    expect(
        unused_variables,
        clippy::needless_pass_by_value,
        reason = "the transparent data path is compiled out on this build, so the task this \
                  starts can never run; the parameters are what keep the signature and the \
                  call site one shape in every build"
    )
)]
fn spawn_transparent_service(
    service: &RegisteredService,
    data_ch_rx: mpsc::Receiver<DataChannel>,
    data_ch_req_tx: mpsc::UnboundedSender<DataChannelRequest>,
    shutdown_rx: broadcast::Receiver<bool>,
    control_task: tokio::task::JoinHandle<()>,
    transparent: TransparentContext,
    pool_died_tx: mpsc::UnboundedSender<()>,
) {
    #[cfg(mh_l3_server)]
    {
        let tun = service.transparent_tun.clone().unwrap_or_default();
        let endpoint = service.bind_addr;
        let wire = service.wire_format;
        tokio::spawn(
            async move {
                if let Err(e) = run_transparent_service(
                    data_ch_rx,
                    data_ch_req_tx,
                    tun,
                    endpoint,
                    wire,
                    shutdown_rx,
                    control_task,
                    transparent,
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
    #[cfg(not(mh_l3_server))]
    {
        // A registration is refused long before this on such a build; the
        // parameters are consumed so the signature stays one shape.
        let _ = (
            service,
            data_ch_rx,
            data_ch_req_tx,
            shutdown_rx,
            control_task,
            pool_died_tx,
        );
    }
}

/// Report that a service's pool task could not serve its listener any more.
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
    plan: Arc<crate::stripe::StripePlan>,
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
        stripe_gather,
        plan,
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
    /// Held for the whole gather, so a striped group's K channels stay atomic.
    stripe_gather: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// The server-wide stripe plan: the target this visitor's group is sized
    /// to, and the plan whose backpressure signal grows it.
    plan: Arc<crate::stripe::StripePlan>,
}

impl<C> Clone for VisitorPool<C> {
    fn clone(&self) -> Self {
        Self {
            data_ch_rx: std::sync::Arc::clone(&self.data_ch_rx),
            data_ch_req_tx: self.data_ch_req_tx.clone(),
            stripe_gather: std::sync::Arc::clone(&self.stripe_gather),
            plan: Arc::clone(&self.plan),
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
    incoming: TcpStream,
    pool: VisitorPool<C>,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut control_alive: tokio::sync::watch::Receiver<bool>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Every visitor is a stripe group; the target starts at one and grows
    // when a group reports path backpressure, so a quiet path pays one
    // channel and a busy one spreads.
    let target = pool.plan.target();
    // A multi-stripe gather is atomic: hold the group lock for the whole
    // attempt so concurrent visitors cannot interleave their K channels. A
    // one-channel group takes one item from the shared queue (already atomic)
    // and must not serialize every visitor behind a slow peer.
    let _gather = if target > 1 {
        Some(pool.stripe_gather.lock().await)
    } else {
        None
    };
    // The boolean told the accept loop to stop; for one visitor both
    // outcomes are "nothing left to do here".
    if let Err(e) = pair_striped_group(
        incoming,
        target,
        &pool.data_ch_rx,
        &pool.data_ch_req_tx,
        Arc::clone(&pool.plan),
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
///
/// Every way an attempt can end without forwarding **releases its group** on
/// the client: the stripes parked there are channels the pool has already
/// charged, and only the client can drop them before the registry's TTL. The
/// release is a request like any other (see
/// [`DataChannelRequest::ReleaseGroup`]), so it travels the same ordered
/// control sink the requests did.
#[instrument(skip_all, fields(stripes = stripe_count))]
async fn pair_striped_group<C>(
    incoming: TcpStream,
    stripe_count: usize,
    data_ch_rx: &SharedChannels<C>,
    data_ch_req_tx: &mpsc::UnboundedSender<DataChannelRequest>,
    plan: Arc<crate::stripe::StripePlan>,
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
                        // The stripes that did arrive are parked on the client.
                        // Release the group before the visitor goes: its
                        // channels must come back to the pool now, not when the
                        // registry's TTL reaps them.
                        let _ = data_ch_req_tx.send(DataChannelRequest::ReleaseGroup(group_bytes));
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
                // A broken pooled channel: drop the attempt, release the
                // stripes the client already parked for it, and gather a fresh
                // one. The next iteration's own requests replace the channels
                // that died with it, so nothing is re-asked here.
                let _ = data_ch_req_tx.send(DataChannelRequest::ReleaseGroup(group_bytes));
                drop(gathered);
                continue 'gather;
            }
        }
        debug!("Visitor paired with a {stripe_count}-stripe group {group}");
        let (read, write) = incoming.into_split();
        // The group reports its own stalls to the plan: a frame that sat
        // parked on the path longer than `STRIPE_BACKPRESSURE` and then moved
        // is this group telling the server that one more stripe would have
        // helped.
        crate::stripe::spawn_group(read, write, gathered, Some(Arc::clone(&plan)));
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
    /// Workers the pool retired on purpose: their exit must not ask for a
    /// replacement.
    retiring: Arc<std::sync::Mutex<std::collections::HashSet<usize>>>,
}

impl Drop for UdpWorkerGuard {
    fn drop(&mut self) {
        let removed = self
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id)
            .is_some();
        if !removed || self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let retired = self
            .retiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        if retired {
            debug!("UDP data channel {} retired", self.id);
            return;
        }
        debug!(
            "UDP data channel {} exited, requesting a replacement",
            self.id
        );
        // Fails only when the control channel is gone; the pool loop
        // breaks on its own in that case.
        let _ = self.req_tx.send(DataChannelRequest::Plain);
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
    /// Workers retired on purpose, so their exit asks for nothing.
    retiring: Arc<std::sync::Mutex<std::collections::HashSet<usize>>>,
    /// When each live worker was started: the idle-retirement age.
    spawned: std::collections::HashMap<usize, Instant>,
    /// The next worker id, and the round-robin cursor a route starts from.
    next: usize,
    /// The client's ceiling for this service's workers.
    max_workers: usize,
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
        let (tx, rx) = mpsc::channel(UDP_QUEUE_SIZE);
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        self.spawned.insert(id, Instant::now());
        let guard = UdpWorkerGuard {
            id,
            workers: Arc::clone(&self.workers),
            req_tx: self.req_tx.clone(),
            shutting_down: Arc::clone(&self.shutting_down),
            retiring: Arc::clone(&self.retiring),
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

/// Ask the client for one more UDP worker when the socket is dropping
/// datagrams, at most once a second. The client enforces its own ceiling.
fn note_udp_pressure(
    worker_set: &UdpWorkerSet,
    last_growth: &mut Instant,
    outcome: UdpRouteOutcome,
) {
    if outcome == UdpRouteOutcome::Enqueued || last_growth.elapsed() < Duration::from_secs(1) {
        return;
    }
    *last_growth = Instant::now();
    debug!("UDP pool under pressure, asking for one more worker");
    let _ = worker_set.req_tx.send(DataChannelRequest::Plain);
}

/// Retire one worker that has held no peer for 30 s, never the last one.
///
/// Dropping its sender closes the worker's queue; the task exits, and the
/// guard sees the id in the retiring set, so its exit asks for no replacement.
fn retire_idle_udp_worker(
    worker_set: &mut UdpWorkerSet,
    workers: &UdpWorkerMap,
    routes: &UdpRouteMap,
) {
    // A worker carrying many peers is the signal that the set should grow
    // *before* its queue overflows: ask for one more, and let the client
    // enforce max_udp_workers. The pressure is recomputed every second, so a
    // saturated worker gets another sibling on the next tick.
    let mut per_worker: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let pinned: std::collections::HashSet<usize> = {
        let guard = routes.lock().unwrap_or_else(PoisonError::into_inner);
        for route in guard.values() {
            *per_worker.entry(route.worker).or_default() += 1;
        }
        guard.values().map(|route| route.worker).collect()
    };
    let busiest = per_worker.values().copied().max().unwrap_or(0);
    if busiest > 2
        && workers.lock().unwrap_or_else(PoisonError::into_inner).len() < worker_set.max_workers
    {
        debug!("A UDP worker carries {busiest} peers, asking for another");
        let _ = worker_set.req_tx.send(DataChannelRequest::Plain);
    }
    let mut live = workers.lock().unwrap_or_else(PoisonError::into_inner);
    if live.len() <= 1 {
        return;
    }
    let candidate = worker_set
        .spawned
        .iter()
        .filter(|(id, started)| {
            !pinned.contains(*id) && started.elapsed() >= Duration::from_secs(30)
        })
        .map(|(id, _)| *id)
        .max();
    if let Some(id) = candidate {
        live.remove(&id);
        worker_set.spawned.remove(&id);
        worker_set
            .retiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
        debug!("Retiring idle UDP worker {id}");
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
    max_workers: usize,
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
        retiring: Arc::new(Mutex::new(std::collections::HashSet::new())),
        spawned: std::collections::HashMap::new(),
        next: 0,
        max_workers,
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
    // Growth is driven by pressure, at most once a second: a queue-full
    // datagram asks the client for one more worker, which it honours up to
    // its own max_udp_workers. Retirement is the mirror: a worker with no
    // pinned peer for 30 s is dropped, and its exit asks for nothing.
    let mut last_growth = Instant::now();
    let mut retire_tick = time::interval(Duration::from_secs(1));
    retire_tick.tick().await;
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
                    let outcome = route_udp_datagram(
                        &workers,
                        &routes,
                        &mut worker_set.next,
                        from,
                        Bytes::copy_from_slice(&buf[..n]),
                    );
                    match outcome {
                        UdpRouteOutcome::Enqueued => {}
                        UdpRouteOutcome::DroppedQueueFull => {
                            UDP_DROPS_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
                        }
                        UdpRouteOutcome::DroppedNoWorker => {
                            UDP_DROPS_NO_WORKER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    note_udp_pressure(&worker_set, &mut last_growth, outcome);
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
            _ = retire_tick.tick() => retire_idle_udp_worker(&mut worker_set, &workers, &routes),
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
/// `MOLEHILL_UDP_STATS` is set. Cumulative counters, like the mhd ones: a
/// reader that knows the window (or takes the first and last line of a run)
/// gets a drop *rate*, which is the number a queue-depth decision needs.
///
/// One line per **live pool** per second, with the pool's identity, its
/// affinity table (live entries and cumulative TTL evictions), its worker
/// count, and each worker's pinned peers (D30). The per-worker pinned counts
/// belong to this line rather than to a new one: they *are* the affinity
/// table, cut by the channel each peer is pinned to.
///
/// Deliberately independent of the mhd stats task: the default carrier is
/// TCP, so a run can exercise the UDP path without any mhd session existing.
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

    /// A lane and a tunnel draw on the **same** session counter, which is what
    /// makes `[server.data].max_tunnels` a bound on the carrier
    /// connections one client holds rather than on one kind of them.
    #[cfg(all(mh_l3_server, feature = "multiplex"))]
    // The module-level waiver cannot carry `expect_used`: this test is gated on
    // three cfgs, so most builds would leave the module's expectation
    // unfulfilled.
    #[expect(
        clippy::expect_used,
        reason = "the test asserts on reservations it just made"
    )]
    #[test]
    fn a_lane_and_a_tunnel_share_one_session_counter() {
        let count = TunnelCount::default();
        let valve = LaneValve::new(Some(count.clone()), 1);

        // The tunnel valve takes the only slot this cap allows.
        let tunnel = count.try_reserve(1).expect("the tunnel holds the one slot");
        assert!(
            matches!(valve.reserve(), Err(1)),
            "a lane must see the slot the tunnel holds, and the count with it"
        );

        // Releasing it lets a lane in, and the lane's own guard gives it back.
        drop(tunnel);
        let lane = valve.reserve().expect("the lane gets the released slot");
        assert!(
            matches!(valve.reserve(), Err(1)),
            "the lane's slot is held while the lane runs"
        );
        drop(lane);
        assert!(valve.reserve().is_ok(), "a released lane slot is reusable");

        // A build (or a session) with no counter never refuses, and an unlimited
        // cap is the default: `Err` is about the cap, not about the counter.
        let unlimited = LaneValve::new(None, 0);
        assert!(unlimited.reserve().is_ok());
        let uncapped = LaneValve::new(Some(TunnelCount::default()), 0);
        assert!(
            uncapped.reserve().is_ok() && uncapped.reserve().is_ok(),
            "0 is unlimited"
        );
    }

    /// Serving L3 is the *server operator's* decision: without the table a
    /// transparent registration is refused by policy — with a reason that names
    /// the missing switch, not a missing device — and with it the device named
    /// there is what the data path gets. `[server.transparent]` is an `Option`
    /// for exactly this reason: a defaulted table would leave this process
    /// armed to attach a device the operator never asked for.
    #[cfg(mh_l3_server)]
    #[test]
    fn the_server_switch_governs_transparent_registrations() {
        let mut server = ServerConfig::default();
        let refusal = transparent_for(&server).unwrap_err();
        assert!(
            refusal.contains("[server.transparent]"),
            "the refusal must name the missing switch, got: {refusal}"
        );

        server.transparent = Some(crate::config::parsing::TransparentConfig {
            tun: "l3test0".to_string(),
        });
        let transparent = transparent_for(&server).unwrap();
        assert_eq!(
            transparent.tun, "l3test0",
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
            let (tx, rx) = mpsc::channel(UDP_QUEUE_SIZE);
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
                    endpoint: "127.0.0.1:0".parse().unwrap(),
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
    /// The prologue a stream of a multiplexed tunnel opens with: the service,
    /// then the tunnel it rides (see `write_tunnel_prologue`).
    async fn write_prologue(tx: &mut tokio::io::DuplexStream, id: u32) {
        let mut buf = [0u8; 8];
        buf[..4].copy_from_slice(&id.to_be_bytes());
        buf[4..].copy_from_slice(&1u32.to_be_bytes());
        tx.write_all(&buf).await.unwrap();
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
        let routed = route_tunnel_stream(&mut rx, &sessions, &nonce, &std::sync::OnceLock::new())
            .await
            .expect("a registered service must route");
        assert!(routed.same_channel(&queue_a));
        assert!(!routed.same_channel(&queue_b));

        // An unregistered id is dropped.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        write_prologue(&mut tx, 9).await;
        assert!(
            route_tunnel_stream(&mut rx, &sessions, &nonce, &std::sync::OnceLock::new())
                .await
                .is_none()
        );

        // A stream that ends before naming a service is dropped too.
        let (tx, mut rx) = tokio::io::duplex(64);
        drop(tx);
        assert!(
            route_tunnel_stream(&mut rx, &sessions, &nonce, &std::sync::OnceLock::new())
                .await
                .is_none()
        );

        // A *sibling* service is routable on the same session, which is what a
        // shared pool (`[client.data].shared_pool`) needs: one tunnel carries
        // streams of several services, each routed by its own prologue.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        write_prologue(&mut tx, 2).await;
        let routed = route_tunnel_stream(&mut rx, &sessions, &nonce, &std::sync::OnceLock::new())
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
        // The release is the group's *id* and its service, nothing else: the
        // client drops whatever it parked for that group and nothing that
        // belongs to another one.
        assert!(matches!(
            data_channel_cmd(
                service,
                &DataChannelRequest::ReleaseGroup([0x01, 0x02, 0x03, 0x04])
            ),
            ControlChannelCmd::ReleaseStripedGroup(id, group)
                if id == service && group == [0x01, 0x02, 0x03, 0x04]
        ));
    }

    /// A gather that no channel ever completes ends after its budget, and its
    /// group is released on the client: the stripes parked there are channels
    /// the pool has charged, and only the client can drop them before its
    /// registry's TTL. Time is paused, so the budget costs the test nothing.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_gather_releases_its_group() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let visitor = tokio::spawn(tokio::net::TcpStream::connect(addr));
        let (incoming, _) = listener.accept().await.unwrap();
        let _visitor = visitor.await.unwrap().unwrap();

        // A channel queue nobody fills: this gather is the shape of a pool at
        // its ceiling, where the client has no channel to give.
        let (_ch_tx, ch_rx) = mpsc::channel::<tokio::io::DuplexStream>(8);
        let data_ch_rx: SharedChannels<tokio::io::DuplexStream> =
            std::sync::Arc::new(tokio::sync::Mutex::new(ch_rx));
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let (_alive_tx, mut alive_rx) = tokio::sync::watch::channel(true);

        let session_over = pair_striped_group(
            incoming,
            2,
            &data_ch_rx,
            &req_tx,
            std::sync::Arc::new(crate::stripe::StripePlan::new(2)),
            &mut shutdown_rx,
            &mut alive_rx,
        )
        .await
        .unwrap();
        assert!(
            !session_over,
            "an abandoned gather must not report the session over"
        );

        let mut requests = Vec::new();
        while let Ok(request) = req_rx.try_recv() {
            requests.push(request);
        }
        let asked: Vec<[u8; 4]> = requests
            .iter()
            .filter_map(|request| match request {
                DataChannelRequest::Stripe { group, .. } => Some(*group),
                _ => None,
            })
            .collect();
        let released: Vec<[u8; 4]> = requests
            .iter()
            .filter_map(|request| match request {
                DataChannelRequest::ReleaseGroup(group) => Some(*group),
                _ => None,
            })
            .collect();
        assert_eq!(
            asked.len(),
            2 * PAIR_ATTEMPTS,
            "each budget expiry must re-ask for the stripes still missing"
        );
        assert_eq!(released.len(), 1, "an abandoned gather releases one group");
        assert!(
            asked.iter().all(|group| *group == released[0]),
            "the release named a group the gather never asked for"
        );
        assert!(
            matches!(requests.last(), Some(DataChannelRequest::ReleaseGroup(_))),
            "the release must come after the requests it cancels"
        );
    }

    /// A claim's member set: its first channels are the set the client opened
    /// and start at once, and from the first member's death on every arriving
    /// channel is a replacement and is paced. Each end asks for exactly one
    /// channel — one death, one request — because a claim short two members
    /// must be repaired twice, not once.
    #[cfg(mh_l3_server)]
    #[test]
    fn a_member_end_makes_the_next_channel_a_paced_replacement() {
        let mut supervisor = MemberSupervisor::default();
        assert!(
            !supervisor.paces_arrivals(),
            "the set the client opened in one go is not paced"
        );
        assert!(
            matches!(supervisor.member_ended(), DataChannelRequest::Plain),
            "one member's end asks for one plain channel"
        );
        assert!(
            supervisor.paces_arrivals(),
            "a channel arriving after a death is a replacement, and is paced"
        );
        assert!(
            matches!(supervisor.member_ended(), DataChannelRequest::Plain),
            "a second end is a second request, never one for two"
        );
    }
}
