//! The process-wide TUN hub: one reader per device, one member set per claim.
//!
//! Both ends of a transparent service use the same shape, only the direction
//! of the decision differs in wording, not in code:
//!
//! - the **server** routes a packet that its host's routing put on the device
//!   to the service whose endpoint the packet's destination names;
//! - the **client** does the same lookup with its own claimed endpoints, which
//!   is an allow-list: a packet for an address this host did not claim is
//!   dropped, so a compromised peer cannot steer traffic into a local address
//!   of its choosing.
//!
//! A claimed endpoint is carried by a **lane set**, not by one channel: each
//! data channel of the claim owns a slot of the set, and every packet is placed
//! in a slot while the device is drained. A slot whose lane is gone — a
//! replacement is in flight — is *empty*, and the packets placed in it are
//! dropped and counted exactly as the single-channel path counted the packets
//! of a channel that had ended; the surviving lanes are untouched. A lane's slot
//! never moves: a replaced channel takes the slot its predecessor left.
//!
//! **Placement is a table, not a modulus.** A flow is placed once, on its first
//! packet, into the least-loaded live slot (ties broken by the flow hash, which
//! is what lets the two ends of a claim agree without a word on the wire), and
//! the entry is what routes its later packets — so a lane joining, or a lane
//! being lent to another claim, cannot move a flow that is already talking, and
//! a *new* flow never lands in an empty slot. Entries are forgotten after
//! [`FLOW_IDLE_EVICTION`] of silence in either direction, which is safe because
//! a flow that quiet has nothing in flight to reorder, and the table stops
//! remembering past [`MAX_TRACKED_FLOWS`] (a visitor must not be able to grow it
//! without bound) — the flows beyond that are placed by the hash alone, which is
//! stable for as long as the set's width is.
//!
//! Queues are bounded and lossy on overflow, UDP semantics: a full queue drops
//! the packet rather than stalling every other service sharing the device.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

use crate::protocol::{IpFrames, IpTraffic, LaneId};
use crate::transparent::flow;
use crate::transparent::ip::{self, PacketInfo};
use crate::transparent::tun::Tun;
use crate::transparent::{Direction, DropReason, Endpoint, EndpointTable, Stats};
#[cfg(feature = "multiplex")]
use crate::transport::multiplex::Carrier;
/// The carriers a lane can ride, for the builds that have a choice.
///
/// Without `multiplex` there is one data plane and nothing to record, so the
/// type is an empty enum and every `Option<Carrier>` in this module is `None` —
/// the carrier-aware paths compile away rather than growing a second shape.
#[cfg(not(feature = "multiplex"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {}

#[cfg(not(feature = "multiplex"))]
impl Carrier {
    /// The stub is never constructed: a build without `multiplex` has no
    /// carrier a lane could ride, so every `Option<Carrier>` in this module is
    /// `None`. The method exists only so the carrier-aware call sites (the
    /// stats line's `map_or("-", Carrier::as_str)`) compile unchanged — the
    /// match is exhaustive because the type is uninhabited.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {}
    }
}

/// Batches waiting for one service's channel. Small on purpose: the point of
/// the queue is to absorb a burst while the channel is busy, not to buffer a
/// second of traffic. A batch is many packets, so this is a byte budget as much
/// as a message count.
const QUEUE: usize = 128;

/// The most packets one batch may carry. This is what keeps a small-packet
/// flow (the workload the batching exists for) from building a message that
/// has to wait for company: it is flushed as soon as the device runs dry
/// either way, and this only bounds how much one hand-over may hold.
const BATCH_PACKETS: usize = 32;

/// The most bytes one batch may carry, whichever comes first. Large enough to
/// fill several carrier segments in one write, small enough that a batch is
/// never a burst of its own.
const BATCH_BYTES: usize = 16 * 1024;

/// The largest packet a TUN read is willing to see: an IPv4 packet with
/// options, bounded by the frame's `u16` length prefix.
pub const MAX_PACKET: usize = u16::MAX as usize;

/// How long a flow's placement is remembered after its last packet.
///
/// The table exists so that a flow never *moves*: its entry is the slot it was
/// placed in for as long as it keeps talking. Idleness is therefore the only
/// safe thing to forget — a flow silent for this long has no packet in flight
/// on any carrier (that window is milliseconds), so re-placing it cannot
/// reorder anything the old slot had already delivered. The value matches the
/// UDP peer-mapping timeout, so one number describes both kinds of forgotten
/// state.
const FLOW_IDLE_EVICTION: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a lane must have been silent for a **peer's** drain to call it
/// empty.
///
/// Not `flows == 0`: the placement table remembers a flow for
/// [`FLOW_IDLE_EVICTION`] after its last packet, which is what keeps a live
/// flow's placement stable, and "remembered" is not "talking". A drain that
/// waited for the table to forget would wait a minute for a flow that stopped
/// seconds ago — longer than the dialer's own budget, so every handover on a
/// workload that changes flows every few seconds would be called off (measured:
/// three runs, three drains asked, none completed, each answered `pending` for a
/// flow that had already gone quiet). This window is short enough to let a
/// handover finish and long enough that a flow with packets in flight is never
/// taken.
const DRAIN_QUIET: std::time::Duration = std::time::Duration::from_secs(3);

/// How many flows one claim's placement table remembers.
///
/// A table without a bound is memory an unauthenticated visitor can drive (one
/// entry per five-tuple), so past this many *live* flows a new flow is placed
/// by [`flow::slot`] alone instead of being remembered: the hash is a pure
/// function of the packet and the live set, so such a flow still keeps one slot
/// while the set's width does, and the idle sweep makes room for the next one.
const MAX_TRACKED_FLOWS: usize = 4096;

/// One TUN device, its reader task, and the per-claim member sets.
pub struct TunHub {
    name: String,
    tun: Arc<Tun>,
    direction: Direction,
    routes: Arc<Routes>,
}

/// One hub per `(device, direction)`: a device read once, with the endpoints
/// of that side.
type HubKey = (String, Direction);
type HubRegistry = OnceLock<Mutex<HashMap<HubKey, Arc<TunHub>>>>;

static HUBS: HubRegistry = OnceLock::new();

impl TunHub {
    /// The hub for `name`, attaching to the device the first time.
    ///
    /// One hub per device per process: two services sharing a device must not
    /// each run a reader, or they would race for the same packets. The hub also
    /// owns the data path's counters, so two claims sharing a device count into
    /// one line instead of each reporting a slice of the same traffic.
    pub fn get_or_spawn(name: &str, direction: Direction) -> Result<Arc<TunHub>> {
        let hubs = HUBS.get_or_init(|| Mutex::new(HashMap::new()));
        let key = (name.to_owned(), direction);
        let mut guard = hubs
            .lock()
            .map_err(|_| anyhow::anyhow!("the TUN hub registry is poisoned"))?;
        if let Some(hub) = guard.get(&key) {
            return Ok(Arc::clone(hub));
        }

        let tun = Arc::new(Tun::attach(name)?);
        let hub = Arc::new(TunHub {
            name: name.to_owned(),
            tun: Arc::clone(&tun),
            direction,
            routes: Arc::new(Routes::new()),
        });
        hub.clone().spawn_reader();
        spawn_stats_reporter(Arc::clone(&hub));
        guard.insert(key, Arc::clone(&hub));
        tracing::info!(tun = %hub.name, ?direction, "Transparent data path attached to the TUN device");
        Ok(hub)
    }

    /// Which end of a packet this hub's endpoints are recognised by.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The counters of this device's data path. One set per hub, so every claim
    /// the device carries is counted in the same place.
    pub fn stats(&self) -> Arc<Stats> {
        self.routes.stats()
    }

    /// Every member slot of every claim this hub carries, live or empty:
    /// which slot carried how much, and which ones a replacement window cost.
    pub fn member_stats(&self) -> Vec<MemberStat> {
        self.routes.member_stats()
    }

    /// Hand one packet to the kernel, as if it had arrived on the device.
    pub async fn inject(&self, packet: &[u8]) -> io::Result<()> {
        self.tun.write_packet(packet).await
    }

    /// Count packets a lane delivered into this end's device (see
    /// [`Routes::count_injected`]).
    pub fn count_injected(&self, endpoint: &Endpoint, slot: usize, packets: u64, bytes: u64) {
        self.routes.count_injected(endpoint, slot, packets, bytes);
    }

    /// Record what the peer reported offering one of this end's lanes (see
    /// [`Routes::note_remote`]).
    ///
    /// The report is named to a **claim** (`endpoint`) as well as a lane: the
    /// hub is shared by every claim on the device, each claim mints its lane
    /// ids from 1, and a lane id alone does not name a lane — a report keyed
    /// only by id landed on the first claim holding a lane with that number
    /// (measured: a report for the ladder's claim read as another claim's
    /// `peer_offered`), which is evidence about nothing.
    pub fn note_remote(
        &self,
        endpoint: Endpoint,
        lane: LaneId,
        offered: u32,
        refused: u32,
        seq: u8,
    ) {
        self.routes
            .note_remote(endpoint, lane, offered, refused, seq);
    }

    /// The lanes claims let go after their last lane left, with what they
    /// carried (see [`Routes::retired_lanes`]).
    ///
    /// A handover is judged against the lane it replaced, and on a one-lane
    /// claim that lane is already gone from the routing table by the time the
    /// decision is taken — this is where its evidence survives.
    pub fn retired_lanes(&self) -> Vec<RetiredLane> {
        self.routes.retired_lanes()
    }

    /// Note that one flow was seen arriving from one of a claim's lanes, so its
    /// placement is not forgotten while it is still talking (see
    /// [`Routes::touch`]).
    pub fn touch(&self, endpoint: &Endpoint, info: &PacketInfo) {
        self.routes.touch(endpoint, info);
    }

    /// Give one data channel a slot in `endpoint`'s member set and register its
    /// queue for it.
    ///
    /// The set's width is *discovered*, not declared: it is the widest the
    /// claim has been seen to hold at once, so a channel that never becomes a
    /// member (its start command failed, or a peer that only ever starts one
    /// channel) leaves no slot behind that nothing could fill. The slot lives
    /// exactly as long as the returned guard.
    fn join(
        &self,
        endpoint: Endpoint,
        queue: mpsc::Sender<Bytes>,
        lane: Option<LaneId>,
        carrier: Option<Carrier>,
    ) -> Result<MemberGuard> {
        let slot = self.routes.join(endpoint, queue, lane, carrier)?;
        // A claim that vanished between the two calls leaves the lane with a
        // wake nobody will ring: it simply never retires, which is the safe
        // direction.
        let (retire, draining) = self.routes.retire_watch(&endpoint, slot);
        Ok(MemberGuard {
            routes: Arc::clone(&self.routes),
            endpoint,
            slot,
            retire,
            draining,
        })
    }

    /// Ask one of a claim's lanes to end, and say whether it is idle right now.
    ///
    /// This is the whole of the hub's part in lending a lane: the hub never ends
    /// a lane itself (the task that owns the connection does, see
    /// [`MemberGuard::retired`]) and never decides *whether* to — that is the
    /// allocator's policy ([`crate::transparent::lanes`]). A caller reaches for
    /// this only for a slot the policy has already found idle; the return value
    /// is the same answer as seen under the lock.
    pub fn retire_lane(&self, endpoint: &Endpoint, slot: usize) -> bool {
        self.routes.retire_lane(endpoint, slot)
    }

    /// Stop placing new flows in the named lane, and say whether this end
    /// already holds none of its own (see [`Routes::begin_drain`]).
    pub fn begin_drain(&self, endpoint: &Endpoint, lane: LaneId) -> Option<bool> {
        self.routes.begin_drain(endpoint, lane)
    }

    /// Put a drained lane back in service (see [`Routes::cancel_drain`]).
    pub fn cancel_drain(&self, endpoint: &Endpoint, lane: LaneId) -> Option<()> {
        self.routes.cancel_drain(endpoint, lane)
    }

    /// The reader loop: device to service queues. Runs until the device is
    /// gone.
    ///
    /// It drains the device and hands over **batches**, one per (endpoint,
    /// member), not packets: a batch is a run of `[u16 length][packet]` frames
    /// in one `Bytes`, so N packets cost one queue message, one `write_all` on
    /// the tunnel and — with a carrier that segments at the MSS — one carrier
    /// header and one acknowledgement instead of N. That per-packet transport
    /// cost was measured at ~80 bytes against a ~35-byte header, which is what
    /// makes this the lever ([benchmarks.md](../../docs/benchmarks.md), "The
    /// transparent-L3 wire question").
    fn spawn_reader(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_PACKET];
            // One buffer per (endpoint, member) while the device is drained.
            // Keyed by the slot rather than by the queue so a batch survives the
            // queue being replaced mid-drain: the flush looks the current queue
            // up.
            let mut batches: HashMap<(Endpoint, usize), Batch> = HashMap::new();
            loop {
                let outcome = self
                    .tun
                    .drain(&mut buf, &mut |packet| self.route(packet, &mut batches))
                    .await;
                // Whatever ended the drain, what was read is written: a batch
                // never waits for the next packet to arrive.
                for ((endpoint, slot), batch) in batches.drain() {
                    self.routes.flush(&endpoint, slot, batch);
                }
                if let Err(e) = outcome {
                    // A dead device stops the whole path; the services'
                    // channels then fail their own way, one log line each.
                    tracing::debug!(tun = %self.name, "TUN read ended: {e}");
                    return;
                }
            }
        });
    }

    /// Parse one packet and append it to its member's batch, flushing that
    /// batch when it is as large as one message may grow.
    fn route(&self, packet: &[u8], batches: &mut HashMap<(Endpoint, usize), Batch>) {
        let info = match ip::parse(packet) {
            Ok(info) => info,
            Err(e) => {
                self.routes.stats.count_drop(DropReason::from(e));
                return;
            }
        };
        let Some((endpoint, slot)) = self.routes.pick(&info, self.direction) else {
            self.routes.stats.count_drop(DropReason::Unclaimed);
            return;
        };
        let batch = batches.entry((endpoint, slot)).or_default();
        if batch.push(packet).is_err() {
            // An oversized packet cannot be framed; it is not the batch's
            // problem, and dropping it keeps the rest.
            self.routes.stats.count_drop(DropReason::Malformed);
            return;
        }
        if batch.is_full()
            && let Some(batch) = batches.remove(&(endpoint, slot))
        {
            self.routes.flush(&endpoint, slot, batch);
        }
    }
}

/// One member slot's lease: joining a member set takes the slot, dropping the
/// guard empties it.
///
/// A guard is what keeps a *replaced* member's slot its own: the slot is cleared
/// rather than removed, so the set's width (the routing modulus) does not move
/// when a member does, and the replacement takes the freed index.
struct MemberGuard {
    routes: Arc<Routes>,
    endpoint: Endpoint,
    slot: usize,
    /// Woken when any lane of this claim is asked to end; the guard then asks
    /// the hub whether the ask was for *its* slot.
    retire: Arc<Notify>,
    /// This slot's own draining mark, readable without the table's lock.
    draining: Arc<std::sync::atomic::AtomicBool>,
}

impl MemberGuard {
    /// Resolves when this lane has been asked to end and has nothing left to
    /// carry.
    ///
    /// The hub does not end a lane: the task that owns the connection does, so
    /// the packets already in flight on it finish first — and no new flow can
    /// have been placed in its slot since the ask, because a draining slot takes
    /// none (`ClaimSlots::live_slots`). Waiting on the notification rather than
    /// polling is what keeps a quiet lane from holding its slot until its next
    /// packet, which may never come.
    /// The wait is *owned* rather than borrowed: it outlives the guard, which
    /// is dropped as the relay returns (vacating the slot) while the wait is
    /// still in scope.
    fn retired_owned(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let routes = Arc::clone(&self.routes);
        let endpoint = self.endpoint;
        let slot = self.slot;
        let retire = Arc::clone(&self.retire);
        let draining = Arc::clone(&self.draining);
        async move {
            loop {
                // Armed before the check, so an ask that lands between the two
                // is not missed (the permit is stored).
                let notified = Arc::clone(&retire).notified_owned();
                // The atomic first: this runs on the lane's hot path (once per
                // relayed packet), and the table's lock is only worth taking
                // when an ask has actually been made.
                if draining.load(std::sync::atomic::Ordering::Relaxed)
                    && routes.should_retire(&endpoint, slot)
                {
                    return;
                }
                notified.await;
            }
        }
    }
}

impl Drop for MemberGuard {
    fn drop(&mut self) {
        self.routes.vacate(&self.endpoint, self.slot);
    }
}

/// What the **peer** reported offering one of this end's lanes, and what that
/// lane's queue refused there.
///
/// The receiving end cannot see any of this: it only ever sees what arrived.
/// On a transparent claim the loss that matters happens where the packets are
/// offered — the peer reads the visitor's traffic off its own device and hands
/// it to the lanes — so a lane whose peer-side queue refuses 44 % of what it
/// is offered is a carrier that is not delivering, while this end's own
/// counters read zero for exactly that lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteLane {
    /// Packets the peer offered this lane, cumulative for its life.
    pub offered: u32,
    /// Packets the peer's lane queue refused, cumulative for its life.
    pub refused: u32,
    /// The peer's sample sequence, so a late report is not read as a fresh one.
    pub seq: u8,
}

/// What one lane carried, counted where the packets actually are.
///
/// The round-3 policy read `no_channel` as "the carrier dropped this", and it
/// is not: `Routes::flush` counts a *local enqueue* result, and two different
/// causes share it — a full batch queue (this end's own backpressure) and a
/// slot whose lane is gone (a replacement window). Neither says anything about
/// the carrier, so a policy comparing carriers on it is comparing noise.
///
/// What a carrier *can* be judged on is the pair: what this end offered a lane
/// (`offered`), what it refused (`overflow` — a queue, not a path), what was
/// lost to a lane being absent (`disconnected`), and what the *other* end
/// actually injected off that lane (`injected`). The last one is the only
/// "delivery" in the set, and it lives on the receiving end, which is why both
/// ends have to report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaneCounters {
    /// Packets the device handed to this lane's slot.
    pub offered_packets: u64,
    /// Their bytes, as framed for the lane (the wire's own denominator).
    pub offered_bytes: u64,
    /// Packets this lane's queue accepted.
    pub enqueued_packets: u64,
    pub enqueued_bytes: u64,
    /// Packets refused because the lane's queue was full: this end's
    /// backpressure, not the path's loss.
    pub overflow_packets: u64,
    pub overflow_bytes: u64,
    /// Packets refused because no lane held the slot.
    pub disconnected_packets: u64,
    pub disconnected_bytes: u64,
    /// Packets this end injected into its device, delivered by this lane.
    pub injected_packets: u64,
    pub injected_bytes: u64,
}

impl LaneCounters {
    /// Count what the device offered this lane.
    fn offer(&mut self, packets: u64, bytes: u64) {
        self.offered_packets += packets;
        self.offered_bytes += bytes;
    }

    /// Count the two ways an offer is refused, separately.
    fn refuse(&mut self, packets: u64, bytes: u64, lane_absent: bool) {
        if lane_absent {
            self.disconnected_packets += packets;
            self.disconnected_bytes += bytes;
        } else {
            self.overflow_packets += packets;
            self.overflow_bytes += bytes;
        }
    }

    /// Count packets this lane delivered into the local device.
    fn inject(&mut self, packets: u64, bytes: u64) {
        self.injected_packets += packets;
        self.injected_bytes += bytes;
    }
}

/// The counters of a lane that no longer holds a slot.
///
/// Kept, bounded, because the interesting lane is often a *gone* one: a
/// handover has to be judged on the lane it replaced, and a retired lane's
/// numbers are the only evidence of how the carrier it rode did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredLane {
    pub endpoint: Endpoint,
    pub lane: LaneKey,
    pub counters: LaneCounters,
}

/// How many retired lanes one claim remembers. The oldest are dropped: a
/// handover is judged on the lane it just replaced, not on a session's
/// archaeology, and a visitor must not be able to grow this.
const RETIRED_LANES: usize = 4;

/// The identity of one lane, as the hub records it.
///
/// A lane is the **connection**, and it is what a carrier can be compared per:
/// a slot is one end's routing index, reused by whatever takes it next (a
/// replacement inherits its predecessor's), so a carrier recorded on a slot
/// describes the *slot's current tenant* — which makes a half-finished handover
/// invisible: the old-carrier lanes still to be migrated no longer appear
/// anywhere. The lane id is named by the dialer in the prologue, so both ends
/// agree on it without agreeing on an index, and the generation separates this
/// connection from the one that will take its slot after it retires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaneKey {
    /// The id the dialing end named in the lane's prologue.
    pub id: LaneId,
    /// How many lanes have held this lane's slot before it. Cheap, local, and
    /// enough to tell a replacement from its predecessor when both are being
    /// counted in one window.
    pub generation: u32,
}

/// One member slot of one claim, as the stats line reports it.
///
/// `Clone` because a snapshot of it is what the policy (and its tests) read:
/// the counters move while a claim carries traffic, so a decision has to be
/// taken on a copy rather than under the routing table's lock.
#[derive(Clone, Debug)]
pub struct MemberStat {
    /// The claimed endpoint the slot belongs to.
    pub endpoint: Endpoint,
    /// The slot's index in the claim's member set.
    pub slot: usize,
    /// Whether a member currently holds the slot.
    pub live: bool,
    /// Whether the claim's owner has asked this lane to end: it takes no new
    /// flows, and ends once the ones it has are gone.
    pub draining: bool,
    /// Whether the *peer* asked this lane to stop taking new flows, which this
    /// end answers rather than acts on.
    pub peer_draining: bool,
    /// The lane currently holding this slot, when its dialing end named one.
    pub lane: Option<LaneKey>,
    /// The carrier this slot's lane rides, when its dialing end knows.
    pub carrier: Option<Carrier>,
    /// Packets this slot handed to its member.
    pub forwarded: u64,
    /// What the lane holding this slot carried, and what it refused.
    pub counters: LaneCounters,
    /// What the peer reported offering this lane, when it reports at all.
    pub remote: Option<RemoteLane>,
    /// Counters of this slot's previous lanes, newest last.
    pub retired: Vec<RetiredLane>,
    /// Packets placed in this slot while no member held it (a replacement
    /// window): the traffic the claim lost with that member.
    pub no_channel: u64,
    /// Flows whose placement still names this slot **and** which have carried a
    /// packet within [`DRAIN_QUIET`]: what a peer's drain waits for.
    pub active_flows: usize,
    /// Flows whose placement still names this slot.
    ///
    /// This is what says whether a lane is idle: it counts the flows the table
    /// remembers *for this slot*, including the ones placed there while the
    /// slot's member was gone (they keep their place and resume when the
    /// replacement joins). A slot is idle when this is zero and nothing is
    /// queued — the condition a lane has to meet before it can be lent
    /// elsewhere.
    pub flows: usize,
}

/// The routing state of one hub, without the device.
///
/// Split from [`TunHub`] on purpose: the member sets and the placement rule are
/// pure bookkeeping, so they are unit-tested without a TUN device — the same
/// split `transport::pool` makes for the tunnel pool's policy.
struct Routes {
    table: Mutex<EndpointTable<ClaimSlots>>,
    stats: Arc<Stats>,
    /// Lanes claims let go *after* their last lane left, newest last.
    ///
    /// This is the archive that outlives a claim: a handover on a one-lane
    /// claim retires that lane, the claim leaves the routing table empty-handed,
    /// and the only evidence of how the old carrier did would go with it.
    retired: Mutex<Vec<RetiredLane>>,
}

/// How many claims' worth of gone lanes the archive keeps. Bounded: a visitor
/// must not be able to grow it, and a handover is judged on the lane it just
/// replaced, not on a session's archaeology.
const ARCHIVE_CLAIMS: usize = 8;

impl Routes {
    fn new() -> Self {
        Self {
            table: Mutex::new(EndpointTable::new()),
            stats: Arc::new(Stats::default()),
            retired: Mutex::new(Vec::new()),
        }
    }

    fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Take a slot in `endpoint`'s member set and register `queue` for it.
    ///
    /// A claim starts as one slot and a join that finds every slot taken widens
    /// the set by one: the claim is holding more members at once than it ever
    /// has, which is the one case a set has to grow for — and the reason the
    /// width, not a declared count, is what the routing rule reads.
    fn join(
        &self,
        endpoint: Endpoint,
        queue: mpsc::Sender<Bytes>,
        #[cfg_attr(
            not(feature = "multiplex"),
            expect(unused_variables, reason = "without multiplex there is one carrier")
        )]
        lane: Option<LaneId>,
        carrier: Option<Carrier>,
    ) -> Result<usize> {
        let mut table = self
            .table
            .lock()
            .map_err(|_| anyhow::anyhow!("the TUN routing table is poisoned"))?;
        if let Some(claim) = table.lookup_endpoint_mut(&endpoint) {
            return Ok(claim.place(endpoint, queue, lane, carrier));
        }
        let mut claim = ClaimSlots::new();
        let slot = claim.place(endpoint, queue, lane, carrier);
        table.insert(endpoint, claim);
        Ok(slot)
    }

    /// The wake one of a claim's lanes waits on to hear that it was asked to
    /// end, and that lane's own draining mark. A claim that vanished between the
    /// join and this call leaves a wake nobody will ring and a mark nobody will
    /// set: the lane simply never retires, which is the safe direction.
    fn retire_watch(
        &self,
        endpoint: &Endpoint,
        slot: usize,
    ) -> (Arc<Notify>, Arc<std::sync::atomic::AtomicBool>) {
        self.table
            .lock()
            .ok()
            .and_then(|mut table| {
                table.lookup_endpoint_mut(endpoint).map(|claim| {
                    let flag = claim
                        .slots
                        .get(slot)
                        .map(|entry| Arc::clone(&entry.flag))
                        .unwrap_or_default();
                    (Arc::clone(&claim.retire), flag)
                })
            })
            .unwrap_or_else(|| {
                (
                    Arc::new(Notify::new()),
                    Arc::new(std::sync::atomic::AtomicBool::default()),
                )
            })
    }

    /// Ask one of a claim's lanes to end, and say whether it is idle right now.
    fn retire_lane(&self, endpoint: &Endpoint, slot: usize) -> bool {
        let Ok(mut table) = self.table.lock() else {
            return false;
        };
        table
            .lookup_endpoint_mut(endpoint)
            .is_some_and(|claim| claim.retire(slot))
    }

    /// Stop placing new flows in the lane with this id, and say whether this
    /// end already holds none.
    ///
    /// `None` when this end does not hold that lane (it ended, or it was never
    /// ours): the caller answers the peer with "unknown" rather than pretending
    /// to have drained something. This is the peer's half of a two-sided
    /// retirement — the end that dialed a lane cannot see the flows *this* end
    /// placed on it, so its own emptiness is only half the question.
    fn begin_drain(&self, endpoint: &Endpoint, lane: LaneId) -> Option<bool> {
        let Ok(mut table) = self.table.lock() else {
            return None;
        };
        let claim = table.lookup_endpoint_mut(endpoint)?;
        let slot = claim.slot_of(lane)?;
        Some(claim.peer_drain(slot))
    }

    /// Put a drained lane back in service: the retirement could not be
    /// completed, and a lane nobody may use is worse than the carrier the drain
    /// was trying to leave.
    fn cancel_drain(&self, endpoint: &Endpoint, lane: LaneId) -> Option<()> {
        let Ok(mut table) = self.table.lock() else {
            return None;
        };
        let claim = table.lookup_endpoint_mut(endpoint)?;
        let slot = claim.slot_of(lane)?;
        claim.peer_undrain(slot);
        Some(())
    }

    /// Whether the lane in `slot` was asked to end and has nothing left.
    fn should_retire(&self, endpoint: &Endpoint, slot: usize) -> bool {
        self.table
            .lock()
            .ok()
            .and_then(|mut table| {
                table
                    .lookup_endpoint_mut(endpoint)
                    .map(|claim| claim.should_retire(slot))
            })
            .unwrap_or(false)
    }

    /// Which claim and which of its lanes one packet belongs to.
    ///
    /// The endpoint lookup is the allow-list; a packet whose claimed end names
    /// no registered endpoint belongs to nobody and is counted as unclaimed by
    /// the caller. The lane comes from the claim's **placement table**
    /// ([`ClaimSlots::route`]): a flow is placed once, on its first packet, and
    /// remembered — which is what makes a lane change (a lane joining, a lane
    /// being lent) unable to move a flow that is already talking.
    fn pick(&self, info: &PacketInfo, direction: Direction) -> Option<(Endpoint, usize)> {
        let mut table = self.table.lock().ok()?;
        let endpoint = table.endpoint_for(info, direction)?;
        let slot = table.lookup_endpoint_mut(&endpoint)?.route(info)?;
        Some((endpoint, slot))
    }

    /// Note that one flow was seen arriving from one of the claim's lanes.
    ///
    /// Placement is decided on the packets read *from the device*, but a lane
    /// carries both directions: a flow whose replies keep arriving is not idle,
    /// and forgetting where it lives would put its next outbound packet
    /// somewhere else. Touching the entry on the way in is what makes "idle"
    /// mean "silent in both directions".
    fn touch(&self, endpoint: &Endpoint, info: &PacketInfo) {
        if let Ok(mut table) = self.table.lock()
            && let Some(claim) = table.lookup_endpoint_mut(endpoint)
        {
            claim.touch(info);
        }
    }

    /// Count packets a lane delivered into this end's device, against the lane
    /// that carried them.
    ///
    /// This is the receiving half of a lane's story, and the half its *peer*
    /// cannot see: the peer knows what it wrote to the lane, not what this end
    /// got out of it. Judging a carrier on one half is how the round-3 policy
    /// ended up reading a counter that was zero while the path was losing a
    /// third of its traffic.
    fn count_injected(&self, endpoint: &Endpoint, slot: usize, packets: u64, bytes: u64) {
        if packets == 0 {
            return;
        }
        let Ok(mut table) = self.table.lock() else {
            return;
        };
        let Some(entry) = table
            .lookup_endpoint_mut(endpoint)
            .and_then(|claim| claim.slots.get_mut(slot))
        else {
            return;
        };
        entry.counters.inject(packets, bytes);
    }

    /// Record what the peer reported offering one of this end's lanes, and what
    /// that lane's queue refused there.
    ///
    /// The lookup is by **claim and lane id**: the peer names the connection it
    /// offered traffic to, and this end's slot is its own bookkeeping (it gets
    /// reused). The claim comes from the report's service — its claimed
    /// endpoint — because lane ids are minted per service session and every
    /// claim's first lane is id 1: a hub shared by several claims cannot tell
    /// them apart by id alone. A report for a lane this end no longer holds —
    /// one that retired between the peer's sample and its arrival — is dropped,
    /// which is the safe direction: stale evidence must never be read as the
    /// current lane's.
    fn note_remote(&self, endpoint: Endpoint, lane: LaneId, offered: u32, refused: u32, seq: u8) {
        let Ok(mut table) = self.table.lock() else {
            return;
        };
        let Some(claim) = table.lookup_endpoint_mut(&endpoint) else {
            return;
        };
        for slot in &mut claim.slots {
            if slot.lane.is_some_and(|key| key.id == lane) {
                // A report older than the one already held is not news:
                // the sequence is per lane, so a wrap or a reorder is
                // visible rather than silently applied.
                let fresh = slot
                    .remote
                    .is_none_or(|held| seq.wrapping_sub(held.seq) < 128);
                if fresh {
                    slot.remote = Some(RemoteLane {
                        offered,
                        refused,
                        seq,
                    });
                }
                return;
            }
        }
    }

    /// Test-only: push every flow placed in `slot` back in time, as if it had
    /// been silent for that long.
    ///
    /// The placement table's clock is what "quiet" is measured against, and a
    /// test cannot wait three seconds for it: this is how the quiet window is
    /// exercised without sleeping through it.
    #[cfg(test)]
    fn touch_backdate(&self, endpoint: &Endpoint, slot: usize, ago: std::time::Duration) {
        if let Ok(mut table) = self.table.lock()
            && let Some(claim) = table.lookup_endpoint_mut(endpoint)
        {
            let now = std::time::Instant::now();
            for entry in claim.flows.values_mut() {
                if entry.slot == slot {
                    entry.last_seen = now.checked_sub(ago).unwrap_or(now);
                }
            }
        }
    }

    /// Hand one member's batch to its queue.
    ///
    /// The batch shares one fate: a queue that is full or gone drops all of its
    /// packets, which is the same UDP-like answer the per-packet path gave,
    /// with the counting still done per packet — and, for a member set, per
    /// member, so which member a claim lost its traffic with is legible.
    fn flush(&self, endpoint: &Endpoint, slot: usize, batch: Batch) -> bool {
        let Batch { frames, packets } = batch;
        if packets == 0 {
            return false;
        }
        // The wire's own denominator: what this batch will cost the lane as
        // framed, not the packets' payload alone.
        let bytes = frames.len() as u64;
        let delivered = {
            let Ok(mut table) = self.table.lock() else {
                return false;
            };
            let entry = table
                .lookup_endpoint_mut(endpoint)
                .and_then(|claim| claim.slots.get_mut(slot));
            match entry {
                Some(entry) => {
                    // What the device offered this lane, counted before the
                    // answer: a refusal is as much evidence as an acceptance.
                    entry.counters.offer(packets as u64, bytes);
                    let lane_absent = entry.queue.is_none();
                    let sent = entry
                        .queue
                        .as_ref()
                        .is_some_and(|queue| queue.try_send(frames.freeze()).is_ok());
                    if sent {
                        entry.forwarded += packets as u64;
                        entry.counters.enqueued_packets += packets as u64;
                        entry.counters.enqueued_bytes += bytes;
                    } else {
                        entry.no_channel += packets as u64;
                        // The two refusals are different facts and are counted
                        // apart: a full queue is this end's backpressure, an
                        // absent lane is a replacement window.
                        entry.counters.refuse(packets as u64, bytes, lane_absent);
                    }
                    sent
                }
                None => false,
            }
        };
        if delivered {
            self.stats.count_forwarded_by(packets);
        } else {
            self.stats.count_no_channel_by(packets);
        }
        delivered
    }

    /// Empty a member's slot. The set keeps the slot: the width is the routing
    /// modulus, and a flow that was placed in it must not be moved by the
    /// member's death.
    ///
    /// A claim whose last member went is removed: a claim nobody carries has no
    /// routing to answer, and the packets for it are counted as having no
    /// channel, exactly as the single-channel path counted them when its channel
    /// ended.
    fn vacate(&self, endpoint: &Endpoint, slot: usize) {
        let Ok(mut table) = self.table.lock() else {
            return;
        };
        let Some(claim) = table.lookup_endpoint_mut(endpoint) else {
            return;
        };
        if let Some(entry) = claim.slots.get_mut(slot) {
            entry.queue = None;
            // A lane that is gone is not on its way out any more: the slot is
            // free for whoever takes it next (a replacement, or a lane lent
            // back), and a draining mark left behind would keep every new flow
            // out of it for good.
            entry.draining = false;
            entry.peer_draining = false;
            entry
                .flag
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        // The lane's evidence is archived here, not only on replacement: a
        // claim whose last lane went leaves the table, and that is exactly the
        // shape a one-lane handover takes.
        claim.forget_lane(*endpoint, slot);
        let retired = claim.retired.clone();
        let live = claim.slots.iter().any(|entry| entry.queue.is_some());
        if !live {
            table.remove(endpoint);
            // The claim is gone but the lane's numbers are not: a handover has
            // to be judged on the lane it replaced, and this is the only place
            // that outlives the entry which held it.
            self.archive_recent(retired);
        }
    }

    /// Keep the lanes a claim let go on its way out, bounded and newest last.
    fn archive_recent(&self, lanes: Vec<RetiredLane>) {
        let Ok(mut archive) = self.retired.lock() else {
            return;
        };
        for lane in lanes {
            archive.push(lane);
        }
        let excess = archive.len().saturating_sub(RETIRED_LANES * ARCHIVE_CLAIMS);
        if excess > 0 {
            archive.drain(..excess);
        }
    }

    /// The lanes claims let go after their last lane left, newest last.
    fn retired_lanes(&self) -> Vec<RetiredLane> {
        match self.retired.lock() {
            Ok(archive) => archive.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Every slot of every claim, in a stable order, for the stats line.
    fn member_stats(&self) -> Vec<MemberStat> {
        let Ok(table) = self.table.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (endpoint, claim) in table.iter() {
            for (slot, entry) in claim.slots.iter().enumerate() {
                out.push(MemberStat {
                    endpoint,
                    slot,
                    live: entry.queue.is_some(),
                    draining: entry.draining,
                    peer_draining: entry.peer_draining,
                    forwarded: entry.forwarded,
                    counters: entry.counters,
                    remote: entry.remote,
                    retired: entry.retired.clone(),
                    lane: entry.lane,
                    carrier: entry.carrier,
                    no_channel: entry.no_channel,
                    active_flows: claim.active_flows(slot, DRAIN_QUIET),
                    flows: entry.flows,
                });
            }
        }
        out.sort_by_key(|member| (member.endpoint, member.slot));
        out
    }
}

/// The lane set of one claimed endpoint: a fixed table of slots, and the flows
/// that were placed in them.
///
/// The slots are the routing targets. A slot exists whether or not a lane
/// holds it, which is what lets a replacement inherit its predecessor's index
/// and the claim's flows stay where they were. The flow table is what makes
/// that stay true when the set *changes*: placement happens once, on a flow's
/// first packet, and is remembered — so a lane joining, or a lane being lent to
/// another claim, cannot move a flow that is already talking, and a reordering
/// is not something a lane change has to be careful about.
struct ClaimSlots {
    slots: Vec<MemberSlot>,
    /// One entry per flow that is talking, keyed by its identity
    /// ([`flow::key`]).
    flows: HashMap<u64, FlowEntry>,
    /// Every lane this claim has let go, newest last, with what it carried.
    ///
    /// Kept on the **claim**, not only on the slot it held, for one reason: a
    /// claim whose last lane went is removed from the routing table, and that
    /// is exactly the shape a handover takes when a claim holds one lane — the
    /// old lane is retired, the slot is empty for a moment, and the evidence of
    /// how the old carrier did has to outlive the entry that held it.
    retired: Vec<RetiredLane>,
    /// How many lanes this claim has placed. The generation half of a
    /// [`LaneKey`]: a lane id is unique per session, but a slot is reused, and
    /// this is what keeps a replacement from being read as its predecessor.
    generation: u32,
    /// When the idle sweep last ran, so the table does not walk itself on every
    /// packet.
    swept: std::time::Instant,
    /// Woken when a lane of this claim is asked to end, so the task that owns
    /// it notices without polling (see [`MemberGuard::retired`]).
    retire: Arc<Notify>,
}

/// One lane's place in a claim's set.
struct MemberSlot {
    /// The queue of the lane that holds this slot, while one does.
    queue: Option<mpsc::Sender<Bytes>>,
    /// Set when the claim's owner has asked this lane to end: new flows are
    /// placed elsewhere, and the lane's task stops once the flows it already
    /// has are gone (`flows` reaches zero).
    draining: bool,
    /// Set when the **peer** asked this lane to stop taking new flows.
    ///
    /// Deliberately not `draining`: a peer-initiated drain must not let this
    /// end's lane task exit. The dialer retires the lane when *both* ends hold
    /// nothing in it, and an end that tore the connection down on its own would
    /// leave the dialer with a dead lane instead of an answer — the black hole
    /// from the other side. So this flag stops placement and nothing else: this
    /// end reports readiness, and the dialer decides.
    peer_draining: bool,
    /// The same mark, readable without the routing table's lock.
    ///
    /// The lane's own task asks "was I asked to end?" on **every** packet it
    /// relays, and reaching the table's `draining` field would mean taking the
    /// table's mutex there — the same mutex every other lane's placement path
    /// takes. The flag is the hot path's copy: an atomic load that is false
    /// almost always, and only when it is true does the task go and ask the
    /// table the rest of the question (is the lane empty yet?). Measured: the
    /// lock in that position cost ~5 % of a bulk claim's throughput.
    flag: Arc<std::sync::atomic::AtomicBool>,
    /// What the lane currently holding this slot carried.
    counters: LaneCounters,
    /// What the peer last reported offering this lane, and refusing it.
    remote: Option<RemoteLane>,
    /// What this slot's *previous* lanes carried, newest last.
    ///
    /// Mirrored on the claim ([`ClaimSlots::retired`]) because the interesting
    /// lane is often the one that just left the **last** slot, and a claim
    /// whose last lane went is removed from the table outright — which would
    /// take the only evidence of how that lane's carrier did with it.
    retired: Vec<RetiredLane>,
    /// The lane currently holding this slot, and the carrier it rides.
    ///
    /// Both are the *lane's*, not the slot's: a slot outlives the connections
    /// that hold it (a replacement inherits the index), so anything recorded
    /// here has to be keyed by the lane or a handover in progress disappears
    /// from the policy's view the moment the slot changes tenant. Named by the
    /// end that dialed the lane (the client, which chose the carrier); the
    /// receiving end knows neither and records `None`.
    lane: Option<LaneKey>,
    carrier: Option<Carrier>,
    /// Packets this slot handed to its lane.
    forwarded: u64,
    /// Packets placed here while the slot was empty.
    no_channel: u64,
    /// Flows the table remembers for this slot.
    flows: usize,
}

/// Where one flow was placed, and when it was last seen.
struct FlowEntry {
    slot: usize,
    last_seen: std::time::Instant,
}

impl Default for MemberSlot {
    fn default() -> Self {
        Self {
            queue: None,
            draining: false,
            peer_draining: false,
            flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            counters: LaneCounters::default(),
            remote: None,
            retired: Vec::new(),
            lane: None,
            carrier: None,
            forwarded: 0,
            no_channel: 0,
            flows: 0,
        }
    }
}

impl ClaimSlots {
    /// A set with the one slot every claim starts with.
    fn new() -> Self {
        Self {
            slots: vec![MemberSlot::default()],
            flows: HashMap::new(),
            retired: Vec::new(),
            generation: 0,
            swept: std::time::Instant::now(),
            retire: Arc::new(Notify::new()),
        }
    }

    /// Take the lowest empty slot, widening the set by one when every slot is
    /// taken. Returns the slot's index.
    fn place(
        &mut self,
        endpoint: Endpoint,
        queue: mpsc::Sender<Bytes>,
        lane: Option<LaneId>,
        carrier: Option<Carrier>,
    ) -> usize {
        // The generation is *this* end's: the dialer names the lane, but only
        // the end that owns the slot knows how many lanes have held it, and
        // two connections must never be able to share one identity here.
        let lane = lane.map(|id| {
            self.generation = self.generation.wrapping_add(1);
            LaneKey {
                id,
                generation: self.generation,
            }
        });
        let slot = if let Some(slot) = self.slots.iter().position(|slot| slot.queue.is_none()) {
            slot
        } else {
            self.slots.push(MemberSlot::default());
            self.slots.len() - 1
        };
        if let Some(entry) = self.slots.get_mut(slot) {
            // A lane that held this slot is gone (or never was): its counters
            // are the only evidence of how the carrier it rode did, and a
            // handover is judged against exactly that.
            if let Some(previous) = entry.lane.take() {
                let retired = RetiredLane {
                    endpoint,
                    lane: previous,
                    counters: entry.counters,
                };
                entry.counters = LaneCounters::default();
                // The peer's numbers are the *lane's* too: a replacement must
                // not inherit what its predecessor was told.
                entry.remote = None;
                entry.retired.push(retired);
                if entry.retired.len() > RETIRED_LANES {
                    entry.retired.remove(0);
                }
            }
            entry.queue = Some(queue);
            entry.lane = lane;
            entry.carrier = carrier;
        }
        slot
    }

    /// Remember a lane this claim has let go, with what it carried.
    ///
    /// Called when a lane's slot is emptied, not only when it is replaced: the
    /// replacement path (`place`) cannot see a lane that left the *last* slot,
    /// because the claim is dropped from the table when that happens.
    fn forget_lane(&mut self, endpoint: Endpoint, slot: usize) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        let Some(previous) = entry.lane.take() else {
            return;
        };
        let retired = RetiredLane {
            endpoint,
            lane: previous,
            counters: entry.counters,
        };
        entry.counters = LaneCounters::default();
        entry.retired.push(retired);
        if entry.retired.len() > RETIRED_LANES {
            entry.retired.remove(0);
        }
        self.retired.push(retired);
        if self.retired.len() > RETIRED_LANES {
            self.retired.remove(0);
        }
    }

    /// The slots a **new** flow may be placed in: those that hold a lane and
    /// are not on their way out.
    ///
    /// A draining lane keeps the flows it already has — they finish there — but
    /// takes no more, which is what makes its flow count fall to zero by itself
    /// and its end safe to wait for.
    fn live_slots(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.queue.is_some() && !slot.draining && !slot.peer_draining)
            .map(|(slot, _)| slot)
            .collect()
    }

    /// Ask the lane in `slot` to end, and say whether it is idle right now.
    ///
    /// The mark goes on first: from here on no new flow can be placed in the
    /// slot (see [`Self::live_slots`]), so its flow count can only fall — the
    /// idle sweep and the flows' own silence do the rest. Waking the claim's
    /// lane tasks is what lets the one holding this slot finish without waiting
    /// for its next packet.
    fn retire(&mut self, slot: usize) -> bool {
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.draining = true;
            entry.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.retire.notify_waiters();
        self.slots.get(slot).is_some_and(|entry| entry.flows == 0)
    }

    /// How many flows placed in `slot` have carried a packet within `window`.
    ///
    /// The flow table's `last_seen` is what makes "idle" mean "silent in both
    /// directions"; this is the same fact asked as a question about a lane.
    fn active_flows(&self, slot: usize, window: std::time::Duration) -> usize {
        let now = std::time::Instant::now();
        self.flows
            .values()
            .filter(|entry| entry.slot == slot && now.duration_since(entry.last_seen) < window)
            .count()
    }

    /// Whether this end is holding nothing that is still talking in `slot`:
    /// what a peer's drain waits for (see [`DRAIN_QUIET`]).
    fn drain_ready(&self, slot: usize) -> bool {
        self.active_flows(slot, DRAIN_QUIET) == 0
    }

    /// Stop placing new flows in a lane the **peer** wants drained, and say
    /// whether this end already holds none of its own.
    ///
    /// No wake-up on purpose: the lane's task must keep running (see
    /// [`MemberSlot::peer_draining`]) — the dialer retires the lane once both
    /// ends are empty, and this end's only job is to stop feeding it and say so.
    fn peer_drain(&mut self, slot: usize) -> bool {
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.peer_draining = true;
        }
        self.slots.get(slot).is_some() && self.drain_ready(slot)
    }

    /// Put a peer-drained lane back in service.
    fn peer_undrain(&mut self, slot: usize) {
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.peer_draining = false;
        }
    }

    /// Which slot the lane with this id holds, if any.
    ///
    /// By id, not by index: the peer names the connection it wants drained, and
    /// this end's slot is its own bookkeeping (a replacement inherits it).
    fn slot_of(&self, lane: LaneId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.lane.is_some_and(|key| key.id == lane))
    }

    /// Whether the lane in `slot` was asked to end and has nothing left to
    /// carry.
    ///
    /// "Nothing left" is the **quiet window** ([`DRAIN_QUIET`]), not an empty
    /// flow table: the table keeps a flow for [`FLOW_IDLE_EVICTION`] (60 s)
    /// after its last packet, and a carrier move that waited for the table to
    /// drain would hang a handover for a minute of silence — measured: the
    /// ladder's flows held a draining lane for the whole measured round, the
    /// replacement never joined, and the cells read level because nothing had
    /// moved. The peer's drain already answers the same question this way
    /// ([`Self::drain_ready`]); the lane's own end now does too.
    fn should_retire(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|entry| entry.draining && self.active_flows(slot, DRAIN_QUIET) == 0)
    }

    /// Which lane one packet belongs to, remembering a new flow's placement.
    ///
    /// Three cases, in this order:
    ///
    /// - **the flow is in the table**: it keeps its slot, whatever that slot's
    ///   state is. A lane whose member died keeps the flows placed in it, so the
    ///   replacement inherits them and nothing is reordered (the documented
    ///   answer, and the reason the entry is not repointed);
    /// - **the flow is new**: the least-loaded live slot, ties broken by the
    ///   flow hash — the hash is what lets the two ends of a claim agree about a
    ///   tie without a word on the wire;
    /// - **the table is full of flows that are still talking**: the hash alone
    ///   decides. It is a pure function of the packet and the live set, so such
    ///   a flow still keeps one lane for as long as the set's width does.
    fn route(&mut self, info: &PacketInfo) -> Option<usize> {
        let key = flow::key(info);
        let now = std::time::Instant::now();
        if let Some(entry) = self.flows.get_mut(&key) {
            entry.last_seen = now;
            return Some(entry.slot);
        }
        let live = self.live_slots();
        self.sweep_if_due(now);
        let chosen = if self.flows.len() >= MAX_TRACKED_FLOWS {
            *live.get(flow::slot(info, live.len()))?
        } else {
            let least = live
                .iter()
                .filter_map(|slot| self.slots.get(*slot))
                .map(|slot| slot.flows)
                .min()?;
            let candidates: Vec<usize> = live
                .iter()
                .copied()
                .filter(|slot| {
                    self.slots
                        .get(*slot)
                        .is_some_and(|entry| entry.flows == least)
                })
                .collect();
            *candidates.get(flow::slot(info, candidates.len()))?
        };
        if self.flows.len() < MAX_TRACKED_FLOWS {
            self.flows.insert(
                key,
                FlowEntry {
                    slot: chosen,
                    last_seen: now,
                },
            );
            if let Some(entry) = self.slots.get_mut(chosen) {
                entry.flows += 1;
            }
        }
        Some(chosen)
    }

    /// Note that one flow was seen on its way *in*.
    ///
    /// Placement is decided on the packets this end reads from its device, but
    /// a lane carries both directions: a flow whose replies keep arriving is not
    /// idle, and forgetting it would put its next outbound packet somewhere
    /// else. The slot is not consulted — each end places for its own direction,
    /// so the lane the reply arrived on is not necessarily the lane this end
    /// chose for it, and the only thing this call has to say is "still talking".
    /// A flow the table does not know is left alone: the other end has an entry
    /// of its own, and inventing one here would put this end's placement under
    /// the other end's choice.
    fn touch(&mut self, info: &PacketInfo) {
        let key = flow::key(info);
        if let Some(entry) = self.flows.get_mut(&key) {
            entry.last_seen = std::time::Instant::now();
        }
    }

    /// Forget the flows that have been silent in both directions for
    /// [`FLOW_IDLE_EVICTION`], at most once a second.
    fn sweep_if_due(&mut self, now: std::time::Instant) {
        if now.duration_since(self.swept) < std::time::Duration::from_secs(1) {
            return;
        }
        self.swept = now;
        let stale: Vec<(u64, usize)> = self
            .flows
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_seen) >= FLOW_IDLE_EVICTION)
            .map(|(key, entry)| (*key, entry.slot))
            .collect();
        for (key, slot) in stale {
            self.flows.remove(&key);
            if let Some(entry) = self.slots.get_mut(slot) {
                entry.flows = entry.flows.saturating_sub(1);
            }
        }
    }
}

/// A run of frames waiting for one endpoint, built while the device is drained.
///
/// One buffer per (endpoint, member), not per packet: `frames` grows into a
/// single `Bytes` (a `BytesMut` freeze is a move, not a copy), so nothing is
/// allocated per packet on this path.
#[derive(Default)]
struct Batch {
    frames: BytesMut,
    packets: usize,
}

impl Batch {
    fn push(&mut self, packet: &[u8]) -> Result<()> {
        IpTraffic::encode_into(&mut self.frames, packet)?;
        self.packets += 1;
        Ok(())
    }

    /// Whether this batch has reached the size one message may grow to.
    ///
    /// Whichever cap is hit first: bytes keep a bulk flow from building a huge
    /// message, packets keep a small-packet flow from doing the same by count.
    fn is_full(&self) -> bool {
        self.packets >= BATCH_PACKETS || self.frames.len() >= BATCH_BYTES
    }
}

/// Forward one transparent data channel until either end stops.
///
/// The channel is a stream of `[u16 length][packet]` frames in both
/// directions; everything on it for `endpoint` is injected into the local
/// kernel, and everything the kernel routes to that endpoint is framed onto
/// whichever of the claim's members the packet's flow is placed on. This
/// channel becomes one *member* of the claim while it runs — it takes a slot of
/// the set, and its own frames are the ones it reads back. The function returns
/// when the channel or the device ends — the caller decides whether to ask for
/// another channel, and the set keeps the slot the channel held so its
/// replacement inherits it.
pub async fn forward_transparent<T>(
    conn: T,
    hub: Arc<TunHub>,
    endpoint: Endpoint,
    lane: Option<LaneId>,
    carrier: Option<Carrier>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stats = hub.stats();
    let (reader, mut writer) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    let guard = hub.join(endpoint, tx, lane, carrier)?;
    let slot = guard.slot;

    // The pump is one `write_all` per batch: the reader already framed every
    // packet, so a batch of N packets costs one syscall and one carrier
    // segment run instead of N of each.
    let pump = tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            if writer.write_all(&batch).await.is_err() {
                break;
            }
        }
    });

    // The receive half is batched the same way the send half is: one socket
    // read carries a run of frames, so the inject loop walks a burst of packets
    // without awaiting between them (and the far side writes them that way).
    let mut frames = IpFrames::new(reader);
    // The wait is built **once**, outside the loop: `tokio::select!` evaluates
    // its futures on every iteration, so constructing it inside would rebuild
    // the wait — and re-take the hub's lock — once per relayed packet.
    let retired = guard.retired_owned();
    tokio::pin!(retired);
    let result = loop {
        tokio::select! {
            // The claim's owner asked this lane to end (its place is wanted
            // elsewhere). Everything already in flight on it has been
            // delivered: the loop reaches here between packets, and the hub
            // stopped placing new flows in this slot when the ask was made.
            () = &mut retired => {
                tracing::info!("Transparent lane {slot} for {endpoint} ended on request");
                break Ok(());
            }
            frame = frames.next() => match frame {
                Ok(packet) => {
                    // Defence in depth: a peer must not be able to steer traffic
                    // into a local address this service did not claim. Which end
                    // of the packet names the claim depends on the side: the
                    // client is the destination, the server is the source of the
                    // traffic it hands back.
                    let Some(info) = claimed_info(&endpoint, packet, hub.direction()) else {
                        stats.count_drop(DropReason::Unclaimed);
                        continue;
                    };
                    hub.touch(&endpoint, &info);
                    if let Err(e) = hub.inject(packet).await {
                        break Err(anyhow::Error::from(e))
                            .with_context(|| format!("Failed to inject a packet for {endpoint}"));
                    }
                    // The one "delivery" in this module: the packet crossed
                    // the lane and reached this end's kernel. Counted against
                    // the lane that carried it, because that is the number the
                    // *other* end cannot see and the policy needs.
                    hub.count_injected(&endpoint, slot, 1, packet.len() as u64);
                }
                Err(e) => {
                    stats.count_channel_error();
                    break Err(e)
                        .with_context(|| format!("Transparent channel for {endpoint} ended"));
                }
            },
        }
    };
    drop(guard);
    pump.abort();
    result
}

/// The parsed packet, when it is one this service claimed.
///
/// The claim lives at the end this side owns: the client injected a visitor's
/// packet for the address it carries, so it matches the *destination*; the
/// server receives that connection's replies, so it matches the *source*. A
/// portless packet (ICMP, and the fragments after the first) matches on the
/// address alone.
///
/// The parse is handed back rather than thrown away: the caller needs the
/// packet's flow identity to keep its placement entry alive (see
/// [`Routes::touch`]), and parsing twice per packet to get it would be a tax on
/// every packet of the path.
fn claimed_info(endpoint: &Endpoint, packet: &[u8], direction: Direction) -> Option<PacketInfo> {
    let info = ip::parse(packet).ok()?;
    let (addr, port) = match direction {
        Direction::Source => (info.dst, info.dst_port),
        Direction::Destination => (info.src, info.src_port),
    };
    (addr == endpoint.ip && port.is_none_or(|port| port == endpoint.port)).then_some(info)
}

/// One `INFO` line per second per process while `MOLEHILL_L3_STATS=1`.
///
/// Cumulative counters, like every other `MOLEHILL_*_STATS` switch: rates come
/// from consecutive lines. The per-member lines that follow the totals are the
/// member set's own view — which slot carried how much, which ones are held,
/// and what a member's absence cost — because a claim's aggregate counters
/// cannot say whether its set is spread or stacked.
fn spawn_stats_reporter(hub: Arc<TunHub>) {
    if !Stats::enabled() {
        return;
    }
    let stats = hub.stats();
    let role = match hub.direction() {
        Direction::Source => "client",
        Direction::Destination => "server",
    };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            tracing::info!(
                target: "molehill::transparent",
                "l3-stats: role={role} forwarded={} dropped(not_ipv4={} malformed={} \
                 unclaimed={} no_channel={}) channel_errors={}",
                stats.forwarded.load(std::sync::atomic::Ordering::Relaxed),
                stats.dropped_not_ipv4.load(std::sync::atomic::Ordering::Relaxed),
                stats.dropped_malformed.load(std::sync::atomic::Ordering::Relaxed),
                stats.dropped_unclaimed.load(std::sync::atomic::Ordering::Relaxed),
                stats
                    .dropped_no_channel
                    .load(std::sync::atomic::Ordering::Relaxed),
                stats.channel_errors.load(std::sync::atomic::Ordering::Relaxed),
            );
            // A lane that left a claim which then left the table: the one-lane
            // handover shape, whose evidence lives nowhere else.
            for gone in hub.retired_lanes() {
                tracing::info!(
                    target: "molehill::transparent",
                    "l3-stats: role={role} gone claim={} lane={} offered={} enqueued={} \\
                     overflow={} disconnected={} injected={}",
                    gone.endpoint,
                    gone.lane.id,
                    gone.counters.offered_packets,
                    gone.counters.enqueued_packets,
                    gone.counters.overflow_packets,
                    gone.counters.disconnected_packets,
                    gone.counters.injected_packets,
                );
            }
            for member in hub.member_stats() {
                tracing::info!(
                    target: "molehill::transparent",
                    "l3-stats: role={role} claim={} member={} live={} lane={} carrier={} flows={} \
                     forwarded={} no_channel={} offered={} enqueued={} overflow={} \
                     disconnected={} injected={} peer_offered={} peer_refused={} \
                     draining={} peer_draining={}",
                    member.endpoint,
                    member.slot,
                    member.live,
                    member
                        .lane
                        .map_or_else(|| "-".to_owned(), |lane| lane.id.to_string()),
                    // Which carrier the lane actually rides: the fact a policy
                    // compares, and the one a slot cannot be asked for (a slot
                    // outlives the connection that held it).
                    member.carrier.map_or("-", Carrier::as_str),
                    member.flows,
                    member.forwarded,
                    member.no_channel,
                    member.counters.offered_packets,
                    member.counters.enqueued_packets,
                    member.counters.overflow_packets,
                    member.counters.disconnected_packets,
                    member.counters.injected_packets,
                    // What the *peer* says it offered this lane and what this
                    // lane's queue refused there: the half of the story this
                    // end cannot see, and the one a carrier has to be judged
                    // on when the loss is at the offering end.
                    member.remote.map_or(0, |remote| remote.offered),
                    member.remote.map_or(0, |remote| remote.refused),
                    // Two different facts: this end retiring the lane, and the
                    // peer asking it to stop taking new flows (which this end
                    // answers rather than acts on).
                    member.draining,
                    member.peer_draining,
                );
                // A lane that no longer holds the slot, and what it carried:
                // a handover is judged against the lane it replaced, and that
                // lane is invisible once the slot has a new tenant.
                for gone in &member.retired {
                    tracing::info!(
                        target: "molehill::transparent",
                        "l3-stats: role={role} retired claim={} lane={} offered={} \
                         enqueued={} overflow={} disconnected={} injected={}",
                        gone.endpoint,
                        gone.lane.id,
                        gone.counters.offered_packets,
                        gone.counters.enqueued_packets,
                        gone.counters.overflow_packets,
                        gone.counters.disconnected_packets,
                        gone.counters.injected_packets,
                    );
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use crate::transparent::ip::PacketInfo;
    use std::net::{IpAddr, Ipv4Addr};

    /// One IPv4/TCP packet with `payload` bytes after its headers, and a total
    /// length that agrees with the bytes it carries — which is what makes it
    /// parseable, and therefore routable.
    fn tcp_packet(payload: usize) -> Vec<u8> {
        let mut packet = vec![0x45, 0x00, 0x00, 0x00, 0, 0, 0, 0, 64, 6, 0, 0, 10, 0, 0, 1];
        packet.extend_from_slice(&[10, 0, 0, 2]);
        packet.extend_from_slice(&[0x1f, 0x90, 0x20, 0x00]); // ports
        packet.resize(20 + 20 + payload, 0);
        let total = u16::try_from(packet.len()).unwrap();
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet
    }

    /// The claim this module's tests carry: one address, one port.
    fn claim() -> Endpoint {
        Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 8443)
    }

    /// The packet the `claim` endpoint is recognised by on the server side:
    /// its destination address and port are the claimed ones.
    fn packet_for_claim() -> Vec<u8> {
        let mut packet = tcp_packet(4);
        packet[16..20].copy_from_slice(&[10, 99, 0, 1]);
        packet[22..24].copy_from_slice(&8443u16.to_be_bytes());
        packet
    }

    /// The claim's packet with a given visitor source port: two of these are
    /// two flows of the same claim.
    fn packet_on_port(port: u16) -> Vec<u8> {
        let mut packet = packet_for_claim();
        packet[20..22].copy_from_slice(&port.to_be_bytes());
        packet
    }

    /// A full queue and a missing lane are **different** refusals.
    ///
    /// This is the distinction the round-3 policy did not have: it read
    /// `no_channel` as "the carrier lost this packet", while that counter sums
    /// a full batch queue (this end's own backpressure, which says nothing
    /// about the path) and a slot with no lane in it (a replacement window,
    /// which is this end's own scheduling). Counting them apart is what makes
    /// the rest of the evidence usable at all.
    #[test]
    fn a_refusal_is_counted_as_its_cause_not_as_loss() {
        let routes = Routes::new();
        let web = claim();
        // A queue of one: the second batch cannot be accepted while the first
        // is still in it.
        let (queue, held) = mpsc::channel::<Bytes>(1);
        let slot = routes.join(web, queue, None, None).unwrap();

        let batch = |packet: &[u8]| {
            let mut frames = bytes::BytesMut::new();
            crate::protocol::IpTraffic::encode_into(&mut frames, packet).unwrap();
            Batch { frames, packets: 1 }
        };

        // One offer, accepted: the queue has room.
        assert!(routes.flush(&web, slot, batch(&packet_for_claim())));
        // A second and third, refused — and the lane is still there, so this
        // is the queue, not an absent lane.
        assert!(!routes.flush(&web, slot, batch(&packet_for_claim())));
        assert!(!routes.flush(&web, slot, batch(&packet_for_claim())));

        let stats = routes.member_stats();
        let entry = &stats[slot];
        assert_eq!(entry.counters.offered_packets, 3, "every offer is counted");
        assert_eq!(entry.counters.enqueued_packets, 1, "one was accepted");
        assert_eq!(
            entry.counters.overflow_packets, 2,
            "a full queue is this end's backpressure"
        );
        assert_eq!(
            entry.counters.disconnected_packets, 0,
            "the lane is holding the slot; nothing was disconnected"
        );
        // The drop is still a drop for the legacy counter, so nothing that
        // reads it changes meaning — it just no longer has to carry a cause.
        assert_eq!(entry.no_channel, 2);
        drop(held);
    }

    /// A lane that is gone is counted as `disconnected`, and its counters
    /// survive it: a handover has to be judged against the lane it replaced,
    /// which is invisible the moment the slot has a new tenant.
    #[test]
    fn a_missing_lane_is_counted_apart_and_remembered_when_replaced() {
        let routes = Routes::new();
        let web = claim();
        let (first, held) = mpsc::channel::<Bytes>(4);
        let slot = routes.join(web, first, Some(LaneId::new(7)), None).unwrap();

        let mut frames = bytes::BytesMut::new();
        crate::protocol::IpTraffic::encode_into(&mut frames, &packet_for_claim()).unwrap();
        routes.flush(&web, slot, Batch { frames, packets: 1 });
        drop(held);

        // The lane is gone: vacate it, then offer the slot more traffic.
        routes.vacate(&web, slot);
        let mut frames = bytes::BytesMut::new();
        crate::protocol::IpTraffic::encode_into(&mut frames, &packet_for_claim()).unwrap();
        routes.flush(&web, slot, Batch { frames, packets: 1 });

        // The claim left the table when its only lane went — so the gone lane's
        // evidence has to be read from the archive. That is the whole point of
        // it: a handover on a one-lane claim retires the only lane and empties
        // the table in the same instant.
        let archived = routes.retired_lanes();
        assert_eq!(archived.len(), 1, "the gone lane is remembered");
        assert_eq!(archived[0].lane.id, LaneId::new(7));
        assert_eq!(
            archived[0].counters.enqueued_packets, 1,
            "with what it carried, which is what the handover is judged on"
        );

        // A replacement takes a slot of its own: it starts from nothing, so
        // two lanes of one claim cannot lend each other's evidence.
        let (second, _held2) = mpsc::channel::<Bytes>(4);
        let slot = routes
            .join(web, second, Some(LaneId::new(8)), None)
            .unwrap();
        let after = routes.member_stats()[slot].clone();
        assert_eq!(after.lane.map(|lane| lane.id), Some(LaneId::new(8)));
        assert_eq!(
            after.counters.enqueued_packets, 0,
            "a new lane is judged on its own traffic"
        );
        assert!(
            after
                .retired
                .iter()
                .all(|gone| gone.lane.id != LaneId::new(8)),
            "and it is not credited with the lane it replaced"
        );
    }

    /// A report is matched to its **claim**, then its lane.
    ///
    /// The defect this pins: every claim mints its lane ids from 1 in one hub
    /// shared by all of them, so a report keyed by lane id alone matched the
    /// first claim holding that number — measured on the bench, the ladder
    /// claim's report read as an idle sibling's `peer_offered`, and the carrier
    /// policy was judging evidence attached to the wrong claim.
    #[test]
    fn a_report_lands_on_the_claim_that_owns_the_lane() {
        let routes = Routes::new();
        let loaded = claim();
        let idle = Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9999);
        let (q1, held1) = mpsc::channel::<Bytes>(4);
        let (q2, held2) = mpsc::channel::<Bytes>(4);
        // Both claims' first lanes are id 1 — that is the collision.
        routes.join(loaded, q1, Some(LaneId::new(1)), None).unwrap();
        routes.join(idle, q2, Some(LaneId::new(1)), None).unwrap();

        // The peer reports what it offered the LOADED claim's lane.
        routes.note_remote(loaded, LaneId::new(1), 1_549_173, 637_950, 1);

        // `join`'s return is the slot index *within the claim*; member_stats is
        // a flat vector over every claim, so the entries are found by endpoint.
        let stat_of = |stats: &[MemberStat], endpoint: Endpoint| {
            stats
                .iter()
                .find(|member| member.endpoint == endpoint)
                .unwrap()
                .clone()
        };
        let stats = routes.member_stats();
        let loaded_stat = stat_of(&stats, loaded);
        assert_eq!(
            loaded_stat.remote.map(|remote| remote.offered),
            Some(1_549_173),
            "the report lands on the claim the service named"
        );
        assert_eq!(
            loaded_stat.remote.map(|remote| remote.refused),
            Some(637_950),
            "with both halves of the pair"
        );
        assert_eq!(
            stat_of(&stats, idle).remote,
            None,
            "the sibling holding the same lane id gets nothing"
        );

        // And a report for the sibling's own lane goes to it, not back to the
        // loaded one.
        routes.note_remote(idle, LaneId::new(1), 10, 0, 1);
        let stats = routes.member_stats();
        assert_eq!(
            stat_of(&stats, idle).remote.map(|remote| remote.offered),
            Some(10)
        );
        assert_eq!(
            stat_of(&stats, loaded).remote.map(|remote| remote.offered),
            Some(1_549_173),
            "the first report is untouched"
        );
        drop(held1);
        drop(held2);
    }

    /// A drain is asked for **by lane**, and it answers whether this end still
    /// holds a flow in it — the half of a two-sided retirement the dialer cannot
    /// see.
    ///
    /// The distinction the test pins: `Some(false)` (draining, flows remain) and
    /// `None` (not this end's lane) are different answers, and the caller tells
    /// the peer which one it got. A cancel puts the lane back in service, so a
    /// drain that cannot be completed does not cost the claim a lane.
    #[test]
    fn a_drain_is_asked_by_lane_and_answers_whether_this_end_is_empty() {
        let routes = Routes::new();
        let web = claim();
        let (queue, _held) = mpsc::channel::<Bytes>(4);
        let slot = routes
            .join(web, queue, Some(LaneId::new(11)), None)
            .unwrap();

        // A lane this end does not hold: the peer is told so rather than being
        // told the lane is empty.
        assert_eq!(routes.begin_drain(&web, LaneId::new(99)), None);
        assert_eq!(routes.cancel_drain(&web, LaneId::new(99)), None);

        // The lane is held and no flow is placed in it yet: draining it is
        // immediately ready — and it is the *peer's* mark, not the local one.
        // That distinction is the whole fix for the tear-down race: if a peer's
        // ask set `draining`, this end's lane task would exit on its own and the
        // peer would see a dead connection instead of an answer.
        assert_eq!(routes.begin_drain(&web, LaneId::new(11)), Some(true));
        assert!(routes.member_stats()[slot].peer_draining);
        assert!(
            !routes.member_stats()[slot].draining,
            "a peer's ask must not retire this end's lane"
        );

        // Put it back in service: the mark goes away, and the lane takes new
        // flows again.
        assert_eq!(routes.cancel_drain(&web, LaneId::new(11)), Some(()));
        assert!(!routes.member_stats()[slot].peer_draining);

        // A flow placed in the lane makes a second drain wait for it: the answer
        // is "draining, not empty", which is what the peer needs to hear.
        let info = ip::parse(&packet_on_port(40_000)).unwrap();
        assert_eq!(
            routes.pick(&info, Direction::Destination),
            Some((web, slot))
        );
        assert_eq!(routes.begin_drain(&web, LaneId::new(11)), Some(false));

        // While the peer's mark is on, a *new* flow is placed elsewhere — the
        // point of the mark — but the flow already there keeps its slot.
        let other = ip::parse(&packet_on_port(40_002)).unwrap();
        assert_ne!(
            routes.pick(&other, Direction::Destination),
            Some((web, slot)),
            "a peer-draining lane takes no new flows"
        );
    }

    /// **A remembered flow is not a talking one.**
    ///
    /// The placement table keeps a flow for a minute after its last packet,
    /// which is what makes a live flow's lane stable — but a peer's drain waits
    /// for a lane that is *quiet*, not for the table to forget. Waiting for
    /// emptiness is what stalled every handover measured before this: the
    /// server answered `pending` for a flow that had already stopped, the
    /// dialer's budget ran out, and the drain was called off.
    #[test]
    fn a_lane_whose_flows_went_quiet_is_drain_ready() {
        let routes = Routes::new();
        let web = claim();
        let (queue, _held) = mpsc::channel::<Bytes>(4);
        let slot = routes
            .join(web, queue, Some(LaneId::new(21)), None)
            .unwrap();

        // A flow is placed in the lane and is talking right now: a drain has to
        // wait for it.
        let info = ip::parse(&packet_on_port(40_000)).unwrap();
        assert_eq!(
            routes.pick(&info, Direction::Destination),
            Some((web, slot))
        );
        assert_eq!(
            routes.begin_drain(&web, LaneId::new(21)),
            Some(false),
            "a flow that just carried a packet is not quiet"
        );
        let stats = routes.member_stats();
        assert_eq!(stats[slot].flows, 1, "the table remembers it");
        assert_eq!(stats[slot].active_flows, 1, "and it is still talking");

        // The same flow, silent past the quiet window: the table still
        // remembers it, and the lane is ready all the same.
        routes.touch_backdate(&web, slot, DRAIN_QUIET + std::time::Duration::from_secs(1));
        assert_eq!(
            routes.begin_drain(&web, LaneId::new(21)),
            Some(true),
            "a flow that stopped talking must not hold a handover for a minute"
        );
        let stats = routes.member_stats();
        assert_eq!(stats[slot].flows, 1, "still remembered by the table");
        assert_eq!(stats[slot].active_flows, 0, "but no longer talking");
    }

    /// A claim's flows ride different members of its set, and each flow keeps
    /// the member it was placed on: the spread is what the set exists for, and
    /// the stability is what keeps a flow's packets in order.
    #[test]
    fn a_claims_flows_are_spread_over_its_members_and_stay_there() {
        let routes = Routes::new();
        let web = claim();
        let (first, mut rx_first) = mpsc::channel(4);
        let (second, mut rx_second) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        routes.join(web, second, None, None).unwrap();
        assert_eq!(
            routes.member_stats().len(),
            2,
            "the set is two members wide"
        );

        // Two visitor ports the placement sends to different members: the hash
        // decides, so the test asks it instead of assuming.
        let mut different: Option<(u16, u16, usize, usize)> = None;
        let mut found: Option<(u16, usize)> = None;
        for port in 40_000..41_000u16 {
            let info = ip::parse(&packet_on_port(port)).unwrap();
            let slot = flow::slot(&info, 2);
            match found {
                Some((seen, seen_slot)) if seen_slot != slot => {
                    different = Some((seen, port, seen_slot, slot));
                    break;
                }
                None => found = Some((port, slot)),
                _ => {}
            }
        }
        let (port_a, port_b, slot_a, slot_b) = different.unwrap_or((40_000, 40_001, 0, 1));
        assert_ne!(slot_a, slot_b, "the two flows were picked to differ");

        // Each flow's frames reach the queue of the member it was placed in —
        // and nothing else does. Slot 0 is the first member that joined, slot 1
        // the second.
        let flows = [
            (slot_a, packet_on_port(port_a)),
            (slot_b, packet_on_port(port_b)),
        ];
        for (slot, packet) in &flows {
            let info = ip::parse(packet).unwrap();
            let (endpoint, placed) = routes.pick(&info, Direction::Destination).unwrap();
            assert_eq!(placed, *slot, "a flow keeps the member it was placed on");
            let mut batch = Batch::default();
            batch.push(packet).unwrap();
            assert!(routes.flush(&endpoint, placed, batch));
        }
        for (slot, queue) in [(0usize, &mut rx_first), (1usize, &mut rx_second)] {
            match flows.iter().find(|(placed, _)| *placed == slot) {
                Some((_, packet)) => {
                    let frames = queue.try_recv().unwrap();
                    assert_eq!(
                        &frames[2..],
                        &packet[..],
                        "member {slot} carried the flow placed in it, and only that"
                    );
                }
                None => assert!(
                    queue.try_recv().is_err(),
                    "no flow was placed in member {slot}"
                ),
            }
        }

        // And the same flow asks for the same member again, which is the
        // property that keeps its packets in order.
        let info = ip::parse(&packet_on_port(port_a)).unwrap();
        assert_eq!(
            routes.pick(&info, Direction::Destination).unwrap().1,
            slot_a
        );
    }

    /// A flow keeps its lane when the set *grows*: placement is remembered, so
    /// a lane joining cannot move a flow that is already talking — the one
    /// event the old width-modulus rule could not survive.
    #[test]
    fn a_joining_lane_does_not_move_a_flow_that_is_already_talking() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        let info = ip::parse(&packet_on_port(40_000)).unwrap();
        assert_eq!(
            routes.pick(&info, Direction::Destination).unwrap().1,
            0,
            "one lane: every flow is in slot 0"
        );

        let (second, _rx_second) = mpsc::channel(4);
        routes.join(web, second, None, None).unwrap();
        assert_eq!(
            routes.pick(&info, Direction::Destination).unwrap().1,
            0,
            "the flow keeps the lane it was placed in, whatever joins"
        );

        // A flow that arrives afterwards takes the empty lane, so the table is
        // not simply pinning everything to the oldest one.
        let fresh = ip::parse(&packet_on_port(40_001)).unwrap();
        assert_eq!(routes.pick(&fresh, Direction::Destination).unwrap().1, 1);
        assert_eq!(routes.member_stats()[1].flows, 1);
    }

    /// A new flow is never placed in a slot whose lane is gone: those packets
    /// would be dropped for nothing while a live lane could carry them.
    #[test]
    fn a_new_flow_is_never_placed_in_an_empty_slot() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        let (second, _rx_second) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        routes.join(web, second, None, None).unwrap();
        routes.vacate(&web, 1);
        assert!(!routes.member_stats()[1].live);

        for port in 40_000..40_020u16 {
            let info = ip::parse(&packet_on_port(port)).unwrap();
            assert_eq!(
                routes.pick(&info, Direction::Destination).unwrap().1,
                0,
                "port {port} was placed in the empty slot"
            );
        }
    }

    /// A flow that has been silent for the eviction window is forgotten, and
    /// its lane's flow count goes back with it — which is what lets a later
    /// sweep, or a claim lending a lane, see that lane as idle.
    #[test]
    fn an_idle_flow_is_forgotten_and_frees_its_lane() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        let info = ip::parse(&packet_on_port(40_000)).unwrap();
        assert_eq!(routes.pick(&info, Direction::Destination).unwrap().1, 0);
        assert_eq!(routes.member_stats()[0].flows, 1);

        // The sweep runs on a placement, so the clock is advanced by hand: the
        // entry is past the window, which is what a resume after a long silence
        // looks like.
        let later =
            std::time::Instant::now() + FLOW_IDLE_EVICTION + std::time::Duration::from_secs(1);
        {
            let mut table = routes.table.lock().unwrap();
            table.lookup_endpoint_mut(&web).unwrap().sweep_if_due(later);
        }
        assert_eq!(
            routes.member_stats()[0].flows,
            0,
            "the lane is idle again once its flow is forgotten"
        );

        // Re-placing it is safe: it has been silent for longer than a packet can
        // be in flight, so nothing it sent earlier can be reordered by this.
        assert_eq!(routes.pick(&info, Direction::Destination).unwrap().1, 0);
        assert_eq!(routes.member_stats()[0].flows, 1);
    }

    /// Past its bound the table stops remembering: a new flow is placed by the
    /// hash alone, which is a pure function of the packet and the live set — so
    /// it still keeps one lane for as long as the set's width does.
    #[test]
    fn a_full_table_places_a_new_flow_by_hash() {
        let mut slots = ClaimSlots::new();
        let (first, _rx_first) = mpsc::channel(4);
        let (second, _rx_second) = mpsc::channel(4);
        let web = claim();
        slots.place(web, first, None, None);
        slots.place(web, second, None, None);
        let now = std::time::Instant::now();
        for key in 0..MAX_TRACKED_FLOWS as u64 {
            slots.flows.insert(
                key,
                FlowEntry {
                    slot: 0,
                    last_seen: now,
                },
            );
        }
        if let Some(entry) = slots.slots.get_mut(0) {
            entry.flows = MAX_TRACKED_FLOWS;
        }

        let info = ip::parse(&packet_on_port(40_000)).unwrap();
        let key = flow::key(&info);
        assert_eq!(
            slots.route(&info).unwrap(),
            flow::slot(&info, 2),
            "a full table falls back to the hash over the live set"
        );
        assert_eq!(
            slots.flows.len(),
            MAX_TRACKED_FLOWS,
            "the table did not grow past its bound"
        );
        assert!(
            !slots.flows.contains_key(&key),
            "the flow was not remembered"
        );
    }

    /// A lane that has been asked to end takes no new flow, and says so only
    /// once the flows it already had are gone: that is what makes lending a lane
    /// safe (nothing is reordered) and atomic (the receiver's lane is dialed
    /// after this one has actually ended).
    #[test]
    fn a_draining_lane_takes_no_new_flow_and_retires_when_empty() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        let (second, _rx_second) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        routes.join(web, second, None, None).unwrap();

        // A flow in slot 0, and one in slot 1, so both lanes are carrying
        // something.
        let busy = ip::parse(&packet_on_port(40_000)).unwrap();
        let other = ip::parse(&packet_on_port(40_001)).unwrap();
        let busy_slot = routes.pick(&busy, Direction::Destination).unwrap().1;
        let other_slot = routes.pick(&other, Direction::Destination).unwrap().1;

        // A claim's owner asks the lane in slot 0 to end while it is carrying a
        // flow: the ask is refused (not idle), and the lane drains.
        assert!(
            !routes.retire_lane(&web, busy_slot),
            "a lane with a flow in it is not idle"
        );
        assert!(!routes.should_retire(&web, busy_slot));
        for port in 41_000..41_020u16 {
            let info = ip::parse(&packet_on_port(port)).unwrap();
            assert_ne!(
                routes.pick(&info, Direction::Destination).unwrap().1,
                busy_slot,
                "a draining lane takes no new flow"
            );
        }

        // Once its flow falls silent — the idle sweep forgets it — the lane has
        // nothing left to carry and retires.
        let later =
            std::time::Instant::now() + FLOW_IDLE_EVICTION + std::time::Duration::from_secs(1);
        {
            let mut table = routes.table.lock().unwrap();
            table.lookup_endpoint_mut(&web).unwrap().sweep_if_due(later);
        }
        assert_eq!(routes.member_stats()[busy_slot].flows, 0);
        assert!(
            routes.should_retire(&web, busy_slot),
            "an empty lane that was asked to end retires"
        );
        assert!(
            !routes.should_retire(&web, other_slot),
            "the lanes nobody asked about are untouched"
        );
    }

    /// A slot whose lane is gone is not draining any more: the replacement the
    /// far end opens inherits a slot that can take flows again, whether the lane
    /// ended because it was asked to or because it failed.
    #[test]
    fn a_vacated_slot_stops_draining() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        let (second, _rx_second) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        routes.join(web, second, None, None).unwrap();
        assert!(routes.retire_lane(&web, 1), "an empty lane is idle");

        // The lane ends (its task drops the guard).
        routes.vacate(&web, 1);
        assert!(
            !routes.should_retire(&web, 1),
            "the slot is not draining now"
        );

        // A replacement joins it: it is a routing target again.
        let (replacement, _rx_replacement) = mpsc::channel(4);
        assert_eq!(
            routes.join(web, replacement, None, None).unwrap(),
            1,
            "the replacement inherits the freed slot"
        );
        for port in 42_000..42_040u16 {
            let info = ip::parse(&packet_on_port(port)).unwrap();
            let _ = routes.pick(&info, Direction::Destination).unwrap();
        }
        assert!(
            routes.member_stats()[1].flows > 0,
            "the replacement takes flows like any other lane"
        );
    }

    /// A batch is a run of the same frames the single-packet path wrote, which
    /// is what lets the far side keep reading them one at a time: what changed
    /// is only how many of them share a hand-over.
    #[tokio::test]
    async fn a_batch_is_a_run_of_the_same_frames() {
        use tokio::io::AsyncWriteExt;

        let first = tcp_packet(4);
        let second = tcp_packet(8);
        let mut batch = Batch::default();
        batch.push(&first).unwrap();
        batch.push(&second).unwrap();
        assert_eq!(batch.packets, 2);

        let (mut tx, mut rx) = tokio::io::duplex(4096);
        tx.write_all(&batch.frames).await.unwrap();
        drop(tx);

        let mut frames = IpFrames::new(&mut rx);
        assert_eq!(frames.next().await.unwrap(), &first[..]);
        assert_eq!(
            frames.next().await.unwrap(),
            &second[..],
            "the second frame follows the first"
        );
    }

    /// The caps are what bound one hand-over: a small-packet flow is stopped by
    /// the packet count, a bulk flow by the bytes, whichever comes first.
    #[test]
    fn a_batch_is_bounded_by_packets_and_by_bytes() {
        let mut by_packets = Batch::default();
        for _ in 0..BATCH_PACKETS {
            assert!(!by_packets.is_full(), "not full before the cap is reached");
            by_packets.push(&tcp_packet(4)).unwrap();
        }
        assert!(by_packets.is_full(), "the packet cap bounds it");

        let mut by_bytes = Batch::default();
        let big = tcp_packet(1400);
        while by_bytes.frames.len() < BATCH_BYTES {
            by_bytes.push(&big).unwrap();
        }
        assert!(by_bytes.is_full(), "the byte cap bounds it");
        assert!(
            by_bytes.packets < BATCH_PACKETS,
            "a bulk flow reaches the byte cap first"
        );
    }

    /// A packet the frame format cannot carry is refused without disturbing the
    /// packets already batched: the batch is not the packet's problem.
    #[test]
    fn an_unframable_packet_leaves_the_batch_intact() {
        let mut batch = Batch::default();
        batch.push(&tcp_packet(4)).unwrap();
        let before = batch.frames.len();

        let oversized = vec![0u8; usize::from(u16::MAX) + 1];
        assert!(batch.push(&oversized).is_err());
        assert_eq!(batch.frames.len(), before);
        assert_eq!(batch.packets, 1);
    }

    /// The routing answer is the endpoint, not the queue, so a flush after the
    /// channel was replaced finds the current one — and finds none when the
    /// claim went away with it.
    #[test]
    fn routing_names_the_endpoint_and_a_replaced_queue_is_found_by_lookup() {
        let mut table: EndpointTable<u32> = EndpointTable::new();
        let web = claim();
        table.insert(web, 1);

        let info = PacketInfo {
            protocol: crate::transparent::ip::TCP,
            src: IpAddr::V4(Ipv4Addr::new(10, 10, 0, 2)),
            dst: web.ip,
            src_port: Some(40_000),
            dst_port: Some(8443),
        };
        assert_eq!(table.endpoint_for(&info, Direction::Destination), Some(web));
        assert_eq!(table.lookup_endpoint(&web), Some(&1));

        table.insert(web, 2);
        assert_eq!(
            table.lookup_endpoint(&web),
            Some(&2),
            "the flush follows the queue that is registered now"
        );
        table.remove(&web);
        assert_eq!(table.lookup_endpoint(&web), None);
    }

    /// Members take the lowest free slot, and a member that leaves **keeps its
    /// slot's place**: the replacement takes the freed index, so the set's
    /// width — the modulus a flow is placed by — does not move when a member
    /// does.
    #[test]
    fn a_member_takes_the_lowest_free_slot_and_a_leave_does_not_narrow_the_set() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        assert_eq!(routes.join(web, first, None, None).unwrap(), 0);
        let (second, _rx_second) = mpsc::channel(4);
        assert_eq!(routes.join(web, second, None, None).unwrap(), 1);
        assert_eq!(routes.member_stats().len(), 2);

        routes.vacate(&web, 0);
        let stats = routes.member_stats();
        assert_eq!(stats.len(), 2, "the slot is kept, the set is not narrowed");
        assert!(!stats[0].live && stats[1].live);

        let (replacement, _rx_replacement) = mpsc::channel(4);
        assert_eq!(
            routes.join(web, replacement, None, None).unwrap(),
            0,
            "the replacement inherits the slot its predecessor held"
        );
        assert_eq!(routes.member_stats().len(), 2);
    }

    /// A claim holds more members at once than it ever has before: the set
    /// widens by one instead of refusing the member, because refusing it would
    /// drop a channel the caller already opened — and the width is what the
    /// set is *discovered* to be, not a count anyone declared.
    #[test]
    fn a_set_widens_when_every_slot_is_taken() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        assert_eq!(routes.join(web, first, None, None).unwrap(), 0);
        let (second, _rx_second) = mpsc::channel(4);
        assert_eq!(routes.join(web, second, None, None).unwrap(), 1);
        assert_eq!(routes.member_stats().len(), 2);
    }

    /// A packet is placed in its set's slot and its batch reaches that member's
    /// queue; a slot with no member drops its own packets and counts them
    /// there, while a live slot keeps forwarding — the whole point of a member
    /// set over one channel.
    #[test]
    fn a_dropped_member_is_counted_apart_and_the_survivors_keep_forwarding() {
        let routes = Routes::new();
        let web = claim();
        let (tx, mut rx_first) = mpsc::channel(4);
        let first_slot = routes.join(web, tx, None, None).unwrap();
        let (spare, mut rx_second) = mpsc::channel(4);
        let second_slot = routes.join(web, spare, None, None).unwrap();
        assert_eq!(
            (first_slot, second_slot),
            (0, 1),
            "join order is slot order"
        );

        let packet = packet_for_claim();
        let info = ip::parse(&packet).unwrap();
        let (endpoint, placed) = routes.pick(&info, Direction::Destination).unwrap();
        assert_eq!(endpoint, web);
        // Which slot the flow is placed in is the hash's business; the test
        // follows it rather than assuming one. Slot 0's queue is the first
        // member that joined, slot 1's the second.
        let (survivor, placed_rx, survivor_rx) = if placed == 0 {
            (1, &mut rx_first, &mut rx_second)
        } else {
            (0, &mut rx_second, &mut rx_first)
        };

        let mut batch = Batch::default();
        batch.push(&packet).unwrap();
        assert!(
            routes.flush(&endpoint, placed, batch),
            "the live member took it"
        );
        let stats = routes.member_stats();
        assert_eq!(stats[placed].forwarded, 1);
        assert_eq!(stats[placed].no_channel, 0);
        assert_eq!(
            stats[survivor].forwarded, 0,
            "the other member was not charged"
        );
        assert!(
            placed_rx.try_recv().is_ok(),
            "the frames reached the placed member's queue"
        );
        assert!(
            survivor_rx.try_recv().is_err(),
            "the other member's queue stayed empty"
        );

        // The member goes away: its slot drops what is placed in it, and says
        // so on its own counters.
        routes.vacate(&web, placed);
        let mut batch = Batch::default();
        batch.push(&packet).unwrap();
        assert!(
            !routes.flush(&endpoint, placed, batch),
            "the empty slot dropped it"
        );
        let stats = routes.member_stats();
        assert_eq!(stats[placed].forwarded, 1);
        assert_eq!(stats[placed].no_channel, 1);
        assert_eq!(
            stats[survivor].forwarded, 0,
            "the survivor was not charged for the dead member's traffic"
        );
        assert_eq!(stats[survivor].no_channel, 0);

        // And the survivor still carries traffic of its own.
        let mut batch = Batch::default();
        batch.push(&packet).unwrap();
        assert!(routes.flush(&endpoint, survivor, batch));
        assert_eq!(routes.member_stats()[survivor].forwarded, 1);
    }

    /// A member that died is replaced **in its own slot**, and the flows that
    /// were placed in it resume there: the slot is what the placement reduces
    /// against, so a replacement has to inherit its predecessor's index rather
    /// than take a new one at the end of the set — otherwise every flow of the
    /// dead member would move, and moving a flow is what reorders it.
    #[test]
    fn a_replacement_takes_the_dead_members_slot_and_its_flows_resume_there() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        let (second, _rx_second) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        routes.join(web, second, None, None).unwrap();

        let packet = packet_on_port(40_000);
        let info = ip::parse(&packet).unwrap();
        let (endpoint, placed) = routes.pick(&info, Direction::Destination).unwrap();
        let survivor = 1 - placed;

        // The member dies: its slot drops this flow's packets, and only its
        // counters move.
        routes.vacate(&web, placed);
        let mut batch = Batch::default();
        batch.push(&packet).unwrap();
        assert!(
            !routes.flush(&endpoint, placed, batch),
            "dropped while the slot is empty"
        );
        assert_eq!(routes.member_stats()[placed].no_channel, 1);
        assert_eq!(routes.member_stats()[survivor].no_channel, 0);

        // The replacement joins and takes the freed slot; the flow is placed
        // there again, so it never moved.
        let (replacement, mut rx_replacement) = mpsc::channel(4);
        assert_eq!(
            routes.join(web, replacement, None, None).unwrap(),
            placed,
            "the replacement inherits the dead member's slot"
        );
        assert_eq!(
            routes.pick(&info, Direction::Destination).unwrap().1,
            placed,
            "and the flow that was placed in it stays there"
        );
        let mut batch = Batch::default();
        batch.push(&packet).unwrap();
        assert!(routes.flush(&endpoint, placed, batch));
        assert!(
            rx_replacement.try_recv().is_ok(),
            "the flow resumes on the replacement"
        );
        assert_eq!(routes.member_stats()[placed].forwarded, 1);
    }

    /// A claim nobody carries any more is not a routing answer: the last
    /// member's departure takes it out of the table, and the packets for it are
    /// unclaimed until a member comes back.
    #[test]
    fn the_last_member_leaving_removes_the_claim() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        routes.join(web, first, None, None).unwrap();
        let info = ip::parse(&packet_for_claim()).unwrap();
        assert!(routes.pick(&info, Direction::Destination).is_some());

        routes.vacate(&web, 0);
        assert!(routes.pick(&info, Direction::Destination).is_none());
        assert!(routes.member_stats().is_empty());
    }

    /// A packet for an address nobody claimed is not routed at all, whichever
    /// end of it the direction looks at.
    #[test]
    fn an_unclaimed_packet_is_not_picked() {
        let routes = Routes::new();
        let (tx, _rx) = mpsc::channel(4);
        routes.join(claim(), tx, None, None).unwrap();

        let info = PacketInfo {
            protocol: crate::transparent::ip::TCP,
            src: IpAddr::V4(Ipv4Addr::new(10, 10, 0, 2)),
            dst: IpAddr::V4(Ipv4Addr::new(10, 99, 0, 9)),
            src_port: Some(40_000),
            dst_port: Some(8443),
        };
        assert!(routes.pick(&info, Direction::Destination).is_none());
        assert!(routes.pick(&info, Direction::Source).is_none());
    }
}
