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

use std::collections::BTreeMap;
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

/// The retransmission timer before any round trip has been measured: long
/// enough that a slow path is not mistaken for a dead one on the first packet,
/// short enough that a lost handshake is re-sent inside the caller's budget.
const INITIAL_RTO: Duration = Duration::from_millis(500);

/// The retransmission timer's floor: below this the timer fires on scheduling
/// noise rather than on a lost packet.
const MIN_RTO: Duration = Duration::from_millis(200);

/// The retransmission timer's ceiling, which is also the idle limit of a silent
/// peer.
const MAX_RTO: Duration = Duration::from_secs(30);

/// How many retransmissions without a single acknowledgement before the path is
/// called dead.
///
/// Without it a black-holed path is a connection that backs off to the ceiling
/// and stays there, holding a lane that will never carry anything.
const DEAD_LINK: u32 = 10;

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
}

impl Rtt {
    fn sample(&mut self, sample: Duration) {
        self.latest = Some(sample);
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
                self.smoothed = Some((smoothed * 7 + sample) / 8);
            }
        }
    }

    /// The reordering window: nine eighths of the round trip.
    fn loss_delay(&self) -> Duration {
        let base = self.latest.or(self.smoothed).unwrap_or(INITIAL_RTO);
        base.saturating_mul(TIME_THRESHOLD_NUM) / TIME_THRESHOLD_DEN
    }

    /// The retransmission timer for the current estimate.
    fn rto(&self) -> Duration {
        let base = match self.smoothed {
            Some(smoothed) => smoothed + self.variance.saturating_mul(4),
            None => INITIAL_RTO,
        };
        base.clamp(MIN_RTO, MAX_RTO)
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
    in_flight: BTreeMap<u32, Sent>,
    rtt: Rtt,
    /// When the timer should fire, if anything is in flight.
    timer: Option<Instant>,
    /// Retransmission timers that expired without an acknowledgement.
    silent: u32,
    /// The backoff applied to the timer since the last acknowledgement.
    backoff: u32,
}

impl Recovery {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            next: 0,
            in_flight: BTreeMap::new(),
            rtt: Rtt::default(),
            timer: None,
            silent: 0,
            backoff: 0,
        }
    }

    /// Reserve the next packet number, whether or not it is tracked.
    pub(crate) fn next_number(&mut self) -> u32 {
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        number
    }

    /// Record a packet the socket took, and arm the timer for it.
    ///
    /// A packet with no range (an acknowledgement, a credit update) is not
    /// tracked: nothing can be re-sent for it, and counting it would let a
    /// peer's silence be denied by our own chatter.
    pub(crate) fn on_sent(&mut self, now: Instant, number: u32, range: Option<SentRange>) {
        let sent = Sent {
            number,
            time: now,
            range,
        };
        if range.is_some() {
            self.in_flight.insert(number, sent);
        }
        self.arm(now);
    }

    fn arm(&mut self, now: Instant) {
        if self.in_flight.is_empty() {
            self.timer = None;
            return;
        }
        let backoff = self.backoff.min(16);
        let rto = self.rtt.rto().saturating_mul(1u32 << backoff.min(10));
        self.timer = Some(now + rto);
    }

    /// Fold one acknowledgement block in.
    pub(crate) fn on_ack(&mut self, now: Instant, ack: Ack) -> Acked {
        let mut result = Acked::default();
        // A packet that had never been acknowledged and is at or below the
        // largest this block names is the only one that can sample the round
        // trip: sampling every acknowledgement would fold the peer's delayed
        // ack timer into the estimate.
        if let Some((_, sent)) = self.in_flight.iter().next_back()
            && ack.largest >= sent.number
        {
            result.progress = true;
            self.rtt.sample(now.saturating_duration_since(sent.time));
            self.timer = Some(now + self.rtt.rto());
            self.backoff = 0;
            self.silent = 0;
        }
        // Delivered: everything the block covers and we still hold.
        let acked: Vec<u32> = self
            .in_flight
            .keys()
            .copied()
            .filter(|number| ack.contains(*number))
            .collect();
        for number in acked {
            if let Some(sent) = self.in_flight.remove(&number) {
                result.newly.push(sent);
            }
        }
        // Lost: three behind the largest, or older than the reordering window.
        let delay = self.rtt.loss_delay();
        let lost: Vec<u32> = self
            .in_flight
            .iter()
            .filter(|(number, sent)| {
                let three_behind = ack.largest.saturating_sub(**number) >= PACKET_THRESHOLD
                    && **number < ack.largest;
                let too_old = now.saturating_duration_since(sent.time) > delay;
                three_behind || too_old
            })
            .map(|(number, _)| *number)
            .collect();
        for number in lost {
            if let Some(sent) = self.in_flight.remove(&number) {
                result.lost.push(sent);
            }
        }
        self.arm(now);
        result
    }

    /// When the caller should look again, if anything is outstanding.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        self.timer
    }

    /// The timer fired: answer why, and what to send again.
    pub(crate) fn on_timeout(&mut self, now: Instant) -> (Expiry, Vec<Sent>) {
        self.timer = None;
        if self.in_flight.is_empty() {
            return (Expiry::Loss, Vec::new());
        }
        self.silent += 1;
        if self.silent >= DEAD_LINK {
            let all: Vec<Sent> = self.in_flight.values().copied().collect();
            self.in_flight.clear();
            return (Expiry::Dead, all);
        }
        let delay = self.rtt.loss_delay();
        let stale: Vec<u32> = self
            .in_flight
            .iter()
            .filter(|(_, sent)| now.saturating_duration_since(sent.time) > delay)
            .map(|(number, _)| *number)
            .collect();
        if stale.is_empty() {
            // Nothing is past the reordering window yet: the timer fired early,
            // so this is a probe of a silent path and every outstanding packet
            // is re-sent, oldest first.
            self.backoff = self.backoff.saturating_add(1);
            let all: Vec<Sent> = self.in_flight.values().copied().collect();
            self.arm(now);
            return (Expiry::Timeout, all);
        }
        let mut lost = Vec::new();
        for number in stale {
            if let Some(sent) = self.in_flight.remove(&number) {
                lost.push(sent);
            }
        }
        self.arm(now);
        (Expiry::Loss, lost)
    }

    /// Whether anything is waiting for an acknowledgement.
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// The round-trip estimate, for the congestion controller and the pacer.
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
        recovery.on_sent(now, number, Some(range(offset)));
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
                bits: 1, // first is one below largest
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

    /// Three later acknowledgements are what turns a gap into a loss; two are
    /// reordering and must not.
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
                bits: 0b1,
            },
        );
        assert!(acked.lost.is_empty(), "two behind is reordering");
        assert_eq!(recovery.in_flight(), 2);

        // The third one behind: a loss, and exactly the packet that was missing.
        let acked = recovery.on_ack(
            start + Duration::from_millis(20),
            Ack {
                largest: later[2],
                bits: 0b11,
            },
        );
        assert_eq!(acked.lost.len(), 1);
        assert_eq!(acked.lost[0].number, lost);
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

    /// Silence backs the timer off and, after the limit, calls the path dead
    /// rather than holding a lane that will never carry anything.
    #[test]
    fn silence_backs_off_and_then_kills_the_path() {
        let start = Instant::now();
        let mut recovery = Recovery::new();
        send_one(&mut recovery, start, 0);
        let mut now = start;
        let mut expiries = Vec::new();
        for _ in 0..DEAD_LINK {
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
                recovery.on_sent(now, number, sent.range);
            }
        }
        assert_eq!(
            expiries.last(),
            Some(&Expiry::Dead),
            "ten silent retransmissions must end the path"
        );
        assert_eq!(recovery.in_flight(), 0);
    }
}
