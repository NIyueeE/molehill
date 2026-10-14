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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::protocol::IpTraffic;
use crate::transparent::ip;
use crate::transparent::tun::Tun;
use crate::transparent::{Direction, DropReason, Endpoint, EndpointTable, Stats};

/// Packets waiting for one service's channel. Small on purpose: the point of
/// the queue is to absorb a burst while the channel is busy, not to buffer a
/// second of traffic.
const QUEUE: usize = 1024;

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
    fn spawn_reader(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_PACKET];
            loop {
                let read = match self.tun.read_packet(&mut buf).await {
                    Ok(read) => read,
                    Err(e) => {
                        // A dead device stops the whole path; the services'
                        // channels then fail their own way, one log line each.
                        tracing::debug!(tun = %self.name, "TUN read ended: {e}");
                        return;
                    }
                };
                let packet = &buf[..read];
                let info = match ip::parse(packet) {
                    Ok(info) => info,
                    Err(e) => {
                        self.stats.count_drop(DropReason::from(e));
                        continue;
                    }
                };

                // The lock is released before anything can await: this is a
                // std mutex held for one table lookup and one `try_send`.
                let delivered = {
                    let Ok(routes) = self.routes.lock() else {
                        return;
                    };
                    if let Some(queue) = routes.lookup_in(&info, self.direction) {
                        queue.try_send(Bytes::copy_from_slice(packet)).is_ok()
                    } else {
                        self.stats.count_drop(DropReason::Unclaimed);
                        continue;
                    }
                };
                if delivered {
                    self.stats.count_forwarded();
                } else {
                    self.stats.count_no_channel();
                }
            }
        });
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
    let (mut reader, mut writer) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    hub.register(endpoint, tx)?;

    // Whatever ends the loop, the endpoint must stop pointing at this channel.
    let guard = Unregister {
        hub: Arc::clone(&hub),
        endpoint,
    };

    let pump = tokio::spawn(async move {
        let mut scratch = BytesMut::new();
        while let Some(packet) = rx.recv().await {
            if IpTraffic::write_frame(&mut writer, &mut scratch, &packet)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut scratch = BytesMut::new();
    let result = loop {
        match IpTraffic::read(&mut reader, &mut scratch).await {
            Ok(len) => {
                // Defence in depth: a peer must not be able to steer traffic
                // into a local address this service did not claim. Which end
                // of the packet names the claim depends on the side: the
                // client is the destination, the server is the source of the
                // traffic it hands back.
                if !claims(&endpoint, &scratch[..len], hub.direction()) {
                    stats.count_drop(DropReason::Unclaimed);
                    continue;
                }
                if let Err(e) = hub.inject(&scratch[..len]).await {
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
