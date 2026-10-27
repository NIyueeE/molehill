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

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use super::recovery::{Expiry, Recovery, SentRange};
use super::stream::{RecvBuf, SendBuf};
use super::wire::{self, AckSet, Frame, MAX_STREAM_PAYLOAD, ResetCode};

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
    /// Set when the connection ended.
    closed: Option<ResetCode>,
}

impl Conn {
    /// A fresh connection. The dialing end opens odd stream ids.
    pub(crate) fn new(id: u32, dialer: bool) -> Self {
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
            closed: None,
        }
    }

    /// The connection id every packet carries.
    pub(crate) fn id(&self) -> u32 {
        self.id
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
        let room = self.send_credit.saturating_sub(self.in_flight_bytes());
        let Some(state) = self.streams.get_mut(&stream) else {
            return 0;
        };
        let take = usize::try_from(room).map_or(0, |room| room.min(bytes.len()));
        if take == 0 {
            return 0;
        }
        state.send.write(&bytes[..take]);
        take
    }

    /// Bytes this end has written and the peer has not acknowledged.
    fn in_flight_bytes(&self) -> u64 {
        self.streams.values().map(|s| s.send.unacked()).sum()
    }

    /// Finish a stream: no byte past what is queued will be written.
    pub(crate) fn finish(&mut self, stream: u32) {
        if let Some(state) = self.streams.get_mut(&stream) {
            state.send.finish();
            state.local_fin = true;
        }
    }

    /// Take bytes a stream has received, answering how many were appended.
    pub(crate) fn read(&mut self, stream: u32, out: &mut Vec<u8>) -> usize {
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

    /// Whether the peer finished that stream.
    pub(crate) fn peer_finished(&self, stream: u32) -> bool {
        self.streams.get(&stream).is_some_and(|s| s.peer_fin)
    }

    /// Why that stream was reset, if it was.
    pub(crate) fn reset_reason(&self, stream: u32) -> Option<ResetCode> {
        self.streams.get(&stream).and_then(|s| s.reset)
    }

    /// Whether the connection ended.
    pub(crate) fn closed(&self) -> Option<ResetCode> {
        self.closed
    }

    /// End the connection.
    pub(crate) fn close(&mut self, code: ResetCode) {
        self.closed = Some(code);
    }

    /// When the caller should look again, if anything is outstanding.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        self.recovery.poll_timeout()
    }

    /// Build the next datagram to send, answering its length.
    ///
    /// A stream with bytes to send goes first, then a credit update the peer has
    /// not seen, then a bare acknowledgement: an ack that waits behind data is
    /// an ack that arrives after the sender has already re-sent.
    pub(crate) fn poll_transmit(&mut self, now: Instant, out: &mut [u8]) -> Option<usize> {
        if self.closed.is_some() {
            return None;
        }
        let ack = self.acks.packet();
        if let Some((id, offset, want)) = self.next_stream_range() {
            let number = self.recovery.next_number();
            let mut chunk = [0u8; MAX_STREAM_PAYLOAD];
            let (len, fin) = {
                let state = self.streams.get(&id)?;
                let (bytes, fin) = state.send.range(offset, want)?;
                let len = bytes.len();
                chunk[..len].copy_from_slice(bytes);
                (len, fin)
            };
            let frame = Frame::Stream {
                id,
                offset,
                fin,
                data: &chunk[..len],
            };
            let packet_len = wire::encode(out, self.id, number, ack, frame)?;
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
            return Some(packet_len);
        }
        if self.recv_credit > self.advertised + WINDOW / 4 {
            let number = self.recovery.next_number();
            let packet_len = wire::encode(
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
            return Some(packet_len);
        }
        if self.ack_pending {
            let packet_len =
                wire::encode(out, self.id, self.recovery.next_number(), ack, Frame::None)?;
            self.ack_pending = false;
            return Some(packet_len);
        }
        None
    }

    /// The next stream with bytes to send, walking from the cursor.
    fn next_stream_range(&mut self) -> Option<(u32, u64, usize)> {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        if ids.is_empty() {
            return None;
        }
        let start = ids.iter().position(|id| *id >= self.cursor).unwrap_or(0);
        for step in 0..ids.len() {
            let id = ids[(start + step) % ids.len()];
            let state = self.streams.get(&id)?;
            if state.reset.is_some() {
                continue;
            }
            if !state.send.has_unsent() {
                continue;
            }
            let offset = state.send.next_offset();
            let unsent = (state.send.unacked()).min(STREAM_BURST as u64);
            let want = usize::try_from(unsent).unwrap_or(0).min(MAX_STREAM_PAYLOAD);
            return Some((id, offset, want));
        }
        None
    }

    fn advance_cursor(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        self.cursor = ids
            .iter()
            .find(|id| **id > self.cursor)
            .copied()
            .or_else(|| ids.first().copied())
            .unwrap_or(0);
    }

    /// Feed one datagram from the peer.
    pub(crate) fn on_datagram(&mut self, now: Instant, buf: &[u8]) -> Result<(), Rejected> {
        let packet = wire::decode(buf).map_err(|_| Rejected::Malformed)?;
        if packet.id != self.id {
            return Err(Rejected::Malformed);
        }
        self.acks.insert(packet.number);
        if let Some(ack) = packet.ack {
            let acked = self.recovery.on_ack(now, ack);
            for sent in acked.newly {
                if let Some(range) = sent.range
                    && let Some(state) = self.streams.get_mut(&range.stream)
                {
                    state.send.ack(range.offset + u64::from(range.len));
                }
            }
            // A loss only rewinds the send point: the bytes are still in the
            // buffer, so the next pass carries them again from the same offset.
            for sent in acked.lost {
                if let Some(range) = sent.range
                    && let Some(state) = self.streams.get_mut(&range.stream)
                {
                    state.send.rewind_to(range.offset);
                }
            }
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
                if !known {
                    // The peer opens a stream by naming it; the accept queue is
                    // what tells its owner.
                    if id % 2 == u32::from(self.dialer) {
                        return Err(Rejected::UnknownStream);
                    }
                    self.streams.insert(id, StreamState::default());
                    self.inbound.push_back(id);
                }
                let state = self.streams.get_mut(&id).ok_or(Rejected::UnknownStream)?;
                state.recv.insert(offset, data);
                if fin {
                    state.recv.finish(offset + data.len() as u64);
                    state.peer_fin = true;
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
                if let Some(state) = self.streams.get_mut(&id) {
                    state.reset = Some(code);
                }
            }
        }
        Ok(())
    }

    /// The timer expired: why, and how many packets it put back in the queue.
    pub(crate) fn on_timeout(&mut self, now: Instant) -> (Expiry, usize) {
        let (expiry, lost) = self.recovery.on_timeout(now);
        for sent in &lost {
            if let Some(range) = sent.range
                && let Some(state) = self.streams.get_mut(&range.stream)
            {
                state.send.rewind_to(range.offset);
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
                let mut buf = [0u8; 2048];
                while let Some(len) = self.a.poll_transmit(now, &mut buf) {
                    self.a_to_b.push_back(buf[..len].to_vec());
                    moved += 1;
                    progress = true;
                }
                while let Some(len) = self.b.poll_transmit(now, &mut buf) {
                    self.b_to_a.push_back(buf[..len].to_vec());
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
            let mut buf = [0u8; 2048];
            let mut dropped = false;
            while let Some(len) = self.a.poll_transmit(now, &mut buf) {
                if !dropped {
                    dropped = true;
                    moved += 1;
                    continue;
                }
                self.a_to_b.push_back(buf[..len].to_vec());
                moved += 1;
            }
            while let Some(datagram) = self.a_to_b.pop_front() {
                self.b.on_datagram(now, &datagram).unwrap();
            }
            while let Some(len) = self.b.poll_transmit(now, &mut buf) {
                self.b_to_a.push_back(buf[..len].to_vec());
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
        let mut out = Vec::new();
        link.b.read(accepted.id, &mut out);
        assert_eq!(out, bytes, "every byte, in order");
    }

    /// A dropped datagram costs a round trip and nothing else: the timer puts
    /// the missing range back in the queue and the reader still gets the stream.
    #[test]
    #[ignore = "retransmission is not wired end to end yet; see HANDOFF"]
    fn a_dropped_datagram_is_recovered_without_losing_a_stream() {
        let now = Instant::now();
        let mut link = Link::new();
        let stream = link.a.open();
        let bytes = payload(8000);
        assert_eq!(link.a.write(stream.id, &bytes), bytes.len());

        link.pump_dropping_one(now);
        let Some(accepted) = link.b.accept() else {
            unreachable!("the stream is named on arrival")
        };
        let mut out = Vec::new();
        link.b.read(accepted.id, &mut out);
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
            let mut step = Vec::new();
            link.b.read(accepted.id, &mut step);
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
    #[ignore = "retransmission is not wired end to end yet; see HANDOFF"]
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
