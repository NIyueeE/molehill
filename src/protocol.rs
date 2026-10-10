pub const HASH_WIDTH_IN_BYTES: usize = 32;

use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::LazyLock;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{trace, warn};

use crate::config::ServiceType;

type ProtocolVersion = u8;
const _PROTO_V0: u8 = 0u8;
const _PROTO_V1: u8 = 1u8;
const _PROTO_V2: u8 = 2u8;
const _PROTO_V3: u8 = 3u8;
const _PROTO_V4: u8 = 4u8;
const PROTO_V5: u8 = 5u8;

/// v4: every connection starts with a one-byte transport selector (`0x00`
/// plain / `0x01` noise) so the server can accept both transports on one
/// listener without a config-side `type` agreement, and the registration
/// carries the service's data-plane carrier (client-declared, server
/// validates). Both ends must upgrade together.
///
/// v4: one authenticated control session per `(client, remote_addr)` carries N
/// service registrations, each with its own credential; commands carry a
/// service id; the server declares its heartbeat cadence in the session ack.
/// A v4 registration drops `pool_size` — the tunnel pool is a client-side,
/// per-carrier concern by then.
///
/// v4 also **names a stripe group on the control channel**: the server asks for
/// each of a striped visitor connection's channels with
/// [`ControlChannelCmd::CreateDataChannelForStripe`], carrying the group's
/// fixed 4-byte id and the stripe's index and count, so the client knows which
/// opens belong together and reserves one tunnel per stripe. Before this the K
/// requests were indistinguishable from K separate visitors, and the whole group
/// landed on one tunnel: the group still worked, but the spread D24 asks for
/// was gone.
/// The data plane (hellos, prologue, the striping frames) is unchanged.
///
/// v4's number, kept for the refusal tests: this build speaks v5 and a v4
/// hello must be turned away with the version the peer actually sent.
#[cfg(test)]
pub const PROTO_V4_VERSION: ProtocolVersion = _PROTO_V4;

/// v5 adds one service type and one data-channel command, nothing else: a
/// registration may declare `ServiceType::Transparent`, whose `bind_addr` is a
/// public `ip:port` the client *claims* rather than a listener the server
/// binds, and whose data channel carries whole IP packets — framed
/// `[u16 length][packet]` by [`IpTraffic`] — once the server has announced it
/// with [`DataChannelCmd::StartForwardTransparent`]. The server keeps no
/// per-visitor state for such a service: it routes packets it was given by the
/// host's own routing, and the client's kernel owns the connections.
///
/// The selector byte, the session handshake, the service prologue, the
/// striping frames and the UDP framing are unchanged from v4.
pub const PROTO_V5_VERSION: ProtocolVersion = PROTO_V5;

/// The dialect this build *speaks*: the version it puts in the hellos it
/// originates. It is deliberately separate from [`SUPPORTED_PROTO_VERSIONS`],
/// because a server has to keep serving the dialects it no longer speaks.
pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V5_VERSION;

/// The dialect a *server* accepts, and the only one it speaks.
///
/// v4 (one control session per endpoint, service-id-carrying commands) stopped
/// being served when v5 landed: this project is self-hosted, both ends are the
/// same binary, and a wire break is announced on the connection it happens on
/// (see [`read_hello`]). Anything else is refused there, loudly, with the
/// version the peer actually sent.
pub const SUPPORTED_PROTO_VERSIONS: [ProtocolVersion; 1] = [PROTO_V5_VERSION];

/// First byte of every byte stream between client and server (TCP
/// connections and KCP sessions alike): `PLAIN_SELECTOR` is followed by
/// the postcard hello, `NOISE_SELECTOR` by the Noise handshake. The
/// opt-in session-resume selector (`0x02`, `noise_resume.rs`) is a
/// third value on the same byte, and an old peer rejects it the same way
/// it rejects an unknown protocol version.
pub const PLAIN_SELECTOR: u8 = 0x00;
pub const NOISE_SELECTOR: u8 = 0x01;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

/// The data-plane carrier a service's tunnels will use, as declared by the
/// client in its registration. Wire contract: stays stable across versions.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Carrier {
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "kcp")]
    Kcp,
}

impl Carrier {
    /// Map the client's config-side carrier to the wire value. Only
    /// exists with the `multiplex` feature (the `DataCarrier` type is
    /// feature-gated); without it the data plane is always TCP.
    #[cfg(feature = "multiplex")]
    pub fn from_data_carrier(c: crate::config::DataCarrier) -> Carrier {
        match c {
            crate::config::DataCarrier::Tcp => Carrier::Tcp,
            crate::config::DataCarrier::Kcp => Carrier::Kcp,
        }
    }
}

/// The client-driven service registration sent inside a v4 session, after
/// its authentication succeeded.
///
/// The server owns no per-service configuration: everything needed to expose
/// a service (its name, type and public bind address) is declared by the
/// client and validated against the server-side policy (`allow_ports`).
///
/// It carries no channel count: the tunnel pool is the client's own,
/// per-carrier concern, sized there (D5), so a registration asks for the
/// service and nothing about how it will be carried.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ServiceRegistration {
    pub name: String,
    pub service_type: ServiceType,
    /// Public address the service is exposed at, chosen by the client.
    pub bind_addr: SocketAddr,
    /// The data-plane carrier this service's tunnels will use
    /// (`tcp`/`kcp`, client-declared). The server validates it against its
    /// own capabilities and lazily opens its listeners on first use.
    pub carrier: Carrier,
    /// Receive buffer size for UDP datagrams of this service. Ignored for
    /// TCP services. Wire-compatible up to `u16::MAX`.
    pub udp_buffer_size: u16,
}

/// Hard upper bound for the encoded size of a [`ServiceRegistration`] frame.
pub const MAX_REGISTRATION_LEN: usize = 1024;

/// A per-session service handle.
///
/// Four wire bytes, never a varint: every command that carries one has a
/// value-independent length, so a tag-dispatched reader knows exactly how many
/// bytes follow. The same rationale as the striped command's group id.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(pub [u8; 4]);

impl ServiceId {
    /// Wrap a numeric id as its big-endian wire form.
    pub fn new(raw: u32) -> Self {
        ServiceId(raw.to_be_bytes())
    }

    /// The numeric form, for logs and for the client's own bookkeeping.
    pub fn get(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.get())
    }
}

/// One service registration inside a v4 session: the service's own credential
/// (`digest(service_token ‖ nonce)`) plus the registration it authorizes.
///
/// A v4 session authenticates once with the endpoint's default token; every
/// service then proves its own token, so one denied service never disturbs the
/// others (D2).
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SessionRegistration {
    pub service_id: ServiceId,
    pub auth: Digest,
    pub reg: ServiceRegistration,
}

/// What a client sends on its session after authentication.
///
/// `Register` and `Deregister` are also how hot reload adds and removes a
/// service without rebuilding the session.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum SessionCmd {
    Register(SessionRegistration),
    Deregister(ServiceId),
}

/// Upper bound for the encoded size of a [`SessionCmd`] frame: a registration
/// (bounded by [`MAX_REGISTRATION_LEN`]) plus the service id and the digest.
pub const MAX_SESSION_CMD_LEN: usize = MAX_REGISTRATION_LEN + 64;

/// Longest rejection reason a framed ack may carry.
///
/// A v4 session carries acks and commands on one stream, and the client tells
/// them apart from the first byte: a framed ack starts with its length's *high*
/// byte, which is `0` while the frame stays under 256 bytes, whereas every
/// command tag a session can receive is `1..=4` (the v3-only tag `0` is never
/// sent in a session). A longer reason would move that high byte into the
/// command-tag range and make the two frames indistinguishable, so the writer
/// shortens the reason instead: the ack frame is `2 + 1 (tag) + varint(len) +
/// len`, and with `len <= 250` the varint is at most two bytes, so the frame
/// cannot reach 256 whatever the reason was.
pub const MAX_REJECTION_REASON_LEN: usize = 250;

/// A rejection reason shortened to fit [`MAX_REJECTION_REASON_LEN`].
///
/// Truncation is on a character boundary and marked with an ellipsis, so a
/// shortened reason says so. The full text stays where it is produced: the
/// server logs every rejection itself, so nothing is lost to an operator.
pub fn fit_rejection_reason(reason: &str) -> String {
    if reason.len() <= MAX_REJECTION_REASON_LEN {
        return reason.to_owned();
    }
    let mut end = MAX_REJECTION_REASON_LEN - '…'.len_utf8();
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut fitted = reason[..end].to_owned();
    fitted.push('…');
    fitted
}

/// Variant names mirror the wire contract and stay stable across versions.
#[expect(clippy::enum_variant_names, reason = "wire-contract variant names")]
#[derive(Deserialize, Serialize, Debug)]
pub enum Hello {
    /// The opening hello of a control channel. The digest is
    /// `sha256(service name)` in v3, where a control channel serves exactly one
    /// service, and 32 random bytes — the *session tag* — in v4, where the
    /// session's identity must not be derivable from a service name.
    /// Accepting both versions, it is 34 bytes either way.
    ControlChannelHello(ProtocolVersion, Digest),
    DataChannelHello(ProtocolVersion, Digest), // token provided by the session
    /// The opening half of a *multiplexed data tunnel*: after this hello the
    /// connection upgrades to yamux and every subsequent data channel is a
    /// stream inside it. See the `multiplex` feature.
    DataChannelTunnelHello(ProtocolVersion, Digest),
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
pub enum Ack {
    Ok,
    AuthFailed,
    /// The client's service registration was rejected by the server policy
    /// (port not allowed, port already in use, ...). The payload is a
    /// human-readable reason for the client to log.
    RegisterRejected(String),
    /// v4: the session authenticated. The server declares the heartbeat cadence
    /// it will send, so the client derives its own timeout from it instead of
    /// guessing, and refuses a config that cannot survive that cadence (D11).
    /// Carries a payload, so it is exchanged through the framed helpers — never
    /// through the fixed-width [`read_ack`].
    SessionOk {
        heartbeat_interval_secs: u64,
    },
    /// v4: a data tunnel was refused because the operator's valve
    /// (`[server.data].max_tunnels_per_client`) is already reached for this
    /// client. A **unit** variant on purpose: a tunnel hello is answered on the
    /// fixed-width ack path, which carries exactly one byte, so the refusal
    /// cannot carry the cap with it — the server names the cap in its own log,
    /// and this variant is what keeps the answer typed instead of a silently
    /// closed connection. Nothing else about the session changes: the client
    /// keeps the tunnels it has (D14).
    TunnelRefused,
}

impl std::fmt::Display for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Ack::Ok => "Ok",
                Ack::AuthFailed => "Incorrect token",
                Ack::RegisterRejected(reason) => reason,
                Ack::SessionOk {
                    heartbeat_interval_secs,
                } => return write!(f, "session ok, heartbeat every {heartbeat_interval_secs}s"),
                Ack::TunnelRefused =>
                    "the server's `[server.data].max_tunnels_per_client` is reached, so this \
                     tunnel was refused; the session keeps running",
            }
        )
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub enum ControlChannelCmd {
    /// The v3 command a server used to send; no v4 build constructs it, and
    /// the client refuses it if a peer in that dialect sends one.
    ///
    /// Its slot is load-bearing: postcard numbers variants by declaration
    /// order, and the session reader tells an ack frame from a command by the
    /// first byte — an ack's is its length's high byte, which is `0` as long
    /// as the frame stays under 256 bytes. Keeping a variant at tag 0 is what
    /// makes every command this build does speak sit at 1..=4, where that
    /// disambiguation holds.
    CreateDataChannel,
    HeartBeat,
    /// v4: one visitor arrived for that service of this session. A session
    /// carries several services, so the command names the one the channel is
    /// for. Tag-dispatched: the payload is a fixed 4-byte service id, so this
    /// command's length never depends on its value.
    CreateDataChannelFor(ServiceId),
    /// v4: the server lost that service's listener (the bind failed, or the
    /// port was taken over). The client is told so it can re-register, instead
    /// of believing a service is still exposed.
    ServiceDropped(ServiceId),
    /// One stripe of a striped visitor connection needs a data channel.
    /// The service the channel is for, the group's id (the same fixed 4-byte
    /// form [`DataChannelCmd::StartForwardStripedTcp`] carries, big-endian
    /// `u32`), the stripe's index and the group's stripe count.
    ///
    /// The point of the command is that the client learns *which opens belong
    /// together* before it places them, so it can reserve one tunnel per stripe
    /// instead of stacking the whole group on one tunnel (D24).
    /// A plain visitor's request keeps the 4-byte form: only a stripe group
    /// carries the group and its index.
    ///
    /// Fixed width on purpose, like every other session command: the group id
    /// is raw bytes rather than a `u32` (postcard varints integers) and the
    /// index and count are `u8`s, so the tag alone still decides that exactly
    /// 10 bytes follow.
    CreateDataChannelForStripe(ServiceId, [u8; 4], u8, u8),
}

/// Variant names mirror the wire contract and stay stable across versions.
#[expect(clippy::enum_variant_names, reason = "wire-contract variant names")]
#[derive(Deserialize, Serialize, Debug)]
pub enum DataChannelCmd {
    StartForwardTcp,
    StartForwardUdp,
    /// Striped TCP forwarding: this data channel is stripe `index` of
    /// `count` of the group named by the fixed 4 bytes (big-endian `u32`).
    /// After this command the channel carries `[u64 seq][u16 len][payload]`
    /// frames instead of a raw byte stream, and the receiver reassembles
    /// them in `seq` order across the whole group (see the `stripe`
    /// module).
    ///
    /// The group id is a fixed-width byte array rather than a `u32` on
    /// purpose: postcard encodes integers as varints, so a `u32` would make
    /// this command's length depend on its value, while every other data
    /// command is fixed-size (the tag alone decides the length the reader
    /// must consume).
    ///
    /// Self-describing on purpose: the variant tag is read first and only
    /// the striped variant carries the 6-byte suffix, so plain channels keep
    /// their exact 1-byte command wire format and old peers keep parsing
    /// them unchanged. A peer that does not know this variant fails loudly
    /// on the unknown tag instead of silently reframing payload bytes.
    StartForwardStripedTcp([u8; 4], u8, u8),
    /// Transparent forwarding: this data channel carries whole IP packets for
    /// the service the stream's prologue named, framed `[u16 length][packet]`
    /// by [`IpTraffic`]. The client's host owns the claimed address, so the
    /// packets are injected on its side and its kernel answers the visitor;
    /// the server never pairs a visitor with this channel.
    ///
    /// A unit variant like the other fixed-size data commands: the tag alone
    /// decides the length the reader must consume.
    StartForwardTransparent,
}

type UdpPacketLen = u16; // `u16` should be enough for any practical UDP traffic on the Internet
#[derive(Deserialize, Serialize, Debug)]
struct UdpHeader {
    from: SocketAddr,
    len: UdpPacketLen,
}

/// Upper bound of the encoded size of a [`UdpHeader`]: an address tag plus at
/// most 16 bytes of IP, a port and a varint length always fit well below this.
pub const MAX_UDP_HEADER_LEN: usize = 32;

// The owned-payload variant is only used on the client side; the server reads
// through the zero-allocation `read_slice` path instead.
#[cfg_attr(
    not(feature = "client"),
    allow(dead_code, reason = "client-only owned UDP payload")
)]
#[derive(Debug)]
pub struct UdpTraffic {
    pub from: SocketAddr,
    pub data: Bytes,
}

/// Frame one datagram into `scratch` as `[hdr_len u8][header][payload]`.
///
/// The wire format is unchanged; the point is that the whole datagram is
/// emitted with a single buffer and therefore a single `write_all` (and a
/// single Noise record), with no per-packet heap allocation when callers
/// reuse the same scratch buffer.
fn encode_udp_frame(scratch: &mut BytesMut, from: SocketAddr, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).with_context(|| {
        format!(
            "Datagram of {} bytes exceeds the wire format limit",
            data.len()
        )
    })?;
    let hdr = UdpHeader { from, len };

    scratch.clear();
    scratch.reserve(1 + MAX_UDP_HEADER_LEN + data.len());

    // Encode the header into a stack buffer, then assemble the whole frame
    // `[hdr_len u8][header][payload]` in the scratch buffer so the datagram
    // is emitted with a single `write_all` and no per-packet heap allocation.
    let prefix_pos = scratch.len();
    scratch.put_u8(0); // placeholder, fixed up below
    let mut hdr_buf = [0u8; MAX_UDP_HEADER_LEN];
    let encoded =
        postcard::to_slice(&hdr, &mut hdr_buf).with_context(|| "Failed to serialize UdpHeader")?;
    // `to_slice` cannot emit more than `MAX_UDP_HEADER_LEN` (32) bytes, which
    // also fits the `u8` length prefix.
    debug_assert!(encoded.len() <= MAX_UDP_HEADER_LEN);
    scratch.extend_from_slice(encoded);
    let hdr_len = u8::try_from(scratch.len() - prefix_pos - 1)
        .with_context(|| "UDP header length exceeds the u8 prefix")?;
    scratch[prefix_pos] = hdr_len;

    trace!("Write {:?} of length {}", hdr, hdr_len);
    scratch.extend_from_slice(data);
    Ok(())
}

async fn read_udp_header<T: AsyncRead + Unpin>(reader: &mut T, hdr_len: u8) -> Result<UdpHeader> {
    if hdr_len as usize > MAX_UDP_HEADER_LEN {
        bail!(
            "UDP header length {hdr_len} exceeds the maximum of {MAX_UDP_HEADER_LEN}, the stream is corrupt"
        );
    }
    let mut buf = [0u8; MAX_UDP_HEADER_LEN];
    reader
        .read_exact(&mut buf[..hdr_len as usize])
        .await
        .with_context(|| "Failed to read udp header")?;

    postcard::from_bytes(&buf[..hdr_len as usize])
        .with_context(|| "Failed to deserialize UdpHeader")
}

/// Drain the payload of an oversized datagram so that the stream framing stays
/// in sync, then report the packet as dropped.
async fn skip_oversized_payload<T: AsyncRead + Unpin>(
    reader: &mut T,
    len: u16,
    from: SocketAddr,
) -> Result<()> {
    warn!("Dropping oversized UDP packet from {from}, {len} bytes");
    // Bounded by `u16::MAX`, so this cannot grow unreasonably.
    let mut sink = vec![0u8; len as usize];
    // Note: tokio's `read_exact` resolves to `io::Result<usize>` (bytes read)
    reader
        .read_exact(&mut sink)
        .await
        .with_context(|| "Failed to skip oversized udp payload")?;
    Ok(())
}

impl UdpTraffic {
    /// Frame one datagram and send it with a **single** `write_all`.
    ///
    /// Callers should reuse the same `scratch` buffer across packets to avoid
    /// per-packet allocations.
    #[cfg_attr(
        not(any(feature = "client", feature = "server")),
        allow(dead_code, reason = "used by both run modes")
    )]
    pub async fn write_frame<T: AsyncWrite + Unpin>(
        writer: &mut T,
        scratch: &mut BytesMut,
        from: SocketAddr,
        data: &[u8],
    ) -> Result<()> {
        encode_udp_frame(scratch, from, data)?;
        writer.write_all(scratch).await?;
        Ok(())
    }

    /// Read one framed datagram into an owned buffer.
    ///
    /// `max_len` is the receiver's configured UDP buffer size: datagrams
    /// larger than it cannot be handled and are dropped in-stream (payload is
    /// drained so the framing stays in sync) instead of tearing down the data
    /// channel.
    #[cfg_attr(
        not(feature = "client"),
        allow(dead_code, reason = "client-only UDP read path")
    )]
    pub async fn read<T: AsyncRead + Unpin>(
        reader: &mut T,
        hdr_len: u8,
        max_len: usize,
    ) -> Result<Option<UdpTraffic>> {
        let hdr = read_udp_header(reader, hdr_len).await?;

        // A UDP payload larger than the receive buffer cannot originate from
        // this implementation; drop it while keeping the stream usable.
        if hdr.len > u16::try_from(max_len).unwrap_or(UdpPacketLen::MAX) {
            skip_oversized_payload(reader, hdr.len, hdr.from).await?;
            return Ok(None);
        }

        let mut data = BytesMut::zeroed(hdr.len as usize);
        reader.read_exact(&mut data).await?;

        Ok(Some(UdpTraffic {
            from: hdr.from,
            data: data.freeze(),
        }))
    }

    /// Zero-allocation variant of [`UdpTraffic::read`] for consumers that use
    /// the payload immediately: on success the payload occupies
    /// `scratch[..len]`. The oversized-packet policy is identical.
    #[cfg_attr(
        not(feature = "server"),
        allow(dead_code, reason = "server-only zero-allocation read path")
    )]
    pub async fn read_slice<T: AsyncRead + Unpin>(
        reader: &mut T,
        hdr_len: u8,
        scratch: &mut BytesMut,
        max_len: usize,
    ) -> Result<Option<(SocketAddr, usize)>> {
        let hdr = read_udp_header(reader, hdr_len).await?;

        if hdr.len > u16::try_from(max_len).unwrap_or(UdpPacketLen::MAX) {
            skip_oversized_payload(reader, hdr.len, hdr.from).await?;
            return Ok(None);
        }

        scratch.resize(hdr.len as usize, 0);
        reader.read_exact(&mut scratch[..]).await?;
        Ok(Some((hdr.from, hdr.len as usize)))
    }
}

pub fn digest(data: &[u8]) -> Digest {
    use sha2::{Digest, Sha256};
    let d = Sha256::new().chain_update(data).finalize();
    d.into()
}

/// The framing of a transparent data channel: `[u16 length][packet]`, in both
/// directions.
///
/// A namespace type rather than a painted buffer — the packet's destination
/// lives *inside* it, so unlike [`UdpTraffic`] there is no address tag to
/// carry, and callers read straight into their own scratch buffer.
// Present where it is carried (the transparent data path) or exercised (this
// module's own framing tests): a build with neither would only know it as dead
// code, and the tests would not compile without it.
#[cfg(any(test, all(feature = "transparent", target_os = "linux")))]
pub struct IpTraffic;

/// How much room a read makes for frames it has not parsed yet. One read of
/// this size carries a whole batch from the far side, so the per-frame cost is
/// a slice, not a syscall.
#[cfg(any(test, all(feature = "transparent", target_os = "linux")))]
const IP_READ_CHUNK: usize = 32 * 1024;

/// Frames read from a transparent channel, with the reads batched.
///
/// The far side writes a run of frames per write, so one read can carry many
/// packets: parsing them out of a buffer costs one syscall per *run* instead of
/// two per frame, and the caller then walks a whole burst without awaiting
/// between packets. Both halves of the data path are batched for the same
/// reason — the measurement is in
/// [benchmarks.md](../docs/benchmarks.md#the-transparent-l3-wire-question-not-part-of-the-soak-model).
#[cfg(any(test, all(feature = "transparent", target_os = "linux")))]
pub struct IpFrames<R> {
    reader: R,
    /// Bytes read ahead and not yet turned into frames.
    buf: BytesMut,
    /// Where the unconsumed part starts.
    start: usize,
}

#[cfg(any(test, all(feature = "transparent", target_os = "linux")))]
impl<R: AsyncRead + Unpin> IpFrames<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: BytesMut::new(),
            start: 0,
        }
    }

    /// The next frame. The returned slice is valid until the next call.
    ///
    /// The end of the stream is an error, not an empty frame: this channel
    /// exists only while its service does, and the caller answers a channel
    /// that ended by asking for another one.
    pub async fn next(&mut self) -> Result<&[u8]> {
        loop {
            if let Some(len) = IpTraffic::frame_at(&self.buf[self.start..])? {
                let start = self.start + 2;
                self.start = start + len;
                return Ok(&self.buf[start..start + len]);
            }
            self.read_more().await?;
        }
    }

    /// Read more from the tunnel, compacting what is left first so a part-read
    /// frame stays at the front.
    async fn read_more(&mut self) -> Result<()> {
        if self.start > 0 {
            // Inherent on `BytesMut`, and it drops exactly the consumed prefix.
            let _consumed = self.buf.split_to(self.start);
            self.start = 0;
        }
        self.buf.reserve(IP_READ_CHUNK);
        let read = self
            .reader
            .read_buf(&mut self.buf)
            .await
            .with_context(|| "Failed to read IP frames")?;
        if read == 0 {
            let leftover = self.buf.len();
            if leftover > 0 {
                bail!("Transparent channel ended mid-frame ({leftover} bytes left)");
            }
            bail!("Transparent channel ended");
        }
        Ok(())
    }
}

#[cfg(any(test, all(feature = "transparent", target_os = "linux")))]
impl IpTraffic {
    /// Append one packet to a batch as a `[u16 length][packet]` frame.
    ///
    /// Synchronous on purpose: a packet is framed where it is read, so a whole
    /// batch of frames can be handed over and written as one unit. The frame
    /// itself is unchanged — a batch is several of them in one write, and the
    /// reader on the far side consumes them one frame at a time.
    pub fn encode_into(batch: &mut BytesMut, packet: &[u8]) -> Result<()> {
        let len = u16::try_from(packet.len()).with_context(|| {
            format!(
                "IP packet of {} bytes exceeds the wire format limit",
                packet.len()
            )
        })?;
        batch.reserve(2 + packet.len());
        batch.put_u16(len);
        batch.extend_from_slice(packet);
        Ok(())
    }

    /// The length of the frame at the front of `available`, when all of it is
    /// there — the frame format's only reader, so the writer and the reader
    /// cannot drift apart.
    ///
    /// A zero-length frame is a protocol error rather than an empty packet.
    pub fn frame_at(available: &[u8]) -> Result<Option<usize>> {
        let Some(header) = available.first_chunk::<2>() else {
            return Ok(None);
        };
        let len = usize::from(u16::from_be_bytes(*header));
        if len == 0 {
            bail!("Empty IP frame: the stream is corrupt");
        }
        if available.len() < 2 + len {
            return Ok(None);
        }
        Ok(Some(len))
    }
}

struct PacketLength {
    hello: usize,
    #[cfg(feature = "client")]
    ack: usize,
    #[cfg(feature = "server")]
    auth: usize,
}

impl PacketLength {
    /// Encoded length of a protocol value, or 0 on the impossible
    /// serialization failure (a fixed-size value cannot fail to
    /// serialize; a 0 length would surface as a read/deserialize error
    /// at the use site rather than as a panic).
    fn encoded_len<T: serde::Serialize>(value: &T) -> usize {
        postcard::to_stdvec(value).map_or(0, |v| v.len())
    }

    pub fn new() -> PacketLength {
        let username = "default";
        let d = digest(username.as_bytes());
        let hello = Self::encoded_len(&Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d));
        #[cfg(feature = "client")]
        let ack = Self::encoded_len(&Ack::Ok);

        #[cfg(feature = "server")]
        let auth = Self::encoded_len(&Auth(d));
        PacketLength {
            hello,
            #[cfg(feature = "client")]
            ack,
            #[cfg(feature = "server")]
            auth,
        }
    }
}

static PACKET_LEN: LazyLock<PacketLength> = LazyLock::new(PacketLength::new);

/// Read one hello, returning the protocol version it declared beside it.
///
/// The version is *returned* rather than only checked because a server has to
/// serve more than one dialect: it branches on what it read, while the client
/// checks that the answer came back in a dialect it accepts. The accepted
/// set is [`SUPPORTED_PROTO_VERSIONS`]; a peer outside it is refused loudly, and
/// — this is what an old server does to a newer client — by closing without a
/// reply, which that client turns into a typed "server is too old" error
/// instead of a retry loop.
pub async fn read_hello<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<(ProtocolVersion, Hello)> {
    let mut buf = vec![0u8; PACKET_LEN.hello];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read hello")?;
    let hello: Hello = postcard::from_bytes(&buf).with_context(|| "Failed to deserialize hello")?;

    let version = match hello {
        Hello::ControlChannelHello(v, _)
        | Hello::DataChannelHello(v, _)
        | Hello::DataChannelTunnelHello(v, _) => v,
    };
    if !SUPPORTED_PROTO_VERSIONS.contains(&version) {
        bail!(
            "Protocol version mismatched. Supported versions: {SUPPORTED_PROTO_VERSIONS:?}, got {version}. Please update `molehill`."
        );
    }

    Ok((version, hello))
}

/// Write the 4-byte service prologue that opens every stream of a v4 data
/// plane: a direct data channel (right after its hello) and every stream
/// inside a multiplexed tunnel alike.
///
/// A v4 session carries N services, so the server can no longer infer the
/// service from the session nonce — the binding is explicit, and it is four
/// raw bytes, never a varint, so a reader knows exactly how many bytes to
/// consume before any application data can follow.
#[cfg(feature = "client")]
pub async fn write_stream_prologue<T: AsyncWrite + Unpin>(
    conn: &mut T,
    service_id: ServiceId,
) -> Result<()> {
    conn.write_all(&service_id.0)
        .await
        .with_context(|| "Failed to write the v4 service prologue")?;
    conn.flush().await?;
    Ok(())
}

/// Read the prologue written by [`write_stream_prologue`].
#[cfg(feature = "server")]
pub async fn read_stream_prologue<T: AsyncRead + Unpin>(conn: &mut T) -> Result<ServiceId> {
    let mut buf = [0u8; std::mem::size_of::<ServiceId>()];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read the v4 service prologue")?;
    Ok(ServiceId(buf))
}

#[cfg(feature = "server")]
pub async fn read_auth<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Auth> {
    let mut buf = vec![0u8; PACKET_LEN.auth];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read auth")?;
    postcard::from_bytes(&buf).with_context(|| "Failed to deserialize auth")
}

/// Fixed-size acks (auth result) keep using `read_ack`; variable-size ones
/// (`RegisterRejected` carries a reason string) are exchanged through
/// u16-length-prefixed frames via the helpers below.
#[cfg(feature = "client")]
pub async fn read_ack<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Ack> {
    let mut bytes = vec![0u8; PACKET_LEN.ack];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read ack")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize ack")
}

/// Read the framed registration result ack sent by the server after a
/// `SessionCmd::Register`.
///
/// Read-only on purpose: it runs on one half of a split session connection
/// (see [`read_session_cmd`]).
#[cfg(feature = "client")]
pub async fn read_register_result<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Ack> {
    let len = conn
        .read_u16()
        .await
        .with_context(|| "Failed to read register result length")?;
    anyhow::ensure!(
        usize::from(len) <= MAX_REGISTRATION_LEN,
        "Register result too large: {len} bytes"
    );
    let mut buf = vec![0u8; usize::from(len)];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read register result")?;
    postcard::from_bytes(&buf).with_context(|| "Failed to deserialize register result")
}

/// Send the framed registration result ack.
///
/// A rejection reason is shortened to `MAX_REJECTION_REASON_LEN` first, which is
/// what keeps the frame's length high byte at `0`: a v4 session's reader tells
/// an ack from a command by that byte, so a longer reason would read as a
/// command. See `MAX_REJECTION_REASON_LEN`.
#[cfg(feature = "server")]
pub async fn write_register_result<T: AsyncWrite + Unpin>(conn: &mut T, ack: &Ack) -> Result<()> {
    let fitted;
    let ack = match ack {
        Ack::RegisterRejected(reason) => {
            fitted = Ack::RegisterRejected(fit_rejection_reason(reason));
            &fitted
        }
        other => other,
    };
    let payload = postcard::to_stdvec(ack).with_context(|| "Failed to serialize ack")?;
    anyhow::ensure!(
        payload.len() <= MAX_REGISTRATION_LEN,
        "Register result too large: {} bytes",
        payload.len()
    );
    let len = u16::try_from(payload.len())
        .with_context(|| "Register result length exceeds the u16 frame prefix")?;
    conn.write_u16(len)
        .await
        .with_context(|| "Failed to write register result length")?;
    conn.write_all(&payload)
        .await
        .with_context(|| "Failed to write register result")?;
    conn.flush().await?;
    Ok(())
}

/// Read one control-channel command.
///
/// Tag-dispatched: the tag alone decides how many bytes follow, which is what
/// keeps every session command value-independent in length. Read-only for the
/// same reason as [`read_register_result`].
#[cfg(feature = "client")]
pub async fn read_control_cmd<T: AsyncRead + Unpin>(conn: &mut T) -> Result<ControlChannelCmd> {
    // 1 tag byte + the largest suffix below (a service id, a group id, an
    // index and a count: the stripe request's 10 bytes).
    let mut buf = [0u8; 11];
    conn.read_exact(&mut buf[..1])
        .await
        .with_context(|| "Failed to read cmd")?;
    let suffix = match buf[0] {
        0 => return Ok(ControlChannelCmd::CreateDataChannel),
        1 => return Ok(ControlChannelCmd::HeartBeat),
        2 | 3 => 4,
        4 => 10,
        tag => bail!("Unknown control channel command tag {tag:#x}"),
    };
    conn.read_exact(&mut buf[1..=suffix])
        .await
        .with_context(|| "Failed to read cmd")?;
    Ok(match buf[0] {
        2 | 3 => {
            let service_id: ServiceId = postcard::from_bytes(&buf[1..=suffix])
                .with_context(|| "Failed to deserialize cmd")?;
            if buf[0] == 2 {
                ControlChannelCmd::CreateDataChannelFor(service_id)
            } else {
                ControlChannelCmd::ServiceDropped(service_id)
            }
        }
        // The only tag left is 4, the stripe request: the dispatch above
        // returned on 0 and 1 and bailed on everything but 2, 3 and 4. Parsed
        // with postcard as one tuple rather than field by field, so the reader
        // and the writer agree on the layout by construction: a reordered or
        // resized field fails here instead of silently reading the next
        // frame's bytes as an index.
        _ => {
            let (service_id, group, index, count): (ServiceId, [u8; 4], u8, u8) =
                postcard::from_bytes(&buf[1..11]).with_context(|| "Failed to deserialize cmd")?;
            ControlChannelCmd::CreateDataChannelForStripe(service_id, group, index, count)
        }
    })
}

/// Send one [`SessionCmd`] as a u16-length-prefixed frame.
#[cfg(feature = "client")]
pub async fn write_session_cmd<T: AsyncWrite + Unpin>(
    conn: &mut T,
    cmd: &SessionCmd,
) -> Result<()> {
    let payload = postcard::to_stdvec(cmd).with_context(|| "Failed to serialize session cmd")?;
    anyhow::ensure!(
        payload.len() <= MAX_SESSION_CMD_LEN,
        "Session command too large: {} bytes",
        payload.len()
    );
    let len = u16::try_from(payload.len())
        .with_context(|| "Session command length exceeds the u16 frame prefix")?;
    conn.write_u16(len)
        .await
        .with_context(|| "Failed to write session cmd length")?;
    conn.write_all(&payload)
        .await
        .with_context(|| "Failed to write session cmd")?;
    conn.flush().await?;
    Ok(())
}

/// Read one framed [`SessionCmd`] sent by [`write_session_cmd`].
///
/// Read-only on purpose: the session reader is one half of a split
/// connection (`tokio::io::split`), which is a reader and nothing else.
#[cfg(feature = "server")]
pub async fn read_session_cmd<T: AsyncRead + Unpin>(conn: &mut T) -> Result<SessionCmd> {
    let len = conn
        .read_u16()
        .await
        .with_context(|| "Failed to read session cmd length")?;
    anyhow::ensure!(
        usize::from(len) <= MAX_SESSION_CMD_LEN,
        "Session command too large: {len} bytes"
    );
    let mut buf = vec![0u8; usize::from(len)];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read session cmd")?;
    postcard::from_bytes(&buf).with_context(|| "Failed to deserialize session cmd")
}

/// Read one [`DataChannelCmd`].
///
/// The command is tag-dispatched instead of fixed-width: `StartForwardTcp`
/// and `StartForwardUdp` stay 1-byte commands, while the striped variant
/// carries a 6-byte suffix, so its tag is read first and the suffix only
/// when the tag says one follows. This keeps the plain wire format
/// byte-identical while letting the reader tell the commands apart without
/// out-of-band agreement.
#[cfg(feature = "client")]
pub async fn read_data_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<DataChannelCmd> {
    let mut buf = [0u8; 7]; // 1 tag byte + the largest suffix below
    let suffix = match read_cmd_tag(conn, &mut buf).await? {
        0 => {
            return Ok(DataChannelCmd::StartForwardTcp);
        }
        1 => {
            return Ok(DataChannelCmd::StartForwardUdp);
        }
        2 => 6, // StartForwardStripedTcp(group u32, index u8, count u8)
        3 => {
            // StartForwardTransparent is a unit variant, so the tag alone is
            // the whole command and the channel's IP framing starts right
            // after it.
            return Ok(DataChannelCmd::StartForwardTransparent);
        }
        tag => bail!("Unknown data channel command tag {tag:#x}"),
    };
    conn.read_exact(&mut buf[1..=suffix])
        .await
        .with_context(|| "Failed to read data cmd")?;
    postcard::from_bytes(&buf[..=suffix]).with_context(|| "Failed to deserialize data cmd")
}

/// Read the command's variant tag into `buf[0]`, returning it.
#[cfg(feature = "client")]
async fn read_cmd_tag<T: AsyncRead + Unpin>(conn: &mut T, buf: &mut [u8; 7]) -> Result<u8> {
    conn.read_exact(&mut buf[..1])
        .await
        .with_context(|| "Failed to read data cmd")?;
    Ok(buf[0])
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn sample_digest(b: u8) -> Digest {
        let mut d = [0u8; HASH_WIDTH_IN_BYTES];
        d[0] = b;
        d
    }

    fn sample_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
    }

    #[test]
    fn hello_roundtrip_control() {
        let d = sample_digest(42);
        let hello = Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d);
        let bytes = postcard::to_stdvec(&hello).unwrap();
        let back: Hello = postcard::from_bytes(&bytes).unwrap();
        match back {
            Hello::ControlChannelHello(v, d2) => {
                assert_eq!(v, CURRENT_PROTO_VERSION);
                assert_eq!(d2, d);
            }
            _ => panic!("Expected ControlChannelHello"),
        }
    }

    #[test]
    fn hello_roundtrip_data() {
        let d = sample_digest(99);
        let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, d);
        let bytes = postcard::to_stdvec(&hello).unwrap();
        let back: Hello = postcard::from_bytes(&bytes).unwrap();
        match back {
            Hello::DataChannelHello(v, d2) => {
                assert_eq!(v, CURRENT_PROTO_VERSION);
                assert_eq!(d2, d);
            }
            _ => panic!("Expected DataChannelHello"),
        }
    }

    #[test]
    fn auth_roundtrip() {
        let d = sample_digest(7);
        let auth = Auth(d);
        let bytes = postcard::to_stdvec(&auth).unwrap();
        let back: Auth = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.0, d);
    }

    #[test]
    fn ack_roundtrip_all_variants() {
        for ack in [
            Ack::Ok,
            Ack::AuthFailed,
            Ack::RegisterRejected("port not allowed".to_string()),
            Ack::SessionOk {
                heartbeat_interval_secs: 30,
            },
        ] {
            let bytes = postcard::to_stdvec(&ack).unwrap();
            let back: Ack = postcard::from_bytes(&bytes).unwrap();
            match (&ack, &back) {
                (Ack::Ok, Ack::Ok) | (Ack::AuthFailed, Ack::AuthFailed) => {}
                (Ack::RegisterRejected(a), Ack::RegisterRejected(b)) => assert_eq!(a, b),
                (
                    Ack::SessionOk {
                        heartbeat_interval_secs: a,
                    },
                    Ack::SessionOk {
                        heartbeat_interval_secs: b,
                    },
                ) => assert_eq!(a, b),
                _ => panic!("Ack round-trip mismatch"),
            }
        }
    }

    /// The session ack is the one ack a v3 client must never receive: it
    /// carries a payload, so it is exchanged through the framed helpers, and the
    /// fixed-width `read_ack` (1 byte for `Ok`) would desync on it. Pin both
    /// facts so a later variant cannot quietly break the framed path.
    #[test]
    fn session_ok_is_not_a_fixed_width_ack() {
        let ok = postcard::to_stdvec(&Ack::Ok).unwrap();
        assert_eq!(ok.len(), 1);
        let session_ok = postcard::to_stdvec(&Ack::SessionOk {
            heartbeat_interval_secs: 30,
        })
        .unwrap();
        assert!(
            session_ok.len() > 1,
            "the session ack must not fit the fixed-width ack reader"
        );
        // Tag byte + the varint payload: 30 is below 128, so it is one byte.
        assert_eq!(session_ok.len(), 2, "tag + one-byte varint");
    }

    #[test]
    fn ack_display() {
        assert_eq!(Ack::Ok.to_string(), "Ok");
        assert_eq!(Ack::AuthFailed.to_string(), "Incorrect token");
        assert_eq!(
            Ack::RegisterRejected("Port 80 is privileged".to_string()).to_string(),
            "Port 80 is privileged"
        );
        assert_eq!(
            Ack::SessionOk {
                heartbeat_interval_secs: 30
            }
            .to_string(),
            "session ok, heartbeat every 30s"
        );
    }

    /// The session's reader tells an ack from a command by the first byte: an
    /// ack's is its length's high byte (`0`), a command's is its tag (`1..=3`).
    /// A rejection reason long enough to push the frame past 255 bytes would
    /// break that, so the writer shortens it — this pins the bound for a reason
    /// far longer than any real one, and pins that the shortened copy still says
    /// what it is.
    #[test]
    fn a_long_rejection_reason_still_frames_below_the_command_tag_range() {
        let long = "x".repeat(4096);
        let fitted = fit_rejection_reason(&long);
        assert!(fitted.len() <= MAX_REJECTION_REASON_LEN);
        assert!(fitted.ends_with('…'), "a shortened reason must show it");

        let payload = postcard::to_stdvec(&Ack::RegisterRejected(fitted.clone())).unwrap();
        assert!(
            payload.len() < 256,
            "the frame's length high byte must stay 0, else it reads as a command tag"
        );
        // Every command a v4 session can receive is tagged 1..=3, so a leading
        // 0 is unambiguous (tag 0 is the reserved v3 command — see
        // `command_tags_stay_out_of_the_ack_frame_range`). Pin that, because it
        // is the other half of the rule.
        for cmd in [
            ControlChannelCmd::HeartBeat,
            ControlChannelCmd::CreateDataChannelFor(ServiceId::new(0)),
            ControlChannelCmd::ServiceDropped(ServiceId::new(0)),
        ] {
            let tag = postcard::to_stdvec(&cmd).unwrap()[0];
            assert!(
                (1..=3).contains(&tag),
                "command {cmd:?} has tag {tag}, which collides with an ack frame"
            );
        }

        // A reason that already fits is passed through untouched.
        assert_eq!(fit_rejection_reason("port in use"), "port in use");
        // Truncation is on a character boundary, never inside one.
        let wide = "é".repeat(400);
        let fitted = fit_rejection_reason(&wide);
        assert!(fitted.len() <= MAX_REJECTION_REASON_LEN);
        assert!(fitted.ends_with('…'));
    }

    /// A refused data tunnel is answered on the **fixed-width** ack path: the
    /// client's tunnel dial reads exactly the length of `Ack::Ok`, so a
    /// refusal that carried a payload (a `RegisterRejected`-style reason)
    /// would be read as a truncated ack and the client would report a
    /// deserialize error instead of a refusal. This pins the one-byte shape
    /// and the round trip.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn a_tunnel_refusal_fits_the_fixed_width_ack() {
        use tokio::io::{AsyncWriteExt, duplex};

        let payload = postcard::to_stdvec(&Ack::TunnelRefused).unwrap();
        assert_eq!(
            payload.len(),
            PACKET_LEN.ack,
            "the refusal must be exactly as wide as Ack::Ok, or read_ack misreads it"
        );

        let (mut tx, mut rx) = duplex(64);
        tx.write_all(&payload).await.unwrap();
        let ack = read_ack(&mut rx).await.unwrap();
        assert!(matches!(ack, Ack::TunnelRefused));
        let msg = ack.to_string();
        assert!(
            msg.contains("max_tunnels_per_client"),
            "the refusal must name the valve: {msg}"
        );
    }

    /// A fresh session's registration answer must fit the same bound through
    /// the framed writer, which is the path the server actually uses.
    #[cfg(feature = "server")]
    #[tokio::test]
    async fn write_register_result_shortens_an_oversized_reason() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(8192);
        let reason = "bind failed: ".to_string() + &"y".repeat(4096);
        write_register_result(&mut tx, &Ack::RegisterRejected(reason.clone()))
            .await
            .unwrap();
        let len = rx.read_u16().await.unwrap();
        assert!(usize::from(len) < 256, "framed ack length {len}");
        let mut buf = vec![0u8; usize::from(len)];
        rx.read_exact(&mut buf).await.unwrap();
        match postcard::from_bytes::<Ack>(&buf).unwrap() {
            Ack::RegisterRejected(short) => {
                assert!(short.starts_with("bind failed: "));
                assert!(short.len() < reason.len());
                assert!(short.ends_with('…'));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn registration_roundtrip() {
        let reg = ServiceRegistration {
            name: "ssh".to_string(),
            service_type: crate::config::ServiceType::Tcp,
            bind_addr: sample_addr(),
            carrier: Carrier::Tcp,
            udp_buffer_size: 2048,
        };
        let bytes = postcard::to_stdvec(&reg).unwrap();
        assert!(bytes.len() <= MAX_REGISTRATION_LEN);
        let back: ServiceRegistration = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.name, "ssh");
        assert_eq!(back.bind_addr, sample_addr());
        assert_eq!(back.udp_buffer_size, 2048);
    }

    #[test]
    fn control_cmd_roundtrip() {
        // CreateDataChannel (the reserved tag, round-tripped like any other)
        let cmd = ControlChannelCmd::CreateDataChannel;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, ControlChannelCmd::CreateDataChannel));

        // HeartBeat
        let cmd = ControlChannelCmd::HeartBeat;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, ControlChannelCmd::HeartBeat));

        // The v4 service-scoped variants
        for cmd in [
            ControlChannelCmd::CreateDataChannelFor(ServiceId::new(0)),
            ControlChannelCmd::CreateDataChannelFor(ServiceId::new(0xdead_beef)),
            ControlChannelCmd::ServiceDropped(ServiceId::new(7)),
        ] {
            let bytes = postcard::to_stdvec(&cmd).unwrap();
            let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(
                format!("{back:?}"),
                format!("{cmd:?}"),
                "control command changed on the wire"
            );
        }

        // The stripe request: the group id is raw bytes, the index and the
        // count are single bytes, and the round trip has to preserve all of
        // them in place — the client's placement keys on exactly these.
        let cmd = ControlChannelCmd::CreateDataChannelForStripe(
            ServiceId::new(0xdead_beef),
            0x0102_0304u32.to_be_bytes(),
            2,
            4,
        );
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(
            format!("{back:?}"),
            format!("{cmd:?}"),
            "the stripe request changed on the wire"
        );
    }

    /// Every command's length is value-independent, and the two that carry a
    /// service id carry a fixed 4-byte one: a tag-dispatched reader stays in
    /// sync whatever the value is.
    #[test]
    fn control_cmd_widths_are_value_independent() {
        let reserved = postcard::to_stdvec(&ControlChannelCmd::CreateDataChannel).unwrap();
        let beat = postcard::to_stdvec(&ControlChannelCmd::HeartBeat).unwrap();
        assert_eq!(reserved.len(), 1);
        assert_eq!(beat.len(), 1);
        assert_ne!(reserved[0], beat[0]);

        for raw in [0u32, 1, 0x7f, 0x80, 0xdead_beef, u32::MAX] {
            let for_service = postcard::to_stdvec(&ControlChannelCmd::CreateDataChannelFor(
                ServiceId::new(raw),
            ))
            .unwrap();
            let dropped =
                postcard::to_stdvec(&ControlChannelCmd::ServiceDropped(ServiceId::new(raw)))
                    .unwrap();
            assert_eq!(for_service.len(), 5, "service {raw:#x} changed the length");
            assert_eq!(dropped.len(), 5, "service {raw:#x} changed the length");
            assert_ne!(for_service[0], reserved[0]);
            assert_ne!(for_service[0], beat[0]);
            assert_ne!(for_service[0], dropped[0]);
        }

        // The stripe request is the widest session command, and it stays at
        // tag + 4 + 4 + 1 + 1 bytes whatever the group, index and count are:
        // that is what lets `read_control_cmd` consume exactly its suffix.
        for raw in [0u32, 0x80, u32::MAX] {
            for index in [0u8, 1, 0xff] {
                for count in [1u8, 0x80, 0xff] {
                    let bytes =
                        postcard::to_stdvec(&ControlChannelCmd::CreateDataChannelForStripe(
                            ServiceId::new(raw),
                            raw.to_be_bytes(),
                            index,
                            count,
                        ))
                        .unwrap();
                    assert_eq!(
                        bytes.len(),
                        11,
                        "stripe request {raw:#x}/{index}/{count} changed the length"
                    );
                    assert_eq!(bytes[0], 4, "the stripe request's tag moved");
                }
            }
        }
    }

    /// The service id's wire form is 4 raw bytes, so every command that carries
    /// one is fixed-width and a tag-dispatched reader stays in sync.
    #[test]
    fn service_id_is_four_raw_bytes() {
        for raw in [0u32, 1, 0x7f, 0x80, 0xdead_beef, u32::MAX] {
            let id = ServiceId::new(raw);
            assert_eq!(id.get(), raw);
            assert_eq!(id.to_string(), raw.to_string());
            let bytes = postcard::to_stdvec(&id).unwrap();
            assert_eq!(bytes.len(), 4);
            assert_eq!(bytes, raw.to_be_bytes());
        }
    }

    #[test]
    fn session_cmd_roundtrip() {
        let cmd = SessionCmd::Register(SessionRegistration {
            service_id: ServiceId::new(3),
            auth: sample_digest(9),
            reg: ServiceRegistration {
                name: "ssh".to_string(),
                service_type: crate::config::ServiceType::Tcp,
                bind_addr: sample_addr(),
                carrier: Carrier::Tcp,
                udp_buffer_size: 2048,
            },
        });
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        assert!(bytes.len() <= MAX_SESSION_CMD_LEN);
        let SessionCmd::Register(back) = postcard::from_bytes::<SessionCmd>(&bytes).unwrap() else {
            panic!("expected a registration");
        };
        assert_eq!(back.service_id, ServiceId::new(3));
        assert_eq!(back.reg.name, "ssh");
        assert_eq!(back.reg.bind_addr, sample_addr());

        let cmd = SessionCmd::Deregister(ServiceId::new(0xdead_beef));
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        assert!(matches!(
            postcard::from_bytes::<SessionCmd>(&bytes).unwrap(),
            SessionCmd::Deregister(id) if id == ServiceId::new(0xdead_beef)
        ));
    }

    /// Tag 0 belongs to a command no v4 build speaks, and that is what keeps
    /// the session's first-byte disambiguation working. Pin the tag range
    /// here rather than where the dispatch lives: the numbering is postcard's,
    /// so reordering the enum silently moves every command's tag.
    #[test]
    fn command_tags_stay_out_of_the_ack_frame_range() {
        let tag = |cmd: &ControlChannelCmd| postcard::to_stdvec(cmd).unwrap()[0];
        assert_eq!(tag(&ControlChannelCmd::CreateDataChannel), 0);
        assert_eq!(tag(&ControlChannelCmd::HeartBeat), 1);
        assert_eq!(
            tag(&ControlChannelCmd::CreateDataChannelFor(ServiceId::new(0))),
            2
        );
        assert_eq!(
            tag(&ControlChannelCmd::ServiceDropped(ServiceId::new(0))),
            3
        );
        assert_eq!(
            tag(&ControlChannelCmd::CreateDataChannelForStripe(
                ServiceId::new(0),
                [0; 4],
                0,
                1
            )),
            4,
            "the stripe request must keep tag 4: a new variant inserted before \
             it would move every later command's tag"
        );
    }

    #[cfg(feature = "client")]
    #[tokio::test]
    async fn read_control_cmd_dispatches_every_variant() {
        use tokio::io::duplex;

        for cmd in [
            ControlChannelCmd::CreateDataChannel,
            ControlChannelCmd::HeartBeat,
            ControlChannelCmd::CreateDataChannelFor(ServiceId::new(0xdead_beef)),
            ControlChannelCmd::ServiceDropped(ServiceId::new(0)),
            ControlChannelCmd::CreateDataChannelForStripe(
                ServiceId::new(0xdead_beef),
                [0xde, 0xad, 0xbe, 0xef],
                3,
                4,
            ),
        ] {
            let (mut tx, mut rx) = duplex(64);
            tx.write_all(&postcard::to_stdvec(&cmd).unwrap())
                .await
                .unwrap();
            let back = read_control_cmd(&mut rx).await.unwrap();
            assert_eq!(
                format!("{back:?}"),
                format!("{cmd:?}"),
                "read_control_cmd changed the command on the wire"
            );
        }
    }

    /// An unknown tag is a protocol error, not a silently reframed payload: a
    /// peer that does not know a v4 command must fail loudly.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn read_control_cmd_rejects_an_unknown_tag() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(64);
        tx.write_all(&[0x7f]).await.unwrap();
        let err = read_control_cmd(&mut rx).await.unwrap_err();
        assert!(format!("{err:#}").contains("Unknown control channel command tag"));
    }

    #[tokio::test]
    async fn read_hello_answers_with_the_version_it_read() {
        use tokio::io::duplex;

        for version in SUPPORTED_PROTO_VERSIONS {
            let (mut tx, mut rx) = duplex(64);
            let hello = Hello::ControlChannelHello(version, sample_digest(1));
            tx.write_all(&postcard::to_stdvec(&hello).unwrap())
                .await
                .unwrap();
            let (read, back) = read_hello(&mut rx).await.unwrap();
            assert_eq!(read, version);
            assert!(matches!(back, Hello::ControlChannelHello(v, _) if v == version));
        }
    }

    #[tokio::test]
    async fn read_hello_rejects_an_unsupported_version() {
        use tokio::io::duplex;

        for version in [0u8, 2, 6, 99] {
            let (mut tx, mut rx) = duplex(64);
            let hello = Hello::ControlChannelHello(version, sample_digest(1));
            tx.write_all(&postcard::to_stdvec(&hello).unwrap())
                .await
                .unwrap();
            let err = read_hello(&mut rx).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("Protocol version mismatched"),
                "unexpected error for version {version}: {msg}"
            );
        }
    }

    /// The prologue is exactly four raw bytes, written in big-endian order,
    /// and nothing else precedes a v4 stream's first byte.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn stream_prologue_is_four_raw_bytes() {
        use tokio::io::duplex;
        use tokio::time::{Duration, timeout};

        let (mut tx, mut rx) = duplex(64);
        let id = ServiceId::new(0xdead_beef);
        write_stream_prologue(&mut tx, id).await.unwrap();

        let mut raw = [0u8; 4];
        rx.read_exact(&mut raw).await.unwrap();
        assert_eq!(raw, [0xde, 0xad, 0xbe, 0xef], "the id must be big-endian");

        // Exactly four: a fifth byte would shift every payload that follows.
        let mut extra = [0u8; 1];
        assert!(
            timeout(Duration::from_millis(50), rx.read(&mut extra))
                .await
                .is_err(),
            "the prologue must be exactly four bytes"
        );
    }

    /// A round trip preserves the id, which is all the data plane needs to
    /// route a stream to its service.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn stream_prologue_round_trips() {
        use tokio::io::duplex;

        for raw in [0u32, 1, 0xdead_beef, u32::MAX] {
            let (mut tx, mut rx) = duplex(64);
            let id = ServiceId::new(raw);
            write_stream_prologue(&mut tx, id).await.unwrap();
            assert_eq!(read_stream_prologue(&mut rx).await.unwrap(), id);
        }
    }

    /// The data-channel handshake: the hello stays 34 bytes, and the four
    /// service bytes follow it — which is where the server reads them.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn data_channel_carries_the_service_after_the_hello() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(128);
        let nonce = sample_digest(9);
        let id = ServiceId::new(0x0102_0304);
        let hello =
            postcard::to_stdvec(&Hello::DataChannelHello(CURRENT_PROTO_VERSION, nonce)).unwrap();
        assert_eq!(hello.len(), PacketLength::new().hello);
        tx.write_all(&hello).await.unwrap();
        write_stream_prologue(&mut tx, id).await.unwrap();

        let (version, back) = read_hello(&mut rx).await.unwrap();
        assert_eq!(version, CURRENT_PROTO_VERSION);
        assert!(matches!(back, Hello::DataChannelHello(_, n) if n == nonce));
        // Only now, and only after the hello, does the server read the
        // prologue: it is what names the service the channel carries.
        assert_eq!(read_stream_prologue(&mut rx).await.unwrap(), id);
    }

    /// The hello frame is 34 bytes in every dialect: v3 puts a service digest in
    /// the second slot, v4 a random session tag, and neither changes the length
    /// — which is what lets an old server read a v4 hello far enough to refuse
    /// it by version instead of by a framing error.
    #[test]
    fn hello_width_is_version_independent() {
        let v3 = postcard::to_stdvec(&Hello::ControlChannelHello(3, sample_digest(1))).unwrap();
        let v4 = postcard::to_stdvec(&Hello::ControlChannelHello(
            PROTO_V4_VERSION,
            sample_digest(2),
        ))
        .unwrap();
        assert_eq!(v3.len(), 34);
        assert_eq!(v4.len(), 34);
        assert_eq!(PacketLength::new().hello, 34);
    }

    #[test]
    fn data_cmd_roundtrip() {
        // StartForwardTcp
        let cmd = DataChannelCmd::StartForwardTcp;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: DataChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, DataChannelCmd::StartForwardTcp));

        // StartForwardUdp
        let cmd = DataChannelCmd::StartForwardUdp;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: DataChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, DataChannelCmd::StartForwardUdp));
    }

    #[test]
    fn plain_data_cmds_keep_their_one_byte_wire_form() {
        // The plain commands' single-byte tag is the wire-compat guarantee
        // for peers that predate the striped variant: the reader dispatches
        // on the tag, so a plain command can never be mistaken for one that
        // carries a suffix.
        let tcp = postcard::to_stdvec(&DataChannelCmd::StartForwardTcp).unwrap();
        let udp = postcard::to_stdvec(&DataChannelCmd::StartForwardUdp).unwrap();
        assert_eq!(tcp.len(), 1);
        assert_eq!(udp.len(), 1);
        assert_ne!(tcp[0], udp[0]);

        // The striped command is fixed 7 bytes for every group id: the
        // group id is 4 wire bytes, not a varint u32.
        for group in [0u32, 1, 0x7f, 0x80, 0xdead_beef, u32::MAX] {
            let bytes = postcard::to_stdvec(&DataChannelCmd::StartForwardStripedTcp(
                group.to_be_bytes(),
                3,
                4,
            ))
            .unwrap();
            assert_eq!(
                bytes.len(),
                7,
                "group {group:#x} changed the command length"
            );
        }
    }

    #[cfg(feature = "client")]
    #[tokio::test]
    async fn read_data_cmd_dispatches_every_variant() {
        use tokio::io::duplex;

        for cmd in [
            DataChannelCmd::StartForwardTcp,
            DataChannelCmd::StartForwardUdp,
            DataChannelCmd::StartForwardStripedTcp(0xdead_beefu32.to_be_bytes(), 3, 4),
        ] {
            let (mut tx, mut rx) = duplex(64);
            tx.write_all(&postcard::to_stdvec(&cmd).unwrap())
                .await
                .unwrap();
            let back = read_data_cmd(&mut rx).await.unwrap();
            let a = postcard::to_stdvec(&cmd).unwrap();
            let b = postcard::to_stdvec(&back).unwrap();
            assert_eq!(a, b, "read_data_cmd changed the command on the wire");
        }
    }

    #[test]
    fn udp_header_roundtrip() {
        let hdr = UdpHeader {
            from: sample_addr(),
            len: 42,
        };
        let bytes = postcard::to_stdvec(&hdr).unwrap();
        assert!(bytes.len() <= MAX_UDP_HEADER_LEN);
        let back: UdpHeader = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.from, sample_addr());
        assert_eq!(back.len, 42);
    }

    #[tokio::test]
    async fn udp_frame_roundtrip() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(64 * 1024);
        let mut scratch = BytesMut::new();

        UdpTraffic::write_frame(&mut tx, &mut scratch, sample_addr(), b"hello")
            .await
            .unwrap();

        // The whole datagram must be emitted as a single buffer: one length
        // prefix byte plus the encoded header plus the payload.
        let hdr_len = scratch[0] as usize;
        assert!(hdr_len > 0);
        assert_eq!(scratch.len(), 1 + hdr_len + b"hello".len());

        let hdr_len = rx.read_u8().await.unwrap();
        let packet = UdpTraffic::read(
            &mut rx,
            hdr_len,
            crate::common::constants::DEFAULT_UDP_BUFFER_SIZE,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(packet.from, sample_addr());
        assert_eq!(&packet.data[..], b"hello");
    }

    #[tokio::test]
    async fn udp_oversized_packet_is_dropped_without_desync() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(128 * 1024);
        let mut scratch = BytesMut::new();

        // A normal frame, then one claiming a payload above UDP_BUFFER_SIZE,
        // then another normal frame. The receiver must drop only the middle
        // one and stay in sync with the stream.
        UdpTraffic::write_frame(&mut tx, &mut scratch, sample_addr(), b"first")
            .await
            .unwrap();

        let oversized_len =
            u16::try_from(crate::common::constants::DEFAULT_UDP_BUFFER_SIZE).unwrap() + 1;
        let hdr = UdpHeader {
            from: sample_addr(),
            len: oversized_len,
        };
        let encoded = postcard::to_stdvec(&hdr).unwrap();
        tx.write_u8(u8::try_from(encoded.len()).unwrap())
            .await
            .unwrap();
        tx.write_all(&encoded).await.unwrap();
        tx.write_all(&vec![0u8; oversized_len as usize])
            .await
            .unwrap();

        UdpTraffic::write_frame(&mut tx, &mut scratch, sample_addr(), b"last")
            .await
            .unwrap();

        let hdr_len = rx.read_u8().await.unwrap();
        let first = UdpTraffic::read(
            &mut rx,
            hdr_len,
            crate::common::constants::DEFAULT_UDP_BUFFER_SIZE,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&first.data[..], b"first");

        let hdr_len = rx.read_u8().await.unwrap();
        assert!(
            UdpTraffic::read(
                &mut rx,
                hdr_len,
                crate::common::constants::DEFAULT_UDP_BUFFER_SIZE
            )
            .await
            .unwrap()
            .is_none()
        );

        let hdr_len = rx.read_u8().await.unwrap();
        let last = UdpTraffic::read(
            &mut rx,
            hdr_len,
            crate::common::constants::DEFAULT_UDP_BUFFER_SIZE,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&last.data[..], b"last");
    }

    #[tokio::test]
    async fn udp_read_slice_roundtrip_zero_alloc_path() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(64 * 1024);
        let mut scratch = BytesMut::new();

        UdpTraffic::write_frame(&mut tx, &mut scratch, sample_addr(), &[7u8; 100])
            .await
            .unwrap();

        let hdr_len = rx.read_u8().await.unwrap();
        let mut payload = BytesMut::new();
        let (from, len) = UdpTraffic::read_slice(
            &mut rx,
            hdr_len,
            &mut payload,
            crate::common::constants::DEFAULT_UDP_BUFFER_SIZE,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(from, sample_addr());
        assert_eq!(len, 100);
        assert_eq!(&payload[..len], &[7u8; 100]);
    }

    #[test]
    fn digest_is_32_bytes() {
        let d = digest(b"hello");
        assert_eq!(d.len(), HASH_WIDTH_IN_BYTES);
    }

    #[test]
    fn packet_lengths_are_stable() {
        let len = PacketLength::new();
        // The fixed-width frames, pinned to their real widths so a protocol
        // change cannot slip through as "still greater than zero". Anything
        // variable-length is tag-dispatched and must NOT appear here.
        assert_eq!(len.hello, 34);
        assert_eq!(len.ack, 1);
        assert_eq!(len.auth, 32);
    }

    /// The session command frame is the same u16-length-prefixed shape the
    /// registration uses, so an over-long command is refused before it is
    /// written rather than desynchronizing the session.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn session_cmd_frame_roundtrip() {
        use tokio::io::duplex;

        let cmd = SessionCmd::Register(SessionRegistration {
            service_id: ServiceId::new(1),
            auth: sample_digest(5),
            reg: ServiceRegistration {
                name: "a-service-with-a-long-enough-name".to_string(),
                service_type: crate::config::ServiceType::Tcp,
                bind_addr: sample_addr(),
                carrier: Carrier::Tcp,
                udp_buffer_size: 2048,
            },
        });
        let (mut tx, mut rx) = duplex(4096);
        write_session_cmd(&mut tx, &cmd).await.unwrap();
        let back = read_session_cmd(&mut rx).await.unwrap();
        assert_eq!(format!("{back:?}"), format!("{cmd:?}"));

        // A length prefix beyond the cap is refused without reading the body.
        let (mut tx, mut rx) = duplex(64);
        let over_cap = u16::try_from(MAX_SESSION_CMD_LEN).unwrap() + 1;
        tx.write_u16(over_cap).await.unwrap();
        let err = read_session_cmd(&mut rx).await.unwrap_err();
        assert!(format!("{err:#}").contains("too large"));
    }

    /// The transparent data channel's framing: `[u16 length][packet]`, and the
    /// packet comes back byte for byte -- including the header, which is the
    /// whole point of this service type.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn ip_frame_roundtrip_preserves_the_packet() {
        use tokio::io::duplex;

        // A minimal but well-formed IPv4 header (20 bytes) plus payload.
        let mut packet = vec![
            0x45, 0x00, 0x00, 0x28, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 0, 2,
        ];
        packet.extend_from_slice(&[10, 0, 0, 1]);
        packet.extend_from_slice(b"payload");

        let (mut tx, mut rx) = duplex(4096);
        // A batch of one here; the writer hands over whole batches.
        let mut batch = BytesMut::new();
        IpTraffic::encode_into(&mut batch, &packet).unwrap();
        tx.write_all(&batch).await.unwrap();

        let mut frames = IpFrames::new(&mut rx);
        let got = frames.next().await.unwrap();
        assert_eq!(got, &packet[..], "the packet must survive verbatim");
    }

    /// A frame longer than the reader is willing to buffer is refused by the
    /// length prefix, and a zero-length frame is a corrupt stream — neither may
    /// be handed to the caller as a packet.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn ip_frame_rejects_empty_and_oversized() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(64);
        tx.write_u16(0).await.unwrap();
        let mut frames = IpFrames::new(&mut rx);
        let err = frames.next().await.unwrap_err();
        assert!(format!("{err:#}").contains("Empty IP frame"));

        // One byte past the wire-format ceiling must be refused while the
        // batch is built, before anything reaches the channel.
        let oversized = vec![0u8; usize::from(u16::MAX) + 1];
        let mut batch = BytesMut::new();
        let err = IpTraffic::encode_into(&mut batch, &oversized).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds the wire format limit"));
        assert!(
            batch.is_empty(),
            "a refused packet must not leave a partial frame in the batch"
        );
    }

    /// A truncated frame (the writer died mid-packet) surfaces as an error, so
    /// the channel is torn down instead of injecting half a packet.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn ip_frame_rejects_a_truncated_packet() {
        use tokio::io::duplex;

        let (mut tx, mut rx) = duplex(4096);
        tx.write_u16(20).await.unwrap();
        tx.write_all(&[0x45; 8]).await.unwrap();
        drop(tx);

        let mut frames = IpFrames::new(&mut rx);
        let err = frames.next().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("ended mid-frame"),
            "a half frame must be named as one, got: {err:#}"
        );
    }

    /// A run of frames written together is read back as those frames, one at a
    /// time: batching changes how many syscalls move them, not the format.
    #[cfg(all(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn a_run_of_frames_reads_back_frame_by_frame() {
        use tokio::io::AsyncWriteExt;

        let packets: Vec<Vec<u8>> = (0..3).map(|i| vec![0x45; 20 + i * 100]).collect();
        let mut batch = BytesMut::new();
        for packet in &packets {
            IpTraffic::encode_into(&mut batch, packet).unwrap();
        }

        let (mut tx, mut rx) = tokio::io::duplex(4096);
        tx.write_all(&batch).await.unwrap();
        drop(tx);

        let mut frames = IpFrames::new(&mut rx);
        for expected in &packets {
            assert_eq!(frames.next().await.unwrap(), &expected[..]);
        }
        assert!(
            frames.next().await.is_err(),
            "the run ended, so the next read is the end of the stream"
        );
    }
}
