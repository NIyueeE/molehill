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
/// The two wait for different things: the congestion window waits for an
/// acknowledgement, the peer's credit waits for its *reader*. A stream that
/// stalls with the credit refusing is waiting on the far end's application, and
/// nothing this connection sends will move it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// This end's congestion window had no room in flight.
    Window,
    /// The peer's advertised credit had no room for new bytes.
    Credit,
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
    /// Datagrams this connection has posted and read, cumulative.
    ///
    /// Per connection because the process-wide pair cannot say *which* lane loses
    /// them: a lane posting into an address nothing reads keeps its `posted`
    /// growing while its `received` stands still, and that is the shape a
    /// re-dialled pool entry would leave behind.
    posted_total: u64,
    received_total: u64,
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
            last_ack_prefix: 0,
            last_ack_below: 0,
            last_ack_bits: 0,
            last_ack_largest: 0,
            repairs_at_base: 0,
            repairs_above: 0,
            acks_applied: 0,
            acks_attached: 0,
            issued: 0,
            posted_total: 0,
            received_total: 0,
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
        let id = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(2);
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
        let take = usize::try_from(room).map_or(0, |room| room.min(bytes.len()));
        if take < bytes.len() {
            // The window took less than the caller offered, so from this instant
            // there are bytes nobody has put on the wire: that is the age
            // [`Conn::waiting`] reports. One clock read per short write, never
            // per packet — and a write the window accepted whole clocks nothing.
            self.unsent_at.get_or_insert_with(Instant::now);
            state.unsent_at.get_or_insert_with(Instant::now);
            // Which bound refused, recorded because the two wait for different
            // things: the congestion window waits for an acknowledgement, the
            // peer's credit waits for its *reader*. A stream that stalls with
            // its budget refusing is waiting on the far end's application, and
            // nothing this connection does will move it.
            // The age gate has already said why, if it is why.
            let refusal = if refused_by_age {
                Refusal::Aged
            } else if credit <= window {
                Refusal::Credit
            } else {
                Refusal::Window
            };
            self.last_refusal = Some(refusal);
            // By bound **and** by class: an opening stream that is refused is the
            // arrival path paying for the bulk spine, and the reserve exists to make
            // that impossible. If this counter never moves, the reserve works and the
            // first stage's cost is somewhere else entirely.
            let opened = still_opening;
            crate::transport::stats::note_refused(match (refusal, opened) {
                (Refusal::Aged, _) => "aged",
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
        let packet = self.build_packet(now, out);
        if packet.is_some() {
            self.posted_total = self.posted_total.saturating_add(1);
        }
        packet
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
        }) = self.next_stream_range()
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
        let copied = state.send.append_to(offset, len, out);
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
        );
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
        self.advance_cursor();
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
            // A finish-only packet carries no bytes of the opening.
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
        let eligible = |(id, state): (&u32, &StreamState)| {
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
        self.streams
            .range(cursor..)
            .find_map(eligible)
            .or_else(|| self.streams.range(..cursor).find_map(eligible))
    }

    /// Capture why this pass built nothing, while the lock is still held.
    fn snapshot_none(&mut self, reason: &'static str) {
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
            Frame::None => {}
            Frame::Stream {
                id,
                offset,
                fin,
                data,
            } => {
                let (data, fresh) = self.open_stream_payload(&packet, id, data)?;
                if offset + data.len() as u64 > self.recv_credit {
                    return Err(Rejected::OverWindow);
                }
                let known = self.streams.contains_key(&id);
                // One of *our* streams, and it is gone: its handle was dropped and
                // the connection retired it while the peer still had bytes on the
                // wire for it. Nobody wants those bytes, and the frame is dropped —
                // but the packet is still acknowledged below, because a sender that
                // keeps re-sending a packet nobody will acknowledge counts ten
                // unanswered retransmissions and declares the whole lane dead. That
                // is how one short-lived visitor connection was killing the lane
                // every other visitor shared.
                let retired = !known && id % 2 == u32::from(self.dialer);
                if retired {
                    tracing::debug!(stream = id, "mhd dropped data for a retired stream");
                    // The bytes are gone and nobody will read them, but the peer
                    // spent its window on them: give that window back, or a lane
                    // carrying many short streams narrows by one dead stream at a
                    // time until it carries nothing.
                    self.advance_read(data.len() as u64);
                } else {
                    if !known {
                        // The peer opens a stream by naming it; the accept queue is
                        // what tells its owner. A sealed stream arrives with the
                        // keys this packet was opened with.
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
                    }
                    let blocks = &mut self.blocks;
                    let state = self.streams.get_mut(&id).ok_or(Rejected::UnknownStream)?;
                    stats::around(sampled, &stats::PHASES.append, || {
                        state.recv.insert(blocks, offset, &data);
                    });
                    state.inserted = state
                        .inserted
                        .saturating_add(u64::try_from(data.len()).unwrap_or(0));
                    state.recv.note_ready(Some(now));
                    if fin {
                        state.recv.finish(offset + data.len() as u64);
                        state.peer_fin = true;
                    }
                    // Remember this stream's arrived prefix, which every
                    // acknowledgement from now on carries: it is what lets the
                    // peer's send buffer move on the bytes rather than on the
                    // packet records that carried them (see `wire::ACK`).
                    self.arrived_stream = id;
                    self.arrived_prefix = state.recv.arrived();
                    // The reader that owns this stream is the only one the pump
                    // has to wake: it got bytes or an end, and the rest of the
                    // lane's readers did not.
                    self.woke.push(id);
                }
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
        // A stream whose every byte is now acknowledged can say its finish
        // (see `settle`): the acknowledgement is the proof that the reset
        // will not overtake the data it follows.
        self.settle_finished();
        ack_timing.stop(&stats::PHASES.ack);
    }

    /// The timer expired: why, and how many packets it put back in the queue.
    pub(crate) fn on_timeout(&mut self, now: Instant) -> (Expiry, usize) {
        if self.closed.is_some() {
            return (Expiry::Loss, 0);
        }
        // The keepalive deadline is the other caller of this: when it fires
        // with nothing in flight there is nothing to recover, and the pump's
        // next pass emits the probe (see `poll_transmit`).
        let (expiry, lost) = self.recovery.on_timeout(now);
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
