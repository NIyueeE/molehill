//! Transparent (L3) services: the client owns the public `ip:port`.
//!
//! A transparent service is never bound by the server. The claimed address is
//! carried by the **client's** TUN device, and the visitor's packets reach the
//! client's kernel *as packets*: the backend sees the visitor's real address,
//! TCP keeps its end-to-end semantics, and the server holds no connection
//! state for the flow. This module installs no route, rule or firewall rule on
//! any platform: Linux attaches to the operator's persistent device, while
//! macOS and Windows create the ephemeral device and leave the routes to the
//! operator.
//!
//! Both ends share the same two primitives:
//!
//! - [`ip::parse`] reads the routed destination of one IP packet without
//!   allocating;
//! - [`EndpointTable`] decides which packet belongs to which service — a
//!   router on the server, an allow-list on the client.
//!
//! Linux and macOS run both ends; Windows can serve a transparent client but
//! cannot be one (its stack has no source-address policy routing). The local
//! frame shape is `Vnet` on a Linux device with segmentation offload and `Ip`
//! everywhere else; the server negotiates one shape for the claim, and
//! [`frame`] converts at a Linux boundary when the pair is mixed.

pub mod check;
pub mod flow;
pub mod frame;
pub mod hub;
pub mod ip;
#[cfg(mh_l3_client)]
pub mod lanes;
pub mod tun;

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::transparent::ip::{PacketInfo, ParseError, TCP, UDP};

/// `MOLEHILL_L3_WIRE=ip` forces the plain-IP wire on either end, which is how
/// the Linux acceptance harness exercises the mixed-format conversion without a
/// macOS or Windows host. Any other value leaves the negotiated format alone.
pub fn force_ip_wire() -> bool {
    static FORCE: LazyLock<bool> = LazyLock::new(|| match std::env::var("MOLEHILL_L3_WIRE") {
        Ok(value) if value.eq_ignore_ascii_case("ip") => true,
        Ok(value) if value.eq_ignore_ascii_case("vnet") || value.eq_ignore_ascii_case("auto") => {
            false
        }
        Ok(value) => {
            tracing::debug!("Ignoring MOLEHILL_L3_WIRE={value:?}: use `ip`, `vnet` or `auto`");
            false
        }
        Err(_) => false,
    });
    *FORCE
}

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
        match self.ip {
            IpAddr::V4(ip) => write!(f, "{ip}:{}", self.port),
            IpAddr::V6(ip) => write!(f, "[{ip}]:{}", self.port),
        }
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
    ///
    /// Test-only since placement became a table: the packet path edits a
    /// claim's slot in place ([`Self::lookup_endpoint_mut`]), and the reader
    /// that is left is the one the routing tests assert with.
    #[cfg(test)]
    pub fn lookup_endpoint(&self, endpoint: &Endpoint) -> Option<&V> {
        self.entries.get(&(endpoint.ip, endpoint.port))
    }

    /// The value registered for exactly this endpoint, mutably: a member set is
    /// edited in place — a slot taken or vacated — without the claim leaving
    /// the table.
    pub fn lookup_endpoint_mut(&mut self, endpoint: &Endpoint) -> Option<&mut V> {
        self.entries.get_mut(&(endpoint.ip, endpoint.port))
    }

    /// Every registered endpoint with its value, in no particular order, for
    /// the telemetry that reports per-claim state.
    pub fn iter(&self) -> impl Iterator<Item = (Endpoint, &V)> {
        self.entries
            .iter()
            .map(|((ip, port), value)| (Endpoint::new(*ip, *port), value))
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
    /// The version nibble is neither IPv4 nor IPv6.
    NotIp,
    /// Too short, or a header length that runs past the packet.
    Malformed,
    /// Well formed, but no registered endpoint claims its destination.
    Unclaimed,
}

impl From<ParseError> for DropReason {
    fn from(e: ParseError) -> Self {
        match e {
            ParseError::NotIp => Self::NotIp,
            ParseError::Malformed => Self::Malformed,
        }
    }
}

/// Counters for the transparent data path, behind `MOLEHILL_L3_STATS=1`.
#[derive(Default)]
pub struct Stats {
    pub forwarded: AtomicU64,
    /// Packets whose version is neither 4 nor 6.
    pub dropped_not_ip: AtomicU64,
    pub dropped_malformed: AtomicU64,
    pub dropped_unclaimed: AtomicU64,
    /// Packets whose channel was gone (reconnect window): UDP semantics, the
    /// visitor's own transport retries.
    pub dropped_no_channel: AtomicU64,
    /// Frames read from the tunnel that failed to parse.
    pub channel_errors: AtomicU64,
    /// Packets the device handed this end, whatever happened to them next —
    /// the denominator every rate below is measured against.
    ///
    /// Kept apart from `forwarded` because a packet can be read and then
    /// dropped for a reason of its own (unclaimed, malformed), and a reader
    /// that is asked "did you keep up" must answer against what it was *given*,
    /// not against what it accepted.
    pub device_offered: AtomicU64,
    /// Wall-clock ms of the last device read that produced packets.
    pub last_read_ms: AtomicU64,
    /// The longest gap between two such reads, and when that gap ended.
    ///
    /// A reader that stops draining its device for seconds is invisible in the
    /// per-packet counters: the packets it never read are not dropped by this
    /// process, they are dropped by the kernel's queue, and a visitor sees a
    /// stall rather than a loss. This is the number that says whether the stall
    /// is *here*. The gap is measured between reads that returned data, so an
    /// idle device does not look like a stall, and the timestamp is kept so a
    /// stall can be placed against the workload's own clock.
    pub max_read_gap_ms: AtomicU64,
    pub max_read_gap_at_ms: AtomicU64,
    /// The same question for the *write* side: the longest gap between two
    /// batches the carrier accepted, and when it ended.
    ///
    /// The reader being healthy does not mean the lane is draining: a carrier
    /// that stops accepting bytes leaves the reader's queue full, and the
    /// overflow is then counted as this end's backpressure rather than as a
    /// stall. Together the two gaps say which half stopped.
    pub last_write_ms: AtomicU64,
    pub max_write_gap_ms: AtomicU64,
    pub max_write_gap_at_ms: AtomicU64,
    /// Packets the device offered in the last complete second, and the same for
    /// what this end forwarded, with the counters they were measured against.
    ///
    /// The two gaps above say whether this process *stopped*; they cannot say
    /// whether it ever *kept up*. A reader that drains 90 % of what is offered
    /// has no gap at all and still loses a tenth of every flow's packets —
    /// which is exactly the shape of a loss that no in-process counter reports
    /// (the packets it never read are the kernel's to drop) and that reaches a
    /// visitor only as retransmits. A per-second reading is what turns "kept up
    /// on average" into "kept up except between t=70 s and t=85 s", which is
    /// the only form in which this question can be answered: the wedge lives in
    /// a five-second window, and a whole-run average cannot see one.
    ///
    /// A whole-run average would also be actively misleading here — a reader
    /// that keeps up for 95 % of a run and starves for 5 % reads as 95 % on the
    /// average and as the cause of the wedge on the timeline.
    pub device_offered_rate: AtomicU64,
    pub forwarded_rate: AtomicU64,
    /// The counters as they stood at the previous sample: a rate is a *delta*,
    /// and a counter divided by its own elapsed time is the run's average
    /// (which is the number this exists to replace).
    pub rate_offered_was: AtomicU64,
    pub rate_forwarded_was: AtomicU64,
    /// When the rate counters were last sampled, so each second is measured
    /// rather than assumed: a reporter that drifted would divide by its own
    /// error.
    pub rate_at_ms: AtomicU64,
}

/// Unix milliseconds, for the stall detector's own clock.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A count's per-second rate from a millisecond window.
///
/// The rate is scaled *before* the division; `u128` keeps a large delta from
/// overflowing, and the conversion saturates rather than wrapping. The
/// previous form divided by milliseconds and therefore under-reported by
/// 1000x — and, with integer truncation, reported zero below 1000 packets
/// per window.
fn per_second(delta: u64, elapsed_ms: u64) -> u64 {
    let rate = u128::from(delta).saturating_mul(1_000) / u128::from(elapsed_ms.max(1));
    u64::try_from(rate).unwrap_or(u64::MAX)
}

impl Stats {
    pub fn count_drop(&self, reason: DropReason) {
        let counter = match reason {
            DropReason::NotIp => &self.dropped_not_ip,
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

    /// One device read that produced packets: close the previous gap.
    pub fn note_device_read(&self) {
        let now = unix_ms();
        let prev = self.last_read_ms.swap(now, Ordering::Relaxed);
        if prev != 0 {
            let gap = now.saturating_sub(prev);
            if gap > self.max_read_gap_ms.load(Ordering::Relaxed) {
                self.max_read_gap_ms.store(gap, Ordering::Relaxed);
                self.max_read_gap_at_ms.store(now, Ordering::Relaxed);
            }
        }
    }

    /// `packets` the device handed this end, whatever happens to them next.
    pub fn count_device_offered_by(&self, packets: usize) {
        self.device_offered
            .fetch_add(packets as u64, Ordering::Relaxed);
    }

    /// Sample the two rate counters against the previous sample, so a reader is
    /// reported at the rate it actually ran at rather than at the rate a
    /// whole-run average implies.
    ///
    /// The delta is taken against the counters as they stood at the *previous*
    /// sample, and the window against the previous sample's clock: both are
    /// needed, because dividing a counter by its own elapsed time yields the
    /// run's average — the very number this replaces — and assuming a one-second
    /// window makes the rate drift with the reporter.
    pub fn sample_rates(&self) {
        self.sample_rates_at(unix_ms());
    }

    /// The clock-taking half of [`Self::sample_rates`]: separated so tests
    /// can measure an exact window instead of sleeping on the wall clock.
    fn sample_rates_at(&self, now: u64) {
        let prev = self.rate_at_ms.swap(now, Ordering::Relaxed);
        let offered = self.device_offered.load(Ordering::Relaxed);
        let forwarded = self.forwarded.load(Ordering::Relaxed);
        let was_offered = self.rate_offered_was.swap(offered, Ordering::Relaxed);
        let was_forwarded = self.rate_forwarded_was.swap(forwarded, Ordering::Relaxed);
        // The first sample only sets the baseline: there is no previous second
        // to measure, and publishing a rate from a partial one would be a number
        // with no window behind it.
        if prev == 0 {
            self.device_offered_rate.store(0, Ordering::Relaxed);
            self.forwarded_rate.store(0, Ordering::Relaxed);
            return;
        }
        let elapsed = now.saturating_sub(prev);
        self.device_offered_rate.store(
            per_second(offered.saturating_sub(was_offered), elapsed),
            Ordering::Relaxed,
        );
        self.forwarded_rate.store(
            per_second(forwarded.saturating_sub(was_forwarded), elapsed),
            Ordering::Relaxed,
        );
    }

    /// One batch the carrier accepted: close the previous write gap.
    pub fn note_carrier_write(&self) {
        let now = unix_ms();
        let prev = self.last_write_ms.swap(now, Ordering::Relaxed);
        if prev != 0 {
            let gap = now.saturating_sub(prev);
            if gap > self.max_write_gap_ms.load(Ordering::Relaxed) {
                self.max_write_gap_ms.store(gap, Ordering::Relaxed);
                self.max_write_gap_at_ms.store(now, Ordering::Relaxed);
            }
        }
    }

    /// Whether the operator asked for the stats line.
    pub fn enabled() -> bool {
        std::env::var_os("MOLEHILL_L3_STATS").is_some()
    }
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
    fn an_ipv6_endpoint_is_bracketed_in_logs() {
        let endpoint = Endpoint::new("2001:db8::1".parse().unwrap(), 8443);
        assert_eq!(endpoint.to_string(), "[2001:db8::1]:8443");
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

    /// The rate is a *per-second* delta against the previous sample, and the
    /// first sample only sets the baseline. The clock is injected so the
    /// window is exact: a test that slept for a second could not tell a
    /// per-millisecond division from a per-second one.
    #[test]
    fn a_rate_is_a_per_second_delta_since_the_last_sample() {
        let stats = Stats::default();

        stats.count_device_offered_by(10);
        stats.sample_rates_at(1_000);
        // Nothing to measure yet: the first sample is a baseline, not a rate.
        assert_eq!(stats.device_offered_rate.load(Ordering::Relaxed), 0);
        assert_eq!(stats.rate_offered_was.load(Ordering::Relaxed), 10);

        // One second in which 40 more packets were offered and 30 forwarded.
        stats.count_device_offered_by(40);
        stats.count_forwarded_by(30);
        stats.sample_rates_at(2_000);
        assert_eq!(
            stats.device_offered_rate.load(Ordering::Relaxed),
            40,
            "40 packets in one second is 40/s, not 40 per millisecond"
        );
        assert_eq!(stats.forwarded_rate.load(Ordering::Relaxed), 30);
        assert_eq!(stats.rate_offered_was.load(Ordering::Relaxed), 50);

        // A half-second window scales the delta up, and an empty window reads
        // zero: both prove the sample uses the measured elapsed time rather
        // than assuming one second.
        stats.count_device_offered_by(20);
        stats.sample_rates_at(2_500);
        assert_eq!(stats.device_offered_rate.load(Ordering::Relaxed), 40);
        stats.sample_rates_at(3_500);
        assert_eq!(stats.device_offered_rate.load(Ordering::Relaxed), 0);
        assert_eq!(
            stats.rate_offered_was.load(Ordering::Relaxed),
            70,
            "the baseline must be the counter as it stands now"
        );
    }
}
