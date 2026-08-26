use crate::common::constants::TCP_COPY_BUFFER_SIZE;
use crate::common::helper::write_and_flush;
use crate::common::multi_map::MultiMap;
use crate::config::ConfigChange;
use crate::config::{Config, ServerConfig, ServiceType, TransportType};
use crate::protocol::Hello::{ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, Ack, ControlChannelCmd, DataChannelCmd, HASH_WIDTH_IN_BYTES, Hello, MAX_UDP_HEADER_LEN,
    UdpTraffic, read_auth, read_hello, read_registration, write_register_result,
};
use crate::transport::{SocketOpts, TcpTransport, Transport};
use anyhow::{Context, Result, anyhow, bail};
use backon::BackoffBuilder;
use backon::ExponentialBuilder;
use bytes::BytesMut;

use rand::TryRng;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, copy_bidirectional_with_sizes};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time;
use tracing::{Instrument, Span, debug, error, info, info_span, instrument, warn};

#[cfg(feature = "noise")]
use crate::transport::NoiseTransport;
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use crate::transport::TlsTransport;
#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
use crate::transport::WebsocketTransport;

type ServiceDigest = protocol::Digest; // SHA256 of a service name
type Nonce = protocol::Digest; // Also called `session_key`

const CHAN_SIZE: usize = 2048; // The capacity of various chans
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake

/// Runtime description of a service, as registered by a client.
///
/// The server owns no per-service configuration: everything needed to expose
/// the service arrives in the client's registration and has already been
/// validated against `[server].allow_ports` / `max_pool_size`.
#[derive(Clone, Debug)]
struct RegisteredService {
    name: String,
    service_type: ServiceType,
    bind_addr: SocketAddr,
    /// Receive buffer size for UDP datagrams; ignored for TCP services.
    udp_buffer_size: usize,
}

// The entrypoint of running a server
pub async fn run_server(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
    update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    let config = match config.server {
        Some(config) => config,
        None => {
            return Err(anyhow!(
                "Try to run as a server, but the configuration is missing. Please add the `[server]` block"
            ));
        }
    };

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut server = Server::<TcpTransport>::from(config).await?;
            server.run(shutdown_rx, update_rx).await?;
        }
        TransportType::Tls => {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            {
                let mut server = Server::<TlsTransport>::from(config).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "native-tls", feature = "rustls")))]
            crate::common::helper::feature_neither_compile("native-tls", "rustls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut server = Server::<NoiseTransport>::from(config).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(feature = "noise"))]
            crate::common::helper::feature_not_compile("noise")
        }
        TransportType::Websocket => {
            #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
            {
                let mut server = Server::<WebsocketTransport>::from(config).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "websocket-native-tls", feature = "websocket-rustls")))]
            crate::common::helper::feature_neither_compile(
                "websocket-native-tls",
                "websocket-rustls",
            )
        }
    }

    Ok(())
}

// A hash map of ControlChannelHandles, indexed by ServiceDigest or Nonce
// See also MultiMap
type ControlChannelMap<T> = MultiMap<ServiceDigest, Nonce, ControlChannelHandle<T>>;

// Server holds all states of running a server
struct Server<T: Transport> {
    // `[server]` config
    config: Arc<ServerConfig>,

    // Collection of contorl channels, each carrying one registered service
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    // Wrapper around the transport layer
    transport: Arc<T>,
}

impl<T: 'static + Transport> Server<T> {
    // Create a server from `[server]`
    pub async fn from(config: ServerConfig) -> Result<Server<T>> {
        let config = Arc::new(config);
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        let transport = Arc::new(T::new(&config.transport)?);
        Ok(Server {
            config,
            control_channels,
            transport,
        })
    }

    // The entry point of Server
    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        // Listen at `server.bind_addr`
        let l = self
            .transport
            .bind(&self.config.bind_addr)
            .await
            .with_context(|| "Failed to listen at `server.bind_addr`")?;
        info!("Listening at {}", self.config.bind_addr);

        // Retry at least every 100ms
        let backoff_builder =
            ExponentialBuilder::default().with_max_delay(Duration::from_millis(100));
        let mut backoff = backoff_builder.build();

        // Wait for connections and shutdown signals
        loop {
            tokio::select! {
                // Wait for incoming control and data channels
                ret = self.transport.accept(&l) => {
                    match ret {
                        Err(err) => {
                            if should_retry_accept(&err) {
                                if let Some(d) = backoff.next() {
                                    error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                                    time::sleep(d).await;
                                } else {
                                    error!("Too many retries. Aborting...");
                                    break;
                                }
                            } else if let Some(e) = err.downcast_ref::<io::Error>() {
                                // Transient connection-level errors (ECONNABORTED, ECONNRESET, etc.)
                                // don't affect the listener, just ignore
                                debug!("Accept interrupted: {e}");
                            }
                            // Non-IO errors from the transport layer are silently ignored
                        }
                        Ok((conn, addr)) => {
                            backoff = backoff_builder.build();

                            // Do transport handshake with a timeout
                            match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), self.transport.handshake(conn)).await {
                                Ok(conn) => {
                                    match conn.with_context(|| "Failed to do transport handshake") {
                                        Ok(conn) => {
                                            let control_channels = self.control_channels.clone();
                                            let server_config = self.config.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, control_channels, server_config).await {
                                                    error!("{:#}", err);
                                                }
                                            }.instrument(info_span!("connection", %addr)));
                                        }, Err(e) => {
                                            error!("{:#}", e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Transport handshake timeout: {}", e);
                                }
                            }
                        }
                    }
                },
                // Wait for the shutdown signal
                _ = shutdown_rx.recv() => {
                    info!("Shutting down gracefully...");
                    break;
                },
                e = update_rx.recv() => {
                    if let Some(e) = e {
                        self.handle_hot_reload(e).await;
                    }
                }
            }
        }

        info!("Shutdown");

        Ok(())
    }

    async fn handle_hot_reload(&mut self, e: ConfigChange) {
        // The server owns no service configuration, so there is nothing to
        // hot-reload here: services come and go with their control channels.
        warn!("Ignored {:?} since running as a server", e);
    }
}

// Handle connections to `server.bind_addr`
async fn handle_connection<T: 'static + Transport>(
    mut conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    // Read hello
    let hello = read_hello(&mut conn).await?;
    match hello {
        ControlChannelHello(_, service_digest) => {
            do_control_channel_handshake(conn, control_channels, service_digest, server_config)
                .await?;
        }
        DataChannelHello(_, nonce) => {
            do_data_channel_handshake(conn, control_channels, nonce, false, server_config).await?;
        }
        #[cfg(feature = "multiplex")]
        Hello::DataChannelTunnelHello(_, nonce) => {
            do_data_channel_handshake(conn, control_channels, nonce, true, server_config).await?;
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

async fn do_control_channel_handshake<T: 'static + Transport>(
    mut conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    debug!("Handshaking a control channel");

    T::hint(&conn, SocketOpts::for_control_channel());

    // Generate a nonce
    let mut nonce = [0u8; HASH_WIDTH_IN_BYTES];
    let mut rng = rand::rngs::SysRng;
    rng.try_fill_bytes(&mut nonce)?;

    // Send hello
    let hello_send = Hello::ControlChannelHello(protocol::CURRENT_PROTO_VERSION, nonce);
    conn.write_all(&postcard::to_stdvec(&hello_send)?).await?;
    conn.flush().await?;

    // Validate the auth response against the global token
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
        bail!("Authentication failed");
    }
    write_and_flush(&mut conn, &postcard::to_stdvec(&Ack::Ok)?).await?;

    // Read the client's service registration
    let reg = read_registration(&mut conn).await?;
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
        write_register_result(&mut conn, &Ack::RegisterRejected(reason.clone())).await?;
        bail!("Service {}: {reason}", reg.name);
    }

    // Clamp the requested pool size to the server-wide maximum
    let pool_size = match server_config.max_pool_size {
        Some(max) => reg.pool_size.min(max),
        None => reg.pool_size,
    } as usize;

    let service = RegisteredService {
        name: reg.name.clone(),
        service_type: reg.service_type,
        bind_addr: reg.bind_addr,
        udp_buffer_size: reg.udp_buffer_size as usize,
    };

    // Take over any previous control channel for this service name *before*
    // binding: dropping the old handle starts the asynchronous teardown of
    // its listeners, and `bind_with_retry` absorbs the remaining race.
    {
        let mut h = control_channels.write().await;
        if h.remove1(&service_digest).is_some() {
            warn!(service = %reg.name, "Dropping previous control channel");
        }
    }

    // Bind the public endpoint eagerly so that conflicts are reported
    // precisely as a rejection instead of surfacing later as pool errors.
    let bound = match bind_with_retry(&service).await {
        Ok(b) => b,
        Err(e) => {
            let reason = format!("{:#}", e);
            warn!(service = %reg.name, "Registration failed: {reason}");
            write_register_result(&mut conn, &Ack::RegisterRejected(reason.clone())).await?;
            bail!("Service {}: {reason}", reg.name);
        }
    };

    write_register_result(&mut conn, &Ack::Ok).await?;

    let handle = ControlChannelHandle::new(
        conn,
        service,
        bound,
        server_config.heartbeat_interval,
        pool_size,
    );

    // Insert the new handle for this control channel
    let mut h = control_channels.write().await;
    let _ = h.insert(service_digest, session_key, handle);

    info!(service = %reg.name, "Control channel established");

    Ok(())
}

/// One end of a forwarded connection, as handed to the connection pool.
///
/// With the `multiplex` feature a data channel is either a plain transport
/// stream (no-mux mode) or one yamux stream of the client's tunnel.
#[cfg(not(feature = "multiplex"))]
type DataChannel<T> = <T as Transport>::Stream;

#[cfg(feature = "multiplex")]
enum DataChannel<T: Transport> {
    Raw(<T as Transport>::Stream),
    Mux(crate::transport::MuxStream),
}

/// Wrap a freshly handshaked transport stream as a pool-ready data channel.
#[cfg(not(feature = "multiplex"))]
fn new_data_channel<T: Transport>(stream: <T as Transport>::Stream) -> <T as Transport>::Stream {
    stream
}

#[cfg(feature = "multiplex")]
fn new_data_channel<T: Transport>(stream: <T as Transport>::Stream) -> DataChannel<T> {
    DataChannel::Raw(stream)
}

#[cfg(feature = "multiplex")]
impl<T: Transport> tokio::io::AsyncRead for DataChannel<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(feature = "multiplex")]
impl<T: Transport> tokio::io::AsyncWrite for DataChannel<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_flush(cx),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            DataChannel::Raw(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            DataChannel::Mux(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

async fn do_data_channel_handshake<T: 'static + Transport>(
    #[cfg_attr(not(feature = "multiplex"), allow(unused_mut))] mut conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    nonce: Nonce,
    #[cfg_attr(not(feature = "multiplex"), allow(unused_variables))] is_tunnel: bool,
    #[cfg_attr(not(feature = "multiplex"), allow(unused_variables))] server_config: Arc<
        ServerConfig,
    >,
) -> Result<()> {
    debug!("Handshaking a data channel");

    // Validate
    let control_channels_guard = control_channels.read().await;
    let handle = match control_channels_guard.get2(&nonce) {
        Some(handle) => handle.clone(),
        None => {
            warn!("Data channel has incorrect nonce");
            return Ok(());
        }
    };
    drop(control_channels_guard);

    T::hint(&conn, SocketOpts::for_service(None));

    #[cfg(feature = "multiplex")]
    {
        // The hello variant told us whether this connection is a plain data
        // channel or the opening of a multiplexed tunnel.
        if is_tunnel {
            // Confirm the upgrade before speaking yamux: the client waits for
            // this ack, so a stale nonce surfaces as a clean error there.
            write_and_flush(&mut conn, &postcard::to_stdvec(&Ack::Ok)?).await?;
            let config = crate::transport::multiplex::mux_config(
                server_config.mux_receive_window,
                server_config.mux_max_streams,
            );
            // Bridge: the tunnel driver produces raw mux streams, which are
            // wrapped and fed into the same pool channel as plain streams.
            let (bridge_tx, mut bridge_rx) = mpsc::channel::<crate::transport::MuxStream>(64);
            tokio::spawn(async move {
                crate::transport::multiplex::run_server_tunnel(conn, config, bridge_tx).await;
                debug!("Multiplexed data tunnel closed");
            });
            tokio::spawn(async move {
                while let Some(stream) = bridge_rx.recv().await {
                    if handle
                        .data_ch_tx
                        .send(DataChannel::Mux(stream))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            return Ok(());
        }
    }

    handle
        .data_ch_tx
        .send(new_data_channel::<T>(conn))
        .await
        .with_context(|| "Data channel for a stale control channel")?;
    Ok(())
}

pub struct ControlChannelHandle<T: Transport> {
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<DataChannel<T>>,
    // Keeps the data-channel request channel alive for as long as the handle
    // exists: the control channel loop exits when every sender is gone.
    _data_ch_req_tx: mpsc::UnboundedSender<bool>,
}

impl<T: Transport> Clone for ControlChannelHandle<T> {
    fn clone(&self) -> Self {
        ControlChannelHandle {
            _shutdown_tx: self._shutdown_tx.clone(),
            data_ch_tx: self.data_ch_tx.clone(),
            _data_ch_req_tx: self._data_ch_req_tx.clone(),
        }
    }
}

/// A public endpoint bound successfully for a registered service.
enum BoundEndpoint {
    Tcp(TcpListener),
    Udp(UdpSocket),
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

impl<T> ControlChannelHandle<T>
where
    T: 'static + Transport,
{
    // Create a control channel handle for an already-bound service: spawn
    // the connection pool task and the control channel handling task.
    #[instrument(name = "handle", skip_all, fields(service = %service.name))]
    fn new(
        conn: T::Stream,
        service: RegisteredService,
        bound: BoundEndpoint,
        heartbeat_interval: u64,
        pool_size: usize,
    ) -> ControlChannelHandle<T> {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Cache some data channels for later use
        for _i in 0..pool_size {
            if let Err(e) = data_ch_req_tx.send(true) {
                error!("Failed to request data channel {}", e);
            };
        }

        let shutdown_rx_clone = shutdown_tx.subscribe();
        // Socket options for visitor-facing connections: latency-friendly
        // defaults (nodelay + keepalive)
        let sock_opts = SocketOpts::for_service(None);
        match bound {
            BoundEndpoint::Tcp(listener) => {
                info!(service = %service.name, "Listening at {}", service.bind_addr);
                let data_ch_req_tx = data_ch_req_tx.clone();
                tokio::spawn(
                    async move {
                        if let Err(e) = run_tcp_connection_pool::<T, _>(
                            listener,
                            sock_opts,
                            data_ch_rx,
                            data_ch_req_tx,
                            shutdown_rx_clone,
                        )
                        .await
                        .with_context(|| "Failed to run TCP connection pool")
                        {
                            error!("{:#}", e);
                        }
                    }
                    .instrument(Span::current()),
                );
            }
            BoundEndpoint::Udp(socket) => {
                info!(service = %service.name, "Listening at {}", service.bind_addr);
                let buffer_size = service.udp_buffer_size;
                tokio::spawn(
                    async move {
                        if let Err(e) = run_udp_connection_pool::<T, _>(
                            Arc::new(socket),
                            buffer_size,
                            data_ch_rx,
                            shutdown_rx_clone,
                        )
                        .await
                        .with_context(|| "Failed to run UDP connection pool")
                        {
                            error!("{:#}", e);
                        }
                    }
                    .instrument(Span::current()),
                );
            }
        }

        // Create the control channel
        let ch = ControlChannel::<T> {
            conn,
            shutdown_rx,
            data_ch_req_rx,
            heartbeat_interval,
        };

        // Run the control channel
        tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            _data_ch_req_tx: data_ch_req_tx,
        }
    }
}

// Control channel, using T as the transport layer. P is TcpStream or UdpTraffic
struct ControlChannel<T: Transport> {
    conn: T::Stream,                               // The connection of control channel
    shutdown_rx: broadcast::Receiver<bool>,        // Receives the shutdown signal
    data_ch_req_rx: mpsc::UnboundedReceiver<bool>, // Receives visitor connections
    heartbeat_interval: u64,                       // Application-layer heartbeat interval in secs
}

impl<T: Transport> ControlChannel<T> {
    async fn write_and_flush(&mut self, data: &[u8]) -> Result<()> {
        write_and_flush(&mut self.conn, data)
            .await
            .with_context(|| "Failed to write control cmds")?;
        Ok(())
    }
    // Run a control channel
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        let create_ch_cmd = postcard::to_stdvec(&ControlChannelCmd::CreateDataChannel)?;
        let heartbeat = postcard::to_stdvec(&ControlChannelCmd::HeartBeat)?;

        // Wait for data channel requests and the shutdown signal
        loop {
            tokio::select! {
                val = self.data_ch_req_rx.recv() => {
                    match val {
                        Some(_) => {
                            if let Err(e) = self.write_and_flush(&create_ch_cmd).await {
                                error!("{:#}", e);
                                break;
                            }
                        }
                        None => {
                            break;
                        }
                    }
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
                            if let Err(e) = self.write_and_flush(&heartbeat).await {
                                error!("{:#}", e);
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
#[instrument(skip_all)]
async fn run_tcp_connection_pool<T, C>(
    l: TcpListener,
    sock_opts: SocketOpts,
    mut data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()>
where
    T: Transport,
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    info!("Listening at {}", l.local_addr()?);
    let cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardTcp)?;

    // Retry at least every 1s
    let backoff_builder = ExponentialBuilder::default().with_max_delay(Duration::from_secs(1));
    let mut backoff = backoff_builder.build();

    'pool: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            val = l.accept() => match val {
                Err(e) => {
                    // `l` is a TCP listener so this must be a IO error
                    // Possibly a EMFILE. So sleep for a while
                    error!("{}. Sleep for a while", e);
                    if let Some(d) = backoff.next() {
                        time::sleep(d).await;
                    } else {
                        // This branch will never be reached for current backoff policy
                        error!("Too many retries. Aborting...");
                        break;
                    }
                }
                Ok((mut incoming, addr)) => {
                    // For every visitor, request to create a data channel
                    if data_ch_req_tx.send(true).with_context(|| "Failed to send data chan create request").is_err() {
                        // An error indicates the control channel is broken
                        // So break the loop
                        break 'pool;
                    }

                    backoff = backoff_builder.build();

                    debug!("New visitor from {}", addr);

                    // The visitor socket gets the same latency-friendly
                    // defaults as the rest of the forwarding path
                    sock_opts.apply(&incoming);

                    // Pair the visitor with a data channel. A broken channel
                    // (e.g. stale pooled one) is discarded and replaced.
                    loop {
                        let Some(mut ch) = data_ch_rx.recv().await else {
                            break 'pool;
                        };
                        if write_and_flush(&mut ch, &cmd).await.is_ok() {
                            tokio::spawn(async move {
                                let _ = copy_bidirectional_with_sizes(
                                    &mut ch,
                                    &mut incoming,
                                    TCP_COPY_BUFFER_SIZE,
                                    TCP_COPY_BUFFER_SIZE,
                                )
                                .await;
                            });
                            break;
                        } else {
                            // Current data channel is broken. Request for a new one
                            if data_ch_req_tx.send(true).is_err() {
                                break 'pool;
                            }
                        }
                    }
                }
            },
        }
    }

    info!("Shutdown");
    Ok(())
}

#[instrument(skip_all)]
async fn run_udp_connection_pool<T, C>(
    l: Arc<UdpSocket>,
    buffer_size: usize,
    mut data_ch_rx: mpsc::Receiver<C>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()>
where
    T: Transport,
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    info!("Listening at {}", l.local_addr()?);

    let cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardUdp)?;

    let mut set = tokio::task::JoinSet::new();

    // Spawn one worker per data channel. Multiple workers concurrently
    // read from the same UDP socket, distributing traffic across channels.
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            maybe_chan = data_ch_rx.recv() => {
                match maybe_chan {
                    Some(mut conn) => {
                        if let Err(e) = write_and_flush(&mut conn, &cmd).await {
                            error!("Failed to init UDP channel: {:#}", e);
                            continue;
                        }
                        let l = Arc::clone(&l);
                        let shutdown = shutdown_rx.resubscribe();
                        set.spawn(async move {
                            if let Err(e) =
                                udp_forward_worker(l, conn, shutdown, buffer_size).await
                            {
                                error!("UDP worker exited: {:#}", e);
                            }
                        });
                    }
                    None => break,
                }
            }
            Some(result) = set.join_next() => {
                if let Err(e) = result {
                    error!("UDP worker panicked: {:?}", e);
                }
            }
        }
    }

    // Drop the socket and task set immediately so the port is released
    // before a replacement pool tries to bind.
    drop(l);
    drop(set);

    debug!("UDP pool dropped");
    Ok(())
}

async fn udp_forward_worker<C>(
    l: Arc<UdpSocket>,
    mut conn: C,
    mut shutdown_rx: broadcast::Receiver<bool>,
    buffer_size: usize,
) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; buffer_size];
    // Scratch buffers reused across datagrams so the hot path allocates nothing.
    let mut tx_scratch = BytesMut::with_capacity(MAX_UDP_HEADER_LEN + buffer_size);
    let mut rx_scratch = BytesMut::with_capacity(MAX_UDP_HEADER_LEN + buffer_size);
    loop {
        tokio::select! {
            // Forward inbound traffic to the client
            val = l.recv_from(&mut buf) => {
                let (n, from) = val?;
                UdpTraffic::write_frame(&mut conn, &mut tx_scratch, from, &buf[..n]).await?;
            }
            // Forward outbound traffic from the client to the visitor
            hdr_len = conn.read_u8() => {
                // `Ok(None)` means an oversized packet was dropped; keep going.
                if let Some((from, len)) =
                    UdpTraffic::read_slice(&mut conn, hdr_len?, &mut rx_scratch, buffer_size).await?
                {
                    l.send_to(&rx_scratch[..len], from).await?;
                }
            }
            _ = shutdown_rx.recv() => {
                break;
            }
        }
    }
    Ok(())
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
            Some(24) | Some(23) | Some(12) | Some(105) // EMFILE, ENFILE, ENOMEM, ENOBUFS
        )
    } else {
        // On non-Unix, treat all IO errors as potentially transient
        io_err.kind() == io::ErrorKind::OutOfMemory || io_err.kind() == io::ErrorKind::StorageFull
    }
}
