//! One carrier session's ARQ health, as the layers above the engine read it.
//!
//! This lives outside [`crate::kcp`] on purpose: the type is two counters and
//! has no dependency on the KCP engine, but the multiplexer and the server's
//! tunnel report thread it through *their* signatures — and those compile in
//! builds without the `kcp` feature. A type that exists only behind a feature
//! its users do not require makes every one of them carry a `cfg`.

#![cfg_attr(
    not(feature = "kcp"),
    allow(
        dead_code,
        reason = "without `kcp` no session carries ARQ counters, so nothing                   constructs this; the type still has to exist because the                   layers that thread it through their signatures compile in                   that build"
    )
)]

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One session's own ARQ counters: what a *per-tunnel* view of the carrier's
/// health reads.
///
/// The KCP engine's process-global counters answer "how is this process
/// doing"; a carrier policy has to answer "how is **this tunnel's** connection
/// doing" — a pool's tunnels are independent sessions, and only the
/// retransmit share of the degraded one rises (measured: 1 vs ~5,000
/// retransmits per cell between the healthy and the lossy carrier, HANDOFF.md
/// round 37's S1). The adapter hands one of these to each engine it spawns; a
/// session without one (tests, tooling) costs nothing — the `Option` keeps the
/// field free until a caller asks for per-session evidence.
#[derive(Debug, Default)]
pub struct SessionHealth {
    /// Datagrams the engine emitted, ARQ payload and its own probes alike.
    pub datagrams_out: AtomicU64,
    /// Segments the engine re-sent: an RTO (silence) or a fast re-send (a gap
    /// the peer's later acks exposed). The share of these in `datagrams_out`
    /// is the degradation signal.
    pub retransmits: AtomicU64,
    /// Bytes the engine emitted, headers included, cumulative.
    ///
    /// The *rate* half of a bandwidth-delay estimate: a window sized to a path
    /// needs the rate the path is carrying, and a session is the only thing
    /// that knows its own (see `kcp::retune_window`).
    pub bytes_out: AtomicU64,
}

impl SessionHealth {
    /// A fresh pair of counters, shared with the engine that will bump them.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Cumulative bytes emitted, headers included.
    pub fn bytes(&self) -> u64 {
        self.bytes_out.load(Relaxed)
    }

    /// The cumulative `(datagrams_out, retransmits)` pair.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.datagrams_out.load(Relaxed),
            self.retransmits.load(Relaxed),
        )
    }
}
