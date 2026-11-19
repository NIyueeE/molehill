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

use crate::transport::carrier::Carrier;

/// How often a live pool reconciles its size against its configured count.
pub(crate) const MAINTAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a pool waits after a repair dial failed before it tries again.
///
/// D14's other half: a refusal (the server's `max_tunnels` valve, or
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
/// dial timeout (`MHD_ESTABLISH_TIMEOUT`, 10 s).
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
/// count (the server's `max_tunnels` valve, an unreachable
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

/// What a service's traffic looks like, as the pool reads it from what it carried.
///
/// The choice between the two carriers is a property of the *workload* as much as
/// of the path, and the operator cannot be asked for it: one deployment carries a
/// chat connection and a backup over one client. So the pool reads the answer from
/// what each service actually did, and the two classes it tells apart are the two
/// the carriers differ on.
///
/// The difference is measured, not assumed (`HANDOFF.md` §4.500; ten-second
/// samples, the condition on the leg the carrier crosses): a conversational flow's
/// tail is **11× tighter** on the datagram carrier at 1 % loss, because one lost
/// segment costs the kernel carrier its 200 ms minimum retransmission timeout,
/// while a bulk flow's rate is **2.8× better** on the kernel carrier at the same
/// loss and 1.2× better at 5 %.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ServiceClass {
    /// Many small exchanges: the tail is the experience. This is also the prior,
    /// because the datagram carrier is the product's default and a service nobody
    /// has watched yet must not be moved on no evidence.
    #[default]
    Conversational,
    /// A few large transfers: the rate is the experience.
    Bulk,
}

impl ServiceClass {
    /// The class as the telemetry line writes it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Conversational => "conversational",
            Self::Bulk => "bulk",
        }
    }
}

/// One thing a channel proved about the service it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observation {
    /// The channel carried at least [`BULK_BYTES`].
    Bulk,
    /// The channel ended having carried less than [`SMALL_BYTES`].
    Small,
}

/// The bytes in one channel that make it a bulk transfer rather than an exchange.
///
/// A megabyte is far above any request/response an interactive protocol produces
/// (the bench's is 64 bytes; a chat message is kilobytes) and far below what a
/// file transfer or a backup moves (the bench's bulk cell carries tens of
/// megabytes). Nothing between the two is decided: a channel that ends between
/// [`SMALL_BYTES`] and this leaves no observation at all, which is the
/// conservative answer for a class that only ever moves *new* channels.
pub(crate) const BULK_BYTES: u64 = 1024 * 1024;

/// The bytes below which a channel counts as a small exchange.
pub(crate) const SMALL_BYTES: u64 = 64 * 1024;

/// How many observations one class reading is made of.
pub(crate) const CLASS_WINDOW: usize = 8;

/// How many of a window's observations must be bulk for the reading to say so.
pub(crate) const CLASS_BULK_MARKS: usize = 4;

/// How many consecutive windows must agree before the class changes.
///
/// Two, so a single burst cannot move a service: an occasional large transfer
/// through an otherwise conversational service needs two windows of evidence, and
/// the actuator it would drive is expensive (a tunnel handover, a fresh handshake
/// and a drain).
pub(crate) const CLASS_CONFIRMATIONS: u8 = 2;

/// What one service has carried, and the class the pool reads from it.
///
/// Pure: the pool hands it observations and asks it for a class, so the rule that
/// decides a carrier is testable without a socket, a tunnel or a clock.
#[derive(Debug)]
pub(crate) struct TrafficProfile {
    class: ServiceClass,
    /// Bulk observations in the window being filled.
    bulk: usize,
    /// Observations in the window being filled.
    seen: usize,
    /// Consecutive completed windows whose reading disagreed with `class`.
    disagree: u8,
    /// How much traffic of each kind this service has ever shown, in observation
    /// units: a bulk observation is one megabyte ([`BULK_BYTES`]), a small one is
    /// the top of its range ([`SMALL_BYTES`]).
    ///
    /// This is what the *split* between the carriers is made of, and it is why a
    /// session whose bulk service dominates gets most of the tunnels while a chat
    /// that happens to sit beside it still keeps the one it needs.
    bulk_units: u64,
    small_units: u64,
    /// Observations this service has ever produced, so a caller can tell "no
    /// evidence" from "evidence that says conversational".
    total: u64,
}

impl Default for TrafficProfile {
    fn default() -> Self {
        Self {
            class: ServiceClass::Conversational,
            bulk: 0,
            seen: 0,
            disagree: 0,
            bulk_units: 0,
            small_units: 0,
            total: 0,
        }
    }
}

impl TrafficProfile {
    /// The class this service's traffic has earned.
    pub(crate) const fn class(&self) -> ServiceClass {
        self.class
    }

    /// How much traffic of each kind this service has shown, in observation
    /// units: `(bulk, small)`.
    pub(crate) const fn weights(&self) -> (u64, u64) {
        (self.bulk_units, self.small_units)
    }

    /// Add one observation, and re-read the class when a window completes.
    pub(crate) fn observe(&mut self, observation: Observation) {
        self.total = self.total.saturating_add(1);
        self.seen += 1;
        match observation {
            Observation::Bulk => {
                self.bulk += 1;
                self.bulk_units = self.bulk_units.saturating_add(1);
            }
            Observation::Small => {
                self.small_units = self.small_units.saturating_add(1);
            }
        }
        if self.seen < CLASS_WINDOW {
            return;
        }
        let reading = if self.bulk >= CLASS_BULK_MARKS {
            ServiceClass::Bulk
        } else {
            ServiceClass::Conversational
        };
        self.seen = 0;
        self.bulk = 0;
        if reading == self.class {
            self.disagree = 0;
            return;
        }
        self.disagree += 1;
        if self.disagree >= CLASS_CONFIRMATIONS {
            self.class = reading;
            self.disagree = 0;
        }
    }
}

/// The carrier a class asks for, or `None` when the pool should not care.
///
/// A conversational flow stays on whatever the pool already rides: the datagram
/// carrier is the product's default, and the kernel one is only 44 µs better on a
/// *clean* path, which is inside the noise of every cell that measures it. A bulk
/// flow asks for the kernel carrier, which is the one that carries more bytes.
pub(crate) fn carrier_for(class: ServiceClass) -> Option<Carrier> {
    match class {
        ServiceClass::Conversational => None,
        ServiceClass::Bulk => Some(Carrier::Tcp),
    }
}

/// The candidate order for one placement, given the carrier its service's traffic
/// asked for and the two things it must skip.
///
/// Four tiers, in this order, and each one is a *floor* under the one before it —
/// the open only ever falls through to the next when the previous is empty, so a
/// placement is never refused for its class:
///
/// 1. the wanted carrier, not already tried, not already in the group;
/// 2. the wanted carrier, not already tried — a bulk group **shares** a tunnel of
///    its carrier instead of spilling onto the other one;
/// 3. any carrier, not tried, not in the group (the rule as it was before the
///    class existed);
/// 4. any carrier, not tried.
///
/// Tier 2 is why this replaced a plain reorder: with one tunnel on the wanted
/// carrier and eight stripes to place, a reorder put the first stripe there and
/// every later one back on the other carrier, because the group's spread
/// preference outranked the class — measured as a policy that moved no bulk
/// traffic at all (`HANDOFF.md` §4.505).
pub(crate) fn order_placement(
    order: &[usize],
    carriers: &[Carrier],
    tried: &[usize],
    avoid: &[usize],
    wanted: Option<Carrier>,
) -> Vec<usize> {
    let free = |index: &usize| !tried.contains(index);
    let unshared = |index: &usize| free(index) && !avoid.contains(index);
    if let Some(wanted) = wanted {
        let on_carrier = |index: &usize| carriers.get(*index) == Some(&wanted);
        let best: Vec<usize> = order
            .iter()
            .copied()
            .filter(|index| on_carrier(index) && unshared(index))
            .collect();
        if !best.is_empty() {
            return best;
        }
        let shared: Vec<usize> = order
            .iter()
            .copied()
            .filter(|index| on_carrier(index) && free(index))
            .collect();
        if !shared.is_empty() {
            return shared;
        }
    }
    let candidates: Vec<usize> = order
        .iter()
        .copied()
        .filter(|index| unshared(index))
        .collect();
    if candidates.is_empty() && !avoid.is_empty() {
        return order.iter().copied().filter(|index| free(index)).collect();
    }
    candidates
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
        let mhd = [Carrier::Mhd; 3];
        let pick =
            |tried: &[usize], avoid: &[usize]| order_placement(&order, &mhd, tried, avoid, None);

        // Nothing excluded: the ordinary order, unchanged.
        assert_eq!(pick(&[], &[]), order);
        // The group's own tunnels are skipped, in the best-first order.
        assert_eq!(pick(&[], &[2]), vec![0, 1]);
        assert_eq!(pick(&[], &[2, 0]), vec![1]);
        // Every tunnel is the group's: the exclusion gives way, so the group
        // still forwards (the fallback the brief calls correctness first).
        assert_eq!(pick(&[], &[0, 1, 2]), order);
        // A tried tunnel stays excluded even when the exclusion is dropped:
        // that one is a refusal, not a preference.
        assert_eq!(pick(&[1], &[0, 1, 2]), vec![2, 0]);
        // Tried and avoided together leave the third candidate.
        assert_eq!(pick(&[2], &[0]), vec![1]);
        // Nothing eligible at all (everything tried): empty, so the caller
        // reports the refusal it has rather than reserving a dead tunnel.
        assert_eq!(pick(&[0, 1, 2], &[]), Vec::<usize>::new());
        assert_eq!(pick(&[0, 1, 2], &[0]), Vec::<usize>::new());
    }

    /// A service nobody has watched is conversational, and stays that way on a
    /// window of small exchanges. The prior is the product's default carrier, so a
    /// wrong guess here costs nothing an unmodified tree does not already pay.
    #[test]
    fn a_conversational_service_stays_conversational() {
        let mut profile = TrafficProfile::default();
        assert_eq!(profile.class(), ServiceClass::Conversational);
        for _ in 0..(CLASS_WINDOW * 4) {
            profile.observe(Observation::Small);
        }
        assert_eq!(profile.class(), ServiceClass::Conversational);
    }

    /// Bulk evidence has to clear a whole window *and* be confirmed by a second
    /// one, so one large transfer through a chat service cannot move it.
    #[test]
    fn a_service_moves_to_bulk_only_after_two_agreeing_windows() {
        let mut profile = TrafficProfile::default();
        for _ in 0..CLASS_WINDOW {
            profile.observe(Observation::Bulk);
        }
        // One window is a reading, not a decision.
        assert_eq!(profile.class(), ServiceClass::Conversational);
        for _ in 0..CLASS_WINDOW {
            profile.observe(Observation::Bulk);
        }
        assert_eq!(profile.class(), ServiceClass::Bulk);
    }

    /// A window that does not read the same way cancels the pending change: the
    /// confirmations are consecutive, not cumulative.
    #[test]
    fn a_disagreeing_window_cancels_a_pending_change() {
        let mut profile = TrafficProfile::default();
        for _ in 0..CLASS_WINDOW {
            profile.observe(Observation::Bulk);
        }
        for _ in 0..CLASS_WINDOW {
            profile.observe(Observation::Small);
        }
        assert_eq!(profile.class(), ServiceClass::Conversational);
        for _ in 0..CLASS_WINDOW {
            profile.observe(Observation::Bulk);
        }
        // The earlier bulk window was cancelled, so this is the first of two again.
        assert_eq!(profile.class(), ServiceClass::Conversational);
    }

    /// A mixed window is decided by the marks, and four of eight is the line.
    #[test]
    fn four_bulk_marks_in_a_window_are_enough() {
        let mut profile = TrafficProfile::default();
        for i in 0..(CLASS_WINDOW * 2) {
            profile.observe(if i % 2 == 0 {
                Observation::Bulk
            } else {
                Observation::Small
            });
        }
        assert_eq!(profile.class(), ServiceClass::Bulk);
    }

    /// The class names the carrier, and a conversational service asks for nothing —
    /// which is what makes the rule inert on a pool riding one carrier.
    #[test]
    fn the_class_names_a_carrier() {
        assert_eq!(carrier_for(ServiceClass::Conversational), None);
        assert_eq!(carrier_for(ServiceClass::Bulk), Some(Carrier::Tcp));
    }

    /// The class outranks the group's spread, and every tier is a floor: an open
    /// is never refused for its class, and a pool with no tunnel of the wanted
    /// carrier places exactly as it did before the rule existed.
    #[test]
    fn a_carrier_preference_is_a_filter_with_floors() {
        // Tunnels 0 and 2 ride the datagram carrier, tunnel 1 the kernel one.
        let order = vec![2, 0, 1];
        let carriers = [Carrier::Mhd, Carrier::Tcp, Carrier::Mhd];
        // No preference: the rule as it was.
        assert_eq!(
            order_placement(&order, &carriers, &[], &[], None),
            vec![2, 0, 1]
        );
        // A wanted carrier takes its own tunnels, in the order they had.
        assert_eq!(
            order_placement(&order, &carriers, &[], &[], Some(Carrier::Tcp)),
            vec![1]
        );
        assert_eq!(
            order_placement(&order, &carriers, &[], &[], Some(Carrier::Mhd)),
            vec![2, 0]
        );
        // **Tier 2**: the group already holds the only tunnel of the wanted
        // carrier, so the next stripe *shares* it rather than spilling onto the
        // other carrier — the defect that made the policy move no bulk traffic.
        assert_eq!(
            order_placement(&order, &carriers, &[], &[1], Some(Carrier::Tcp)),
            vec![1]
        );
        // A tried tunnel is never revisited, even to satisfy the class: that one
        // refused the stream, and the fall-through asks again with it added.
        assert_eq!(
            order_placement(&order, &carriers, &[1], &[], Some(Carrier::Tcp)),
            vec![2, 0]
        );
        assert_eq!(
            order_placement(&order, &carriers, &[1, 2], &[], Some(Carrier::Tcp)),
            vec![0]
        );
        // **Tier 3**: a pool with no tunnel of the wanted carrier is unchanged.
        let uniform = [Carrier::Mhd, Carrier::Mhd, Carrier::Mhd];
        assert_eq!(
            order_placement(&order, &uniform, &[], &[], Some(Carrier::Tcp)),
            order
        );
        assert_eq!(
            order_placement(&order, &uniform, &[], &[2], Some(Carrier::Tcp)),
            vec![0, 1]
        );
        // Everything tried: empty, so the caller reports the refusal it has.
        assert_eq!(
            order_placement(&order, &carriers, &[0, 1, 2], &[], None),
            Vec::<usize>::new()
        );
    }
}
