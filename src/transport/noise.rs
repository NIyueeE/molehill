use std::net::SocketAddr;

use super::{AddrMaybeCached, NoiseStream, TcpTransport, Transport};
use crate::config::{NoiseConfig, TransportConfig};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use snow::Builder;
use snow::params::NoiseParams;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

/// Parsed Noise key material from `[transport.noise]`.
///
/// Shared by the Noise transport (TCP control/data channels) and the KCP
/// tunnel path (arm 2 of the transport comparison), where the same Noise
/// handshake rides on a KCP byte stream instead of TCP — only the plaintext
/// leg is replaced, keys and pattern are unchanged.
#[derive(Clone)]
pub(crate) struct NoiseKeys {
    config: NoiseConfig,
    params: NoiseParams,
    local_private_key: Vec<u8>,
    remote_public_key: Option<Vec<u8>>,
    psk: Option<(u8, Vec<u8>)>,
}

impl std::fmt::Debug for NoiseKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.config)
    }
}

impl NoiseKeys {
    /// Parse and validate a `[transport.noise]` block. When no local private
    /// key is configured, an ephemeral keypair is generated (matching the
    /// long-standing `NoiseTransport` behavior).
    pub(crate) fn from_config(config: &NoiseConfig) -> Result<Self> {
        let builder = Builder::new(config.pattern.parse()?);

        let remote_public_key = match &config.remote_public_key {
            Some(x) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(x)
                    .with_context(|| "Failed to decode remote_public_key")?,
            ),
            None => None,
        };

        let local_private_key = match &config.local_private_key {
            Some(x) => base64::engine::general_purpose::STANDARD
                .decode(x.as_bytes())
                .with_context(|| "Failed to decode local_private_key")?,
            None => builder.generate_keypair()?.private,
        };

        let params: NoiseParams = config.pattern.parse()?;

        let psk = match &config.psk {
            Some(psk_b64) => {
                let psk_location = config.psk_location.unwrap_or(0);
                let psk_bytes = base64::engine::general_purpose::STANDARD
                    .decode(psk_b64.as_bytes())
                    .with_context(|| "Failed to decode psk")?;
                Some((psk_location, psk_bytes))
            }
            None => None,
        };

        Ok(NoiseKeys {
            config: config.clone(),
            params,
            local_private_key,
            remote_public_key,
            psk,
        })
    }

    fn builder(&self) -> Result<Builder<'_>> {
        let mut builder =
            Builder::new(self.params.clone()).local_private_key(&self.local_private_key)?;
        if let Some(x) = &self.remote_public_key {
            builder = builder.remote_public_key(x)?;
        }
        if let Some((loc, key)) = &self.psk {
            // snow 0.10 takes a fixed-size PSK slice and validates the
            // length; the config parser already enforced 32 bytes.
            builder = builder.psk(*loc, key.as_slice().try_into()?)?;
        }
        Ok(builder)
    }

    /// Run the Noise handshake as the initiator (client side) over any byte
    /// stream.
    pub(crate) async fn wrap_initiator<S>(&self, stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        NoiseStream::handshake(stream, self.builder()?.build_initiator()?)
            .await
            .with_context(|| "Failed to do noise handshake")
    }

    /// Run the Noise handshake as the responder (server side) over any byte
    /// stream.
    pub(crate) async fn wrap_responder<S>(&self, stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        NoiseStream::handshake(stream, self.builder()?.build_responder()?)
            .await
            .with_context(|| "Failed to do noise handshake")
    }
}

pub struct NoiseTransport {
    tcp: TcpTransport,
    keys: NoiseKeys,
}

impl std::fmt::Debug for NoiseTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.keys)
    }
}

#[async_trait]
impl Transport for NoiseTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    // `snow` supplies the crypto: the ring-accelerated ChaChaPoly data
    // path serves every pattern (see the `noise` feature in Cargo.toml);
    // `NoiseStream` (noise_stream.rs) wraps the stream in tokio IO with
    // the u16-framed message format.
    type Stream = super::NoiseStream<TcpStream>;

    fn new(config: &TransportConfig) -> Result<Self> {
        let tcp = TcpTransport::new(config)?;

        let noise_config = match &config.noise {
            Some(v) => v.clone(),
            None => return Err(anyhow!("Missing noise config")),
        };
        let keys = NoiseKeys::from_config(&noise_config)?;

        Ok(NoiseTransport { tcp, keys })
    }

    #[cfg(feature = "server")]
    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor> {
        Ok(TcpListener::bind(addr).await?)
    }

    #[cfg(feature = "server")]
    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        self.tcp
            .accept(a)
            .await
            .with_context(|| "Failed to accept TCP connection")
    }

    #[cfg(feature = "client")]
    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self
            .tcp
            .connect_raw(addr)
            .await
            .with_context(|| "Failed to connect TCP socket")?;
        // v3 transport selector: announce this connection speaks Noise.
        let mut conn = conn;
        conn.writable().await?;
        conn.write_all(&[crate::protocol::NOISE_SELECTOR]).await?;

        self.keys.wrap_initiator(conn).await
    }
}
