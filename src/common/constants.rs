use backon::ExponentialBuilder;
use std::time::Duration;

/// Receive buffer size for UDP sockets.
///
/// Covers the maximum Ethernet payload (1500) with IP/UDP header overhead
/// plus some headroom, while remaining safe for stack allocation.
pub const UDP_BUFFER_SIZE: usize = 2048;
pub const UDP_SENDQ_SIZE: usize = 1024;
pub const UDP_TIMEOUT: u64 = 60;

pub fn listen_backoff() -> ExponentialBuilder {
    ExponentialBuilder::default().with_max_delay(Duration::from_secs(1))
}

pub fn run_control_chan_backoff(interval: u64) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_factor(3.0)
        .with_max_delay(Duration::from_secs(interval))
        .with_jitter()
}
