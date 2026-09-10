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
/// Noise layer). 32 KiB batches 4x more bytes per iteration while keeping
/// memory bounded at 64 KiB per active connection.
#[cfg(any(feature = "client", feature = "server"))]
pub const TCP_COPY_BUFFER_SIZE: usize = 32 * 1024;

/// Default number of pre-established data channels per TCP service.
pub const DEFAULT_TCP_POOL_SIZE: u16 = 8;
/// Default number of pre-established data channels per UDP service.
pub const DEFAULT_UDP_POOL_SIZE: u16 = 2;

/// Queue size for visitor-bound UDP datagrams per data channel, on both the
/// server (affinity routing queue) and the client (channel writer queue).
pub const DEFAULT_UDP_SENDQ_SIZE: usize = 1024;

/// Default number of parallel multiplex tunnels per control session.
///
/// 4 is the measured sweet spot of the transport comparison: it aggregates
/// throughput beyond a single TCP flow (loopback 8-stream 4.3 -> 12 Gbps,
/// 1% loss 3.7 -> 7.1 Gbps in the bench matrix) and cuts head-of-line
/// latency (`rtt10` `HoL` max 101 -> 81 ms) while keeping connection overhead
/// modest. Each tunnel is a full physical connection (TCP handshake +
/// transport crypto + yamux session); `[client.data].default_count = 1` reproduces
/// the single-tunnel behavior.
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_TUNNELS: usize = 4;

/// Upper bound for `[client.data].default_count`; larger values are clamped. Each
/// tunnel is a full physical connection (TCP handshake + transport crypto +
/// yamux session), so the sane range is small by construction.
#[cfg(feature = "multiplex")]
pub const MAX_MUX_TUNNELS: usize = 64;

/// Default total yamux receive window (bytes) advertised per tunnel, on both
/// ends.
///
/// yamux's own default is 1 GiB per connection: under loss the receiver's
/// credit lets the peer keep unbounded data in flight, so the backlog grows
/// without bound (measured: 211 MiB avg / 620 MiB peak at 1% loss on the
/// client end). The cap must still be generous, because yamux's auto-tuner
/// never decreases a stream window and a single stream's steady throughput
/// is roughly `window / (2 * RTT)` — 16 MiB with 32 streams (8 MiB
/// allocatable) measured 30-65% slower on delayed links. With 32 streams
/// reserving 8 MiB, a 64 MiB window leaves 56 MiB allocatable: the full
/// bench matrix keeps its throughput (peak per-tunnel need ~13.8 MiB at
/// 100 ms RTT) and loss backlog stays bounded near `count * 64 MiB`.
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_RECEIVE_WINDOW: usize = 64 * 1024 * 1024;

/// Default maximum concurrent streams per tunnel connection.
///
/// This is NOT a free knob: yamux reserves `streams * 256 KiB` of the
/// connection window as guaranteed credit and only lets the auto-tuner
/// allocate the remainder. 32 streams keep 56 MiB allocatable out of the
/// 64 MiB window; 64 streams would eat the whole window at 16 MiB — and
/// measured 0.1 Gbps at 10 ms RTT (a 30x drop), since the auto-tuner can
/// then never grow any stream's window.
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_MAX_STREAMS: usize = 32;

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
