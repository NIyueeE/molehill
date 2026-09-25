//! Batch UDP datagram IO on Linux (`recvmmsg` / `sendmmsg`).
//!
//! KCP pays one syscall per datagram in userspace — the cost TCP amortizes
//! in the kernel via GSO/TSO. Batching a whole readiness wake into one
//! `recvmmsg`/`sendmmsg` call is the same amortization QUIC stacks get
//! from UDP GSO, and it changes nothing on the wire: the datagrams are
//! byte-identical, so peers and protocol are unaffected.
//!
//! The unsafe that remains is confined to the FFI boundary itself and the
//! `sockaddr` reinterpretation the kernel ABI defines: the zeroed-syscall
//! templates, the cast that reads a received address back, and the two
//! `mmsg` calls. Everything else — the iovec pointers, the address the
//! datagrams go to, the per-message indexing — is expressed with safe
//! references and bounds-checked slices, so a mistake is a panic rather
//! than UB. The `Send`/`Sync` impls are the one deliberate exception: the
//! reusable descriptor arrays hold raw pointers by construction, and each
//! batch is confined to a single task (the receive batch needs `Sync` as
//! well, because its reader borrow lives across an await). Non-Linux
//! platforms keep the single-datagram tokio paths.
//!
//! This is the codebase's only `unsafe` (`unsafe_code = "deny"` crate-wide,
//! per-item expectations here). It has no `std` equivalent, and the wrapper
//! crates were evaluated rather than assumed: `nix`'s `MultiHeaders` is
//! itself `!Send`, so it cannot remove the `Send`/`Sync` proofs, and
//! `quinn-udp` could remove all eight at the price of GSO/GRO semantics on
//! the send path. The reasoning, and what would have to be measured before
//! changing it, is in docs/lint-policy.md ("Unsafe").

#![cfg(target_os = "linux")]

use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::RawFd;

use crate::kcp::KCP_OVERHEAD;
use bytes::Bytes;

/// Datagrams per syscall. 32 × ~1.5 KiB ≈ 48 KiB per wake; far below
/// `UIO_MAXIOV`, and a whole burst fits in one call.
pub const BATCH: usize = 32;

/// An all-zero `msghdr`: null name/control pointers, zero lengths.
///
/// Built with `zeroed()` rather than a struct literal because musl's
/// `msghdr` carries private padding fields (`__pad1`/`__pad2`) that a
/// literal cannot name, while glibc's has none — the literal compiled only
/// on glibc. Every field the batching code relies on is assigned explicitly
/// before the header is used.
#[expect(
    unsafe_code,
    reason = "audited FFI: `msghdr` is a plain C struct for which all-zero is a valid \
              value (null pointers, zero lengths, no control messages)"
)]
fn empty_msghdr() -> libc::msghdr {
    // SAFETY: all-zero is a valid `msghdr`; the kernel only writes through
    // it inside `recvmmsg`/`sendmmsg`.
    unsafe { std::mem::zeroed() }
}

/// `MSG_DONTWAIT` with the type this target's `recvmmsg`/`sendmmsg` take:
/// glibc declares the flags argument `c_int`, musl `c_uint` (same constant,
/// different ABI spelling — a plain `as` cast would trip the pedantic cast
/// lints, and the value is positive so the conversion cannot fail).
#[cfg(target_env = "musl")]
fn msg_dontwait() -> libc::c_uint {
    u32::try_from(libc::MSG_DONTWAIT).unwrap_or(0)
}

/// See the musl variant above.
#[cfg(not(target_env = "musl"))]
fn msg_dontwait() -> libc::c_int {
    libc::MSG_DONTWAIT
}

/// Convert a kernel-filled `sockaddr_storage` into a `SocketAddr`.
///
/// The address's family decides which `sockaddr_*` struct the storage
/// holds, so it is matched before either cast is taken; an unrecognized
/// family yields the unspecified address, which the caller's peer filter
/// then drops.
fn addr_from_storage(ss: &libc::sockaddr_storage) -> SocketAddr {
    // The family is matched before either cast is taken, so the
    // reinterpretation below is only reached for a storage the kernel
    // filled as that family's `sockaddr_*` struct.
    #[expect(
        unsafe_code,
        reason = "audited FFI: the matched family is the kernel's own guarantee that the \
                  storage holds that family's sockaddr struct, whose layout within \
                  sockaddr_storage is the kernel ABI"
    )]
    // SAFETY: `ss_family` is `AF_INET`/`AF_INET6` in both cast arms, so
    // the storage holds a live `sockaddr_in`/`sockaddr_in6` prefix and
    // every field read stays inside the storage's own 128 bytes.
    unsafe {
        match libc::c_int::from(ss.ss_family) {
            libc::AF_INET => {
                let sa = &*std::ptr::from_ref(ss).cast::<libc::sockaddr_in>();
                SocketAddr::V4(SocketAddrV4::new(
                    std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr)),
                    u16::from_be(sa.sin_port),
                ))
            }
            libc::AF_INET6 => {
                let sa = &*std::ptr::from_ref(ss).cast::<libc::sockaddr_in6>();
                SocketAddr::V6(SocketAddrV6::new(
                    std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr),
                    u16::from_be(sa.sin6_port),
                    0,
                    0,
                ))
            }
            _ => SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0)),
        }
    }
}

/// Reusable `recvmmsg` state: per-datagram buffers, source addresses and
/// the scatter-gather descriptors, all owned here so their addresses stay
/// stable across calls.
pub struct RecvBatch {
    bufs: Vec<Vec<u8>>,
    names: Vec<libc::sockaddr_storage>,
    iovs: Vec<libc::iovec>,
    msgs: Vec<libc::mmsghdr>,
    pub count: usize,
}

impl RecvBatch {
    /// `count` datagrams of up to `buf_len` bytes each.
    pub fn new(count: usize, buf_len: usize) -> Self {
        let mut bufs = Vec::with_capacity(count);
        for _ in 0..count {
            bufs.push(vec![0u8; buf_len]);
        }
        #[expect(
            unsafe_code,
            reason = "audited FFI: zeroed sockaddr_storage is all-zero and read-only until \
                      the kernel fills it in recv()"
        )]
        // SAFETY: zeroed bytes are a valid `sockaddr_storage`; the kernel
        // only writes through the per-message `msg_name` pointer.
        let names = vec![unsafe { std::mem::zeroed() }; count];
        let mut iovs = Vec::with_capacity(count);
        let mut msgs = Vec::with_capacity(count);
        for b in &bufs {
            iovs.push(libc::iovec {
                iov_base: b.as_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            });
            msgs.push(libc::mmsghdr {
                msg_hdr: empty_msghdr(),
                msg_len: 0,
            });
        }
        Self {
            bufs,
            names,
            iovs,
            msgs,
            count: 0,
        }
    }

    /// Fill the batch from `fd` (non-blocking). Returns the number of
    /// datagrams received, or `Err(WouldBlock)` when drained. Callers
    /// invoke this through `UdpSocket::try_io` so the EAGAIN also clears
    /// tokio's cached readiness.
    pub fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        // Point every message at this batch's own storage and iovec. The
        // three arrays are walked side by side, so each descriptor is
        // derived from a bounds-checked index — no unchecked pointer
        // arithmetic, and the borrows are of disjoint fields.
        debug_assert_eq!(self.msgs.len(), self.iovs.len());
        debug_assert_eq!(self.msgs.len(), self.names.len());
        debug_assert_eq!(self.iovs.len(), self.bufs.len());
        let namelen =
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_storage>()).unwrap_or(0);
        for ((msg, iov), name) in self
            .msgs
            .iter_mut()
            .zip(&mut self.iovs)
            .zip(&mut self.names)
        {
            msg.msg_hdr.msg_name = std::ptr::from_mut(name).cast();
            msg.msg_hdr.msg_namelen = namelen;
            msg.msg_hdr.msg_iov = std::ptr::from_mut(iov);
            msg.msg_hdr.msg_iovlen = 1;
            msg.msg_hdr.msg_control = std::ptr::null_mut();
            msg.msg_hdr.msg_controllen = 0;
        }
        #[expect(
            unsafe_code,
            reason = "audited FFI: recvmmsg with caller-owned, in-bounds buffers on a \
                      non-blocking socket (the same pattern QUIC stacks use)"
        )]
        // SAFETY: all pointers reference this batch's own live allocations
        // (bufs/names/iovs/msgs), the iovecs are within bounds, `fd` is a
        // valid non-blocking UDP socket, and MSG_DONTWAIT prevents any
        // blocking; the kernel only writes into those buffers.
        let n = unsafe {
            libc::recvmmsg(
                fd,
                self.msgs.as_mut_ptr(),
                u32::try_from(self.msgs.len()).unwrap_or(0),
                msg_dontwait(),
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                self.count = 0;
                return Err(err);
            }
            return Err(err);
        }
        self.count = usize::try_from(n).unwrap_or(0);
        Ok(self.count)
    }

    /// Iterate the received datagrams as `(source address, bytes)`.
    pub fn iter(&self) -> impl Iterator<Item = (SocketAddr, &[u8])> + '_ {
        (0..self.count).map(|i| {
            let addr = addr_from_storage(&self.names[i]);
            (addr, &self.bufs[i][..self.msgs[i].msg_len as usize])
        })
    }
}

#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: a batch is owned by exactly one task (the dispatcher or the
// session pump); its raw pointers always reference its own heap
// allocations, whose addresses are stable across moves (Vec buffers do not
// move when the struct moves).
unsafe impl Send for RecvBatch {}

#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: the reader loop holds `RecvBatch::iter`'s borrow across an
// await inside the spawned ingress task, so `Sync` is required there, not
// just `Send`. It is sound for the same confinement reason as `Send`: the
// batch never leaves the one task that owns it, so the borrow is never
// shared across threads.
unsafe impl Sync for RecvBatch {}

/// One datagram of a batch on its way to the wire.
///
/// Either a contiguous span of the batch's staging buffer, or a staged
/// header plus an external payload — the engine segment's own buffer,
/// sent as a second iovec so the payload is never copied for the wire.
/// The two shapes are byte-identical on the wire: a datagram is a
/// 24-byte header plus its payload either way.
#[derive(Clone)]
pub enum Span {
    /// A whole datagram at `off..off + len` inside the batch buffer.
    Staged { off: usize, len: usize },
    /// A two-iovec datagram: a `KCP_OVERHEAD` header at `hdr_off` inside
    /// the batch buffer, then `payload` by reference. The `Bytes` handle
    /// is an O(1) share of the engine segment's buffer.
    Split { hdr_off: usize, payload: Bytes },
}

impl Span {
    /// The datagram's total length on the wire (header + payload).
    pub fn len(&self) -> usize {
        match self {
            Span::Staged { len, .. } => *len,
            Span::Split { payload, .. } => KCP_OVERHEAD + payload.len(),
        }
    }
}

/// Reusable `sendmmsg` state: the scatter-gather descriptors are owned here
/// so their addresses stay stable across calls.
pub struct SendBatch {
    iovs: Vec<libc::iovec>,
    msgs: Vec<libc::mmsghdr>,
}

// `Send` only: the send batch is never borrowed across an await, so the
// task it lives in needs no `Sync` proof from it.
#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: same confinement reasoning as `RecvBatch`.
unsafe impl Send for SendBatch {}

impl Default for SendBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl SendBatch {
    pub fn new() -> Self {
        Self {
            // A batch holds at most `BATCH` datagrams, and a two-iovec
            // (split) datagram takes two of them.
            iovs: Vec::with_capacity(2 * BATCH),
            msgs: Vec::with_capacity(BATCH),
        }
    }

    /// Send the `spans` datagrams of one batch to one peer in a single
    /// syscall. Each span is one datagram: either contiguous in the
    /// batch's staging `buf` (one iovec), or a staged header plus an
    /// external payload (two iovecs — the payload pointer is the engine
    /// segment's own buffer, so no copy stages it). A partial send
    /// (return value < `spans.len()`) drops the unsent tail, and
    /// `Err(WouldBlock)` means the kernel send buffer is full — in both
    /// cases the caller's ARQ re-emits the datagrams. Callers invoke this
    /// through `UdpSocket::try_io` so the EAGAIN also clears tokio's
    /// cached writability.
    pub fn send_spans(
        &mut self,
        fd: RawFd,
        peer: SocketAddr,
        buf: &Bytes,
        spans: &[Span],
    ) -> io::Result<usize> {
        let n = spans.len().min(BATCH);
        // socket2 builds the filled address storage (and its exact
        // length) for us; the batch only borrows the pointer below.
        let peer = socket2::SockAddr::from(peer);
        self.iovs.clear();
        self.msgs.clear();
        // Reserve the descriptors this call can need: at most two iovecs
        // and one message per span, so the arrays cannot reallocate and
        // the pointers stored in the messages stay valid.
        self.iovs.reserve(2 * n);
        self.msgs.reserve(n);
        for span in &spans[..n] {
            let iov_start = self.iovs.len();
            match span {
                Span::Staged { off, len } => {
                    // Slicing first: the range is bounds-checked, so an
                    // out-of-range offset panics here instead of handing
                    // the kernel a wild pointer.
                    self.iovs.push(libc::iovec {
                        iov_base: buf[*off..].as_ptr() as *mut libc::c_void,
                        iov_len: *len,
                    });
                }
                Span::Split { hdr_off, payload } => {
                    self.iovs.push(libc::iovec {
                        iov_base: buf[*hdr_off..].as_ptr() as *mut libc::c_void,
                        iov_len: KCP_OVERHEAD,
                    });
                    // The engine segment's own buffer, shared by
                    // reference: `payload.len()` is its exact length and
                    // the caller holds the handle for the whole call.
                    self.iovs.push(libc::iovec {
                        iov_base: payload.as_ptr() as *mut libc::c_void,
                        iov_len: payload.len(),
                    });
                }
            }
            let msg_iovlen = self.iovs.len() - iov_start;
            // `iov_start` indexes the iovec this span just pushed, so the
            // bounds-checked lookup cannot miss; the raw pointer it
            // yields is what the message stores.
            let iov_ptr = {
                let Some(iov) = self.iovs.get_mut(iov_start) else {
                    return Err(io::Error::other("kcp send batch lost its staged iovec"));
                };
                std::ptr::from_mut(iov)
            };
            self.msgs.push(libc::mmsghdr {
                msg_hdr: {
                    let mut hdr = empty_msghdr();
                    hdr.msg_name = peer.as_ptr().cast_mut().cast::<libc::c_void>();
                    hdr.msg_namelen = peer.len();
                    hdr.msg_iov = iov_ptr;
                    hdr.msg_iovlen = msg_iovlen;
                    hdr
                },
                msg_len: 0,
            });
        }
        #[expect(
            unsafe_code,
            reason = "audited FFI: sendmmsg with caller-owned, in-bounds buffers on a \
                      non-blocking socket (the same pattern QUIC stacks use)"
        )]
        // SAFETY: `self.iovs`/`self.msgs` were reserved for exactly this
        // many descriptors and cannot reallocate below, so every message's
        // iovec pointer stays valid; each iovec points into `buf`
        // (bounds-checked above) or into a payload the caller keeps alive
        // for the call, `peer` outlives the call, `fd` is a valid
        // non-blocking UDP socket and MSG_DONTWAIT prevents blocking.
        let sent = unsafe {
            libc::sendmmsg(
                fd,
                self.msgs.as_mut_ptr(),
                u32::try_from(n).unwrap_or(0),
                msg_dontwait(),
            )
        };
        if sent < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Err(err);
            }
            return Err(err);
        }
        Ok(usize::try_from(sent).unwrap_or(0))
    }
}
