//! `mhd`: the data plane's own carrier — reliable streams, unreliable
//! datagrams and unreliable L3 frames over UDP.
//!
//! One **connection** is one pool lane: a UDP 4-tuple, a packet-number space, a
//! connection-level acknowledgement bitmap and a byte-offset credit. Inside it
//! live **streams** (reliable, ordered, used by TCP forwarding and the
//! transparent path) and, from the same engine, unreliable frames that the
//! layers above hand to `dgram` and `transparent` when they can take them.
//! Every stream is addressed by a byte offset, so a lost datagram is a hole in
//! one stream and never a stall of the connection: that is the structural
//! difference from the segment-ordered engine this carrier replaces.
//!
//! The state machine ([`conn`]) owns no socket and reads no clock; [`lane`]
//! turns it into the async seam the pool uses, and [`pump`] moves its datagrams
//! between it and a UDP socket. [`wire`] is the format, [`stream`] the two
//! per-stream buffers, and [`recovery`] the loss detection and timer rules.
//!
//! ## Lineage
//!
//! The protocol line descends from skywind3000's KCP: the loss-detection
//! discipline (a packet three behind the largest acknowledged one is lost, the
//! reordering window is nine eighths of the round trip, the timer backs off
//! exponentially and a path that stays silent is declared dead) and the
//! acknowledgement-block idea are that design's. Everything in this module
//! tree is this repository's own code. The deltas are deliberate:
//!
//! - **packet numbers acknowledge the wire, byte offsets reassemble a stream**,
//!   so loss isolation is structural instead of every segment waiting behind
//!   the missing one;
//! - **one packet carries one frame**, so a retransmission is a byte range and
//!   never a rebuilt segment list;
//! - **credit is an absolute connection-wide byte count**, so a lost credit
//!   update drifts nothing;
//! - **the stream is the unit of fairness**: a round-robin cursor over streams,
//!   not a send queue ordered by arrival;
//! - **no congestion window is cut on loss** (the rate-based controller lives
//!   above this layer), which is the change that makes a lossy path usable
//!   without halving the pipe for every reordering event.
//!
//! ## Security
//!
//! The carrier provides reliability, not confidentiality. Noise rides **above**
//! it, per stream, exactly where the TCP carrier puts it; the carrier's own
//! header carries no secrets. A datagram naming an unknown connection id starts
//! one, so the guard against an unauthenticated flood is a separate budget
//! (see [`pump`]), not a handshake.

pub(crate) mod cipher;
pub(crate) mod conn;
pub(crate) mod lane;
pub(crate) mod pump;
pub(crate) mod recovery;
pub(crate) mod stream;
pub(crate) mod wire;

/// The bytes every staged packet header reserves.
///
/// A packet's head is nine bytes; the acknowledgement block is twelve when a
/// packet carries one, and every data packet does. The batching layer stages a
/// header separately from its payload and needs the width to reserve, which is
/// why the value is stated here rather than reconstructed at the call site.
pub(crate) const MHD_OVERHEAD: usize = wire::HEAD + wire::ACK;

pub(crate) use pump::{MhdLane, MhdListener};

/// Whether lane-root sealing is enabled, read once per process.
///
/// **On by default**, and `MOLEHILL_MHD_SEAL=0` is the hatch that turns it off
/// for one process — the switch exists so a report can be A/B'd in one
/// variable, not because the answer is uncertain.
///
/// A lane's first stream still pays its Noise KK: that handshake is where the
/// root comes from. Every later stream derives its key from it instead. Measured
/// on the product's own open line (2026-11-11, `MOLEHILL_MHD_STATS=1`): the
/// handshake phase goes from 1 221 µs to **11 µs**, a stream open under a bulk
/// transfer from p50 1 414 µs / p99 41 ms to **211 µs / 9 ms**, and the same
/// cell's interactive p99 from 29–48 ms to **7.4–8.7 ms** — for 13 % of the
/// clean-path throughput (9.0 → 7.85 Gbit/s). `churn-16`'s p99 does **not**
/// move: its tail is the request round trip, not the open, which is why the open
/// is now read from the product's own line rather than inferred from a cell.
///
/// **The two reasons it used to be off, and where each stands (2026-11-11).**
///
/// - *A group of four sealed stripes deadlocked about half the time* (2026-11-04:
///   sealed 3 of 6 runs, unsealed 6 of 6; stall signature `written ≫ next` with
///   `credit_blocked=false` and `retransmits=0`, never root-caused). It **did not
///   reproduce** in nine runs of the same shape on this tree — six
///   `striped_data_channels` and three full integration suites, 22/22 each — so
///   either the changes since then removed it or it needs a rarer timing. The
///   switch stays precisely because that is not a root cause, and a recurrence
///   can be isolated to one variable.
/// - *Sealing cost 3.13× per packet* (8.77 → 3.75 Gbit/s on `clean`). Most of
///   that was the AEAD, and most of the AEAD was the implementation, not the
///   cipher: `tests/primitives_test.rs` prices three candidates on this host —
///   `RustCrypto` `ChaCha20-Poly1305` **2 805** ns a 1 400-byte seal, `ring`'s
///   `ChaCha` **1 017**, `ring`'s `AES-256-GCM` **671** — and the sealing path now
///   uses the last. `bulk-n` on `clean` reads **11.30 off / 9.79 on**: 85 % of
///   the unsealed throughput instead of 54 %, at 1.06 CPU-seconds per Gbit
///   against 1.02.
pub(crate) fn sealing_enabled() -> bool {
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};
    static ENABLED: AtomicBool = AtomicBool::new(true);
    static READ: Once = Once::new();
    READ.call_once(|| {
        let set = std::env::var_os("MOLEHILL_MHD_SEAL")
            .is_none_or(|value| value != std::ffi::OsStr::new("0"));
        ENABLED.store(set, Ordering::Relaxed);
    });
    ENABLED.load(Ordering::Relaxed)
}
