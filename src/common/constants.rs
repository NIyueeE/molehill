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

/// How long a forwarded connection may move **no bytes in either direction**
/// before the proxy gives up on it and closes it.
///
/// A stalled forward is not a rare edge: under loss a visitor's socket stops
/// draining, the copy task blocks on its write, stops polling its reader, and
/// the peer's flow-control window closes behind it. Nothing in TCP ends that
/// by itself — an application waiting for a reply that can never arrive has no
/// timeout of its own — and a v0.10.0 data channel is a *stream of a shared
/// tunnel*, so every stalled visitor holds a slice of the tunnel's stream
/// budget. Measured on 2026-09-27: a shaped bulk run wedged one tunnel's 64
/// streams this way until the engine's cap killed the whole tunnel, taking
/// every visitor on it (see HANDOFF.md, "the engine's stream cap is still
/// reachable").
///
/// Five minutes is chosen to be far above any legitimate quiet period in the
/// workloads this project measures (the longest single `iperf3` run is 2 min)
/// while still bounding a wedge in a way an operator can reason about: a
/// connection that has moved nothing for five minutes is reported as failed
/// and its budget returned. It is deliberately not a configuration key yet —
/// the number has one measurement behind it, and the S1-style rule is that a
/// knob arrives with the evidence to tune it.
#[cfg(any(feature = "client", feature = "server"))]
pub const FORWARD_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Default number of data channels a UDP service's worker set uses
/// (`[client.services.<name>].udp_workers`).
///
/// A UDP service's channels are its *workers*: the server shards distinct
/// visitors across them (session affinity) and the pool keeps at least as many
/// tunnels as the workers need, so this is the one per-service channel count
/// that survives. A TCP service has no worker set — one data channel is opened
/// per visitor, on demand.
pub const DEFAULT_UDP_WORKERS: u16 = 2;

/// Queue size for visitor-bound UDP datagrams per data channel, on both the
/// server (affinity routing queue) and the client (channel writer queue).
pub const DEFAULT_UDP_SENDQ_SIZE: usize = 1024;

/// Default total yamux receive window (bytes) advertised per tunnel, on both
/// ends.
///
/// yamux's own default is 1 GiB per connection: under loss the receiver's
/// credit lets the peer keep unbounded data in flight, so the backlog grows
/// without bound (measured: 211 MiB avg / 620 MiB peak at 1% loss on the
/// client end). The window has to be generous because yamux's auto-tuner never
/// decreases a stream window, but it also has to be *bounded enough that a
/// finished test can finish*: on a shaped path the sender keeps filling the
/// credit, so the last control exchange of a run (an `iperf3` test's results)
/// queues behind whatever bulk data is still in flight. At 64 MiB that queue
/// outlived `iperf3`'s own patience — it exited 1 with
/// `unable to receive results` after 121 good intervals, which reads as "the
/// bulk spine produced nothing" to the harness (see HANDOFF.md).
///
/// 32 MiB is the measured compromise on the `rate100` path class: the run
/// completes (client exit 0) at 0.24 Gbit/s against 64 MiB's 0.26 Gbit/s and
/// exit 1 — correctness bought for ~8 % of a rate-limited cell, not the 30-65 %
/// a window that is too small costs on an unshaped delayed link.
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_RECEIVE_WINDOW: usize = 32 * 1024 * 1024;

/// Default maximum concurrent streams per tunnel connection.
///
/// This is NOT a free knob: yamux reserves `streams * 256 KiB` of the
/// connection window as guaranteed credit and only lets the auto-tuner
/// allocate the remainder. A stream count whose reservation swallows the
/// window pins every stream at 256 KiB — measured 0.1 Gbps at 10 ms RTT,
/// a ~30x drop — so the pairing with `DEFAULT_MUX_RECEIVE_WINDOW` is
/// guarded by a unit test (the reservation must stay under half the
/// window). 64 streams reserve 16 MiB of the 32 MiB window and still
/// leave 16 MiB (50%) auto-tunable, while doubling the per-client
/// connection ceiling at the default `tunnels = 4` (128 -> 256).
/// Measured on the `mux1` arm across the full matrix: see HANDOFF.md,
/// "Phase 4: L2 landed".
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_MAX_STREAMS: usize = 64;

/// Upper bound for `[client.data.tcp|kcp].tunnels`.
///
/// `tunnels` is how many carrier connections a pool establishes at service
/// start and keeps for its lifetime, so it is validated (`>= 1`) and clamped,
/// never silently obeyed with an absurd value.
#[cfg(feature = "multiplex")]
pub const MAX_MUX_TUNNELS_CAP: u16 = 64;

/// Default `[client.data.tcp|kcp].tunnels`.
///
/// Four is the number the pool carried as its elastic cap's default, and the
/// measurements behind the sizing guidance are unchanged by the pool being
/// fixed instead of elastic: one tunnel is the worst configuration on every
/// path (an L3 claim on one carrier connection measures the same for one inner
/// flow and for eight), two captures most of the lossy-path gain
/// (8 flows: 3.34 → 5.92 Gbit/s on `loss1`), and more keeps paying on a clean
/// fast path (12.85 → 22.81 Gbit/s from one tunnel to eight). An explicit
/// `tunnels` overrides it, and a UDP service's declared worker set raises it
/// (see `udp_floor`).
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_TUNNELS: u16 = 4;

// The pool no longer reaps idle tunnels (`[client.data].idle_timeout` and this
// constant are gone with the elastic model): a pool's tunnels are established
// at service start and kept, so that the capacity a deployment offers does not
// depend on what it happened to be doing a minute ago.

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
