use backon::ExponentialBuilder;
use std::time::Duration;

/// Default receive buffer size for UDP sockets.
///
/// Covers the maximum Ethernet payload (1500) with IP/UDP header overhead
/// plus some headroom. Configurable per service (`udp_buffer_size`); the wire
/// format carries a `u16` length, so any value up to 65535 is compatible.
pub const DEFAULT_UDP_BUFFER_SIZE: usize = 2048;

/// Per-direction userspace buffer for bidirectional TCP copying.
///
/// tokio's `copy_bidirectional` defaults to two 8 KiB buffers, which costs
/// extra syscalls and task wakeups on fast links (and extra passes through the
/// TLS/Noise layer). 32 KiB batches 4x more bytes per iteration while keeping
/// memory bounded at 64 KiB per active connection; payloads beyond this are
/// split by TLS record limits anyway.
#[cfg(any(feature = "client", feature = "server"))]
pub const TCP_COPY_BUFFER_SIZE: usize = 32 * 1024;

/// Default number of pre-established data channels per TCP service.
pub const DEFAULT_TCP_POOL_SIZE: u16 = 8;
/// Default number of pre-established data channels per UDP service.
pub const DEFAULT_UDP_POOL_SIZE: u16 = 2;

/// Queue size for visitor-bound UDP datagrams per data channel, on both the
/// server (affinity routing queue) and the client (channel writer queue).
pub const DEFAULT_UDP_SENDQ_SIZE: usize = 1024;
/// Default idle timeout (seconds) after which an inactive UDP peer mapping is
/// cleaned up on the client side.
pub const DEFAULT_UDP_IDLE_TIMEOUT_SECS: u64 = 60;

/// Time-to-live (seconds) for the server-side UDP session-affinity table.
///
/// An expired entry only re-shards an idle peer onto another data channel;
/// the proxy client keeps the peer's outbound socket, so the source port the
/// local service sees is unaffected. The TTL bounds memory under address
/// churn (e.g. scans), not session lifetime.
#[cfg(feature = "server")]
pub const UDP_ROUTE_TTL_SECS: u64 = 300;

#[cfg(feature = "client")]
pub fn run_control_chan_backoff(interval: u64) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_factor(3.0)
        .with_max_delay(Duration::from_secs(interval))
        .with_jitter()
}
