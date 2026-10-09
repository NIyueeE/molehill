//! The process-wide TUN hub: one reader per device, one queue per service.
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
use crate::transparent::ip;
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

/// One TUN device, its reader task, and the per-endpoint queues.
pub struct TunHub {
    name: String,
    tun: Arc<Tun>,
    direction: Direction,
    routes: Mutex<EndpointTable<mpsc::Sender<Bytes>>>,
    stats: Arc<Stats>,
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
    /// each run a reader, or they would race for the same packets.
    pub fn get_or_spawn(
        name: &str,
        stats: Arc<Stats>,
        direction: Direction,
    ) -> Result<Arc<TunHub>> {
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
            routes: Mutex::new(EndpointTable::new()),
            stats,
        });
        hub.clone().spawn_reader();
        guard.insert(key, Arc::clone(&hub));
        tracing::info!(tun = %hub.name, ?direction, "Transparent data path attached to the TUN device");
        Ok(hub)
    }

    /// Route packets for `endpoint` to `queue`. A second registration for the
    /// same endpoint replaces the first (a service restarting its channel).
    pub fn register(&self, endpoint: Endpoint, queue: mpsc::Sender<Bytes>) -> Result<()> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| anyhow::anyhow!("the TUN routing table is poisoned"))?;
        routes.insert(endpoint, queue);
        Ok(())
    }

    /// Which end of a packet this hub's endpoints are recognised by.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn unregister(&self, endpoint: &Endpoint) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(endpoint);
        }
    }

    /// Hand one packet to the kernel, as if it had arrived on the device.
    pub async fn inject(&self, packet: &[u8]) -> io::Result<()> {
        self.tun.write_packet(packet).await
    }

    /// The reader loop: device to service queues. Runs until the device is
    /// gone.
    ///
    /// It drains the device and hands over **batches**, one per endpoint, not
    /// packets: a batch is a run of `[u16 length][packet]` frames in one
    /// `Bytes`, so N packets cost one queue message, one `write_all` on the
    /// tunnel and — with a carrier that segments at the MSS — one carrier
    /// header and one acknowledgement instead of N. That per-packet transport
    /// cost was measured at ~80 bytes against a ~35-byte header, which is what
    /// makes this the lever ([benchmarks.md](../../docs/benchmarks.md), "The
    /// transparent-L3 wire question").
    fn spawn_reader(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_PACKET];
            // One buffer per endpoint while the device is drained. Keyed by
            // endpoint rather than by queue so a batch survives the queue being
            // replaced mid-drain: the flush looks the current queue up.
            let mut batches: HashMap<Endpoint, Batch> = HashMap::new();
            loop {
                let outcome = self
                    .tun
                    .drain(&mut buf, &mut |packet| self.route(packet, &mut batches))
                    .await;
                // Whatever ended the drain, what was read is written: a batch
                // never waits for the next packet to arrive.
                for (endpoint, batch) in batches.drain() {
                    self.flush(endpoint, batch);
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

    /// Parse one packet and append it to its endpoint's batch, flushing that
    /// batch when it is as large as one message may grow.
    fn route(&self, packet: &[u8], batches: &mut HashMap<Endpoint, Batch>) {
        let info = match ip::parse(packet) {
            Ok(info) => info,
            Err(e) => {
                self.stats.count_drop(DropReason::from(e));
                return;
            }
        };
        let endpoint = {
            let Ok(routes) = self.routes.lock() else {
                return;
            };
            let Some(endpoint) = routes.endpoint_for(&info, self.direction) else {
                self.stats.count_drop(DropReason::Unclaimed);
                return;
            };
            endpoint
        };
        let batch = batches.entry(endpoint).or_default();
        if batch.push(packet).is_err() {
            // An oversized packet cannot be framed; it is not the batch's
            // problem, and dropping it keeps the rest.
            self.stats.count_drop(DropReason::Malformed);
            return;
        }
        if batch.is_full()
            && let Some(batch) = batches.remove(&endpoint)
        {
            self.flush(endpoint, batch);
        }
    }

    /// Hand one endpoint's batch to its queue.
    ///
    /// The batch shares one fate: a queue that is full or gone drops all of its
    /// packets, which is the same UDP-like answer the per-packet path gave,
    /// with the counting still done per packet.
    fn flush(&self, endpoint: Endpoint, batch: Batch) {
        let Batch { frames, packets } = batch;
        if packets == 0 {
            return;
        }
        let delivered = {
            let Ok(routes) = self.routes.lock() else {
                return;
            };
            routes
                .lookup_endpoint(&endpoint)
                .is_some_and(|queue| queue.try_send(frames.freeze()).is_ok())
        };
        if delivered {
            self.stats.count_forwarded_by(packets);
        } else {
            self.stats.count_no_channel_by(packets);
        }
    }
}

/// A run of frames waiting for one endpoint, built while the device is drained.
///
/// One buffer per endpoint, not per packet: `frames` grows into a single `Bytes`
/// (a `BytesMut` freeze is a move, not a copy), so nothing is allocated per
/// packet on this path.
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
/// it. The function returns when the channel or the device ends — the caller
/// decides whether to ask for another channel.
pub async fn forward_transparent<T>(
    conn: T,
    hub: Arc<TunHub>,
    endpoint: Endpoint,
    stats: Arc<Stats>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    hub.register(endpoint, tx)?;

    // Whatever ends the loop, the endpoint must stop pointing at this channel.
    let guard = Unregister {
        hub: Arc::clone(&hub),
        endpoint,
    };

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

/// Removes the endpoint from the hub however the forward ends.
struct Unregister {
    hub: Arc<TunHub>,
    endpoint: Endpoint,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        self.hub.unregister(&self.endpoint);
    }
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

    fn tcp_packet(payload: usize) -> Vec<u8> {
        let mut packet = vec![0x45, 0x00, 0x00, 0x00, 0, 0, 0, 0, 64, 6, 0, 0, 10, 0, 0, 1];
        packet.extend_from_slice(&[10, 0, 0, 2]);
        packet.extend_from_slice(&[0x1f, 0x90, 0x20, 0x00]); // ports
        packet.resize(20 + 20 + payload, 0);
        packet
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
        let web = Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 8443);
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
}
