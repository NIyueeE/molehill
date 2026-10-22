//! Batch UDP datagram IO on Linux (`recvmmsg` / `sendmmsg`).
//!
//! KCP pays one syscall per datagram in userspace — the cost TCP amortizes
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
//! Measured on the KCP `l4` path (`just bench --profile screen`, four runs),
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

use crate::kcp::KCP_OVERHEAD;
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

/// The most segments one GSO message carries.
///
/// A GSO message is one skb and a queue slot holds one skb, so an uncapped run
/// multiplies the bytes a fixed-depth path may queue by its length. Measured on
/// the bench topology, runs of up to 32 segments put ~20 KiB packets in the
/// veth's queue; eight segments keeps most of the per-skb saving and bounds what
/// one slot can hold (HANDOFF.md).
const GSO_MAX_SEGMENTS: usize = 8;

/// Set once a batch reported that this kernel, or the path to its peer, will not
/// take UDP GSO.
///
/// The first attempt is optimistic — GSO needs no socket option, only a message
/// header — and a refusal is remembered for the process: a datagram path must
/// not pay a refused syscall on every batch. It is process-wide rather than
/// per-peer because the two things that refuse it (an old kernel, a middlebox
/// that cannot reassemble) are properties of the host or the uplink, and the
/// cost of being wrong is only that the old shape is used.
static GSO_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a batch may try UDP GSO.
fn gso_allowed() -> bool {
    !GSO_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stop attempting UDP GSO for the rest of the process.
fn gso_forget() {
    GSO_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The send half's stateless syscall: `nix` builds the per-call descriptors
/// and addresses from the caller's own buffer and payloads.
pub struct SendBatch;

impl SendBatch {
    /// Send the `spans` datagrams of one batch to one peer: equal-sized runs go
    /// out as **one GSO message each**, the rest as one `sendmmsg`.
    ///
    /// Each span is one datagram: either contiguous in the batch's staging `buf`
    /// (one iovec), or a staged header plus an external payload (two iovecs —
    /// the payload pointer is the engine segment's own buffer, so no copy stages
    /// it). A partial send (return value < `spans.len()`) drops the unsent tail,
    /// and `Err(WouldBlock)` means the kernel send buffer is full — in both
    /// cases the caller's ARQ re-emits the datagrams. Callers invoke this
    /// through `UdpSocket::try_io` so the EAGAIN also clears tokio's cached
    /// writability.
    ///
    /// The GSO half is what a userspace ARQ cannot otherwise amortise: the batch
    /// already arrives as equal MSS-sized segments of one contiguous iovec
    /// stream, so `UDP_SEGMENT` lets the kernel build **one** skb and segment it
    /// at the destination instead of building one skb per datagram — measured at
    /// 0.153 s/Gbit of sender time for `sendmmsg` against 0.046 for the same
    /// segments as one GSO message (HANDOFF.md). A kernel or a path that refuses
    /// it is remembered once (`GSO_DISABLED`) and the batch is sent the old way,
    /// and a run longer than `GSO_MAX_SEGMENTS` leaves as several messages so
    /// one queue slot cannot hold an unbounded amount.
    pub fn send_spans(
        fd: RawFd,
        peer: SocketAddr,
        buf: &Bytes,
        spans: &[Span],
    ) -> io::Result<usize> {
        let n = spans.len().min(BATCH);
        let mut rest = &spans[..n];
        let mut sent = 0usize;
        while gso_allowed() {
            let Some((len, run)) = equal_prefix(rest) else {
                break;
            };
            // One queue slot holds one skb: an uncapped run multiplies the bytes
            // a path may hold per slot by its length, which is bufferbloat on a
            // fixed-depth qdisc. The cap keeps the per-skb saving and bounds it.
            let take = run.min(GSO_MAX_SEGMENTS);
            match gso_send(fd, peer, buf, &rest[..take], len) {
                Ok(()) => {
                    sent += take;
                    rest = &rest[take..];
                }
                Err(e) if gso_refused(&e) => {
                    gso_forget();
                    tracing::debug!("UDP GSO refused ({e}); sending this batch with sendmmsg");
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(sent),
                Err(e) => return Err(e),
            }
        }
        if !rest.is_empty() {
            sent += send_batch(fd, peer, buf, rest)?;
        }
        Ok(sent)
    }
}

/// One batch through `sendmmsg`: one syscall, one skb per datagram.
fn send_batch(fd: RawFd, peer: SocketAddr, buf: &Bytes, spans: &[Span]) -> io::Result<usize> {
    let iovs = one_iov_per_datagram(buf, spans);
    let address = SockaddrStorage::from(peer);
    let addrs = vec![Some(address); spans.len()];
    let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(spans.len(), None);
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

/// One `sendmsg` for `spans`, every datagram `len` bytes, segmented by the
/// kernel: `UDP_SEGMENT` makes one skb out of all of them and the destination
/// (or the egress driver) splits it back into datagrams of `len`.
fn gso_send(
    fd: RawFd,
    peer: SocketAddr,
    buf: &Bytes,
    spans: &[Span],
    len: usize,
) -> io::Result<()> {
    // GSO takes the segment size as a u16, and a datagram that large is past any
    // path's MTU: a batch that does not fit goes out the old way.
    let Ok(segment) = u16::try_from(len) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a datagram too large for UDP GSO",
        ));
    };
    let iovs = one_iov_per_datagram(buf, spans);
    let flat: Vec<IoSlice<'_>> = iovs
        .iter()
        .flat_map(|iov| iov.slices[..iov.len].iter().copied())
        .collect();
    let control = [ControlMessage::UdpGsoSegments(&segment)];
    let address = SockaddrStorage::from(peer);
    sendmsg(fd, &flat, &control, MsgFlags::MSG_DONTWAIT, Some(&address))
        .map_err(io::Error::from)?;
    Ok(())
}

/// The iovecs of one batch, one group per datagram.
fn one_iov_per_datagram<'a>(buf: &'a Bytes, spans: &'a [Span]) -> Vec<OneIov<'a>> {
    spans
        .iter()
        .map(|span| match span {
            Span::Staged { off, len } => OneIov {
                slices: [IoSlice::new(&buf[*off..*off + *len]), IoSlice::new(&[])],
                len: 1,
            },
            Span::Split { hdr_off, payload } => OneIov {
                slices: [
                    IoSlice::new(&buf[*hdr_off..*hdr_off + KCP_OVERHEAD]),
                    IoSlice::new(payload),
                ],
                len: 2,
            },
        })
        .collect()
}

/// The longest prefix of `spans` whose datagrams are all `len` bytes, with that
/// length — `None` unless at least two share it, because one datagram is not a
/// segment run.
fn equal_prefix(spans: &[Span]) -> Option<(usize, usize)> {
    let len = span_len(spans.first()?);
    let run = spans
        .iter()
        .take_while(|span| span_len(span) == len)
        .count();
    (run >= 2).then_some((len, run))
}

/// One datagram's on-the-wire length.
fn span_len(span: &Span) -> usize {
    match span {
        Span::Staged { len, .. } => *len,
        Span::Split { payload, .. } => KCP_OVERHEAD + payload.len(),
    }
}

/// Errors that mean "this kernel or this path will not take GSO", as opposed to
/// a full send buffer (`WouldBlock`, which the ARQ recovers) or a real fault.
fn gso_refused(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::EMSGSIZE | libc::EIO | libc::ENOPROTOOPT | libc::EOPNOTSUPP)
    )
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

        let header = [0xABu8; KCP_OVERHEAD];
        let mut staging = vec![0u8; KCP_OVERHEAD + 4];
        staging[..KCP_OVERHEAD].copy_from_slice(&header);
        staging[KCP_OVERHEAD..].copy_from_slice(b"tail");
        let buf = Bytes::from(staging);
        let payload = Bytes::from_static(b"payload");
        let spans = [
            Span::Staged {
                off: KCP_OVERHEAD,
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

    /// The only shape GSO may take: a run of equally sized datagrams. A mixed
    /// batch has no single segment size, and a lone datagram is not a run — both
    /// must fall to the `sendmmsg` path rather than be segmented by the kernel.
    #[test]
    fn only_equal_sized_runs_are_a_gso_candidate() {
        let staged = |len: usize| Span::Staged { off: 0, len };
        let split = |len: usize| Span::Split {
            hdr_off: 0,
            payload: Bytes::from(vec![0u8; len - KCP_OVERHEAD]),
        };
        assert_eq!(
            equal_prefix(&[staged(1472), staged(1472), staged(1472)]),
            Some((1472, 3))
        );
        assert_eq!(
            equal_prefix(&[split(1472), split(1472)]),
            Some((1472, 2)),
            "a payload by reference is one datagram like any other"
        );
        assert_eq!(
            equal_prefix(&[staged(1472), staged(1000), staged(1000)]),
            None,
            "a run of one is not a segment run, however long the batch is"
        );
        assert_eq!(
            equal_prefix(&[staged(1472), staged(1472), staged(1000)]),
            Some((1472, 2))
        );
        assert_eq!(
            equal_prefix(&[staged(1472)]),
            None,
            "one datagram is not a run"
        );
        assert_eq!(equal_prefix(&[staged(1472), staged(1000)]), None);
        assert_eq!(equal_prefix(&[]), None);
    }

    /// The GSO path delivers the same bytes as the batch path: a run of equal
    /// staged datagrams arrives as that many datagrams of that size. On a kernel
    /// or path that refuses GSO the same assertions hold through `sendmmsg`,
    /// which is what the first batch's optimistic attempt is allowed to become.
    #[test]
    fn an_equal_sized_run_arrives_as_its_own_datagrams() {
        const SEG: usize = 1472;
        const RUN: usize = 8;
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = receiver.local_addr().unwrap();

        let mut staging = Vec::with_capacity(SEG * RUN);
        for datagram in 0..RUN {
            let mut one = vec![u8::try_from(datagram).unwrap(); SEG];
            one[..4].copy_from_slice(&u32::try_from(datagram).unwrap().to_be_bytes());
            staging.extend_from_slice(&one);
        }
        let buf = Bytes::from(staging);
        let spans: Vec<Span> = (0..RUN)
            .map(|i| Span::Staged {
                off: i * SEG,
                len: SEG,
            })
            .collect();

        let sent = SendBatch::send_spans(sender.as_raw_fd(), peer, &buf, &spans).unwrap();
        assert_eq!(sent, RUN, "the whole run must be accepted");
        let mut got = vec![0u8; 2048];
        for datagram in 0..RUN {
            let (n, _) = receiver.recv_from(&mut got).unwrap();
            assert_eq!(n, SEG, "every datagram keeps its size");
            assert_eq!(
                u32::from_be_bytes(got[..4].try_into().unwrap()),
                u32::try_from(datagram).unwrap(),
                "datagram {datagram} arrived out of order or split"
            );
        }
    }
}
