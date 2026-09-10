//! yamux-based data channel multiplexing (`multiplex` feature).
//!
//! One extra physical connection ("tunnel") is dialed by the client right
//! after its registration succeeds. Both ends upgrade it to a yamux session;
//! afterwards every forwarded data channel is a cheap stream instead of a
//! full TCP + crypto handshake:
//!
//! ```text
//! client                          server
//!   │ DataChannelTunnelHello(nonce) ►   validated like a plain data channel
//!   │ ◄────────── Ack::Ok ──────────
//!   ╞══ yamux session (Mode::Client / Mode::Server) ══╗
//!   │ ── open_stream ──►  stream accepted ────────────┤ … pooled/paired
//! ```
//!
//! The decision belongs to the client alone (`[client].mux`): the server
//! adapts per connection based on which hello variant arrives, so mixed
//! deployments work without any coordination.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;

use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::debug;
use yamux::{Config, Connection, Mode};

/// A multiplexed stream adapted to tokio's IO traits.
pub type MuxStream = Compat<yamux::Stream>;

/// Announce a freshly opened outbound stream, then deliver it to the caller.
///
/// rust-yamux opens outbound streams lazily: the SYN flag is piggybacked on
/// the first outbound frame, and a read-only consumer never produces one.
/// Our data-channel protocol is server-speaks-first (`StartForward*`), so a
/// freshly pooled stream starts by reading — without this zero-length write
/// it would never be announced to the server and both ends would wait for
/// each other forever.
///
/// This is a *future* rather than an await inside the driver loop: the
/// driver polls it alongside the connection state machine, so a
/// backpressured socket completes the announcement whenever it becomes
/// writable without ever stalling the tunnel's inbound processing.
struct SynAnnounce {
    stream: Option<MuxStream>,
    reply: Option<oneshot::Sender<Result<MuxStream, yamux::ConnectionError>>>,
}

impl std::future::Future for SynAnnounce {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        // Take the stream out; when it is already gone the announcement was
        // delivered, so stay total for a misused second poll.
        let Some(mut stream) = this.stream.take() else {
            return Poll::Ready(());
        };
        match tokio::io::AsyncWrite::poll_write(Pin::new(&mut stream), cx, &[]) {
            Poll::Ready(Ok(_)) => {
                if let Some(reply) = this.reply.take() {
                    let _ = reply.send(Ok(stream));
                }
                Poll::Ready(())
            }
            Poll::Ready(Err(e)) => {
                debug!(error = %e, "Failed to announce outbound multiplexed stream");
                if let Some(reply) = this.reply.take() {
                    let _ = reply.send(Err(yamux::ConnectionError::Closed));
                }
                Poll::Ready(())
            }
            Poll::Pending => {
                // Park the stream again for the next poll.
                this.stream = Some(stream);
                Poll::Pending
            }
        }
    }
}

/// Build the session configuration for every tunnel: a 64 MiB total
/// receive window (bounded loss backlog — yamux's own 1 GiB default
/// accumulates without bound under loss) with 32 streams, each guaranteed
/// the 256 KiB default credit, leaving 56 MiB for the auto-tuner.
///
/// The two values are coupled by an upstream invariant (`window >=
/// streams * 256 KiB` asserted on every setter), so they are fixed internal
/// constants rather than config knobs: tuning them independently measured
/// 30x regressions (streams that swallow the window pin every stream at
/// 256 KiB) and 30-65% throughput drops on delayed links (windows too
/// small for the auto-tuner). The frame split size stays at yamux's
/// 16 KiB default: larger frames (64 KiB) measured faster on loopback but
/// 30-60% slower under round-trip delay.
pub(crate) fn mux_config() -> Config {
    use crate::common::constants::{DEFAULT_MUX_MAX_STREAMS, DEFAULT_MUX_RECEIVE_WINDOW};

    let mut config = Config::default();
    // Setter order keeps the upstream assertion (`window >= 256 KiB *
    // streams`) satisfied at every step: lower the stream count under the
    // default 1 GiB window first, then bound the window under the final
    // stream count.
    config.set_max_num_streams(DEFAULT_MUX_MAX_STREAMS);
    config.set_max_connection_receive_window(Some(DEFAULT_MUX_RECEIVE_WINDOW));
    config
}

/// Handle to a client-side tunnel: allows opening data channels as streams.
#[derive(Clone)]
pub struct ClientTunnel {
    open_tx: mpsc::Sender<oneshot::Sender<Result<MuxStream, yamux::ConnectionError>>>,
}

impl ClientTunnel {
    /// Spawn the driver task for a client-mode session.
    ///
    /// The returned handle stays valid until `shutdown` is dropped; the
    /// driver also exits when the underlying connection dies.
    pub fn start<I>(
        io: I,
        config: Config,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> ClientTunnel
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (open_tx, mut open_rx) =
            mpsc::channel::<oneshot::Sender<Result<MuxStream, yamux::ConnectionError>>>(16);

        tokio::spawn(async move {
            // yamux speaks the `futures-io` trait family; adapt the tokio
            // socket once at the boundary.
            let mut conn = Connection::new(io.compat(), config, Mode::Client);
            let mut waiting: Option<oneshot::Sender<Result<MuxStream, yamux::ConnectionError>>> =
                None;
            // A freshly opened stream whose SYN announcement is in flight.
            // The announcement is driven inside the poll closure (via the
            // shared mutex, so the closure never captures it mutably),
            // keeping the driver event-driven: a backpressured socket must
            // not stall inbound frame processing for the whole tunnel. The
            // lock is only ever held by this one driver task, so it never
            // contends.
            let announce: std::sync::Mutex<Option<SynAnnounce>> = std::sync::Mutex::new(None);

            loop {
                enum Step {
                    /// The pending announcement settled; its reply was
                    /// already delivered to the waiting caller.
                    Announced,
                    Opened(Result<yamux::Stream, yamux::ConnectionError>),
                    Inbound(Option<Result<yamux::Stream, yamux::ConnectionError>>),
                }

                let step = poll_fn(|cx| {
                    // 1. Drive the pending SYN announcement first; while it
                    //    stays pending the inbound poll below keeps
                    //    registering wakers, so data keeps flowing under
                    //    socket backpressure.
                    {
                        let mut slot = announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(f) = slot.as_mut()
                            && let Poll::Ready(()) = std::pin::Pin::new(&mut *f).poll(cx)
                        {
                            *slot = None;
                            return Poll::Ready(Step::Announced);
                        }
                    }
                    // 2. Serve the next outbound open — one at a time, like
                    //    before: only when a request is pending and no
                    //    announcement is in flight.
                    if waiting.is_some()
                        && announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_none()
                        && let Poll::Ready(r) = conn.poll_new_outbound(cx)
                    {
                        return Poll::Ready(Step::Opened(r));
                    }
                    // 3. Inbound flows in every state: the server never
                    //    opens streams toward us, so drop any that appear;
                    //    errors and end of stream close the tunnel.
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(v) => Poll::Ready(Step::Inbound(v)),
                        Poll::Pending => Poll::Pending,
                    }
                });

                tokio::select! {
                    _ = shutdown.changed() => break,
                    step = step => match step {
                        Step::Announced => {}
                        Step::Opened(result) => {
                            if let Some(reply) = waiting.take() {
                                match result {
                                    Ok(stream) => {
                                        *announce
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            Some(SynAnnounce {
                                                stream: Some(stream.compat()),
                                                reply: Some(reply),
                                            });
                                    }
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                }
                            }
                        }
                        Step::Inbound(Some(Ok(_stream))) => {}
                        Step::Inbound(_) => break,
                    },
                    req = open_rx.recv(),
                    if waiting.is_none()
                        && announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_none() =>
                    {
                        match req {
                            Some(reply) => waiting = Some(reply),
                            None => break, // all handles dropped
                        }
                    }
                }
            }
        });

        ClientTunnel { open_tx }
    }

    /// Open a new data channel as a multiplexed stream.
    pub async fn open_stream(&self) -> Result<MuxStream, yamux::ConnectionError> {
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .await
            .map_err(|_| yamux::ConnectionError::Closed)?;
        rx.await.map_err(|_| yamux::ConnectionError::Closed)?
    }
}

/// A pool of parallel client tunnels (arm 1 of the transport comparison:
/// N physical connections instead of one).
///
/// `open_stream` spreads streams round-robin over the tunnels. A tunnel whose
/// driver died returns `Closed` immediately, so the pool transparently tries
/// the next tunnel; only when every tunnel is dead does the open fail (the
/// control channel's heartbeat then triggers the usual full reconnect, which
/// re-establishes the whole pool).
///
/// Clones share one round-robin counter, so placement stays balanced across
/// concurrently opened data channels.
#[derive(Clone)]
pub struct TunnelPool {
    inner: std::sync::Arc<TunnelPoolInner>,
}

struct TunnelPoolInner {
    tunnels: Vec<ClientTunnel>,
    next: std::sync::atomic::AtomicUsize,
}

impl TunnelPool {
    /// Wrap the established tunnels. `tunnels` must not be empty.
    pub fn new(tunnels: Vec<ClientTunnel>) -> TunnelPool {
        TunnelPool {
            inner: std::sync::Arc::new(TunnelPoolInner {
                tunnels,
                next: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Open a data-channel stream on the next tunnel, falling through to the
    /// remaining ones if a tunnel is already closed.
    pub async fn open_stream(&self) -> Result<MuxStream, yamux::ConnectionError> {
        use std::sync::atomic::Ordering;

        let n = self.inner.tunnels.len();
        // Wrapping is fine: the index is only used modulo `n`.
        let start = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let mut last_err = yamux::ConnectionError::Closed;
        for i in 0..n {
            match self.inner.tunnels[(start + i) % n].open_stream().await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    debug!("Tunnel {} of {n} refused a stream: {e}", (start + i) % n);
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }
}

/// Spawn the driver task for a server-mode session, forwarding every inbound
/// stream (i.e. every requested data channel) into `tx`.
pub async fn run_server_tunnel<I>(io: I, config: Config, tx: mpsc::Sender<MuxStream>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    debug!("server tunnel driver started");
    let mut conn = Connection::new(io.compat(), config, Mode::Server);
    while let Some(result) = poll_fn(|cx| conn.poll_next_inbound(cx)).await {
        match result {
            Ok(stream) => {
                debug!("server tunnel accepted an inbound stream");
                if tx.send(stream.compat()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn client_opens_streams_server_receives() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

        // Open a stream and push some bytes through
        let mut stream = tunnel.open_stream().await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();

        let mut server_stream = inbound_rx.recv().await.unwrap();
        let mut buf = [0u8; 4];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // Reply on the same stream: full duplex
        server_stream.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        drop(tunnel);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_first_stream_is_announced_to_the_server() {
        // Regression test for the 0.7.0 data-path stall: production data
        // channels are server-speaks-first, so the client starts by READING
        // the freshly opened stream. yamux attaches the stream's SYN flag to
        // its first outbound frame; without an explicit empty-write kick the
        // SYN is never emitted and neither peer ever sees the stream.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

        let mut stream = tunnel.open_stream().await.unwrap();

        // The server receives the stream even though the client has not
        // written any payload, then speaks first like the pool pairing code.
        let server_side = tokio::spawn(async move {
            let mut server_stream =
                tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
                    .await
                    .expect("server did not receive the read-only stream")
                    .expect("server tunnel closed");
            server_stream.write_all(b"go").await.unwrap();
            let mut buf = [0u8; 4];
            server_stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        });

        let mut cmd = [0u8; 2];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_exact(&mut cmd),
        )
        .await
        .expect("server command never arrived")
        .unwrap();
        assert_eq!(&cmd, b"go");

        stream.write_all(b"ping").await.unwrap();
        server_side.await.unwrap();

        drop(tunnel);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn client_opens_streams_over_real_tcp() {
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            run_server_tunnel(sock, mux_config(), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Open several streams back-to-back before reading anything
        let mut streams = Vec::new();
        for i in 0..4 {
            let mut s = tunnel.open_stream().await.unwrap();
            s.write_all(format!("msg{i}").as_bytes()).await.unwrap();
            streams.push(s);
        }

        // Server side mirrors production pooling: write a command INTO each
        // accepted stream (like StartForwardTcp) BEFORE the client reads it.
        for i in 0..4 {
            let mut s = inbound_rx.recv().await.unwrap();
            s.write_all(format!("cmd{i}").as_bytes()).await.unwrap();
            // hold the stream alive like the pool pairing task would
            tokio::spawn(async move {
                let mut echo = [0u8; 8];
                // keep reading so window updates flow
                loop {
                    match s.read(&mut echo).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => eprintln!("[srv-stream] got {n} bytes"),
                    }
                }
            });
        }

        // Client reads the commands back
        for (i, stream) in streams.iter_mut().enumerate() {
            let mut buf = [0u8; 5];
            match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                .await
            {
                Err(_) => eprintln!("[cli] cmd{i}: READ PENDING (waker lost?)"),
                Ok(Ok(0)) => eprintln!("[cli] cmd{i}: EOF"),
                Ok(Ok(n)) => eprintln!("[cli] cmd{i}: got {n} bytes"),
                Ok(Err(e)) => eprintln!("[cli] cmd{i}: err {e}"),
            }
        }
        drop(streams);
        drop(tunnel);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rapid_opens_without_immediate_consumer() {
        // Mimic production pool pre-creation: many open requests arrive
        // back-to-back while the server-side consumer has not read anything.
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, inbound_rx) = mpsc::channel(4);

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            run_server_tunnel(sock, mux_config(), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Fire 12 opens concurrently without awaiting them in order.
        let mut handles = Vec::new();
        for i in 0..12 {
            let t = tunnel.clone();
            handles.push(tokio::spawn(async move {
                let mut s =
                    tokio::time::timeout(std::time::Duration::from_secs(3), t.open_stream())
                        .await
                        .expect("open_stream timed out")
                        .expect("open failed");
                s.write_all(format!("m{i}").as_bytes()).await.unwrap();
            }));
        }
        // Give the client driver time to wedge if it is going to
        for check in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let done = handles.iter().filter(|h| h.is_finished()).count();
            eprintln!("[rapid] {done}/12 after {}ms", (check + 1) * 200);
            if done == 12 {
                break;
            }
        }
        let done = handles.iter().filter(|h| h.is_finished()).count();
        assert_eq!(done, 12, "driver wedged");

        // Dropping the receiver closes the tunnel from the consumer side;
        // the server driver must then exit cleanly.
        drop(inbound_rx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("server driver did not exit after consumer dropped");
    }

    #[tokio::test]
    async fn open_after_idle_period() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};

        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::level_filters::LevelFilter::TRACE)
            .with_ansi(false)
            .try_init();
        // Production failure signature: initial pooled opens succeed, then
        // after an idle period a NEW open never completes.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(64);

        let mut client_sock = TcpStream::connect(addr).await.unwrap();
        let mut server_sock = {
            let (s, _) = listener.accept().await.unwrap();
            s
        };
        // Mimic production: hello + ack bytes flow on the socket BEFORE the
        // yamux sessions are constructed.
        client_sock.write_all(&[0u8; 34]).await.unwrap();
        let mut hello = [0u8; 34];
        server_sock.read_exact(&mut hello).await.unwrap();
        let mut ack = [0u8; 1];
        server_sock.write_all(&ack).await.unwrap();
        client_sock.read_exact(&mut ack).await.unwrap();

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            run_server_tunnel(server_sock, mux_config(), inbound_tx).await;
        });

        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Phase 1: burst of 8 opens like the pool pre-creation
        let mut first = Vec::new();
        for i in 0..8 {
            let mut s = tunnel.open_stream().await.expect("phase1 open failed");
            s.write_all(format!("msg{i}").as_bytes()).await.unwrap();
            first.push(s);
        }
        // Drain server side fully
        for i in 0..8 {
            eprintln!("[test] waiting for server stream {i}");
            let mut srv_stream =
                tokio::time::timeout(std::time::Duration::from_secs(3), inbound_rx.recv())
                    .await
                    .expect("recv timed out")
                    .unwrap();
            eprintln!(
                "[test] got stream {i} debug={:?} reading data",
                srv_stream.get_ref()
            );
            let mut buf = [0u8; 4];
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                srv_stream.read_exact(&mut buf),
            )
            .await
            .expect("read_exact timed out")
            .unwrap();
            eprintln!(
                "[test] stream {i} data ok: {}",
                std::str::from_utf8(&buf).unwrap()
            );
            assert_eq!(&buf, format!("msg{i}").as_bytes());
        }

        // Phase 2: go idle, then try one more open
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut s = tokio::time::timeout(std::time::Duration::from_secs(2), tunnel.open_stream())
            .await
            .expect("open after idle TIMED OUT")
            .expect("open failed");
        s.write_all(b"late").await.unwrap();
        let mut srv_stream = inbound_rx.recv().await.unwrap();
        let mut buf = [0u8; 4];
        srv_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"late");

        drop(first);
        drop(tunnel);
        server.await.unwrap();
    }

    #[test]
    fn mux_config_is_total() {
        // The config is built from fixed internal constants; keep a guard
        // so a change to those constants cannot make the builder panic.
        let _ = mux_config();
    }

    #[test]
    fn default_window_keeps_auto_tunable_credit() {
        // Regression guard: yamux reserves `max_streams * 256 KiB` of the
        // connection window as guaranteed per-stream credit; the auto-tuner
        // may only allocate the remainder. A stream count that swallows the
        // whole window pins every stream at 256 KiB (measured: 0.1 Gbps at
        // 10 ms RTT, a ~30x drop from the tuned window).
        use crate::common::constants::{DEFAULT_MUX_MAX_STREAMS, DEFAULT_MUX_RECEIVE_WINDOW};

        const CREDIT: usize = 256 * 1024; // yamux DEFAULT_CREDIT
        let reserved = DEFAULT_MUX_MAX_STREAMS * CREDIT;
        assert!(
            DEFAULT_MUX_RECEIVE_WINDOW > reserved,
            "the credit reservation must not swallow the whole window"
        );
        assert!(
            DEFAULT_MUX_RECEIVE_WINDOW - reserved >= DEFAULT_MUX_RECEIVE_WINDOW / 2,
            "at least half the window must stay auto-tunable"
        );
        let _ = mux_config();
    }

    #[tokio::test]
    async fn pool_spreads_streams_over_tunnels() {
        // Two tunnels, four opens: round-robin must place two streams on
        // each tunnel, and every stream must survive a full round trip.
        const TUNNELS: usize = 2;
        const OPENS: usize = 4;

        let mut tunnels = Vec::new();
        let mut per_tunnel_counts = Vec::new();
        // One shutdown sender per tunnel keeps every driver alive; dropping
        // them at the end of the test stops all drivers.
        let shutdown_senders: Vec<_> = (0..TUNNELS)
            .map(|_| tokio::sync::watch::channel(false).0)
            .collect();

        for sender in &shutdown_senders {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (inbound_tx, mut inbound_rx) = mpsc::channel::<MuxStream>(8);
            let counter = tokio::spawn(async move {
                let mut count = 0usize;
                while let Some(mut stream) = inbound_rx.recv().await {
                    count += 1;
                    let mut buf = [0u8; 4];
                    stream.read_exact(&mut buf).await.unwrap();
                    assert_eq!(&buf, format!("msg{count}").as_bytes());
                    // Round trip: prove the stream is usable both ways before
                    // the client drops it.
                    stream.write_all(b"ok").await.unwrap();
                    stream.flush().await.unwrap();
                }
                count
            });
            let server = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));
            tunnels.push(ClientTunnel::start(
                client_io,
                mux_config(),
                sender.subscribe(),
            ));
            per_tunnel_counts.push((counter, server));
        }

        let pool = TunnelPool::new(tunnels);

        for i in 0..OPENS {
            let mut s = pool.open_stream().await.unwrap();
            // Round-robin sends open `i` to tunnel `i % TUNNELS`, where it is
            // the `(i / TUNNELS + 1)`-th stream to arrive.
            s.write_all(format!("msg{}", i / TUNNELS + 1).as_bytes())
                .await
                .unwrap();
            s.flush().await.unwrap();
            let mut ok = [0u8; 2];
            s.read_exact(&mut ok).await.unwrap();
            assert_eq!(&ok, b"ok");
        }
        drop(pool);
        drop(shutdown_senders);

        for (counter, server) in per_tunnel_counts {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
            let count = tokio::time::timeout(std::time::Duration::from_secs(3), counter)
                .await
                .expect("tunnel counter did not finish")
                .unwrap();
            assert_eq!(count, OPENS / TUNNELS, "streams were not spread evenly");
        }
    }

    #[tokio::test]
    async fn pool_skips_dead_tunnel() {
        // Kill one tunnel's driver (drop its shutdown sender): opens must
        // fall through to the surviving tunnel instead of failing.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_io2, server_io2) = tokio::io::duplex(64 * 1024);

        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);
        let s1 = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx1));
        let s2 = tokio::spawn(run_server_tunnel(server_io2, mux_config(), tx2));

        let (shutdown1, shutdown1_rx) = tokio::sync::watch::channel(false);
        let (_shutdown2, shutdown2_rx) = tokio::sync::watch::channel(false);
        let t1 = ClientTunnel::start(client_io, mux_config(), shutdown1_rx);
        let t2 = ClientTunnel::start(client_io2, mux_config(), shutdown2_rx);
        let pool = TunnelPool::new(vec![t1, t2]);

        // Sanity: both tunnels work.
        let mut a = pool.open_stream().await.unwrap();
        a.write_all(b"aaaa").await.unwrap();
        let mut b = pool.open_stream().await.unwrap();
        b.write_all(b"bbbb").await.unwrap();
        let mut got = Vec::new();
        for rx in [&mut rx1, &mut rx2] {
            let mut s = rx.recv().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            got.push(buf);
        }
        assert_eq!(got.len(), 2);

        // Shut tunnel 1 down; every subsequent open must still succeed on
        // tunnel 2 regardless of where the round-robin pointer stands.
        send_shutdown(&shutdown1);
        // Let the driver actually exit: an open that lands on the dying
        // driver while it is still draining its select loop would win the
        // race and return a stream that dies a moment later (WriteZero).
        // Once the driver is gone the pool's fall-through is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        for _ in 0..4 {
            let mut s = pool.open_stream().await.unwrap();
            s.write_all(b"cccc").await.unwrap();
        }
        for _ in 0..4 {
            let mut s = tokio::time::timeout(std::time::Duration::from_secs(3), rx2.recv())
                .await
                .expect("surviving tunnel stopped accepting streams")
                .unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"cccc");
        }

        drop(a);
        drop(b);
        drop(pool);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), s2).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), s1).await;
    }

    fn send_shutdown(tx: &tokio::sync::watch::Sender<bool>) {
        tx.send(true).unwrap();
    }
}
