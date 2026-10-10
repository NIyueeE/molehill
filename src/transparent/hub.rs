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
//! A claimed endpoint is carried by a **member set**, not by one channel: each
//! data channel of the claim owns a slot of the set, and every packet is placed
//! in a slot while the device is drained. A slot whose member is gone — a
//! replacement is in flight — is *empty*, and the packets placed in it are
//! dropped and counted exactly as the single-channel path counted the packets
//! of a channel that had ended; the surviving members are untouched. The set's
//! width is therefore the routing rule's modulus, and it never shrinks: a
//! replaced member takes the slot its predecessor left, so nothing that routes
//! by slot has to move.
//!
//! Queues are bounded and lossy on overflow, UDP semantics: a full queue drops
//! the packet rather than stalling every other service sharing the device.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::protocol::{IpFrames, IpTraffic};
use crate::transparent::flow;
use crate::transparent::ip::{self, PacketInfo};
use crate::transparent::tun::Tun;
use crate::transparent::{Direction, DropReason, Endpoint, EndpointTable, Stats};

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

    /// Give one data channel a slot in `endpoint`'s member set and register its
    /// queue for it.
    ///
    /// The set's width is *discovered*, not declared: it is the widest the
    /// claim has been seen to hold at once, so a channel that never becomes a
    /// member (its start command failed, or a peer that only ever starts one
    /// channel) leaves no slot behind that nothing could fill. The slot lives
    /// exactly as long as the returned guard.
    fn join(&self, endpoint: Endpoint, queue: mpsc::Sender<Bytes>) -> Result<MemberGuard> {
        let slot = self.routes.join(endpoint, queue)?;
        Ok(MemberGuard {
            routes: Arc::clone(&self.routes),
            endpoint,
            slot,
        })
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
}

impl Drop for MemberGuard {
    fn drop(&mut self) {
        self.routes.vacate(&self.endpoint, self.slot);
    }
}

/// One member slot of one claim, as the stats line reports it.
pub struct MemberStat {
    /// The claimed endpoint the slot belongs to.
    pub endpoint: Endpoint,
    /// The slot's index in the claim's member set.
    pub slot: usize,
    /// Whether a member currently holds the slot.
    pub live: bool,
    /// Packets this slot handed to its member.
    pub forwarded: u64,
    /// Packets placed in this slot while no member held it (a replacement
    /// window): the traffic the claim lost with that member.
    pub no_channel: u64,
}

/// The routing state of one hub, without the device.
///
/// Split from [`TunHub`] on purpose: the member sets and the placement rule are
/// pure bookkeeping, so they are unit-tested without a TUN device — the same
/// split `transport::pool` makes for the tunnel pool's policy.
struct Routes {
    table: Mutex<EndpointTable<ClaimSlots>>,
    stats: Arc<Stats>,
}

impl Routes {
    fn new() -> Self {
        Self {
            table: Mutex::new(EndpointTable::new()),
            stats: Arc::new(Stats::default()),
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
    fn join(&self, endpoint: Endpoint, queue: mpsc::Sender<Bytes>) -> Result<usize> {
        let mut table = self
            .table
            .lock()
            .map_err(|_| anyhow::anyhow!("the TUN routing table is poisoned"))?;
        if let Some(claim) = table.lookup_endpoint_mut(&endpoint) {
            return Ok(claim.place(queue));
        }
        let mut claim = ClaimSlots::new();
        let slot = claim.place(queue);
        table.insert(endpoint, claim);
        Ok(slot)
    }

    /// Which claim and which of its members one packet belongs to.
    ///
    /// The endpoint lookup is the allow-list; a packet whose claimed end names
    /// no registered endpoint belongs to nobody and is counted as unclaimed by
    /// the caller. The member is the claim's **flow placement**
    /// ([`flow::slot`]): a pure function of the packet's five-tuple and the
    /// set's width, so a flow keeps the member it was placed on for as long as
    /// the width does — a member *joining* is the one thing that changes the
    /// width, and a member leaving deliberately does not ([`Self::vacate`]).
    fn pick(&self, info: &PacketInfo, direction: Direction) -> Option<(Endpoint, usize)> {
        let table = self.table.lock().ok()?;
        let endpoint = table.endpoint_for(info, direction)?;
        let width = table.lookup_endpoint(&endpoint)?.slots.len();
        Some((endpoint, flow::slot(info, width)))
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
        let delivered = {
            let Ok(mut table) = self.table.lock() else {
                return false;
            };
            let entry = table
                .lookup_endpoint_mut(endpoint)
                .and_then(|claim| claim.slots.get_mut(slot));
            match entry {
                Some(entry) => {
                    let sent = entry
                        .queue
                        .as_ref()
                        .is_some_and(|queue| queue.try_send(frames.freeze()).is_ok());
                    if sent {
                        entry.forwarded += packets as u64;
                    } else {
                        entry.no_channel += packets as u64;
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
        }
        let live = claim.slots.iter().any(|entry| entry.queue.is_some());
        if !live {
            table.remove(endpoint);
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
                    forwarded: entry.forwarded,
                    no_channel: entry.no_channel,
                });
            }
        }
        out.sort_by_key(|member| (member.endpoint, member.slot));
        out
    }
}

/// The member set of one claimed endpoint: a fixed table of slots.
///
/// The slots are the routing targets. A slot exists whether or not a member
/// holds it, which is what lets a replacement inherit its predecessor's index
/// and the claim's flows stay where they were.
#[derive(Default)]
struct ClaimSlots {
    slots: Vec<MemberSlot>,
}

/// One member's place in a claim's set.
#[derive(Default)]
struct MemberSlot {
    /// The queue of the member that holds this slot, while one does.
    queue: Option<mpsc::Sender<Bytes>>,
    /// Packets this slot handed to its member.
    forwarded: u64,
    /// Packets placed here while the slot was empty.
    no_channel: u64,
}

impl ClaimSlots {
    /// A set with the one slot every claim starts with.
    fn new() -> Self {
        Self {
            slots: vec![MemberSlot::default()],
        }
    }

    /// Take the lowest empty slot, widening the set by one when every slot is
    /// taken. Returns the slot's index.
    fn place(&mut self, queue: mpsc::Sender<Bytes>) -> usize {
        let slot = if let Some(slot) = self.slots.iter().position(|slot| slot.queue.is_none()) {
            slot
        } else {
            self.slots.push(MemberSlot::default());
            self.slots.len() - 1
        };
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.queue = Some(queue);
        }
        slot
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
pub async fn forward_transparent<T>(conn: T, hub: Arc<TunHub>, endpoint: Endpoint) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stats = hub.stats();
    let (reader, mut writer) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    let guard = hub.join(endpoint, tx)?;

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
    let result = loop {
        match frames.next().await {
            Ok(packet) => {
                // Defence in depth: a peer must not be able to steer traffic
                // into a local address this service did not claim. Which end
                // of the packet names the claim depends on the side: the
                // client is the destination, the server is the source of the
                // traffic it hands back.
                if !claims(&endpoint, packet, hub.direction()) {
                    stats.count_drop(DropReason::Unclaimed);
                    continue;
                }
                if let Err(e) = hub.inject(packet).await {
                    break Err(anyhow::Error::from(e))
                        .with_context(|| format!("Failed to inject a packet for {endpoint}"));
                }
            }
            Err(e) => {
                stats.count_channel_error();
                break Err(e).with_context(|| format!("Transparent channel for {endpoint} ended"));
            }
        }
    };
    drop(guard);
    pump.abort();
    result
}

/// Whether a packet off the tunnel is one this service claimed.
///
/// The claim lives at the end this side owns: the client injected a visitor's
/// packet for the address it carries, so it matches the *destination*; the
/// server receives that connection's replies, so it matches the *source*. A
/// portless packet (ICMP, and the fragments after the first) matches on the
/// address alone.
fn claims(endpoint: &Endpoint, packet: &[u8], direction: Direction) -> bool {
    let Ok(info) = ip::parse(packet) else {
        return false;
    };
    let (addr, port) = match direction {
        Direction::Source => (info.dst, info.dst_port),
        Direction::Destination => (info.src, info.src_port),
    };
    addr == endpoint.ip && port.is_none_or(|port| port == endpoint.port)
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
            for member in hub.member_stats() {
                tracing::info!(
                    target: "molehill::transparent",
                    "l3-stats: role={role} claim={} member={} live={} forwarded={} \
                     no_channel={}",
                    member.endpoint,
                    member.slot,
                    member.live,
                    member.forwarded,
                    member.no_channel,
                );
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

    /// A claim's flows ride different members of its set, and each flow keeps
    /// the member it was placed on: the spread is what the set exists for, and
    /// the stability is what keeps a flow's packets in order.
    #[test]
    fn a_claims_flows_are_spread_over_its_members_and_stay_there() {
        let routes = Routes::new();
        let web = claim();
        let (first, mut rx_first) = mpsc::channel(4);
        let (second, mut rx_second) = mpsc::channel(4);
        routes.join(web, first).unwrap();
        routes.join(web, second).unwrap();
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
        assert_eq!(routes.join(web, first).unwrap(), 0);
        let (second, _rx_second) = mpsc::channel(4);
        assert_eq!(routes.join(web, second).unwrap(), 1);
        assert_eq!(routes.member_stats().len(), 2);

        routes.vacate(&web, 0);
        let stats = routes.member_stats();
        assert_eq!(stats.len(), 2, "the slot is kept, the set is not narrowed");
        assert!(!stats[0].live && stats[1].live);

        let (replacement, _rx_replacement) = mpsc::channel(4);
        assert_eq!(
            routes.join(web, replacement).unwrap(),
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
        assert_eq!(routes.join(web, first).unwrap(), 0);
        let (second, _rx_second) = mpsc::channel(4);
        assert_eq!(routes.join(web, second).unwrap(), 1);
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
        let first_slot = routes.join(web, tx).unwrap();
        let (spare, mut rx_second) = mpsc::channel(4);
        let second_slot = routes.join(web, spare).unwrap();
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

    /// A claim nobody carries any more is not a routing answer: the last
    /// member's departure takes it out of the table, and the packets for it are
    /// unclaimed until a member comes back.
    #[test]
    fn the_last_member_leaving_removes_the_claim() {
        let routes = Routes::new();
        let web = claim();
        let (first, _rx_first) = mpsc::channel(4);
        routes.join(web, first).unwrap();
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
        routes.join(claim(), tx).unwrap();

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
