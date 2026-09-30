//! The elastic tunnel pool's *policy*: the internal constants, the placement
//! arithmetic and the shrink predicate.
//!
//! The pool's runtime lives in [`crate::transport::multiplex`] (it owns the
//! tunnels, the driver tasks and the telemetry); everything in this module is
//! pure so the rules can be unit-tested without a socket. The constants stay
//! internal on purpose: they are a starting point to be tuned by the S1
//! observation, and a knob nobody has measured is a knob nobody can defend
//! (see HANDOFF.md D15 — the benchmark matrices are what promote one to a
//! configuration key).

/// Grow when the pool's total usage reaches this fraction of its total stream
/// capacity (equivalently, when every tunnel is that loaded once placement has
/// spread the traffic).
///
/// The cap is `DEFAULT_MUX_MAX_STREAMS` (64) and the vendored engine logs an
/// unguarded `error!` when a stream cap is hit, so the pool must grow while it
/// still has headroom: at 80 % of 64 the next 12 opens have room, which is
/// more than a burst can consume inside one maintenance tick.
pub(crate) const GROW_PERCENT: usize = 80;

/// How long the pool stays warm after a growth before a shrink may remove a
/// tunnel. Without it, a pool grown for one burst shrinks immediately after
/// it — and the next burst pays the grow again.
pub(crate) const MIN_WARM_HOLD: std::time::Duration = std::time::Duration::from_secs(10);

/// How long after a shrink the pool waits before considering another one.
/// Shrink is deliberately unhurried: a mistake costs the next visitor a
/// tunnel establishment.
pub(crate) const SHRINK_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

/// How often a live pool re-evaluates its size.
pub(crate) const MAINTAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a pool waits after a growth attempt failed before it tries again.
///
/// D14's other half: a refusal (the server's `max_tunnels_per_client` valve, or
/// a dial that could not be established) must *stop* growth, not merely fail it
/// — the maintenance tick runs every 50 ms, so without this the client would
/// dial-and-be-refused twenty times a second against the server's accept path.
/// The wait is cut short by the two events that make a retry meaningful: a
/// tunnel dying, and the pool having shrunk.
pub(crate) const GROW_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long an open waits for a growth that is already in flight.
///
/// A burst of opens on a cold pool — a stripe group's K channels arrive
/// back-to-back — must not have all but one of them race past the in-flight
/// growth and fail on an empty pool: they wait for it instead. This budget is a
/// backstop, not the expected path: a growth either finishes or fails, and a
/// caller that waited it out then reports the pool's real state instead of
/// hanging on a `resizing` flag some panicked task left set.
pub(crate) const GROW_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// An open that waited longer than this means the pool has no spare stream
/// ready: grow. The condition is the *real* open latency (which includes one
/// round trip to the server), and it is only ever consulted for opens that
/// have not completed yet.
pub(crate) const OPEN_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a *cold* pool's open waits for the growth already in flight before
/// giving up on it.
///
/// Only one open wins the pool's resize flag; the others must wait for the
/// tunnel that grow is dialing, or they would reserve against an empty pool
/// and fail a visitor the winner is already answering — observed as a striped
/// group that never completes (four channel opens, three `NoTunnel` failures).
/// The grow's own completion signal is the normal path; this bounds the wait
/// for a dialer that never returns, so it has to cover the slowest carrier's
/// dial timeout (`KCP_ESTABLISH_TIMEOUT`, 10 s).
pub(crate) const COLD_GROW_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The in-flight open budget of one tunnel.
///
/// The measured budget is the driver's request queue: a 16-slot channel plus
/// the single `waiting` slot the driver serves one at a time (see
/// `ClientTunnel::start`). The candidate list handed to the placement code is
/// trimmed to this many least-loaded tunnels, so no page of a multi-open
/// request can put more than the budget into reservation state — the overshoot
/// then only lives in the driver's own queue, which is bounded already.
pub(crate) const OPEN_BUDGET: usize = 16;

/// Why the pool changed size. Rendered verbatim in the `pool-stats` timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrowReason {
    /// The pool had no tunnel and a service asked for one.
    Cold,
    /// Every tunnel is at or above `GROW_AT_STREAM_FRACTION` of its cap.
    Load,
    /// An open waited longer than `OPEN_WAIT_BUDGET`.
    Wait,
    /// The UDP-derived floor (D7) is above the current size.
    UdpFloor,
    /// An open was left queueing (a refused open, or a tunnel whose open budget
    /// was full): the demand flag is set and the next maintenance tick adds a
    /// tunnel. A stream-cap hit arrives here too — it is a growth signal, never
    /// a tunnel's death.
    Demand,
}

impl GrowReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Load => "load",
            Self::Wait => "wait",
            Self::UdpFloor => "udp_floor",
            Self::Demand => "demand",
        }
    }
}

/// Why the pool removed a tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShrinkReason {
    /// No streams, no pending opens, no pinned peers, idle past the timeout.
    Idle,
}

impl ShrinkReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
        }
    }
}

/// The per-tunnel bookkeeping the placement and shrink rules read.
///
/// `streams` counts *established* streams, `pending` counts opens that were
/// reserved but have not completed, and `pinned` counts the peers whose
/// session currently ends on this tunnel (UDP affinity, D30).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TunnelLoad {
    pub(crate) streams: usize,
    pub(crate) pending: usize,
    pub(crate) pinned: usize,
}

impl TunnelLoad {
    /// The load placement compares: an established stream and a reserved open
    /// cost the same, because both consume one of the tunnel's stream slots.
    pub(crate) const fn total(self) -> usize {
        self.streams + self.pending
    }
}

/// The tunnel placement picked, plus the spread data S1 records for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    /// The tunnel's current index in the pool (`0` is the oldest survivor).
    pub(crate) chosen: usize,
    /// Eligible tunnels considered (every live one).
    pub(crate) candidates: usize,
    /// The values of the *best* candidate — the one placement picked, before
    /// the refusal fallback.
    pub(crate) best: TunnelLoad,
    /// The values of the *worst* candidate at the same instant. `worst - best`
    /// is what a placement rule can win: if every candidate is equally loaded,
    /// choosing between them is worth nothing, and that is the measurement S1
    /// exists to make before a smarter rule is written (M2b/D28).
    pub(crate) worst: TunnelLoad,
    /// The chosen tunnel's values at selection time.
    pub(crate) chosen_load: TunnelLoad,
    /// Whether the reserved tunnel refused and the open fell through.
    pub(crate) fallback: bool,
}

/// Order the candidate indices for one open: least-loaded first (by
/// `streams + pending`), the round-robin cursor (`start`) only breaking ties.
///
/// A weighted score was deliberately not used: placement is a *scheduling*
/// decision inside one client, and the spread data S1 collects is what would
/// justify a smarter rule later (HANDOFF M2b/D28). Least-loaded is the rule
/// whose failure mode is legible in the telemetry.
pub(crate) fn order_candidates(loads: &[TunnelLoad], start: usize) -> Vec<usize> {
    let n = loads.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|i| {
        let offset = (*i + n - (start % n)) % n;
        (loads[*i].total(), offset)
    });
    order
}

/// The stream-cap headroom the grow rule uses: `size * cap * fraction`.
///
/// The threshold is compared against the pool's *total* usage, which is
/// exactly "every tunnel is at least `GROW_AT_STREAM_FRACTION` of its cap"
/// once placement has spread the load the way it does — and unlike a
/// per-tunnel comparison it stays reachable for a tunnel the pool just added.
pub(crate) fn grow_threshold(size: usize, stream_cap: usize) -> usize {
    // The pool size and the stream cap are small counters; this is the
    // integer form of `size * cap * 80 / 100`, with the fraction's own
    // precision (0.8 is not exact in binary) immaterial at these magnitudes.
    size * stream_cap * GROW_PERCENT / 100
}

/// The UDP-derived floor (D7): the number of tunnels that must stay for the
/// deepest active UDP service to keep its configured workers.
///
/// `pool_size` is the service's channel count (`udp_workers` after the
/// configuration milestone) and `stream_cap` the per-tunnel stream ceiling, so
/// the floor is `ceil(channels / cap)` — the smallest pool that can carry all
/// of the service's channels *at once*.
///
/// The floor is the larger of that capacity term and the service's own worker
/// count (capped at `cap` by the `ceil` term when the workers do not fit): a
/// service configured with N workers is asking for N paths, and the pool it
/// draws from keeps at least N tunnels so those paths are spread rather than
/// stacked. The pool's `max_tunnels` bounds the demand, so a service cannot
/// force a pool beyond its cap.
///
/// An inactive service contributes nothing; the floor is what *active* UDP
/// demand needs, which is why it is maintained across a tunnel's death rather
/// than recomputed from the dead pool.
pub(crate) fn udp_floor(
    active_channels: impl IntoIterator<Item = usize>,
    stream_cap: usize,
) -> usize {
    let cap = stream_cap.max(1);
    active_channels
        .into_iter()
        .map(|channels| channels.max(channels.div_ceil(cap)))
        .max()
        .unwrap_or(0)
}

/// The same floor, with the pool's own cap applied: a service may not demand
/// more tunnels than the pool is allowed to grow to.
pub(crate) fn udp_floor_capped(
    active_channels: impl IntoIterator<Item = usize>,
    stream_cap: usize,
    max_tunnels: usize,
) -> usize {
    udp_floor(active_channels, stream_cap).min(max_tunnels.max(1))
}

/// Whether one tunnel may be removed from a pool of `size`, given the whole
/// pool's state.
///
/// Shrink is conservative by construction (D26, D30): the pool must be above
/// its floor, the tunnel must be completely idle (no established stream, no
/// pending open, no pinned peer), and the pool must have been quiet for the
/// caller's `idle_timeout` — the caller owns the clock, this predicate owns
/// the conditions.
pub(crate) fn may_shrink(
    size: usize,
    floor: usize,
    load: TunnelLoad,
    idle: bool,
    warm_hold_elapsed: bool,
    cooldown_elapsed: bool,
) -> bool {
    size > floor
        && load.streams == 0
        && load.pending == 0
        && load.pinned == 0
        && idle
        && warm_hold_elapsed
        && cooldown_elapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_is_the_deepest_active_udp_service() {
        assert_eq!(udp_floor([], 64), 0, "no active UDP service: no floor");
        // Two workers ask for two paths; one tunnel carries 64 streams, so the
        // workers would fit — but they would share one path, which is not what
        // the service configured them for.
        assert_eq!(udp_floor([2], 64), 2);
        // Past the stream cap the capacity term wins and keeps growing.
        assert_eq!(udp_floor([64], 64), 64);
        assert_eq!(udp_floor([65], 64), 65);
        assert_eq!(udp_floor([2, 130], 64), 130, "the deepest service wins");
        // The pool's own cap bounds the demand.
        assert_eq!(udp_floor_capped([130], 64, 2), 2);
        assert_eq!(udp_floor_capped([2], 64, 3), 2);
        assert_eq!(udp_floor_capped([2], 64, 1), 1);
    }

    #[test]
    fn shrink_needs_every_condition() {
        let idle = TunnelLoad::default();
        assert!(may_shrink(2, 1, idle, true, true, true));
        // A pinned peer keeps the tunnel even when it has no streams (D30).
        let pinned = TunnelLoad {
            pinned: 1,
            ..Default::default()
        };
        assert!(!may_shrink(2, 1, pinned, true, true, true));
        // A live stream, a reserved open, the floor, the idle clock, the warm
        // hold and the cooldown each veto on their own.
        assert!(!may_shrink(
            2,
            1,
            TunnelLoad {
                streams: 1,
                ..Default::default()
            },
            true,
            true,
            true
        ));
        assert!(!may_shrink(
            2,
            1,
            TunnelLoad {
                pending: 1,
                ..Default::default()
            },
            true,
            true,
            true
        ));
        assert!(
            !may_shrink(1, 1, idle, true, true, true),
            "never below the floor"
        );
        assert!(!may_shrink(2, 1, idle, false, true, true));
        assert!(!may_shrink(2, 1, idle, true, false, true));
        assert!(!may_shrink(2, 1, idle, true, true, false));
    }

    #[test]
    fn placement_prefers_the_least_loaded_and_breaks_ties_round_robin() {
        let loads = [
            TunnelLoad {
                streams: 2,
                ..Default::default()
            },
            TunnelLoad {
                streams: 0,
                ..Default::default()
            },
            TunnelLoad {
                streams: 0,
                pending: 1,
                ..Default::default()
            },
        ];
        // Tunnel 1 is empty; tunnel 2 has one reserved open; tunnel 0 carries
        // two streams. The cursor cannot promote a loaded tunnel.
        assert_eq!(order_candidates(&loads, 0), vec![1, 2, 0]);
        assert_eq!(order_candidates(&loads, 2), vec![1, 2, 0]);
        let tied = [TunnelLoad::default(), TunnelLoad::default()];
        assert_eq!(order_candidates(&tied, 0), vec![0, 1]);
        assert_eq!(order_candidates(&tied, 1), vec![1, 0]);
    }

    #[test]
    fn grow_threshold_tracks_size() {
        assert_eq!(grow_threshold(1, 64), 51);
        assert_eq!(grow_threshold(4, 64), 204);
        assert_eq!(grow_threshold(0, 64), 0);
    }
}
