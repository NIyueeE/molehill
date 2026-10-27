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
