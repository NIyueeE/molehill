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

/// The default ceiling on a UDP service's worker set
/// (`[client.services.<name>].max_udp_workers`).
///
/// A UDP service starts with **one** data channel and the server asks for
/// another when its affinity/queue state needs one, up to this ceiling. The
/// set is therefore demand-driven rather than configured at its final width.
pub const DEFAULT_MAX_UDP_WORKERS: u16 = 2;

/// The hard ceiling on `max_udp_workers`, like the pool cap: a configuration
/// typo is clamped rather than obeyed.
pub const MAX_UDP_WORKERS_CAP: u16 = 64;

/// Queue depth of one UDP data channel's writer, on both ends. Internal: the
/// policy that matters is `CoDel`'s sojourn target below, not a queue length.
pub const UDP_QUEUE_SIZE: usize = 1024;

/// `CoDel`'s sojourn target for UDP: a datagram that has waited longer than this
/// in a queue is dropped instead of being delivered late.
pub const UDP_CODEL_TARGET: std::time::Duration = std::time::Duration::from_millis(10);

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

/// Upper bound for `[client.data].tunnels`.
///
/// `tunnels` is how many carrier connections a pool establishes at service
/// start and keeps for its lifetime, so it is validated (`>= 1`) and clamped,
/// never silently obeyed with an absurd value.
#[cfg(feature = "multiplex")]
pub const MAX_MUX_TUNNELS_CAP: u16 = 64;

/// Default `[client.data].tunnels`.
///
/// Four is the number the pool carried as its elastic cap's default, and the
/// measurements behind the sizing guidance are unchanged by the pool being
/// fixed instead of elastic: one tunnel is the worst configuration on every
/// path (an L3 claim on one carrier connection measures the same for one inner
/// flow and for eight), two captures most of the lossy-path gain
/// (8 flows: 3.34 → 5.92 Gbit/s on `loss1`), and more keeps paying on a clean
/// fast path (12.85 → 22.81 Gbit/s from one tunnel to eight). An explicit
/// `tunnels` replaces it; nothing raises it above the written value.
#[cfg(feature = "multiplex")]
pub const DEFAULT_MUX_TUNNELS: u16 = 4;

/// Upper bound for a transparent client's lane budget
/// (`[transparent.data].tunnels`).
///
/// A claim's lane **is** a carrier connection — no multiplexer sits above it —
/// so this is not the pool ceiling ([`MAX_MUX_TUNNELS_CAP`]): a client serving
/// a hundred claims needs at least a hundred lanes before any claim can hold a
/// second one. The bound is a resource guard, not a tuning knob: a lane is a
/// connection, measured at 2.6 file descriptors and 0.5–0.8 MiB RSS with no
/// threads, and a typo should be refused rather than dialled.
#[cfg(feature = "carrier")]
pub const MAX_TRANSPARENT_LANES: u16 = 1024;

/// The lanes a claim gets when the client writes no budget
/// (`[transparent.data].tunnels`).
///
/// **Two — the largest value that keeps both axes.** A claim's data path is
/// serialized per lane, so a busy claim's throughput follows its lanes; so does
/// the pressure it puts on the one device reader every claim of the client
/// shares, which is what the second table measures. One host, one binary, five
/// claims, everything else at its default:
///
/// Bulk alone (`bulk-n`, eight inner flows):
///
/// | lanes per claim | throughput | CPU per Gbit |
/// |---|---|---|
/// | 1 | 2.351 Gbit/s | 2.482 s |
/// | **2** | **3.283 (+40 %)** | 3.076 |
/// | 4 | 5.191 (+121 %) | 2.329 |
/// | 8 | 5.255 | 2.253 |
///
/// With an interactive stream beside the load (the capacity ramp: one fresh
/// connection per sample, and the SLO's 50 ms p99):
///
/// | lanes per claim | streams sustained | plateau | interactive p99 |
/// |---|---|---|---|
/// | **2** | **8 — the ramp's ceiling** | **3.32 Gbit/s** | **2 ms** |
/// | 4 | 3 | one level short of the plateau | 271 ms from four streams |
/// | 4, lending off | 3 | ditto | ditto |
///
/// Four lanes buys another 58 % of *bulk* throughput and spends the interactive
/// stream's headroom doing it: 5.19 Gbit/s at five bulk streams, with the
/// probe's p99 at 560 ms and its sample rate halved. Two lanes hold the whole
/// eight-stream ramp at a 2 ms p99 and still carry 40 % more than one. A
/// default has to serve both — the bulk ceiling is reachable by writing
/// `tunnels`, and the SLO is not something a default may quietly break — so it
/// is two, and the first version of this constant said four on the strength of
/// the first table alone.
///
/// The cost is linear in the claim count, and the configuration page prices it:
/// `lanes x claims` connections, each a socket, ~2.6 file descriptors and
/// 0.5-0.8 MiB RSS on **both** ends, and a slot in the server's
/// `[server.data].max_tunnels` valve (unlimited by default).
pub const DEFAULT_CLAIM_LANES: u16 = 2;

// The pool no longer reaps idle tunnels (`[client.data].idle_timeout` and this
// constant are gone with the elastic model): a pool's tunnels are established
// at service start and kept, so that the capacity a deployment offers does not
// depend on what it happened to be doing a minute ago.

/// The adaptive lifetime of one UDP peer mapping.
///
/// A peer that talks steadily keeps its mapping (and the local source port the
/// service sees); a peer that has gone quiet gives it back. Four times the
/// largest gap observed for that peer, bounded to 30-300 s: the lower bound
/// keeps a bursty game session alive between rounds, and the upper bound is
/// the same resource guard the fixed timeout used to be. Pure and logged by
/// the caller.
#[must_use]
pub fn udp_lifetime(max_gap: std::time::Duration) -> std::time::Duration {
    const MIN: std::time::Duration = std::time::Duration::from_secs(30);
    const MAX: std::time::Duration = std::time::Duration::from_secs(300);
    (max_gap.saturating_mul(4)).clamp(MIN, MAX)
}

/// Time-to-live (seconds) for the server-side UDP session-affinity table.
///
/// An expired entry only re-shards an idle peer onto another data channel;
/// the proxy client keeps the peer's outbound socket, so the source port the
/// local service sees is unaffected. The TTL bounds memory under address
/// churn (e.g. scans), not session lifetime.
#[cfg(feature = "server")]
pub const UDP_ROUTE_TTL_SECS: u64 = 300;

/// The adaptive cap on a control channel's reconnect backoff, in seconds.
///
/// **The cap follows the failure count, not the clock.** A server that is
/// restarting (or a laptop that changed networks) is reachable again in
/// seconds, and hammering it with the same one-second retry a client uses for
/// a five-hour outage is what turns one restart into a reconnect storm. The
/// sequence doubles with each consecutive failure and stops at a minute:
/// 1, 2, 4, 8, 16, 32, 60, 60, ... A session that stayed up for more than a
/// few seconds starts over (see `ClientSession::drive`), so the sequence
/// describes one outage rather than a process's lifetime.
///
/// An explicit `retry_backoff_max` replaces it for every step. The function is
/// pure and logged where it is used, so a deployment can say which cap the
/// client chose without guessing from the timing.
#[cfg(feature = "client")]
#[must_use]
pub fn backoff_cap(failures: u32) -> u64 {
    const STEPS: [u64; 6] = [1, 2, 4, 8, 16, 32];
    STEPS.get(failures as usize).copied().unwrap_or(60)
}

#[cfg(feature = "client")]
pub fn run_control_chan_backoff(cap_secs: u64) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_factor(3.0)
        .with_max_delay(Duration::from_secs(cap_secs))
        .with_jitter()
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::backoff_cap;

    #[test]
    fn the_backoff_cap_doubles_then_stops_at_a_minute() {
        assert_eq!(backoff_cap(0), 1);
        assert_eq!(backoff_cap(1), 2);
        assert_eq!(backoff_cap(2), 4);
        assert_eq!(backoff_cap(3), 8);
        assert_eq!(backoff_cap(4), 16);
        assert_eq!(backoff_cap(5), 32);
        assert_eq!(backoff_cap(6), 60);
        assert_eq!(backoff_cap(u32::MAX), 60, "the cap is bounded");
    }
}

/// The adaptive control-channel heartbeat cadence, in seconds.
///
/// The heartbeat is a liveness check, and how often it must run follows how
/// many sessions one server carries: a handful of clients need a short beat to
/// notice a dead one quickly, while a busy server would spend more time
/// writing heartbeats than serving. The tiers are 30 s up to eight sessions,
/// 60 s up to 32, and 120 s above that; a configured
/// `[server.control].heartbeat_interval` replaces this entirely, and 0 turns
/// the heartbeat off. The function is pure and logged where it is used.
#[must_use]
pub fn heartbeat_interval_for(sessions: usize) -> u64 {
    if sessions <= 8 {
        30
    } else if sessions <= 32 {
        60
    } else {
        120
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::heartbeat_interval_for;

    #[test]
    fn the_heartbeat_cadence_follows_the_session_count() {
        assert_eq!(heartbeat_interval_for(0), 30);
        assert_eq!(heartbeat_interval_for(8), 30);
        assert_eq!(heartbeat_interval_for(9), 60);
        assert_eq!(heartbeat_interval_for(32), 60);
        assert_eq!(heartbeat_interval_for(33), 120);
        assert_eq!(heartbeat_interval_for(usize::MAX), 120);
    }
}

/// The adaptive global tunnel cap, derived from the process's open-file limit.
///
/// A tunnel and a lane are each one file descriptor, so the soft `RLIMIT_NOFILE`
/// is the one hard ceiling the operator already set for the process. Half of it
/// is left for listeners, accepted visitors, backends and the control channels;
/// 64 more are held back for everything that is not a tunnel. A limit that
/// cannot be read answers 0 (unlimited), which is the same value the operator
/// writes to disable the valve: refusing every tunnel because `/proc` is absent
/// would be worse than not capping at all.
#[must_use]
#[cfg(feature = "multiplex")]
pub fn max_tunnels_for(soft_nofile: u64) -> usize {
    if soft_nofile == 0 {
        return 0;
    }
    usize::try_from((soft_nofile / 2).saturating_sub(64)).unwrap_or(usize::MAX)
}

/// The process's soft open-file limit, or 0 when it cannot be read.
///
/// Linux exposes it in `/proc/self/limits`. Other platforms answer 0, which
/// makes the derived cap unlimited: the valve is an optimisation, never a
/// correctness requirement.
#[must_use]
#[cfg(feature = "multiplex")]
pub fn soft_nofile() -> u64 {
    let Ok(limits) = std::fs::read_to_string("/proc/self/limits") else {
        return 0;
    };
    for line in limits.lines() {
        if let Some(rest) = line.strip_prefix("Max open files") {
            let mut fields = rest.split_whitespace();
            if let Some(soft) = fields.next()
                && let Ok(value) = soft.parse::<u64>()
            {
                return value;
            }
        }
    }
    0
}

#[cfg(test)]
#[cfg(feature = "multiplex")]
mod max_tunnels_tests {
    use super::max_tunnels_for;

    #[test]
    fn the_global_cap_leaves_room_for_everything_else() {
        assert_eq!(max_tunnels_for(0), 0, "unreadable = unlimited");
        assert_eq!(max_tunnels_for(128), 0, "half of 128 minus 64");
        assert_eq!(max_tunnels_for(1024), 448);
        assert!(max_tunnels_for(u64::MAX) > 1_000_000);
    }
}
