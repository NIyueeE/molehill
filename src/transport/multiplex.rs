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
/// growth rule needs it to keep every tunnel strictly below the cap, because a
/// cap hit makes the vendored engine log an unguarded `error!`.
pub(crate) fn stream_cap() -> usize {
    crate::common::constants::DEFAULT_MUX_MAX_STREAMS
}

/// Microseconds since the process's first use of the pool clock: the base the
/// reservation-age metric is measured against (a monotonic clock, so it never
/// jumps backwards).
fn now_us() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
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
// The elastic pool: bookkeeping, placement, growth and shrink.
// ---------------------------------------------------------------------------
use crate::transport::pool::{GrowReason, Placement, ShrinkReason, TunnelLoad};

/// One tunnel's bookkeeping counters, shared by the pool's placement, shrink
/// and telemetry code.
///
/// All three are atomics because placement reads them while it holds only a
/// shared borrow of the tunnel list: inserting an entry would otherwise need a
/// write lock on the hot path of every data channel.
#[derive(Debug, Default)]
pub(crate) struct TunnelCounters {
    /// Established streams. Incremented when an open completes, decremented
    /// when the caller drops the stream ([`StreamLease`]).
    streams: std::sync::atomic::AtomicUsize,
    /// Opens requested but not yet completed on this tunnel.
    pending: std::sync::atomic::AtomicUsize,
    /// When the oldest *current* reservation was taken, as micros since the
    /// process's first pool. An open that has waited longer than
    /// `OPEN_WAIT_BUDGET` is the pool's "grow now" signal (`GrowReason::Wait`).
    oldest_pending_us: std::sync::atomic::AtomicU64,
}

impl TunnelCounters {
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

    /// Charge one open to this tunnel before it is awaited. The reservation's
    /// age is what the wait rule reads, so the first of a burst sets it and
    /// the last one to clear it resets it.
    fn reserve(&self) {
        use std::sync::atomic::Ordering;
        if self.pending.fetch_add(1, Ordering::Relaxed) == 0 {
            self.oldest_pending_us.store(now_us(), Ordering::Relaxed);
        }
    }

    /// The open failed or was abandoned: the reservation comes back.
    fn release(&self) {
        use std::sync::atomic::Ordering;
        if self.pending.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.oldest_pending_us.store(0, Ordering::Relaxed);
        }
    }

    /// The open completed: the reservation becomes a live stream.
    fn charge(&self) {
        use std::sync::atomic::Ordering;
        self.streams.fetch_add(1, Ordering::Relaxed);
        if self.pending.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.oldest_pending_us.store(0, Ordering::Relaxed);
        }
    }

    /// How long the oldest reservation on this tunnel has been waiting.
    fn oldest_wait(&self) -> Option<std::time::Duration> {
        use std::sync::atomic::Ordering;
        let since = self.oldest_pending_us.load(Ordering::Relaxed);
        (since != 0).then(|| std::time::Duration::from_micros(now_us().saturating_sub(since)))
    }
}

/// One opened stream, charged to its tunnel for exactly as long as it lives.
///
/// `ClientTunnel::open_stream` hands the raw stream back already charged;
/// wrapping it keeps the charge exact, because the stream is a `Drop` point no
/// matter which task ends up holding it (a forwarded visitor, a stripe group,
/// a pre-opened channel). A tunnel's stream count can therefore never leak past
/// the stream's life, which is what the shrink predicate reads.
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
    /// `true` for a growth, `false` for a shrink.
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
    /// The cap the pool may grow to (`[client.data.tcp|kcp].max_tunnels`).
    pub max_tunnels: usize,
    /// The UDP-derived floor the pool must not shrink below (D7).
    pub udp_floor: usize,
    /// Cumulative growths since the pool was created.
    pub grows: u64,
    /// Cumulative shrinks since the pool was created.
    pub shrinks: u64,
    /// Per-tunnel `(streams, pending, pinned)`, in pool order.
    pub tunnels: Vec<(usize, usize, usize)>,
}

impl PoolSnapshot {
    /// Established streams anywhere in the pool.
    #[must_use]
    pub fn streams(&self) -> usize {
        self.tunnels.iter().map(|(s, _, _)| *s).sum()
    }

    /// The pool's pinned peers, summed over its tunnels.
    #[must_use]
    pub fn pinned(&self) -> usize {
        self.tunnels.iter().map(|(_, _, p)| *p).sum()
    }
}

/// The established tunnels of one pool plus the placement bookkeeping.
struct PoolState {
    /// The live tunnels, in pool order. An index is stable between removals
    /// and names the tunnel in the telemetry.
    entries: Vec<PoolEntry>,
    /// Round-robin cursor: the tie-break of the least-loaded rule.
    next_cursor: usize,
    /// Set while an open could not be answered immediately; the next
    /// maintenance tick turns it into a growth.
    demand: bool,
    max_tunnels: usize,
}

/// One live tunnel of a pool.
struct PoolEntry {
    tunnel: ClientTunnel,
    /// Dropping this ends the driver; the pool holds it for the tunnel's whole
    /// life so a shrink can actually release the connection.
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl PoolEntry {
    fn load(&self) -> TunnelLoad {
        self.tunnel.counters.load()
    }
}

impl PoolState {
    /// The load of every tunnel, in pool order.
    fn loads(&self) -> Vec<TunnelLoad> {
        self.entries.iter().map(PoolEntry::load).collect()
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
type DialResult = Result<(ClientTunnel, tokio::sync::watch::Sender<bool>), String>;

/// The pool's way to add a tunnel. `ClientTunnel`s carry no index; the pool
/// assigns one when it inserts the tunnel.
pub type Dialer = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = DialResult> + Send>>
        + Send
        + Sync,
>;

/// Why an open failed.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The pool has no tunnel at all (nothing established, no dialer).
    NoTunnel,
    /// Every candidate tunnel refused the stream.
    Refused(crate::mux::ConnectionError),
    /// Every tunnel is at [`crate::transport::pool::TUNNEL_STREAM_CEILING`] and
    /// the pool could not grow past it (its own `max_tunnels`, or the server's
    /// `max_tunnels_per_client` valve).
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
                "every tunnel is at its stream ceiling and the pool cannot grow"
            ),
        }
    }
}

/// The pool's shared state: what placement, the maintenance tick and the
/// opt-in telemetry tasks read.
struct PoolShared {
    carrier: Carrier,
    /// The pool's key within its session: `session` (shared) or
    /// `service:<name>`.
    key: String,
    /// Streams per tunnel (`DEFAULT_MUX_MAX_STREAMS`), the growth ceiling.
    stream_cap: usize,
    /// `[client.data].idle_timeout`.
    idle_timeout: std::time::Duration,
    /// When the pool last had work (a stream, a reserved open or a pinned
    /// peer): the shrink clock.
    last_activity: std::sync::Mutex<std::time::Instant>,
    /// The last growth: shrink waits the warm hold out from here.
    last_grown: std::sync::Mutex<std::time::Instant>,
    /// The last shrink: the next one waits the cooldown out from here.
    last_shrunk: std::sync::Mutex<std::time::Instant>,
    /// When a growth attempt last failed: the pool stops growing until
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
    /// Set while the pool is growing or shrinking, so the two cannot overlap.
    resizing: std::sync::atomic::AtomicBool,
    /// Signalled when one growth attempt ends, worked or not. A cold pool's
    /// losing opens wait on it for the tunnel the winner is dialing, instead
    /// of reserving against an empty pool.
    grown: tokio::sync::Notify,
    /// Signalled when a tunnel's stream count drops, so an open that found
    /// every tunnel at its ceiling can re-read the pool instead of failing
    /// while capacity is about to come back. Only ever waited on with a
    /// timeout (`CAPACITY_WAIT`).
    capacity_freed: tokio::sync::Notify,
    /// Dropped with the last real user of the pool: the maintenance and
    /// telemetry tasks hold only a `Weak` and exit on it.
    alive: std::sync::Arc<()>,
}

impl PoolShared {
    /// Note that the pool has work now: the shrink clock restarts.
    fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
    }

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
}

/// A pool of parallel client tunnels (arm 1 of the transport comparison:
/// N physical connections instead of one), with elastic size.
///
/// `open_stream` picks the least-loaded tunnel (by `streams + pending`), with
/// the round-robin cursor as the tie-break; a tunnel whose driver died returns
/// `Closed` immediately and the placement falls through to the next candidate,
/// so only a pool where every tunnel is dead fails an open (the control
/// channel's heartbeat then triggers the usual full reconnect, which
/// re-establishes the tunnels).
///
/// `open_streams(n)` reserves all `n` before awaiting any, so a stripe group
/// of K lands on K distinct tunnels whenever the pool has K (D24).
///
/// Clones share one state, so placement, growth and shrink see every service
/// that uses the pool — which is the whole point of a shared pool.
#[derive(Clone)]
pub struct TunnelPool {
    inner: std::sync::Arc<TunnelPoolInner>,
}

struct TunnelPoolInner {
    shared: std::sync::Arc<PoolShared>,
    state: parking_lot::Mutex<PoolState>,
    /// Creates a new tunnel for a growth. `None` on a pool built without one.
    dial: Option<Dialer>,
    /// The client's UDP pin accounting: a tunnel whose tunnel id appears here
    /// with a non-zero count is pinned and must never be shrunk (D30).
    pins: std::sync::Arc<PinRegistry>,
}

impl TunnelPool {
    /// Wrap the tunnels an establishment pass already produced, on a pool that
    /// cannot grow (no dialer).
    #[must_use]
    pub fn new(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        max_tunnels: usize,
        idle_timeout: std::time::Duration,
    ) -> TunnelPool {
        Self::with_dialer(
            carrier,
            key,
            initial,
            max_tunnels,
            idle_timeout,
            None,
            std::sync::Arc::new(PinRegistry::new()),
        )
    }

    /// The same, with the dialer a growth uses (`None` on a pool that cannot
    /// grow) and the client's UDP pin registry.
    pub fn with_dialer(
        carrier: Carrier,
        key: String,
        initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)>,
        max_tunnels: usize,
        idle_timeout: std::time::Duration,
        dial: Option<Dialer>,
        pins: std::sync::Arc<PinRegistry>,
    ) -> TunnelPool {
        let shared = std::sync::Arc::new(PoolShared {
            carrier,
            key,
            stream_cap: stream_cap(),
            idle_timeout,
            last_activity: std::sync::Mutex::new(std::time::Instant::now()),
            last_grown: std::sync::Mutex::new(std::time::Instant::now()),
            last_shrunk: std::sync::Mutex::new(std::time::Instant::now()),
            grow_failed_at: std::sync::Mutex::new(None),
            udp_floor: std::sync::atomic::AtomicUsize::new(0),
            grows: std::sync::atomic::AtomicU64::new(0),
            shrinks: std::sync::atomic::AtomicU64::new(0),
            events: std::sync::Mutex::new(Vec::new()),
            resizing: std::sync::atomic::AtomicBool::new(false),
            grown: tokio::sync::Notify::new(),
            capacity_freed: tokio::sync::Notify::new(),
            alive: std::sync::Arc::new(()),
        });
        let mut state = PoolState {
            entries: Vec::new(),
            next_cursor: 0,
            demand: false,
            max_tunnels: max_tunnels.max(1),
        };
        for (tunnel, shutdown) in initial {
            state.entries.push(PoolEntry {
                tunnel,
                _shutdown: shutdown,
            });
        }
        let pool = TunnelPool {
            inner: std::sync::Arc::new(TunnelPoolInner {
                shared,
                state: parking_lot::Mutex::new(state),
                dial,
                pins,
            }),
        };
        // The "cold + demand -> +1" rule has to be evaluated even when nothing
        // else happens, and the shrink clock needs a heartbeat: one task per
        // pool, exiting when the pool drops.
        pool.spawn_maintenance();
        pool_stats::register(&pool.inner);
        LIVE_POOLS
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::sync::Arc::downgrade(&pool.inner));
        pool
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

    /// The UDP-derived floor (D7) this pool must not shrink below.
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
            max_tunnels: 0,
            udp_floor: 0,
            grows: 0,
            shrinks: 0,
            tunnels: Vec::new(),
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
            max_tunnels: state.max_tunnels,
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
                    )
                })
                .collect(),
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
    /// A cold pool grows synchronously here, so the first open of a cold pool
    /// answers with a fresh tunnel instead of failing; every other growth
    /// happens on the maintenance tick. The reservation is charged before the
    /// first `await`, which is what makes back-to-back opens land on distinct
    /// tunnels while the pool has them.
    pub(crate) async fn open_stream(&self) -> Result<StreamLease, OpenError> {
        self.open_avoiding(&[]).await
    }

    /// Open one stream for a stripe of a group, on a tunnel the group does not
    /// already occupy (D24 structural).
    ///
    /// `used` are the process-unique tunnel ids the group's earlier stripes
    /// took and `stripes` is the group's K — the server names the group before
    /// its channels are opened, which is what lets the client do what the
    /// placement rule alone never could.
    ///
    /// The pool grows to `min(stripes, max_tunnels)` *first*: a pool with fewer
    /// tunnels than the group has stripes cannot place two of them apart
    /// however placement chooses, and a cold pool — the elastic pool's default
    /// state — has *none*, while a group's K streams sit below the growth
    /// rule's per-tunnel threshold (7 on the shipped cap), so nothing else in
    /// this path would ever have asked for a second tunnel. The growth is
    /// bounded by the group's own K and by `max_tunnels`, and a refusal (D14) or
    /// a pool that cannot grow stops it; the placement below then shares what
    /// there is.
    pub(crate) async fn open_stream_on_distinct(
        &self,
        used: &[usize],
        stripes: usize,
    ) -> Result<StreamLease, OpenError> {
        self.grow_for_stripes(stripes).await;
        self.open_avoiding(used).await
    }

    /// The body of every open: `avoid` names tunnels this request must not
    /// take while it can take another (only a stripe group passes a non-empty
    /// list — see [`Self::open_stream_on_distinct`]).
    async fn open_avoiding(&self, avoid: &[usize]) -> Result<StreamLease, OpenError> {
        if self.size() == 0 {
            self.grow(GrowReason::Cold).await;
            // The growth above is a no-op when another task already holds the
            // resize flag, so a *concurrent* first open would reserve against
            // a still-empty pool and fail (`NoTunnel`) while the winner is
            // dialing the very tunnel it needs. Wait for that attempt instead;
            // the timeout is only a backstop for a dialer that never returns.
            let _ =
                tokio::time::timeout(crate::transport::pool::COLD_GROW_WAIT, self.await_growth(1))
                    .await;
        }
        let ceiling = self.ceiling();
        // Grow *before* placing, not (only) after: a burst that arrives while
        // every tunnel already sits at the growth threshold must spread over
        // the tunnels it will use, not land all of it on one and queue its
        // interactive and control streams behind that bulk.
        self.grow_before_placing().await;
        let avoid = self.indices_of(avoid);
        let Some(reservation) = self.reserve(&[], &avoid, ceiling) else {
            // Nothing under the ceiling. An empty pool is a different failure
            // from a full one — the dialer could not bring a tunnel up — and
            // reporting it as `AtCapacity` would blame the load for the
            // network.
            if self.size() == 0 {
                return Err(OpenError::NoTunnel);
            }
            // Every tunnel carries as many streams as it may. Growth is the
            // rule's answer, so ask for it and give a retiring stream (or the
            // tick) a moment before refusing this visitor: without the wait a
            // pool that cannot grow would fail whichever open happened to
            // arrive while every tunnel sat exactly at the ceiling.
            self.demand();
            let freed = self.inner.shared.capacity_freed.notified();
            tokio::pin!(freed);
            // Register before the second look, or a retirement landing in
            // between would be lost and this open would sleep out the budget.
            freed.as_mut().enable();
            if self.reserve(&[], &avoid, ceiling).is_none() {
                let _ = tokio::time::timeout(crate::transport::pool::CAPACITY_WAIT, freed).await;
            }
            let Some(reservation) = self.reserve(&[], &avoid, ceiling) else {
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

    /// Grow one tunnel when the next open would add to a tunnel that is already
    /// at the growth threshold, so the burst spreads *before* it is placed.
    ///
    /// The maintenance tick grows too, but it samples at 50 ms intervals: a
    /// burst of K back-to-back opens (a 20-stream bulk test) completes long
    /// before the first tick sees it, so every one of them places on the same
    /// tunnel and the tick can only fix the *next* burst. Growing in the open
    /// path is what makes the spreading synchronous, at the cost of one dial
    /// for the opens that trip the rule — the same dial the cold path already
    /// pays, and never one for an open that does not need it.
    ///
    /// Every guard turns this into a no-op, and they are all conditions under
    /// which growing is wrong or already happening: a growth is in flight, a
    /// refused growth is still holding the pool back (D14), the pool is at its
    /// own `max_tunnels`, or the pool is cold (which [`Self::open_stream`]
    /// grew synchronously above). The dial is bounded by the carrier's own
    /// establish timeout, exactly like the cold path's.
    async fn grow_before_placing(&self) {
        use std::sync::atomic::Ordering;
        if self.inner.shared.resizing.load(Ordering::Acquire) || self.growth_held_off() {
            return;
        }
        let grow_at = crate::transport::pool::tunnel_grow_at(self.inner.shared.stream_cap);
        let busy = {
            let state = self.inner.state.lock();
            let size = state.entries.len();
            size > 0
                && size < state.max_tunnels
                && state.entries.iter().any(|e| e.load().total() >= grow_at)
        };
        if busy {
            self.grow(GrowReason::Load).await;
        }
    }

    /// Grow until the pool can place `target` tunnels apart, one dial at a
    /// time and never past `max_tunnels`.
    ///
    /// Every stripe group pays this once: K stripes need K tunnels to be spread
    /// over, and the pool's other growth rules are load-based, so a group whose
    /// K streams sit below the per-tunnel threshold would never trigger one
    /// (that is exactly how a cold pool ended up carrying a whole group on one
    /// tunnel). The one-at-a-time shape is the pool's own: `grow` is guarded by
    /// a resize flag, so K concurrent stripe placements take turns dialing
    /// instead of racing, and each turn is one dial rather than a storm.
    ///
    /// Terminating by construction: every iteration either returns or leaves
    /// the pool bigger, and the target is capped by `max_tunnels`. A refused
    /// growth holds the loop off for its cooldown (D14), and a pool whose dialer
    /// cannot add a tunnel returns instead of spinning.
    async fn grow_for_stripes(&self, target: usize) {
        let target = target.max(1);
        loop {
            let (size, cap) = {
                let state = self.inner.state.lock();
                (state.entries.len(), state.max_tunnels)
            };
            let target = target.min(cap);
            if size >= target || self.growth_held_off() {
                return;
            }
            self.grow(GrowReason::Stripe).await;
            // Wait for the growth in flight — someone else's as much as ours —
            // before re-reading the size: placing against the still-small pool
            // is the very thing this loop exists to avoid.
            self.await_growth(target).await;
            if self.size() <= size {
                // No tunnel was added and none is being dialed any more: the
                // pool cannot grow (no dialer, or a refusal that now holds it
                // back). The placement that follows shares what there is.
                return;
            }
        }
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
        let loads = state.loads();
        let cursor = state.next_cursor;
        state.next_cursor = cursor.wrapping_add(1);
        let order = crate::transport::pool::order_candidates(&loads, cursor);
        // The group's own tunnels first: a stripe takes one its siblings do not
        // have yet, and only when every tunnel is already part of the group (a
        // pool the group cannot spread over) does the exclusion give way — the
        // group must still forward, exactly as it did before it had a name.
        let candidates = crate::transport::pool::order_candidates_for(&order, tried, avoid);
        // The least-loaded untried tunnel with pending budget left; when every
        // candidate is at its budget, the least-loaded untried one still takes
        // the open — refusing a visitor outright is worse, and a full budget is
        // exactly the demand the growth rule reads.
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
            let Some(load) = loads.get(index) else {
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
        let load = loads.get(index).copied().unwrap_or_default();
        // The worst eligible candidate: what the placement rule gave up by not
        // picking it. S1's spread is `worst - best`.
        let worst = order
            .last()
            .and_then(|i| loads.get(*i))
            .copied()
            .unwrap_or_default();
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
    /// `TooManyStreams` means "the pool should grow", never "the tunnel is
    /// dead": it is counted as a refusal, the reservation is released, and the
    /// demand flag makes the next maintenance tick add a tunnel. The growth
    /// rule keeps every tunnel strictly below its cap precisely so this path
    /// stays rare — the vendored engine logs an unguarded `error!` on a cap
    /// hit, and `tests/log_budget_test.rs` fails a healthy run with one ERROR.
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
                self.inner.shared.touch();
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
                    // A cap hit is a growth signal, never a death: record it
                    // and stop here so the caller (and the maintenance tick)
                    // sees the demand.
                    self.demand();
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
                            self.inner.shared.touch();
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
                            if matches!(e, crate::mux::ConnectionError::TooManyStreams) {
                                self.demand();
                            } else if matches!(e, crate::mux::ConnectionError::Closed) {
                                // Another dead tunnel: same as above (D14).
                                next.tunnel.mark_dead();
                                self.release_growth_hold();
                            }
                            last = e;
                        }
                    }
                }
                self.demand();
                PlacementStats::record(placement, started.elapsed(), Some(note));
                Err(OpenError::Refused(last))
            }
        }
    }

    /// Note that an open was not answered immediately: the next maintenance
    /// tick grows the pool.
    fn demand(&self) {
        self.inner.state.lock().demand = true;
    }

    /// Add one tunnel.
    async fn grow(&self, reason: GrowReason) {
        use std::sync::atomic::Ordering;
        if self.inner.shared.resizing.swap(true, Ordering::AcqRel) {
            return;
        }
        let result = self.grow_locked(reason).await;
        self.inner.shared.resizing.store(false, Ordering::Release);
        // Whoever waited on a cold pool re-reads the size here and either
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
        let (size, max) = {
            let state = self.inner.state.lock();
            (state.entries.len(), state.max_tunnels)
        };
        if size >= max {
            return Err(format!("already at max_tunnels ({max})"));
        }
        let (tunnel, shutdown) = dial().await?;
        let mut state = self.inner.state.lock();
        if state.entries.len() >= state.max_tunnels {
            return Err("the pool reached max_tunnels while dialing".to_owned());
        }
        let from = state.entries.len();
        state.entries.push(PoolEntry {
            tunnel,
            _shutdown: shutdown,
        });
        let to = state.entries.len();
        drop(state);
        *self
            .inner
            .shared
            .last_grown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
        self.inner.shared.grows.fetch_add(1, Ordering::Relaxed);
        self.inner.shared.push_event(PoolEvent {
            grow: true,
            reason: reason.as_str(),
            from,
            to,
        });
        // Growth is a lifecycle event: one INFO line per size change.
        info!(
            pool = %self.inner.shared.key,
            carrier = self.inner.shared.carrier.as_str(),
            reason = reason.as_str(),
            from,
            to,
            "pool-stats: tunnel pool grew"
        );
        self.inner.shared.touch();
        Ok(())
    }

    /// Remove one tunnel when the whole pool is idle, unpinned and past its
    /// idle timeout. Returns whether one was removed.
    /// Remove every tunnel whose driver has ended.
    ///
    /// [`Self::shrink_if_idle`] is the pool's only other removal path, and it
    /// needs the *whole* pool quiet: one stream that outlives its connection
    /// keeps a dead tunnel — and its slot against `max_tunnels` — in the pool
    /// for the life of the session. The measured shape is a stage transition
    /// where the connection dies but its last stream is still held, so
    /// `streams > 0` blocks the idle rule indefinitely while placement keeps
    /// handing the corpse new opens, every one of which fails with `Closed`.
    ///
    /// Reaping is therefore not policy: it has no idle, warm or cooldown gate
    /// and the dead tunnel's load is irrelevant (those streams are already
    /// broken). What it owes the pool afterwards is a replacement, so a reap
    /// raises `demand` when the dead tunnel was carrying something and the
    /// next tick grows; D14's growth hold is released too, because a death is
    /// the event that makes a retry meaningful.
    fn reap_dead(&self) -> bool {
        use std::sync::atomic::Ordering;
        let (from, to) = {
            let mut state = self.inner.state.lock();
            if state.entries.iter().all(|e| e.tunnel.is_alive()) {
                return false;
            }
            let from = state.entries.len();
            let mut carried = false;
            let mut index = 0;
            while index < state.entries.len() {
                if state.entries[index].tunnel.is_alive() {
                    index += 1;
                } else {
                    // Dropping the entry drops the driver's shutdown sender:
                    // whatever the task was still doing ends here.
                    let entry = state.entries.remove(index);
                    carried |= entry.load().total() > 0;
                }
            }
            state.demand |= carried;
            (from, state.entries.len())
        };
        self.release_growth_hold();
        self.inner.shared.shrinks.fetch_add(1, Ordering::Relaxed);
        self.inner.shared.push_event(PoolEvent {
            grow: false,
            reason: ShrinkReason::Dead.as_str(),
            from,
            to,
        });
        // A death is a lifecycle event: one INFO line per size change.
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

    fn shrink_if_idle(&self) -> bool {
        use std::sync::atomic::Ordering;
        let now = std::time::Instant::now();
        {
            let state = self.inner.state.lock();
            let busy = state.entries.iter().any(|e| {
                let load = e.load();
                load.total() > 0 || self.inner.pins.pinned_on(e.tunnel.id()) > 0
            });
            if busy {
                drop(state);
                self.inner.shared.touch();
                return false;
            }
        }
        let idle = self
            .inner
            .shared
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= self.inner.shared.idle_timeout;
        let warm = self
            .inner
            .shared
            .last_grown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= crate::transport::pool::MIN_WARM_HOLD;
        let cooled = self
            .inner
            .shared
            .last_shrunk
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= crate::transport::pool::SHRINK_COOLDOWN;
        let floor = self.inner.shared.udp_floor.load(Ordering::Relaxed).max(1);
        let victim = {
            let state = self.inner.state.lock();
            let size = state.entries.len();
            if size <= floor {
                return false;
            }
            state.entries.iter().position(|e| {
                let mut load = e.load();
                // The client's own pin count is the authoritative one (the
                // server's affinity table is the same fact, seen from the
                // other end).
                load.pinned = self.inner.pins.pinned_on(e.tunnel.id());
                crate::transport::pool::may_shrink(size, floor, load, idle, warm, cooled)
            })
        };
        let Some(victim) = victim else {
            return false;
        };
        if self.inner.shared.resizing.swap(true, Ordering::AcqRel) {
            return false;
        }
        let removed = {
            let mut state = self.inner.state.lock();
            let from = state.entries.len();
            if victim >= from {
                None
            } else {
                let entry = state.entries.remove(victim);
                Some((entry, from, state.entries.len()))
            }
        };
        self.inner.shared.resizing.store(false, Ordering::Release);
        let Some((entry, from, to)) = removed else {
            return false;
        };
        // Dropping the entry drops the driver's shutdown sender: the physical
        // connection goes away with the tunnel.
        drop(entry);
        // The pool is smaller than it was, so a refused growth is worth
        // re-attempting (D14).
        self.release_growth_hold();
        *self
            .inner
            .shared
            .last_shrunk
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
        self.inner.shared.touch();
        self.inner.shared.shrinks.fetch_add(1, Ordering::Relaxed);
        self.inner.shared.push_event(PoolEvent {
            grow: false,
            reason: ShrinkReason::Idle.as_str(),
            from,
            to,
        });
        // Shrink is a lifecycle event: one INFO line per size change.
        info!(
            pool = %self.inner.shared.key,
            carrier = self.inner.shared.carrier.as_str(),
            reason = ShrinkReason::Idle.as_str(),
            from,
            to,
            "pool-stats: tunnel pool shrank"
        );
        true
    }

    /// The maintenance tick: the growth and shrink rules of
    /// [`crate::transport::pool`], evaluated for one pool.
    async fn maintain(&self) {
        // A dead tunnel leaves before any growth or shrink decision reads the
        // pool: freeing its slot is what lets the replacement be dialed in
        // this same tick. Unlike a shrink, the reap does not end the tick.
        self.reap_dead();
        if self.shrink_if_idle() {
            return;
        }
        let reason = {
            let mut state = self.inner.state.lock();
            let loads: Vec<TunnelLoad> = state.entries.iter().map(PoolEntry::load).collect();
            let size = loads.len();
            let busy = loads.iter().any(|l| l.total() > 0)
                || state
                    .entries
                    .iter()
                    .any(|e| self.inner.pins.pinned_on(e.tunnel.id()) > 0);
            let floor = self
                .inner
                .shared
                .udp_floor
                .load(std::sync::atomic::Ordering::Relaxed);
            let demand = std::mem::take(&mut state.demand);
            if size == 0 {
                // Cold: nothing to place a stream on. Grow only when a service
                // asked for one (or the UDP floor requires it).
                (demand || floor > size).then_some(GrowReason::Cold)
            } else if floor > size {
                // The UDP-derived floor is a requirement, not a suggestion.
                Some(GrowReason::UdpFloor)
            } else if demand {
                Some(GrowReason::Demand)
            } else if state.entries.iter().any(|e| {
                e.tunnel
                    .counters
                    .oldest_wait()
                    .is_some_and(|w| w > crate::transport::pool::OPEN_WAIT_BUDGET)
            }) {
                // An open that has been waiting longer than the budget means
                // the pool has no ready stream for it: grow.
                Some(GrowReason::Wait)
            } else if !busy {
                None
            } else {
                // Both questions matter, and the per-tunnel one is the stricter
                // of the two above size 1: the pool's total threshold scales
                // with its size (a tunnel of a 4-tunnel pool reaches 4x its
                // share before the total crosses), while the placement ceiling
                // does not. Growing on the per-tunnel rule is what keeps
                // `TUNNEL_STREAM_CEILING` from ever being placement's answer.
                let used: usize = loads.iter().map(|l| l.total()).sum();
                let cap = self.inner.shared.stream_cap;
                let total_busy = used > crate::transport::pool::grow_threshold(size, cap);
                let tunnel_busy = loads
                    .iter()
                    .any(|l| l.total() >= crate::transport::pool::tunnel_grow_at(cap));
                (total_busy || tunnel_busy).then_some(GrowReason::Load)
            }
        };
        // An idle pool just keeps its shrink clock running.
        let Some(reason) = reason else {
            return;
        };
        if self.growth_held_off() {
            // D14: a refused growth stops growth. The demand flag was consumed
            // above, so this tick does nothing; the next attempt waits the
            // cooldown out, or a tunnel's death releases the hold. The cold
            // path in `open_stream` is deliberately not gated — it is driven by
            // an arriving visitor, not by this tick, so it cannot become a
            // storm.
            return;
        }
        if reason == GrowReason::UdpFloor {
            // Grow straight to the floor, one tunnel at a time (each step
            // re-reads the size, so a racing shrink cannot overshoot).
            loop {
                let (size, floor, max) = {
                    let state = self.inner.state.lock();
                    (
                        state.entries.len(),
                        self.inner
                            .shared
                            .udp_floor
                            .load(std::sync::atomic::Ordering::Relaxed),
                        state.max_tunnels,
                    )
                };
                if size >= floor || size >= max {
                    break;
                }
                if self.grow_locked(GrowReason::UdpFloor).await.is_err() {
                    break;
                }
            }
            return;
        }
        self.grow(reason).await;
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
            .map(|(streams, pending, pinned)| format!("{streams}/{pending}/{pinned}"))
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
            max_tunnels = snapshot.max_tunnels,
            udp_floor = snapshot.udp_floor,
            streams = snapshot.streams(),
            pinned = snapshot.pinned(),
            tunnels = %tunnels.join(","),
            timeline = %if timeline.is_empty() { "-".to_owned() } else { timeline.join(" ") },
            grows = snapshot.grows,
            shrinks = snapshot.shrinks,
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
            counters: std::sync::Arc::new(TunnelCounters::default()),
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

    /// A pool over `n` in-memory tunnels. The caller keeps the shutdown
    /// senders; the pool drops its own copy of each tunnel's when it removes
    /// the tunnel.
    fn duplex_pool(n: usize, max_tunnels: usize) -> (TunnelPool, Vec<mpsc::Receiver<MuxStream>>) {
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
                ClientTunnel::start(client_io, mux_config(), shutdown_rx),
                tokio::sync::watch::channel(false).0,
            ));
            rxs.push(rx);
        }
        let pool = TunnelPool::new(
            Carrier::Tcp,
            "test".to_owned(),
            tunnels,
            max_tunnels,
            std::time::Duration::from_secs(60),
        );
        (pool, rxs)
    }

    /// One in-memory tunnel whose server side drains every stream it is sent:
    /// the unit these pool fixtures are built from, and the unit a *growth*
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

    /// A pool that starts with `n` in-memory tunnels and can grow up to
    /// `max_tunnels`: every growth dials one more.
    fn growable_pool(n: usize, max_tunnels: usize) -> TunnelPool {
        let dial: Dialer = std::sync::Arc::new(|| {
            Box::pin(async move { Ok((duplex_tunnel(), tokio::sync::watch::channel(false).0)) })
        });
        let initial: Vec<(ClientTunnel, tokio::sync::watch::Sender<bool>)> = (0..n)
            .map(|_| (duplex_tunnel(), tokio::sync::watch::channel(false).0))
            .collect();
        TunnelPool::with_dialer(
            Carrier::Tcp,
            "stripes".to_owned(),
            initial,
            max_tunnels,
            std::time::Duration::from_secs(60),
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        )
    }

    /// The same, with the dialer counted: how many times the pool grew, as a
    /// number a test can assert on.
    fn counting_pool(
        n: usize,
        max_tunnels: usize,
        dials: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> TunnelPool {
        let dial: Dialer = std::sync::Arc::new(move || {
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
            max_tunnels,
            std::time::Duration::from_secs(60),
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

    /// The eager stripe growth's price, which HANDOFF recorded as a cost with
    /// no test behind it: the *first* group to meet a pool with fewer tunnels
    /// than the group has stripes dials the difference, and it is `K-1` extra
    /// dials on a pool that already has one — the cold dial a plain visitor
    /// would have paid anyway is the K-th.
    ///
    /// Exact, because the dialer is this test's: between the opens below, only
    /// the pool's own growth rules can call it, and the count is what they
    /// cost. It would catch the guarantee being dropped (the group then places
    /// on the tunnels that exist, and no dial happens) and the growth being
    /// paid per *stripe request* instead of once per pool (K² dials).
    #[tokio::test]
    async fn a_stripe_group_dials_a_cold_pool_up_to_its_stripe_count_once() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        // A cold pool: no tunnels at all, the elastic pool's default state.
        let cold_dials = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(0, 4, Arc::clone(&cold_dials));
        assert_eq!(pool.size(), 0, "the pool starts cold");

        let used = open_one_stripe_group(&pool, 4).await;
        assert_eq!(
            cold_dials.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "a cold pool must dial once per stripe: K dials, of which K-1 are the group's own"
        );
        assert_eq!(
            pool.snapshot().grows,
            4,
            "the same number through the counter the telemetry and the suite read"
        );
        assert_eq!(pool.size(), 4, "the group grew the pool to its own count");
        assert_eq!(
            used.iter().collect::<std::collections::HashSet<_>>().len(),
            4,
            "the four stripes must be on four distinct tunnels: {used:?}"
        );

        // A second group of the same pool: warm for `idle_timeout`, so the
        // growth is once per pool, not once per group.
        let used2 = open_one_stripe_group(&pool, 4).await;
        assert_eq!(
            cold_dials.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "a warm pool's second group must dial nothing"
        );
        assert_eq!(
            used2.iter().collect::<std::collections::HashSet<_>>().len(),
            4,
            "the second group must also be spread over four tunnels: {used2:?}"
        );

        // The same growth on a pool that already has one tunnel: three dials,
        // which is the K-1 every operator pays on the first striped visitor.
        let warm_dials = Arc::new(AtomicUsize::new(0));
        let warm = counting_pool(1, 4, Arc::clone(&warm_dials));
        let used3 = open_one_stripe_group(&warm, 4).await;
        assert_eq!(
            warm_dials.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "one existing tunnel plus a group of K costs K-1 dials"
        );
        assert_eq!(
            used3.iter().collect::<std::collections::HashSet<_>>().len(),
            4,
            "K-1 dials must have left K tunnels to spread over: {used3:?}"
        );
    }

    /// D14's other half: a refused growth *stops* growth. The maintenance tick
    /// runs every 50 ms, so without the hold a client at the server's
    /// `max_tunnels_per_client` cap would dial-and-be-refused twenty times a
    /// second against the server's accept path.
    #[tokio::test]
    async fn a_refused_growth_holds_the_pool_back() {
        // No dialer: every growth attempt fails, which is the shape the
        // server's tunnel refusal takes at the pool.
        let (pool, _rxs) = duplex_pool(0, 4);
        assert!(!pool.growth_held_off(), "nothing has failed yet");

        pool.grow(GrowReason::Cold).await;
        assert!(
            pool.growth_held_off(),
            "a failed growth must hold growth off"
        );

        // With demand behind it, the tick still does not dial again.
        pool.demand();
        pool.maintain().await;
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
        let (pool, _rxs) = duplex_pool(4, 8);
        let mut streams = Vec::new();
        for _ in 0..3 {
            streams.push(pool.open_stream().await.expect("an open"));
        }
        let used: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _)| *streams)
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
        drop(streams);
    }

    /// More opens than tunnels still all produce a stream: the pool reuses
    /// what it has (and the maintenance rule grows it when the demand is real)
    /// instead of failing a visitor.
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
            .map(|(streams, _, _)| *streams)
            .collect();
        assert_eq!(used, vec![2, 2], "the pool reuses both tunnels: {used:?}");
        drop(streams);
    }

    /// D24 structural, at the pool's own level: a stripe group's K opens land
    /// on K *distinct* tunnels even from a pool that has fewer — one, here,
    /// which is the state a cold pool reaches after its first visitor. The
    /// group-aware open grows the pool to its K first and then avoids the
    /// tunnels its earlier stripes took; the load rule alone could not have
    /// done it, because K concurrent streams sit below the growth threshold.
    #[tokio::test]
    async fn a_stripe_group_grows_the_pool_and_spreads_over_distinct_tunnels() {
        const STRIPES: usize = 3;
        let pool = growable_pool(1, 4);
        let mut used: Vec<usize> = Vec::new();
        let mut streams = Vec::new();
        for _ in 0..STRIPES {
            let lease = pool
                .open_stream_on_distinct(&used, STRIPES)
                .await
                .expect("a stripe open");
            used.push(lease.tunnel_id());
            streams.push(lease);
        }
        assert_eq!(
            pool.size(),
            STRIPES,
            "the group must have grown the pool to one tunnel per stripe"
        );
        let distinct: std::collections::HashSet<usize> = used.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            STRIPES,
            "{STRIPES} stripes must occupy {STRIPES} distinct tunnels: {used:?}"
        );
        let per_tunnel: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _)| *streams)
            .collect();
        assert_eq!(
            per_tunnel,
            vec![1; STRIPES],
            "one stripe per tunnel, none doubled up: {per_tunnel:?}"
        );
        drop(streams);
    }

    /// Correctness first: a group the pool cannot spread over still opens
    /// every stripe. `max_tunnels` is the cap, so the third and fourth stripe
    /// reuse the two tunnels — exactly the behaviour a striped visitor had
    /// before the group had a name, and the reason the exclusion is a
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
        assert_eq!(pool.size(), 2, "the cap bounds the growth");
        let distinct: std::collections::HashSet<usize> = used.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            2,
            "the group can only spread as far as the cap allows: {used:?}"
        );
        let per_tunnel: Vec<usize> = pool
            .snapshot()
            .tunnels
            .iter()
            .map(|(streams, _, _)| *streams)
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

    /// A cold pool grows on the first open when it has a dialer, instead of
    /// refusing it: `max_tunnels` is the cap, and the growth respects it.
    #[tokio::test]
    async fn a_cold_pool_grows_up_to_its_cap() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let dial: Dialer = std::sync::Arc::new(move || {
            let counter = std::sync::Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let (tx, rx) = mpsc::channel(8);
                std::mem::forget(rx);
                let server = tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
                std::mem::forget(server);
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                std::mem::forget(shutdown_tx);
                Ok((
                    ClientTunnel::start(client_io, mux_config(), shutdown_rx),
                    tokio::sync::watch::channel(false).0,
                ))
            })
        });
        let pool = TunnelPool::with_dialer(
            Carrier::Tcp,
            "cold".to_owned(),
            Vec::new(),
            2,
            std::time::Duration::from_secs(60),
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        );
        assert_eq!(pool.size(), 0);
        let first = pool.open_stream().await.expect("the cold pool must grow");
        assert_eq!(pool.size(), 1, "one open grows the pool by one tunnel");
        assert_eq!(pool.snapshot().grows, 1);
        // The cap: even a second cold open cannot push it past `max_tunnels`.
        drop(first);
        let second = pool.open_stream().await.expect("the pool still serves");
        assert!(pool.size() <= 2, "the cap is max_tunnels");
        drop(second);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            pool.size(),
            "the dialer is called once per tunnel"
        );
    }

    /// A burst spreads *while it is placed*, not on the next maintenance tick.
    ///
    /// The rule this pins: an open that would add to a tunnel already at the
    /// growth threshold grows first, so a K-open burst (a 20-stream bulk test)
    /// lands on the tunnels it will use instead of stacking all of it on one —
    /// the head-of-line blocking that queue a shared tunnel's interactive and
    /// control streams behind the bulk. The maintenance tick would fix the
    /// *next* burst 50 ms later; this makes the first one spread too.
    ///
    /// Falsified by reverting to place-then-grow: the whole burst stacks on
    /// the one cold tunnel (10 streams on one tunnel against a threshold of
    /// 7), which is the shape the two-stage rate reproduction measured.
    #[tokio::test]
    async fn a_burst_spreads_over_tunnels_while_it_is_placed() {
        const OPENS: usize = 10;
        // One live duplex tunnel to start from, its inbound drained so every
        // stream the pool opens is accepted by the tunnel's server side.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (tx, mut first_rx) = mpsc::channel::<MuxStream>(8);
        tokio::spawn(async move { while first_rx.recv().await.is_some() {} });
        tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        // A dialer that mints a live duplex tunnel per call: the burst's growth
        // is a real dial, drained so the tunnel can accept every stream.
        let dial: Dialer = std::sync::Arc::new(move || {
            let counter = std::sync::Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let (tx, mut rx) = mpsc::channel::<MuxStream>(8);
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                tokio::spawn(run_server_tunnel(server_io, mux_config(), tx));
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                std::mem::forget(shutdown_tx);
                Ok((
                    ClientTunnel::start(client_io, mux_config(), shutdown_rx),
                    tokio::sync::watch::channel(false).0,
                ))
            })
        });
        let pool = TunnelPool::with_dialer(
            Carrier::Tcp,
            "burst".to_owned(),
            vec![(
                ClientTunnel::start(client_io, mux_config(), shutdown_rx),
                tokio::sync::watch::channel(false).0,
            )],
            4,
            std::time::Duration::from_secs(60),
            Some(dial),
            std::sync::Arc::new(PinRegistry::new()),
        );

        let mut live = Vec::new();
        for _ in 0..OPENS {
            live.push(pool.open_stream().await.expect("burst open"));
        }
        // Read immediately: the maintenance tick (50 ms) would grow the pool
        // anyway, so a *late* read cannot distinguish the two rules.
        let snap = pool.snapshot();
        let streams: Vec<usize> = snap.tunnels.iter().map(|(s, _, _)| *s).collect();
        assert!(
            snap.size >= 2,
            "the burst must grow the pool while it is placed, not on the next tick: {streams:?}"
        );
        assert!(
            streams.iter().copied().max().unwrap_or(0) < OPENS,
            "no tunnel may carry the whole burst: {streams:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            snap.size - 1,
            "one dial per tunnel the burst added"
        );
        drop(live);
    }

    /// The client's pin count is what gates the shrink (D30): a tunnel with a
    /// pinned peer is not removable even when it has no streams.
    #[tokio::test]
    async fn a_pinned_tunnel_is_not_shrinkable() {
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
        // depend on which tunnel the placement picked.
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

        let pool = TunnelPool::new(
            Carrier::Tcp,
            "test".to_owned(),
            tunnels,
            4,
            std::time::Duration::from_secs(60),
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
                .map(|(s, _, _)| *s)
                .collect::<Vec<_>>(),
            vec![OPENS / TUNNELS; TUNNELS],
            "the least-loaded rule must spread four opens evenly over two tunnels"
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
            assert_eq!(count, OPENS / TUNNELS, "streams were not spread evenly");
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
            4,
            std::time::Duration::from_secs(60),
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

    /// A tunnel whose connection dies must leave the pool even while it still
    /// "holds" a stream, and the pool must dial a replacement.
    ///
    /// `shrink_if_idle` requires the *whole* pool to be quiet, so before the
    /// reap one stream that outlives its connection kept a corpse placeable
    /// for the session's life. `max_tunnels = 1` makes the assertion sharp:
    /// while the corpse occupies the only slot there is no room for a
    /// replacement, so an open after the death can only succeed if the dead
    /// tunnel was actually removed.
    #[tokio::test]
    async fn a_dead_tunnel_is_reaped_and_replaced() {
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
        let dialer: Dialer = std::sync::Arc::new(move || {
            let dials = std::sync::Arc::clone(&dials_for_dialer);
            Box::pin(async move {
                dials.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            1, // no room for a replacement until the dead one is gone
            std::time::Duration::from_secs(60),
            Some(dialer),
            std::sync::Arc::new(PinRegistry::new()),
        );

        // Hold a stream, so the tunnel is not "idle" and `may_shrink` can
        // never be the path that removes it.
        let held = pool.open_stream().await.unwrap();
        assert_eq!(pool.size(), 1);
        assert_eq!(dials.load(std::sync::atomic::Ordering::Relaxed), 0);

        // The peer goes away: the driver observes the closed duplex and ends.
        server0.abort();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut served = false;
        while std::time::Instant::now() < deadline {
            if pool.open_stream().await.is_ok() {
                served = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(served, "the pool never replaced the dead tunnel");
        assert!(
            dials.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the replacement must be dialed, not conjured"
        );
        assert!(
            pool.snapshot().shrinks >= 1,
            "a reap is a recorded size change"
        );

        drop(held);
        drop(pool);
    }
}
