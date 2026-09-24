//! Thin tokio adapter around the in-house KCP state machine (`kcp` feature,
//! arm 2 of the transport comparison).
//!
//! The `kcp` module (`src/kcp/`) is a pure state machine implementing the
//! KCP (ARQ) protocol, aligned with the reference C implementation: it owns
//! no socket, no clock and no stream semantics.
//! This module supplies exactly that thin missing layer:
//!
//! ```text
//! UDP socket ⇄ pump task ⇄ Kcp (stream mode) ⇄ bounded channels ⇄ KcpStream
//!                          ▲ check()-driven update timer
//! ```
//!
//! - One **pump task** per session owns the `Kcp` instance. It multiplexes
//!   three event sources: app data from the writer channel, inbound datagrams
//!   (client: a per-session ingress task reading the socket; server: the
//!   listener's dispatcher), and a `Kcp::check`-driven update timer. Every
//!   datagram KCP emits is sent out over the session's UDP socket.
//! - **Backpressure**: the writer side is gated by a semaphore and
//!   `Kcp::wait_snd()` (the ARQ send queue). The reader side never blocks the
//!   pump: undeliverable chunks spill to a local queue and stay in KCP's
//!   receive queue otherwise, shrinking the advertised window until the
//!   reader catches up — parking the pump instead would delay acks and
//!   escalate the peer's RTO into seconds-long stalls. The server dispatcher
//!   hands datagrams to sessions with a blocking send: a lagging session
//!   backpressures through the (burst-sized) kernel socket buffer to the
//!   peer's window rather than losing datagrams in userspace.
//! - **KCP stream mode has no FIN**: a session ends when both stream halves
//!   are dropped (graceful: already-queued data is flushed best-effort
//!   first), when the reader half is dropped, when the peer stops acking
//!   (dead-link after the module's 20-retransmit limit), or on fatal errors.
//!
//! Keepalive (adapter control frames): every session PINGs the peer every
//! 2 s with a 16-byte control datagram (magic-tagged, never fed to the
//! Noise/yamux layers). A PONG that never arrives is the pacer's
//! congestion signal (it cuts the send rate); the echoed RTT itself only
//! gates whether a reply counts as clean. The PING cadence also keeps NAT
//! mappings warm. Death detection is still data-driven: a vanished peer
//! that stops answering is only confirmed when the next write exhausts the
//! ARQ retransmit budget (dead-link after ~20 RTOs), but an idle tunnel no
//! longer goes silent — the TCP arms additionally rely on kernel TCP
//! keepalive.
//!
//! **Fixed protocol parameters** (recorded for the arm-2 comparison, see
//! HANDOFF.md): stream mode; nodelay with a 10 ms interval, fast-resend
//! trigger 2, congestion control disabled (`nc=1`); send window 2048
//! segments (~2.8 MiB in flight), receive window 4096; MTU 1400 (protocol
//! default); dead-link default (20 retransmits); 32 MiB socket buffers.
//! SACK gap notification: 10 ms cooldown, 16-segment pile-up (below the
//! nodelay RTO floor, so the SACK beats the RTO backoff).
//!
//! **IO batching and the zero-copy send path** (userspace and syscall):
//! the engine's datagrams stage in a reusable per-session buffer and
//! cross to the pump as ONE channel message per batch (`DatagramOut` —
//! closed at 32 datagrams, the ~46 KiB staging cap, or the engine's flush
//! boundary). Stream-mode PUSH datagrams take the engine's
//! `write_datagram` boundary instead: the 24-byte header stages while the
//! payload travels as a second iovec that points at the engine segment's
//! own buffer, so the payload is copied zero times between the app write
//! and `sendmmsg`. The reader side hands the engine's own segment buffers
//! to the reader channel BY OWNERSHIP (`recv_owned` freezes each segment
//! in place; no payload copy on the read path), batched into one
//! `ReadBatch` message per ~16 KiB (`deliver_recv`). On Linux the wire
//! path then batches with
//! `recvmmsg`/`sendmmsg` (up to 32 datagrams per syscall, the
//! amortization QUIC stacks get from UDP GSO — see `udp_batch.rs`). All
//! of it is pure amortization: the datagrams and the byte stream are
//! byte-identical to one message per datagram, and the wire format is
//! untouched. Other platforms keep single-datagram calls (with the same
//! userspace batching).
//!
//! **The owned write path** (link L3): the writer channel carries whole
//! owned buffers, and the engine's `send_owned` shares each one per
//! segment (O(1) `Bytes` splits) — no per-segment copy. Over Noise, the
//! record layer encrypts into a fresh buffer and hands the record over by
//! ownership (`AsyncWriteOwned`, the shared owned-write boundary), so the
//! channel boundary is a move rather than a copy; a transport that cannot
//! take owned records (plain TCP) keeps the pooled-buffer path.
//!
//! Security note: KCP provides reliability, not confidentiality. In the
//! arm-2 stack Noise rides **on top** of `KcpStream`
//! (`NoiseStream<KcpStream>`), replacing only the plaintext TCP leg of a
//! tunnel; the tunnel hello/ack and yamux layers are unchanged.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::kcp::{DatagramSink, KCP_OVERHEAD, Kcp, get_conv};
use anyhow::{Context as _, Result, bail};
use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, info, trace, warn};

use crate::transport::udp_batch::Span;

/// KCP flush interval in ms (`nodelay` mode; also the pump's timer floor
/// and the retransmit RTO base). 10 ms is the measured sweet spot:
/// 5 ms halves recovery latency but regressed loopback 8-stream
/// throughput ~3x (busier update timer stealing pump iterations under
/// saturation) for no measurable weak-cell gain.
const KCP_INTERVAL_MS: i32 = 10;
/// Fast-resend trigger: retransmit a segment after this many skipped acks.
const KCP_FAST_RESEND: i32 = 2;
/// Send/receive window in segments. Send window 2048 ≈ 2.8 MiB in flight
/// with MTU 1400 (baseline was 1024 ≈ 1.4 MiB; that capped throughput at
/// BDP/RTT — e.g. ~0.28 Gbps at 40 ms tunnel RTT and ~28 Mbps at 400 ms,
/// both measured — so it has been doubled; see the measured collapse note
/// below). Receive doubles the send window, so a momentary reader backlog
/// does not slam the peer's window to zero (classic KCP zero-window
/// oscillation).
///
/// Much larger windows (2048/8192 segments) burst past the shared listener
/// socket buffers with several sessions and collapsed throughput in every
/// measured cell (rtt10: 0.55 -> 0.35/0.24 Gbps; rtt100: 0.055 -> 0.04);
/// the 32 MiB socket buffers and the adaptive pacer bound bursts, but the
/// window stays at the largest size that keeps 4-session bursts within
/// them (4 × 2.8 MiB = 11.2 MiB send-side).
const KCP_SND_WND: u16 = 2048;
const KCP_RCV_WND: u16 = 4096;
/// Cap on app data queued in the `Kcp` send queue, in segments. Beyond this
/// the pump stops pulling from the writer channel, backpressuring
/// `AsyncWrite`.
const SND_QUEUE_LIMIT: usize = 2 * KCP_SND_WND as usize;
/// One `Kcp::send` call must stay under 128 segments (`UserBufTooBig`), so
/// app writes are chunked well below that.
const SEND_CHUNK: usize = 64 * 1024;
/// Graceful-close budget: after BOTH stream halves are gone, keep pumping
/// the ARQ tail (retransmits until acked) for at most this long.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// Consecutive quiet pump rounds (nothing sent, nothing delivered, queue
/// empty) that end a graceful close early.
const CLOSE_QUIET_ROUNDS: u32 = 3;
/// Socket buffer size for KCP UDP sockets. A full multi-session burst
/// (4 × 2.8 MiB at 2048 segments) lands at once; the kernel default
/// (~208 KiB) drops most of it even on loopback, and every drop escalates
/// that segment's RTO (nodelay x1.5 per retransmit) into seconds-long
/// stalls. 32 MiB absorbs the largest burst with headroom; requests are
/// clamped by the system's `rmem_max`/`wmem_max`.
const KCP_SOCKET_BUF_BYTES: usize = 32 * 1024 * 1024;
/// Inbound app-data channel depth (messages of one `ReadBatch`, at most
/// `COALESCE_LIMIT_BYTES` across its parts, or one segment under reader
/// backpressure); half the ARQ receive window in segments, so reader
/// bursts never force datagram drops at the pump (the spill queue absorbs
/// the rest). While the reader keeps up the depth holds ~16 KiB per
/// message, so a FULLY stalled reader (the peer still sending inside its
/// window) parks up to `INBOUND_CHANNEL_DEPTH` batches ≈ 32 MiB here,
/// against ~2.8 MiB of single-segment messages before coalescing — the
/// pressure check in `deliver_recv` switches the tail of the fill back
/// to one segment per message, so only the already-queued batches are
/// oversized. The parts move by ownership, so this bounds residency,
/// not copies.
const INBOUND_CHANNEL_DEPTH: usize = 2048;
/// Writer → pump channel depth (writes, not bytes).
const OUTBOUND_CHANNEL_DEPTH: usize = 64;
/// Ingress/dispatcher → pump channel depth for inbound datagrams. Sized
/// for one full-window burst (8192 segments) plus slack; the blocking
/// dispatcher parks on a full channel and the 32 MiB kernel buffer absorbs
/// the burst in the meantime.
const DATAGRAM_CHANNEL_DEPTH: usize = 8192;
/// Hard cap on concurrent sessions per listener (a random-conv flood must
/// not grow the session map without bound).
#[cfg(any(feature = "server", test))]
const MAX_LISTENER_SESSIONS: usize = 4096;
/// Max datagrams fed to `Kcp::input` per pump iteration. Bounding the batch
/// keeps delivery + ack flushing interleaved with intake, so the advertised
/// window reflects the post-delivery state instead of a burst-inflated
/// receive queue. Sized generously: a small batch would force extra select
/// rounds (and with tokio's uniform select fairness, steal iterations from
/// the writer arm) at high link rates.
const INPUT_BATCH_LIMIT: usize = 512;
/// Inbound datagram scratch buffer (one datagram is at most one MTU).
const DGRAM_BUF: usize = 2048;
/// Arm-2 MTU (KCP's protocol default; never tuned — see the module docs).
/// Only sizes the send batch's staging buffer.
const KCP_MTU: usize = 1400;
/// Staging capacity of one outbound batch: a full `BATCH` of
/// maximum-size datagrams (MTU + header) plus one datagram of slack, so
/// appending never reallocates inside a batch (≈46 KiB).
const BATCH_STAGE_BYTES: usize =
    crate::transport::udp_batch::BATCH * (KCP_MTU + KCP_OVERHEAD) + KCP_MTU + KCP_OVERHEAD;
/// Reader-side coalescing target: consecutive segments are batched into
/// one channel message up to this many bytes (the mux/yamux/Noise reader
/// above asks for ~24 such segments per frame, so this cuts the
/// reader-channel hops by roughly that factor). The parts move by
/// ownership, so the limit bounds the message's total bytes, not a copy.
const COALESCE_LIMIT_BYTES: usize = 16 * 1024;
/// Part-count bound for one message: the byte budget above usually
/// binds first (~11 MSS-sized parts); this caps the `parts` vector for
/// streams of very small segments.
const MAX_READ_PARTS: usize = 32;
/// Reader-channel permits below which coalescing stops: under reader
/// backpressure each segment flushes on its own again, which keeps the
/// stall residency of the (bounded) channel at the pre-coalescing
/// granularity.
const COALESCE_PRESSURE_PERMITS: usize = 64;
// --- adapter control frames ------------------------------------------
// Datagrams whose 5th byte is CTRL_MAGIC are NOT KCP segments (KCP commands
// live at that offset with values 0x81..=0x84). Shapes (16 B, same order as
// a KCP header so the path is trivial):
//   PING: conv(4) magic(1) 0x01 ts(8) pad(2)
//   PONG: conv(4) magic(1) 0x02 ts(8) pad(2)   (ts echoed from the PING)
//   SACK: conv(4) magic(1) 0x03 sn(4) pad(6)   (resend this segment)
const CTRL_MAGIC: u8 = 0xEE;
const CTRL_PING: u8 = 0x01;
const CTRL_PONG: u8 = 0x02;
const CTRL_SACK: u8 = 0x03;
const CTRL_FRAME_LEN: usize = 16;

fn is_ctrl_frame(pkt: &[u8]) -> bool {
    pkt.len() == CTRL_FRAME_LEN && pkt[4] == CTRL_MAGIC
}

fn ctrl_frame(conv: u32, kind: u8, payload: &[u8]) -> [u8; CTRL_FRAME_LEN] {
    let mut f = [0u8; CTRL_FRAME_LEN];
    f[..4].copy_from_slice(&conv.to_le_bytes());
    f[4] = CTRL_MAGIC;
    f[5] = kind;
    f[6..6 + payload.len().min(8)].copy_from_slice(&payload[..payload.len().min(8)]);
    f
}

fn ctrl_payload(f: &[u8]) -> &[u8] {
    &f[6..14]
}
// --- adaptive send pacing --------------------------------------------
// The crate's `nc=1` disables congestion control entirely; a full-window
// flush (2048 segments ≈ 2.8 MiB) bursts far beyond any path queue and
// gets dropped en masse (netem qdisc limit, router buffers). The pump
// therefore paces outbound datagrams with a token bucket whose rate
// starts at the cap (BDP-matched seeding was tried and regressed
// single-stream throughput — the flush already fills the window, so a
// rate cap only adds bucket latency), is cut on PONG timeouts (path over
// capacity) and is nudged up on sustained clean PONGs — a loss-signal-
// driven stand-in for congestion control.
const PACER_MIN_BPS: f64 = 50.0e6;
const PACER_MAX_BPS: f64 = 12.0e9; // above any single-session link rate
const PACER_BUCKET_BYTES: f64 = 512.0 * 1024.0;
const PACER_DOWN_FACTOR: f64 = 0.75; // PONG timeout
const PACER_UP_FACTOR: f64 = 1.05; // sustained clean PONGs
const KEEPALIVE_MS: u32 = 2000; // PING period (also NAT keep-alive)
const PONG_TIMEOUT: Duration = Duration::from_millis(2500);
/// Gap-notification cooldown and pile-up threshold. The cooldown sits
/// BELOW the nodelay RTO floor (~30 ms) so a SACK beats the RTO backoff —
/// the previous 50 ms only fired after the RTO had already retransmitted,
/// making the extension inert on loss cells (measured: 10 ms / 16 segments
/// gains +9% on `loss1_rtt10`, flat elsewhere, no jitter-cell regression).
/// The pile-up still filters sub-ms loopback reordering.
const SACK_COOLDOWN: Duration = Duration::from_millis(10);
const SACK_PILEUP_SEGMENTS: usize = 16;

struct Pacer {
    tokens: f64,
    last: Instant,
}

impl Pacer {
    fn new(now: Instant) -> Self {
        Self {
            tokens: PACER_BUCKET_BYTES,
            last: now,
        }
    }

    /// Try to allow `n` bytes at `now`; on denial return how long until
    /// enough tokens accrue at `rate_bps`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "datagram sizes are bounded by the 1400-byte MTU, far \
                  below f64's exact integer range"
    )]
    fn allow(&mut self, now: Instant, n: usize, rate_bps: f64) -> Result<(), Duration> {
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * rate_bps / 8.0).min(PACER_BUCKET_BYTES);
        let need = n as f64;
        if self.tokens >= need {
            self.tokens -= need;
            Ok(())
        } else {
            let wait_s = (need - self.tokens) / (rate_bps / 8.0);
            Err(Duration::from_secs_f64(wait_s))
        }
    }
}

// --- pump statistics (opt-in, MOLEHILL_KCP_STATS) -----------------------
//
// The adapter-side half of the KCP attribution tool (the engine counters
// live in `kcp.rs`): what the pump does that the protocol engine cannot
// see — control frames, round counts, and coarse per-phase timings that
// split a segment's userspace cost into intake, delivery, the writer
// drain, the wire drain and the ARQ update. Read by a single periodic
// task when the environment variable is set, exactly like the mux
// framing counters.

/// SACK gap notifications the adapter sent (its own loss-recovery
/// extension, outside the engine).
pub(crate) static KCP_SACKS_SENT: AtomicU64 = AtomicU64::new(0);
/// Completed pump rounds (select iterations): the scheduling granularity
/// of all per-segment work.
pub(crate) static KCP_PUMP_ROUNDS: AtomicU64 = AtomicU64::new(0);
/// Reader-channel messages handed over (several consecutive segments
/// batched into one message — the receive-path amortization; the ratio
/// `datagrams_in / blobs_out` is the hop reduction it achieves).
pub(crate) static KCP_BLOBS_OUT: AtomicU64 = AtomicU64::new(0);
/// Receive-queue segments handed to the reader channel (before batching):
/// the denominator for a per-segment delivery cost.
pub(crate) static KCP_SEGMENTS_DELIVERED: AtomicU64 = AtomicU64::new(0);
/// `recv` calls that found the receive queue empty — the fixed per-round
/// probe every delivery round pays, separated from the data path so the
/// two can be attributed apart.
pub(crate) static KCP_RECV_EMPTY: AtomicU64 = AtomicU64::new(0);
/// Coarse phase durations, in nanoseconds, accumulated across all
/// sessions of this process. `KCP_NS_DELIVER` is the whole delivery
/// phase and stays for continuity; the two splits below carve the spill
/// flush and the receive-queue drain out of it.
pub(crate) static KCP_NS_INPUT: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_DELIVER: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_DELIVER_SPILL: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_DELIVER_RECV: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_WRITER: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_OUTPUT: AtomicU64 = AtomicU64::new(0);
pub(crate) static KCP_NS_UPDATE: AtomicU64 = AtomicU64::new(0);

/// Times one pump phase into its counter on drop, so a `break`/early
/// return mid-phase still books the elapsed time.
struct PhaseTimer<'a> {
    stat: &'a AtomicU64,
    start: Instant,
}

impl<'a> PhaseTimer<'a> {
    fn new(stat: &'a AtomicU64) -> Self {
        PhaseTimer {
            stat,
            start: Instant::now(),
        }
    }
}

impl Drop for PhaseTimer<'_> {
    fn drop(&mut self) {
        let ns = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.stat.fetch_add(ns, Ordering::Relaxed);
    }
}

/// Snapshot of every KCP path counter, engine and adapter alike.
pub(crate) struct KcpStats {
    pub datagrams_in: u64,
    pub datagrams_out: u64,
    pub retransmits: u64,
    pub acks_out: u64,
    pub sacks_sent: u64,
    pub blobs_out: u64,
    pub segments_delivered: u64,
    pub recv_empty: u64,
    pub pump_rounds: u64,
    pub ms_input: f64,
    pub ms_deliver: f64,
    pub ms_deliver_spill: f64,
    pub ms_deliver_recv: f64,
    pub ms_writer: f64,
    pub ms_output: f64,
    pub ms_update: f64,
}

/// Snapshot of the KCP path counters, for the periodic stats line.
pub(crate) fn kcp_stats() -> KcpStats {
    use crate::kcp::kcp_engine_stats;
    #[expect(
        clippy::cast_precision_loss,
        reason = "nanosecond accumulators stay far below 2^53 for any \
                  process lifetime worth measuring"
    )]
    fn ms(stat: &AtomicU64) -> f64 {
        stat.load(Ordering::Relaxed) as f64 / 1e6
    }
    KcpStats {
        datagrams_in: kcp_engine_stats().0,
        datagrams_out: kcp_engine_stats().1,
        retransmits: kcp_engine_stats().2,
        acks_out: kcp_engine_stats().3,
        sacks_sent: KCP_SACKS_SENT.load(Ordering::Relaxed),
        blobs_out: KCP_BLOBS_OUT.load(Ordering::Relaxed),
        segments_delivered: KCP_SEGMENTS_DELIVERED.load(Ordering::Relaxed),
        recv_empty: KCP_RECV_EMPTY.load(Ordering::Relaxed),
        pump_rounds: KCP_PUMP_ROUNDS.load(Ordering::Relaxed),
        ms_input: ms(&KCP_NS_INPUT),
        ms_deliver: ms(&KCP_NS_DELIVER),
        ms_deliver_spill: ms(&KCP_NS_DELIVER_SPILL),
        ms_deliver_recv: ms(&KCP_NS_DELIVER_RECV),
        ms_writer: ms(&KCP_NS_WRITER),
        ms_output: ms(&KCP_NS_OUTPUT),
        ms_update: ms(&KCP_NS_UPDATE),
    }
}

/// Guards the one-time spawn of the stats task (see `spawn_kcp_stats`).
static KCP_STATS_SPAWNED: std::sync::Once = std::sync::Once::new();

/// Spawn the periodic stats line, once per process, when
/// `MOLEHILL_KCP_STATS` is set. The counters are cumulative, so a reader
/// that knows the window (or takes the first and last line of a run)
/// gets per-second rates and cost-per-datagram.
fn spawn_kcp_stats() {
    if std::env::var_os("MOLEHILL_KCP_STATS").is_none() {
        return;
    }
    KCP_STATS_SPAWNED.call_once(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            // A missed tick is not worth catching up on: the counters are
            // cumulative, so a late line still reports the true totals.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let s = kcp_stats();
                info!(
                    datagrams_in = s.datagrams_in,
                    datagrams_out = s.datagrams_out,
                    retransmits = s.retransmits,
                    acks_out = s.acks_out,
                    sacks_sent = s.sacks_sent,
                    blobs_out = s.blobs_out,
                    segments_delivered = s.segments_delivered,
                    recv_empty = s.recv_empty,
                    pump_rounds = s.pump_rounds,
                    ms_input = format!("{:.3}", s.ms_input),
                    ms_deliver = format!("{:.3}", s.ms_deliver),
                    ms_deliver_spill = format!("{:.3}", s.ms_deliver_spill),
                    ms_deliver_recv = format!("{:.3}", s.ms_deliver_recv),
                    ms_writer = format!("{:.3}", s.ms_writer),
                    ms_output = format!("{:.3}", s.ms_output),
                    ms_update = format!("{:.3}", s.ms_update),
                    "kcp-stats: cumulative counters"
                );
            }
        });
    });
}

/// Type-erased `Semaphore::acquire_owned` future (the concrete type is not
/// exported by tokio).
type AcquirePermitFuture = dyn std::future::Future<
        Output = Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>,
    > + Send;

/// One batch of outbound datagrams on its way to the wire: a single
/// contiguous buffer holding the staged datagrams plus the span of each
/// inside it. Batching is pure amortization — the datagrams are
/// byte-identical to one message per datagram; it replaces N channel
/// messages and N `Bytes` allocations with one of each. A PUSH datagram
/// travels as two iovecs — a staged 24-byte header plus the engine
/// segment's own payload buffer by reference ([`Span::Split`]) — so the
/// payload is not copied into the staging buffer either. The pacer may
/// deny individual spans, in which case only that span is dropped and
/// KCP's ARQ re-emits the segment on the next flush.
struct DgramBatch {
    buf: Bytes,
    spans: Vec<Span>,
}

/// One batch of received segments on its way to the reader: the engine's
/// own segment buffers, handed over by ownership (`recv_owned` freezes
/// each segment's `BytesMut` in place), batched into one channel
/// message. No payload copy happens on this path — the parts move by
/// reference count, so a byte is copied once by the engine's `input`
/// parse (its ownership copy) and never again until the reader above
/// consumes it. The byte stream, the delivery order and the
/// window-broadcast timing are identical to one segment per message.
pub(crate) struct ReadBatch {
    /// The segments, in delivery order; each is one complete stream-mode
    /// message (≤ one MSS).
    pub parts: Vec<Bytes>,
}

/// Collects `Kcp` output as whole datagrams: each `Write::write` call from
/// `flush`/`update` is exactly one datagram (at most MTU bytes), staged
/// contiguously, while each [`DatagramSink::write_datagram`] call is one
/// two-iovec datagram (staged header + payload by reference). Datagrams
/// stage in a reusable buffer and travel to the pump as one
/// [`DgramBatch`] per batch — the batch closes at `BATCH` datagrams, at
/// the staging cap, or at the engine's flush boundary (`Write::flush`,
/// which `Kcp::flush` now calls once per flush).
struct DatagramOut {
    tx: mpsc::UnboundedSender<DgramBatch>,
    /// Batch in progress: staged datagrams, back to back.
    staging: BytesMut,
    /// One span per staged datagram, in emission order.
    spans: Vec<Span>,
}

impl DatagramOut {
    fn new(tx: mpsc::UnboundedSender<DgramBatch>) -> Self {
        DatagramOut {
            tx,
            staging: BytesMut::with_capacity(BATCH_STAGE_BYTES),
            spans: Vec::with_capacity(crate::transport::udp_batch::BATCH),
        }
    }

    /// Close the batch in progress: one channel message for the staged
    /// datagrams.
    fn emit(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        // The pump owns the receiver and drops it only together with the
        // `Kcp` instance, so a send error here is unreachable in practice;
        // a dropped batch would still be recovered by ARQ.
        let _ = self.tx.send(DgramBatch {
            buf: std::mem::replace(
                &mut self.staging,
                BytesMut::with_capacity(BATCH_STAGE_BYTES),
            )
            .freeze(),
            spans: std::mem::replace(
                &mut self.spans,
                Vec::with_capacity(crate::transport::udp_batch::BATCH),
            ),
        });
    }
}

impl io::Write for DatagramOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            // Unreachable through the engine (every write carries one
            // datagram), but an empty span would put an empty datagram on
            // the wire, so refuse it here.
            return Ok(0);
        }
        self.staging.extend_from_slice(buf);
        self.spans.push(Span::Staged {
            off: self.staging.len() - buf.len(),
            len: buf.len(),
        });
        crate::kcp::KCP_DATAGRAMS_OUT.fetch_add(1, Ordering::Relaxed);
        if self.spans.len() >= crate::transport::udp_batch::BATCH
            || self.staging.len() >= BATCH_STAGE_BYTES
        {
            self.emit();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // The engine calls this once at the end of every `Kcp::flush`:
        // the natural batch boundary. Between boundaries the size/count
        // caps in `write` keep the message bounded.
        self.emit();
        Ok(())
    }
}

impl DatagramSink for DatagramOut {
    /// One two-iovec datagram: the header stages into the batch buffer,
    /// the payload stays the engine segment's own `Bytes` (an O(1) handle
    /// share — no copy). This is the send path's zero-copy half; the wire
    /// bytes are identical to the packed `write` path above.
    fn write_datagram(&mut self, header: &[u8], payload: &Bytes) -> io::Result<usize> {
        if header.is_empty() {
            // Unreachable through the engine (the header is the fixed
            // 24 bytes), but an empty header would corrupt the framing.
            return Ok(0);
        }
        let hdr_off = self.staging.len();
        self.staging.extend_from_slice(header);
        if payload.is_empty() {
            // A header-only datagram (message mode's empty segment): stage
            // it whole rather than send an empty second iovec.
            self.spans.push(Span::Staged {
                off: hdr_off,
                len: header.len(),
            });
        } else {
            self.spans.push(Span::Split {
                hdr_off,
                payload: payload.clone(),
            });
        }
        crate::kcp::KCP_DATAGRAMS_OUT.fetch_add(1, Ordering::Relaxed);
        if self.spans.len() >= crate::transport::udp_batch::BATCH
            || self.staging.len() >= BATCH_STAGE_BYTES
        {
            self.emit();
        }
        Ok(header.len() + payload.len())
    }
}

/// Apply the fixed arm-2 protocol parameters (see the module docs).
fn configure(kcp: &mut Kcp<DatagramOut>) {
    kcp.set_nodelay(true, KCP_INTERVAL_MS, KCP_FAST_RESEND, true);
    kcp.set_wndsize(KCP_SND_WND, KCP_RCV_WND);
}

/// Request large kernel buffers on a KCP socket (see
/// `KCP_SOCKET_BUF_BYTES`). Tokio does not expose the option on UDP sockets,
/// so this goes through socket2 (the same pattern as `try_set_tcp_keepalive`).
/// Failures are non-fatal: the system cap may be lower, and KCP's ARQ still
/// recovers the occasional drop.
fn tune_socket_buffers(socket: &UdpSocket) {
    let s = socket2::SockRef::from(socket);
    if let Err(e) = s.set_recv_buffer_size(KCP_SOCKET_BUF_BYTES) {
        warn!("Failed to raise the KCP socket receive buffer: {e}");
    }
    if let Err(e) = s.set_send_buffer_size(KCP_SOCKET_BUF_BYTES) {
        warn!("Failed to raise the KCP socket send buffer: {e}");
    }
}

/// Milliseconds since `start`, on KCP's wrapping u32 clock.
#[expect(
    clippy::cast_possible_truncation,
    reason = "KCP's protocol clock is a wrapping u32 of milliseconds; its \
              timediff arithmetic handles the 49-day wraparound by design"
)]
fn ms_now(start: Instant) -> u32 {
    start.elapsed().as_millis() as u32
}

/// Hand one owned write to the engine, sharing it per segment.
///
/// The zero-copy write path (link L3): the buffer the writer handed over
/// becomes the segments' payloads by reference — `send_owned` splits it
/// with O(1) `Bytes` handles, so no per-segment copy is paid. Chunking
/// stays `Kcp::send`-safe (each call under the engine's segment bound).
fn kcp_send_owned_all(kcp: &mut Kcp<DatagramOut>, mut data: Bytes) -> Result<()> {
    while !data.is_empty() {
        let head = if data.len() > SEND_CHUNK {
            data.split_to(SEND_CHUNK)
        } else {
            std::mem::take(&mut data)
        };
        let want = head.len();
        match kcp.send_owned(head) {
            // Stream mode queues the whole chunk or fails; a partial accept
            // would duplicate bytes on resend, so treat it as fatal.
            Ok(n) if n == want => {}
            Ok(_) => bail!("KCP send made partial progress"),
            Err(e) => bail!("KCP send failed: {e}"),
        }
    }
    Ok(())
}

/// Identifies one server-side session in the listener's map.
type SessionKey = (SocketAddr, u32);

/// How a session's pump reaches its UDP socket. `gone_tx` is `Some` for
/// server sessions, so the dispatcher can forget them when the pump exits.
struct SessionNet {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    gone_tx: Option<mpsc::Sender<SessionKey>>,
    key: SessionKey,
}

impl SessionNet {
    async fn send_datagram(&self, d: &[u8]) -> io::Result<()> {
        self.socket.send_to(d, self.peer).await?;
        Ok(())
    }

    async fn announce_gone(self) {
        if let Some(tx) = self.gone_tx {
            let _ = tx.send(self.key).await;
        }
    }
}

/// Feed one datagram to the state machine; returns whether it was accepted.
/// Stray/corrupt datagrams must not kill the session — ARQ recovers anything
/// lost.
fn feed_input(kcp: &mut Kcp<DatagramOut>, pkt: &[u8]) -> bool {
    match kcp.input(pkt) {
        Ok(_) => true,
        Err(e) => {
            trace!("KCP input rejected a datagram: {e}");
            false
        }
    }
}

/// Feed the first datagram plus the rest of the ready backlog, bounded to
/// `INPUT_BATCH_LIMIT` per pump iteration so delivery and ack flushing stay
/// interleaved with intake. Returns whether any datagram was accepted (an
/// ack flush is then due).
fn feed_batch(
    kcp: &mut Kcp<DatagramOut>,
    pkt_rx: &mut mpsc::Receiver<Bytes>,
    first: &[u8],
) -> bool {
    let mut accepted = feed_input(kcp, first);
    let mut batch = 1;
    while batch < INPUT_BATCH_LIMIT {
        match pkt_rx.try_recv() {
            Ok(more) => {
                batch += 1;
                if feed_input(kcp, &more) {
                    accepted = true;
                }
            }
            Err(_) => break,
        }
    }
    accepted
}

/// Outcome of one non-blocking delivery round.
enum Delivery {
    /// Delivery finished this round; `delivered` tells whether any chunk
    /// reached the reader (used by the graceful-close quiescence check).
    Done { delivered: bool },
    /// The reader half is gone; the session must end.
    ReaderGone,
}

/// Where one batched message ended up.
enum BlobFlush {
    /// Handed to the reader channel.
    Sent,
    /// The channel is full; the batch went to the spill queue.
    Spilled,
    /// The reader half is gone.
    Closed,
}

/// Hand the collected parts to the reader channel as ONE message, or
/// spill them when the channel is full (the pump never blocks on the
/// reader). The parts vector is replaced with a full-capacity one, so a
/// busy link reallocates once per message instead of doubling its way
/// there.
fn flush_parts(
    in_tx: &mpsc::Sender<ReadBatch>,
    parts: &mut Vec<Bytes>,
    spill: &mut std::collections::VecDeque<ReadBatch>,
) -> BlobFlush {
    let batch = ReadBatch {
        parts: std::mem::replace(parts, Vec::with_capacity(MAX_READ_PARTS)),
    };
    match in_tx.try_send(batch) {
        Ok(()) => {
            KCP_BLOBS_OUT.fetch_add(1, Ordering::Relaxed);
            BlobFlush::Sent
        }
        Err(TrySendError::Full(batch)) => {
            spill.push_back(batch);
            BlobFlush::Spilled
        }
        Err(TrySendError::Closed(_)) => BlobFlush::Closed,
    }
}

/// Move received app data from the KCP receive queue to the reader channel
/// without ever blocking: batches the bounded channel cannot take spill to
/// a local queue (and stay in KCP's queue otherwise), which shrinks the
/// advertised window until the reader catches up — backpressure through the
/// protocol instead of a parked pump.
///
/// The segments move BY OWNERSHIP: the engine hands each segment's buffer
/// over (`recv_owned` freezes it in place), so no payload copy happens
/// here — this is the read half of the zero-copy route (link L1). While
/// the reader keeps up, consecutive segments (each ≤ one MSS in stream
/// mode) are batched into one message of up to `COALESCE_LIMIT_BYTES` /
/// `MAX_READ_PARTS` parts: the mux/yamux reader above asks for ~24
/// segments per frame, so one message per batch cuts its channel hops by
/// about that factor. Batching changes the message granularity only — the
/// byte stream, the delivery order and the window-broadcast timing are
/// untouched. Under reader backpressure (`in_tx` nearly full) each
/// segment flushes on its own again, keeping the stall residency of the
/// bounded channel at the pre-coalescing granularity.
fn deliver_recv(
    kcp: &mut Kcp<DatagramOut>,
    in_tx: &mpsc::Sender<ReadBatch>,
    spill: &mut std::collections::VecDeque<ReadBatch>,
    parts: &mut Vec<Bytes>,
) -> Delivery {
    let mut delivered = false;

    // Flush the spill queue from earlier rounds first. Taking the batch
    // out and putting it back on a full channel keeps this loop
    // allocation-free (a `front.clone()` of the parts vector would
    // allocate once per spill entry per round).
    {
        let _t = PhaseTimer::new(&KCP_NS_DELIVER_SPILL);
        while let Some(batch) = spill.pop_front() {
            match in_tx.try_send(batch) {
                Ok(()) => delivered = true,
                Err(TrySendError::Full(batch)) => {
                    spill.push_front(batch);
                    break;
                }
                Err(TrySendError::Closed(_)) => return Delivery::ReaderGone,
            }
        }
    }

    // Then drain the KCP receive queue into the channel (or the spill).
    if spill.is_empty() {
        let _t = PhaseTimer::new(&KCP_NS_DELIVER_RECV);
        let mut parts_bytes = 0usize;
        loop {
            match kcp.recv_owned() {
                Err(crate::kcp::Error::RecvQueueEmpty | crate::kcp::Error::ExpectingFragment) => {
                    // The fixed empty probe every delivery round pays:
                    // counted apart from data segments so the two costs
                    // can be attributed separately.
                    KCP_RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                Ok(data) => {
                    KCP_SEGMENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    // A zero-length segment carries no bytes for the
                    // reader (and `poll_read` would skip it): stop here,
                    // the same way the buffer API's `Ok(0)` did.
                    if data.is_empty() {
                        break;
                    }
                    parts_bytes += data.len();
                    parts.push(data);
                    if parts_bytes >= COALESCE_LIMIT_BYTES
                        || parts.len() >= MAX_READ_PARTS
                        || in_tx.capacity() <= COALESCE_PRESSURE_PERMITS
                    {
                        match flush_parts(in_tx, parts, spill) {
                            BlobFlush::Sent => {
                                delivered = true;
                                parts_bytes = 0;
                            }
                            // Channel full: stop delivering this round; the
                            // rest stays in KCP's queue and shrinks the
                            // advertised window.
                            BlobFlush::Spilled => break,
                            BlobFlush::Closed => return Delivery::ReaderGone,
                        }
                    }
                }
                Err(e) => {
                    warn!("KCP recv failed: {e}");
                    break;
                }
            }
        }
        // A partial batch at the loop's end (the receive queue ran dry
        // before the coalescing target was reached).
        if !parts.is_empty() {
            match flush_parts(in_tx, parts, spill) {
                BlobFlush::Sent => delivered = true,
                BlobFlush::Spilled | BlobFlush::Closed => {}
            }
        }
    }

    Delivery::Done { delivered }
}

/// Outcome of draining the writer channel inside one select arm.
enum Drain {
    /// All queued writes were encoded into the ARQ queue (the caller then
    /// flushes once).
    Flushed,
    /// `kcp_send_owned_all` failed midway — the pump must exit.
    Fatal,
}

/// Consume writer messages until the ARQ queue cap or the channel runs dry.
/// One select arm handles the whole batch: tokio's uniform select fairness
/// would otherwise starve the writer arm down to a fraction of the link
/// rate when the inbound channel is permanently ready under load.
///
/// Every message is an owned buffer (a Noise record's ciphertext on the
/// kcp4+noise path, a copied slice on the others), so it is handed to the
/// engine by ownership — `send_owned` shares it per segment and the write
/// path pays no per-segment copy.
fn drain_writer(
    kcp: &mut Kcp<DatagramOut>,
    out_rx: &mut mpsc::UnboundedReceiver<Bytes>,
    out_sem: &Semaphore,
    first: Bytes,
) -> Drain {
    let mut data = first;
    loop {
        if let Err(e) = kcp_send_owned_all(kcp, data) {
            warn!("KCP session send failed: {e:#}");
            return Drain::Fatal;
        }
        // One queued write consumed: hand the capacity permit back to the
        // writer half.
        out_sem.add_permits(1);
        if kcp.wait_snd() >= SND_QUEUE_LIMIT {
            break;
        }
        match out_rx.try_recv() {
            Ok(more) => data = more,
            // Empty: channel drained. Disconnected: the writer half is
            // gone; the next select's `recv()` then returns None and
            // enters the closing branch.
            Err(_) => break,
        }
    }
    Drain::Flushed
}

/// Run event-driven updates that are due right now; returns None when the
/// pump must exit, or a POSITIVE timer delay for the select below. The
/// crate's `check` returns a relative ms value (unlike the C reference's
/// absolute timestamp); a due timer (0-1 ms under load) must be consumed
/// here, otherwise it stays ready and tokio's uniform select fairness lets
/// it steal ~1/3 of the pump iterations from the data paths.
fn update_due(kcp: &mut Kcp<DatagramOut>, start: Instant) -> Option<u32> {
    let mut delay = kcp.check(ms_now(start)).min(1000);
    if delay == 0 {
        if let Err(e) = kcp.update(ms_now(start)) {
            warn!("KCP session update failed: {e}");
            return None;
        }
        delay = kcp.check(ms_now(start)).min(1000);
        if delay == 0 {
            delay = 1;
        }
    }
    Some(delay)
}

/// Adaptive-pacing state shared by the pump's control-frame handling.
struct PaceState {
    rate_bps: f64,
    pacer: Pacer,
    last_ping: Instant,
    last_ping_us: u64,
    ping_outstanding: bool,
    clean_pongs: u32,
}

impl PaceState {
    fn new(start: Instant) -> Self {
        // The pacer starts unthrottled (the cap): BDP-matched clamping was
        // tried and regressed single-stream throughput (the flush burst
        // already fills the window; a rate cap just adds bucket latency).
        // It exists to cut the rate on congestion signals (PONG timeouts).
        Self {
            rate_bps: PACER_MAX_BPS,
            pacer: Pacer::new(start),
            last_ping: start,
            last_ping_us: 0,
            ping_outstanding: false,
            clean_pongs: 0,
        }
    }

    /// PONG for the most recent PING: probe the rate up after a run of
    /// clean PONGs. Timestamps are microseconds — on loopback the RTT is
    /// sub-millisecond, below a ms-resolution clock's reach. The parsed
    /// RTT is only a validity gate here (a PONG with insane timestamps is
    /// a clock wrap or a stray frame, not evidence of a clean path); the
    /// pacing signal is the PONG timeout, not its size.
    #[expect(
        clippy::cast_precision_loss,
        reason = "RTT deltas are bounded by the 5 s sanity window, far \
                  below f64's exact integer range"
    )]
    fn on_pong(&mut self, ping_us: u64, now_us: u64) {
        if ping_us == self.last_ping_us && self.ping_outstanding {
            let rtt_ms = (now_us.saturating_sub(ping_us) as f64) / 1000.0;
            if (0.05..=5000.0).contains(&rtt_ms) {
                self.clean_pongs += 1;
                if self.clean_pongs >= 4 {
                    self.rate_bps = (self.rate_bps * PACER_UP_FACTOR).min(PACER_MAX_BPS);
                    self.clean_pongs = 0;
                }
            }
            self.ping_outstanding = false;
        }
    }

    /// A PING went unanswered past the deadline: the path is over capacity,
    /// cut the send rate (the pacer is the congestion-control stand-in).
    fn on_ping_timeout(&mut self) {
        self.rate_bps = (self.rate_bps * PACER_DOWN_FACTOR).max(PACER_MIN_BPS);
        self.clean_pongs = 0;
        debug!("KCP pacer: PONG timeout, rate -> {:.0} bps", self.rate_bps);
    }
}

/// Handle an adapter control frame (NOT a KCP segment). `start` anchors
/// the pump's microsecond clock for RTT computation.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the microsecond clock wraps after ~584k years of uptime; \
              a truncated value merely yields a bad RTT sample that the \
              sanity window rejects"
)]
async fn handle_ctrl(
    kind: u8,
    payload: &[u8],
    kcp: &mut Kcp<DatagramOut>,
    net: &SessionNet,
    pace: &mut PaceState,
    start: Instant,
) {
    match kind {
        CTRL_PING => {
            // Echo the timestamp back: the peer measures RTT via PONG.
            let f = ctrl_frame(kcp.conv(), CTRL_PONG, payload);
            if let Err(e) = net.send_datagram(&f).await {
                debug!("KCP PONG send failed (peer {}): {e}", net.peer);
            }
        }
        CTRL_PONG => {
            let mut ts = [0u8; 8];
            ts.copy_from_slice(&payload[..8]);
            let now_us = start.elapsed().as_micros() as u64;
            pace.on_pong(u64::from_le_bytes(ts), now_us);
        }
        CTRL_SACK => {
            // A gap notification: the peer is missing segment `sn`, parked
            // behind nothing else (a prefix gap, where fastack can never
            // fire because no ack can advance past it). Resend it out of
            // turn — the immediate recovery path for lossy links.
            let mut sn = [0u8; 4];
            sn.copy_from_slice(&payload[..4]);
            let sn = u32::from_le_bytes(sn);
            if !kcp.retransmit_sn(sn) {
                trace!("KCP SACK for segment {sn} not in the send buffer");
            }
        }
        _ => trace!("KCP unknown control frame kind {kind}"),
    }
}

/// Send the periodic adapter PING (keepalive + RTT probe); when the
/// previous PING went unanswered, cut the pacing rate first (the pacer is
/// the congestion-control stand-in with nc=1).
#[expect(
    clippy::cast_possible_truncation,
    reason = "same microsecond-clock truncation reasoning as handle_ctrl"
)]
async fn maybe_ping(
    kcp: &Kcp<DatagramOut>,
    net: &SessionNet,
    pace: &mut PaceState,
    start: Instant,
) {
    // A PING that went unanswered past the deadline is a congestion
    // signal: cut the rate immediately instead of waiting for the next
    // keepalive tick, and allow an immediate retry.
    if pace.ping_outstanding && pace.last_ping.elapsed() >= PONG_TIMEOUT {
        pace.on_ping_timeout();
        pace.ping_outstanding = false;
    }
    if pace.last_ping.elapsed() < Duration::from_millis(u64::from(KEEPALIVE_MS)) {
        return;
    }
    if pace.ping_outstanding {
        pace.on_ping_timeout();
    }
    pace.last_ping = Instant::now();
    let us = start.elapsed().as_micros() as u64;
    pace.last_ping_us = us;
    pace.ping_outstanding = true;
    let f = ctrl_frame(kcp.conv(), CTRL_PING, &us.to_le_bytes());
    if let Err(e) = net.send_datagram(&f).await {
        debug!("KCP PING send failed (peer {}): {e}", net.peer);
    }
}

/// Drain KCP's outbound datagrams onto the wire, through the pacer. Each
/// staged batch reaches the pump as one channel message; the pacer is
/// consulted per datagram and a denied span is dropped on its own (KCP's
/// ARQ still holds the segment and re-emits it on the next flush), so the
/// drain never blocks the pump and a partial denial costs exactly that
/// datagram, not the rest of the batch. A full kernel send buffer drops
/// the unsent tail the same way.
async fn drain_dgrams(
    dgram_rx: &mut mpsc::UnboundedReceiver<DgramBatch>,
    net: &SessionNet,
    pace: &mut PaceState,
    sent_any: &mut bool,
) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use tokio::io::Interest;

        let mut send_batch = crate::transport::udp_batch::SendBatch::new();
        // Spans the pacer allowed from the batch in hand (= the whole batch
        // unless the rate is currently cut). A `Span::Split` clones the
        // payload's `Bytes` handle (an O(1) refcount share), never the
        // payload itself.
        let mut allowed: Vec<Span> = Vec::with_capacity(crate::transport::udp_batch::BATCH);
        loop {
            let Ok(batch) = dgram_rx.try_recv() else {
                return;
            };
            allowed.clear();
            for span in &batch.spans {
                if pace
                    .pacer
                    .allow(Instant::now(), span.len(), pace.rate_bps)
                    .is_ok()
                {
                    allowed.push(span.clone());
                }
                // denied: drop just this span, ARQ holds the segment
            }
            if allowed.is_empty() {
                return;
            }
            // Park on writability first: this arms tokio's writable
            // interest — a bare `try_io` would only run the closure on
            // *cached* readiness and drop the very first batch. After an
            // EAGAIN, `try_io` clears the cached flag, so this await parks
            // until the kernel send buffer drains — no spinning.
            if net.socket.writable().await.is_err() {
                return;
            }
            match net.socket.try_io(Interest::WRITABLE, || {
                send_batch.send_spans(net.socket.as_raw_fd(), net.peer, &batch.buf, &allowed)
            }) {
                Ok(k) => {
                    *sent_any = true;
                    if k < allowed.len() {
                        // Partial send: drop the rest — KCP's ARQ re-emits
                        // them on the next flush.
                        return;
                    }
                }
                // WouldBlock: kernel send buffer full; the datagrams are
                // dropped and KCP's ARQ re-emits them on the next flush.
                Err(e) => {
                    debug!("KCP datagram send failed (peer {}): {e}", net.peer);
                    return;
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No sendmmsg here, so a two-iovec (split) datagram is reassembled
        // into one buffer before the single-datagram send — the copy this
        // platform pays, byte-identical on the wire.
        let mut dgram_buf = [0u8; KCP_MTU + KCP_OVERHEAD];
        while let Ok(batch) = dgram_rx.try_recv() {
            for span in &batch.spans {
                let (header, payload): (&[u8], &[u8]) = match span {
                    Span::Staged { off, len } => (&batch.buf[*off..*off + *len], &[]),
                    Span::Split { hdr_off, payload } => (
                        &batch.buf[*hdr_off..*hdr_off + KCP_OVERHEAD],
                        payload.as_ref(),
                    ),
                };
                let total = header.len() + payload.len();
                if total > dgram_buf.len() {
                    continue; // cannot happen: one datagram is at most one MTU
                }
                match pace.pacer.allow(Instant::now(), total, pace.rate_bps) {
                    Ok(()) => {
                        // Same park-then-try pattern as the Linux path.
                        if net.socket.writable().await.is_err() {
                            return;
                        }
                        dgram_buf[..header.len()].copy_from_slice(header);
                        dgram_buf[header.len()..total].copy_from_slice(payload);
                        match net.socket.try_send_to(&dgram_buf[..total], net.peer) {
                            Ok(_) => *sent_any = true,
                            // WouldBlock: drop and let the ARQ re-emit on the
                            // next flush; tokio's try_send_to clears the
                            // cached writability so the next park waits.
                            Err(e) => {
                                debug!("KCP datagram send failed (peer {}): {e}", net.peer);
                            }
                        }
                    }
                    // denied: drop just this span
                    Err(_) => {}
                }
            }
        }
    }
}

/// Whether a half-closed session is done: the ARQ tail is quiescent and
/// the close budget is spent (or the reader is already gone).
fn closing_quiescent(
    sent_any: bool,
    delivered: bool,
    kcp: &Kcp<DatagramOut>,
    spill: &std::collections::VecDeque<ReadBatch>,
    quiet_rounds: &mut u32,
    close_deadline: Instant,
) -> bool {
    // Quiet = ARQ queue empty and nothing emitted or delivered:
    // everything KCP accepted was acked (or the peer vanished, which the
    // deadline bounds).
    if !sent_any && !delivered && kcp.wait_snd() == 0 && spill.is_empty() {
        *quiet_rounds += 1;
    } else {
        *quiet_rounds = 0;
    }
    *quiet_rounds >= CLOSE_QUIET_ROUNDS || Instant::now() >= close_deadline
}

/// One pump round's head: run due updates and the keepalive PING; returns
/// the timer delay for the select arm (None = the pump must exit).
async fn pump_head(
    kcp: &mut Kcp<DatagramOut>,
    net: &SessionNet,
    pace: &mut PaceState,
    start: Instant,
) -> Option<u32> {
    let delay = update_due(kcp, start)?;
    maybe_ping(kcp, net, pace, start).await;
    Some(delay)
}

/// Flush acks for a consumed input batch, then fill the newly-opened
/// window slots straight from `snd_queue` — waiting for the next writer
/// message or timer tick would idle a full RTT at large RTTs.
fn flush_after_batch(kcp: &mut Kcp<DatagramOut>) -> crate::kcp::KcpResult<()> {
    kcp.flush_ack()?;
    kcp.flush()
}

/// Send a SACK gap notification when a prefix gap is parked in the receive
/// pipeline (nothing delivered, no reader backpressure, segments piled up)
/// — the peer resends the missing segment out of turn instead of waiting
/// out the RTO backoff.
async fn maybe_sack(
    kcp: &mut Kcp<DatagramOut>,
    net: &SessionNet,
    delivered: bool,
    spill: &std::collections::VecDeque<ReadBatch>,
    last_sack_sent: &mut Instant,
) {
    if !delivered
        && spill.is_empty()
        && kcp.recv_queue_len() > SACK_PILEUP_SEGMENTS
        && last_sack_sent.elapsed() >= SACK_COOLDOWN
    {
        *last_sack_sent = Instant::now();
        let f = ctrl_frame(kcp.conv(), CTRL_SACK, &kcp.rcv_nxt_sn().to_le_bytes());
        if let Err(e) = net.send_datagram(&f).await {
            debug!("KCP SACK send failed (peer {}): {e}", net.peer);
        } else {
            KCP_SACKS_SENT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Outcome of one pump round's post-select tail (delivery, SACK, ack
/// flush, wire drain, liveness).
enum Tail {
    /// Keep pumping.
    Continue,
    /// The pump must exit.
    Exit,
}

/// One pump round after its select: deliver received data, notify gaps,
/// flush the batch's acks, push KCP's datagrams onto the wire, and check
/// liveness. Split out of `run_session` so the select stays readable.
#[expect(
    clippy::too_many_arguments,
    reason = "the pump's per-round state is exactly this set; bundling it \
              into a context struct would only move the field list"
)]
async fn pump_tail(
    kcp: &mut Kcp<DatagramOut>,
    net: &SessionNet,
    pace: &mut PaceState,
    in_tx: &mpsc::Sender<ReadBatch>,
    spill: &mut std::collections::VecDeque<ReadBatch>,
    parts: &mut Vec<Bytes>,
    dgram_rx: &mut mpsc::UnboundedReceiver<DgramBatch>,
    last_sack_sent: &mut Instant,
    ack_flush_due: &mut bool,
    sent_any: &mut bool,
    closing: bool,
    quiet_rounds: &mut u32,
    close_deadline: Instant,
) -> Tail {
    // Deliver received app data to the reader half — strictly non-blocking
    // (see `deliver_recv`): a stalled reader must never park the pump,
    // because parking would delay the acks/datagrams of everything
    // arriving meanwhile and the peer's RTO escalates (x1.5 per
    // retransmit in nodelay mode) into seconds-long stalls.
    //
    // The block is load-bearing: a `let _t = ...;` binding drops at the
    // end of its SCOPE, not its statement, so without the explicit block
    // this timer would book everything below (SACK, ack flush, the wire
    // drain) into the delivery phase.
    let delivered = {
        let _t = PhaseTimer::new(&KCP_NS_DELIVER);
        match deliver_recv(kcp, in_tx, spill, parts) {
            Delivery::Done { delivered } => delivered,
            Delivery::ReaderGone => {
                debug!("KCP session reader gone, closing (peer {})", net.peer);
                return Tail::Exit;
            }
        }
    };

    // SACK gap detection (see `maybe_sack`): tell the peer to resend
    // the missing segment instead of waiting out the RTO backoff.
    maybe_sack(kcp, net, delivered, spill, last_sack_sent).await;

    // 3) Flush acks for the batch just consumed — now that delivery has
    //    drained the receive queue, the advertised window is honest.
    if *ack_flush_due {
        *ack_flush_due = false;
        if let Err(e) = flush_after_batch(kcp) {
            warn!("KCP session ack flush failed: {e}");
            return Tail::Exit;
        }
    }

    // 4) Push datagrams KCP emitted this round onto the wire — through the
    //    pacer (see `drain_dgrams`).
    {
        let _t = PhaseTimer::new(&KCP_NS_OUTPUT);
        drain_dgrams(dgram_rx, net, pace, sent_any).await;
    }

    if kcp.is_dead_link() {
        debug!("KCP session dead link (peer {})", net.peer);
        return Tail::Exit;
    }

    if closing {
        // Writer half is gone. With the reader also gone there is
        // nobody left to serve: flush the ARQ tail briefly, then exit.
        // With the reader still alive this is a half-close — keep
        // serving reads; a vanished peer is bounded by the dead-link
        // check above.
        if in_tx.is_closed()
            && closing_quiescent(
                *sent_any,
                delivered,
                kcp,
                spill,
                quiet_rounds,
                close_deadline,
            )
        {
            return Tail::Exit;
        }
    }

    Tail::Continue
}

/// The per-session pump: owns the `Kcp` state machine and drives it between
/// the writer channel, the inbound-datagram channel and the update timer.
async fn run_session(
    mut kcp: Kcp<DatagramOut>,
    net: SessionNet,
    mut pkt_rx: mpsc::Receiver<Bytes>,
    mut dgram_rx: mpsc::UnboundedReceiver<DgramBatch>,
    mut out_rx: mpsc::UnboundedReceiver<Bytes>,
    out_sem: Arc<Semaphore>,
    in_tx: mpsc::Sender<ReadBatch>,
) {
    let start = Instant::now();
    // Prime the clock: `flush`/`check` require one `update` call first.
    if let Err(e) = kcp.update(ms_now(start)) {
        warn!("KCP session initial update failed: {e}");
        out_sem.close();
        net.announce_gone().await;
        return;
    }

    let mut closing = false;
    let mut close_deadline = Instant::now() + CLOSE_FLUSH_TIMEOUT;
    let mut quiet_rounds: u32 = 0;
    // Reader-channel batching staging: the engine's segment buffers,
    // collected into one message (see `deliver_recv`).
    let mut parts: Vec<Bytes> = Vec::with_capacity(MAX_READ_PARTS);
    // Batches recv'd but not yet accepted by the reader channel.
    let mut spill: std::collections::VecDeque<ReadBatch> = std::collections::VecDeque::new();
    let mut ack_flush_due = false;
    // Adaptive send pacing + keepalive/RTT probing (see `PaceState`).
    let mut pace = PaceState::new(start);
    // Last SACK gap notification (throttled by SACK_COOLDOWN).
    let mut last_sack_sent = Instant::now();
    while let Some(delay) = {
        let _t = PhaseTimer::new(&KCP_NS_UPDATE); // pump_head = update + keepalive
        pump_head(&mut kcp, &net, &mut pace, start).await
    } {
        KCP_PUMP_ROUNDS.fetch_add(1, Ordering::Relaxed);
        let mut sent_any = false;
        tokio::select! {
            // App data from the writer half (gated on the ARQ queue cap).
            // Drain the channel in this one select arm: with the inbound
            // channel permanently ready under load, tokio's uniform select
            // fairness would otherwise starve the writer side to a fraction
            // of the link rate (measured ~half the window limit at 20 ms).
            data = out_rx.recv(), if !closing && kcp.wait_snd() < SND_QUEUE_LIMIT => {
                if let Some(data) = data {
                    let mut fatal = false;
                    let _t = PhaseTimer::new(&KCP_NS_WRITER);
                    if let Drain::Fatal =
                        drain_writer(&mut kcp, &mut out_rx, &out_sem, data)
                    {
                        fatal = true;
                    }
                    // Push immediately instead of waiting for the next
                    // timer tick (interactive latency).
                    if let Err(e) = kcp.flush() {
                        warn!("KCP session flush failed: {e}");
                        break;
                    }
                    if fatal {
                        break;
                    }
                } else {
                    // Writer half dropped: flush what KCP accepted, then end.
                    closing = true;
                    close_deadline = Instant::now() + CLOSE_FLUSH_TIMEOUT;
                }
            }
            // Inbound datagrams: feed a bounded batch to the state machine. Acks are
            // flushed AFTER delivery, so the advertised window is honest.
            pkt = pkt_rx.recv() => {
                match pkt {
                    Some(pkt) => {
                        let _t = PhaseTimer::new(&KCP_NS_INPUT);
                        if is_ctrl_frame(&pkt) {
                            let kind = pkt[5];
                            handle_ctrl(kind, ctrl_payload(&pkt), &mut kcp,
                                        &net, &mut pace, start).await;
                        } else {
                            ack_flush_due |= feed_batch(&mut kcp, &mut pkt_rx, &pkt);
                        }
                    }
                    // Ingress task/dispatcher gone: the session is over.
                    None => break,
                }
            }
            // Update timer: retransmits, delayed acks, window probes.
            () = tokio::time::sleep(Duration::from_millis(u64::from(delay))) => {
                let _t = PhaseTimer::new(&KCP_NS_UPDATE);
                if let Err(e) = kcp.update(ms_now(start)) {
                    warn!("KCP session update failed: {e}");
                    break;
                }
            }
        }

        match pump_tail(
            &mut kcp,
            &net,
            &mut pace,
            &in_tx,
            &mut spill,
            &mut parts,
            &mut dgram_rx,
            &mut last_sack_sent,
            &mut ack_flush_due,
            &mut sent_any,
            closing,
            &mut quiet_rounds,
            close_deadline,
        )
        .await
        {
            Tail::Continue => {}
            Tail::Exit => break,
        }
    }

    // Release any writer parked on the capacity semaphore: the pump is
    // gone, so further writes must fail fast (BrokenPipe) instead of
    // pending forever. `close` wakes all queued acquirers with an error,
    // and later `add_permits` calls are impossible (this is the only task
    // holding the pump side).
    out_sem.close();
    let peer = net.peer;
    net.announce_gone().await;
    debug!("KCP session pump exited (peer {peer})");
}

/// Build the channel quartet + configured `Kcp` for one session and spawn
/// its pump. Returns the user-facing stream and the datagram-in sender the
/// ingress side must hold (dropping it ends the session).
fn spawn_session(conv: u32, net: SessionNet) -> (KcpStream, mpsc::Sender<Bytes>) {
    let (dgram_tx, dgram_rx) = mpsc::unbounded_channel();
    let mut kcp = Kcp::new_stream(conv, DatagramOut::new(dgram_tx));
    configure(&mut kcp);

    let (pkt_tx, pkt_rx) = mpsc::channel(DATAGRAM_CHANNEL_DEPTH);
    // Writer → pump: an unbounded channel gated by a semaphore (the pump
    // returns one permit per consumed write), because tokio's bounded
    // `Sender` has no poll-based send for `AsyncWrite::poll_write`.
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let out_sem = Arc::new(Semaphore::new(OUTBOUND_CHANNEL_DEPTH));
    let (in_tx, in_rx) = mpsc::channel::<ReadBatch>(INBOUND_CHANNEL_DEPTH);

    spawn_kcp_stats();
    tokio::spawn(run_session(
        kcp,
        net,
        pkt_rx,
        dgram_rx,
        out_rx,
        Arc::clone(&out_sem),
        in_tx,
    ));
    (KcpStream::new(out_tx, out_sem, in_rx), pkt_tx)
}

/// A KCP session presented as a reliable byte stream (tokio IO traits), so
/// Noise/yamux ride on top unchanged.
///
/// Dropping the stream ends the session: the pump flushes what KCP already
/// accepted (best effort, bounded), then exits.
pub struct KcpStream {
    out_tx: Option<mpsc::UnboundedSender<Bytes>>,
    /// Capacity gate for `out_tx`: one permit per in-flight write, returned
    /// by the pump as it feeds each write to KCP.
    out_sem: Arc<Semaphore>,
    /// Parked permit acquisition, kept across `poll_write` calls. Tokio
    /// does not export the future type, so it is type-erased here.
    out_acquire: Option<Pin<Box<AcquirePermitFuture>>>,

    in_rx: Option<mpsc::Receiver<ReadBatch>>,
    /// Parts of the batch in hand, in delivery order. One `poll_read`
    /// serves at most one part's worth (the reader above buffers across
    /// reads itself), so the granularity the reader sees is unchanged
    /// from the per-segment messages — only the channel hops fell.
    read_queue: std::collections::VecDeque<Bytes>,
}

impl std::fmt::Debug for KcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpStream")
            .field("write_open", &self.out_tx.is_some())
            .field("read_open", &self.in_rx.is_some())
            .finish_non_exhaustive()
    }
}

impl KcpStream {
    fn new(
        out_tx: mpsc::UnboundedSender<Bytes>,
        out_sem: Arc<Semaphore>,
        in_rx: mpsc::Receiver<ReadBatch>,
    ) -> KcpStream {
        KcpStream {
            out_tx: Some(out_tx),
            out_sem,
            out_acquire: None,
            in_rx: Some(in_rx),
            read_queue: std::collections::VecDeque::new(),
        }
    }

    /// Capacity gate shared by both write paths: acquire one permit
    /// (registering the waker when the pump is behind).
    fn acquire_write_permit(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<tokio::sync::OwnedSemaphorePermit, io::Error>> {
        match self.out_sem.clone().try_acquire_owned() {
            Ok(permit) => Poll::Ready(Ok(permit)),
            Err(tokio::sync::TryAcquireError::Closed) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session closed",
            ))),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                let fut = self.out_acquire.get_or_insert_with(|| {
                    Box::pin(self.out_sem.clone().acquire_owned()) as Pin<Box<AcquirePermitFuture>>
                });
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(permit)) => {
                        self.out_acquire = None;
                        Poll::Ready(Ok(permit))
                    }
                    // The semaphore is never closed explicitly; treat it as
                    // a dead session anyway.
                    Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "KCP session closed",
                    ))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

/// The owned-record write boundary: a whole record crosses to the pump by
/// ownership — no copy at the channel, and the engine shares it per
/// segment. This is link L3's half on the stream side; the record producer
/// is the Noise layer's owned-record path.
impl crate::common::owned_write::AsyncWriteOwned for KcpStream {
    const TAKES_OWNED: bool = true;

    fn poll_write_owned(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        record: Bytes,
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        if record.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if me.out_tx.is_none() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session write half already shut down",
            )));
        }
        // Capacity gate: acquire one permit (registering the waker when the
        // pump is behind), then hand the record to the unbounded channel.
        let permit = std::task::ready!(me.acquire_write_permit(cx))?;
        let n = record.len();
        match me.out_tx.as_ref() {
            Some(tx) => match tx.send(record) {
                Ok(()) => {
                    permit.forget();
                    Poll::Ready(Ok(n))
                }
                // Pump gone: the session is dead.
                Err(_) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "KCP session closed",
                ))),
            },
            // Checked above; the half cannot close while this poll runs.
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session write half already shut down",
            ))),
        }
    }
}

impl AsyncRead for KcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            if let Some(front) = me.read_queue.front_mut() {
                let n = std::cmp::min(buf.remaining(), front.len());
                buf.put_slice(&front[..n]);
                front.advance(n);
                if front.is_empty() {
                    me.read_queue.pop_front();
                }
                return Poll::Ready(Ok(()));
            }
            let Some(rx) = me.in_rx.as_mut() else {
                // Read half closed (pump gone): EOF.
                return Poll::Ready(Ok(()));
            };
            match rx.poll_recv(cx) {
                Poll::Ready(Some(batch)) => {
                    for part in batch.parts {
                        if !part.is_empty() {
                            me.read_queue.push_back(part);
                        }
                    }
                }
                Poll::Ready(None) => {
                    me.in_rx = None; // EOF from here on
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for KcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if me.out_tx.is_none() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session write half already shut down",
            )));
        }
        // Capacity gate: acquire one permit (registering the waker when the
        // pump is behind), then hand the write to the unbounded channel.
        let permit = std::task::ready!(me.acquire_write_permit(cx))?;
        match me.out_tx.as_ref() {
            Some(tx) => match tx.send(Bytes::copy_from_slice(buf)) {
                Ok(()) => {
                    permit.forget();
                    Poll::Ready(Ok(buf.len()))
                }
                // Pump gone: the session is dead.
                Err(_) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "KCP session closed",
                ))),
            },
            // Checked above; the half cannot close while this poll runs.
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session write half already shut down",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // KCP has no user-visible flush: the pump pushes every accepted byte
        // onto the wire immediately (nodelay) and retransmits until acked.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Drop the writer half; the pump notices, flushes best-effort and
        // exits. Reads keep working until the pump is gone (then EOF).
        self.get_mut().out_tx = None;
        Poll::Ready(Ok(()))
    }
}

/// The tunnel byte stream over a KCP session: plain, or Noise-wrapped when
/// the control transport is `noise` — arm 2 keeps the crypto stack and only
/// replaces the TCP leg. The tunnel hello/ack and the yamux session ride on
/// this.
pub enum KcpTunnelStream {
    /// Plain KCP (control transport is `tcp`).
    Plain(KcpStream),
    /// Noise over KCP (control transport is `noise`). Boxed: the noise
    /// state machine dwarfs the plain variant, and this enum lives on the
    /// stack of every tunnel task.
    #[cfg(feature = "noise")]
    Noise(Box<crate::transport::NoiseStream<KcpStream>>),
}

impl std::fmt::Debug for KcpTunnelStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KcpTunnelStream::Plain(s) => f.debug_tuple("Plain").field(s).finish(),
            #[cfg(feature = "noise")]
            KcpTunnelStream::Noise(_) => f.debug_tuple("Noise").finish(),
        }
    }
}

impl AsyncRead for KcpTunnelStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            KcpTunnelStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "noise")]
            KcpTunnelStream::Noise(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for KcpTunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            KcpTunnelStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "noise")]
            KcpTunnelStream::Noise(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            KcpTunnelStream::Plain(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "noise")]
            KcpTunnelStream::Noise(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            KcpTunnelStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "noise")]
            KcpTunnelStream::Noise(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Client side: open one KCP session to `remote` under `conv`.
///
/// The session owns a fresh ephemeral UDP socket (arm 2 runs N sessions, so
/// each gets an independent kernel queue and source port). Packets from any
/// address other than `remote` are dropped.
#[cfg(any(feature = "client", test))]
pub async fn connect(remote: SocketAddr, conv: u32) -> Result<KcpStream> {
    let bind = if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("Failed to bind the KCP client socket ({bind})"))?;
    tune_socket_buffers(&socket);
    let socket = Arc::new(socket);

    let net = SessionNet {
        socket: Arc::clone(&socket),
        peer: remote,
        gone_tx: None,
        key: (remote, conv),
    };
    let (stream, pkt_tx) = spawn_session(conv, net);

    // Ingress task: socket → session pump, filtered to the fixed peer.
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let mut batch = crate::transport::udp_batch::RecvBatch::new(
            crate::transport::udp_batch::BATCH,
            DGRAM_BUF,
        );
        #[cfg(not(target_os = "linux"))]
        let mut buf = [0u8; DGRAM_BUF];
        loop {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                use tokio::io::Interest;
                if socket.readable().await.is_err() {
                    break;
                }
                // `try_io` clears the cached readiness on EAGAIN (see
                // `dispatch_recv`).
                match socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd())) {
                    Ok(_) => {
                        let mut pump_gone = false;
                        for (from, data) in batch.iter() {
                            if from != remote {
                                trace!("Dropping a KCP datagram from an unexpected peer {from}");
                                continue;
                            }
                            if pkt_tx.send(Bytes::copy_from_slice(data)).await.is_err() {
                                pump_gone = true;
                                break; // session pump gone
                            }
                        }
                        if pump_gone {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Drained (stale readiness flag); park again.
                    }
                    Err(e) => {
                        debug!("KCP client socket recv failed: {e}");
                        break;
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                match socket.recv_from(&mut buf).await {
                    Ok((n, from)) => {
                        if from != remote {
                            trace!("Dropping a KCP datagram from an unexpected peer {from}");
                            continue;
                        }
                        if pkt_tx
                            .send(Bytes::copy_from_slice(&buf[..n]))
                            .await
                            .is_err()
                        {
                            break; // session pump gone
                        }
                    }
                    Err(e) => {
                        debug!("KCP client socket recv failed: {e}");
                        break;
                    }
                }
            }
        }
    });

    debug!(%remote, conv, "KCP session dialed (client)");
    Ok(stream)
}

/// One accepted server-side KCP session, before any hello validation.
#[cfg(any(feature = "server", test))]
pub struct AcceptedSession {
    /// The reliable byte stream (Noise/yamux ride on top).
    pub stream: KcpStream,
    /// The peer's UDP address.
    pub peer: SocketAddr,
}

/// Server-side KCP listener: one UDP socket demultiplexed into sessions by
/// `(peer address, conv)`. A datagram carrying an unknown conv adopts that
/// conv into a fresh session (standard KCP listener behavior — the protocol
/// has no handshake of its own; authentication is the tunnel hello's job).
#[cfg(any(feature = "server", test))]
pub struct KcpAcceptor {
    socket: Arc<UdpSocket>,
    accepted_rx: Mutex<mpsc::Receiver<AcceptedSession>>,
    shutdown_tx: watch::Sender<bool>,
}

#[cfg(any(feature = "server", test))]
impl std::fmt::Debug for KcpAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpAcceptor")
            .field("addr", &self.socket.local_addr())
            .finish_non_exhaustive()
    }
}

#[cfg(any(feature = "server", test))]
impl KcpAcceptor {
    /// Bind the listener socket and start the dispatcher task.
    pub async fn bind<A: tokio::net::ToSocketAddrs + Send>(addr: A) -> Result<KcpAcceptor> {
        let socket = UdpSocket::bind(&addr)
            .await
            .with_context(|| "Failed to bind the KCP listener socket")?;
        let bound = socket.local_addr()?;
        tune_socket_buffers(&socket);
        let socket = Arc::new(socket);

        let (accepted_tx, accepted_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(dispatch(Arc::clone(&socket), accepted_tx, shutdown_rx));

        debug!(
            %bound,
            "KCP listener up (stream mode, nodelay=1 interval={KCP_INTERVAL_MS}ms \
             fast-resend={KCP_FAST_RESEND} nc=1 \
             sndwnd={KCP_SND_WND} rcvwnd={KCP_RCV_WND} mtu=1400)"
        );
        Ok(KcpAcceptor {
            socket,
            accepted_rx: Mutex::new(accepted_rx),
            shutdown_tx,
        })
    }

    /// The local address the listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Accept the next session. Returns `None` after shutdown.
    ///
    /// Note: the dispatcher pauses while the accept queue (16 deep) is full,
    /// so the consumer should hand sessions off to their own task promptly.
    pub async fn accept(&self) -> Option<AcceptedSession> {
        self.accepted_rx.lock().await.recv().await
    }
}

#[cfg(any(feature = "server", test))]
impl Drop for KcpAcceptor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Route one inbound listener datagram: adapter control frames to the
/// owning session, data to the session by `(peer, conv)`, adopting unknown
/// conversations (subject to the session cap). Returns false when the
/// acceptor is gone (the dispatcher should exit).
#[cfg(any(feature = "server", test))]
async fn route_datagram(
    socket: &Arc<UdpSocket>,
    sessions: &mut HashMap<SessionKey, mpsc::Sender<Bytes>>,
    accepted_tx: &mpsc::Sender<AcceptedSession>,
    gone_tx: &mpsc::Sender<SessionKey>,
    from: SocketAddr,
    data: &[u8],
) -> bool {
    // Adapter control frames (PING/PONG/SACK) are shorter than
    // a KCP header: recognize them by the magic byte instead
    // of dropping them as runts — the pacing/keepalive state
    // machine depends on these crossing the listener.
    let (conv, is_ctrl) = if is_ctrl_frame(data) {
        let mut c = [0u8; 4];
        c.copy_from_slice(&data[..4]);
        (u32::from_le_bytes(c), true)
    } else if data.len() >= KCP_OVERHEAD {
        (get_conv(data), false)
    } else {
        trace!("Dropping a runt KCP datagram ({}) from {from}", data.len());
        return true;
    };
    let key = (from, conv);
    let pkt = Bytes::copy_from_slice(data);

    // Control frames must reach the session pump even if no
    // data session exists yet? No — they carry the session
    // conv, so route like any other datagram; a PING for an
    // unknown conv is silently dropped (never adopted).
    if is_ctrl && !sessions.contains_key(&key) {
        return true;
    }

    if let Some(tx) = sessions.get(&key) {
        // Pure blocking hand-off: backpressure travels
        // dispatcher → kernel socket buffer (sized for a full
        // ARQ burst) → the peer's window, instead of dropping
        // datagrams in userspace. A bounded wait was tried and
        // rejected: with a burst (full window) larger than the
        // session channel, the 2 ms cap dropped datagrams
        // every burst and collapsed throughput. The per-session
        // head-of-line cost (one lagging session parking the
        // dispatcher) is bounded by the kernel socket buffers
        // and the peer's window backpressure.
        match tx.send(pkt).await {
            Ok(()) => {}
            Err(_) => {
                sessions.remove(&key);
            }
        }
        return true;
    }

    if sessions.len() >= MAX_LISTENER_SESSIONS {
        warn!(
            "KCP listener at the session cap ({}), dropping new conv {} from {from}",
            MAX_LISTENER_SESSIONS, conv
        );
        return true;
    }

    // Adopt the conversation into a fresh session.
    let net = SessionNet {
        socket: Arc::clone(socket),
        peer: from,
        gone_tx: Some(gone_tx.clone()),
        key,
    };
    let (stream, pkt_tx) = spawn_session(conv, net);
    if pkt_tx.try_send(pkt).is_err() {
        warn!("Fresh KCP session {key:?} rejected its first datagram");
        return true;
    }
    sessions.insert(key, pkt_tx);
    debug!(peer = %from, conv, "KCP session adopted");
    accepted_tx
        .send(AcceptedSession { stream, peer: from })
        .await
        .is_ok()
}

/// One dispatcher recv round: on Linux, wait for readability and drain one
/// `recvmmsg` batch (bounded, so the shutdown/gone arms stay fair under
/// load); elsewhere, receive one datagram. Routing happens inside.
#[cfg(any(feature = "server", test))]
async fn dispatch_recv(
    socket: &Arc<UdpSocket>,
    sessions: &mut HashMap<SessionKey, mpsc::Sender<Bytes>>,
    accepted_tx: &mpsc::Sender<AcceptedSession>,
    gone_tx: &mpsc::Sender<SessionKey>,
    #[cfg(target_os = "linux")] batch: &mut crate::transport::udp_batch::RecvBatch,
    #[cfg(not(target_os = "linux"))] buf: &mut [u8],
) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use tokio::io::Interest;
        if socket.readable().await.is_err() {
            return;
        }
        // `try_io` clears tokio's cached readiness when the drain hits
        // EAGAIN, so the next `readable()` parks instead of spinning.
        match socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd())) {
            Ok(_) => {
                for (from, data) in batch.iter() {
                    if !route_datagram(socket, sessions, accepted_tx, gone_tx, from, data).await {
                        return; // acceptor dropped
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Drained: the stale-readiness flag was cleared by
                // `try_io`, so the next `readable()` parks again.
            }
            Err(e) => {
                warn!("KCP listener recv failed: {e}");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        match socket.recv_from(buf).await {
            Ok((n, from)) => {
                route_datagram(socket, sessions, accepted_tx, gone_tx, from, &buf[..n]).await;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Drained: the stale-readiness flag was cleared by
                // `try_io`, so the next `readable()` parks again.
            }
            Err(e) => {
                warn!("KCP listener recv failed: {e}");
            }
        }
    }
}

/// The listener dispatcher: owns the session map, routes datagrams by
/// `(peer, conv)` and adopts new conversations. Routing never blocks on a
/// session's queue — overflow datagrams are dropped and recovered by KCP's
/// ARQ.
#[cfg(any(feature = "server", test))]
async fn dispatch(
    socket: Arc<UdpSocket>,
    accepted_tx: mpsc::Sender<AcceptedSession>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut sessions: HashMap<SessionKey, mpsc::Sender<Bytes>> = HashMap::new();
    let (gone_tx, mut gone_rx) = mpsc::channel::<SessionKey>(64);
    #[cfg(target_os = "linux")]
    let mut batch =
        crate::transport::udp_batch::RecvBatch::new(crate::transport::udp_batch::BATCH, DGRAM_BUF);
    #[cfg(not(target_os = "linux"))]
    let mut buf = [0u8; DGRAM_BUF];

    loop {
        let recv_fut = dispatch_recv(
            &socket,
            &mut sessions,
            &accepted_tx,
            &gone_tx,
            #[cfg(target_os = "linux")]
            &mut batch,
            #[cfg(not(target_os = "linux"))]
            &mut buf,
        );
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            // Forget exited sessions so their key can be reused cleanly.
            Some(key) = gone_rx.recv() => {
                sessions.remove(&key);
            }
            () = recv_fut => {}
        }
    }

    // Shutdown: dropping the senders ends every session pump.
    sessions.clear();
    debug!("KCP listener dispatcher exited");
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        reason = "tests unwrap values they just constructed; test windows are far below usize::MAX"
    )]
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read exactly `n` bytes with a generous timeout (KCP is userspace ARQ
    /// ticking at a 10 ms interval even on loopback).
    async fn read_exact_timeout(stream: &mut KcpStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(30), stream.read_exact(&mut buf))
            .await
            .expect("KCP read timed out")
            .unwrap();
        buf
    }

    #[test]
    fn send_batches_close_at_the_flush_boundary() {
        // The engine emits one datagram per `write` call; the adapter must
        // stage them into ONE channel message per batch, closed at the
        // `BATCH` count or the engine's flush boundary — never per
        // datagram, and never losing the datagram boundaries (the spans
        // must tile the staging buffer exactly).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut out = DatagramOut::new(tx);
        let dgram = [0x5Au8; 24];

        // A partial batch stays staged: nothing crosses the channel yet.
        for _ in 0..crate::transport::udp_batch::BATCH - 1 {
            std::io::Write::write_all(&mut out, &dgram[..]).unwrap();
        }
        assert!(rx.try_recv().is_err(), "a partial batch must stay staged");

        // The 32nd datagram closes the batch.
        std::io::Write::write_all(&mut out, &dgram[..]).unwrap();
        let batch = rx.try_recv().expect("the full batch must cross at once");
        assert_eq!(batch.spans.len(), crate::transport::udp_batch::BATCH);
        // Spans tile the buffer: contiguous, in order, no gaps.
        let mut off = 0;
        for span in &batch.spans {
            let Span::Staged { off: o, len } = span else {
                panic!("a packed write must stage its whole datagram");
            };
            assert_eq!((*o, *len), (off, dgram.len()));
            off += len;
        }
        assert_eq!(off, batch.buf.len());
        assert!(rx.try_recv().is_err(), "one message per batch");

        // The flush boundary closes a partial batch.
        std::io::Write::write_all(&mut out, &dgram[..]).unwrap();
        assert!(rx.try_recv().is_err());
        std::io::Write::flush(&mut out).unwrap();
        let batch = rx.try_recv().expect("flush must close the batch");
        assert_eq!(batch.spans.len(), 1);
    }

    #[test]
    fn push_datagrams_travel_as_a_header_plus_a_payload_reference() {
        // The engine's two-iovec boundary: a PUSH datagram crosses the
        // channel as a staged header plus the segment's own payload `Bytes`
        // — the same handle, not a copy. The retransmit re-emits the very
        // same buffer, so the two payload pointers must match.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut out = DatagramOut::new(tx);
        let payload = Bytes::from_static(b"the payload is shared, not copied");

        crate::kcp::DatagramSink::write_datagram(&mut out, b"0123456789abcdef01234567", &payload)
            .unwrap();
        std::io::Write::flush(&mut out).unwrap();
        let batch = rx.try_recv().expect("the datagram must cross the channel");
        assert_eq!(batch.spans.len(), 1);
        let Span::Split {
            hdr_off,
            payload: got,
        } = &batch.spans[0]
        else {
            panic!("a PUSH datagram must travel split (header + payload)");
        };
        // The staged header is the first 24 bytes of the batch buffer.
        assert_eq!(
            &batch.buf[*hdr_off..*hdr_off + KCP_OVERHEAD],
            b"0123456789abcdef01234567"
        );
        // Same allocation as the engine segment's buffer: no copy.
        assert!(
            std::ptr::eq(got.as_ptr(), payload.as_ptr()),
            "the payload was copied instead of shared"
        );

        // A packed (non-split) datagram still stages whole.
        std::io::Write::write_all(&mut out, b"packed").unwrap();
        std::io::Write::flush(&mut out).unwrap();
        let batch = rx.try_recv().expect("the packed datagram must cross");
        assert!(matches!(batch.spans.as_slice(), [Span::Staged { .. }]));
    }

    #[tokio::test]
    async fn coalescing_merges_segments_into_blobs() {
        // One 32 KiB write crosses as ~24 MTU-sized segments; the
        // receiver's deliver loop must merge them into far fewer
        // reader-channel messages (COALESCE_LIMIT_BYTES each) while the
        // byte stream arrives intact.
        const PAYLOAD: usize = 32 * 1024;
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();
        let before = super::kcp_stats();

        let mut client = connect(addr, 0x0BAD_F00D).await.unwrap();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| (i % 251) as u8).collect();
        client.write_all(&payload).await.unwrap();

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut session = session.stream;
        let got = read_exact_timeout(&mut session, PAYLOAD).await;
        assert_eq!(got, payload, "coalescing must not alter the byte stream");

        let after = super::kcp_stats();
        let segments = after.datagrams_in - before.datagrams_in;
        let blobs = after.blobs_out - before.blobs_out;
        assert!(
            blobs * 3 < segments,
            "coalescing did not merge: {blobs} blobs for {segments} segments"
        );
    }

    #[tokio::test]
    async fn stalled_reader_resumes_with_intact_stream() {
        // A reader that parks mid-stream while the tunnel keeps sending,
        // then drains slowly in small reads: the batched (`ReadBatch`)
        // zero-copy path must hand back an intact, in-order byte stream.
        // (The pump's deep spill queue only engages past 2048 unread
        // messages — ~32 MiB with the ~11-segment batches — which is a
        // bench-cell scale, not a unit-test one; its zero-alloc
        // take/put-back loop shares this ordering guarantee.)
        const PAYLOAD: usize = 512 * 1024;
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();

        let mut client = connect(addr, 0x0BAD_F00D).await.unwrap();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| (i % 251) as u8).collect();
        // The write pushes the whole payload through the tunnel (the
        // writer half is unbounded, so this returns once KCP accepted
        // the bytes) and is what makes the server adopt the session.
        client.write_all(&payload).await.unwrap();

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut session = session.stream;

        // Read only a slice, then stall: the writer keeps pushing, so the
        // reader channel (2048 messages) fills and delivery spills.
        let head = 64 * 1024;
        let mut got = read_exact_timeout(&mut session, head).await;
        // Give the pump time to fill the channel and spill while the
        // reader is parked (one KCP flush interval is 10 ms; the payload
        // is ~380 segments, so a few intervals suffice).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now drain the rest slowly, a small chunk at a time.
        let mut chunk = vec![0u8; 4096];
        while got.len() < PAYLOAD {
            let want = (PAYLOAD - got.len()).min(chunk.len());
            let n = tokio::time::timeout(Duration::from_secs(30), session.read(&mut chunk[..want]))
                .await
                .expect("slow-reader drain timed out")
                .unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(got.len(), PAYLOAD, "spill path lost bytes");
        assert_eq!(got, payload, "spill path reordered or corrupted bytes");

        // Keep the writer half referenced until here so the session
        // cannot close mid-drain.
        drop(client);
    }

    #[tokio::test]
    async fn stats_counters_move_with_traffic() {
        // The counters are process-global (every session adds to them), so
        // the test reads deltas around one round trip rather than
        // absolute values.
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();
        let before = super::kcp_stats();

        let mut client = connect(addr, 0x0BAD_F00D).await.unwrap();
        client.write_all(b"stats probe").await.unwrap();

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut session = session.stream;

        let got = read_exact_timeout(&mut session, b"stats probe".len()).await;
        assert_eq!(&got, b"stats probe");

        session.write_all(b"ack-back").await.unwrap();
        let got = read_exact_timeout(&mut client, b"ack-back".len()).await;
        assert_eq!(&got, b"ack-back");

        let after = super::kcp_stats();
        assert!(
            after.datagrams_in > before.datagrams_in,
            "no datagram was counted on input"
        );
        assert!(
            after.datagrams_out > before.datagrams_out,
            "no datagram was counted on output"
        );
        assert!(
            after.pump_rounds > before.pump_rounds,
            "no pump round was counted"
        );
        assert!(
            after.segments_delivered > before.segments_delivered,
            "no delivered segment was counted"
        );
        assert!(
            after.recv_empty > before.recv_empty,
            "no empty receive probe was counted"
        );
        // Every phase that ran for this transfer books time, even on a
        // quiet loopback session.
        let ns =
            after.ms_input + after.ms_deliver + after.ms_writer + after.ms_output + after.ms_update;
        assert!(
            ns > before.ms_input
                + before.ms_deliver
                + before.ms_writer
                + before.ms_output
                + before.ms_update,
            "no phase booked time"
        );
    }

    #[tokio::test]
    async fn client_server_roundtrip() {
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();

        let mut client = connect(addr, 0x1234_5678).await.unwrap();
        client.write_all(b"hello kcp").await.unwrap();

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut server = session.stream;

        let got = read_exact_timeout(&mut server, "hello kcp".len()).await;
        assert_eq!(&got, b"hello kcp");

        server.write_all(b"hello back").await.unwrap();
        let got = read_exact_timeout(&mut client, "hello back".len()).await;
        assert_eq!(&got, b"hello back");
    }

    #[tokio::test]
    async fn two_sessions_stay_isolated() {
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();

        let mut c1 = connect(addr, 11).await.unwrap();
        let mut c2 = connect(addr, 22).await.unwrap();
        c1.write_all(b"aaaa").await.unwrap();
        c2.write_all(b"bbbb").await.unwrap();

        // The two sessions must arrive separately, each with its own data.
        let mut seen = Vec::new();
        for _ in 0..2 {
            let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
                .await
                .expect("accept timed out")
                .expect("acceptor closed");
            let mut server = session.stream;
            let got = read_exact_timeout(&mut server, 4).await;
            seen.push(got);
            // Keep the session alive and draining.
            tokio::spawn(async move {
                let mut sink = vec![0u8; 64];
                loop {
                    match server.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
        seen.sort();
        assert_eq!(seen, vec![b"aaaa".to_vec(), b"bbbb".to_vec()]);
    }

    #[tokio::test]
    async fn bulk_transfer_is_lossless() {
        // 4 MiB through one session: exercises chunking, the ARQ window,
        // pump backpressure and the update timer under load.
        const SIZE: usize = 4 * 1024 * 1024;
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();

        let mut client = connect(addr, 77).await.unwrap();
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        let expect_sum: u64 = payload.iter().map(|b| u64::from(*b)).sum();

        let writer = tokio::spawn(async move {
            client.write_all(&payload).await.unwrap();
            client.flush().await.unwrap();
            // Keep the session alive until the reader is done.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut server = session.stream;

        let mut sum: u64 = 0;
        let mut total = 0usize;
        let mut buf = [0u8; 8192];
        while total < SIZE {
            let n = tokio::time::timeout(Duration::from_secs(60), server.read(&mut buf))
                .await
                .expect("bulk read timed out")
                .unwrap();
            assert!(n > 0, "unexpected EOF at {total} bytes");
            sum += buf[..n].iter().map(|b| u64::from(*b)).sum::<u64>();
            total += n;
        }
        assert_eq!(total, SIZE);
        assert_eq!(sum, expect_sum);
        writer.abort();
    }

    #[tokio::test]
    async fn graceful_shutdown_flushes_queued_data() {
        // Write a chunk, immediately shut the write half: the pump must
        // still deliver everything KCP accepted before exiting.
        const SIZE: usize = 256 * 1024;
        let acceptor = KcpAcceptor::bind("127.0.0.1:0").await.unwrap();
        let addr = acceptor.local_addr().unwrap();

        let mut client = connect(addr, 99).await.unwrap();
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 199) as u8).collect();
        let expect_sum: u64 = payload.iter().map(|b| u64::from(*b)).sum();
        client.write_all(&payload).await.unwrap();
        client.shutdown().await.unwrap();

        let session = tokio::time::timeout(Duration::from_secs(10), acceptor.accept())
            .await
            .expect("accept timed out")
            .expect("acceptor closed");
        let mut server = session.stream;

        let got = read_exact_timeout(&mut server, SIZE).await;
        let sum: u64 = got.iter().map(|b| u64::from(*b)).sum();
        assert_eq!(sum, expect_sum);
    }
}
