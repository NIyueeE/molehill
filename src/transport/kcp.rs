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
//! Noise/yamux layers). PONGs measure the path RTT and double as the
//! congestion signal for the pacer; the PING cadence also keeps NAT
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
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::kcp::{KCP_OVERHEAD, Kcp, get_conv};
use anyhow::{Context as _, Result, bail};
use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, trace, warn};

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
/// Inbound app-data channel depth (chunks of at most one MSS); half the
/// ARQ receive window, so reader bursts never force datagram drops at the
/// pump (the spill queue absorbs the rest).
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
/// Receive scratch buffer: one KCP stream-mode `recv` returns at most one
/// segment (<= MTU - overhead).
const RECV_BUF: usize = 2048;
/// Inbound datagram scratch buffer (one datagram is at most one MTU).
const DGRAM_BUF: usize = 2048;
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
// flush (8192 segments ≈ 11.5 MiB) bursts far beyond any path queue and
// gets dropped en masse (netem qdisc limit, router buffers). The pump
// therefore paces outbound datagrams with a token bucket whose rate is
// (a) seeded from the BDP estimate at the measured RTT, (b) cut on PONG
// timeouts (path over capacity) and (c) nudged up on sustained clean
// PONGs — a loss-signal-driven stand-in for congestion control.
const PACER_INIT_RTT_MS: f64 = 20.0; // BDP seed before the first PONG
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

/// Type-erased `Semaphore::acquire_owned` future (the concrete type is not
/// exported by tokio).
type AcquirePermitFuture = dyn std::future::Future<
        Output = Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>,
    > + Send;

/// Collects `Kcp` output as whole datagrams: each `Write::write` call from
/// `flush`/`update` is exactly one datagram (at most MTU bytes), forwarded to
/// the pump for a single `send_to`.
struct DatagramOut {
    tx: mpsc::UnboundedSender<Bytes>,
}

impl io::Write for DatagramOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The pump owns the receiver and drops it only together with the
        // `Kcp` instance, so a send error here is unreachable in practice;
        // a dropped datagram would still be recovered by ARQ.
        let _ = self.tx.send(Bytes::copy_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

/// Split one app write into `Kcp::send`-safe chunks.
fn kcp_send_all(kcp: &mut Kcp<DatagramOut>, data: &[u8]) -> Result<()> {
    let mut rest = data;
    while !rest.is_empty() {
        let (head, tail) = rest.split_at(rest.len().min(SEND_CHUNK));
        match kcp.send(head) {
            // Stream mode queues the whole chunk or fails; a partial accept
            // would duplicate bytes on resend, so treat it as fatal.
            Ok(n) if n == head.len() => rest = tail,
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

/// Move received app data from the KCP receive queue to the reader channel
/// without ever blocking: chunks the bounded channel cannot take spill to a
/// local queue (and stay in KCP's queue otherwise), which shrinks the
/// advertised window until the reader catches up — backpressure through the
/// protocol instead of a parked pump.
fn deliver_recv(
    kcp: &mut Kcp<DatagramOut>,
    in_tx: &mpsc::Sender<Bytes>,
    spill: &mut std::collections::VecDeque<Bytes>,
    recv_buf: &mut [u8],
) -> Delivery {
    let mut delivered = false;

    // Flush the spill queue from earlier rounds first.
    while let Some(front) = spill.front() {
        match in_tx.try_send(front.clone()) {
            Ok(()) => {
                spill.pop_front();
                delivered = true;
            }
            Err(TrySendError::Full(_)) => break,
            Err(TrySendError::Closed(_)) => return Delivery::ReaderGone,
        }
    }

    // Then drain the KCP receive queue into the channel (or the spill).
    if spill.is_empty() {
        loop {
            match kcp.recv(recv_buf) {
                Ok(0)
                | Err(crate::kcp::Error::RecvQueueEmpty | crate::kcp::Error::ExpectingFragment) => {
                    break;
                }
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&recv_buf[..n]);
                    match in_tx.try_send(chunk) {
                        Ok(()) => delivered = true,
                        Err(TrySendError::Full(chunk)) => {
                            spill.push_back(chunk);
                            break;
                        }
                        Err(TrySendError::Closed(_)) => return Delivery::ReaderGone,
                    }
                }
                Err(e) => {
                    warn!("KCP recv failed: {e}");
                    break;
                }
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
    /// `kcp_send_all` failed midway — the pump must exit.
    Fatal,
}

/// Consume writer messages until the ARQ queue cap or the channel runs dry.
/// One select arm handles the whole batch: tokio's uniform select fairness
/// would otherwise starve the writer arm down to a fraction of the link
/// rate when the inbound channel is permanently ready under load.
fn drain_writer(
    kcp: &mut Kcp<DatagramOut>,
    out_rx: &mut mpsc::UnboundedReceiver<Bytes>,
    out_sem: &Semaphore,
    first: Bytes,
) -> Drain {
    let mut data = first;
    loop {
        if let Err(e) = kcp_send_all(kcp, &data) {
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
    rtt_ms: f64,
    rate_bps: f64,
    pacer: Pacer,
    last_ping: Instant,
    last_ping_us: u64,
    ping_outstanding: bool,
    clean_pongs: u32,
    pongs: u32,
}

impl PaceState {
    fn new(start: Instant) -> Self {
        // The pacer starts unthrottled (the cap): BDP-matched clamping was
        // tried and regressed single-stream throughput (the flush burst
        // already fills the window; a rate cap just adds bucket latency).
        // It exists to cut the rate on congestion signals (PONG timeouts).
        let rtt_ms = PACER_INIT_RTT_MS;
        Self {
            rtt_ms,
            rate_bps: PACER_MAX_BPS,
            pacer: Pacer::new(start),
            last_ping: start,
            last_ping_us: 0,
            ping_outstanding: false,
            clean_pongs: 0,
            pongs: 0,
        }
    }

    /// PONG for the most recent PING: blend the measured RTT into the EMA,
    /// re-anchor the pacing rate on the BDP estimate, and probe up after a
    /// run of clean PONGs. Timestamps are microseconds — on loopback the
    /// RTT is sub-millisecond, below a ms-resolution clock's reach.
    #[expect(
        clippy::cast_precision_loss,
        reason = "RTT deltas are bounded by the 5 s sanity window, far \
                  below f64's exact integer range"
    )]
    fn on_pong(&mut self, ping_us: u64, now_us: u64) {
        if ping_us == self.last_ping_us && self.ping_outstanding {
            let rtt_ms = (now_us.saturating_sub(ping_us) as f64) / 1000.0;
            if (0.05..=5000.0).contains(&rtt_ms) {
                // First sample replaces the (guessed) initial RTT outright:
                // an EMA from the 20 ms seed would need tens of PONGs to
                // converge on a sub-ms loopback path.
                self.rtt_ms = if self.pongs == 0 {
                    rtt_ms
                } else {
                    self.rtt_ms * 0.75 + rtt_ms * 0.25
                };
                self.pongs += 1;
            }
            self.ping_outstanding = false;
            self.clean_pongs += 1;
            if self.clean_pongs >= 4 {
                self.rate_bps = (self.rate_bps * PACER_UP_FACTOR).min(PACER_MAX_BPS);
                self.clean_pongs = 0;
            }
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

/// Drain KCP's outbound datagrams onto the wire, through the pacer. A
/// denied token or a full kernel send buffer drops the datagram — KCP's
/// ARQ still holds the segment and re-emits it on the next flush, so the
/// drain never blocks the pump.
async fn drain_dgrams(
    dgram_rx: &mut mpsc::UnboundedReceiver<Bytes>,
    net: &SessionNet,
    pace: &mut PaceState,
    sent_any: &mut bool,
) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use tokio::io::Interest;

        let mut batch: Vec<Bytes> = Vec::with_capacity(crate::transport::udp_batch::BATCH);
        let mut send_batch = crate::transport::udp_batch::SendBatch::new();
        loop {
            batch.clear();
            while batch.len() < crate::transport::udp_batch::BATCH {
                match dgram_rx.try_recv() {
                    Ok(d) => {
                        if pace
                            .pacer
                            .allow(Instant::now(), d.len(), pace.rate_bps)
                            .is_ok()
                        {
                            batch.push(d);
                        } else {
                            break; // denied: drop the copy, ARQ holds the segment
                        }
                    }
                    Err(_) => break,
                }
            }
            if batch.is_empty() {
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
                send_batch.send(net.socket.as_raw_fd(), net.peer, &batch)
            }) {
                Ok(k) => {
                    *sent_any = true;
                    if k < batch.len() {
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
        while let Ok(d) = dgram_rx.try_recv() {
            match pace.pacer.allow(Instant::now(), d.len(), pace.rate_bps) {
                Ok(()) => {
                    // Same park-then-try pattern as the Linux path.
                    if net.socket.writable().await.is_err() {
                        return;
                    }
                    match net.socket.try_send_to(&d, net.peer) {
                        Ok(_) => *sent_any = true,
                        // WouldBlock: drop and let the ARQ re-emit on the
                        // next flush; tokio's try_send_to clears the
                        // cached writability so the next park waits.
                        Err(e) => {
                            debug!("KCP datagram send failed (peer {}): {e}", net.peer);
                        }
                    }
                }
                Err(_) => break,
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
    spill: &std::collections::VecDeque<Bytes>,
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
    spill: &std::collections::VecDeque<Bytes>,
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
        }
    }
}

/// The per-session pump: owns the `Kcp` state machine and drives it between
/// the writer channel, the inbound-datagram channel and the update timer.
async fn run_session(
    mut kcp: Kcp<DatagramOut>,
    net: SessionNet,
    mut pkt_rx: mpsc::Receiver<Bytes>,
    mut dgram_rx: mpsc::UnboundedReceiver<Bytes>,
    mut out_rx: mpsc::UnboundedReceiver<Bytes>,
    out_sem: Arc<Semaphore>,
    in_tx: mpsc::Sender<Bytes>,
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
    let mut recv_buf = [0u8; RECV_BUF];
    // Chunks recv'd but not yet accepted by the reader channel.
    let mut spill: std::collections::VecDeque<Bytes> = std::collections::VecDeque::new();
    let mut ack_flush_due = false;
    // Adaptive send pacing + keepalive/RTT probing (see `PaceState`).
    let mut pace = PaceState::new(start);
    // Last SACK gap notification (throttled by SACK_COOLDOWN).
    let mut last_sack_sent = Instant::now();
    while let Some(delay) = pump_head(&mut kcp, &net, &mut pace, start).await {
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
                if let Err(e) = kcp.update(ms_now(start)) {
                    warn!("KCP session update failed: {e}");
                    break;
                }
            }
        }

        // Deliver received app data to the reader half — strictly non-blocking
        // (see `deliver_recv`): a stalled reader must never park the pump,
        // because parking would delay the acks/datagrams of everything
        // arriving meanwhile and the peer's RTO escalates (x1.5 per
        // retransmit in nodelay mode) into seconds-long stalls.
        let delivered = match deliver_recv(&mut kcp, &in_tx, &mut spill, &mut recv_buf) {
            Delivery::Done { delivered } => delivered,
            Delivery::ReaderGone => {
                debug!("KCP session reader gone, closing (peer {})", net.peer);
                break;
            }
        };

        // SACK gap detection (see `maybe_sack`): tell the peer to resend
        // the missing segment instead of waiting out the RTO backoff.
        maybe_sack(&mut kcp, &net, delivered, &spill, &mut last_sack_sent).await;

        // 3) Flush acks for the batch just consumed — now that delivery has
        //    drained the receive queue, the advertised window is honest.
        if ack_flush_due {
            ack_flush_due = false;
            if let Err(e) = flush_after_batch(&mut kcp) {
                warn!("KCP session ack flush failed: {e}");
                break;
            }
        }

        // 4) Push datagrams KCP emitted this round onto the wire — through the
        //    pacer (see `drain_dgrams`).
        drain_dgrams(&mut dgram_rx, &net, &mut pace, &mut sent_any).await;

        if kcp.is_dead_link() {
            debug!("KCP session dead link (peer {})", net.peer);
            break;
        }

        if closing {
            // Writer half is gone. With the reader also gone there is
            // nobody left to serve: flush the ARQ tail briefly, then exit.
            // With the reader still alive this is a half-close — keep
            // serving reads; a vanished peer is bounded by the dead-link
            // check above.
            if in_tx.is_closed()
                && closing_quiescent(
                    sent_any,
                    delivered,
                    &kcp,
                    &spill,
                    &mut quiet_rounds,
                    close_deadline,
                )
            {
                break;
            }
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
    let mut kcp = Kcp::new_stream(conv, DatagramOut { tx: dgram_tx });
    configure(&mut kcp);

    let (pkt_tx, pkt_rx) = mpsc::channel(DATAGRAM_CHANNEL_DEPTH);
    // Writer → pump: an unbounded channel gated by a semaphore (the pump
    // returns one permit per consumed write), because tokio's bounded
    // `Sender` has no poll-based send for `AsyncWrite::poll_write`.
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let out_sem = Arc::new(Semaphore::new(OUTBOUND_CHANNEL_DEPTH));
    let (in_tx, in_rx) = mpsc::channel(INBOUND_CHANNEL_DEPTH);

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

    in_rx: Option<mpsc::Receiver<Bytes>>,
    read_pending: Option<Bytes>,
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
        in_rx: mpsc::Receiver<Bytes>,
    ) -> KcpStream {
        KcpStream {
            out_tx: Some(out_tx),
            out_sem,
            out_acquire: None,
            in_rx: Some(in_rx),
            read_pending: None,
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
            if let Some(mut pending) = me.read_pending.take() {
                let n = std::cmp::min(buf.remaining(), pending.len());
                buf.put_slice(&pending[..n]);
                pending.advance(n);
                if !pending.is_empty() {
                    me.read_pending = Some(pending);
                }
                return Poll::Ready(Ok(()));
            }
            let Some(rx) = me.in_rx.as_mut() else {
                // Read half closed (pump gone): EOF.
                return Poll::Ready(Ok(()));
            };
            match rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) if !chunk.is_empty() => {
                    me.read_pending = Some(chunk);
                }
                Poll::Ready(Some(_empty)) => {} // skip, poll again
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
        let Some(tx) = me.out_tx.as_ref() else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session write half already shut down",
            )));
        };
        // Capacity gate: acquire one permit (registering the waker when the
        // pump is behind), then hand the write to the unbounded channel.
        let permit = match me.out_sem.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "KCP session closed",
                )));
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                let fut = me.out_acquire.get_or_insert_with(|| {
                    Box::pin(me.out_sem.clone().acquire_owned()) as Pin<Box<AcquirePermitFuture>>
                });
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(permit)) => {
                        me.out_acquire = None;
                        permit
                    }
                    // The semaphore is never closed explicitly; treat it as
                    // a dead session anyway.
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "KCP session closed",
                        )));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        };
        match tx.send(Bytes::copy_from_slice(buf)) {
            Ok(()) => {
                permit.forget();
                Poll::Ready(Ok(buf.len()))
            }
            // Pump gone: the session is dead.
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KCP session closed",
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
            "KCP listener up (stream mode, nodelay=1 interval=10ms fast-resend=2 nc=1 \
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
