//! Reading an IP packet's routed destination, without allocating.
//!
//! Only what routing needs: the two addresses and — when the packet carries
//! one — the destination port. IPv4 and IPv6 are both carried; a version this
//! reader does not know is reported as a typed reason instead of parsed
//! halfway.
//!
//! IPv6 is not one fixed header: the routing fields live in the 40-byte base
//! header, but the transport header (and therefore the ports) can sit behind a
//! bounded chain of extension headers, and a fragment extension decides
//! whether a transport header is present at all. The walk below is bounded
//! (both by the packet length and by a header-count ceiling) because every
//! offset here is attacker-influenced.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const TCP: u8 = 6;
pub const UDP: u8 = 17;

/// The fixed part of an IPv4 header.
pub const IPV4_MIN_HEADER: usize = 20;
/// The fixed part of an IPv6 header.
pub const IPV6_MIN_HEADER: usize = 40;

/// The most extension headers one packet may chain before the transport
/// header. A legitimate packet needs at most a few; a longer chain is either
/// broken or an attempt to make this walk expensive.
const MAX_EXTENSION_HEADERS: usize = 16;

/// Why a packet cannot be routed at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// Not an IP version this path carries (neither 4 nor 6).
    NotIp,
    /// Too short, a header length that runs past the packet, or a total length
    /// that disagrees with the bytes read.
    Malformed,
}

/// What one packet says about where it is going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketInfo {
    /// The upper-layer protocol (TCP, UDP, ICMP, `ICMPv6`, ...).
    pub protocol: u8,
    pub src: IpAddr,
    pub dst: IpAddr,
    /// `None` for ICMP/ICMPv6 and for the non-first fragments of a datagram,
    /// which carry no transport header the router could read.
    pub src_port: Option<u16>,
    pub dst_port: Option<u16>,
}

/// Parse the routing fields of one IPv4 or IPv6 packet.
///
/// The length checks matter: a packet off a TUN device is attacker-influenced
/// (the visitor's), so every offset read here is bounded before it is taken.
pub fn parse(packet: &[u8]) -> Result<PacketInfo, ParseError> {
    let Some(&first) = packet.first() else {
        return Err(ParseError::Malformed);
    };
    match first >> 4 {
        4 => parse_v4(packet),
        6 => parse_v6(packet),
        _ => Err(ParseError::NotIp),
    }
}

fn parse_v4(packet: &[u8]) -> Result<PacketInfo, ParseError> {
    if packet.len() < IPV4_MIN_HEADER {
        return Err(ParseError::Malformed);
    }

    let header_len = usize::from(packet[0] & 0x0f) * 4;
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
        transport_ports(packet, header_len, total_len)?
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

fn parse_v6(packet: &[u8]) -> Result<PacketInfo, ParseError> {
    if packet.len() < IPV6_MIN_HEADER {
        return Err(ParseError::Malformed);
    }
    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let total_len = IPV6_MIN_HEADER + payload_len;
    // A zero payload length with bytes behind it is a jumbogram (or a lie).
    // Jumbograms are not carried: the TUN MTU and the frame ceiling are far
    // below 64 KiB, and a jumbo option would need a length source this reader
    // does not trust. Rejecting it is safer than guessing.
    if payload_len == 0 && packet.len() > IPV6_MIN_HEADER {
        return Err(ParseError::Malformed);
    }
    if total_len > packet.len() {
        return Err(ParseError::Malformed);
    }

    let src = IpAddr::V6(Ipv6Addr::from(
        <[u8; 16]>::try_from(&packet[8..24]).map_err(|_| ParseError::Malformed)?,
    ));
    let dst = IpAddr::V6(Ipv6Addr::from(
        <[u8; 16]>::try_from(&packet[24..40]).map_err(|_| ParseError::Malformed)?,
    ));

    let mut next = packet[6];
    let mut offset = IPV6_MIN_HEADER;
    let mut headers = 0usize;
    loop {
        headers += 1;
        if headers > MAX_EXTENSION_HEADERS {
            return Err(ParseError::Malformed);
        }
        match next {
            // Hop-by-Hop, Routing, Destination Options, Mobility, HIP, Shim6:
            // `[next_header][hdr_ext_len]`, with the length in 8-octet units
            // after the first 8 octets.
            0 | 43 | 60 | 135 | 139 | 140 => {
                let (header_next, len) = extension_len(packet, offset, total_len, false)?;
                next = header_next;
                offset += len;
            }
            // Authentication Header: `[next_header][payload_len]`, with the
            // length in 4-octet units after the first 8 octets.
            51 => {
                let (header_next, len) = extension_len(packet, offset, total_len, true)?;
                next = header_next;
                offset += len;
            }
            // Fragment: the first fragment still carries the transport header;
            // any later one does not, and its ports are unknowable here.
            44 => {
                if offset + 8 > total_len {
                    return Err(ParseError::Malformed);
                }
                let fragment_offset =
                    u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) >> 3;
                next = packet[offset];
                if fragment_offset != 0 {
                    return Ok(PacketInfo {
                        protocol: next,
                        src,
                        dst,
                        src_port: None,
                        dst_port: None,
                    });
                }
                offset += 8;
            }
            // ESP is opaque: no transport header this reader can name.
            50 => {
                return Ok(PacketInfo {
                    protocol: 50,
                    src,
                    dst,
                    src_port: None,
                    dst_port: None,
                });
            }
            // No Next Header: the packet ends at the previous header.
            59 => {
                return Ok(PacketInfo {
                    protocol: 59,
                    src,
                    dst,
                    src_port: None,
                    dst_port: None,
                });
            }
            TCP | UDP => {
                let (src_port, dst_port) = transport_ports(packet, offset, total_len)?;
                return Ok(PacketInfo {
                    protocol: next,
                    src,
                    dst,
                    src_port,
                    dst_port,
                });
            }
            // ICMPv6 and every other upper-layer protocol: routable by address
            // only.
            _ => {
                return Ok(PacketInfo {
                    protocol: next,
                    src,
                    dst,
                    src_port: None,
                    dst_port: None,
                });
            }
        }
    }
}

/// Read one extension header's `(next_header, length)` and bound it against
/// the packet. `auth` selects AH's 4-octet length units.
fn extension_len(
    packet: &[u8],
    offset: usize,
    total_len: usize,
    auth: bool,
) -> Result<(u8, usize), ParseError> {
    if offset + 2 > total_len {
        return Err(ParseError::Malformed);
    }
    let next = packet[offset];
    let units = usize::from(packet[offset + 1]);
    let len = if auth {
        (units + 2) * 4
    } else {
        (units + 1) * 8
    };
    if len < 8 || offset + len > total_len {
        return Err(ParseError::Malformed);
    }
    Ok((next, len))
}

/// Read the two ports at `offset`, refusing a packet that stops before them.
fn transport_ports(
    packet: &[u8],
    offset: usize,
    total_len: usize,
) -> Result<(Option<u16>, Option<u16>), ParseError> {
    if offset + 4 > total_len {
        return Err(ParseError::Malformed);
    }
    Ok((
        Some(u16::from_be_bytes([packet[offset], packet[offset + 1]])),
        Some(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]])),
    ))
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
    const ICMPV6: u8 = 58;

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

    /// An IPv6 packet with `next_header` and `payload`; the payload starts at
    /// byte 40 unless an extension header is prepended by the caller.
    fn v6_packet(next_header: u8, src: [u8; 16], dst: [u8; 16], payload: &[u8]) -> Vec<u8> {
        let payload_len = u16::try_from(payload.len()).unwrap();
        let mut p = vec![0u8; IPV6_MIN_HEADER];
        p[0] = 0x60;
        p[4..6].copy_from_slice(&payload_len.to_be_bytes());
        p[6] = next_header;
        p[7] = 64;
        p[8..24].copy_from_slice(&src);
        p[24..40].copy_from_slice(&dst);
        p.extend_from_slice(payload);
        p
    }

    /// One 8-byte extension header with `next` and a zero length field.
    fn ext8(next: u8) -> Vec<u8> {
        let mut h = vec![0u8; 8];
        h[0] = next;
        h[1] = 0;
        h
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
        let frag = parse(&packet(TCP, [10, 99, 0, 1], 0x2001, &ports(1, 2))).unwrap();
        assert_eq!(frag.dst_port, None);
    }

    #[test]
    fn ipv4_and_ipv6_are_both_carried() {
        let v6 = v6_packet(
            TCP,
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            &ports(50000, 443),
        );
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.dst, "2001:db8::2".parse::<IpAddr>().unwrap());
        assert_eq!(parsed.dst_port, Some(443));

        let mut other = vec![0u8; 40];
        other[0] = 0x50;
        assert_eq!(parse(&other), Err(ParseError::NotIp));
        assert_eq!(parse(&[]), Err(ParseError::Malformed));
    }

    #[test]
    fn ipv6_udp_yields_its_destination_port() {
        let v6 = v6_packet(UDP, [0u8; 16], [0u8; 16], &ports(5353, 443));
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.protocol, UDP);
        assert_eq!(parsed.dst_port, Some(443));
    }

    #[test]
    fn an_ipv6_extension_header_shifts_the_transport_header() {
        let mut payload = ext8(TCP);
        payload.extend_from_slice(&ports(50000, 8443));
        let v6 = v6_packet(0, [0u8; 16], [0u8; 16], &payload);
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.protocol, TCP);
        assert_eq!(parsed.dst_port, Some(8443));
    }

    #[test]
    fn an_ipv6_first_fragment_can_still_name_a_port() {
        let mut payload = vec![0u8; 8];
        payload[0] = TCP; // Fragment header: next header
        payload.extend_from_slice(&ports(50000, 8443));
        let v6 = v6_packet(44, [0u8; 16], [0u8; 16], &payload);
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.dst_port, Some(8443));
    }

    #[test]
    fn an_ipv6_later_fragment_has_no_port() {
        let mut payload = vec![0u8; 8];
        payload[0] = TCP;
        payload[2..4].copy_from_slice(&(1u16 << 3).to_be_bytes());
        payload.extend_from_slice(&ports(50000, 8443));
        let v6 = v6_packet(44, [0u8; 16], [0u8; 16], &payload);
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.dst_port, None);
        assert_eq!(parsed.protocol, TCP);
    }

    #[test]
    fn an_ipv6_icmp_packet_is_routable_by_address() {
        let v6 = v6_packet(ICMPV6, [0u8; 16], [0u8; 16], &[128, 0, 0, 0]);
        let parsed = parse(&v6).unwrap();
        assert_eq!(parsed.protocol, ICMPV6);
        assert_eq!(parsed.dst_port, None);
    }

    #[test]
    fn malformed_ipv6_headers_are_refused() {
        // A base header that stops before its addresses.
        let short = vec![0x60u8; 10];
        assert_eq!(parse(&short), Err(ParseError::Malformed));

        // A payload length that runs past the packet.
        let mut bad = v6_packet(TCP, [0u8; 16], [0u8; 16], &ports(1, 2));
        bad[4..6].copy_from_slice(&9000u16.to_be_bytes());
        assert_eq!(parse(&bad), Err(ParseError::Malformed));

        // An extension header whose length runs past the packet.
        let mut ext = ext8(TCP);
        ext[1] = 100; // 808 bytes, far beyond the packet
        let bad = v6_packet(0, [0u8; 16], [0u8; 16], &ext);
        assert_eq!(parse(&bad), Err(ParseError::Malformed));

        // A zero payload length with trailing bytes is a jumbogram, not
        // supported on this path.
        let mut jumbo = v6_packet(59, [0u8; 16], [0u8; 16], &[]);
        jumbo.push(0);
        assert_eq!(parse(&jumbo), Err(ParseError::Malformed));
    }

    #[test]
    fn a_transport_header_that_stops_before_its_ports_is_refused() {
        let v6 = v6_packet(TCP, [0u8; 16], [0u8; 16], &[0, 80]);
        assert_eq!(parse(&v6), Err(ParseError::Malformed));
    }

    #[test]
    fn header_options_shift_the_transport_header() {
        let mut p = packet(TCP, [10, 99, 0, 1], 0, &[0, 0, 0, 0, 0, 0, 0, 0]);
        p[0] = 0x46;
        p[26..28].copy_from_slice(&443u16.to_be_bytes());
        let total = u16::try_from(p.len()).unwrap();
        p[2..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(parse(&p).unwrap().dst_port, Some(443));
    }
}
