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
    ControlMessage, ControlMessageOwned, MsgFlags, MultiHeaders, SockaddrStorage, recvmsg,
    sendmmsg, sendmsg,
};

use crate::transport::mhd::MHD_OVERHEAD;

use super::dgram::{BATCH, Span};
#[cfg(test)]
use bytes::Bytes;

/// Bytes in one receive region: a GRO run coalesces up to this much before the
/// kernel hands it back.
pub const GRO_BUF: usize = 64 * 1024;

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

/// The receive half: one region big enough for a GRO run, and where each
/// datagram sits inside it.
///
/// With `UDP_GRO` on, one `recvmsg` returns a whole coalesced run rather than one
/// datagram, so the buffer is sized for a run and the kernel hands back the
/// segment size it used. Splitting on that size is what keeps the rest of the
/// carrier unchanged: every consumer still sees individual datagrams.
pub struct RecvBatch {
    /// Filled by one recvmsg per call.
    region: Vec<u8>,
    /// Each datagram's (offset, length) inside region, after splitting.
    spans: Vec<(usize, usize)>,
    /// The run's source address. GRO coalesces one 4-tuple only, so a run has
    /// one peer.
    peer: SocketAddr,
    /// Datagrams the last call produced.
    pub count: usize,
}

impl RecvBatch {
    /// A batch for `count` datagrams of up to `buf_len` bytes each.
    ///
    /// The region is sized for a **whole GRO run** whatever the caller's nominal
    /// batch: the kernel coalesces up to [`GRO_BUF`] of segments into one
    /// datagram, and `recvmsg` without `MSG_TRUNC` answers only what it copied.
    /// A region smaller than a run therefore truncates it — the tail is lost
    /// without an error, which on a bulk transfer is a hole in the stream. `count`
    /// bounds how many datagrams the caller will see, not how many the kernel
    /// hands back in one piece.
    pub fn new(count: usize, buf_len: usize) -> Self {
        let total = count.saturating_mul(buf_len);
        Self {
            region: vec![0u8; GRO_BUF.max(buf_len.max(1)).max(total)],
            spans: Vec::with_capacity(BATCH),
            peer: SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0)),
            count: 0,
        }
    }

    /// Fill the batch from fd (non-blocking): one recvmsg, split into the
    /// datagrams it carried. Callers invoke this through `UdpSocket::try_io` so
    /// the EAGAIN also clears tokio's cached readiness.
    pub fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        self.count = 0;
        self.spans.clear();
        let mut region = std::mem::take(&mut self.region);
        let room = region.len();
        let mut space = nix::cmsg_space!(i32);
        // The region is put back on every path: a socket with nothing queued
        // answers EAGAIN on most iterations, and losing the buffer to the first
        // one would leave a batch that can never receive again.
        //
        // `MSG_TRUNC` makes a run that did not fit report its real length
        // instead of the truncated one, which is the only way to notice. Silently
        // keeping the copy would drop the tail of the run, and a dropped tail is
        // a hole in a stream.
        let received = {
            let mut iov = [IoSliceMut::new(&mut region)];
            recvmsg::<SockaddrStorage>(
                fd,
                &mut iov,
                Some(&mut space),
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_TRUNC,
            )
            .map(|msg| {
                let peer = msg.address.as_ref().map_or_else(
                    || SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0)),
                    socket_addr,
                );
                let segment = msg.cmsgs().ok().and_then(|mut cmsgs| {
                    cmsgs.find_map(|cmsg| match cmsg {
                        ControlMessageOwned::UdpGroSegments(size) if size > 0 => {
                            Some(usize::try_from(size).unwrap_or(0))
                        }
                        _ => None,
                    })
                });
                (msg.bytes, peer, segment)
            })
        };
        self.region = region;
        let (len, peer, segment) = received.map_err(io::Error::from)?;
        if len > room {
            // The run was bigger than the buffer. Keeping the part that fitted
            // would hand the caller a run whose segment boundaries lie (the last
            // piece is cut mid-datagram), so the whole run is dropped and the
            // peer's recovery re-sends it.
            tracing::debug!("mhd receive dropped a {len}-byte run: its buffer is {room}");
            return Ok(0);
        }
        self.peer = peer;
        // A run with a segment size came from the kernel's GRO; anything else
        // is one datagram, which is what a socket without GRO hands back.
        match segment.filter(|size| *size > 0 && *size <= len) {
            Some(size) => {
                let mut off = 0;
                while off < len && self.spans.len() < BATCH {
                    let take = size.min(len - off);
                    self.spans.push((off, take));
                    off += take;
                }
            }
            None => {
                if len > 0 {
                    self.spans.push((0, len));
                }
            }
        }
        self.count = self.spans.len();
        Ok(self.count)
    }

    /// Iterate the received datagrams as (source address, bytes).
    pub fn iter(&self) -> impl Iterator<Item = (SocketAddr, &[u8])> + '_ {
        self.spans
            .iter()
            .map(move |(off, len)| (self.peer, &self.region[*off..*off + *len]))
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
        buf: &[u8],
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
    /// MTU-sized segments all share; the trailing piece may be shorter. A run
    /// with a short piece anywhere but the end is not eligible at all: a partial
    /// super-packet would silently drop the spans it did not carry, and the
    /// caller has already marked them sent.
    pub fn send_gso(fd: RawFd, peer: SocketAddr, buf: &[u8], spans: &[Span]) -> io::Result<usize> {
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
            // GSO allows exactly one **shorter** segment, and only as the last one. A
            // *longer* tail is not a segment this batch can carry: handing it to
            // `UDP_SEGMENT` makes the kernel split it into pieces of `segment` bytes, and
            // the receiver gets fragments of a packet instead of the packet. That is how a
            // lane's first stream packet — the run is [credit, stream@0] — arrived as five
            // 17-byte fragments and was refused by the decoder on every lane
            // (`HANDOFF.md` §4.210). The caller falls back to one datagram per span.
            if n + 1 != limit || spans[n].len() > segment {
                return Ok(0);
            }
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
    /// A run whose second datagram is *longer* than the first is not a GSO batch.
    ///
    /// `UDP_SEGMENT` splits the whole buffer into segments of one size, so a tail longer
    /// than that size is chopped into fragments of it — the packet never arrives whole, and
    /// the receiver refuses the pieces as malformed. The run that opens a lane is exactly
    /// that shape: a 17-byte credit followed by the first stream packet.
    #[test]
    fn a_longer_tail_is_refused_so_the_caller_sends_it_whole() {
        use std::os::fd::AsRawFd;
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = receiver.local_addr().unwrap();
        let buf = Bytes::from(vec![0u8; 17 + 74]);
        let spans = [
            Span::Staged { off: 0, len: 17 },
            Span::Staged { off: 17, len: 74 },
        ];
        let refused = SendBatch::send_gso(sender.as_raw_fd(), peer, &buf, &spans);
        assert!(
            matches!(refused, Ok(0)),
            "a longer tail is not a GSO batch, the caller must send the spans itself: {refused:?}"
        );
    }

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

    /// Two datagrams sent back to back arrive as two, whichever way the kernel
    /// delivered them: a GRO run is split on the segment size the kernel
    /// reports, and a plain datagram passes through whole. The boundaries are
    /// the assertion — coalescing must never merge two datagrams into one
    /// payload.
    #[test]
    fn a_receive_batch_splits_a_gro_run_back_into_datagrams() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let _ = nix::sys::socket::setsockopt(
            &receiver,
            nix::sys::socket::sockopt::UdpGroSegment,
            &true,
        );
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = receiver.local_addr().unwrap();
        sender.send_to(b"first", peer).unwrap();
        sender.send_to(b"second", peer).unwrap();

        let mut batch = RecvBatch::new(4, 2048);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while seen.len() < 2 && std::time::Instant::now() < deadline {
            match batch.recv(receiver.as_raw_fd()) {
                Ok(0) => std::thread::sleep(Duration::from_millis(1)),
                Ok(_) => {
                    for (_, bytes) in batch.iter() {
                        seen.push(bytes.to_vec());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    eprintln!("unexpected receive error: {e}");
                    break;
                }
            }
        }
        assert_eq!(
            seen,
            vec![b"first".to_vec(), b"second".to_vec()],
            "both datagrams must arrive, each intact and in order"
        );
    }
}
