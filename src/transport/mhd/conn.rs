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

/// Where the doubling phase stops.
///
/// Slow start doubles the flight per round trip, and this carrier has no pacer to
/// space the result: a window that outgrows the peer's socket buffer makes the
/// path drop *everything* for a while, and a sender that then hears nothing for
/// ten timers declares a live lane dead (`reaped dead tunnels ... reason="dead"`
/// in a bench arm's log, twice). Two megabytes is what a bulk transfer reaches in
/// a few round trips anyway, and it is small enough that the peer's receive buffer
/// absorbs the burst while the additive phase takes over.
const SLOW_START_CEILING: u64 = 2 * 1024 * 1024;

/// Why a frame could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The datagram is not one of this carrier's packets.
    Malformed,
    /// Bytes arrived that the advertised window does not cover.
    OverWindow,
    /// The frame names a stream this end never opened.
    UnknownStream,
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
}

/// A connection between two of this carrier's endpoints.
#[derive(Debug)]
pub(crate) struct Conn {
    id: u32,
    /// The dialing end numbers its streams odd and the accepting end even, so
    /// both allocate from disjoint spaces and an id is unique with no
    /// negotiation at all.
    dialer: bool,
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
    /// Streams the peer opened that nothing has taken yet.
    inbound: VecDeque<u32>,
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
    /// A fresh connection. The dialing end opens odd stream ids.
    pub(crate) fn new(id: u32, dialer: bool) -> Self {
        let now = Instant::now();
        Self {
            id,
            dialer,
            next_stream: u32::from(dialer),
            streams: BTreeMap::new(),
            cursor: u32::from(dialer),
            unsent_at: None,
            stall_logged_at: None,
            last_refusal: None,
            recovery: Recovery::new(),
            blocks: Blocks::default(),
            acks: AckSet::default(),
            ack_pending: false,
            send_credit: WINDOW / 2,
            recv_credit: WINDOW,
            advertised: 0,
            read: 0,
            arrived_stream: 0,
            arrived_prefix: 0,
            inbound: VecDeque::new(),
            resets: VecDeque::new(),
            pending_fins: VecDeque::new(),
            finishing: BTreeSet::new(),
            written: 0,
            buffered: 0,
            cwnd: INITIAL_CWND,
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
        let id = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(2);
        self.streams.insert(id, StreamState::default());
        StreamHandle { id }
    }

    /// Take a stream the peer opened, if one is waiting.
    pub(crate) fn accept(&mut self) -> Option<StreamHandle> {
        let id = self.inbound.pop_front()?;
        Some(StreamHandle { id })
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
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        // A stream that has already written a reserve's worth stops short of it,
        // so an opening stream always finds room (see `OPEN_RESERVE`).
        let room = if opening(state) {
            room
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
            // Which bound refused, recorded because the two wait for different
            // things: the congestion window waits for an acknowledgement, the
            // peer's credit waits for its *reader*. A stream that stalls with
            // its budget refusing is waiting on the far end's application, and
            // nothing this connection does will move it.
            self.last_refusal = Some(if credit <= window {
                Refusal::Credit
            } else {
                Refusal::Window
            });
        }
        if take == 0 {
            return 0;
        }
        state.send.write(&bytes[..take]);
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
            if let Some(state) = self.streams.remove(&stream) {
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
    fn has_queued_transmit(&self) -> bool {
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
        if self.closed.is_some() {
            return None;
        }
        let ack = self.acks.packet().map(|block| wire::Ack {
            stream: self.arrived_stream,
            prefix: self.arrived_prefix,
            ..block
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
        if self.ack_pending {
            let packet_len =
                wire::encode_control(out, self.id, self.recovery.last_number(), ack, Frame::None)?;
            self.ack_pending = false;
            self.keepalive_at = now + KEEPALIVE;
            return Some(packet_len);
        }
        self.transmit_keepalive(now, out, ack)
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
        let Pick {
            id,
            offset,
            len: want,
            repair,
        } = self.next_stream_range()?;
        let number = self.recovery.next_number();
        let state = self.streams.get(&id)?;
        // How much the packet carries, and whether it ends the stream: the
        // buffer answers both without being walked, because the payload is
        // appended straight into the run afterwards. A repair already came back
        // clamped to what is still owed, so only new data is bounded here.
        let len = if repair {
            want.min(state.send.available(offset, want))
        } else {
            state.send.available(offset, want)
        };
        if len == 0 {
            return None;
        }
        let fin = state.send.ends_at(offset, len);
        pick.stop(&stats::PHASES.tx_pick);
        let encode = stats::Timing::start(sampled);
        let packet_len = wire::encode_stream(
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
        )?;
        // The payload follows its header, drawn chunk by chunk: a packet is no
        // longer capped by the chunk boundary the send point happens to sit in.
        state.send.append_to(offset, len, out);
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
        );
        if let Some(state) = self.streams.get_mut(&id) {
            if repair {
                // The range is on the wire again; what is still owed stays
                // queued. The send point does not move: fresh data must not be
                // forced to wait behind a repair, and a repair must not be
                // forgotten because the send point moved past it.
                state.send.repair_sent(offset, len);
            } else {
                state.send.mark_sent(len);
            }
        }
        self.advance_cursor();
        book.stop(&stats::PHASES.tx_book);
        self.ack_pending = false;
        self.keepalive_at = now + KEEPALIVE;
        Some(packet_len)
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
        })
    }

    /// What this end's readers have left unread, per the whole connection.
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
    pub(crate) fn credit_blocked(&self) -> bool {
        matches!(self.last_refusal, Some(Refusal::Credit))
    }

    /// What the connection's three write budgets have left, for the
    /// open-latency line: `(credit, window, buffer, streams with bytes queued)`.
    ///
    /// A stream that opens slowly is slow for one of four reasons, and they look
    /// identical from outside: the peer's credit is spent, the congestion window
    /// is full, the send buffers are at their ceiling, or none of them is short
    /// and the open is waiting on the wire. Read only when an open is already
    /// late (see `MhdLane::open_stream`), so a healthy run pays nothing.
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
        let sampled = stats::enabled() && stats::PHASES.timing();
        let total = stats::Timing::start(sampled);
        let outcome = self.apply_datagram(now, buf, sampled);
        total.stop(&stats::PHASES.rx_total);
        outcome
    }

    /// The body of [`Conn::on_datagram`], with this datagram's sampling decision.
    fn apply_datagram(&mut self, now: Instant, buf: &[u8], sampled: bool) -> Result<(), Rejected> {
        let decode = stats::Timing::start(sampled);
        let packet = wire::decode(buf).map_err(|_| Rejected::Malformed)?;
        decode.stop(&stats::PHASES.decode);
        if packet.id != self.id {
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
                        // what tells its owner.
                        self.streams.insert(id, StreamState::default());
                        self.inbound.push_back(id);
                    }
                    let blocks = &mut self.blocks;
                    let state = self.streams.get_mut(&id).ok_or(Rejected::UnknownStream)?;
                    stats::around(sampled, &stats::PHASES.append, || {
                        state.recv.insert(blocks, offset, data);
                    });
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
                }
                self.ack_pending = true;
            }
            Frame::Credit { connection } => {
                self.send_credit = connection;
            }
            Frame::Datagram { .. } | Frame::L3 { .. } => {
                // Delivered by the layer above; the connection acknowledges them
                // so the sender's loss detection does not chase data nobody
                // promised to deliver.
                self.ack_pending = true;
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
        let ack_timing = stats::Timing::start(sampled);
        let acked = self.recovery.on_ack(now, ack);
        for sent in acked.newly {
            let Some(range) = sent.range else {
                continue;
            };
            let moved = match self.streams.get_mut(&range.stream) {
                Some(state) => state.send.ack(range.offset, usize::from(range.len)),
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
                if let Some(state) = self.streams.get_mut(&range.stream) {
                    state.send.repair(range.offset, usize::from(range.len));
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
                // [`SLOW_START_CEILING`]).
                self.cwnd = self.cwnd.saturating_add(bytes).min(SLOW_START_CEILING);
            } else {
                self.acked_since_growth += bytes;
                if self.acked_since_growth >= self.cwnd {
                    self.acked_since_growth = 0;
                    self.cwnd = self
                        .cwnd
                        .saturating_add(MAX_STREAM_PAYLOAD as u64)
                        .min(MAX_CWND);
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
                if let Some(state) = self.streams.get_mut(&range.stream) {
                    state.send.repair(range.offset, usize::from(range.len));
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

    /// The whole stack in memory: what one end writes, the other end reads, byte
    /// for byte, with the stream opened by naming it and accepted from the queue.
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
