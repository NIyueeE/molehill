//! One connection: its streams, its flight, its credit, and what to send next.
//!
//! The state machine owns no socket and reads no clock. It is fed datagrams and
//! a monotonic Instant, and answers with the next datagram to put on the wire,
//! so the whole protocol is exercised in memory: two connections wired together
//! by a queue are the harness, and a test can drop one datagram between them
//! without a network in the way.
//!
//! Two rules shape the send side. Every stream gets its turn — the cursor walks
//! the stream table, so one bulk transfer cannot starve an interactive one — and
//! nothing is sent that the peer's credit does not cover. Credit is an absolute
//! byte count the peer advertises, not a delta, because a lost delta drifts the
//! window for ever.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bytes::Bytes;
use std::time::{Duration, Instant};

use super::recovery::{Expiry, Recovery, SentRange};
use super::stream::{Blocks, RecvBuf, SendBuf};
use super::wire::{self, AckSet, Frame, MAX_STREAM_PAYLOAD, ResetCode};
use crate::transport::health::SessionHealth;
use crate::transport::stats;

/// How often an idle connection proves the path still carries it.
///
/// The cadence is the NAT one: a mapping that sees no packet for longer than a
/// couple of minutes is reused by the next flow, and a carrier that goes quiet
/// between bursts would lose its mapping exactly when a visitor arrives. Two
/// seconds is what the adapter this replaces used, and it keeps a resumed
/// laptop's first pass to a single probe (see [`Conn::poll_transmit`]).
pub(crate) const KEEPALIVE: Duration = Duration::from_secs(2);

/// Silence after which the peer is gone.
///
/// Ten keepalive intervals, so a path that stops carrying *anything* is called
/// dead after ten unanswered probes — the "ten unanswered retransmissions" the
/// carrier promises — instead of holding a lane that will never carry a byte
/// again. A single arriving datagram resets the count: liveness is the peer's
/// own traffic, not an acknowledgement of ours.
const DEAD_AFTER: Duration = Duration::from_secs(20);

/// The window this end advertises: how many bytes a peer may have in flight.
///
/// A lane stays full while its window covers `bandwidth × RTT`, so this number
/// is the one lever on both throughput and resident memory: a lane holds up to
/// a window of receive blocks, and the send-buffer cap below is expressed in
/// terms of the same window. Sixteen megabytes keeps a lane at about
/// 650 Mbit/s across a 200 ms path and fills a short one outright, and it is
/// what puts the benchmark's four-lane cell under its memory target — the same
/// cell reads 438 MiB at 32 MiB and **265 MiB at 16 MiB**, with CPU per carried
/// Gbit and throughput inside the run's own scatter (`HANDOFF.md`, "the window
/// is where the cell's memory is").
pub(crate) const WINDOW: u64 = 16 * 1024 * 1024;

/// The most this end will hold in its send buffers, across every stream.
///
/// The credit window bounds what the *peer* can hold; this bounds what this end
/// keeps for the re-send, and it exists because a byte's buffer is released only
/// when an acknowledgement moves the stream's frontier past it. A frontier can
/// stall — one range the peer received but never acknowledged, or acknowledged
/// after its recovery record was forgotten — and without a ceiling the buffer
/// grows with everything written after the hole. Measured: 6.7 GiB resident in a
/// forty-second cell before this existed. At twice the window this refuses a
/// write instead, which is backpressure the writer already understands; the
/// connection's own stall line names the reason.
const MAX_BUFFERED: u64 = 2 * WINDOW;

/// How many bytes of the connection's budgets an **opening** stream may always
/// use, whichever stream has filled the rest.
///
/// The three budgets a write draws on — the peer's credit, the congestion
/// window, the buffer ceiling — are the connection's, not a stream's, so a bulk
/// stream takes them in arrival order and a stream that opens behind it waits for
/// them to drain. Measured on the model's `capacity` ramp with one bulk stream
/// running: the interactive probe's p50 stayed at **1.9-2.0 ms** while its p99
/// read **2 704, 5 002 and 5 002 ms** in three cells. The median is a healthy
/// path and the tail is a stream open waiting for a budget the bulk stream holds.
///
/// So the last `OPEN_RESERVE` bytes of every budget are reserved: a stream that
/// has not yet written this many bytes may use them, and a stream that has must
/// stop short of them. Eight packets carry a Noise handshake with room to spare
/// (the KK handshake is about 200 bytes).
///
/// The reserve is taken *inside* the peer's credit, never on top of it: a sender
/// that overran the advertised window would have its frames rejected as
/// `OverWindow`, never acknowledged, and re-sent until the lane was declared
/// dead — the failure this file's `write` comment records. Bulk stopping short
/// is what keeps the reserve genuinely free.
///
/// It replaces `STREAM_BURST`, which never bound: it clamped a byte count that
/// the next line clamped again to one packet, so the fairness its comment
/// described did not exist.
pub(crate) const OPEN_RESERVE: u64 = 8 * MAX_STREAM_PAYLOAD as u64;

/// How long an abandoned stream may hold unacknowledged bytes before it is retired.
///
/// A stream whose handle was dropped still delivers what it has queued — a
/// response must not be truncated — and it retires when the peer acknowledges all
/// of it. When the peer *never* acknowledges (it is gone, or the path has stopped
/// carrying), that wait has no end, and the bytes it is waiting on stay counted in
/// `buffered` for the lane's life. `write` subtracts exactly that number from
/// [`MAX_BUFFERED`], so one such stream refuses **every later stream on the
/// connection**: measured, the first lossy stage of a sweep took the bulk spine
/// from 7.7 to 0.21 Gbit/s and it never came back, with `buffered` climbing stage
/// by stage to the ceiling (HANDOFF §4.50).
///
/// Thirty seconds is chosen against the layer above and the layer below: the
/// stripe group's own reorder deadline is thirty (`STRIPE_REORDER_DEADLINE`), the
/// carrier's dead-path floor is five and `DEAD_AFTER` is twenty, and the longest
/// stall this carrier has been measured to recover from is twelve. A stream that
/// has been abandoned for longer than all of those is one nobody is waiting for.
pub(crate) const ABANDON_HOLD: Duration = Duration::from_secs(30);

/// A gap long enough to count as a tail event rather than scheduling noise.
///
/// The pre-registration in HANDOFF §4.62 is stated against 200 µs: the gap is 5–7 µs at
/// p50, so a gap past this is either a descheduling or a page fault, and the share it
/// accounts for decides whether the cycle's 26 % gap is many small waits or a few large
/// ones.
pub(crate) const GAP_SLOW_US: u64 = 200;

/// How many datagrams one lane queues for the wire before it drops the oldest.
///
/// A queue, not a window: a datagram that waited longer than this many slots is one the
/// application has already given up on, and holding it would cost the *next* datagram the
/// latency this frame exists to buy. Small on purpose — a burst absorber, not a buffer.
pub(crate) const DATAGRAM_QUEUE: usize = 64;

/// Whether a stream is still opening: it has not written a reserve's worth yet.
fn opening(state: &StreamState) -> bool {
    state.send.written() < OPEN_RESERVE
}

/// The congestion window a connection starts with: ten full packets.
///
/// The peer's credit bounds what it can *hold*; this bounds what the path has in
/// flight, which is what keeps the recovery table proportional to the
/// bandwidth-delay product instead of to the credit window.
const INITIAL_CWND: u64 = 10 * MAX_STREAM_PAYLOAD as u64;

/// The most the congestion window may grow to.
///
/// A window has to be bounded by something that is not "however much the peer
/// will take", or the flight becomes the credit window again.
const MAX_CWND: u64 = 16 * 1024 * 1024;

/// The smallest window a lane may grow to, at the shortest path.
///
/// A congestion window exists to cover a bandwidth-delay product. Everything
/// above the product is a **standing queue at the peer** that buys no throughput
/// and costs every small packet its latency, so the ceiling is the floor scaled by
/// the measured round trip rather than a flat number: a flat two mebibytes is
/// forty times the product on loopback (fifty microseconds at eight gigabits is
/// fifty kilobytes), and it built a queue that put 4.5 ms in front of every
/// handshake packet — measured, the open went from 1.34 ms to 13.5 ms as the bulk
/// spine crossed eight gigabits a second (`HANDOFF.md`, "at 8.4 Gbit/s the runtime
/// is idle and the handshake is ten times slow").
///
/// The floor is also what a lane starts with, so a fresh connection cannot open
/// over-wide before it has any round-trip sample at all.
const WINDOW_FLOOR: u64 = 256 * 1024;

/// The round trip the floor is calibrated at — one millisecond, which is the
/// round-trip estimator's own granularity (`recovery::GRANULARITY`), so this is the
/// shortest path the carrier can actually see. Calibrating at fifty microseconds
/// would have put the floor twenty times out of reach: the estimate never goes
/// below its granularity, so the smallest ceiling the formula can produce would have
/// been five mebibytes — the flat ceiling this change exists to remove.
const WINDOW_REFERENCE: u64 = 1_000;

/// Why a frame could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The datagram is not one of this carrier's packets.
    Malformed,
    /// Bytes arrived that the advertised window does not cover.
    OverWindow,
    /// The frame names a stream this end never opened.
    UnknownStream,
    /// A sealed packet whose key is missing or whose tag does not verify.
    ///
    /// A tag that fails is either a corrupted packet or a peer that does not hold
    /// the lane's root; both are dropped rather than guessed at.
    Unsealed,
}

impl Rejected {
    /// The name this refusal is counted under (see `stats::note_rejected`).
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::OverWindow => "over_window",
            Self::UnknownStream => "unknown",
            Self::Unsealed => "unsealed",
        }
    }
}

/// One stream, as the connection sees it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the four flags are the stream's independent lifecycle facts \
              (each side finished, either reset it, the handle was dropped); an \
              enum would tie unrelated transitions together"
)]
#[derive(Debug, Default)]
struct StreamState {
    send: SendBuf,
    recv: RecvBuf,
    /// Whether this stream was **created** after its lane had a root.
    ///
    /// The rule is per stream, not per lane: a lane's first stream is opened
    /// before the handshake that produces the root, and it stays plain for its
    /// whole life — it is the control channel, and refusing its traffic the
    /// moment the lane acquires a root would kill the lane it just established.
    sealed: bool,
    /// This stream's key towards the peer, when the lane has a root.
    ///
    /// One per direction, because a key is derived for `(stream, direction)`:
    /// sharing one would let a reflected packet open as if it were the peer's.
    send_cipher: Option<Box<super::cipher::StreamCipher>>,
    /// This stream's key towards this end.
    recv_cipher: Option<Box<super::cipher::StreamCipher>>,
    /// Whether anything this end wrote on this stream has been acknowledged.
    ///
    /// While it is false the stream is **opening**: its packets are the handshake
    /// flight, one or two datagrams in the air however busy the lane is, and a
    /// lost one must be re-sent on the cheap floor (see `Sent::opening`). The
    /// first acknowledgement is what ends the opening, because from then on the
    /// stream has a round-trip sample and a wide flight behind it.
    acknowledged: bool,
    /// The local writer finished.
    local_fin: bool,
    /// The peer finished.
    peer_fin: bool,
    /// Set when either end reset the stream.
    reset: Option<ResetCode>,
    /// The local handle was dropped: nothing will read this stream again, and
    /// it is retired once everything written has been acknowledged.
    abandoned: bool,
    /// When this stream's handle was dropped, for the bounded wait below.
    ///
    /// A graceful close may wait for an acknowledgement for as long as the peer
    /// takes, and that is right: a response must not be truncated. It may not
    /// wait for ever, because the bytes it is waiting on stay counted in
    /// `buffered`, and `write`'s buffer ceiling is that number — see
    /// [`ABANDON_HOLD`] and `reap_abandoned`.
    abandoned_at: Option<Instant>,
    /// The finish has been told to the peer. A finish rides the last data frame
    /// when there is one; otherwise it is the reset frame this flag records, so
    /// it is queued once and never lost.
    fin_sent: bool,
    /// When this stream's send buffer first held bytes nobody had put on the
    /// wire yet.
    ///
    /// The last interval between "`Conn::write` accepted it" and "a packet
    /// carries it": every stage on both ends measured fast, the writer never
    /// parked, and a reader waited seven seconds, so the bytes were somewhere in
    /// between — and this is the only place that is. Cleared when a packet is
    /// built for the stream, so a backlogged stream is measured once per burst.
    queued_at: Option<Instant>,
    /// How many times this stream's bytes were declared lost and re-queued.
    ///
    /// Per stream, unlike the connection's own counter, because the question it
    /// answers is about one stream: a Noise handshake that took five seconds is
    /// either a queue or eight lost packets, and only this says which.
    retransmits: u32,
    /// Since when this stream has bytes nobody put on the wire.
    ///
    /// Per stream because the connection's own marker only fires a line when a
    /// whole pass builds nothing — and a lane carrying four streams never has an
    /// empty pass, so one parked writer stayed invisible for hundreds of seconds
    /// in every failing run of this cycle.
    unsent_at: Option<Instant>,
    /// Bytes inserted into this stream's receive buffer, which is what the peer's
    /// repairs are supposed to make grow. A lane whose repairs are posted and
    /// whose inserts stand still is a lane whose packets are not arriving.
    inserted: u64,
    /// Repair packets actually posted by the scheduler for this stream.
    ///
    /// `retransmits` counts a range being *declared* lost, which is a decision; this
    /// counts one being *put on the wire*, which is the fact. A stream whose
    /// declarations grow while this stands still is owed bytes that nothing picks,
    /// and the difference is invisible in every other counter.
    repairs_sent: u32,
}

/// One open stream the connection is holding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamHandle {
    /// The stream's id, which is what every frame names.
    pub(crate) id: u32,
}

/// Which bound refused a partial write.
///
/// The three wait for different things: the congestion window waits for an
/// acknowledgement, the peer's credit waits for its *reader*, and the buffer
/// ceiling waits for the peer's frontier to move past bytes this end already
/// holds. A stream that stalls with the credit refusing is waiting on the far
/// end's application, and nothing this connection sends will move it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// This end's congestion window had no room in flight.
    Window,
    /// The peer's advertised credit had no room for new bytes.
    Credit,
    /// `MAX_BUFFERED` had no room: the bytes are queued and unacknowledged, so the
    /// writer waits on the peer's frontier rather than on a packet in flight.
    ///
    /// Its own variant because this case used to be reported as `Window`
    /// (`credit.min(window).min(held)` keeps the smallest, and the classification
    /// only compared two of the three). On a lossy path the difference decides
    /// everything: a window refusal says acknowledgements are not coming back, a
    /// buffer refusal says they are coming back too slowly for what was admitted.
    Buffer,
    /// The oldest data packet is older than the retransmission timer, so nothing
    /// new may join a flight the timer cannot police.
    ///
    /// Its own variant because it is the one refusal no other instrument sees: the
    /// stall line reports a backlog and a credit block, and this refusal leaves
    /// neither — the bytes never enter the buffer, so the sender looks idle while
    /// the writer is parked and the visitor's window stays shut.
    Aged,
}

impl Conn {
    /// One log a second, however many writes the gate refuses.
    ///
    /// `&self` because the caller already holds the stream's own borrow: the stamp
    /// is a cell for exactly that reason, and this connection is never shared.
    fn table_log_due(&self) -> bool {
        let now = Instant::now();
        let due = self
            .table_logged_at
            .get()
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1));
        if due {
            self.table_logged_at.set(Some(now));
        }
        due
    }
}

/// One arriving stream range, as the frame carried it.
///
/// A named type because the receive path takes five things at once — where the
/// bytes start, what they are, which stream, whether they end it — and passing them
/// one by one is what pushed that function past the argument budget. The fields are
/// the frame's own, so a reader can check them against `wire::Frame::Stream`.
#[derive(Clone, Copy)]
struct Incoming<'a> {
    /// Which stream.
    id: u32,
    /// Where these bytes start in it.
    offset: u64,
    /// The bytes.
    data: &'a [u8],
    /// Whether this range ends the stream.
    fin: bool,
}

/// What the scheduler picked for one packet.
///
/// `repair` is the difference between new data and a range the recovery already
/// declared lost: the send point moves for the first and not for the second, and
/// mixing them up either re-sends a burst or loses a hole.
#[derive(Debug, Clone, Copy)]
struct Pick {
    /// Which stream.
    id: u32,
    /// Where the range starts in it.
    offset: u64,
    /// How many bytes it may carry, before the buffer's own bound.
    len: usize,
    /// Whether this range is being re-sent rather than sent.
    repair: bool,
}

/// One coherent picture of why a pass built nothing.
///
/// Taken **inside** the connection lock, at the instant the pass gave up, and
/// read later by the pump. Reading the connection afterwards would race the
/// dispatcher: `ack_pending` or a stream's queue can change between the refusal
/// and the log, and the two numbers then describe different moments — which cost
/// this investigation two runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NoneSnapshot {
    /// Which branch refused.
    pub(crate) reason: &'static str,
    /// Streams in the table, and how many the scheduler may serve.
    pub(crate) visited: usize,
    pub(crate) open: usize,
    /// Streams holding bytes the scheduler could not pick.
    pub(crate) queued: usize,
    /// Control work waiting: finishes, resets, and an acknowledgement.
    pub(crate) fins: usize,
    pub(crate) resets: usize,
    pub(crate) ack: bool,
    /// The three write budgets, as they stood.
    pub(crate) credit: u64,
    pub(crate) window: u64,
    pub(crate) buffer: u64,
    /// Where the round-robin cursor was.
    pub(crate) cursor: u32,
    /// The first stream the walk should have found, with the facts that decide
    /// it: `(id, open, has_unsent, next_offset, written, base)`.
    ///
    /// The summary above said `queued=1 … reason="no eligible stream"` — a
    /// stream the scheduler may serve, holding bytes, and a walk that found
    /// nothing — and no amount of aggregation resolves a contradiction like
    /// that. These are the numbers themselves.
    pub(crate) witness: Option<(u32, bool, bool, u64, u64, u64)>,
}

/// What this end is holding that its own readers have not taken.
///
/// The receive-side mirror of [`Conn::waiting`]. The credit this end advertises
/// follows **consumption**, so a stream whose reader stopped is the one fact
/// that can stall the *peer* while nothing here is waiting to go out, and no
/// other counter sees it. The worst stream is named because "one reader pinned
/// the window" and "everyone is behind" are different defects with different
/// fixes — the first is what a per-stream window bounds, the second is a link
/// that is simply too slow.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Backlog {
    /// How many streams hold bytes nobody has read.
    pub(crate) streams: usize,
    /// The stream holding the most, and how many bytes.
    pub(crate) worst: Option<(u32, u64)>,
    /// Everything held, across every stream.
    pub(crate) total: u64,
}

/// One stall line's worth of facts, already sampled.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stall {
    /// How long the oldest byte nobody could put on the wire has waited.
    pub(crate) unsent_ms: u128,
    /// How long the oldest byte in flight has waited for an acknowledgement.
    pub(crate) unacked_ms: u128,
    /// What this end's own readers owe the peer.
    pub(crate) backlog: Backlog,
    /// The stream that is holding the wire back, with the facts that decide why.
    ///
    /// The ages say *that* a stream stalled; this says *which* and whether it is
    /// bytes nobody could send (no room), bytes that were lost and are owed again
    /// (retransmits), bytes the writer never produced, or a stream the scheduler
    /// skips because it was reset.
    pub(crate) worst: Option<WorstStream>,
    /// What this end will **tell the peer** on its next packet: its cumulative
    /// receive frontier and the largest number it has seen.
    ///
    /// Beside the flight's own range this answers the question a stall cannot: a
    /// sender stuck waiting for numbers the block can no longer name — anything
    /// more than 64 behind `largest` — is a different defect from one whose
    /// numbers are inside the window and simply not acknowledged
    /// (`HANDOFF.md` §4.107).
    pub(crate) recv_below: u32,
    pub(crate) recv_largest: u32,
    /// The contiguous byte frontier this end reports, **and the stream it names**.
    ///
    /// The stream matters as much as the number: the prefix is evidence about one
    /// stream only, so a sender stuck on a different one learns nothing from it
    /// (`HANDOFF.md` §4.109).
    pub(crate) recv_prefix: u64,
    pub(crate) recv_stream: u32,
    /// The lowest and highest packet numbers still in flight here.
    pub(crate) flight_numbers: Option<(u32, u32)>,
}

/// The one stream whose owed bytes are holding a lane back.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorstStream {
    /// Which stream.
    pub(crate) id: u32,
    /// Bytes accepted from the writer and not yet on the wire.
    pub(crate) unsent: u64,
    /// How many times this stream's ranges have been declared lost.
    pub(crate) retransmits: u32,
    /// Where the send cursor stands.
    pub(crate) next: u64,
    /// How much the writer has handed over.
    pub(crate) written: u64,
    /// Whether an abort is what keeps this stream out of the scheduler.
    pub(crate) reset: bool,
    /// Repair packets this stream has actually posted.
    pub(crate) repairs_sent: u32,
    /// The repair range the scheduler would send next, if any.
    pub(crate) repair_at: u64,
    /// How many repair ranges are still owed.
    pub(crate) repairs_pending: usize,
    /// Tracked records covering the frontier, and the stream's records in total.
    pub(crate) covering: u32,
    pub(crate) tracked: u32,
    /// The packet number of the record covering the frontier.
    pub(crate) covering_number: u32,
    /// The delivery frontier: everything below it is acknowledged.
    pub(crate) base: u64,
    /// Bytes this stream has handed to packets.
    pub(crate) handed_over: u64,
    /// The repair it is owed, if it is owed one: `(offset, length)`.
    ///
    /// A stream that has put every byte on the wire and is still "owed" a range
    /// is a stream in a repair loop, and *which* range it is re-sending is the
    /// difference between a lost tail and a loss that is declared forever. This
    /// field is why the sealed striped deadlock could finally be named.
    pub(crate) repair: Option<(u64, u64)>,
}

/// The frame class, for a log line that must not print the payload.
///
/// Its own function because the one caller needs a `&'static str` inside a `match`
/// that later takes the frame by value, and because a payload printed next to a
/// packet number is the kind of log line this crate's budget forbids.
fn frame_name(frame: &super::wire::Frame<'_>) -> &'static str {
    match frame {
        super::wire::Frame::None => "none",
        super::wire::Frame::Stream {
            fin: true,
            data: [],
            ..
        } => "fin",
        super::wire::Frame::Stream { .. } => "stream",
        super::wire::Frame::Credit { .. } => "credit",
        super::wire::Frame::Reset { .. } => "reset",
        super::wire::Frame::Datagram { .. } => "datagram",
    }
}

/// Seal the payload `out[payload_at..]` in place and append its tag.
///
/// The header — everything before the payload — is the associated data, so the
/// tag covers which stream this packet belongs to and which number it carries.
/// The tag goes after the ciphertext, inside the room `encode_stream` reserved.
fn seal_stream_payload(
    cipher: Option<&super::cipher::StreamCipher>,
    sealed: bool,
    out: &mut Vec<u8>,
    packet_start: usize,
    payload_at: usize,
    number: u32,
) -> bool {
    if !sealed {
        return true;
    }
    let Some(cipher) = cipher else {
        return false;
    };
    // This packet's header alone: what the receiver has in front of the payload.
    let header = out[packet_start..payload_at].to_vec();
    let Some(tag) = cipher.seal(u64::from(number), &header, &mut out[payload_at..]) else {
        // Nothing half-built reaches the wire: the caller gets its room back
        // rather than a payload in the clear.
        out.truncate(payload_at);
        return false;
    };
    out.extend_from_slice(&tag);
    crate::transport::stats::note_sealed_sent();
    true
}

/// The two directions of one stream's keys, when the lane has a root.
///
/// A named type because it travels: minted when a stream is opened or accepted,
/// kept in its `StreamState`, and asked for by both the send and the receive
/// path. Every `None` in it is load-bearing — a missing direction means "do not
/// seal this stream", never "send it in the clear".
type StreamKeys = (
    Option<Box<super::cipher::StreamCipher>>,
    Option<Box<super::cipher::StreamCipher>>,
);

/// One datagram's payload after opening, and the keys a *new* stream needs.
///
/// The second field is `Some` only when the packet opened a stream this end had
/// never seen: that is the one moment the receive path mints keys, and returning
/// them here is what keeps "opened by this packet" and "a stream already in the
/// table" from being two different derivations.
type Opened<'a> = (std::borrow::Cow<'a, [u8]>, Option<StreamKeys>);

/// What one lane put on the wire, against what the peer confirmed — per lane.
///
/// The window line has always known how many bytes it *posted*; it never knew how
/// many of those were a byte sent before. That difference is the first question a
/// lossy path asks: a lane that re-sends a fifth of its bytes is paying for a
/// recovery defect, not for a small window, and the two want opposite fixes. The
/// process-wide counters cannot answer it — `stats::note_refused` and
/// `stats::note_none_reason` sum every lane into one distribution, so four
/// interleaved lanes read as one that oscillates (`HANDOFF.md` §4.179).
///
/// Every field is a count this end can produce from what it already holds: no new
/// clock read, no new allocation, and two adds per packet on the send path.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SendLedger {
    /// Stream bytes handed to packets, repairs included: everything that left.
    sent_bytes: u64,
    /// How many of those bytes were a range already sent once.
    ///
    /// `sent_bytes - sent_repair_bytes` is the new data the lane moved, and
    /// `sent_repair_bytes` above it is the price of recovery. A carrier with a
    /// large share here is not short of window; it is re-sending what the path
    /// dropped, and the question becomes whether it had to.
    sent_repair_bytes: u64,
    /// Packets that carried stream bytes.
    sent_packets: u64,
    /// Stream bytes the peer confirmed, which is the delivery rate's numerator.
    ///
    /// Counted from the **stream's frontier**, not from the records: a byte that
    /// crossed the wire twice is one byte of delivery, and adding up retired records
    /// would count it twice — once when the repair was acknowledged, once when a
    /// prefix finally covered the original. The frontier cannot be fooled that way,
    /// which is why this is the number every ratio here is taken against.
    acked_bytes: u64,
    /// Record bytes retired by the packet-number block, by the stream byte prefix.
    ///
    /// Record bytes rather than stream bytes: a repair and its original are two
    /// records of one range, and this pair says which path retired which record —
    /// the accounting question, `pfx` being the path that does *not* grow the window.
    acked_by_number: u64,
    acked_by_prefix: u64,
    /// Of those, the ones whose record had already been declared lost.
    ///
    /// A declaration the path did not support, or one whose range a repair then
    /// delivered: either way this end had given up before the bytes were confirmed,
    /// and the two cases are told apart by which path retired the record.
    acked_late: u64,
    /// Loss declarations by the evidence that named them: a later packet seen, or
    /// the timer finding nothing fresh to probe.
    lost_by_threshold: u64,
    lost_by_timer: u64,
    /// Timer probes that re-sent one range because nothing was stale.
    probes: u64,
    /// Bytes the window grew by since the last report.
    grown: u64,
    /// Writes refused by each bound. `aged` is the flight-age gate, which refuses a
    /// lane that has done nothing wrong except fall behind on acknowledgements;
    /// `buffer` is the ceiling this end's own `MAX_BUFFERED` put on bytes the peer
    /// has not confirmed.
    refused_window: u64,
    refused_credit: u64,
    refused_buffer: u64,
    refused_aged: u64,
    /// Passes that built nothing, by reason — the per-lane shape of
    /// `stats::note_none_reason`'s process-wide table.
    stop_no_stream: u64,
    stop_nothing_to_say: u64,
    stop_closed: u64,
    stop_other: u64,
    /// Repair packets the scheduler had ready but did not send, because the re-sent
    /// bytes already in flight were at or above the window.
    ///
    /// The number that says the budget bit rather than the lane having nothing to do.
    repairs_gated: u64,
    /// Stream bytes that arrived here, and how many of them the stream already
    /// held.
    ///
    /// The measurement §4.183 asks for: a carrier that re-sends a range the peer
    /// already buffered pays for its own recovery with no delivery to show for it,
    /// and no counter distinguished the two before this one. `dup` as a share of
    /// `recv` is what says whether the lossy cell's 70–90 % duplicate share is real
    /// loss or re-sent bytes the peer had.
    recv_bytes: u64,
    recv_duplicate_bytes: u64,
}

impl SendLedger {
    /// A packet carrying `len` stream bytes, `repair` if the range had been sent.
    fn note_sent(&mut self, len: usize, repair: bool) {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        self.sent_bytes = self.sent_bytes.saturating_add(len);
        if repair {
            self.sent_repair_bytes = self.sent_repair_bytes.saturating_add(len);
        }
        self.sent_packets = self.sent_packets.saturating_add(1);
    }

    /// Stream bytes the peer confirmed, from the frontier that moved to make them
    /// confirmed.
    fn note_delivered(&mut self, bytes: u64) {
        self.acked_bytes = self.acked_bytes.saturating_add(bytes);
    }

    /// One retired record, from whichever acknowledgement named it.
    fn note_retired(&mut self, bytes: u64, by_prefix: bool, late: bool) {
        if by_prefix {
            self.acked_by_prefix = self.acked_by_prefix.saturating_add(bytes);
        } else {
            self.acked_by_number = self.acked_by_number.saturating_add(bytes);
        }
        if late {
            self.acked_late = self.acked_late.saturating_add(bytes);
        }
    }

    /// The share of what left that was a range already sent once, in percent.
    fn repair_share_pct(&self) -> u64 {
        self.sent_repair_bytes
            .saturating_mul(100)
            .checked_div(self.sent_bytes)
            .unwrap_or(0)
    }
}

/// One line, so the window report stays one field wide.
///
/// The words are chosen to be read in place: `sent` and `ack` are the two sides of
/// the duplicate question, `rep` is the answer as a percentage, `late` is how much
/// of `ack` arrived after the lane had already decided the packet was lost, and
/// `refus`/`stop` say which bound or budget ended the passes in this second.
impl std::fmt::Display for SendLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sent={} rep={}({}%) pkt={} ack={} num={} pfx={} late={} \
             loss={}t/{}m probe={} grow={} refus={}w/{}b/{}c/{}a \
             stop={}none/{}quiet/{}closed/{}other recv={} dup={} gate={}",
            self.sent_bytes,
            self.sent_repair_bytes,
            self.repair_share_pct(),
            self.sent_packets,
            self.acked_bytes,
            self.acked_by_number,
            self.acked_by_prefix,
            self.acked_late,
            self.lost_by_threshold,
            self.lost_by_timer,
            self.probes,
            self.grown,
            self.refused_window,
            self.refused_buffer,
            self.refused_credit,
            self.refused_aged,
            self.stop_no_stream,
            self.stop_nothing_to_say,
            self.stop_closed,
            self.stop_other,
            self.recv_bytes,
            self.recv_duplicate_bytes,
            self.repairs_gated,
        )
    }
}

/// A connection between two of this carrier's endpoints.
#[derive(Debug)]
pub(crate) struct Conn {
    id: u32,
    /// The dialing end numbers its streams odd and the accepting end even, so
    /// both allocate from disjoint spaces and an id is unique with no
    /// negotiation at all.
    dialer: bool,
    /// The lane's root, once its first stream finished a handshake.
    ///
    /// Set by the carrier from that handshake's **secret** material and its
    /// transcript (see [`super::cipher::LaneRoot`]); from then on every stream
    /// this connection opens or accepts is sealed with a key derived from it, and
    /// the peer knows the same root. `None` means the lane is still in its
    /// handshake phase and streams are plain — or that the root could not be
    /// derived, in which case sealing stays off rather than falling back to a
    /// root nobody computed.
    lane_root: Option<super::cipher::LaneRoot>,
    next_stream: u32,
    streams: BTreeMap<u32, StreamState>,
    /// Where the round-robin cursor stands.
    cursor: u32,
    /// When a stream last had bytes the scheduler could not put on the wire.
    ///
    /// `None` while everything queued has been handed to the pump; set on the
    /// first write that finds nothing sendable (window closed), cleared when the
    /// scheduler next finds work. It is the send-buffer half of [`Conn::waiting`]
    /// and it costs two `Instant` reads per write call, not per packet.
    unsent_at: Option<Instant>,
    /// When a write found the connection **idle** and queued bytes on it, and
    /// which stream they were for.
    ///
    /// This is the one number that separates the two ways an open can be slow:
    /// bytes that waited for the pump to wake (a scheduler defect, and it shows
    /// here as milliseconds between queue and post) versus bytes that went out at
    /// once and waited for the far end to answer (a path or acknowledgement
    /// defect, invisible here). Measured at millisecond resolution, logged only
    /// past a threshold, so a hot lane pays one clock read per idle write.
    idle_queued: Option<(Instant, u32)>,
    /// How many pump passes have found that idle write still queued.
    ///
    /// One means the first pass that looked at this connection carried it; two or
    /// more mean passes ran and chose not to serve this lane, which is a
    /// scheduling question in this end's own loop rather than a wake-up one.
    idle_passes: u32,
    /// When a stall line was last written, so one long stall costs one line a
    /// second rather than one per pass.
    stall_logged_at: Option<Instant>,
    /// Which bound refused the last partial write (see [`Conn::write`]).
    last_refusal: Option<Refusal>,
    /// When the recovery table's hold was last logged, so a refused write in a hot
    /// loop costs one line a second and not one per call.
    table_logged_at: std::cell::Cell<Option<Instant>>,
    /// Why the last [`Self::poll_transmit`] answered `None`, when it did.
    ///
    /// The pump's "built nothing while bytes were queued" line reads this: a
    /// refusal has several sources that look identical from outside (no eligible
    /// stream, a stream the peer reset, a zero-length range, a frame that would
    /// not fit, or simply nothing left to say), and each is fixed somewhere else.
    none_reason: Option<&'static str>,
    /// The coherent picture behind that reason (see [`NoneSnapshot`]).
    none_snapshot: Option<NoneSnapshot>,
    recovery: Recovery,
    /// The connection's receive blocks.
    ///
    /// Every stream takes from here and gives back as the reader passes what it
    /// took, so the receive path's footprint is what the streams hold and a
    /// burst reuses the blocks the last one released rather than asking the
    /// allocator once per datagram.
    blocks: Blocks,
    acks: AckSet,
    ack_pending: bool,
    /// Bytes the peer allows this end to have in flight, absolute.
    send_credit: u64,
    /// Bytes this end allows the peer, absolute.
    recv_credit: u64,
    /// The highest stream id this end has accepted from the peer.
    ///
    /// Stream ids are monotonic, so this is what separates "a stream the peer is
    /// opening" from "a packet for a stream this end already retired": an unknown
    /// id in the peer's space that is **below** this one cannot be new, and taking
    /// it would open a phantom stream and a phantom accept for a visitor whose
    /// connection had already closed (`HANDOFF.md` §4.120).
    highest_inbound: Option<u32>,
    /// The last cumulative prefix the peer reported, kept only for reporting: at a
    /// stall the question is whether the sender retired past the peer's word (it
    /// believes bytes delivered that the peer never had) or the peer's word is
    /// ahead of the retirement (the peer holds bytes the sender keeps re-sending).
    last_ack_prefix: u64,
    /// The peer's packet-number watermark and bitmap, for the same line.
    last_ack_below: u32,
    last_ack_bits: u64,
    /// The highest packet number the peer reports having seen.
    last_ack_largest: u32,
    /// Declared-loss ranges that covered a stream's frontier, and those that sat
    /// above it. Only the first kind can unblock a stalled stream.
    repairs_at_base: u64,
    repairs_above: u64,
    /// What this lane sent, what the peer confirmed, and why it stopped.
    ///
    /// Per lane, and reset once a second by the window line: the numbers are only
    /// useful as *this* lane's series, and the process-wide totals this file used
    /// to lean on cannot be divided back into lanes (`HANDOFF.md` §4.179).
    ledger: SendLedger,
    /// Acknowledgements this end applied and attached, for the window line.
    acks_applied: u64,
    acks_attached: u64,
    /// Packet numbers taken, and packets actually built from one.
    ///
    /// The difference is the one exit no line names on a busy lane: `next_number`
    /// is taken before the header is framed, and a frame that does not fit returns
    /// without posting — leaving a number consumed and, if the peer never sees it,
    /// a frontier pinned under it for the connection's life.
    issued: u64,
    /// Bytes this connection has handed to the socket, cumulative.
    ///
    /// The rate's numerator (`posted_rate`): what left *this* lane's socket, which
    /// is the only per-lane answer to "is the send path the constraint".
    posted: u64,
    /// When `posted` was last sampled for the one-second report, and its value then.
    posted_at: Instant,
    posted_at_report: u64,
    posted_datagrams_at_report: u64,
    /// This lane's own pass counters: passes, packets built, and the two waits.
    ///
    /// **Per lane, and that is the point again.** The process-wide versions cannot be
    /// divided by the number of lanes: `posted_mbit_per_s` already showed the lanes are
    /// not symmetric, so "19 207 flushes a second over four lanes is 208 µs each" was
    /// arithmetic over an assumption (HANDOFF §4.60). Reported once a second, so this
    /// costs two adds per pass and nothing per packet.
    passes: u64,
    pass_packets: u64,
    flush_us: u64,
    gap_us: u64,
    /// The gap's tail: its maximum, and how many gaps reached `GAP_SLOW_US`.
    ///
    /// The gap is the second-largest term in the cycle (26 % of a busy lane-second) yet
    /// 5-7 µs at p50, so its distribution is tail-dominated and nothing had read the
    /// tail (HANDOFF §4.62). Two fields, two comparisons per pass: enough to say whether
    /// the 26 % is many small gaps or a few large ones, which is the question that
    /// decides where the fix goes.
    gap_max_us: u64,
    gap_slow: u64,
    /// Microseconds spent inside `yield_now`, so the gap can be split either side of it.
    yield_us: u64,
    /// Microseconds spent in the payload copy, and how many copies it came from.
    ///
    /// `flush` is 41.6 % of a busy lane-second at 1 µs a packet, and the copy is the one
    /// part of a packet that moves a kilobyte; everything else is bookkeeping. Separating
    /// them is what decides whether the next change is in the data path or around it.
    copy_us: u64,
    copies: u64,
    copy_bytes: u64,
    /// Microseconds spent in the stream pick, and in the recovery record.
    ///
    /// The two `BTreeMap` operations per packet, timed apart from everything else: §4.64
    /// showed the payload copy is 0.5 % of the build cost, so the question is now whether
    /// one of these two is most of the rest (HANDOFF §4.65).
    pick_us: u64,
    record_us: u64,
    /// Microseconds spent advancing the round-robin cursor, a second walk per packet.
    advance_us: u64,
    /// Datagrams queued by the layer above, oldest first, and the counters.
    ///
    /// **Separate from every stream**, which is the whole point: a datagram has no
    /// offset, so it cannot be repaired and must not be counted in the flight. Bounded
    /// because a queue is a queue — a producer faster than the pump drops its oldest
    /// datagram, which is what a datagram service already tolerates.
    out_datagrams: VecDeque<(u32, Bytes)>,
    /// Datagrams handed to the wire, and datagrams received off it.
    ///
    /// The receive counter is what the next increment watches: it must grow while the
    /// flight does not (HANDOFF §4.71).
    datagrams_out: u64,
    datagrams_in: u64,
    /// Datagrams that arrived, waiting for the layer above to take them.
    ///
    /// Bounded like the outgoing queue and for the same reason: an arriving datagram
    /// that has waited this long is one the application has given up on, and holding it
    /// delays the next one. The receive counter counts everything that *arrived*; this
    /// queue holds what has not been collected yet, so the two differ by exactly the
    /// drops.
    in_datagrams: VecDeque<(u32, Bytes)>,
    /// This lane's own interval clock.
    ///
    /// **Not `posted_at`.** The first version reused it and `posted_rate` — called a few
    /// lines earlier in the same window report — had just set it to `now`, so the
    /// interval came out zero and every line printed `cycle=n/a` for a whole run. Two
    /// independent samples need two independent clocks.
    pass_at: Instant,
    /// Datagrams this connection has posted and read, cumulative.
    ///
    /// Per connection because the process-wide pair cannot say *which* lane loses
    /// them: a lane posting into an address nothing reads keeps its `posted`
    /// growing while its `received` stands still, and that is the shape a
    /// re-dialled pool entry would leave behind.
    posted_total: u64,
    received_total: u64,
    /// Packet numbers this end framed and identified as its own, and those it was
    /// able to acknowledge.
    ///
    /// Acknowledgements are inserted only after a frame is accepted, so a refused
    /// frame never advances the cumulative watermark. One such number can then pin
    /// the peer's whole flight for the connection's life while the path underneath
    /// is perfectly reliable — the difference between these two counts is that
    /// number, counted where it is created.
    numbers_seen: u64,
    numbers_acked: u64,
    /// The first data packet number posted, and the first seen with the watermark
    /// it arrived under. The cumulative watermark advances only when the number
    /// equal to it arrives, so these two say whether the first number ever left and
    /// whether it ever landed.
    first_posted: Option<u32>,
    first_seen: Option<(u32, u32)>,
    built: u64,
    /// The credit this end has already told the peer about.
    advertised: u64,
    /// Bytes the reader has taken, which is what `recv_credit` follows.
    read: u64,
    /// The stream whose arrived prefix rides this end's acknowledgements.
    ///
    /// One stream's frontier, not every stream's: a packet acknowledges one
    /// block, and the most recent one is enough because the value is monotonic
    /// per stream and every packet carries a fresh copy (see `wire::ACK`).
    arrived_stream: u32,
    /// How many of that stream's bytes arrived with no hole.
    arrived_prefix: u64,
    /// Streams this connection touched on the receive path since the pump last
    /// asked, so it can wake exactly those readers.
    ///
    /// A lane carries several streams and a batch usually delivers bytes for one
    /// of them. Waking the whole lane scheduled every parked reader to find
    /// nothing — the churn behind the wake-to-poll latency an arrival pays for
    /// its handshake — so the receive path records *which* streams changed and
    /// the pump wakes only those. Duplicates are harmless and cheap: a second
    /// wake of an already-woken task is one atomic flag.
    woke: Vec<u32>,
    /// Streams the peer opened that nothing has taken yet.
    inbound: VecDeque<u32>,
    /// When each of those was opened by the peer's first datagram.
    ///
    /// The accept path reads it to say how long a stream waited between its
    /// first datagram and the task that takes it — the number that separates "the
    /// carrier delivered it late" from "the task was not scheduled" (see
    /// `MhdLane::accept_stream`).
    inbound_at: BTreeMap<u32, Instant>,
    /// When each live stream was created, whichever end created it.
    ///
    /// One clock read per stream, and the number that separates the two halves
    /// of a slow open: the age at the moment a packet carrying the stream's first
    /// byte is *handed to the socket* is the time the carrier held it, while the
    /// rest of the opening's duration is the wire. Without the split a "the open
    /// took 750 ms" reading cannot say which side to fix (see `transmit_stream`).
    stream_opened: BTreeMap<u32, Instant>,
    /// Aborts waiting for the next pass: a stream one end let go of.
    resets: VecDeque<(u32, ResetCode)>,
    /// Finishes waiting for a packet of their own: (stream, end offset).
    ///
    /// A finish normally rides the last data packet. When every byte was already
    /// acknowledged there is no such packet left to carry it, and the finish needs
    /// one of its own — a tracked, retransmitted one. A finish carried by an
    /// unacknowledged control frame is lost with that frame, and the peer's reader
    /// then waits for an end that will never arrive.
    pending_fins: VecDeque<(u32, u64)>,
    /// Streams whose writer finished but whose finish has not been said yet.
    ///
    /// A set rather than a scan: an acknowledgement settles every stream in it,
    /// and walking the whole table per acknowledgement is one more cost that
    /// grows with the number of live streams.
    finishing: BTreeSet<u32>,
    /// Every byte this end has ever queued for the peer.
    ///
    /// The credit window is spent on this, and it is monotonic: a byte counts
    /// against the window from the moment it is queued until the peer's reader
    /// has taken it, which is what the peer's absolute credit describes.
    written: u64,
    /// Bytes the send buffers still hold, across every stream.
    ///
    /// Kept rather than summed: it bounds a write on every call, and a sum over
    /// the stream table there would grow with the connection's width.
    buffered: u64,
    /// Bytes on the wire and not yet acknowledged.
    ///
    /// This is what the congestion window bounds. The peer's credit says how much
    /// it can *hold*; this says how much the path is allowed to have in flight,
    /// and the two are deliberately different: a 32 MiB credit window on a fast
    /// path would otherwise put twenty thousand packets in the recovery table,
    /// where every acknowledgement is a walk of a tree that size. The count lives
    /// in the recovery table, which is the only thing that removes a packet from
    /// it.
    /// The congestion window: the most this end may have in flight.
    ///
    /// Grown additively as acknowledgements arrive and never cut on loss — a
    /// loss is repaired by retransmission, not paid for by halving the pipe for a
    /// round trip. The rate itself is the pacer's business (see the I/O layer);
    /// this is the bound that keeps the flight, and therefore the recovery
    /// table, proportional to the path's bandwidth-delay product.
    cwnd: u64,
    /// When the periodic stats line last read the window.
    window_reported_at: Instant,
    /// Whether the window is still looking for the path's capacity.
    ///
    /// True until the path drops something: while it is, every acknowledged byte
    /// buys a byte of window (doubling per round trip). It latches off at the
    /// first loss and never comes back — a path that has dropped is a path whose
    /// capacity has been found.
    slow_start: bool,
    /// Bytes acknowledged since the window last grew additively.
    acked_since_growth: u64,
    /// Set when the connection ended.
    closed: Option<ResetCode>,
    /// A close that waits for what is already queued to leave first.
    ///
    /// A refusal or a shutdown answer is written and then the lane is told to
    /// die; closing immediately would drop the answer with the connection and
    /// leave the peer waiting on a request that was in fact refused.
    close_after: Option<ResetCode>,
    /// When the next keepalive is due.
    keepalive_at: Instant,
    /// When the peer's last datagram arrived.
    last_recv: Instant,
    /// The per-connection counters the status API reads, when a lane owns this
    /// connection. The engine bumps the re-send count here; the pump counts
    /// what leaves the socket. `None` in the in-memory tests, which measure
    /// behaviour rather than telemetry.
    health: Option<std::sync::Arc<SessionHealth>>,
}

impl Conn {
    /// Give this connection the lane's root, built from one finished handshake.
    ///
    /// Called once, when the lane's first stream completed its handshake — the
    /// moment both ends hold the same secret and the same transcript. A connection
    /// that already has a root keeps the first one: re-keying a live lane would
    /// leave the streams already in flight unreadable.
    ///
    /// A derivation that refuses leaves `lane_root` empty, which means **no
    /// sealing**: the streams stay plain and both ends keep working, rather than
    /// sealing under a root nobody computed.
    pub(crate) fn set_lane_secret(&mut self, secret: &super::cipher::Secret) {
        if self.lane_root.is_none() {
            self.lane_root = super::cipher::LaneRoot::from_handshake(&secret.0, &secret.1);
        }
    }

    /// Whether this connection has a root, so a new stream is sealed.
    pub(crate) fn has_lane_root(&self) -> bool {
        self.lane_root.is_some()
    }

    /// The two directions of one stream's key, when the lane has a root.
    fn stream_ciphers(&self, stream: u32) -> StreamKeys {
        use super::cipher::Direction;
        let Some(root) = self.lane_root.as_ref() else {
            return (None, None);
        };
        let out = if self.dialer {
            Direction::ToServer
        } else {
            Direction::ToClient
        };
        let back = if self.dialer {
            Direction::ToClient
        } else {
            Direction::ToServer
        };
        match (root.stream(out, stream), root.stream(back, stream)) {
            (Some(send), Some(recv)) => (Some(Box::new(send)), Some(Box::new(recv))),
            // Unreachable for a 32-byte derivation; an unsealed stream would be a
            // silent hole, so both directions go missing together and the send and
            // receive paths refuse rather than fall back to plaintext.
            _ => (None, None),
        }
    }

    /// A fresh connection. The dialing end opens odd stream ids.
    pub(crate) fn new(id: u32, dialer: bool) -> Self {
        let now = Instant::now();
        Self {
            id,
            dialer,
            lane_root: None,
            next_stream: u32::from(dialer),
            streams: BTreeMap::new(),
            cursor: u32::from(dialer),
            unsent_at: None,
            idle_queued: None,
            idle_passes: 0,
            stall_logged_at: None,
            last_refusal: None,
            table_logged_at: std::cell::Cell::new(None),
            none_reason: None,
            none_snapshot: None,
            recovery: Recovery::new(),
            blocks: Blocks::default(),
            acks: AckSet::default(),
            ack_pending: false,
            send_credit: WINDOW / 2,
            recv_credit: WINDOW,
            highest_inbound: None,
            last_ack_prefix: 0,
            last_ack_below: 0,
            last_ack_bits: 0,
            last_ack_largest: 0,
            repairs_at_base: 0,
            repairs_above: 0,
            ledger: SendLedger::default(),
            acks_applied: 0,
            acks_attached: 0,
            issued: 0,
            posted_total: 0,
            posted: 0,
            posted_at: now,
            posted_at_report: 0,
            posted_datagrams_at_report: 0,
            passes: 0,
            pass_packets: 0,
            flush_us: 0,
            gap_us: 0,
            gap_max_us: 0,
            gap_slow: 0,
            yield_us: 0,
            out_datagrams: VecDeque::new(),
            datagrams_out: 0,
            datagrams_in: 0,
            in_datagrams: VecDeque::new(),
            copy_us: 0,
            copies: 0,
            copy_bytes: 0,
            pick_us: 0,
            record_us: 0,
            advance_us: 0,
            pass_at: now,
            received_total: 0,
            numbers_seen: 0,
            numbers_acked: 0,
            first_posted: None,
            first_seen: None,
            built: 0,
            advertised: 0,
            read: 0,
            arrived_stream: 0,
            arrived_prefix: 0,
            woke: Vec::new(),
            inbound: VecDeque::new(),
            inbound_at: BTreeMap::new(),
            stream_opened: BTreeMap::new(),
            resets: VecDeque::new(),
            pending_fins: VecDeque::new(),
            finishing: BTreeSet::new(),
            written: 0,
            buffered: 0,
            cwnd: INITIAL_CWND,
            window_reported_at: Instant::now(),
            slow_start: true,
            acked_since_growth: 0,
            closed: None,
            close_after: None,
            keepalive_at: now + KEEPALIVE,
            last_recv: now,
            health: None,
        }
    }

    /// Attach the counters the status API reads.
    pub(crate) fn with_health(mut self, health: std::sync::Arc<SessionHealth>) -> Self {
        self.health = Some(health);
        self
    }

    fn note_retransmits(&self, count: usize) {
        if let Some(health) = &self.health
            && count > 0
        {
            health.retransmits.fetch_add(
                u64::try_from(count).unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// Open a stream, answering its id.
    pub(crate) fn open(&mut self) -> StreamHandle {
        self.open_at(Instant::now())
    }

    /// Open a stream, recording when it was created.
    fn open_at(&mut self, now: Instant) -> StreamHandle {
        // **Stream ids wrap by two, so a wrapped id can name a stream still in
        // use.** Inserting unconditionally would replace that stream's state — its
        // ciphers, its frontier, its place in the window — with a fresh one, and
        // the visitor reading it would never see an error (invariant 1 of
        // `HANDOFF.md` §4.120). Probing at most `len() + 1` candidates of one parity
        // always finds a free id: only `len()` ids are occupied in total, so by the
        // pigeonhole one of `len() + 1` candidates is free.
        let mut id = self.next_stream;
        let mut probes = 0usize;
        while self.streams.contains_key(&id) && probes <= self.streams.len() {
            id = id.wrapping_add(2);
            probes += 1;
        }
        self.next_stream = id.wrapping_add(2);
        let (send_cipher, recv_cipher) = self.stream_ciphers(id);
        let sealed = self.lane_root.is_some();
        let state = StreamState {
            sealed,
            send_cipher,
            recv_cipher,
            ..StreamState::default()
        };
        self.streams.insert(id, state);
        self.stream_opened.insert(id, now);
        StreamHandle { id }
    }

    /// Take a stream the peer opened, if one is waiting.
    ///
    /// Tests only: every real caller wants the age, which is what separates a
    /// late delivery from a late accept (see [`Self::accept_aged`]).
    #[cfg(test)]
    pub(crate) fn accept(&mut self) -> Option<StreamHandle> {
        self.accept_aged(Instant::now()).map(|(handle, _)| handle)
    }

    /// Take a stream the peer opened, with how long it waited for this call.
    ///
    /// The age is measured from the datagram that named the stream, so it is the
    /// wait the *accept path* added — the carrier's delivery time is inside it
    /// only as far as the datagram itself was late.
    /// When this stream first had bytes for its reader, if it has yet.
    pub(crate) fn reader_ready_at(&self, stream: u32) -> Option<Instant> {
        self.streams.get(&stream)?.recv.ready_at()
    }

    pub(crate) fn accept_aged(&mut self, now: Instant) -> Option<(StreamHandle, Duration)> {
        let id = self.inbound.pop_front()?;
        let age = self
            .inbound_at
            .remove(&id)
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        Some((StreamHandle { id }, age))
    }

    /// Queue bytes on a stream, answering how many the credit let through.
    ///
    /// A partial write is normal: the caller writes again when the window moves,
    /// which is what makes a slow reader backpressure its writer instead of
    /// filling the sender's memory.
    pub(crate) fn write(&mut self, stream: u32, bytes: &[u8]) -> usize {
        // The peer's credit is an absolute byte count — where its reader has
        // got to, plus one window — so the room left is measured against
        // everything this end has ever queued, not against what is still
        // unacknowledged. Subtracting the unacknowledged count instead lets a
        // sender write one extra window per acknowledgement it receives: on a
        // gigabit transfer that is hundreds of megabytes past the peer's
        // buffer, every packet of which the peer must drop and therefore never
        // acknowledge, which is how a healthy lane gets declared dead.
        //
        // The congestion window is the second bound, and it is what the flight —
        // and therefore the recovery table — is actually sized by.
        let credit = self.send_credit.saturating_sub(self.written);
        let window = self.cwnd.saturating_sub(self.recovery.flight_bytes());
        // The buffer ceiling is the third bound: a byte stays buffered until an
        // acknowledgement moves its stream's frontier past it, and a frontier
        // that stalls must cost the writer a wait rather than unbounded memory.
        let held = MAX_BUFFERED.saturating_sub(self.buffered);
        let room = credit.min(window).min(held);
        // The fourth bound, and the one this cycle needed: **the flight may not
        // outlive the retransmission timer.** A carrier that lets a flight grow
        // past the timeout supposed to find a hole inside it is asking two
        // contradictory things of one number — the hole is then older than the
        // timer, the timer is armed from a newer packet, and nothing recovers it
        // (measured as one connection in a hundred arriving 0.85-4.1 s late).
        // Below this age, the oldest data packet is always younger than
        // [`MIN_RTO`], so a firing means a real hole and not a deep queue.
        let aged = self
            .recovery
            .oldest_data_age(Instant::now())
            .is_some_and(|age| age >= self.recovery.current_rto());
        // Decided before the stream's own borrow: the age gate needs to know
        // whether this stream is still opening, and the log needs `&self`.
        let still_opening = self.streams.get(&stream).is_some_and(opening);
        let refused_by_age = aged && !still_opening;
        let log_table =
            refused_by_age && crate::transport::stats::enabled() && self.table_log_due();
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        // **An opening is exempt**: the bound protects a data flight from outliving
        // the timer that finds its holes, and a new stream's handshake is not part of
        // that flight. Applying it here made a lane behind on acknowledgements — which
        // is every lane on a lossy path — refuse to start a stream at all.
        //
        // The label is built fresh for this call. Reading the previous call's value
        // made `Aged` sticky: once set, every later refusal was labelled `Aged` even
        // with an empty table, and a measurement built on it said the age gate
        // refused 4.6 million writes when many of those were the congestion window.
        let room = if refused_by_age { 0 } else { room };
        // A stream that has already written a reserve's worth stops short of it,
        // so an opening stream always finds room (see `OPEN_RESERVE`).
        //
        // **And an opening may overdraw the congestion window by the reserve.**
        // Stopping bulk a reserve short of the minimum is not enough, because the
        // minimum can be *zero* while a flight is already wider than the window:
        // measured on the run's first two minutes, the ceiling at 2 MiB with the
        // bulk flight at 3.85 MB left `window = cwnd - flight = 0`, and 18 492
        // openings were refused with nothing wrong with either bound they could
        // see — credit 11.7 MB, buffer 28 MB. The reserve has to be a *lane*
        // budget an opening can spend, not a share of whatever is left, or it
        // reserves nothing exactly when it is needed.
        let room = if opening(state) {
            credit.min(window.saturating_add(OPEN_RESERVE)).min(held)
        } else {
            room.saturating_sub(OPEN_RESERVE)
        };
        // Was this connection idle before this call? A write that queues bytes on
        // an idle connection is the moment an opening can begin waiting for the
        // pump, and nothing else in the tree can see that wait.
        let was_idle = self.unsent_at.is_none() && self.idle_queued.is_none();
        let take = usize::try_from(room).map_or(0, |room| room.min(bytes.len()));
        if take > 0 && was_idle {
            self.idle_queued = Some((Instant::now(), stream));
        }
        if take < bytes.len() {
            // The window took less than the caller offered, so from this instant
            // there are bytes nobody has put on the wire: that is the age
            // [`Conn::waiting`] reports. One clock read per short write, never
            // per packet — and a write the window accepted whole clocks nothing.
            self.unsent_at.get_or_insert_with(Instant::now);
            state.unsent_at.get_or_insert_with(Instant::now);
            // Which bound refused, recorded because the three wait for different
            // things: the congestion window waits for an acknowledgement, the
            // peer's credit waits for its *reader*, and the buffer ceiling waits for
            // the peer's frontier. A stream that stalls with its credit refusing is
            // waiting on the far end's application, and nothing this connection does
            // will move it. The age gate has already said why, if it is why.
            //
            // Three minima, and the smallest is the one that refused: `room` is
            // `credit.min(window).min(held)`, so comparing only two of them — as
            // this did — reports every buffer refusal as a window refusal. The
            // order of the comparisons is the tie-break, and `credit` still wins a
            // three-way tie, so the long-standing counter keeps its old meaning for
            // the two cases it could already tell apart.
            let refusal = if refused_by_age {
                Refusal::Aged
            } else if credit <= window && credit <= held {
                Refusal::Credit
            } else if window <= held {
                Refusal::Window
            } else {
                Refusal::Buffer
            };
            self.last_refusal = Some(refusal);
            // Per lane, beside the process-wide counter: four lanes summed into one
            // line cannot say whether *this* writer was parked on the window or on
            // the buffer (`HANDOFF.md` §4.179).
            match refusal {
                Refusal::Aged => {
                    self.ledger.refused_aged = self.ledger.refused_aged.saturating_add(1);
                }
                Refusal::Buffer => {
                    self.ledger.refused_buffer = self.ledger.refused_buffer.saturating_add(1);
                }
                Refusal::Credit => {
                    self.ledger.refused_credit = self.ledger.refused_credit.saturating_add(1);
                }
                Refusal::Window => {
                    self.ledger.refused_window = self.ledger.refused_window.saturating_add(1);
                }
            }
            // By bound **and** by class: an opening stream that is refused is the
            // arrival path paying for the bulk spine, and the reserve exists to make
            // that impossible. If this counter never moves, the reserve works and the
            // first stage's cost is somewhere else entirely.
            let opened = still_opening;
            crate::transport::stats::note_refused(match (refusal, opened) {
                (Refusal::Aged, _) => "aged",
                (Refusal::Buffer, _) => "buffer",
                (Refusal::Credit, true) => "credit_opening",
                (Refusal::Credit, false) => "credit",
                (Refusal::Window, _) => "window",
            });
            // What the recovery table is holding when the age gate refuses. A
            // record that is neither acknowledged nor declared lost never leaves
            // the table, so its timestamp is the gate's input for ever: the lane
            // then refuses every write with no backlog and no credit block, which
            // is a stage that carries nothing and blames nothing.
            if log_table {
                let (held, oldest) = self.recovery.table_stats();
                tracing::debug!(
                    stream,
                    flight_packets = held,
                    oldest_data_ms =
                        oldest.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    "mhd write refused: what the recovery table holds"
                );
            }
        }
        if take == 0 {
            // An opening that cannot write a byte is the arrival path's whole
            // latency, so it says which bound held it and for how long the lane
            // has been waiting. The three are one line because they are read
            // together and a reader that has to correlate them will not.
            if crate::transport::stats::enabled() && opening(state) {
                tracing::debug!(
                    stream,
                    credit,
                    window,
                    held,
                    flight = self.recovery.flight_bytes(),
                    cwnd = self.cwnd,
                    offered = bytes.len(),
                    "mhd opening refused: which bound had no room"
                );
            }
            return 0;
        }
        state.send.write(&bytes[..take]);
        state.queued_at.get_or_insert_with(Instant::now);
        self.written += take as u64;
        self.buffered += take as u64;
        take
    }

    /// Finish a stream: no byte past what is queued will be written.
    pub(crate) fn finish(&mut self, stream: u32) {
        if let Some(state) = self.streams.get_mut(&stream) {
            state.send.finish();
            state.local_fin = true;
        }
        self.settle(stream);
    }

    /// Let one stream go: the caller dropped the last handle to it.
    ///
    /// Whatever was written still goes out. The stream is retired only after the
    /// peer has acknowledged all of it and the finish has been said, so dropping
    /// a channel at the end of a request cannot truncate the response. The reset
    /// is queued at that point — a peer that never sees one keeps a dead stream
    /// for the connection's life.
    pub(crate) fn close_stream(&mut self, stream: u32) {
        let blocks = &mut self.blocks;
        let Some(state) = self.streams.get_mut(&stream) else {
            return;
        };
        state.abandoned = true;
        // The bounded wait starts here: nothing will read this stream again, so
        // an acknowledgement it never gets must cost it [`ABANDON_HOLD`] and not
        // the connection's whole buffer budget (see `reap_abandoned`).
        state.abandoned_at.get_or_insert_with(Instant::now);
        // Nothing will read this half again: release what arrived, and with it
        // the blocks a later stream on this connection can use.
        state.recv.discard(blocks);
        state.send.finish();
        state.local_fin = true;
        self.settle(stream);
    }

    /// Move a finishing stream along: say the finish, retire it when it is done.
    fn settle(&mut self, stream: u32) {
        let Some(state) = self.streams.get_mut(&stream) else {
            self.finishing.remove(&stream);
            return;
        };
        if state.local_fin && !state.fin_sent && state.send.unacked() == 0 {
            state.fin_sent = true;
            // No byte carries the fin now (everything queued is already
            // acknowledged), so it needs a packet of its own — and one that is
            // retransmitted, because a peer that never sees its end waits for it
            // for ever.
            let at = state.send.finished_at().unwrap_or(0);
            self.pending_fins.push_back((stream, at));
        }
        let done = state.abandoned && state.fin_sent && state.send.unacked() == 0;
        if state.local_fin && !state.fin_sent {
            self.finishing.insert(stream);
        } else {
            self.finishing.remove(&stream);
        }
        if done {
            // The stream leaves with whatever its reader never took. Those
            // bytes were spent from the peer's window when they were sent, so
            // they have to be given back here: the window follows the reader,
            // and nobody will ever read a stream that no longer exists.
            self.release_unread(stream);
            self.stream_opened.remove(&stream);
            if let Some(state) = self.streams.remove(&stream) {
                // **A close must not overtake queued data.** The bytes this end
                // accepted but never put on the wire are dropped with the stream, and
                // nothing else counts them: no gap, no rejection, no retransmission.
                // A stream that leaves with `written > next` is that defect, named on
                // the stream that did it.
                let written = state.send.written();
                let next = state.send.next_offset();
                // `base` is the frontier the peer has **acknowledged**. A stream
                // reaches this line only when `unacked` is zero, so `base == written`
                // is not a coincidence of this log: the peer said it had every byte.
                // If the peer's own reader saw less, the acknowledgement is what
                // lied — and this number is where that becomes visible.
                let acked = state.send.base();
                tracing::debug!(
                    stream,
                    written,
                    next,
                    acked,
                    unsent = written.saturating_sub(next),
                    reset = ?state.reset,
                    peer_fin = state.peer_fin,
                    local_fin = state.local_fin,
                    "mhd stream closed"
                );
                // Its unacknowledged tail is no longer buffered by this end.
                self.buffered = self
                    .buffered
                    .saturating_sub(u64::try_from(state.send.held()).unwrap_or(u64::MAX));
            }
        }
    }

    /// Give back the window a stream will never consume.
    ///
    /// A stream can end holding bytes nobody read — its owner dropped the
    /// handle, or it was retired while the peer still had frames on the wire.
    /// Counting them as consumed is the only honest answer: the alternative is
    /// that the peer's window never reopens, a little at a time, for every
    /// short-lived stream a long-lived lane carries. A lane would then narrow
    /// until it carried nothing, and the only symptom would be
    /// `credit_blocked` on a connection whose readers are keeping up.
    fn release_unread(&mut self, stream: u32) {
        let blocks = &mut self.blocks;
        let held = self
            .streams
            .get_mut(&stream)
            .map_or(0, |state| state.recv.discard(blocks));
        self.advance_read(held);
    }

    /// Move this end's consumed position, and the window that follows it.
    fn advance_read(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.read = self.read.saturating_add(bytes);
        // The window follows the reader: credit is what has been consumed plus
        // one window, so a peer that fills it meets a reader that drains it.
        self.recv_credit = self.read.saturating_add(WINDOW);
        // **And the move has to be speakable.** `credit_due` wakes the pump, but
        // a pump with nothing to send emits no packet, and a packet that is not
        // an acknowledgement carries no credit. So a reader that drains a window
        // on a quiet lane — a one-way stream, which is every response flowing
        // back to a visitor — used to wait for the keepalive's two seconds, or,
        // with the keepalive also quiet, for ever: the sender sat on fifteen
        // megabytes it could not send while this end reported an empty buffer.
        // Arming the acknowledgement here is what makes the wake mean something.
        if self.credit_due() {
            self.ack_pending = true;
        }
    }

    /// Settle every finishing stream, after an acknowledgement moved a base.
    fn settle_finished(&mut self) {
        // Taken before the loop: `settle` edits the set, and the ids it removes
        // are exactly the ones that no longer need settling.
        let waiting: Vec<u32> = self.finishing.iter().copied().collect();
        for id in waiting {
            self.settle(id);
        }
    }

    /// Whether that stream is still in the table.
    pub(crate) fn stream_open(&self, stream: u32) -> bool {
        self.streams.contains_key(&stream)
    }

    /// Whether this end may still write to the stream.
    ///
    /// A peer's finish closes its read direction, not ours; only an abort ends
    /// both. Writing into a finished stream is how a half-closed connection
    /// still answers.
    pub(crate) fn write_open(&self, stream: u32) -> bool {
        self.streams
            .get(&stream)
            .is_some_and(|state| state.reset.is_none())
    }

    /// Whether anyone could still be waiting on this connection.
    ///
    /// The pump asks this to decide whether it may sleep for the keepalive or
    /// must look at its socket more often: a lane with no live stream has no
    /// visitor to be late for, while one that is carrying something has a reader
    /// whose latency is bounded by how quickly the pump notices a datagram
    /// (`pump::READ_POLL`, `HANDOFF.md` §4.101).
    ///
    /// A reset stream is not live: both directions are over and it stays in the
    /// table only until its finish is acknowledged. An *abandoned* one still
    /// counts — its handle is gone but its bytes are still owed to the peer, so
    /// the pump keeps looking promptly until it retires.
    pub(crate) fn has_live_streams(&self) -> bool {
        self.streams.values().any(|state| state.reset.is_none())
    }

    /// Take bytes a stream has received, answering how many were written.
    ///
    /// The caller's own buffer is filled directly. Staging the prefix in a
    /// buffer of the connection's own and handing it out a caller-sized slice at
    /// a time is a copy per byte that only exists to serve a shape no reader
    /// has.
    pub(crate) fn read(&mut self, stream: u32, out: &mut [u8]) -> usize {
        let blocks = &mut self.blocks;
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        let taken = state.recv.read_into(blocks, out);
        self.advance_read(taken as u64);
        taken
    }

    /// Whether the receive window has moved far enough to be worth telling the
    /// peer about.
    ///
    /// The reader is the only thing that moves it, and the reader is not the
    /// pump: without this predicate the reader cannot ask for a pass, and a
    /// sender blocked on the window waits for the pump's keepalive timer to
    /// hear that the window opened. That is a one-to-two second stall on every
    /// window, which is what a bulk transfer spends its time in.
    pub(crate) fn credit_due(&self) -> bool {
        self.recv_credit > self.advertised + WINDOW / 4
    }

    /// Whether the peer finished that stream, or the whole connection ended.
    pub(crate) fn peer_finished(&self, stream: u32) -> bool {
        if self.closed.is_some() {
            return true;
        }
        self.streams.get(&stream).is_some_and(|s| {
            // An abort ends the stream wherever it is: the peer gave up and
            // whatever is missing will never arrive. A clean finish ends it only
            // once every byte before the end has been read, or a finish that
            // overtook a hole would truncate the stream.
            s.reset.is_some() || (s.peer_fin && s.recv.is_complete())
        })
    }

    /// Why that stream was reset, if it was.
    pub(crate) fn reset_reason(&self, stream: u32) -> Option<ResetCode> {
        self.streams.get(&stream).and_then(|s| s.reset)
    }

    /// Whether the connection ended.
    pub(crate) fn closed(&self) -> Option<ResetCode> {
        if let Some(code) = self.closed {
            return Some(code);
        }
        // A graceful close is complete once the queue is empty: the pump drains
        // it, sees this, and marks the connection closed for good.
        self.close_after.filter(|_| !self.has_queued_transmit())
    }

    /// End the connection, but only after what is queued has been sent.
    pub(crate) fn close_gracefully(&mut self, code: ResetCode) {
        if self.closed.is_none() {
            self.close_after.get_or_insert(code);
        }
    }

    /// Whether anything is still waiting for the wire.
    pub(crate) fn has_queued_transmit(&self) -> bool {
        self.ack_pending
            || !self.resets.is_empty()
            || !self.pending_fins.is_empty()
            || self
                .streams
                .values()
                .any(|state| state.send.has_unsent() && state.reset.is_none())
    }

    /// End the connection.
    pub(crate) fn close(&mut self, code: ResetCode) {
        self.closed = Some(code);
    }

    /// When the caller should look again: the recovery timer, or the next
    /// keepalive, whichever comes first.
    ///
    /// An idle connection still has a deadline; without one a lane whose peer
    /// vanished while nothing was in flight would never learn it.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        if self.closed.is_some() {
            return None;
        }
        match self.recovery.poll_timeout() {
            Some(timer) => Some(timer.min(self.keepalive_at)),
            None => Some(self.keepalive_at),
        }
    }

    /// Append the next datagram to `out`, answering its length.
    ///
    /// A credit update the peer has not seen goes first, then a stream with
    /// bytes to send, then a reset, then a bare acknowledgement, and finally a
    /// keepalive: an ack that waits behind data is an ack that arrives after the
    /// sender has already re-sent.
    ///
    /// The credit comes first because it is the one frame whose delay stops the
    /// peer outright. A sender blocked on the window has nothing to send, so it
    /// cannot piggyback the update the way an acknowledgement rides a data
    /// packet; a receiver whose own outbound queue is always full would
    /// otherwise never say the window moved.
    ///
    /// `out` is the run the pump is staging, so a stream frame's payload is
    /// appended from the stream's own buffer and never staged twice.
    pub(crate) fn poll_transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<usize> {
        let before = self.built;
        // **One pass that found the idle write still queued.** Incremented before
        // the build, so a pass that carries it counts as one rather than zero.
        if self.idle_queued.is_some() {
            self.idle_passes = self.idle_passes.saturating_add(1);
        }
        let packet = self.build_packet(now, out);
        if self.built > before {
            let number = self.recovery.last_number();
            self.note_first_posted(number);
            if let Some((queued, stream)) = self.idle_queued.take() {
                let passes = self.idle_passes;
                self.idle_passes = 0;
                stats::note_idle_passes(u64::from(passes));
                let waited = now.saturating_duration_since(queued);
                if waited >= Duration::from_millis(5) && stats::enabled() {
                    tracing::info!(
                        lane = self.id,
                        stream,
                        waited_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                        "mhd idle write waited for the pump"
                    );
                }
            }
        }
        packet
    }

    /// Count datagrams this connection actually **posted**.
    ///
    /// Called by the pump after `send_run` returns, not by `poll_transmit`: a built
    /// packet is not a posted one, and counting builds here made a per-lane "loss"
    /// that was really "built but never sent" — the exact confusion the global
    /// counters were introduced to end.
    /// This end's own cumulative watermark: every packet number below it arrived.
    pub(crate) fn own_below(&self) -> u32 {
        self.acks.packet().map_or(0, |ack| ack.below)
    }

    /// The first number posted and the first seen (see [`Conn::first_seen`]).
    pub(crate) fn first_numbers(&self) -> (Option<u32>, Option<(u32, u32)>) {
        (self.first_posted, self.first_seen)
    }

    fn note_first_posted(&mut self, number: u32) {
        if self.first_posted.is_none() {
            self.first_posted = Some(number);
        }
    }

    fn note_first_seen(&mut self, number: u32) {
        if self.first_seen.is_some() {
            return;
        }
        self.first_seen = Some((number, self.own_below()));
    }

    /// This end's own numbering as it stands, for the one-line first-packet log.
    fn own_numbering(&self) -> (u32, u32) {
        (self.recovery.last_number(), self.own_below())
    }

    pub(crate) fn note_posted(&mut self, count: usize, bytes: usize) {
        self.posted_total = self
            .posted_total
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        self.posted = self
            .posted
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    /// What this lane handed to the socket since its last one-second report.
    ///
    /// **Per lane, and that is the point.** The window line used to print a
    /// process-wide counter here, which the *first* lane to report each second
    /// consumed and every other lane then read as zero — measured as
    /// `posted_datagrams_per_s` 728 624 on one lane against 1 937 on another in the
    /// same second. This is the number the next question needs (HANDOFF §4.57):
    /// bytes leaving *this* lane's socket, to compare with the stage's own
    /// throughput. The count includes each datagram's own header — 3 % of a full
    /// 1 400-byte datagram — which is inside the 15 % the pre-registration asks for
    /// and is stated rather than corrected away.
    pub(crate) fn posted_rate(&mut self, now: Instant) -> (u64, u64, u64) {
        let elapsed = now.saturating_duration_since(self.posted_at);
        let bytes = self.posted.saturating_sub(self.posted_at_report);
        let datagrams = self
            .posted_total
            .saturating_sub(self.posted_datagrams_at_report);
        self.posted_at = now;
        self.posted_at_report = self.posted;
        self.posted_datagrams_at_report = self.posted_total;
        (
            bytes,
            datagrams,
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        )
    }

    /// Queue one datagram for the wire, dropping the oldest if the queue is full.
    ///
    /// Answers whether it was taken; `false` means the queue was full and the *oldest*
    /// datagram went instead, which is the only policy a datagram service can accept:
    /// the newest is the one the application still cares about.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the producer is the UDP service layer, which lands with the server \
                      side of the switch; the receive door below is already used (HANDOFF §4.76)"
        )
    )]
    pub(crate) fn queue_datagram(&mut self, id: u32, data: Bytes) -> bool {
        let room = self.out_datagrams.len() < DATAGRAM_QUEUE;
        if !room {
            self.out_datagrams.pop_front();
        }
        self.out_datagrams.push_back((id, data));
        room
    }

    /// Datagrams sent and received, cumulative, for the window line.
    pub(crate) fn datagram_counts(&self) -> (u64, u64) {
        (self.datagrams_out, self.datagrams_in)
    }

    /// Account for one arriving datagram and queue it for the layer above.
    ///
    /// Its own function because `apply_datagram` is at the line budget, and because the
    /// two things it does are the two things a datagram service depends on: count what
    /// arrived, and drop the *oldest* queued one when the queue is full.
    /// Whether an id this end does not hold names a stream it has already retired.
    ///
    /// Two rules, and they answer different halves of the same question
    /// (`HANDOFF.md` §4.120). An id in *our* space that we do not hold is one of our
    /// own streams, retired. An id in the *peer's* space is normally a stream it is
    /// opening — unless it sits below one this end already accepted, in which case
    /// stream ids being monotonic means the peer cannot be opening it, so the packet
    /// is late. Without the second rule a late packet for an accepted-then-retired
    /// stream opened a **new** stream and a phantom accept.
    fn is_retired_stream(&self, id: u32, known: bool) -> bool {
        if known {
            return false;
        }
        if id % 2 == u32::from(self.dialer) {
            return true;
        }
        self.highest_inbound.is_some_and(|highest| id < highest)
    }

    fn accept_datagram(&mut self, id: u32, data: &[u8]) {
        self.datagrams_in = self.datagrams_in.saturating_add(1);
        if self.in_datagrams.len() >= DATAGRAM_QUEUE {
            self.in_datagrams.pop_front();
        }
        self.in_datagrams
            .push_back((id, Bytes::copy_from_slice(data)));
    }

    /// Take every datagram that has arrived since the last call.
    ///
    /// The layer above empties this queue; nothing else may, or a datagram would be
    /// counted as received and never delivered. Cheap when the carrier is not carrying
    /// datagrams: the queue is empty and this allocates nothing.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the consumer is the client's udp_read_loop, which needs a wake \
                      mechanism of its own and is the next increment (HANDOFF §4.76)"
        )
    )]
    pub(crate) fn take_datagrams(&mut self) -> VecDeque<(u32, Bytes)> {
        std::mem::take(&mut self.in_datagrams)
    }

    /// One pass of the scheduler, without the post accounting.
    fn build_packet(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<usize> {
        self.none_reason = None;
        if self.closed.is_some() {
            self.none_reason = Some("connection closed");
            self.snapshot_none("connection closed");
            return None;
        }
        let ack = self.acks.packet().map(|block| {
            self.acks_attached = self.acks_attached.saturating_add(1);
            wire::Ack {
                stream: self.arrived_stream,
                prefix: self.arrived_prefix,
                ..block
            }
        });
        if self.credit_due() {
            return self.transmit_credit(now, out, ack);
        }
        // **Before the streams.** A datagram is the latency-sensitive class: it is
        // already on the wire from the application's point of view, and a queue behind
        // a bulk stream is exactly the delay this frame exists to remove. One per
        // packet, so a burst cannot crowd out a stream for a whole run.
        if let Some(len) = self.transmit_datagram(out, ack) {
            return Some(len);
        }
        if let Some(len) = self.transmit_stream(now, out, ack) {
            return Some(len);
        }
        // Nothing went out for a stream, so either no stream has bytes or every
        // one that does is blocked. Only the first stops the clock: an age that
        // keeps growing means bytes nobody could put on the wire, which is what
        // [`Conn::waiting`] exists to name.
        if !self.streams.values().any(|state| state.send.has_unsent()) {
            self.unsent_at = None;
        }
        if let Some(len) = self.transmit_reset(now, out, ack) {
            return Some(len);
        }
        if let Some(len) = self.transmit_fin(now, out, ack) {
            return Some(len);
        }
        if self.ack_pending
            && let Some(packet_len) =
                wire::encode_control(out, self.id, self.recovery.last_number(), ack, Frame::None)
        {
            self.ack_pending = false;
            self.keepalive_at = now + KEEPALIVE;
            return Some(packet_len);
        }
        let packet = self.transmit_keepalive(now, out, ack);
        if packet.is_none() {
            let reason = self.none_reason.unwrap_or("nothing to say");
            self.snapshot_none(reason);
        }
        packet
    }

    /// A credit update: where this end's reader has got to.
    ///
    /// The credit comes first in a pass because it is the one frame whose delay
    /// stops the peer outright. A sender blocked on the window has nothing to send,
    /// so it cannot piggyback the update the way an acknowledgement rides a data
    /// packet; a receiver whose own outbound queue is always full would otherwise
    /// never say the window moved.
    fn transmit_credit(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        ack: Option<wire::Ack>,
    ) -> Option<usize> {
        // A control packet repeats the last data packet's number: it is not
        // retransmitted, so a number of its own would be a hole in the peer's
        // frontier that nothing could ever fill (see `wire::FLAG_UNNUMBERED`).
        let number = self.recovery.last_number();
        let packet_len = wire::encode_control(
            out,
            self.id,
            number,
            ack,
            Frame::Credit {
                connection: self.recv_credit,
            },
        )?;
        self.advertised = self.recv_credit;
        self.ack_pending = false;
        self.keepalive_at = now + KEEPALIVE;
        Some(packet_len)
    }

    /// The next stream's bytes, if any stream has unsent data.
    #[expect(
        clippy::too_many_lines,
        reason = "one packet's whole decision — which stream, how much, sealed or \
                  not, and the bookkeeping after — is one sequence; splitting it \
                  separates the length from the seal that must agree with it"
    )]
    fn transmit_stream(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        ack: Option<wire::Ack>,
    ) -> Option<usize> {
        // Sampled, because these run once per packet and a timestamp pair costs
        // more than the work it measures at this size (see `stats`).
        let sampled = stats::enabled() && stats::PHASES.timing();
        let pick = stats::Timing::start(sampled);
        let Some(Pick {
            id,
            offset,
            len: want,
            repair,
        }) = ({
            let picking = stats::enabled().then(std::time::Instant::now);
            let picked = self.next_stream_range();
            if let Some(at) = picking {
                self.pick_us = self
                    .pick_us
                    .saturating_add(u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX));
            }
            picked
        })
        else {
            self.none_reason = Some("no eligible stream");
            return None;
        };
        // Before the stream is borrowed: the instrument touches another field.
        self.note_first_byte_wait(id, offset, repair, now);
        // **Everything that can refuse happens before a packet number is taken**,
        // and before the dwell stamp is cleared. A number consumed by a packet
        // that is never built is a hole in the peer's cumulative *number*
        // frontier, and that hole can never be filled: a retransmission carries
        // a new number, so the frontier stays pinned for the connection's life.
        // Every later acknowledgement then re-declares everything above it lost
        // (three behind the largest, absent from the bitmap), so one refused
        // packet becomes a repair loop that spends the whole window — measured on
        // the sealed striped path as `retransmits=1532` on one stream with
        // nothing refused and no byte owed (`unsent=0`), starving the four
        // streams that were waiting for their first packet.
        let (sealed, len, fin) = {
            let Some(state) = self.streams.get(&id) else {
                self.none_reason = Some("stream retired mid-pick");
                return None;
            };
            let Some((want, sealed)) = Self::payload_plan(state, want) else {
                self.none_reason = Some("stream has no key");
                return None;
            };
            // How much the packet carries, and whether it ends the stream: the
            // buffer answers both without being walked, because the payload is
            // appended straight into the run afterwards. A repair already came
            // back clamped to what is still owed, so only new data is bounded
            // here.
            let len = if repair {
                want.min(state.send.available(offset, want))
            } else {
                state.send.available(offset, want)
            };
            if len == 0 {
                self.none_reason = Some("range is empty");
                return None;
            }
            (sealed, len, state.send.ends_at(offset, len))
        };
        let number = self.recovery.next_number();
        self.issued = self.issued.saturating_add(1);
        // The dwell: from the write that made this stream non-empty to the packet
        // that carries its oldest owed byte. Stamped out here rather than above,
        // because a refusal does not carry the bytes and must not clear the
        // measurement of how long they have waited.
        if let Some(at) = self
            .streams
            .get_mut(&id)
            .and_then(|state| state.queued_at.take())
        {
            let waited = now.saturating_duration_since(at);
            // Every wait, not only the long ones (see `stats::note_dwell`).
            stats::note_dwell(waited);
            if waited >= Duration::from_millis(20) {
                tracing::debug!(
                    stream = id,
                    dwell_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                    "mhd stream byte waited in the send buffer"
                );
            }
        }
        // Re-borrowed for the payload and the accounting below: the refusal
        // checks above deliberately ended their borrow so that no packet number
        // could be taken for a packet that is never built.
        let Some(state) = self.streams.get(&id) else {
            self.none_reason = Some("stream retired mid-pick");
            return None;
        };
        pick.stop(&stats::PHASES.tx_pick);
        let encode = stats::Timing::start(sampled);
        // Where this packet starts inside the run buffer. **Not** the buffer's
        // start: a run holds up to `SEND_RUN` packets, and taking the header as
        // `out[..payload_at]` makes every packet after the first sign a header
        // that contains its predecessors — which no receiver can reproduce, so
        // exactly one packet per run opens and the rest fail their tag.
        // The length this packet can actually carry: the header, the seal and the
        // send point all take this number, or a packet claims bytes it does not
        // hold and they are skipped for ever.
        let len = state.send.present(offset, len);
        if len == 0 {
            self.none_reason = Some("nothing to copy at the requested offset");
            return None;
        }
        let packet_start = out.len();
        let Some(packet_len) = wire::encode_stream(
            out,
            self.id,
            number,
            ack,
            wire::StreamRange {
                id,
                offset,
                fin,
                len,
            },
            sealed,
        ) else {
            self.none_reason = Some("frame does not fit");
            return None;
        };
        // The payload follows its header, drawn chunk by chunk: a packet is no
        // longer capped by the chunk boundary the send point happens to sit in.
        self.built = self.built.saturating_add(1);
        let payload_at = out.len();
        // **The copy, on its own clock.** One pair of reads per packet is the whole
        // perturbation this instrument asks for, and it is what separates the payload
        // copy from everything else in a packet's microsecond — the split the next
        // change needs (HANDOFF §4.63). Off unless the stats switch is on.
        let copying = stats::enabled().then(std::time::Instant::now);
        let copied = state.send.append_to(offset, len, out);
        if let Some(at) = copying {
            self.copy_us = self
                .copy_us
                .saturating_add(u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX));
            self.copies = self.copies.saturating_add(1);
            self.copy_bytes = self
                .copy_bytes
                .saturating_add(u64::try_from(len).unwrap_or(0));
        }
        debug_assert_eq!(copied, len, "the framing length matches what is copied");
        // A seal that cannot be produced must not become a plaintext packet, and
        // the packet number it already took must not become a **hole** either: a
        // number that never reaches the wire pins the peer's cumulative frontier
        // for the connection's life, and every later acknowledgement re-declares
        // everything above it lost. That is the same defect the refusal path had,
        // one level further in, and it exists only on the sealed path — which is
        // why sealing alone deadlocks stripes. So a lane that cannot seal is
        // closed here, where the pool can rebuild it, instead of continuing with
        // a hole it can never fill.
        if !seal_stream_payload(
            state.send_cipher.as_deref(),
            sealed,
            out,
            packet_start,
            payload_at,
            number,
        ) {
            stats::note_no_key();
            self.closed = Some(ResetCode::Aborted);
            self.none_reason = Some("a packet could not be sealed");
            return None;
        }
        encode.stop(&stats::PHASES.tx_encode);
        let book = stats::Timing::start(sampled);
        let recording = stats::enabled().then(std::time::Instant::now);
        self.recovery.on_sent(
            now,
            number,
            Some(SentRange {
                stream: id,
                offset,
                len: u16::try_from(len).unwrap_or(u16::MAX),
                fin,
            }),
            // Every packet of a stream before its first acknowledgement is the
            // opening flight, not only the one at offset zero: the handshake is
            // several datagrams (a selector byte, then the message), and losing
            // any of them costs the arrival its latency.
            !state.acknowledged,
            // Which of the two this packet is, for the repair budget: the scheduler
            // admits a repair only while the re-sent bytes in flight are under the
            // window (`HANDOFF.md` §4.187).
            repair,
        );
        // What this packet cost the wire, and whether the range was new: the pair
        // is the whole of the "is the lane re-sending itself" question, and it is
        // recorded here because this is the one place a data packet leaves.
        self.ledger.note_sent(len, repair);
        if let Some(state) = self.streams.get_mut(&id) {
            if repair {
                // The range is on the wire again; what is still owed stays
                // queued. The send point does not move: fresh data must not be
                // forced to wait behind a repair, and a repair must not be
                // forgotten because the send point moved past it.
                state.send.repair_sent(offset, len);
                state.repairs_sent = state.repairs_sent.saturating_add(1);
            } else {
                state.send.mark_sent(len);
                if !state.send.has_unsent() {
                    state.unsent_at = None;
                }
            }
        }
        if let Some(at) = recording {
            self.record_us = self
                .record_us
                .saturating_add(u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
        // **The second cursor walk per packet**, and the one thing left untimed in
        // `build_packet`: `next_stream_range` walks the table forwards to choose, and
        // this walks it again from `cursor` to remember where to start next time. Its
        // sibling measured a nanosecond, which is exactly why this one is measured
        // rather than assumed (HANDOFF §4.65).
        let advancing = stats::enabled().then(std::time::Instant::now);
        self.advance_cursor();
        if let Some(at) = advancing {
            self.advance_us = self
                .advance_us
                .saturating_add(u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
        book.stop(&stats::PHASES.tx_book);
        self.ack_pending = false;
        self.keepalive_at = now + KEEPALIVE;
        Some(packet_len)
    }

    /// The plaintext room one packet has, and whether it must be sealed.
    ///
    /// `None` is the one outcome that is not allowed to reach the wire: the lane
    /// has a root, so the peer expects ciphertext, but this stream lost its key —
    /// sending the bytes in the clear would be worse than sending nothing.
    fn payload_plan(state: &StreamState, want: usize) -> Option<(usize, bool)> {
        let sealed = state.sealed;
        if sealed && state.send_cipher.is_none() {
            // A stream created with a root but no key is the one state that must
            // never reach the wire in the clear.
            stats::note_no_key();
            return None;
        }
        // A sealed payload holds its tag inside the same datagram, so the room for
        // plaintext is the frame's payload minus the tag.
        Some((
            if sealed {
                want.min(wire::MAX_SEALED_PAYLOAD)
            } else {
                want
            },
            sealed,
        ))
    }

    /// A stream's very first byte, handed to the socket: its age is the part of
    /// a slow open the carrier owned (see `stream_opened`).
    fn note_first_byte_wait(&mut self, id: u32, offset: u64, repair: bool, now: Instant) {
        if offset != 0 || repair || !stats::enabled() {
            return;
        }
        let Some(at) = self.stream_opened.get(&id) else {
            return;
        };
        let waited = now.saturating_duration_since(*at);
        if waited < Duration::from_millis(50) {
            return;
        }
        tracing::debug!(
            stream = id,
            waited_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
            "mhd first stream byte waited for the socket"
        );
        self.stream_opened.remove(&id);
    }

    /// Open a sealed payload, or answer it unchanged when it is not sealed.
    ///
    /// Borrowed when there is nothing to undo (the common case on a plain lane)
    /// and owned when the bytes had to be unsealed in place, so the caller pays a
    /// copy only for a stream that is actually sealed.
    /// Open a sealed payload, or answer it unchanged when it is not sealed.
    ///
    /// It deliberately does **not** create the stream: announcing a new stream to
    /// the accept loop is the caller's job (`inbound.push_back`), and a version
    /// that inserted the state here made `known` true one line later, so the
    /// stream existed but nothing was ever told to accept it — a silent stall
    /// that looked exactly like a peer which never sent anything.
    fn open_stream_payload<'a>(
        &mut self,
        packet: &wire::Packet<'_>,
        id: u32,
        data: &'a [u8],
    ) -> Result<Opened<'a>, Rejected> {
        if !packet.sealed {
            return Ok((std::borrow::Cow::Borrowed(data), None));
        }
        let Some(tag) = packet.tag else {
            stats::note_rejected("unsealed_no_tag");
            return Err(Rejected::Unsealed);
        };
        // A stream this end has never seen is opened by this packet, and its key
        // comes from the lane's root on both ends.
        let fresh = if self.streams.contains_key(&id) {
            None
        } else {
            let (send_cipher, recv_cipher) = self.stream_ciphers(id);
            if recv_cipher.is_none() {
                // **The one refusal that must be said.** A sealed packet on a lane
                // whose root this end does not hold is dropped, and the stream
                // that packet opens then never receives its first byte: the peer
                // times out half a minute later with nothing in the log to explain
                // it. `unsealed_tags` counts *tag failures*, so it read zero
                // through exactly this, which is why it is counted apart.
                if stats::enabled() {
                    tracing::debug!(
                        stream = id,
                        packet = packet.number,
                        has_root = self.has_lane_root(),
                        "mhd sealed packet refused: this end holds no root"
                    );
                }
                stats::note_rejected("unsealed_no_root");
                return Err(Rejected::Unsealed);
            }
            Some((send_cipher, recv_cipher))
        };
        let cipher = match fresh.as_ref() {
            Some((_, recv)) => recv.as_ref(),
            None => self
                .streams
                .get(&id)
                .and_then(|state| state.recv_cipher.as_ref()),
        };
        let Some(cipher) = cipher else {
            stats::note_rejected("unsealed_no_key");
            return Err(Rejected::Unsealed);
        };
        let mut opened = data.to_vec();
        if !cipher.open(u64::from(packet.number), packet.ad, &mut opened, tag) {
            stats::note_unsealed();
            return Err(Rejected::Unsealed);
        }
        Ok((std::borrow::Cow::Owned(opened), fresh))
    }

    /// One stream's abort, if one is queued.
    /// Put one queued datagram on the wire, or answer nothing.
    ///
    /// No packet number, no recovery record, no credit: the frame's own flags say it is
    /// unnumbered, and nothing here touches the flight. What it *does* count is the
    /// datagram, so the next increment can watch `datagrams_in` grow while `flight` stays
    /// where it was (HANDOFF §4.71).
    fn transmit_datagram(&mut self, out: &mut Vec<u8>, ack: Option<wire::Ack>) -> Option<usize> {
        let (id, data) = self.out_datagrams.front()?;
        // `encode_control` appends into the run the pump is staging, which is the same
        // shape every unnumbered frame uses; a bare `encode` would need a caller-sized
        // slice, and the pump's buffer starts empty.
        let encoded = wire::encode_control(
            out,
            self.id,
            self.recovery.last_number(),
            ack,
            wire::Frame::Datagram { id: *id, data },
        );
        if encoded.is_some() {
            // Only dequeued once it is on the wire: a datagram that did not fit stays
            // queued and is tried next pass rather than being silently lost.
            self.out_datagrams.pop_front();
            self.datagrams_out = self.datagrams_out.saturating_add(1);
        }
        encoded
    }

    fn transmit_reset(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        ack: Option<wire::Ack>,
    ) -> Option<usize> {
        let (id, code) = self.resets.pop_front()?;
        let packet_len = wire::encode_control(
            out,
            self.id,
            self.recovery.last_number(),
            ack,
            Frame::Reset { id, code },
        )?;
        self.ack_pending = false;
        self.keepalive_at = now + KEEPALIVE;
        Some(packet_len)
    }

    /// A finish with nothing left to ride: a tracked, empty stream frame at the
    /// stream's end.
    ///
    /// It is retransmitted like data, because a peer whose reader never learns the
    /// stream ended would hold the stream for the connection's life.
    fn transmit_fin(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        ack: Option<wire::Ack>,
    ) -> Option<usize> {
        let (id, at) = self.pending_fins.pop_front()?;
        let number = self.recovery.next_number();
        let packet_len = wire::encode_stream(
            out,
            self.id,
            number,
            ack,
            wire::StreamRange {
                id,
                offset: at,
                fin: true,
                len: 0,
            },
            // A finish is metadata, not payload: it carries no bytes to seal, and
            // the flag it sets is already in the clear. Sealing a zero-length
            // frame would only add a tag the peer's decoder would have to carve
            // out of a body that does not exist.
            false,
        )?;
        self.recovery.on_sent(
            now,
            number,
            Some(SentRange {
                stream: id,
                offset: at,
                len: 0,
                fin: true,
            }),
            // A finish-only packet carries no bytes of the opening, and it is not a
            // repair: it has no range for the budget to count.
            false,
            false,
        );
        self.ack_pending = false;
        self.keepalive_at = now + KEEPALIVE;
        Some(packet_len)
    }

    /// The idle probe, when its interval is due.
    fn transmit_keepalive(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        ack: Option<wire::Ack>,
    ) -> Option<usize> {
        if now < self.keepalive_at {
            return None;
        }
        // One probe per due interval, however long the process was asleep: a
        // resumed laptop must not fire the whole missed schedule at once.
        self.keepalive_at = now + KEEPALIVE;
        // A connection with nothing on loan keeps no receive blocks: the pool is
        // what the streams took, and giving it back inside a keepalive interval
        // is what makes a lane that carried a window's worth of data small again.
        self.blocks.shrink();
        if now.saturating_duration_since(self.last_recv) > DEAD_AFTER {
            // One line, at the moment a lane decides its peer is gone: the age
            // is the evidence, and a lane that dies having heard nothing since it
            // was created is a different defect from one that went quiet under
            // load.
            tracing::debug!(
                id = self.id,
                silent_ms =
                    u64::try_from(now.saturating_duration_since(self.last_recv).as_millis())
                        .unwrap_or(u64::MAX),
                "mhd lane declared dead: the peer has been silent"
            );
            self.closed = Some(ResetCode::Aborted);
            return None;
        }
        wire::encode_control(out, self.id, self.recovery.last_number(), ack, Frame::None)
    }

    /// The next stream with bytes to send, walking from the cursor.
    ///
    /// The walk is two bounded ranges over the table rather than a collected
    /// vector of ids: a packet is built for every pass, and allocating a list of
    /// every live stream per packet is a cost that grows with the connection's
    /// width.
    fn next_stream_range(&mut self) -> Option<Pick> {
        if self.streams.is_empty() {
            self.unsent_at = None;
            return None;
        }
        let cursor = self.cursor;
        // **A repair may only join a flight that still has window left for it.** The
        // writer bounds new data against `cwnd − flight`, but a repair is not written
        // at all — it is scheduled from the recovery table — so until this rule existed
        // a lane could put an unbounded amount of re-sent bytes in flight, which is the
        // amplifier behind the storm (`HANDOFF.md` §4.182, §4.187). Measured before it:
        // one lane posted 1 909 MB and delivered 226 MB.
        let repairs_allowed = self.recovery.repair_flight() < self.cwnd;
        let mut gated = 0u64;
        let mut eligible = |(id, state): (&u32, &StreamState)| {
            // `Finished` ends the peer's *write* direction, not the stream: the
            // other direction is still ours to use (that is what half-close
            // means). Only an abort or a refusal stops our writes too.
            let open = state.reset.is_none_or(|code| code == ResetCode::Finished);
            if !open {
                return None;
            }
            // A declared loss first: it is the range that unblocks the peer's
            // reader, and it is one range — not a rewind of everything after it.
            if let Some((offset, len)) = state.send.next_repair(MAX_STREAM_PAYLOAD) {
                if !repairs_allowed {
                    // Counted, not dropped silently: a lane that stops repairing
                    // because its budget is spent is a different finding from one
                    // with nothing to repair, and only this counter tells them apart.
                    gated = gated.saturating_add(1);
                    return None;
                }
                return Some(Pick {
                    id: *id,
                    offset,
                    len,
                    repair: true,
                });
            }
            state.send.has_unsent().then(|| {
                let offset = state.send.next_offset();
                let unsent = state.send.unacked();
                let want = usize::try_from(unsent).unwrap_or(0).min(MAX_STREAM_PAYLOAD);
                Pick {
                    id: *id,
                    offset,
                    len: want,
                    repair: false,
                }
            })
        };
        let picked = self
            .streams
            .range(cursor..)
            .find_map(&mut eligible)
            .or_else(|| self.streams.range(..cursor).find_map(&mut eligible));
        self.ledger.repairs_gated = self.ledger.repairs_gated.saturating_add(gated);
        picked
    }

    /// Capture why this pass built nothing, while the lock is still held.
    fn snapshot_none(&mut self, reason: &'static str) {
        // Counted here, once per pass that built nothing: the distribution of *why*
        // the pump stops is the number that says whether a budget or the scheduler's
        // walk is what keeps `flight` below the window (HANDOFF §4.58).
        //
        // **Per lane as well as process-wide.** The process-wide table is what mixed
        // four lanes into one line and made "no eligible stream" look like a window
        // problem; a lane that stops because it has nothing queued and a lane that
        // stops because its window is shut are opposite findings, and only the
        // per-lane copy can tell them apart (`HANDOFF.md` §4.179).
        match reason {
            "no eligible stream" => {
                self.ledger.stop_no_stream = self.ledger.stop_no_stream.saturating_add(1);
            }
            "nothing to say" => {
                self.ledger.stop_nothing_to_say = self.ledger.stop_nothing_to_say.saturating_add(1);
            }
            "connection closed" => {
                self.ledger.stop_closed = self.ledger.stop_closed.saturating_add(1);
            }
            _ => {
                self.ledger.stop_other = self.ledger.stop_other.saturating_add(1);
            }
        }
        crate::transport::stats::note_none_reason(reason);
        self.none_snapshot = Some(NoneSnapshot {
            reason,
            visited: self.streams.len(),
            open: self
                .streams
                .values()
                .filter(|state| state.reset.is_none_or(|code| code == ResetCode::Finished))
                .count(),
            // The **same** test the scheduler applies, or the count contradicts
            // the walk: a stream the peer reset keeps its queued bytes and is
            // never served, so counting it produced lines that read
            // `queued=1 … why="no eligible stream"` and sent three rounds of
            // investigation after an artifact of this counter.
            queued: self
                .streams
                .values()
                .filter(|state| {
                    state.reset.is_none_or(|code| code == ResetCode::Finished)
                        && state.send.has_unsent()
                })
                .count(),
            fins: self.pending_fins.len(),
            resets: self.resets.len(),
            ack: self.ack_pending,
            credit: self.send_credit.saturating_sub(self.written),
            window: self.cwnd.saturating_sub(self.recovery.flight_bytes()),
            buffer: MAX_BUFFERED.saturating_sub(self.buffered),
            cursor: self.cursor,
            witness: self.streams.iter().next().map(|(&id, state)| {
                let open = state.reset.is_none_or(|code| code == ResetCode::Finished);
                (
                    id,
                    open,
                    state.send.has_unsent(),
                    state.send.next_offset(),
                    state.send.written(),
                    state.send.base(),
                )
            }),
        });
    }

    #[cfg(test)]
    /// Put this lane on a path with the given round trip (see the ceiling).
    pub(crate) fn seed_rtt(&mut self, rtt: std::time::Duration) {
        self.recovery.seed_rtt(rtt);
    }

    /// The window this lane may grow to, given the path it is on.
    ///
    /// The floor scaled by the measured round trip, never above [`MAX_CWND`]. A
    /// hundred-millisecond path's product really is megabytes, so a shaped lane
    /// keeps the window it always had; a loopback lane — whose estimate is the
    /// estimator's own one-millisecond granularity — keeps a quarter of a
    /// mebibyte.
    pub(crate) fn window_ceiling(&self) -> u64 {
        // The **minimum** round trip, not the smoothed one: a ceiling derived from a
        // queued estimate raises its own ceiling, and the lane then latches into a
        // regime where every small packet pays for the queue that set it.
        let Some(smoothed) = self
            .recovery
            .min_rtt()
            .or_else(|| self.recovery.smoothed_rtt())
        else {
            return WINDOW_FLOOR;
        };
        let hops = u64::try_from(smoothed.as_micros())
            .unwrap_or(u64::MAX)
            .checked_div(WINDOW_REFERENCE)
            .unwrap_or(1)
            .max(1);
        WINDOW_FLOOR.saturating_mul(hops).min(MAX_CWND)
    }

    /// Move the cursor past the stream that was just served, wrapping to the
    /// smallest id so the walk starts again where it began.
    fn advance_cursor(&mut self) {
        use std::collections::Bound;
        self.cursor = self
            .streams
            .range((Bound::Excluded(self.cursor), Bound::Unbounded))
            .next()
            .or_else(|| self.streams.iter().next())
            .map_or(0, |(id, _)| *id);
    }

    /// How long this connection's bytes have been waiting, in either place.
    ///
    /// Two queues can hold a byte and neither is visible from the outside: the
    /// send buffer, when the scheduler cannot hand it to the wire (window
    /// closed, or the pump starved), and the flight, when the wire took it and
    /// no acknowledgement came back. A stall that the drop counters cannot see
    /// is one of these two ages growing, so it is worth one line when it does.
    /// Whether a write is queued on an idle connection and still waiting for a
    /// pump pass — the condition the pump's wake kind is counted against.
    pub(crate) fn has_idle_queued(&self) -> bool {
        self.idle_queued.is_some()
    }

    pub(crate) fn waiting(&self, now: Instant) -> (Option<Duration>, Option<Duration>) {
        let unsent = self.unsent_at.map(|at| now.saturating_duration_since(at));
        (unsent, self.recovery.oldest_flight_age(now))
    }

    /// The ages worth one log line, at most once a second.
    ///
    /// A stall the drop counters cannot see is a byte waiting in one of the two
    /// queues [`Conn::waiting`] reports. Two hundred milliseconds is forty times
    /// the round trip a healthy path here measures, so anything above it is not a
    /// queue doing its job.
    ///
    /// The same line fires on a **receive backlog**, and that is the half this
    /// end's own ages cannot show: a stream whose reader stopped does not wait
    /// here at all — it stops the *peer*, through the credit — so a connection
    /// blocked on the far end's credit and a connection whose own reader owes
    /// the peer a megabyte were, until this, the same absence of log lines.
    /// The stream with the most bytes the wire has not taken, and its own facts.
    /// The peer's last cumulative prefix, for the window line.
    /// Bytes this end has inserted into every stream's receive buffer.
    ///
    /// Reported beside the send side because it is the other half of one
    /// question: whether the repairs a peer posts are arriving at all.
    /// The highest packet number this end has issued.
    ///
    /// Beside the peer's `largest` it answers one question exactly: whether the
    /// numbers this end posted are the numbers the peer has seen.
    pub(crate) fn last_number(&self) -> u32 {
        self.recovery.last_number()
    }

    /// Pieces every stream holds beyond its own contiguous frontier.
    pub(crate) fn beyond_frontier(&self) -> (u32, u64) {
        let mut pieces = 0u32;
        let mut bytes = 0u64;
        for state in self.streams.values() {
            let (p, b) = state.recv.beyond_frontier();
            pieces = pieces.saturating_add(p);
            bytes = bytes.saturating_add(b);
        }
        (pieces, bytes)
    }

    pub(crate) fn inserted_total(&self) -> u64 {
        self.streams.values().map(|state| state.inserted).sum()
    }

    /// How many streams have been waiting to put bytes on the wire for at least
    /// `after`, and how long the oldest has waited.
    pub(crate) fn parked_writers(&self, now: Instant, after: std::time::Duration) -> (u32, u64) {
        let mut count = 0;
        let mut oldest = 0;
        for state in self.streams.values() {
            let Some(at) = state.unsent_at else { continue };
            let waited = now.saturating_duration_since(at);
            if waited < after {
                continue;
            }
            count += 1;
            oldest = oldest.max(u64::try_from(waited.as_millis()).unwrap_or(u64::MAX));
        }
        (count, oldest)
    }

    /// Declared-loss ranges that covered a frontier, and those above it.
    /// Acknowledgements applied and attached since the last call.
    /// Packet numbers taken and packets built since the last call, plus the
    /// reason the last refusal carried.
    /// Datagrams this connection has posted and read, cumulative.
    /// Whether this end dialled the lane, for a log line that has to name a side.
    pub(crate) fn is_dialer(&self) -> bool {
        self.dialer
    }

    /// Numbers framed as this connection's own, and numbers acknowledged.
    pub(crate) fn number_counts(&self) -> (u64, u64) {
        (self.numbers_seen, self.numbers_acked)
    }

    pub(crate) fn datagram_totals(&self) -> (u64, u64) {
        (self.posted_total, self.received_total)
    }

    pub(crate) fn take_issued(&mut self) -> (u64, u64) {
        let issued = self.issued;
        let built = self.built;
        self.issued = 0;
        self.built = 0;
        (issued, built)
    }

    pub(crate) fn take_acks(&mut self) -> (u64, u64) {
        let applied = self.acks_applied;
        let attached = self.acks_attached;
        self.acks_applied = 0;
        self.acks_attached = 0;
        (applied, attached)
    }

    pub(crate) fn repair_sides(&self) -> (u64, u64) {
        (self.repairs_at_base, self.repairs_above)
    }

    pub(crate) fn last_ack_prefix(&self) -> u64 {
        self.last_ack_prefix
    }

    /// The peer's packet-number watermark and bitmap, for the window line.
    pub(crate) fn last_ack_marks(&self) -> (u32, u64, u32) {
        (
            self.last_ack_below,
            self.last_ack_bits,
            self.last_ack_largest,
        )
    }

    pub(crate) fn worst_stream(&self) -> Option<WorstStream> {
        self.streams
            .iter()
            // The stream with the most **unacknowledged** bytes, not the most
            // unsent ones: a repair loop is a stream whose `next` has caught up with
            // `written` and whose records still are not retired, and filtering on
            // unsent hid exactly that stream — the instrument reported "no worst
            // stream" at a stall that was one.
            .filter(|(_, state)| state.send.unacked() > 0)
            .max_by_key(|(_, state)| state.send.unacked())
            .map(|(&id, state)| WorstStream {
                id,
                unsent: state.send.unacked(),
                retransmits: state.retransmits,
                repairs_sent: state.repairs_sent,
                repair_at: state.send.repair_head().map_or(0, |(offset, _)| offset),
                repairs_pending: state.send.repairs_pending(),
                covering: self.recovery.covering(id, state.send.base()).0,
                tracked: self.recovery.covering(id, state.send.base()).1,
                covering_number: self.recovery.covering(id, state.send.base()).2,
                next: state.send.next_offset(),
                written: state.send.written(),
                base: state.send.base(),
                handed_over: state.send.handed_over(),
                // Whether an abort is what keeps this stream out of the
                // scheduler, and which repair it is owed. Those two are the only
                // ways a stream with unsent bytes is passed over without a packet
                // being built for it, and without them a stalled lane reads as
                // "bytes waiting" with no reason attached — which is exactly how
                // the sealed striped deadlock stayed unexplained.
                reset: state.reset.is_some(),
                repair: state
                    .send
                    .next_repair(super::wire::MAX_STREAM_PAYLOAD)
                    .map(|(offset, len)| (offset, len as u64)),
            })
    }

    pub(crate) fn stall_report(&mut self, now: Instant) -> Option<Stall> {
        const FLOOR: Duration = Duration::from_millis(200);
        /// A reader that is a second behind keeps kilobytes here, never this.
        const BACKLOG: u64 = 1024 * 1024;
        const EVERY: Duration = Duration::from_secs(1);
        let (unsent, unacked) = self.waiting(now);
        let backlog = self.backlog();
        let worst = unsent.unwrap_or_default().max(unacked.unwrap_or_default());
        if worst < FLOOR && backlog.total < BACKLOG {
            self.stall_logged_at = None;
            return None;
        }
        if self
            .stall_logged_at
            .is_some_and(|at| now.saturating_duration_since(at) < EVERY)
        {
            return None;
        }
        self.stall_logged_at = Some(now);
        Some(Stall {
            unsent_ms: unsent.map_or(0, |d| d.as_millis()),
            unacked_ms: unacked.map_or(0, |d| d.as_millis()),
            backlog,
            worst: self.worst_stream(),
            recv_below: self.acks.below(),
            recv_largest: self.acks.largest().unwrap_or(0),
            recv_prefix: self.arrived_prefix,
            recv_stream: self.arrived_stream,
            flight_numbers: self.recovery.number_range(),
        })
    }

    /// What this end's readers have left unread, per the whole connection.
    /// The smoothed round trip, for the window line.
    pub(crate) fn smoothed_rtt(&self) -> Option<std::time::Duration> {
        self.recovery.smoothed_rtt()
    }

    pub(crate) fn backlog(&self) -> Backlog {
        let mut out = Backlog::default();
        for (&id, state) in &self.streams {
            let held = state.recv.held();
            if held == 0 {
                continue;
            }
            out.streams += 1;
            out.total += held;
            if out.worst.is_none_or(|(_, most)| held > most) {
                out.worst = Some((id, held));
            }
        }
        out
    }

    /// Whether the last refusal was the peer's credit (see [`Conn::write`]).
    /// The streams a receive batch touched, and forget them.
    ///
    /// The pump wakes one reader per id instead of every reader on the lane; see
    /// [`Conn::woke`]. Answering an empty vector is the common case on a pass
    /// that only sent, and it costs nothing.
    pub(crate) fn take_woken(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.woke)
    }

    pub(crate) fn credit_blocked(&self) -> bool {
        matches!(self.last_refusal, Some(Refusal::Credit))
    }

    /// A coherent picture of the state *now*, when bytes are queued but a pass
    /// built nothing.
    ///
    /// Read under the same lock as the queue test: sampling the two separately
    /// let a writer's `Conn::write` land between them, and the resulting line
    /// described a connection with nothing queued — which is exactly the wrong
    /// conclusion to draw from it.
    pub(crate) fn refusal_now(&mut self) -> Option<NoneSnapshot> {
        // Only the snapshot taken **at the refusal**, inside the same critical
        // section as the walk that produced it. Re-sampling here was the bug that
        // made this counter lie for three rounds: the pump calls it after `flush`
        // has already returned, so a writer landing in between made the counts
        // describe a later instant than the reason — which is how a line came to
        // read `queued=1 … reason="no eligible stream"` while the walk had run on
        // an empty queue. A reason and its counts must come from one instant or
        // the alarm is worse than no alarm.
        self.none_snapshot
    }

    /// The connection id, so a diagnostic can be matched with the peer's line.
    pub(crate) fn connection_id(&self) -> u32 {
        self.id
    }

    /// The hole that stream's reader is stuck behind, if any.
    pub(crate) fn hole(&self, stream: u32) -> Option<(u64, u64, usize, Option<u64>)> {
        self.streams.get(&stream).map(|s| s.recv.prefix_state())
    }

    /// How long that stream has had bytes its reader has not taken.
    pub(crate) fn ready_for(&self, stream: u32, now: Instant) -> Option<Duration> {
        self.streams
            .get(&stream)
            .and_then(|s| s.recv.ready_for(now))
    }

    /// How many streams have had unread bytes for longer than `threshold`.
    ///
    /// The observation a reader cannot make about itself: a stream whose reader
    /// task was never polled never runs a line of code, so every counter inside
    /// `poll_read` reads zero exactly when the defect is worst. The pump can see
    /// it, because the buffer and the clock are both its business. A sender sees
    /// the same condition as a shut receive window for the whole stage.
    pub(crate) fn starved_streams(&self, now: Instant, threshold: std::time::Duration) -> usize {
        self.streams
            .keys()
            .filter(|id| {
                self.ready_for(**id, now)
                    .is_some_and(|ready| ready > threshold)
            })
            .count()
    }

    /// How many times that stream's ranges were declared lost.
    pub(crate) fn stream_retransmits(&self, stream: u32) -> u32 {
        self.streams
            .get(&stream)
            .map_or(0, |state| state.retransmits)
    }

    /// What the connection's three write budgets have left, for the
    /// open-latency line: `(credit, window, buffer, streams with bytes queued)`.
    ///
    /// A stream that opens slowly is slow for one of four reasons, and they look
    /// identical from outside: the peer's credit is spent, the congestion window
    /// is full, the send buffers are at their ceiling, or none of them is short
    /// and the open is waiting on the wire. Read only when an open is already
    /// late (see `MhdLane::open_stream`), so a healthy run pays nothing.
    /// When this end last reported its window, so the pump reports once a second
    /// rather than once a pass.
    pub(crate) fn window_reported_at(&self) -> Instant {
        self.window_reported_at
    }

    /// Record that the window has just been reported.
    pub(crate) fn note_window_reported(&mut self, at: Instant) {
        self.window_reported_at = at;
    }

    /// This connection's wire id, so a periodic line can be read as one series
    /// per lane instead of four interleaved ones.
    pub(crate) fn wire_id(&self) -> u32 {
        self.id
    }

    /// The congestion window's own state, for the periodic stats line.
    ///
    /// `(cwnd, flight, slow_start)` — the three numbers that say whether a lane
    /// that has fallen behind a clean path is growing its window or sitting on
    /// one. `slow_start` latches off at the first loss and never returns, so a
    /// lane measured late in a shaping schedule is in congestion avoidance by
    /// construction; this is what makes that visible instead of inferred.
    pub(crate) fn window_state(&self) -> (u64, u64, bool, u64) {
        (
            self.cwnd,
            self.recovery.flight_bytes(),
            self.slow_start,
            self.acked_since_growth,
        )
    }

    /// This lane's send/receive ledger since the last call, and clear it.
    ///
    /// Once a second, from the same place the window line is read, so the two lines
    /// describe one interval. Taking rather than reading is deliberate: the numbers
    /// are a *rate* question (`sent` against `acked` per second is the duplicate
    /// share of what the wire carried right now), and a cumulative total would let a
    /// burst at the start of a stage dominate every later second of the series.
    pub(crate) fn take_ledger(&mut self) -> SendLedger {
        std::mem::take(&mut self.ledger)
    }

    /// What the window is compared against, and what is really waiting.
    ///
    /// Three numbers that must agree and, measured on the clean cell, do not:
    ///
    /// - `flight` is [`Recovery::flight_bytes`], the bytes the recovery table
    ///   still counts as handed to packets. It is what `write` subtracts from
    ///   `cwnd`, so it is what *closes* the window.
    /// - `buffered` is what the streams actually hold and the peer has not
    ///   acknowledged. A frontier that moved by the byte prefix while a packet
    ///   number stayed unacknowledged leaves records counting bytes the peer
    ///   already has.
    /// - `packets` is the table's length.
    ///
    /// Measured: the staged cell logs `flight` at **3.4–12 MB** against a `cwnd`
    /// of 262 144, which cannot be a real queue — a lane that had that much on
    /// the wire with a window that small would carry 21 Mbit/s. Either the
    /// counter is inflated by re-sent ranges (each re-send adds to `flight` while
    /// the original record is held until its number is acknowledged or
    /// [`LOST_HOLD`](super::recovery) expires) or the flight is real and the
    /// window is not a bound. These three distinguish them without a rebuild.
    /// Record one pass of this lane's pump.
    ///
    /// Called once per flush and once per gap, so two adds per pass and nothing per
    /// packet. What it keeps is the cycle's whole shape: passes, packets, and
    /// microseconds of flush against microseconds of gap.
    pub(crate) fn note_pass(&mut self, built: usize, flush: std::time::Duration) {
        self.passes = self.passes.saturating_add(1);
        self.pass_packets = self
            .pass_packets
            .saturating_add(u64::try_from(built).unwrap_or(u64::MAX));
        self.flush_us = self
            .flush_us
            .saturating_add(u64::try_from(flush.as_micros()).unwrap_or(u64::MAX));
    }

    /// Record the post-to-next-pass gap of this lane, and its tail.
    pub(crate) fn note_gap(&mut self, gap: std::time::Duration) {
        let us = u64::try_from(gap.as_micros()).unwrap_or(u64::MAX);
        self.gap_us = self.gap_us.saturating_add(us);
        self.gap_max_us = self.gap_max_us.max(us);
        if us >= GAP_SLOW_US {
            self.gap_slow = self.gap_slow.saturating_add(1);
        }
    }

    /// Record time spent inside the run's `yield_now`.
    pub(crate) fn note_yield(&mut self, waited: std::time::Duration) {
        self.yield_us = self
            .yield_us
            .saturating_add(u64::try_from(waited.as_micros()).unwrap_or(u64::MAX));
    }

    /// This lane's pass counters since the last read, with the interval they cover.
    ///
    /// Reported in the same per-lane line as `cwnd` and `srtt`, so "how long a pass
    /// takes here" and "how much of the second that accounts for" are one subtraction
    /// apart (HANDOFF §4.61).
    pub(crate) fn take_passes(&mut self, now: Instant) -> [u64; 14] {
        let interval = u64::try_from(now.saturating_duration_since(self.pass_at).as_micros())
            .unwrap_or(u64::MAX);
        self.pass_at = now;
        let out = [
            self.passes,
            self.pass_packets,
            self.flush_us,
            self.gap_us,
            interval,
            self.gap_max_us,
            self.gap_slow,
            self.yield_us,
            self.copy_us,
            self.copies,
            self.copy_bytes,
            self.pick_us,
            self.record_us,
            self.advance_us,
        ];
        self.passes = 0;
        self.pass_packets = 0;
        self.flush_us = 0;
        self.gap_us = 0;
        self.gap_max_us = 0;
        self.gap_slow = 0;
        self.yield_us = 0;
        self.copy_us = 0;
        self.copies = 0;
        self.copy_bytes = 0;
        self.pick_us = 0;
        self.record_us = 0;
        self.advance_us = 0;
        out
    }

    pub(crate) fn flight_and_buffered(&self) -> (u64, u64, usize) {
        (
            self.recovery.flight_bytes(),
            self.buffered,
            self.recovery.table_stats().0,
        )
    }

    pub(crate) fn budget(&self) -> (u64, u64, u64, usize) {
        (
            self.send_credit.saturating_sub(self.written),
            self.cwnd.saturating_sub(self.recovery.flight_bytes()),
            MAX_BUFFERED.saturating_sub(self.buffered),
            self.streams
                .values()
                .filter(|state| state.send.has_unsent())
                .count(),
        )
    }

    /// Feed one datagram from the peer.
    ///
    /// The whole of it is timed as `rx_total` when the phase switch is on, and
    /// the three phases inside are timed from a single sampling decision so they
    /// describe the same datagram (see [`crate::transport::stats`]).
    pub(crate) fn on_datagram(&mut self, now: Instant, buf: &[u8]) -> Result<(), Rejected> {
        self.received_total = self.received_total.saturating_add(1);
        let sampled = stats::enabled() && stats::PHASES.timing();
        let total = stats::Timing::start(sampled);
        let outcome = self.apply_datagram(now, buf, sampled);
        total.stop(&stats::PHASES.rx_total);
        if let Err(refused) = outcome {
            stats::note_rejected(match refused {
                Rejected::Malformed => "malformed",
                Rejected::OverWindow => "over_window",
                Rejected::UnknownStream => "unknown_stream",
                Rejected::Unsealed => "unsealed",
            });
        }
        outcome
    }

    /// Count one arriving packet, and say once per connection what carried it.
    ///
    /// Its own function because the first-packet log is thirty lines of explanation
    /// for one `debug!`, and `apply_datagram` is at the length the lint allows. It
    /// exists because the artifact says the accepting end's first sighting is number
    /// 1 on every lane — never 0 — so the dialer's first numbered packet is lost on
    /// the way and the cumulative frontier is pinned for the lane's life
    /// (`HANDOFF.md` §4.182). The frame class is what says whether the listener's
    /// "an unknown connection may only be opened by a stream frame at offset zero
    /// **carrying bytes**" rule dropped it.
    fn count_seen(&mut self, packet: &super::wire::Packet<'_>) {
        if packet.unnumbered {
            return;
        }
        self.numbers_seen = self.numbers_seen.saturating_add(1);
        let first = self.first_seen.is_none();
        self.note_first_seen(packet.number);
        if !first {
            return;
        }
        let (own_last, own_below) = self.own_numbering();
        tracing::debug!(
            number = packet.number,
            frame = frame_name(&packet.frame),
            own_last,
            own_below,
            "mhd first numbered packet this end ever saw"
        );
    }

    /// One arriving stream range: the window check, the stream's creation if this
    /// packet opens it, the storage, what arrived against what was news, and the
    /// acknowledgement it owes.
    ///
    /// Its own function because `apply_datagram` is at the length the lint allows and
    /// this arm is the longest of its frames — and because the arrival accounting is
    /// a question of its own (`HANDOFF.md` §4.183).
    fn accept_stream(
        &mut self,
        now: Instant,
        range: &Incoming<'_>,
        fresh: Option<StreamKeys>,
        sampled: bool,
    ) -> Result<(), Rejected> {
        let Incoming {
            id,
            offset,
            data,
            fin,
        } = *range;
        if offset + data.len() as u64 > self.recv_credit {
            return Err(Rejected::OverWindow);
        }
        let known = self.streams.contains_key(&id);
        // One of *our* streams, and it is gone: its handle was dropped and the
        // connection retired it while the peer still had bytes on the wire for it.
        // Nobody wants those bytes, and the frame is dropped — but the packet is
        // still acknowledged by the caller, because a sender that keeps re-sending a
        // packet nobody will acknowledge counts ten unanswered retransmissions and
        // declares the whole lane dead. That is how one short-lived visitor
        // connection was killing the lane every other visitor shared.
        let retired = self.is_retired_stream(id, known);
        if retired {
            tracing::debug!(stream = id, "mhd dropped data for a retired stream");
            // The bytes are gone and nobody will read them, but the peer spent its
            // window on them: give that window back, or a lane carrying many short
            // streams narrows by one dead stream at a time until it carries nothing.
            self.advance_read(data.len() as u64);
            return Ok(());
        }
        if !known {
            // The peer opens a stream by naming it; the accept queue is what tells
            // its owner. A sealed stream arrives with the keys this packet was
            // opened with.
            let state = match fresh {
                Some((send_cipher, recv_cipher)) => StreamState {
                    sealed: true,
                    send_cipher,
                    recv_cipher,
                    ..StreamState::default()
                },
                None => StreamState::default(),
            };
            self.streams.insert(id, state);
            self.inbound.push_back(id);
            self.inbound_at.insert(id, now);
            self.stream_opened.insert(id, now);
            self.highest_inbound = Some(self.highest_inbound.map_or(id, |h| h.max(id)));
        }
        let blocks = &mut self.blocks;
        let state = self.streams.get_mut(&id).ok_or(Rejected::UnknownStream)?;
        // **What arrived, against what was news.** The stream buffers out-of-order
        // bytes, so a re-sent range whose first copy simply arrived late is stored
        // again and dropped by offset — invisible to every counter this end had.
        // Whether the carrier is re-sending data the peer already holds is *the*
        // question §4.183 leaves, and this is the only place that can answer it: the
        // frontier and the held bytes before and after the insert, subtracted from
        // what the packet carried.
        let before = (state.recv.arrived(), state.recv.held());
        stats::around(sampled, &stats::PHASES.append, || {
            state.recv.insert(blocks, offset, data);
        });
        let after = (state.recv.arrived(), state.recv.held());
        let arrived = u64::try_from(data.len()).unwrap_or(0);
        let news = after
            .0
            .saturating_sub(before.0)
            .saturating_add(after.1.saturating_sub(before.1));
        self.ledger.recv_bytes = self.ledger.recv_bytes.saturating_add(arrived);
        self.ledger.recv_duplicate_bytes = self
            .ledger
            .recv_duplicate_bytes
            .saturating_add(arrived.saturating_sub(news));
        state.inserted = state.inserted.saturating_add(arrived);
        state.recv.note_ready(Some(now));
        if fin {
            state.recv.finish(offset + data.len() as u64);
            state.peer_fin = true;
        }
        // Remember this stream's arrived prefix, which every acknowledgement from
        // now on carries: it is what lets the peer's send buffer move on the bytes
        // rather than on the packet records that carried them (see `wire::ACK`).
        self.arrived_stream = id;
        self.arrived_prefix = state.recv.arrived();
        // The reader that owns this stream is the only one the pump has to wake: it
        // got bytes or an end, and the rest of the lane's readers did not.
        self.woke.push(id);
        Ok(())
    }

    /// The body of [`Conn::on_datagram`], with this datagram's sampling decision.
    fn apply_datagram(&mut self, now: Instant, buf: &[u8], sampled: bool) -> Result<(), Rejected> {
        let decode = stats::Timing::start(sampled);
        // Two different defects hide behind one word, so they are counted apart:
        // a datagram this end cannot frame at all, and one framed for another
        // lane. A run that logs a couple of either every second is not healthy,
        // and which of the two it is decides where to look.
        let packet = wire::decode(buf).map_err(|_| {
            stats::note_rejected("malformed_decode");
            Rejected::Malformed
        })?;
        decode.stop(&stats::PHASES.decode);
        self.count_seen(&packet);
        if packet.id != self.id {
            stats::note_rejected("malformed_lane");
            return Err(Rejected::Malformed);
        }
        if self.closed.is_some() {
            // A closed connection consumes nothing and acknowledges nothing:
            // the peer will learn it from its own silence, and re-awakening a
            // dead lane here would let a stale datagram revive it.
            return Ok(());
        }
        self.last_recv = now;
        if let Some(ack) = packet.ack {
            self.apply_ack(now, ack, sampled);
        }
        match packet.frame {
            // Nothing above consumes a datagram frame yet; it arrives, is counted, and
            // is dropped without touching a stream, a credit or the recovery table.
            // The count is the instrument: it must grow while `flight` does not
            // (HANDOFF §4.71).
            wire::Frame::Datagram { id, data } => {
                self.accept_datagram(id, data);
                return Ok(());
            }
            Frame::None => {}
            Frame::Stream {
                id,
                offset,
                fin,
                data,
            } => {
                let (data, fresh) = self.open_stream_payload(&packet, id, data)?;
                let range = Incoming {
                    id,
                    offset,
                    data: data.as_ref(),
                    fin,
                };
                self.accept_stream(now, &range, fresh, sampled)?;
                self.ack_pending = true;
            }
            Frame::Credit { connection } => {
                self.send_credit = connection;
            }
            Frame::Reset { id, code } => {
                // Only an abort ends a stream wherever it is. A clean finish
                // rides a tracked frame with its offset (see `pending_fins`), so if
                // one arrives as a reset it is a peer that predates this dialect —
                // there is none — and it is ignored rather than guessed at.
                if code != ResetCode::Finished
                    && let Some(state) = self.streams.get_mut(&id)
                {
                    // The peer ended this stream in both directions. One line,
                    // because a stream that a visitor was reading is exactly the
                    // kind of thing a "the connection just closed" report needs
                    // evidence for.
                    tracing::debug!(
                        stream = id,
                        code = ?code,
                        "mhd peer reset a stream"
                    );
                    state.reset = Some(code);
                    // A parked reader has to learn that its stream is over, and
                    // it is the one reader this datagram concerns.
                    self.woke.push(id);
                }
            }
        }
        // Acknowledged only after the frame was accepted: an acknowledgement is
        // the promise that makes the sender forget the bytes, so it may not be
        // sent for a frame this end rejected. A control packet repeats a number
        // the peer already used and is never retransmitted, so counting it would
        // acknowledge a data packet that may never have arrived.
        if !packet.unnumbered {
            self.numbers_acked = self.numbers_acked.saturating_add(1);
            self.acks.insert(packet.number);
        }
        Ok(())
    }

    /// Apply the peer's acknowledgement: retire what it covers, rewind what
    /// it reports lost, and open the window on the progress it proves.
    fn apply_ack(&mut self, now: Instant, ack: wire::Ack, sampled: bool) {
        self.acks_applied = self.acks_applied.saturating_add(1);
        self.last_ack_prefix = ack.prefix;
        self.last_ack_below = ack.below;
        self.last_ack_bits = ack.bits;
        self.last_ack_largest = ack.largest;
        let ack_timing = stats::Timing::start(sampled);
        let acked = self.recovery.on_ack(now, ack);
        for sent in acked.newly {
            let Some(range) = sent.range else {
                continue;
            };
            let moved = match self.streams.get_mut(&range.stream) {
                Some(state) => {
                    state.acknowledged = true;
                    state.send.ack(range.offset, usize::from(range.len))
                }
                None => 0,
            };
            self.buffered = self.buffered.saturating_sub(moved);
            // The frontier's own advance is what the peer confirmed — see
            // `SendLedger::acked_bytes` for why a record count cannot stand in for it.
            self.ledger.note_delivered(moved);
            // `lost_at` survives `take`, so this is the one place that can tell a
            // packet confirmed in its own right from one confirmed **after** the
            // lane had already declared it lost and queued the range again: the
            // second is a duplicate this carrier paid for on its own judgement.
            self.ledger
                .note_retired(u64::from(range.len), false, sent.lost_at.is_some());
        }
        // The byte prefix is the authority on this stream's frontier, because
        // it cannot stall the way the range acknowledgements can: a packet
        // number lost for good stops the peer's cumulative frontier, and every
        // range acknowledged behind that hole then waits on a key the frontier
        // walk never reaches. The prefix only ever grows, so it frees those
        // bytes without needing the hole to be filled.
        if ack.prefix > 0
            && let Some(state) = self.streams.get_mut(&ack.stream)
        {
            let moved = state.send.advance_to(ack.prefix);
            state.acknowledged = true;
            self.buffered = self.buffered.saturating_sub(moved);
            // The prefix moves the same frontier the number path does, so its
            // delivery is counted the same way and only once.
            self.ledger.note_delivered(moved);
        }
        // **And the same confirmation retires the recovery records it covers.** The
        // prefix is the only acknowledgement that cannot stall on a lost packet
        // number, and the flight is what `write` admits against: freeing the stream
        // buffer while leaving the bytes in the flight closed the window on a lane
        // whose peer had received everything (`HANDOFF.md` §4.108).
        //
        // `buffered` is not touched again: `advance_to` above already released those
        // bytes from the stream's buffer, and the flight is a separate total.
        if ack.prefix > 0 {
            let (freed, late) = self.recovery.confirm_prefix(ack.stream, ack.prefix);
            // The second confirmation path, kept apart on purpose: these bytes are
            // **not** in `Acked::bytes`, so they free the flight without growing
            // the window. Whether that omission matters is a measurement, not a
            // reading of the code — this is the counter that decides it.
            //
            // `freed` alone would under-count what the peer confirmed: a record
            // already declared lost left the flight then, so retiring it frees
            // nothing, and its bytes arrive here as `late`. Both are confirmed all
            // the same, which is why the ledger adds them together and reports the
            // late part separately.
            // Two populations, counted separately: the records this end was still
            // waiting for, and the ones it had already declared lost. `freed` is the
            // first (they carried the flight total), `late` the second.
            self.ledger.note_retired(freed, true, false);
            self.ledger.note_retired(late, true, true);
        }
        // A loss only rewinds the send point: the bytes are still in the
        // buffer, so the next pass carries them again from the same offset.
        // Its bytes have already left the flight — the recovery table is the
        // one thing that removes a packet, and it keeps the byte total — and
        // they rejoin it when they are sent again, so the window counts what
        // is on the wire and not what is owed.
        self.note_retransmits(acked.lost.len());
        if !acked.lost.is_empty() {
            // The path dropped something: its capacity has been found, and the
            // window grows additively from here on. Nothing is cut — the loss
            // itself is repaired by retransmission.
            self.ledger.lost_by_threshold = self
                .ledger
                .lost_by_threshold
                .saturating_add(u64::try_from(acked.lost.len()).unwrap_or(u64::MAX));
            self.slow_start = false;
        }
        for sent in acked.lost {
            if let Some(range) = sent.range {
                if range.len == 0 && range.fin {
                    // A finish-only packet: nothing to re-send, so queue the
                    // finish again.
                    self.pending_fins.push_back((range.stream, range.offset));
                    continue;
                }
                let mut at_base = false;
                let mut above = false;
                if let Some(state) = self.streams.get_mut(&range.stream) {
                    // Which side of the frontier this declaration re-sends. A repair
                    // that covers the frontier can unblock it; one above it cannot,
                    // however often it is sent — the difference the counters below
                    // keep, because every repair counter in this file until now
                    // counted both as "a repair" (rounds 43 to 61).
                    let base = state.send.base();
                    let start = range.offset;
                    let end = start.saturating_add(u64::from(range.len));
                    at_base = start <= base && base < end;
                    above = start > base;
                    state.send.repair(range.offset, usize::from(range.len));
                    state.retransmits = state.retransmits.saturating_add(1);
                }
                if at_base {
                    self.repairs_at_base = self.repairs_at_base.saturating_add(1);
                } else if above {
                    self.repairs_above = self.repairs_above.saturating_add(1);
                }
            }
        }
        // The window opens in two phases, the way every transport that has to
        // find a path's capacity does it. Until the path has dropped
        // something, every acknowledged byte buys a byte of window: the flight
        // doubles once per round trip, which is what lets a flow of *small*
        // packets reach the path's capacity in a few round trips instead of
        // crawling there one big packet per window. After a loss it is
        // additive — one packet per window acknowledged — and the window is
        // never cut: a loss is repaired by retransmission, not paid for by
        // halving the pipe for a round trip.
        let before = self.cwnd;
        if acked.progress {
            let bytes = u64::try_from(acked.bytes).unwrap_or(u64::MAX);
            if self.slow_start {
                // Doubling stops at the ceiling and the additive rule takes
                // over, because an unspaced burst that outruns the peer's
                // buffer costs the connection its liveness (see
                // [`WINDOW_FLOOR`]).
                self.cwnd = self.cwnd.saturating_add(bytes).min(self.window_ceiling());
            } else {
                self.acked_since_growth += bytes;
                if self.acked_since_growth >= self.cwnd {
                    self.acked_since_growth = 0;
                    self.cwnd = self
                        .cwnd
                        .saturating_add(MAX_STREAM_PAYLOAD as u64)
                        .min(self.window_ceiling());
                }
            }
        }
        // What the window actually gained, beside how many bytes were confirmed:
        // the two together say whether growth is one packet per window (as the
        // code reads) or something else entirely (as a lossy path measures).
        self.ledger.grown = self
            .ledger
            .grown
            .saturating_add(self.cwnd.saturating_sub(before));
        // A stream whose every byte is now acknowledged can say its finish
        // (see `settle`): the acknowledgement is the proof that the reset
        // will not overtake the data it follows.
        self.settle_finished();
        ack_timing.stop(&stats::PHASES.ack);
    }

    /// Retire streams whose owner is gone and whose bytes can no longer arrive.
    ///
    /// `settle` waits for the peer to acknowledge everything, which is right for a
    /// graceful close and wrong for a stream nobody will read again: an
    /// unacknowledged tail then keeps its bytes in `buffered` for the lane's life,
    /// and `write`'s buffer ceiling is that number (see [`ABANDON_HOLD`]). This is
    /// the only place a stream leaves the table without being acknowledged.
    ///
    /// Called from [`Conn::on_timeout`], which the pump already drives, so there is
    /// no new clock read on any hot path and no new task.
    fn reap_abandoned(&mut self, now: Instant) -> usize {
        let stale: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, state)| {
                state.abandoned
                    && state.send.held() > 0
                    && state
                        .abandoned_at
                        .is_some_and(|at| now.saturating_duration_since(at) >= ABANDON_HOLD)
            })
            .map(|(&id, _)| id)
            .collect();
        for id in &stale {
            // The peer is told the stream is over, so its own table does not hold a
            // half-open stream that nothing will ever finish.
            self.resets.push_back((*id, ResetCode::Aborted));
            let held = self.streams.remove(id).map_or(0, |mut state| {
                state.recv.discard(&mut self.blocks);
                u64::try_from(state.send.held()).unwrap_or(u64::MAX)
            });
            self.buffered = self.buffered.saturating_sub(held);
            self.stream_opened.remove(id);
            self.finishing.remove(id);
            tracing::debug!(
                stream = id,
                released_bytes = held,
                "mhd abandoned stream retired without an acknowledgement"
            );
        }
        stale.len()
    }

    /// The timer expired: why, and how many packets it put back in the queue.
    pub(crate) fn on_timeout(&mut self, now: Instant) -> (Expiry, usize) {
        if self.closed.is_some() {
            return (Expiry::Loss, 0);
        }
        self.reap_abandoned(now);
        // The keepalive deadline is the other caller of this: when it fires
        // with nothing in flight there is nothing to recover, and the pump's
        // next pass emits the probe (see `poll_transmit`).
        let (expiry, lost) = self.recovery.on_timeout(now);
        // Which evidence named this loss: a probe of a silent path, or the timer
        // finding live records older than the reordering window. They are different
        // defects — one is a path that answered nothing, the other is a path whose
        // acknowledgements are late — and only one of them is what a lossy cell
        // should be doing.
        let declared = u64::try_from(lost.len()).unwrap_or(u64::MAX);
        if expiry == Expiry::Timeout {
            self.ledger.probes = self.ledger.probes.saturating_add(declared);
        } else {
            self.ledger.lost_by_timer = self.ledger.lost_by_timer.saturating_add(declared);
        }
        self.note_retransmits(lost.len());
        if !lost.is_empty() {
            // A timer that re-sent the flight is a path that could not carry it:
            // the capacity phase is over.
            self.slow_start = false;
        }
        for sent in &lost {
            if let Some(range) = sent.range {
                if range.len == 0 && range.fin {
                    self.pending_fins.push_back((range.stream, range.offset));
                    continue;
                }
                let mut at_base = false;
                let mut above = false;
                if let Some(state) = self.streams.get_mut(&range.stream) {
                    let base = state.send.base();
                    let start = range.offset;
                    let end = start.saturating_add(u64::from(range.len));
                    at_base = start <= base && base < end;
                    above = start > base;
                    state.send.repair(range.offset, usize::from(range.len));
                    state.retransmits = state.retransmits.saturating_add(1);
                }
                if at_base {
                    self.repairs_at_base = self.repairs_at_base.saturating_add(1);
                } else if above {
                    self.repairs_above = self.repairs_above.saturating_add(1);
                }
            }
        }
        if expiry == Expiry::Dead {
            // Silence is what makes a path dead, not a count of timers. A peer
            // whose socket buffer is full drops everything and answers nothing,
            // but it is alive — and the keepalives it still sends prove it, so the
            // connection keeps probing instead of closing a lane the pool would
            // have to rebuild. Only a peer that has sent nothing at all for
            // `DEAD_AFTER` is gone, and that check is in `poll_transmit`.
            if now.saturating_duration_since(self.last_recv) > DEAD_AFTER {
                self.closed = Some(ResetCode::Aborted);
            } else {
                // Report it as the probe it turned into, so the pump does not
                // treat the connection as ended.
                return (Expiry::Timeout, lost.len());
            }
        }
        (expiry, lost.len())
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use std::time::Duration;

    /// One in-memory link between two connections. Dropping a datagram here is
    /// what a test does instead of losing one on a network.
    struct Link {
        a: Conn,
        b: Conn,
        a_to_b: VecDeque<Vec<u8>>,
        b_to_a: VecDeque<Vec<u8>>,
    }

    impl Link {
        fn new() -> Self {
            Self {
                a: Conn::new(9, true),
                b: Conn::new(9, false),
                a_to_b: VecDeque::new(),
                b_to_a: VecDeque::new(),
            }
        }

        /// Move everything both ends want to send and drop every `drop_every`th
        /// datagram in each direction, which is the one thing the clean `pump`
        /// cannot do and the only difference between this harness and a real
        /// socket under load.
        fn pump_lossy(&mut self, now: Instant, drop_every: usize) -> usize {
            let mut moved = 0;
            let mut dropped = 0;
            loop {
                let mut progress = false;
                let mut buf = Vec::new();
                while let Some(len) = self.a.poll_transmit(now, &mut buf) {
                    let datagram = buf[buf.len() - len..].to_vec();
                    moved += 1;
                    dropped += 1;
                    if dropped % drop_every != 0 {
                        self.a_to_b.push_back(datagram);
                    }
                    progress = true;
                }
                while let Some(len) = self.b.poll_transmit(now, &mut buf) {
                    let datagram = buf[buf.len() - len..].to_vec();
                    moved += 1;
                    dropped += 1;
                    if dropped % drop_every != 0 {
                        self.b_to_a.push_back(datagram);
                    }
                    progress = true;
                }
                while let Some(datagram) = self.a_to_b.pop_front() {
                    self.b.on_datagram(now, &datagram).unwrap();
                    progress = true;
                }
                while let Some(datagram) = self.b_to_a.pop_front() {
                    self.a.on_datagram(now, &datagram).unwrap();
                    progress = true;
                }
                if !progress {
                    return moved;
                }
            }
        }

        /// Move everything both ends want to send, then deliver it, until both
        /// are quiet. Answers how many datagrams moved.
        fn pump(&mut self, now: Instant) -> usize {
            let mut moved = 0;
            loop {
                let mut progress = false;
                let mut buf = Vec::new();
                while let Some(len) = self.a.poll_transmit(now, &mut buf) {
                    self.a_to_b.push_back(buf[buf.len() - len..].to_vec());
                    moved += 1;
                    progress = true;
                }
                while let Some(len) = self.b.poll_transmit(now, &mut buf) {
                    self.b_to_a.push_back(buf[buf.len() - len..].to_vec());
                    moved += 1;
                    progress = true;
                }
                while let Some(datagram) = self.a_to_b.pop_front() {
                    self.b.on_datagram(now, &datagram).unwrap();
                    progress = true;
                }
                while let Some(datagram) = self.b_to_a.pop_front() {
                    self.a.on_datagram(now, &datagram).unwrap();
                    progress = true;
                }
                if !progress {
                    return moved;
                }
            }
        }

        /// The same, but the first datagram in flight is dropped.
        fn pump_dropping_one(&mut self, now: Instant) -> usize {
            let mut moved = 0;
            let mut buf = Vec::new();
            let mut dropped = false;
            while let Some(len) = self.a.poll_transmit(now, &mut buf) {
                if !dropped {
                    dropped = true;
                    moved += 1;
                    continue;
                }
                self.a_to_b.push_back(buf[buf.len() - len..].to_vec());
                moved += 1;
            }
            while let Some(datagram) = self.a_to_b.pop_front() {
                self.b.on_datagram(now, &datagram).unwrap();
            }
            while let Some(len) = self.b.poll_transmit(now, &mut buf) {
                self.b_to_a.push_back(buf[buf.len() - len..].to_vec());
            }
            while let Some(datagram) = self.b_to_a.pop_front() {
                self.a.on_datagram(now, &datagram).unwrap();
            }
            moved
        }
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    /// The window a stream's bytes were sent against comes back when the stream
    /// ends without a reader.
    ///
    /// The window follows the reader, so bytes this end discards would be spent
    /// for ever: the peer counts them against the window it was granted and the
    /// window never reopens. That leaks one dead stream at a time, and the only
    /// symptom is a long-lived lane that ends up `credit_blocked` with every
    /// reader keeping up — which is the shape a lane carrying many short
    /// visitors has.
    #[test]
    fn a_stream_retired_with_unread_bytes_returns_the_window() {
        let mut conn = Conn::new(9, false);
        let opening = conn.recv_credit;
        let blocks = &mut conn.blocks;
        let state = conn.streams.entry(2).or_default();
        state.recv.insert(blocks, 0, &payload(10));
        assert_eq!(conn.backlog().total, 10, "ten bytes are held");

        conn.release_unread(2);
        assert_eq!(
            conn.recv_credit,
            opening + 10,
            "the ten bytes come back as window"
        );
        assert_eq!(conn.backlog().total, 0, "and are no longer reported held");

        // Idempotent: a stream released twice must not hand out the window
        // twice, or the peer would be allowed twice what this end can hold.
        conn.release_unread(2);
        assert_eq!(conn.recv_credit, opening + 10);
    }

    /// Take the contiguous prefix of one stream, the way a caller with a large
    /// buffer would.
    fn take(conn: &mut Conn, stream: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 1 << 20];
        let n = conn.read(stream, &mut buf);
        buf.truncate(n);
        buf
    }

    /// One handshake's worth of root material, from a seed.
    ///
    /// Two directional secrets and a transcript, which is what a real handshake
    /// contributes. A test cares that both ends hold the *same* material and that
    /// different lanes hold different material — not that these bytes resemble a
    /// Diffie-Hellman output, so a seed is enough and it keeps the failure
    /// legible.
    fn secret(seed: u8) -> super::super::cipher::Secret {
        (
            [[seed; 32], [seed.wrapping_add(1); 32]],
            [seed.wrapping_add(2); 32],
        )
    }

    /// The same four sealed streams and the same frames, with loss.
    ///
    /// `pump_lossy` drops one datagram in every `drop_every`, which is the only
    /// difference between this harness and a real socket on a loaded host. If the
    /// sealed striped integration test's failure lives in the stripe layer's
    /// interaction with a sealed stream, this is the smallest place it can appear.
    /// The lossy four-stream frame transfer, sealed or not: the control that says
    /// whether a failure is the seal's or the harness's.
    fn frames_through_loss(sealed: bool) -> Vec<Vec<u8>> {
        const FRAME: usize = 32 * 1024;
        let now = Instant::now();
        let mut link = Link::new();
        if sealed {
            let root = secret(7);
            link.a.set_lane_secret(&root);
            link.b.set_lane_secret(&root);
        }

        let per = 256 * 1024;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; per]).collect();
        let ids: Vec<u32> = (0..4).map(|_| link.a.open().id).collect();
        let mut queued = vec![0usize; ids.len()];
        for (index, id) in ids.iter().enumerate() {
            queued[index] += link.a.write(*id, &bodies[index][..1]);
        }
        let mut later = now;
        link.pump_lossy(later, 500);
        while link.b.accept().is_some() {}

        let mut got: Vec<Vec<u8>> = vec![Vec::new(); ids.len()];
        let mut moved = 0;
        for _ in 0..200_000 {
            for (index, id) in ids.iter().enumerate() {
                while queued[index] < per {
                    let end = (queued[index] + FRAME).min(per);
                    let n = link.a.write(*id, &bodies[index][queued[index]..end]);
                    if n == 0 {
                        break;
                    }
                    queued[index] += n;
                }
            }
            // Time moves and **the timer is driven**, because a lost datagram is
            // only re-sent when something decides it is owed again: a harness that
            // advances the clock without calling `on_timeout` reproduces a stall
            // that is the harness's own.
            later += Duration::from_millis(1);
            link.a.on_timeout(later);
            link.b.on_timeout(later);
            moved += link.pump_lossy(later, 500);
            for (index, id) in ids.iter().enumerate() {
                let out = take(&mut link.b, *id);
                got[index].extend_from_slice(&out);
            }
            if got.iter().map(Vec::len).sum::<usize>() == per * ids.len() {
                break;
            }
        }
        assert!(moved > 0);
        // The carrier's own account at the moment the transfer stopped moving, so
        // a failure names why instead of only where.
        let (cwnd, flight, slow_start, _) = link.a.window_state();
        let backlog = link.a.backlog();
        eprintln!(
            "sealed={sealed} cwnd={cwnd} flight={flight} slow_start={slow_start} \
             backlog_streams={} backlog_bytes={} worst={:?}",
            backlog.streams, backlog.total, backlog.worst
        );
        for id in 0..4u32 {
            eprintln!("  stream {id}: got={}", got[id as usize].len());
        }
        got
    }

    #[test]
    fn four_unsealed_streams_carry_frames_through_loss() {
        let per = 256 * 1024;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; per]).collect();
        let got = frames_through_loss(false);
        for (index, body) in bodies.iter().enumerate() {
            assert_eq!(
                got[index].len(),
                body.len(),
                "unsealed stream {index} delivered every byte through loss"
            );
        }
    }

    #[test]
    fn four_sealed_streams_carry_frames_through_loss() {
        let per = 256 * 1024;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; per]).collect();
        let got = frames_through_loss(true);
        for (index, body) in bodies.iter().enumerate() {
            assert_eq!(
                got[index].len(),
                body.len(),
                "sealed stream {index} delivered every byte through loss"
            );
            assert_eq!(&got[index], body, "sealed stream {index} is byte-exact");
        }
    }

    /// The window ceiling is the path's, not a flat number.
    ///
    /// A flat two mebibytes is forty times the bandwidth-delay product of a
    /// loopback path, and the standing queue it builds is what put 4.5 ms in front
    /// of every handshake packet once a bulk spine crossed eight gigabits a second.
    /// The ceiling is therefore the floor scaled by the measured round trip — and
    /// this test pins all three regimes: the shortest path gets the floor, a shaped
    /// path gets its product, and nothing gets more than the absolute cap.
    #[test]
    fn the_window_ceiling_scales_with_the_path() {
        // A fresh lane per regime: the estimate is an average, so seeding 2 ms onto
        // a lane that already smoothed 1 ms would test the averaging and not the
        // formula.
        let ceiling_at = |rtt: Duration| {
            let mut link = Link::new();
            link.a.seed_rtt(rtt);
            link.a.window_ceiling()
        };
        assert_eq!(
            ceiling_at(Duration::from_millis(1)),
            WINDOW_FLOOR,
            "a one-millisecond path gets the floor"
        );
        assert_eq!(
            ceiling_at(Duration::from_millis(2)),
            WINDOW_FLOOR * 2,
            "a two-millisecond path gets two floors"
        );
        assert_eq!(
            ceiling_at(Duration::from_millis(50)),
            WINDOW_FLOOR * 50,
            "a fifty-millisecond path gets fifty floors"
        );
        assert_eq!(
            ceiling_at(Duration::from_secs(10)),
            MAX_CWND,
            "no path gets more than the absolute cap"
        );
    }

    /// A queue cannot raise its own ceiling.
    ///
    /// The ceiling is derived from the smallest round trip ever measured, so a lane
    /// that has seen a quiet path keeps that path's ceiling however long its queue
    /// grows afterwards. Deriving it from the smoothed estimate does the opposite:
    /// the queue inflates the estimate, the estimate raises the ceiling, and the
    /// lane latches into a regime where every small packet pays for the queue that
    /// set it — which is what the first stage's bad mode looks like from outside
    /// (median four times the good mode's at the same bulk rate).
    #[test]
    fn a_queue_does_not_raise_its_own_ceiling() {
        let mut link = Link::new();
        link.a.seed_rtt(Duration::from_millis(1));
        assert_eq!(link.a.window_ceiling(), WINDOW_FLOOR, "a quiet path");
        // Twenty milliseconds of standing queue, four times over.
        for _ in 0..4 {
            link.a.seed_rtt(Duration::from_millis(20));
        }
        assert_eq!(
            link.a.window_ceiling(),
            WINDOW_FLOOR,
            "the ceiling still belongs to the path, not to the queue"
        );
    }

    /// Four sealed streams carrying **32 KiB writes**, which is what the stripe
    /// layer does.
    ///
    /// The passing four-stream test writes whatever `Conn::write` takes; the
    /// stripe layer writes a whole frame at a time, so every write is longer than
    /// the sealed packet's clamped payload (`MAX_SEALED_PAYLOAD`) and the sender
    /// sees partial writes on every one. That is the shape the sealed striped
    /// integration test fails in, and no unit test had it.
    #[test]
    fn four_sealed_streams_carry_thirty_two_kib_frames() {
        const FRAME: usize = 32 * 1024;
        let now = Instant::now();
        let mut link = Link::new();
        let root = secret(6);
        link.a.set_lane_secret(&root);
        link.b.set_lane_secret(&root);

        let per = 1 << 20;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; per]).collect();
        let ids: Vec<u32> = (0..4).map(|_| link.a.open().id).collect();
        let mut queued = vec![0usize; ids.len()];
        for (index, id) in ids.iter().enumerate() {
            queued[index] += link.a.write(*id, &bodies[index][..1]);
        }
        link.pump(now);
        let mut accepted = 0;
        while link.b.accept().is_some() {
            accepted += 1;
        }
        assert_eq!(accepted, 4, "four streams arrived");

        let mut got: Vec<Vec<u8>> = vec![Vec::new(); ids.len()];
        for _ in 0..400_000 {
            for (index, id) in ids.iter().enumerate() {
                // One frame at a time: the slice handed to `write` is a frame, not
                // the rest of the stream.
                while queued[index] < per {
                    let end = (queued[index] + FRAME).min(per);
                    let n = link.a.write(*id, &bodies[index][queued[index]..end]);
                    if n == 0 {
                        break;
                    }
                    queued[index] += n;
                }
            }
            link.pump(now);
            for (index, id) in ids.iter().enumerate() {
                let out = take(&mut link.b, *id);
                got[index].extend_from_slice(&out);
            }
            if got.iter().map(Vec::len).sum::<usize>() == per * ids.len() {
                break;
            }
        }
        for (index, body) in bodies.iter().enumerate() {
            assert_eq!(
                got[index].len(),
                body.len(),
                "stream {index} delivered every byte"
            );
            assert_eq!(&got[index], body, "stream {index} is byte-exact");
        }
    }

    /// A sealed stream repairs one dropped packet instead of looping on it.
    ///
    /// The sealed striped test fails about half the time with thousands of
    /// retransmissions on one stream and every byte already on the wire, which is
    /// the shape of a repair that is declared for ever. This is the smallest
    /// reproduction that could produce one: a sealed stream, one packet taken off
    /// the wire, and the timer that owes it again. If the loop needs something
    /// else — four streams, a real socket, a wide flight — this test will pass and
    /// say so, which is worth knowing before another integration round.
    #[test]
    fn a_sealed_stream_repairs_one_dropped_packet() {
        let now = Instant::now();
        let mut link = Link::new();
        let root = secret(11);
        link.a.set_lane_secret(&root);
        link.b.set_lane_secret(&root);
        let stream = link.a.open();
        // Under one congestion window: a first write is bounded by
        // `INITIAL_CWND`, and a short write here would be this test's business
        // rather than the carrier's.
        let bytes = payload(8 * 1024);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        // Take **one stream packet** off the wire and drop it. Counting packets
        // rather than assuming the first one is a stream: the first is usually a
        // credit frame, and dropping that would test something else entirely.
        let mut dropped = 0;
        for _ in 0..8 {
            let mut buf = Vec::new();
            let Some(len) = link.a.poll_transmit(now, &mut buf) else {
                break;
            };
            let Some(packet) = super::wire::decode(&buf[buf.len() - len..]).ok() else {
                unreachable!("every packet this end builds decodes")
            };
            let carries = matches!(
                packet.frame,
                super::wire::Frame::Stream { id, .. } if id == stream.id
            );
            if carries && dropped == 0 {
                dropped += 1;
                continue;
            }
            link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
        }
        assert_eq!(dropped, 1, "one stream packet was dropped");

        // The timer fires and the range is owed again.
        let later = now + Duration::from_millis(30);
        let (_, resent) = link.a.on_timeout(later);
        assert!(
            resent >= 1,
            "the dropped packet is owed again, not forgotten"
        );

        // Everything else flows, and the stream completes.
        let mut got: Vec<u8> = Vec::new();
        for step in 0..64u64 {
            link.pump(later + Duration::from_millis(step));
            got.extend(take(&mut link.b, stream.id));
            if got.len() == bytes.len() {
                break;
            }
        }
        assert_eq!(
            got, bytes,
            "the sealed stream repaired the loss and delivered every byte"
        );
    }

    /// Eight mebibytes over one sealed stream, written as the writer would.
    ///
    /// The striped echo moves exactly this much and dies part-way; this says
    /// whether the volume alone is enough to reproduce it, with no stripes, no
    /// forwarding loop and no sockets in the way.
    #[test]
    fn a_sealed_stream_carries_eight_mebibytes() {
        let now = Instant::now();
        let mut link = Link::new();
        let root = secret(9);
        link.a.set_lane_secret(&root);
        link.b.set_lane_secret(&root);
        let stream = link.a.open();
        let total = 8 * 1024 * 1024;
        let chunk = payload(64 * 1024);
        let mut written = 0usize;
        let mut got: Vec<u8> = Vec::with_capacity(total);
        // The stream exists at the peer only once a packet names it.
        link.a.write(stream.id, &chunk[..1]);
        written += 1;
        link.pump(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the peer names the stream")
        };
        for _ in 0..200_000 {
            while written < total {
                let n = link
                    .a
                    .write(stream.id, &chunk[..(total - written).min(chunk.len())]);
                if n == 0 {
                    break;
                }
                written += n;
            }
            link.pump(now);
            let out = take(&mut link.b, accepted.id);
            got.extend_from_slice(&out);
            if got.len() == total {
                break;
            }
        }
        assert_eq!(written, total, "every byte was accepted by the writer");
        assert_eq!(got.len(), total, "and every byte crossed the sealed stream");
    }

    /// Several sealed streams carry data at once, as a striped group does.
    ///
    /// The striped echo dies only when sealing and stripes are together, so this
    /// is the carrier's half of that combination: four sealed streams, written
    /// interleaved and read at the peer, each with its own bytes.
    #[test]
    fn four_sealed_streams_carry_their_own_bytes() {
        let now = Instant::now();
        let mut link = Link::new();
        let root = secret(4);
        link.a.set_lane_secret(&root);
        link.b.set_lane_secret(&root);

        let per = 256 * 1024;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; per]).collect();
        let ids: Vec<u32> = (0..4).map(|_| link.a.open().id).collect();
        // Name every stream first, so the peer's accept queue has all four.
        let mut queued = vec![0usize; ids.len()];
        for (index, id) in ids.iter().enumerate() {
            queued[index] += link.a.write(*id, &bodies[index][..1]);
        }
        link.pump(now);
        let mut accepted = 0;
        while link.b.accept().is_some() {
            accepted += 1;
        }
        assert_eq!(accepted, 4, "four streams arrived");

        // The writer is bounded by the peer's credit, so it is fed as the window
        // moves — which is how the forwarding loop writes too.
        let mut got: Vec<Vec<u8>> = vec![Vec::new(); ids.len()];
        for _ in 0..200_000 {
            for (index, id) in ids.iter().enumerate() {
                while queued[index] < per {
                    let n = link.a.write(*id, &bodies[index][queued[index]..]);
                    if n == 0 {
                        break;
                    }
                    queued[index] += n;
                }
            }
            link.pump(now);
            for (index, id) in ids.iter().enumerate() {
                let out = take(&mut link.b, *id);
                got[index].extend_from_slice(&out);
            }
            if got.iter().map(Vec::len).sum::<usize>() == per * ids.len() {
                break;
            }
        }
        for (index, body) in bodies.iter().enumerate() {
            assert_eq!(got[index].len(), body.len(), "stream {index} lost bytes");
            assert_eq!(&got[index], body, "stream {index} has the wrong bytes");
        }
    }

    /// The whole stack in memory: what one end writes, the other end reads, byte
    /// for byte, with the stream opened by naming it and accepted from the queue.
    /// The whole stack in memory: what one end writes, the other end reads, byte
    /// for byte, with the stream opened by naming it and accepted from the queue.
    /// A sealed stream carries a payload **larger than one packet**.
    ///
    /// A sealed packet holds `MAX_SEALED_PAYLOAD`, which is `MAX_STREAM_PAYLOAD`
    /// minus the tag — so a writer that hands the stream a full record needs a
    /// second packet for the remaining bytes. If that tail is never emitted the
    /// frame never completes and the writer parks for ever, which is the shape of
    /// the striped echo's stalls.
    #[test]
    fn a_sealed_stream_sends_the_tail_of_an_oversized_payload() {
        let now = Instant::now();
        let mut link = Link::new();
        // Both ends hold the same root, as they do once a lane's first handshake
        // has completed.
        let root = secret(7);
        link.a.set_lane_secret(&root);
        link.b.set_lane_secret(&root);
        let stream = link.a.open();
        let max = super::wire::MAX_STREAM_PAYLOAD;
        let bytes = payload(max);
        assert!(bytes.len() > super::wire::MAX_SEALED_PAYLOAD);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump(now);

        let Some(accepted) = link.b.accept() else {
            unreachable!("the peer named a stream, so it must be queued")
        };
        let out = take(&mut link.b, accepted.id);
        assert_eq!(out, bytes, "every byte, including the tail past one packet");
    }

    #[test]
    fn bytes_written_on_one_end_are_read_on_the_other() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(4096);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump(now);

        let Some(accepted) = link.b.accept() else {
            unreachable!("the peer named a stream, so it must be queued")
        };
        assert_eq!(accepted.id, stream.id, "the id survives the wire");
        let out = take(&mut link.b, accepted.id);
        assert_eq!(out, bytes, "every byte, in order");
    }

    /// A dropped datagram costs a round trip and nothing else: the timer puts
    /// the missing range back in the queue and the reader still gets the stream.
    #[test]
    fn a_dropped_datagram_is_recovered_without_losing_a_stream() {
        let now = Instant::now();
        let mut link = Link::new();
        // Settle the opening frames first: a connection advertises its receive
        // window before it has anything to send, and dropping *that* is not the
        // loss this test is about.
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(8000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump_dropping_one(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        let mut out = take(&mut link.b, accepted.id);
        assert!(
            out.len() < bytes.len(),
            "the dropped datagram must actually cost bytes, got {}",
            out.len()
        );

        // The repair is fast retransmit, not the timer: the later packets this
        // pass delivered put the missing one three behind, so the recovery
        // rewound the stream already. A few passes carry the rest.
        let mut clock = now;
        for _ in 0..8 {
            clock += Duration::from_millis(10);
            link.pump(clock);
            let step = take(&mut link.b, accepted.id);
            out.extend_from_slice(&step);
            if out.len() == bytes.len() {
                break;
            }
        }
        assert_eq!(out, bytes, "the stream is whole after the retransmission");
    }

    /// When the exchange is over, nothing is left counted.
    ///
    /// The accounting invariant that keeps `write`'s admission honest across a
    /// stream's whole life: after a loss, its repair and the acknowledgements that
    /// retire both, the flight, the buffered bytes and the recovery table all read
    /// zero. A leak here is not a slow number — it is the ceiling behind §4.50,
    /// where one abandoned stream's unacknowledged tail refused every later stream
    /// on the lane for the lane's whole life.
    #[test]
    fn a_finished_exchange_leaves_nothing_counted() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(8000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump_dropping_one(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        let mut out = take(&mut link.b, accepted.id);

        let mut clock = now;
        for _ in 0..12 {
            clock += Duration::from_millis(10);
            link.pump(clock);
            out.extend_from_slice(&take(&mut link.b, accepted.id));
        }
        assert_eq!(out, bytes, "every byte arrived, once");

        // A few quiet passes let the last acknowledgements retire what is left.
        for _ in 0..4 {
            clock += Duration::from_millis(10);
            link.pump(clock);
        }
        let (flight, buffered, packets) = link.a.flight_and_buffered();
        assert_eq!(flight, 0, "the flight is empty once the peer has it all");
        assert_eq!(buffered, 0, "and nothing is held for a re-send");
        assert_eq!(
            link.a.recovery.in_flight(),
            0,
            "nothing is still on the wire: whatever record is left is a *lost* \
             one, kept to catch the original if it arrives late"
        );
        assert!(
            packets <= 1,
            "only the one dropped packet's record can remain, got {packets}"
        );

        // And that record is bounded by its hold, not by the connection's life: a
        // path that keeps dropping must not grow the table for ever.
        clock += crate::transport::mhd::recovery::LOST_HOLD + Duration::from_millis(1);
        link.pump(clock);
        let _ = link.a.on_timeout(clock);
        assert_eq!(
            link.a.flight_and_buffered().2,
            0,
            "past its hold the last record goes too"
        );
    }

    /// A queued reset leaves, even with bytes for that stream still unsent.
    ///
    /// The second half of the plan's fifth lifecycle invariant (`HANDOFF.md`
    /// §4.120): "reset interleaved with unsent or unacknowledged data". A peer that
    /// never sees the reset keeps a dead stream for the connection's life — its own
    /// comment says so — and the writer that outran its reader is exactly the case
    /// that produces unsent bytes alongside a queued reset.
    #[test]
    fn a_reset_leaves_even_with_bytes_still_unsent() {
        let mut conn = Conn::new(7, true);
        let stream = conn.open();
        // More than one pass can carry, so bytes are still queued when it closes.
        assert!(conn.write(stream.id, &payload(64 * 1024)) > 0);
        conn.close_stream(stream.id);
        // The hold starts at the real clock inside `close_stream`, not at this
        // test's `now`, and the write above costs real time — so derive the deadline
        // from the stream's own stamp.
        let abandoned_at = conn
            .streams
            .get(&stream.id)
            .and_then(|state| state.abandoned_at)
            .unwrap();
        let later = abandoned_at + ABANDON_HOLD + Duration::from_millis(1);
        let _ = conn.on_timeout(later);
        assert!(
            !conn.resets.is_empty(),
            "the retired stream owes the peer a reset"
        );

        // Drain what this end wants to send, in order, and stop at the reset.
        let mut out = Vec::new();
        let mut saw_reset = false;
        let mut before = 0usize;
        while let Some(len) = conn.poll_transmit(later, &mut out) {
            let datagram = &out[out.len() - len..];
            if let Ok(packet) = super::wire::decode(datagram)
                && matches!(packet.frame, super::wire::Frame::Reset { .. })
            {
                saw_reset = true;
                break;
            }
            before += 1;
        }
        assert!(
            saw_reset,
            "the reset has to reach the wire: {before} packets went out without it"
        );
        assert!(
            conn.resets.is_empty(),
            "and sending it has to empty the queue, or every pass repeats it"
        );
    }

    /// A datagram that carried stream data makes an acknowledgement pending, and
    /// the next packet this end sends carries it — with nothing else in it.
    ///
    /// §6.3 of the plan asks which packets must trigger an acknowledgement
    /// immediately, and the answer the code gives is **every packet that carried
    /// stream data** (`apply_datagram`'s stream arm), plus every credit change
    /// (`credit_due`). There is no delayed-acknowledgement timer: the pending block
    /// rides the next packet this end builds, and if there is nothing else to say it
    /// goes alone. That is the whole policy, and nothing pinned it — a carrier that
    /// silently stopped scheduling acknowledgements would look healthy from every
    /// counter and stall on the sender's window instead.
    #[test]
    fn a_packet_that_carried_data_is_acknowledged_in_a_packet_of_its_own() {
        let now = Instant::now();
        let mut sender = Conn::new(7, true);
        let mut receiver = Conn::new(7, false);
        let stream = sender.open();
        assert!(sender.write(stream.id, b"hello") > 0);

        // **A fresh end's first packet is its credit**, not its data (§6.3's policy is
        // about packets that *carry stream data*), so take the first one that does.
        let mut buf = Vec::new();
        let mut delivered = false;
        for _ in 0..4 {
            let Some(len) = sender.poll_transmit(now, &mut buf) else {
                break;
            };
            let datagram = buf[buf.len() - len..].to_vec();
            let packet = super::wire::decode(&datagram).unwrap();
            if matches!(packet.frame, super::wire::Frame::Stream { .. }) {
                receiver.on_datagram(now, &datagram).unwrap();
                delivered = true;
                break;
            }
        }
        assert!(delivered, "the write has to reach the wire as stream data");

        // The receiver has nothing of its own to send, so within a few packets
        // whatever it produces is the acknowledgement (a fresh connection also owes
        // its first credit, which is a separate packet and legitimately comes first).
        let mut carried_ack = false;
        let mut unnumbered = false;
        for _ in 0..4 {
            let Some(len) = receiver.poll_transmit(now, &mut buf) else {
                break;
            };
            let packet = super::wire::decode(&buf[buf.len() - len..]).unwrap();
            if packet.ack.is_some() {
                carried_ack = true;
                unnumbered = packet.unnumbered;
            }
        }
        assert!(
            carried_ack,
            "a packet that carried data must be acknowledged"
        );
        assert!(
            unnumbered,
            "an acknowledgement-only packet takes no packet number (§6.3)"
        );
    }

    /// A packet that arrives after this end retired a stream must not bring it
    /// back.
    ///
    /// The third of the plan's eight lifecycle invariants (`HANDOFF.md` §4.120). A
    /// visitor connection that closes leaves bytes on the wire in both directions;
    /// if the peer's late data re-created the stream, the connection would hold a
    /// half-open stream that nothing will ever read, and the window those bytes
    /// spent would stay spent — one dead visitor at a time, until the lane carried
    /// nothing.
    #[test]
    fn a_late_packet_does_not_resurrect_a_retired_stream() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        assert!(link.a.write(stream.id, b"hello") > 0);
        link.pump(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        assert_eq!(accepted.id, stream.id);

        // A gives up on the stream: nothing will ever read it again.
        link.a.close_stream(stream.id);
        let later = now + ABANDON_HOLD + Duration::from_millis(1);
        let _ = link.a.on_timeout(later);
        assert!(
            !link.a.streams.contains_key(&stream.id),
            "past its hold the retired stream is gone"
        );

        // B still has bytes for it on the wire.
        assert!(link.b.write(accepted.id, b"world") > 0);
        let credit_before = link.a.recv_credit;
        link.pump(later);

        assert!(
            !link.a.streams.contains_key(&stream.id),
            "a late packet must not recreate a stream this end retired"
        );
        assert!(
            link.a.recv_credit > credit_before,
            "and the window those bytes spent has to come back: {credit_before} -> {}",
            link.a.recv_credit
        );
    }

    /// The stream-id minter must never hand out an id a live stream still holds.
    ///
    /// The first of the plan's eight lifecycle invariants (`HANDOFF.md` §4.120).
    /// Ids wrap by two, and `open_at` inserted unconditionally — so an id that
    /// wrapped onto a stream still in use would **replace its state**, and the
    /// visitor reading that stream would lose its frontier, its ciphers and its
    /// place in the receive window without any error anywhere.
    #[test]
    fn opening_a_stream_never_hands_out_an_id_a_live_stream_holds() {
        let mut conn = Conn::new(9, true);
        let first = conn.open();
        // Force the minter back onto the id that is still open, which is what a
        // wrap does once the space is exhausted.
        conn.next_stream = first.id;
        let second = conn.open();

        assert_ne!(
            second.id, first.id,
            "the minter handed out id {} while a live stream held it",
            first.id
        );
        assert!(
            conn.streams.contains_key(&first.id),
            "and the stream that held it must still be there"
        );
        assert!(
            conn.streams.contains_key(&second.id),
            "the new stream is in the table too"
        );
    }

    /// The same, for a stream this end **accepted** and then retired.
    ///
    /// The dialer's own ids are protected by the parity test in `apply_datagram`;
    /// the peer's are not, so the code as it stood would take a late packet for a
    /// retired accepted stream and open a *new* one with the same id — a phantom
    /// stream and a phantom accept, for a visitor whose connection had already
    /// closed. Stream ids are monotonic, so an unknown id in the peer's space that
    /// is *below* the highest accepted one cannot be new.
    #[test]
    fn a_late_packet_for_an_accepted_stream_does_not_open_a_new_one() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let first = link.a.open();
        let second = link.a.open();
        assert!(link.a.write(first.id, b"one") > 0);
        assert!(link.a.write(second.id, b"two") > 0);
        link.pump(now);
        let mut accepted = Vec::new();
        while let Some(stream) = link.b.accept() {
            accepted.push(stream.id);
        }
        assert_eq!(accepted, vec![first.id, second.id]);

        // B retires the first one — the visitor's channel closed.
        link.b.close_stream(first.id);
        let later = now + ABANDON_HOLD + Duration::from_millis(1);
        let _ = link.b.on_timeout(later);
        assert!(!link.b.streams.contains_key(&first.id), "it is retired");

        // A has not seen the reset yet and has more bytes for it. Hand B only that
        // datagram, so the test is about a late packet and not about the reset.
        assert!(link.a.write(first.id, b"more") > 0);
        let mut out = Vec::new();
        let mut delivered = false;
        while let Some(len) = link.a.poll_transmit(later, &mut out) {
            let datagram = out[out.len() - len..].to_vec();
            if let Ok(packet) = super::wire::decode(&datagram)
                && matches!(packet.frame, super::wire::Frame::Stream { id, .. } if id == first.id)
            {
                link.b.on_datagram(later, &datagram).unwrap();
                delivered = true;
            }
        }
        assert!(delivered, "the late packet has to reach B");
        assert!(
            link.b.accept().is_none(),
            "a late packet for a retired accepted stream must not open a new one"
        );
        assert!(
            !link.b.streams.contains_key(&first.id),
            "and it must not re-create the stream either"
        );
    }

    /// Credit goes out even when this end has nothing else to say.
    ///
    /// The fourth of the plan's eight lifecycle invariants (`HANDOFF.md` §4.120):
    /// a reader that has drained its buffer must be able to tell the peer it may
    /// send more, and it must be able to do so **while its own writer is silent** —
    /// otherwise a one-way flow stalls the moment the receiver is the quiet end.
    /// `build_packet` checks `credit_due()` first for exactly this reason, and
    /// nothing pinned it.
    #[test]
    fn credit_goes_out_from_an_end_that_has_nothing_else_to_send() {
        // The fourth of the plan's eight lifecycle invariants (`HANDOFF.md` §4.120):
        // a reader that has drained must tell the peer it may send more, and must be
        // able to do so **while its own writer is silent** — otherwise a one-way flow
        // stalls the moment the receiver is the quiet end. `build_packet` checks
        // `credit_due()` before datagrams and streams for exactly this reason.
        //
        // Driven at the policy rather than through a transfer: credit is due only
        // once the reader has consumed a quarter of the window (4 MiB), and pushing
        // that much through a `Link` would take thousands of passes to assert one
        // ordering.
        let now = Instant::now();
        let mut conn = Conn::new(1, false);
        // A fresh end owes the whole window: the peer may not send until it is told.
        assert!(conn.credit_due(), "a fresh end advertises its window");
        let mut out = Vec::new();
        let Some(len) = conn.poll_transmit(now, &mut out) else {
            unreachable!("the first packet an end produces is its credit")
        };
        assert!(
            matches!(
                super::wire::decode(&out[out.len() - len..]).unwrap().frame,
                super::wire::Frame::Credit { .. }
            ),
            "the first packet has to carry the window"
        );
        assert!(!conn.credit_due(), "paying it settles the debt");

        // The invariant proper: the reader drains, and the end owes more — with no
        // stream of its own to carry it.
        conn.advance_read(WINDOW / 4 + 1);
        assert!(
            conn.credit_due(),
            "past a quarter of the window, credit is owed"
        );
        let Some(len) = conn.poll_transmit(now, &mut out) else {
            unreachable!("an end with nothing to send still hands back its credit")
        };
        assert!(
            matches!(
                super::wire::decode(&out[out.len() - len..]).unwrap().frame,
                super::wire::Frame::Credit { .. }
            ),
            "the packet a quiet end produces has to carry the credit"
        );
        assert!(
            !conn.credit_due(),
            "paying it must settle the debt, or every pass would repeat it"
        );
    }

    /// A second opening frame for a stream the peer already has is not a second
    /// stream.
    ///
    /// `apply_datagram` guards this with `if !known`, and *nothing pinned it*
    /// (`HANDOFF.md` §4.120). It is reachable without any pathological peer: an
    /// acknowledgement lost on the way back makes the sender re-send the opening
    /// packet, and a duplicate open that queued a second accept would hand the
    /// visitor's reader a stream the peer never opened — the failure the seam is
    /// supposed to make impossible.
    #[test]
    fn a_second_open_for_a_known_stream_is_not_accepted_twice() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        assert_eq!(link.a.write(stream.id, b"hello"), 5);

        // Take A's datagrams without pumping them, so the same bytes can be
        // delivered twice — which is what a lost acknowledgement produces.
        let mut buf = Vec::new();
        let mut sent = Vec::new();
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            sent.push(buf[buf.len() - len..].to_vec());
        }
        assert!(!sent.is_empty(), "the opening frame has to reach the wire");
        for datagram in &sent {
            link.b.on_datagram(now, datagram).unwrap();
        }
        let Some(first) = link.b.accept() else {
            unreachable!("the opening frame names the stream")
        };
        assert_eq!(first.id, stream.id);

        // The identical bytes again.
        for datagram in &sent {
            link.b.on_datagram(now, datagram).unwrap();
        }
        assert!(
            link.b.accept().is_none(),
            "a repeated opening frame must not queue a second accept for stream {}",
            stream.id
        );
    }

    /// The flight never outgrows the window it is admitted against.
    ///
    /// **The invariant phase 1 did not pin, and the one `write` actually depends
    /// on.** It closed on "a re-sent range is counted once" and "a finished
    /// exchange leaves nothing counted", both true, and neither of them is this:
    /// a repair is exempt from the window by design — a loss could never be
    /// repaired otherwise — so the flight *can* rise past `cwnd`, and until
    /// §4.108 nothing brought it back. Measured then as 32 796 in flight against a
    /// 13 490 window on a live lane, with every writer refused and both ends idle.
    ///
    /// **This is a guard, not a reproduction, and the difference is worth stating.**
    /// It passes on the current code *and* with §4.108's prefix retirement disabled,
    /// so in this in-process harness the cumulative frontier already keeps the table
    /// honest: the live failure needed a listener dropping 97 % of a direction, which
    /// this `Link` cannot produce. What it protects is the invariant itself, so that
    /// a future path adding bytes to the flight without retiring them fails here
    /// rather than on a live lane ten rounds later.
    #[test]
    fn the_flight_never_outgrows_the_window() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(256 * 1024);

        // A transfer that keeps the window full and keeps losing datagrams: the
        // repair of a loss and the acknowledgement of new data are both in the
        // table at once, which is the state `flight > cwnd` was measured in.
        let mut clock = now;
        let mut written = 0usize;
        let mut accepted: Option<u32> = None;
        let mut dropped = 0usize;
        for step in 0..120 {
            clock += Duration::from_millis(5);
            if step % 4 == 1 {
                link.pump_dropping_one(clock);
                dropped += 1;
            } else {
                link.pump(clock);
            }
            if accepted.is_none() {
                accepted = link.b.accept().map(|stream| stream.id);
            }
            if let Some(id) = accepted {
                let _ = take(&mut link.b, id);
            }
            if written < bytes.len() {
                written += link.a.write(stream.id, &bytes[written..]);
            }
            let (cwnd, flight, _, _) = link.a.window_state();
            assert!(
                flight <= cwnd,
                "step {step} ({dropped} dropped): the flight {flight} outgrew the window \
                 {cwnd}. write() admits against cwnd - flight, so this is the deadlock of \
                 §4.107 with every writer refused and nothing draining the table"
            );
        }
        assert!(
            written > 0 && dropped > 10,
            "the transfer has to run: {written}"
        );
    }

    /// A byte prefix frees the buffer with no packet record at all.
    ///
    /// This is the property the whole block exists for: the peer's cumulative
    /// *number* frontier can be stopped for good by one lost packet (a
    /// retransmission takes a new number, so the hole never fills), but the
    /// bytes it holds keep growing. A frontier keyed on bytes therefore frees a
    /// stream whose packet records are all gone, which is exactly the state
    /// that used to grow to gigabytes.
    #[test]
    fn a_byte_prefix_frees_the_buffer_without_any_packet_record() {
        let now = Instant::now();
        let mut conn = Conn::new(9, true);
        let stream = conn.open();
        let bytes = payload(4000);
        assert_eq!(conn.write(stream.id, &bytes), bytes.len());
        assert_eq!(usize::try_from(conn.buffered).unwrap(), bytes.len());

        // Nothing was ever sent, so there is no flight entry to retire: only
        // the prefix can move the frontier.
        let block = wire::Ack {
            largest: 0,
            below: 0,
            bits: 0,
            stream: stream.id,
            prefix: 4000,
        };
        conn.apply_ack(now, block, false);

        let state = conn.streams.get(&stream.id).unwrap();
        assert_eq!(state.send.held(), 0, "the buffer is empty");
        assert_eq!(conn.buffered, 0, "and the connection's total agrees");
    }

    /// The buffer a stream's bytes are held in empties once the peer has
    /// acknowledged them — including a range it acknowledged only after
    /// declaring it lost.
    ///
    /// The regression this pins is the resident growth: a lost packet's record
    /// was deleted when the loss was declared, so the late acknowledgement had
    /// nowhere to land and the stream's frontier stopped at that hole. Nothing
    /// compacted the buffer again for the life of the connection, and it grew
    /// with everything written.
    #[test]
    fn a_late_acknowledgement_still_frees_the_send_buffer() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(8000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump_dropping_one(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        let mut out = take(&mut link.b, accepted.id);

        let mut clock = now;
        for _ in 0..8 {
            clock += Duration::from_millis(10);
            link.pump(clock);
            out.extend_from_slice(&take(&mut link.b, accepted.id));
            if out.len() == bytes.len() {
                break;
            }
        }
        assert_eq!(out, bytes, "the stream is whole");
        let state = link.a.streams.get(&stream.id).unwrap();
        assert_eq!(
            state.send.held(),
            0,
            "every acknowledged byte left the buffer"
        );
    }

    /// Two ranges dropped in one window are both retransmitted.
    ///
    /// The rewind used to assign each lost offset in turn, leaving the send
    /// point at the highest one: every range below the last hole was never
    /// re-sent, so the peer's frontier stopped there and the hole was permanent.
    #[test]
    fn two_dropped_datagrams_in_one_window_are_both_recovered() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(12_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        // The second and fourth datagrams of the transfer fall on the floor;
        // the third and fifth put three later packets behind each of them, so a
        // single acknowledgement declares both lost.
        let mut buf = Vec::new();
        let mut seen = 0usize;
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            seen += 1;
            if seen == 2 || seen == 4 {
                continue;
            }
            link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
        }
        for datagram in std::mem::take(&mut link.a_to_b) {
            link.b.on_datagram(now, &datagram).unwrap();
        }

        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        let mut out = take(&mut link.b, accepted.id);
        let mut clock = now;
        for _ in 0..8 {
            clock += Duration::from_millis(10);
            link.pump(clock);
            out.extend_from_slice(&take(&mut link.b, accepted.id));
            if out.len() == bytes.len() {
                break;
            }
        }
        assert_eq!(out, bytes, "both holes are repaired");
    }

    /// Two streams share one connection, and a loss on one of them is not a
    /// stall on the other: that is the property the whole engine exists for.
    #[test]
    fn a_lost_datagram_holds_one_stream_and_not_the_other() {
        let now = Instant::now();
        let mut link = Link::new();
        let bulk = link.a.open();
        let interactive = link.a.open();
        let big = payload(6000);
        link.a.write(bulk.id, &big);
        link.a.write(interactive.id, b"ping");

        link.pump_dropping_one(now);

        // The interactive stream's first packet may be the one that was dropped,
        // so the assertion is about what the cursor does afterwards: the bulk
        // stream still has bytes to send, and it does not wait for the hole.
        let later = now + Duration::from_millis(10);
        link.pump(later);
        // Both streams are known to the receiver; the order they are accepted in
        // is the order their first bytes arrived, which the round-robin cursor
        // decides and no test should pin.
        let first = link.b.accept().unwrap();
        let second = link.b.accept().unwrap();
        let accepted: Vec<u32> = vec![first.id, second.id];
        assert!(accepted.contains(&bulk.id), "the bulk stream must be there");
        assert!(
            accepted.contains(&interactive.id),
            "the interactive stream must be there too"
        );
    }

    /// A stream with bytes to send is found by the walk **whatever the cursor
    /// says**.
    ///
    /// The bench produced a refusal line that contradicted itself — `visited=2
    /// open=2 queued=1 … reason="no eligible stream"` with a witness of
    /// `(5, open=true, has_unsent=true, next=934 070 444, written=934 095 627)`
    /// and a cursor of 11, i.e. an id no longer in the table. This pins the
    /// invariant the scheduler is supposed to have: a cursor past every live id
    /// wraps, and a stream owing bytes is never skipped.
    #[test]
    fn a_stream_owing_bytes_is_found_whatever_the_cursor_says() {
        let mut conn = Conn::new(9, false);
        let older = conn.open();
        let younger = conn.open();
        // Retire the older one, so the surviving id is not where the cursor sits.
        conn.streams.remove(&older.id);
        assert_eq!(conn.write(younger.id, b"payload"), 7);

        // The cursor is advanced the way a served packet does it, landing past
        // every live id — the state the bench logged.
        conn.cursor = younger.id + 2;
        let Some(pick) = conn.next_stream_range() else {
            unreachable!("a stream owing bytes must be found")
        };
        assert_eq!(pick.id, younger.id, "the walk wrapped to the only stream");
        assert_eq!(pick.offset, 0);
        assert_eq!(pick.len, 7);

        // And the same after the cursor has been moved by a real serve.
        conn.advance_cursor();
        assert!(
            conn.next_stream_range().is_some(),
            "still found after the cursor moved: cursor={}",
            conn.cursor
        );
    }

    /// Every byte that crosses this lane is counted once, and what crossed twice is
    /// counted as what it is.
    ///
    /// The ledger exists to answer one question a lossy path asks and the old window
    /// line could not: **what share of what left was a byte already sent once**. On a
    /// clean path the answer must be zero, and the accounting must close — bytes sent,
    /// bytes confirmed, and no repair among them.
    #[test]
    fn a_clean_transfer_sends_each_byte_once_and_counts_it_once() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(6_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());
        link.pump(now);

        let ledger = link.a.take_ledger();
        let len = u64::try_from(bytes.len()).unwrap();
        assert_eq!(ledger.sent_bytes, len, "every byte went out once");
        assert_eq!(ledger.acked_bytes, len, "and every byte was confirmed once");
        assert_eq!(ledger.sent_repair_bytes, 0, "nothing had to be re-sent");
        assert_eq!(
            ledger.acked_late, 0,
            "nothing arrived after being declared lost"
        );
        assert!(ledger.grown > 0, "a clean path does grow the window");
    }

    /// A repaired byte crosses the wire twice and is still acked once.
    ///
    /// This is the shape the lossy cell is made of, in one stream: the dropped range
    /// leaves the flight, its repair re-enters it, so `sent` exceeds `acked` by
    /// exactly the price of recovery. If the two were equal on a losing path, either
    /// nothing was lost or the repair was never sent.
    #[test]
    fn a_repaired_byte_crosses_the_wire_twice_and_is_counted_once() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(8 * 1024);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        // Take the first datagram that carries stream data off the wire.
        let mut dropped = 0;
        for _ in 0..8 {
            let mut buf = Vec::new();
            let Some(len) = link.a.poll_transmit(now, &mut buf) else {
                break;
            };
            let datagram = buf[buf.len() - len..].to_vec();
            let packet = super::wire::decode(&datagram).unwrap();
            let carries = matches!(
                packet.frame,
                super::wire::Frame::Stream { id, .. } if id == stream.id
            );
            if carries && dropped == 0 {
                dropped += 1;
                continue;
            }
            link.a_to_b.push_back(datagram);
        }
        assert_eq!(dropped, 1, "one stream packet was dropped");

        // Let the lane find the hole and repair it.
        for step in 0..8 {
            link.pump(now + Duration::from_millis(step));
        }

        let ledger = link.a.take_ledger();
        let len = u64::try_from(bytes.len()).unwrap();
        assert_eq!(ledger.acked_bytes, len, "the peer ended up with every byte");
        assert!(
            ledger.sent_repair_bytes > 0,
            "the dropped range went out a second time: {ledger}"
        );
        assert!(
            ledger.sent_bytes > ledger.acked_bytes,
            "the wire carried more than the payload, which is the price of recovery: {ledger}"
        );
        assert!(
            ledger.lost_by_threshold + ledger.lost_by_timer > 0,
            "the loss was named by evidence, not guessed: {ledger}"
        );
    }

    /// An acknowledgement that arrives after the loss was declared is counted as one.
    ///
    /// A declaration the path did not support costs twice: the original arrived, and
    /// the lane had already queued and sent its range again. That is why `late` is its
    /// own number — it is the size of the spurious-loss problem, and a lane whose
    /// `late` is a large share of `ack` is over-declaring, not under-repairing.
    #[test]
    fn an_acknowledgement_after_a_loss_declaration_is_counted_as_late() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(1_000);
        assert!(link.a.write(stream.id, &bytes) > 0);

        // Hold the first stream datagram back instead of delivering it.
        let mut held = None;
        for _ in 0..4 {
            let mut buf = Vec::new();
            let Some(len) = link.a.poll_transmit(now, &mut buf) else {
                break;
            };
            let datagram = buf[buf.len() - len..].to_vec();
            if matches!(
                super::wire::decode(&datagram).unwrap().frame,
                super::wire::Frame::Stream { .. }
            ) {
                held = Some(datagram);
                break;
            }
        }
        let Some(held) = held else {
            unreachable!("the write reaches the wire as a stream packet")
        };

        // The timer finds it stale, so this is a declaration rather than a probe: the
        // record stays in the table, which is what a late acknowledgement lands on.
        let later = now + Duration::from_secs(2);
        let (_, lost) = link.a.on_timeout(later);
        assert_eq!(lost, 1, "the timer named the unacknowledged range lost");

        // Now the original arrives, after the lane already decided it would not.
        link.b.on_datagram(later, &held).unwrap();
        let mut buf = Vec::new();
        while let Some(len) = link.b.poll_transmit(later, &mut buf) {
            link.b_to_a.push_back(buf[buf.len() - len..].to_vec());
        }
        while let Some(datagram) = link.b_to_a.pop_front() {
            link.a.on_datagram(later, &datagram).unwrap();
        }

        let ledger = link.a.take_ledger();
        assert!(
            ledger.acked_late > 0,
            "the late acknowledgement was counted as late: {ledger}"
        );
    }

    /// The byte prefix retires records, and the window does not grow from it.
    ///
    /// The prefix is the second confirmation path, and today it is *not* part of
    /// `Acked::bytes`, so those bytes free the flight without buying window. Whether
    /// that is right is a measurement — the ledger reports the two paths apart so the
    /// question can be answered — but the behaviour must not change by accident, so it
    /// is pinned here. The block in this test says nothing at all, which is what a
    /// hole in a *different* stream produces.
    #[test]
    fn a_prefix_confirmation_retires_bytes_without_growing_the_window() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(2_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());
        // Send everything and deliver nothing: the records are in flight.
        let mut buf = Vec::new();
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
        }
        let before = link.a.cwnd;

        let len = u64::try_from(bytes.len()).unwrap();
        // The block a real peer sends when a hole in *another* stream pins the
        // cumulative frontier at zero while a thousand later packets have arrived:
        // the bitmap reaches sixty-four numbers below `largest`, so this stream's
        // records are outside it and only the prefix can retire them. `below = 0`
        // is what a hole at the start of the number space looks like.
        link.a.apply_ack(
            now,
            super::wire::Ack {
                largest: 1_000,
                below: 0,
                bits: 0,
                stream: stream.id,
                prefix: len,
            },
            false,
        );

        let ledger = link.a.take_ledger();
        assert_eq!(
            ledger.acked_by_prefix, len,
            "the prefix retired every byte: {ledger}"
        );
        assert_eq!(ledger.acked_by_number, 0, "the block named none of them");
        assert_eq!(
            ledger.acked_bytes, len,
            "and the two paths together counted each byte once"
        );
        assert_eq!(ledger.grown, 0, "the prefix path does not grow the window");
        assert_eq!(link.a.cwnd, before, "the window is exactly where it was");
    }

    /// A prefix that covers a declared loss confirms those bytes, and says they were late.
    ///
    /// Retiring a record that had already been declared lost frees nothing — it left
    /// the flight at the declaration — so a ledger built on the freed count alone
    /// would under-count what the peer confirmed by exactly the bytes this end had
    /// given up on, and the duplicate share built on it would look larger than it is.
    #[test]
    fn a_prefix_over_a_declared_loss_counts_those_bytes_as_late() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(2_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());
        let mut buf = Vec::new();
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
        }
        // The timer gives up on both records; nothing is pumped, so nothing is
        // re-sent and the ledger's send side stays where it was.
        let (_, declared) = link.a.on_timeout(now + Duration::from_secs(2));
        assert_eq!(declared, 2, "both records are stale");

        let len = u64::try_from(bytes.len()).unwrap();
        link.a.apply_ack(
            now,
            super::wire::Ack {
                largest: 1_000,
                below: 0,
                bits: 0,
                stream: stream.id,
                prefix: len,
            },
            false,
        );

        let ledger = link.a.take_ledger();
        assert_eq!(
            ledger.acked_by_prefix, len,
            "the prefix confirmed every byte"
        );
        assert_eq!(ledger.acked_by_number, 0, "the block named none of them");
        assert_eq!(
            ledger.acked_late, len,
            "and every byte had been given up on before it arrived: {ledger}"
        );
        assert_eq!(ledger.grown, 0, "the prefix path does not grow the window");
    }

    /// A repair is gated once the re-sent bytes in flight are at the window.
    ///
    /// The rule the scheduler now holds (`HANDOFF.md` §4.187): new data is bounded by the
    /// writer against `cwnd − flight`, but a repair is not written at all — it is
    /// scheduled from the recovery table — so before this rule a lane could put an
    /// unbounded amount of re-sent bytes in flight. One lane measured 1 909 MB posted
    /// against 226 MB delivered.
    ///
    /// Built rather than driven: on a merely lossy link the budget never fills, because
    /// declaring a loss frees exactly the bytes its repair will take, so the gate is a
    /// safety valve for the state where re-sends dominate — which is what the test
    /// states directly. The invariant it protects is checked by
    /// `a_repair_never_pushes_the_re_sent_flight_past_the_window` in the same file.
    #[test]
    fn a_repair_is_gated_when_the_re_sent_flight_is_at_the_window() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(4 * 1024);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());
        let mut buf = Vec::new();
        while link.a.poll_transmit(now, &mut buf).is_some() {}
        let (_, lost) = link.a.on_timeout(now + Duration::from_secs(2));
        assert!(lost > 0, "the timer queued a repair");

        // Borrow the whole window's worth of repair budget the smallest way the counter
        // allows: records this test invents, so the gate is what is under test and not a
        // loss pattern that happens to fill it.
        let mut number = 1_000u32;
        while link.a.recovery.repair_flight() < link.a.cwnd {
            link.a.recovery.on_sent(
                now,
                number,
                Some(SentRange {
                    stream: stream.id,
                    offset: 0,
                    len: u16::try_from(1_349).unwrap(),
                    fin: false,
                }),
                false,
                true,
            );
            number += 1;
        }
        assert_eq!(
            link.a.recovery.repair_flight(),
            link.a.cwnd,
            "the budget is exactly full"
        );

        let pick = link.a.next_stream_range();
        assert!(
            pick.is_none_or(|pick| !pick.repair),
            "no repair joins a full budget"
        );
        assert!(
            link.a.ledger.repairs_gated > 0,
            "and the refusal is counted, so a lane that stops repairing is not mistaken              for one with nothing to repair"
        );
    }

    /// A 5 %-lossy memory link delivers everything, and it does **not** storm.
    ///
    /// Two facts, and the second is the finding. A deterministic link that drops one
    /// datagram in twenty still delivers every byte, with threshold declarations
    /// doing the work and the timer never firing — because a memory link has no
    /// queueing delay, so no acknowledgement is ever late. The regime §4.182 measured
    /// (a window pinned at its initial value while most of the wire was repairs) is
    /// therefore **not reproducible here**: it needs the delay a real qdisc adds, and
    /// the reproduction has to be at the bench. This test keeps the memory link's
    /// shape pinned so a change that makes *it* storm is caught immediately.
    #[test]
    fn five_percent_loss_on_a_fake_link_delivers_without_storming() {
        let start = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(128 * 1024);
        let mut written = 0;
        let mut step = 0u64;
        while written < bytes.len() && step < 400 {
            written += link.a.write(stream.id, &bytes[written..]);
            link.pump_lossy(start + Duration::from_millis(step), 20);
            step += 1;
        }
        // Then a clean path, so the assertion is about delivery and not about luck. The
        // timer is driven here because a real pump drives it: without it this harness
        // only ever repairs on the three-packet threshold, so a hole with no later
        // packet behind it waits for a timeout that never comes — and any change in the
        // datagram sequence is then enough to make the transfer stall short of the end.
        // That is a harness gap, not a carrier one: the earlier pass was luck.
        for extra in 0..400 {
            let at = start + Duration::from_millis(step + extra);
            let _ = link.a.on_timeout(at);
            let _ = link.b.on_timeout(at);
            link.pump(at);
        }
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..64 {
            got.extend(take(&mut link.b, stream.id));
        }
        let ledger = link.a.take_ledger();
        assert_eq!(
            got.len(),
            bytes.len(),
            "every byte arrives even when one in twenty is dropped: {ledger}"
        );
        assert!(
            ledger.lost_by_threshold > 0,
            "the drops are found by later packets, not by the clock: {ledger}"
        );
        assert_eq!(
            ledger.lost_by_timer, 0,
            "a link with no queueing delay never makes an acknowledgement late: {ledger}"
        );
        assert!(
            ledger.sent_repair_bytes * 5 < ledger.sent_bytes,
            "and the re-sent share stays small, which is what a storm is not: {ledger}"
        );
    }

    /// A range that arrives twice is counted as a duplicate, not as delivery.
    ///
    /// The counter exists for §4.183's question: whether the carrier's 70–90 %
    /// duplicate share on a lossy path is re-sending bytes the peer already holds.
    /// The stream drops a repeat by offset, so without this the second copy is
    /// invisible — delivered once, counted twice is the mistake this forbids.
    #[test]
    fn a_duplicate_range_is_counted_as_a_duplicate() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(3_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        // Deliver everything once, keeping the first stream packet to send again.
        let mut repeat = None;
        for _ in 0..8 {
            let mut buf = Vec::new();
            let Some(len) = link.a.poll_transmit(now, &mut buf) else {
                break;
            };
            let datagram = buf[buf.len() - len..].to_vec();
            let carried = match super::wire::decode(&datagram).unwrap().frame {
                super::wire::Frame::Stream { id, data, .. } if id == stream.id => Some(data.len()),
                _ => None,
            };
            link.b.on_datagram(now, &datagram).unwrap();
            if repeat.is_none()
                && let Some(size) = carried
            {
                repeat = Some((datagram, size));
            }
        }
        let first = link.b.take_ledger();
        assert_eq!(
            first.recv_duplicate_bytes, 0,
            "a clean first delivery has no duplicates: {first}"
        );

        let Some((datagram, size)) = repeat else {
            unreachable!("the write reaches the wire as a stream packet")
        };
        link.b.on_datagram(now, &datagram).unwrap();
        let again = link.b.take_ledger();
        let size = u64::try_from(size).unwrap();
        assert_eq!(again.recv_bytes, size, "the repeat carried those bytes");
        assert_eq!(
            again.recv_duplicate_bytes, size,
            "and every one of them was already held: {again}"
        );
        assert_eq!(again.acked_bytes, 0, "so none of it is new delivery");
    }

    /// The accepting end counts the dialing end's first numbered packet.
    ///
    /// **Measured, and the reason this test exists**: across three runs and forty
    /// lanes, every accepting end's `first_seen` was **1** and never 0 — the dialing
    /// end's number 0 was posted (`first_posted=0`) and never seen — and every
    /// dialing end read `below=0` for its whole life, so the peer's cumulative
    /// frontier never advanced past the hole. That pins the *sender's* retirement to
    /// the 64-number bitmap and the per-stream byte prefix for the life of every
    /// lane, which is exactly the shape the lossy cell shows (`HANDOFF.md` §4.181,
    /// §4.182).
    ///
    /// This is the smallest reproduction: two connections, the dialer's first two
    /// datagrams, no socket, no dispatcher. If it passes, the hole is made outside
    /// this seam — and the next place to look is the listener's admission rule,
    /// which drops an unknown connection's datagram unless it is a stream frame at
    /// offset zero *carrying bytes*.
    #[test]
    fn the_accepting_end_counts_the_dialers_first_numbered_packet() {
        let now = Instant::now();
        let mut dialer = Conn::new(11, true);
        let stream = dialer.open();
        assert!(dialer.write(stream.id, b"selector") > 0);

        let mut acceptor = Conn::new(11, false);
        let mut buf = Vec::new();
        let mut fed = 0;
        for _ in 0..2 {
            let Some(len) = dialer.poll_transmit(now, &mut buf) else {
                break;
            };
            let datagram = buf[buf.len() - len..].to_vec();
            let _ = acceptor.on_datagram(now, &datagram);
            fed += 1;
        }
        assert_eq!(fed, 2, "a fresh end owes a credit packet and an opening");
        assert_eq!(
            acceptor.first_numbers().1.map(|(number, _)| number),
            Some(0),
            "the first numbered packet the acceptor sees is the dialer's number 0"
        );
        assert_eq!(acceptor.own_below(), 1, "and its frontier advances past it");
    }

    /// A finished flow stops growing, and stops accumulating.
    ///
    /// The window line's maximum is a maximum over the stage, and the stage outlives
    /// the flow: §4.174 read a value taken in the bulk stream's last second as a
    /// steady-state plateau, and it was neither steady nor a plateau. This pins the
    /// fact that makes that reading wrong — once the peer has everything, the next
    /// second adds nothing to either side.
    #[test]
    fn a_finished_flow_adds_nothing_to_the_ledger() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(6_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());
        link.pump(now);
        let first = link.a.take_ledger();
        assert!(first.acked_bytes > 0);

        let (flight, buffered, _) = link.a.flight_and_buffered();
        assert_eq!(flight, 0, "nothing is left in flight");
        assert_eq!(buffered, 0, "and nothing is left buffered");

        link.pump(now);
        let after = link.a.take_ledger();
        assert_eq!(after.grown, 0, "the window does not keep growing");
        assert_eq!(after.acked_bytes, 0, "and no byte is confirmed twice");
        assert_eq!(
            after.sent_repair_bytes, 0,
            "nothing is re-sent to a quiet peer"
        );
    }

    /// A refused packet must not consume a packet number.
    ///
    /// A number that never reaches the wire is a hole in the peer's cumulative
    /// frontier, and a retransmission carries a *new* number, so the hole can
    /// never be filled: every later acknowledgement then re-declares everything
    /// above it lost. One refused packet therefore becomes a repair loop that
    /// spends the window, which is what the sealed striped path measured as
    /// `retransmits=1532` with nothing refused and no byte owed.
    ///
    /// The refusal here is the sealing one — a stream marked sealed with no key
    /// is the state `payload_plan` exists to refuse — and the assertion is that
    /// the connection's own numbering did not move.
    /// A datagram crosses the wire without a packet number and without a flight entry.
    ///
    /// The three properties §4.71 names, pinned from the receiving end: the peer counts
    /// the datagram, the sender's flight stays at zero (nothing to retire, nothing to
    /// repair), and the packet carries a number it does not consume — so the *next*
    /// stream packet's number is the one after the last stream packet, not after this
    /// one.
    #[test]
    fn a_datagram_crosses_without_touching_the_flight() {
        let mut a = Conn::new(1, true);
        let mut b = Conn::new(1, false);
        let mut buf = Vec::new();
        let stream = a.open().id;
        a.write(stream, &[9u8; 100]);
        // One stream packet goes out first, so the flight is non-zero to begin with.
        let first = a.poll_transmit(Instant::now(), &mut buf).unwrap();
        let mut moving = VecDeque::from([buf[..first].to_vec()]);
        while let Some(packet) = moving.pop_front() {
            b.on_datagram(Instant::now(), &packet).unwrap();
        }
        // Now a datagram, queued at the layer above. Poll until the pass that carries it:
        // the connection may first owe its peer a credit update or a bare acknowledgement,
        // and neither is this test's subject.
        assert!(a.queue_datagram(7, Bytes::from_static(b"datagram")));
        let number_before_datagram = a.recovery.last_number();
        // What the table held *before* the datagram, so the assertion is "the datagram
        // added nothing" rather than "the table has n entries", which depends on which
        // control frame the pass happened to build first.
        let table_before = a.recovery.table_stats().0;
        for _ in 0..8 {
            let Some(encoded) = a.poll_transmit(Instant::now(), &mut buf) else {
                break;
            };
            // The pass appends to the run, so the packet it built is the last `encoded`
            // bytes of the buffer rather than the whole of it.
            let packet = &buf[buf.len() - encoded..];
            let is_datagram = matches!(
                wire::decode(packet).map(|decoded| decoded.frame),
                Ok(wire::Frame::Datagram { .. })
            );
            b.on_datagram(Instant::now(), packet).unwrap();
            if is_datagram {
                assert_eq!(
                    a.recovery.last_number(),
                    number_before_datagram,
                    "a datagram must not consume a packet number"
                );
                break;
            }
        }
        assert_eq!(a.datagram_counts().0, 1, "one datagram reached the wire");
        assert_eq!(b.datagram_counts().1, 1, "the peer counted one datagram");
        assert_eq!(b.datagram_counts().0, 0);
        // The datagram added nothing to the flight: nothing to retire, nothing to repair.
        assert_eq!(a.recovery.table_stats().0, table_before);
    }

    /// An abandoned stream gives its bytes back after the hold, and not before.
    ///
    /// Its bytes stay in the connection's `buffered` total while it waits for an
    /// acknowledgement, and `write`'s buffer ceiling is exactly that number — so a
    /// stream whose peer never acknowledges refuses every later stream on the lane.
    /// Measured: the bulk spine went from 7.7 to 0.21 Gbit/s after the first lossy
    /// stage and never recovered (HANDOFF §4.50).
    #[test]
    fn an_abandoned_stream_releases_its_buffer_after_the_hold() {
        let mut conn = Conn::new(3, true);
        let stream = conn.open().id;
        assert_eq!(conn.write(stream, &[7u8; 4096]), 4096, "the write lands");
        let room = conn.budget().2;
        conn.close_stream(stream);
        let now = Instant::now();
        // One second in: the graceful close is still waiting for its ack.
        conn.on_timeout(now + Duration::from_secs(1));
        assert_eq!(
            conn.budget().2,
            room,
            "an abandoned stream must keep waiting inside the hold"
        );
        // Past the hold: the bytes come back and the ceiling is the lane's again.
        conn.on_timeout(now + ABANDON_HOLD + Duration::from_secs(1));
        assert_eq!(
            conn.budget().2,
            room + 4096,
            "the hold must release exactly what the stream held"
        );
    }

    #[test]
    fn a_refused_packet_does_not_consume_a_packet_number() {
        let now = Instant::now();
        let mut conn = Conn::new(1, true);
        let stream = conn.open();
        assert_eq!(conn.write(stream.id, b"handshake"), 9);

        // Sealed with no cipher: the one state that must never reach the wire in
        // the clear, and therefore the one that is refused.
        let state = conn.streams.get_mut(&stream.id).unwrap();
        state.sealed = true;
        assert!(state.send_cipher.is_none());

        let before = conn.recovery.last_number();
        let mut out = Vec::new();
        assert!(
            conn.transmit_stream(now, &mut out, None).is_none(),
            "a sealed stream without a key is refused"
        );
        assert_eq!(
            conn.recovery.last_number(),
            before,
            "the refusal took no packet number, so the peer has no hole to wait for"
        );
        assert!(out.is_empty(), "and nothing half-built reached the run");
        // The bytes are still owed, so the next pass can still carry them once a
        // key exists.
        assert_eq!(conn.streams.get(&stream.id).unwrap().send.held(), 9);
    }

    /// An opening stream finds room in the connection's budget even when a bulk
    /// stream has taken all of it.
    ///
    /// This is the tail the `capacity` ramp measured: the interactive probe's
    /// p50 stayed at two milliseconds while its p99 read seconds, because the
    /// stream it opened behind a bulk transfer could not write its handshake at
    /// all. The reserve is what makes that impossible, and it is taken *inside*
    /// the peer's credit so no frame is ever over the advertised window.
    #[test]
    fn an_opening_stream_finds_room_a_bulk_stream_holds() {
        let mut conn = Conn::new(9, true);
        // A window wide enough for the credit to be the binding bound.
        conn.cwnd = WINDOW;

        let bulk = conn.open();
        let chunk = payload(usize::try_from(OPEN_RESERVE).unwrap() + 100);
        assert_eq!(conn.write(bulk.id, &chunk), chunk.len());

        // Fill the rest: the bulk stream may not take the reserve.
        let filler = payload(4096);
        let mut written = chunk.len();
        loop {
            let took = conn.write(bulk.id, &filler);
            if took == 0 {
                break;
            }
            written += took;
        }
        let reserved = conn.send_credit.saturating_sub(conn.written);
        assert!(
            reserved > 0 && reserved <= OPEN_RESERVE,
            "the bulk stream stops a reserve short of the credit, left {reserved}"
        );

        // A stream that opens now can still write its handshake.
        let fresh = conn.open();
        assert_eq!(
            conn.write(fresh.id, b"handshake"),
            9,
            "the reserve belongs to the stream that is opening"
        );
        assert!(written > chunk.len(), "the bulk stream really did fill it");
    }

    /// A loss is re-sent as **the hole**, not as everything after it.
    ///
    /// This is the bandwidth a rewind cost: a stream that lost one packet
    /// re-sent every byte from that offset to the tail, so a lossy path paid the
    /// bandwidth-delay product in duplicates per loss event. The assertion is on
    /// the wire — the next stream frame's offset is the dropped one's, and its
    /// length is the dropped range's — because that is what the peer and the
    /// path see.
    #[test]
    fn a_loss_re_sends_the_hole_and_not_what_follows_it() {
        let now = Instant::now();
        let mut link = Link::new();
        link.pump(now);
        let stream = link.a.open();
        let bytes = payload(12_000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        // Send the stream, dropping one datagram in the middle and remembering
        // exactly what it carried.
        let mut buf = Vec::new();
        let mut seen = 0usize;
        let mut dropped: Option<(u64, usize)> = None;
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            let packet = &buf[buf.len() - len..];
            if let Ok(wire::Packet {
                frame: Frame::Stream { offset, data, .. },
                ..
            }) = wire::decode(packet)
            {
                seen += 1;
                if seen == 2 {
                    dropped = Some((offset, data.len()));
                    continue;
                }
            }
            link.a_to_b.push_back(packet.to_vec());
        }
        let Some((dropped_at, dropped_len)) = dropped else {
            unreachable!("a middle datagram is dropped")
        };
        assert!(
            seen >= 4,
            "the transfer must span several packets, got {seen}"
        );

        for datagram in std::mem::take(&mut link.a_to_b) {
            link.b.on_datagram(now, &datagram).unwrap();
        }
        // The acknowledgements that read the hole as a loss.
        let mut acked = Vec::new();
        while let Some(len) = link.b.poll_transmit(now, &mut buf) {
            acked.push(buf[buf.len() - len..].to_vec());
        }
        for datagram in acked {
            link.a.on_datagram(now, &datagram).unwrap();
        }

        // Everything the connection now puts on the wire for streams: with a
        // repair it is the hole and nothing else, with a rewind it was every
        // byte from the hole to the tail.
        let mut resent = 0usize;
        let mut first = None;
        while let Some(len) = link.a.poll_transmit(now, &mut buf) {
            if let Ok(wire::Packet {
                frame: Frame::Stream { offset, data, .. },
                ..
            }) = wire::decode(&buf[buf.len() - len..])
            {
                first.get_or_insert((offset, data.len()));
                resent += data.len();
            }
        }
        assert_eq!(
            first,
            Some((dropped_at, dropped_len)),
            "the first frame re-sent is the hole itself"
        );
        assert_eq!(
            resent, dropped_len,
            "one lost datagram costs one datagram of re-send, not the rest of the stream"
        );
    }

    /// A lost opening packet costs the cheap timer, not the burst's, and the
    /// simulated clock says exactly how long the peer waits.
    ///
    /// This is the latency the carrier was losing to one constant: an opening is
    /// a *small flight* (one handshake packet in the air), so a lost one is
    /// re-sent after `MIN_RTO_SMALL_FLIGHT` rather than after the wide floor.
    /// The state machine owns no clock, which is what makes the assertion exact:
    /// the loop below advances a simulated millisecond at a time and stops at the
    /// first one where the peer can read the bytes.
    #[test]
    fn a_lost_opening_packet_is_re_sent_within_the_cheap_timer() {
        let start = Instant::now();
        let mut link = Link::new();
        // A lane that has carried something: the opening below then has a
        // round-trip sample, which is the case a stream opened on a live lane
        // hits (a fresh lane pays `INITIAL_RTO` instead; see the unit test).
        let warm = link.a.open();
        assert_eq!(link.a.write(warm.id, b"warm"), 4);
        link.pump(start);
        let warmed = link.b.accept().unwrap();
        assert_eq!(take(&mut link.b, warmed.id), b"warm", "the warm-up lands");

        let opening = link.a.open();
        assert_eq!(link.a.write(opening.id, b"ping"), 4);
        let mut buf = Vec::new();
        let mut dropped = false;
        while let Some(len) = link.a.poll_transmit(start, &mut buf) {
            if !dropped {
                dropped = true; // the opening packet, on the floor
                continue;
            }
            link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
        }
        assert!(dropped, "the opening packet must have been sent");

        let deadline = link.a.poll_timeout().unwrap();
        assert_eq!(
            deadline.saturating_duration_since(start),
            crate::transport::mhd::recovery::MIN_RTO_SMALL_FLIGHT,
            "one packet in flight is a handshake, not a burst"
        );

        let mut waited = Duration::ZERO;
        let mut answered = false;
        let mut peer = None;
        while waited <= Duration::from_millis(250) {
            let now = start + waited;
            if link.a.poll_timeout().is_some_and(|at| at <= now) {
                let (expiry, _) = link.a.on_timeout(now);
                assert_eq!(
                    expiry,
                    Expiry::Loss,
                    "a stale packet is a loss, not a probe"
                );
                while let Some(len) = link.a.poll_transmit(now, &mut buf) {
                    link.a_to_b.push_back(buf[buf.len() - len..].to_vec());
                }
                for datagram in std::mem::take(&mut link.a_to_b) {
                    link.b.on_datagram(now, &datagram).unwrap();
                }
            }
            // The stream is accepted once and read from then on: `accept` pops
            // it from the queue, so it will not be handed out twice.
            if peer.is_none() {
                peer = link.b.accept().map(|handle| handle.id);
            }
            if let Some(id) = peer
                && !take(&mut link.b, id).is_empty()
            {
                answered = true;
                break;
            }
            waited += Duration::from_millis(1);
        }
        assert!(answered, "the opening must arrive after its re-send");
        assert!(
            waited <= Duration::from_millis(30),
            "a lost opening waits the cheap timer, took {waited:?}"
        );
    }
}
