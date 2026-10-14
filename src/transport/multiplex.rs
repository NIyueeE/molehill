//! yamux-based data channel multiplexing (`multiplex` feature).
//!
//! One extra physical connection ("tunnel") is dialed by the client right
//! after its registration succeeds. Both ends upgrade it to a yamux session;
//! afterwards every forwarded data channel is a cheap stream instead of a
//! full TCP + crypto handshake:
//!
//! ```text
//! client                          server
//!   │ DataChannelTunnelHello(nonce) ►   validated like a plain data channel
//!   │ ◄────────── Ack::Ok ──────────
//!   ╞══ yamux session (Mode::Client / Mode::Server) ══╗
//!   │ ── open_stream ──►  stream accepted ────────────┤ … pooled/paired
//! ```
//!
//! The decision belongs to the client alone (`[client].mux`): the server
//! adapts per connection based on which hello variant arrives, so mixed
//! deployments work without any coordination.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;

use crate::mux::{Config, Connection, Mode};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

/// A multiplexed stream adapted to tokio's IO traits.
pub type MuxStream = crate::mux::Stream;

/// Announce a freshly opened outbound stream, then deliver it to the caller.
///
/// rust-yamux opens outbound streams lazily: the SYN flag is piggybacked on
/// the first outbound frame, and a read-only consumer never produces one.
/// Our data-channel protocol is server-speaks-first (`StartForward*`), so a
/// freshly pooled stream starts by reading — without this zero-length write
/// it would never be announced to the server and both ends would wait for
/// each other forever.
///
/// This is a *future* rather than an await inside the driver loop: the
/// driver polls it alongside the connection state machine, so a
/// backpressured socket completes the announcement whenever it becomes
/// writable without ever stalling the tunnel's inbound processing.
struct SynAnnounce {
    stream: Option<MuxStream>,
    reply: Option<oneshot::Sender<Result<MuxStream, crate::mux::ConnectionError>>>,
}

impl std::future::Future for SynAnnounce {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        // Take the stream out; when it is already gone the announcement was
        // delivered, so stay total for a misused second poll.
        let Some(mut stream) = this.stream.take() else {
            return Poll::Ready(());
        };
        match tokio::io::AsyncWrite::poll_write(Pin::new(&mut stream), cx, &[]) {
            Poll::Ready(Ok(_)) => {
                if let Some(reply) = this.reply.take() {
                    let _ = reply.send(Ok(stream));
                }
                Poll::Ready(())
            }
            Poll::Ready(Err(e)) => {
                debug!(error = %e, "Failed to announce outbound multiplexed stream");
                if let Some(reply) = this.reply.take() {
                    let _ = reply.send(Err(crate::mux::ConnectionError::Closed));
                }
                Poll::Ready(())
            }
            Poll::Pending => {
                // Park the stream again for the next poll.
                this.stream = Some(stream);
                Poll::Pending
            }
        }
    }
}

/// Build the session configuration for every tunnel: a 32 MiB total
/// receive window (bounded loss backlog — yamux's own 1 GiB default
/// accumulates without bound under loss) with 64 streams, each guaranteed
/// the 256 KiB default credit, leaving 16 MiB for the auto-tuner.
///
/// The two values are coupled by an upstream invariant (`window >=
/// streams * 256 KiB` asserted on every setter), so they are fixed internal
/// constants rather than config knobs: tuning them independently measured
/// 30x regressions (streams that swallow the window pin every stream at
/// 256 KiB) and 30-65% throughput drops on delayed links (windows too
/// small for the auto-tuner). The frame split size stays at yamux's
/// 16 KiB default: larger frames (64 KiB) measured faster on loopback but
/// 30-60% slower under round-trip delay.
pub(crate) fn mux_config() -> Config {
    use crate::common::constants::{DEFAULT_MUX_MAX_STREAMS, DEFAULT_MUX_RECEIVE_WINDOW};

    let mut config = Config::default();
    // Setter order keeps the upstream assertion (`window >= 256 KiB *
    // streams`) satisfied at every step: lower the stream count under the
    // default 1 GiB window first, then bound the window under the final
    // stream count.
    config.set_max_num_streams(DEFAULT_MUX_MAX_STREAMS);
    config.set_max_connection_receive_window(Some(DEFAULT_MUX_RECEIVE_WINDOW));
    config
}

/// Periodically log the framing counters when `MOLEHILL_MUX_STATS=1`.
///
/// A diagnostic facility for attributing cost to the framing path: the
/// lines carry cumulative frame counts, so a reader that knows the window
/// (or takes the first and last line of a run) gets frames/s, and beside
/// the measured CPU that becomes CPU-per-frame. Off by default so normal
/// operation is silent.
fn spawn_framing_stats() {
    if std::env::var_os("MOLEHILL_MUX_STATS").is_none() {
        return;
    }
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        // A missed tick is not worth catching up on: the counters are
        // cumulative, so a late line still reports the true totals.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let (written, read, bytes) = crate::mux::framing_stats();
            info!(
                written,
                read, bytes, "mux-stats: cumulative framing counters"
            );
        }
    });
}
/// The stream cap of one tunnel, from the same constant `mux_config` sets: the
/// placement ceiling derives from it to keep every tunnel strictly below the
/// cap, because a cap hit makes the vendored engine log an unguarded `error!`.
pub(crate) fn stream_cap() -> usize {
    crate::common::constants::DEFAULT_MUX_MAX_STREAMS
}

/// The next tunnel id: unique per process, so two pools' tunnels can never be
/// confused in the client's pin accounting.
fn next_tunnel_id() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Spawn the driver task for a server-mode session, forwarding every inbound
/// stream (i.e. every requested data channel) into `tx`.
pub async fn run_server_tunnel<I>(io: I, config: Config, tx: mpsc::Sender<MuxStream>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    debug!("server tunnel driver started");
    spawn_framing_stats();
    let mut conn = Connection::new(io, config, Mode::Server);
    while let Some(result) = poll_fn(|cx| conn.poll_next_inbound(cx)).await {
        match result {
            Ok(stream) => {
                debug!("server tunnel accepted an inbound stream");
                if tx.send(stream).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// The pinned tunnel pool: bookkeeping, placement and repair.
// ---------------------------------------------------------------------------
use crate::transport::pool::{GrowReason, Placement, ShrinkReason, TunnelLoad};

/// One tunnel's bookkeeping counters, shared by the pool's placement and
/// telemetry code.
///
/// All of them are atomics because placement reads them while it holds only a
/// shared borrow of the tunnel list: inserting an entry would otherwise need a
/// write lock on the hot path of every data channel.
#[derive(Debug, Default)]
pub(crate) struct TunnelCounters {
    /// Established streams. Incremented when an open completes, decremented
    /// when the caller drops the stream ([`StreamLease`]).
    streams: std::sync::atomic::AtomicUsize,
    /// Opens requested but not yet completed on this tunnel.
    pending: std::sync::atomic::AtomicUsize,
    /// Bytes delivered from this tunnel's streams to the forwarding code.
    ///
    /// Counted per IO poll by [`crate::core::client::ClientDataChannel::Mux`]
    /// through a relaxed add — the predecessor of this pair was a process-wide
    /// `Mutex<HashMap>` the *same* poll took, which serialized every
    /// multiplexed byte behind one lock for an opt-in diagnostic.
    delivered: std::sync::atomic::AtomicU64,
    /// Bytes the forwarding code handed to this tunnel's streams.
    written: std::sync::atomic::AtomicU64,
    /// The KCP session's own ARQ counters, when this tunnel rides KCP (see
    /// [`crate::transport::health::SessionHealth`]): the retransmit share of the pair is
    /// the per-tunnel signal a carrier policy judges. `None` on a TCP tunnel
    /// — the kernel's TCP has no userspace retransmit counter to offer, which
    /// is also why the carrier rule is one-sided by construction.
    health: Option<std::sync::Arc<crate::transport::health::SessionHealth>>,
    /// What the **peer's** session for this tunnel has emitted and re-sent, as
    /// the peer reports it (`ControlChannelCmd::TunnelHealth`).
    ///
    /// One session has an engine at each end, so a tunnel's real ARQ share is
    /// both ends' counters together: a path that polices UDP destroys the
    /// *sender's* datagrams, and on a downstream workload that sender is the
    /// peer — measured at 22.9 % re-sent there against 0.04 % here. The
    /// sequence is what makes a stale report (one that overtook a newer sample
    /// in flight) ignorable.
    peer: std::sync::Mutex<PeerHealth>,
}

/// The peer's last report for one tunnel: its counters and the report's
/// sequence, or nothing until the first one arrives.
#[derive(Debug, Default, Clone, Copy)]
struct PeerHealth {
    counters: Option<(u64, u64)>,
    /// Bytes the peer's application *handed* this tunnel, cumulative. The
    /// delivery ratio's numerator is this end's received bytes and its
    /// denominator is this, so the pair says what fraction of what was offered
    /// actually arrived — the distinction the ARQ share could not make.
    handed: u64,
    seq: Option<u8>,
}

impl TunnelCounters {
    /// The tunnel's counters, with the KCP session's health when it rides KCP.
    fn with_health(
        health: Option<std::sync::Arc<crate::transport::health::SessionHealth>>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            streams: std::sync::atomic::AtomicUsize::new(0),
            pending: std::sync::atomic::AtomicUsize::new(0),
            delivered: std::sync::atomic::AtomicU64::new(0),
            written: std::sync::atomic::AtomicU64::new(0),
            health,
            peer: std::sync::Mutex::new(PeerHealth::default()),
        })
    }

    /// Bytes this end delivered off the tunnel to the forwarding code.
    pub(crate) fn received(&self) -> u64 {
        self.delivered.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes the peer's application has handed this tunnel, as it reports them.
    pub(crate) fn peer_handed(&self) -> u64 {
        self.peer.lock().map_or(0, |peer| peer.handed)
    }

    /// The tunnel's ARQ counters **from both ends**, summed: what this end's
    /// session emitted and re-sent, plus what the peer reports for the same
    /// session. `None` on a TCP tunnel (no userspace ARQ at all).
    ///
    /// Summing is what makes the signal direction-independent, which is the
    /// whole point of the peer's report: one session carries both directions,
    /// and the loss that matters may be entirely on the other one.
    pub(crate) fn health_counts(&self) -> Option<(u64, u64)> {
        let local = self.health.as_ref()?.counts();
        let peer = self
            .peer
            .lock()
            .map(|peer| peer.counters)
            .unwrap_or_default()
            .unwrap_or((0, 0));
        Some((
            local.0.saturating_add(peer.0),
            local.1.saturating_add(peer.1),
        ))
    }

    /// Record the peer's report for this tunnel, ignoring one that arrives out
    /// of order: the sequence is cumulative per tunnel connection, so an older
    /// sample must not overwrite a newer one.
    pub(crate) fn note_peer_health(&self, counters: (u64, u64), handed: u64, seq: u8) {
        let Ok(mut peer) = self.peer.lock() else {
            return;
        };
        let newer = peer.seq.is_none_or(|last| {
            // `wrapping_sub` in `u8` space: a sequence that wrapped is still
            // newer than the one before it, and one from two lives ago is not.
            seq.wrapping_sub(last) != 0 && seq.wrapping_sub(last) < 128
        });
        if newer {
            peer.counters = Some(counters);
            peer.handed = handed;
            peer.seq = Some(seq);
        }
    }

    fn load(&self) -> TunnelLoad {
        use std::sync::atomic::Ordering;
        TunnelLoad {
            streams: self.streams.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Relaxed),
            // The client's pin count lives in the `PinRegistry` (keyed by
            // tunnel id, not by index); placement does not read it.
            pinned: 0,
        }
    }

    /// Charge one open to this tunnel before it is awaited.
    fn reserve(&self) {
        use std::sync::atomic::Ordering;
        self.pending.fetch_add(1, Ordering::Relaxed);
    }

    /// The open failed or was abandoned: the reservation comes back.
    fn release(&self) {
        use std::sync::atomic::Ordering;
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }

    /// The open completed: the reservation becomes a live stream.
    fn charge(&self) {
        use std::sync::atomic::Ordering;
        self.streams.fetch_add(1, Ordering::Relaxed);
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record what one IO poll moved across this tunnel (see the field docs).
    fn count_bytes(&self, delivered: u64, written: u64) {
        use std::sync::atomic::Ordering;
        self.delivered.fetch_add(delivered, Ordering::Relaxed);
        self.written.fetch_add(written, Ordering::Relaxed);
    }

    /// The cumulative `(delivered, written)` pair, for the tunnel-stats line
    /// and the pool snapshot's per-tunnel window.
    fn bytes(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.delivered.load(Ordering::Relaxed),
            self.written.load(Ordering::Relaxed),
        )
    }
}

/// One opened stream, charged to its tunnel for exactly as long as it lives.
///
/// `ClientTunnel::open_stream` hands the raw stream back already charged;
/// wrapping it keeps the charge exact, because the stream is a `Drop` point no
/// matter which task ends up holding it (a forwarded visitor, a stripe group,
/// a pre-opened channel). A tunnel's stream count can therefore never leak past
/// the stream's life, which is what the placement ceiling reads.
///
/// The lease *is* the stream the client's forwarding code carries (it
/// implements both IO traits), so the charge lives exactly as long as the data
/// channel does.
pub(crate) struct StreamLease {
    stream: MuxStream,
    /// The id of the tunnel this stream is charged to (unique per process).
    tunnel_id: usize,
    counters: std::sync::Arc<TunnelCounters>,
    /// The pool this stream came from, for the capacity wake-up: an open that
    /// found every tunnel at its ceiling is waiting for exactly this drop, and
    /// a weak reference cannot keep a dead pool alive.
    pool: std::sync::Weak<PoolShared>,
}

impl StreamLease {
    /// The tunnel this stream is charged to (the pin accounting's key).
    pub(crate) fn tunnel_id(&self) -> usize {
        self.tunnel_id
    }

    /// Record what one IO poll moved across the stream's tunnel: the
    /// forwarding wrapper ([`crate::core::client::ClientDataChannel::Mux`])
    /// calls this instead of the global registry its predecessor kept — see
    /// [`TunnelCounters::count_bytes`].
    pub(crate) fn count_bytes(&self, delivered: u64, written: u64) {
        self.counters.count_bytes(delivered, written);
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.counters
            .streams
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(pool) = self.pool.upgrade() {
            // One stream of capacity came back. `notify_waiters` (not
            // `notify_one`) because every waiter re-reads the whole pool: the
            // wake-up is a hint that the state changed, not a hand-off of one
            // slot.
            pool.capacity_freed.notify_waiters();
        }
    }
}

impl tokio::io::AsyncRead for StreamLease {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for StreamLease {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}

/// The data-plane carrier a pool belongs to.
///
/// Part of the pool's key: two carriers cannot share a physical tunnel, so a
/// `(session, carrier)` pool is really one pool per carrier of that session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Carrier {
    /// Tunnels over the control transport's TCP stack (arms 0/1).
    Tcp,
    /// Tunnels over KCP-over-UDP sessions (arm 2, `kcp` feature).
    Kcp,
}

impl Carrier {
    /// The carrier's name, as the configuration writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Kcp => "kcp",
        }
    }
}

/// One pool timeline entry: a size change and the reason for it.
#[derive(Debug, Clone, Copy)]
struct PoolEvent {
    /// `true` for a repair, `false` for a death.
    grow: bool,
    /// `GrowReason::as_str` / `ShrinkReason::as_str`.
    reason: &'static str,
    from: usize,
    to: usize,
}

/// A snapshot of one live pool: the `MOLEHILL_POOL_STATS` line's content, and
/// what the integration suite asserts on.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    /// The pool's key in its session: `session` (shared) or `service:<id>`.
    pub key: String,
    /// `Carrier::as_str`.
    pub carrier: &'static str,
    /// The number of live tunnels.
    pub size: usize,
    /// How many tunnels the pool establishes (`[client.data].tunnels`).
    /// A pool at this size is complete; one below it is repairing.
    pub count: usize,
    /// The UDP-derived floor the pool's configured count must cover (D7).
    pub udp_floor: usize,
    /// Cumulative establishments since the pool was created.
    pub grows: u64,
    /// Cumulative removals since the pool was created: dead tunnels and
    /// completed carrier handovers. Dead entries are repaired; handovers retire
    /// and replace exactly one old-carrier tunnel.
    pub shrinks: u64,
    /// Per-tunnel `(streams, pending, pinned, carrier)`, in pool order.
    pub tunnels: Vec<(usize, usize, usize, Carrier)>,
    /// Per-tunnel cumulative `(delivered, written)` bytes, in the same order as
    /// [`PoolSnapshot::tunnels`]: the `MOLEHILL_TUNNEL_STATS` line's source and
    /// the windowed per-tunnel evidence a carrier policy differences.
    pub bytes: Vec<(u64, u64)>,
    /// Per-tunnel KCP ARQ health, cumulative `(datagrams_out, retransmits)`,
    /// in the same order — `None` where the tunnel does not ride KCP (a TCP
    /// tunnel's retransmit state stays in the kernel). The carrier rule's
    /// degradation signal: a healthy session re-sends ~nothing, a carrier
    /// losing its traffic re-sends segments by the thousands (S1, round 37).
    pub kcp_health: Vec<Option<(u64, u64)>>,
    /// Whether each entry in `tunnels` accepts new streams.
    pub draining: Vec<bool>,
    /// Active handover as `(old tunnel id, target carrier, phase)`.
    pub handover: Option<(usize, Carrier, &'static str)>,
}

impl PoolSnapshot {
    /// Established streams anywhere in the pool.
    #[must_use]
    pub fn streams(&self) -> usize {
        self.tunnels.iter().map(|(s, _, _, _)| *s).sum()
    }

    /// The pool's pinned peers, summed over its tunnels.
    #[must_use]
    pub fn pinned(&self) -> usize {
        self.tunnels.iter().map(|(_, _, p, _)| *p).sum()
    }
}

/// The established tunnels of one pool plus the placement bookkeeping.
struct PoolState {
    /// The live tunnels, in pool order. An index is stable between removals
    /// and names the tunnel in the telemetry.
    entries: Vec<PoolEntry>,
    /// Round-robin cursor: the tie-break of the least-loaded rule.
    next_cursor: usize,
    /// The count this pool establishes and keeps. `entries.len() < count` means
    /// a tunnel is missing and the repair tick will dial one; nothing else moves
    /// this number.
    count: usize,
    /// At most one tunnel is drained for a carrier move at a time.
    handover: Option<PoolHandover>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunnelPhase {
    Serving,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoverPhase {
    Draining,
    Dialing,
}

struct PoolHandover {
    tunnel_id: usize,
    target: Carrier,
    phase: HandoverPhase,
    /// When the current phase began: each phase has its own budget.
    phase_since: std::time::Instant,
}

/// One live tunnel of a pool.
struct PoolEntry {
    /// The carrier this tunnel was established on. Per entry, not per pool: a
    /// repair may ride a different one than the tunnels around it, and the
    /// telemetry has to say what is true rather than what the pool was born as.
    carrier: Carrier,
    /// Draining entries keep established streams but receive no new opens.
    phase: TunnelPhase,
    tunnel: ClientTunnel,
    /// Dropping this ends the driver; the pool holds it for the tunnel's whole
    /// life, so removal actually releases the connection.
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl PoolEntry {
    fn load(&self) -> TunnelLoad {
        self.tunnel.counters.load()
    }
}

/// One reserved (not yet completed) open: the tunnel it is charged to and the
/// placement data S1 records for it.
struct Reservation {
    /// The tunnel's index in the pool.
    index: usize,
    /// The pool the reservation was taken from, handed to the lease it becomes
    /// (see [`StreamLease::pool`]).
    pool: std::sync::Weak<PoolShared>,
    placement: Placement,
    tunnel: ClientTunnel,
}

/// What one pool needs to create a new tunnel: the dialer, plus the shutdown
/// sender of the driver it started.
pub type DialResult = Result<(ClientTunnel, tokio::sync::watch::Sender<bool>), String>;

/// The pool's way to add a tunnel. `ClientTunnel`s carry no index; the pool
/// assigns one when it inserts the tunnel.
pub type Dialer = std::sync::Arc<
    dyn Fn(Carrier) -> std::pin::Pin<Box<dyn std::future::Future<Output = DialResult> + Send>>
        + Send
        + Sync,
>;

/// Which carrier the pool should establish **next**.
///
/// A function rather than a value: the answer changes while the pool lives (the
/// client's carrier choice falls back, and a quality decision can move it), and
/// a pool that captured one carrier at birth kept repairing into a path that had
/// stopped working — the defect this parameter exists to fix. The pool does not
/// learn *why* the answer changed, only what it is.
pub type PreferredCarrier = std::sync::Arc<dyn Fn() -> Carrier + Send + Sync>;

/// Why an open failed.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The pool has no tunnel at all (nothing established, no dialer).
    NoTunnel,
    /// Every candidate tunnel refused the stream.
    Refused(crate::mux::ConnectionError),
    /// Every tunnel is at [`crate::transport::pool::TUNNEL_STREAM_CEILING`] and
    /// the pool could not reach its count (its own `tunnels`, or the server's
    /// `max_tunnels_per_client` valve), or the carrier's dial failed.
    ///
    /// A typed refusal rather than a queued open: the engine's cap is *fatal*
    /// to a tunnel, so the pool refuses one visitor instead of risking every
    /// other visitor on that tunnel. The alternative — waiting for a stream to
    /// retire — is what the caller does first (`CAPACITY_WAIT`); this is what
    /// it reports when the wait ran out.
    AtCapacity,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTunnel => write!(f, "the pool has no tunnel"),
            Self::Refused(e) => write!(f, "every tunnel refused the stream: {e}"),
            Self::AtCapacity => write!(
                f,
                "every tunnel is at its stream ceiling; this pool is at its configured tunnel \
                 count (`tunnels`)"
            ),
        }
    }
}

/// The pool's shared state: what placement, the maintenance tick and the
/// opt-in telemetry tasks read.
/// One health sample: every tunnel's cumulative counters, keyed by tunnel id, in
/// pool order — `(datagrams_out, retransmits, delivered, handed)`.
///
/// The first pair is this end's own ARQ (plus the peer's, summed) and the second
/// is the **delivery pair**: what this end received off the tunnel against what
/// the peer's application handed it. The rule judges on the second; the first is
/// kept because it is what the telemetry shows and what the misfire was measured
/// against.
#[cfg(feature = "client")]
type HealthSample = Vec<(usize, (u64, u64, u64, u64))>;

/// The carrier rule's probe state: one at a time, or the backoff after one.
///
/// A probe is a tunnel handed to the carrier the pool is *not* riding, so the
/// two carriers can be compared on the path they actually share — no local
/// threshold separates a policed carrier from a merely lossy one (measured:
/// the re-send share fires on both classes, the delivery ratio on neither,
/// because a reliable ARQ turns a policed path into throughput). The only way
/// to learn what the other carrier would do is to put a tunnel on it and
/// compare what the two carry.
#[cfg(feature = "client")]
#[derive(Default)]
struct ProbeState {
    /// The carrier a probe tunnel was handed to, and when: `None` when no
    /// probe is running. The probe's tunnel is whatever entry of the pool
    /// rides this carrier — there is exactly one while a probe runs, and the
    /// handover that created it is the pool's own mechanism.
    active: Option<(Carrier, std::time::Instant)>,
    /// No probe before this instant (`CARRIER_RULE_PROBE_BACKOFF` after one
    /// came back empty).
    backoff_until: Option<std::time::Instant>,
}

struct PoolShared {
    carrier: Carrier,
    started: std::time::Instant,
    diagnostic_handover_fired: std::sync::atomic::AtomicBool,
    /// When this pool's diagnostic handover fires, read from the environment
    /// **at pool creation**: the switch is documented as "that many seconds
    /// after the pool starts", so each pool answers for itself. (A process
    /// wide read would let whichever pool was created first consume the value
    /// for every pool created after it — and a test binary that creates
    /// several pools would hand the hatch to the first one only.)
    hatch_at: Option<std::time::Duration>,

    /// The pool's key within its session: `session` (shared) or
    /// `service:<name>`.
    key: String,
    /// Streams per tunnel (`DEFAULT_MUX_MAX_STREAMS`): the engine cap the
    /// placement ceiling leaves headroom below.
    stream_cap: usize,
    /// When a repair attempt last failed: the pool stops dialing until
    /// `GROW_FAILURE_COOLDOWN` has passed, or a tunnel dies (D14).
    grow_failed_at: std::sync::Mutex<Option<std::time::Instant>>,
    /// The UDP-derived floor (D7). The session maintains it from its active
    /// UDP services, and it survives a tunnel's death: what it encodes is how
    /// many tunnels the *services* need, not how many exist.
    udp_floor: std::sync::atomic::AtomicUsize,
    /// Counters for the opt-in telemetry line.
    grows: std::sync::atomic::AtomicU64,
    shrinks: std::sync::atomic::AtomicU64,
    /// The pool timeline, drained once per second by the telemetry task.
    events: std::sync::Mutex<Vec<PoolEvent>>,
    /// Set while the pool is establishing a tunnel, so two repair dials cannot
    /// overlap.
    resizing: std::sync::atomic::AtomicBool,
    /// Signalled when one repair attempt ends, worked or not. An open that
    /// found the pool empty waits on it for the tunnel the winner is dialing,
    /// instead of reserving against an empty pool.
    grown: tokio::sync::Notify,
    /// Signalled when a tunnel's stream count drops, so an open that found
    /// every tunnel at its ceiling can re-read the pool instead of failing
    /// while capacity is about to come back. Only ever waited on with a
    /// timeout (`CAPACITY_WAIT`).
    capacity_freed: tokio::sync::Notify,
    /// Dropped with the last real user of the pool: the maintenance and
    /// telemetry tasks hold only a `Weak` and exit on it.
    alive: std::sync::Arc<()>,
    /// The carrier rule's previous health sample (see `carrier_rule_step`):
    /// `None` until the first tick, and replaced every tick after. A window
    /// is this minus the previous — the same difference-two-snapshots shape
    /// the lane policy keeps.
    #[cfg(feature = "client")]
    health_sample: std::sync::Mutex<Option<HealthSample>>,
    /// How many consecutive ticks the carrier rule has held its verdict
    /// (see [`CARRIER_RULE_CONFIRM_TICKS`]).
    #[cfg(feature = "client")]
    rule_streak: std::sync::Mutex<u32>,
    /// The carrier rule's probe: which carrier a probe tunnel was handed to
    /// and when, or the backoff that follows one that came back empty.
    #[cfg(feature = "client")]
    probe: std::sync::Mutex<ProbeState>,
    /// Maintenance ticks since the pool started, so the rule can judge once per
    /// [`CARRIER_RULE_WINDOW_TICKS`] rather than once per tick.
    #[cfg(feature = "client")]
    rule_tick: std::sync::atomic::AtomicU32,
    /// Whether this pool's carrier rule runs at all, read from the
    /// environment **at pool creation** (the hatch's pattern): a test builds
    /// the rule on without touching process state.
    #[cfg(feature = "client")]
    rule_enabled: bool,
    /// The data endpoint's carrier decision, when the pool's owner handed one
    /// over: the authority a rule's move goes through, so a handover respects
    /// the same switch cooldown every other switch does. `None` on pools
    /// built without one (tests) — those move on the rule's own word.
    #[cfg(feature = "client")]
    carrier_choice: Option<std::sync::Arc<crate::transport::carrier::CarrierChoice>>,
}

impl PoolShared {
    /// Record one size change for the telemetry timeline. Bounded: a pool
    /// that flaps cannot grow the buffer without bound.
    fn push_event(&self, event: PoolEvent) {
        const MAX_EVENTS: usize = 64;
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.len() == MAX_EVENTS {
            events.remove(0);
        }
        events.push(event);
    }
}

/// The client's UDP pin accounting, shared by every hub of the pools a
/// session owns.
///
/// A peer is *pinned* to the data channel its route currently sends through,
/// which is a stream of one tunnel: `bind` maps a channel to that tunnel the
/// first time it carries a peer's traffic, and `update` moves the tunnel's
/// count up or down. The pool reads the counts through
/// [`TunnelPool::pinned_peers`], so a tunnel carrying a live UDP peer is never
/// shrunk (D30) — the client knows this locally, from its own route table, and
/// needs no reverse channel from the server.
#[derive(Debug, Default)]
pub struct PinRegistry {
    /// Channel id -> tunnel id, for the channels currently carrying peers.
    bound: std::sync::Mutex<std::collections::HashMap<u64, usize>>,
    /// How many channels a given tunnel currently carries.
    counts: std::sync::Mutex<std::collections::HashMap<usize, usize>>,
}

impl PinRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> PinRegistry {
        PinRegistry::default()
    }

    /// Record that channel `channel` is a stream of tunnel `tunnel`. Idempotent
    /// while the channel keeps its tunnel; a different tunnel re-binds it.
    pub(crate) fn bind(&self, channel: u64, tunnel: usize) {
        self.bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(channel, tunnel);
    }

    /// Forget a channel (its data channel ended).
    pub(crate) fn unbind(&self, channel: u64) {
        self.bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&channel);
    }

    /// Move one channel's pin up (`true`) or down (`false`).
    pub(crate) fn update(&self, channel: u64, pinned: bool) {
        let Some(tunnel) = self
            .bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&channel)
            .copied()
        else {
            return;
        };
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = counts.entry(tunnel).or_insert(0);
        if pinned {
            *entry = entry.saturating_add(1);
        } else {
            *entry = entry.saturating_sub(1);
        }
        if *entry == 0 {
            counts.remove(&tunnel);
        }
    }

    /// How many peers currently end on tunnel `tunnel`.
    #[must_use]
    pub fn pinned_on(&self, tunnel: usize) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&tunnel)
            .copied()
            .unwrap_or(0)
    }

    fn counts(&self) -> std::collections::HashMap<usize, usize> {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A pool of parallel client tunnels (arm 1 of the transport comparison:
/// N physical connections instead of one), pinned at its configured count.
///
/// `open_stream` picks the least-loaded tunnel (by `streams + pending`), with
/// the round-robin cursor as the tie-break; a tunnel whose driver died returns
/// `Closed` immediately and the placement falls through to the next candidate,
/// so only a pool where every tunnel is dead fails an open (the control
/// channel's heartbeat then triggers the usual full reconnect, which
/// re-establishes the tunnels).
///
/// `open_streams(n)` reserves all `n` before awaiting any, so a stripe group
/// of K lands on K distinct tunnels whenever the pool has K and shares them
/// when it has fewer (D24).
///
/// Clones share one state, so placement and repair see every service that uses
/// the pool — which is the whole point of a shared pool.
#[derive(Clone)]
pub struct TunnelPool {
    inner: std::sync::Arc<TunnelPoolInner>,
}

struct TunnelPoolInner {
    shared: std::sync::Arc<PoolShared>,
    state: parking_lot::Mutex<PoolState>,
    /// Creates a new tunnel for a repair. `None` on a pool built without one.
    dial: Option<Dialer>,
    /// The carrier a repair should establish now (see [`PreferredCarrier`]).
    preferred: PreferredCarrier,
    /// The client's UDP pin accounting: which peers end on which tunnel (D30).
    pins: std::sync::Arc<PinRegistry>,
}

impl TunnelPool {
    /// Wrap the tunnels an establishment pass already produced, on a pool that
    /// cannot repair (no dialer).
    #[must_use]
    pub fn new(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        tunnels: usize,
    ) -> TunnelPool {
        Self::with_dialer(
            carrier,
            key,
            initial,
            tunnels,
            None,
            std::sync::Arc::new(PinRegistry::new()),
        )
    }

    /// The same, with the dialer a repair uses (`None` on a pool that cannot
    /// replace a dead tunnel) and the client's UDP pin registry.
    pub fn with_dialer(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        tunnels: usize,
        dial: Option<Dialer>,
        pins: std::sync::Arc<PinRegistry>,
    ) -> TunnelPool {
        Self::with_preference(
            carrier,
            key,
            initial,
            tunnels,
            dial,
            std::sync::Arc::new(move || carrier),
            pins,
        )
    }

    /// The same, with the carrier a repair should establish decided at dial time
    /// rather than fixed at pool creation.
    ///
    /// This is the seam the resilience defect needed: a pool whose dialer is
    /// pinned to the carrier it was born on keeps repairing into a path that has
    /// stopped carrying it, while the client's own carrier choice has already
    /// fallen back.
    pub fn with_preference(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        tunnels: usize,
        dial: Option<Dialer>,
        preferred: PreferredCarrier,
        pins: std::sync::Arc<PinRegistry>,
    ) -> TunnelPool {
        Self::with_choice(carrier, key, initial, tunnels, dial, preferred, pins, None)
    }

    /// The same, and the data endpoint's carrier decision beside it: the
    /// carrier rule's moves go through the choice's `prefer`, so a handover
    /// respects the switch cooldown the endpoint's other switches do. `None`
    /// (tests, tooling) leaves the rule answering only to its own floors.
    #[expect(
        clippy::too_many_arguments,
        reason = "the pool constructor is the one seam every caller builds a                   pool through; a params struct would fork the existing call                   sites for one optional the tests pass as None"
    )]
    pub fn with_choice(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        tunnels: usize,
        dial: Option<Dialer>,
        preferred: PreferredCarrier,
        pins: std::sync::Arc<PinRegistry>,
        carrier_choice: Option<std::sync::Arc<crate::transport::carrier::CarrierChoice>>,
    ) -> TunnelPool {
        let rule_enabled = carrier_rule_enabled();
        let shared = std::sync::Arc::new(PoolShared {
            carrier,
            started: std::time::Instant::now(),
            diagnostic_handover_fired: std::sync::atomic::AtomicBool::new(false),
            hatch_at: diagnostic_handover_at(),
            key,
            stream_cap: stream_cap(),
            grow_failed_at: std::sync::Mutex::new(None),
            udp_floor: std::sync::atomic::AtomicUsize::new(0),
            grows: std::sync::atomic::AtomicU64::new(0),
            shrinks: std::sync::atomic::AtomicU64::new(0),
            events: std::sync::Mutex::new(Vec::new()),
            resizing: std::sync::atomic::AtomicBool::new(false),
            grown: tokio::sync::Notify::new(),
            capacity_freed: tokio::sync::Notify::new(),
            alive: std::sync::Arc::new(()),
            #[cfg(feature = "client")]
            health_sample: std::sync::Mutex::new(None),
            #[cfg(feature = "client")]
            rule_streak: std::sync::Mutex::new(0),
            #[cfg(feature = "client")]
            probe: std::sync::Mutex::new(ProbeState::default()),
            #[cfg(feature = "client")]
            rule_tick: std::sync::atomic::AtomicU32::new(0),
            #[cfg(feature = "client")]
            rule_enabled,
            #[cfg(feature = "client")]
            carrier_choice,
        });
        let mut state = PoolState {
            entries: Vec::new(),
            next_cursor: 0,
            count: tunnels.max(1),
            handover: None,
        };
        for (tunnel, shutdown) in initial {
            state.entries.push(PoolEntry {
                carrier,
                phase: TunnelPhase::Serving,
                tunnel,
                _shutdown: shutdown,
            });
        }
        let pool = TunnelPool {
            inner: std::sync::Arc::new(TunnelPoolInner {
                shared,
                state: parking_lot::Mutex::new(state),
                dial,
                preferred,
                pins,
            }),
        };
        // The repair tick has to run even when nothing else happens, so a
        // missing tunnel is dialed without a visitor arriving first: one task
        // per pool, exiting when the pool drops.
        pool.spawn_maintenance();
        pool_stats::register(&pool.inner);
        LIVE_POOLS
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::sync::Arc::downgrade(&pool.inner));
        pool
    }

    #[cfg(feature = "client")]
    fn diagnostic_handover(&self) {
        let Some(at) = self.inner.shared.hatch_at else {
            return;
        }; // Once per pool: the hatch measures *one* switch. The flag is set when
        // a handover starts, so a completed or timed-out one is never repeated;
        // it is deliberately NOT consumed on a tick that finds no eligible
        // candidate — a visitor mid-open or both tunnels busy is a moment's
        // transient state, and burning the hatch on it would measure nothing.
        if self.inner.shared.started.elapsed() < at
            || self
                .inner
                .shared
                .diagnostic_handover_fired
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // `from` is the carrier the pool's tunnels actually ride — after a
        // fallback a pool born on KCP may hold only TCP entries, and the hatch
        // has to move one of *those* — and the target is the other one.
        let state = self.inner.state.lock();
        let Some(&from) = state
            .entries
            .iter()
            .filter(|entry| entry.phase == TunnelPhase::Serving)
            .map(|entry| &entry.carrier)
            .next()
        else {
            return;
        };
        drop(state);
        let target = match from {
            Carrier::Kcp => Carrier::Tcp,
            Carrier::Tcp => Carrier::Kcp,
        };
        if self.begin_handover(from, target) {
            self.inner
                .shared
                .diagnostic_handover_fired
                .store(true, std::sync::atomic::Ordering::Relaxed);
            info!(
                pool = %self.inner.shared.key,
                from = from.as_str(),
                to = target.as_str(),
                "pool-stats: diagnostic tunnel carrier handover requested"
            );
        }
    }

    /// Log one judgement window: every tunnel's emitted/re-sent pair and its
    /// share, whether or not the window produced a verdict.
    ///
    /// **Every** window, not only the ones that fire: the distribution of the
    /// share across windows is what a detector has to survive, and a rule that
    /// speaks only when it fires cannot show why it stayed silent — or how
    /// close it came. One line per window per pool, at debug.
    #[cfg(feature = "client")]
    fn log_window(
        &self,
        last: &HealthSample,
        now: &HealthSample,
        firing: bool,
        suspicion: Option<f64>,
    ) {
        // This runs every maintenance tick of every pool, and its body
        // allocates (a Vec plus two Strings per tunnel) only to feed a DEBUG
        // event: when the level is filtered out the event is dropped whole,
        // strings and all. The gate makes the filtered case free, the same
        // discipline `note_gauges` and `PhaseTimer` follow — the diagnostic
        // costs nothing unless someone asked for it.
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let window: Vec<(usize, u64, String, String)> = now
            .iter()
            .map(|(id, (out, ret, delivered, handed))| {
                let before = last
                    .iter()
                    .find(|(p, _)| p == id)
                    .map_or((*out, *ret, *delivered, *handed), |(_, quad)| *quad);
                let emitted = out.saturating_sub(before.0);
                let resent = ret.saturating_sub(before.1);
                let share = window_share_pct(emitted, resent)
                    .map_or_else(|| "-".to_owned(), |s| format!("{s:.2}"));
                let got = delivered.saturating_sub(before.2);
                let offered = handed.saturating_sub(before.3);
                (
                    *id,
                    offered,
                    share,
                    format!("{:.1}", delivery_pct(got, offered)),
                )
            })
            .collect();
        debug!(
            pool = %self.inner.shared.key,
            firing,
            suspicion = ?suspicion,
            window = ?window,
            "carrier rule window: (tunnel, handed, re-sent share%, delivered%)"
        );
    }

    /// The quality rule the handover mechanism was built for: hand one KCP
    /// tunnel over to TCP when its session is measurably losing traffic.
    ///
    /// The signal is the tunnel's own ARQ counters (see
    /// [`crate::transport::health::SessionHealth`]): the retransmit share of what the
    /// session emits. Measured before this existed (S1, round 37): a healthy
    /// carrier re-sends ~nothing (`retransmits` 1 against `datagrams_out` in
    /// the hundreds of thousands per cell), a carrier losing its traffic
    /// re-sends by the thousands — a separation orders of magnitude wide, so
    /// the floors below sit far inside it and far above scatter. TCP has no
    /// userspace equivalent, so the rule is **one-sided by construction**: it
    /// moves KCP → TCP on KCP's own evidence, and never judges a TCP tunnel
    /// (the kernel keeps its retransmit state where userspace cannot read
    /// it); a move back is the L3 policy's business, driven by that rule's
    /// evidence chain.
    ///
    /// The window is `carrier_health_window()`'s last sample minus this one,
    /// per tunnel — the same difference-two-snapshots shape the lane policy
    /// uses. [`CARRIER_RULE_MIN_DATAGRAMS`] keeps a quiet session from being
    /// judged on one stray retransmit; [`CARRIER_RULE_RETRANSMIT_SHARE_PCT`]
    /// is the share at which the cell the S1 ran on says the carrier is
    /// losing (1 % loss path, KCP re-sending 5.7 % at its ceiling); a
    /// retransmitting tunnel is preferred for the move over a clean one of
    /// the same carrier (the worst lane leaves first, as the lane policy
    /// retires its worst).
    ///
    /// The pool keeps the verdict for [`CARRIER_RULE_CONFIRM_TICKS`]
    /// maintenance ticks (one per 50 ms tick here, unlike the lane policy's
    /// one-second ticks — a burst of loss is minutes long by these units
    /// before anything moves) before asking [`Self::begin_handover`], and the
    /// endpoint's [`crate::transport::carrier::CarrierChoice::prefer`] is the
    /// authority: the pool itself only moves a tunnel, the *choice* moves
    /// when the policy says so, under the same 120 s cooldown every other
    /// switch respects. `MOLEHILL_TUNNEL_CARRIER_RULE=0` switches the whole
    /// rule off (the hatch's `MOLEHILL_TUNNEL_SWITCH_AT` stays independent of
    /// it — one is a measurement instrument, the other a policy).
    #[cfg(feature = "client")]
    fn carrier_rule_step(&self) {
        if !self.inner.shared.rule_enabled {
            return;
        }
        // A forced carrier is the run's answer, not a preference to be argued
        // with (the same guard the lane policy's carrier_step keeps): the
        // environment pinned the carrier for a measurement, and moving
        // tunnels against it would poison the arm.
        if crate::transport::carrier::forced_carrier().is_some() {
            return;
        }
        // Judge once per window, not once per tick: the volume floor is a
        // *window* floor, and a 50 ms window cannot hold enough datagrams to
        // meet it on a slow path (see `CARRIER_RULE_MIN_DATAGRAMS`).
        let tick = self
            .inner
            .shared
            .rule_tick
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        if !tick.is_multiple_of(CARRIER_RULE_WINDOW_TICKS) {
            return;
        }
        let now = self.health_sample();
        let mut previous = self
            .inner
            .shared
            .health_sample
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(last) = previous.replace(now.clone()) else {
            return; // a first sample is a baseline, not a window
        };
        let verdict = carrier_rule_verdict(&last, &now);
        // The suspicion half: the worst re-sent share on any tunnel carrying
        // material traffic. The delivery verdict above names a tunnel that
        // *stops* delivering; the share names a sender that is *fighting* the
        // path — on the policed condition it is the only local signal that
        // fires at all (measured: 27 % there, delivery pinned at 99 % by the
        // ARQ's own re-sends), and it fires on a merely lossy path too, which
        // is why what it earns is a probe rather than a move.
        let share = carrier_rule_suspicion(&last, &now);
        self.log_window(&last, &now, verdict.is_some(), share);
        // An active probe is judged on every window, whether or not a
        // suspicion fired in it: the probe's verdict comes from the two
        // carriers' carried bytes, and gating it on the sender would gate the
        // answer on the metric measured to be the wrong shape.
        self.probe_judge(&last, &now);
        drop(previous);
        let Some((tunnel_id, share_pct)) = verdict else {
            // No delivery verdict: the streak resets (one clean tick is
            // enough — the confirm ticks exist to ride out bursts, not to
            // require a unanimous history). A share suspicion starts a probe
            // directly: it needs no confirm streak, because nothing moves on
            // it — the probe measures and the comparison decides.
            if share.is_some() && self.probe_step() {
                return;
            }
            *self
                .inner
                .shared
                .rule_streak
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = 0;
            return;
        };
        let mut streak = self
            .inner
            .shared
            .rule_streak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *streak += 1;
        if *streak < CARRIER_RULE_CONFIRM_TICKS {
            return;
        }
        *streak = 0;
        drop(streak);
        // The suspicion is confirmed — but the sender's own share cannot tell
        // a policed carrier from a merely lossy one (measured, round 6: it
        // fires on both classes), so the confirmed move is a **probe**, not a
        // flip: hand one tunnel to the other carrier and let the two carry
        // the same path side by side. `probe_step` starts it, or `false`
        // means no probe is possible right now (a one-tunnel pool, a handover
        // already in flight, the backoff still running) and the flip below
        // is what the suspicion earned instead.
        if self.probe_step() {
            return;
        }
        // The move: exclude the tunnel from new placements, and flip the
        // endpoint's choice so every later dial rides TCP under the switch
        // cooldown. `prefer` is refused inside the cooldown — then the
        // handover does not start either: a carrier that cannot change yet
        // must not have a tunnel taken out of service for it.
        let switched = self
            .inner
            .shared
            .carrier_choice
            .as_deref()
            .is_none_or(|choice| {
                choice.prefer(
                    Carrier::Tcp,
                    &format!(
                        "tunnel {tunnel_id} re-sent {share_pct:.1}% of its datagrams over the window"
                    ),
                )
            });
        if !switched {
            debug!(
                pool = %self.inner.shared.key,
                "carrier rule: move to tcp deferred by the switch cooldown"
            );
            return;
        }
        if self.begin_handover(Carrier::Kcp, Carrier::Tcp) {
            info!(
                pool = %self.inner.shared.key,
                tunnel = tunnel_id,
                share = format!("{share_pct:.1}%"),
                "carrier rule: tunnel re-sent its share of the window; \
                 handing it over to tcp"
            );
        }
    }

    /// One sample of every tunnel's KCP health, keyed by tunnel id, in pool
    /// order — the rule's window input.
    #[cfg(feature = "client")]
    fn health_sample(&self) -> HealthSample {
        // The rule's own tick may block briefly on the state lock (a
        // placement holding it): a skipped sample would widen the next
        // window's baseline instead of the window, so the rule waits rather
        // than skips. The telemetry path keeps its `try_lock`.
        let state = self.inner.state.lock();
        state
            .entries
            .iter()
            .map(|e| {
                (
                    e.tunnel.id,
                    // The ARQ pair (both ends summed — the peer's report is what
                    // makes it direction-independent) and the delivery pair: what
                    // this end received off the tunnel against what the peer's
                    // application handed it. The rule judges on the second.
                    {
                        let arq = e.tunnel.counters.health_counts().unwrap_or((0, 0));
                        (
                            arq.0,
                            arq.1,
                            e.tunnel.counters.received(),
                            e.tunnel.counters.peer_handed(),
                        )
                    },
                )
            })
            .collect()
    }

    /// Record one tunnel's peer-side health report.
    ///
    /// The client is the deciding end and the tunnels are named by the id it
    /// assigned them (`next_tunnel_id`), so a report is routed by that id alone —
    /// there is no per-service ambiguity to resolve, and a tunnel the pool no
    /// longer holds (a report in flight when it was repaired) is simply dropped.
    /// Returns the tunnel's counters **after** recording the report — both ends
    /// summed — so the caller can see the value it just contributed to, or
    /// `None` when this pool does not hold that tunnel.
    pub fn note_peer_health(
        &self,
        tunnel: crate::protocol::TunnelId,
        counters: (u64, u64),
        handed: u64,
        seq: u8,
    ) -> Option<(u64, u64)> {
        let wanted = usize::try_from(u32::from_be_bytes(tunnel.0)).unwrap_or(usize::MAX);
        let state = self.inner.state.lock();
        let Some(entry) = state
            .entries
            .iter()
            .find(|entry| entry.tunnel.id() == wanted)
        else {
            debug!(
                pool = %self.inner.shared.key,
                tunnel = wanted,
                held = ?state.entries.iter().map(|e| e.tunnel.id()).collect::<Vec<_>>(),
                "peer health report for a tunnel this pool does not hold"
            );
            return None;
        };
        entry
            .tunnel
            .counters
            .note_peer_health(counters, handed, seq);
        entry.tunnel.counters.health_counts()
    }

    /// Start the rule's probe, or report that one is already running: `true`
    /// means a probe is now active (the caller's flip waits for its verdict),
    /// `false` that none was possible and the caller should act on its own
    /// suspicion.
    ///
    /// A probe is one tunnel of the pool handed to the carrier the pool is
    /// not riding — the same `begin_handover` every move uses, the difference
    /// is intent: the pool's own carrier choice stays where it is and the
    /// probe is *measured* (per-window carried bytes, from the same health
    /// sample the rule already takes) until the two carriers separate or the
    /// budget runs out. A probe that came back empty parks the pool in
    /// [`CARRIER_RULE_PROBE_BACKOFF`] instead of being retried at the next
    /// window.
    #[cfg(feature = "client")]
    fn probe_step(&self) -> bool {
        let mut probe = self
            .inner
            .shared
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if probe.active.is_some() {
            return true; // already running; `probe_judge` owns its verdict
        }
        if probe
            .backoff_until
            .is_some_and(|until| std::time::Instant::now() < until)
        {
            return false;
        }
        // The probe rides the carrier the pool is NOT on: a Kcp pool probes
        // tcp, and a Tcp pool probes kcp (the shape the lane policy's
        // suspicion-driven probe takes on its own scale).
        let probed = match self.inner.shared.carrier {
            Carrier::Kcp => Carrier::Tcp,
            Carrier::Tcp => Carrier::Kcp,
        };
        if self.begin_handover(self.inner.shared.carrier, probed) {
            probe.active = Some((probed, std::time::Instant::now()));
            info!(
                pool = %self.inner.shared.key,
                probe = probed.as_str(),
                "carrier rule: suspicion confirmed; probing the other carrier \
                 with one tunnel"
            );
            true
        } else {
            // Nothing to probe with (a one-tunnel pool, or a handover already
            // in flight): the ordinary flip is the only move available.
            false
        }
    }

    /// Judge the running probe on one window: the pool carrier's per-tunnel
    /// carried bytes against the probe tunnel's, acted on when they separate.
    ///
    /// A probe that stays inconclusive for [`CARRIER_RULE_PROBE_MAX`] is
    /// given back (and its backoff starts) here rather than by `probe_step`,
    /// because this is where the windows are — the caller already took the
    /// health sample the verdict reads.
    #[cfg(feature = "client")]
    fn probe_judge(&self, last: &HealthSample, now: &HealthSample) {
        let seconds = f64::from(CARRIER_RULE_WINDOW_TICKS) * 0.05;
        let probe_tunnel = {
            let probe = self
                .inner
                .shared
                .probe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match probe.active {
                Some((probed, started)) if started.elapsed() <= CARRIER_RULE_PROBE_MAX => probed,
                Some((probed, _)) => {
                    // Inconclusive for the whole budget: the probe comes back
                    // and the pool pays its backoff before suspecting again.
                    let mut probe = self
                        .inner
                        .shared
                        .probe
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if probe.active.is_some() {
                        probe.active = None;
                        probe.backoff_until =
                            Some(std::time::Instant::now() + CARRIER_RULE_PROBE_BACKOFF);
                        drop(probe);
                        info!(
                            pool = %self.inner.shared.key,
                            "carrier rule: probe inconclusive for its whole budget; \
                             giving the tunnel back and backing off"
                        );
                        self.begin_handover(probed, self.inner.shared.carrier);
                    }
                    return;
                }
                None => return,
            }
        };
        let Some(probe_id) = self.probe_tunnel_of(probe_tunnel) else {
            // The probe's tunnel is gone (a repair removed it, or the handover
            // that dialled it has not completed): there is nothing to compare.
            return;
        };
        let (chosen_bytes, probe_bytes) = probe_carried_bytes(last, now, probe_id);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's byte counts are far below 2^53, the point where an f64 stops \
                      counting integers exactly"
        )]
        let (chosen_per_s, probe_per_s) =
            (chosen_bytes as f64 / seconds, probe_bytes as f64 / seconds);
        // A probe whose window carried (near) nothing has not been measured:
        // placement routes new streams to it, but a window can close before
        // the first open lands. Judging an idle tunnel against a busy
        // incumbent reads "carries less" on zero evidence and gives the probe
        // back — measured, four probes in a row reverted on exactly that
        // artifact. Hold until the probed carrier has carried material
        // traffic; the budget expires it if the routing never fills it.
        if probe_per_s < CARRIER_RULE_PROBE_MIN_BYTES_PER_S {
            return;
        }
        match probe_verdict(chosen_per_s, probe_per_s) {
            ProbeVerdict::Hold => {}
            ProbeVerdict::Move => {
                if !self.probe_conclude(probe_tunnel) {
                    return;
                }
                info!(
                    pool = %self.inner.shared.key,
                    probe_bytes_per_s = format!("{probe_per_s:.0}"),
                    chosen_bytes_per_s = format!("{chosen_per_s:.0}"),
                    "carrier rule: the probe's carrier carries materially more; \
                     moving the pool to it"
                );
                let switched = self
                    .inner
                    .shared
                    .carrier_choice
                    .as_deref()
                    .is_none_or(|choice| {
                        choice.prefer(
                            probe_tunnel,
                            &format!(
                                "probe tunnel carried {probe_per_s:.0} B/s against \
                                 {chosen_per_s:.0} B/s on the pool's carrier"
                            ),
                        )
                    });
                if switched {
                    // The probe's tunnel is the seed of the new carrier: the
                    // move hands a tunnel of the *old* carrier to the probed
                    // one, which is what flips the pool's entries over.
                    self.begin_handover(self.inner.shared.carrier, probe_tunnel);
                }
            }
            ProbeVerdict::Revert => {
                if !self.probe_conclude(probe_tunnel) {
                    return;
                }
                // The probed carrier carried less: the suspicion was a lossy
                // path, not a policed one, and probing again at the next
                // window would be churn. The backoff is what makes a healthy
                // path pay one split tunnel for half a minute and nothing
                // after ([`CARRIER_RULE_PROBE_BACKOFF`]).
                self.inner
                    .shared
                    .probe
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .backoff_until = Some(std::time::Instant::now() + CARRIER_RULE_PROBE_BACKOFF);
                info!(
                    pool = %self.inner.shared.key,
                    probe_bytes_per_s = format!("{probe_per_s:.0}"),
                    chosen_bytes_per_s = format!("{chosen_per_s:.0}"),
                    "carrier rule: the probe's carrier carries materially less; \
                     giving the tunnel back and backing off"
                );
                self.begin_handover(probe_tunnel, self.inner.shared.carrier);
            }
        }
    }

    /// Close out a probe whose verdict has been acted on, or `false` when
    /// another path (the budget expiring mid-window) closed it first.
    #[cfg(feature = "client")]
    fn probe_conclude(&self, probed: Carrier) -> bool {
        let mut probe = self
            .inner
            .shared
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if probe.active.as_ref().is_none_or(|(p, _)| *p != probed) {
            return false;
        }
        probe.active = None;
        true
    }

    /// The pool's probe tunnel: the one entry riding `carrier` that the rest
    /// of the pool does not. There is exactly one while a probe runs — the
    /// handover that created it removed its old-carrier twin — and `None`
    /// when the handover is still dialling (no entry rides the probed carrier
    /// yet).
    #[cfg(feature = "client")]
    fn probe_tunnel_of(&self, probed: Carrier) -> Option<usize> {
        let state = self.inner.state.lock();
        let candidates: Vec<usize> = state
            .entries
            .iter()
            .filter(|entry| entry.phase == TunnelPhase::Serving && entry.carrier == probed)
            .map(|entry| entry.tunnel.id())
            .collect();
        match candidates.as_slice() {
            [id] => Some(*id),
            // Zero: the probe's replacement is still dialling. More than one:
            // a repair raced the probe and dialled onto the probed carrier
            // too — the comparison is ambiguous, so it waits for the pool to
            // settle rather than read two tunnels as one probe.
            _ => None,
        }
    }

    /// Begin a no-disruption carrier handover: exclude one unpinned tunnel of
    /// `from` from future placement. Its live streams stay on it; the repair
    /// dials its replacement on `target` once the entry drains to zero.
    fn begin_handover(&self, from: Carrier, target: Carrier) -> bool {
        if from == target {
            return false;
        }
        let mut state = self.inner.state.lock();
        if state.handover.is_some()
            || state
                .entries
                .iter()
                .filter(|entry| entry.phase == TunnelPhase::Serving)
                .count()
                <= 1
        {
            return false;
        }
        let pin_counts = self.inner.pins.counts();
        let Some(entry) = state
            .entries
            .iter_mut()
            .filter(|entry| entry.phase == TunnelPhase::Serving && entry.carrier == from)
            .filter(|entry| pin_counts.get(&entry.tunnel.id()).copied().unwrap_or(0) == 0)
            .min_by_key(|entry| entry.load().total())
        else {
            return false;
        };
        let tunnel_id = entry.tunnel.id();
        entry.phase = TunnelPhase::Draining;
        let now = std::time::Instant::now();
        state.handover = Some(PoolHandover {
            tunnel_id,
            target,
            phase: HandoverPhase::Draining,
            phase_since: now,
        });
        true
    }

    fn handover_step(&self) {
        let mut state = self.inner.state.lock();
        let Some(handover) = state.handover.as_ref() else {
            return;
        };
        // Every phase is bounded, for the reason the lane drain has a budget:
        // a pool one tunnel short for ever is worse than the carrier the
        // handover was trying to leave.
        let elapsed = handover.phase_since.elapsed();
        let budget = match handover.phase {
            // Draining waits for live streams to end on their own — a long
            // flow finishes where it is. Past the budget the move is called
            // off and the tunnel returns to service.
            HandoverPhase::Draining => crate::transport::carrier::SWITCH_COOLDOWN,
            // Dialing waits for one replacement, which a healthy dial answers
            // well inside the slowest carrier's own establish timeout; a
            // target that cannot be established (UDP blocked, server down)
            // releases the record so repair dials the live preference instead.
            HandoverPhase::Dialing => crate::transport::pool::COLD_GROW_WAIT,
        };
        let tunnel_id = handover.tunnel_id;
        let phase = handover.phase;
        if elapsed < budget {
            if phase == HandoverPhase::Dialing {
                return;
            }
        } else {
            state.handover = None;
            if phase == HandoverPhase::Draining
                && let Some(entry) = state
                    .entries
                    .iter_mut()
                    .find(|entry| entry.tunnel.id() == tunnel_id)
            {
                entry.phase = TunnelPhase::Serving;
            }
            debug!(
                pool = %self.inner.shared.key,
                phase = match phase {
                    HandoverPhase::Draining => "draining",
                    HandoverPhase::Dialing => "dialing",
                },
                "carrier handover timed out; pool resumes its ordinary repair"
            );
            return;
        }
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.tunnel.id() == tunnel_id)
        else {
            state.handover = None;
            return;
        };
        if !state.entries[index].tunnel.is_alive() {
            state.entries.remove(index);
            state.handover = None;
            drop(state);
            self.release_growth_hold();
            return;
        }
        let load = state.entries[index].load();
        let pinned = self.inner.pins.pinned_on(handover.tunnel_id);
        if load.streams == 0 && load.pending == 0 && pinned == 0 {
            if let Some(handover) = state.handover.as_mut() {
                handover.phase = HandoverPhase::Dialing;
                handover.phase_since = std::time::Instant::now();
            }
            let entry = state.entries.remove(index);
            drop(entry);
            let from = state.entries.len() + 1;
            let to = state.entries.len();
            drop(state);
            self.inner
                .shared
                .shrinks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.shared.push_event(PoolEvent {
                grow: false,
                reason: "handover",
                from,
                to,
            });
            info!(
                pool = %self.inner.shared.key,
                waited_ms = elapsed.as_millis(),
                "pool-stats: carrier handover drained tunnel"
            );
            self.release_growth_hold();
        }
    }

    /// The pool's key within its session (`session`, or `service:<name>`).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.shared.key
    }

    /// The carrier this pool's tunnels are established over.
    #[cfg(test)]
    #[must_use]
    pub fn carrier(&self) -> Carrier {
        self.inner.shared.carrier
    }

    /// The pool's size: the number of live tunnels.
    #[must_use]
    pub fn size(&self) -> usize {
        self.inner.state.lock().entries.len()
    }

    /// The UDP-derived floor (D7) the pool's configured count must cover.
    pub fn set_udp_floor(&self, floor: usize) {
        self.inner
            .shared
            .udp_floor
            .store(floor, std::sync::atomic::Ordering::Relaxed);
    }

    /// A consistent view of the pool: the telemetry line's content, and what
    /// the integration suite asserts on.
    #[cfg(test)]
    #[must_use]
    pub fn snapshot(&self) -> PoolSnapshot {
        self.try_snapshot().unwrap_or_else(|| PoolSnapshot {
            key: self.inner.shared.key.clone(),
            carrier: self.inner.shared.carrier.as_str(),
            size: 0,
            count: 0,
            udp_floor: 0,
            grows: 0,
            shrinks: 0,
            tunnels: Vec::new(),
            bytes: Vec::new(),
            kcp_health: Vec::new(),
            draining: Vec::new(),
            handover: None,
        })
    }

    /// The same, without waiting for the pool lock: the telemetry task and the
    /// live-pool registry must never block a data-channel placement.
    fn try_snapshot(&self) -> Option<PoolSnapshot> {
        use std::sync::atomic::Ordering;
        let state = self.inner.state.try_lock()?;
        Some(PoolSnapshot {
            key: self.inner.shared.key.clone(),
            carrier: self.inner.shared.carrier.as_str(),
            size: state.entries.len(),
            count: state.count,
            udp_floor: self.inner.shared.udp_floor.load(Ordering::Relaxed),
            grows: self.inner.shared.grows.load(Ordering::Relaxed),
            shrinks: self.inner.shared.shrinks.load(Ordering::Relaxed),
            tunnels: state
                .entries
                .iter()
                .map(|e| {
                    let load = e.load();
                    (
                        load.streams,
                        load.pending,
                        self.inner.pins.pinned_on(e.tunnel.id()),
                        e.carrier,
                    )
                })
                .collect(),
            bytes: state
                .entries
                .iter()
                .map(|e| e.tunnel.counters.bytes())
                .collect(),
            // The rule's own reading — both ends summed — rather than this
            // end's half: an operator watching the line sees what the policy
            // acts on, and the peer's half is where a policed path shows up.
            kcp_health: state
                .entries
                .iter()
                .map(|e| e.tunnel.counters.health_counts())
                .collect(),
            draining: state
                .entries
                .iter()
                .map(|entry| entry.phase == TunnelPhase::Draining)
                .collect(),
            handover: state.handover.as_ref().map(|handover| {
                let phase = match handover.phase {
                    HandoverPhase::Draining => "draining",
                    HandoverPhase::Dialing => "dialing",
                };
                (handover.tunnel_id, handover.target, phase)
            }),
        })
    }

    /// The client's pin accounting, shared with every UDP hub of the session.
    #[cfg(test)]
    #[must_use]
    pub fn pins(&self) -> std::sync::Arc<PinRegistry> {
        std::sync::Arc::clone(&self.inner.pins)
    }

    /// How many UDP peers currently end on the tunnel at `index` (D30).
    #[cfg(test)]
    #[must_use]
    pub fn pinned_peers(&self, index: usize) -> usize {
        let Some(id) = self
            .inner
            .state
            .lock()
            .entries
            .get(index)
            .map(|e| e.tunnel.id)
        else {
            return 0;
        };
        self.inner.pins.pinned_on(id)
    }

    /// Open one stream, on the least-loaded tunnel.
    ///
    /// An empty pool repairs synchronously here — the state a start whose
    /// establishment dials all failed leaves — so the first open answers with a
    /// fresh tunnel instead of failing; every other repair happens on the
    /// maintenance tick. The reservation is charged before the first `await`,
    /// which is what makes back-to-back opens land on distinct tunnels while the
    /// pool has them.
    pub(crate) async fn open_stream(&self) -> Result<StreamLease, OpenError> {
        self.open_avoiding(&[]).await
    }

    /// Open one stream for a stripe of a group, on a tunnel the group does not
    /// already occupy *while one is left* (D24 structural).
    ///
    /// `used` are the process-unique tunnel ids the group's earlier stripes
    /// took and `stripes` is the group's K — the server names the group before
    /// its channels are opened, which is what lets the client do what the
    /// placement rule alone never could.
    ///
    /// A pinned pool cannot dial a tunnel per stripe. The exclusion is a
    /// preference with a floor: the group spreads over the tunnels that exist
    /// and, when it has more stripes than the pool has members, its extra
    /// stripes share them rather than failing.
    pub(crate) async fn open_stream_on_distinct(
        &self,
        used: &[usize],
        stripes: usize,
    ) -> Result<StreamLease, OpenError> {
        // `stripes` stays in the signature because the caller still sizes the
        // request by it; placement reads only the group's occupied tunnels.
        let _ = stripes;
        self.open_avoiding(used).await
    }

    /// The body of every open: `avoid` names tunnels this request must not
    /// take while it can take another (only a stripe group passes a non-empty
    /// list — see [`Self::open_stream_on_distinct`]).
    async fn open_avoiding(&self, avoid: &[usize]) -> Result<StreamLease, OpenError> {
        if self.size() == 0 {
            self.grow(GrowReason::Repair).await;
            // The repair above is a no-op when another task already holds the
            // resize flag, so a *concurrent* first open would reserve against
            // a still-empty pool and fail (`NoTunnel`) while the winner is
            // dialing the very tunnel it needs. Wait for that attempt instead;
            // the timeout is only a backstop for a dialer that never returns.
            let _ =
                tokio::time::timeout(crate::transport::pool::COLD_GROW_WAIT, self.await_growth(1))
                    .await;
        }
        let ceiling = self.ceiling();
        let avoid = self.indices_of(avoid);
        let Some(reservation) = self.reserve(&[], &avoid, ceiling) else {
            // Nothing under the ceiling. An empty pool is a different failure
            // from a full one — the dialer could not bring a tunnel up — and
            // reporting it as `AtCapacity` would blame the load for the
            // network.
            if self.size() == 0 {
                return Err(OpenError::NoTunnel);
            }
            // Every tunnel carries as many streams as it may, and a pinned pool
            // cannot dial its way out: give a retiring stream a moment before
            // refusing this visitor, so a burst that meets a full pool is
            // served by the streams retiring under it.
            let freed = self.inner.shared.capacity_freed.notified();
            tokio::pin!(freed);
            // Register before the second look, or a retirement landing in
            // between would be lost and this open would sleep out the budget.
            freed.as_mut().enable();
            if self.reserve(&[], &avoid, ceiling).is_none() {
                let _ = tokio::time::timeout(crate::transport::pool::CAPACITY_WAIT, freed).await;
            }
            let Some(reservation) = self.reserve(&[], &avoid, ceiling) else {
                // The final refusal: every serving tunnel is at the ceiling,
                // and no stream retired inside the budget. During a handover,
                // this is short-lived placement pressure, not pool saturation.
                if self.inner.state.lock().handover.is_none() {
                    self.report_saturation();
                }
                return Err(OpenError::AtCapacity);
            };
            return self.complete(reservation, &avoid).await;
        };
        self.complete(reservation, &avoid).await
    }

    /// The concurrent streams one tunnel of this pool may carry.
    ///
    /// Always strictly below the engine's cap (see
    /// [`crate::transport::pool::TUNNEL_STREAM_CEILING`]): the pool's whole
    /// reason to exist is that a cap hit is fatal to a tunnel, so placement
    /// must make it unreachable.
    fn ceiling(&self) -> usize {
        crate::transport::pool::tunnel_ceiling(self.inner.shared.stream_cap)
    }

    /// Say once, per process, that placement is what refused a visitor: a
    /// pinned pool cannot dial its way out, so the fix is a wider
    /// `[client.data.<carrier>].tunnels`.
    ///
    /// INFO on the first report and DEBUG afterwards — a saturated pool under
    /// load would otherwise repeat the line for every refused visitor — and the
    /// line names the configured count, the ceiling and the key to raise.
    /// Returns which of the two this call was, so a test can pin the once-only
    /// semantics.
    fn report_saturation(&self) -> bool {
        let count = self.inner.state.lock().count;
        let ceiling = self.ceiling();
        let carrier = self.inner.shared.carrier.as_str();
        POOL_SATURATED.report(
            || {
                info!(
                    pool = %self.inner.shared.key,
                    carrier,
                    count,
                    ceiling,
                    "pool-stats: pool at its stream ceiling; raise [client.data.{carrier}].tunnels"
                );
                true
            },
            || {
                debug!(
                    pool = %self.inner.shared.key,
                    carrier,
                    count,
                    ceiling,
                    "pool-stats: pool still at its stream ceiling"
                );
                false
            },
        )
    }

    /// The pool indices of the tunnels named by `ids`, as of now.
    ///
    /// Ids are process-unique and survive a removal while indices shift with
    /// one, so the mapping is taken fresh for every placement rather than
    /// cached. An id whose tunnel is gone simply drops out.
    fn indices_of(&self, ids: &[usize]) -> Vec<usize> {
        if ids.is_empty() {
            return Vec::new();
        }
        let state = self.inner.state.lock();
        state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| ids.contains(&entry.tunnel.id()))
            .map(|(index, _)| index)
            .collect()
    }

    /// Wait for the growth in flight to end, or for the pool to reach `target`.
    ///
    /// Raced deliberately: the caller re-reads the size after every wake-up,
    /// and a growth that finished between the check and the registration is
    /// caught by the second check (`Notify::notify_waiters` does not remember
    /// a signal).
    async fn await_growth(&self, target: usize) {
        use std::sync::atomic::Ordering;
        let resizing = &self.inner.shared.resizing;
        // The budget is a backstop: a growth either finishes or fails, and a
        // `resizing` flag some panicked task left set must not park an open for
        // ever. On timeout the caller sees the pool's real state.
        let _ = tokio::time::timeout(crate::transport::pool::GROW_WAIT_BUDGET, async {
            loop {
                if self.size() >= target || !resizing.load(Ordering::Acquire) {
                    return;
                }
                let grown = self.inner.shared.grown.notified();
                tokio::pin!(grown);
                // Register *before* the last check: `notify_waiters` remembers
                // nothing, so a wake-up landing between the check and the first
                // poll would otherwise be lost and the caller would sleep until
                // some later resize happened to notify.
                grown.as_mut().enable();
                if self.size() >= target || !resizing.load(Ordering::Acquire) {
                    return;
                }
                grown.await;
            }
        })
        .await;
    }

    /// Reserve the least-loaded tunnel for one open, skipping the tunnels this
    /// request has already tried (the fall-through after a refusal) and — for a
    /// stripe — the ones its group already occupies.
    ///
    /// Synchronous, and the only place placement reads the tunnel list: the
    /// reservation is charged before the first `await` of an open, which is
    /// what makes back-to-back opens land on distinct tunnels — and what makes
    /// a stripe group's concurrent placements spread even when a sibling has
    /// not recorded its tunnel yet.
    fn reserve(&self, tried: &[usize], avoid: &[usize], ceiling: usize) -> Option<Reservation> {
        let mut state = self.inner.state.lock();
        let loads: Vec<TunnelLoad> = state.entries.iter().map(PoolEntry::load).collect();
        let cursor = state.next_cursor;
        state.next_cursor = cursor.wrapping_add(1);
        let mut order: Vec<usize> = crate::transport::pool::order_candidates(&loads, cursor)
            .into_iter()
            .filter(|index| {
                state
                    .entries
                    .get(*index)
                    .is_some_and(|entry| entry.phase == TunnelPhase::Serving)
            })
            .collect();
        // While a carrier probe runs, every new stream the visitor opens is
        // routed to the probe tunnel first: the comparison is meaningless
        // unless the probed carrier carries traffic, and a probe that sits
        // idle measures nothing while the incumbent's windows keep confirming
        // its own lead (measured: four probes gave back on a first window of
        // zero carried bytes against 3 MB/s on the incumbent — an idle
        // tunnel, not a verdict). A stream here is one open; the probe's
        // budget is seconds, and the drain hands its streams back if the
        // verdict is Revert.
        #[cfg(feature = "client")]
        if let Some((probed, _)) = self
            .inner
            .shared
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            && let Some(probe_index) = state
                .entries
                .iter()
                .position(|entry| entry.phase == TunnelPhase::Serving && entry.carrier == probed)
        {
            order.retain(|index| *index != probe_index);
            order.insert(0, probe_index);
        }
        // The group's own tunnels first: a stripe takes one its siblings do not
        // have yet, and only when every tunnel is already part of the group (a
        // pool the group cannot spread over) does the exclusion give way — the
        // group must still forward, exactly as it did before it had a name.
        let candidates = crate::transport::pool::order_candidates_for(&order, tried, avoid);
        // The least-loaded untried tunnel with pending budget left; when every
        // candidate is at its budget, the least-loaded untried one still takes
        // the open — refusing a visitor outright is worse than queueing it
        // behind the driver's own bounded request queue.
        //
        // The stream ceiling is a hard bound, unlike the pending budget, and it
        // has no fallback: a tunnel at the ceiling is *never* given another
        // stream, however empty the contender list looks. The engine's own cap
        // would take the whole tunnel down, so the pool's answer to a full pool
        // is `open_stream`'s bounded wait and then a typed refusal — not one
        // more stream, which is the mistake that cost a tunnel in the v0.10.0
        // sweep.
        let mut over_budget = None;
        let mut chosen = None;
        for index in candidates.iter().copied() {
            let Some(load) = state.entries.get(index).map(PoolEntry::load) else {
                continue;
            };
            if load.total() >= ceiling {
                continue;
            }
            if load.pending < crate::transport::pool::OPEN_BUDGET {
                chosen = Some(index);
                break;
            }
            over_budget.get_or_insert(index);
        }
        let fallback = chosen.is_none();
        let index = chosen.or(over_budget)?;
        let entry = state.entries.get(index)?;
        entry.tunnel.counters.reserve();
        let load = entry.load();
        // The worst eligible candidate: what the placement rule gave up by not
        // picking it. S1's spread is `worst - best`.
        let worst = order
            .last()
            .and_then(|i| state.entries.get(*i))
            .map_or_else(TunnelLoad::default, PoolEntry::load);
        Some(Reservation {
            index,
            pool: std::sync::Arc::downgrade(&self.inner.shared),
            placement: Placement {
                chosen: index,
                candidates: order.len(),
                best: load,
                worst,
                chosen_load: load,
                fallback,
            },
            tunnel: entry.tunnel.clone(),
        })
    }

    /// Complete one reserved open: the actual `open_stream` on the reserved
    /// tunnel, with the pool's fall-through when it refuses.
    ///
    /// `avoid` is the caller's stripe exclusion, carried into the fall-through
    /// so a refused stripe still prefers a tunnel its group does not have (see
    /// [`Self::open_stream_on_distinct`]).
    ///
    /// `TooManyStreams` means "this tunnel is full", never "the tunnel is
    /// dead": it is counted as a refusal and the open falls through to the
    /// next candidate. The placement ceiling keeps every tunnel strictly below
    /// the engine's cap precisely so this path stays rare — the vendored engine
    /// logs an unguarded `error!` on a cap hit, and `tests/log_budget_test.rs`
    /// fails a healthy run with one ERROR.
    async fn complete(
        &self,
        reservation: Reservation,
        avoid: &[usize],
    ) -> Result<StreamLease, OpenError> {
        let started = std::time::Instant::now();
        let Reservation {
            index,
            pool,
            mut placement,
            tunnel,
        } = reservation;
        let counters = std::sync::Arc::clone(&tunnel.counters);
        match tunnel.open_stream().await {
            Ok(stream) => {
                counters.charge();
                // `chosen_load` is what the tunnel looks like *after* its open;
                // `best` stays the candidate snapshot the placement compared
                // against `worst`, or the spread `worst - best` would be
                // measured against a value the open itself changed (it went
                // negative on a single-tunnel pool, which is what caught this).
                placement.chosen_load = counters.load();
                PlacementStats::record(placement, started.elapsed(), None);
                Ok(StreamLease {
                    stream,
                    tunnel_id: tunnel.id(),
                    counters,
                    pool,
                })
            }
            Err(e) => {
                counters.release();
                debug!(
                    pool = %self.inner.shared.key,
                    tunnel = index,
                    "pool-stats: tunnel refused an open: {e}"
                );
                let note = if matches!(e, crate::mux::ConnectionError::TooManyStreams) {
                    // A cap hit is a refusal of this open, never a death:
                    // record it and report it back to the caller.
                    PlacementStats::record(placement, started.elapsed(), Some("too_many_streams"));
                    return Err(OpenError::Refused(e));
                } else {
                    if matches!(e, crate::mux::ConnectionError::Closed) {
                        // A tunnel died, so a retried growth is meaningful
                        // again: cut the failed-growth hold short (D14). The
                        // tunnel itself is marked dead here rather than left
                        // for the maintenance tick: the driver task may be
                        // parked on a socket that will never wake, and until
                        // the reap runs, placement keeps choosing it.
                        tunnel.mark_dead();
                        self.release_growth_hold();
                    }
                    "fallback"
                };
                // Fall through to the next candidate: a dead driver returns
                // `Closed` immediately, so this is a bounded scan.
                let mut last = e;
                let mut tried = vec![index];
                let ceiling = self.ceiling();
                while let Some(mut next) = self.reserve(&tried, avoid, ceiling) {
                    tried.push(next.index);
                    next.placement.fallback = true;
                    match next.tunnel.open_stream().await {
                        Ok(stream) => {
                            let counters = std::sync::Arc::clone(&next.tunnel.counters);
                            counters.charge();
                            next.placement.chosen_load = counters.load();
                            PlacementStats::record(
                                next.placement,
                                started.elapsed(),
                                Some("fallback"),
                            );
                            return Ok(StreamLease {
                                stream,
                                tunnel_id: next.tunnel.id(),
                                counters,
                                pool: next.pool,
                            });
                        }
                        Err(e) => {
                            next.tunnel.counters.release();
                            if matches!(e, crate::mux::ConnectionError::Closed) {
                                // Another dead tunnel: same as above (D14).
                                next.tunnel.mark_dead();
                                self.release_growth_hold();
                            }
                            last = e;
                        }
                    }
                }
                PlacementStats::record(placement, started.elapsed(), Some(note));
                Err(OpenError::Refused(last))
            }
        }
    }

    /// Add one tunnel.
    async fn grow(&self, reason: GrowReason) {
        use std::sync::atomic::Ordering;
        if self.inner.shared.resizing.swap(true, Ordering::AcqRel) {
            return;
        }
        let result = self.grow_locked(reason).await;
        self.inner.shared.resizing.store(false, Ordering::Release);
        // Whoever waited on an empty pool re-reads the size here and either
        // places its open or fails on its own.
        self.inner.shared.grown.notify_waiters();
        match result {
            Ok(()) => {
                *self.lock_grow_failure() = None;
                GROW_REFUSED.clear();
            }
            Err(e) => {
                // D14: a refusal stops growth. Without the stamp below the
                // maintenance tick would retry every 50 ms — a dial storm
                // against the server's accept path, which is what the
                // `max_tunnels_per_client` valve exists to prevent.
                *self.lock_grow_failure() = Some(std::time::Instant::now());
                // One line the first time (an operator who set the valve wants
                // to know it is biting, and a server-side outage is visible on
                // the control channel), then DEBUG: this can repeat per attempt.
                let pool = self.inner.shared.key.clone();
                GROW_REFUSED.report(
                    || info!(pool = %pool, "pool-stats: growth refused, holding off: {e}"),
                    || debug!(pool = %pool, "pool-stats: could not grow the tunnel pool: {e}"),
                );
            }
        }
    }

    /// The failed-growth stamp, with a poisoned lock treated as "no stamp"
    /// (the pool's own state is what matters; a panic elsewhere must not turn
    /// a policy into a panic).
    fn lock_grow_failure(&self) -> std::sync::MutexGuard<'_, Option<std::time::Instant>> {
        self.inner
            .shared
            .grow_failed_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a failed growth still holds the pool back (D14).
    fn growth_held_off(&self) -> bool {
        self.lock_grow_failure()
            .is_some_and(|at| at.elapsed() < crate::transport::pool::GROW_FAILURE_COOLDOWN)
    }

    /// A tunnel died (or one was removed): a retry is meaningful again, so the
    /// failed-growth hold is cut short (D14).
    fn release_growth_hold(&self) {
        *self.lock_grow_failure() = None;
    }

    async fn grow_locked(&self, reason: GrowReason) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let Some(dial) = self.inner.dial.clone() else {
            return Err("the pool has no dialer".to_owned());
        };
        let (size, count) = {
            let state = self.inner.state.lock();
            (state.entries.len(), state.count)
        };
        if size >= count {
            return Err(format!("already at the configured tunnel count ({count})"));
        }
        // A deliberate replacement follows the requested target even when the
        // endpoint choice has not committed yet; ordinary repairs follow the
        // current preference, including a fallback that happened mid-dial.
        let wanted = self
            .inner
            .state
            .lock()
            .handover
            .as_ref()
            .filter(|handover| handover.phase == HandoverPhase::Dialing)
            .map_or_else(|| (self.inner.preferred)(), |handover| handover.target);
        let (tunnel, shutdown) = dial(wanted).await?;
        let mut state = self.inner.state.lock();
        if state.entries.len() >= state.count {
            return Err("the pool reached its configured tunnel count while dialing".to_owned());
        }
        let from = state.entries.len();
        state.entries.push(PoolEntry {
            carrier: wanted,
            phase: TunnelPhase::Serving,
            tunnel,
            _shutdown: shutdown,
        });
        if state.handover.as_ref().is_some_and(|handover| {
            handover.phase == HandoverPhase::Dialing
                && state
                    .entries
                    .iter()
                    .filter(|entry| entry.carrier == handover.target)
                    .count()
                    >= 1
        }) {
            state.handover = None;
        }
        let to = state.entries.len();
        drop(state);
        self.inner.shared.grows.fetch_add(1, Ordering::Relaxed);
        self.inner.shared.push_event(PoolEvent {
            grow: true,
            reason: reason.as_str(),
            from,
            to,
        });
        // A repair is a lifecycle event: one INFO line per size change. The
        // carrier named is the one the new tunnel actually rides (`wanted`) —
        // the pool's birth carrier would hide a handover's replacement (and
        // did: a switched run read `carrier="kcp"` for every repair while the
        // truth lived only in the pool-stats timeline).
        info!(
            pool = %self.inner.shared.key,
            carrier = wanted.as_str(),
            reason = reason.as_str(),
            from,
            to,
            "pool-stats: tunnel pool repaired"
        );
        Ok(())
    }

    /// Remove every tunnel whose driver has ended.
    ///
    /// A dead tunnel is removed whatever it carries: the alternative (a
    /// removal gated on the whole pool being quiet) keeps a corpse — and its
    /// slot against the configured count — in the pool for as long as one
    /// stream outlives its connection, while placement keeps handing the
    /// corpse new opens, every one of which fails with `Closed`.
    ///
    /// Reaping is therefore not policy: it has no idle or cooldown gate
    /// and the dead tunnel's load is irrelevant (those streams are already
    /// broken). What it owes the pool afterwards is a replacement — the repair
    /// tick dials one for every missing member, whether or not the corpse was
    /// carrying — and D14's dial hold is released, because a death is the event
    /// that makes a retry meaningful.
    fn reap_dead(&self) -> bool {
        let (from, to) = {
            let mut state = self.inner.state.lock();
            if state.entries.iter().all(|e| e.tunnel.is_alive()) {
                return false;
            }
            let from = state.entries.len();
            let mut index = 0;
            while index < state.entries.len() {
                if state.entries[index].tunnel.is_alive() {
                    index += 1;
                } else {
                    // Dropping the entry drops the driver's shutdown sender:
                    // whatever the task was still doing ends here.
                    state.entries.remove(index);
                }
            }
            (from, state.entries.len())
        };
        self.release_growth_hold();
        if to < from {
            self.inner
                .shared
                .shrinks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.inner.shared.push_event(PoolEvent {
            grow: false,
            reason: ShrinkReason::Dead.as_str(),
            from,
            to,
        });
        // A death is a lifecycle event: one INFO line per size change. It is
        // not a shrink — a pinned pool replaces what it lost, and the repair
        // that follows is logged as the establishment it is.
        info!(
            pool = %self.inner.shared.key,
            carrier = self.inner.shared.carrier.as_str(),
            reason = ShrinkReason::Dead.as_str(),
            from,
            to,
            "pool-stats: tunnel pool reaped dead tunnels"
        );
        true
    }

    /// The maintenance tick, in repair form.
    ///
    /// A pinned pool has exactly one thing to reconcile: how many tunnels it
    /// holds against how many it was configured to hold. A dead one is reaped
    /// and replaced in the same tick — that is *repair*, not growth, and it is
    /// the only establishment that happens after service start. Everything
    /// else that used to live here (grow on load, grow for a stripe group,
    /// grow to the UDP floor, shrink when idle) is gone: capacity is a function
    /// of configuration, so it cannot depend on what the pool was doing a
    /// minute ago.
    async fn maintain(&self) {
        #[cfg(feature = "client")]
        self.diagnostic_handover();
        #[cfg(feature = "client")]
        self.carrier_rule_step();
        self.reap_dead();
        self.handover_step();
        self.repair().await;
    }

    /// Dial until the pool holds its configured count again.
    ///
    /// One dial per call, guarded by the same resize flag a cold open used: the
    /// next tick continues where this one stopped, and a refusal (the server's
    /// valve, an unreachable endpoint) holds the pool back for
    /// [`crate::transport::pool::GROW_FAILURE_COOLDOWN`] rather than letting a
    /// 50 ms tick become a dial storm.
    async fn repair(&self) {
        let (size, target) = {
            let state = self.inner.state.lock();
            (state.entries.len(), state.count)
        };
        if size >= target || self.growth_held_off() {
            return;
        }
        self.grow(GrowReason::Repair).await;
    }

    /// Start the per-pool maintenance task; it exits with the pool.
    fn spawn_maintenance(&self) {
        let weak = std::sync::Arc::downgrade(&self.inner);
        // The pool's own `alive` handle, held weakly: this task is one of the
        // two references, so `strong_count == 1` means the session (and every
        // service) has let the pool go.
        let alive = std::sync::Arc::downgrade(&self.inner.shared.alive);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::transport::pool::MAINTAIN_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let Some(_alive) = alive.upgrade() else { break };
                let pool = TunnelPool { inner };
                pool.maintain().await;
            }
        });
    }
}

/// A refused growth: INFO once per process, DEBUG after (the maintenance tick
/// re-attempts it, so it can repeat — see `GROW_FAILURE_COOLDOWN`).
static GROW_REFUSED: crate::logging::RepeatNotice = crate::logging::RepeatNotice::new();

/// A pool whose every tunnel is at its stream ceiling and whose count is
/// already established: the one refusal a pinned pool cannot dial its way out
/// of, so it is worth one INFO line naming the key to raise. INFO once per
/// process, DEBUG afterwards — see [`TunnelPool::report_saturation`].
static POOL_SATURATED: crate::logging::RepeatNotice = crate::logging::RepeatNotice::new();

/// Every live pool of this process, weak: the integration suite reads the
/// pool's own state here instead of guessing it from the telemetry text, and
/// the registry itself never keeps a pool alive.
static LIVE_POOLS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<TunnelPoolInner>>>> =
    std::sync::OnceLock::new();

/// A snapshot of every live pool, for tests and diagnostics.
#[doc(hidden)]
#[must_use]
pub fn live_pools() -> Vec<PoolSnapshot> {
    let registry = LIVE_POOLS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|w| w.strong_count() > 0);
    guard
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|inner| TunnelPool { inner }.try_snapshot())
        .collect()
}

/// The S1 placement observation: `MOLEHILL_PLACEMENT_STATS=1`.
///
/// Aggregated, once-guarded and one DEBUG line per second per process. A
/// per-placement line would be one syscall per data channel and, at INFO,
/// would break the log-budget gate (zero WARN/ERROR, a bounded number of INFO
/// lines and at most three repeats of any message shape on a healthy run).
mod placement_stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// One placement, as it happened.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Record {
        pub(super) candidates: usize,
        pub(super) chosen_streams: usize,
        pub(super) chosen_pending: usize,
        pub(super) best_total: usize,
        pub(super) worst_total: usize,
        pub(super) fallback: bool,
        pub(super) micros: u64,
    }

    /// The process-wide accumulator.
    struct Accumulator {
        placements: AtomicU64,
        fallbacks: AtomicU64,
        candidates: AtomicU64,
        chosen_streams: AtomicU64,
        chosen_pending: AtomicU64,
        best_total: AtomicU64,
        worst_total: AtomicU64,
        micros: AtomicU64,
        max_micros: AtomicU64,
    }

    impl Accumulator {
        const fn new() -> Self {
            Self {
                placements: AtomicU64::new(0),
                fallbacks: AtomicU64::new(0),
                candidates: AtomicU64::new(0),
                chosen_streams: AtomicU64::new(0),
                chosen_pending: AtomicU64::new(0),
                best_total: AtomicU64::new(0),
                worst_total: AtomicU64::new(0),
                micros: AtomicU64::new(0),
                max_micros: AtomicU64::new(0),
            }
        }

        fn record(&self, r: Record) {
            self.placements.fetch_add(1, Ordering::Relaxed);
            if r.fallback {
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
            }
            self.candidates
                .fetch_add(r.candidates as u64, Ordering::Relaxed);
            self.chosen_streams
                .fetch_add(r.chosen_streams as u64, Ordering::Relaxed);
            self.chosen_pending
                .fetch_add(r.chosen_pending as u64, Ordering::Relaxed);
            self.best_total
                .fetch_add(r.best_total as u64, Ordering::Relaxed);
            self.worst_total
                .fetch_add(r.worst_total as u64, Ordering::Relaxed);
            self.micros.fetch_add(r.micros, Ordering::Relaxed);
            self.max_micros.fetch_max(r.micros, Ordering::Relaxed);
        }

        /// Take the interval's aggregates (all but the two cumulative
        /// counters, which the caller diffs).
        fn drain(&self) -> Record {
            // The interval's aggregates are small (at most one second of
            // placements), so narrowing is total in practice; `try_from` keeps
            // it honest on a 32-bit target instead of truncating silently.
            fn narrow(v: u64) -> usize {
                usize::try_from(v).unwrap_or(usize::MAX)
            }
            Record {
                candidates: narrow(self.candidates.swap(0, Ordering::Relaxed)),
                chosen_streams: narrow(self.chosen_streams.swap(0, Ordering::Relaxed)),
                chosen_pending: narrow(self.chosen_pending.swap(0, Ordering::Relaxed)),
                best_total: narrow(self.best_total.swap(0, Ordering::Relaxed)),
                worst_total: narrow(self.worst_total.swap(0, Ordering::Relaxed)),
                fallback: false,
                micros: self.micros.swap(0, Ordering::Relaxed),
            }
        }

        fn totals(&self) -> (u64, u64, u64) {
            (
                self.placements.load(Ordering::Relaxed),
                self.fallbacks.load(Ordering::Relaxed),
                self.max_micros.load(Ordering::Relaxed),
            )
        }
    }

    static ACC: Accumulator = Accumulator::new();

    /// `true` when `MOLEHILL_PLACEMENT_STATS=1` is set in the environment.
    pub(super) fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| crate::logging::env_switch("MOLEHILL_PLACEMENT_STATS"))
    }

    /// Record one placement, spawning the reporter on first use.
    pub(super) fn record(r: Record) {
        static SPAWNED: std::sync::Once = std::sync::Once::new();
        ACC.record(r);
        SPAWNED.call_once(|| {
            tokio::spawn(async {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut seen_placements = 0u64;
                let mut seen_fallbacks = 0u64;
                loop {
                    tick.tick().await;
                    let (placements, fallbacks, max_micros) = ACC.totals();
                    let interval = placements.saturating_sub(seen_placements);
                    if interval == 0 {
                        continue;
                    }
                    let interval_fallbacks = fallbacks.saturating_sub(seen_fallbacks);
                    seen_placements = placements;
                    seen_fallbacks = fallbacks;
                    let r = ACC.drain();
                    // The spread is the S1 question: what a placement rule can
                    // win. `mean_spread` is the average gap, in stream slots,
                    // between the best and the worst candidate at the instant a
                    // placement was made — near zero means the choice is worth
                    // nothing and no smarter rule should be written.
                    let mean_spread =
                        (r.worst_total.saturating_sub(r.best_total)) as u64 / interval.max(1);
                    // INFO, like every other opt-in telemetry switch: enabling
                    // the switch is the consent, and a line an operator has to
                    // raise `RUST_LOG` to see never reaches a results file.
                    tracing::info!(
                        placements = interval,
                        fallbacks = interval_fallbacks,
                        candidates = r.candidates,
                        chosen_streams = r.chosen_streams,
                        chosen_pending = r.chosen_pending,
                        best_total = r.best_total,
                        worst_total = r.worst_total,
                        mean_spread,
                        mean_us = r.micros / interval,
                        max_us = max_micros,
                        "placement-stats: aggregate of the last interval"
                    );
                }
            });
        });
    }
}

/// The placement recorder: a zero-sized handle (the state is process-wide and
/// the whole path short-circuits when the switch is off).
#[derive(Clone, Copy, Default)]
struct PlacementStats;

impl PlacementStats {
    fn record(placement: Placement, elapsed: std::time::Duration, note: Option<&'static str>) {
        if !placement_stats::enabled() {
            return;
        }
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        placement_stats::record(placement_stats::Record {
            candidates: placement.candidates,
            chosen_streams: placement.chosen_load.streams,
            chosen_pending: placement.chosen_load.pending,
            best_total: placement.best.total(),
            worst_total: placement.worst.total(),
            fallback: note.is_some() || placement.fallback,
            micros,
        });
    }
}

/// The S1 pool observation: `MOLEHILL_POOL_STATS=1`.
///
/// One INFO line per live pool per second, plus the timeline of size changes.
/// Once-guarded and session-owned: the reporter holds `Weak` references, so it
/// reports every pool this process creates and exits when they are gone — the
/// mistake `spawn_framing_stats` makes (one line per tunnel per process) is not
/// repeated here.
mod pool_stats {
    use super::{TunnelPool, TunnelPoolInner};
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    static REGISTRY: OnceLock<Mutex<Vec<Weak<TunnelPoolInner>>>> = OnceLock::new();

    /// `true` when `MOLEHILL_POOL_STATS=1` is set in the environment.
    pub(super) fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| crate::logging::env_switch("MOLEHILL_POOL_STATS"))
    }

    fn registry() -> &'static Mutex<Vec<Weak<TunnelPoolInner>>> {
        REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Register one pool, spawning the reporter on the first registration.
    pub(super) fn register(inner: &Arc<TunnelPoolInner>) {
        static SPAWNED: std::sync::Once = std::sync::Once::new();
        if !enabled() {
            return;
        }
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::downgrade(inner));
        SPAWNED.call_once(|| {
            tokio::spawn(async {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let pools: Vec<Arc<TunnelPoolInner>> = {
                        let mut guard = registry()
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.retain(|w| w.strong_count() > 0);
                        guard.iter().filter_map(Weak::upgrade).collect()
                    };
                    if pools.is_empty() {
                        // Every pool of this process is gone.
                        break;
                    }
                    for inner in pools {
                        let pool = TunnelPool { inner };
                        pool.report();
                    }
                }
            });
        });
    }
}

/// The least the **peer** must have handed a tunnel in one window for the window
/// to be judged at all.
///
/// A quiet tunnel's ratio is noise (one byte delivered of two is 50 %), so the
/// verdict needs a volume behind it: 64 KiB per window is a couple of
/// milliseconds of even a policed path's traffic and orders of magnitude above a
/// keepalive-only session.
const CARRIER_RULE_MIN_HANDED_BYTES: u64 = 64 * 1024;

/// The delivery ratio below which a tunnel is judged to be losing its traffic,
/// in percent: what this end received against what the peer handed over.
///
/// **It fires on neither class, and that is measured rather than assumed.** On
/// the policed path this end receives *everything* the peer hands over — 100.0 %
/// with the tunnel's `handed` sitting at the policer's own ceiling — because KCP
/// is reliable: a dropped datagram is re-sent until it arrives, so a policed
/// carrier surfaces as **throughput**, not as a delivery gap. A threshold on any
/// local quantity therefore cannot separate the classes (the re-send share fires
/// on both, this ratio on neither), and the rule's next form is the comparison
/// the L3 lane policy already makes: put a tunnel on the other carrier and see
/// which one carries more (HANDOFF.md, round 7).
///
/// The historical calibration, kept because the constant still bounds the
/// verdict: measured on the two
/// classes (HANDOFF.md, round 6-7): a UDP-policed path — where the handover to
/// TCP is worth 10-13x — delivers ~10-20 % of what the server hands it, while the
/// healthy lossy class, where KCP is the carrier to keep, delivers ~95-100 %.
/// The two do not overlap, which is why a floor here can be both sensitive and
/// safe; 60 % sits between them with a 3x margin below and a 1.6x margin above.
/// The residues that keep a healthy tunnel below 100 % (the multiplexer's own
/// framing, a stream retiring mid-window) are a few percent, not tens.
const CARRIER_RULE_MIN_DELIVERY_PCT: f64 = 60.0;

/// How many maintenance ticks make one judgement window (50 ms each: a second).
///
/// Lengthening the window is what lets a *slow* carrier be judged at all; the
/// share is a ratio, so a longer window costs nothing but the time to fill it,
/// and the confirm count below is then counted in windows rather than ticks.
const CARRIER_RULE_WINDOW_TICKS: u32 = 20;

/// How many consecutive **windows** the verdict must hold before the move.
/// With the window at [`CARRIER_RULE_WINDOW_TICKS`], three of them are three
/// seconds of sustained degradation — a loss event that lasts longer than an RTO
/// cycle is a path property, not a burst, which is the same judgment the lane
/// policy's three one-second ticks make at its scale.
const CARRIER_RULE_CONFIRM_TICKS: u32 = 3;

/// How much more one carrier's tunnels must carry before the pool moves a
/// tunnel to it (or gives a probe back).
///
/// A ratio, not a margin, and the same 1.5 the lane policy uses: the measured
/// separation on the cells where the carriers diverge is 1.9x and up (and
/// 10-13x on the policed one), while the cells where they are level sit near
/// 1.0.
const CARRIER_RULE_PROBE_EDGE: f64 = 1.5;

/// How long a probe runs before it is given back if the comparison stays
/// inconclusive.
///
/// A probe splits the pool's tunnels between the carriers, so an inconclusive
/// one is a cost paid for nothing: 30 s is far longer than the seconds a
/// divergent cell needs to separate and short enough not to hold a split for
/// a whole run.
const CARRIER_RULE_PROBE_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a pool waits before probing again after one came back empty.
///
/// The suspicion that starts a probe fires on a merely lossy path too — that
/// is measured (round 6) and unavoidable, because a lossy path re-sends
/// heavily whether or not it is the right carrier. What keeps that from
/// becoming churn is this backoff: a probe that came back does not go out
/// again for ten minutes, so a healthy path pays one split tunnel for half a
/// minute and then nothing for the rest of it.
const CARRIER_RULE_PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(600);

/// The re-sent share above which a window *suspects* the carrier and starts a
/// probe — the share floor the S1 cells calibrated (5.7 % at a saturated
/// ceiling) and the wiring run re-measured at the other extreme (0.6 % where
/// traffic merely flows), kept here not as a verdict but as the trigger of
/// the comparison that decides. Measured on the policed condition the share
/// is the only local signal that fires at all (27 % there, 3.5 % median
/// healthy — delivery stays at 99 % because the ARQ re-sends until it
/// arrives), and it also fires on a merely lossy path, which is exactly why
/// it starts a probe rather than a move: the share says the sender is
/// fighting the path, the probe says which carrier is winning.
const CARRIER_RULE_SUSPICION_SHARE_PCT: f64 = 4.0;

/// The least the probe tunnel must carry in one window before its window is
/// a measurement: below it the comparison holds rather than reverts, because
/// an idle tunnel against a busy incumbent reads "carries less" on zero
/// evidence. Half the window's share floor, rounded to the byte — enough to
/// prove traffic was routed, far below anything worth calling a rate.
const CARRIER_RULE_PROBE_MIN_BYTES_PER_S: f64 = 8_192.0;

/// The rule's switch: `MOLEHILL_TUNNEL_CARRIER_RULE=1` turns it on for the
/// process. **Off by default**, deliberately, and the first wiring run is
/// the measurement that decided it: the only signal the rule can read — the
/// sender's own retransmit share — is *rate-dependent*. On the S1 cells that
/// saturated into the lossy path the share was 5.7 %; on the paced cell
/// where traffic actually flowed at that same 1 % loss, the probed reading
/// was **0.6 %** — under the floor, so the rule sat silent while the
/// carriers still diverged in delivered loss. The reason is the same
/// asymmetry the L3 evidence chain was built around: the loss that matters
/// happens at the *peer's* queue, and a sender's ARQ counters only see the
/// part the path itself dropped. A floor calibrated on the saturated cells
/// cannot fire on the others, and lowering it without a measurement behind
/// the new number would be guessing. The rule, its floors and its plumbing
/// stay; turning it on is an operator's (or a measurement's) decision, and
/// the peer-side report that would make the sender's signal honest — the L4
/// analogue of the lane `CarrierReport` — is the recorded next instrument.
fn carrier_rule_enabled() -> bool {
    // Read at pool creation, uncached: the hatch's round-36 lesson applies
    // verbatim (a process-wide cache would let whichever pool was created
    // first pin the answer onto every pool after it).
    std::env::var("MOLEHILL_TUNNEL_CARRIER_RULE").is_ok_and(|v| v.trim() == "1")
}

/// One window's verdict: the tunnel id to hand over and the retransmit share
/// that justified it, when any tunnel of the pool is measurably losing.
///
/// The window is `previous` minus `now`, per tunnel id (tunnels that joined
/// between the samples have no window and are not evidence — the safe
/// direction, as the lane policy puts it). The worst *retransmitting* tunnel
/// wins: the move retires the one whose loss the replacement has to beat.
#[must_use]
/// One window's re-sent share, in percent, or `None` when nothing was emitted
/// (a quiet tunnel's share is not zero, it is undefined).
fn window_share_pct(emitted: u64, resent: u64) -> Option<f64> {
    if emitted == 0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "the window's datagram counts are far below 2^53, the point where an f64 \
                  stops counting integers exactly"
    )]
    let share = 100.0 * resent as f64 / emitted as f64;
    Some(share)
}

/// The verdict: a tunnel that delivered **a fraction of what the peer handed
/// it** is a carrier losing traffic.
///
/// This replaces the re-sent share, which was measured not to discriminate: a
/// lossy-but-viable path re-sends heavily and is still the carrier to keep
/// (healthy median 3.5 % share, policed 28.7 %, overlapping around any floor),
/// while the delivery ratio separates them by a wide margin because it measures
/// the *path*, not the protocol's own recovery behaviour. A tunnel needs enough
/// handed over in the window to be judged at all; the worst ratio wins, ties to
/// the lower tunnel id so the confirm streak keeps meaning one tunnel.
/// What one comparison window decided about the probe tunnel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The probed carrier carries materially more: move the pool to it.
    Move,
    /// The probed carrier carries materially less: give the probe back.
    Revert,
    /// No separation: keep the probe and look again next window.
    Hold,
}

/// Compare what the two carriers carried, in bytes per second per tunnel.
///
/// This is the L4 half of the shape the L3 lane policy already uses: no local
/// threshold can separate a policed carrier from a merely lossy one — measured,
/// the re-send share fires on both classes and the delivery ratio on neither,
/// because a reliable ARQ turns a policed path into *throughput* (HANDOFF.md,
/// rounds 6-7). The only way to learn what the other carrier would do is to put
/// a tunnel on it and compare what the two carry.
///
/// Both inputs are the window's carried bytes **per second** (the window's
/// bytes over the window's seconds), so the comparison survives windows of
/// different lengths. `probe_bytes_per_s` is judged against the pool's own
/// carrier's *average* tunnel (`chosen_bytes_per_s`): the probe is one tunnel,
/// the incumbent is several, and a raw byte total would let the incumbent win
/// by counting, not by carrying.
#[must_use]
pub fn probe_verdict(chosen_bytes_per_s: f64, probe_bytes_per_s: f64) -> ProbeVerdict {
    if chosen_bytes_per_s <= 0.0 && probe_bytes_per_s <= 0.0 {
        // Nothing moved through either side: there is nothing to compare, and
        // calling it here is how a probe would flunk an idle path.
        return ProbeVerdict::Hold;
    }
    if probe_bytes_per_s > chosen_bytes_per_s * CARRIER_RULE_PROBE_EDGE {
        ProbeVerdict::Move
    } else if chosen_bytes_per_s > probe_bytes_per_s * CARRIER_RULE_PROBE_EDGE {
        ProbeVerdict::Revert
    } else {
        ProbeVerdict::Hold
    }
}

/// The bytes the two sides carried in one window: the pool carrier's tunnels
/// (every tunnel but the probe's, averaged — see [`probe_verdict`]) against
/// the probe tunnel's own.
///
/// A tunnel that joined between the samples has no window and contributes
/// nothing — the same safe direction the verdict takes: absence is not
/// evidence. The probe tunnel is excluded from the average even if it is the
/// only tunnel on the probed carrier (its twin on the pool's carrier was
/// removed by the handover).
#[cfg(feature = "client")]
fn probe_carried_bytes(last: &HealthSample, now: &HealthSample, probe_tunnel: usize) -> (u64, u64) {
    let window = |sample: &HealthSample| -> Vec<(usize, u64)> {
        sample
            .iter()
            .map(|(id, (_, _, delivered, _))| {
                let before = last
                    .iter()
                    .find(|(p, _)| p == id)
                    .map_or(0u64, |(_, quad)| quad.2);
                (*id, delivered.saturating_sub(before))
            })
            .collect()
    };
    let deltas = window(now);
    let mut chosen: Vec<u64> = Vec::new();
    let mut probe = 0u64;
    for (id, delivered) in deltas {
        if id == probe_tunnel {
            probe = delivered;
        } else {
            chosen.push(delivered);
        }
    }
    let chosen_avg = if chosen.is_empty() {
        0
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's byte counts are far below 2^53, the point where an f64 stops \
                      counting integers exactly"
        )]
        let sum = chosen.iter().sum::<u64>() as f64;
        #[expect(
            clippy::cast_precision_loss,
            reason = "tunnel counts are single digits"
        )]
        let n = chosen.len() as f64;
        // An average of byte counts rounds down to the byte; the verdict
        // compares ratios, where one byte cannot matter.
        let avg = sum / n;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "see above: the fractional byte is noise next to a 1.5x edge"
        )]
        let rounded = avg as u64;
        rounded
    };
    (chosen_avg, probe)
}

/// The rule's suspicion: the worst re-sent share on any tunnel that carried
/// material traffic in the window, or `None` when no tunnel crossed
/// [`CARRIER_RULE_SUSPICION_SHARE_PCT`]. `Some` starts a probe — never a
/// move — because the share cannot tell a policed carrier from a merely
/// lossy one (measured, rounds 6-7): the comparison can.
#[cfg(feature = "client")]
fn carrier_rule_suspicion(previous: &HealthSample, now: &HealthSample) -> Option<f64> {
    let mut worst: Option<f64> = None;
    for (id, (emitted_now, resent_now, _, handed_now)) in now {
        let Some((_, (out_prev, ret_prev, _, handed_prev))) =
            previous.iter().find(|(p, _)| p == id)
        else {
            continue;
        };
        let handed = handed_now.saturating_sub(*handed_prev);
        if handed < CARRIER_RULE_MIN_HANDED_BYTES {
            continue;
        }
        let emitted = emitted_now.saturating_sub(*out_prev);
        let resent = resent_now.saturating_sub(*ret_prev);
        let Some(share) = window_share_pct(emitted, resent) else {
            continue;
        };
        if share >= CARRIER_RULE_SUSPICION_SHARE_PCT && worst.is_none_or(|best| share > best) {
            worst = Some(share);
        }
    }
    worst
}

#[cfg(feature = "client")]
fn carrier_rule_verdict(previous: &HealthSample, now: &HealthSample) -> Option<(usize, f64)> {
    let mut worst: Option<(usize, f64)> = None;
    for (id, (_, _, delivered_now, handed_now)) in now {
        let Some((_, (_, _, delivered_prev, handed_prev))) = previous.iter().find(|(p, _)| p == id)
        else {
            continue;
        };
        let delivered = delivered_now.saturating_sub(*delivered_prev);
        let handed = handed_now.saturating_sub(*handed_prev);
        if handed < CARRIER_RULE_MIN_HANDED_BYTES {
            continue;
        }
        let delivered_ratio_pct = delivery_pct(delivered, handed);
        if delivered_ratio_pct >= CARRIER_RULE_MIN_DELIVERY_PCT {
            continue;
        }
        // Lowest ratio first; ties to the lower tunnel id (see above).
        if worst.is_none_or(|(_, best)| delivered_ratio_pct < best) {
            worst = Some((*id, delivered_ratio_pct));
        }
    }
    worst
}

/// What fraction of what the peer handed a tunnel this end received, in percent.
fn delivery_pct(delivered: u64, handed: u64) -> f64 {
    if handed == 0 {
        return 100.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a window's byte counts are far below 2^53, the point where an f64 stops \
                  counting integers exactly"
    )]
    let pct = 100.0 * delivered.min(handed) as f64 / handed as f64;
    pct
}

/// The hatch's offset, read **per pool** at the moment the pool is created:
/// the documented contract is "at that many seconds after a multiplexed pool
/// starts", so every pool must see the value that is set when *it* begins —
/// a process-wide cache would pin the first pool's answer onto every pool a
/// long-lived process builds after it. A value that is not a whole number of
/// seconds is reported once per process (the condition can repeat at every
/// pool creation) and ignored.
fn diagnostic_handover_at() -> Option<std::time::Duration> {
    static INVALID: crate::logging::RepeatNotice = crate::logging::RepeatNotice::new();
    let raw = std::env::var("MOLEHILL_TUNNEL_SWITCH_AT").ok()?;
    let Ok(secs) = raw.trim().parse::<u64>() else {
        INVALID.report(
            || {
                tracing::warn!(
                    "MOLEHILL_TUNNEL_SWITCH_AT={raw:?} is not a whole number of seconds; ignoring it"
                );
            },
            || {
                tracing::debug!(
                    "MOLEHILL_TUNNEL_SWITCH_AT={raw:?} is not a whole number of seconds; ignoring it"
                );
            },
        );
        return None;
    };
    Some(std::time::Duration::from_secs(secs))
}

impl TunnelPool {
    /// Emit the pool's line, with the timeline of the changes since the last
    /// one. Called once per second by the `MOLEHILL_POOL_STATS` reporter.
    fn report(&self) {
        let Some(snapshot) = self.try_snapshot() else {
            // The pool lock is held by a placement: report it next second
            // rather than block a data channel.
            return;
        };
        let events: Vec<PoolEvent> = {
            let mut guard = self
                .inner
                .shared
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        let tunnels: Vec<String> = snapshot
            .tunnels
            .iter()
            .zip(&snapshot.draining)
            .map(|((streams, pending, pinned, carrier), draining)| {
                format!(
                    "{streams}/{pending}/{pinned}/{}/{}",
                    carrier.as_str(),
                    if *draining { "draining" } else { "serving" }
                )
            })
            .collect();
        let handover = snapshot.handover.map_or_else(
            || "-".to_owned(),
            |(tunnel, target, phase)| format!("{tunnel}/{}:{phase}", target.as_str()),
        );
        // The KCP half of the carrier rule's evidence, on the same line:
        // per-tunnel cumulative `(datagrams_out, retransmits)` — `-` where the
        // tunnel does not ride KCP. A reader that differences two lines per
        // tunnel sees exactly the window the rule judges.
        let health: Vec<String> = snapshot
            .kcp_health
            .iter()
            .map(|h| h.map_or_else(|| "-".to_owned(), |(out, ret)| format!("{out}/{ret}")))
            .collect();
        let timeline: Vec<String> = events
            .iter()
            .map(|e| {
                format!(
                    "{}{}:{}->{}",
                    if e.grow { "+" } else { "-" },
                    e.reason,
                    e.from,
                    e.to
                )
            })
            .collect();
        info!(
            pool = %snapshot.key,
            carrier = snapshot.carrier,
            size = snapshot.size,
            count = snapshot.count,
            udp_floor = snapshot.udp_floor,
            streams = snapshot.streams(),
            pinned = snapshot.pinned(),
            tunnels = %tunnels.join(","),
            timeline = %if timeline.is_empty() { "-".to_owned() } else { timeline.join(" ") },
            grows = snapshot.grows,
            shrinks = snapshot.shrinks,
            handover = %handover,
            kcp_health = %health.join(","),
            "pool-stats: tunnel pool timeline"
        );
    }
}

/// Clears a tunnel's `alive` flag when its driver task ends, however it ends.
///
/// A guard rather than a trailing store: the driver loop has several exits
/// (shutdown, EOF, error) and a panic must clear the flag too, or the pool
/// would keep a tunnel whose task has already unwound.
struct AliveGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Handle to a client-side tunnel: opens data channels as streams of one
/// yamux session, and carries the tunnel's own bookkeeping.
#[derive(Clone)]
pub struct ClientTunnel {
    /// The tunnel's process-unique id: the key of the client's pin accounting
    /// (UDP affinity, D30) and the label the telemetry uses.
    id: usize,
    /// The tunnel's bookkeeping, reached through this handle by the pool.
    pub(crate) counters: std::sync::Arc<TunnelCounters>,
    /// True while the driver task is still running. The pool polls it: a
    /// tunnel whose connection died must leave the pool even if it still
    /// "holds" streams, or every later placement lands on a corpse.
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    open_tx: mpsc::Sender<oneshot::Sender<Result<MuxStream, crate::mux::ConnectionError>>>,
}

impl ClientTunnel {
    /// Spawn the driver task for a client-mode session.
    ///
    /// The returned handle stays valid until `shutdown` is dropped; the
    /// driver also exits when the underlying connection dies.
    pub fn start<I>(
        io: I,
        config: Config,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> ClientTunnel
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Self::start_with_health(io, None, config, shutdown)
    }

    /// The same, carrying the KCP session's ARQ health when the tunnel's
    /// connection is a KCP session (see [`crate::transport::health::SessionHealth`]): the
    /// counters live on the tunnel's own bookkeeping, so the pool reads this
    /// tunnel's retransmit share apart from every other tunnel's. A TCP
    /// tunnel passes `None` — the kernel keeps its retransmit state where
    /// userspace cannot read it.
    pub fn start_with_health<I>(
        io: I,
        health: Option<std::sync::Arc<crate::transport::health::SessionHealth>>,
        config: Config,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> ClientTunnel
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (open_tx, mut open_rx) =
            mpsc::channel::<oneshot::Sender<Result<MuxStream, crate::mux::ConnectionError>>>(16);

        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = AliveGuard(std::sync::Arc::clone(&alive));

        spawn_framing_stats();
        tokio::spawn(async move {
            let _guard = guard;
            let mut conn = Connection::new(io, config, Mode::Client);
            let mut waiting: Option<
                oneshot::Sender<Result<MuxStream, crate::mux::ConnectionError>>,
            > = None;
            // A freshly opened stream whose SYN announcement is in flight.
            // The announcement is driven inside the poll closure (via the
            // shared mutex, so the closure never captures it mutably),
            // keeping the driver event-driven: a backpressured socket must
            // not stall inbound frame processing for the whole tunnel. The
            // lock is only ever held by this one driver task, so it never
            // contends.
            let announce: std::sync::Mutex<Option<SynAnnounce>> = std::sync::Mutex::new(None);

            loop {
                enum Step {
                    /// The pending announcement settled; its reply was
                    /// already delivered to the waiting caller.
                    Announced,
                    Opened(Result<crate::mux::Stream, crate::mux::ConnectionError>),
                    Inbound(Option<Result<crate::mux::Stream, crate::mux::ConnectionError>>),
                }

                let step = poll_fn(|cx| {
                    // 1. Drive the pending SYN announcement first; while it
                    //    stays pending the inbound poll below keeps
                    //    registering wakers, so data keeps flowing under
                    //    socket backpressure.
                    {
                        let mut slot = announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(f) = slot.as_mut()
                            && let Poll::Ready(()) = std::pin::Pin::new(&mut *f).poll(cx)
                        {
                            *slot = None;
                            return Poll::Ready(Step::Announced);
                        }
                    }
                    // 2. Serve the next outbound open — one at a time, like
                    //    before: only when a request is pending and no
                    //    announcement is in flight.
                    if waiting.is_some()
                        && announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_none()
                        && let Poll::Ready(r) = conn.poll_new_outbound(cx)
                    {
                        return Poll::Ready(Step::Opened(r));
                    }
                    // 3. Inbound flows in every state: the server never
                    //    opens streams toward us, so drop any that appear;
                    //    errors and end of stream close the tunnel.
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(v) => Poll::Ready(Step::Inbound(v)),
                        Poll::Pending => Poll::Pending,
                    }
                });

                tokio::select! {
                    _ = shutdown.changed() => break,
                    step = step => match step {
                        Step::Announced => {}
                        Step::Opened(result) => {
                            if let Some(reply) = waiting.take() {
                                match result {
                                    Ok(stream) => {
                                        *announce
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            Some(SynAnnounce {
                                                stream: Some(stream),
                                                reply: Some(reply),
                                            });
                                    }
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                }
                            }
                        }
                        Step::Inbound(Some(Ok(_stream))) => {}
                        Step::Inbound(_) => break,
                    },
                    req = open_rx.recv(),
                    if waiting.is_none()
                        && announce
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_none() =>
                    {
                        match req {
                            Some(reply) => waiting = Some(reply),
                            None => break, // all handles dropped
                        }
                    }
                }
            }
        });

        ClientTunnel {
            id: next_tunnel_id(),
            counters: TunnelCounters::with_health(health),
            alive,
            open_tx,
        }
    }

    /// The tunnel's process-unique id (the client's pin accounting key).
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    /// Whether the driver task is still running.
    ///
    /// A tunnel whose connection died answers `Closed` to every open and is
    /// removed by `TunnelPool::reap_dead`; this is what "died" means, as
    /// opposed to "has no streams right now".
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Mark the tunnel dead from the pool's side. An open that already saw
    /// `Closed` is proof the connection is gone, and waiting for the driver
    /// task to notice would leave the corpse placeable for a whole tick.
    pub(crate) fn mark_dead(&self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Open a new data channel as a multiplexed stream.
    pub async fn open_stream(&self) -> Result<MuxStream, crate::mux::ConnectionError> {
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .await
            .map_err(|_| crate::mux::ConnectionError::Closed)?;
        rx.await.map_err(|_| crate::mux::ConnectionError::Closed)?
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The carrier rule's verdict: a tunnel that delivered a fraction of what
    /// the peer handed it is named; a merely lossy-but-delivering window is not
    /// (that was the old signal's misfire), nor is a window with too little
    /// handed over, nor a tunnel that joined between the samples.
    #[test]
    fn the_carrier_rule_judges_a_tunnel_that_stops_delivering() {
        const MIB: u64 = 1 << 20;
        // A baseline window: some traffic, some re-sends, and the peer handing
        // the tunnel 8 MiB more.
        let before = vec![(7usize, (100_000u64, 1u64, 5 * MIB, 5 * MIB))];
        // Healthy-but-lossy: 11.4 % re-sent share (the old signal would have
        // fired) and 94 % delivered (the carrier is doing its job).
        let delivering = vec![(7, (200_000, 11_401, 5 * MIB + 7_900_000, 13 * MIB))];
        assert_eq!(
            carrier_rule_verdict(&before, &delivering),
            None,
            "re-sending is not the signal"
        );

        // Policed: the same handed over, 1.2 MiB delivered — 14 %.
        let policed = vec![(7, (200_000, 11_401, 5 * MIB + 1_200_000, 13 * MIB))];
        let (id, pct) = carrier_rule_verdict(&before, &policed)
            .expect("a tunnel that stops delivering is the signal");
        assert_eq!(id, 7);
        assert!((pct - 14.3).abs() < 0.5, "{pct}");

        // Too little handed over in the window to judge anything.
        let tiny = vec![(7, (100, 0, 5 * MIB + 4, 5 * MIB + 4_096))];
        assert_eq!(carrier_rule_verdict(&before, &tiny), None);

        // A tunnel that joined between the samples has no window.
        let joined = vec![(9, (100, 0, 1, MIB))];
        assert_eq!(carrier_rule_verdict(&before, &joined), None);
    }

    /// The probe lifecycle on a real pool: the delivery signal confirms, a
    /// tunnel is handed to the other carrier, and the *carried bytes* decide —
    /// more, and the endpoint's choice moves (`prefer` is the authority, the
    /// switch cooldown included); less, and the tunnel comes back with a
    /// backoff. The verdicts are driven by the same per-tunnel byte counters
    /// the pool snapshot exposes, exactly as production feeds them.
    #[tokio::test]
    #[expect(
        unsafe_code,
        reason = "edition 2024 makes env mutation unsafe; the switch is read per pool at                   creation and this suite is serial, so the value is set before the pool exists"
    )]
    async fn the_probe_moves_the_pool_when_the_probed_carrier_carries_more() {
        // SAFETY: single-threaded test process; read at this pool's creation
        // (below), before any other pool in the suite is built.
        unsafe {
            std::env::set_var("MOLEHILL_TUNNEL_CARRIER_RULE", "1");
        }
        // A Kcp-carrier pool with a live choice: the probe hands a tunnel to
        // tcp, and a Move verdict goes through the choice's `prefer`.
        let (pool, _rxs, choice) = duplex_pool_with_choice(Carrier::Kcp, 2, 2);
        let counters: Vec<std::sync::Arc<TunnelCounters>> = {
            let state = pool.inner.state.lock();
            state
                .entries
                .iter()
                .map(|e| std::sync::Arc::clone(&e.tunnel.counters))
                .collect()
        };

        // Baseline window: the peer hands both tunnels 8 MiB and this end
        // delivers nearly all of it — then the losing windows the rule reads.
        let mut seq = 0u8;
        let mut handed_total: u64 = 0;
        let mut window = |delivered: u64, handed: u64| {
            seq = seq.wrapping_add(1);
            handed_total = handed_total.saturating_add(handed);
            for counters in &counters {
                counters
                    .delivered
                    .fetch_add(delivered, std::sync::atomic::Ordering::Relaxed);
                counters.note_peer_health((100_000, 1_000), handed_total, seq);
            }
        };
        window(7 << 20, 8 << 20);
        for _ in 0..20 {
            pool.carrier_rule_step();
        }
        assert!(
            pool.inner.shared.probe.lock().unwrap().active.is_none(),
            "a delivering window starts nothing"
        );

        // Losing windows until the suspicion confirms and the probe starts:
        // the peer keeps handing 8 MiB, this end delivers 1 MiB — 12.5 %.
        for _ in 0..200 {
            window(1 << 20, 8 << 20);
            for _ in 0..20 {
                pool.carrier_rule_step();
            }
            if pool.inner.shared.probe.lock().unwrap().active.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let probed = pool
            .inner
            .shared
            .probe
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|(carrier, _)| *carrier)
            .expect("the confirmed suspicion starts a probe");
        assert_eq!(probed, Carrier::Tcp, "a Kcp pool probes tcp");
        assert!(
            pool.snapshot().handover.is_some(),
            "the probe is a real handover: {snap:?}",
            snap = pool.snapshot()
        );

        // Wait for the handover to dial the probe's replacement, then let the
        // probe tunnel carry materially more than the incumbent tunnels: the
        // written half of the byte counters is what `probe_carried_bytes`
        // differences, and a window of 4 MiB against 1 MiB is 4x — past the
        // 1.5x edge with the measured 1.9x+ of the divergent cells to spare.
        for _ in 0..400 {
            pool.maintain().await;
            if pool.probe_tunnel_of(probed).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let probe_id = pool
            .probe_tunnel_of(probed)
            .expect("the probe's tunnel arrives with the handover's dial");
        // One more losing window so the suspicion's health sample has a
        // baseline, then the probe's carrying window.
        window(1 << 20, 8 << 20);
        let probe_counters: std::sync::Arc<TunnelCounters> = {
            let state = pool.inner.state.lock();
            state
                .entries
                .iter()
                .find(|e| e.tunnel.id() == probe_id)
                .map(|e| std::sync::Arc::clone(&e.tunnel.counters))
                .expect("the probe tunnel is held")
        };
        probe_counters
            .delivered
            .fetch_add(4 << 20, std::sync::atomic::Ordering::Relaxed);
        for counters in &counters {
            if !std::sync::Arc::ptr_eq(counters, &probe_counters) {
                counters
                    .delivered
                    .fetch_add(1 << 20, std::sync::atomic::Ordering::Relaxed);
            }
        }
        for _ in 0..20 {
            pool.carrier_rule_step();
        }
        assert!(
            choice.carrier() == Carrier::Tcp,
            "a Move verdict moves the endpoint's choice: {snap:?}",
            snap = pool.snapshot()
        );
        assert!(
            pool.inner.shared.probe.lock().unwrap().active.is_none(),
            "the verdict closes the probe"
        );
    }

    /// The other half of the same lifecycle: a probe whose carrier carries
    /// clearly less gives the tunnel back and parks the pool in its backoff,
    /// so a merely lossy path pays one split tunnel and then nothing.
    /// Needs both carriers: a probe hands a tunnel to the carrier the pool is
    /// not riding, and a build without `kcp` has only one to ride.
    #[cfg(feature = "kcp")]
    #[tokio::test]
    #[expect(
        unsafe_code,
        reason = "edition 2024 makes env mutation unsafe; the switch is read per pool at                   creation and this suite is serial, so the value is set before the pool exists"
    )]
    async fn the_probe_gives_the_tunnel_back_when_the_probed_carrier_carries_less() {
        // SAFETY: single-threaded test process; read at this pool's creation
        // (below), before any other pool in the suite is built.
        unsafe {
            std::env::set_var("MOLEHILL_TUNNEL_CARRIER_RULE", "1");
        }
        let (pool, _rxs, choice) = duplex_pool_with_choice(Carrier::Kcp, 2, 2);
        let counters: Vec<std::sync::Arc<TunnelCounters>> = {
            let state = pool.inner.state.lock();
            state
                .entries
                .iter()
                .map(|e| std::sync::Arc::clone(&e.tunnel.counters))
                .collect()
        };
        let mut seq = 0u8;
        let mut handed_total: u64 = 0;
        let mut window = |delivered: u64, handed: u64| {
            seq = seq.wrapping_add(1);
            handed_total = handed_total.saturating_add(handed);
            for counters in &counters {
                counters
                    .delivered
                    .fetch_add(delivered, std::sync::atomic::Ordering::Relaxed);
                counters.note_peer_health((100_000, 1_000), handed_total, seq);
            }
        };
        window(7 << 20, 8 << 20);
        for _ in 0..20 {
            pool.carrier_rule_step();
        }
        for _ in 0..200 {
            window(1 << 20, 8 << 20);
            for _ in 0..20 {
                pool.carrier_rule_step();
            }
            if pool.inner.shared.probe.lock().unwrap().active.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let probed = pool
            .inner
            .shared
            .probe
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|(carrier, _)| *carrier)
            .expect("the suspicion starts a probe");
        for _ in 0..400 {
            pool.maintain().await;
            if pool.probe_tunnel_of(probed).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let probe_id = pool
            .probe_tunnel_of(probed)
            .expect("the probe's tunnel arrives");
        window(1 << 20, 8 << 20);
        // The probe carries a *tenth* of the incumbent average: a clear
        // Revert — and above the idle-window floor, so the comparison is a
        // measurement and not the routing artifact the floor exists to hold.
        let probe_counters: std::sync::Arc<TunnelCounters> = {
            let state = pool.inner.state.lock();
            state
                .entries
                .iter()
                .find(|e| e.tunnel.id() == probe_id)
                .map(|e| std::sync::Arc::clone(&e.tunnel.counters))
                .expect("the probe tunnel is held")
        };
        probe_counters
            .delivered
            .fetch_add(8_192 * 20, std::sync::atomic::Ordering::Relaxed);
        for counters in &counters {
            if !std::sync::Arc::ptr_eq(counters, &probe_counters) {
                counters
                    .delivered
                    .fetch_add(1 << 20, std::sync::atomic::Ordering::Relaxed);
            }
        }
        for _ in 0..20 {
            pool.maintain().await;
        }
        {
            // Scoped: the guard must not cross the `await` below (clippy's
            // await-holding check, and the right discipline anyway).
            let probe = pool.inner.shared.probe.lock().unwrap();
            assert!(probe.active.is_none(), "a Revert verdict closes the probe");
            assert!(
                probe
                    .backoff_until
                    .is_some_and(|until| until > std::time::Instant::now()),
                "a probe that came back parks the pool in its backoff"
            );
        }
        assert_eq!(
            choice.carrier(),
            Carrier::Kcp,
            "a Revert verdict leaves the endpoint's choice alone"
        );
        // Same reason as the Move test: the registry entry must expire before
        // the parallel byte-sum test reads it.
        drop(pool);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    /// The whole chain, not just the verdict: the rule reads the pool's own
    /// counters, confirms over windows, and hands a tunnel over to TCP — with
    /// the replacement dialed by the repair path exactly as in production.
    #[tokio::test]
    #[expect(
        unsafe_code,
        reason = "edition 2024 makes env mutation unsafe; the switch's one reader caches in a \
                  OnceLock, this suite is serial, and the value is set before any pool in \
                  the test exists"
    )]
    async fn the_carrier_rule_hands_a_tunnel_over_after_its_ticks() {
        // SAFETY: single-threaded test process; the variable is read once per
        // process by `carrier_rule_enabled`, before this test touches it.
        unsafe {
            std::env::set_var("MOLEHILL_TUNNEL_CARRIER_RULE", "1");
        }
        // A Kcp-carrier pool: the rule's move is Kcp → Tcp, and a handover
        // looks for Kcp entries in the pool it runs on.
        let (pool, _rxs) = duplex_pool_on(Carrier::Kcp, 2, 2);
        let counters: Vec<std::sync::Arc<TunnelCounters>> = {
            let state = pool.inner.state.lock();
            state
                .entries
                .iter()
                .map(|e| std::sync::Arc::clone(&e.tunnel.counters))
                .collect()
        };

        // A baseline window: the peer hands each tunnel 8 MiB and this end
        // delivers nearly all of it — the healthy-lossy shape the old re-send
        // share misfired on.
        let mut seq = 0u8;
        let mut handed_total: u64 = 0;
        // Deltas in, cumulative counters out: the peer's report and this end's
        // own counter are both cumulative for the connection's life, and the rule
        // is what differences them.
        let mut window = |delivered: u64, handed: u64| {
            seq = seq.wrapping_add(1);
            handed_total = handed_total.saturating_add(handed);
            for counters in &counters {
                counters
                    .delivered
                    .fetch_add(delivered, std::sync::atomic::Ordering::Relaxed);
                counters.note_peer_health((100_000, 1_000), handed_total, seq);
            }
        };
        window(7 << 20, 8 << 20);
        // One window is 20 maintenance ticks; the pool's own task shares this
        // rule, so the test drives enough ticks and asserts the *outcome*.
        for _ in 0..20 {
            pool.carrier_rule_step();
        }
        assert!(pool.snapshot().handover.is_none());

        // Losing windows until the handover fires: the peer keeps handing over
        // 8 MiB and this end delivers 1 MiB of it — 12.5 %.
        for _ in 0..200 {
            window(1 << 20, 8 << 20);
            for _ in 0..20 {
                pool.carrier_rule_step();
            }
            if pool.snapshot().handover.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let snap = pool.snapshot();
        assert!(
            snap.handover.is_some(),
            "a tunnel that stops delivering hands over: {snap:?}"
        );
        assert_eq!(
            snap.handover.as_ref().map(|(_, to, _)| *to),
            Some(Carrier::Tcp),
            "the replacement rides tcp: {snap:?}"
        );

        // After the fire, a window that delivers again resets the streak: the
        // rule waits for a fresh verdict before it can move another tunnel.
        window(8 << 20, 8 << 20);
        for _ in 0..20 {
            pool.carrier_rule_step();
        }
        assert_eq!(
            *pool.inner.shared.rule_streak.lock().unwrap(),
            0,
            "a delivering window resets the streak after the move"
        );
    }

    /// The suspicion: a tunnel re-sending a share of its window *suspects*
    /// the carrier and starts the probe — measured on the policed condition
    /// (27 % share, delivery pinned at 99 %) this is the only local signal
    /// that fires at all; a healthy sender (3.5 % median) suspects nothing.
    #[test]
    fn the_share_suspicion_starts_what_the_probe_decides() {
        const MIB: u64 = 1 << 20;
        let before = vec![(7usize, (100_000u64, 1u64, 5 * MIB, 5 * MIB))];
        // Policed-shaped: 27 % re-sent, everything delivered — the delivery
        // verdict reads nothing here and the share must.
        let policed = vec![(7, (200_000, 27_000, 5 * MIB + 8 * MIB, 13 * MIB))];
        assert_eq!(carrier_rule_verdict(&before, &policed), None);
        let share = carrier_rule_suspicion(&before, &policed).expect("27 % suspects");
        assert!(share > 20.0, "{share}");

        // Healthy: 3.5 % re-sent — no suspicion.
        let healthy = vec![(7, (200_000, 3_500, 5 * MIB + 8 * MIB, 13 * MIB))];
        assert_eq!(carrier_rule_suspicion(&before, &healthy), None);

        // A tunnel carrying too little to judge suspects nothing: one stray
        // re-send must not start a probe.
        let tiny = vec![(7, (100, 20, 5 * MIB + 4_096, 5 * MIB + 4_096))];
        assert_eq!(carrier_rule_suspicion(&before, &tiny), None);

        // A tunnel that joined between the samples has no window.
        let joined = vec![(9, (100_000, 50_000, 1, MIB))];
        assert_eq!(carrier_rule_suspicion(&before, &joined), None);
    }

    /// The probe's verdict: the two carriers' carried bytes per second per
    /// tunnel, separated by the same 1.5x edge the lane policy uses. An idle
    /// path is held, not failed — calling it is how a probe would flunk a
    /// quiet tunnel.
    #[test]
    fn the_probe_verdict_separates_the_carriers() {
        use ProbeVerdict::{Hold, Move, Revert};

        // Level: within the edge either way — hold, and look again.
        assert_eq!(probe_verdict(100.0, 120.0), Hold);
        assert_eq!(probe_verdict(100.0, 150.0), Hold, "1.5x is the edge itself");
        assert_eq!(probe_verdict(150.0, 100.0), Hold);

        // The measured separation on the divergent cells is 1.9x and up, so
        // the probe has room: a probed carrier carrying half again or more
        // than the incumbent's average tunnel moves the pool, and carrying
        // clearly less gives the tunnel back.
        assert_eq!(probe_verdict(100.0, 151.0), Move);
        assert_eq!(probe_verdict(100.0, 20.0), Revert);
        assert_eq!(
            probe_verdict(0.0, 1.0),
            Move,
            "an idle incumbent loses to anything"
        );

        // Nothing moved through either side: nothing to compare, and a probe
        // that called it here would flunk an idle path.
        assert_eq!(probe_verdict(0.0, 0.0), Hold);
    }

    /// The probe's window arithmetic: the pool carrier's tunnels average
    /// (a probe is one tunnel, the incumbent several), the probe tunnel is
    /// excluded from that average, and a tunnel with no previous sample
    /// contributes nothing.
    #[test]
    fn probe_carried_bytes_averages_the_incumbent_tunnels() {
        const MIB: u64 = 1 << 20;
        // Cumulative delivered bytes: two incumbent tunnels and the probe.
        let last = vec![
            (1usize, (0u64, 0, MIB, MIB)),
            (2, (0, 0, MIB, MIB)),
            (9, (0, 0, 0, MIB)),
        ];
        let now = vec![
            (1, (0, 0, 3 * MIB, MIB)),
            (2, (0, 0, MIB + MIB / 2, MIB)),
            (9, (0, 0, 3 * MIB, MIB)),
        ];
        let (chosen, probe) = probe_carried_bytes(&last, &now, 9);
        // Incumbents moved 2 MiB and 0.5 MiB; their average is 1.25 MiB.
        assert_eq!(chosen, MIB + MIB / 4, "the incumbent tunnels average");
        assert_eq!(probe, 3 * MIB, "the probe tunnel's own window");

        // A tunnel that joined between the samples has no window.
        let joined_last = vec![(7usize, (0, 0, MIB, MIB))];
        let joined_now = vec![(7, (0, 0, MIB + 1, MIB))];
        let (chosen, probe) = probe_carried_bytes(&joined_last, &joined_now, 7);
        assert_eq!(probe, 1, "the probe's own window is counted");
        assert_eq!(chosen, 0, "an incumbent with no window contributes nothing");
    }

    /// A tunnel's byte counts accumulate per tunnel, and the pool snapshot
    /// exposes them cumulatively so a policy can difference two snapshots into
    /// a window.
    ///
    /// This tests the accumulator; the *wiring* (counting every read and write
    /// of a mux channel) is exercised by the mux end-to-end suites, which
    /// carry traffic through the forwarding wrapper that calls it.
    #[tokio::test]
    async fn tunnel_byte_counts_accumulate_per_tunnel() {
        let (pool, _rxs) = duplex_pool(2, 2);
        let mut streams = Vec::new();
        for _ in 0..2 {
            streams.push(pool.open_stream().await.expect("an open"));
        }
        // Two polls on the first tunnel, one on the second, on both sides of
        // the counter: the pairs add up apart, never mixed.
        streams[0].count_bytes(100, 10);
        streams[1].count_bytes(7, 0);
        streams[0].count_bytes(50, 5);

        let snap = pool.snapshot();
        // The snapshot carries one pair per tunnel, in pool order; the
        // counting itself is verified by summing: 150/15 on the tunnel that
        // took both polls, 7/0 on the other.
        let total_delivered: u64 = snap.bytes.iter().map(|(d, _)| *d).sum();
        let total_written: u64 = snap.bytes.iter().map(|(_, w)| *w).sum();
        assert_eq!(total_delivered, 157, "delivered adds up across tunnels");
        assert_eq!(total_written, 15, "written adds up across tunnels");
        assert_eq!(snap.bytes.len(), 2, "one pair per tunnel: {snap:?}");
        // The client's own read of the same counters (the tunnel-stats line's
        // source) sees this pool's pair among the process's live pools. The
        // process total is asserted as a *superset*, not an equality: the
        // suite runs its pools in parallel, and another test's pool carrying
        // its own bytes is not this test's subject.
        let seen = crate::core::client::tunnel_bytes();
        assert!(
            seen.iter().any(|(d, w)| *d >= 150 && *w >= 15),
            "tunnel_bytes carries this pool's counted pair: {seen:?}"
        );
        let seen_delivered: u64 = seen.iter().map(|(d, _)| *d).sum();
        let seen_written: u64 = seen.iter().map(|(_, w)| *w).sum();
        assert!(
            seen_delivered >= 157 && seen_written >= 15,
            "{seen_delivered} {seen_written}"
        );
    }

    #[tokio::test]
    async fn client_opens_streams_server_receives() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

        // Open a stream and push some bytes through
        let mut stream = tunnel.open_stream().await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();

        let mut server_stream = inbound_rx.recv().await.unwrap();
        let mut buf = [0u8; 4];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // Reply on the same stream: full duplex
        server_stream.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        drop(tunnel);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_first_stream_is_announced_to_the_server() {
        // Regression test for the 0.7.0 data-path stall: production data
        // channels are server-speaks-first, so the client starts by READING
        // the freshly opened stream. yamux attaches the stream's SYN flag to
        // its first outbound frame; without an explicit empty-write kick the
        // SYN is never emitted and neither peer ever sees the stream.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

        let mut stream = tunnel.open_stream().await.unwrap();

        // The server receives the stream even though the client has not
        // written any payload, then speaks first like the pool pairing code.
        let server_side = tokio::spawn(async move {
            let mut server_stream =
                tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
                    .await
                    .expect("server did not receive the read-only stream")
                    .expect("server tunnel closed");
            server_stream.write_all(b"go").await.unwrap();
            let mut buf = [0u8; 4];
            server_stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        });

        let mut cmd = [0u8; 2];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_exact(&mut cmd),
        )
        .await
        .expect("server command never arrived")
        .unwrap();
        assert_eq!(&cmd, b"go");

        stream.write_all(b"ping").await.unwrap();
        server_side.await.unwrap();

        drop(tunnel);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn client_opens_streams_over_real_tcp() {
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            run_server_tunnel(sock, mux_config(), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Open several streams back-to-back before reading anything
        let mut streams = Vec::new();
        for i in 0..4 {
            let mut s = tunnel.open_stream().await.unwrap();
            s.write_all(format!("msg{i}").as_bytes()).await.unwrap();
            streams.push(s);
        }

        // Server side mirrors production pooling: write a command INTO each
        // accepted stream (like StartForwardTcp) BEFORE the client reads it.
        for i in 0..4 {
            let mut s = inbound_rx.recv().await.unwrap();
            s.write_all(format!("cmd{i}").as_bytes()).await.unwrap();
            // hold the stream alive like the pool pairing task would
            tokio::spawn(async move {
                let mut echo = [0u8; 8];
                // keep reading so window updates flow
                loop {
                    match s.read(&mut echo).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }

        // Client reads the commands back: each stream must carry exactly the
        // command the server wrote into it, which is what a read-only pooled
        // stream's SYN announcement buys.
        for (i, stream) in streams.iter_mut().enumerate() {
            let mut buf = [0u8; 5];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                    .await
                    .expect("stream {i}: the command never arrived")
                    .expect("stream {i}: the stream read failed");
            assert!(read > 0, "stream {i}: the server closed it with no command");
            assert_eq!(
                &buf[..read],
                format!("cmd{i}").as_bytes(),
                "stream {i} carried the wrong command"
            );
        }
        drop(streams);
        drop(tunnel);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rapid_opens_without_immediate_consumer() {
        // Mimic production pool pre-creation: many open requests arrive
        // back-to-back while the server-side consumer has not read anything.
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, inbound_rx) = mpsc::channel(4);

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            run_server_tunnel(sock, mux_config(), inbound_tx).await;
        });

        let client_sock = TcpStream::connect(addr).await.unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Fire 12 opens concurrently without awaiting them in order.
        let mut handles = Vec::new();
        for i in 0..12 {
            let t = tunnel.clone();
            handles.push(tokio::spawn(async move {
                let mut s =
                    tokio::time::timeout(std::time::Duration::from_secs(3), t.open_stream())
                        .await
                        .expect("open_stream timed out")
                        .expect("open failed");
                s.write_all(format!("m{i}").as_bytes()).await.unwrap();
            }));
        }
        // Give the client driver time to wedge if it is going to: every open
        // must complete inside two seconds even though the server side reads
        // none of them yet.
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if handles.iter().all(tokio::task::JoinHandle::is_finished) {
                break;
            }
        }
        let done = handles.iter().filter(|h| h.is_finished()).count();
        assert_eq!(done, 12, "driver wedged");

        // Dropping the receiver closes the tunnel from the consumer side;
        // the server driver must then exit cleanly.
        drop(inbound_rx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("server driver did not exit after consumer dropped");
    }

    #[tokio::test]
    async fn open_after_idle_period() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};

        // Production failure signature: initial pooled opens succeed, then
        // after an idle period a NEW open never completes.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(64);

        let mut client_sock = TcpStream::connect(addr).await.unwrap();
        let mut server_sock = {
            let (s, _) = listener.accept().await.unwrap();
            s
        };
        // Mimic production: hello + ack bytes flow on the socket BEFORE the
        // yamux sessions are constructed.
        client_sock.write_all(&[0u8; 34]).await.unwrap();
        let mut hello = [0u8; 34];
        server_sock.read_exact(&mut hello).await.unwrap();
        let mut ack = [0u8; 1];
        server_sock.write_all(&ack).await.unwrap();
        client_sock.read_exact(&mut ack).await.unwrap();

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            run_server_tunnel(server_sock, mux_config(), inbound_tx).await;
        });

        let tunnel = ClientTunnel::start(client_sock, mux_config(), shutdown_rx);

        // Phase 1: burst of 8 opens like the pool pre-creation
        let mut first = Vec::new();
        for i in 0..8 {
            let mut s = tunnel.open_stream().await.expect("phase1 open failed");
            s.write_all(format!("msg{i}").as_bytes()).await.unwrap();
            first.push(s);
        }
        // Drain server side fully
        for i in 0..8 {
            let mut srv_stream =
                tokio::time::timeout(std::time::Duration::from_secs(3), inbound_rx.recv())
                    .await
                    .expect("recv timed out")
                    .unwrap();
            let mut buf = [0u8; 4];
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                srv_stream.read_exact(&mut buf),
            )
            .await
            .expect("read_exact timed out")
            .unwrap();
            assert_eq!(&buf, format!("msg{i}").as_bytes());
        }

        // Phase 2: go idle, then try one more open
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut s = tokio::time::timeout(std::time::Duration::from_secs(2), tunnel.open_stream())
            .await
            .expect("open after idle TIMED OUT")
            .expect("open failed");
        s.write_all(b"late").await.unwrap();
        let mut srv_stream = inbound_rx.recv().await.unwrap();
        let mut buf = [0u8; 4];
        srv_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"late");

        drop(first);
        drop(tunnel);
        server.await.unwrap();
    }

    /// A pool over `n` in-memory tunnels, pinned at its configured `count`.
    /// The caller keeps the shutdown senders; the pool drops its own copy of
    /// each tunnel's when it removes the tunnel.
    fn duplex_pool(n: usize, count: usize) -> (TunnelPool, Vec<mpsc::Receiver<MuxStream>>) {
        duplex_pool_on(Carrier::Tcp, n, count)
    }

    /// The same, on the named carrier: the carrier rule's tests need a pool
    /// whose entries ride Kcp, because its move is `Kcp → Tcp` and a
    /// `begin_handover` refuses to look for Kcp entries in a Tcp pool.
    fn duplex_pool_on(
        carrier: Carrier,
        n: usize,
        count: usize,
    ) -> (TunnelPool, Vec<mpsc::Receiver<MuxStream>>) {
        let mut tunnels = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..n {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (tx, rx) = mpsc::channel(8);
            let server = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
            std::mem::forget(server);
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            std::mem::forget(shutdown_tx);
            tunnels.push((
                ClientTunnel::start_with_health(
                    client_io,
                    Some(crate::transport::health::SessionHealth::new()),
                    mux_config(),
                    shutdown_rx,
                ),
                tokio::sync::watch::channel(false).0,
            ));
            rxs.push(rx);
        }
        let pool = TunnelPool::new(carrier, "test".to_owned(), tunnels, count);
        (pool, rxs)
    }

    /// The duplex pool with a live `CarrierChoice` installed, so a probe's
    /// `Move` verdict has the authority to act on: the rule's moves go through
    /// `prefer`, and a pool without a choice can still hand a tunnel over but
    /// cannot move the endpoint.
    fn duplex_pool_with_choice(
        carrier: Carrier,
        n: usize,
        count: usize,
    ) -> (
        TunnelPool,
        Vec<mpsc::Receiver<MuxStream>>,
        std::sync::Arc<crate::transport::carrier::CarrierChoice>,
    ) {
        // `CarrierChoice::new` already returns an `Arc<Self>`.
        let choice = crate::transport::carrier::CarrierChoice::new("probe-test:1");
        let (pool, rxs) = {
            let mut tunnels = Vec::new();
            let mut rxs = Vec::new();
            for _ in 0..n {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let (tx, rx) = mpsc::channel(8);
                tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
                let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                tunnels.push((
                    ClientTunnel::start_with_health(
                        client_io,
                        Some(crate::transport::health::SessionHealth::new()),
                        mux_config(),
                        shutdown_rx,
                    ),
                    tokio::sync::watch::channel(false).0,
                ));
                rxs.push(rx);
            }
            let pins = std::sync::Arc::new(PinRegistry::new());
            // A dialer is part of the probe's own mechanics: the handover's
            // Dialing phase dials the probe's replacement through it, so a
            // fixture without one can never produce the tunnel the probe is
            // supposed to measure.
            let dial: Dialer = std::sync::Arc::new(|_: Carrier| {
                Box::pin(async move { Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0)) })
            });
            (
                TunnelPool::with_choice(
                    carrier,
                    "test".to_owned(),
                    tunnels,
                    count,
                    Some(dial),
                    std::sync::Arc::new(move || carrier),
                    pins,
                    Some(std::sync::Arc::clone(&choice)),
                ),
                rxs,
            )
        };
        (pool, rxs, choice)
    }

    /// One in-memory tunnel whose server side drains every stream it is sent:
    /// the unit these pool fixtures are built from, and the unit a *repair*
    /// dials.
    fn duplex_tunnel() -> ClientTunnel {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<MuxStream>(8);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        std::mem::forget(tokio::spawn(run_server_tunnel(server_io, mux_config(), tx)));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);
        ClientTunnel::start(client_io, mux_config(), shutdown_rx)
    }

    /// A pool that starts with `n` in-memory tunnels and is configured for
    /// `count`: every repair dials one more, until the count is reached.
    fn growable_pool(n: usize, count: usize) -> TunnelPool {
        let dial: Dialer = std::sync::Arc::new(|_: Carrier| {
            Box::pin(async move { Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0)) })
        });
        let initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)> = (0..n)
            .map(|_| (duplex_tunnel(), tokio::sync::watch::channel(false).0))
            .collect();
        TunnelPool::with_dialer(
            Carrier::Tcp,
            "stripes".to_owned(),
            initial,
            count,
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        )
    }

    /// The same, with the dialer counted: how many times the pool repaired, as
    /// a number a test can assert on.
    fn counting_pool(
        n: usize,
        count: usize,
        dials: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> TunnelPool {
        let dial: Dialer = std::sync::Arc::new(move |_: Carrier| {
            let dials = std::sync::Arc::clone(&dials);
            Box::pin(async move {
                dials.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0))
            })
        });
        let initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)> = (0..n)
            .map(|_| (duplex_tunnel(), tokio::sync::watch::channel(false).0))
            .collect();
        TunnelPool::with_dialer(
            Carrier::Tcp,
            "stripes".to_owned(),
            initial,
            count,
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        )
    }

    /// Open one stripe group of `stripes` streams the way the client does —
    /// each stripe avoiding the tunnels the group already holds — and return
    /// the tunnel ids the group landed on.
    async fn open_one_stripe_group(pool: &TunnelPool, stripes: usize) -> Vec<usize> {
        let mut used = Vec::with_capacity(stripes);
        let mut streams = Vec::with_capacity(stripes);
        for _ in 0..stripes {
            let stream = pool
                .open_stream_on_distinct(&used, stripes)
                .await
                .expect("a stripe of the group must open");
            used.push(stream.tunnel_id());
            streams.push(stream);
        }
        // Hold the streams until the whole group is placed, so a lease cannot
        // retire mid-group and change what the next stripe avoids.
        drop(streams);
        used
    }

    /// A stripe group larger than its pool shares the tunnels that exist
    /// instead of dialing: `k > count` distinct-stripe opens all succeed, the
    /// pool stays exactly `count`, and the dialer is never called.
    ///
    /// The group's exclusion is a preference, not a constraint (D24), and a
    /// pinned pool cannot dial a tunnel per stripe: capacity is what the
    /// configuration says, so the extra stripes ride the tunnels the group
    /// already has.
    #[tokio::test]
    async fn a_stripe_group_larger_than_the_pool_shares_it_without_dialing() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        const COUNT: usize = 2;
        const STRIPES: usize = 5;
        let dials = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(COUNT, COUNT, Arc::clone(&dials));
        assert_eq!(
            pool.snapshot().count,
            COUNT,
            "the configured count is reported"
        );

        let used = open_one_stripe_group(&pool, STRIPES).await;
        assert_eq!(used.len(), STRIPES, "every stripe of the group opened");
        assert_eq!(
            dials.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a pinned pool never dials for a stripe group"
        );
        assert_eq!(pool.size(), COUNT, "the pool stays at its configured count");
        assert_eq!(pool.snapshot().grows, 0, "no establishment was recorded");
        assert_eq!(
            used.iter().collect::<std::collections::HashSet<_>>().len(),
            COUNT,
            "the group spreads over the tunnels it has, sharing them: {used:?}"
        );
    }

    /// D14's other half: a refused repair *stops* the repair dials. The
    /// maintenance tick runs every 50 ms, so without the hold a client at the
    /// server's `max_tunnels_per_client` cap would dial-and-be-refused twenty
    /// times a second against the server's accept path.
    #[tokio::test]
    async fn a_refused_repair_holds_the_pool_back() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A dialer that always refuses, counted: the shape the server's
        // `max_tunnels_per_client` valve takes at the pool.
        let dials = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&dials);
        let dial: Dialer = Arc::new(move |_: Carrier| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err("refused by the server's tunnel valve".to_owned())
            })
        });
        let pool = TunnelPool::with_dialer(
            Carrier::Tcp,
            "refused".to_owned(),
            Vec::new(),
            4,
            Some(dial),
            Arc::new(PinRegistry::new()),
        );
        assert!(!pool.growth_held_off(), "nothing has failed yet");

        pool.grow(GrowReason::Repair).await;
        assert!(
            pool.growth_held_off(),
            "a failed repair must hold the next one off"
        );

        // While the hold stands, no tick dials again.
        let attempts = dials.load(Ordering::SeqCst);
        assert!(attempts >= 1, "the refused repair was attempted");
        for _ in 0..5 {
            pool.maintain().await;
        }
        assert_eq!(
            dials.load(Ordering::SeqCst),
            attempts,
            "no dial while the repair is held off"
        );
        assert_eq!(pool.snapshot().size, 0, "no tunnel appeared while held");

        // A tunnel dying (or one being removed) is what makes a retry
        // meaningful again.
        pool.release_growth_hold();
        assert!(!pool.growth_held_off());
    }

    /// D24: a stripe group's channels arrive as back-to-back requests, and the
    /// reservation is charged before the first `await`, so back-to-back opens
    /// take back-to-back least-loaded places — a group of K lands on K
    /// **distinct** tunnels whenever the pool has K.
    #[tokio::test]
    async fn back_to_back_opens_land_on_distinct_tunnels() {
        let (pool, _rxs) = duplex_pool(4, 4);
        let mut streams = Vec::new();
        for _ in 0..3 {
            streams.push(pool.open_stream().await.expect("an open"));
        }
        let used: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _, _)| *streams)
            .collect();
        assert_eq!(
            used.iter().filter(|n| **n > 0).count(),
            3,
            "three opens on a four-tunnel pool must use three tunnels: {used:?}"
        );
        assert_eq!(
            used.iter().filter(|n| **n == 1).count(),
            3,
            "one stream per tunnel, none doubled up: {used:?}"
        );
        assert_eq!(pool.size(), 4, "the pool is exactly its count: {used:?}");
        assert_eq!(pool.snapshot().count, 4);
        drop(streams);
    }

    /// More opens than tunnels still all produce a stream: a pinned pool
    /// reuses what it has instead of failing a visitor, and the extra opens
    /// never push it past its configured count.
    #[tokio::test]
    async fn more_opens_than_tunnels_still_answer_every_open() {
        let (pool, _rxs) = duplex_pool(2, 2);
        let mut streams = Vec::new();
        for _ in 0..4 {
            streams.push(pool.open_stream().await.expect("an open"));
        }
        assert_eq!(streams.len(), 4, "every open still produced a stream");
        let used: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _, _)| *streams)
            .collect();
        assert_eq!(used, vec![2, 2], "the pool reuses both tunnels: {used:?}");
        assert_eq!(pool.size(), 2, "the pool never grows for load");
        drop(streams);
    }

    /// Correctness first: a group the pool cannot spread over still opens
    /// every stripe. `count` is the pool's width, so the third and fourth
    /// stripe reuse the two tunnels — exactly the behaviour a striped visitor
    /// had before the group had a name, and the reason the exclusion is a
    /// preference rather than a constraint.
    #[tokio::test]
    async fn a_stripe_group_the_pool_cannot_spread_over_still_opens() {
        let pool = growable_pool(2, 2);
        let mut used: Vec<usize> = Vec::new();
        let mut streams = Vec::new();
        for _ in 0..4 {
            let lease = pool
                .open_stream_on_distinct(&used, 4)
                .await
                .expect("every stripe must still open");
            used.push(lease.tunnel_id());
            streams.push(lease);
        }
        assert_eq!(pool.size(), 2, "the count bounds the spread");
        assert_eq!(pool.snapshot().count, 2);
        let distinct: std::collections::HashSet<usize> = used.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            2,
            "the group can only spread as far as the pool allows: {used:?}"
        );
        let per_tunnel: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _, _)| *streams)
            .collect();
        assert_eq!(
            per_tunnel,
            vec![2, 2],
            "both tunnels are reused, evenly: {per_tunnel:?}"
        );
        drop(streams);
    }

    /// The stream count is a `Drop` charge: it must come back when the
    /// consumer lets the stream go, or no pool would ever look idle.
    #[tokio::test]
    async fn a_closed_stream_leaves_the_tunnel_count() {
        let (pool, _rxs) = duplex_pool(1, 1);
        let stream = pool.open_stream().await.expect("one open");
        assert_eq!(pool.snapshot().streams(), 1);
        drop(stream);
        assert_eq!(pool.snapshot().streams(), 0);
    }

    /// The placement ceiling is the only oversubscription protection a pinned
    /// pool has: a pool at its count whose every tunnel sits at the ceiling
    /// waits [`crate::transport::pool::CAPACITY_WAIT`] out and then refuses the
    /// visitor — telling the operator once, at INFO, which key widens the pool
    /// — and a stream retiring under the wait lets the next one in.
    #[tokio::test]
    async fn a_pool_at_its_placement_ceiling_waits_then_refuses() {
        // The saturation notice is process-wide, and this is the only test that
        // drives a pool into saturation.
        POOL_SATURATED.clear();
        let (pool, _rxs) = duplex_pool(1, 1);
        let ceiling = pool.ceiling();
        assert_eq!(pool.size(), 1, "the pool is its configured count");

        let mut held = Vec::new();
        for _ in 0..ceiling {
            held.push(pool.open_stream().await.expect("an open under the ceiling"));
        }
        assert_eq!(
            pool.snapshot()
                .tunnels
                .iter()
                .map(|(streams, _, _, _)| *streams)
                .sum::<usize>(),
            ceiling,
            "every stream slot this pool has is taken"
        );

        let refusal = pool.open_stream().await;
        assert!(
            matches!(&refusal, Err(OpenError::AtCapacity)),
            "a pool at its ceiling must refuse with AtCapacity after the wait: {:?}",
            refusal.map(|_| ())
        );

        // That refusal reported the saturation (INFO); the notice fires once
        // per process, so the next ones are DEBUG-only.
        assert!(
            !pool.report_saturation(),
            "the refusal above must have been the first report"
        );
        assert!(
            !pool.report_saturation(),
            "the notice must not fire twice per process"
        );
        POOL_SATURATED.clear();
        assert!(
            pool.report_saturation(),
            "a cleared notice reports the next saturation again"
        );
        assert!(!pool.report_saturation(), "and only once each time");

        // One stream retires: the next visitor is served by the pool that
        // exists, with no dial and no new tunnel.
        held.pop();
        let next = pool
            .open_stream()
            .await
            .expect("a retiring stream must let the next visitor in");
        assert_eq!(pool.size(), 1, "the pool never dialed for the visitor");
        drop(next);
        drop(held);
    }

    /// A pool that came up short of its configured count repairs to it: the
    /// dialer is called once per missing tunnel and never past `count`.
    ///
    /// The pool starts with no tunnel at all — the state a service start whose
    /// establishment dials all failed leaves — so the open path dials one to
    /// answer its visitor and the maintenance tick finishes the repair.
    #[tokio::test]
    async fn a_pool_short_of_its_count_repairs_up_to_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const COUNT: usize = 3;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let dial: Dialer = std::sync::Arc::new(move |_: Carrier| {
            let counter = std::sync::Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0))
            })
        });
        let pool = TunnelPool::with_dialer(
            Carrier::Tcp,
            "short".to_owned(),
            Vec::new(),
            COUNT,
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        );
        assert_eq!(pool.size(), 0, "the pool started with no tunnel");
        assert_eq!(
            pool.snapshot().count,
            COUNT,
            "but it is configured for its count"
        );

        let first = pool
            .open_stream()
            .await
            .expect("the first open repairs a tunnel");
        assert_eq!(pool.size(), 1, "the open dialed exactly one tunnel");
        assert_eq!(pool.snapshot().grows, 1);
        drop(first);

        // The tick keeps repairing, one dial at a time, until the count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pool.size() < COUNT && std::time::Instant::now() < deadline {
            pool.maintain().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(pool.size(), COUNT, "the repair stopped at the count");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            COUNT,
            "one dial per missing tunnel, never more"
        );

        // A complete pool has nothing to repair.
        for _ in 0..5 {
            pool.maintain().await;
        }
        assert_eq!(pool.size(), COUNT, "a complete pool stays at its count");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            COUNT,
            "a complete pool dials nothing"
        );
    }

    /// The client's pin count is the UDP affinity accounting the hub and the
    /// snapshot read: a bound, active peer shows up against its tunnel, and
    /// releasing it takes the count back to zero.
    #[tokio::test]
    async fn pin_accounting_tracks_the_tunnels_peers() {
        let (pool, _rxs) = duplex_pool(2, 2);
        let id = pool
            .inner
            .state
            .lock()
            .entries
            .first()
            .map(|e| e.tunnel.id())
            .expect("two tunnels");
        assert_eq!(pool.pinned_peers(0), 0);
        pool.pins().bind(7, id);
        pool.pins().update(7, true);
        assert_eq!(pool.pinned_peers(0), 1);
        assert_eq!(pool.snapshot().pinned(), 1);
        pool.pins().update(7, false);
        assert_eq!(pool.pinned_peers(0), 0);
    }

    #[test]
    fn mux_config_is_total() {
        // The config is built from fixed internal constants; keep a guard
        // so a change to those constants cannot make the builder panic.
        let _ = mux_config();
    }

    #[test]
    fn default_window_keeps_auto_tunable_credit() {
        // Regression guard: yamux reserves `max_streams * 256 KiB` of the
        // connection window as guaranteed per-stream credit; the auto-tuner
        // may only allocate the remainder. A stream count that swallows the
        // whole window pins every stream at 256 KiB (measured: 0.1 Gbps at
        // 10 ms RTT, a ~30x drop from the tuned window).
        use crate::common::constants::{DEFAULT_MUX_MAX_STREAMS, DEFAULT_MUX_RECEIVE_WINDOW};

        const CREDIT: usize = 256 * 1024; // yamux DEFAULT_CREDIT
        let reserved = DEFAULT_MUX_MAX_STREAMS * CREDIT;
        assert!(
            DEFAULT_MUX_RECEIVE_WINDOW > reserved,
            "the credit reservation must not swallow the whole window"
        );
        assert!(
            DEFAULT_MUX_RECEIVE_WINDOW - reserved >= DEFAULT_MUX_RECEIVE_WINDOW / 2,
            "at least half the window must stay auto-tunable"
        );
        let _ = mux_config();
    }

    #[tokio::test]
    async fn pool_spreads_streams_over_tunnels() {
        // Two tunnels, four opens: the least-loaded rule must place two
        // streams on each tunnel, and every stream must survive a full round
        // trip. Each stream carries its own payload, so the assertions do not
        // depend on which tunnel the placement picked. A second burst then
        // takes the stream count past the pool's width: the extra streams
        // spread over the same two tunnels and the pool never grows.
        const TUNNELS: usize = 2;
        const OPENS: usize = 4;

        let mut tunnels = Vec::new();
        let mut per_tunnel_counts = Vec::new();
        // One shutdown sender per tunnel keeps every driver alive; the pool
        // holds it and dropping the pool stops all drivers.
        let mut shutdown_senders = Vec::new();

        for _ in 0..TUNNELS {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (inbound_tx, mut inbound_rx) = mpsc::channel::<MuxStream>(8);
            let counter = tokio::spawn(async move {
                let mut count = 0usize;
                while let Some(mut stream) = inbound_rx.recv().await {
                    count += 1;
                    let mut buf = [0u8; 8];
                    stream.read_exact(&mut buf).await.unwrap();
                    // Round trip: prove the stream is usable both ways before
                    // the client drops it.
                    stream.write_all(b"ok").await.unwrap();
                    stream.flush().await.unwrap();
                }
                count
            });
            let server = tokio::spawn(run_server_tunnel(server_io, mux_config(), inbound_tx));
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            tunnels.push((
                ClientTunnel::start(client_io, mux_config(), shutdown_rx),
                shutdown_tx.clone(),
            ));
            shutdown_senders.push(shutdown_tx);
            per_tunnel_counts.push((counter, server));
        }

        let pool = TunnelPool::new(Carrier::Tcp, "test".to_owned(), tunnels, TUNNELS);
        assert_eq!(
            pool.snapshot().count,
            TUNNELS,
            "the pinned count is reported"
        );

        let mut live = Vec::new();
        for i in 0..OPENS {
            let mut s = pool.open_stream().await.unwrap();
            // Eight bytes, one payload per open: the receiving tunnel echoes
            // without needing to know its ordinal.
            s.write_all(format!("msg{i:05}").as_bytes()).await.unwrap();
            s.flush().await.unwrap();
            let mut ok = [0u8; 2];
            s.read_exact(&mut ok).await.unwrap();
            assert_eq!(&ok, b"ok");
            live.push(s);
        }
        // While the four streams are open, the least-loaded rule has put two
        // on each tunnel: that is the placement rule's observable outcome.
        assert_eq!(
            pool.snapshot()
                .tunnels
                .iter()
                .map(|(s, _, _, _)| *s)
                .collect::<Vec<_>>(),
            vec![OPENS / TUNNELS; TUNNELS],
            "the least-loaded rule must spread four opens evenly over two tunnels"
        );

        // More opens than the pool is wide: they are placed on the tunnels
        // that exist, and the pinned pool never dials a third.
        for i in 0..OPENS {
            let mut s = pool.open_stream().await.unwrap();
            s.write_all(format!("more{i:04}").as_bytes()).await.unwrap();
            s.flush().await.unwrap();
            let mut ok = [0u8; 2];
            s.read_exact(&mut ok).await.unwrap();
            assert_eq!(&ok, b"ok");
            live.push(s);
        }
        assert_eq!(pool.size(), TUNNELS, "the pool never grows for load");
        assert_eq!(
            pool.snapshot()
                .tunnels
                .iter()
                .map(|(s, _, _, _)| *s)
                .collect::<Vec<_>>(),
            vec![2 * OPENS / TUNNELS; TUNNELS],
            "the extra streams spread over the same tunnels"
        );
        drop(live);
        drop(pool);
        drop(shutdown_senders);

        for (counter, server) in per_tunnel_counts {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
            let count = tokio::time::timeout(std::time::Duration::from_secs(3), counter)
                .await
                .expect("tunnel counter did not finish")
                .unwrap();
            assert_eq!(count, 2 * OPENS / TUNNELS, "streams were not spread evenly");
        }
    }

    #[tokio::test]
    async fn pool_skips_dead_tunnel() {
        // Kill one tunnel's driver (drop its shutdown sender): opens must
        // fall through to the surviving tunnel instead of failing.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_io2, server_io2) = tokio::io::duplex(64 * 1024);

        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);
        let s1 = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx1));
        let s2 = tokio::spawn(run_server_tunnel(server_io2, mux_config(), tx2));

        let (shutdown1, shutdown1_rx) = tokio::sync::watch::channel(false);
        let (shutdown2, shutdown2_rx) = tokio::sync::watch::channel(false);
        let t1 = ClientTunnel::start(client_io, mux_config(), shutdown1_rx);
        let t2 = ClientTunnel::start(client_io2, mux_config(), shutdown2_rx);
        let pool = TunnelPool::new(
            Carrier::Tcp,
            "test".to_owned(),
            vec![(t1, shutdown1.clone()), (t2, shutdown2)],
            2,
        );

        // Sanity: both tunnels work.
        let mut a = pool.open_stream().await.unwrap();
        a.write_all(b"aaaa").await.unwrap();
        let mut b = pool.open_stream().await.unwrap();
        b.write_all(b"bbbb").await.unwrap();
        let mut got = Vec::new();
        for rx in [&mut rx1, &mut rx2] {
            let mut s = rx.recv().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            got.push(buf);
        }
        assert_eq!(got.len(), 2);

        // Shut tunnel 1 down; every subsequent open must still succeed on
        // tunnel 2 regardless of where the round-robin pointer stands.
        send_shutdown(&shutdown1);
        // Let the driver actually exit: an open that lands on the dying
        // driver while it is still draining its select loop would win the
        // race and return a stream that dies a moment later (WriteZero).
        // Once the driver is gone the pool's fall-through is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        for _ in 0..4 {
            let mut s = pool.open_stream().await.unwrap();
            s.write_all(b"cccc").await.unwrap();
        }
        for _ in 0..4 {
            let mut s = tokio::time::timeout(std::time::Duration::from_secs(3), rx2.recv())
                .await
                .expect("surviving tunnel stopped accepting streams")
                .unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"cccc");
        }

        drop(a);
        drop(b);
        drop(pool);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), s2).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), s1).await;
    }

    fn send_shutdown(tx: &tokio::sync::watch::Sender<bool>) {
        tx.send(true).unwrap();
    }

    /// A tunnel whose connection dies leaves the pool even while it still
    /// "holds" a stream, and the pool dials its replacement.
    ///
    /// **A repair asks for the carrier the endpoint prefers now, not the one the
    /// pool was born on.**
    ///
    /// The pool's carrier is part of its identity, and its dialer used to be
    /// built for that one carrier at pool creation — so a pool that came up on
    /// KCP kept repairing into KCP after UDP stopped working, which is the
    /// defect this seam exists for. The fixture makes the two answers differ on
    /// purpose: born on TCP, preferring KCP, and the *first* repair must ask for
    /// KCP.
    #[tokio::test]
    async fn a_handover_excludes_new_streams_until_existing_lease_ends() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dials = std::sync::Arc::new(AtomicUsize::new(0));
        let recorder = std::sync::Arc::clone(&dials);
        let dial: Dialer = std::sync::Arc::new(move |carrier| {
            let recorder = std::sync::Arc::clone(&recorder);
            Box::pin(async move {
                recorder.fetch_add(1, Ordering::SeqCst);
                assert_eq!(carrier, Carrier::Tcp);
                Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0))
            })
        });
        let initial = vec![
            (duplex_tunnel(), tokio::sync::watch::channel(false).0),
            (duplex_tunnel(), tokio::sync::watch::channel(false).0),
        ];
        let pool = TunnelPool::with_preference(
            Carrier::Kcp,
            "handover".to_owned(),
            initial,
            2,
            Some(dial),
            std::sync::Arc::new(|| Carrier::Tcp),
            std::sync::Arc::new(PinRegistry::new()),
        );
        let held = pool.open_stream().await.unwrap();
        assert!(pool.begin_handover(Carrier::Kcp, Carrier::Tcp));
        let snapshot = pool.snapshot();
        assert!(
            snapshot
                .draining
                .iter()
                .enumerate()
                .any(|(index, draining)| *draining && snapshot.tunnels[index].3 == Carrier::Kcp),
            "a KCP tunnel is unavailable to placement: {snapshot:?}"
        );
        assert!(
            snapshot.handover.is_some_and(|(_, carrier, phase)| {
                carrier == Carrier::Tcp && phase == "draining"
            }),
            "target carrier and phase are visible: {snapshot:?}"
        );
        assert!(snapshot.draining.iter().any(|is_draining| *is_draining));
        let replacement_stream = pool.open_stream().await.unwrap();
        assert!(
            snapshot.draining.iter().any(|draining| *draining),
            "pool must mark old carrier entry unavailable"
        );
        let serving_index = snapshot
            .draining
            .iter()
            .position(|draining| !draining)
            .unwrap();
        let serving_id = pool.inner.state.lock().entries[serving_index].tunnel.id();
        assert_eq!(replacement_stream.tunnel_id(), serving_id);
        assert_eq!(dials.load(Ordering::SeqCst), 0, "busy tunnel stays intact");
        drop(held);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while dials.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            pool.maintain().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "handover replaces one tunnel"
        );
        assert_eq!(pool.size(), 2, "pool count is preserved");
        assert!(
            pool.snapshot().handover.is_none(),
            "target tunnel completed handover"
        );
        assert!(
            pool.snapshot()
                .tunnels
                .iter()
                .any(|tunnel| tunnel.3 == Carrier::Tcp)
        );
        drop(replacement_stream);
        drop(pool);
    }

    #[tokio::test]
    async fn a_repair_establishes_the_preferred_carrier() {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&asked);
        let dial: Dialer = std::sync::Arc::new(move |carrier: Carrier| {
            let recorder = std::sync::Arc::clone(&recorder);
            Box::pin(async move {
                recorder.lock().unwrap().push(carrier);
                Err("no server in this test".to_owned())
            })
        });
        let preferred: PreferredCarrier = std::sync::Arc::new(|| Carrier::Kcp);
        // Empty pool, count 1: the maintenance tick's first act is a repair.
        let _pool = TunnelPool::with_preference(
            Carrier::Tcp,
            "preferred".to_owned(),
            Vec::new(),
            1,
            Some(dial),
            preferred,
            std::sync::Arc::new(PinRegistry::new()),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !asked.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let seen = asked.lock().unwrap().clone();
        assert_eq!(
            seen.first().copied(),
            Some(Carrier::Kcp),
            "the repair must ask for the preferred carrier, not the pool's birth one: {seen:?}"
        );
    }

    /// The old pool asked for a replacement only when the dead tunnel had
    /// carried something, and its idle shrink needed the *whole* pool quiet —
    /// so a stream that outlived its connection kept a corpse placeable for
    /// the session's life. Repair is by count alone. `count = 1` makes the
    /// assertion sharp: there is no room for the replacement until the corpse
    /// has actually been removed.
    #[tokio::test]
    async fn a_dead_tunnel_holding_a_stream_is_reaped_and_repaired() {
        use std::sync::atomic::Ordering;

        let dials = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // The initial tunnel, plus the server task whose abort kills it.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (tx0, rx0) = mpsc::channel(8);
        // Keep the receiver alive: a dropped one ends the server task.
        std::mem::forget(rx0);
        let server0 = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx0));
        let (shutdown0, shutdown0_rx) = tokio::sync::watch::channel(false);
        let t0 = ClientTunnel::start(client_io, mux_config(), shutdown0_rx);

        let dials_for_dialer = std::sync::Arc::clone(&dials);
        let dialer: Dialer = std::sync::Arc::new(move |_: Carrier| {
            let dials = std::sync::Arc::clone(&dials_for_dialer);
            Box::pin(async move {
                dials.fetch_add(1, Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let (tx, rx) = mpsc::channel(8);
                // Keep the receiver: a dropped one ends the server task and
                // would make the fresh tunnel die too, which is not what this
                // test is about.
                std::mem::forget(rx);
                let server = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
                std::mem::forget(server);
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);
                Ok((tunnel, shutdown_tx))
            })
        });

        let pool = TunnelPool::with_dialer(
            Carrier::Tcp,
            "test".to_owned(),
            vec![(t0, shutdown0)],
            1,
            Some(dialer),
            std::sync::Arc::new(PinRegistry::new()),
        );

        // Hold a stream, so the tunnel is busy when its peer goes away: a
        // death is repaired whether or not the tunnel was carrying, and the
        // held stream must not keep the corpse in the pool.
        let held = pool.open_stream().await.unwrap();
        assert_eq!(pool.size(), 1);
        assert_eq!(dials.load(Ordering::Relaxed), 0);

        // The peer goes away: the driver observes the closed duplex and ends.
        server0.abort();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let dead = pool
                .inner
                .state
                .lock()
                .entries
                .iter()
                .all(|e| !e.tunnel.is_alive());
            if dead {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The tick reaps the corpse and dials exactly one replacement, whoever
        // gets there first (the pool's own task or this call).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pool.size() < 1 && std::time::Instant::now() < deadline {
            pool.maintain().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(pool.size(), 1, "the count is restored");
        assert_eq!(
            dials.load(Ordering::Relaxed),
            1,
            "exactly one replacement is dialed for one death"
        );
        assert!(
            pool.inner
                .state
                .lock()
                .entries
                .iter()
                .all(|e| e.tunnel.is_alive()),
            "the only member of the pool is the live replacement"
        );
        assert!(pool.snapshot().grows >= 1, "the repair is recorded");

        // The replacement serves visitors.
        let served = pool
            .open_stream()
            .await
            .expect("the replacement must serve the next visitor");
        drop(served);

        drop(held);
        drop(pool);
    }

    /// A tunnel that never carried a stream is replaced just the same.
    ///
    /// The old pool only raised `demand` when the dead tunnel had carried
    /// something, so an idle corpse — the common case, a carrier connection
    /// that died between visitors — stayed in the pool. Repair is by count
    /// alone, so idleness changes nothing.
    #[tokio::test]
    async fn an_idle_dead_tunnel_is_replaced_too() {
        use std::sync::atomic::Ordering;

        const COUNT: usize = 2;
        let dials = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pool = counting_pool(COUNT, COUNT, std::sync::Arc::clone(&dials));
        assert_eq!(pool.size(), COUNT);
        assert_eq!(pool.snapshot().streams(), 0, "both tunnels are idle");

        // Kill one from the pool's side: the state a peer that went away
        // leaves behind (an open that saw `Closed` marks it exactly so).
        let victim = pool
            .inner
            .state
            .lock()
            .entries
            .first()
            .map(|e| e.tunnel.clone())
            .expect("two tunnels");
        victim.mark_dead();

        // The tick reaps it and dials one replacement, up to the count.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            pool.maintain().await;
            if pool.size() >= COUNT && dials.load(Ordering::SeqCst) >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(pool.size(), COUNT, "the count is restored");
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "one idle death, one replacement"
        );
        assert!(pool.snapshot().grows >= 1, "the repair is recorded");
        assert!(
            pool.inner
                .state
                .lock()
                .entries
                .iter()
                .all(|e| e.tunnel.is_alive()),
            "every member of the pool is live again"
        );
        assert_eq!(
            pool.snapshot().tunnels.len(),
            COUNT,
            "the replacement is a live member of the pool"
        );
    }
}
