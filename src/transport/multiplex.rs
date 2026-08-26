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

/// Make a freshly opened outbound stream emit its SYN frame immediately.
///
/// rust-yamux opens outbound streams lazily: the SYN flag is piggybacked on
/// the first outbound frame, and a read-only consumer never produces one.
/// Our data-channel protocol is server-speaks-first (`StartForward*`), so a
/// freshly pooled stream starts by reading. Without this zero-length write
/// the stream would never be announced to the server and both ends would
/// wait for each other forever.
async fn send_stream_syn(stream: &mut MuxStream) -> std::io::Result<()> {
    poll_fn(
        |cx| match tokio::io::AsyncWrite::poll_write(Pin::new(stream), cx, &[]) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        },
    )
    .await
}

/// Build the session configuration, clamping values so that the upstream
/// crate's range assertions cannot panic on a bad config.
///
/// Note: rust-yamux auto-tunes each stream's receive window towards the
/// bandwidth-delay product (starting from 256 KiB), so unlike frp's yamux
/// fork there is no fixed per-stream window to raise; the knob here bounds
/// the *total* advertised buffering per tunnel.
pub(crate) fn mux_config(receive_window: Option<usize>, max_streams: Option<usize>) -> Config {
    // Upstream asserts `window >= DEFAULT_CREDIT * max_streams` on EVERY
    // setter (against the *other* field's current value), so the setters
    // must be driven in an order that keeps the invariant intact. The
    // default window is 1 GiB, i.e. up to 4096 streams are always safe.
    const CREDIT: usize = 256 * 1024; // rust-yamux DEFAULT_CREDIT
    const DEFAULT_WINDOW: usize = 1024 * 1024 * 1024;
    const MAX_SAFE_STREAMS_WITH_DEFAULT_WINDOW: usize = DEFAULT_WINDOW / CREDIT;

    let max_streams = max_streams.unwrap_or(512).clamp(1, 8192);
    let mut config = Config::default();

    match receive_window {
        None => {
            // Window stays unlimited-ish (default 1 GiB total).
            config.set_max_num_streams(max_streams.min(MAX_SAFE_STREAMS_WITH_DEFAULT_WINDOW));
        }
        Some(window) => {
            // 1. Lower the stream count to a value that is safe under the
            //    current default window.
            let intermediate = max_streams.min(MAX_SAFE_STREAMS_WITH_DEFAULT_WINDOW);
            config.set_max_num_streams(intermediate);
            // 2. Raise the window to cover the final stream count.
            let needed = CREDIT.saturating_mul(max_streams).max(window);
            config.set_max_connection_receive_window(Some(needed));
            // 3. Apply the requested stream count.
            config.set_max_num_streams(max_streams);
        }
    }
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

            loop {
                // One unified poll: an outbound open (when a request is
                // pending) takes precedence over inbound work. Both arms of
                // the connection state machine register their wakers here.
                enum Step {
                    Opened(Result<yamux::Stream, yamux::ConnectionError>),
                    Inbound(Option<Result<yamux::Stream, yamux::ConnectionError>>),
                }

                let step = poll_fn(|cx| {
                    if waiting.is_some()
                        && let std::task::Poll::Ready(r) = conn.poll_new_outbound(cx)
                    {
                        return std::task::Poll::Ready(Step::Opened(r));
                    }
                    match conn.poll_next_inbound(cx) {
                        std::task::Poll::Ready(v) => std::task::Poll::Ready(Step::Inbound(v)),
                        std::task::Poll::Pending => std::task::Poll::Pending,
                    }
                });

                tokio::select! {
                    _ = shutdown.changed() => break,
                    step = step => match step {
                        Step::Opened(result) => {
                            if let Some(reply) = waiting.take() {
                                let response = match result {
                                    Ok(stream) => {
                                        let mut stream = stream.compat();
                                        match send_stream_syn(&mut stream).await {
                                            Ok(()) => Ok(stream),
                                            Err(e) => {
                                                debug!(error = %e, "Failed to announce outbound multiplexed stream");
                                                Err(yamux::ConnectionError::Closed)
                                            }
                                        }
                                    }
                                    Err(e) => Err(e),
                                };
                                let _ = reply.send(response);
                            }
                        }
                        // The server never opens streams toward us; drop any
                        // that appear. Errors/end of stream close the tunnel.
                        Step::Inbound(Some(Ok(_stream))) => {}
                        Step::Inbound(_) => break,
                    },
                    req = open_rx.recv(), if waiting.is_none() => {
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn client_opens_streams_server_receives() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(run_server_tunnel(
            server_io,
            mux_config(None, None),
            inbound_tx,
        ));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(None, None), shutdown_rx);

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
        let server_task = tokio::spawn(run_server_tunnel(
            server_io,
            mux_config(None, None),
            inbound_tx,
        ));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(None, None), shutdown_rx);

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
            run_server_tunnel(sock, mux_config(None, None), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(None, None), shutdown_rx);

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
            run_server_tunnel(sock, mux_config(None, None), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(None, None), shutdown_rx);

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
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::level_filters::LevelFilter::TRACE)
            .with_ansi(false)
            .try_init();
        // Production failure signature: initial pooled opens succeed, then
        // after an idle period a NEW open never completes.
        use tokio::net::{TcpListener, TcpStream};

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
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        client_sock.write_all(&[0u8; 34]).await.unwrap();
        let mut hello = [0u8; 34];
        server_sock.read_exact(&mut hello).await.unwrap();
        let mut ack = [0u8; 1];
        server_sock.write_all(&ack).await.unwrap();
        client_sock.read_exact(&mut ack).await.unwrap();

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            run_server_tunnel(server_sock, mux_config(None, None), inbound_tx).await;
        });

        let tunnel = ClientTunnel::start(client_sock, mux_config(None, None), shutdown_rx);

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
    fn mux_config_clamps_bad_values() {
        // Must not panic even with absurd inputs
        let _ = mux_config(Some(1), Some(0));
        let _ = mux_config(Some(usize::MAX), Some(usize::MAX));
        let _ = mux_config(None, None);
    }
}
