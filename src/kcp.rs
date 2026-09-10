//! Internal implementation of the KCP (ARQ) protocol engine.
//!
//! molehill maintains this module in-repo as its own code: the algorithm
//! follows the reference C implementation by skywind3000
//! (<https://github.com/skywind3000/kcp>) and is deliberately **not** a
//! vendored crate. Self-maintaining keeps three things under molehill's
//! control: the SACK extensions the adapter needs (`Kcp::retransmit_sn`,
//! `Kcp::recv_queue_len`, `Kcp::rcv_nxt_sn`), the dependency graph
//! (nothing external to patch around), and the lint and waiver story (this
//! module is bound by molehill's rules like any other module).
//!
//! Local rules and deliberate deltas vs the reference:
//! - the SACK extensions the adapter uses (`Kcp::retransmit_sn`,
//!   `Kcp::recv_queue_len`, `Kcp::rcv_nxt_sn`) add explicit gap recovery
//!   beyond the reference;
//! - the reference's `IKCP_FASTACK_CONSERVE` semantics are implemented
//!   unconditionally (the reference's shipped default: fast-retransmit
//!   credit is gated on the ack timestamp);
//! - `check()` returns a relative duration instead of the reference's
//!   absolute timestamp (the adapter adds it to its own clock);
//! - the `tokio`-feature code and the unused accessor methods are trimmed
//!   (datagram mode, `set_mtu`, `set_interval`, the conv-adoption hook);
//! - buffers are grown through the safe `BytesMut::zeroed` instead of
//!   `unsafe` length manipulation;
//! - errors are molehill's own `Error` type.
//!
//! The protocol core is a pure state machine: it owns no socket, no clock
//! and no stream semantics. See `src/transport/kcp.rs` for the tokio
//! adapter.

mod error;

pub use error::Error;

/// KCP result
pub type KcpResult<T> = Result<T, Error>;

use std::cmp;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt::{self, Debug};
use std::io::{self, Cursor, Read, Write};

use bytes::{Buf, BufMut, BytesMut};

use log::{debug, trace};

const KCP_RTO_NDL: u32 = 30; // no delay min rto
const KCP_RTO_MIN: u32 = 100; // normal min rto
const KCP_RTO_DEF: u32 = 200;
const KCP_RTO_MAX: u32 = 60000;

const KCP_CMD_PUSH: u8 = 81; // cmd: push data
const KCP_CMD_ACK: u8 = 82; // cmd: ack
const KCP_CMD_WASK: u8 = 83; // cmd: window probe (ask)
const KCP_CMD_WINS: u8 = 84; // cmd: window size (tell)

const KCP_ASK_SEND: u32 = 1; // need to send IKCP_CMD_WASK
const KCP_ASK_TELL: u32 = 2; // need to send IKCP_CMD_WINS

const KCP_WND_SND: u16 = 32;
const KCP_WND_RCV: u16 = 128; // must >= max fragment size

const KCP_MTU_DEF: usize = 1400;

const KCP_INTERVAL: u32 = 100;
/// KCP Header size
pub const KCP_OVERHEAD: usize = 24;
const KCP_DEADLINK: u32 = 20;

const KCP_THRESH_INIT: u16 = 2;
const KCP_THRESH_MIN: u16 = 2;

const KCP_PROBE_INIT: u32 = 5000; // 5 secs to probe window size
const KCP_PROBE_LIMIT: u32 = 120_000; // up to 120 secs to probe window
const KCP_FASTACK_LIMIT: u32 = 5; // max times to trigger fastack

/// Read `conv` from raw buffer
pub fn get_conv(mut buf: &[u8]) -> u32 {
    assert!(buf.len() >= KCP_OVERHEAD);
    buf.get_u32_le()
}

#[inline]
fn bound(lower: u32, v: u32, upper: u32) -> u32 {
    cmp::min(cmp::max(lower, v), upper)
}

#[inline]
/// Mod-2^32 clock difference, reinterpreted as signed (same bit pattern as
/// the reference's `(IINT32)(later - earlier)`).
fn timediff(later: u32, earlier: u32) -> i32 {
    i32::from_le_bytes(later.wrapping_sub(earlier).to_le_bytes())
}

#[derive(Default, Clone, Debug)]
struct KcpSegment {
    conv: u32,
    cmd: u8,
    frg: u8,
    wnd: u16,
    ts: u32,
    sn: u32,
    una: u32,
    resendts: u32,
    rto: u32,
    fastack: u32,
    xmit: u32,
    data: BytesMut,
}

impl KcpSegment {
    fn new_with_data(data: BytesMut) -> Self {
        KcpSegment {
            conv: 0,
            cmd: 0,
            frg: 0,
            wnd: 0,
            ts: 0,
            sn: 0,
            una: 0,
            resendts: 0,
            rto: 0,
            fastack: 0,
            xmit: 0,
            data,
        }
    }

    // The payload length is bounded by `mss` (≤ `mtu` − `KCP_OVERHEAD`), so
    // the u32 wire field cannot truncate.
    #[expect(clippy::cast_possible_truncation)]
    fn encode(&self, buf: &mut BytesMut) {
        assert!(
            buf.remaining_mut() >= self.encoded_len(),
            "REMAIN {} encoded {} {:?}",
            buf.remaining_mut(),
            self.encoded_len(),
            self
        );

        buf.put_u32_le(self.conv);
        buf.put_u8(self.cmd);
        buf.put_u8(self.frg);
        buf.put_u16_le(self.wnd);
        buf.put_u32_le(self.ts);
        buf.put_u32_le(self.sn);
        buf.put_u32_le(self.una);
        buf.put_u32_le(self.data.len() as u32);
        buf.put_slice(&self.data);
    }

    fn encoded_len(&self) -> usize {
        KCP_OVERHEAD + self.data.len()
    }
}

#[derive(Default)]
struct KcpOutput<O>(O);

impl<O: Write> Write for KcpOutput<O> {
    #[inline]
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        trace!("[RO] {} bytes", data.len());
        self.0.write(data)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Session flag bits, packed into one byte: the reference's boolean state
/// fields grouped so the hot state stays compact (and clippy's bool-field
/// limit applies to the packed type, not the protocol struct).
#[derive(Default, Clone, Copy)]
struct KcpFlags(u8);

impl KcpFlags {
    const NODELAY: u8 = 1 << 0;
    const UPDATED: u8 = 1 << 1;
    const NOCWND: u8 = 1 << 2;
    const STREAM: u8 = 1 << 3;

    #[inline]
    fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    #[inline]
    fn set(&mut self, bit: u8, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

/// KCP control
#[derive(Default)]
pub struct Kcp<Output> {
    /// Conversation ID
    conv: u32,
    /// Maximum Transmission Unit
    mtu: usize,
    /// Maximum Segment Size
    mss: usize,
    /// Connection state
    state: i32,

    /// First unacknowledged packet
    snd_una: u32,
    /// Next packet
    snd_nxt: u32,
    /// Next packet to be received
    rcv_nxt: u32,

    /// Congestion window threshold
    ssthresh: u16,

    /// ACK receive variable RTT
    rx_rttval: u32,
    /// ACK receive static RTT
    rx_srtt: u32,
    /// Resend time (calculated by ACK delay time)
    rx_rto: u32,
    /// Minimal resend timeout
    rx_minrto: u32,

    /// Send window
    snd_wnd: u16,
    /// Receive window
    rcv_wnd: u16,
    /// Remote receive window
    rmt_wnd: u16,
    /// Congestion window
    cwnd: u16,
    /// Check window
    /// - `IKCP_ASK_TELL`, telling window size to remote
    /// - `IKCP_ASK_SEND`, ask remote for window size
    probe: u32,

    /// Last update time
    current: u32,
    /// Flush interval
    interval: u32,
    /// Next flush interval
    ts_flush: u32,
    xmit: u32,

    /// Packed session flags (nodelay / updated / nocwnd / stream)
    flags: KcpFlags,

    /// Next check window timestamp
    ts_probe: u32,
    /// Check window wait time
    probe_wait: u32,

    /// Maximum resend time
    dead_link: u32,
    /// Maximum payload size
    incr: usize,

    snd_queue: VecDeque<KcpSegment>,
    rcv_queue: VecDeque<KcpSegment>,
    snd_buf: VecDeque<KcpSegment>,
    rcv_buf: VecDeque<KcpSegment>,

    /// Pending ACK
    acklist: VecDeque<(u32, u32)>,
    buf: BytesMut,

    /// ACK number to trigger fast resend
    fastresend: u32,
    fastlimit: u32,

    output: KcpOutput<Output>,
}

impl<Output> Debug for Kcp<Output> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kcp")
            .field("conv", &self.conv)
            .field("mtu", &self.mtu)
            .field("mss", &self.mss)
            .field("state", &self.state)
            .field("snd_una", &self.snd_una)
            .field("snd_nxt", &self.snd_nxt)
            .field("rcv_nxt", &self.rcv_nxt)
            .field("ssthresh", &self.ssthresh)
            .field("rx_rttval", &self.rx_rttval)
            .field("rx_srtt", &self.rx_srtt)
            .field("rx_rto", &self.rx_rto)
            .field("rx_minrto", &self.rx_minrto)
            .field("snd_wnd", &self.snd_wnd)
            .field("rcv_wnd", &self.rcv_wnd)
            .field("rmt_wnd", &self.rmt_wnd)
            .field("cwnd", &self.cwnd)
            .field("probe", &self.probe)
            .field("current", &self.current)
            .field("interval", &self.interval)
            .field("ts_flush", &self.ts_flush)
            .field("xmit", &self.xmit)
            .field("nodelay", &self.flags.has(KcpFlags::NODELAY))
            .field("updated", &self.flags.has(KcpFlags::UPDATED))
            .field("ts_probe", &self.ts_probe)
            .field("probe_wait", &self.probe_wait)
            .field("dead_link", &self.dead_link)
            .field("incr", &self.incr)
            .field("snd_queue.len", &self.snd_queue.len())
            .field("rcv_queue.len", &self.rcv_queue.len())
            .field("snd_buf.len", &self.snd_buf.len())
            .field("rcv_buf.len", &self.rcv_buf.len())
            .field("acklist.len", &self.acklist.len())
            .field("buf.len", &self.buf.len())
            .field("fastresend", &self.fastresend)
            .field("fastlimit", &self.fastlimit)
            .field("nocwnd", &self.flags.has(KcpFlags::NOCWND))
            .field("stream", &self.flags.has(KcpFlags::STREAM))
            // the output sink is opaque; omit its contents
            .field("output", &"<omitted>")
            .finish()
    }
}

impl<Output> Kcp<Output> {
    /// Creates a KCP control object in stream mode, `conv` must be equal in both endpoints in one connection.
    /// `output` is the callback object for writing.
    ///
    /// `conv` represents conversation.
    pub fn new_stream(conv: u32, output: Output) -> Self {
        Kcp::construct(conv, output, true)
    }

    fn construct(conv: u32, output: Output, stream: bool) -> Self {
        Kcp {
            conv,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            ts_probe: 0,
            probe_wait: 0,
            snd_wnd: KCP_WND_SND,
            rcv_wnd: KCP_WND_RCV,
            rmt_wnd: KCP_WND_RCV,
            cwnd: 0,
            incr: 0,
            probe: 0,
            mtu: KCP_MTU_DEF,
            mss: KCP_MTU_DEF - KCP_OVERHEAD,
            flags: {
                let mut flags = KcpFlags::default();
                flags.set(KcpFlags::STREAM, stream);
                flags
            },

            buf: BytesMut::with_capacity((KCP_MTU_DEF + KCP_OVERHEAD) * 3),

            snd_queue: VecDeque::new(),
            rcv_queue: VecDeque::new(),
            snd_buf: VecDeque::new(),
            rcv_buf: VecDeque::new(),

            state: 0,

            acklist: VecDeque::new(),

            rx_srtt: 0,
            rx_rttval: 0,
            rx_rto: KCP_RTO_DEF,
            rx_minrto: KCP_RTO_MIN,

            current: 0,
            interval: KCP_INTERVAL,
            ts_flush: KCP_INTERVAL,
            ssthresh: KCP_THRESH_INIT,
            fastresend: 0,
            fastlimit: KCP_FASTACK_LIMIT,
            xmit: 0,
            dead_link: KCP_DEADLINK,

            output: KcpOutput(output),
        }
    }

    // move available data from rcv_buf -> rcv_queue
    pub fn move_buf(&mut self) {
        loop {
            let take = match self.rcv_buf.front() {
                Some(seg)
                    if seg.sn == self.rcv_nxt && self.rcv_queue.len() < self.rcv_wnd as usize =>
                {
                    self.rcv_nxt += 1;
                    true
                }
                _ => false,
            };
            if !take {
                break;
            }
            if let Some(seg) = self.rcv_buf.pop_front() {
                self.rcv_queue.push_back(seg);
            }
        }
    }

    /// Receive data from buffer
    pub fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        if self.rcv_queue.is_empty() {
            return Err(Error::RecvQueueEmpty);
        }

        let peeksize = self.peeksize()?;

        if peeksize > buf.len() {
            debug!("recv peeksize={} bufsize={} too small", peeksize, buf.len());
            return Err(Error::UserBufTooSmall);
        }

        let recover = self.rcv_queue.len() >= self.rcv_wnd as usize;

        // Merge fragment
        let mut cur = Cursor::new(buf);
        while let Some(seg) = self.rcv_queue.pop_front() {
            Write::write_all(&mut cur, &seg.data)?;

            trace!("recv sn={}", seg.sn);

            if seg.frg == 0 {
                break;
            }
        }
        assert_eq!(cur.position(), peeksize as u64);

        self.move_buf();

        // fast recover
        if self.rcv_queue.len() < self.rcv_wnd as usize && recover {
            // ready to send back IKCP_CMD_WINS in ikcp_flush
            // tell remote my window size
            self.probe |= KCP_ASK_TELL;
        }

        // `write_all` advanced the cursor by exactly `peeksize` (asserted above)
        Ok(peeksize)
    }

    /// Check buffer size without actually consuming it
    pub fn peeksize(&self) -> KcpResult<usize> {
        match self.rcv_queue.front() {
            Some(segment) => {
                if segment.frg == 0 {
                    return Ok(segment.data.len());
                }

                if self.rcv_queue.len() < usize::from(segment.frg) + 1 {
                    return Err(Error::ExpectingFragment);
                }

                let mut len = 0;

                for segment in &self.rcv_queue {
                    len += segment.data.len();
                    if segment.frg == 0 {
                        break;
                    }
                }

                Ok(len)
            }
            None => Err(Error::RecvQueueEmpty),
        }
    }

    // `frg` fits u8: `count` is checked against `KCP_WND_RCV` (128) below.
    #[expect(clippy::cast_possible_truncation)]
    /// Send bytes into buffer
    pub fn send(&mut self, mut buf: &[u8]) -> KcpResult<usize> {
        let mut sent_size = 0;

        assert!(self.mss > 0);

        // append to previous segment in streaming mode (if possible)
        if self.flags.has(KcpFlags::STREAM) {
            if let Some(old) = self.snd_queue.back_mut() {
                let l = old.data.len();
                if l < self.mss {
                    let capacity = self.mss - l;
                    let extend = cmp::min(buf.len(), capacity);

                    trace!(
                        "send stream mss={} last length={} extend={}",
                        self.mss, l, extend
                    );

                    let (lf, rt) = buf.split_at(extend);
                    old.data.extend_from_slice(lf);
                    buf = rt;

                    old.frg = 0;
                    sent_size += extend;
                }
            }

            if buf.is_empty() {
                return Ok(sent_size);
            }
        }

        let count = buf.len().div_ceil(self.mss);

        if count >= KCP_WND_RCV as usize {
            debug!("send bufsize={} mss={} too large", buf.len(), self.mss);
            // stream mode already extended the queued segment: report the
            // partial progress like the reference instead of erroring (a
            // retry after the error would duplicate the appended bytes)
            if self.flags.has(KcpFlags::STREAM) && sent_size > 0 {
                return Ok(sent_size);
            }
            return Err(Error::UserBufTooBig);
        }

        let count = cmp::max(1, count);

        for i in 0..count {
            let size = cmp::min(self.mss, buf.len());

            let (lf, rt) = buf.split_at(size);

            let mut new_segment = KcpSegment::new_with_data(lf.into());
            buf = rt;

            new_segment.frg = if self.flags.has(KcpFlags::STREAM) {
                0
            } else {
                (count - i - 1) as u8
            };

            self.snd_queue.push_back(new_segment);
            sent_size += size;
        }

        Ok(sent_size)
    }

    fn update_ack(&mut self, rtt: u32) {
        if self.rx_srtt == 0 {
            self.rx_srtt = rtt;
            self.rx_rttval = rtt / 2;
        } else {
            let delta = rtt.abs_diff(self.rx_srtt);
            self.rx_rttval = (3 * self.rx_rttval + delta) / 4;
            self.rx_srtt = (7 * self.rx_srtt + rtt) / 8;
            if self.rx_srtt < 1 {
                self.rx_srtt = 1;
            }
        }
        let rto = self.rx_srtt + cmp::max(self.interval, 4 * self.rx_rttval);
        self.rx_rto = bound(self.rx_minrto, rto, KCP_RTO_MAX);
    }

    #[inline]
    fn shrink_buf(&mut self) {
        self.snd_una = match self.snd_buf.front() {
            Some(seg) => seg.sn,
            None => self.snd_nxt,
        };
    }

    fn parse_ack(&mut self, sn: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        let mut i = 0;
        while i < self.snd_buf.len() {
            match sn.cmp(&self.snd_buf[i].sn) {
                Ordering::Equal => {
                    self.snd_buf.remove(i);
                    break;
                }
                Ordering::Less => break,
                Ordering::Greater => i += 1,
            }
        }
    }

    fn parse_una(&mut self, una: u32) {
        while let Some(seg) = self.snd_buf.front() {
            if timediff(una, seg.sn) > 0 {
                self.snd_buf.pop_front();
            } else {
                break;
            }
        }
    }

    fn parse_fastack(&mut self, sn: u32, ts: u32) {
        if timediff(sn, self.snd_una) < 0 || timediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        for seg in &mut self.snd_buf {
            if timediff(sn, seg.sn) < 0 {
                break;
            } else if sn != seg.sn && timediff(ts, seg.ts) >= 0 {
                // fastack-conserve (as the reference defines it): an ack
                // earns fast-retransmit credit only for segments it is
                // newer than — stale (reordered) acks do not
                seg.fastack += 1;
            }
        }
    }

    #[inline]
    fn ack_push(&mut self, sn: u32, ts: u32) {
        self.acklist.push_back((sn, ts));
    }

    fn parse_data(&mut self, new_segment: KcpSegment) {
        let sn = new_segment.sn;

        if timediff(sn, self.rcv_nxt + u32::from(self.rcv_wnd)) >= 0
            || timediff(sn, self.rcv_nxt) < 0
        {
            return;
        }

        let mut repeat = false;
        let mut new_index = self.rcv_buf.len();

        for segment in self.rcv_buf.iter().rev() {
            if segment.sn == sn {
                repeat = true;
                break;
            }
            if timediff(sn, segment.sn) > 0 {
                break;
            }
            new_index -= 1;
        }

        if !repeat {
            self.rcv_buf.insert(new_index, new_segment);
        }

        // move available data from rcv_buf -> rcv_queue
        self.move_buf();
    }

    /// Get `conv`
    #[inline]
    pub fn conv(&self) -> u32 {
        self.conv
    }

    // Mod-2^32 clock casts, both guarded: `rtt` by `rtt >= 0`, and the
    // cwnd recompute is clamped to `rmt_wnd` (≤ u16::MAX) right after.
    // The single-pass segment parser deliberately stays one function:
    // splitting it would thread the ack-selection state through helpers
    // without clarifying the protocol loop.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    /// Call this when you received a packet from raw connection
    pub fn input(&mut self, buf: &[u8]) -> KcpResult<usize> {
        let input_size = buf.len();

        trace!("[RI] {} bytes", buf.len());

        if buf.len() < KCP_OVERHEAD {
            debug!(
                "input bufsize={} too small, at least {KCP_OVERHEAD}",
                buf.len()
            );
            return Err(Error::InvalidSegmentSize(buf.len()));
        }

        let mut flag = false;
        let mut max_ack = 0;
        let old_una = self.snd_una;
        let mut latest_ts = 0;

        let mut buf = Cursor::new(buf);
        while buf.remaining() >= KCP_OVERHEAD {
            let conv = buf.get_u32_le();
            if conv != self.conv {
                // like the reference: a mismatched conversation is a hard
                // error — the adapter routes sessions by (addr, conv) via
                // `get_conv` before feeding datagrams to `input`
                debug!("input conv={conv} expected conv={} not match", self.conv);
                return Err(Error::ConvInconsistent(self.conv, conv));
            }

            let cmd = buf.get_u8();
            let frg = buf.get_u8();
            let wnd = buf.get_u16_le();
            let ts = buf.get_u32_le();
            let sn = buf.get_u32_le();
            let una = buf.get_u32_le();
            let len = buf.get_u32_le() as usize;

            if buf.remaining() < len {
                debug!(
                    "input bufsize={input_size} payload length={len} remaining={} not match",
                    buf.remaining()
                );
                return Err(Error::SegmentDataSizeMismatch(len, buf.remaining()));
            }

            match cmd {
                KCP_CMD_PUSH | KCP_CMD_ACK | KCP_CMD_WASK | KCP_CMD_WINS => {}
                _ => {
                    debug!("input cmd={cmd} unrecognized");
                    return Err(Error::UnsupportedCmd(cmd));
                }
            }

            self.rmt_wnd = wnd;

            self.parse_una(una);
            self.shrink_buf();

            let mut has_read_data = false;

            match cmd {
                KCP_CMD_ACK => {
                    let rtt = timediff(self.current, ts);
                    if rtt >= 0 {
                        self.update_ack(rtt as u32);
                    }
                    self.parse_ack(sn);
                    self.shrink_buf();

                    if !flag {
                        flag = true;
                        max_ack = sn;
                        latest_ts = ts;
                    } else if timediff(sn, max_ack) > 0 && timediff(ts, latest_ts) > 0 {
                        // fastack-conserve (as the reference defines it):
                        // only a strictly newer ack replaces the max ack
                        max_ack = sn;
                        latest_ts = ts;
                    }

                    trace!(
                        "input ack: sn={sn} rtt={} rto={}",
                        timediff(self.current, ts),
                        self.rx_rto
                    );
                }
                KCP_CMD_PUSH => {
                    trace!("input psh: sn={sn} ts={ts}");

                    if timediff(sn, self.rcv_nxt + u32::from(self.rcv_wnd)) < 0 {
                        self.ack_push(sn, ts);
                        if timediff(sn, self.rcv_nxt) >= 0 {
                            // `BytesMut::zeroed` grows the payload buffer
                            // safely to the wire-declared length
                            let mut sbuf = BytesMut::zeroed(len);
                            buf.read_exact(&mut sbuf)?;
                            has_read_data = true;

                            let mut segment = KcpSegment::new_with_data(sbuf);

                            segment.conv = conv;
                            segment.cmd = cmd;
                            segment.frg = frg;
                            segment.wnd = wnd;
                            segment.ts = ts;
                            segment.sn = sn;
                            segment.una = una;

                            self.parse_data(segment);
                        }
                    }
                }
                KCP_CMD_WASK => {
                    // ready to send back IKCP_CMD_WINS in ikcp_flush
                    // tell remote my window size
                    trace!("input probe");
                    self.probe |= KCP_ASK_TELL;
                }
                KCP_CMD_WINS => {
                    // Do nothing
                    trace!("input wins: {wnd}");
                }
                _ => unreachable!(),
            }

            // Force skip unread data
            if !has_read_data {
                let next_pos = buf.position() + len as u64;
                buf.set_position(next_pos);
            }
        }

        if flag {
            self.parse_fastack(max_ack, latest_ts);
        }

        if timediff(self.snd_una, old_una) > 0 && self.cwnd < self.rmt_wnd {
            let mss = self.mss;
            if self.cwnd < self.ssthresh {
                self.cwnd += 1;
                self.incr += mss;
            } else {
                if self.incr < mss {
                    self.incr = mss;
                }
                self.incr += (mss * mss) / self.incr + (mss / 16);
                if (self.cwnd as usize + 1) * mss <= self.incr {
                    // self.cwnd += 1;
                    self.cwnd = ((self.incr + mss - 1) / if mss > 0 { mss } else { 1 }) as u16;
                }
            }
            if self.cwnd > self.rmt_wnd {
                self.cwnd = self.rmt_wnd;
                self.incr = self.rmt_wnd as usize * mss;
            }
        }

        Ok(buf.position() as usize)
    }

    // `rcv_queue.len()` < `rcv_wnd` ≤ u16::MAX by the guard below, so the
    // u16 cast cannot truncate.
    #[expect(clippy::cast_possible_truncation)]
    fn wnd_unused(&self) -> u16 {
        if self.rcv_queue.len() < self.rcv_wnd as usize {
            self.rcv_wnd - self.rcv_queue.len() as u16
        } else {
            0
        }
    }

    fn probe_wnd_size(&mut self) {
        // probe window size (if remote window size equals zero)
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = KCP_PROBE_INIT;
                self.ts_probe = self.current + self.probe_wait;
            } else if timediff(self.current, self.ts_probe) >= 0 {
                if self.probe_wait < KCP_PROBE_INIT {
                    self.probe_wait = KCP_PROBE_INIT;
                }

                self.probe_wait += self.probe_wait / 2;

                if self.probe_wait > KCP_PROBE_LIMIT {
                    self.probe_wait = KCP_PROBE_LIMIT;
                }

                self.ts_probe = self.current + self.probe_wait;
                self.probe |= KCP_ASK_SEND;
            }
        } else {
            self.ts_probe = 0;
            self.probe_wait = 0;
        }
    }

    // Mod-2^32 clock casts, both guarded positive: the first by the
    // `timediff(current, ts_flush) >= 0` early return above, the second by
    // the per-segment `diff <= 0` early return.
    #[expect(clippy::cast_sign_loss)]
    /// Determine when you should call `update`.
    /// Returns the relative delay in milliseconds until the next `update`
    /// is due (assuming no `input`/`send` in between) — a duration, unlike
    /// the reference's absolute timestamp.
    pub fn check(&self, current: u32) -> u32 {
        if !self.flags.has(KcpFlags::UPDATED) {
            return 0;
        }

        let mut ts_flush = self.ts_flush;
        let mut resend_due = u32::MAX;

        if timediff(current, ts_flush) >= 10000 || timediff(current, ts_flush) < -10000 {
            ts_flush = current;
        }

        if timediff(current, ts_flush) >= 0 {
            return 0;
        }

        let flush_due = timediff(ts_flush, current) as u32;
        for seg in &self.snd_buf {
            let diff = timediff(seg.resendts, current);
            if diff <= 0 {
                return 0;
            }
            if (diff as u32) < resend_due {
                resend_due = diff as u32;
            }
        }

        let mut minimal = cmp::min(resend_due, flush_due);
        if minimal >= self.interval {
            minimal = self.interval;
        }

        minimal
    }
    // `interval` is clamped to [10, 5000] by the match guards and `resend`
    // is guarded by `resend >= 0`, so both i32→u32 casts are safe.
    #[inline]
    #[expect(clippy::cast_sign_loss)]
    /// Fastest config: `nodelay(true, 20, 2, true)`.
    ///
    /// `nodelay`: default is disable (false)
    /// `interval`: internal update timer interval in millisec, default is 100ms
    /// `resend`: 0:disable fast resend(default), 1:enable fast resend
    /// `nc`: `false`: normal congestion control(default), `true`: disable congestion control
    pub fn set_nodelay(&mut self, nodelay: bool, interval: i32, resend: i32, nc: bool) {
        self.flags.set(KcpFlags::NODELAY, nodelay);
        if nodelay {
            self.rx_minrto = KCP_RTO_NDL;
        } else {
            self.rx_minrto = KCP_RTO_MIN;
        }

        match interval {
            interval if interval < 10 => self.interval = 10,
            interval if interval > 5000 => self.interval = 5000,
            _ => self.interval = interval as u32,
        }

        if resend >= 0 {
            self.fastresend = resend as u32;
        }

        self.flags.set(KcpFlags::NOCWND, nc);
    }

    /// Set `wndsize`
    /// set maximum window size: `sndwnd=32`, `rcvwnd=32` by default
    pub fn set_wndsize(&mut self, sndwnd: u16, rcvwnd: u16) {
        if sndwnd > 0 {
            self.snd_wnd = sndwnd;
        }

        if rcvwnd > 0 {
            self.rcv_wnd = cmp::max(rcvwnd, KCP_WND_RCV);
        }
    }

    /// Get `waitsnd`, how many packet is waiting to be sent
    #[inline]
    pub fn wait_snd(&self) -> usize {
        self.snd_buf.len() + self.snd_queue.len()
    }

    /// Force an immediate retransmit of segment `sn` (adapter SACK
    /// support). Returns whether the segment was still in the send buffer.
    /// The resend bypasses the RTO backoff: the peer explicitly told us it
    /// is missing, so the next flush/update sends it right away.
    pub fn retransmit_sn(&mut self, sn: u32) -> bool {
        for seg in &mut self.snd_buf {
            if seg.sn == sn {
                seg.resendts = self.current;
                return true;
            }
            if timediff(sn, seg.sn) < 0 {
                break;
            }
        }
        false
    }

    /// Number of segments parked in the receive pipeline (out-of-order or
    /// undelivered), for the adapter's gap detection.
    #[inline]
    pub fn recv_queue_len(&self) -> usize {
        self.rcv_buf.len() + self.rcv_queue.len()
    }

    /// The next in-order segment number the receiver expects (the adapter
    /// tells the peer to resend exactly this one).
    #[inline]
    pub fn rcv_nxt_sn(&self) -> u32 {
        self.rcv_nxt
    }

    /// Check if KCP connection is dead (resend times excceeded)
    #[inline]
    pub fn is_dead_link(&self) -> bool {
        self.state != 0
    }
}

impl<Output: Write> Kcp<Output> {
    fn flush_ack_inner(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush acknowledges
        for &(sn, ts) in &self.acklist {
            if self.buf.len() + KCP_OVERHEAD > self.mtu {
                self.output.write_all(&self.buf)?;
                self.buf.clear();
            }
            segment.sn = sn;
            segment.ts = ts;
            segment.encode(&mut self.buf);
        }
        self.acklist.clear();

        Ok(())
    }

    fn flush_probe_commands_inner(&mut self, cmd: u8, segment: &mut KcpSegment) -> KcpResult<()> {
        segment.cmd = cmd;
        if self.buf.len() + KCP_OVERHEAD > self.mtu {
            self.output.write_all(&self.buf)?;
            self.buf.clear();
        }
        segment.encode(&mut self.buf);
        Ok(())
    }

    fn flush_probe_commands(&mut self, segment: &mut KcpSegment) -> KcpResult<()> {
        // flush window probing commands
        if (self.probe & KCP_ASK_SEND) != 0 {
            self.flush_probe_commands_inner(KCP_CMD_WASK, segment)?;
        }

        // flush window probing commands
        if (self.probe & KCP_ASK_TELL) != 0 {
            self.flush_probe_commands_inner(KCP_CMD_WINS, segment)?;
        }
        self.probe = 0;
        Ok(())
    }

    /// Flush pending ACKs
    pub fn flush_ack(&mut self) -> KcpResult<()> {
        if !self.flags.has(KcpFlags::UPDATED) {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        let mut segment = KcpSegment {
            conv: self.conv,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self.flush_ack_inner(&mut segment)
    }

    // `inflight`/`resent` truncation: `snd_nxt - snd_una` is window-gated
    // (≤ `snd_wnd` ≤ u16::MAX), and `change > 0` implies fast retransmit
    // is enabled, so `resent` is `fastresend`, not u32::MAX. `flush` stays
    // one function on purpose — it is the hot path and its steps share the
    // segment/window state; splitting would obscure the flush order.
    #[expect(clippy::cast_possible_truncation, clippy::too_many_lines)]
    /// Flush pending data in buffer.
    pub fn flush(&mut self) -> KcpResult<()> {
        if !self.flags.has(KcpFlags::UPDATED) {
            debug!("flush updated() must be called at least once");
            return Err(Error::NeedUpdate);
        }

        // reference: the timeout ssthresh halves the ack-maintained cwnd as
        // it stood when this flush began, not the min() window below
        let prior_cwnd = self.cwnd;

        let mut segment = KcpSegment {
            conv: self.conv,
            cmd: KCP_CMD_ACK,
            wnd: self.wnd_unused(),
            una: self.rcv_nxt,
            ..Default::default()
        };

        self.flush_ack_inner(&mut segment)?;
        self.probe_wnd_size();
        self.flush_probe_commands(&mut segment)?;

        // calculate window size
        let mut cwnd = cmp::min(self.snd_wnd, self.rmt_wnd);
        if !self.flags.has(KcpFlags::NOCWND) {
            cwnd = cmp::min(self.cwnd, cwnd);
        }

        // move data from snd_queue to snd_buf
        while timediff(self.snd_nxt, self.snd_una + u32::from(cwnd)) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut new_segment) => {
                    new_segment.conv = self.conv;
                    new_segment.cmd = KCP_CMD_PUSH;
                    new_segment.wnd = segment.wnd;
                    new_segment.ts = self.current;
                    new_segment.sn = self.snd_nxt;
                    self.snd_nxt += 1;
                    new_segment.una = self.rcv_nxt;
                    new_segment.resendts = self.current;
                    new_segment.rto = self.rx_rto;
                    new_segment.fastack = 0;
                    new_segment.xmit = 0;
                    self.snd_buf.push_back(new_segment);
                }
                None => break,
            }
        }

        // calculate resent
        let resent = if self.fastresend > 0 {
            self.fastresend
        } else {
            u32::MAX
        };

        let rtomin = if self.flags.has(KcpFlags::NODELAY) {
            0
        } else {
            self.rx_rto >> 3
        };

        let mut lost = false;
        let mut change = 0;

        for snd_segment in &mut self.snd_buf {
            let mut need_send = false;

            if snd_segment.xmit == 0 {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.rto = self.rx_rto;
                snd_segment.resendts = self.current + snd_segment.rto + rtomin;
            } else if timediff(self.current, snd_segment.resendts) >= 0 {
                need_send = true;
                snd_segment.xmit += 1;
                self.xmit += 1;
                if self.flags.has(KcpFlags::NODELAY) {
                    // nodelay steps the RTO by half instead of doubling it
                    let step = snd_segment.rto;
                    snd_segment.rto += step / 2;
                } else {
                    snd_segment.rto += cmp::max(snd_segment.rto, self.rx_rto);
                }
                snd_segment.resendts = self.current + snd_segment.rto;
                lost = true;
            } else if snd_segment.fastack >= resent
                && (snd_segment.xmit <= self.fastlimit || self.fastlimit == 0)
            {
                need_send = true;
                snd_segment.xmit += 1;
                snd_segment.fastack = 0;
                snd_segment.resendts = self.current + snd_segment.rto;
                change += 1;
            }

            if need_send {
                snd_segment.ts = self.current;
                snd_segment.wnd = segment.wnd;
                snd_segment.una = self.rcv_nxt;

                let need = KCP_OVERHEAD + snd_segment.data.len();

                if self.buf.len() + need > self.mtu {
                    self.output.write_all(&self.buf)?;
                    self.buf.clear();
                }

                snd_segment.encode(&mut self.buf);

                if snd_segment.xmit >= self.dead_link {
                    self.state = -1; // (IUINT32)-1
                }
            }
        }

        // Flush all data in buffer
        if !self.buf.is_empty() {
            self.output.write_all(&self.buf)?;
            self.buf.clear();
        }

        // update ssthresh
        if change > 0 {
            let inflight = self.snd_nxt - self.snd_una;
            self.ssthresh = inflight as u16 / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = self.ssthresh + resent as u16;
            self.incr = self.cwnd as usize * self.mss;
        }

        if lost {
            self.ssthresh = prior_cwnd / 2;
            if self.ssthresh < KCP_THRESH_MIN {
                self.ssthresh = KCP_THRESH_MIN;
            }
            self.cwnd = 1;
            self.incr = self.mss;
        }

        if self.cwnd == 0 {
            self.cwnd = 1;
            self.incr = self.mss;
        }

        Ok(())
    }

    /// Update state every 10ms ~ 100ms.
    ///
    /// Or you can ask `check` when to call this again.
    pub fn update(&mut self, current: u32) -> KcpResult<()> {
        self.current = current;

        if !self.flags.has(KcpFlags::UPDATED) {
            self.flags.set(KcpFlags::UPDATED, true);
            self.ts_flush = self.current;
        }

        let mut slap = timediff(self.current, self.ts_flush);

        if !(-10000..10000).contains(&slap) {
            self.ts_flush = self.current;
            slap = 0;
        }

        if slap >= 0 {
            self.ts_flush += self.interval;
            if timediff(self.current, self.ts_flush) >= 0 {
                self.ts_flush = self.current + self.interval;
            }
            self.flush()?;
        }

        Ok(())
    }
}
