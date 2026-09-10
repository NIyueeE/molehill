//! Batch UDP datagram IO on Linux (`recvmmsg` / `sendmmsg`).
//!
//! KCP pays one syscall per datagram in userspace — the cost TCP amortizes
//! in the kernel via GSO/TSO. Batching a whole readiness wake into one
//! `recvmmsg`/`sendmmsg` call is the same amortization QUIC stacks get
//! from UDP GSO, and it changes nothing on the wire: the datagrams are
//! byte-identical, so peers and protocol are unaffected.
//!
//! This module is an audited unsafe site (the other one is
//! `src/common/multi_map.rs`): every unsafe item below carries a SAFETY
//! comment. Non-Linux platforms keep the single-datagram tokio paths.

#![cfg(target_os = "linux")]

use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::RawFd;

use bytes::Bytes;

/// Datagrams per syscall. 32 × ~1.5 KiB ≈ 48 KiB per wake; far below
/// `UIO_MAXIOV`, and a whole burst fits in one call.
pub const BATCH: usize = 32;

/// Convert a filled `sockaddr_storage` into a `SocketAddr`.
///
/// # Safety
/// `ss` must point to a `sockaddr_storage` that the kernel just filled for
/// an `AF_INET`/`AF_INET6` datagram: `ss_family` then identifies a valid
/// `sockaddr_in`/`sockaddr_in6` prefix of the storage, whose reads are
/// within the storage's size.
#[expect(
    unsafe_code,
    reason = "audited FFI: casting a filled sockaddr_storage to its family-specific type \
              is the standard, kernel-documented way to read a received address"
)]
unsafe fn addr_from_storage(ss: &libc::sockaddr_storage) -> SocketAddr {
    match libc::c_int::from(ss.ss_family) {
        libc::AF_INET => {
            #[expect(
                unsafe_code,
                reason = "audited FFI: kernel-ABI cast of a filled address"
            )]
            // SAFETY: the caller guarantees a filled AF_INET storage; the
            // struct layout of `sockaddr_in` within `sockaddr_storage` is
            // the kernel ABI.
            let sa = unsafe { &*std::ptr::from_ref(ss).cast::<libc::sockaddr_in>() };
            SocketAddr::V4(SocketAddrV4::new(
                std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr)),
                u16::from_be(sa.sin_port),
            ))
        }
        libc::AF_INET6 => {
            #[expect(
                unsafe_code,
                reason = "audited FFI: kernel-ABI cast of a filled address"
            )]
            // SAFETY: same kernel-ABI reasoning as the AF_INET arm.
            let sa = unsafe { &*std::ptr::from_ref(ss).cast::<libc::sockaddr_in6>() };
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

/// Convert a `SocketAddr` into a filled `sockaddr_storage` (for send).
fn storage_from_addr(addr: SocketAddr) -> libc::sockaddr_storage {
    #[expect(
        unsafe_code,
        reason = "audited FFI: zeroed sockaddr_storage is all-zero, safe for any family \
                  to read; the family-specific writes below stay within its size"
    )]
    // SAFETY: zeroed bytes are a valid `sockaddr_storage` for any family;
    // the writes below happen through family-specific views that stay
    // within the storage's size.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            #[expect(
                unsafe_code,
                reason = "audited FFI: bounded write into sockaddr_storage"
            )]
            // SAFETY: `ss` is large enough for a `sockaddr_in`; the written
            // fields are all within it.
            let sa = unsafe { &mut *std::ptr::addr_of_mut!(ss).cast::<libc::sockaddr_in>() };
            sa.sin_family = u16::try_from(libc::AF_INET).unwrap_or(0);
            sa.sin_port = v4.port().to_be();
            sa.sin_addr.s_addr = v4.ip().to_bits().to_be();
        }
        SocketAddr::V6(v6) => {
            #[expect(
                unsafe_code,
                reason = "audited FFI: bounded write into sockaddr_storage"
            )]
            // SAFETY: same reasoning as above for `sockaddr_in6`.
            let sa = unsafe { &mut *std::ptr::addr_of_mut!(ss).cast::<libc::sockaddr_in6>() };
            sa.sin6_family = u16::try_from(libc::AF_INET6).unwrap_or(0);
            sa.sin6_port = v6.port().to_be();
            sa.sin6_addr.s6_addr = v6.ip().octets();
        }
    }
    ss
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
                msg_hdr: libc::msghdr {
                    msg_name: std::ptr::null_mut(),
                    msg_namelen: 0,
                    msg_iov: std::ptr::null_mut(),
                    msg_iovlen: 0,
                    msg_control: std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags: 0,
                },
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
    /// datagrams received; the caller loops until this is 0 (EAGAIN).
    /// Fill the batch from `fd` (non-blocking). Returns the number of
    /// datagrams received, or `Err(WouldBlock)` when drained. Callers
    /// invoke this through `UdpSocket::try_io` so the EAGAIN also clears
    /// tokio's cached readiness.
    pub fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        for (i, msg) in self.msgs.iter_mut().enumerate() {
            msg.msg_hdr.msg_name = std::ptr::addr_of_mut!(self.names[i]).cast();
            msg.msg_hdr.msg_namelen =
                libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_storage>())
                    .unwrap_or(0);
            // SAFETY: `i` < `self.iovs.len()` (both iterate the same
            // count), so the pointer stays within the live allocation.
            #[expect(unsafe_code, reason = "audited FFI: in-bounds pointer arithmetic")]
            let iov_ptr = unsafe { self.iovs.as_mut_ptr().add(i) };
            msg.msg_hdr.msg_iov = iov_ptr;
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
                libc::MSG_DONTWAIT,
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
            #[expect(
                unsafe_code,
                reason = "audited FFI: reads a sockaddr the kernel just filled"
            )]
            // SAFETY: the kernel filled `names[i]` for a received datagram
            // in `recv`, so the family/fields are valid.
            let addr = unsafe { addr_from_storage(&self.names[i]) };
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
// move when the struct moves). `Sync` is safe for the same confinement
// reason: no code path ever shares the batch across tasks.
unsafe impl Send for RecvBatch {}

#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: same reasoning as the `Send` impl above.
unsafe impl Sync for RecvBatch {}

/// Reusable `sendmmsg` state: the scatter-gather descriptors are owned here
/// so their addresses stay stable across calls.
pub struct SendBatch {
    iovs: Vec<libc::iovec>,
    msgs: Vec<libc::mmsghdr>,
}

#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: same confinement reasoning as `RecvBatch`.
unsafe impl Send for SendBatch {}

#[expect(
    unsafe_code,
    reason = "audited FFI: raw-pointer scratch confined to one task, pointers point \
              into the batch's own stable heap allocations"
)]
// SAFETY: same confinement reasoning as `RecvBatch`.
unsafe impl Sync for SendBatch {}

impl Default for SendBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl SendBatch {
    pub fn new() -> Self {
        Self {
            iovs: Vec::with_capacity(BATCH),
            msgs: Vec::with_capacity(BATCH),
        }
    }

    /// Send up to `dgrams.len()` datagrams to one peer in a single syscall.
    /// Returns the number sent, or `Err(WouldBlock)` when the kernel send
    /// buffer is full (the caller retries next pump iteration; a partial
    /// send drops the remainder, which KCP's ARQ absorbs — the segments
    /// stay in the send buffer). Callers invoke this through
    /// `UdpSocket::try_io` so the EAGAIN also clears tokio's cached
    /// writability.
    pub fn send(&mut self, fd: RawFd, peer: SocketAddr, dgrams: &[Bytes]) -> io::Result<usize> {
        let n = dgrams.len().min(BATCH);
        let ss = storage_from_addr(peer);
        self.iovs.clear();
        self.msgs.clear();
        for d in &dgrams[..n] {
            self.iovs.push(libc::iovec {
                iov_base: d.as_ptr() as *mut libc::c_void,
                iov_len: d.len(),
            });
        }
        for i in 0..n {
            // SAFETY: `i` < `self.iovs.len()`, same in-bounds
            // reasoning as `RecvBatch::recv`.
            #[expect(unsafe_code, reason = "audited FFI: in-bounds pointer arithmetic")]
            let iov_ptr = unsafe { self.iovs.as_mut_ptr().add(i) };
            self.msgs.push(libc::mmsghdr {
                msg_hdr: libc::msghdr {
                    // SAFETY of the shared name pointer: `ss` lives for the
                    // whole call below and every message targets the same
                    // peer.
                    msg_name: std::ptr::addr_of!(ss).cast_mut().cast::<libc::c_void>(),
                    msg_namelen: libc::socklen_t::try_from(std::mem::size_of::<
                        libc::sockaddr_storage,
                    >())
                    .unwrap_or(0),
                    msg_iov: iov_ptr,
                    msg_iovlen: 1,
                    msg_control: std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags: 0,
                },
                msg_len: 0,
            });
        }
        #[expect(
            unsafe_code,
            reason = "audited FFI: sendmmsg with caller-owned, in-bounds buffers on a \
                      non-blocking socket (the same pattern QUIC stacks use)"
        )]
        // SAFETY: `self.iovs`/`self.msgs` are this call's own live vectors,
        // each iovec points into a `Bytes` borrowed for the call, `ss`
        // outlives the call, `fd` is a valid non-blocking UDP socket and
        // MSG_DONTWAIT prevents blocking.
        let sent = unsafe {
            libc::sendmmsg(
                fd,
                self.msgs.as_mut_ptr(),
                u32::try_from(n).unwrap_or(0),
                libc::MSG_DONTWAIT,
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
