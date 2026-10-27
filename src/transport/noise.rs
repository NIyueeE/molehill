use std::net::SocketAddr;

use super::{AddrMaybeCached, NoiseStream, TcpTransport, Transport};
use crate::common::owned_write::AsyncWriteOwned;
use crate::config::{NoiseConfig, TransportConfig};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use snow::Builder;
use snow::params::NoiseParams;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

/// The one handshake every session speaks.
///
/// KK is fixed rather than configured: both ends already know each other's
/// static key, so the handshake authenticates both directions and no config
/// can select an anonymous suite. Declaring it once here is what lets the
/// server refuse a plaintext control channel by name.
pub(crate) const NOISE_PATTERN: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Parsed KK key material: this host's private key and the peer's public key.
///
/// Shared by the TCP transport (control and, when enabled, data) and the mhd
/// tunnel path, where the same handshake rides on an mhd byte stream instead.
#[derive(Clone)]
pub(crate) struct NoiseKeys {
    params: NoiseParams,
    local_private_key: Vec<u8>,
    remote_public_key: Vec<u8>,
}

impl std::fmt::Debug for NoiseKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "NoiseKeys({NOISE_PATTERN})")
    }
}

fn decode_key(encoded: &str, what: &str) -> Result<Vec<u8>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .with_context(|| format!("Failed to decode the Noise {what}"))?;
    anyhow::ensure!(
        bytes.len() == 32,
        "The Noise {what} must decode to 32 bytes, got {}",
        bytes.len()
    );
    Ok(bytes)
}

impl NoiseKeys {
    /// Parse a KK key pair. The parser has already validated both keys; this
    /// repeats the length check because the transport is also built by tests
    /// that construct a config directly.
    pub(crate) fn from_config(config: &NoiseConfig) -> Result<Self> {
        let params: NoiseParams = NOISE_PATTERN.parse()?;
        Ok(NoiseKeys {
            params,
            local_private_key: decode_key(&config.private_key, "private_key")?,
            remote_public_key: decode_key(&config.public_key, "public_key")?,
        })
    }

    fn builder(&self) -> Result<Builder<'_>> {
        Ok(Builder::new(self.params.clone())
            .local_private_key(&self.local_private_key)?
            .remote_public_key(&self.remote_public_key)?)
    }

    /// Run the initiator's side of the KK handshake over any byte stream.
    pub(crate) async fn wrap_initiator<S>(&self, mut stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        stream.write_all(&[crate::protocol::NOISE_SELECTOR]).await?;
        stream.flush().await?;
        NoiseStream::handshake(stream, self.builder()?.build_initiator()?)
            .await
            .with_context(|| "Failed to do noise handshake")
    }

    /// Run the responder's side of the KK handshake over any byte stream.
    pub(crate) async fn wrap_responder<S>(&self, stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        NoiseStream::handshake(stream, self.builder()?.build_responder()?)
            .await
            .with_context(|| "Failed to do noise handshake")
    }
}

#[doc(hidden)]
pub struct NoiseTransport {
    tcp: TcpTransport,
    keys: NoiseKeys,
}

impl std::fmt::Debug for NoiseTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.keys)
    }
}

impl NoiseTransport {
    /// Dial the endpoint without a transport upgrade.
    #[cfg(feature = "client")]
    async fn dial(&self, addr: &AddrMaybeCached) -> Result<TcpStream> {
        self.tcp
            .connect_raw(addr)
            .await
            .with_context(|| "Failed to connect TCP socket")
    }
}

#[async_trait]
impl Transport for NoiseTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    // `snow` supplies the crypto: the ring-accelerated ChaChaPoly data path
    // is used for the fixed KK handshake; `NoiseStream` wraps the stream in
    // tokio IO with the u16-framed message format.
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
        self.keys.wrap_initiator(self.dial(addr).await?).await
    }
}

#[cfg(all(test, feature = "client", feature = "server"))]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap and expect on values they just constructed"
    )]
    use super::*;
    use crate::config::MaskedString;
    use crate::transport::Transport;
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    /// A KK pair for one host: the private key it keeps and the public key its
    /// peer records.
    fn keypair() -> (String, String) {
        let params = NOISE_PATTERN.parse().unwrap();
        let pair = snow::Builder::new(params).generate_keypair().unwrap();
        let engine = base64::engine::general_purpose::STANDARD;
        (engine.encode(pair.private), engine.encode(pair.public))
    }

    /// The fixed KK handshake carries a payload both ways, and the transport
    /// builds it from the two static keys alone.
    #[tokio::test]
    async fn a_kk_pair_carries_a_payload() {
        let (server_private, server_public) = keypair();
        let (client_private, client_public) = keypair();
        let server_cfg = NoiseConfig {
            public_key: client_public,
            private_key: MaskedString::from(server_private.as_str()),
        };
        let client_cfg = NoiseConfig {
            public_key: server_public,
            private_key: MaskedString::from(client_private.as_str()),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_keys = NoiseKeys::from_config(&server_cfg).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut selector = [0u8; 1];
            let mut stream = stream;
            stream.read_exact(&mut selector).await.unwrap();
            assert_eq!(selector[0], crate::protocol::NOISE_SELECTOR);
            let mut stream = server_keys.wrap_responder(stream).await.unwrap();
            let mut ping = [0u8; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let client_transport: NoiseTransport = Transport::new(&TransportConfig {
            noise: Some(client_cfg),
            ..Default::default()
        })
        .unwrap();
        let target = AddrMaybeCached::new(&addr.to_string());
        let result = timeout(Duration::from_secs(10), async move {
            let mut stream = client_transport.connect(&target).await.unwrap();
            stream.write_all(b"ping").await.unwrap();
            let mut pong = [0u8; 4];
            stream.read_exact(&mut pong).await.unwrap();
            assert_eq!(&pong, b"pong");
        })
        .await;
        result.expect("the KK round trip timed out");
        timeout(Duration::from_secs(10), server)
            .await
            .expect("the server task timed out")
            .unwrap();
    }
}
