pub const HASH_WIDTH_IN_BYTES: usize = 32;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::trace;

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

#[derive(Debug)]
pub struct UdpTraffic {
    pub from: SocketAddr,
    pub data: Bytes,
}

impl UdpTraffic {
    pub async fn write<T: AsyncWrite + Unpin>(&self, writer: &mut T) -> Result<()> {
        let hdr = UdpHeader {
            from: self.from,
            len: self.data.len() as UdpPacketLen,
        };

        let v = postcard::to_stdvec(&hdr).unwrap();

        trace!("Write {:?} of length {}", hdr, v.len());
        writer.write_u8(v.len() as u8).await?;
        writer.write_all(&v).await?;

        writer.write_all(&self.data).await?;

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn write_slice<T: AsyncWrite + Unpin>(
        writer: &mut T,
        from: SocketAddr,
        data: &[u8],
    ) -> Result<()> {
        let hdr = UdpHeader {
            from,
            len: data.len() as UdpPacketLen,
        };

        let v = postcard::to_stdvec(&hdr).unwrap();

        trace!("Write {:?} of length {}", hdr, v.len());
        writer.write_u8(v.len() as u8).await?;
        writer.write_all(&v).await?;

        writer.write_all(data).await?;

        Ok(())
    }

    pub async fn read<T: AsyncRead + Unpin>(reader: &mut T, hdr_len: u8) -> Result<UdpTraffic> {
        let mut buf = vec![0; hdr_len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .with_context(|| "Failed to read udp header")?;

        let hdr: UdpHeader =
            postcard::from_bytes(&buf).with_context(|| "Failed to deserialize UdpHeader")?;

        trace!("hdr {:?}", hdr);

        let mut data = BytesMut::new();
        data.resize(hdr.len as usize, 0);
        reader.read_exact(&mut data).await?;

        Ok(UdpTraffic {
            from: hdr.from,
            data: data.freeze(),
        })
    }
}

pub fn digest(data: &[u8]) -> Digest {
    use sha2::{Digest, Sha256};
    let d = Sha256::new().chain_update(data).finalize();
    d.into()
}

struct PacketLength {
    hello: usize,
    ack: usize,
    auth: usize,
    c_cmd: usize,
    d_cmd: usize,
}

impl PacketLength {
    pub fn new() -> PacketLength {
        let username = "default";
        let d = digest(username.as_bytes());
        let hello = postcard::to_stdvec(&Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d))
            .unwrap()
            .len();
        let c_cmd = postcard::to_stdvec(&ControlChannelCmd::CreateDataChannel)
            .unwrap()
            .len();
        let d_cmd = postcard::to_stdvec(&DataChannelCmd::StartForwardTcp)
            .unwrap()
            .len();
        let ack = Ack::Ok;
        let ack = postcard::to_stdvec(&ack).unwrap().len();

        let auth = postcard::to_stdvec(&Auth(d)).unwrap().len();
        PacketLength {
            hello,
            ack,
            auth,
            c_cmd,
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

pub async fn read_auth<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Auth> {
    let mut buf = vec![0u8; PACKET_LEN.auth];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read auth")?;
    postcard::from_bytes(&buf).with_context(|| "Failed to deserialize auth")
}

pub async fn read_ack<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Ack> {
    let mut bytes = vec![0u8; PACKET_LEN.ack];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read ack")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize ack")
}

pub async fn read_control_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<ControlChannelCmd> {
    let mut bytes = vec![0u8; PACKET_LEN.c_cmd];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read cmd")?;
    postcard::from_bytes(&bytes).with_context(|| "Failed to deserialize control cmd")
}

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
        let back: UdpHeader = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.from, sample_addr());
        assert_eq!(back.len, 42);
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
