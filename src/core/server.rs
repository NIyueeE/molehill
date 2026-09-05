use crate::common::constants::{DEFAULT_UDP_SENDQ_SIZE, TCP_COPY_BUFFER_SIZE, UDP_ROUTE_TTL_SECS};
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
use bytes::{Bytes, BytesMut};

use rand::TryRng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use std::time::Instant;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, copy_bidirectional_with_sizes};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc::error::TrySendError;
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
    let Some(config) = config.server else {
        return Err(anyhow!(
            "Try to run as a server, but the configuration is missing. Please add the `[server]` block"
        ));
    };

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut server = Server::<TcpTransport>::from(config)?;
            server.run(shutdown_rx, update_rx).await?;
        }
        TransportType::Tls => {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            {
                let mut server = Server::<TlsTransport>::from(config)?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "native-tls", feature = "rustls")))]
            crate::common::helper::feature_neither_compile("native-tls", "rustls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut server = Server::<NoiseTransport>::from(config)?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(feature = "noise"))]
            crate::common::helper::feature_not_compile("noise")
        }
        TransportType::Websocket => {
            #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
            {
                let mut server = Server::<WebsocketTransport>::from(config)?;
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
    pub fn from(config: ServerConfig) -> Result<Server<T>> {
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
                        // The server owns no service configuration, so there
                        // is nothing to hot-reload here: services come and go
                        // with their control channels.
                        warn!("Ignored {e:?} since running as a server");
                    }
                }
            }
        }

        info!("Shutdown");

        Ok(())
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
            let reason = format!("{e:#}");
            warn!(service = %reg.name, "Registration failed: {reason}");
            write_register_result(&mut conn, &Ack::RegisterRejected(reason.clone())).await?;
            bail!("Service {}: {reason}", reg.name);
        }
    };

    write_register_result(&mut conn, &Ack::Ok).await?;

    let handle = ControlChannelHandle::new(
        conn,
        &service,
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
    let Some(handle) = control_channels_guard.get2(&nonce).cloned() else {
        warn!("Data channel has incorrect nonce");
        return Ok(());
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

#[expect(
    clippy::struct_field_names,
    reason = "the handle deliberately holds the three channel senders whose \
              lifetime keeps the control channel and its pools alive"
)]
pub struct ControlChannelHandle<T: Transport> {
    // Shutdown the control channel by dropping it
    shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<DataChannel<T>>,
    // Keeps the data-channel request channel alive for as long as the handle
    // exists: the control channel loop exits when every sender is gone.
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
}

impl<T: Transport> Clone for ControlChannelHandle<T> {
    fn clone(&self) -> Self {
        ControlChannelHandle {
            shutdown_tx: self.shutdown_tx.clone(),
            data_ch_tx: self.data_ch_tx.clone(),
            data_ch_req_tx: self.data_ch_req_tx.clone(),
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
        service: &RegisteredService,
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
            }
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
                let data_ch_req_tx = data_ch_req_tx.clone();
                tokio::spawn(
                    async move {
                        if let Err(e) = run_udp_connection_pool::<T, _>(
                            Arc::new(socket),
                            buffer_size,
                            data_ch_rx,
                            data_ch_req_tx,
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
            shutdown_tx,
            data_ch_tx,
            data_ch_req_tx,
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
                () = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
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
                        }
                        // Current data channel is broken. Request for a new one
                        if data_ch_req_tx.send(true).is_err() {
                            break 'pool;
                        }
                    }
                }
            },
        }
    }

    info!("Shutdown");
    Ok(())
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

/// Cleans up after a UDP worker task exits: removes its queue from the
/// routing table and asks the control channel for a replacement data channel
/// so the pool keeps its size. Runs on normal exits and on panics (the guard
/// drops on unwind).
struct UdpWorkerGuard {
    id: usize,
    workers: Arc<UdpWorkerMap>,
    req_tx: mpsc::UnboundedSender<bool>,
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
            let _ = self.req_tx.send(true);
        }
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
async fn run_udp_connection_pool<T, C>(
    l: Arc<UdpSocket>,
    buffer_size: usize,
    mut data_ch_rx: mpsc::Receiver<C>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()>
where
    T: Transport,
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    info!("Listening at {}", l.local_addr()?);

    let cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardUdp)?;

    // Live data channels, keyed by a monotonically increasing worker id.
    // Workers remove their own entry on exit (via the guard) and request a
    // replacement, so the pool keeps its size for the session's lifetime.
    let workers: Arc<UdpWorkerMap> = Arc::new(Mutex::new(HashMap::new()));
    // The affinity table: peer address -> assigned data channel.
    let routes: Arc<UdpRouteMap> = Arc::new(Mutex::new(HashMap::new()));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let mut next_worker = 0usize;
    // One socket reader: `recv_from` is the single entry point for all
    // visitors, and the affinity table below decides the channel. A single
    // reader also means one slow worker can never stall other peers.
    let mut buf = vec![0u8; buffer_size];

    let mut sweep = time::interval(Duration::from_secs(UDP_ROUTE_TTL_SECS));
    // The first tick of an interval completes immediately; consume it.
    sweep.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                shutting_down.store(true, Ordering::Relaxed);
                break;
            }
            maybe_chan = data_ch_rx.recv() => {
                let Some(mut conn) = maybe_chan else {
                    shutting_down.store(true, Ordering::Relaxed);
                    break;
                };
                if let Err(e) = write_and_flush(&mut conn, &cmd).await {
                    error!("Failed to init UDP channel: {:#}", e);
                    continue;
                }
                let (tx, rx) = mpsc::channel(DEFAULT_UDP_SENDQ_SIZE);
                let id = next_worker;
                next_worker += 1;
                workers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(id, tx);
                let guard = UdpWorkerGuard {
                    id,
                    workers: Arc::clone(&workers),
                    req_tx: data_ch_req_tx.clone(),
                    shutting_down: Arc::clone(&shutting_down),
                };
                tokio::spawn(udp_forward_worker(
                    Arc::clone(&l),
                    conn,
                    rx,
                    shutdown_rx.resubscribe(),
                    buffer_size,
                    guard,
                ));
            }
            recv = l.recv_from(&mut buf) => match recv {
                Ok((n, from)) => {
                    route_udp_datagram(
                        &workers,
                        &routes,
                        &mut next_worker,
                        from,
                        Bytes::copy_from_slice(&buf[..n]),
                    );
                }
                // Linux surfaces a stale ICMP error (the recipient of an
                // earlier datagram has gone) as ECONNREFUSED on the next
                // recv; it is transient and must not tear down the pool.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    debug!("Transient UDP recv error: {e}");
                }
                Err(e) => {
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
                routes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|_, route| {
                        route.last_seen.elapsed() < Duration::from_secs(UDP_ROUTE_TTL_SECS)
                    });
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
) {
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
                return;
            }
            Err(TrySendError::Full(_)) => {
                debug!("UDP worker queue full, dropping a datagram from {from}");
                return;
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
        return;
    }
    let idx = *next_worker % workers.len();
    *next_worker = next_worker.wrapping_add(1);
    let Some((id, tx)) = workers.iter().nth(idx).map(|(id, tx)| (*id, tx.clone())) else {
        return; // Unreachable: the map is non-empty.
    };
    debug!("UDP peer {from} assigned to data channel {id}");
    routes.insert(
        from,
        UdpRoute {
            worker: id,
            last_seen: now,
        },
    );
    if let Err(e) = tx.try_send((from, data)) {
        debug!("Dropped a datagram from {from}: {e}");
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
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
}
