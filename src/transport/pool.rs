//! The pinned tunnel pool's *policy*: the internal constants, the placement
//! arithmetic and the placement ceiling.
//!
//! The pool's runtime lives in [`crate::transport::multiplex`] (it owns the
//! tunnels, the driver tasks and the telemetry); everything in this module is
//! pure so the rules can be unit-tested without a socket. The constants stay
//! internal on purpose: they are a starting point to be tuned by the S1
//! observation, and a knob nobody has measured is a knob nobody can defend
//! (see HANDOFF.md D15 — the benchmark matrices are what promote one to a
//! configuration key).
//!
//! A pinned pool establishes its configured `count` tunnels at service start
//! and keeps them; the only runtime establishment is *repair* (a dead tunnel
//! is re-dialed), so this module no longer carries growth or shrink rules.

/// How often a live pool reconciles its size against its configured count.
pub(crate) const MAINTAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a pool waits after a repair dial failed before it tries again.
///
/// D14's other half: a refusal (the server's `max_tunnels_per_client` valve, or
/// a dial that could not be established) must *stop* the repair dials, not
/// merely fail one — the maintenance tick runs every 50 ms, so without this the
/// client would dial-and-be-refused twenty times a second against the server's
/// accept path. The wait is cut short by the event that makes a retry
/// meaningful: a tunnel dying.
pub(crate) const GROW_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long an open waits for a repair that is already in flight.
///
/// A burst of opens on an empty pool — a stripe group's K channels arrive
/// back-to-back — must not have all but one of them race past the in-flight
/// repair and fail on the still-empty pool: they wait for it instead. This
/// budget is a backstop, not the expected path: a repair either finishes or
/// fails, and a caller that waited it out then reports the pool's real state
/// instead of hanging on a `resizing` flag some panicked task left set.
pub(crate) const GROW_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How long an empty pool's open waits for the repair already in flight before
/// giving up on it.
///
/// Only one open wins the pool's resize flag; the others must wait for the
/// tunnel the repair is dialing, or they would reserve against an empty pool
/// and fail a visitor the winner is already answering — observed as a striped
/// group that never completes (four channel opens, three `NoTunnel` failures).
/// The repair's own completion signal is the normal path; this bounds the wait
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

/// The concurrent streams the pool lets one tunnel carry.
///
/// This is a *hard* placement ceiling, strictly below the engine's own
/// `DEFAULT_MUX_MAX_STREAMS` (64). The gap is not cosmetic: a 65th inbound
/// stream is refused (the engine used to terminate the *whole connection* for
/// it — see `mux/connection.rs`), and a refused stream is a visitor that fails.
/// Crossing the ceiling needed a pool that could not reach its configured
/// count (the server's `max_tunnels_per_client` valve, an unreachable
/// endpoint, or a `tunnels` count below the load) while the load kept
/// arriving, which is exactly what a bulk run does; the reservation is charged
/// before the first `await`, so the ceiling has to bound `streams + pending`,
/// not the established count alone.
///
/// 56 leaves 8 slots of headroom: room for a stream the peer has not released
/// yet (the engine drops a stream from its map on the *local* handle, so the
/// two sides can disagree by a few during a teardown) and for the engine's own
/// bookkeeping.
///
/// Placement is the *only* oversubscription protection a pinned pool has: it
/// cannot grow for load, so when every tunnel is at the ceiling an open waits
/// [`CAPACITY_WAIT`] for a stream to retire and is then refused with
/// `OpenError::AtCapacity`. The ceiling is a *soft* bound in the sense that
/// reaching the engine's cap is not fatal: `mux/connection.rs` refuses the
/// stream that would cross it instead of terminating the connection (see the
/// comment there — the old behaviour took every stream on the tunnel down with
/// it). Refusing a stream is still worse than placing it elsewhere, which is
/// why the ceiling sits strictly below the engine's own cap.
pub(crate) const TUNNEL_STREAM_CEILING: usize = 56;

/// The streams one tunnel may carry, never above the engine's own cap.
///
/// Two bounds, whichever is stricter: the pool's absolute ceiling
/// ([`TUNNEL_STREAM_CEILING`], the one that caps one tunnel's concurrency for
/// any engine cap) and a proportional headroom for a cap small enough that the
/// absolute one would not leave any. The headroom is an eighth of the cap, at
/// least one stream, so the ceiling is always strictly below the engine's cap.
#[must_use]
pub(crate) fn tunnel_ceiling(stream_cap: usize) -> usize {
    let headroom = (stream_cap / 8).max(1);
    TUNNEL_STREAM_CEILING
        .min(stream_cap.saturating_sub(headroom))
        .max(1)
}

/// How long an open waits for a stream to retire when every tunnel is at the
/// ceiling and the pool is already at its configured count.
///
/// Bounded on purpose: a refused visitor is a failure this pool can report,
/// while an open that waits forever is a hung connection nobody can attribute.
/// The wait exists so a burst that meets a full pool is served by the streams
/// retiring under it, not refused on the instant it arrives.
pub(crate) const CAPACITY_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Why the pool changed size. Rendered verbatim in the `pool-stats` timeline.
///
/// A pinned pool establishes its configured count at service start; the one
/// size change left to record is a *repair* after a tunnel died. There is no
/// load growth and no idle shrink — capacity is a function of configuration,
/// and a deployment's throughput must not depend on what it happened to be
/// doing a minute ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrowReason {
    /// A tunnel was lost (or never came up) and the count is short.
    Repair,
}

impl GrowReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Repair => "repair",
        }
    }
}

/// Why the pool removed a tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShrinkReason {
    /// The tunnel's driver ended: the connection is gone, whatever its load
    /// says. Removal here is not a policy decision, and the repair tick
    /// replaces it — a pinned pool keeps its count.
    Dead,
}

impl ShrinkReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Dead => "dead",
        }
    }
}

/// The per-tunnel bookkeeping the placement rule reads.
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

/// The candidate order for one placement, given what it must skip.
///
/// Two kinds of skip, deliberately different:
///
/// - `tried` — tunnels this open already tried and that refused the stream.
///   They are never revisited, or the fall-through loop would retry a tunnel it
///   knows is dead or full;
/// - `avoid` — the tunnels a stripe group already occupies. That one is a
///   *preference*, not a constraint: when every candidate is in it (a pool
///   smaller than the group's stripe count — a pinned pool cannot dial more for
///   it), the unfiltered order answers instead. A group that cannot spread must
///   still forward — correctness first, exactly as before the group had a name
///   (D24).
///
/// Pure on purpose: the exclusion rule is the part of stripe placement that can
/// be checked without a socket, and the pool's own state lock is the only other
/// input the caller has to supply.
pub(crate) fn order_candidates_for(
    order: &[usize],
    tried: &[usize],
    avoid: &[usize],
) -> Vec<usize> {
    let eligible = |i: &usize| !tried.contains(i) && !avoid.contains(i);
    let candidates: Vec<usize> = order.iter().copied().filter(eligible).collect();
    if candidates.is_empty() && !avoid.is_empty() {
        return order
            .iter()
            .copied()
            .filter(|i| !tried.contains(i))
            .collect();
    }
    candidates
}

/// The UDP-derived floor (D7): how many tunnels the deepest UDP service of a
/// carrier needs so its configured workers keep distinct paths.
///
/// `active_channels` is each service's channel count (`udp_workers`) and
/// `stream_cap` the per-tunnel stream ceiling, so the floor is
/// `ceil(channels / cap)` — the smallest pool that can carry all of a service's
/// channels *at once*.
///
/// The floor is the larger of that capacity term and the service's own worker
/// count (capped at `cap` by the `ceil` term when the workers do not fit): a
/// service configured with N workers is asking for N paths, and the pool it
/// draws from establishes at least N tunnels so those paths are spread rather
/// than stacked. An explicit `tunnels` below this floor is refused by the
/// configuration layer (the pinned pool cannot grow to meet it); an unset one
/// resolves to `max(default, floor)`.
///
/// An inactive service contributes nothing; the floor is what *active* UDP
/// demand needs.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The placement ceiling is the pool's promise that the engine's own cap is
    /// unreachable: it stays strictly below the cap for every cap, and the
    /// headroom never disappears. Placement is the only oversubscription
    /// protection a pinned pool has.
    #[test]
    fn the_placement_ceiling_keeps_headroom_below_the_engine_cap() {
        for cap in [4usize, 16, 32, 64, 128, 256] {
            let ceiling = tunnel_ceiling(cap);
            assert!(
                ceiling < cap,
                "the ceiling ({ceiling}) must stay below the engine's cap ({cap})"
            );
        }
        // The shipped cap: placement refuses at 56, the engine refuses a stream
        // at 64 (and, since the connection no longer terminates, that refusal
        // costs one visitor).
        assert_eq!(tunnel_ceiling(64), 56);
        // A cap too small for the absolute ceiling still yields a usable one:
        // the proportional headroom binds instead.
        assert_eq!(tunnel_ceiling(16), 14);
        assert_eq!(tunnel_ceiling(4), 3);
    }

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

    /// The stripe exclusion is a preference with a floor: a group's next stripe
    /// skips the tunnels its siblings took, and when there is nothing left to
    /// skip to, the ordinary least-loaded order comes back rather than a
    /// refusal.
    #[test]
    fn stripe_candidates_avoid_the_groups_tunnels_until_none_are_left() {
        let order = vec![2, 0, 1];

        // Nothing excluded: the ordinary order, unchanged.
        assert_eq!(order_candidates_for(&order, &[], &[]), order);
        // The group's own tunnels are skipped, in the best-first order.
        assert_eq!(order_candidates_for(&order, &[], &[2]), vec![0, 1]);
        assert_eq!(order_candidates_for(&order, &[], &[2, 0]), vec![1]);
        // Every tunnel is the group's: the exclusion gives way, so the group
        // still forwards (the fallback the brief calls correctness first).
        assert_eq!(order_candidates_for(&order, &[], &[0, 1, 2]), order);
        // A tried tunnel stays excluded even when the exclusion is dropped:
        // that one is a refusal, not a preference.
        assert_eq!(order_candidates_for(&order, &[1], &[0, 1, 2]), vec![2, 0]);
        // Tried and avoided together leave the third candidate.
        assert_eq!(order_candidates_for(&order, &[2], &[0]), vec![1]);
        // Nothing eligible at all (everything tried): empty, so the caller
        // reports the refusal it has rather than reserving a dead tunnel.
        assert_eq!(
            order_candidates_for(&order, &[0, 1, 2], &[]),
            Vec::<usize>::new()
        );
        assert_eq!(
            order_candidates_for(&order, &[0, 1, 2], &[0]),
            Vec::<usize>::new()
        );
    }
}
