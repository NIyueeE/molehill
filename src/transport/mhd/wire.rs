//! The carrier's own wire format: one datagram, one packet, one or more frames.
//!
//! This is the replacement for the KCP segment format, and the differences are
//! the point of the exercise:
//!
//! - **A packet number for acknowledgement, a byte offset for reassembly.**
//!   KCP numbers segments and reassembles them in order, so one lost segment
//!   stalls every stream above it. Numbering the packet and addressing the
//!   stream by bytes lets a loss hold up exactly one stream.
//! - **A 64-bit acknowledgement bitmap** instead of a segment list: one
//!   u32/u64 pair says what arrived below the largest packet seen.
//! - **One reliable frame class and two connection-level ones.** A stream frame
//!   for what must arrive (every data channel and the control path), a credit
//!   frame for the connection-wide send allowance, and a reset for a stream that
//!   ends early. The two unreliable classes a v5 carrier declared for datagrams
//!   and whole IP packets were never produced by any sender in this dialect; a
//!   UDP service and a transparent claim both ride reliable streams, so the
//!   classes were removed rather than carried as tags nothing sends.
//!
//! A frame never spans two packets. That is what makes a sent packet own a
//! byte range of exactly one stream, so retransmission is "send these bytes at
//! this offset again" and never a rebuild of a frame list.
//!
//! All integers are little-endian.

use std::collections::BTreeSet;
use std::fmt;

/// The largest datagram this carrier sends.
///
/// 1400 bytes fits any path that carries IPv4 over a `PPPoE` link without
/// fragmenting. The carrier does not discover the path MTU by probing here; a
/// path that refuses a datagram makes the sender shrink (see the adapter), and
/// that is cheaper than a probe round trip on every connection.
pub(crate) const MAX_DATAGRAM: usize = 1400;

const KIND_MASK: u8 = 0b0000_0111;
const FLAG_ACK: u8 = 0b0000_1000;
const FLAG_FIN: u8 = 0b0001_0000;
/// Set on a packet that consumes no packet number and is not acknowledged.
///
/// A control packet — a credit update, a reset, a bare acknowledgement, a
/// keepalive — is never retransmitted, so if it took a number it would leave a
/// hole in the peer's cumulative frontier the moment it was lost, and nothing
/// would ever fill it: the frontier would stop advancing and the sender's flight
/// would never retire. The number it carries repeats the last data packet's, and
/// this flag is what tells the receiver not to read it as news — which also means
/// a control packet can never acknowledge a data packet that was in fact lost.
const FLAG_UNNUMBERED: u8 = 0b0010_0000;
/// Set on a packet whose payload is sealed with its stream's own key.
///
/// In the header rather than inferred from lane state, because a stream can be
/// opened while the lane's first handshake is still in flight: one end would
/// decide "sealed" and the other "not", and each would read the other's bytes as
/// the wrong thing. The flag travels with the stream, and — because the header is
/// the AEAD's associated data — the bytes that say "sealed" are the bytes the tag
/// covers, so a peer cannot flip it.
const FLAG_SEALED: u8 = 0b0100_0000;

/// The tag a sealed packet carries.
pub(crate) const SEAL_TAG: usize = 16;

/// flags + connection id + packet number.
pub(crate) const HEAD: usize = 9;
/// largest + frontier + bitmap + the stream prefix block.
///
/// The last twelve bytes are the receiver's own stream frontier: which stream
/// it last took bytes for, and how many of that stream's bytes it now holds
/// without a hole. They exist because the packet-number frontier above cannot
/// carry that fact. A number that was lost is never retransmitted under the
/// same number, so a single real drop stops `below` for the life of the
/// connection; everything that scrolls out of the 64-bit window is then
/// declared lost, its recovery record is eventually forgotten, and the
/// acknowledgement it would have carried lands nowhere. The stream's send
/// buffer then waits on a frontier that can never move — measured at 6.7 GiB
/// resident in a forty-second cell. A byte prefix is monotonic across losses,
/// duplicates and reordering, so it is the one field a stalled number frontier
/// cannot corrupt. It rides every packet, which costs twelve bytes: 0.86 % of a
/// full datagram.
pub(crate) const ACK: usize = 28;
/// stream id + offset + length.
pub(crate) const STREAM: usize = 14;
/// the connection-wide credit, absolute.
pub(crate) const CREDIT: usize = 8;
/// stream id + reason.
pub(crate) const RESET: usize = 5;
/// The datagram frame's fixed part: the layer's `u32` id, then the payload.
pub(crate) const DATAGRAM: usize = 4;
/// The ranges frame's fixed part: stream, the prefix the ranges sit above, a count.
pub(crate) const RANGES: usize = 4 + 8 + 1;
/// One range: how far above the prefix it starts, and how long it is.
pub(crate) const RANGE_ENTRY: usize = 4 + 2;
/// The most ranges one frame names.
///
/// Four, because the point is to cover the holes a receiver is actually holding —
/// usually one or two — and because six bytes each must not crowd a datagram that is
/// otherwise carrying payload. A receiver with more than four holes sends the four
/// lowest; the next one carries the rest.
pub(crate) const MAX_RANGES: usize = 4;

/// The most stream bytes a reliable frame can carry.
pub(crate) const MAX_STREAM_PAYLOAD: usize = MAX_DATAGRAM - HEAD - ACK - STREAM;

/// What a packet is for.
///
/// The discriminants are the wire contract and are deliberately compact: the
/// two unreliable classes a v5 carrier declared — one message and one whole IP
/// packet — were never produced by any sender in this dialect, so they are gone
/// rather than carried as dead tags. A peer from another build of this dialect
/// does not exist (both ends ship together), and an unknown tag is refused by
/// `Kind::from_bits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// Acknowledges what arrived and carries nothing else.
    Ack = 0,
    /// Reliable stream bytes at an offset.
    Stream = 1,
    /// The connection-wide credit this end may still send.
    Credit = 2,
    /// One stream is finished early.
    Reset = 3,
    /// Unreliable payload for a datagram service, addressed by a layer id.
    ///
    /// **The class the v5 dialect was emptied of and this one needs.** A UDP service
    /// has nothing to gain from ordering or from a repair: a datagram that arrives late
    /// is a datagram the application has already given up on, and re-sending it costs
    /// the lane's credit that the *next* datagram needs. So this frame carries no
    /// stream offset, takes no packet number (`FLAG_UNNUMBERED`), never enters the
    /// recovery table and never touches connection credit — the loss is the
    /// application's to absorb, exactly as it would be on the wire without a tunnel.
    ///
    /// `id` is opaque to the carrier: the service layer assigns it per visitor and
    /// reads it back on arrival, so the carrier never learns or cares which socket it
    /// was.
    Datagram = 4,
    /// Byte ranges this end holds **above** the prefix it reports in the
    /// acknowledgement block.
    ///
    /// The one thing the 28-byte block cannot say, and the reason a lossy path
    /// re-sends what the peer already has (`HANDOFF.md` §4.185): a number that was
    /// lost is never re-sent under the same number, so the cumulative frontier stops
    /// there for the connection's life, and the bitmap only reaches sixty-four
    /// numbers behind the largest seen. Everything older is unrepresentable — the
    /// sender cannot retire it, so it keeps the flight closed and re-sends data the
    /// receiver is holding in its reorder buffer.
    ///
    /// A separate frame rather than a longer acknowledgement block on purpose: the
    /// ordinary block stays 28 bytes and this cost is paid only by a packet that has
    /// out-of-order data to name.
    Ranges = 5,
}

impl Kind {
    const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            0 => Some(Self::Ack),
            1 => Some(Self::Stream),
            2 => Some(Self::Credit),
            3 => Some(Self::Reset),
            4 => Some(Self::Datagram),
            5 => Some(Self::Ranges),
            _ => None,
        }
    }
}

/// Why a stream was reset, so the peer can report a truthful reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ResetCode {
    /// The reader went away.
    Aborted = 0,
    /// The writer finished and the peer may forget the stream.
    Finished = 1,
    /// The stream's buffers could not be allocated.
    Refused = 2,
}

impl ResetCode {
    const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            0 => Some(Self::Aborted),
            1 => Some(Self::Finished),
            2 => Some(Self::Refused),
            _ => None,
        }
    }
}

/// An acknowledgement block: a cumulative frontier, the highest packet number
/// seen, and a bitmap of the 64 below it.
///
/// The frontier is what makes a wide flight work. A block that only named the
/// newest 64 numbers could retire nothing older: a sender with ten thousand
/// packets in flight would re-send the oldest of them for the connection's whole
/// life, because no later acknowledgement could ever say they arrived — which is
/// a retransmission storm, not a window. `below` says exactly that: every packet
/// numbered under it has arrived. The bitmap then covers the reordering window
/// just above it, where a genuine hole is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Ack {
    /// The highest packet number the receiver has seen.
    pub(crate) largest: u32,
    /// Every packet numbered below this has arrived.
    pub(crate) below: u32,
    /// Bit n set means largest - n - 1 arrived.
    pub(crate) bits: u64,
    /// The stream whose frontier [`Self::prefix`] describes.
    ///
    /// Meaningless when `prefix` is zero, which is what a receiver that has
    /// taken no stream bytes yet sends.
    pub(crate) stream: u32,
    /// How many of that stream's bytes the receiver holds with no hole.
    ///
    /// Monotonic: it can only grow for a given stream, so a lost or duplicated
    /// acknowledgement costs nothing permanent — the next one carries a value
    /// at least as large.
    pub(crate) prefix: u64,
}

impl Ack {
    /// Whether the block covers this packet number.
    pub(crate) fn contains(self, number: u32) -> bool {
        if number < self.below {
            return true;
        }
        if number == self.largest {
            return true;
        }
        if number > self.largest {
            return false;
        }
        let behind = self.largest - number;
        behind <= 64 && self.bits & (1u64 << (behind - 1)) != 0
    }
}

/// The receiver's view of what has arrived: one acknowledgement under construction.
///
/// `below` is the cumulative frontier — every number unter it has arrived — and
/// `gaps` remembers the numbers above it that arrived out of order, so the
/// frontier can walk forward as holes are filled. The set is bounded: a peer more
/// than [`AckSet::AHEAD`] packets ahead of a hole has that data re-sent rather
/// than remembered, which costs one round trip on a path that bad and no memory
/// at all on a path that is not.
#[derive(Debug, Clone, Default)]
pub(crate) struct AckSet {
    largest: Option<u32>,
    bits: u64,
    /// Every number below this has arrived.
    below: u32,
    /// Numbers above `below` that arrived out of order.
    gaps: BTreeSet<u32>,
}

impl AckSet {
    /// How many out-of-order numbers one receiver may remember.
    const AHEAD: usize = 256;

    /// Record that this packet number arrived. Answers whether it was news.
    pub(crate) fn insert(&mut self, number: u32) -> bool {
        if number < self.below {
            return false;
        }
        let mut fresh = true;
        match self.largest {
            None => {
                self.largest = Some(number);
                self.bits = 0;
            }
            Some(largest) if number > largest => {
                let gap = number - largest;
                self.bits = if gap >= 64 { 0 } else { self.bits << gap };
                if gap <= 64 {
                    self.bits |= 1u64 << (gap - 1);
                }
                self.largest = Some(number);
            }
            Some(largest) => {
                let behind = largest - number;
                if behind == 0 {
                    return false;
                }
                if behind <= 64 {
                    let bit = 1u64 << (behind - 1);
                    fresh = self.bits & bit == 0;
                    self.bits |= bit;
                }
            }
        }
        if number == self.below {
            self.below = self.below.wrapping_add(1);
            while self.gaps.remove(&self.below) {
                self.below = self.below.wrapping_add(1);
            }
        } else if self.gaps.len() < Self::AHEAD {
            self.gaps.insert(number);
        }
        fresh
    }

    /// Every number below this has arrived.
    ///
    /// Read by the stall line, which needs to know what this end is *telling* the
    /// peer rather than what it has received (`HANDOFF.md` §4.107).
    pub(crate) const fn below(&self) -> u32 {
        self.below
    }

    /// The largest number seen, or `None` before anything has.
    pub(crate) const fn largest(&self) -> Option<u32> {
        self.largest
    }

    /// The block to put on the wire, or None before anything has arrived.
    pub(crate) const fn packet(&self) -> Option<Ack> {
        match self.largest {
            Some(largest) => Some(Ack {
                largest,
                below: self.below,
                bits: self.bits,
                // The stream frontier is not the acknowledgement set's
                // business: the connection attaches it, because only the
                // connection knows which stream it last took bytes for.
                stream: 0,
                prefix: 0,
            }),
            None => None,
        }
    }

    /// Whether this packet number has arrived.
    #[cfg(test)]
    pub(crate) fn contains(&self, number: u32) -> bool {
        self.packet().is_some_and(|ack| ack.contains(number))
    }
}

/// The one frame a packet carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    /// Nothing beyond the acknowledgement.
    None,
    /// Reliable bytes at an offset; fin says no byte past this range is coming.
    Stream {
        /// Which stream.
        id: u32,
        /// Where these bytes start in it.
        offset: u64,
        /// Whether this range ends the stream.
        fin: bool,
        /// The bytes.
        data: &'a [u8],
    },
    /// The connection-wide credit this end still allows.
    Credit {
        /// Bytes the peer may still have in flight.
        connection: u64,
    },
    /// One stream ends here, early or done.
    Reset {
        /// Which stream.
        id: u32,
        /// Why.
        code: ResetCode,
    },
    /// Unreliable payload for a datagram service (see [`Kind::Datagram`]).
    Datagram {
        /// The layer's own id for this datagram's destination.
        id: u32,
        /// The bytes, exactly as the application handed them over.
        data: &'a [u8],
    },
    /// Byte ranges this end holds above [`Frame::Ranges::base`] (see [`Kind::Ranges`]).
    ///
    /// `base` is the contiguous prefix the same packet's acknowledgement block
    /// reports, carried here as well so the frame is self-describing: a sender that
    /// somehow lost the block can still place the ranges, and a reader does not have
    /// to pair two fields to know what the deltas mean.
    Ranges {
        /// Which stream these ranges belong to.
        stream: u32,
        /// The contiguous frontier the deltas are measured from.
        base: u64,
        /// How many of `entries` are live.
        count: u8,
        /// `(delta above base, length)` per range, lowest first.
        entries: [(u32, u16); MAX_RANGES],
    },
}

impl Frame<'_> {
    /// The frame's own length beyond the packet head and acknowledgement block.
    const fn extra(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Stream { data, .. } => STREAM + data.len(),
            Self::Credit { .. } => CREDIT,
            Self::Reset { .. } => RESET,
            Self::Datagram { data, .. } => DATAGRAM + data.len(),
            Self::Ranges { count, .. } => RANGES + RANGE_ENTRY * (*count as usize),
        }
    }

    const fn kind(&self) -> Kind {
        match self {
            Self::None => Kind::Ack,
            Self::Stream { .. } => Kind::Stream,
            Self::Credit { .. } => Kind::Credit,
            Self::Reset { .. } => Kind::Reset,
            Self::Datagram { .. } => Kind::Datagram,
            Self::Ranges { .. } => Kind::Ranges,
        }
    }
}

impl Frame<'_> {
    /// A ranges frame from a slice of `(delta, length)`, refusing what will not fit.
    ///
    /// The constructor is what keeps `count` honest: an encoder that trusted a caller
    /// to count its own entries would write ranges the decoder cannot read. More than
    /// [`MAX_RANGES`] are truncated to the lowest ones, which is the useful half —
    /// the lowest hole is the one holding the frontier back.
    pub(crate) fn ranges(
        stream: u32,
        base: u64,
        ranges: impl IntoIterator<Item = (u32, u16)>,
    ) -> Self {
        let mut entries = [(0u32, 0u16); MAX_RANGES];
        let mut count = 0u8;
        for (delta, len) in ranges {
            if len == 0 || usize::from(count) == MAX_RANGES {
                continue;
            }
            entries[usize::from(count)] = (delta, len);
            count = count.saturating_add(1);
        }
        Self::Ranges {
            stream,
            base,
            count,
            entries,
        }
    }

    /// Whether `[start, end)` is named by this frame's ranges.
    ///
    /// Wholly inside, never overlapping: a range the peer partly holds is still owed
    /// from its own first byte, and retiring a record on a partial match would leave a
    /// hole nothing could describe — the same rule the byte prefix follows.
    ///
    /// `cfg(test)` because the sender applies this rule inside
    /// `Recovery::confirm_range`, against packet records rather than byte offsets; this
    /// is the statement of it that a wire test can check without building a connection.
    #[cfg(test)]
    pub(crate) fn covers(&self, start: u64, end: u64) -> bool {
        let Self::Ranges {
            base,
            count,
            entries,
            ..
        } = self
        else {
            return false;
        };
        entries
            .iter()
            .take(usize::from(*count))
            .any(|(delta, len)| {
                let at = base.saturating_add(u64::from(*delta));
                let to = at.saturating_add(u64::from(*len));
                start >= at && end <= to
            })
    }
}

/// A ranges frame's body: the stream, the base, a count, then that many entries.
///
/// Its own function because `decode` is at the line budget, and because the validation
/// is half the point: a count the array cannot hold, or a zero-length range — which is
/// what a truncated frame looks like — is refused rather than read as data.
fn ranges_frame(buf: &[u8], at: usize) -> Result<Frame<'_>, Malformed> {
    let stream = buf.get(at..at + 4).ok_or(Malformed)?;
    let stream = u32::from_le_bytes(stream.try_into().map_err(|_| Malformed)?);
    let base = buf.get(at + 4..at + 12).ok_or(Malformed)?;
    let base = u64::from_le_bytes(base.try_into().map_err(|_| Malformed)?);
    let count = buf.get(at + 12).copied().ok_or(Malformed)?;
    if usize::from(count) > MAX_RANGES {
        return Err(Malformed);
    }
    let mut entries = [(0u32, 0u16); MAX_RANGES];
    for (index, entry) in entries.iter_mut().take(usize::from(count)).enumerate() {
        let start = at + RANGES + index * RANGE_ENTRY;
        let delta = buf.get(start..start + 4).ok_or(Malformed)?;
        let delta = u32::from_le_bytes(delta.try_into().map_err(|_| Malformed)?);
        let len = buf.get(start + 4..start + RANGE_ENTRY).ok_or(Malformed)?;
        let len = u16::from_le_bytes(len.try_into().map_err(|_| Malformed)?);
        if len == 0 {
            return Err(Malformed);
        }
        *entry = (delta, len);
    }
    Ok(Frame::Ranges {
        stream,
        base,
        count,
        entries,
    })
}

/// A datagram frame's body: a fixed-width id, then whatever bytes are left.
///
/// Its own function because `decode` is at the line budget. There is no length field on
/// purpose: a datagram has no offset and nothing to reassemble, so a truncated one is
/// simply a shorter datagram — which is what "unreliable" means at this layer.
fn datagram_frame(buf: &[u8], at: usize) -> Result<Frame<'_>, Malformed> {
    let end = at.checked_add(DATAGRAM).ok_or(Malformed)?;
    let id = buf.get(at..end).ok_or(Malformed)?;
    let id = u32::from_le_bytes(id.try_into().map_err(|_| Malformed)?);
    let data = buf.get(end..).ok_or(Malformed)?;
    Ok(Frame::Datagram { id, data })
}

/// A packet that could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Malformed;

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the datagram is not one of this carrier's packets")
    }
}

impl std::error::Error for Malformed {}

/// Write one packet into buf, answering its length.
///
/// The caller sizes buf for the largest packet it may build; a frame that would
/// not fit is the caller's mistake, and this refuses rather than truncating a
/// datagram nobody could decode.
pub(crate) fn encode(
    buf: &mut [u8],
    id: u32,
    number: u32,
    ack: Option<Ack>,
    frame: Frame<'_>,
) -> Option<usize> {
    let head = HEAD + ack.map_or(0, |_| ACK);
    let total = head + frame.extra();
    if total > buf.len() || total > MAX_DATAGRAM {
        return None;
    }
    let mut flags = frame.kind() as u8;
    if ack.is_some() {
        flags |= FLAG_ACK;
    }
    if matches!(
        frame,
        Frame::None
            | Frame::Credit { .. }
            | Frame::Reset { .. }
            | Frame::Datagram { .. }
            | Frame::Ranges { .. }
    ) {
        // Nothing here is retransmitted, so nothing here may hold a number the
        // peer's frontier would wait for (see `FLAG_UNNUMBERED`).
        flags |= FLAG_UNNUMBERED;
    }
    buf[0] = flags;
    buf[1..5].copy_from_slice(&id.to_le_bytes());
    buf[5..9].copy_from_slice(&number.to_le_bytes());
    let mut at = HEAD;
    if let Some(block) = ack {
        buf[at..at + 4].copy_from_slice(&block.largest.to_le_bytes());
        buf[at + 4..at + 8].copy_from_slice(&block.below.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&block.bits.to_le_bytes());
        buf[at + 16..at + 20].copy_from_slice(&block.stream.to_le_bytes());
        buf[at + 20..at + ACK].copy_from_slice(&block.prefix.to_le_bytes());
        at += ACK;
    }
    match frame {
        Frame::None => {}
        Frame::Datagram { id: layer, data } => {
            buf[at..at + 4].copy_from_slice(&layer.to_le_bytes());
            buf[at + 4..at + 4 + data.len()].copy_from_slice(data);
        }
        Frame::Stream {
            id,
            offset,
            fin,
            data,
        } => {
            if fin {
                buf[0] |= FLAG_FIN;
            }
            buf[at..at + 4].copy_from_slice(&id.to_le_bytes());
            buf[at + 4..at + 12].copy_from_slice(&offset.to_le_bytes());
            let len = u16::try_from(data.len()).ok()?;
            buf[at + 12..at + 14].copy_from_slice(&len.to_le_bytes());
            buf[at + STREAM..at + STREAM + data.len()].copy_from_slice(data);
        }
        Frame::Credit { connection } => {
            buf[at..at + 8].copy_from_slice(&connection.to_le_bytes());
        }
        Frame::Reset { id, code } => {
            buf[at..at + 4].copy_from_slice(&id.to_le_bytes());
            buf[at + 4] = code as u8;
        }
        Frame::Ranges {
            stream,
            base,
            count,
            entries,
        } => {
            buf[at..at + 4].copy_from_slice(&stream.to_le_bytes());
            buf[at + 4..at + 12].copy_from_slice(&base.to_le_bytes());
            buf[at + 12] = count;
            for (index, (delta, len)) in entries.iter().take(usize::from(count)).enumerate() {
                let entry = at + RANGES + index * RANGE_ENTRY;
                buf[entry..entry + 4].copy_from_slice(&delta.to_le_bytes());
                buf[entry + 4..entry + RANGE_ENTRY].copy_from_slice(&len.to_le_bytes());
            }
        }
    }
    Some(total)
}

/// One reliable stream frame's identity: which stream, where in it, and
/// whether the range ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamRange {
    /// Which stream.
    pub(crate) id: u32,
    /// Where these bytes start in it.
    pub(crate) offset: u64,
    /// Whether this range ends the stream.
    pub(crate) fin: bool,
    /// How many payload bytes the caller appends after this header.
    ///
    /// The payload is not carried here: the stream appends it straight into the
    /// staged run out of its own buffer, which is what lets one packet draw on
    /// more than one chunk of that buffer (see `SendBuf::append_to`).
    pub(crate) len: usize,
}

/// Append one reliable-stream packet to `out`, answering its length.
///
/// The payload is appended straight from the stream's own buffer, so a bulk
/// transfer is copied once on its way out rather than once into a scratch
/// array and again into the batch. The caller sizes `out` for the run it is
/// staging; a frame that would not fit is refused rather than truncated.
pub(crate) fn encode_stream(
    out: &mut Vec<u8>,
    id: u32,
    number: u32,
    ack: Option<Ack>,
    range: StreamRange,
    sealed: bool,
) -> Option<usize> {
    let StreamRange {
        id: stream,
        offset,
        fin,
        len,
    } = range;
    let head = HEAD + ack.map_or(0, |_| ACK);
    // A sealed payload carries its tag inside the datagram, so the room it needs
    // is `len` plus the tag — reserving here is what keeps the encoder from
    // producing a packet the far end would have to truncate.
    let total = head + STREAM + len + if sealed { SEAL_TAG } else { 0 };
    if total > MAX_DATAGRAM {
        return None;
    }
    let start = out.len();
    out.reserve(total);
    out.resize(start + HEAD, 0);
    let mut flags = Kind::Stream as u8;
    if ack.is_some() {
        flags |= FLAG_ACK;
    }
    if fin {
        flags |= FLAG_FIN;
    }
    if sealed {
        flags |= FLAG_SEALED;
    }
    out[start] = flags;
    out[start + 1..start + 5].copy_from_slice(&id.to_le_bytes());
    out[start + 5..start + 9].copy_from_slice(&number.to_le_bytes());
    if let Some(block) = ack {
        out.extend_from_slice(&block.largest.to_le_bytes());
        out.extend_from_slice(&block.below.to_le_bytes());
        out.extend_from_slice(&block.bits.to_le_bytes());
        out.extend_from_slice(&block.stream.to_le_bytes());
        out.extend_from_slice(&block.prefix.to_le_bytes());
    }
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    // The frame's length describes the payload **on the wire**, so a sealed one
    // counts its tag: the receiver carves the last sixteen bytes out of exactly
    // this many. Writing the plaintext length here is a silent mismatch — the
    // peer reads sixteen bytes too few as the body and sixteen bytes of body as
    // the tag, and every sealed packet fails to open.
    let wire_len = len + if sealed { SEAL_TAG } else { 0 };
    let len = u16::try_from(wire_len).ok()?;
    out.extend_from_slice(&len.to_le_bytes());
    Some(total)
}

/// The plaintext a sealed packet may carry, which is the frame's payload minus
/// the tag it has to hold.
pub(crate) const MAX_SEALED_PAYLOAD: usize = MAX_STREAM_PAYLOAD - SEAL_TAG;

/// Append one control packet (anything but a reliable stream frame) to `out`.
///
/// These frames are at most a few dozen bytes, so they are encoded into a
/// stack buffer and appended; only the stream frame, which carries the bulk of
/// every transfer, needs the append-straight-from-its-own-buffer path above.
pub(crate) fn encode_control(
    out: &mut Vec<u8>,
    id: u32,
    number: u32,
    ack: Option<Ack>,
    frame: Frame<'_>,
) -> Option<usize> {
    let mut scratch = [0u8; MAX_DATAGRAM];
    let len = encode(&mut scratch, id, number, ack, frame)?;
    out.extend_from_slice(&scratch[..len]);
    Some(len)
}

/// One decoded packet: its identity, its acknowledgement and its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packet<'a> {
    /// Which connection.
    pub(crate) id: u32,
    /// The packet's number, which is what an acknowledgement names.
    pub(crate) number: u32,
    /// Whether the number repeats another packet's and must not be acknowledged
    /// (see [`FLAG_UNNUMBERED`]).
    pub(crate) unnumbered: bool,
    /// Whether the payload is sealed with its stream's key.
    pub(crate) sealed: bool,
    /// The header bytes a sealed payload is authenticated against: everything
    /// before the payload, exactly as they arrived.
    pub(crate) ad: &'a [u8],
    /// The tag that closes a sealed payload, when there is one.
    pub(crate) tag: Option<&'a [u8]>,
    /// What this packet acknowledges, when it carries a block.
    pub(crate) ack: Option<Ack>,
    /// The one frame it carries.
    pub(crate) frame: Frame<'a>,
}

/// Read one datagram. Anything this carrier did not write is rejected, never
/// guessed at.
pub(crate) fn decode(buf: &[u8]) -> Result<Packet<'_>, Malformed> {
    if buf.len() < HEAD {
        return Err(Malformed);
    }
    let flags = buf[0];
    let kind = Kind::from_bits(flags & KIND_MASK).ok_or(Malformed)?;
    let fin = flags & FLAG_FIN != 0;
    let sealed = flags & FLAG_SEALED != 0;
    let bytes = |range: std::ops::Range<usize>| -> Result<u32, Malformed> {
        let slice = buf.get(range).ok_or(Malformed)?;
        let array: [u8; 4] = slice.try_into().map_err(|_| Malformed)?;
        Ok(u32::from_le_bytes(array))
    };
    let id = bytes(1..5)?;
    let number = bytes(5..9)?;
    let mut at = HEAD;
    let ack = if flags & FLAG_ACK != 0 {
        let largest = bytes(at..at + 4)?;
        let below = bytes(at + 4..at + 8)?;
        let slice = buf.get(at + 8..at + 16).ok_or(Malformed)?;
        let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
        let bits = u64::from_le_bytes(array);
        let stream = bytes(at + 16..at + 20)?;
        let slice = buf.get(at + 20..at + ACK).ok_or(Malformed)?;
        let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
        at += ACK;
        Some(Ack {
            largest,
            below,
            bits,
            stream,
            prefix: u64::from_le_bytes(array),
        })
    } else {
        None
    };
    let payload = |start: usize, len: usize| -> Result<&[u8], Malformed> {
        let slice = buf.get(start..start + len).ok_or(Malformed)?;
        Ok(slice)
    };
    let u16_at = |at: usize| -> Result<u16, Malformed> {
        let slice = buf.get(at..at + 2).ok_or(Malformed)?;
        let array: [u8; 2] = slice.try_into().map_err(|_| Malformed)?;
        Ok(u16::from_le_bytes(array))
    };
    let mut ad: &[u8] = &[];
    let mut tag: Option<&[u8]> = None;
    let frame = match kind {
        Kind::Ack => Frame::None,
        Kind::Stream => {
            let stream = bytes(at..at + 4)?;
            let slice = buf.get(at + 4..at + 12).ok_or(Malformed)?;
            let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
            let offset = u64::from_le_bytes(array);
            let len = usize::from(u16_at(at + 12)?);
            // A sealed payload is `len` bytes *including* its tag, and the header
            // before it is what the tag authenticates.
            let payload_at = at + STREAM;
            ad = buf.get(..payload_at).ok_or(Malformed)?;
            let (data, tag_at) = if sealed {
                let body = len.checked_sub(SEAL_TAG).ok_or(Malformed)?;
                (payload(payload_at, body)?, Some(payload_at + body))
            } else {
                (payload(payload_at, len)?, None)
            };
            if let Some(tag_at) = tag_at {
                tag = Some(payload(tag_at, SEAL_TAG)?);
            }
            Frame::Stream {
                id: stream,
                offset,
                fin,
                data,
            }
        }
        Kind::Credit => {
            let slice = buf.get(at..at + 8).ok_or(Malformed)?;
            let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
            Frame::Credit {
                connection: u64::from_le_bytes(array),
            }
        }
        Kind::Reset => {
            let stream = bytes(at..at + 4)?;
            let code = buf.get(at + 4).copied().ok_or(Malformed)?;
            let code = ResetCode::from_bits(code).ok_or(Malformed)?;
            Frame::Reset { id: stream, code }
        }
        Kind::Datagram => datagram_frame(buf, at)?,
        Kind::Ranges => ranges_frame(buf, at)?,
    };
    Ok(Packet {
        id,
        number,
        unnumbered: flags & FLAG_UNNUMBERED != 0,
        sealed,
        ad,
        tag,
        ack,
        frame,
    })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    fn roundtrip<'a>(
        buf: &'a mut [u8; MAX_DATAGRAM],
        id: u32,
        number: u32,
        ack: Option<Ack>,
        frame: Frame<'_>,
    ) -> Packet<'a> {
        let len = encode(buf, id, number, ack, frame).unwrap();
        assert!(len <= MAX_DATAGRAM);
        decode(&buf[..len]).unwrap()
    }

    /// Every frame kind survives the wire with its fields intact.
    #[test]
    fn every_frame_round_trips() {
        let ack = Ack {
            largest: 7,
            below: 2,
            bits: 0b1010_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0001,
            // Non-zero on purpose: a decode that forgot the stream block would
            // still pass every other test with it.
            stream: 42,
            prefix: 9_000_000_000,
        };
        let data = [1u8, 2, 3, 4, 5];
        let cases = [
            Frame::Stream {
                id: 3,
                offset: 4096,
                fin: true,
                data: &data,
            },
            Frame::Credit {
                connection: 1 << 40,
            },
            Frame::Reset {
                id: 5,
                code: ResetCode::Aborted,
            },
            Frame::Datagram {
                id: 0xdead_beef,
                data: &data,
            },
            Frame::None,
            // The frame that carries out-of-order evidence, with a full set of ranges:
            // `count` is the only thing that says how many of `entries` are live, so a
            // decode that ignored it would still pass a one-entry case.
            Frame::ranges(
                7,
                1 << 32,
                [(0u32, 60u16), (1_349, 300), (4_000, 1), (9_000, 1_349)],
            ),
            Frame::ranges(11, 0, std::iter::empty()),
        ];
        for frame in cases {
            let mut buf = [0u8; MAX_DATAGRAM];
            let packet = roundtrip(&mut buf, 0xAB, 42, Some(ack), frame);
            assert_eq!(packet.id, 0xAB);
            assert_eq!(packet.number, 42);
            assert_eq!(packet.ack, Some(ack));
            assert_eq!(packet.frame, frame, "frame {frame:?} must survive");
        }
    }

    /// A range names what sits wholly inside it, and nothing else.
    ///
    /// The rule that keeps a partly-covered record in flight: a range the peer holds only
    /// part of is still owed from its own first byte, and retiring it would leave a hole
    /// nothing could describe — the same rule the byte prefix follows.
    #[test]
    fn a_range_covers_only_what_sits_wholly_inside_it() {
        let frame = Frame::ranges(1, 1_000, [(50u32, 100u16)]);
        assert!(frame.covers(1_050, 1_150), "the range itself");
        assert!(frame.covers(1_060, 1_140), "a record wholly inside it");
        assert!(!frame.covers(1_040, 1_060), "one that starts before it");
        assert!(!frame.covers(1_140, 1_160), "one that ends after it");
        assert!(!frame.covers(1_150, 1_160), "one past its end");
        assert!(
            !Frame::None.covers(0, 1),
            "and no other frame covers anything"
        );
        // A frame with no live entries covers nothing, whatever its array holds.
        assert!(!Frame::ranges(1, 1_000, std::iter::empty()).covers(1_000, 1_001));
    }

    /// A datagram frame takes no packet number and never reaches the recovery table.
    ///
    /// The one property the class exists for: a lost datagram must not leave a hole in
    /// the peer's cumulative frontier, or the sender's flight would never retire and the
    /// next datagram would pay for a repair the application does not want.
    #[test]
    fn a_datagram_frame_is_unnumbered() {
        let data = [7u8; 1200];
        let frame = Frame::Datagram { id: 9, data: &data };
        let mut buf = [0u8; MAX_DATAGRAM];
        let written = encode(&mut buf, 1, 424_242, None, frame).unwrap();
        assert!(
            buf[0] & FLAG_UNNUMBERED != 0,
            "a datagram frame must not consume a packet number"
        );
        let packet = decode(&buf[..written]).unwrap();
        assert_eq!(
            packet.number, 424_242,
            "the number repeats the last data packet"
        );
        assert_eq!(packet.frame, frame);
        // A payload that fills a datagram is the size the class is for.
        assert_eq!(written, HEAD + DATAGRAM + data.len());
    }

    /// A packet without an acknowledgement block is four bytes shorter and says
    /// so, which is how a bare stream frame stays cheap.
    #[test]
    fn a_packet_without_an_acknowledgement_decodes_as_one() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let len = encode(
            &mut buf,
            1,
            1,
            None,
            Frame::Stream {
                id: 1,
                offset: 0,
                fin: false,
                data: b"hello",
            },
        )
        .unwrap();
        assert_eq!(len, HEAD + STREAM + 5);
        let packet = decode(&buf[..len]).unwrap();
        assert_eq!(packet.ack, None);
    }

    /// The bitmap remembers what arrived below the largest number, in any order.
    #[test]
    fn the_acknowledgement_bitmap_remembers_any_order() {
        let mut set = AckSet::default();
        assert!(set.insert(10));
        assert!(set.contains(10));
        assert!(set.insert(8));
        assert!(set.insert(9));
        assert!(!set.insert(9), "a repeated packet is not news");
        assert!(set.contains(8) && set.contains(9) && set.contains(10));
        assert!(!set.contains(7), "nothing was said about 7");
        let block = set.packet().unwrap();
        assert_eq!(block.largest, 10);
        assert!(set.insert(70), "a jump folds the window forward");
        assert!(set.contains(70));
        assert!(
            set.contains(10),
            "a jump of sixty keeps the old window inside the bitmap"
        );
        assert!(
            set.insert(200),
            "a jump past the window drops what was below"
        );
        assert!(
            !set.contains(70),
            "the old window falls out of a 64-deep map"
        );
    }

    /// Anything this carrier did not write is rejected, never guessed at.
    #[test]
    fn malformed_datagrams_are_rejected() {
        assert!(decode(&[]).is_err(), "an empty datagram");
        assert!(decode(&[0u8; 8]).is_err(), "shorter than the head");
        let mut buf = [0u8; MAX_DATAGRAM];
        let len = encode(
            &mut buf,
            1,
            1,
            None,
            Frame::Stream {
                id: 1,
                offset: 0,
                fin: false,
                data: b"hello world",
            },
        )
        .unwrap();
        assert!(decode(&buf[..len - 1]).is_err(), "a truncated payload");
        let mut unknown = buf;
        unknown[0] = 0b0000_0111;
        assert!(
            decode(&unknown[..len]).is_err(),
            "an unknown frame kind is not guessed at"
        );
    }

    /// A packet never grows past the datagram budget, whatever the frame.
    #[test]
    fn the_largest_frame_fits_the_datagram_budget() {
        let payload = [7u8; MAX_STREAM_PAYLOAD];
        let mut buf = [0u8; MAX_DATAGRAM];
        let len = encode(
            &mut buf,
            1,
            1,
            Some(Ack::default()),
            Frame::Stream {
                id: 1,
                offset: 0,
                fin: false,
                data: &payload,
            },
        )
        .unwrap();
        assert_eq!(len, MAX_DATAGRAM);
        let too_big = [7u8; MAX_STREAM_PAYLOAD + 1];
        assert!(
            encode(
                &mut buf,
                1,
                1,
                Some(Ack::default()),
                Frame::Stream {
                    id: 1,
                    offset: 0,
                    fin: false,
                    data: &too_big,
                },
            )
            .is_none(),
            "a frame that would not fit must be refused, not truncated"
        );
    }
}
