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

#[cfg(all(test, feature = "multiplex"))]
mod seam_tests {
    #![expect(
        clippy::expect_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::super::multiplex::{ClientTunnel, mux_config};
    use crate::transport::carrier::CarrierConn;
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
        let conn = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

        let mut stream = CarrierConn::open_stream(&conn).await.expect("a stream");
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

        assert!(CarrierConn::is_alive(&conn));
    }
}
