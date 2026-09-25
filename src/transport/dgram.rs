//! One outbound datagram batch, described portably.
//!
//! The *shape* of a batch — a reusable staging buffer plus one [`Span`] per
//! datagram — is shared by every platform, because the engine emits datagrams
//! the same way everywhere and the pump consumes them the same way. Only the
//! syscall differs: Linux hands the spans to `sendmmsg` as scatter-gather
//! iovecs (so a [`Span::Split`] payload travels by reference and is never
//! copied), and every other platform reassembles a split datagram into one
//! buffer and sends it with a plain `send_to` (paying one copy, byte-identical
//! on the wire).
//!
//! This module exists so that a platform without `sendmmsg` can still compile
//! the surrounding code: these items used to live in `udp_batch.rs`, which is
//! `#![cfg(target_os = "linux")]`, so the non-Linux send arm referred to types
//! that did not exist there — a macOS/Windows build failure that only CI could
//! see, since the fallback itself was already written.

use bytes::Bytes;

use crate::kcp::KCP_OVERHEAD;

/// Datagrams per syscall. 32 × ~1.5 KiB ≈ 48 KiB per wake; far below
/// `UIO_MAXIOV`, and a whole burst fits in one call.
///
/// Portable on purpose: it also bounds the non-Linux batch, where it is the
/// number of plain `send_to` calls a batch may cost.
pub const BATCH: usize = 32;

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
