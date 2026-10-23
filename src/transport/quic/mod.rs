//! The UDP carrier's engine: QUIC's transport with its crypto subtracted.
//!
//! `quinn-proto` is sans-IO. It owns the framing, the ACK/RACK/PTO recovery,
//! the pacing, the congestion controller, the streams and the flow control,
//! and it reaches every cryptographic operation through five traits
//! (`crypto::{Session, ClientConfig, ServerConfig, PacketKey, HeaderKey}`).
//! This module implements those traits with nothing behind them: the
//! handshake is Noise — the identity the control channel already establishes —
//! and `crate::transport::noise_stream` remains the only thing that encrypts
//! a stream's bytes.
//!
//! What is deliberately absent is a packet cipher. There is no
//! Initial/Handshake/1-RTT key schedule, no header protection, no AEAD tag
//! and no key update, because `[client.noise].data_enable` is the only
//! encryption switch this project has, and it has to keep meaning what it
//! means today: the payload is wrapped in the session's record stream, or it
//! goes as it is. What that costs is written down in `docs/transport.md`
//! ("The UDP carrier") and in `docs/internals.md`.

pub(crate) mod channel;
pub(crate) mod conn;
mod crypto;
mod driver;
mod params;
mod record;
pub(crate) mod seam;
pub(crate) mod session;
pub(crate) mod socket;
