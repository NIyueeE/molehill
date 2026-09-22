// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

//! The yamux framing engine, maintained in-repo as molehill's own code.
//!
//! Vendored from [rust-yamux](https://github.com/paritytech/yamux) 0.14.0
//! when the multiplex engine moved in-repo: it was the last network-layer
//! protocol engine owned by an external crate, and the changes molehill
//! wants on the framing path (frame split size, the stream cap, credit
//! allocation, the IO layer) need code ownership rather than call-site
//! tuning. It is a move, not a rewrite — the engine stays wire-identical
//! with the [yamux specification](https://github.com/hashicorp/yamux/blob/master/spec.md),
//! so a 0.8.x peer keeps interoperating. The phased plan that follows the
//! vendoring is recorded in HANDOFF.md, "Direction ① design document".
//!
//! Deviations from the vendored copy: logging goes through
//! `tracing` instead of the `log` facade; `web-time` and
//! `static_assertions` are replaced by `std` equivalents; the upstream
//! property tests (their `quickcheck` dev-dependency) are dropped in
//! favour of molehill's e2e suite; internal paths are rebased under
//! `crate::mux`.
//!
//! The two primary objects the transport layer interacts with are:
//!
//! - [`Connection`], which wraps the underlying I/O resource, e.g. a socket, and
//!   provides methods for opening outbound or accepting inbound streams.
//! - [`Stream`], which implements tokio's `AsyncRead` / `AsyncWrite` traits
//!   directly — the engine is tokio-native since the futures-io layer was
//!   dropped, so the transport passes its sockets and streams in without a
//!   compatibility shim.

#![forbid(unsafe_code)]

mod chunks;
mod error;
mod frame;

pub(crate) mod connection;
mod tagged_stream;

pub use crate::mux::connection::{Connection, Mode, Stream};
pub use crate::mux::error::ConnectionError;
pub use crate::mux::frame::header::StreamId;

const KIB: usize = 1024;
const MIB: usize = KIB * 1024;
/// `MIB` as `f64` for the flow-control window maths (lossless: 2^20 is
/// exactly representable).
#[expect(
    clippy::cast_precision_loss,
    reason = "MIB is 2^20, exactly representable in f64"
)]
const MIB_F64: f64 = MIB as f64;
const GIB: usize = MIB * 1024;

#[expect(
    clippy::cast_possible_truncation,
    reason = "256 KiB fits comfortably in u32"
)]
pub const DEFAULT_CREDIT: u32 = 256 * KIB as u32; // as per yamux specification

pub type Result<T> = std::result::Result<T, ConnectionError>;

/// The maximum number of streams we will open without an acknowledgement from the other peer.
///
/// This enables a very basic form of backpressure on the creation of streams.
const MAX_ACK_BACKLOG: usize = 256;

/// Default maximum number of bytes a Yamux data frame might carry as its
/// payload when being send. Larger Payloads will be split.
///
/// The data frame payload size is not restricted by the yamux specification.
/// Still, this implementation restricts the size to:
///
/// 1. Reduce delays sending time-sensitive frames, e.g. window updates.
/// 2. Minimize head-of-line blocking across streams.
/// 3. Enable better interleaving of send and receive operations, as each is
///    carried out atomically instead of concurrently with its respective
///    counterpart.
///
/// For details on why this concrete value was chosen, see
/// <https://github.com/paritytech/yamux/issues/100>.
const DEFAULT_SPLIT_SEND_SIZE: usize = 32 * KIB;

/// Cumulative frame counters for the framing path, used by the optional
/// periodic stats line (`MOLEHILL_MUX_STATS=1`).
///
/// They exist so a run can attribute cost to the path: frames/s beside the
/// measured CPU turns a throughput number into CPU-per-frame, which is what
/// separates "the engine does too much work per frame" from "there are too
/// many frames". A relaxed atomic add per frame is a few nanoseconds against
/// the frame's own cost, and the counters are only read by the stats task.
pub(crate) static FRAMES_WRITTEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static FRAMES_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Body bytes only (the 12-byte headers excluded) across both directions.
pub(crate) static FRAME_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the framing counters, for the periodic stats line.
pub(crate) fn framing_stats() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        FRAMES_WRITTEN.load(Relaxed),
        FRAMES_READ.load(Relaxed),
        FRAME_BYTES.load(Relaxed),
    )
}

/// Yamux configuration.
///
/// The default configuration values are as follows:
///
/// - max. for the total receive window size across all streams of a connection = 1 GiB
/// - max. number of streams = 512
/// - read after close = true
/// - split send size = 32 KiB (the vendored default is 16 KiB; the 32 KiB
///   split measured +45.7% non-overlapping on the single-tunnel 8-stream
///   cell and +5..9% on the loss cells, with everything else inside the
///   spread — the original 16 KiB preference came from a run against the
///   dead-receiver leak described in HANDOFF.md, "The leaked receivers",
///   and its numbers are not comparable)
#[derive(Debug, Clone)]
pub struct Config {
    max_connection_receive_window: Option<usize>,
    max_num_streams: usize,
    read_after_close: bool,
    split_send_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_connection_receive_window: Some(GIB),
            max_num_streams: 512,
            read_after_close: true,
            split_send_size: DEFAULT_SPLIT_SEND_SIZE,
        }
    }
}

impl Config {
    /// Set the upper limit for the total receive window size across all streams of a connection.
    ///
    /// Must be `>= 256 KiB * max_num_streams` to allow each stream at least the Yamux default
    /// window size.
    ///
    /// The window of a stream starts at 256 KiB and is increased (auto-tuned) based on the
    /// connection's round-trip time and the stream's bandwidth (striving for the
    /// bandwidth-delay-product).
    ///
    /// Set to `None` to disable limit, i.e. allow each stream to grow receive window based on
    /// connection's round-trip time and stream's bandwidth without limit.
    ///
    /// ## DOS attack mitigation
    ///
    /// A remote node (attacker) might trick the local node (target) into allocating large stream
    /// receive windows, trying to make the local node run out of memory.
    ///
    /// This attack is difficult, as the local node only increases the stream receive window up to
    /// 2x the bandwidth-delay-product, where bandwidth is the amount of bytes read, not just
    /// received. In other words, the attacker has to send (and have the local node read)
    /// significant amount of bytes on a stream over a long period of time to increase the stream
    /// receive window. E.g. on a 60ms 10Gbit/s connection the bandwidth-delay-product is ~75 MiB
    /// and thus the local node will at most allocate ~150 MiB (2x bandwidth-delay-product) per
    /// stream.
    ///
    /// Despite the difficulty of the attack one should choose a reasonable
    /// `max_connection_receive_window` to protect against this attack, especially since an attacker
    /// might use more than one stream per connection.
    pub fn set_max_connection_receive_window(&mut self, n: Option<usize>) -> &mut Self {
        self.max_connection_receive_window = n;

        assert!(
            self.max_connection_receive_window.unwrap_or(usize::MAX)
                >= self.max_num_streams * DEFAULT_CREDIT as usize,
            "`max_connection_receive_window` must be `>= 256 KiB * max_num_streams` to allow each
            stream at least the Yamux default window size"
        );

        self
    }

    /// Set the max. number of streams per connection.
    pub fn set_max_num_streams(&mut self, n: usize) -> &mut Self {
        self.max_num_streams = n;

        assert!(
            self.max_connection_receive_window.unwrap_or(usize::MAX)
                >= self.max_num_streams * DEFAULT_CREDIT as usize,
            "`max_connection_receive_window` must be `>= 256 KiB * max_num_streams` to allow each
            stream at least the Yamux default window size"
        );

        self
    }

    /// Allow or disallow streams to read from buffered data after
    /// the connection has been closed.
    /// Set the max. payload size used when sending data frames. Payloads larger
    /// than the configured max. will be split.
    // The frame-split lever (direction ① phase 3) adds the caller; until
    // then the knob is part of the vendored config surface, unused here.
    #[expect(dead_code, reason = "L1 (conditional frame split) adds the caller")]
    pub fn set_split_send_size(&mut self, n: usize) -> &mut Self {
        self.split_send_size = n;
        self
    }
}

// Check that we can safely cast a `usize` to a `u64`.
const _: () = assert!(std::mem::size_of::<usize>() <= std::mem::size_of::<u64>());

// Check that we can safely cast a `u32` to a `usize`.
const _: () = assert!(std::mem::size_of::<u32>() <= std::mem::size_of::<usize>());
