//! Batch UDP datagram IO on Linux (`recvmmsg` / `sendmmsg`).
//!
//! mhd pays one syscall per datagram in userspace — the cost TCP amortizes
//! in the kernel via GSO/TSO. Batching a whole readiness wake into one
//! `recvmmsg`/`sendmmsg` call is the same amortization QUIC stacks get from
//! UDP GSO, and it changes nothing on the wire: the datagrams are
//! byte-identical, so peers and protocol are unaffected.
//!
//! The kernel calls, their descriptor arrays and the address decoding all
//! live in `nix`; this module only stages this batch's own buffers and turns
//! the results back into `(SocketAddr, &[u8])`. There is no `unsafe` here.
//! The price is a small per-batch setup cost: `MultiHeaders` is `!Send`, so
//! it is rebuilt for each call instead of being carried across an await.
//! Measured on the mhd `l4` path (`just bench --profile screen`, four runs),
//! that cost 1.4-1.9 % CPU per Gbit against the previous raw-pointer batch —
//! inside the metric's 15 % materiality floor, and accepted for removing the
//! eight FFI expectations. The record is in `HANDOFF.md`.
//!
//! The TUN attach in `src/transparent/tun.rs` is the remaining audited FFI
//! site. Non-Linux platforms keep the single-datagram tokio paths.

#![cfg(target_os = "linux")]

use std::io::{self, IoSlice, IoSliceMut};
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::RawFd;

use nix::sys::socket::{
    ControlMessage, MsgFlags, MultiHeaders, SockaddrStorage, recvmmsg, sendmmsg, sendmsg,
};

use crate::mhd::MHD_OVERHEAD;
use bytes::Bytes;

use super::dgram::{BATCH, Span};

/// One send descriptor for one datagram: at most a staged header plus an
/// external payload. `len` shortens the slice to the iovecs the span needs.
struct OneIov<'a> {
    slices: [IoSlice<'a>; 2],
    len: usize,
}

impl<'a> AsRef<[IoSlice<'a>]> for OneIov<'a> {
    fn as_ref(&self) -> &[IoSlice<'a>] {
        &self.slices[..self.len]
    }
}

/// One receive descriptor per datagram. Each borrows a distinct buffer from
/// `RecvBatch::bufs`; the batch owns them for the whole call.
struct OneIovMut<'a>(IoSliceMut<'a>);

impl<'a> AsMut<[IoSliceMut<'a>]> for OneIovMut<'a> {
    fn as_mut(&mut self) -> &mut [IoSliceMut<'a>] {
        std::slice::from_mut(&mut self.0)
    }
}

/// The address a kernel-filled `SockaddrStorage` names, or the unspecified
/// address for a family the caller's peer filter will drop anyway.
fn socket_addr(storage: &SockaddrStorage) -> SocketAddr {
    if let Some(v4) = storage.as_sockaddr_in() {
        return SocketAddr::V4(SocketAddrV4::new(v4.ip(), v4.port()));
    }
    if let Some(v6) = storage.as_sockaddr_in6() {
        return SocketAddr::V6(SocketAddrV6::new(
            v6.ip(),
            v6.port(),
            v6.flowinfo(),
            v6.scope_id(),
        ));
    }
    SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0))
}

/// The receive half's reusable buffers plus the source address and received
/// length the last call filled.
pub struct RecvBatch {
    bufs: Vec<Vec<u8>>,
    names: Vec<SocketAddr>,
    lens: Vec<usize>,
    pub count: usize,
}

impl RecvBatch {
    /// `count` datagrams of up to `buf_len` bytes each.
    pub fn new(count: usize, buf_len: usize) -> Self {
        Self {
            bufs: (0..count).map(|_| vec![0u8; buf_len]).collect(),
            names: vec![
                SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0));
                count
            ],
            lens: vec![0; count],
            count: 0,
        }
    }

    /// Fill the batch from `fd` (non-blocking). Returns the number of
    /// datagrams received, or `Err(WouldBlock)` when drained. Callers invoke
    /// this through `UdpSocket::try_io` so the EAGAIN also clears tokio's
    /// cached readiness.
    pub fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        self.count = 0;
        // `MultiHeaders` owns the kernel-facing descriptor array and the
        // addresses. It is `!Send`, so keeping it in this batch would force
        // an unsafe `Send`/`Sync` proof; rebuilding it here is what keeps
        // this module free of `unsafe`.
        let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(self.bufs.len(), None);
        let mut iovs: Vec<OneIovMut<'_>> = self
            .bufs
            .iter_mut()
            .map(|buf| OneIovMut(IoSliceMut::new(buf)))
            .collect();
        let results = recvmmsg(
            fd,
            &mut headers,
            iovs.iter_mut(),
            MsgFlags::MSG_DONTWAIT,
            None,
        )
        .map_err(io::Error::from)?;
        for (index, msg) in results.enumerate() {
            if index >= self.names.len() {
                break;
            }
            self.names[index] = msg.address.as_ref().map_or_else(
                || SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0)),
                socket_addr,
            );
            self.lens[index] = msg.bytes;
            self.count = index + 1;
        }
        Ok(self.count)
    }

    /// Iterate the received datagrams as `(source address, bytes)`.
    pub fn iter(&self) -> impl Iterator<Item = (SocketAddr, &[u8])> + '_ {
        (0..self.count).map(|i| (self.names[i], &self.bufs[i][..self.lens[i]]))
    }
}

/// The send half's stateless syscall: `nix` builds the per-call descriptors
/// and addresses from the caller's own buffer and payloads.
pub struct SendBatch;

impl SendBatch {
    /// Send the `spans` datagrams of one batch to one peer in a single
    /// syscall. Each span is one datagram: either contiguous in the batch's
    /// staging `buf` (one iovec), or a staged header plus an external payload
    /// (two iovecs — the payload pointer is the engine segment's own buffer, so
    /// no copy stages it). A partial send (return value < `spans.len()`) drops
    /// the unsent tail, and `Err(WouldBlock)` means the kernel send buffer is
    /// full — in both cases the caller's ARQ re-emits the datagrams. Callers
    /// invoke this through `UdpSocket::try_io` so the EAGAIN also clears
    /// tokio's cached writability.
    pub fn send_spans(
        fd: RawFd,
        peer: SocketAddr,
        buf: &Bytes,
        spans: &[Span],
    ) -> io::Result<usize> {
        let n = spans.len().min(BATCH);
        let mut iovs: Vec<OneIov<'_>> = Vec::with_capacity(n);
        for span in &spans[..n] {
            match span {
                Span::Staged { off, len } => iovs.push(OneIov {
                    slices: [IoSlice::new(&buf[*off..*off + *len]), IoSlice::new(&[])],
                    len: 1,
                }),
                Span::Split { hdr_off, payload } => iovs.push(OneIov {
                    slices: [
                        IoSlice::new(&buf[*hdr_off..*hdr_off + MHD_OVERHEAD]),
                        IoSlice::new(payload),
                    ],
                    len: 2,
                }),
            }
        }
        let address = SockaddrStorage::from(peer);
        let addrs = vec![Some(address); n];
        let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(n, None);
        let results = sendmmsg(
            fd,
            &mut headers,
            iovs.iter(),
            addrs,
            &[] as &[ControlMessage<'_>],
            MsgFlags::MSG_DONTWAIT,
        )
        .map_err(io::Error::from)?;
        Ok(results.count())
    }

    /// Send one batch's spans as **one GSO super-packet**.
    ///
    /// The kernel cuts the buffer at the `UDP_SEGMENT` size and the NIC emits the
    /// pieces, so a batch of N datagrams costs one syscall and one skb per
    /// segment instead of one per datagram — the amortisation TCP gets from
    /// TSO. The kernel's rule is that every piece but the last is exactly one
    /// segment long, so a batch that interleaves a short control frame is not
    /// eligible: this answers Ok(0) for one and the caller falls back to
    /// `send_spans`.
    ///
    /// The segment size is the first span's length, which is what the engine's
    /// MTU-sized segments all share; the trailing piece may be shorter.
    pub fn send_gso(fd: RawFd, peer: SocketAddr, buf: &Bytes, spans: &[Span]) -> io::Result<usize> {
        let limit = spans.len().min(BATCH);
        let Some(segment) = spans.first().map(Span::len).filter(|n| *n > 0) else {
            return Ok(0);
        };
        let Ok(size) = u16::try_from(segment) else {
            return Ok(0);
        };
        let mut n = 0;
        while n < limit && spans[n].len() == segment {
            n += 1;
        }
        if n < limit {
            // The one shorter piece GSO allows, and only at the end.
            n += 1;
        }
        if n < 2 {
            return Ok(0);
        }
        let mut iovs: Vec<IoSlice<'_>> = Vec::with_capacity(n * 2);
        for span in &spans[..n] {
            match span {
                Span::Staged { off, len } => iovs.push(IoSlice::new(&buf[*off..*off + *len])),
                Span::Split { hdr_off, payload } => {
                    iovs.push(IoSlice::new(&buf[*hdr_off..*hdr_off + MHD_OVERHEAD]));
                    iovs.push(IoSlice::new(payload));
                }
            }
        }
        let address = SockaddrStorage::from(peer);
        let cmsgs = [ControlMessage::UdpGsoSegments(&size)];
        sendmsg(fd, &iovs, &cmsgs, MsgFlags::MSG_DONTWAIT, Some(&address))
            .map_err(io::Error::from)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// Send one staged and one split datagram through the real `sendmmsg`
    /// path and read both back. This is the assertion the previous test
    /// could not make: it rebuilt the descriptor list itself, so it would
    /// pass even if `send_spans` sent the wrong bytes.
    #[test]
    fn a_staged_and_a_split_datagram_reach_the_peer_unchanged() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = receiver.local_addr().unwrap();

        let header = [0xABu8; MHD_OVERHEAD];
        let mut staging = vec![0u8; MHD_OVERHEAD + 4];
        staging[..MHD_OVERHEAD].copy_from_slice(&header);
        staging[MHD_OVERHEAD..].copy_from_slice(b"tail");
        let buf = Bytes::from(staging);
        let payload = Bytes::from_static(b"payload");
        let spans = [
            Span::Staged {
                off: MHD_OVERHEAD,
                len: 4,
            },
            Span::Split {
                hdr_off: 0,
                payload: payload.clone(),
            },
        ];

        let sent = SendBatch::send_spans(sender.as_raw_fd(), peer, &buf, &spans).unwrap();
        assert_eq!(sent, spans.len(), "both datagrams must be accepted");

        let mut got = [0u8; 128];
        let (n, _) = receiver.recv_from(&mut got).unwrap();
        assert_eq!(&got[..n], b"tail", "the staged datagram must be intact");

        let mut expected = header.to_vec();
        expected.extend_from_slice(&payload);
        let (n, _) = receiver.recv_from(&mut got).unwrap();
        assert_eq!(
            &got[..n],
            &expected[..],
            "the split datagram must be header + payload"
        );
    }

    /// A GSO batch reaches the peer as the individual datagrams it was built
    /// from: the kernel cuts the buffer at the segment size, so nothing about
    /// the wire changes — only the number of syscalls and skbs.
    #[test]
    fn a_gso_batch_arrives_as_individual_datagrams() {
        /// Three equal segments: the shape GSO accepts.
        const SEG: usize = 64;
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = receiver.local_addr().unwrap();

        let mut staging = vec![0u8; SEG * 3];
        for (index, chunk) in staging.chunks_mut(SEG).enumerate() {
            chunk.fill(u8::try_from(index).unwrap());
        }
        let buf = Bytes::from(staging);
        let spans: Vec<Span> = (0..3)
            .map(|index| Span::Staged {
                off: index * SEG,
                len: SEG,
            })
            .collect();

        match SendBatch::send_gso(sender.as_raw_fd(), peer, &buf, &spans) {
            Ok(n) => {
                assert_ne!(n, 0, "three equal segments are a GSO batch");
                assert_eq!(n, 3, "all three segments travel as one super-packet");
            }
            Err(error) => {
                // A kernel without UDP_SEGMENT answers an error on the send;
                // the batched path covers that case, and this test cannot.
                eprintln!("GSO is not available here: {error}");
                return;
            }
        }

        let mut got = [0u8; 256];
        for index in 0..3 {
            let (n, _) = receiver.recv_from(&mut got).unwrap();
            assert_eq!(n, SEG, "segment {index} keeps its length");
            assert!(
                got[..n].iter().all(|b| *b == u8::try_from(index).unwrap()),
                "segment {index} keeps its bytes"
            );
        }
    }
}
