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
//! - **Three frame classes.** A reliable stream frame for what must arrive (TCP
//!   forwarding, the control path), an unreliable datagram frame for what must
//!   not wait (UDP forwarding), and an unreliable L3 frame for whole IP packets
//!   (the transparent path, where the inner transport already recovers).
//!
//! A frame never spans two packets. That is what makes a sent packet own a
//! byte range of exactly one stream, so retransmission is "send these bytes at
//! this offset again" and never a rebuild of a frame list.
//!
//! All integers are little-endian.

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

/// flags + connection id + packet number.
pub(crate) const HEAD: usize = 9;
/// largest + bitmap.
pub(crate) const ACK: usize = 12;
/// stream id + offset + length.
pub(crate) const STREAM: usize = 14;
/// stream id + length (an unreliable datagram frame carries no offset).
pub(crate) const DATAGRAM: usize = 6;
/// length only: an L3 frame is one whole IP packet.
pub(crate) const L3: usize = 2;
/// the connection-wide credit, absolute.
pub(crate) const CREDIT: usize = 8;
/// stream id + reason.
pub(crate) const RESET: usize = 5;

/// The most stream bytes a reliable frame can carry.
pub(crate) const MAX_STREAM_PAYLOAD: usize = MAX_DATAGRAM - HEAD - ACK - STREAM;
/// The most bytes an unreliable datagram frame can carry.
pub(crate) const MAX_DATAGRAM_PAYLOAD: usize = MAX_DATAGRAM - HEAD - ACK - DATAGRAM;
/// The most bytes an L3 frame can carry (one inner IP packet, up to its MTU).
pub(crate) const MAX_L3_PAYLOAD: usize = MAX_DATAGRAM - HEAD - ACK - L3;

/// What a packet is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// Acknowledges what arrived and carries nothing else.
    Ack = 0,
    /// Reliable stream bytes at an offset.
    Stream = 1,
    /// One unreliable message, delivered or not.
    Datagram = 2,
    /// One unreliable whole IP packet.
    L3 = 3,
    /// The connection-wide credit this end may still send.
    Credit = 4,
    /// One stream is finished early.
    Reset = 5,
}

impl Kind {
    const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            0 => Some(Self::Ack),
            1 => Some(Self::Stream),
            2 => Some(Self::Datagram),
            3 => Some(Self::L3),
            4 => Some(Self::Credit),
            5 => Some(Self::Reset),
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

/// An acknowledgement block: the highest packet number seen and a bitmap of the
/// 64 below it.
///
/// Bit n set means largest - n - 1 arrived. A receiver further behind than this
/// window is one whose acknowledgement is stale anyway; widening it costs four
/// bytes on every packet and buys nothing a 64-deep window does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Ack {
    /// The highest packet number the receiver has seen.
    pub(crate) largest: u32,
    /// Bit n set means largest - n - 1 arrived.
    pub(crate) bits: u64,
}

impl Ack {
    /// Whether the block covers this packet number.
    pub(crate) fn contains(self, number: u32) -> bool {
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
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AckSet {
    largest: Option<u32>,
    bits: u64,
}

impl AckSet {
    /// Record that this packet number arrived. Answers whether it was news.
    pub(crate) fn insert(&mut self, number: u32) -> bool {
        match self.largest {
            None => {
                self.largest = Some(number);
                self.bits = 0;
                true
            }
            Some(largest) if number > largest => {
                let gap = number - largest;
                self.bits = if gap >= 64 { 0 } else { self.bits << gap };
                if gap <= 64 {
                    self.bits |= 1u64 << (gap - 1);
                }
                self.largest = Some(number);
                true
            }
            Some(largest) => {
                let behind = largest - number;
                if behind == 0 || behind > 64 {
                    return false;
                }
                let bit = 1u64 << (behind - 1);
                let fresh = self.bits & bit == 0;
                self.bits |= bit;
                fresh
            }
        }
    }

    /// The block to put on the wire, or None before anything has arrived.
    pub(crate) const fn packet(&self) -> Option<Ack> {
        match self.largest {
            Some(largest) => Some(Ack {
                largest,
                bits: self.bits,
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
    /// One unreliable message on a stream, delivered or not.
    Datagram {
        /// Which stream.
        id: u32,
        /// The message.
        data: &'a [u8],
    },
    /// One whole IP packet, delivered or not.
    L3 {
        /// The packet.
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
}

impl Frame<'_> {
    /// The frame's own length beyond the packet head and acknowledgement block.
    const fn extra(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Stream { data, .. } => STREAM + data.len(),
            Self::Datagram { data, .. } => DATAGRAM + data.len(),
            Self::L3 { data } => L3 + data.len(),
            Self::Credit { .. } => CREDIT,
            Self::Reset { .. } => RESET,
        }
    }

    const fn kind(&self) -> Kind {
        match self {
            Self::None => Kind::Ack,
            Self::Stream { .. } => Kind::Stream,
            Self::Datagram { .. } => Kind::Datagram,
            Self::L3 { .. } => Kind::L3,
            Self::Credit { .. } => Kind::Credit,
            Self::Reset { .. } => Kind::Reset,
        }
    }
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
    buf[0] = flags;
    buf[1..5].copy_from_slice(&id.to_le_bytes());
    buf[5..9].copy_from_slice(&number.to_le_bytes());
    let mut at = HEAD;
    if let Some(block) = ack {
        buf[at..at + 4].copy_from_slice(&block.largest.to_le_bytes());
        buf[at + 4..at + ACK].copy_from_slice(&block.bits.to_le_bytes());
        at += ACK;
    }
    match frame {
        Frame::None => {}
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
        Frame::Datagram { id, data } => {
            buf[at..at + 4].copy_from_slice(&id.to_le_bytes());
            let len = u16::try_from(data.len()).ok()?;
            buf[at + 4..at + 6].copy_from_slice(&len.to_le_bytes());
            buf[at + DATAGRAM..at + DATAGRAM + data.len()].copy_from_slice(data);
        }
        Frame::L3 { data } => {
            let len = u16::try_from(data.len()).ok()?;
            buf[at..at + 2].copy_from_slice(&len.to_le_bytes());
            buf[at + L3..at + L3 + data.len()].copy_from_slice(data);
        }
        Frame::Credit { connection } => {
            buf[at..at + 8].copy_from_slice(&connection.to_le_bytes());
        }
        Frame::Reset { id, code } => {
            buf[at..at + 4].copy_from_slice(&id.to_le_bytes());
            buf[at + 4] = code as u8;
        }
    }
    Some(total)
}

/// One decoded packet: its identity, its acknowledgement and its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packet<'a> {
    /// Which connection.
    pub(crate) id: u32,
    /// The packet's number, which is what an acknowledgement names.
    pub(crate) number: u32,
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
        let slice = buf.get(at + 4..at + ACK).ok_or(Malformed)?;
        let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
        at += ACK;
        Some(Ack {
            largest,
            bits: u64::from_le_bytes(array),
        })
    } else {
        None
    };
    let payload = |start: usize, len: usize| -> Result<&[u8], Malformed> {
        let slice = buf.get(start..start + len).ok_or(Malformed)?;
        Ok(slice)
    };
    let frame = match kind {
        Kind::Ack => Frame::None,
        Kind::Stream => {
            let stream = bytes(at..at + 4)?;
            let slice = buf.get(at + 4..at + 12).ok_or(Malformed)?;
            let array: [u8; 8] = slice.try_into().map_err(|_| Malformed)?;
            let offset = u64::from_le_bytes(array);
            let len = u16::from_le_bytes([buf[at + 12], buf[at + 13]]) as usize;
            Frame::Stream {
                id: stream,
                offset,
                fin,
                data: payload(at + STREAM, len)?,
            }
        }
        Kind::Datagram => {
            let stream = bytes(at..at + 4)?;
            let len = u16::from_le_bytes([buf[at + 4], buf[at + 5]]) as usize;
            Frame::Datagram {
                id: stream,
                data: payload(at + DATAGRAM, len)?,
            }
        }
        Kind::L3 => {
            let len = u16::from_le_bytes([buf[at], buf[at + 1]]) as usize;
            Frame::L3 {
                data: payload(at + L3, len)?,
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
            let code = ResetCode::from_bits(buf[at + 4]).ok_or(Malformed)?;
            Frame::Reset { id: stream, code }
        }
    };
    Ok(Packet {
        id,
        number,
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
            bits: 0b1010_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0001,
        };
        let data = [1u8, 2, 3, 4, 5];
        let cases = [
            Frame::Stream {
                id: 3,
                offset: 4096,
                fin: true,
                data: &data,
            },
            Frame::Datagram { id: 9, data: &data },
            Frame::L3 { data: &data },
            Frame::Credit {
                connection: 1 << 40,
            },
            Frame::Reset {
                id: 5,
                code: ResetCode::Aborted,
            },
            Frame::None,
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
        // The old window falls out of a 64-deep bitmap after a jump this size.
        assert!(!set.contains(10), "the old window falls out");
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
