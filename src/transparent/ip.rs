//! Reading an IP packet's routed destination, without allocating.
//!
//! Only what routing needs: the destination address and — when the packet
//! carries one — its destination port. IPv4 only for now; anything else is
//! reported as a typed reason instead of being parsed halfway.

use std::net::{IpAddr, Ipv4Addr};

pub const TCP: u8 = 6;
pub const UDP: u8 = 17;

/// The fixed part of an IPv4 header.
pub const IPV4_MIN_HEADER: usize = 20;

/// Why a packet cannot be routed at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// Not IPv4 (IPv6 is not carried yet).
    NotIpv4,
    /// Too short, a header length that runs past the packet, or a total length
    /// that disagrees with the bytes read.
    Malformed,
}

/// What one packet says about where it is going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketInfo {
    pub protocol: u8,
    pub src: IpAddr,
    pub dst: IpAddr,
    /// `None` for ICMP and for the fragments after the first, which carry no
    /// transport header the router could read.
    pub src_port: Option<u16>,
    pub dst_port: Option<u16>,
}

/// Parse the routing fields of one packet.
///
/// The length checks matter: a packet off a TUN device is attacker-influenced
/// (the visitor's), so every offset read here is bounded before it is taken.
pub fn parse(packet: &[u8]) -> Result<PacketInfo, ParseError> {
    let Some(&first) = packet.first() else {
        return Err(ParseError::Malformed);
    };
    if first >> 4 != 4 {
        return Err(ParseError::NotIpv4);
    }
    if packet.len() < IPV4_MIN_HEADER {
        return Err(ParseError::Malformed);
    }

    let header_len = usize::from(first & 0x0f) * 4;
    if header_len < IPV4_MIN_HEADER || header_len > packet.len() {
        return Err(ParseError::Malformed);
    }
    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if total_len < header_len || total_len > packet.len() {
        return Err(ParseError::Malformed);
    }

    let protocol = packet[9];
    let src = IpAddr::V4(Ipv4Addr::new(
        packet[12], packet[13], packet[14], packet[15],
    ));
    let dst = IpAddr::V4(Ipv4Addr::new(
        packet[16], packet[17], packet[18], packet[19],
    ));

    let fragment_offset = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff;
    let (src_port, dst_port) = if fragment_offset == 0 && matches!(protocol, TCP | UDP) {
        // The transport header starts at `header_len`; a packet that stops
        // before its ports is malformed rather than portless.
        if packet.len() < header_len + 4 {
            return Err(ParseError::Malformed);
        }
        (
            Some(u16::from_be_bytes([
                packet[header_len],
                packet[header_len + 1],
            ])),
            Some(u16::from_be_bytes([
                packet[header_len + 2],
                packet[header_len + 3],
            ])),
        )
    } else {
        (None, None)
    };

    Ok(PacketInfo {
        protocol,
        src,
        dst,
        src_port,
        dst_port,
    })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    /// ICMP: no transport header to route by, which is the whole point of the
    /// portless case. (The constant is test vocabulary: the routing code never
    /// names it.)
    const ICMP: u8 = 1;

    /// A 20-byte IPv4 header plus `payload` bytes of transport header.
    fn packet(protocol: u8, dst: [u8; 4], flags_offset: u16, payload: &[u8]) -> Vec<u8> {
        let total = IPV4_MIN_HEADER + payload.len();
        let mut p = vec![0u8; IPV4_MIN_HEADER];
        p[0] = 0x45;
        p[1] = 0;
        p[2..4].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
        p[6..8].copy_from_slice(&flags_offset.to_be_bytes());
        p[8] = 64;
        p[9] = protocol;
        p[12..16].copy_from_slice(&[10, 0, 0, 2]);
        p[16..20].copy_from_slice(&dst);
        p.extend_from_slice(payload);
        p
    }

    /// Source port, destination port.
    fn ports(src: u16, dst: u16) -> [u8; 4] {
        let mut out = [0u8; 4];
        out[..2].copy_from_slice(&src.to_be_bytes());
        out[2..].copy_from_slice(&dst.to_be_bytes());
        out
    }

    #[test]
    fn tcp_and_udp_yield_their_destination_port() {
        let tcp = parse(&packet(TCP, [10, 99, 0, 1], 0, &ports(50000, 443))).unwrap();
        assert_eq!(tcp.dst, "10.99.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(tcp.src, "10.0.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(tcp.dst_port, Some(443));

        let udp = parse(&packet(UDP, [10, 99, 0, 1], 0, &ports(53, 5353))).unwrap();
        assert_eq!(udp.dst_port, Some(5353));
    }

    #[test]
    fn icmp_has_no_port_to_route_by() {
        let icmp = parse(&packet(ICMP, [10, 99, 0, 1], 0, &[8, 0, 0, 0])).unwrap();
        assert_eq!(icmp.protocol, ICMP);
        assert_eq!(icmp.dst_port, None);
    }

    #[test]
    fn a_later_fragment_carries_no_port() {
        // Fragment offset 1 (in 8-byte units) with more-fragments set: the
        // payload is not a transport header, whatever it looks like.
        let frag = parse(&packet(TCP, [10, 99, 0, 1], 0x2001, &ports(1, 2))).unwrap();
        assert_eq!(frag.dst_port, None);
    }

    #[test]
    fn ipv6_is_refused_by_version() {
        let mut v6 = vec![0u8; 40];
        v6[0] = 0x60;
        assert_eq!(parse(&v6), Err(ParseError::NotIpv4));
        assert_eq!(parse(&[]), Err(ParseError::Malformed));
    }

    #[test]
    fn length_lies_are_refused() {
        // Header length beyond the packet.
        let mut bad = packet(TCP, [10, 99, 0, 1], 0, &ports(1, 2));
        bad[0] = 0x4f;
        assert_eq!(parse(&bad), Err(ParseError::Malformed));

        // Total length beyond the bytes we were handed.
        let mut bad = packet(TCP, [10, 99, 0, 1], 0, &ports(1, 2));
        bad[2..4].copy_from_slice(&9000u16.to_be_bytes());
        assert_eq!(parse(&bad), Err(ParseError::Malformed));

        // Total length below the header.
        let mut bad = packet(TCP, [10, 99, 0, 1], 0, &ports(1, 2));
        bad[2..4].copy_from_slice(&8u16.to_be_bytes());
        assert_eq!(parse(&bad), Err(ParseError::Malformed));

        // A transport header that stops before its port.
        assert_eq!(
            parse(&packet(TCP, [10, 99, 0, 1], 0, &[0, 80])),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn header_options_shift_the_transport_header() {
        // IHL = 6: 24-byte header, so the ports start four bytes later.
        let mut p = packet(TCP, [10, 99, 0, 1], 0, &[0, 0, 0, 0, 0, 0, 0, 0]);
        p[0] = 0x46;
        // The destination port is the transport header's third and fourth
        // bytes, and the header is 24 bytes now.
        p[26..28].copy_from_slice(&443u16.to_be_bytes());
        // Recompute the total length for the extra header bytes.
        let total = u16::try_from(p.len()).unwrap();
        p[2..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(parse(&p).unwrap().dst_port, Some(443));
    }
}
