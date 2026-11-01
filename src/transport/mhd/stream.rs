//! One reliable stream's two halves: what has not been acknowledged, and what
//! has arrived out of order.
//!
//! Both halves are addressed by byte offset, which is the whole reason a loss
//! can be confined to one stream: a packet carries a range of one stream, so a
//! missing packet is a hole in exactly that stream and every other stream keeps
//! delivering.
//!
//! The send half is a byte queue with a cumulative base: bytes below the base
//! are acknowledged and gone, bytes above it are still owed to the peer. The
//! receive half keeps the out-of-order pieces beside the running read offset and
//! hands out only the contiguous prefix, so a caller reads a stream and never a
//! hole.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;

/// The bytes a stream has written and the peer has not acknowledged.
///
/// The bytes are a queue of chunks, one per record the writer handed over, and
/// the head is dropped when the peer acknowledges past it. A single growable
/// `Vec` could not do that: acknowledging a prefix meant memmoving everything
/// still owed down to the front, megabytes per acknowledgement at a 32 MiB
/// window.
///
/// A packet draws on the queue through [`Self::append_to`], which walks from a
/// cursor and may cross chunk boundaries, so packetisation no longer depends on
/// where a record happens to end.
#[derive(Debug, Default)]
pub(crate) struct SendBuf {
    chunks: VecDeque<Bytes>,
    /// The absolute offset of the first byte of `chunks.front()`.
    head: u64,
    /// The absolute offset just past the last byte the writer queued.
    tail: u64,
    /// The offset of the first byte the peer has not acknowledged.
    base: u64,
    /// The chunk the send point sits in, and that chunk's absolute offset.
    cursor: usize,
    cursor_at: u64,
    fin_at: Option<u64>,
    sent: u64,
    acked: BTreeMap<u64, u64>,
    /// Ranges the recovery has declared lost and this end still owes the peer,
    /// keyed by their start offset.
    ///
    /// They are *extra* to the send point, not a rewind of it: a lost range is
    /// one more range to put on the wire, and everything the send point has
    /// already covered stays covered. That is the whole difference from the
    /// single `sent` cursor this replaces — a rewind re-sent every byte after
    /// the hole on every loss (the bandwidth-delay product of duplicates per
    /// loss event), while a repair re-sends the hole.
    repairs: BTreeMap<u64, u64>,
}

impl SendBuf {
    /// Append bytes, answering the offset they start at.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> u64 {
        let at = self.tail;
        if !bytes.is_empty() {
            self.tail += bytes.len() as u64;
            self.chunks.push_back(Bytes::copy_from_slice(bytes));
        }
        at
    }

    /// One past the last byte the writer has queued.
    pub(crate) fn written(&self) -> u64 {
        self.tail
    }

    /// How many bytes a packet starting here may carry.
    pub(crate) fn available(&self, offset: u64, max: usize) -> usize {
        let room = self.written().saturating_sub(offset);
        usize::try_from(room).map_or(0, |room| room.min(max))
    }

    /// Whether the range `offset..offset + len` ends the stream.
    pub(crate) fn ends_at(&self, offset: u64, len: usize) -> bool {
        self.fin_at == Some(offset + len as u64)
    }

    /// Record that everything below `offset + len` arrived, answering how far
    /// the base moved.
    pub(crate) fn ack(&mut self, offset: u64, len: usize) -> u64 {
        let written = self.written();
        let start = offset.max(self.base).min(written);
        let end = offset.saturating_add(len as u64).min(written);
        if end <= start {
            return 0;
        }
        let before = self.base;
        self.absorb(start, end);
        while let Some(end) = self.acked.remove(&self.base) {
            self.base = end;
            self.sent = self.sent.max(self.base);
        }
        self.trim();
        self.base - before
    }

    /// Add one acknowledged range, merging it into the ones it overlaps.
    fn absorb(&mut self, start: u64, end: u64) {
        let mut start = start;
        let mut end = end;
        let preceding = self
            .acked
            .range(..=start)
            .next_back()
            .map(|(&at, &until)| (at, until));
        if let Some((at, until)) = preceding
            && until >= start
        {
            self.acked.remove(&at);
            start = at;
            end = end.max(until);
        }
        let touching: Vec<(u64, u64)> = self
            .acked
            .range(start..)
            .take_while(|item| *item.0 <= end)
            .map(|(&at, &until)| (at, until))
            .collect();
        for (at, until) in touching {
            self.acked.remove(&at);
            end = end.max(until);
        }
        self.acked.insert(start, end);
    }

    /// Advance the frontier to a byte prefix the peer says it holds.
    pub(crate) fn advance_to(&mut self, prefix: u64) -> u64 {
        let target = prefix.min(self.written()).max(self.base);
        let moved = target - self.base;
        if moved == 0 {
            return 0;
        }
        self.base = target;
        self.sent = self.sent.max(self.base);
        self.trim();
        self.follow();
        moved
    }

    /// Drop the chunks the frontier has passed.
    ///
    /// A chunk whose start is below the base but whose end is above it stays,
    /// because the bytes still owed inside it are read from it. That slack is
    /// bounded by one record per live stream.
    fn trim(&mut self) {
        while let Some(chunk) = self.chunks.front() {
            let end = self.head + chunk.len() as u64;
            if end > self.base {
                break;
            }
            self.head = end;
            self.chunks.pop_front();
            if self.cursor > 0 {
                self.cursor -= 1;
            } else {
                self.cursor_at = self.head;
            }
        }
        if let Some((&first, _)) = self.acked.first_key_value()
            && first < self.base
            && self.acked.len() > 64
        {
            self.acked = self.acked.split_off(&self.base);
        }
        // A repair the peer has already acknowledged is not owed any more; one
        // that straddles the new frontier keeps only its unacknowledged tail.
        while let Some((&start, &until)) = self.repairs.first_key_value() {
            if until <= self.base {
                self.repairs.remove(&start);
                continue;
            }
            if start < self.base {
                self.repairs.remove(&start);
                self.repairs.insert(self.base, until);
            }
            break;
        }
    }

    fn follow(&mut self) {
        if self.cursor >= self.chunks.len() {
            self.cursor = 0;
            self.cursor_at = self.head;
        }
        while self.cursor < self.chunks.len() {
            let end = self.cursor_at + self.chunks[self.cursor].len() as u64;
            if self.sent < end {
                return;
            }
            self.cursor_at = end;
            self.cursor += 1;
        }
    }

    /// Append `len` bytes starting at `offset` to `out`, walking the chunks.
    pub(crate) fn append_to(&self, offset: u64, len: usize, out: &mut Vec<u8>) {
        let from_cursor = offset >= self.cursor_at && self.cursor < self.chunks.len();
        let (mut index, mut at) = if from_cursor {
            (self.cursor, self.cursor_at)
        } else {
            (0, self.head)
        };
        let mut skip = usize::try_from(offset.saturating_sub(at)).unwrap_or(0);
        let mut left = len;
        while left > 0 {
            let Some(chunk) = self.chunks.get(index) else {
                break;
            };
            let end = at + chunk.len() as u64;
            if offset >= end {
                skip = skip.saturating_sub(chunk.len());
            } else if skip < chunk.len() {
                let take = left.min(chunk.len() - skip);
                out.extend_from_slice(&chunk[skip..skip + take]);
                left -= take;
                skip = 0;
            }
            at = end;
            index += 1;
        }
    }

    pub(crate) fn finish(&mut self) {
        self.fin_at = Some(self.written());
    }

    pub(crate) fn finished_at(&self) -> Option<u64> {
        self.fin_at
    }

    #[cfg(test)]
    pub(crate) fn finished(&self) -> bool {
        self.fin_at.is_some()
    }

    pub(crate) fn next_offset(&self) -> u64 {
        self.sent.max(self.base)
    }

    pub(crate) fn mark_sent(&mut self, len: usize) {
        self.sent = self.next_offset().saturating_add(len as u64);
        self.follow();
    }

    /// Mark one range lost: the recovery wants these bytes again.
    ///
    /// The range is kept by offset and merged with the ones it touches, so a
    /// loss burst costs one entry per hole rather than one per packet. It does
    /// **not** move the send point: fresh data keeps flowing while the repair is
    /// owed, because the two are independent ranges of the same stream and the
    /// receiver reassembles by offset.
    pub(crate) fn repair(&mut self, offset: u64, len: usize) {
        let mut start = offset.max(self.base);
        let mut end = offset.saturating_add(len as u64).min(self.tail);
        if end <= start {
            return; // already acknowledged, or past what was written
        }
        if let Some((&at, &until)) = self.repairs.range(..=start).next_back()
            && until >= start
        {
            self.repairs.remove(&at);
            start = at;
            end = end.max(until);
        }
        let touching: Vec<(u64, u64)> = self
            .repairs
            .range(start..)
            .take_while(|(at, _)| **at <= end)
            .map(|(&at, &until)| (at, until))
            .collect();
        for (at, until) in touching {
            self.repairs.remove(&at);
            end = end.max(until);
        }
        self.repairs.insert(start, end);
    }

    /// The next repair to put on the wire, and how much of it one packet may
    /// carry.
    pub(crate) fn next_repair(&self, max: usize) -> Option<(u64, usize)> {
        let (&start, &end) = self.repairs.iter().next()?;
        let start = start.max(self.base);
        if end <= start {
            return None;
        }
        let len = usize::try_from(end - start).map_or(0, |len| len.min(max));
        (len > 0).then_some((start, len))
    }

    /// Record that a repair range went out, trimming what is still owed.
    pub(crate) fn repair_sent(&mut self, offset: u64, len: usize) {
        let end = offset.saturating_add(len as u64);
        let Some((&start, &until)) = self.repairs.range(..=offset).next_back() else {
            return;
        };
        if until <= offset {
            return;
        }
        self.repairs.remove(&start);
        if until > end {
            self.repairs.insert(end, until);
        }
    }

    /// Whether a declared loss is still owed to the peer.
    pub(crate) fn has_repair(&self) -> bool {
        self.next_repair(1).is_some()
    }

    pub(crate) fn has_unsent(&self) -> bool {
        self.next_offset() < self.written() || self.has_repair()
    }

    /// The acknowledged frontier: bytes below it are gone.
    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    pub(crate) fn unacked(&self) -> u64 {
        self.written().saturating_sub(self.base)
    }

    /// Up to `max` bytes at `offset`, and whether the range ends the stream.
    ///
    /// Tests only: the transmitter appends through [`Self::append_to`], which
    /// may cross chunks.
    #[cfg(test)]
    pub(crate) fn range(&self, offset: u64, max: usize) -> Option<(&[u8], bool)> {
        if offset < self.head {
            return None;
        }
        let mut at = self.head;
        for chunk in &self.chunks {
            let end = at + chunk.len() as u64;
            if offset < end {
                let skip = usize::try_from(offset - at).ok()?;
                let take = max.min(chunk.len() - skip);
                let slice = chunk.get(skip..skip + take)?;
                return Some((slice, self.fin_at == Some(offset + slice.len() as u64)));
            }
            at = end;
        }
        None
    }

    pub(crate) fn held(&self) -> usize {
        usize::try_from(self.tail.saturating_sub(self.head)).unwrap_or(usize::MAX)
    }
}

/// Bytes in one receive block.
///
/// Arrived bytes are stored in blocks of this size instead of a growable vector
/// per piece. A block is taken from the connection's [`Blocks`] as a run fills
/// it and given back as the reader passes it, so the receive path's footprint is
/// the bytes it holds rather than the peak a vector's doubling once reserved,
/// and a datagram in the middle of a run takes no allocation at all. A datagram
/// is smaller than a block, so one block carries several of them; only the last
/// block of a run is partial.
pub(crate) const BLOCK: usize = 16 * 1024;

/// The connection's supply of receive blocks.
///
/// Every stream on the connection takes its blocks from here and returns them
/// once the reader has passed them, so a burst reuses what the previous burst
/// released. That reuse is the point: allocating a right-sized buffer per
/// datagram is what fragmented the carrier's heap, and the fragments outlived
/// the transfer that produced them.
#[derive(Debug, Default)]
pub(crate) struct Blocks {
    /// The blocks themselves, addressed by index. Nothing reuses an index
    /// before the stream that took it gives it back.
    slabs: Vec<Box<[u8]>>,
    /// Indices nobody holds.
    free: Vec<u32>,
    /// How many indices are out on loan.
    live: usize,
}

impl Blocks {
    /// One block, reused when the free list has one.
    fn take(&mut self) -> u32 {
        self.live += 1;
        if let Some(at) = self.free.pop() {
            return at;
        }
        let at = u32::try_from(self.slabs.len()).unwrap_or(u32::MAX);
        self.slabs.push(vec![0u8; BLOCK].into_boxed_slice());
        at
    }

    fn give(&mut self, at: u32) {
        self.live = self.live.saturating_sub(1);
        self.free.push(at);
    }

    /// Blocks this connection is holding on loan.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.live
    }

    fn read(&self, at: u32, start: usize, len: usize) -> &[u8] {
        &self.slabs[at as usize][start..start + len]
    }

    fn write(&mut self, at: u32, start: usize, bytes: &[u8]) {
        self.slabs[at as usize][start..start + bytes.len()].copy_from_slice(bytes);
    }

    /// Move bytes between two blocks, which is what folding a hole costs: the
    /// run behind it is copied once, exactly as a growable piece copied it.
    fn copy(&mut self, from: u32, from_start: usize, to: u32, to_start: usize, len: usize) {
        let end = from_start + len;
        if from == to {
            self.slabs[to as usize].copy_within(from_start..end, to_start);
            return;
        }
        let (low, high) = (from.min(to) as usize, from.max(to) as usize);
        let (left, right) = self.slabs.split_at_mut(high);
        let (src, dst) = if from < to {
            (&left[low], &mut right[0])
        } else {
            (&right[0], &mut left[low])
        };
        dst[to_start..to_start + len].copy_from_slice(&src[from_start..end]);
    }

    /// Keep the blocks that are on loan and let the free ones go.
    ///
    /// A connection that has carried a window's worth of data holds thirty-two
    /// megabytes of blocks afterwards; giving them back when nothing is on loan
    /// is what keeps an idle lane small.
    pub(crate) fn shrink(&mut self) {
        if self.live == 0 {
            self.slabs.clear();
            self.free.clear();
        }
    }
}

/// Bytes `[start, start + len)` of one pooled block: the storage of a run.
#[derive(Debug, Clone, Copy)]
struct Held {
    at: u32,
    start: u16,
    len: u16,
}

/// One contiguous run of a stream, in blocks taken from the pool.
#[derive(Debug, Default)]
struct Piece {
    spans: VecDeque<Held>,
    /// The run's whole length, delivered or not.
    len: usize,
    /// How much of it the reader has taken.
    consumed: usize,
}

impl Piece {
    /// Append a block's worth of bytes, filling the last block before taking
    /// another one from the pool.
    fn append_from(
        &mut self,
        blocks: &mut Blocks,
        from: u32,
        mut from_start: usize,
        mut len: usize,
    ) {
        while len > 0 {
            let Some(last) = self.spans.back_mut() else {
                let at = blocks.take();
                self.spans.push_back(Held {
                    at,
                    start: 0,
                    len: 0,
                });
                continue;
            };
            // The span's first bytes may have been read already, so its room is
            // what is left of the block, not of the datagrams.
            let room = BLOCK.saturating_sub(usize::from(last.start) + usize::from(last.len));
            if room == 0 {
                let at = blocks.take();
                self.spans.push_back(Held {
                    at,
                    start: 0,
                    len: 0,
                });
                continue;
            }
            let take = room.min(len);
            let (at, to_start) = (last.at, usize::from(last.start) + usize::from(last.len));
            blocks.copy(from, from_start, at, to_start, take);
            last.len += u16::try_from(take).unwrap_or(u16::MAX);
            self.len += take;
            len -= take;
            from_start += take;
        }
    }

    /// Append a caller's slice.
    fn append(&mut self, blocks: &mut Blocks, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let Some(last) = self.spans.back_mut() else {
                let at = blocks.take();
                self.spans.push_back(Held {
                    at,
                    start: 0,
                    len: 0,
                });
                continue;
            };
            // The span's first bytes may have been read already, so its room is
            // what is left of the block, not of the datagrams.
            let room = BLOCK.saturating_sub(usize::from(last.start) + usize::from(last.len));
            if room == 0 {
                let at = blocks.take();
                self.spans.push_back(Held {
                    at,
                    start: 0,
                    len: 0,
                });
                continue;
            }
            let take = room.min(bytes.len());
            let (at, start) = (last.at, usize::from(last.start) + usize::from(last.len));
            blocks.write(at, start, &bytes[..take]);
            last.len += u16::try_from(take).unwrap_or(u16::MAX);
            self.len += take;
            bytes = &bytes[take..];
        }
    }

    /// Give every block back to the pool.
    fn release(&mut self, blocks: &mut Blocks) {
        for span in self.spans.drain(..) {
            blocks.give(span.at);
        }
    }
}

/// The bytes of a stream that have arrived, in and out of order.
#[derive(Debug, Default)]
pub(crate) struct RecvBuf {
    /// Pieces keyed by the offset they start at.
    ///
    /// A piece may start below [`Self::read`]: the reader hands bytes out in
    /// whatever slice the caller offered, so the piece it is part-way through
    /// stays where it arrived until it is used up. Nothing is moved to make it
    /// fit — an earlier version drained the whole buffer per caller chunk,
    /// which is a memmove of everything still held on every read and measured
    /// as a memmove per byte carried.
    ///
    /// The bytes themselves live in blocks from the connection's [`Blocks`], so
    /// a piece's cost is what it holds; [`Self::discard`] and the read path
    /// return the blocks, and nothing here allocates per datagram.
    pieces: BTreeMap<u64, Piece>,
    /// Everything below this offset has been read out.
    read: u64,
    /// When bytes first became readable at the read offset and nothing took them.
    ///
    /// The discriminator for a parked reader: if this instant is long before the
    /// poll that finally got the bytes, the data was there and the wake-up was
    /// late; if it is at the same moment, the data itself arrived late and the
    /// fault is upstream of this end.
    ready_at: Option<std::time::Instant>,
    /// Where the peer said the stream ends, once it did.
    ///
    /// A finish may arrive on a range whose predecessors are still missing (the
    /// last packet carries the flag, and UDP does not promise the ones before it
    /// arrived first). The stream is only complete when this equals `read`: a
    /// reader that stopped at the flag would truncate the stream at the first
    /// hole, which is exactly what a bulk transfer under loss produces.
    fin_at: Option<u64>,
}

impl RecvBuf {
    /// Take a range. Answers whether it was news.
    ///
    /// A range that starts below the read offset is trimmed, and one that
    /// repeats a range already held is dropped: a retransmission is the same
    /// bytes again, never a different range over the same hole.
    ///
    /// A range that continues the piece it follows is **appended to it** rather
    /// than stored beside it. That is the shape a healthy path produces — the
    /// sender emits a stream in order, one packet after another — and a piece
    /// per packet would enter and leave the table once per packet, which is two
    /// tree operations and an allocation for every 1400 bytes carried. Appending
    /// costs one copy into a block the pool already holds.
    /// Whether the reader is owed bytes right now, stamped the first time it is.
    pub(crate) fn note_ready(&mut self, now: Option<std::time::Instant>) {
        if let Some(now) = now
            && self.ready_at.is_none()
            && self.arrived() > self.read
        {
            self.ready_at = Some(now);
        }
    }

    /// The hole the reader is stuck behind, if it is stuck: `(read, arrived,
    /// pieces, first piece start)`.
    ///
    /// `arrived()` only advances over a contiguous prefix, so bytes can be
    /// present and unreadable at the same time — which is exactly what a reader
    /// that is woken hundreds of times and reads nothing looks like from outside.
    pub(crate) fn prefix_state(&self) -> (u64, u64, usize, Option<u64>) {
        (
            self.read,
            self.arrived(),
            self.pieces.len(),
            self.pieces.keys().next().copied(),
        )
    }

    /// How long bytes have been waiting for a reader, if they are.
    pub(crate) fn ready_for(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.ready_at.map(|at| now.saturating_duration_since(at))
    }

    pub(crate) fn insert(&mut self, blocks: &mut Blocks, offset: u64, bytes: &[u8]) -> bool {
        let (mut offset, mut bytes) = (offset, bytes);
        if bytes.is_empty() {
            return false;
        }
        // Bytes the reader has already taken are gone: a retransmission of them
        // is not news, and keeping it would hand it out twice.
        if offset < self.read {
            let skip = usize::try_from(self.read - offset).unwrap_or(bytes.len());
            if skip >= bytes.len() {
                return false;
            }
            offset = self.read;
            bytes = &bytes[skip..];
        }
        if let Some((start, held)) = self.pieces.range(..offset).next_back() {
            let piece_end = start + held.len as u64;
            if piece_end > offset {
                // The range starts inside a piece already held: skip the part
                // that is there, and keep whatever extends past it.
                let skip = usize::try_from(piece_end - offset).unwrap_or(bytes.len());
                if skip >= bytes.len() {
                    return false;
                }
                offset = piece_end;
                bytes = &bytes[skip..];
            }
        }
        if bytes.is_empty() {
            return false;
        }
        // Never take bytes a piece already ahead of us holds: they have arrived,
        // and storing them twice would hand them out twice.
        if let Some((next, _)) = self.pieces.range(offset..).next()
            && *next < offset + bytes.len() as u64
        {
            let keep = usize::try_from(*next - offset).unwrap_or(0);
            if keep == 0 {
                return false;
            }
            bytes = &bytes[..keep];
        }
        let continues = self
            .pieces
            .range(..offset)
            .next_back()
            .map(|(start, piece)| *start + piece.len as u64)
            .is_some_and(|end| end == offset);
        if continues && let Some((start, _)) = self.pieces.range(..offset).next_back() {
            let start = *start;
            if let Some(piece) = self.pieces.get_mut(&start) {
                piece.append(blocks, bytes);
            }
            self.absorb_next(blocks, start);
            return true;
        }
        if self.pieces.contains_key(&offset) {
            return false;
        }
        let mut piece = Piece::default();
        piece.append(blocks, bytes);
        self.pieces.insert(offset, piece);
        self.absorb_next(blocks, offset);
        true
    }

    /// Fold the pieces that now continue `start`'s piece into it.
    ///
    /// Only a piece that arrives exactly where another ended can leave two
    /// adjacent entries; each fold removes one, so the table stays proportional
    /// to the number of holes rather than to the bytes carried. The fold copies
    /// the run it absorbs, which is what a growable piece did, and gives back
    /// the blocks it leaves empty.
    fn absorb_next(&mut self, blocks: &mut Blocks, start: u64) {
        loop {
            let Some(held) = self.pieces.get(&start) else {
                return;
            };
            let end = start + held.len as u64;
            let Some(mut following) = self.pieces.remove(&end) else {
                return;
            };
            for span in following.spans.iter().copied() {
                if let Some(piece) = self.pieces.get_mut(&start) {
                    piece.append_from(
                        blocks,
                        span.at,
                        usize::from(span.start),
                        usize::from(span.len),
                    );
                }
            }
            following.release(blocks);
        }
    }

    /// Mark the stream finished at this offset.
    pub(crate) fn finish(&mut self, at: u64) {
        self.fin_at = Some(match self.fin_at {
            Some(previous) => previous.min(at),
            None => at,
        });
    }

    /// How many of this stream's bytes have arrived with no hole.
    ///
    /// The receiver's half of [`SendBuf::advance_to`]: the number it puts in
    /// every acknowledgement, so the peer's frontier moves on the bytes
    /// themselves rather than on the packet records that carried them.
    pub(crate) fn arrived(&self) -> u64 {
        let Some((&at, piece)) = self.pieces.iter().next() else {
            return self.read;
        };
        if at > self.read {
            // The first piece starts beyond the read offset: a hole sits
            // between them and nothing past it can be reported.
            return self.read;
        }
        (at + piece.len as u64).max(self.read)
    }

    /// Bytes that have arrived and not yet been read out of this stream.
    ///
    /// The number a stalled reader is measured by: the connection's credit
    /// follows consumption, not arrival, so a stream whose reader has stopped
    /// is invisible in every other counter while it holds the window back. Each
    /// piece counts what the reader has not taken from it, so a piece the reader
    /// is part-way through contributes the rest of itself and nothing else.
    pub(crate) fn held(&self) -> u64 {
        self.pieces
            .values()
            .map(|piece| u64::try_from(piece.len - piece.consumed).unwrap_or(0))
            .sum()
    }

    /// Throw away everything that has not been read, answering how many bytes
    /// that was.
    ///
    /// For a stream that is leaving the table with a reader that will never come
    /// back: the bytes are unreachable, so holding them only misreports what
    /// this end is behind on, and their blocks belong to the next stream.
    pub(crate) fn discard(&mut self, blocks: &mut Blocks) -> u64 {
        let held = self.held();
        for (_, mut piece) in std::mem::take(&mut self.pieces) {
            piece.release(blocks);
        }
        held
    }

    /// Move the contiguous prefix into `out`, answering how many bytes were
    /// written.
    ///
    /// This is the only way bytes leave the buffer, so a caller never sees a
    /// hole: what a lost packet costs is the tail of this stream, and nothing
    /// else. Writing straight into the caller's slice is the point: staging the
    /// prefix in a buffer of this layer's own and then handing out a
    /// caller-sized slice of it is a second copy per byte, and the memmove that
    /// keeps the staged remainder in place made it worse than that.
    ///
    /// A block the reader has passed goes back to the pool here, which is what
    /// keeps a lane that has carried a window's worth of data from holding it.
    pub(crate) fn read_into(&mut self, blocks: &mut Blocks, out: &mut [u8]) -> usize {
        let read = self.read_into_inner(blocks, out);
        if read > 0 {
            // Everything owed at the old offset has been taken; whatever is
            // still owed becomes ready now.
            self.ready_at = None;
            self.note_ready(None);
        }
        read
    }

    fn read_into_inner(&mut self, blocks: &mut Blocks, out: &mut [u8]) -> usize {
        let mut filled = 0;
        loop {
            let room = out.len().saturating_sub(filled);
            if room == 0 {
                break;
            }
            let Some((&start, _)) = self.pieces.iter().next() else {
                break;
            };
            if start > self.read {
                break; // a hole: the prefix ends here
            }
            let Some(piece) = self.pieces.get_mut(&start) else {
                break;
            };
            if usize::try_from(self.read - start).unwrap_or(usize::MAX) >= piece.len {
                // Every byte of this piece has been handed out already.
                self.pieces.remove(&start);
                continue;
            }
            let mut taken = 0;
            while taken < room {
                let Some(front) = piece.spans.front_mut() else {
                    break;
                };
                let take = room.saturating_sub(taken).min(usize::from(front.len));
                if take == 0 {
                    // A span with nothing left in it: give it back and look at
                    // the next one rather than reading zero bytes forever.
                    let at = front.at;
                    piece.spans.pop_front();
                    blocks.give(at);
                    continue;
                }
                let bytes = blocks.read(front.at, usize::from(front.start), take);
                out[filled + taken..filled + taken + take].copy_from_slice(bytes);
                front.start += u16::try_from(take).unwrap_or(0);
                front.len -= u16::try_from(take).unwrap_or(0);
                piece.consumed += take;
                taken += take;
                self.read += take as u64;
                if front.len == 0 {
                    let at = front.at;
                    piece.spans.pop_front();
                    blocks.give(at);
                }
            }
            filled += taken;
            if taken == 0 {
                break;
            }
            if piece.consumed >= piece.len {
                self.pieces.remove(&start);
            }
        }
        filled
    }

    /// Whether the finish arrived and every byte before it has been read.
    pub(crate) fn is_complete(&self) -> bool {
        self.fin_at == Some(self.read) && self.pieces.is_empty()
    }

    /// The first hole: how many bytes are missing and where the next piece is.
    #[cfg(test)]
    pub(crate) fn hole(&self) -> Option<(u64, u64)> {
        let (offset, _) = self.pieces.iter().next()?;
        (*offset > self.read).then(|| (*offset - self.read, *offset))
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    fn stream_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    /// Read everything the buffer holds into a fresh vector, the way a caller
    /// with an unbounded buffer would.
    fn drain_all(recv: &mut RecvBuf, blocks: &mut Blocks) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = recv.read_into(blocks, &mut chunk);
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    /// The property this half exists for: pieces that arrive in any order
    /// reassemble into exactly the stream that was written, and the read offset
    /// advances only over contiguous bytes.
    #[test]
    fn a_stream_assembled_from_shuffled_pieces_is_the_original() {
        let original = stream_bytes(4096);
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        // Twelve pieces, inserted in an order no sender would choose.
        let mut bounds: Vec<(usize, usize)> = Vec::new();
        let mut at = 0;
        for step in [
            64usize, 128, 256, 512, 512, 512, 512, 512, 256, 256, 360, 216,
        ] {
            bounds.push((at, at + step));
            at += step;
        }
        assert_eq!(at, original.len());
        for index in [5usize, 0, 11, 3, 8, 1, 10, 4, 9, 2, 7, 6] {
            let (start, end) = bounds[index];
            recv.insert(&mut blocks, start as u64, &original[start..end]);
        }
        let out = drain_all(&mut recv, &mut blocks);
        assert_eq!(out.len(), original.len(), "every byte must come out");
        assert_eq!(out, original, "the bytes must be the stream, in order");
    }

    /// What a stalled reader is measured by: bytes that arrived and were not
    /// read out — including the part of a piece the reader is in the middle of,
    /// and including bytes behind a hole, because those are held too.
    #[test]
    fn held_counts_what_arrived_and_nobody_read() {
        let original = stream_bytes(600);
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        recv.insert(&mut blocks, 0, &original[..300]);
        recv.insert(&mut blocks, 300, &original[300..]);
        assert_eq!(recv.held(), 600, "everything arrived and nothing was read");

        let mut chunk = [0u8; 128];
        assert_eq!(recv.read_into(&mut blocks, &mut chunk), 128);
        assert_eq!(
            recv.held(),
            600 - 128,
            "the reader's own progress is the difference"
        );

        let mut rest = [0u8; 4096];
        assert_eq!(recv.read_into(&mut blocks, &mut rest), 600 - 128);
        assert_eq!(recv.held(), 0, "a drained stream holds nothing");

        // A hole does not make the bytes behind it free: the peer sent them,
        // this end has them, and the credit the peer will be granted counts
        // them until they are read.
        let mut holed = RecvBuf::default();
        holed.insert(&mut blocks, 0, &original[..100]);
        holed.insert(&mut blocks, 200, &original[200..300]);
        assert_eq!(holed.held(), 200, "a hole holds its own bytes as well");
    }

    /// The point of the pool: a stream's blocks are what it holds, not the peak
    /// a growable piece once reached, and a reader that passes them gives them
    /// back for the next stream on the connection instead of the allocator.
    #[test]
    fn a_drained_stream_gives_its_blocks_back_for_the_next_one() {
        let original = stream_bytes(BLOCK * 4);
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        for (index, chunk) in original.chunks(1400).enumerate() {
            recv.insert(&mut blocks, (index * 1400) as u64, chunk);
        }
        assert_eq!(
            blocks.live(),
            original.len().div_ceil(BLOCK),
            "one block per {BLOCK} bytes carried, with no doubling"
        );
        assert_eq!(
            recv.held(),
            u64::try_from(original.len()).unwrap(),
            "a run holds exactly the bytes that arrived"
        );

        let mut chunk = [0u8; 1400];
        let mut read = 0;
        while read < original.len() {
            let n = recv.read_into(&mut blocks, &mut chunk);
            assert!(n > 0, "a full prefix must always make progress");
            read += n;
        }
        assert_eq!(blocks.live(), 0, "a drained stream holds no blocks");

        // The blocks came back to the pool rather than to the allocator: the
        // next stream is carried by the same slabs.
        let grown = blocks.slabs.len();
        let mut next = RecvBuf::default();
        for (index, chunk) in original.chunks(1400).enumerate() {
            next.insert(&mut blocks, (index * 1400) as u64, chunk);
        }
        assert_eq!(
            blocks.slabs.len(),
            grown,
            "the pool is reused, not regrown per stream"
        );
        assert_eq!(drain_all(&mut next, &mut blocks).len(), original.len());
        blocks.shrink();
        assert_eq!(blocks.slabs.len(), 0, "an idle connection holds no blocks");
    }

    /// A hole stops one stream at the hole and says where the next piece is; it
    /// does not discard what arrived behind it.
    #[test]
    fn a_hole_stops_the_prefix_and_reports_itself() {
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        assert!(recv.insert(&mut blocks, 20, b"later"));
        assert!(recv.insert(&mut blocks, 0, b"first"));
        let mut out = [0u8; 32];
        assert_eq!(recv.read_into(&mut blocks, &mut out), 5);
        assert_eq!(&out[..5], b"first");
        assert_eq!(recv.hole(), Some((15, 20)), "the missing range is 5..20");
        assert!(
            recv.insert(&mut blocks, 5, &[b'x'; 15]),
            "the hole can be filled"
        );
        assert_eq!(drain_all(&mut recv, &mut blocks), b"xxxxxxxxxxxxxxxlater");
    }

    /// A reader that takes the prefix in slices smaller than the pieces gets the
    /// same bytes, exactly once: the piece it is part-way through is not handed
    /// out again, and a retransmission of bytes it already delivered is not
    /// taken a second time.
    #[test]
    fn a_reader_that_takes_small_slices_sees_each_byte_once() {
        let original = stream_bytes(4096);
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        for (index, chunk) in original.chunks(331).enumerate() {
            assert!(recv.insert(&mut blocks, (index * 331) as u64, chunk));
        }
        let mut got = Vec::new();
        let mut small = [0u8; 37];
        while got.len() < original.len() {
            // A retransmission of bytes already delivered in front of the read
            // offset must not duplicate them.
            let _ = recv.insert(&mut blocks, 0, &original[..64]);
            let n = recv.read_into(&mut blocks, &mut small);
            assert!(n > 0, "a full prefix must always make progress");
            got.extend_from_slice(&small[..n]);
        }
        assert_eq!(got, original);
        assert!(recv.is_complete() || recv.fin_at.is_none());
    }

    /// An acknowledgement moves the base and clamps a peer that acks bytes
    /// nobody sent.
    #[test]
    fn an_acknowledgement_moves_the_base_and_clamps_the_impossible() {
        let mut send = SendBuf::default();
        assert_eq!(send.write(b"hello world"), 0);
        assert_eq!(send.next_offset(), 0);
        send.ack(0, 6);
        assert_eq!(send.next_offset(), 6);
        assert_eq!(send.unacked(), 5);
        send.ack(0, 1_000_000);
        assert_eq!(send.next_offset(), 11, "an ack past the end is clamped");
        assert_eq!(send.unacked(), 0);
        assert!(send.range(0, 4).is_none(), "acknowledged bytes are gone");
    }

    /// The range that ends a stream carries the fin, and only that one.
    #[test]
    fn the_finishing_range_carries_the_fin() {
        let mut send = SendBuf::default();
        send.write(b"abcdef");
        send.finish();
        assert!(send.finished());
        let (head, fin) = send.range(0, 3).unwrap();
        assert_eq!(head, b"abc");
        assert!(!fin, "only the last range ends the stream");
        let (tail, fin) = send.range(3, 3).unwrap();
        assert_eq!(tail, b"def");
        assert!(fin);
    }

    /// A byte prefix moves the frontier on its own, which is the property the
    /// packet-number acknowledgements cannot offer.
    #[test]
    fn a_byte_prefix_moves_the_frontier_without_a_range() {
        let mut send = SendBuf::default();
        send.write(&[3u8; 3000]);

        assert_eq!(send.advance_to(1200), 1200, "the prefix frees its bytes");
        assert_eq!(send.unacked(), 1800);
        assert_eq!(send.advance_to(900), 0, "a stale prefix moves nothing");
        assert_eq!(
            send.advance_to(u64::MAX),
            1800,
            "a peer cannot acknowledge bytes nobody sent"
        );
        assert_eq!(send.unacked(), 0);
        assert_eq!(send.held(), 0, "and the buffer is emptied");
    }

    /// The received prefix stops at the first hole and grows past it once the
    /// hole is filled — that is what makes it safe to send as an acknowledgement.
    #[test]
    fn the_arrived_prefix_stops_at_the_first_hole() {
        let mut recv = RecvBuf::default();
        let mut blocks = Blocks::default();
        assert_eq!(recv.arrived(), 0);
        recv.insert(&mut blocks, 0, &[1u8; 100]);
        assert_eq!(recv.arrived(), 100);
        recv.insert(&mut blocks, 300, &[2u8; 100]);
        assert_eq!(recv.arrived(), 100, "a hole at 100 stops the prefix");
        recv.insert(&mut blocks, 100, &[3u8; 200]);
        assert_eq!(recv.arrived(), 400, "filling it moves the prefix on");
    }

    /// A declared loss is one owed range, and it does not move the send point.
    ///
    /// The rewind this replaces re-sent every byte after the hole on every loss
    /// — the bandwidth-delay product of duplicates per loss event, and the
    /// reason a lossy path carried a fraction of what the same path carries over
    /// TCP. A repair is the hole and nothing else, and fresh data keeps flowing
    /// while it is owed.
    #[test]
    fn a_loss_owes_only_its_own_range() {
        let mut send = SendBuf::default();
        send.write(&[7u8; 3000]);
        send.mark_sent(3000);
        assert_eq!(send.next_offset(), 3000);

        send.repair(1400, 200);
        assert_eq!(send.next_offset(), 3000, "the send point does not move");
        assert_eq!(
            send.next_repair(1400),
            Some((1400, 200)),
            "the hole is what is owed"
        );

        // Two losses in one acknowledgement merge with the ranges they touch
        // into one entry, and a third that is still separate stays separate.
        send.repair(1600, 100);
        assert_eq!(send.next_repair(usize::MAX), Some((1400, 300)));
        send.repair(2500, 100);
        assert_eq!(send.next_repair(usize::MAX), Some((1400, 300)));

        // Sending part of it trims what is owed, not the whole range.
        send.repair_sent(1400, 100);
        assert_eq!(send.next_repair(usize::MAX), Some((1500, 200)));
        send.repair_sent(1500, 200);
        assert_eq!(send.next_repair(usize::MAX), Some((2500, 100)));
        send.repair_sent(2500, 100);
        assert_eq!(send.next_repair(usize::MAX), None, "nothing is owed");
        assert_eq!(send.next_offset(), 3000, "and the send point never moved");
    }

    /// Once the peer acknowledges a range, a repair for it is not owed any more;
    /// one that straddles the new frontier keeps only its unacknowledged tail.
    #[test]
    fn an_acknowledged_range_is_not_repaired() {
        let mut send = SendBuf::default();
        send.write(&[1u8; 3000]);
        send.mark_sent(3000);
        send.repair(0, 1000);
        send.repair(1000, 1000);

        // The peer's prefix reaches 1500: the first range is gone and the second
        // is owed from 1500 on.
        assert_eq!(send.advance_to(1500), 1500);
        assert_eq!(send.next_repair(usize::MAX), Some((1500, 500)));
        assert!(send.has_repair(), "one range is still owed");

        assert_eq!(send.advance_to(3000), 1500);
        assert_eq!(send.next_repair(usize::MAX), None, "nothing is owed");
        assert_eq!(send.held(), 0, "and the buffer is empty");
    }

    /// An acknowledgement that overlaps one already held extends it instead of
    /// hiding the bytes it covers behind a key the frontier will never look up.
    ///
    /// A retransmission produces exactly this shape: the peer acknowledges a
    /// range that is already partly acknowledged, and a plain insert leaves the
    /// second copy keyed below the frontier.
    #[test]
    fn an_acknowledgement_that_overlaps_one_already_held_extends_it() {
        let mut send = SendBuf::default();
        send.write(&[1u8; 4000]);

        assert_eq!(send.ack(2000, 1000), 0, "a range above the base waits");
        assert_eq!(send.ack(1500, 1000), 0, "overlapping it waits too");

        // The prefix arrives. The frontier must walk through the merged range
        // (1500..3000), not stop at its own 1600.
        assert_eq!(send.ack(0, 1600), 3000, "the whole run is acknowledged");
        assert_eq!(send.unacked(), 1000, "only the tail is still owed");
    }

    /// The property the compaction rests on: acknowledging in any order still
    /// leaves every unacknowledged byte readable, and the acknowledged ones are
    /// dropped only once the prefix has paid for the move.
    #[test]
    fn acknowledging_out_of_order_keeps_every_unacknowledged_byte() {
        let mut send = SendBuf::default();
        // A window-sized stream: four blocks of 16 KiB.
        let mut at = 0;
        for _ in 0..4 {
            let block = vec![0x5Au8; 16 * 1024];
            assert_eq!(send.write(&block), at);
            at += 16 * 1024;
        }
        // Ack the third block first, then the first, then the second: the base
        // may only walk over the contiguous run, so the hole at the second
        // block must survive.
        send.ack(2 * 16 * 1024, 16 * 1024);
        send.ack(0, 16 * 1024);
        assert_eq!(send.unacked(), 3 * 16 * 1024, "the hole is still owed");
        send.ack(16 * 1024, 16 * 1024);
        assert_eq!(send.unacked(), 16 * 1024, "only the last block is owed");
        let (bytes, _) = send.range(3 * 16 * 1024, 16 * 1024).unwrap();
        assert_eq!(bytes.len(), 16 * 1024);
        assert_eq!(bytes[0], 0x5A);
        send.ack(3 * 16 * 1024, 16 * 1024);
        assert_eq!(send.unacked(), 0);
        assert!(!send.has_unsent());
    }

    /// A buffer that has been compacted away answers `None` for what it no
    /// longer holds, rather than handing back the wrong bytes.
    #[test]
    fn a_compacted_prefix_is_not_handed_back_again() {
        let mut send = SendBuf::default();
        for _ in 0..4 {
            send.write(&vec![0x5Au8; 16 * 1024]);
        }
        send.ack(0, 4 * 16 * 1024);
        assert!(send.range(0, 16).is_none(), "acknowledged bytes are gone");
        assert_eq!(send.unacked(), 0);
        // And the offsets still line up after the prefix is dropped.
        send.write(b"tail");
        let (bytes, fin) = send.range(4 * 16 * 1024, 4).unwrap();
        assert_eq!(bytes, b"tail");
        assert!(!fin);
        send.finish();
        let (bytes, fin) = send.range(4 * 16 * 1024, 4).unwrap();
        assert_eq!(bytes, b"tail");
        assert!(fin);
    }

    /// The compaction is amortised: the acknowledged prefix is dropped once it
    /// is at least half of what is held, so a window-sized stream never keeps a
    /// window-sized buffer and the bytes moved per byte carried stay bounded.
    /// The regression this pins moved the whole window on every acknowledgement.
    #[test]
    fn compaction_drops_the_acknowledged_prefix_as_it_goes() {
        let mut send = SendBuf::default();
        for block in 0..64u64 {
            let at = send.write(&vec![0x5Au8; 16 * 1024]);
            send.ack(at, 16 * 1024);
            // The prefix is dropped once it reaches half the buffer, so what is
            // held never exceeds twice the unacknowledged bytes.
            let held = u64::try_from(send.held()).unwrap_or(u64::MAX);
            assert!(
                held <= send.unacked() * 2,
                "block {block}: holding {held} bytes for {} unacknowledged",
                send.unacked()
            );
        }
        assert_eq!(send.unacked(), 0);
        assert_eq!(send.held(), 0, "an empty stream holds nothing");
    }
}
