//! Conversion between the two transparent frame shapes.
//!
//! A Linux TUN device attached with `IFF_VNET_HDR` hands over a *vnet frame*:
//! the 10-byte virtio-net header followed by an IPv4 or IPv6 packet or a GSO
//! super-frame. The other supported devices (macOS utun, Windows Wintun, and a
//! Linux device whose offload declaration was refused) hand over bare IP
//! packets.
//!
//! The wire carries one shape for the life of a claim, chosen by the server
//! when it answers `StartForwardTransparent`:
//!
//! - `Vnet` when both devices are vnet-capable (a Linux pair), so the current
//!   GSO path is preserved byte for byte;
//! - `Ip` otherwise, with each side's Linux device converting at its boundary.
//!
//! Conversion is only needed in the two mixed cases. `Ip` wire to a `Vnet`
//! device prepends the zero header (the packet is already checksummed).
//! `Vnet` wire to an `Ip` device splits a GSO super-frame into IPv4 segments
//! with computed transport checksums, or completes a plain `NEEDS_CSUM`
//! packet. The reverse directions are impossible by the negotiation invariant
//! and are rejected.

use std::borrow::Cow;

use anyhow::{Result, bail};

use crate::protocol::{FrameFormat, VNET_HDR_LEN};

/// `VIRTIO_NET_HDR_F_NEEDS_CSUM`: the transport checksum field holds the
/// pseudo-header sum and must be completed before the packet leaves here.
#[cfg(mh_linux)]
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

/// The largest frame the transparent path moves: the virtio-net header plus
/// the kernel's ~64 KiB GSO budget. Plain IP frames are smaller, but this is
/// the buffer both sides size their reads with.
pub const MAX_FRAME: usize = VNET_HDR_LEN + 66_000;

/// Reusable buffers for a GSO split. One instance lives in the hub reader and
/// one in each lane task, so a mixed-format claim does not allocate per packet.
#[derive(Default)]
pub struct SegmentScratch {
    #[cfg(mh_linux)]
    input: Vec<u8>,
    #[cfg(mh_linux)]
    bufs: Vec<Vec<u8>>,
    #[cfg(mh_linux)]
    sizes: Vec<usize>,
}

impl SegmentScratch {
    /// Copy `packet` into the reusable input buffer.
    #[cfg(mh_linux)]
    fn input(&mut self, packet: &[u8]) -> &mut [u8] {
        self.input.clear();
        self.input.extend_from_slice(packet);
        &mut self.input
    }

    /// Make room for `count` output segments of at most `len` bytes each.
    #[cfg(mh_linux)]
    fn prepare(&mut self, count: usize, len: usize) {
        self.bufs.resize_with(count, Vec::new);
        self.sizes.resize(count, 0);
        for buf in &mut self.bufs {
            if buf.len() < len {
                buf.resize(len, 0);
            }
        }
    }
}

/// Turn one device frame into the wire shape `wire`.
///
/// `emit` receives each frame in order; a GSO super-frame can produce many.
pub fn device_to_wire(
    frame: &[u8],
    device: FrameFormat,
    wire: FrameFormat,
    scratch: &mut SegmentScratch,
    emit: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    match (device, wire) {
        (FrameFormat::Ip, FrameFormat::Ip) | (FrameFormat::Vnet, FrameFormat::Vnet) => emit(frame),
        (FrameFormat::Ip, FrameFormat::Vnet) => {
            bail!("an IP device cannot produce a vnet wire frame")
        }
        (FrameFormat::Vnet, FrameFormat::Ip) => vnet_to_ip(frame, scratch, emit),
    }
}

/// The frame a `device` expects for one `wire` frame.
///
/// Matching shapes pass through without copying. `(Ip, Vnet)` needs the zero
/// virtio-net header (the packet is already checksummed); `(Vnet, Ip)` is
/// impossible under the negotiation invariant and is rejected.
pub(crate) fn frame_for_device(
    frame: &[u8],
    wire: FrameFormat,
    device: FrameFormat,
) -> Result<Cow<'_, [u8]>> {
    match (wire, device) {
        (FrameFormat::Ip, FrameFormat::Ip) | (FrameFormat::Vnet, FrameFormat::Vnet) => {
            Ok(Cow::Borrowed(frame))
        }
        (FrameFormat::Ip, FrameFormat::Vnet) => {
            // The kernel expects the header on every write; a zero header is
            // a fully checksummed packet with no segmentation metadata.
            let mut framed = Vec::with_capacity(VNET_HDR_LEN + frame.len());
            framed.extend_from_slice(&[0u8; VNET_HDR_LEN]);
            framed.extend_from_slice(frame);
            Ok(Cow::Owned(framed))
        }
        (FrameFormat::Vnet, FrameFormat::Ip) => {
            bail!("a vnet wire frame cannot be injected into an IP device")
        }
    }
}

#[cfg(mh_linux)]
fn vnet_to_ip(
    frame: &[u8],
    scratch: &mut SegmentScratch,
    emit: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    use tun_rs::VirtioNetHdr;

    if frame.len() < VNET_HDR_LEN {
        bail!("vnet frame shorter than its header ({} bytes)", frame.len());
    }
    let hdr = VirtioNetHdr::decode(&frame[..VNET_HDR_LEN])?;
    let packet = &frame[VNET_HDR_LEN..];
    let is_v6 = match packet.first().map(|byte| byte >> 4) {
        Some(4) => false,
        Some(6) => true,
        _ => bail!("transparent path carries an unsupported IP version"),
    };

    if hdr.gso_type == 0 {
        if hdr.flags & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0 {
            let packet = scratch.input(packet);
            complete_checksum(packet, hdr.csum_start, hdr.csum_offset)?;
            emit(packet)?;
        } else {
            emit(packet)?;
        }
        return Ok(());
    }

    if hdr.gso_size == 0 || hdr.hdr_len < hdr.csum_start || hdr.hdr_len as usize > packet.len() {
        bail!("invalid GSO header");
    }
    let payload = packet.len() - usize::from(hdr.hdr_len);
    if payload == 0 {
        bail!("GSO frame carries no payload");
    }
    let count = payload.div_ceil(usize::from(hdr.gso_size));
    let max_len = usize::from(hdr.hdr_len) + usize::from(hdr.gso_size);
    if max_len > MAX_FRAME {
        bail!("GSO segment larger than the frame ceiling ({max_len} bytes)");
    }
    scratch.prepare(count, max_len);
    let SegmentScratch { input, bufs, sizes } = scratch;
    input.clear();
    input.extend_from_slice(packet);
    let n = tun_rs::gso_split(input, hdr, bufs, sizes, 0, is_v6)?;
    for i in 0..n {
        emit(&bufs[i][..sizes[i]])?;
    }
    Ok(())
}

#[cfg(not(mh_linux))]
fn vnet_to_ip(
    _frame: &[u8],
    _scratch: &mut SegmentScratch,
    _emit: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    bail!("this platform cannot convert a vnet frame")
}

/// Complete a `NEEDS_CSUM` packet whose checksum field already holds the
/// pseudo-header sum, the way the kernel's TUN read path expects it to be
/// finished before the packet leaves this process.
#[cfg(mh_linux)]
fn complete_checksum(packet: &mut [u8], csum_start: u16, csum_offset: u16) -> Result<()> {
    let at = usize::from(csum_start) + usize::from(csum_offset);
    if at + 2 > packet.len() {
        bail!("checksum field outside the packet");
    }
    let initial = u16::from_be_bytes([packet[at], packet[at + 1]]);
    packet[at] = 0;
    packet[at + 1] = 0;
    let sum = checksum(&packet[usize::from(csum_start)..], u32::from(initial));
    let sum = !sum;
    packet[at..at + 2].copy_from_slice(&sum.to_be_bytes());
    Ok(())
}

#[cfg(mh_linux)]
fn checksum(data: &[u8], mut sum: u32) -> u16 {
    let (chunks, remainder) = data.as_chunks::<2>();
    for chunk in chunks {
        sum += u32::from(u16::from_be_bytes(*chunk));
    }
    if let Some(&last) = remainder.first() {
        sum += u32::from(last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    u16::try_from(sum).unwrap_or(u16::MAX)
}

#[cfg(all(test, mh_linux))]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests build frames they just constructed and assert on their refusal"
    )]
    use super::*;
    use tun_rs::{
        VIRTIO_NET_HDR_GSO_TCPV4, VIRTIO_NET_HDR_GSO_TCPV6, VIRTIO_NET_HDR_GSO_UDP_L4,
        VIRTIO_NET_HDR_LEN, VirtioNetHdr,
    };

    fn tcp_v4_packet(payload_len: usize) -> Vec<u8> {
        let total = 20 + 20 + payload_len;
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
        p[8] = 64;
        p[9] = 6;
        p[12..16].copy_from_slice(&[10, 0, 0, 1]);
        p[16..20].copy_from_slice(&[10, 0, 0, 2]);
        p[20..22].copy_from_slice(&1234u16.to_be_bytes());
        p[22..24].copy_from_slice(&80u16.to_be_bytes());
        p[32] = 0x50;
        for (i, b) in p[40..].iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        // A real IPv4 header checksum is not needed for the splitter; the
        // packet's transport checksum is what the conversion recomputes.
        p
    }

    fn vnet_frame(packet: &[u8], hdr: VirtioNetHdr) -> Vec<u8> {
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        hdr.encode(&mut frame).unwrap();
        frame.extend_from_slice(packet);
        frame
    }

    fn tcp_v6_packet(payload_len: usize) -> Vec<u8> {
        let total = 40 + 20 + payload_len;
        let mut p = vec![0u8; total];
        p[0] = 0x60;
        p[4..6].copy_from_slice(&u16::try_from(20 + payload_len).unwrap().to_be_bytes());
        p[6] = 6;
        p[7] = 64;
        p[8..24].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        p[24..40].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        p[40..42].copy_from_slice(&1234u16.to_be_bytes());
        p[42..44].copy_from_slice(&80u16.to_be_bytes());
        p[52] = 0x50;
        for (i, b) in p[60..].iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        p
    }

    #[test]
    fn the_two_header_constants_agree() {
        assert_eq!(VNET_HDR_LEN, VIRTIO_NET_HDR_LEN);
    }

    #[test]
    fn a_plain_vnet_frame_loses_only_the_header() {
        let packet = tcp_v4_packet(8);
        let frame = vnet_frame(&packet, VirtioNetHdr::default());
        let mut scratch = SegmentScratch::default();
        let mut seen = Vec::new();
        device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |out| {
                seen.push(out.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![packet]);
    }

    #[test]
    fn a_tcp_gso_frame_splits_into_checksummed_segments() {
        let mut packet = tcp_v4_packet(3 * 100);
        // The first TCP checksum field holds the pseudo-header sum; the
        // splitter computes each segment's final checksum.
        packet[36..38].copy_from_slice(&0u16.to_be_bytes());
        let hdr = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
            hdr_len: 40,
            gso_size: 100,
            csum_start: 20,
            csum_offset: 16,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let mut seen = Vec::new();
        device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |out| {
                seen.push(out.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 3);
        for segment in &seen {
            assert_eq!(segment[0] >> 4, 4);
            assert!(segment.len() <= 140);
        }
    }

    #[test]
    fn a_tcpv6_gso_frame_splits_into_ipv6_segments() {
        let mut packet = tcp_v6_packet(3 * 100);
        packet[56..58].copy_from_slice(&0u16.to_be_bytes());
        let hdr = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV6,
            hdr_len: 60,
            gso_size: 100,
            csum_start: 40,
            csum_offset: 16,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let mut seen = Vec::new();
        device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |out| {
                seen.push(out.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 3);
        for segment in &seen {
            assert_eq!(segment[0] >> 4, 6);
            assert!(segment.len() <= 160);
        }
    }

    fn udp_v4_packet(payload_len: usize) -> Vec<u8> {
        let total = 20 + 8 + payload_len;
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
        p[8] = 64;
        p[9] = 17; // UDP
        p[12..16].copy_from_slice(&[10, 0, 0, 1]);
        p[16..20].copy_from_slice(&[10, 0, 0, 2]);
        p[20..22].copy_from_slice(&1234u16.to_be_bytes());
        p[22..24].copy_from_slice(&53u16.to_be_bytes());
        p[24..26].copy_from_slice(&u16::try_from(8 + payload_len).unwrap().to_be_bytes());
        for (i, b) in p[28..].iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        p
    }

    /// UDP segmentation offload is a supported GSO type, not just TCP; the
    /// splitter must emit UDP segments with the UDP protocol still in the IP
    /// header.
    #[test]
    fn a_udp_gso_frame_splits_into_udp_segments() {
        let packet = udp_v4_packet(300);
        let hdr = VirtioNetHdr {
            flags: 0,
            gso_type: VIRTIO_NET_HDR_GSO_UDP_L4,
            hdr_len: 28,
            gso_size: 100,
            csum_start: 20,
            csum_offset: 6,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let mut seen = Vec::new();
        device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |out| {
                seen.push(out.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 3);
        for segment in &seen {
            assert_eq!(segment[0] >> 4, 4);
            assert_eq!(segment[9], 17, "the UDP protocol must survive the split");
            assert!(segment.len() <= 128);
        }
    }

    #[test]
    fn an_ip_frame_to_a_vnet_device_gets_the_zero_header() {
        let packet = tcp_v4_packet(8);
        let framed = frame_for_device(&packet, FrameFormat::Ip, FrameFormat::Vnet).unwrap();
        assert_eq!(framed.len(), VNET_HDR_LEN + packet.len());
        assert_eq!(&framed[..VNET_HDR_LEN], &[0u8; VNET_HDR_LEN]);
        assert_eq!(&framed[VNET_HDR_LEN..], &packet);
    }

    #[test]
    fn impossible_directions_are_rejected() {
        let packet = tcp_v4_packet(8);
        assert!(
            frame_for_device(&packet, FrameFormat::Vnet, FrameFormat::Ip).is_err(),
            "a vnet frame cannot be injected into an IP device"
        );
        let mut scratch = SegmentScratch::default();
        assert!(
            device_to_wire(
                &packet,
                FrameFormat::Ip,
                FrameFormat::Vnet,
                &mut scratch,
                &mut |_| Ok(())
            )
            .is_err(),
            "an IP device cannot produce a vnet wire frame"
        );
    }

    #[test]
    fn malformed_vnet_frames_are_rejected() {
        let mut scratch = SegmentScratch::default();
        assert!(
            device_to_wire(
                &[0u8; 4],
                FrameFormat::Vnet,
                FrameFormat::Ip,
                &mut scratch,
                &mut |_| Ok(())
            )
            .is_err()
        );

        // A header with no packet behind it has no IP version to route by.
        let header_only = vnet_frame(&[], VirtioNetHdr::default());
        assert!(
            format!(
                "{:#}",
                device_to_wire(
                    &header_only,
                    FrameFormat::Vnet,
                    FrameFormat::Ip,
                    &mut scratch,
                    &mut |_| Ok(())
                )
                .unwrap_err()
            )
            .contains("unsupported IP version")
        );

        // Neither is a packet whose first nibble is not 4 or 6.
        let mut unknown_version = tcp_v4_packet(0);
        unknown_version[0] = 0x70;
        let frame = vnet_frame(&unknown_version, VirtioNetHdr::default());
        assert!(
            format!(
                "{:#}",
                device_to_wire(
                    &frame,
                    FrameFormat::Vnet,
                    FrameFormat::Ip,
                    &mut scratch,
                    &mut |_| Ok(())
                )
                .unwrap_err()
            )
            .contains("unsupported IP version")
        );
    }

    /// Every malformed GSO shape gets a name rather than reaching tun-rs or
    /// producing a partial emission. These are the branches a forged or
    /// corrupted device frame would hit; the valid split is tested above.
    #[test]
    fn malformed_gso_headers_are_rejected() {
        let packet = tcp_v4_packet(100);
        let base = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
            hdr_len: 40,
            gso_size: 100,
            csum_start: 20,
            csum_offset: 16,
        };
        let cases = [
            (
                "zero gso_size",
                VirtioNetHdr {
                    gso_size: 0,
                    ..base
                },
                "invalid GSO header",
            ),
            (
                "hdr_len before csum_start",
                VirtioNetHdr {
                    hdr_len: 10,
                    csum_start: 20,
                    ..base
                },
                "invalid GSO header",
            ),
            (
                "hdr_len past the packet",
                VirtioNetHdr {
                    hdr_len: 10_000,
                    ..base
                },
                "invalid GSO header",
            ),
        ];
        for (name, hdr, needle) in cases {
            let frame = vnet_frame(&packet, hdr);
            let mut scratch = SegmentScratch::default();
            let err = device_to_wire(
                &frame,
                FrameFormat::Vnet,
                FrameFormat::Ip,
                &mut scratch,
                &mut |_| Ok(()),
            )
            .expect_err(name);
            let message = format!("{err:#}");
            assert!(message.contains(needle), "{name}: {message}");
        }
    }

    #[test]
    fn a_gso_segment_past_the_frame_ceiling_is_rejected() {
        let packet = tcp_v4_packet(2_000);
        let hdr = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
            hdr_len: 2_000,
            gso_size: 65_000,
            csum_start: 20,
            csum_offset: 16,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let err = device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |_| Ok(()),
        )
        .expect_err("a segment past the frame ceiling must be refused");
        assert!(format!("{err:#}").contains("larger than the frame ceiling"));
    }

    #[test]
    fn a_gso_frame_without_payload_is_rejected() {
        let packet = tcp_v4_packet(0);
        let hdr = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
            hdr_len: 40,
            gso_size: 100,
            csum_start: 20,
            csum_offset: 16,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let err = device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |_| Ok(()),
        )
        .expect_err("a GSO frame with no payload must not split");
        assert!(format!("{err:#}").contains("no payload"));
    }

    #[test]
    fn a_checksum_field_outside_the_packet_is_rejected() {
        let packet = tcp_v4_packet(0);
        let hdr = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: 0,
            hdr_len: 0,
            gso_size: 0,
            csum_start: 40,
            csum_offset: 0,
        };
        let frame = vnet_frame(&packet, hdr);
        let mut scratch = SegmentScratch::default();
        let err = device_to_wire(
            &frame,
            FrameFormat::Vnet,
            FrameFormat::Ip,
            &mut scratch,
            &mut |_| Ok(()),
        )
        .expect_err("a checksum field past the packet must be refused");
        assert!(format!("{err:#}").contains("checksum field outside"));
    }
}
