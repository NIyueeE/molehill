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

use crate::transparent::Endpoint;

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
    /// The lane with the fewest flows, and that count.
    #[must_use]
    pub fn quietest(&self) -> Option<LaneLoad> {
        self.lanes.iter().copied().min_by_key(|lane| lane.flows)
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
        .filter_map(|claim| claim.quietest().map(|lane| (lane, claim)))
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
                })
                .collect(),
        }
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
}
