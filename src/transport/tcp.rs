use crate::{common::helper::tcp_connect_with_proxy, config::TransportConfig};
use tokio::io::AsyncWriteExt;

use super::{AddrMaybeCached, SocketOpts, Transport};
use anyhow::Result;
use async_trait::async_trait;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use url::Url;

#[derive(Debug)]
pub struct TcpTransport {
    socket_opts: SocketOpts,
    proxy: Option<Url>,
}

#[async_trait]
impl Transport for TcpTransport {
    type Acceptor = TcpListener;
    type Stream = TcpStream;
    type RawStream = TcpStream;

    fn new(config: &TransportConfig) -> Result<Self> {
        Ok(TcpTransport {
            // Fixed latency-friendly defaults; every call site applies a
            // `hint` right after connecting, so this is only the base.
            socket_opts: SocketOpts::for_service(None),
            proxy: config.proxy.clone(),
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
    pub(crate) async fn connect_raw(&self, addr: &AddrMaybeCached) -> Result<TcpStream> {
        let s = tcp_connect_with_proxy(addr, self.proxy.as_ref()).await?;
        self.socket_opts.apply(&s);
        Ok(s)
    }
}
