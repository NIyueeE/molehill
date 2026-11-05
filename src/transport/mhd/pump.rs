//! The socket pump: one task per connection that moves datagrams between the
//! state machine and the UDP socket.
//!
//! It is deliberately dumb. Every pass first drains whatever the connection
//! wants to send, then waits for input; the wait has a deadline, because the
//! connection has timers of its own (a re-send, a keepalive, a dead path) and a
//! pump that only woke on a socket would never fire them. A readiness-driven
//! pump belongs with the pool attachment, where the wake-up path is designed
//! once for both.
//!
//! Two shapes exist: a dialing lane owns its socket and both directions here;
//! an accepted lane's socket is shared by the listener's dispatcher, which
//! routes arriving datagrams by connection id, so its pump only sends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket;

use super::conn::Conn;
use super::lane::{Shared, StreamIo};
use super::recovery::Expiry;
use super::wire::ResetCode;

/// Bind the local socket of a connection.
pub(crate) async fn bind() -> std::io::Result<Arc<UdpSocket>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    tune_socket(&socket);
    Ok(Arc::new(socket))
}

/// The kernel buffer a lane asks for: 32 MiB absorbs the largest burst of a
/// wide-area window, and the request is clamped by the system's
/// `rmem_max`/`wmem_max` anyway. Failures are non-fatal and quiet: the window
/// still bounds what can be in flight, and a debug line is enough for a host
/// that would rather keep its default.
fn tune_socket(socket: &UdpSocket) {
    let s = socket2::SockRef::from(socket);
    if let Err(e) = s.set_recv_buffer_size(32 * 1024 * 1024) {
        tracing::debug!("mhd socket kept its default receive buffer: {e}");
    }
    if let Err(e) = s.set_send_buffer_size(32 * 1024 * 1024) {
        tracing::debug!("mhd socket kept its default send buffer: {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) =
        nix::sys::socket::setsockopt(socket, nix::sys::socket::sockopt::UdpGroSegment, &true)
    {
        // Growable receive is an optimisation, not a requirement: a socket
        // that refuses it keeps one datagram per recvmsg.
        tracing::debug!("mhd socket refused GRO: {e}");
    }
}

/// How many datagrams one pass hands the kernel at once.
const SEND_RUN: usize = 44;

/// How many connections a listener may hold before it stops admitting new
/// ones.
///
/// The carrier has no handshake, so a datagram naming an unknown id allocates
/// state. An off-path flood must not grow the table without bound: the cap is
/// generous for a real deployment and the refusal is silent (the sender of a
/// bogus id learns nothing) except for one debug line.
const MAX_CONNECTIONS: usize = 4096;

/// Put one pass's datagrams on the wire.
///
/// The state machine answers one packet per call, so the pass collects a run and
/// hands it over whole where the platform can segment it: one syscall for the
/// run, and the NIC cuts it, which is the amortisation TCP gets from TSO without
/// leaving the socket API.
#[cfg(target_os = "linux")]
async fn send_run(
    socket: &UdpSocket,
    peer: SocketAddr,
    run: &[u8],
    spans: &[(usize, usize)],
) -> std::io::Result<()> {
    use crate::transport::dgram::Span;
    use crate::transport::udp_batch::SendBatch;
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;

    let all: Vec<Span> = spans
        .iter()
        .map(|(off, len)| Span::Staged {
            off: *off,
            len: *len,
        })
        .collect();
    let mut at = 0;
    while at < all.len() {
        socket.writable().await?;
        let chunk = &all[at..];
        let outcome = socket.try_io(Interest::WRITABLE, || {
            // A run GSO cannot take, or a kernel without it, falls back to the
            // batched syscall for the same datagrams.
            match SendBatch::send_gso(socket.as_raw_fd(), peer, run, chunk) {
                Ok(0) | Err(_) => SendBatch::send_spans(socket.as_raw_fd(), peer, run, chunk),
                Ok(n) => Ok(n),
            }
        });
        match outcome {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "the socket accepted none of the datagrams",
                ));
            }
            // A partial `sendmmsg` is normal under memory pressure: the rest of
            // the run goes in the next pass, never as a silent loss.
            Ok(sent) => at += sent,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The same, one datagram at a time (see the Linux version for the batched one).
#[cfg(not(target_os = "linux"))]
async fn send_run(
    socket: &UdpSocket,
    peer: SocketAddr,
    run: &[u8],
    spans: &[(usize, usize)],
) -> std::io::Result<()> {
    for (off, len) in spans {
        socket.send_to(&run[*off..*off + *len], peer).await?;
    }
    Ok(())
}

/// The staging buffers one pump reuses between passes.
///
/// A pass stages a whole run — up to 45 KiB with the batch this carrier sends —
/// and allocating it per pass is a 64 KiB allocation, a page-faulting first
/// touch and a free, once per run. The buffers belong to the pump, so the run is
/// written into warm memory.
#[derive(Default)]
struct Staging {
    /// The run: every packet of this pass, laid end to end.
    run: Vec<u8>,
    /// Each packet's (offset, length) inside `run`.
    spans: Vec<(usize, usize)>,
}

impl Staging {
    fn new() -> Self {
        Self {
            run: Vec::with_capacity(SEND_RUN * super::wire::MAX_DATAGRAM),
            spans: Vec::with_capacity(SEND_RUN),
        }
    }

    fn clear(&mut self) {
        self.run.clear();
        self.spans.clear();
    }
}

/// Drain what the connection wants to say, in as few syscalls as the platform
/// allows.
///
/// One run per call on purpose: a flush that emptied a multi-megabyte send
/// buffer before reading its socket let acknowledgements pile up behind the
/// sends, and the first ack read afterwards jumped hundreds of packet numbers —
/// every one of them then classified lost, and the connection re-sent megabytes
/// the peer already had. The caller alternates a run with a non-blocking ack
/// drain, so send and acknowledgement progress together.
/// Once a second, the window itself: whether a lane that has fallen behind a
/// clean path is growing or sitting on one.
///
/// `slow_start` latches off at the first loss and never returns, so a lane read
/// late in a shaping schedule is in congestion avoidance by construction, and the
/// two numbers beside this flag are what say whether that is why its throughput is
/// flat. The lane id is part of the line because four connections report
/// independently: without it the four series interleave and read as one that
/// oscillates.
fn report_window(shared: &Arc<Shared>) {
    if !crate::transport::stats::enabled() {
        return;
    }
    shared.with(|conn| {
        let now = Instant::now();
        if now.duration_since(conn.window_reported_at()).as_secs() < 1 {
            return;
        }
        conn.note_window_reported(now);
        let (cwnd, flight, slow_start, acked_since_growth) = conn.window_state();
        let ceiling = conn.window_ceiling();
        // The *receive* side of the same picture, once a second. The send side has
        // had a line since this file began and the receive side had none, so a
        // stream whose bytes arrived and whose reader never took them was
        // invisible: the stall line needs a *sender* with bytes it cannot post,
        // and in the wedge the sender is fine — the reader is not. `held` is what
        // the peer's credit paid for and nobody consumed, and `ready_ms` is how
        // long the oldest of it has been waiting.
        let backlog = conn.backlog();
        let (worst_stream, worst_ready_ms) = backlog
            .worst
            .and_then(|(id, _)| conn.ready_for(id, now).map(|waited| (id, waited)))
            .map_or((0, 0), |(id, waited)| {
                (id, u64::try_from(waited.as_millis()).unwrap_or(u64::MAX))
            });
        // The stream that is furthest behind, once a second: at a stall the lane's
        // own numbers are healthy — nothing unsent, nothing unacknowledged — and the
        // question is which stream stopped and where its cursor sits. The stall line
        // carries this too, but it only fires on a *sender* with bytes it cannot
        // post, and the sealed striped stall has neither.
        let worst = conn.worst_stream();
        tracing::debug!(
            worst_stream = worst.map_or(0, |w| w.id),
            worst_next = worst.map_or(0, |w| w.next),
            worst_written = worst.map_or(0, |w| w.written),
            worst_unsent = worst.map_or(0, |w| w.unsent),
            worst_retransmits = worst.map_or(0, |w| w.retransmits),
            lane = conn.wire_id(),
            cwnd,
            flight,
            slow_start,
            acked_since_growth,
            backlog_streams = backlog.streams,
            backlog_bytes = backlog.total,
            worst_stream,
            worst_ready_ms,
            // The ceiling the window is actually allowed to reach, and the
            // round trip it is derived from: a queue that inflates the estimate
            // would raise its own ceiling, and that feedback has to be visible
            // in the same line as the number it moves.
            ceiling,
            srtt_us = conn
                .smoothed_rtt()
                .map_or(0, |rtt| u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX)),
            "mhd window"
        );
    });
}

async fn flush(
    socket: &UdpSocket,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    staging: &mut Staging,
) -> std::io::Result<usize> {
    let Staging { run, spans } = staging;
    let build = crate::transport::stats::Timing::start(crate::transport::stats::enabled());
    while spans.len() < SEND_RUN {
        let now = Instant::now();
        let off = run.len();
        let sampled =
            crate::transport::stats::enabled() && crate::transport::stats::PHASES.timing();
        let call = crate::transport::stats::Timing::start(sampled);
        let filled = shared.with(|conn| conn.poll_transmit(now, run));
        call.stop(&crate::transport::stats::PHASES.tx_call);
        let Some(len) = filled else {
            break;
        };
        spans.push((off, len));
    }
    build.stop(&crate::transport::stats::PHASES.tx_build);
    if spans.is_empty() {
        // Nothing was built. Two causes look identical from outside and are
        // fixed in different places: the state machine refused every packet (a
        // budget), or the pump never ran (a lost wake-up or a starved task).
        // This line is the discriminator, and it is gated on the switch.
        if crate::transport::stats::enabled() {
            let refused =
                shared.with(|conn| conn.refusal_now().map(|snap| (conn.connection_id(), snap)));
            // Only the real alarm: a pass that ran and *refused to send bytes it
            // was holding*. A snapshot taken after the pass raced a writer
            // landing immediately behind it, and that case is not a defect —
            // which is why the condition is on the queued count taken with the
            // same lock, not on "something was queued a moment ago".
            if let Some((id, snap)) = refused
                && snap.queued > 0
            {
                tracing::debug!(
                    id,
                    reason = snap.reason,
                    visited = snap.visited,
                    open = snap.open,
                    queued = snap.queued,
                    pending_fins = snap.fins,
                    pending_resets = snap.resets,
                    ack_pending = snap.ack,
                    credit = snap.credit,
                    window = snap.window,
                    buffer = snap.buffer,
                    cursor = snap.cursor,
                    witness = ?snap.witness,
                    "mhd pump built nothing while bytes were queued"
                );
            }
        }
        return Ok(0);
    }
    let sent = spans.len();
    let posting = crate::transport::stats::Timing::start(crate::transport::stats::enabled());
    // `send_run` waits on `socket.writable()` before each chunk it posts, and
    // that wait is the one place a pass can block for an unbounded time: a lane
    // whose bulk stream has filled the socket's send buffer makes every later
    // packet — an opening stream's handshake included — queue behind it. Nothing
    // else in the pump charges that time, so it is measured here.
    let sampled = crate::transport::stats::enabled();
    let before_wait = Instant::now();
    send_run(socket, peer, run, spans).await?;
    // The whole distribution, not only the extreme: a ten-millisecond post is
    // invisible to a hundred-millisecond bar, and ten milliseconds is the same
    // order as the arrival latency this pump is being measured for. Every pass
    // contributes one sample, gated on the switch.
    let posted = before_wait.elapsed();
    crate::transport::stats::note_post(posted);
    if sampled && posted >= std::time::Duration::from_millis(100) {
        let blocked = socket
            .try_io(tokio::io::Interest::WRITABLE, || Ok(()))
            .is_err();
        tracing::debug!(
            posted_us = u64::try_from(posted.as_micros()).unwrap_or(u64::MAX),
            datagrams = sent,
            still_blocked = blocked,
            "mhd pump waited on the socket"
        );
    }
    posting.stop(&crate::transport::stats::PHASES.tx_send);
    crate::transport::stats::sent_run(sent, run.len());
    report_window(shared);
    if let Some(stall) = shared.with(|conn| conn.stall_report(Instant::now())) {
        let credit_blocked = shared.with(|conn| conn.credit_blocked());
        tracing::debug!(
            unsent_ms = stall.unsent_ms,
            unacked_ms = stall.unacked_ms,
            credit_blocked,
            backlog_streams = stall.backlog.streams,
            backlog_worst_stream = stall.backlog.worst.map_or(0, |(id, _)| id),
            backlog_worst_bytes = stall.backlog.worst.map_or(0, |(_, held)| held),
            backlog_bytes = stall.backlog.total,
            worst_stream = stall.worst.map_or(0, |w| w.id),
            worst_stream_unsent = stall.worst.map_or(0, |w| w.unsent),
            worst_stream_retransmits = stall.worst.map_or(0, |w| w.retransmits),
            worst_stream_next = stall.worst.map_or(0, |w| w.next),
            worst_stream_written = stall.worst.map_or(0, |w| w.written),
            worst_stream_reset = stall.worst.is_some_and(|w| w.reset),
            worst_repair_at = stall.worst.and_then(|w| w.repair).map_or(0, |(at, _)| at),
            worst_repair_len = stall.worst.and_then(|w| w.repair).map_or(0, |(_, len)| len),
            "mhd stalled: bytes waiting"
        );
    }
    for _ in 0..sent {
        shared.note_sent();
    }
    // Wake the readers on every pass, not only when a receive batch lands.
    // A reader that missed its wake-up is otherwise parked until the pump's own
    // timer — and the chain measured on the soak is exactly that: a client
    // reader holding a megabyte unread, its advertised credit therefore frozen,
    // the server blocked on `credit_blocked` with sixteen megabytes queued, the
    // forwarding connections never finishing, and the pool filling to its
    // ceiling until it refuses a visitor. `wake_readers` on an empty list is a
    // `mem::take` of nothing, so a pass that has nothing to wake costs a branch.
    shared.wake_readers();
    Ok(sent)
}

/// Read whatever the socket has right now, without waiting for readiness.
///
/// The dialing pump alternates this with its sends, so an acknowledgement is
/// applied before the next run is built. A server-side lane does not use it:
/// its socket's receive side belongs to the listener's dispatcher.
#[cfg(target_os = "linux")]
fn drain_ready(
    socket: &UdpSocket,
    batch: &mut crate::transport::udp_batch::RecvBatch,
    shared: &Arc<Shared>,
    peer: SocketAddr,
) {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    let outcome = socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd()));
    match outcome {
        Ok(_) => {
            let now = Instant::now();
            let mut datagrams = 0usize;
            let mut bytes = 0usize;
            for (from, data) in batch.iter() {
                if from != peer {
                    continue;
                }
                datagrams += 1;
                bytes += data.len();
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, data);
                });
            }
            if crate::transport::stats::enabled() && datagrams > 0 {
                crate::transport::stats::received_run(datagrams, bytes);
            }
            wake_touched(shared);
            shared.wake_writers();
            shared.wake_streams();
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(_) => {}
    }
}

/// The same, one datagram at a time.
#[cfg(not(target_os = "linux"))]
fn drain_ready(socket: &UdpSocket, _batch: &mut (), shared: &Arc<Shared>, peer: SocketAddr) {
    loop {
        let mut buf = [0u8; 65536];
        match socket.try_recv_from(&mut buf) {
            Ok((len, from)) => {
                if from != peer {
                    continue;
                }
                let now = Instant::now();
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, &buf[..len]);
                });
                wake_touched(shared);
                shared.wake_writers();
                shared.wake_streams();
            }
            Err(_) => return,
        }
    }
}

/// The receive side: one batched read, split back into the datagrams it
/// carried.
#[cfg(target_os = "linux")]
async fn receive(
    socket: &UdpSocket,
    batch: &mut crate::transport::udp_batch::RecvBatch,
    mut on_datagram: impl FnMut(std::net::SocketAddr, &[u8]),
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    socket.readable().await?;
    match socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd())) {
        Ok(_) => {
            let mut datagrams = 0usize;
            let mut bytes = 0usize;
            for (from, data) in batch.iter() {
                datagrams += 1;
                bytes += data.len();
                on_datagram(from, data);
            }
            if crate::transport::stats::enabled() {
                crate::transport::stats::received_run(datagrams, bytes);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e),
    }
}

/// The same, one datagram at a time.
#[cfg(not(target_os = "linux"))]
async fn receive(
    socket: &UdpSocket,
    _batch: &mut (),
    mut on_datagram: impl FnMut(std::net::SocketAddr, &[u8]),
) -> std::io::Result<()> {
    let mut buf = [0u8; 65536];
    let (len, from) = socket.recv_from(&mut buf).await?;
    on_datagram(from, &buf[..len]);
    Ok(())
}

/// Why the pump's wait ended.
enum Wake {
    /// Something new is queued: go round and flush it.
    Signal,
    /// The connection's timer fired: run its expiry.
    Timer,
    /// The connection ended.
    Closed,
}

/// Wait for whichever of the three reasons to look again comes first.
async fn wait(shared: &Arc<Shared>) -> Wake {
    let deadline = shared.with(|conn| conn.poll_timeout());
    let timer = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = shared.pump_signal() => Wake::Signal,
        () = shared.closed() => Wake::Closed,
        () = timer => Wake::Timer,
    }
}

/// Start the pump for one connection that owns its socket.
pub(crate) fn spawn(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let mut batch = crate::transport::udp_batch::RecvBatch::new(SEND_RUN, 2048);
        #[cfg(not(target_os = "linux"))]
        let mut batch = ();
        let mut staging = Staging::new();
        // Once a second the pump asks the question a reader cannot answer about
        // itself: are there streams with bytes waiting and nobody polling them?
        let mut starve_check = std::time::Instant::now();
        loop {
            staging.clear();
            if starve_check.elapsed() >= std::time::Duration::from_secs(1) {
                starve_check = std::time::Instant::now();
                let now = std::time::Instant::now();
                let n = shared
                    .with(|conn| conn.starved_streams(now, std::time::Duration::from_secs(1)));
                crate::transport::stats::note_starved_streams(n);
            }
            let sent = match flush(&socket, peer, &shared, &mut staging).await {
                Ok(sent) => sent,
                Err(e) => {
                    tracing::debug!("mhd pump stopped: {e}");
                    shared.close(ResetCode::Aborted);
                    return;
                }
            };
            if sent > 0 {
                drain_ready(&socket, &mut batch, &shared, peer);
            }
            if shared.is_closed() {
                shared.finish_close();
                return;
            }
            if sent == SEND_RUN {
                // More is queued: send it, but only after the drain above had
                // its chance to apply the peer's acknowledgements, and after a
                // yield so a peer task (the server's dispatcher) can run.
                tokio::task::yield_now().await;
                continue;
            }
            let arriving = Arc::clone(&shared);
            tokio::select! {
                result = receive(&socket, &mut batch, |from, data| {
                    if from != peer {
                        return;
                    }
                    let now = Instant::now();
                    arriving.with(|conn| {
                        let _ = conn.on_datagram(now, data);
                    });
                }) => {
                    if result.is_err() {
                        shared.close(ResetCode::Aborted);
                        return;
                    }
                    // Once per batch, never once per datagram: a wake-up
                    // schedules the reader task, so waking it for each of a
                    // batch's thirty-two datagrams is thirty-two scheduler
                    // calls to hand out the same bytes.
                    wake_touched(&shared);
                    shared.wake_writers();
                    shared.wake_streams();
                }
                keep = wait(&shared) => {
                    match keep {
                        Wake::Signal => {}
                        Wake::Closed => {
                            // One last pass so a queued reset or FIN leaves
                            // before the socket does; a failed send ends the
                            // lane anyway.
                            staging.clear();
                            let _ = flush(&socket, peer, &shared, &mut staging).await;
                            shared.finish_close();
                            return;
                        }
                        Wake::Timer => {
                            let now = Instant::now();
                            let (expiry, _) = shared.with(|conn| conn.on_timeout(now));
                            if expiry == Expiry::Dead {
                                shared.close(ResetCode::Aborted);
                            }
                            shared.wake_readers();
                            shared.wake_writers();
                        }
                    }
                }
            }
        }
    })
}

/// A pump that only sends: the listener owns the socket's receive side and
/// routes each datagram to the connection it names, so an accepted lane must not
/// read from it as well.
fn spawn_sender(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut staging = Staging::new();
        loop {
            staging.clear();
            let sent = match flush(&socket, peer, &shared, &mut staging).await {
                Ok(sent) => sent,
                Err(e) => {
                    tracing::debug!("mhd sender stopped: {e}");
                    shared.close(ResetCode::Aborted);
                    return;
                }
            };
            if sent == SEND_RUN {
                // More is queued; the listener's dispatcher keeps the
                // acknowledgements flowing while this pass sends, but give the
                // runtime a chance to run it.
                tokio::task::yield_now().await;
                continue;
            }
            match wait(&shared).await {
                Wake::Signal => {}
                Wake::Closed => {
                    staging.clear();
                    let _ = flush(&socket, peer, &shared, &mut staging).await;
                    shared.finish_close();
                    return;
                }
                Wake::Timer => {
                    let now = Instant::now();
                    let (expiry, _) = shared.with(|conn| conn.on_timeout(now));
                    if expiry == Expiry::Dead {
                        shared.close(ResetCode::Aborted);
                    }
                    shared.wake_readers();
                    shared.wake_writers();
                }
            }
        }
    })
}

/// Wake the readers of the streams a batch touched, and nobody else.
///
/// A lane carries several streams and a batch usually delivers bytes for one of
/// them, so waking every parked reader schedules the rest to find nothing. The
/// connection records which streams changed on its receive path; this drains that
/// and wakes one reader per id. A stream that was missed by some future change
/// still has the pump's timer as its net, which is where the two-second parks in
/// the current numbers come from anyway.
fn wake_touched(shared: &Arc<Shared>) {
    for id in shared.with(super::conn::Conn::take_woken) {
        shared.wake_stream(id);
    }
}

/// One connection as the pool holds it: a shared state machine, the counters
/// its streams are charged to, and the socket its pump sends on.
pub(crate) struct MhdLane {
    id: usize,
    shared: Arc<Shared>,
    counters: Arc<crate::transport::multiplex::TunnelCounters>,
    /// The Noise keys a stream opened on this lane is wrapped in, when the
    /// service's data plane is encrypted. Per stream, not per lane: every
    /// application stream is its own record stream, so no stream can read
    /// another's bytes and a reordering never desynchronises a counter.
    noise: Option<crate::config::NoiseConfig>,
    /// The session nonce every stream's hello names.
    nonce: crate::protocol::Digest,
    /// Whether this lane's first stream was acknowledged, which is the moment
    /// the server reserved the lane's slot. From then on a stream's hello and
    /// its acknowledgement can ride the handshake's two messages instead of
    /// costing a second round trip: the server no longer has to decide anything
    /// before it replies, because the slot is already held.
    ///
    /// Only the client's own view. The server answers either way — a payload in
    /// the opening message is answered in the reply, an absent one is answered
    /// with a record as before — so the two views cannot disagree into a
    /// deadlock; a lane whose first open failed after the server reserved just
    /// falls back to the two-flight shape.
    registered: std::sync::atomic::AtomicBool,
    /// The root material this lane's first stream's handshake produced.
    ///
    /// Recorded here until the first stream is acknowledged, then installed on
    /// the connection (see `seal_from_now`): a stream sealed while its own
    /// handshake is still in flight is ciphertext the peer cannot open.
    lane_hash: std::sync::Mutex<Option<super::cipher::Secret>>,
}

impl std::fmt::Debug for MhdLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MhdLane")
            .field("id", &self.id)
            .field("alive", &(!self.shared.is_closed()))
            .finish_non_exhaustive()
    }
}

/// Dropping the lane ends its connection once what is queued has left.
///
/// The pump task holds only the shared state (and the socket), so without this
/// a removed lane would leave a live socket and a task behind for ever; the
/// graceful close lets a queued reset reach the peer first.
impl Drop for MhdLane {
    fn drop(&mut self) {
        self.shared.begin_close(ResetCode::Aborted);
    }
}

impl MhdLane {
    /// The lane's process-unique id.
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    /// Dial a connection to a listening endpoint, without proving it accepted.
    ///
    /// A direct data channel uses this: its own hello is the proof, and paying
    /// a throwaway stream for a second one would add a round trip to every
    /// visitor.
    pub(crate) async fn dial(
        peer: SocketAddr,
        id: usize,
        noise: Option<crate::config::NoiseConfig>,
        nonce: crate::protocol::Digest,
    ) -> std::io::Result<Arc<Self>> {
        let socket = bind().await?;
        let shared = Shared::new(
            Conn::new(u32::try_from(id).unwrap_or(1), true),
            crate::transport::health::SessionHealth::new(),
        );
        let task = spawn(Arc::clone(&socket), peer, Arc::clone(&shared));
        let registered = std::sync::atomic::AtomicBool::new(false);
        // The task owns the socket and the lane; when the lane drops, the
        // connection closes and the task's next pass ends it.
        std::mem::forget(task);
        Ok(Arc::new(Self {
            id,
            counters: crate::transport::multiplex::TunnelCounters::for_lane_with(Some(Arc::clone(
                &shared.health,
            ))),
            shared,
            noise,
            nonce,
            registered,
            lane_hash: std::sync::Mutex::new(None),
        }))
    }

    /// Dial a session and prove the peer accepted it.
    ///
    /// The proof is one stream that completes the tunnel hello and is then let
    /// go: the server reserves the lane's slot on that hello, so a pool never
    /// holds a lane the valve refused.
    pub(crate) async fn connect(
        peer: SocketAddr,
        id: usize,
        noise: Option<crate::config::NoiseConfig>,
        nonce: crate::protocol::Digest,
    ) -> anyhow::Result<Arc<Self>> {
        use tokio::io::AsyncWriteExt;
        let lane = Self::dial(peer, id, noise, nonce).await?;
        let mut stream = crate::transport::multiplex::PoolLane::open_stream(&*lane)
            .await
            .map_err(|e| anyhow::anyhow!("the mhd lane refused its opening stream: {e}"))?;
        stream.shutdown().await?;
        Ok(lane)
    }

    /// Open one application stream: the transport selector, the optional Noise
    /// handshake, and nothing above that.
    ///
    /// The caller owns what the stream means (a tunnel hello, a direct data
    /// channel's hello); the lane owns the carrier's part of the opening.
    pub(crate) async fn open_channel(
        &self,
    ) -> Result<crate::transport::multiplex::CarrierStream, crate::transport::multiplex::LaneError>
    {
        Ok(self.open_channel_with_id().await?.1)
    }

    /// The same, answering the stream's id too.
    ///
    /// `open_stream` needs the id to read that stream's own retransmit count when
    /// an open turns out slow (see `stream_retransmits`), and the id is minted
    /// here.
    async fn open_channel_with_id(
        &self,
    ) -> Result<
        (u32, crate::transport::multiplex::CarrierStream),
        crate::transport::multiplex::LaneError,
    > {
        use tokio::io::AsyncWriteExt;
        if self.shared.is_closed() {
            return Err(crate::transport::multiplex::LaneError::Closed);
        }
        let id = self.shared.with(|conn| conn.open().id);
        self.shared.wake_pump();
        let plain: crate::transport::multiplex::CarrierStream =
            Box::new(StreamIo::new(Arc::clone(&self.shared), id));
        // A lane that has a root seals every later stream with a key derived from
        // it: no handshake, so the only bytes before the hello are one selector
        // byte the connection itself protects.
        if self.shared.has_lane_root() {
            let mut plain = plain;
            plain
                .write_all(&[crate::protocol::DERIVED_SELECTOR])
                .await
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            plain
                .flush()
                .await
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            return Ok((id, plain));
        }
        match &self.noise {
            #[cfg(feature = "noise")]
            Some(config) => {
                let keys = crate::transport::NoiseKeys::from_config(config)
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                let wrapped = keys
                    .wrap_initiator(plain)
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                self.remember_secret(id, wrapped.session_roots());
                Ok((id, Box::new(wrapped)))
            }
            // Without the `noise` feature `noise` is always None, so this arm
            // is the only one the compiler needs there.
            _ => {
                let mut plain = plain;
                plain
                    .write_all(&[crate::protocol::PLAIN_SELECTOR])
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                plain
                    .flush()
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                Ok((id, plain))
            }
        }
    }

    /// Open a stream whose opening message carries `payload`, answering the
    /// stream and the payload the peer's reply carried.
    ///
    /// One round trip instead of two (see
    /// [`crate::transport::NoiseStream::handshake_carrying`]).
    async fn open_channel_carrying(
        &self,
        payload: &[u8],
    ) -> Result<
        (u32, crate::transport::multiplex::CarrierStream, Vec<u8>),
        crate::transport::multiplex::LaneError,
    > {
        if self.shared.is_closed() {
            return Err(crate::transport::multiplex::LaneError::Closed);
        }
        let id = self.shared.with(|conn| conn.open().id);
        self.shared.wake_pump();
        let plain: crate::transport::multiplex::CarrierStream =
            Box::new(StreamIo::new(Arc::clone(&self.shared), id));
        match &self.noise {
            #[cfg(feature = "noise")]
            Some(config) => {
                let keys = crate::transport::NoiseKeys::from_config(config)
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                let (wrapped, theirs) = keys
                    .wrap_initiator_carrying(plain, payload)
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                self.remember_secret(id, wrapped.session_roots());
                Ok((id, Box::new(wrapped), theirs))
            }
            _ => Err(crate::transport::multiplex::LaneError::Closed),
        }
    }

    /// Keep the lane's root material, the first time a handshake completes.
    ///
    /// Recorded, not acted on: the root is installed by `open_stream` once the
    /// first stream has been **acknowledged**, because a stream sealed while its
    /// own handshake is still in flight would be ciphertext the peer cannot open
    /// — it has no root until that handshake returns.
    fn remember_secret(&self, id: u32, secret: super::cipher::Secret) {
        // **Stream 1 only.** Two opens that race would each complete a handshake
        // with its own transcript, and the two ends could pick different ones —
        // then one seals with a key the other cannot derive. Pinning the root to
        // the lane's first stream makes the choice agree by construction: both
        // ends know which stream it is.
        if id != 1 {
            return;
        }
        if let Ok(mut slot) = self.lane_hash.lock()
            && slot.is_none()
        {
            *slot = Some(secret);
            tracing::info!(
                lane = self.id,
                transcript = %hex::encode(&secret.1[..4]),
                "mhd lane handshake completed"
            );
        }
    }

    /// Install the root once it is safe to seal: the first stream is done.
    ///
    /// Gated on [`super::sealing_enabled`], which is off by default: with sealing
    /// on, a four-stripe group deadlocks about half the time (see that function).
    /// Recording the secret above is unconditional and free — it is what the
    /// switch's own experiment reads — and only the *installation* is gated, so
    /// turning the switch on cannot leave half a lane sealed.
    fn seal_from_now(&self) {
        if !super::sealing_enabled() {
            return;
        }
        if let Ok(slot) = self.lane_hash.lock()
            && let Some(secret) = slot.as_ref()
        {
            self.shared.set_lane_secret(secret);
        }
    }

    /// Wait for a stream the peer opens.
    ///
    /// The dispatcher signals the connection when a datagram opens a stream, so
    /// this parks on that signal rather than polling the queue. A connection
    /// that ends wakes it with `None`.
    pub(crate) async fn wait_stream(&self) -> Option<StreamIo> {
        loop {
            if let Some(stream) = self.accept_stream() {
                return Some(stream);
            }
            tokio::select! {
                () = self.shared.closed() => return None,
                () = self.shared.stream_added() => {}
            }
        }
    }

    /// Take a stream the peer opened, if one is waiting.
    ///
    /// A hundred milliseconds is fifty times the round trip a healthy path here
    /// measures, so a stream that waited longer than that for its own accept is a
    /// stall, and the number says whether it is the carrier's delivery or this
    /// task's scheduling: the age is measured from the datagram that named the
    /// stream.
    pub(crate) fn accept_stream(&self) -> Option<StreamIo> {
        let (id, age) = self.shared.with(|conn| {
            let now = Instant::now();
            conn.accept_aged(now).map(|(handle, age)| (handle.id, age))
        })?;
        // The whole distribution, not only the hundred-millisecond tail. This is
        // the interval between the datagram that named the stream and the loop
        // that takes it — the first of the two hops an arrival pays that no
        // counter used to see, and the one a task-spawn delay would show up in.
        crate::transport::stats::note_accept_age(age);
        if age >= std::time::Duration::from_millis(100) {
            tracing::debug!(
                lane = self.id,
                stream = id,
                accepted_us = u64::try_from(age.as_micros()).unwrap_or(u64::MAX),
                "mhd stream waited for its accept"
            );
        }
        Some(StreamIo::new(Arc::clone(&self.shared), id))
    }
}

#[async_trait::async_trait]
impl crate::transport::multiplex::PoolLane for MhdLane {
    fn id(&self) -> usize {
        self.id
    }

    fn counters(&self) -> &Arc<crate::transport::multiplex::TunnelCounters> {
        &self.counters
    }

    fn is_alive(&self) -> bool {
        !self.shared.is_closed()
    }

    fn mark_dead(&self) {
        self.shared.begin_close(ResetCode::Aborted);
    }

    async fn open_stream(
        &self,
    ) -> Result<crate::transport::multiplex::CarrierStream, crate::transport::multiplex::LaneError>
    {
        use tokio::io::AsyncWriteExt;
        // One clock read per stream open, and the only number that says whether
        // a visitor's first byte waited on this carrier: `open_channel` is the
        // Noise handshake, and the hello and its acknowledgement follow it.
        let opening = Instant::now();
        // The opening's own phases, because the whole of it says only "late":
        // a slow Noise handshake, a slow hello and a slow acknowledgement are
        // three different waits (see `MIN_RTO` for the timer one).
        let hello = crate::protocol::Hello::DataChannelTunnelHello(
            crate::protocol::CURRENT_PROTO_VERSION,
            self.nonce,
        );
        let hello = postcard::to_stdvec(&hello)
            .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
        // A lane whose slot the server already holds takes one round trip: the
        // hello rides the handshake's opening message and the acknowledgement
        // rides the reply. The lane's first stream cannot, because the server
        // has to reserve before it can answer — so it pays the two flights, and
        // this end learns from it that the slot is held.
        // Only the encrypted data plane has a carrying handshake: a plain opening
        // writes its selector byte and nothing else, so there is no first message
        // to put a hello in and the two flights are the only shape it has.
        // A sealed lane carries nothing: there is no handshake to put a hello in,
        // and a stream that took this branch would be wrapped in Noise on one side
        // and sealed on the other.
        let carries = self.noise.is_some()
            && self.registered.load(std::sync::atomic::Ordering::Relaxed)
            && !self.shared.has_lane_root();
        // `hs_at` is stamped **after** the channel is open and **before** the
        // hello goes out, so the two numbers below are the two waits a visitor
        // can pay and not the three they look like: `hs_us` is the handshake
        // (which, in the carrying shape, already contains the hello) and `ack_us`
        // is the acknowledgement. Stamped before the branch instead — as this was
        // first written — `hs_us` measured the preparation and read a flat `1`,
        // which cost a round of believing the handshake was free.
        let (stream_id, stream, ack, hs_us) = if carries {
            let (id, stream, theirs) = self.open_channel_carrying(&hello).await?;
            let hs = u64::try_from(opening.elapsed().as_micros()).unwrap_or(u64::MAX);
            let ack = postcard::from_bytes::<crate::protocol::Ack>(&theirs)
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            (id, stream, ack, hs)
        } else {
            let (id, mut stream) = self.open_channel_with_id().await?;
            let hs = u64::try_from(opening.elapsed().as_micros()).unwrap_or(u64::MAX);
            stream
                .write_all(&hello)
                .await
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            stream
                .flush()
                .await
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            let ack = crate::protocol::read_ack(&mut stream)
                .await
                .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
            (id, stream, ack, hs)
        };
        let ack_at = Instant::now();
        // The acknowledgement's wait: from the hello going out to its answer —
        // or, in the carrying shape, from the handshake returning to the reply's
        // payload being parsed.
        let ack_us = u64::try_from(ack_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        // The server reserved this lane's slot on this open, so the next one can
        // carry its hello in the handshake's opening message and be answered in
        // the reply: one round trip instead of two.
        if matches!(ack, crate::protocol::Ack::Ok) {
            self.registered
                .store(true, std::sync::atomic::Ordering::Relaxed);
            // The server installs its root before it writes this acknowledgement,
            // so by the time it arrives both ends hold the same value and every
            // later stream can be sealed.
            self.seal_from_now();
        }
        match ack {
            crate::protocol::Ack::Ok => {
                let us = u64::try_from(opening.elapsed().as_micros()).unwrap_or(u64::MAX);
                crate::transport::stats::note_open_handshake(hs_us);
                tracing::debug!(
                    lane = self.id,
                    us,
                    hs_us,
                    ack_us,
                    "mhd stream opened and acknowledged"
                );
                // A hundred milliseconds is fifty times the round trip a healthy
                // path here measures, so an open past it is a stall and worth the
                // four numbers that tell its four causes apart (`Conn::budget`).
                // Five milliseconds behind the stats switch, the same scale as the
                // server's two halves of this wait. At a hundred milliseconds this line
                // reported nothing on a cell whose tail is twenty: the instrument was
                // four times coarser than the defect it was written to find.
                //
                // `info!`, like the two server-side lines and for the same reason:
                // the configuration page promises all three at `info` while the switch
                // is set, and a `debug!` line is invisible in a release run's default
                // filter — which is precisely the run whose SLO failure needs it.
                if us >= 5_000 && crate::transport::stats::enabled() {
                    let (credit, window, buffer, queued, retransmits) = self.shared.with(|conn| {
                        let (credit, window, buffer, queued) = conn.budget();
                        (
                            credit,
                            window,
                            buffer,
                            queued,
                            conn.stream_retransmits(stream_id),
                        )
                    });
                    // The split matters as much as the total: a wait in the
                    // handshake, a wait for the acknowledgement and a wait on the
                    // budgets are three different defects, and the line below only
                    // says which *budget* was short — which is the answer to a
                    // question this line assumed had already been asked.
                    tracing::info!(
                        lane = self.id,
                        us,
                        hs_us = hs_us,
                        ack_us = ack_us,
                        credit,
                        window,
                        buffer,
                        queued_streams = queued,
                        retransmits,
                        "mhd slow stream open: what the write budgets had left"
                    );
                }
                Ok(stream)
            }
            _ => Err(crate::transport::multiplex::LaneError::Closed),
        }
    }
}

/// The listener's own cadence, for the switch that reports the carrier's cost.
///
/// The receive side had no instrument at all: a datagram the dispatcher has not
/// taken yet is invisible to every counter the connection keeps, so a stall
/// measured on the *sender's* side could sit unexplained while the receiver was
/// the one not being fed. One line a second, and the number that matters is the
/// longest gap between batches — that is the whole of what a waiting reader pays.
#[derive(Default)]
struct Cadence {
    batches: u64,
    last: Option<Instant>,
    max_gap: std::time::Duration,
    reported_at: Option<Instant>,
}

/// Fold one batch into the listener's cadence, printing once a second.
fn report_dispatch(cadence: &mut Cadence) {
    let now = Instant::now();
    if let Some(last) = cadence.last {
        let gap = now.saturating_duration_since(last);
        if gap > cadence.max_gap {
            cadence.max_gap = gap;
        }
    }
    cadence.last = Some(now);
    cadence.batches += 1;
    // The clock starts on the first batch that ever arrives, not on the first one
    // that prints: initialising it inside the print branch leaves it `None` for
    // ever, `since == now`, and the line never appears.
    let since = *cadence.reported_at.get_or_insert(now);
    if !crate::transport::stats::enabled() || now.duration_since(since).as_secs() < 1 {
        return;
    }
    cadence.reported_at = Some(now);
    tracing::debug!(
        batches = cadence.batches,
        max_gap_us = u64::try_from(cadence.max_gap.as_micros()).unwrap_or(u64::MAX),
        "mhd listener cadence"
    );
    cadence.batches = 0;
    cadence.max_gap = std::time::Duration::ZERO;
}

/// The listener's cap on live connections: see [`MAX_CONNECTIONS`].
///
/// A refused datagram is dropped without a reply: the carrier must not become
/// an amplifier, and the peer that guesses no id learns nothing.
static CONNECTION_TABLE_FULL: crate::logging::RepeatNotice = crate::logging::RepeatNotice::new();

fn admit(registry: &mut std::collections::HashMap<u32, Arc<Shared>>, id: u32) -> bool {
    if registry.len() < MAX_CONNECTIONS {
        return true;
    }
    CONNECTION_TABLE_FULL.report(
        || {
            tracing::warn!(
                "mhd listener refused a new connection: its table holds {MAX_CONNECTIONS} and \
                 the cap is full. Further refusals are logged at debug level."
            );
        },
        || tracing::debug!("mhd listener refused connection {id}: table full"),
    );
    false
}

/// The listening end: one socket per core, one connection per id it has never
/// seen, and one dispatcher per socket that routes.
pub(crate) struct MhdListener {
    addr: SocketAddr,
    incoming: tokio::sync::mpsc::UnboundedReceiver<Arc<MhdLane>>,
    /// Every connection the listener admitted, so shutdown can end them.
    ///
    /// An accepted lane's sender pump holds a clone of a *listening* socket;
    /// leaving one alive after the listener stops keeps the port bound (and,
    /// under `SO_REUSEPORT`, lets a restarted server's traffic land on the
    /// dead one). Closing each connection releases its socket.
    registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>>,
    /// Held so a dropped listener closes its sockets: the dispatcher tasks
    /// each hold one socket, and a listener nobody polls has no other way to
    /// release the port.
    _sockets: Vec<Arc<UdpSocket>>,
    /// Stops the dispatchers when the listener goes away.
    stop: tokio::sync::watch::Sender<bool>,
    _next_id: Arc<std::sync::atomic::AtomicUsize>,
}

/// How many listening sockets one acceptor binds.
fn listener_socket_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, 4)
}

/// One non-blocking UDP socket bound with `SO_REUSEPORT`, so the kernel can spread
/// arriving 4-tuples over several sockets instead of one.
fn shared_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    let socket = UdpSocket::from_std(std_socket)?;
    tune_socket(&socket);
    Ok(socket)
}

/// Bind the sockets this listener dispatches from: the first takes the port, the
/// rest share it. Without `SO_REUSEPORT` the address cannot be bound twice and the
/// fallback is one socket, which is what the carrier had before.
fn bind_listener_sockets(addr: SocketAddr) -> std::io::Result<Vec<Arc<UdpSocket>>> {
    let first = shared_socket(addr)?;
    let bound = first.local_addr()?;
    let want = listener_socket_count();
    let mut sockets = vec![Arc::new(first)];
    for _ in 1..want {
        match shared_socket(bound) {
            Ok(socket) => sockets.push(Arc::new(socket)),
            Err(_) => break,
        }
    }
    Ok(sockets)
}

/// One socket's dispatcher: read a run, route what it carried.
///
/// It stops on `stop`, which is what lets a restarted server bind the port
/// alone: a dispatcher that outlived its listener would keep a `SO_REUSEPORT`
/// socket bound and take a share of the next server's datagrams.
fn spawn_dispatcher(
    socket: Arc<UdpSocket>,
    registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>>,
    tx: tokio::sync::mpsc::UnboundedSender<Arc<MhdLane>>,
    next_id: Arc<std::sync::atomic::AtomicUsize>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    #[cfg(target_os = "linux")]
    let mut batch = crate::transport::udp_batch::RecvBatch::new(16, 2048);
    #[cfg(not(target_os = "linux"))]
    let mut batch = ();
    tokio::spawn(async move {
        let mut cadence = Cadence::default();
        loop {
            // One wake per connection per batch. Waking inside the per-datagram
            // path would schedule the same reader and the same sender once per
            // datagram, and a batch is thirty-two of them.
            let mut touched: Vec<Arc<Shared>> = Vec::new();
            let read = tokio::select! {
                changed = stop.changed() => {
                    let _ = changed;
                    return;
                }
                read = receive(&socket, &mut batch, |from, datagram| {
                    let Ok(packet) = crate::transport::mhd::wire::decode(datagram) else {
                        return;
                    };
                    // **A lane is only created by a datagram that could be opening
                    // one.** The wire format carries no binding before the handshake, so
                    // "a packet whose id it has never seen starts a connection" admits
                    // whatever happens to decode: a stray datagram gets a `Shared`, a
                    // `Conn`, a sender task and a window, unauthenticated, bounded only
                    // by `MAX_CONNECTIONS`. Measured as 29 such lanes in one
                    // seven-minute run — ids scattered across the whole `u32` range
                    // where the client's own minter is sequential — and they show up in
                    // every per-lane instrument as lanes that are not lanes.
                    //
                    // A real client's first datagram is a stream at offset zero carrying
                    // its selector byte, so requiring exactly that costs a real
                    // connection nothing and makes a stray one free to drop.
                    let opening = matches!(
                        packet.frame,
                        crate::transport::mhd::wire::Frame::Stream { offset: 0, data, .. }
                            if !data.is_empty()
                    );
                    let now = Instant::now();
                    {
                        let existing = registry.lock().get(&packet.id).cloned();
                        if let Some(shared) = existing {
                            if let Err(reason) = shared.with(|conn| conn.on_datagram(now, datagram))
                            {
                                tracing::debug!("mhd listener dropped a datagram: {reason:?}");
                            }
                            if !touched.iter().any(|seen| Arc::ptr_eq(seen, &shared)) {
                                touched.push(Arc::clone(&shared));
                            }
                            return;
                        }
                    }
                    if !opening {
                        return;
                    }
                    {
                        // How much of the *receive callback* goes into
                        // admitting a lane: the registry lock, the admission
                        // table, a `Conn`, a `Shared`, a spawned sender task and
                        // two insertions. All of it runs before the next `recv`
                        // of this batch, so bulk traffic and a new lane's first
                        // packet share one task's critical section — which is the
                        // last candidate for the handshake delay the slow-open
                        // population shows below the retransmission floor.
                        let admitting = Instant::now();
                        let mut table = registry.lock();
                        if !admit(&mut table, packet.id) {
                            return;
                        }
                        let shared = Shared::new(
                            Conn::new(packet.id, false),
                            crate::transport::health::SessionHealth::new(),
                        );
                        shared.with(|conn| {
                            let _ = conn.on_datagram(now, datagram);
                        });
                        table.insert(packet.id, Arc::clone(&shared));
                        drop(table);
                        let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let task = spawn_sender(Arc::clone(&socket), from, Arc::clone(&shared));
                        std::mem::forget(task);
                        let lane = Arc::new(MhdLane {
                            id,
                            counters: crate::transport::multiplex::TunnelCounters::for_lane_with(
                                Some(Arc::clone(&shared.health)),
                            ),
                            shared,
                            noise: None,
                            nonce: [0u8; crate::protocol::HASH_WIDTH_IN_BYTES],
                            registered: std::sync::atomic::AtomicBool::new(false),
                            lane_hash: std::sync::Mutex::new(None),
                        });
                        // The lane's owner is gone (the listener dropped): the
                        // dispatcher keeps routing for the connections it
                        // already has, and the listener's `stop` ends it. The
                        // lane itself is dropped here, which closes its
                        // connection.
                        let _ = tx.send(lane);
                        crate::transport::stats::note_lane_admit(admitting.elapsed());
                    }
                }) => read,
            };
            if read.is_err() {
                return;
            }
            // The receive side had no instrument at all, which is why a stall
            // measured entirely on the *sender's* side could sit unexplained: a
            // datagram the dispatcher has not taken yet is invisible to every
            // counter the connection keeps. One line a second says whether this
            // task is being fed — the gap between batches is the whole of what a
            // waiting reader can be paying.
            report_dispatch(&mut cadence);
            for shared in touched.drain(..) {
                wake_touched(&shared);
                shared.wake_writers();
                shared.wake_pump();
                shared.wake_streams();
            }
            // A socket that always has data would keep this task ready on every
            // await and starve the connection pumps, whose acknowledgements are
            // what let the senders forget the data. Yield once per batch so an
            // ack goes out well inside the window it describes.
            tokio::task::yield_now().await;
        }
    });
}

impl MhdListener {
    /// Bind the listener and start one dispatcher per socket.
    pub(crate) fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let sockets = bind_listener_sockets(addr)?;
        let bound = sockets[0].local_addr()?;
        let (tx, incoming) = tokio::sync::mpsc::unbounded_channel();
        let registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let next_id = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        for socket in &sockets {
            spawn_dispatcher(
                Arc::clone(socket),
                Arc::clone(&registry),
                tx.clone(),
                Arc::clone(&next_id),
                stop_rx.clone(),
            );
        }
        Ok(Self {
            addr: bound,
            incoming,
            registry,
            _sockets: sockets,
            stop,
            _next_id: next_id,
        })
    }

    /// The address it bound.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// End every connection this listener admitted and release its sockets.
    ///
    /// Called when the listener stops serving: a lane that outlived shutdown
    /// would hold a listening socket bound, and the restarted server would
    /// share the port with a connection table nobody reads. The dispatchers are
    /// stopped with the lanes — leaving one reading would keep a `SO_REUSEPORT`
    /// socket bound and take a share of the next server's datagrams.
    pub(crate) fn close_all(&self) {
        let _ = self.stop.send(true);
        let mut table = self.registry.lock();
        for shared in table.values() {
            shared.close(ResetCode::Aborted);
        }
        table.clear();
    }

    /// The next connection a peer opens.
    ///
    /// A datagram naming an id this end has never seen starts the connection:
    /// there is no handshake to wait for, which is the point of the carrier.
    pub(crate) async fn accept(&mut self) -> Option<Arc<MhdLane>> {
        self.incoming.recv().await
    }
}

/// A listener that is dropped without an explicit shutdown still releases its
/// port: the dispatchers are told to stop, and the sockets follow when they do.
impl Drop for MhdListener {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod lane_tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    #![expect(
        clippy::panic,
        reason = "a stalled round trip is exactly what these tests exist to \
                  catch, and a panic names the step that stalled"
    )]
    use super::*;
    use crate::transport::multiplex::PoolLane;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The transport seam: a dialing lane opens one stream and a listening lane
    /// reads it back, over real UDP sockets.
    ///
    /// The stream here is a bare channel, not a pool open: a pool open carries
    /// the tunnel hello and waits for the server's ack, which only the server
    /// speaks (the integration suite covers that path).
    /// A datagram that is not opening a stream does not create a lane.
    ///
    /// The listener's rule used to be "a decodable packet with an unseen id starts a
    /// connection", which admits whatever happens to decode: measured as 29 stray lanes
    /// in one seven-minute run, with ids scattered across the whole `u32` range where the
    /// client's own minter is sequential. They cost a `Shared`, a `Conn`, a sender task
    /// and a window each, unauthenticated, and they appear in every per-lane instrument as
    /// lanes that are not lanes.
    #[tokio::test]
    async fn a_datagram_that_is_not_opening_a_stream_creates_no_lane() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let from: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = Arc::new(tokio::net::UdpSocket::bind(from).await.unwrap());
        socket.connect(bound).await.unwrap();

        // A decodable packet with an id nobody has used, carrying no stream bytes: the
        // shape a stray datagram takes.
        let mut out = Vec::new();
        let written = crate::transport::mhd::wire::encode_control(
            &mut out,
            0xDEAD_BEEF,
            0,
            Some(crate::transport::mhd::wire::Ack::default()),
            crate::transport::mhd::wire::Frame::None,
        );
        assert!(written.is_some(), "the stray packet must be encodable");
        socket.send(&out).await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "a datagram that opens nothing must not become a connection"
        );
    }

    #[tokio::test]
    async fn a_lane_opens_a_stream_the_listener_accepts() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 3, None, [7u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();

        let mut stream = dialer.open_channel().await.unwrap();
        tokio::spawn(async move {
            stream.write_all(b"lane to lane").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let waiting = tokio::time::timeout(Duration::from_secs(5), accepted.wait_stream()).await;
        let Some(Some(mut inbound)) = waiting.ok() else {
            unreachable!("the listener must accept the stream")
        };
        let mut got = Vec::new();
        let selector = tokio::time::timeout(Duration::from_secs(5), inbound.read_u8())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selector, crate::protocol::PLAIN_SELECTOR);
        let read =
            tokio::time::timeout(Duration::from_secs(5), inbound.read_to_end(&mut got)).await;
        let Ok(Ok(_)) = read else {
            unreachable!("the read must finish: got {got:?}")
        };
        assert_eq!(got, b"lane to lane");
        assert!(PoolLane::is_alive(&*accepted));
    }

    /// A yamux session over one mhd stream carries four independent streams and
    /// loses no byte of them.
    ///
    /// This is the seam the multiplexed-lane experiment depended on: four data
    /// channels, each a multiplexed stream over one carrier stream, a megabyte
    /// each, and a count the peer has to confirm. It exists because that
    /// experiment lost frames in the integration suite (four readers ending at
    /// sequences 150, 176, 195 and 201 while 202 were sent), and the first
    /// question is whether the loss is here or in the pool above it.
    #[cfg(feature = "multiplex")]
    #[tokio::test]
    async fn a_multiplexed_stream_carries_four_streams_without_losing_bytes() {
        use crate::transport::multiplex::{ClientTunnel, mux_config, run_server_tunnel};

        const PAYLOAD: u64 = 1024 * 1024;
        const STREAMS: usize = 4;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(
            bound,
            21,
            None,
            [11u8; crate::protocol::HASH_WIDTH_IN_BYTES],
        )
        .await
        .unwrap();

        // The server end: read the selector, hand the stream to a yamux session,
        // and count what each inbound stream carried before answering with the
        // count. Counting rather than echoing keeps the test one-directional, so a
        // failure names the direction that lost bytes.
        let server = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut inbound = accepted.wait_stream().await.unwrap();
            assert_eq!(
                inbound.read_u8().await.unwrap(),
                crate::protocol::PLAIN_SELECTOR
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(run_server_tunnel(inbound, mux_config(), tx));
            while let Some(stream) = rx.recv().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = tokio::io::split(stream);
                    let mut buf = vec![0u8; 16 * 1024];
                    let mut total: u64 = 0;
                    loop {
                        match r.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => total += n as u64,
                            Err(e) => {
                                // A reset loses the tail exactly the way a
                                // truncation would; naming it is what tells the
                                // two apart.
                                eprintln!("server stream ended with {e:?} after {total} bytes");
                                break;
                            }
                        }
                    }
                    let _ = w.write_all(&total.to_be_bytes()).await;
                    let _ = w.flush().await;
                });
            }
        });

        let stream = dialer.open_channel().await.unwrap();
        let (_shutdown, ended) = tokio::sync::watch::channel(false);
        let tunnel = ClientTunnel::start(stream, mux_config(), ended);

        let mut senders = Vec::with_capacity(STREAMS);
        for index in 0..STREAMS {
            let tunnel = tunnel.clone();
            senders.push(tokio::spawn(async move {
                let mut stream = tunnel.open_stream().await.unwrap();
                let chunk = vec![u8::try_from(index).unwrap_or(0); 16 * 1024];
                let mut sent = 0u64;
                while sent < PAYLOAD {
                    let take = usize::try_from((PAYLOAD - sent).min(chunk.len() as u64)).unwrap();
                    stream.write_all(&chunk[..take]).await.unwrap();
                    sent += take as u64;
                }
                stream.flush().await.unwrap();
                stream.shutdown().await.unwrap();
                let mut answer = [0u8; 8];
                stream.read_exact(&mut answer).await.unwrap();
                u64::from_be_bytes(answer)
            }));
        }
        for (index, sender) in senders.into_iter().enumerate() {
            // The module opts into `panic` for exactly this: a stalled stream is
            // the failure this test exists to catch, and the panic names which
            // one.
            let Ok(joined) = tokio::time::timeout(Duration::from_secs(20), sender).await else {
                panic!("stream {index} stalled")
            };
            let counted = joined.unwrap();
            assert_eq!(counted, PAYLOAD, "stream {index} lost bytes");
        }
        drop(tunnel);
        server.abort();
    }

    /// A stream carries a real conversation: many round trips, each one a
    /// request the peer answers. This is the shape every forwarded TCP channel
    /// has, and it is where a lost wake-up or a stalled credit shows up.
    #[tokio::test]
    async fn a_stream_carries_a_hundred_round_trips() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 4, None, [9u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();
        let mut client = dialer.open_channel().await.unwrap();

        let opening = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            // The opening byte is the transport selector, which the server
            // consumes before any application data.
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            assert_eq!(selector[0], crate::protocol::PLAIN_SELECTOR);
            let mut got = vec![0u8; 1024];
            loop {
                match server.read_exact(&mut got).await {
                    Ok(_) => {
                        tokio::io::AsyncWriteExt::write_all(&mut server, &got)
                            .await
                            .unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
        });

        let mut wr = [0u8; 1024];
        let mut rd = [0u8; 1024];
        for step in 0u8..100 {
            wr[0] = step;
            let round = async {
                tokio::io::AsyncWriteExt::write_all(&mut client, &wr)
                    .await
                    .unwrap();
                tokio::io::AsyncReadExt::read_exact(&mut client, &mut rd)
                    .await
                    .unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), round)
                .await
                .unwrap_or_else(|_| panic!("round trip {step} stalled"));
            assert_eq!(wr, rd);
        }
        drop(client);
        opening.await.unwrap();
    }

    /// The same conversation through the Noise record stream: this is the
    /// product's actual stream stack, and the handshake runs inside the stream
    /// open, which is where a partial read or a stalled credit would show.
    #[cfg(feature = "noise")]
    #[tokio::test]
    async fn a_noise_stream_carries_a_hundred_round_trips() {
        use crate::config::{MaskedString, NoiseConfig};

        let client_keys = NoiseConfig {
            public_key: "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=".to_owned(),
            private_key: MaskedString::from("8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="),
        };
        let server_keys = NoiseConfig {
            public_key: "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=".to_owned(),
            private_key: MaskedString::from("kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE="),
        };

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let keys = crate::transport::NoiseKeys::from_config(&server_keys).unwrap();
        let opening = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            assert_eq!(selector[0], crate::protocol::NOISE_SELECTOR);
            let server: crate::transport::multiplex::CarrierStream = Box::new(server);
            let mut server = keys.wrap_responder(server).await.unwrap();
            let mut got = vec![0u8; 1024];
            loop {
                match server.read_exact(&mut got).await {
                    Ok(_) => {
                        tokio::io::AsyncWriteExt::write_all(&mut server, &got)
                            .await
                            .unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
        });

        let dialer = MhdLane::dial(
            bound,
            5,
            Some(client_keys),
            [3u8; crate::protocol::HASH_WIDTH_IN_BYTES],
        )
        .await
        .unwrap();
        let mut client = dialer.open_channel().await.unwrap();
        let mut wr = [0u8; 1024];
        let mut rd = [0u8; 1024];
        for step in 0u8..100 {
            wr[0] = step;
            let round = async {
                tokio::io::AsyncWriteExt::write_all(&mut client, &wr)
                    .await
                    .unwrap();
                tokio::io::AsyncReadExt::read_exact(&mut client, &mut rd)
                    .await
                    .unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), round)
                .await
                .unwrap_or_else(|_| panic!("noise round trip {step} stalled"));
            assert_eq!(wr, rd);
        }
        drop(client);
        opening.await.unwrap();
    }

    /// A bulk write whose reader is slow: the sender must keep every byte even
    /// when the reader only drains a slice every few milliseconds, exactly the
    /// shape of an echo service feeding a visitor nobody reads yet.
    #[tokio::test]
    async fn a_bulk_stream_survives_a_slow_reader() {
        let total = 8 * 1024 * 1024;
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 6, None, [5u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();
        let mut client = dialer.open_channel().await.unwrap();

        let reading = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            let mut got = vec![0u8; 64 * 1024];
            let mut read = 0usize;
            loop {
                match server.read(&mut got).await {
                    Ok(0) => break,
                    Ok(n) => {
                        read += n;
                        if read % (512 * 1024) < n {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
            read
        });

        let chunk = vec![0xA5u8; 64 * 1024];
        let mut written = 0;
        while written < total {
            client.write_all(&chunk).await.unwrap();
            written += chunk.len();
        }
        client.shutdown().await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(20), reading)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            read, total,
            "the reader must see every byte the writer sent"
        );
    }

    /// The carrier's cost on a clean path, measured on loopback.
    ///
    /// A diagnostic rather than a gate: the number is a loopback ceiling with no
    /// shaping, so it says how much the engine costs per byte and nothing about
    /// a real path. What it is for is the direction — a change that doubles the
    /// per-byte cost shows up here in seconds, and the bench that would prove a
    /// regression takes minutes and root.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "a diagnostic, not a gate: run it with --ignored"]
    async fn a_bulk_stream_costs_what_the_bench_measures() {
        /// This process's user and system CPU time, in seconds.
        ///
        /// `USER_HZ` is 100 on Linux, which is the only platform this
        /// diagnostic runs on; a portable reading would need a crate for one
        /// division.
        fn cpu_seconds() -> f64 {
            const USER_HZ: f64 = 100.0;
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let tail = stat.rsplit_once(')').unwrap().1;
            let fields: Vec<&str> = tail.split_whitespace().collect();
            let utime: u64 = fields[11].parse().unwrap();
            let stime: u64 = fields[12].parse().unwrap();
            f64::from(u32::try_from(utime + stime).unwrap_or(u32::MAX)) / USER_HZ
        }

        let total = 64 * 1024 * 1024;
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 21, None, [5u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();
        let mut client = dialer.open_channel().await.unwrap();

        let reading = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            let mut got = vec![0u8; 256 * 1024];
            let mut read = 0usize;
            loop {
                match server.read(&mut got).await {
                    Ok(0) => break,
                    Ok(n) => read += n,
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
            read
        });

        let chunk = vec![0xA5u8; 128 * 1024];
        let start = std::time::Instant::now();
        let cpu_start = cpu_seconds();
        let mut written = 0;
        while written < total {
            client.write_all(&chunk).await.unwrap();
            written += chunk.len();
        }
        client.shutdown().await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(120), reading)
            .await
            .unwrap()
            .unwrap();
        let wall = start.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu_start;
        assert_eq!(
            read, total,
            "the reader must see every byte the writer sent"
        );
        let gbit = f64::from(u32::try_from(total).unwrap()) * 8.0 / 1e9;
        let gbps = gbit / wall;
        let cost = cpu / gbit;
        println!(
            "mhd bulk: {total} B in {wall:.3}s = {gbps:.3} Gbit/s, {cpu:.3} cpu-s, \
             {cost:.3} cpu-s/Gbit, {cores:.2} cores",
            cores = cpu / wall,
        );
        // The floor is deliberately far below the measured figure: this is a
        // direction alarm, not a benchmark. The regression it exists to catch
        // was quadratic in the send window and cost 260 cpu-s/Gbit, so anything
        // above a tenth of the current reading is a real change and not noise.
        assert!(
            cost < 5.0,
            "the engine now costs {cost:.3} cpu-s/Gbit on loopback; the last \
             regression measured 267 and this floor is one twentieth of it"
        );
    }
}
