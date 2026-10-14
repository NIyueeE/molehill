//! Transparent (L3) services: the client owns the public `ip:port`.
//!
//! A transparent service is never bound by the server. The claimed address is
//! carried by the **client's** TUN device — the operator's own routing moves
//! traffic in and out, this module never installs a route or a netfilter rule
//! — so the visitor's packets reach the client's kernel *as packets*: the
//! backend sees the visitor's real address, TCP keeps its end-to-end
//! semantics, and the server holds no connection state for the flow.
//!
//! Both ends share the same two primitives:
//!
//! - [`ip::parse`] reads the routed destination of one IP packet without
//!   allocating;
//! - [`EndpointTable`] decides which packet belongs to which service — a
//!   router on the server, an allow-list on the client.
//!
//! Linux only, because the data path is a TUN device.

pub mod check;
pub mod hub;
pub mod ip;
pub mod tun;

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::transparent::ip::{PacketInfo, ParseError, TCP, UDP};

/// Which end of a packet a routing table keys on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Direction {
    /// Route by destination: the server, whose host routes the claimed address
    /// into the tunnel.
    Destination,
    /// Route by source: the client, whose host carries the claimed address and
    /// therefore *emits* it on the return path.
    Source,
}

/// One claimed public endpoint: the address and port the client owns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Endpoint {
    pub ip: IpAddr,
    pub port: u16,
}

impl Endpoint {
    pub fn new(ip: IpAddr, port: u16) -> Self {
        Self { ip, port }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The endpoints one node is responsible for.
///
/// Used in both directions: the server routes an incoming packet to the
/// service that claimed its destination; the client checks that a packet it
/// is about to inject was actually claimed locally (defence in depth — a
/// compromised or buggy peer must not be able to steer traffic into an
/// arbitrary local address).
///
/// A port-carrying packet matches its exact `(ip, port)` entry. A packet with
/// no usable port — ICMP, and the non-first fragments that carry no transport
/// header — matches by address, and only when that address has **exactly one**
/// claimant: with two services on one address there is nothing to choose by,
/// so the packet is dropped rather than guessed.
pub struct EndpointTable<V> {
    entries: HashMap<(IpAddr, u16), V>,
    per_ip: HashMap<IpAddr, usize>,
}

// Manual, not derived: the table is empty whatever `V` is, and a derived
// `Default` would demand `V: Default` from every caller for no reason.
impl<V> Default for EndpointTable<V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            per_ip: HashMap::new(),
        }
    }
}

impl<V> EndpointTable<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `endpoint`. Returns the value it replaced, if any.
    pub fn insert(&mut self, endpoint: Endpoint, value: V) -> Option<V> {
        let replaced = self.entries.insert((endpoint.ip, endpoint.port), value);
        if replaced.is_none() {
            *self.per_ip.entry(endpoint.ip).or_insert(0) += 1;
        }
        replaced
    }

    pub fn remove(&mut self, endpoint: &Endpoint) -> Option<V> {
        let removed = self.entries.remove(&(endpoint.ip, endpoint.port));
        if removed.is_some()
            && let Some(count) = self.per_ip.get_mut(&endpoint.ip)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_ip.remove(&endpoint.ip);
            }
        }
        removed
    }

    /// Only the tests need the size today; the production paths act on
    /// `lookup`/`insert`/`remove` alone.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Only the tests need the size today; the production paths act on
    /// `lookup`/`insert`/`remove` alone.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Which endpoint a packet belongs to, looking at the end this side owns.
    ///
    /// The two ends own opposite ends of every packet: the server's host
    /// routes the claimed *destination* into the tunnel, while the client's
    /// host **is** the claimed address, so its return traffic is recognised by
    /// its *source*.
    ///
    /// The endpoint rather than the value, because the caller batches per
    /// endpoint: it keeps one buffer per claim while it drains the device, and
    /// looks the queue up when it hands the batch over — a queue can be
    /// replaced in between, and the flush must find the current one.
    pub fn endpoint_for(&self, info: &PacketInfo, direction: Direction) -> Option<Endpoint> {
        let (ip, port) = match direction {
            Direction::Destination => (info.dst, info.dst_port),
            Direction::Source => (info.src, info.src_port),
        };
        if let Some(port) = port
            && (info.protocol == TCP || info.protocol == UDP)
            && self.entries.contains_key(&(ip, port))
        {
            return Some(Endpoint::new(ip, port));
        }
        // No port to route by — ICMP, the fragments after the first, or a
        // transport this parser does not read ports from: only an address with
        // a single claimant is unambiguous.
        if self.per_ip.get(&ip).copied() == Some(1)
            && let Some(((ip, port), _)) = self
                .entries
                .iter()
                .find(|((entry_ip, _), _)| *entry_ip == ip)
        {
            return Some(Endpoint::new(*ip, *port));
        }
        None
    }

    /// The value registered for exactly this endpoint, if it is still
    /// registered.
    pub fn lookup_endpoint(&self, endpoint: &Endpoint) -> Option<&V> {
        self.entries.get(&(endpoint.ip, endpoint.port))
    }

    /// The value a packet belongs to, looking at the end this side owns.
    ///
    /// The rule lives in [`Self::endpoint_for`]; this is the same answer with
    /// the table's value attached.
    #[cfg(test)]
    pub fn lookup_in(&self, info: &PacketInfo, direction: Direction) -> Option<&V> {
        let endpoint = self.endpoint_for(info, direction)?;
        self.lookup_endpoint(&endpoint)
    }

    /// The value a packet belongs to, if any.
    #[cfg(test)]
    pub fn lookup(&self, info: &PacketInfo) -> Option<&V> {
        self.lookup_in(info, Direction::Destination)
    }
}

/// Why a packet was not forwarded. Counted, never logged per packet: one
/// misdirected packet is not an operator's problem, a stream of them is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropReason {
    /// The header is not IPv4 (IPv6 is not carried yet).
    NotIpv4,
    /// Too short, or a header length that runs past the packet.
    Malformed,
    /// Well formed, but no registered endpoint claims its destination.
    Unclaimed,
}

impl From<ParseError> for DropReason {
    fn from(e: ParseError) -> Self {
        match e {
            ParseError::NotIpv4 => Self::NotIpv4,
            ParseError::Malformed => Self::Malformed,
        }
    }
}

/// Counters for the transparent data path, behind `MOLEHILL_L3_STATS=1`.
#[derive(Default)]
pub struct Stats {
    pub forwarded: AtomicU64,
    pub dropped_not_ipv4: AtomicU64,
    pub dropped_malformed: AtomicU64,
    pub dropped_unclaimed: AtomicU64,
    /// Packets whose channel was gone (reconnect window): UDP semantics, the
    /// visitor's own transport retries.
    pub dropped_no_channel: AtomicU64,
    /// Frames read from the tunnel that failed to parse.
    pub channel_errors: AtomicU64,
}

impl Stats {
    pub fn count_drop(&self, reason: DropReason) {
        let counter = match reason {
            DropReason::NotIpv4 => &self.dropped_not_ipv4,
            DropReason::Malformed => &self.dropped_malformed,
            DropReason::Unclaimed => &self.dropped_unclaimed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// `packets` forwarded in one batch: the counters are per packet however
    /// packets travel, because that is what an operator compares against
    /// traffic they can see.
    pub fn count_forwarded_by(&self, packets: usize) {
        self.forwarded.fetch_add(packets as u64, Ordering::Relaxed);
    }

    /// `packets` whose channel was gone or full. A batch shares one fate, and
    /// this is where that fate is counted per packet.
    pub fn count_no_channel_by(&self, packets: usize) {
        self.dropped_no_channel
            .fetch_add(packets as u64, Ordering::Relaxed);
    }

    /// A frame off the tunnel could not be read.
    pub fn count_channel_error(&self) {
        self.channel_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the operator asked for the stats line.
    pub fn enabled() -> bool {
        std::env::var_os("MOLEHILL_L3_STATS").is_some()
    }
}

/// One `INFO` line per second per process while `MOLEHILL_L3_STATS=1`.
///
/// Cumulative counters, like every other `MOLEHILL_*_STATS` switch: rates come
/// from consecutive lines.
pub fn spawn_stats_reporter(role: &'static str, stats: Arc<Stats>) {
    if !Stats::enabled() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            tracing::info!(
                target: "molehill::transparent",
                "l3-stats: role={role} forwarded={} dropped(not_ipv4={} malformed={} \
                 unclaimed={} no_channel={}) channel_errors={}",
                stats.forwarded.load(Ordering::Relaxed),
                stats.dropped_not_ipv4.load(Ordering::Relaxed),
                stats.dropped_malformed.load(Ordering::Relaxed),
                stats.dropped_unclaimed.load(Ordering::Relaxed),
                stats.dropped_no_channel.load(Ordering::Relaxed),
                stats.channel_errors.load(Ordering::Relaxed),
            );
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
    use std::net::Ipv4Addr;

    fn info(dst: &str, port: Option<u16>, protocol: u8) -> PacketInfo {
        PacketInfo {
            protocol,
            src: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            dst: dst.parse().unwrap(),
            src_port: None,
            dst_port: port,
        }
    }

    #[test]
    fn the_return_path_is_recognised_by_its_source() {
        let mut table = EndpointTable::new();
        table.insert(Endpoint::new("10.99.0.1".parse().unwrap(), 8443), "web");

        // A reply from the claimed address, on its way back to a visitor: the
        // destination is the visitor's, so only the source identifies it.
        let reply = PacketInfo {
            protocol: TCP,
            src: "10.99.0.1".parse().unwrap(),
            dst: "10.10.0.2".parse().unwrap(),
            src_port: Some(8443),
            dst_port: Some(41000),
        };
        assert_eq!(table.lookup_in(&reply, Direction::Source), Some(&"web"));
        assert_eq!(table.lookup_in(&reply, Direction::Destination), None);

        // Unrelated local traffic that the kernel happened to route here is
        // not claimed by anybody.
        let other = PacketInfo {
            protocol: TCP,
            src: "10.30.0.2".parse().unwrap(),
            dst: "10.10.0.2".parse().unwrap(),
            src_port: Some(5000),
            dst_port: Some(41000),
        };
        assert_eq!(table.lookup_in(&other, Direction::Source), None);
    }

    #[test]
    fn a_port_carrying_packet_routes_by_ip_and_port() {
        let mut table = EndpointTable::new();
        table.insert(Endpoint::new("10.99.0.1".parse().unwrap(), 443), "web");
        table.insert(Endpoint::new("10.99.0.1".parse().unwrap(), 8443), "admin");

        assert_eq!(
            table.lookup(&info("10.99.0.1", Some(443), TCP)),
            Some(&"web")
        );
        assert_eq!(
            table.lookup(&info("10.99.0.1", Some(8443), TCP)),
            Some(&"admin")
        );
        // A port nobody claimed on an address somebody did is still unclaimed.
        assert_eq!(table.lookup(&info("10.99.0.1", Some(80), TCP)), None);
        assert_eq!(table.lookup(&info("10.99.0.9", Some(443), TCP)), None);
    }

    #[test]
    fn a_portless_packet_needs_a_single_claimant_on_that_address() {
        let mut table = EndpointTable::new();
        table.insert(Endpoint::new("10.99.0.1".parse().unwrap(), 443), "web");
        assert_eq!(table.lookup(&info("10.99.0.1", None, 1)), Some(&"web"));

        // A second claimant on the same address makes ICMP ambiguous: it is
        // dropped, not guessed.
        table.insert(Endpoint::new("10.99.0.1".parse().unwrap(), 8443), "admin");
        assert_eq!(table.lookup(&info("10.99.0.1", None, 1)), None);
    }

    #[test]
    fn removal_keeps_the_address_count_honest() {
        let mut table = EndpointTable::new();
        let a = Endpoint::new("10.99.0.1".parse().unwrap(), 443);
        let b = Endpoint::new("10.99.0.1".parse().unwrap(), 8443);
        table.insert(a, 1);
        table.insert(b, 2);
        assert_eq!(table.len(), 2);

        assert_eq!(table.remove(&a), Some(1));
        assert_eq!(
            table.remove(&a),
            None,
            "removing twice is not twice a count"
        );
        // One claimant left: an ICMP packet is unambiguous again.
        assert_eq!(table.lookup(&info("10.99.0.1", None, 1)), Some(&2));
        assert_eq!(table.remove(&b), Some(2));
        assert!(table.is_empty());
        assert_eq!(table.lookup(&info("10.99.0.1", None, 1)), None);
    }

    #[test]
    fn a_replaced_value_counts_once() {
        let mut table = EndpointTable::new();
        let ep = Endpoint::new("10.99.0.1".parse().unwrap(), 443);
        assert_eq!(table.insert(ep, 1), None);
        assert_eq!(table.insert(ep, 2), Some(1));
        assert_eq!(table.len(), 1);
        // The address still has exactly one claimant, so ICMP still routes.
        assert_eq!(table.lookup(&info("10.99.0.1", None, 1)), Some(&2));
    }
}
