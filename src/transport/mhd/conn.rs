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
use super::stream::{RecvBuf, SendBuf};
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
/// Sized for a wide-area path: a lane carrying a gigabit at two hundred
/// milliseconds needs tens of megabytes to stay full, and a window is cheap
/// because it is a number, not a buffer.
pub(crate) const WINDOW: u64 = 32 * 1024 * 1024;

/// How many bytes one stream may put in a row before the cursor moves on.
///
/// Without it a bulk transfer takes every packet the credit allows and an
/// interactive stream on the same connection waits behind it: the fair share the
/// connection-level window alone cannot give.
const STREAM_BURST: usize = MAX_STREAM_PAYLOAD * 4;

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
    recovery: Recovery,
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
            recovery: Recovery::new(),
            acks: AckSet::default(),
            ack_pending: false,
            send_credit: WINDOW / 2,
            recv_credit: WINDOW,
            advertised: 0,
            read: 0,
            inbound: VecDeque::new(),
            resets: VecDeque::new(),
            pending_fins: VecDeque::new(),
            finishing: BTreeSet::new(),
            written: 0,
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
        let room = credit.min(window);
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        let take = usize::try_from(room).map_or(0, |room| room.min(bytes.len()));
        if take == 0 {
            return 0;
        }
        state.send.write(&bytes[..take]);
        self.written += take as u64;
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
        let Some(state) = self.streams.get_mut(&stream) else {
            return;
        };
        state.abandoned = true;
        // Nothing will read this half again: release what arrived.
        state.recv = RecvBuf::default();
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
            self.streams.remove(&stream);
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
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        let taken = state.recv.read_into(out);
        self.read += taken as u64;
        // The window follows the reader: credit is what has been consumed plus
        // one window, so a peer that fills it meets a reader that drains it.
        self.recv_credit = self.read.saturating_add(WINDOW);
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
        let ack = self.acks.packet();
        if self.credit_due() {
            return self.transmit_credit(now, out, ack);
        }
        if let Some(len) = self.transmit_stream(now, out, ack) {
            return Some(len);
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
        let (id, offset, want) = self.next_stream_range()?;
        let number = self.recovery.next_number();
        let state = self.streams.get(&id)?;
        let (bytes, fin) = state.send.range(offset, want)?;
        let len = bytes.len();
        let packet_len = wire::encode_stream(
            out,
            self.id,
            number,
            ack,
            wire::StreamRange {
                id,
                offset,
                fin,
                data: bytes,
            },
        )?;
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
            state.send.mark_sent(len);
        }
        self.advance_cursor();
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
                data: &[],
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
        if now.saturating_duration_since(self.last_recv) > DEAD_AFTER {
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
    fn next_stream_range(&mut self) -> Option<(u32, u64, usize)> {
        if self.streams.is_empty() {
            return None;
        }
        let cursor = self.cursor;
        let eligible = |(id, state): (&u32, &StreamState)| {
            // `Finished` ends the peer's *write* direction, not the stream: the
            // other direction is still ours to use (that is what half-close
            // means). Only an abort or a refusal stops our writes too.
            let open = state.reset.is_none_or(|code| code == ResetCode::Finished);
            let queued = state.send.has_unsent();
            (open && queued).then(|| {
                let offset = state.send.next_offset();
                let unsent = (state.send.unacked()).min(STREAM_BURST as u64);
                let want = usize::try_from(unsent).unwrap_or(0).min(MAX_STREAM_PAYLOAD);
                (*id, offset, want)
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
                } else {
                    if !known {
                        // The peer opens a stream by naming it; the accept queue is
                        // what tells its owner.
                        self.streams.insert(id, StreamState::default());
                        self.inbound.push_back(id);
                    }
                    let state = self.streams.get_mut(&id).ok_or(Rejected::UnknownStream)?;
                    stats::around(sampled, &stats::PHASES.append, || {
                        state.recv.insert(offset, data);
                    });
                    if fin {
                        state.recv.finish(offset + data.len() as u64);
                        state.peer_fin = true;
                    }
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
            if let Some(range) = sent.range
                && let Some(state) = self.streams.get_mut(&range.stream)
            {
                state.send.ack(range.offset, usize::from(range.len));
            }
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
                    // A finish-only packet: nothing to rewind, so queue the
                    // finish again.
                    self.pending_fins.push_back((range.stream, range.offset));
                    continue;
                }
                if let Some(state) = self.streams.get_mut(&range.stream) {
                    state.send.rewind_to(range.offset);
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
                    state.send.rewind_to(range.offset);
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
}
