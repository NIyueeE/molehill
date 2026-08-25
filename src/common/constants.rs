use backon::ExponentialBuilder;
use std::time::Duration;

/// Receive buffer size for UDP sockets.
///
/// Covers the maximum Ethernet payload (1500) with IP/UDP header overhead
/// plus some headroom, while remaining safe for stack allocation.
pub const UDP_BUFFER_SIZE: usize = 2048;

/// Per-direction userspace buffer for bidirectional TCP copying.
///
/// tokio's `copy_bidirectional` defaults to two 8 KiB buffers, which costs
/// extra syscalls and task wakeups on fast links (and extra passes through the
/// TLS/Noise layer). 32 KiB batches 4x more bytes per iteration while keeping
/// memory bounded at 64 KiB per active connection; payloads beyond this are
/// split by TLS record limits anyway.
#[cfg(any(feature = "client", feature = "server"))]
pub const TCP_COPY_BUFFER_SIZE: usize = 32 * 1024;

#[cfg(feature = "client")]
pub const UDP_SENDQ_SIZE: usize = 1024;
#[cfg(feature = "client")]
pub const UDP_TIMEOUT: u64 = 60;

#[cfg(feature = "server")]
pub fn listen_backoff() -> ExponentialBuilder {
    ExponentialBuilder::default().with_max_delay(Duration::from_secs(1))
}

#[cfg(feature = "client")]
pub fn run_control_chan_backoff(interval: u64) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_factor(3.0)
        .with_max_delay(Duration::from_secs(interval))
        .with_jitter()
}
