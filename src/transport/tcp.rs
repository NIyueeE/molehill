use crate::config::TransportConfig;
#[cfg(feature = "client")]
use tokio::io::AsyncWriteExt;

#[cfg(feature = "client")]
use super::AddrMaybeCached;
use super::{SocketOpts, Transport};
use anyhow::Result;
use async_trait::async_trait;
#[cfg(feature = "server")]
use std::net::SocketAddr;
#[cfg(feature = "server")]
use tokio::net::ToSocketAddrs;
use tokio::net::{TcpListener, TcpStream};

#[cfg(feature = "multiplex")]
pub(crate) mod mux;

#[derive(Debug)]
pub struct TcpTransport {
    socket_opts: SocketOpts,
}

#[async_trait]
impl Transport for TcpTransport {
    type Acceptor = TcpListener;
    type Stream = TcpStream;
    type RawStream = TcpStream;

    fn new(_config: &TransportConfig) -> Result<Self> {
        #[cfg(not(feature = "client"))]
        let _ = _config;
        Ok(TcpTransport {
            // Fixed latency-friendly defaults; every call site applies a
            // hint right after connecting, so this is only the base.
            socket_opts: SocketOpts::for_service(),
        })
    }

    #[cfg(feature = "server")]
    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor> {
        Ok(TcpListener::bind(addr).await?)
    }

    #[cfg(feature = "server")]
    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        let (s, addr) = a.accept().await?;
        self.socket_opts.apply(&s);
        Ok((s, addr))
    }

    #[cfg(feature = "client")]
    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let mut s = self.connect_raw(addr).await?;
        // v3 transport selector: announce this connection is plain, so the
        // server can accept plain and Noise connections on one listener.
        s.writable().await?;
        s.write_all(&[crate::protocol::PLAIN_SELECTOR]).await?;
        Ok(s)
    }
}

impl TcpTransport {
    /// Dial and apply socket options, without the v3 transport selector.
    /// The Noise transport uses this and writes its own selector byte
    /// before the handshake.
    #[cfg(feature = "client")]
    pub(crate) async fn connect_raw(&self, addr: &AddrMaybeCached) -> Result<TcpStream> {
        let s = match addr.socket_addr {
            Some(s) => TcpStream::connect(s).await?,
            None => TcpStream::connect(&addr.addr).await?,
        };
        self.socket_opts.apply(&s);
        Ok(s)
    }
}

/// One yamux session over one connection, seen through the seam.
///
/// The session is the same whether its bytes travel over TCP or over the KCP
/// carrier, so the carrier is a field rather than the type's name: the pool
/// knows which one it dialed, and this handle repeats that instead of
/// guessing it from the code path.
#[cfg(feature = "multiplex")]
#[cfg(feature = "quic")]
pub(crate) struct MuxConn {
    tunnel: super::multiplex::ClientTunnel,
    carrier: super::carrier::Carrier,
}

#[cfg(feature = "multiplex")]
#[cfg(feature = "quic")]
impl MuxConn {
    /// Start the session over an established byte stream.
    pub(crate) fn new<I>(
        io: I,
        shutdown: tokio::sync::watch::Receiver<bool>,
        carrier: super::carrier::Carrier,
    ) -> Self
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            tunnel: super::multiplex::ClientTunnel::start(
                io,
                super::multiplex::mux_config(),
                shutdown,
            ),
            carrier,
        }
    }
}

#[cfg(feature = "multiplex")]
#[async_trait::async_trait]
#[cfg(feature = "quic")]
impl super::carrier::CarrierConn for MuxConn {
    async fn open_stream(
        &self,
    ) -> Result<super::carrier::CarrierStream, super::carrier::StreamError> {
        match self.tunnel.open_stream().await {
            Ok(stream) => Ok(stream),
            Err(crate::transport::tcp::mux::ConnectionError::TooManyStreams) => {
                Err(super::carrier::StreamError::Busy)
            }
            Err(_) => Err(super::carrier::StreamError::Closed),
        }
    }

    fn carrier(&self) -> super::carrier::Carrier {
        self.carrier
    }

    fn is_alive(&self) -> bool {
        self.tunnel.is_alive()
    }

    async fn stats(&self) -> super::carrier::CarrierStats {
        // yamux counts the streams; the kernel keeps the losses and has no
        // round-trip estimate to offer, so the carrier reports none rather
        // than inventing one.
        super::carrier::CarrierStats {
            streams: self.tunnel.counters.streams(),
            rtt: self.tunnel.counters.rtt(),
            lost_packets: 0,
        }
    }
}

#[cfg(all(test, feature = "multiplex"))]
#[cfg(feature = "quic")]
mod seam_tests {
    use super::*;
    use crate::transport::carrier::CarrierConn as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn the_tcp_carrier_yields_a_stream_through_the_seam() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(super::super::multiplex::run_server_tunnel(
            server_io,
            super::super::multiplex::mux_config(),
            tx,
        ));
        let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let conn = MuxConn::new(client_io, shutdown_rx, super::super::carrier::Carrier::Tcp);

        let mut stream = conn.open_stream().await.expect("a stream");
        stream.write_all(b"hello").await.expect("the write");
        stream.flush().await.expect("the flush");

        let deadline = std::time::Duration::from_secs(5);
        let mut inbound = tokio::time::timeout(deadline, rx.recv())
            .await
            .expect("a stream arrives")
            .expect("the server task is alive");
        let mut buf = [0u8; 5];
        tokio::time::timeout(deadline, inbound.read_exact(&mut buf))
            .await
            .expect("the read completes")
            .expect("the read succeeds");
        assert_eq!(&buf, b"hello");

        assert_eq!(conn.carrier(), super::super::carrier::Carrier::Tcp);
        assert!(conn.is_alive());
    }
}
