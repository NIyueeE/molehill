//! Placing a claim's flows on the members of its set.
//!
//! A claimed endpoint's packets are spread over the members it holds by one
//! rule: **one flow, one member**. The hub asks this module which slot a packet
//! belongs to while it drains the device, and the answer is a pure function of
//! the packet's flow — not of the packet's position, not of the instant it
//! arrived, and not of which member happens to be idle. That is what keeps a
//! flow's packets in order: each member is one ordered carrier, so a flow that
//! stayed on one member arrives in the order its sender wrote it, while a flow
//! whose packets were dealt out one by one would be interleaved across carriers
//! that drain at their own speeds and arrive — at the far end, which injects
//! what it reads — out of order.
//!
//! The key is the five-tuple every router knows, written as a fixed byte
//! sequence so the hash is reproducible by hand:
//!
//! ```text
//! [protocol][low endpoint][high endpoint]
//! endpoint := [address bytes][port bytes]   (the port only when the packet has one)
//! ```
//!
//! The two endpoints are **sorted** (address bytes, then port), which makes the
//! key direction-agnostic: the two directions of one flow — and the two ends of
//! a claim, each of which sees one of them — produce the same key. Portless
//! packets (ICMP, and the fragments after the first) carry the protocol and the
//! address pair alone; a first fragment may therefore land on another member
//! than its siblings, which IP reassembly tolerates because it is
//! order-insensitive ([internals.md](../../docs/internals.md), "Transparent
//! (L3) services").
//!
//! FNV-1a, 64-bit, no seed: a hash whose value an operator can reproduce, and
//! whose overflow is the wrapping multiplication rather than a panic. It is not
//! a security boundary — a visitor who can choose its ports can choose its
//! member — because the members are peers of one claim and every one of them
//! carries the same traffic for the same address.
//!
//! The value is avalanched before the modulo, and that is not decoration.
//! FNV-1a's *low* bits are weak: its last step is a multiplication by a prime,
//! which preserves the residue structure of the bytes mixed last, so a set of
//! flows whose ports share their low bits — and real ephemeral allocators
//! produce exactly that, a run of the *same* two low bits is common — collapsed
//! onto one member. Measured: eight `iperf3` streams with the ports
//! `52044, 52052, 52054, 52058, 52070, 52082, 52088, 52094` took two members
//! under a bare `% width`, which is why a four-member claim carried 45 % more
//! than a one-member claim instead of the four members' worth. The avalanche
//! (splitmix64's finalizer, the standard remedy) mixes every bit into every
//! bit, and the same eight flows then take three of the four members.

use std::net::IpAddr;

use crate::transparent::ip::PacketInfo;

/// FNV-1a's 64-bit offset basis.
const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a's 64-bit prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which member slot of a claim's set a packet's flow belongs to.
///
/// `width` is the number of slots the claim holds (its member set's width, the
/// value the hub routes by). A set of one has one answer, and saying so up
/// front is what keeps a one-member claim free of the hash entirely.
#[must_use]
pub fn slot(info: &PacketInfo, width: usize) -> usize {
    if width <= 1 {
        return 0;
    }
    reduce(avalanche(hash(info)), width)
}

/// Mix every input bit into every output bit (splitmix64's finalizer).
///
/// The modulo takes the *low* bits of its input, and FNV-1a's low bits carry
/// the structure of the bytes mixed last — two flows whose source ports differ
/// by a multiple of four hashed four apart, and a stride-4 run of ephemeral
/// ports therefore landed on one member. Three shifts and two odd
/// multiplications are the standard fix, and they are cheap next to the hash
/// they follow.
fn avalanche(mut hash: u64) -> u64 {
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// FNV-1a over the flow key described in the module docs.
fn hash(info: &PacketInfo) -> u64 {
    let left = (info.src, info.src_port);
    let right = (info.dst, info.dst_port);
    // Sorted, so both directions of a flow — and both ends of a claim — hash
    // the same key.
    let (first, second) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let mut hash = mix(OFFSET_BASIS, &[info.protocol]);
    hash = endpoint(hash, first);
    endpoint(hash, second)
}

/// One endpoint's contribution: its address bytes, then its port bytes when the
/// packet carries a port.
fn endpoint(hash: u64, (addr, port): (IpAddr, Option<u16>)) -> u64 {
    let hash = match addr {
        IpAddr::V4(v4) => mix(hash, &v4.octets()),
        IpAddr::V6(v6) => mix(hash, &v6.octets()),
    };
    match port {
        Some(port) => mix(hash, &port.to_be_bytes()),
        None => hash,
    }
}

/// Fold one run of bytes into the hash.
fn mix(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// `hash` reduced into `0..width`.
///
/// The remainder is below `width`, so the conversion cannot fail on a target
/// whose `usize` can hold `width` at all; the fallback is unreachable and keeps
/// the function total.
fn reduce(hash: u64, width: usize) -> usize {
    match u64::try_from(width) {
        Ok(width) => usize::try_from(hash % width).unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transparent::ip::{TCP, UDP};
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(src: [u8; 4], src_port: Option<u16>, dst: [u8; 4], dst_port: Option<u16>) -> PacketInfo {
        PacketInfo {
            protocol: TCP,
            src: IpAddr::V4(Ipv4Addr::from(src)),
            dst: IpAddr::V4(Ipv4Addr::from(dst)),
            src_port,
            dst_port,
        }
    }

    /// A flow is a property of the connection, not of the direction a packet is
    /// going: the two ends of a claim see opposite directions of the same flow
    /// and must place it on the same member, and so does the far end of a reply.
    #[test]
    fn a_flow_hashes_the_same_either_way_round() {
        let out = v4([10, 0, 0, 2], Some(40_000), [10, 99, 0, 1], Some(8443));
        let back = v4([10, 99, 0, 1], Some(8443), [10, 0, 0, 2], Some(40_000));
        assert_eq!(hash(&out), hash(&back));

        // The canonical order is by address first: a packet between the same
        // address pair on two different ports is one flow, both ways.
        let other = v4([10, 99, 0, 1], Some(8444), [10, 0, 0, 2], Some(40_001));
        assert_ne!(hash(&out), hash(&other), "different flows hash apart");
    }

    /// The protocol and both ports are part of the key: a UDP datagram and a
    /// TCP segment between the same endpoints are different flows, and so are
    /// two connections that differ only by one port.
    #[test]
    fn the_key_carries_the_protocol_and_both_ports() {
        let tcp = v4([10, 0, 0, 2], Some(40_000), [10, 99, 0, 1], Some(8443));
        let mut udp = tcp;
        udp.protocol = UDP;
        assert_ne!(hash(&tcp), hash(&udp));

        let mut moved_source = tcp;
        moved_source.src_port = Some(40_001);
        assert_ne!(hash(&tcp), hash(&moved_source));

        let mut moved_destination = tcp;
        moved_destination.dst_port = Some(8444);
        assert_ne!(hash(&tcp), hash(&moved_destination));
    }

    /// A portless packet hashes on the protocol and the address pair alone: it
    /// still has a flow (an ICMP echo's two directions agree), and it is not
    /// confused with a port-carrying flow.
    #[test]
    fn a_portless_packet_hashes_without_ports() {
        let icmp = v4([10, 0, 0, 2], None, [10, 99, 0, 1], None);
        let mut back = icmp;
        std::mem::swap(&mut back.src, &mut back.dst);
        assert_eq!(hash(&icmp), hash(&back));

        let with_ports = v4([10, 0, 0, 2], Some(0), [10, 99, 0, 1], Some(0));
        assert_ne!(
            hash(&icmp),
            hash(&with_ports),
            "a port 0 is a port, and the key says so"
        );
    }

    /// IPv6 is not carried yet, but the key is defined over the parsed packet,
    /// so a v6 flow hashes as stably as a v4 one — including the sorted order
    /// across the two families' byte widths.
    #[test]
    fn an_ipv6_flow_hashes_like_any_other() {
        let mut packet = v4([10, 0, 0, 2], Some(40_000), [10, 99, 0, 1], Some(8443));
        packet.src = IpAddr::V6(Ipv6Addr::LOCALHOST);
        packet.dst = IpAddr::V6(Ipv6Addr::from([
            0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        let mut back = packet;
        std::mem::swap(&mut back.src, &mut back.dst);
        std::mem::swap(&mut back.src_port, &mut back.dst_port);
        assert_eq!(hash(&packet), hash(&back));
    }

    /// A set of one has one answer whatever the packet is, and every slot the
    /// function can return is inside the set.
    #[test]
    fn every_flow_lands_inside_the_set() {
        for width in 1..=8usize {
            assert_eq!(slot(&v4([10, 0, 0, 2], None, [10, 99, 0, 1], None), 1), 0);
            for port in 0..512u16 {
                let packet = v4(
                    [10, 0, 0, 2],
                    Some(40_000 + port),
                    [10, 99, 0, 1],
                    Some(8443),
                );
                let placed = slot(&packet, width);
                assert!(placed < width, "slot {placed} is outside a set of {width}");
                assert_eq!(placed, slot(&packet, width), "placement is stable");
            }
        }
    }

    /// The ports a real allocator hands out are not an even sample: they come
    /// in runs that share their low bits, and a hash whose low bits carry the
    /// port's structure puts such a run on one member. This is the case a
    /// measured bench cell produced — eight `iperf3` streams, all even ports,
    /// several four apart — and under a bare FNV-1a `% 4` they took two of four
    /// members, which is exactly what the run's per-member counters showed.
    #[test]
    fn ports_that_share_their_low_bits_do_not_collapse_onto_one_member() {
        let measured = [52044u16, 52052, 52054, 52058, 52070, 52082, 52088, 52094];
        let occupied = |ports: &[u16], width: usize| {
            ports
                .iter()
                .map(|port| {
                    slot(
                        &v4([10, 10, 0, 2], Some(*port), [10, 99, 0, 1], Some(2402)),
                        width,
                    )
                })
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert!(
            occupied(&measured, 4) >= 3,
            "the measured port set took {} of four members",
            occupied(&measured, 4)
        );

        // And structurally: no arithmetic run of ports may collapse onto one
        // member, whatever its stride — a run of the same low bits is normal,
        // and the finalizer is what stops it from being a placement rule.
        for width in 2..=8usize {
            for stride in 1..=32u16 {
                let ports: Vec<u16> = (0..8).map(|i| 50_000 + stride * i).collect();
                assert!(
                    occupied(&ports, width) > 1,
                    "stride {stride} put all eight flows on one of {width} members"
                );
            }
        }
    }

    /// The spread is what the member set exists for: over many flows, every slot
    /// of the set takes a share of them. A rule that stacked them on one member
    /// would pass every other test here and leave the claim at one carrier's
    /// throughput.
    #[test]
    fn flows_spread_over_the_whole_set() {
        const FLOWS: u16 = 4096;
        for width in [2usize, 3, 4, 8] {
            let mut counts = vec![0usize; width];
            for port in 0..FLOWS {
                let packet = v4(
                    [10, 0, 0, 2],
                    Some(20_000 + port),
                    [10, 99, 0, 1],
                    Some(8443),
                );
                counts[slot(&packet, width)] += 1;
            }
            let expected = usize::from(FLOWS) / width;
            for (member, count) in counts.iter().enumerate() {
                assert!(
                    *count > expected / 2 && *count < expected * 2,
                    "member {member} of {width} took {count} flows, expected around {expected}"
                );
            }
        }
    }
}
