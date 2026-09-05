use crate::common::helper::{host_port_pair, udp_connect};
#[cfg(feature = "notify")]
use crate::config::ClientServiceChange;
use crate::config::ConfigChange;
use crate::config::{
    ClientConfig, ClientServiceConfig, Config, HealthCheckConfig, HealthCheckType, MaskedString,
    ServiceType, TransportType,
};
use crate::protocol::Hello::{self, ControlChannelHello};
use crate::protocol::{
    self, Ack, Auth, CURRENT_PROTO_VERSION, ControlChannelCmd, DataChannelCmd, MAX_UDP_HEADER_LEN,
    ServiceRegistration, UdpTraffic, read_ack, read_control_cmd, read_data_cmd, read_hello,
    read_register_result, write_registration,
};
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
use anyhow::{Context, Result, anyhow, bail};
use backon::BackoffBuilder;
use backon::ExponentialBuilder;
use backon::Retryable;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, copy_bidirectional_with_sizes};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot, watch};
use tokio::time::{self, Duration, Instant};
use tracing::{Instrument, Span, debug, error, info, instrument, trace, warn};

#[cfg(feature = "noise")]
use crate::transport::NoiseTransport;
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use crate::transport::TlsTransport;
#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
use crate::transport::WebsocketTransport;
#[cfg(feature = "multiplex")]
use crate::transport::multiplex::ClientTunnel;

use crate::common::constants::{
    DEFAULT_TCP_POOL_SIZE, DEFAULT_UDP_BUFFER_SIZE, DEFAULT_UDP_IDLE_TIMEOUT_SECS,
    DEFAULT_UDP_POOL_SIZE, DEFAULT_UDP_SENDQ_SIZE, TCP_COPY_BUFFER_SIZE, run_control_chan_backoff,
};

/// The server rejected this service's registration (port not allowed, port
/// already in use, ...). Retrying cannot help, so the client gives up on this
/// service until the configuration changes.
#[derive(Debug)]
struct RegistrationRejected(String);

impl std::fmt::Display for RegistrationRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegistrationRejected {}

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

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut client = Client::<TcpTransport>::from(config)?;
            client.run(shutdown_rx, update_rx).await
        }
        TransportType::Tls => {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            {
                let mut client = Client::<TlsTransport>::from(config)?;
                client.run(shutdown_rx, update_rx).await
            }
            #[cfg(not(any(feature = "native-tls", feature = "rustls")))]
            crate::common::helper::feature_neither_compile("native-tls", "rustls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut client = Client::<NoiseTransport>::from(config)?;
                client.run(shutdown_rx, update_rx).await
            }
            #[cfg(not(feature = "noise"))]
            crate::common::helper::feature_not_compile("noise")
        }
        TransportType::Websocket => {
            #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
            {
                let mut client = Client::<WebsocketTransport>::from(config)?;
                client.run(shutdown_rx, update_rx).await
            }
            #[cfg(not(any(feature = "websocket-native-tls", feature = "websocket-rustls")))]
            crate::common::helper::feature_neither_compile(
                "websocket-native-tls",
                "websocket-rustls",
            )
        }
    }
}

type ServiceDigest = protocol::Digest;
type Nonce = protocol::Digest;

/// Multiplexing knobs resolved from `[client]`.
#[derive(Clone, Copy, Debug, Default)]
struct MuxOpts {
    enabled: bool,
    #[cfg(feature = "multiplex")]
    receive_window: Option<usize>,
    #[cfg(feature = "multiplex")]
    max_streams: Option<usize>,
}

/// Placeholder so control-channel code compiles unchanged without the
/// `multiplex` feature (`MuxOpts::enabled` is always `false` there).
#[cfg(not(feature = "multiplex"))]
#[derive(Clone)]
struct ClientTunnel;

impl From<&ClientConfig> for MuxOpts {
    fn from(c: &ClientConfig) -> Self {
        MuxOpts {
            enabled: c.mux_enabled(),
            #[cfg(feature = "multiplex")]
            receive_window: c.mux_receive_window(),
            #[cfg(feature = "multiplex")]
            max_streams: c.mux_max_streams(),
        }
    }
}

// Holds the state of a client
struct Client<T: Transport> {
    config: ClientConfig,
    service_handles: HashMap<String, ControlChannelHandle>,
    transport: Arc<T>,
}

impl<T: 'static + Transport> Client<T> {
    // Create a Client from `[client]` config block
    fn from(config: ClientConfig) -> Result<Client<T>> {
        let transport =
            Arc::new(T::new(&config.transport).with_context(|| "Failed to create the transport")?);
        Ok(Client {
            config,
            service_handles: HashMap::new(),
            transport,
        })
    }

    // The entrypoint of Client
    async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        for (name, config) in &self.config.services {
            // Create a control channel for each service defined
            let handle = ControlChannelHandle::new(
                (*config).clone(),
                self.config.remote_addr.clone(),
                self.config.default_token.clone(),
                self.transport.clone(),
                self.config.heartbeat_timeout,
                MuxOpts::from(&self.config),
            );
            self.service_handles.insert(name.clone(), handle);
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

        // Shutdown all services
        for (_, handle) in self.service_handles.drain() {
            handle.shutdown();
        }

        Ok(())
    }

    /// Apply one client-service config change (add or remove a service).
    #[cfg(feature = "notify")]
    fn handle_hot_reload(&mut self, e: ConfigChange) {
        match e {
            ConfigChange::ClientChange(client_change) => match client_change {
                ClientServiceChange::Add(cfg) => {
                    let name = cfg.name.clone();
                    let handle = ControlChannelHandle::new(
                        cfg,
                        self.config.remote_addr.clone(),
                        self.config.default_token.clone(),
                        self.transport.clone(),
                        self.config.heartbeat_timeout,
                        MuxOpts::from(&self.config),
                    );
                    let _ = self.service_handles.insert(name, handle);
                }
                ClientServiceChange::Delete(s) => {
                    let _ = self.service_handles.remove(&s);
                }
            },
            ignored @ ConfigChange::General(_) => {
                warn!("Ignored {ignored:?} since running as a client");
            }
        }
    }
}

struct RunDataChannelArgs<T: Transport> {
    session_key: Nonce,
    remote_addr: AddrMaybeCached,
    connector: Arc<T>,
    socket_opts: SocketOpts,
    service: ClientServiceConfig,
    /// Shared UDP hub for the service; `Some` iff this is a UDP service.
    udp: Option<Arc<UdpHub>>,
}

async fn do_data_channel_handshake<T: Transport>(
    args: Arc<RunDataChannelArgs<T>>,
) -> Result<T::Stream> {
    // Retry at least every 100ms, at most for 10 seconds
    let backoff = ExponentialBuilder::default()
        .with_max_delay(Duration::from_millis(100))
        .with_total_delay(Some(Duration::from_secs(10)));

    // Connect to remote_addr
    let mut conn: T::Stream = (|| async {
        args.connector
            .connect(&args.remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", args.remote_addr))
    })
    .retry(backoff)
    .notify(|e: &anyhow::Error, duration| {
        warn!("{:#}. Retry in {:?}", e, duration);
    })
    .await?;

    T::hint(&conn, args.socket_opts);

    // Send nonce
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, args.session_key);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;

    Ok(conn)
}

async fn run_data_channel<T: Transport>(args: Arc<RunDataChannelArgs<T>>) -> Result<()> {
    // Do the handshake
    let conn = do_data_channel_handshake(args.clone()).await?;

    // Forward
    forward_data_channel(conn, &args.service, args.udp.clone()).await
}

/// Run a data channel as one stream of the multiplexed tunnel.
#[cfg(feature = "multiplex")]
async fn run_mux_data_channel<T: Transport>(
    args: &Arc<RunDataChannelArgs<T>>,
    tunnel: &ClientTunnel,
) -> Result<()> {
    let stream = tunnel
        .open_stream()
        .await
        .map_err(|e| anyhow!("Failed to open a multiplexed data channel: {e}"))?;
    trace!("Multiplexed data channel opened");
    forward_data_channel(stream, &args.service, args.udp.clone()).await
}

/// Wait for the server's forwarding command and start copying traffic.
async fn forward_data_channel<S>(
    mut conn: S,
    service: &ClientServiceConfig,
    udp_hub: Option<Arc<UdpHub>>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardTcp => {
            if service.service_type != ServiceType::Tcp {
                bail!("Expect TCP traffic. Please check the configuration.")
            }
            let sock_opts = SocketOpts::from_client_cfg(service);
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
    }
    Ok(())
}

/// Dial an extra connection and upgrade it into a yamux data tunnel.
///
/// The returned sender shuts the driver down when dropped.
#[cfg(feature = "multiplex")]
#[allow(clippy::too_many_arguments)]
async fn establish_tunnel<T: Transport>(
    transport: &Arc<T>,
    remote_addr: &AddrMaybeCached,
    session_key: Nonce,
    opts: MuxOpts,
    service_name: &str,
) -> Result<(ClientTunnel, watch::Sender<bool>)> {
    let mut conn = transport
        .connect(remote_addr)
        .await
        .with_context(|| format!("Failed to connect the data tunnel to {remote_addr}"))?;
    T::hint(&conn, SocketOpts::for_control_channel());

    let hello = Hello::DataChannelTunnelHello(CURRENT_PROTO_VERSION, session_key);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;

    match read_ack(&mut conn).await? {
        Ack::Ok => {}
        v => bail!("Service {service_name}: the server refused the multiplexed data tunnel: {v}"),
    }

    let config = crate::transport::multiplex::mux_config(opts.receive_window, opts.max_streams);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tunnel = ClientTunnel::start(conn, config, shutdown_rx);
    debug!(service = %service_name, "Multiplexed data tunnel established");
    Ok((tunnel, shutdown_tx))
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

fn udp_sendq_size(s: &ClientServiceConfig) -> usize {
    s.udp_sendq_size
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
    let _ = copy_bidirectional_with_sizes(
        &mut conn,
        &mut local,
        TCP_COPY_BUFFER_SIZE,
        TCP_COPY_BUFFER_SIZE,
    )
    .await;
    Ok(())
}

/// Creation-time parameters for the per-service UDP hub, resolved once from
/// the service configuration per control-channel session.
struct UdpForwardParams {
    local_addr: String,
    prefer_ipv6: bool,
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
}

impl UdpHub {
    fn new(params: UdpForwardParams) -> Self {
        UdpHub {
            params,
            routes: RwLock::new(HashMap::new()),
            channels: RwLock::new(Vec::new()),
            next_channel: AtomicUsize::new(0),
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
    ) {
        {
            let mut routes = me.routes.write().await;
            if let Some(route) = routes.get_mut(&from) {
                route.outbound = channel;
                if let Err(e) = route.inbound.try_send(data) {
                    debug!("UDP forwarder queue full for {from}, dropping a datagram: {e}");
                }
                return;
            }
        }
        // First datagram from this peer: bind its dedicated local socket
        // outside the lock, then insert the route. Another channel may have
        // created the peer in the meantime; reuse that forwarder.
        let socket = match udp_connect(&me.params.local_addr, me.params.prefer_ipv6).await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to connect to the local UDP service: {e:#}");
                return;
            }
        };
        let mut routes = me.routes.write().await;
        if let Some(route) = routes.get_mut(&from) {
            // Lost the race; drop our freshly bound socket and reuse the
            // existing forwarder.
            drop(socket);
            route.outbound = channel;
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
            ));
            routes.insert(
                from,
                UdpVisitorRoute {
                    inbound: inbound_tx.clone(),
                    outbound: channel,
                },
            );
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
    async fn send_outbound(&self, from: SocketAddr, mut t: UdpTraffic) {
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
        self.routes
            .write()
            .await
            .entry(from)
            .and_modify(|r| r.outbound = fallback.clone());
        if let Err(e) = fallback.try_send(t) {
            debug!("Dropped a UDP datagram for {from}: {e}");
        }
    }
}

#[instrument(skip_all)]
async fn run_data_channel_for_udp<S>(conn: S, hub: Arc<UdpHub>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    debug!("New data channel starts forwarding");

    let (wr_tx, mut wr_rx) = mpsc::channel::<UdpTraffic>(hub.params.sendq_size);
    hub.register_channel(wr_tx.clone()).await;

    let (mut rd, mut wr) = io::split(conn);

    // Keep sending datagrams the hub routes to this channel to the server.
    // The scratch buffer is reused across packets: each datagram is framed
    // into it once and emitted with a single write (single TLS/Noise record).
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

    let res = udp_read_loop(&mut rd, &hub, &wr_tx).await;

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
        UdpHub::deliver(Arc::clone(hub), packet.from, packet.data, wr_tx.clone()).await;
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
                let Ok(len) = val else { break };

                let t = UdpTraffic{
                    from,
                    data: Bytes::copy_from_slice(&buf[..len])
                };

                hub.send_outbound(from, t).await;
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

// Control channel, using T as the transport layer
struct ControlChannel<T: Transport> {
    digest: ServiceDigest,              // SHA256 of the service name
    service: ClientServiceConfig,       // `[client.services.foo]` config block
    token: MaskedString,                // `client.default_token`
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    remote_addr: String,                // `client.remote_addr`
    transport: Arc<T>,                  // Wrapper around the transport layer
    heartbeat_timeout: u64,             // Application layer heartbeat timeout in secs
    mux: MuxOpts,                       // Multiplexing knobs
}

/// Build the per-service UDP hub for a UDP service session: every data
/// channel registers with it, and every remote peer keeps one local
/// forwarder socket regardless of which channel carries it (session
/// affinity, see `UdpHub`). TCP services get `None`.
fn build_udp_hub(service: &ClientServiceConfig) -> Option<Arc<UdpHub>> {
    match service.service_type {
        ServiceType::Udp => Some(Arc::new(UdpHub::new(UdpForwardParams {
            local_addr: service.local_addr.clone(),
            prefer_ipv6: service.prefer_ipv6,
            buffer_size: udp_buffer_size(service),
            idle_timeout_secs: udp_idle_timeout_secs(service),
            sendq_size: udp_sendq_size(service),
        }))),
        ServiceType::Tcp => None,
    }
}

// Handle of a control channel
// Dropping it will also drop the actual control channel
struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
    // Stops the health-check task; dropped together with the handle
    health_stop_tx: oneshot::Sender<u8>,
}

impl<T: 'static + Transport> ControlChannel<T> {
    #[instrument(skip_all)]
    async fn run(&mut self, mut health_rx: Option<&mut watch::Receiver<bool>>) -> Result<()> {
        let mut remote_addr = AddrMaybeCached::new(&self.remote_addr);
        remote_addr.resolve().await?;

        let mut conn = self
            .transport
            .connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", self.remote_addr))?;
        T::hint(&conn, SocketOpts::for_control_channel());

        // Authenticate the session, then register the service: the server
        // owns no per-service configuration, so the client declares its
        // public endpoint and the server validates it against its policy.
        let session_key = self.authenticate(&mut conn).await?;
        let reg = self.register(&mut conn).await?;
        info!(
            service = %self.service.name,
            "Registered, exposed at {}", reg.bind_addr
        );

        // Establish the multiplexed tunnel if enabled: one extra connection
        // carrying every future data channel as a yamux stream.
        #[cfg_attr(not(feature = "multiplex"), allow(unused_variables))]
        let (tunnel, _tunnel_shutdown_tx) = if self.mux.enabled {
            #[cfg(feature = "multiplex")]
            match establish_tunnel(
                &self.transport,
                &remote_addr,
                session_key,
                self.mux,
                &self.service.name,
            )
            .await
            {
                Ok((t, tx)) => (Some(t), Some(tx)),
                Err(e) => return Err(e),
            }
            #[cfg(not(feature = "multiplex"))]
            (None::<ClientTunnel>, None::<watch::Sender<bool>>)
        } else {
            (None, None)
        };

        // Channel ready
        info!("Control channel established, remote {}", self.remote_addr);

        // Socket options for the data channel
        let socket_opts = SocketOpts::from_client_cfg(&self.service);
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            connector: self.transport.clone(),
            socket_opts,
            service: self.service.clone(),
            udp: build_udp_hub(&self.service),
        });

        loop {
            tokio::select! {
                val = read_control_cmd(&mut conn) => {
                    let val = val?;
                    debug!( "Received {:?}", val);
                    match val {
                        ControlChannelCmd::CreateDataChannel => {
                            let args = data_ch_args.clone();
                            #[cfg(feature = "multiplex")]
                            let tunnel = tunnel.clone();
                            tokio::spawn(async move {
                                let res = {
                                    #[cfg(feature = "multiplex")]
                                    match &tunnel {
                                        Some(t) => run_mux_data_channel(&args, t).await,
                                        None => run_data_channel(args).await,
                                    }
                                    #[cfg(not(feature = "multiplex"))]
                                    run_data_channel(args).await
                                };
                                if let Err(e) =
                                    res.with_context(|| "Failed to run the data channel")
                                {
                                    warn!("{:#}", e);
                                }
                            }.instrument(Span::current()));
                        },
                        ControlChannelCmd::HeartBeat => ()
                    }
                },
                () = time::sleep(Duration::from_secs(self.heartbeat_timeout)), if self.heartbeat_timeout != 0 => {
                    return Err(anyhow!(
                        "Heartbeat timed out after {} seconds",
                        self.heartbeat_timeout
                    ))
                }
                _ = &mut self.shutdown_rx => {
                    break;
                }
                changed = health_changed(health_rx.as_deref_mut()), if health_rx.is_some() => {
                    if changed == Some(false) {
                        // The local service went down: drop this channel. The
                        // retry loop in `ControlChannelHandle::new` waits for
                        // the service to recover before reconnecting.
                        debug!("Local service is unhealthy, dropping the control channel");
                        break;
                    }
                }
            }
        }

        info!("Control channel shutdown");
        Ok(())
    }

    /// Hello/auth handshake of a control channel.
    ///
    /// Returns the session key derived from the server's nonce; it also
    /// authenticates the multiplexed tunnel.
    async fn authenticate(&self, conn: &mut T::Stream) -> Result<Nonce> {
        // Send hello
        debug!("Sending hello");
        let hello_send = Hello::ControlChannelHello(CURRENT_PROTO_VERSION, self.digest);
        conn.write_all(&postcard::to_stdvec(&hello_send)?).await?;
        conn.flush().await?;

        // Read hello
        debug!("Reading hello");
        let ControlChannelHello(_, nonce) = read_hello(conn).await? else {
            bail!("Unexpected type of hello");
        };

        // Send auth
        debug!("Sending auth");
        let mut concat = Vec::from(self.token.as_bytes());
        concat.extend_from_slice(&nonce);

        let session_key = protocol::digest(&concat);
        conn.write_all(&postcard::to_stdvec(&Auth(session_key))?)
            .await?;
        conn.flush().await?;

        // Read ack
        debug!("Reading ack");
        match read_ack(conn).await? {
            Ack::Ok => {}
            v => {
                return Err(anyhow!("{v}"))
                    .with_context(|| format!("Authentication failed: {}", self.service.name));
            }
        }
        Ok(session_key)
    }

    /// Register the service on an authenticated control channel and wait
    /// for the server's verdict.
    ///
    /// Returns the accepted registration.
    async fn register(&self, conn: &mut T::Stream) -> Result<ServiceRegistration> {
        let bind_addr: SocketAddr = self.service.remote_bind_addr.parse().map_err(|_| {
            anyhow!(
                "service {}: invalid `remote_bind_addr`: {:?}",
                self.service.name,
                self.service.remote_bind_addr
            )
        })?;
        let reg = ServiceRegistration {
            name: self.service.name.clone(),
            service_type: self.service.service_type,
            bind_addr,
            pool_size: self
                .service
                .pool_size
                .unwrap_or(match self.service.service_type {
                    ServiceType::Tcp => DEFAULT_TCP_POOL_SIZE,
                    ServiceType::Udp => DEFAULT_UDP_POOL_SIZE,
                }),
            udp_buffer_size: self
                .service
                .udp_buffer_size
                .unwrap_or_else(|| u16::try_from(DEFAULT_UDP_BUFFER_SIZE).unwrap_or(u16::MAX)),
        };
        write_registration(conn, &reg).await?;

        debug!(service = %self.service.name, "Waiting for the registration result");
        match read_register_result(conn).await? {
            Ack::Ok => {}
            Ack::RegisterRejected(reason) => {
                return Err(RegistrationRejected(reason)).with_context(|| {
                    format!("Service {} was rejected by the server", self.service.name)
                });
            }
            v @ Ack::AuthFailed => bail!("Unexpected registration result: {v}"),
        }
        Ok(reg)
    }
}

impl ControlChannelHandle {
    #[instrument(name="handle", skip_all, fields(service = %service.name))]
    fn new<T: 'static + Transport>(
        service: ClientServiceConfig,
        remote_addr: String,
        token: MaskedString,
        transport: Arc<T>,
        heartbeat_timeout: u64,
        mux: MuxOpts,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(service.name.as_bytes());

        info!("Starting service {}", service.name);
        debug!("Service digest: {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Dropped together with the handle so the health task never leaks
        let (health_stop_tx, health_stop_rx) = oneshot::channel();

        // Config validation fills `retry_interval` from the global default
        let backoff_builder = run_control_chan_backoff(service.retry_interval.unwrap_or(1));

        // Health-check wiring. The watch starts healthy and is flipped by the
        // health task when the local service goes down/up. The control channel
        // task drops the channel while unhealthy (which removes the service
        // from the server) and reconnects once the service recovers.
        let mut health_rx = match service.health_check.clone() {
            Some(hc) => {
                let (health_tx, health_rx) = watch::channel(true);
                tokio::spawn(run_health_check(
                    hc,
                    service.local_addr.clone(),
                    service.name.clone(),
                    health_tx,
                    health_stop_rx,
                ));
                Some(health_rx)
            }
            None => None,
        };

        let mut s = ControlChannel {
            digest,
            service,
            token,
            shutdown_rx,
            remote_addr,
            transport,
            heartbeat_timeout,
            mux,
        };

        tokio::spawn(
            async move {
                let mut start = Instant::now();
                let mut retry_backoff = backoff_builder.build();

                loop {
                    // Wait until the local service is healthy again (a no-op
                    // when no health check is configured). While the service
                    // is down no control channel is kept on the server, so
                    // visitors fail fast instead of being forwarded to a dead
                    // local service.
                    if let Some(health_rx) = &mut health_rx {
                        while !*health_rx.borrow() {
                            if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty)
                            {
                                return;
                            }
                            // Poll once per second so that shutdown signals
                            // are still noticed while waiting for recovery.
                            let _ =
                                tokio::time::timeout(Duration::from_secs(1), health_rx.changed())
                                    .await;
                        }
                    }

                    match s
                        .run(health_rx.as_mut())
                        .await
                        .with_context(|| "Failed to run the control channel")
                    {
                        Ok(()) => {
                            if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty)
                            {
                                return;
                            }
                            // `run` returned because the local service became
                            // unhealthy; wait for recovery in the loop above.
                        }
                        Err(err) => {
                            if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty)
                            {
                                return;
                            }

                            // A rejected registration is a configuration or
                            // policy problem on the server: retrying just
                            // spams it. Stop and surface a precise error.
                            if let Some(rej) = err.downcast_ref::<RegistrationRejected>() {
                                error!(
                                    "Server rejected service {}: {}. Giving up. Fix the client config or the server's `allow_ports`, then restart.",
                                    s.service.name, rej
                                );
                                return;
                            }

                            if start.elapsed() > Duration::from_secs(3) {
                                // The client runs for at least 3 secs and then disconnects
                                retry_backoff = backoff_builder.build();
                            }

                            if let Some(duration) = retry_backoff.next() {
                                error!("{:#}. Retry in {:?}...", err, duration);
                                time::sleep(duration).await;
                            } else {
                                // Should never be reached with the current backoff policy,
                                // but keep the channel alive instead of panicking.
                                warn!("{:#}. Backoff exhausted, retrying in 1s", err);
                                time::sleep(Duration::from_secs(1)).await;
                            }

                            start = Instant::now();
                        }
                    }
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            shutdown_tx,
            health_stop_tx,
        }
    }

    fn shutdown(self) {
        // A send failure shows that the actor has already shutdown.
        let _ = self.shutdown_tx.send(0u8);
        let _ = self.health_stop_tx.send(0u8);
    }
}

// Awaits the next health-state change. Returns `None` when there is no health
// monitor (or it has exited, which only happens on shutdown).
async fn health_changed(rx: Option<&mut watch::Receiver<bool>>) -> Option<bool> {
    let rx = rx?;
    rx.changed().await.ok().map(|()| *rx.borrow())
}

// Probes the local service of a service and flips `health_tx` whenever the
// healthy state changes. Exits when the handle is dropped (`shutdown_rx`
// closes), at which point the control channel task stops watching health.
#[instrument(skip_all, fields(service = %service_name))]
async fn run_health_check(
    cfg: HealthCheckConfig,
    local_addr: String,
    service_name: String,
    health_tx: watch::Sender<bool>,
    mut shutdown_rx: oneshot::Receiver<u8>,
) {
    let mut consecutive_failures: u32 = 0;
    let mut healthy = true;
    loop {
        // Probe immediately on start, then once per interval
        let ok = tokio::time::timeout(
            Duration::from_secs(cfg.timeout),
            health_probe(&cfg, &local_addr),
        )
        .await
        .is_ok_and(|r| r.is_ok());

        consecutive_failures = if ok {
            0
        } else {
            consecutive_failures.saturating_add(1)
        };
        let new_healthy = consecutive_failures < cfg.max_failed;
        if new_healthy != healthy {
            healthy = new_healthy;
            let _ = health_tx.send(healthy);
            if healthy {
                info!("Local service is healthy again, re-registering the service");
            } else {
                warn!(
                    "Local service is unhealthy ({} consecutive failures), removing the service from the server",
                    consecutive_failures
                );
            }
        }

        tokio::select! {
            _ = &mut shutdown_rx => return,
            () = time::sleep(Duration::from_secs(cfg.interval)) => {}
        }
    }
}

// Probe the local service. Returns Ok(()) when it is reachable.
async fn health_probe(cfg: &HealthCheckConfig, local_addr: &str) -> Result<()> {
    match cfg.check_type {
        HealthCheckType::Tcp => {
            let _ = TcpStream::connect(local_addr).await?;
            Ok(())
        }
        HealthCheckType::Http => {
            let mut stream = TcpStream::connect(local_addr).await?;
            let (host, port) = host_port_pair(local_addr)?;
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                cfg.http_path, host, port
            );
            stream.write_all(req.as_bytes()).await?;
            stream.flush().await?;

            // Read until the status line is available
            let mut buf = Vec::with_capacity(256);
            let mut chunk = [0u8; 256];
            loop {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() >= 4096 {
                    break;
                }
            }

            let status = String::from_utf8_lossy(&buf)
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            if (200..400).contains(&status) {
                Ok(())
            } else {
                bail!("HTTP health check returned status {status}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn health_cfg(check_type: HealthCheckType) -> HealthCheckConfig {
        HealthCheckConfig {
            check_type,
            interval: 1,
            timeout: 1,
            max_failed: 1,
            http_path: "/".to_string(),
        }
    }

    // Serves a fixed HTTP response. The request is read first: closing a
    // socket with unread data sends RST on Windows instead of FIN, which
    // would surface as a connection-reset error in the probe.
    fn spawn_http_server(listener: TcpListener, response: &'static [u8]) {
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let response = response.to_vec();
                tokio::spawn(async move {
                    let mut req = [0u8; 1024];
                    let _ = s.read(&mut req).await;
                    let _ = s.write_all(&response).await;
                    let _ = s.flush().await;
                });
            }
        });
    }

    #[tokio::test]
    async fn health_probe_tcp_ok() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(
            health_probe(&health_cfg(HealthCheckType::Tcp), &addr)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn health_probe_tcp_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        // A port that was just released must refuse connections
        assert!(
            health_probe(&health_cfg(HealthCheckType::Tcp), &addr)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn health_probe_http_ok() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_http_server(listener, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");

        assert!(
            health_probe(&health_cfg(HealthCheckType::Http), &addr)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn health_probe_http_5xx_is_unhealthy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_http_server(
            listener,
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        );

        assert!(
            health_probe(&health_cfg(HealthCheckType::Http), &addr)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn health_check_flips_state_and_recovers() {
        // The service is up at first
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (health_tx, mut health_rx) = watch::channel(true);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(run_health_check(
            health_cfg(HealthCheckType::Tcp),
            addr.clone(),
            "test".to_string(),
            health_tx,
            shutdown_rx,
        ));

        // Healthy at start, still healthy after a few probes
        assert!(*health_rx.borrow());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(*health_rx.borrow());

        // The service goes down: the state must flip to unhealthy
        drop(listener);
        tokio::time::timeout(Duration::from_secs(5), health_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(!*health_rx.borrow());

        // The service comes back: the state must flip to healthy again.
        // Re-binding the just-released port can briefly fail on some
        // platforms, so retry.
        let listener = {
            let mut rebound = None;
            for _ in 0..20 {
                match TcpListener::bind(&addr).await {
                    Ok(l) => {
                        rebound = Some(l);
                        break;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
            rebound.expect("failed to rebind the released port")
        };
        tokio::time::timeout(Duration::from_secs(5), health_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*health_rx.borrow());
        drop(listener);

        // Dropping the sender stops the health task
        drop(shutdown_tx);
    }
}
