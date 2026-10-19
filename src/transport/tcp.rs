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
        let _ = config;
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
