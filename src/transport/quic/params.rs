//! The engine's measured transport parameters.
//!
//! These are not constants of convenience: the flow-control windows are what a
//! stream and a connection may have in flight, and the congestion controller is
//! the one the bench model's method fingerprint names. The defaults are
//! quinn-proto's (Cubic, a 1.25 MiB stream window, unlimited connection
//! window); this carrier carries bulk traffic on a path whose round trip the
//! interactive streams share, so the windows have to cover the measured
//! bandwidth-delay product and the controller has to be the one the plan
//! chose.
//!
//! A value here is part of the measurement's method, not a tuning knob: the
//! bench fingerprint records it, and changing one re-baselines the carrier
//! numbers.

use std::sync::Arc;

use quinn_proto::TransportConfig;
use quinn_proto::VarInt;
use quinn_proto::congestion::BbrConfig;

/// What one stream may have in flight, in each direction.
///
/// A stream can only carry what fits in its window per round trip: 3 Gbit/s
/// over a 20 ms leg is 7.5 MB, and the bench's `bulk-1` cell at 1 % loss moved
/// from 0.454 to 2.405 Gbit/s once the window (and the controller's initial
/// window below) stopped capping it below the path's bandwidth-delay product.
pub(crate) const STREAM_WINDOW: u32 = 32 * 1024 * 1024;

/// What the whole connection may have in flight. Streams may each fill their
/// own window, and the ceiling is the sum a path's round trip has to hold:
/// eight streams at 12 Gbit/s over 20 ms is 30 MB, which is the `bulk-n` cell
/// this number exists for. It still bounds a lazy reader's pinned memory.
pub(crate) const CONNECTION_WINDOW: u32 = 64 * 1024 * 1024;

/// The engine's configuration for one endpoint, client or server.
pub(crate) fn transport_config() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        // BBR is the controller this carrier was designed around: it does not
        // read the clean path's tail as loss, and it is the one the fingerprint
        // records. Cubic stays the crate's default for a reason; this is a
        // choice, and the bench gate prices it.
        .congestion_controller_factory(Arc::new({
            // A BDP-sized initial window: the engine's own default is ten
            // segments, and on a 20 ms path this carrier has to fill megabytes
            // before the controller can measure anything. Starting at 2 MiB is
            // what lets the first round carry a link's worth instead of
            // crawling up from 12 KiB while the path keeps dropping packets.
            let mut bbr = BbrConfig::default();
            bbr.initial_window(2 * 1024 * 1024);
            bbr
        }))
        // A 1200-byte datagram is QUIC's guaranteed minimum, not a choice: on a
        // 1500-byte path it buys 20 % more packets per byte than the path
        // allows, and the carrier's cost is per packet. Discovery stays off
        // (`allow_mtud` is false at every call site), so this is the size every
        // path gets: it is safe under a 1500-byte link MTU, which is what the
        // bench and every tunnel this carrier rides on carries.
        .initial_mtu(1452)
        .stream_receive_window(VarInt::from_u32(STREAM_WINDOW))
        .receive_window(VarInt::from_u32(CONNECTION_WINDOW))
        .send_window(u64::from(CONNECTION_WINDOW))
        // The pool repairs a dead tunnel; an idle timeout turns a silently
        // dead NAT mapping into that repair rather than a stuck lane. 60 s is
        // above the pool's 60 s re-probe, so a healthy idle tunnel survives a
        // probe and a dead one is noticed within the repair tick.
        .max_idle_timeout(Some(VarInt::from_u32(60_000).into()));
    Arc::new(config)
}
