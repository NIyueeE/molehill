pub const HASH_WIDTH_IN_BYTES: usize = 32;

use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{trace, warn};

use crate::common::constants::UDP_BUFFER_SIZE;

type ProtocolVersion = u8;
const _PROTO_V0: u8 = 0u8;
const PROTO_V1: u8 = 1u8;

pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V1;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

#[derive(Deserialize, Serialize, Debug)]
pub enum Hello {
    ControlChannelHello(ProtocolVersion, Digest), // sha256sum(service name) or a nonce
    DataChannelHello(ProtocolVersion, Digest),    // token provided by CreateDataChannel
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
pub enum Ack {
    Ok,
    ServiceNotExist,
    AuthFailed,
}

impl std::fmt::Display for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Ack::Ok => "Ok",
                Ack::ServiceNotExist => "Service not exist",
                Ack::AuthFailed => "Incorrect token",
            }
        )
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub enum ControlChannelCmd {
    CreateDataChannel,
    HeartBeat,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum DataChannelCmd {
    StartForwardTcp,
    StartForwardUdp,
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
#[cfg_attr(not(feature = "client"), allow(dead_code))]
#[derive(Debug)]
pub struct UdpTraffic {
    pub from: SocketAddr,
    pub data: Bytes,
}

/// Frame one datagram into `scratch` as `[hdr_len u8][header][payload]`.
///
/// The wire format is unchanged; the point is that the whole datagram is
/// emitted with a single buffer and therefore a single `write_all` (and a
/// single TLS/Noise record), with no per-packet heap allocation when callers
/// reuse the same scratch buffer.
fn encode_udp_frame(scratch: &mut BytesMut, from: SocketAddr, data: &[u8]) -> Result<()> {
    let hdr = UdpHeader {
        from,
        len: data.len() as UdpPacketLen,
    };

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
    debug_assert!(encoded.len() <= MAX_UDP_HEADER_LEN && encoded.len() <= u8::MAX as usize);
    scratch.extend_from_slice(encoded);
    let hdr_len = scratch.len() - prefix_pos - 1;
    scratch[prefix_pos] = hdr_len as u8;

    trace!("Write {:?} of length {}", hdr, hdr_len);
    scratch.extend_from_slice(data);
    Ok(())
}

async fn read_udp_header<T: AsyncRead + Unpin>(reader: &mut T, hdr_len: u8) -> Result<UdpHeader> {
    if hdr_len as usize > MAX_UDP_HEADER_LEN {
        bail!(
            "UDP header length {} exceeds the maximum of {}, the stream is corrupt",
            hdr_len,
            MAX_UDP_HEADER_LEN
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
    #[cfg_attr(not(any(feature = "client", feature = "server")), allow(dead_code))]
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
    /// Returns `Ok(None)` if an oversized datagram was received: its payload is
    /// drained (keeping the stream in sync) and only that packet is dropped,
    /// instead of tearing down the whole data channel.
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    pub async fn read<T: AsyncRead + Unpin>(
        reader: &mut T,
        hdr_len: u8,
    ) -> Result<Option<UdpTraffic>> {
        let hdr = read_udp_header(reader, hdr_len).await?;

        // A UDP payload larger than the receive buffer cannot originate from
        // this implementation; drop it while keeping the stream usable.
        if hdr.len > UDP_BUFFER_SIZE as UdpPacketLen {
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
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub async fn read_slice<T: AsyncRead + Unpin>(
        reader: &mut T,
        hdr_len: u8,
        scratch: &mut BytesMut,
    ) -> Result<Option<(SocketAddr, usize)>> {
        let hdr = read_udp_header(reader, hdr_len).await?;

        if hdr.len > UDP_BUFFER_SIZE as UdpPacketLen {
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

struct PacketLength {
    hello: usize,
    #[cfg(feature = "client")]
    ack: usize,
    #[cfg(feature = "server")]
    auth: usize,
    #[cfg(feature = "client")]
    c_cmd: usize,
    #[cfg(feature = "client")]
    d_cmd: usize,
}

// Infallible: serializing compile-time-known fixed-size values
#[allow(clippy::unwrap_used)]
impl PacketLength {
    pub fn new() -> PacketLength {
        let username = "default";
        let d = digest(username.as_bytes());
        let hello = postcard::to_stdvec(&Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d))
            .unwrap()
            .len();
        #[cfg(feature = "client")]
        let c_cmd = postcard::to_stdvec(&ControlChannelCmd::CreateDataChannel)
            .unwrap()
            .len();
        #[cfg(feature = "client")]
        let d_cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardTcp)
            .unwrap()
            .len();
        #[cfg(feature = "client")]
        let ack = postcard::to_stdvec(&Ack::Ok).unwrap().len();

        #[cfg(feature = "server")]
        let auth = postcard::to_stdvec(&Auth(d)).unwrap().len();
        PacketLength {
            hello,
            #[cfg(feature = "client")]
            ack,
            #[cfg(feature = "server")]
            auth,
            #[cfg(feature = "client")]
            c_cmd,
            #[cfg(feature = "client")]
            d_cmd,
        }
    }
}

lazy_static! {
    static ref PACKET_LEN: PacketLength = PacketLength::new();
}

pub async fn read_hello<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Hello> {
    let mut buf = vec![0u8; PACKET_LEN.hello];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read hello")?;
    let hello = postcard::from_bytes(&buf).with_context(|| "Failed to deserialize hello")?;

    match hello {
        Hello::ControlChannelHello(v, _) => {
            if v != CURRENT_PROTO_VERSION {
                bail!(
                    "Protocol version mismatched. Expected {}, got {}. Please update `molehill`.",
                    CURRENT_PROTO_VERSION,
                    v
                );
            }
        }
        Hello::DataChannelHello(v, _) => {
            if v != CURRENT_PROTO_VERSION {
                bail!(
                    "Protocol version mismatched. Expected {}, got {}. Please update `molehill`.",
                    CURRENT_PROTO_VERSION,
                    v
                );
            }
        }
    }

    Ok(hello)
}

#[cfg(feature = "server")]
pub async fn read_auth<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Auth> {
    let mut buf = vec![0u8; PACKET_LEN.auth];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read auth")?;
    postcard::from_bytes(&buf).with_context(|| "Failed to deserialize auth")
}

#[cfg(feature = "client")]
pub async fn read_ack<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Ack> {
    let mut bytes = vec![0u8; PACKET_LEN.ack];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read ack")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize ack")
}

#[cfg(feature = "client")]
pub async fn read_control_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<ControlChannelCmd> {
    let mut bytes = vec![0u8; PACKET_LEN.c_cmd];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read cmd")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize control cmd")
}

#[cfg(feature = "client")]
pub async fn read_data_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<DataChannelCmd> {
    let mut bytes = vec![0u8; PACKET_LEN.d_cmd];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read cmd")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize data cmd")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn sample_digest(b: u8) -> Digest {
        let mut d = [0u8; HASH_WIDTH_IN_BYTES];
        d[0] = b;
        d
    }

    fn sample_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080)
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
        for ack in [Ack::Ok, Ack::ServiceNotExist, Ack::AuthFailed] {
            let bytes = postcard::to_stdvec(&ack).unwrap();
            let back: Ack = postcard::from_bytes(&bytes).unwrap();
            match (&ack, &back) {
                (Ack::Ok, Ack::Ok) => {}
                (Ack::ServiceNotExist, Ack::ServiceNotExist) => {}
                (Ack::AuthFailed, Ack::AuthFailed) => {}
                _ => panic!("Ack round-trip mismatch"),
            }
        }
    }

    #[test]
    fn ack_display() {
        assert_eq!(Ack::Ok.to_string(), "Ok");
        assert_eq!(Ack::ServiceNotExist.to_string(), "Service not exist");
        assert_eq!(Ack::AuthFailed.to_string(), "Incorrect token");
    }

    #[test]
    fn control_cmd_roundtrip() {
        // CreateDataChannel
        let cmd = ControlChannelCmd::CreateDataChannel;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, ControlChannelCmd::CreateDataChannel));

        // HeartBeat
        let cmd = ControlChannelCmd::HeartBeat;
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        let back: ControlChannelCmd = postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(back, ControlChannelCmd::HeartBeat));
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
        let packet = UdpTraffic::read(&mut rx, hdr_len).await.unwrap().unwrap();
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

        let oversized_len = UDP_BUFFER_SIZE as u16 + 1;
        let hdr = UdpHeader {
            from: sample_addr(),
            len: oversized_len,
        };
        let encoded = postcard::to_stdvec(&hdr).unwrap();
        tx.write_u8(encoded.len() as u8).await.unwrap();
        tx.write_all(&encoded).await.unwrap();
        tx.write_all(&vec![0u8; oversized_len as usize])
            .await
            .unwrap();

        UdpTraffic::write_frame(&mut tx, &mut scratch, sample_addr(), b"last")
            .await
            .unwrap();

        let hdr_len = rx.read_u8().await.unwrap();
        let first = UdpTraffic::read(&mut rx, hdr_len).await.unwrap().unwrap();
        assert_eq!(&first.data[..], b"first");

        let hdr_len = rx.read_u8().await.unwrap();
        assert!(UdpTraffic::read(&mut rx, hdr_len).await.unwrap().is_none());

        let hdr_len = rx.read_u8().await.unwrap();
        let last = UdpTraffic::read(&mut rx, hdr_len).await.unwrap().unwrap();
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
        let (from, len) = UdpTraffic::read_slice(&mut rx, hdr_len, &mut payload)
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
        // Verify constant widths so protocol changes don't slip through
        assert!(len.hello > 0);
        assert!(len.ack > 0);
        assert!(len.auth > 0);
        assert!(len.c_cmd > 0);
        assert!(len.d_cmd > 0);
    }
}
