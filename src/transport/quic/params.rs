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
/// The BDP of a 4 Gbit/s path at a 10 ms tail is about 5 MB; 8 MiB leaves room
/// without letting one stream hold an unbounded amount of the receiver's
/// memory.
pub(crate) const STREAM_WINDOW: u32 = 8 * 1024 * 1024;

/// What the whole connection may have in flight. Two streams may each fill
/// their own window; the connection ceiling keeps a lazy reader from pinning
/// an unbounded buffer.
pub(crate) const CONNECTION_WINDOW: u32 = 16 * 1024 * 1024;

/// The engine's configuration for one endpoint, client or server.
pub(crate) fn transport_config() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        // BBR is the controller this carrier was designed around: it does not
        // read the clean path's tail as loss, and it is the one the fingerprint
        // records. Cubic stays the crate's default for a reason; this is a
        // choice, and the bench gate prices it.
        .congestion_controller_factory(Arc::new(BbrConfig::default()))
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
