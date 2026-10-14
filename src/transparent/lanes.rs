//! Which claim gets how many lanes, decided from what the lanes are carrying.
//!
//! The configuration gives every claim of a carrier an *equal share* of the
//! operator's lane budget (`[transparent.data.<carrier>].tunnels`). Equal is the
//! right starting point — it is a function of the configuration, so the capacity
//! a deployment offers does not depend on what it happened to be doing a minute
//! ago — but it is the wrong steady state: a claim serving one busy flow and a
//! claim serving nothing have no use for the same number of connections.
//!
//! This module is the policy that moves one to the other, and it is **pure**:
//! it reads a snapshot of what the lanes carry and answers with at most one
//! move. Everything that touches the world — asking a lane to end, dialing a new
//! one — is the runtime's (`core::client`), which is what makes the decision
//! testable without a device, a peer or a clock.
//!
//! The signal is the placement table's own: a lane's **flow count** is how many
//! flows the claim has placed in it (`MemberSlot::flows`), and the sum over a
//! claim's lanes is how many flows it is carrying. A claim wants **one lane per
//! talking flow**: lanes are what a claim's flows are spread over, so that is
//! the number at which no flow is queueing behind another — and it is also the
//! ceiling on what more lanes can do for a claim, because a single flow can
//! never use two of them. Its packets must stay in order, which is why the
//! placement table pins a flow to one lane for as long as it talks; two flows
//! on one lane are two flows taking turns on one carrier, and that is what
//! lending a lane fixes.
//!
//! Nothing here moves a flow: a flow stays in the slot it was placed in, and a
//! lane is only ever asked to end when it holds **no** flow at all (the idle
//! condition the hub re-checks under its own lock), so lending cannot reorder
//! anything. That is the property the placement table was built for.

use std::time::Duration;

use crate::protocol::LaneId;
use crate::transparent::Endpoint;
// The carrier is what the quality rule compares, and only a build with a
// choice has one; without it the rule's types still exist (the allocator reads
// them) but no lane can name a carrier.
#[cfg(feature = "multiplex")]
use crate::transport::multiplex::Carrier;

/// How often the runtime looks at the claims.
///
/// One second is the scale a human notices and the scale a flow's arrival is
/// worth reacting on; faster would spend decisions on a burst that is already
/// over, slower would leave a busy claim short for as long as it takes to
/// notice.
pub const LANE_TICK: Duration = Duration::from_secs(1);

/// One lane, as the allocator sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneLoad {
    /// The lane's slot in its claim's set — the index the hub takes.
    pub slot: usize,
    /// The flows the claim has placed in it.
    pub flows: usize,
    /// Whether this lane is being handed over to another carrier right now.
    ///
    /// A lane under a drain is already on its way out: the peer has been asked
    /// to stop placing flows in it, and the dialer retires it as soon as both
    /// ends are empty. Lending it *again* would spend the one move the handover
    /// is making — which is why a handover in flight outranks a lend (repair,
    /// then handover, then lending).
    pub handover: bool,
}

/// One claim, as the allocator sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimLoad {
    /// The claimed endpoint: the claim's identity in the hub.
    pub endpoint: Endpoint,
    /// The lanes it holds right now, in slot order.
    pub lanes: Vec<LaneLoad>,
}

impl ClaimLoad {
    /// The quietest lane this claim could **give up**, if it has one.
    ///
    /// Quietest first, and a lane under a handover is not a candidate at all: it
    /// is already on its way out for a reason the allocator decided, so a lend
    /// has to look past it rather than spend it — or, worse, skip the claim
    /// entirely because its quietest lane happened to be the one being handed
    /// over.
    #[must_use]
    pub fn lendable(&self) -> Option<LaneLoad> {
        self.lanes
            .iter()
            .filter(|lane| !lane.handover)
            .min_by_key(|lane| lane.flows)
            .copied()
    }

    /// The flows this claim is carrying: what it wants one lane per.
    #[must_use]
    pub fn talking_flows(&self) -> usize {
        self.lanes.iter().map(|lane| lane.flows).sum()
    }

    /// How many lanes this claim is short of, `0` when it is not.
    ///
    /// A claim with no flows at all wants the one lane every claim keeps — the
    /// minimum that makes it reachable — so an idle claim is never short and
    /// never grows.
    #[must_use]
    pub fn shortfall(&self) -> usize {
        self.talking_flows().max(1).saturating_sub(self.lanes.len())
    }
}

/// One lane's evidence over one sampling window.
///
/// The pair the report made readable: what the **peer** offered this lane and
/// what that lane's queue refused there (both from the peer's cumulative
/// counters, differenced per window), and what this end injected off it. The
/// peer's numbers are the only view of the loss that matters — on a transparent
/// claim the packets are offered on the peer's device, so this end's own
/// counters read zero for exactly the lane that is failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneEvidence {
    /// The lane's slot, so a verdict can name the one to retire.
    pub slot: usize,
    /// The lane's identity: what a drain is asked for, since a slot is reused.
    pub lane: LaneId,
    /// The carrier it actually rides, when the dialing end knows.
    pub carrier: Option<Carrier>,
    /// Packets the peer offered it in this window.
    pub offered: u64,
    /// Packets the peer's lane queue refused in this window.
    pub refused: u64,
    /// Packets this end injected off it in this window.
    pub injected: u64,
}

impl LaneEvidence {
    /// How much of what the peer offered failed to reach the lane's own queue,
    /// in percent, or `None` when it was offered nothing to judge on.
    #[must_use]
    pub fn refused_pct(&self) -> Option<f64> {
        (self.offered > 0).then(|| 100.0 * as_f64(self.refused) / as_f64(self.offered))
    }
}

/// The least evidence a lane has to carry in one window to be judged.
///
/// A quiet lane's ratio is noise: one refused packet out of two is 50 %, and a
/// policy that acted on it would move lanes for a single stray datagram. 256
/// packets in a one-second window is a real flow (a few hundred kbit/s even at
/// the smallest datagram size) and still reacts within seconds.
pub const CARRIER_MIN_PACKETS: u64 = 256;

/// The refusal share at which a carrier is judged to be losing traffic.
///
/// Below it the lane is carrying what it is offered; the peer's queue refusing
/// one packet in a hundred is a burst, not a carrier that cannot deliver.
pub const CARRIER_DROP_PCT: f64 = 1.0;

/// How many points lower the other carrier's refusal share has to be.
///
/// One point is the bench model's own materiality floor for inner loss
/// (`udp_loss_pct`): a smaller difference is not evidence of anything, and a
/// policy that acted on it would move lanes on scatter. Aligning the two means
/// the policy and the measurement that justifies it agree about "better".
pub const CARRIER_DROP_MARGIN_PCT: f64 = 1.0;

/// How much more the other carrier has to deliver per lane before it wins.
///
/// Used only when the refusal shares cannot separate them: the measured
/// divergence was 1.9x in the TCP fallback's favour, and the cells where the
/// carriers are level sit near 1.0.
pub const CARRIER_EDGE: f64 = 1.5;

/// What the rule decided for one claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarrierMove {
    /// The slot to ask the peer to drain, so its replacement can ride `prefer`.
    pub retire: usize,
    /// The lane id of that slot, which is what the drain names.
    pub lane: LaneId,
    /// The carrier the replacement should ride.
    pub prefer: Carrier,
}

/// Decide whether a claim should move a lane to the other carrier.
///
/// The rule, in order:
///
/// 1. only lanes whose carrier is known and which carried enough to judge
///    ([`CARRIER_MIN_PACKETS`]) are evidence at all;
/// 2. the claim's lanes on the carrier **in use** must be losing traffic
///    ([`CARRIER_DROP_PCT`]) — a claim carrying everything it is offered has
///    nothing to fix, however busy it is;
/// 3. there has to be somewhere to move to: either nothing rides the other
///    carrier yet (a *probe* — the only way to learn is to put a lane on it),
///    or it refuses materially less ([`CARRIER_DROP_MARGIN_PCT`]) or delivers
///    [`CARRIER_EDGE`] times as much per lane;
/// 4. and the lane to retire is the **worst** one on the losing carrier — the
///    one whose flows the move costs least — preferring a lane with no flows of
///    this end's own so the drain completes promptly.
#[must_use]
pub fn carrier_move(evidence: &[LaneEvidence], chosen: Carrier) -> Option<CarrierMove> {
    let other = match chosen {
        Carrier::Kcp => Carrier::Tcp,
        Carrier::Tcp => Carrier::Kcp,
    };
    let usable: Vec<&LaneEvidence> = evidence
        .iter()
        .filter(|lane| lane.carrier.is_some() && lane.offered >= CARRIER_MIN_PACKETS)
        .collect();
    let mine: Vec<&&LaneEvidence> = usable
        .iter()
        .filter(|lane| lane.carrier == Some(chosen))
        .collect();
    if mine.is_empty() {
        return None;
    }
    let losing = mine.iter().any(|lane| {
        lane.refused_pct()
            .is_some_and(|pct| pct >= CARRIER_DROP_PCT && lane.refused > 0)
    });
    if !losing {
        return None;
    }

    let theirs: Vec<&&LaneEvidence> = usable
        .iter()
        .filter(|lane| lane.carrier == Some(other))
        .collect();
    let better = if theirs.is_empty() {
        // Nothing has tried the other carrier: a probe is the only way to learn,
        // and the drain is what makes one possible.
        true
    } else {
        let mean_share = |set: &[&&LaneEvidence]| -> f64 {
            let judged: Vec<f64> = set.iter().filter_map(|lane| lane.refused_pct()).collect();
            if judged.is_empty() {
                0.0
            } else {
                judged.iter().sum::<f64>() / as_f64(u64::try_from(judged.len()).unwrap_or(u64::MAX))
            }
        };
        let mean_rate = |set: &[&&LaneEvidence]| -> f64 {
            let n = as_f64(u64::try_from(set.len()).unwrap_or(u64::MAX));
            set.iter().map(|lane| as_f64(lane.injected)).sum::<f64>() / n
        };
        let (mine_share, their_share) = (mean_share(&mine), mean_share(&theirs));
        their_share + CARRIER_DROP_MARGIN_PCT < mine_share
            || mean_rate(&theirs) > mean_rate(&mine) * CARRIER_EDGE
    };
    if !better {
        return None;
    }

    // The worst lane of the losing carrier: the highest refusal share, and
    // among equals the one this end is least attached to.
    let retire = mine
        .iter()
        .max_by(|a, b| {
            let share = |lane: &LaneEvidence| lane.refused_pct().unwrap_or(0.0);
            share(a)
                .partial_cmp(&share(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.injected.cmp(&a.injected))
                .then_with(|| b.slot.cmp(&a.slot))
        })
        .map(|lane| CarrierMove {
            retire: lane.slot,
            lane: lane.lane,
            prefer: other,
        })?;
    Some(retire)
}

/// A count as an `f64` without the precision worry the lint is about: a window's
/// packet count is orders of magnitude below `u32::MAX`, and the saturating
/// fallback could only mean something is already wrong.
fn as_f64(n: u64) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// What one tick decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Dial one more lane for this claim, out of budget no claim is using.
    Add {
        /// The claim that is short.
        to: Endpoint,
    },
    /// End one idle lane of a claim and dial one for another.
    ///
    /// The two halves are one decision: the total number of carrier connections
    /// the operator pays for does not change.
    Lend {
        /// The claim giving the lane up.
        from: Endpoint,
        /// The slot it gives up, which is idle.
        from_slot: usize,
        /// The claim that is short.
        to: Endpoint,
    },
}

/// Decide at most one lane to move, from what the claims are carrying.
///
/// `budget` is the carrier's lane budget (`[transparent.data.<carrier>].tunnels`
/// as written, else the claim count). `used` is how many lanes the claims hold
/// right now.
///
/// The order is deliberate: free budget first (nobody loses anything), then a
/// swap. A claim is short when it holds fewer lanes than it has talking flows; a
/// claim must hold more than one lane to give one up — the minimum one lane is
/// the promise that a claim is always reachable — and only an idle lane is ever
/// given up, so no flow is ever moved by this. Ties are broken by endpoint and
/// slot so the same snapshot always yields the same decision.
#[must_use]
pub fn plan(loads: &[ClaimLoad], budget: usize, used: usize) -> Option<Plan> {
    // The claim with the most flows per lane held is the one furthest from a
    // lane per flow; the endpoint breaks a tie so the same snapshot always
    // yields the same decision.
    // A claim with no live lane at all is not short of anything *yet*: its lanes
    // are still joining (or all of them just died, which is the hub's replacement
    // path to fix, not this one). Acting on it here would dial a lane beside the
    // share the claim is already opening.
    let short = loads
        .iter()
        .filter(|claim| !claim.lanes.is_empty() && claim.shortfall() > 0)
        .max_by_key(|claim| (claim.shortfall(), claim.endpoint))?;

    // Budget no claim is using: growing costs nobody.
    if used < budget {
        return Some(Plan::Add { to: short.endpoint });
    }

    // Otherwise it is a swap: the idlest lane of a claim that has a lane to
    // spare. The giver must not be the receiver (a claim cannot lend to
    // itself), and the receiver must not be the giver's only lane.
    let donor = loads
        .iter()
        .filter(|claim| claim.endpoint != short.endpoint && claim.lanes.len() > 1)
        .filter_map(|claim| claim.lendable().map(|lane| (lane, claim)))
        // Emptiness, not merely quiet: it is the condition the hub retires on
        // (`MemberSlot::flows == 0`), so the retire below is atomic — the
        // receiver's new lane is dialed only once the donor's has actually ended,
        // and the pair never holds more carrier connections than the budget at
        // any instant. A lane whose one quiet flow is still placed in it becomes
        // lendable when that flow falls silent and the table forgets it
        // (`FLOW_IDLE_EVICTION`, 60 s).
        .filter(|(lane, _)| lane.flows == 0)
        .min_by_key(|(lane, claim)| (lane.flows, claim.endpoint, lane.slot))?;
    Some(Plan::Lend {
        from: donor.1.endpoint,
        from_slot: donor.0.slot,
        to: short.endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// A claim for the tests: `flows` are its lanes' flow counts, in slot order.
    fn claim(last_octet: u8, flows: &[usize]) -> ClaimLoad {
        ClaimLoad {
            endpoint: Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 99, 0, last_octet)), 8443),
            lanes: flows
                .iter()
                .enumerate()
                .map(|(slot, flows)| LaneLoad {
                    slot,
                    flows: *flows,
                    handover: false,
                })
                .collect(),
        }
    }

    /// The same claim, with one of its lanes being handed over to another
    /// carrier: a lane that is already on its way out.
    fn claim_handing_over(last_octet: u8, flows: &[usize], handing_over: usize) -> ClaimLoad {
        let mut claim = claim(last_octet, flows);
        if let Some(lane) = claim.lanes.get_mut(handing_over) {
            lane.handover = true;
        }
        claim
    }

    /// The endpoint helper, so an expectation reads as the claim it means.
    fn at(last_octet: u8) -> Endpoint {
        claim(last_octet, &[]).endpoint
    }

    /// A claim carrying more flows than it has lanes is the one that grows, and
    /// free budget is spent before anything is taken away: nobody loses a lane
    /// while the operator's number has room.
    #[test]
    fn a_claim_short_of_its_flows_grows_into_unused_budget_first() {
        let loads = [claim(1, &[3]), claim(2, &[1])];
        assert_eq!(plan(&loads, 4, 2), Some(Plan::Add { to: at(1) }));
    }

    /// With the budget spent, an idle lane of another claim is the one that
    /// moves — and the decision names the slot, because that is what the hub is
    /// asked to end.
    #[test]
    fn a_spent_budget_lends_an_idle_lane_from_another_claim() {
        let loads = [claim(1, &[2]), claim(2, &[0, 0])];
        assert_eq!(
            plan(&loads, 3, 3),
            Some(Plan::Lend {
                from: at(2),
                from_slot: 0,
                to: at(1),
            }),
        );
    }

    /// A claim never gives up its last lane, however idle: one connection is
    /// what makes it reachable at all.
    #[test]
    fn a_claims_last_lane_is_never_lent() {
        let loads = [claim(1, &[4]), claim(2, &[0]), claim(3, &[0])];
        assert_eq!(
            plan(&loads, 3, 3),
            None,
            "no claim holds a lane it can spare"
        );
    }

    /// Only an idle lane is given up, so lending can never move a flow: a claim
    /// whose quietest lane is carrying something is not a donor.
    #[test]
    fn a_lane_that_is_carrying_anything_is_not_lent_away() {
        let loads = [claim(1, &[3]), claim(2, &[1, 1])];
        assert_eq!(plan(&loads, 3, 3), None);
    }

    /// A claim with a lane per talking flow has nothing left to gain from
    /// another one — a flow cannot use two lanes, because its packets have to
    /// stay in order. That is what makes the steady state stable rather than a
    /// claim that grows until the budget runs out.
    #[test]
    fn a_lane_per_flow_is_the_steady_state() {
        let loads = [claim(1, &[1, 1, 1]), claim(2, &[0, 0])];
        assert_eq!(plan(&loads, 5, 5), None);
    }

    /// An idle claim is not short: it keeps the one lane every claim keeps and
    /// asks for nothing.
    #[test]
    fn an_idle_claim_is_never_short() {
        assert_eq!(claim(1, &[0, 0, 0]).shortfall(), 0);
        assert_eq!(claim(1, &[]).shortfall(), 1, "a claim with no lanes at all");
        assert_eq!(plan(&[claim(1, &[0, 0])], 2, 2), None);
    }

    /// The claim furthest from a lane per flow receives; the endpoint breaks a
    /// tie, so the same snapshot yields the same decision every time.
    #[test]
    fn the_claim_furthest_from_a_lane_per_flow_receives() {
        let loads = [claim(1, &[2]), claim(2, &[5]), claim(3, &[0, 0])];
        assert_eq!(
            plan(&loads, 4, 4),
            Some(Plan::Lend {
                from: at(3),
                from_slot: 0,
                to: at(2),
            }),
        );
    }

    /// A claim whose lanes have not joined yet is not short: the share it is
    /// already opening is on its way, and dialing beside it would double it.
    #[test]
    fn a_claim_with_no_live_lane_is_not_short() {
        let loads = [claim(1, &[]), claim(2, &[0, 0])];
        assert_eq!(plan(&loads, 2, 2), None);
    }

    /// One claim on its own cannot lend to itself, and an empty set decides
    /// nothing: the policy is silent rather than inventing a reason to act.
    #[test]
    fn one_claim_alone_is_never_a_donor() {
        assert_eq!(plan(&[claim(1, &[50, 0, 0])], 3, 3), None);
        assert_eq!(plan(&[], 3, 0), None);
    }

    /// An evidence sample: slot, lane id, carrier, offered, refused, injected.
    fn proof(
        slot: usize,
        lane: u32,
        carrier: Carrier,
        offered: u64,
        refused: u64,
        injected: u64,
    ) -> LaneEvidence {
        LaneEvidence {
            slot,
            lane: LaneId::new(lane),
            carrier: Some(carrier),
            offered,
            refused,
            injected,
        }
    }

    /// A claim carrying everything it is offered has nothing to fix, whichever
    /// carrier it rides: no loss, no probe, no move.
    #[test]
    fn a_carrier_losing_nothing_is_left_alone() {
        let evidence = [
            proof(0, 1, Carrier::Kcp, 10_000, 0, 9_900),
            proof(1, 2, Carrier::Kcp, 9_000, 5, 8_900),
        ];
        assert_eq!(carrier_move(&evidence, Carrier::Kcp), None);
        assert_eq!(carrier_move(&evidence, Carrier::Tcp), None);
    }

    /// A lane that is offered too little to judge is not evidence: one refused
    /// packet out of ten is 10 %, and acting on it would move lanes for a stray
    /// datagram.
    #[test]
    fn a_lane_below_the_evidence_floor_is_not_judged() {
        let evidence = [proof(0, 1, Carrier::Kcp, CARRIER_MIN_PACKETS - 1, 5, 10)];
        assert_eq!(carrier_move(&evidence, Carrier::Kcp), None);
    }

    /// A claim losing traffic on its chosen carrier probes the other one: with
    /// no lane there yet, a lane is the only way to learn, and the worst lane is
    /// the one whose flows the move costs least.
    #[test]
    fn a_lossy_carrier_is_probed_by_draining_its_worst_lane() {
        let evidence = [
            proof(0, 7, Carrier::Kcp, 10_000, 4_000, 6_000),
            proof(1, 8, Carrier::Kcp, 10_000, 1_000, 9_000),
        ];
        assert_eq!(
            carrier_move(&evidence, Carrier::Kcp),
            Some(CarrierMove {
                retire: 0,
                lane: LaneId::new(7),
                prefer: Carrier::Tcp,
            }),
            "the lane refusing most is the one to replace"
        );
    }

    /// Once the other carrier has a lane, the comparison decides: a materially
    /// lower refusal share wins, one that is level or worse is left where it is.
    #[test]
    fn the_comparison_needs_a_real_gap() {
        // The probe refuses 0.2 % against the rest's 40 %: move.
        let better = [
            proof(0, 1, Carrier::Kcp, 10_000, 4_000, 6_000),
            proof(1, 2, Carrier::Kcp, 10_000, 3_000, 7_000),
            proof(2, 3, Carrier::Tcp, 10_000, 20, 9_980),
        ];
        assert_eq!(
            carrier_move(&better, Carrier::Kcp),
            Some(CarrierMove {
                retire: 0,
                lane: LaneId::new(1),
                prefer: Carrier::Tcp,
            })
        );

        // Level (inside the margin): no move, however lossy the path is.
        let level = [
            proof(0, 1, Carrier::Kcp, 10_000, 4_000, 6_000),
            proof(1, 2, Carrier::Kcp, 10_000, 3_000, 7_000),
            proof(2, 3, Carrier::Tcp, 10_000, 3_500, 6_500),
        ];
        assert_eq!(carrier_move(&level, Carrier::Kcp), None);

        // Higher refusal share but far more delivered: the rate arm of the rule
        // is what decides, and it needs CARRIER_EDGE.
        let rate = [
            proof(0, 1, Carrier::Kcp, 10_000, 2_000, 1_000),
            proof(1, 2, Carrier::Tcp, 10_000, 2_500, 2_000),
        ];
        assert_eq!(
            carrier_move(&rate, Carrier::Kcp),
            Some(CarrierMove {
                retire: 0,
                lane: LaneId::new(1),
                prefer: Carrier::Tcp,
            }),
            "twice the delivered rate is a real edge"
        );
    }

    /// A lane that does not say what it rides is not evidence.
    ///
    /// It can neither be judged nor retired for a carrier move: the rule leaves
    /// it out entirely, so a claim whose *only* known lane is unknown decides
    /// nothing, and a claim with one known (lossy) lane probes on that one.
    #[test]
    fn unknown_carriers_are_not_evidence() {
        let unknown = LaneEvidence {
            slot: 0,
            lane: LaneId::new(1),
            carrier: None,
            offered: 10_000,
            refused: 9_000,
            injected: 0,
        };

        // Only an unknown lane: nothing to judge, nothing to retire.
        assert_eq!(carrier_move(&[unknown], Carrier::Kcp), None);

        // One known, lossy lane beside it: the probe is decided on the known
        // lane — the unknown one is not evidence for it, and is not a target.
        let evidence = [unknown, proof(1, 2, Carrier::Kcp, 10_000, 8_000, 1_000)];
        assert_eq!(
            carrier_move(&evidence, Carrier::Kcp),
            Some(CarrierMove {
                retire: 1,
                lane: LaneId::new(2),
                prefer: Carrier::Tcp,
            }),
            "the unknown lane is neither a target nor a veto"
        );
    }

    /// **A handover in flight outranks a lend.**
    ///
    /// A lane being handed over is already on its way out: the peer has been
    /// asked to stop placing flows in it and the dialer retires it as soon as
    /// both ends are empty. Lending that same lane would spend the move the
    /// handover is making, so the allocator looks past it — and only decides
    /// nothing when there is no other idle lane to give.
    #[test]
    fn a_lane_being_handed_over_is_never_lent() {
        let receiver = claim(2, &[5]);

        // The donor's only idle lane is the one being handed over: nothing to
        // lend, and the receiver waits rather than cancelling the handover.
        let loads = [claim_handing_over(3, &[3, 0], 1), receiver.clone()];
        assert_eq!(plan(&loads, 4, 4), None);

        // A second idle lane exists, so the lend happens — on that one, not on
        // the handover's, whichever slot it sits in.
        let loads = [claim_handing_over(3, &[0, 0], 0), receiver.clone()];
        assert_eq!(
            plan(&loads, 4, 4),
            Some(Plan::Lend {
                from: at(3),
                from_slot: 1,
                to: at(2),
            }),
            "the handover's lane is passed over, not spent"
        );

        // The same, with the handover in the middle: the lend takes the first
        // idle lane that is not the handover's.
        let loads = [claim_handing_over(3, &[0, 0, 0], 1), receiver];
        assert_eq!(
            plan(&loads, 4, 4),
            Some(Plan::Lend {
                from: at(3),
                from_slot: 0,
                to: at(2),
            }),
        );
    }
}
