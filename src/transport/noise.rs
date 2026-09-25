use std::net::SocketAddr;

use super::{AddrMaybeCached, NoiseStream, TcpTransport, Transport};
use crate::common::owned_write::AsyncWriteOwned;
use crate::config::{NoiseConfig, TransportConfig};
use crate::transport::noise_resume::{
    self, ClientResumeCache, NOISE_RESUME_SELECTOR, ResumeRequest, ServerResumeStore,
};
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
    /// Session resume is opt-in (`[transport.noise].resume`): the client's
    /// ticket cache and the server's ticket store live with the keys, so
    /// every connection of every transport built from these keys shares
    /// them. Without the feature the stores exist but are never used.
    resume: bool,
    client_cache: std::sync::Arc<ClientResumeCache>,
    server_store: std::sync::Arc<ServerResumeStore>,
}

/// The error a declined resume produces: the caller re-dials and runs a
/// full handshake instead.
#[derive(Debug)]
pub(crate) struct ResumeDeclined;

impl std::fmt::Display for ResumeDeclined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server declined the noise session resume")
    }
}

impl std::error::Error for ResumeDeclined {}

impl std::fmt::Debug for NoiseKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.config)
    }
}

/// The outcome of a client's resume attempt. The stream is boxed in the
/// resumed variant so the enum stays small (`NoiseStream` carries three
/// 64 KiB buffer references and the pin-projected inner stream).
enum ResumeAttempt<S> {
    /// Resume is off or nothing is cached: the stream is untouched.
    NotAttempted(S),
    /// The session resumed.
    Resumed(Box<NoiseStream<S>>),
    /// The server declined: the connection is spent.
    Declined,
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
            client_cache: std::sync::Arc::new(ClientResumeCache::default()),
            server_store: std::sync::Arc::new(ServerResumeStore::new(&local_private_key)),
            resume: config.resume.unwrap_or(false),
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

    /// Whether this key set is configured for session resume.
    fn resume_enabled(&self) -> bool {
        self.resume
    }

    /// The client's half of a resume attempt: announce the selector, run
    /// the exchange, and report the outcome.
    async fn try_resume<S>(&self, mut stream: S) -> Result<ResumeAttempt<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        let Some(server_static) = self.remote_public_key.as_deref() else {
            return Ok(ResumeAttempt::NotAttempted(stream)); // no known static key
        };
        let Some(request) = ResumeRequest::build(&self.client_cache, server_static)? else {
            return Ok(ResumeAttempt::NotAttempted(stream)); // nothing cached
        };
        stream.write_all(&[NOISE_RESUME_SELECTOR]).await?;
        stream.flush().await?;
        match noise_resume::client_resume(&mut stream, &request, &self.client_cache).await? {
            Some(cipher) => Ok(ResumeAttempt::Resumed(Box::new(NoiseStream::from_resumed(
                stream, cipher,
            )))),
            None => Ok(ResumeAttempt::Declined),
        }
    }

    /// Run the Noise handshake as the initiator (client side) over any byte
    /// stream. A resume attempt comes first when one is configured and
    /// cached; a decline surfaces as [`ResumeDeclined`] so the caller can
    /// re-dial with a full handshake.
    pub(crate) async fn wrap_initiator<S>(&self, stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        if self.resume_enabled() {
            match self.try_resume(stream).await? {
                ResumeAttempt::Resumed(stream) => return Ok(*stream),
                ResumeAttempt::Declined => return Err(ResumeDeclined.into()),
                ResumeAttempt::NotAttempted(stream) => {
                    return self.wrap_initiator_full(stream).await;
                }
            }
        }
        self.wrap_initiator_full(stream).await
    }

    /// The initiator's full handshake, always: the selector byte, the
    /// Noise exchange, and (when resume is configured) the ticket that
    /// makes the *next* connection resumable.
    pub(crate) async fn wrap_initiator_full<S>(&self, mut stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        stream.write_all(&[crate::protocol::NOISE_SELECTOR]).await?;
        stream.flush().await?;
        let state = self.builder()?.build_initiator()?;
        // The ticket exchange runs on every full handshake, on both sides:
        // the initiator's `want` byte is what tells the responder an
        // exchange follows, so gating it per side would deadlock (or
        // reframe the peer's bytes as payload). What the *configuration*
        // decides is whether a ticket is issued (responder) and whether a
        // cached one is attempted (initiator) — an empty answer is a
        // well-formed "no", not an error.
        let static_for_cache = self.remote_public_key.clone().unwrap_or_default();
        NoiseStream::handshake_and_take_ticket(stream, state, &self.client_cache, &static_for_cache)
            .await
            .with_context(|| "Failed to do noise handshake")
    }

    /// Run the Noise handshake as the responder (server side) over any byte
    /// stream, issuing a session-resume ticket when configured.
    pub(crate) async fn wrap_responder<S>(&self, stream: S) -> Result<NoiseStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        // The exchange always runs (see `wrap_initiator_full`); the store
        // decides whether this handshake earns a ticket.
        let store = self.resume_enabled().then(|| &*self.server_store);
        NoiseStream::handshake_and_issue_ticket(stream, self.builder()?.build_responder()?, store)
            .await
            .with_context(|| "Failed to do noise handshake")
    }

    /// The responder's half of a resume attempt: read the request, verify
    /// it, and return the resumed stream. `Ok(None)` means the request was
    /// declined (the caller drops the connection).
    pub(crate) async fn run_resume<S>(&self, mut stream: S) -> Result<Option<NoiseStream<S>>>
    where
        S: AsyncRead + AsyncWrite + Unpin + AsyncWriteOwned,
    {
        match noise_resume::server_resume(&mut stream, &self.server_store).await? {
            Some(cipher) => Ok(Some(NoiseStream::from_resumed(stream, cipher))),
            None => Ok(None),
        }
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

impl NoiseTransport {
    /// Dial the endpoint (no transport upgrade), for the resume attempt
    /// and its full-handshake fallback.
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
        match self.keys.wrap_initiator(self.dial(addr).await?).await {
            Ok(stream) => Ok(stream),
            // A declined resume consumed its connection: re-dial and run
            // the full handshake, which re-issues the ticket.
            Err(e) if e.downcast_ref::<ResumeDeclined>().is_some() => {
                self.keys.wrap_initiator_full(self.dial(addr).await?).await
            }
            Err(e) => Err(e),
        }
    }
}
