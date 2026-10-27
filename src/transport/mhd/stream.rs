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

use std::collections::BTreeMap;

/// The bytes a stream has written and the peer has not acknowledged.
#[derive(Debug, Default)]
pub(crate) struct SendBuf {
    /// Unacknowledged bytes, starting at base.
    data: Vec<u8>,
    /// The offset of data[0]: everything below it is acknowledged.
    base: u64,
    /// The offset the writer finished at, once it has.
    fin_at: Option<u64>,
    /// How far the wire has already been given these bytes. Without it a sender
    /// would re-send the first unacknowledged range on every pass, which is a
    /// retransmission storm rather than a stream.
    sent: u64,
    /// Ranges the peer acknowledged, keyed by their start offset.
    ///
    /// Acknowledgements arrive per packet and out of order, so the base may only
    /// advance over the contiguous run that starts at it. Advancing to the
    /// highest acknowledged offset instead walks the base over a hole and
    /// destroys the bytes behind it.
    acked: BTreeMap<u64, u64>,
}

impl SendBuf {
    /// Append bytes, answering the offset they start at.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> u64 {
        let at = self.base + self.data.len() as u64;
        self.data.extend_from_slice(bytes);
        at
    }

    /// Record that everything below upto arrived.
    ///
    /// An acknowledgement past what was written is a peer mistake and is
    /// clamped, never trusted: it would otherwise drop bytes nobody sent.
    pub(crate) fn ack(&mut self, offset: u64, len: usize) {
        let written = self.base + self.data.len() as u64;
        let start = offset.max(self.base).min(written);
        let end = offset.saturating_add(len as u64).min(written);
        if end <= start {
            return;
        }
        self.acked.insert(start, end);
        // Advance only over the run that starts at the base: a hole above it is
        // exactly what a retransmission is for, and stepping past it would drop
        // bytes the peer never acknowledged.
        while let Some(end) = self.acked.remove(&self.base) {
            let drop = usize::try_from(end - self.base).unwrap_or(self.data.len());
            self.data.drain(..drop.min(self.data.len()));
            self.base = end;
            self.sent = self.sent.max(self.base);
        }
    }

    /// Finish the stream: no byte past this offset will be written.
    pub(crate) fn finish(&mut self) {
        self.fin_at = Some(self.base + self.data.len() as u64);
    }

    /// Whether the writer finished.
    pub(crate) fn finished(&self) -> bool {
        self.fin_at.is_some()
    }

    /// The offset the next packet should carry: the first byte neither
    /// acknowledged nor already on the wire.
    pub(crate) fn next_offset(&self) -> u64 {
        self.sent.max(self.base)
    }

    /// Record that a range starting here has been handed to the socket.
    pub(crate) fn mark_sent(&mut self, len: usize) {
        self.sent = self.next_offset().saturating_add(len as u64);
    }

    /// Rewind the send point to a range the peer never acknowledged, so the next
    /// pass carries it again.
    pub(crate) fn rewind_to(&mut self, offset: u64) {
        self.sent = offset.max(self.base);
    }

    /// Whether any queued byte has not been on the wire yet.
    pub(crate) fn has_unsent(&self) -> bool {
        self.next_offset() < self.base + self.data.len() as u64
    }

    /// Unacknowledged bytes, which is what a credit window is spent on.
    pub(crate) fn unacked(&self) -> u64 {
        self.data.len() as u64
    }

    /// Up to max bytes starting at offset, and whether the range ends the stream.
    ///
    /// None when that offset is not held any more: the peer asked for bytes that
    /// were already acknowledged.
    pub(crate) fn range(&self, offset: u64, max: usize) -> Option<(&[u8], bool)> {
        if offset < self.base {
            return None;
        }
        let start = usize::try_from(offset - self.base).ok()?;
        let end = (start + max).min(self.data.len());
        let slice = self.data.get(start..end)?;
        let fin = self.fin_at == Some(offset + slice.len() as u64);
        Some((slice, fin))
    }
}

/// The bytes of a stream that have arrived, in and out of order.
#[derive(Debug, Default)]
pub(crate) struct RecvBuf {
    /// Out-of-order pieces, keyed by the offset they start at.
    pieces: BTreeMap<u64, Vec<u8>>,
    /// Everything below this offset has been read out.
    read: u64,
    /// Where the peer said the stream ends, once it did.
    fin_at: Option<u64>,
}

impl RecvBuf {
    /// Take a range. Answers whether it was news.
    ///
    /// A range that starts below the read offset is trimmed, and one that
    /// repeats a range already held is dropped: a retransmission is the same
    /// bytes again, never a different range over the same hole.
    pub(crate) fn insert(&mut self, offset: u64, bytes: &[u8]) -> bool {
        let end = offset + bytes.len() as u64;
        if bytes.is_empty() || end <= self.read {
            return false;
        }
        let (offset, bytes) = if offset < self.read {
            let skip = usize::try_from(self.read - offset).unwrap_or(bytes.len());
            (self.read, &bytes[skip..])
        } else {
            (offset, bytes)
        };
        if self.pieces.contains_key(&offset) {
            return false;
        }
        self.pieces.insert(offset, bytes.to_vec());
        true
    }

    /// Mark the stream finished at this offset.
    pub(crate) fn finish(&mut self, at: u64) {
        self.fin_at = Some(match self.fin_at {
            Some(previous) => previous.min(at),
            None => at,
        });
    }

    /// Drain the contiguous prefix into out, answering how many bytes were added.
    ///
    /// This is the only way bytes leave the buffer, so a caller never sees a
    /// hole: what a lost packet costs is the tail of this stream, and nothing
    /// else.
    pub(crate) fn read_into(&mut self, out: &mut Vec<u8>) -> usize {
        let before = out.len();
        loop {
            let next = self
                .pieces
                .range(self.read..)
                .next()
                .map(|(offset, _)| *offset);
            let Some(offset) = next else { break };
            if offset > self.read {
                break; // a hole: the prefix ends here
            }
            let Some(bytes) = self.pieces.remove(&self.read) else {
                break;
            };
            self.read += bytes.len() as u64;
            out.extend_from_slice(&bytes);
        }
        out.len() - before
    }

    /// The first hole: how many bytes are missing and where the next piece is.
    pub(crate) fn hole(&self) -> Option<(u64, u64)> {
        let (offset, _) = self.pieces.iter().next()?;
        (*offset > self.read).then(|| (*offset - self.read, *offset))
    }

    /// Whether the stream finished and everything up to the end has been read.
    pub(crate) fn is_complete(&self) -> bool {
        self.fin_at == Some(self.read) && self.pieces.is_empty()
    }

    /// The offset the reader will deliver next.
    pub(crate) fn read_offset(&self) -> u64 {
        self.read
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

    /// The property this half exists for: pieces that arrive in any order
    /// reassemble into exactly the stream that was written, and the read offset
    /// advances only over contiguous bytes.
    #[test]
    fn a_stream_assembled_from_shuffled_pieces_is_the_original() {
        let original = stream_bytes(4096);
        let mut recv = RecvBuf::default();
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
            recv.insert(start as u64, &original[start..end]);
        }
        let mut out = Vec::new();
        let read = recv.read_into(&mut out);
        assert_eq!(read, original.len(), "every byte must come out");
        assert_eq!(out, original, "the bytes must be the stream, in order");
    }

    /// A hole stops one stream at the hole and says where the next piece is; it
    /// does not discard what arrived behind it.
    #[test]
    fn a_hole_stops_the_prefix_and_reports_itself() {
        let mut recv = RecvBuf::default();
        assert!(recv.insert(20, b"later"));
        assert!(recv.insert(0, b"first"));
        let mut out = Vec::new();
        assert_eq!(recv.read_into(&mut out), 5);
        assert_eq!(out, b"first");
        assert_eq!(recv.hole(), Some((15, 20)), "the missing range is 5..20");
        assert!(recv.insert(5, &[b'x'; 15]), "the hole can be filled");
        let mut out = Vec::new();
        assert_eq!(recv.read_into(&mut out), 20);
        assert_eq!(out, b"xxxxxxxxxxxxxxxlater");
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
}
