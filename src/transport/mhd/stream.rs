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
///
/// The bytes live in a `Vec` that starts at [`Self::drained`], not at the base:
/// acknowledging a prefix only moves `base`, and the prefix is dropped in one
/// pass once it is as large as what is still owed. Draining on every
/// acknowledgement instead would memmove the whole window each time — with a
/// 32 MiB window that is tens of megabytes moved per packet, which measured as
/// 143 CPU-seconds to carry 64 MiB on loopback.
#[derive(Debug, Default)]
pub(crate) struct SendBuf {
    /// Bytes from `drained` up to what the writer has queued.
    data: Vec<u8>,
    /// The absolute offset of `data[0]`: everything below it is gone.
    drained: u64,
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
        let at = self.written();
        self.data.extend_from_slice(bytes);
        at
    }

    /// One past the last byte the writer has queued.
    fn written(&self) -> u64 {
        self.drained + self.data.len() as u64
    }

    /// Record that everything below upto arrived, answering how many bytes the
    /// base moved.
    ///
    /// An acknowledgement past what was written is a peer mistake and is
    /// clamped, never trusted: it would otherwise drop bytes nobody sent.
    pub(crate) fn ack(&mut self, offset: u64, len: usize) -> u64 {
        let written = self.written();
        let start = offset.max(self.base).min(written);
        let end = offset.saturating_add(len as u64).min(written);
        if end <= start {
            return 0;
        }
        let before = self.base;
        self.acked.insert(start, end);
        // Advance only over the run that starts at the base: a hole above it is
        // exactly what a retransmission is for, and stepping past it would drop
        // bytes the peer never acknowledged.
        while let Some(end) = self.acked.remove(&self.base) {
            self.base = end;
            self.sent = self.sent.max(self.base);
        }
        self.compact();
        self.base - before
    }

    /// Drop the acknowledged prefix, but only once it pays for the move.
    ///
    /// The buffer holds `[drained, written)`; the acknowledged part is
    /// `[drained, base)`. Compacting moves the rest, so it is worth doing only
    /// when the prefix is at least half of it — otherwise every acknowledgement
    /// pays a memmove proportional to the window. At that threshold the bytes
    /// moved never exceed the bytes reclaimed, so the amortised cost is one
    /// copy per byte carried rather than one per packet.
    fn compact(&mut self) {
        let wasted = self.base.saturating_sub(self.drained);
        if wasted == 0 || wasted * 2 < self.data.len() as u64 {
            return;
        }
        let drop = usize::try_from(wasted).unwrap_or(self.data.len());
        self.data.drain(..drop.min(self.data.len()));
        self.drained = self.base;
        // Ranges the base has passed can never match it again, and keeping them
        // would grow the table for the connection's whole life.
        self.acked = self.acked.split_off(&self.base);
    }

    /// Finish the stream: no byte past this offset will be written.
    pub(crate) fn finish(&mut self) {
        self.fin_at = Some(self.written());
    }

    /// Whether the writer finished.
    #[cfg(test)]
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
        self.next_offset() < self.written()
    }

    /// Unacknowledged bytes, which is what a credit window is spent on.
    pub(crate) fn unacked(&self) -> u64 {
        self.written().saturating_sub(self.base)
    }

    /// Up to max bytes starting at offset, and whether the range ends the stream.
    ///
    /// None when that offset is not held any more: the peer asked for bytes that
    /// were already acknowledged and compacted away.
    pub(crate) fn range(&self, offset: u64, max: usize) -> Option<(&[u8], bool)> {
        if offset < self.drained {
            return None;
        }
        let start = usize::try_from(offset - self.drained).ok()?;
        let end = (start + max).min(self.data.len());
        let slice = self.data.get(start..end)?;
        let fin = self.fin_at == Some(offset + slice.len() as u64);
        Some((slice, fin))
    }

    /// How many bytes the buffer still holds, the acknowledged prefix that
    /// compaction has not dropped included.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.data.len()
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
