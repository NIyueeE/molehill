//! What is in flight, what the peer has, what is lost, and when to send again.
//!
//! The rules are the ones that survive a real path: a packet is lost when three
//! later packets have been acknowledged (reordering tolerance), or when it has
//! been outstanding for longer than the path's reordering window (nine eighths
//! of the round trip), and a retransmission timer backs off exponentially until
//! something is acknowledged. Everything here is driven by a clock the caller
//! passes in, so the whole state machine is unit-testable without a socket.
//!
//! What it deliberately does not do is decide how much to send (that is the
//! congestion controller's job) or remember the bytes themselves: a lost packet
//! is reported as the range it carried, and the stream that owns those bytes
//! still has them.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::wire::Ack;

/// How many later packets must be acknowledged before a gap is read as loss.
///
/// Three is TCP's duplicate-acknowledgement threshold and QUIC's packet
/// threshold: at two, ordinary reordering on a multipath or a bond is read as
/// loss and the carrier retransmits data the peer already has.
const PACKET_THRESHOLD: u32 = 3;

/// The reordering window as a fraction of the round trip: nine eighths.
const TIME_THRESHOLD_NUM: u32 = 9;
const TIME_THRESHOLD_DEN: u32 = 8;

/// The retransmission timer before any round trip has been measured.
///
/// 333 ms is the value RFC 9002 §6.2.1 gives the same state (`kInitialRto`), and
/// this carrier had been using half a second. The difference is not academic: a
/// lane whose first handshake packet is lost waits this timer before it can open
/// anything, and a pool of four lanes dialed under one percent loss measured
/// **504 ms** of establishment with 500 ms here, which is long enough to surface
/// as a visitor arriving before the pool is ready. Backoff doubles from here, so
/// a genuinely slow path still gets its second, fourth and eighth chance.
const INITIAL_RTO: Duration = Duration::from_millis(333);

/// The retransmission timer's floor for a **wide** flight: below this the timer
/// fires on scheduling noise rather than on a lost packet.
///
/// Measured, not chosen: on a calm host the transparent path's sixteen-flow arm
/// runs 16/16 in **1.517 s** with this floor and in **0.344 s** with 20 ms, so
/// the floor dominates that workload's latency — a lost packet on a
/// request/response flow costs the floor, and a two-lane claim loses a few.
///
/// Lowering it *globally* is refuted twice over: an interleaved A/B of two
/// builds differing only in this constant failed **six of six** bulk cells with
/// `control socket has closed unexpectedly` against **one of six** here, and a
/// second A/B with [`DEAD_FLOOR`] in place failed every sample again. So this is
/// the floor for a flight worth protecting — which is why the cheap floor below
/// is scoped to a flight too small to storm (see [`MIN_RTO_SMALL_FLIGHT`]).
const MIN_RTO: Duration = Duration::from_millis(200);

/// The floor for a flight of one or two packets: the opening handshake's cost.
///
/// The measured defect this exists for is the *open*: `open_channel` (the Noise
/// KK handshake) cost **502.3-503.9 ms** under load against a 4 µs hello and a
/// 0.2-1.7 ms acknowledgement on the same lane — exactly `INITIAL_RTO`, because
/// the handshake's first packet is lost or late and a small flight has no
/// round-trip sample to base a timer on yet.
///
/// A global 20 ms floor is refuted (see [`MIN_RTO`]): it made a *wide* flight
/// re-send itself on scheduling noise, and a whole-flight re-send is a storm.
/// A flight of one or two packets cannot storm — a duplicate of a single
/// handshake packet is dropped by its offset and costs one datagram — so the
/// cheap floor is applied **only** while the connection has at most
/// [`SMALL_FLIGHT_PACKETS`] packets outstanding. Once the burst starts, the
/// timer is the wide one again.
///
/// It also only binds on a fast path: the floor is a *clamp* under
/// `smoothed + 4 x variance`, so a 200 ms path's estimate already sits above it
/// and nothing changes there.
pub(crate) const MIN_RTO_SMALL_FLIGHT: Duration = Duration::from_millis(20);

/// How many packets in flight still count as "the handshake", not "the burst".
///
/// Two covers the request/response pair a stream open is (the handshake message
/// and its answer) with no room for a bulk run to qualify.
const SMALL_FLIGHT_PACKETS: usize = 2;

/// The timer a **small flight** arms when the lane has no round-trip sample yet.
///
/// [`INITIAL_RTO`] is the right timer for a flight whose width is unknown: at
/// 333 ms a re-send of a thousand packets costs a thousand duplicates, so erring
/// late is cheap. It is the wrong timer for a stream opening on a lane that has
/// never carried a sample, and that case is measured: the dialer's own phase
/// split puts a cluster of opens at **335-337 ms**, which is `INITIAL_RTO` to the
/// millisecond, and the server's corresponding wait for the first stream byte
/// reads 752 ms — a lost handshake packet, twice.
///
/// A flight of one or two packets cannot pay that: a duplicate is one datagram
/// the peer drops by offset. So an unmeasured small flight arms this instead, and
/// the exponential backoff bounds the duplicates on a genuinely long path (20,
/// 40, 80, 160 ms — four re-sends before a 200 ms round trip has even answered
/// once), while [`DEAD_FLOOR`] keeps a busy host from being read as a dead path.
const SMALL_FLIGHT_INITIAL_RTO: Duration = Duration::from_millis(20);

/// The retransmission timer's ceiling, which is also the idle limit of a silent
/// peer.
const MAX_RTO: Duration = Duration::from_secs(30);

/// How many retransmissions without a single acknowledgement before the path is
/// called dead.
///
/// Without it a black-holed path is a connection that backs off to the ceiling
/// and stays there, holding a lane that will never carry anything.
const DEAD_LINK: u32 = 10;

/// The shortest silence that can end a path, whatever the timer's floor is.
///
/// `DEAD_LINK` counts unanswered retransmissions, and that count only measures
/// silence *through the retransmission timer* — which is fine at the default
/// floor, where ten backed-off retries span minutes, and wrong at a cheap one,
/// where they span a fraction of a second. Measured: a build with `MIN_RTO` at
/// 20 ms failed **six of six** bulk cells with `control socket has closed
/// unexpectedly`, against **one of six** at 200 ms, because a busy host's
/// ordinary few hundred milliseconds of scheduling delay was being read as a
/// dead lane.
///
/// Five seconds is more than twice the keepalive interval and a quarter of the
/// silence limit, so a genuinely dead path is still ended by the count, and a
/// cheap timer can no longer end one by itself.
const DEAD_FLOOR: Duration = Duration::from_secs(5);

/// How long the record of a packet declared lost is kept, waiting for an
/// acknowledgement that may still arrive.
///
/// A packet is declared lost on the evidence of *later* packets, not on
/// evidence about itself: the original may merely be reordered, and its
/// acknowledgement can arrive after the retransmission was already queued.
/// Dropping the record when the loss is declared is what made that
/// acknowledgement land nowhere: the stream's send buffer then never learned
/// that the range had arrived, its acknowledgement frontier stopped at the
/// hole, and everything written after it piled up behind a frontier that could
/// not move — measured at 6.7 GiB resident in a 40-second cell before this
/// existed. Three seconds is far past any reordering window this carrier
/// tolerates (nine eighths of a round trip) and far below the point where
/// holding the record costs anything worth measuring.
///
/// `pub(crate)` so the connection's own tests can advance a simulated clock past
/// it, rather than hard-coding a duration that goes stale the day this number
/// moves.
pub(crate) const LOST_HOLD: Duration = Duration::from_secs(3);
/// The most lost records kept at once, so a path that drops everything cannot
/// grow the table without bound before the dead-link rule ends it.
///
/// Sixteen megabytes of flight is about twelve thousand packets; this is a
/// third of that, which is the order a real path's loss burst reaches.
const MAX_LOST: usize = 4096;

/// The range of one stream a packet carried, kept so a loss can be re-sent
/// without the stream having to say what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentRange {
    /// Which stream.
    pub(crate) stream: u32,
    /// Where the bytes start.
    pub(crate) offset: u64,
    /// How many bytes.
    pub(crate) len: u16,
    /// Whether the range ends the stream.
    pub(crate) fin: bool,
}

/// One packet on the wire, kept until it is acknowledged or declared lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sent {
    /// Its packet number.
    pub(crate) number: u32,
    /// When it was handed to the socket.
    pub(crate) time: Instant,
    /// What it carried, when that is something the stream can re-send.
    pub(crate) range: Option<SentRange>,
    /// When it was declared lost, if it was.
    ///
    /// A lost packet has left the wire, but its record stays in the table until
    /// an acknowledgement of it arrives (see [`LOST_HOLD`]). It is not in
    /// flight: the byte total and the congestion window do not count it.
    pub(crate) lost_at: Option<Instant>,
    /// Whether this packet is part of a stream's **opening flight**.
    ///
    /// A stream's packets before its first acknowledgement are the handshake,
    /// which is one or two datagrams in the air however busy the lane is, so a
    /// lost one must be re-sent on the cheap floor (see `MIN_RTO_SMALL_FLIGHT`).
    /// Keying that on [`Recovery::live`] instead made the exemption work only on
    /// an idle lane — the one place an opening is never lost.
    pub(crate) opening: bool,
}

impl Recovery {
    #[cfg(test)]
    /// Seed the estimate, so a test can put a lane on a path of its choosing.
    ///
    /// Only the ceiling's calibration uses it: production samples come from
    /// acknowledgements, and a harness that sends and acknowledges inside one
    /// instant reports a round trip of zero, which is not a path anyone runs on.
    pub(crate) fn seed_rtt(&mut self, rtt: Duration) {
        self.rtt.sample(rtt);
        self.rtt.sample(rtt);
    }

    /// The smoothed round trip, once a sample exists.
    ///
    /// Read by the sender's window ceiling, which scales with the path (see
    /// `Conn::window_ceiling`).
    pub(crate) fn smoothed_rtt(&self) -> Option<Duration> {
        self.rtt.smoothed
    }

    /// The path's own length: the smallest round trip ever measured.
    pub(crate) fn min_rtt(&self) -> Option<Duration> {
        self.rtt.min
    }
}

/// The round-trip estimate.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Rtt {
    /// The smoothed estimate, once there is one.
    pub(crate) smoothed: Option<Duration>,
    /// The mean deviation, for the timer's margin.
    pub(crate) variance: Duration,
    /// The most recent sample.
    pub(crate) latest: Option<Duration>,
    /// The smallest sample ever seen: the path's own length, without any queue.
    ///
    /// The window ceiling is derived from this rather than from the smoothed
    /// estimate, because a ceiling derived from a queued estimate raises its own
    /// ceiling: a deeper queue makes the estimate larger, the larger estimate
    /// admits a deeper queue, and the lane latches into a regime where every small
    /// packet pays for it. Measured: the bad mode of the first stage has a median
    /// four times the good mode's with the same bulk rate, and 98 of its 183
    /// seconds above 60 ms against the good mode's 8.
    pub(crate) min: Option<Duration>,
}

/// The floor under any round-trip sample and the reordering window.
///
/// A loopback path answers inside a few microseconds, and a zero estimate makes
/// `loss delay` zero: every packet still in flight then looks "older than the
/// window" the moment any acknowledgement arrives, so the connection
/// retransmits data the peer already has on every pass. One millisecond is the
/// timer granularity of the hosts this runs on, and it is the same floor TCP
/// and QUIC use for the same reason.
const GRANULARITY: Duration = Duration::from_millis(1);

impl Rtt {
    fn sample(&mut self, sample: Duration) {
        let sample = sample.max(GRANULARITY);
        self.latest = Some(sample);
        // The smallest ever seen, which is the path and nothing else.
        self.min = Some(self.min.map_or(sample, |min| min.min(sample)));
        match self.smoothed {
            None => {
                self.smoothed = Some(sample);
                self.variance = sample / 2;
            }
            Some(smoothed) => {
                // The standard 1/8 and 1/4 filters: slow enough that one delayed
                // acknowledgement does not move the estimate, fast enough to
                // follow a path that changes.
                let drift = smoothed.saturating_sub(sample);
                self.variance = (self.variance * 3 + drift) / 4;
                self.smoothed = Some(((smoothed * 7 + sample) / 8).max(GRANULARITY));
            }
        }
    }

    /// The reordering window: nine eighths of the round trip.
    fn loss_delay(&self) -> Duration {
        // The *larger* of the latest and the smoothed estimate: a fast probe
        // can sample a millisecond while the bulk data in flight was queued for
        // ten, and a window built on the probe alone declares every queued
        // packet lost. The rule is the one QUIC adopted for the same reason.
        let base = match (self.latest, self.smoothed) {
            (Some(latest), Some(smoothed)) => latest.max(smoothed),
            (Some(one), None) | (None, Some(one)) => one,
            (None, None) => INITIAL_RTO,
        }
        .max(GRANULARITY);
        (base.saturating_mul(TIME_THRESHOLD_NUM) / TIME_THRESHOLD_DEN).max(GRANULARITY)
    }

    /// The retransmission timer for the current estimate.
    ///
    /// `small_flight` selects the cheap floor: a connection with one or two
    /// packets outstanding is opening a stream — a lost handshake packet there
    /// costs the floor and nothing else can be re-sent by mistake — while a
    /// wider flight keeps the floor that survived every bulk A/B.
    fn rto(&self, small_flight: bool) -> Duration {
        let base = match self.smoothed {
            Some(smoothed) => smoothed + self.variance.saturating_mul(4),
            // A lane that has carried nothing yet: the wide value unless the
            // flight is too small to storm (see `SMALL_FLIGHT_INITIAL_RTO`).
            None if small_flight => SMALL_FLIGHT_INITIAL_RTO,
            None => INITIAL_RTO,
        };
        let floor = if small_flight {
            MIN_RTO_SMALL_FLIGHT
        } else {
            MIN_RTO
        };
        base.clamp(floor, MAX_RTO)
    }
}

/// What one acknowledgement did.
#[derive(Debug, Default)]
pub(crate) struct Acked {
    /// Packets this acknowledgement proved arrived.
    pub(crate) newly: Vec<Sent>,
    /// Packets it proved lost: three behind the largest, or past the window.
    pub(crate) lost: Vec<Sent>,
    /// Whether it advanced the largest number seen, which is what makes it a
    /// round-trip sample.
    pub(crate) progress: bool,
    /// How many stream bytes it acknowledged, which is what grows the window.
    pub(crate) bytes: usize,
    /// Whether a packet it proved lost was part of an opening flight.
    pub(crate) opening_lost: bool,
}

/// Why the timer fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// A packet passed the reordering window.
    Loss,
    /// Nothing has been acknowledged for a whole retransmission timer.
    Timeout,
    /// Retransmissions without an acknowledgement reached the limit: the path
    /// is not slow, it is gone.
    Dead,
}

/// The in-flight table, the round-trip estimate and both timers.
#[derive(Debug)]
pub(crate) struct Recovery {
    next: u32,
    /// The number the most recent tracked packet took.
    last: u32,
    in_flight: BTreeMap<u32, Sent>,
    /// Packets that are off the wire but still remembered, keyed by when they
    /// were declared lost, so the reaper is a prefix walk.
    lost: BTreeSet<(Instant, u32)>,
    /// Packets actually on the wire: `in_flight` minus the lost records.
    ///
    /// Kept as a counter rather than computed, because the timer arms on every
    /// send and a scan of the table there would grow with the flight.
    live: usize,
    /// Stream bytes the table holds, kept exact by [`Self::retire`].
    flight_bytes: u64,
    rtt: Rtt,
    /// When the timer should fire, if anything is in flight.
    timer: Option<Instant>,
    /// Retransmission timers that expired without an acknowledgement.
    silent: u32,
    /// When the current run of silent timers began: the first of them.
    silent_since: Option<Instant>,
    /// The backoff applied to the timer since the last acknowledgement.
    backoff: u32,
}

impl Recovery {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            next: 0,
            last: 0,
            in_flight: BTreeMap::new(),
            lost: BTreeSet::new(),
            live: 0,
            flight_bytes: 0,
            rtt: Rtt::default(),
            timer: None,
            silent: 0,
            silent_since: None,
            backoff: 0,
        }
    }

    /// Reserve the next packet number, whether or not it is tracked.
    /// Start numbering just below the wrap, so a test can walk the numbers through it.
    ///
    /// The counter is a `u32` and the wire carries it as one: after four billion packets it wraps,
    /// and every comparison against it — the cumulative frontier, the sixty-four-number bitmap, the
    /// retirement loop — has to survive that. Nothing in the suite had ever crossed the boundary.
    #[cfg(test)]
    pub(crate) fn seed_number(&mut self, next: u32) {
        self.next = next;
    }

    pub(crate) fn next_number(&mut self) -> u32 {
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        self.last = number;
        number
    }

    /// The number a control packet repeats.
    ///
    /// Control packets are not retransmitted, so one that took a fresh number
    /// would leave a hole in the peer's cumulative frontier as soon as it was
    /// lost, and nothing would ever fill it. They repeat the last data packet's
    /// number and are marked so the peer does not read them as news.
    pub(crate) fn last_number(&self) -> u32 {
        self.last
    }

    /// Bytes on the wire and not yet acknowledged.
    ///
    /// Kept as a running total rather than a sum over the table: the window this
    /// feeds is asked on every write, and a sum over a wide flight per write is
    /// a cost that grows with the bandwidth-delay product.
    /// How many packets the table is holding, and how old its oldest data is.
    ///
    /// A record that is neither acknowledged nor declared lost sits in the table
    /// for ever, and the age gate reads its timestamp: the oldest age then grows
    /// without bound and refuses every write on the lane, with no backlog and no
    /// credit block to show for it. Two numbers make that visible.
    pub(crate) fn table_stats(&self) -> (usize, Option<Duration>) {
        let oldest = self
            .in_flight
            .values()
            .filter(|sent| sent.lost_at.is_none())
            .map(|sent| sent.time)
            .min();
        (
            self.in_flight.len(),
            oldest.map(|time| Instant::now().saturating_duration_since(time)),
        )
    }

    /// The lowest and highest packet numbers still in the table.
    ///
    /// For the stall line, and for the question it exists to answer: whether the
    /// numbers a sender is waiting for are ones the peer's acknowledgement block
    /// can still name at all (`HANDOFF.md` §4.107).
    pub(crate) fn number_range(&self) -> Option<(u32, u32)> {
        Some((
            *self.in_flight.keys().next()?,
            *self.in_flight.keys().next_back()?,
        ))
    }

    pub(crate) fn flight_bytes(&self) -> u64 {
        self.flight_bytes
    }

    /// Retire every record the peer's byte prefix has already confirmed.
    ///
    /// **The escape hatch the design already had, applied to the number that
    /// gates admission** (`HANDOFF.md` §4.108). A packet number lost for good
    /// stops the peer's cumulative number frontier, and the acknowledgement
    /// bitmap only carries 64 numbers behind the largest seen — so a record older
    /// than that window can never be reported again by the block. The peer's
    /// *byte* prefix cannot stall that way (it is the contiguous prefix of one
    /// stream), and `Conn::apply_ack` already used it to free the stream's send
    /// buffer. It never used it here, so the same bytes stayed in the flight for
    /// ever: the congestion window closed on a lane whose peer had received
    /// everything, the writer was refused, no new data went out, and the flow
    /// deadlocked with 32 KB in flight against a 13 KB window.
    ///
    /// A record is retired only when its **whole** range sits below the prefix —
    /// a partially confirmed range is still owed from its own first byte, and
    /// retiring it would leave a hole the prefix cannot describe. `len == 0`
    /// records (a bare finish) are left to the number path, because a prefix of
    /// `offset` says the bytes arrived and not that the peer saw the close.
    ///
    /// Answers `(freed, late)`: the bytes this removed from the flight total, and
    /// how many of them belonged to a record that had **already been declared
    /// lost**.
    ///
    /// The second number is the one the ledger needs (`Conn::apply_ack`). A declared
    /// loss leaves the flight but keeps its record, so `take` retires it without
    /// touching `flight_bytes` — which is why the first number alone under-counts
    /// what the peer confirmed, and why the duplicate share built on it would look
    /// larger than it is. Those bytes are confirmed all the same: the peer has them,
    /// by the *other* path, after this end had already decided it did not.
    pub(crate) fn confirm_prefix(&mut self, stream: u32, prefix: u64) -> (u64, u64) {
        let confirmed: Vec<u32> = self
            .in_flight
            .iter()
            .filter(|(_, sent)| {
                sent.range.is_some_and(|range| {
                    range.stream == stream
                        && range.len > 0
                        && range.offset.saturating_add(u64::from(range.len)) <= prefix
                })
            })
            .map(|(number, _)| *number)
            .collect();
        let before = self.flight_bytes;
        let mut late = 0u64;
        for number in confirmed {
            // Read the record before `take` removes it: `lost_at` is what separates
            // "this end was still waiting for the number" from "it had given up".
            if let Some(sent) = self.in_flight.get(&number)
                && sent.lost_at.is_some()
            {
                late = late.saturating_add(sent.range.map_or(0, |range| u64::from(range.len)));
            }
            // `take` is the one place that knows how to remove a record without
            // double-retiring a declared loss, so the accounting stays in one pair
            // of hands.
            let _ = self.take(number);
        }
        (before.saturating_sub(self.flight_bytes), late)
    }

    /// Drop one packet's bytes from the flight total.
    ///
    /// Called exactly once for every packet that was on the wire, whichever
    /// path removes it: an acknowledgement, a loss declaration, or a probe.
    fn retire(&mut self, sent: &Sent) {
        if let Some(range) = sent.range {
            self.flight_bytes = self.flight_bytes.saturating_sub(u64::from(range.len));
        }
    }

    /// Remove one packet's record, keeping the counters exact.
    fn take(&mut self, number: u32) -> Option<Sent> {
        let sent = self.in_flight.remove(&number)?;
        if let Some(at) = sent.lost_at {
            self.lost.remove(&(at, number));
        } else {
            self.live = self.live.saturating_sub(1);
            self.retire(&sent);
        }
        Some(sent)
    }

    /// Declare one packet lost: it leaves the wire, but its record stays.
    ///
    /// The record is what a late acknowledgement lands on. Answering `None`
    /// for one already declared lost keeps the caller from re-sending the same
    /// range twice for one report.
    fn declare_lost(&mut self, now: Instant, number: u32) -> Option<Sent> {
        if self.lost.len() >= MAX_LOST {
            self.reap(now);
        }
        let entry = self.in_flight.get_mut(&number)?;
        if entry.lost_at.is_some() {
            return None;
        }
        entry.lost_at = Some(now);
        let sent = *entry;
        self.live = self.live.saturating_sub(1);
        self.retire(&sent);
        self.lost.insert((now, number));
        Some(sent)
    }

    /// Forget the lost records nothing can still acknowledge.
    ///
    /// Two bounds: age (`LOST_HOLD`, past which no reordering this carrier
    /// tolerates can still deliver the original) and count (`MAX_LOST`), so a
    /// path that drops everything cannot grow the table before the dead-link
    /// rule ends it.
    /// How many tracked records cover one byte of one stream, and how many the
    /// stream has in total.
    ///
    /// At a stall the question is whether anything is still *owed* at the frozen
    /// frontier: a record covering it is what a loss declaration and a repair both
    /// start from, and a frontier with no record under it can never move again.
    pub(crate) fn covering(&self, stream: u32, offset: u64) -> (u32, u32, u32) {
        let mut covering = 0;
        let mut total = 0;
        let mut number = 0;
        for sent in self.in_flight.values() {
            let Some(range) = sent.range else { continue };
            if range.stream != stream {
                continue;
            }
            total += 1;
            let start = range.offset;
            let end = start.saturating_add(u64::from(range.len));
            if start <= offset && offset < end {
                if covering == 0 {
                    number = sent.number;
                }
                covering += 1;
            }
        }
        (covering, total, number)
    }

    fn reap(&mut self, now: Instant) {
        let cutoff = now.checked_sub(LOST_HOLD);
        while let Some(&(at, number)) = self.lost.first() {
            let expired = cutoff.is_some_and(|cutoff| at < cutoff);
            if !expired && self.lost.len() < MAX_LOST {
                break;
            }
            self.lost.remove(&(at, number));
            self.in_flight.remove(&number);
        }
    }

    /// Record a packet the socket took, and arm the timer for it.
    ///
    /// A packet with no range (an acknowledgement, a credit update) is not
    /// tracked: nothing can be re-sent for it, and counting it would let a
    /// peer's silence be denied by our own chatter.
    pub(crate) fn on_sent(
        &mut self,
        now: Instant,
        number: u32,
        range: Option<SentRange>,
        opening: bool,
    ) {
        let sent = Sent {
            number,
            time: now,
            range,
            lost_at: None,
            opening,
        };
        if let Some(range) = range {
            self.flight_bytes += u64::from(range.len);
            self.in_flight.insert(number, sent);
            self.live += 1;
        }
        // The packet's own nature arms the timer, not the lane's size: a re-sent
        // opening must get the cheap floor here as well, because this is the call
        // that arms the timer for the *retransmission* after a timeout.
        self.arm(now, opening);
    }

    fn arm(&mut self, now: Instant, opening_lost: bool) {
        if self.live == 0 {
            self.timer = None;
            return;
        }
        let backoff = self.backoff.min(16);
        // A flight of one or two packets is a stream opening, not a burst: it
        // gets the cheap floor, because a duplicate of a handshake packet is
        // one datagram the peer drops by offset (see [`MIN_RTO_SMALL_FLIGHT`]).
        // The floor belongs to the *packet*, not to the lane: `opening_lost`
        // says a handshake datagram is among the losses, and that is small
        // whatever else this connection is carrying.
        let small = self.live <= SMALL_FLIGHT_PACKETS || opening_lost;
        let rto = self.rtt.rto(small).saturating_mul(1u32 << backoff.min(10));
        // A small flight's backoff stops at the wide timer. Without this a lane
        // waiting for its first sample climbs 20, 40, ... 2 560 ms, and the SUM
        // of those steps — 5.1 s — is what a stream open then waits, against the
        // 2.7 s the old flat 333 ms produced over the same count. Measured: an
        // open of 5 266 433 µs whose Noise handshake owned 5 265 951 of it, with
        // exactly one retransmit recorded. The cheap floor is worth having for
        // the first step; it must not extend the ladder.
        let rto = if small { rto.min(INITIAL_RTO) } else { rto };
        // Armed from the oldest packet that **carries stream data**, not from the
        // send that happened to arm it: `now` is the newest packet's time, so a busy
        // lane pushed the timer forward with every packet and an old hole never
        // expired (a reader parked seven seconds on one missing byte with
        // `retransmits` at 1). A packet with no range has nothing to re-send, so its
        // age must not drive the timer — letting it did cost `churn-16` its median.
        let base = self.oldest_data_time().unwrap_or(now);
        self.timer = Some(base + rto);
    }

    /// Fold one acknowledgement block in.
    pub(crate) fn on_ack(&mut self, now: Instant, ack: Ack) -> Acked {
        let mut result = Acked::default();
        // Before anything is retired: a record whose hold has run out must not
        // shadow the number this acknowledgement names.
        self.reap(now);
        // The newest packet this block proves arrived, which is what a round-trip
        // sample is taken from. Both retirement paths feed it: the cumulative
        // one carries most of a healthy acknowledgement, so a sample taken only
        // from the bitmap window would never fire and the congestion window
        // would never grow.
        let mut newest: Option<(u32, Instant)> = None;
        let mut note = |sent: &Sent| {
            if newest.is_none_or(|(number, _)| sent.number > number) {
                newest = Some((sent.number, sent.time));
            }
        };
        // Everything the frontier names is delivered, however far back it is. That
        // is the step that retires a wide flight: with a bitmap alone, a
        // connection with ten thousand packets outstanding could only ever
        // release the newest sixty-four of them, and the rest would be re-sent
        // until the connection died.
        while let Some((&number, _)) = self.in_flight.first_key_value() {
            if number >= ack.below {
                break;
            }
            if let Some(sent) = self.take(number) {
                note(&sent);
                if let Some(range) = sent.range {
                    result.bytes += usize::from(range.len);
                }
                result.newly.push(sent);
            }
        }
        // The bitmap covers `largest` and the 64 numbers below it, so nothing
        // outside that range can be acknowledged — or declared lost — by it.
        // Bounding the scan to it is what keeps an acknowledgement O(window)
        // instead of O(flight). A block whose frontier is already past the
        // largest number it names has said everything it can: the cumulative
        // step above took the whole flight.
        let oldest = ack.below.max(ack.largest.saturating_sub(64));
        if oldest > ack.largest {
            if let Some((_, time)) = newest {
                let sample = now.saturating_duration_since(time);
                result.progress = true;
                self.rtt.sample(sample);
                self.heard();
            }
            self.arm(now, false);
            return result;
        }
        // Delivered: everything the block covers and we still hold.
        let acked: Vec<u32> = self
            .in_flight
            .range(oldest..=ack.largest)
            .filter(|(number, _)| ack.contains(**number))
            .map(|(number, _)| *number)
            .collect();
        for number in acked {
            if let Some(sent) = self.take(number) {
                note(&sent);
                if let Some(range) = sent.range {
                    result.bytes += usize::from(range.len);
                }
                result.newly.push(sent);
            }
        }
        if let Some((_, time)) = newest {
            let sample = now.saturating_duration_since(time);
            result.progress = true;
            self.rtt.sample(sample);
            self.heard();
        }
        // Lost: three behind the largest, and not named by the bitmap.
        //
        // A packet below the frontier is delivered by definition, and one the
        // bitmap names arrived out of order; what is left in the window with
        // three later packets already seen is a hole the peer is waiting on.
        let lost: Vec<u32> = self
            .in_flight
            .range(oldest..=ack.largest)
            .filter(|(number, sent)| {
                sent.lost_at.is_none()
                    && **number >= ack.below
                    && **number < ack.largest
                    && ack.largest.saturating_sub(**number) >= PACKET_THRESHOLD
                    && !ack.contains(**number)
            })
            .map(|(number, _)| *number)
            .collect();
        for number in lost {
            if let Some(sent) = self.declare_lost(now, number) {
                if sent.opening {
                    result.opening_lost = true;
                }
                result.lost.push(sent);
            }
        }
        self.arm(now, result.opening_lost);
        result
    }

    /// When the caller should look again, if anything is outstanding.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        self.timer
    }

    /// The timer fired: answer why, and what to send again.
    pub(crate) fn on_timeout(&mut self, now: Instant) -> (Expiry, Vec<Sent>) {
        self.timer = None;
        self.reap(now);
        if self.live == 0 {
            return (Expiry::Loss, Vec::new());
        }
        self.silent += 1;
        let since = *self.silent_since.get_or_insert(now);
        if self.silent >= DEAD_LINK && now.saturating_duration_since(since) >= DEAD_FLOOR {
            let all: Vec<Sent> = self.in_flight.values().copied().collect();
            self.in_flight.clear();
            self.lost.clear();
            self.live = 0;
            self.flight_bytes = 0;
            return (Expiry::Dead, all);
        }
        let delay = self.rtt.loss_delay();
        let stale: Vec<u32> = self
            .in_flight
            .iter()
            .filter(|(_, sent)| {
                sent.lost_at.is_none() && now.saturating_duration_since(sent.time) > delay
            })
            .map(|(number, _)| *number)
            .collect();
        if stale.is_empty() {
            // Nothing is past the reordering window yet: the timer fired early, so
            // this is a probe of a silent path.
            //
            // **One range, not the flight** (`HANDOFF.md` §4.126). A probe exists to
            // elicit an acknowledgement, and one new number does that: the peer's
            // block answers it, the frontier moves, and the rest of the flight is
            // still in the table to be retired by it. Re-sending every outstanding
            // packet instead multiplies the flight by the number of probes — the
            // congestion window closes on a path that lost nothing, and each pass
            // re-sends more than the last. The oldest live range is the one whose
            // acknowledgement unblocks the most, and the one a repair queue would
            // have taken first.
            //
            // The probed record leaves the table, because it returns as a *new*
            // number and is tracked there; keeping both copies would count the same
            // bytes twice. A record already declared lost is not probed: its
            // retransmission is already queued, and its record stays so a late
            // acknowledgement of the original still lands.
            self.backoff = self.backoff.saturating_add(1);
            let oldest = self
                .in_flight
                .values()
                .filter(|sent| sent.lost_at.is_none())
                .min_by_key(|sent| (sent.time, sent.number))
                .copied();
            let probed = match oldest {
                Some(sent) => {
                    let _ = self.take(sent.number);
                    vec![sent]
                }
                None => Vec::new(),
            };
            self.arm(now, false);
            return (Expiry::Timeout, probed);
        }
        let mut lost = Vec::new();
        let mut opening_lost = false;
        for number in stale {
            if let Some(sent) = self.declare_lost(now, number) {
                if sent.opening {
                    opening_lost = true;
                }
                lost.push(sent);
            }
        }
        // A lone opening packet has nothing behind it, so no acknowledgement can
        // ever name it lost: this path is the only one it takes, and arming it
        // with the burst's floor — as this did — made the cheap timer
        // unreachable for exactly the packet it exists for.
        self.arm(now, opening_lost);
        (Expiry::Loss, lost)
    }

    /// The retransmission timeout this connection would arm now.
    ///
    /// The write path bounds the flight's age with it, which is the invariant that
    /// makes the timer sufficient: a packet is never admitted that could be older
    /// than the timeout supposed to find it. It has to be the *RTT-derived* value
    /// and not a constant: a flat 100 ms bounds a 400 ms path to a quarter of its
    /// bandwidth (measured — `rtt100` fell from 3.40 to 0.85 Gbit/s), while the RTO
    /// on that path is already several hundred milliseconds.
    pub(crate) fn current_rto(&self) -> Duration {
        self.rtt.rto(false)
    }

    /// When the oldest packet **carrying stream data** was sent.
    ///
    /// The timer follows this, and the write path bounds the flight by its age: a
    /// carrier that lets a flight grow past its own retransmission timeout cannot
    /// then time out a hole inside it, and the earlier attempts to do so either
    /// stormed the latency cells or cost the bulk cell sixty per cent.
    pub(crate) fn oldest_data_time(&self) -> Option<Instant> {
        self.in_flight
            .values()
            .find(|sent| sent.lost_at.is_none() && sent.range.is_some_and(|r| r.len > 0))
            .map(|sent| sent.time)
    }

    /// How long the oldest data packet has been outstanding.
    pub(crate) fn oldest_data_age(&self, now: Instant) -> Option<Duration> {
        self.oldest_data_time()
            .map(|time| now.saturating_duration_since(time))
    }

    /// Whether anything is waiting for an acknowledgement, lost records aside.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.live
    }

    /// How many lost records the table is holding, which is what the reaper
    /// exists to bound.
    #[cfg(test)]
    pub(crate) fn lost_records(&self) -> usize {
        self.lost.len()
    }

    /// Something arrived from the peer: the run of silent timers is over.
    fn heard(&mut self) {
        self.backoff = 0;
        self.silent = 0;
        self.silent_since = None;
    }

    /// How long the oldest packet still in flight has been waiting.
    ///
    /// The flight is keyed by packet number, so its first entry is the oldest
    /// send: an age that keeps growing while nothing retires it is a stall with
    /// no loss to explain it, which is the shape both open defects have.
    pub(crate) fn oldest_flight_age(&self, now: Instant) -> Option<Duration> {
        let (_, sent) = self
            .in_flight
            .iter()
            .find(|(_, sent)| sent.lost_at.is_none())?;
        Some(now.saturating_duration_since(sent.time))
    }

    /// The round-trip estimate, for the congestion controller and the pacer.
    #[cfg(test)]
    pub(crate) fn rtt(&self) -> Rtt {
        self.rtt
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    fn range(offset: u64) -> SentRange {
        SentRange {
            stream: 1,
            offset,
            len: 100,
            fin: false,
        }
    }

    fn send_one(recovery: &mut Recovery, now: Instant, offset: u64) -> u32 {
        let number = recovery.next_number();
        recovery.on_sent(now, number, Some(range(offset)), false);
        number
    }

    /// An acknowledgement that covers a packet takes it out of the flight and
    /// samples the round trip once.
    #[test]
    fn an_acknowledgement_clears_the_flight_and_samples_the_trip() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let second = send_one(&mut recovery, start, 100);
        assert_eq!(recovery.in_flight(), 2);

        let acked = recovery.on_ack(
            start + Duration::from_millis(40),
            Ack {
                largest: second,
                below: 0,
                bits: 1, // first is one below largest
                ..Ack::default()
            },
        );
        assert_eq!(acked.newly.len(), 2);
        assert_eq!(acked.lost.len(), 0);
        assert!(acked.progress, "a larger number is a round-trip sample");
        assert_eq!(recovery.in_flight(), 0);
        assert_eq!(
            recovery.poll_timeout(),
            None,
            "an empty flight has no timer"
        );
        let rtt = recovery.rtt();
        assert!(rtt.smoothed.unwrap() >= Duration::from_millis(30));
        assert!(first < second);
    }

    /// A packet declared lost keeps its record, so an acknowledgement that
    /// arrives afterwards still retires it.
    ///
    /// This is the regression behind the 6.7 GiB resident growth: the record
    /// was deleted at the moment of the loss declaration, the late
    /// acknowledgement found no entry, the stream's send buffer never learned
    /// that the range had arrived, and its frontier stopped at the hole for the
    /// life of the stream.
    #[test]
    fn an_acknowledgement_after_the_loss_still_retires_the_record() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let _second = send_one(&mut recovery, start, 100);
        let _third = send_one(&mut recovery, start, 200);
        let fourth = send_one(&mut recovery, start, 300);

        // The newest arrives, the two below it arrive, the first does not: with
        // three later numbers seen it is a loss.
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: fourth,
                below: 0,
                bits: 0b011, // third and second, not first
                ..Ack::default()
            },
        );
        assert_eq!(acked.lost.len(), 1, "one packet is declared lost");
        assert_eq!(acked.lost[0].number, first);
        assert_eq!(recovery.in_flight(), 0, "it is off the wire");
        assert_eq!(recovery.lost_records(), 1, "but its record is kept");
        assert_eq!(recovery.flight_bytes(), 0, "and it costs no window");

        // The original finally shows up, long after the retransmission was
        // queued. The record is what lets this land.
        let late = recovery.on_ack(
            start + Duration::from_millis(20),
            Ack {
                largest: fourth,
                below: fourth + 1,
                bits: 0,
                ..Ack::default()
            },
        );
        assert!(
            late.newly.iter().any(|sent| sent.number == first),
            "the late acknowledgement retires the lost packet"
        );
        assert_eq!(recovery.lost_records(), 0, "and the record is gone");
        assert_eq!(
            recovery.flight_bytes(),
            0,
            "no byte is left counted against the window"
        );
    }

    /// A re-sent range is on the wire once, and the flight says so.
    ///
    /// The invariant the write path leans on: `flight_bytes` bounds admission
    /// (`Conn::write`), so a repair that added its range on top of the copy it is
    /// repairing would make the congestion window close on a path that lost
    /// nothing. The bookkeeping is *retire on declare, charge on send*: the
    /// original leaves the flight when it is declared lost, the repair enters it
    /// when it is posted, and the two never overlap.
    #[test]
    fn a_declared_lost_range_and_its_repair_are_never_both_in_flight() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let original = send_one(&mut recovery, start, 0);
        assert_eq!(recovery.flight_bytes(), 100, "the send is in flight");

        let Some(lost) = recovery.declare_lost(start, original) else {
            unreachable!("a packet in the table can be declared lost")
        };
        assert_eq!(lost.number, original);
        assert_eq!(
            recovery.flight_bytes(),
            0,
            "a declared loss leaves the flight, so the queue can move again"
        );

        // The repair: the same byte range, a new number.
        let repair = recovery.next_number();
        recovery.on_sent(start, repair, Some(range(0)), false);
        assert_eq!(
            recovery.flight_bytes(),
            100,
            "the repair is charged once, not twice, for the bytes it re-sends"
        );

        // Both numbers are now acknowledged. The original is a lost record, so
        // its retirement must not subtract the bytes a second time.
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: repair,
                below: repair + 1,
                bits: 0,
                ..Ack::default()
            },
        );
        assert_eq!(acked.newly.len(), 2, "both records are retired");
        assert_eq!(
            recovery.flight_bytes(),
            0,
            "a late acknowledgement of the original must not drive the flight negative"
        );
        assert_eq!(recovery.lost_records(), 0, "and no lost record is left");
    }

    /// The byte prefix retires what the number block can no longer name.
    ///
    /// The defect this exists for (`HANDOFF.md` §4.108): a number lost for good
    /// stops the peer's cumulative frontier, and the bitmap only covers 64 numbers
    /// behind the largest seen, so older records can never be acknowledged again.
    /// The flight then keeps `cwnd` closed while the peer has every byte — measured
    /// as 32 KB in flight against a 13 KB window, with the writer refused and both
    /// ends idle.
    #[test]
    fn a_confirmed_prefix_retires_records_the_number_block_cannot_name() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let _ = send_one(&mut recovery, start, 0);
        let _ = send_one(&mut recovery, start, 100);
        let _ = send_one(&mut recovery, start, 200);
        assert_eq!(recovery.flight_bytes(), 300);

        // A prefix that ends inside the second range confirms only the first: a
        // partly confirmed range is still owed from its own first byte.
        assert_eq!(recovery.confirm_prefix(1, 180), (100, 0));
        assert_eq!(recovery.flight_bytes(), 200, "the partial range stays");
        assert_eq!(recovery.in_flight(), 2);

        // Another stream's records are not this stream's business.
        assert_eq!(recovery.confirm_prefix(9, 10_000), (0, 0));
        assert_eq!(recovery.flight_bytes(), 200);

        // The rest of the stream confirmed: everything the prefix covers goes.
        assert_eq!(recovery.confirm_prefix(1, 300), (200, 0));
        assert_eq!(recovery.flight_bytes(), 0);
        assert_eq!(recovery.in_flight(), 0);
    }

    /// A record already declared lost is retired once, not twice, when the prefix
    /// later covers it.
    #[test]
    fn a_prefix_over_a_declared_loss_does_not_retire_the_bytes_twice() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let _ = send_one(&mut recovery, start, 100);
        assert_eq!(recovery.flight_bytes(), 200);

        assert!(recovery.declare_lost(start, first).is_some());
        assert_eq!(
            recovery.flight_bytes(),
            100,
            "a declared loss leaves the flight"
        );
        assert_eq!(recovery.lost_records(), 1);

        // The prefix now covers both ranges. The lost record contributes nothing
        // to the flight — it left when it was declared lost — and must not be
        // subtracted a second time; it *is* reported back as a late confirmation,
        // because the peer had those bytes after this end had given up on them.
        assert_eq!(recovery.confirm_prefix(1, 200), (100, 100));
        assert_eq!(recovery.flight_bytes(), 0);
        assert_eq!(recovery.in_flight(), 0);
        assert_eq!(
            recovery.lost_records(),
            0,
            "and its lost record goes with it"
        );
    }

    /// A record nobody can still acknowledge is forgotten, so the table cannot
    /// grow for the life of the connection.
    #[test]
    fn a_lost_record_is_forgotten_after_its_hold() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let _ = send_one(&mut recovery, start, 100);
        let _ = send_one(&mut recovery, start, 200);
        let fourth = send_one(&mut recovery, start, 300);
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: fourth,
                below: 0,
                bits: 0b011, // third and second, not first
                ..Ack::default()
            },
        );
        assert_eq!(acked.lost.len(), 1);
        assert_eq!(acked.lost[0].number, first);
        assert_eq!(recovery.lost_records(), 1);

        // An acknowledgement that names nothing, a whole hold later: the reaper
        // runs before the flight is read, and the record is gone.
        let later = start + Duration::from_millis(10) + LOST_HOLD + Duration::from_millis(1);
        let _ = recovery.on_ack(
            later,
            Ack {
                largest: fourth,
                below: 0,
                bits: 0,
                ..Ack::default()
            },
        );
        assert_eq!(recovery.lost_records(), 0, "the hold bounds the table");
        assert_eq!(recovery.in_flight(), 0);
        assert!(first < fourth);
    }

    /// However many packets a path drops, the lost table stays bounded.
    #[test]
    fn the_lost_table_is_capped() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        for i in 0..u32::try_from(MAX_LOST).unwrap() + 16 {
            let number = recovery.next_number();
            recovery.on_sent(start, number, Some(range(u64::from(i) * 100)), false);
            let _ = recovery.declare_lost(start, number);
        }
        assert!(
            recovery.lost_records() <= MAX_LOST,
            "{} records are more than the cap",
            recovery.lost_records()
        );
    }

    /// Three later acknowledgements are what turns a gap into a loss; two are
    /// reordering and must not.
    /// The **last** packet of a flight is recovered, and only the timer can do it.
    ///
    /// §6.4's list asks for "the first and the last packet lost alone". The first is
    /// `Connection::a_lost_opening_packet_is_re_sent_within_the_cheap_timer`; the last
    /// had nothing: it has no later packet to reach the three-packet threshold, and
    /// when it is also the end of the data there is nothing behind it to ack, so the
    /// loss deadline is the only thing that can name it (`HANDOFF.md` §4.155).
    #[test]
    fn the_last_packet_of_a_flight_is_recovered_by_the_timer_alone() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let mut sent = Vec::new();
        for step in 0..4u64 {
            sent.push(send_one(&mut recovery, start, step * 100));
        }

        // The first three arrive; the fourth is lost and nothing follows it.
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: sent[2],
                below: 0,
                bits: 0b111,
                ..Ack::default()
            },
        );
        assert!(
            acked.lost.is_empty(),
            "one behind is reordering, not the three-packet threshold"
        );

        // No packet can ever name it, so the deadline is all that is left.
        let (expiry, resent) = recovery.on_timeout(start + Duration::from_millis(500));
        assert!(
            matches!(expiry, Expiry::Loss),
            "a lost tail is a loss, not a probe of a silent path"
        );
        assert!(
            resent.iter().any(|packet| packet.number == sent[3]),
            "and it is the tail that goes back on the wire"
        );
    }

    /// The cell RACK needs: a flight of three, its **middle** packet lost.
    ///
    /// §12.5 of the plan says RACK may not be re-attempted before a cell exists "where a
    /// stream keeps two or three packets in flight". This is that cell, and it exists to
    /// measure the current rule rather than to justify a change:
    ///
    /// - the later packet's acknowledgement **is** evidence that the middle one is gone —
    ///   it was sent after it and arrived;
    /// - the rule ignores that evidence, because `on_ack` declares a loss only on the
    ///   three-later-packet threshold and a flight of three has one later packet;
    /// - so the declaration waits for the clock, and the clock's floor on a wide flight
    ///   is [`MIN_RTO`].
    ///
    /// The number this pins is the gap between the two: evidence at 10 ms, declaration no
    /// earlier than the timer — 20 ms on the cheap floor a one-packet flight gets, and
    /// `MIN_RTO` (200 ms) the moment the flight is wider. RACK-shaped detection would
    /// anchor on the arrival of the later packet instead, which is what a candidate
    /// would have to beat and what a re-opened plan would pre-register against.
    #[test]
    fn a_middle_packet_of_three_in_flight_waits_for_the_clock_despite_later_evidence() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let middle = send_one(&mut recovery, start, 100);
        let last = send_one(&mut recovery, start, 200);
        assert_eq!(recovery.in_flight(), 3, "three packets in flight");

        // The middle is lost; the third arrives ten milliseconds later and is
        // acknowledged. That acknowledgement is later evidence about the middle.
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: last,
                // The first arrived cumulatively, the third by being the largest seen —
                // and the bitmap names neither the middle nor anything else.
                below: 1,
                bits: 0,
                ..Ack::default()
            },
        );
        assert_eq!(
            acked.newly.len(),
            2,
            "the first and the third are acknowledged: the evidence is in hand"
        );
        assert!(
            acked.lost.is_empty(),
            "and yet nothing is declared lost, because one later packet is below the \
             three-packet threshold"
        );
        assert_eq!(
            recovery.in_flight(),
            1,
            "only the lost packet's record is still held, and nothing has named it"
        );

        // Only the clock can name it now, and it names it later than the evidence — on
        // the *cheap* floor, because a flight of one is a small flight. On a wide flight
        // the same declaration would wait for `MIN_RTO` instead.
        let soon = recovery.poll_timeout();
        assert!(
            soon.is_some_and(|at| at >= start + MIN_RTO_SMALL_FLIGHT),
            "the timer is never earlier than the small-flight floor: {soon:?}"
        );
        assert!(
            soon.is_some_and(|at| at > start + Duration::from_millis(10)),
            "and it is later than the acknowledgement that carried the evidence: {soon:?}"
        );
        let (expiry, resent) = recovery.on_timeout(start + MIN_RTO);
        assert!(
            matches!(expiry, Expiry::Loss),
            "a shallow flight's hole is a loss, not a probe of a silent path"
        );
        assert!(
            resent.iter().any(|packet| packet.number == middle),
            "and it is the middle packet that goes back on the wire"
        );
        let _ = first;
    }

    #[test]
    fn three_later_acks_declare_a_loss() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let lost = send_one(&mut recovery, start, 0);
        let mut later = Vec::new();
        for step in 1u64..=3 {
            later.push(send_one(&mut recovery, start, step * 100));
        }
        // Two behind: not a loss yet.
        let acked = recovery.on_ack(
            start + Duration::from_millis(10),
            Ack {
                largest: later[1],
                below: 0,
                bits: 0b1,
                ..Ack::default()
            },
        );
        assert!(acked.lost.is_empty(), "two behind is reordering");
        assert_eq!(recovery.in_flight(), 2);

        // The third one behind: a loss, and exactly the packet that was missing.
        let acked = recovery.on_ack(
            start + Duration::from_millis(20),
            Ack {
                largest: later[2],
                below: 0,
                bits: 0b11,
                ..Ack::default()
            },
        );
        assert_eq!(acked.lost.len(), 1);
        assert_eq!(acked.lost[0].number, lost);
    }

    /// A re-sent opening takes the cheap floor even on a busy lane.
    ///
    /// This is the case the first version of the rule missed twice over: the hint
    /// was passed only on the acknowledgement path, and `on_sent` — which is what
    /// arms the timer for the *retransmission* — hardcoded the burst's floor. A
    /// lone opening has no later packet for a SACK to name it, so the timer is the
    /// only thing that can find it lost, and the change was inoperative for
    /// exactly the packet it was written for.
    #[test]
    fn a_re_sent_opening_takes_the_cheap_timer_on_a_busy_lane() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        for step in 1u64..=16 {
            let number = recovery.next_number();
            recovery.on_sent(start, number, Some(range(step * 100)), false);
        }
        assert!(recovery.in_flight() > SMALL_FLIGHT_PACKETS);
        let at = start + Duration::from_millis(10);
        let number = recovery.next_number();
        recovery.on_sent(at, number, Some(range(0)), true);
        let waited = recovery
            .poll_timeout()
            .unwrap()
            .saturating_duration_since(at);
        assert!(
            waited < MIN_RTO,
            "an opening cleared from a busy lane must use the cheap floor, \
             but it armed the wide one: {waited:?}"
        );
    }

    /// The contrast: a burst packet on the same lane keeps the wide floor, which
    /// is the rule that survived every bulk A/B.
    /// A probe re-sends **one** range, not the whole flight.
    ///
    /// §6.2 of the plan (`HANDOFF.md` §4.126): a probe timeout exists to elicit an
    /// acknowledgement, and one new number does that. Re-sending every outstanding
    /// packet instead multiplies the flight by the number of probes — the window
    /// closes on a path that lost nothing, and each pass re-sends more than the
    /// last, which is the shape §4.107 measured.
    /// The one timer already fires no later than any probe deadline would.
    ///
    /// §6.2 asks for the probe deadline to be computed "independently from the latest
    /// still-outstanding packet". That is a **no-op in this design**, and the reason is
    /// arithmetic rather than taste: `arm` anchors the timer on the **oldest** data
    /// packet with the same `rto` a probe would use, and `oldest ≤ latest`, so
    /// `oldest + rto ≤ latest + rto` always — a second timer taking `min` of the two
    /// could only ever pick the one that exists. What §6.2 actually wants is that the
    /// timer is a *backstop* while loss is decided by evidence, and that is what
    /// `on_timeout` does: it declares whatever is past the reorder window and probes
    /// only when nothing is.
    ///
    /// This pins the ordering, so that a future change anchoring the timer on the
    /// newest packet — which would delay every repair by a send interval — fails here.
    #[test]
    fn the_timer_is_never_later_than_a_latest_anchored_probe_would_be() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        // An old hole and a fresh packet behind it.
        let _old = send_one(&mut recovery, start, 0);
        let latest = start + Duration::from_millis(30);
        let _newest = send_one(&mut recovery, latest, 100);
        recovery.arm(latest, false);
        let deadline = recovery.poll_timeout().unwrap();

        assert_eq!(
            recovery.oldest_data_time().unwrap(),
            start,
            "the anchor is the old hole, not the newest send"
        );
        // Two packets is a small flight, so the timeout is 20 ms: armed from the old
        // hole it lands 10 ms *before* the newest packet was even sent. Anchoring it
        // on the newest instead would land 20 ms after, and every repair would wait a
        // send interval longer.
        assert!(
            deadline <= latest,
            "the timer must not wait for the newest packet: {deadline:?} vs {latest:?}"
        );
    }

    #[test]
    fn a_probe_re_sends_one_range_not_the_flight() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        for step in 1u64..3 {
            send_one(&mut recovery, start, step * 100);
        }
        assert_eq!(recovery.in_flight(), 3);

        // Fired before anything is past the reordering window: a probe, not a loss.
        let (expiry, probed) = recovery.on_timeout(start + Duration::from_millis(1));
        assert!(
            matches!(expiry, Expiry::Timeout),
            "an early timer is a probe"
        );
        assert_eq!(
            probed.len(),
            1,
            "a probe re-sends one range; it offered {}",
            probed.len()
        );
        assert_eq!(probed[0].number, first, "and it is the oldest");
        assert_eq!(
            recovery.in_flight(),
            2,
            "the other two stay tracked, to be retired by the answer to the probe"
        );
    }

    #[test]
    fn a_re_sent_burst_packet_keeps_the_wide_timer() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        for step in 1u64..=16 {
            let number = recovery.next_number();
            recovery.on_sent(start, number, Some(range(step * 100)), false);
        }
        assert!(recovery.in_flight() > SMALL_FLIGHT_PACKETS);
        let waited = recovery
            .poll_timeout()
            .unwrap()
            .saturating_duration_since(start);
        assert!(
            waited >= MIN_RTO,
            "a burst keeps the wide floor: it armed {waited:?}"
        );
    }

    /// A packet outstanding longer than the reordering window is lost even when
    /// nothing later ever arrives, which is what covers a tail loss.
    #[test]
    fn the_reordering_window_declares_a_loss_without_later_acks() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, start, 0);
        let (expiry, resent) = recovery.on_timeout(start + Duration::from_secs(1));
        assert_eq!(expiry, Expiry::Loss);
        assert_eq!(resent.len(), 1, "the stale packet is re-sent");
        assert_eq!(recovery.in_flight(), 0);
    }

    /// A cheap retransmission timer must not end a path on its own.
    ///
    /// The count rule measures silence *through the timer*, so a low floor turns
    /// a few hundred milliseconds of a busy host into "ten unanswered
    /// retransmissions". The floor on the silence is what keeps the count
    /// honest: the same ten retries are enough after five seconds and not
    /// before.
    #[test]
    fn ten_silent_retries_need_five_seconds_of_silence() {
        let now = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, now, 0);
        // Nine behind it, all inside the last second: one more must not kill.
        recovery.silent = DEAD_LINK - 1;
        recovery.silent_since = Some(now.checked_sub(Duration::from_secs(1)).unwrap());
        recovery.timer = Some(now);
        let (expiry, _) = recovery.on_timeout(now);
        assert_ne!(
            expiry,
            Expiry::Dead,
            "a second of silence is not a dead path"
        );

        // The same tenth retry, with the silence old enough to mean it.
        if recovery.in_flight() == 0 {
            send_one(&mut recovery, now, 100);
        }
        recovery.silent = DEAD_LINK - 1;
        let long_ago = DEAD_FLOOR + Duration::from_secs(1);
        recovery.silent_since = Some(now.checked_sub(long_ago).unwrap());
        recovery.timer = Some(now);
        let (expiry, resent) = recovery.on_timeout(now);
        assert_eq!(expiry, Expiry::Dead, "five seconds of silence ends it");
        assert!(!resent.is_empty(), "a dead path reports what it held");
    }

    /// Silence backs the timer off and, once the count AND the silence floor are
    /// both met, calls the path dead rather than holding a lane that will never
    /// carry anything.
    ///
    /// The two conditions are the point of [`DEAD_FLOOR`]: a cheap timer reaches
    /// `DEAD_LINK` retries in a fraction of a second, so the count alone would
    /// end a lane that a busy host merely delayed. The loop below therefore runs
    /// until the path dies rather than assuming it dies at the tenth retry.
    /// A real receipt resets the silence, so probe counts alone cannot kill a path.
    ///
    /// §6.2: "真实接收/静默决定存活，廉价 PTO 次数不能单独杀死路径". `Expiry::Dead` needs
    /// `DEAD_LINK` unanswered retransmissions **and** `DEAD_FLOOR` of silence — and the
    /// pair is reset by one datagram that arrives. This pins the reset, which is the half
    /// `ten_silent_retries_need_five_seconds_of_silence` does not cover: that test shows
    /// the count and the floor, this one shows that a receipt clears both, so the very
    /// next timeout cannot be dead however the counts stood a moment before.
    #[test]
    fn a_real_receipt_resets_the_silence_that_would_have_killed_the_path() {
        let now = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, now, 0);

        // The state that kills (the other test proves it does): a full count of
        // unanswered retransmissions and more than the floor of silence.
        recovery.silent = DEAD_LINK;
        recovery.silent_since = Some(
            now.checked_sub(DEAD_FLOOR + Duration::from_secs(1))
                .unwrap(),
        );
        recovery.heard();

        assert_eq!(recovery.silent, 0, "a receipt clears the unanswered count");
        assert!(
            recovery.silent_since.is_none(),
            "and the clock the floor is measured from"
        );

        // So the next timeout is a probe, not a death, even though a moment ago the
        // same counters said otherwise.
        recovery.timer = Some(now);
        let (expiry, _) = recovery.on_timeout(now);
        assert_ne!(
            expiry,
            Expiry::Dead,
            "a path its peer just answered must not be declared dead"
        );
    }

    #[test]
    fn silence_backs_off_and_then_kills_the_path() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, start, 0);
        let mut now = start;
        let mut expiries = Vec::new();
        // A bounded run: the timer backs off towards `MAX_RTO`, so the silence
        // floor is reached in a handful of ticks. The bound is what keeps a
        // broken rule from looping here for ever.
        for _ in 0..64 {
            let Some(deadline) = recovery.poll_timeout() else {
                break;
            };
            now = deadline.max(now);
            let (expiry, resent) = recovery.on_timeout(now);
            expiries.push(expiry);
            if expiry == Expiry::Dead {
                assert!(!resent.is_empty(), "a dead path reports what it held");
                break;
            }
            // A real connection re-sends what the timer reported, which puts
            // it back in the flight and arms the next timer: that repetition
            // is what the kill rule counts.
            for sent in resent {
                let number = recovery.next_number();
                recovery.on_sent(now, number, sent.range, sent.opening);
            }
        }
        assert_eq!(
            expiries.last(),
            Some(&Expiry::Dead),
            "the count and the silence floor together must end the path"
        );
        assert!(
            now.saturating_duration_since(start) >= DEAD_FLOOR,
            "it may only die once the floor is met, died after {:?}",
            now.saturating_duration_since(start)
        );
        assert_eq!(recovery.in_flight(), 0);
    }

    /// One packet in flight gets the cheap floor: a stream opening on a lane
    /// that has already measured its round trip must not wait 200 ms for a lost
    /// handshake packet.
    ///
    /// This is the measured defect the floor exists for: `open_channel` cost
    /// 502 ms against a 4 µs hello and a 0.2-1.7 ms acknowledgement on the same
    /// lane. A lane carrying bulk has samples, so its estimate is milliseconds —
    /// and the wide floor turned that into the whole timer.
    #[test]
    fn a_small_flight_retransmits_in_twenty_milliseconds() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        // One round trip, so the estimate is a real one: two milliseconds.
        let first = send_one(&mut recovery, start, 0);
        let acked = recovery.on_ack(
            start + Duration::from_millis(2),
            Ack {
                largest: first,
                below: first + 1,
                bits: 0,
                ..Ack::default()
            },
        );
        assert!(acked.progress, "the acknowledgement samples the trip");
        assert_eq!(
            recovery.poll_timeout(),
            None,
            "an empty flight has no timer"
        );

        // The opening packet of the next stream: one packet, estimate 2 ms.
        let sent_at = start + Duration::from_millis(3);
        send_one(&mut recovery, sent_at, 100);
        let deadline = recovery.poll_timeout().unwrap();
        assert_eq!(
            deadline.saturating_duration_since(sent_at),
            MIN_RTO_SMALL_FLIGHT,
            "a single handshake packet waits the cheap floor, not the wide one"
        );
    }

    /// A burst keeps the wide floor: that is the case every `MIN_RTO` A/B
    /// measured, and the reason the cheap floor is scoped to a small flight
    /// rather than applied to the connection.
    #[test]
    fn a_wide_flight_keeps_the_two_hundred_millisecond_floor() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        let first = send_one(&mut recovery, start, 0);
        let _ = recovery.on_ack(
            start + Duration::from_millis(2),
            Ack {
                largest: first,
                below: first + 1,
                bits: 0,
                ..Ack::default()
            },
        );

        let sent_at = start + Duration::from_millis(3);
        for offset in 0..=SMALL_FLIGHT_PACKETS {
            send_one(&mut recovery, sent_at, 100 + offset as u64 * 100);
        }
        let deadline = recovery.poll_timeout().unwrap();
        assert_eq!(
            deadline.saturating_duration_since(sent_at),
            MIN_RTO,
            "one packet past the small-flight limit is a burst, and keeps the wide floor"
        );
    }

    /// Before any sample there is nothing to be cheap about: the initial timer
    /// is still the RFC 9002 value, whatever the flight size.
    #[test]
    fn an_unmeasured_path_keeps_the_initial_timer() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, start, 0);
        let deadline = recovery.poll_timeout().unwrap();
        assert_eq!(
            deadline.saturating_duration_since(start),
            SMALL_FLIGHT_INITIAL_RTO,
            "a lone packet on a sample-less lane is a handshake, and gets the \
             cheap timer"
        );
    }

    /// A small flight's ladder stops at the wide timer, so one opening cannot
    /// climb to seconds.
    ///
    /// The cheap first step is the point of [`MIN_RTO_SMALL_FLIGHT`]; an
    /// unbounded geometric climb is not. Nine silent timers on a sample-less
    /// lane must therefore never wait longer than nine wide intervals — the old
    /// flat 333 ms behaviour — whatever the cheap base is.
    #[test]
    fn a_small_flights_ladder_stops_at_the_wide_timer() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, start, 0);
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..9 {
            let Some(deadline) = recovery.poll_timeout() else {
                break;
            };
            waits.push(deadline.saturating_duration_since(now));
            now = deadline.max(now);
            let (_, resent) = recovery.on_timeout(now);
            for sent in resent {
                let number = recovery.next_number();
                recovery.on_sent(now, number, sent.range, sent.opening);
            }
        }
        assert_eq!(waits[0], MIN_RTO_SMALL_FLIGHT, "the first step is cheap");
        assert!(
            waits.iter().all(|wait| *wait <= INITIAL_RTO),
            "no step may exceed the wide timer: {waits:?}"
        );
        let total: std::time::Duration = waits.iter().sum();
        assert!(
            total < std::time::Duration::from_secs(3),
            "the whole ladder stays under three seconds, got {total:?}"
        );
    }

    /// A **wide** flight on a sample-less lane still arms the wide timer: that
    /// is the case every `MIN_RTO` A/B priced, and it must not change.
    #[test]
    fn an_unmeasured_burst_keeps_the_initial_timer() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        for offset in 0..=SMALL_FLIGHT_PACKETS {
            send_one(&mut recovery, start, offset as u64 * 100);
        }
        let deadline = recovery.poll_timeout().unwrap();
        assert_eq!(
            deadline.saturating_duration_since(start),
            INITIAL_RTO,
            "an unmeasured burst errs late, because re-sending it costs its width"
        );
    }

    /// **The declared-loss set stays bounded, and a full table does not lose the newest loss.**
    ///
    /// `declare_lost` reaps before it records, so a lane under a long loss burst cannot grow the set
    /// without limit — and, just as important, the loss it is asked about *now* is the one that gets
    /// recorded rather than silently refused. A refusal would be a packet sent and never repaired,
    /// which is the one failure this carrier has no other way to notice (`HANDOFF.md` §4.269).
    #[test]
    fn the_declared_loss_set_stays_bounded() {
        let mut recovery = Recovery::new();
        let now = Instant::now();
        let range = SentRange {
            stream: 4,
            offset: 0,
            len: 64,
            fin: false,
        };
        let total = u32::try_from(MAX_LOST).unwrap_or(u32::MAX) + 64;
        for number in 0..total {
            recovery.on_sent(now, number, Some(range), false);
            assert!(
                recovery.declare_lost(now, number).is_some(),
                "every loss asked about must be recorded, not refused: number {number}"
            );
        }
        assert!(
            recovery.lost.len() <= MAX_LOST,
            "the set is bounded: {} entries",
            recovery.lost.len()
        );
        assert!(
            recovery.in_flight.len() <= MAX_LOST,
            "and the records it kept go with it: {}",
            recovery.in_flight.len()
        );
    }
}
