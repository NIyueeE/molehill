//! Data-channel striping: one logical connection spread over K parallel
//! data channels.
//!
//! A striped visitor connection is carried by `count` independent data
//! channels (a *stripe group*): each direction numbers its chunks
//! sequentially and spreads them round-robin over the group, and the
//! receiver reassembles them in sequence order, so the pair behaves like
//! one connection whose ceiling and window are the sum of its stripes.
//!
//! ```text
//!   sender                                   receiver
//!   chunk 0 ── stripe 0 ══► ─┐
//!   chunk 1 ── stripe 1 ══► ─┼─ reorder by seq ─► destination
//!   chunk 2 ── stripe 2 ══► ─┤   (BTreeMap<seq, chunk>,
//!   chunk 3 ── stripe 3 ══► ─┘    deliver while contiguous)
//! ```
//!
//! Wire format, after the [`DataChannelCmd::StartForwardStripedTcp`]
//! command that announces the group: every chunk is one frame
//! `[u64 seq][u16 len][payload]` on whichever stripe the round-robin
//! picks. `seq` counts frames per direction from zero; a frame travels
//! whole on one stripe (a stripe that stalls mid-frame holds it — moving
//! a half-written frame would truncate the stream's framing). `len` is
//! bounded by [`STRIPE_CHUNK_SIZE`], which matches the engine's yamux
//! frame split size, so one chunk is one frame on the wire.
//!
//! The layer sits *above* the data channel protocol and *below* the
//! copy loops: it works over plain transport connections and yamux
//! streams alike, and it keeps the yamux wire format untouched — the
//! striping is molehill's own data-channel framing, so a 0.8.x peer's
//! yamux framing is unaffected. Channels that do not carry the striped
//! command are byte-identical to the unstriped path.
//!
//! [`DataChannelCmd::StartForwardStripedTcp`]: crate::protocol::DataChannelCmd::StartForwardStripedTcp

use std::collections::BTreeMap;
#[cfg(feature = "multiplex")]
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
#[cfg(feature = "multiplex")]
use std::sync::Mutex;
use std::task::{Context, Poll};
#[cfg(feature = "multiplex")]
use std::time::Duration;
#[cfg(feature = "multiplex")]
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::debug;

/// Payload bytes per stripe frame.
///
/// Matches the framing engine's split size (`DEFAULT_SPLIT_SEND_SIZE`), so
/// one chunk becomes exactly one yamux frame and the framing counters of a
/// striped arm stay comparable with an unstriped one.
pub const STRIPE_CHUNK_SIZE: usize = 32 * 1024;

/// Frame header length: a big-endian `u64` sequence number followed by a
/// big-endian `u16` payload length.
pub const STRIPE_HEADER_LEN: usize = 10;

/// Upper bound for a group's stripe count. A stripe is a full data channel
/// (a yamux stream in multiplex mode), so this mirrors the per-tunnel
/// stream budget rather than inventing a new scale.
#[cfg(feature = "multiplex")]
pub const MAX_STRIPES: usize = 64;

/// Frames one receive direction may hold out of order before its readers
/// stall on the queue (the engine's per-stripe window is the real in-flight
/// bound; this queue is the *reorder* allowance on top of it).
pub const STRIPE_REORDER_QUEUE: usize = 32;

/// How long an incomplete group's stripes are parked before the registry
/// drops them. A completed gather takes milliseconds; an entry that stays
/// incomplete this long belongs to a server-side gather that was abandoned
/// (its control channel died), and its parked channels would otherwise
/// never be reaped.
#[cfg(feature = "multiplex")]
const STRIPE_GROUP_TTL: Duration = Duration::from_secs(60);

/// Environment override for the server's stripe count (multiplex builds
/// only).
///
/// Same pattern as `MOLEHILL_TCP_BUFFER_BYTES`: an opt-in switch for
/// measurements (the bench sets it per arm) that never enters the config
/// surface. An unparsable or out-of-range value is ignored with a warning,
/// so a typo degrades to the configured count instead of the data path.
#[cfg(feature = "multiplex")]
pub const STRIPE_COUNT_ENV: &str = "MOLEHILL_STRIPE_COUNT";

/// Resolve the effective stripe count for one registration: the
/// environment override wins when set and valid, then `[server.data]
/// stripe_count`, then `1` (no striping).
///
/// Multiplex-only by construction: the knob lives on `[server.data]`, and
/// without the feature a build can never produce a striped command anyway,
/// so the constant `1` is compiled in.
#[cfg(feature = "multiplex")]
pub fn stripe_count(configured: Option<u16>) -> usize {
    if let Some(v) = std::env::var_os(STRIPE_COUNT_ENV) {
        match v.to_string_lossy().parse::<usize>() {
            Ok(n) if (1..=MAX_STRIPES).contains(&n) => return n,
            _ => tracing::warn!(
                "Ignoring {STRIPE_COUNT_ENV}={:?}: expected 1..={MAX_STRIPES}",
                v
            ),
        }
    }
    configured.map_or(1, |n| n as usize).clamp(1, MAX_STRIPES)
}

/// Encode one stripe frame header into `buf` (`[u64 seq][u16 len]`, both
/// big-endian) and return the payload length it announces.
///
/// Kept as a free function so the sender and the tests share the exact
/// wire format.
fn encode_header(buf: &mut [u8; STRIPE_HEADER_LEN], seq: u64, len: usize) -> io::Result<u16> {
    let len = u16::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("stripe frame payload of {len} bytes exceeds the u16 wire length"),
        )
    })?;
    buf[..8].copy_from_slice(&seq.to_be_bytes());
    buf[8..].copy_from_slice(&len.to_be_bytes());
    Ok(len)
}

/// Parse one stripe frame header.
fn decode_header(buf: &[u8; STRIPE_HEADER_LEN]) -> (u64, usize) {
    // Slicing a fixed-size array by compile-time indices cannot fail.
    let seq = u64::from_be_bytes(buf[..8].try_into().unwrap_or([0; 8]));
    let len = u16::from_be_bytes([buf[8], buf[9]]) as usize;
    (seq, len)
}

/// Round-robin writer over one stripe group's channels.
///
/// Each `send` completes one whole frame on one stripe. A stripe that
/// cannot take the frame *before any byte of it has been committed* is
/// skipped (the round-robin moves on), which is what keeps one stalled
/// stripe from stalling the group; once bytes are committed the frame can
/// only finish on that stripe.
pub struct StripeSender<W> {
    streams: Vec<W>,
    /// Index the next frame starts from.
    next: usize,
    /// Bytes of the in-flight frame already committed per stripe.
    committed: Vec<usize>,
}

impl<W: AsyncWrite + Unpin> StripeSender<W> {
    pub fn new(streams: Vec<W>) -> Self {
        let committed = vec![0; streams.len()];
        StripeSender {
            streams,
            next: 0,
            committed,
        }
    }

    /// Write one framed chunk, completing the whole frame before returning.
    ///
    /// The frame (header + payload, the sequence number inside the header)
    /// is handed over by ownership: the read buffer became the frame, so
    /// the send direction copies nothing on its own account (link S1).
    pub async fn send_owned(&mut self, frame: Bytes) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_send(cx, &frame)).await
    }

    fn poll_send(&mut self, cx: &mut Context<'_>, frame: &Bytes) -> Poll<io::Result<()>> {
        let k = self.streams.len();
        let mut tried = 0;
        loop {
            let idx = self.next % k;
            let off = self.committed[idx];
            match Pin::new(&mut self.streams[idx]).poll_write(cx, &frame[off..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "a stripe data channel accepted a zero-length write",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    let end = off + n;
                    if end == frame.len() {
                        self.committed[idx] = 0;
                        self.next = idx + 1;
                        return Poll::Ready(Ok(()));
                    }
                    // Partial commit: the frame can only finish on this
                    // stripe, but `poll_write` does not register a waker on
                    // a partial write (only on `Pending`), so keep polling
                    // this stripe until it finishes or parks. Every
                    // iteration makes progress, so this cannot spin.
                    self.committed[idx] = end;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    if off > 0 {
                        // The committed stripe parked: its `poll_write`
                        // registered this task, so suspending here is safe.
                        return Poll::Pending;
                    }
                    // Nothing committed yet: another stripe may take the
                    // whole frame instead. Every `poll_write` above stored
                    // its waker, so the group wakes as soon as any stripe
                    // becomes writable.
                    self.next = idx + 1;
                    tried += 1;
                    if tried == k {
                        return Poll::Pending;
                    }
                }
            }
        }
    }

    /// Half-close every stripe's write side (FIN), after the source ended.
    pub async fn finish(&mut self) {
        for s in &mut self.streams {
            let _ = s.shutdown().await;
        }
    }
}

/// Send direction: copy `src` into the group, chunk by chunk.
///
/// Each chunk is read directly into the payload region of its frame
/// buffer — behind the 10-byte header, which is written in front once
/// the read length is known — so the frame crosses to the stripe by
/// ownership and the payload is copied zero times (link S1).
async fn send_loop<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut src: R,
    mut sender: StripeSender<W>,
) -> anyhow::Result<()> {
    let mut seq = 0u64;
    loop {
        let mut frame = BytesMut::with_capacity(STRIPE_HEADER_LEN + STRIPE_CHUNK_SIZE);
        // Reserve the header room; the read appends behind it.
        frame.resize(STRIPE_HEADER_LEN, 0);
        let n = match src.read_buf(&mut frame).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => anyhow::bail!("stripe source read failed: {e}"),
        };
        encode_header(
            (&mut frame[..STRIPE_HEADER_LEN])
                .try_into()
                .map_err(|_| anyhow::anyhow!("stripe frame header slice"))?,
            seq,
            n,
        )
        .map_err(|e| anyhow::anyhow!("stripe frame header: {e}"))?;
        sender
            .send_owned(frame.freeze())
            .await
            .map_err(|e| anyhow::anyhow!("stripe write failed: {e}"))?;
        seq += 1;
    }
    sender.finish().await;
    debug!("stripe send direction finished after {seq} frames");
    Ok(())
}

/// One frame from one stripe, or the news that the stripe broke.
enum StripeEvent {
    Frame { seq: u64, bytes: Bytes },
    Broken(String),
}

/// Read frames off one stripe and feed them to the group's reorder loop.
///
/// A clean EOF at a frame boundary ends the stripe silently; anything else
/// (a truncated frame, a read error) reports `Broken`, which makes the
/// receive direction abandon the group instead of waiting forever for a
/// sequence number that will never arrive.
async fn stripe_reader<R: AsyncRead + Unpin>(mut r: R, tx: mpsc::Sender<StripeEvent>) {
    let mut header = [0u8; STRIPE_HEADER_LEN];
    loop {
        // The first byte alone: a clean end between frames is a normal
        // half-close, a partial header is a broken frame.
        match r.read_u8().await {
            Ok(b) => header[0] = b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(e) => {
                let _ = tx
                    .send(StripeEvent::Broken(format!("stripe read failed: {e}")))
                    .await;
                return;
            }
        }
        if let Err(e) = r.read_exact(&mut header[1..]).await {
            let _ = tx
                .send(StripeEvent::Broken(format!(
                    "stripe frame header truncated: {e}"
                )))
                .await;
            return;
        }
        let (seq, len) = decode_header(&header);
        let mut payload = BytesMut::zeroed(len);
        if let Err(e) = r.read_exact(&mut payload).await {
            let _ = tx
                .send(StripeEvent::Broken(format!(
                    "stripe frame payload truncated: {e}"
                )))
                .await;
            return;
        }
        if tx
            .send(StripeEvent::Frame {
                seq,
                bytes: payload.freeze(),
            })
            .await
            .is_err()
        {
            return; // the receive direction is gone; this stripe is over
        }
    }
}

/// Receive direction: reassemble every stripe's frames in sequence order
/// and write them to `dest`.
///
/// Frames are held in a `BTreeMap` keyed by sequence number while an earlier
/// one is missing, and released as soon as the gap fills. Out-of-order
/// buffering is bounded by [`STRIPE_REORDER_QUEUE`]: when the queue is full
/// the readers stall, which propagates through the stripes' windows as
/// backpressure to the peer's sender.
async fn recv_loop<R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin>(
    reads: Vec<R>,
    mut dest: W,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<StripeEvent>(STRIPE_REORDER_QUEUE);
    for r in reads {
        let tx = tx.clone();
        tokio::spawn(stripe_reader(r, tx));
    }
    drop(tx); // only the readers hold senders now: their exit closes the channel

    let mut next = 0u64;
    let mut pending: BTreeMap<u64, Bytes> = BTreeMap::new();
    while let Some(event) = rx.recv().await {
        match event {
            StripeEvent::Frame { seq, bytes } => {
                if seq == next {
                    write_dest(&mut dest, &bytes).await?;
                    next += 1;
                    while let Some(bytes) = pending.remove(&next) {
                        write_dest(&mut dest, &bytes).await?;
                        next += 1;
                    }
                } else if seq > next {
                    pending.insert(seq, bytes);
                }
                // seq < next: a duplicated frame (the sender re-sent after
                // a rotate); already delivered, so drop it.
            }
            StripeEvent::Broken(reason) => {
                let _ = dest.shutdown().await;
                anyhow::bail!("stripe group broke: {reason}");
            }
        }
    }
    dest.shutdown().await?;
    debug!("stripe receive direction finished after {next} frames");
    Ok(())
}

async fn write_dest<W: AsyncWrite + Unpin>(dest: &mut W, bytes: &[u8]) -> anyhow::Result<()> {
    dest.write_all(bytes)
        .await
        .map_err(|e| anyhow::anyhow!("stripe destination write failed: {e}"))
}

/// Forward one stripe group: the send direction copies `src` into the
/// group's stripes, the receive direction reassembles them back into
/// `dest`.
///
/// The two directions are independent (like `copy_bidirectional`'s); a
/// failure in either aborts the other and ends the group, because a broken
/// stripe can never deliver the missing sequence numbers.
pub fn spawn_group<R, D, S>(src: R, dest: D, streams: Vec<S>) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    D: AsyncWrite + Unpin + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reads = Vec::with_capacity(streams.len());
        let mut writes = Vec::with_capacity(streams.len());
        for s in streams {
            let (r, w) = tokio::io::split(s);
            reads.push(r);
            writes.push(w);
        }
        let stripes = writes.len();
        debug!("stripe group started with {stripes} stripes");

        let mut send = tokio::spawn(send_loop(src, StripeSender::new(writes)));
        let mut recv = tokio::spawn(recv_loop(reads, dest));
        // Wait for both directions. A failure in either aborts the other
        // (the group is broken and its missing sequence numbers will never
        // arrive); a direction that ended cleanly lets the other run to its
        // own end. Each handle is polled exactly once per select! arm and
        // never again afterwards — polling a finished JoinHandle twice is a
        // panic.
        let mut send_res = None;
        let mut recv_res = None;
        while send_res.is_none() || recv_res.is_none() {
            tokio::select! {
                res = &mut send, if send_res.is_none() => {
                    if matches!(res, Ok(Err(_))) {
                        recv.abort();
                    }
                    send_res = Some(res);
                }
                res = &mut recv, if recv_res.is_none() => {
                    if matches!(res, Ok(Err(_))) {
                        send.abort();
                    }
                    recv_res = Some(res);
                }
            }
        }
        debug!("stripe group finished");
    })
}

/// Client-side registry of stripe groups.
///
/// The stripes of one group arrive as independent data channels (the server
/// requests them one by one), so the first arrivals park their stream here
/// until the group is complete; the registrar then takes the group out and
/// forwards it. Entries that never complete (an abandoned server-side
/// gather) are dropped after [`STRIPE_GROUP_TTL`], which also closes their
/// parked channels.
///
/// Multiplex-only: without the feature the client can never receive a
/// striped command, so the registry has nothing to register.
#[cfg(feature = "multiplex")]
pub struct StripeGroups<S> {
    groups: Mutex<HashMap<u32, PartialGroup<S>>>,
}

/// One stripe parked while its group is still incomplete.
#[cfg(feature = "multiplex")]
struct PartialGroup<S> {
    /// How many stripes the group has (from each stripe's command).
    count: u8,
    /// Parked streams by stripe index; `None` until that stripe arrives.
    streams: Vec<Option<S>>,
    /// Local endpoint the complete group forwards to.
    local_addr: String,
    /// Socket options for the local connection.
    sock_opts: crate::transport::SocketOpts,
    started: Instant,
}

/// A complete stripe group, ready to be forwarded.
#[cfg(feature = "multiplex")]
pub struct CompleteGroup<S> {
    pub streams: Vec<S>,
    pub local_addr: String,
    pub sock_opts: crate::transport::SocketOpts,
}

#[cfg(feature = "multiplex")]
impl<S> Default for StripeGroups<S> {
    fn default() -> Self {
        StripeGroups {
            groups: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(feature = "multiplex")]
impl<S> StripeGroups<S> {
    pub fn new() -> Self {
        StripeGroups::default()
    }

    /// Park one stripe's stream in its group.
    ///
    /// Returns the group once its last stripe arrives (the registrar then
    /// dials the local service and forwards it). A command whose metadata
    /// contradicts an already-parked stripe is a wire error and fails that
    /// channel instead of silently splitting the group.
    pub fn register(
        &self,
        group: u32,
        index: u8,
        count: u8,
        stream: S,
        local_addr: &str,
        sock_opts: crate::transport::SocketOpts,
    ) -> anyhow::Result<Option<CompleteGroup<S>>> {
        anyhow::ensure!(
            count >= 1 && index < count,
            "striped command claims stripe {index} of {count}"
        );
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Reap groups the server abandoned: their parked channels are
        // closed here, which ends any wait the server still holds on them.
        let now = Instant::now();
        groups.retain(|_, g| now.duration_since(g.started) < STRIPE_GROUP_TTL);
        let entry = groups.entry(group).or_insert_with(|| PartialGroup {
            count,
            streams: (0..count).map(|_| None).collect(),
            local_addr: local_addr.to_owned(),
            sock_opts,
            started: now,
        });
        anyhow::ensure!(
            entry.count == count,
            "stripe group {group} mixes {count} and {} stripes",
            entry.count
        );
        let slot = entry
            .streams
            .get_mut(index as usize)
            .ok_or_else(|| anyhow::anyhow!("stripe index {index} out of range"))?;
        anyhow::ensure!(
            slot.is_none(),
            "stripe group {group} received a second stripe {index}"
        );
        *slot = Some(stream);
        if entry.streams.iter().all(Option::is_some) {
            // The completeness check above guarantees the entry is still
            // parked; take it out and hand its stripes to the caller.
            let Some(entry) = groups.remove(&group) else {
                return Ok(None);
            };
            let streams = entry.streams.into_iter().flatten().collect();
            return Ok(Some(CompleteGroup {
                streams,
                local_addr: entry.local_addr,
                sock_opts: entry.sock_opts,
            }));
        }
        Ok(None)
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
    use std::time::Duration;
    use tokio::io::duplex;
    use tokio::time::{sleep, timeout};

    fn frame_bytes(seq: u64, payload: &[u8]) -> Bytes {
        let mut header = [0u8; STRIPE_HEADER_LEN];
        encode_header(&mut header, seq, payload.len()).unwrap();
        let mut frame = BytesMut::with_capacity(STRIPE_HEADER_LEN + payload.len());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(payload);
        frame.freeze()
    }

    #[test]
    fn header_roundtrip() {
        let mut header = [0u8; STRIPE_HEADER_LEN];
        assert_eq!(encode_header(&mut header, 42, 0x1234).unwrap(), 0x1234);
        assert_eq!(decode_header(&header), (42, 0x1234));

        // The wire length is a u16: anything larger is refused rather than
        // silently truncated.
        let mut header = [0u8; STRIPE_HEADER_LEN];
        assert!(encode_header(&mut header, 1, u16::MAX as usize + 1).is_err());
    }

    #[cfg(feature = "multiplex")]
    #[test]
    fn stripe_count_clamps_the_configured_value() {
        // The environment override is a bench-only switch that cannot be
        // set from a test (edition-2024 env mutation is unsafe); what is
        // testable here is the config clamp to [1, MAX_STRIPES].
        assert_eq!(stripe_count(None), 1);
        assert_eq!(stripe_count(Some(0)), 1);
        assert_eq!(stripe_count(Some(4)), 4);
        assert_eq!(stripe_count(Some(u16::MAX)), MAX_STRIPES);
    }

    #[tokio::test]
    async fn out_of_order_frames_are_reordered() {
        // Three stripes; the peer writes seq 0 and 2 before seq 1, so the
        // receiver must hold 2 back until 1 arrives and emit 0,1,2.
        let (mut w0, r0) = duplex(64 * 1024);
        let (mut w1, r1) = duplex(64 * 1024);
        let (mut w2, r2) = duplex(64 * 1024);
        // The send direction's source: EOF immediately (the peer below is
        // the direct writer of the stripe pipes, not this group).
        let (src_w, src_r) = duplex(16);
        let (dest_w, mut dest_r) = duplex(256 * 1024);
        drop(src_w);

        let group = spawn_group(src_r, dest_w, vec![r0, r1, r2]);

        let p0 = b"aaaa".to_vec();
        let p1 = b"bbbb".to_vec();
        let p2 = b"cccc".to_vec();
        w0.write_all(&frame_bytes(0, &p0)).await.unwrap();
        w1.write_all(&frame_bytes(2, &p2)).await.unwrap();
        sleep(Duration::from_millis(50)).await; // let 0 and 2 land first
        w2.write_all(&frame_bytes(1, &p1)).await.unwrap();
        sleep(Duration::from_millis(50)).await;
        drop(w0);
        drop(w1);
        drop(w2);

        let mut got = Vec::new();
        timeout(Duration::from_secs(5), dest_r.read_to_end(&mut got))
            .await
            .expect("receive direction did not finish")
            .unwrap();
        assert_eq!(got, [p0, p1, p2].concat());
        timeout(Duration::from_secs(5), group)
            .await
            .expect("group task did not finish")
            .unwrap();
    }

    /// A writer that accepts a fixed number of bytes per ready poll and
    /// records everything written, so the round-robin's stripe choice is
    /// observable without real sockets. `accept: None` refuses every write
    /// (the stand-in for a backpressured socket that has room for nothing).
    struct MockWriter {
        accept: Option<usize>,
        written: Vec<u8>,
    }

    impl MockWriter {
        fn accepting(accept: usize) -> Self {
            MockWriter {
                accept: Some(accept),
                written: Vec::new(),
            }
        }

        fn refusing() -> Self {
            MockWriter {
                accept: None,
                written: Vec::new(),
            }
        }
    }

    impl tokio::io::AsyncWrite for MockWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            match self.accept {
                None => std::task::Poll::Pending,
                Some(accept) => {
                    let n = buf.len().min(accept);
                    self.written.extend_from_slice(&buf[..n]);
                    std::task::Poll::Ready(Ok(n))
                }
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn noop_waker() -> std::task::Waker {
        std::task::Waker::noop().clone()
    }

    #[test]
    fn an_uncommitted_frame_rotates_to_a_writable_stripe() {
        let mut sender =
            StripeSender::new(vec![MockWriter::refusing(), MockWriter::accepting(4096)]);
        let payload = [0xAB; 100];
        let waker = noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = Box::pin(sender.send_owned(frame_bytes(7, &payload)));
        assert!(
            fut.as_mut().poll(&mut cx).is_ready(),
            "the frame should complete on the writable stripe"
        );
        drop(fut);
        // The refusing stripe never saw a byte; the frame is whole on the
        // accepting one.
        assert!(sender.streams[0].written.is_empty());
        assert_eq!(sender.streams[1].written, frame_bytes(7, &payload).to_vec());
    }

    #[test]
    fn a_committed_frame_finishes_on_its_own_stripe() {
        // A stripe that accepted half a frame must finish it: moving the
        // remainder to another stripe would corrupt that stream's framing.
        let mut sender = StripeSender::new(vec![
            MockWriter::accepting(50), // halves every write
            MockWriter::accepting(4096),
        ]);
        let payload = [0xCD; 200];
        let waker = noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = Box::pin(sender.send_owned(frame_bytes(3, &payload)));
        let mut polls = 0;
        loop {
            polls += 1;
            if fut.as_mut().poll(&mut cx).is_ready() {
                break;
            }
            assert!(polls < 20, "the committed frame never completed");
        }
        drop(fut);
        assert!(
            sender.streams[1].written.is_empty(),
            "the frame was moved mid-commit"
        );
        assert_eq!(sender.streams[0].written, frame_bytes(3, &payload).to_vec());
    }

    #[tokio::test]
    async fn a_stalled_stripe_never_loses_a_frame() {
        // Stripe 1's pipe is too small for the transfer, so it stalls and
        // resumes repeatedly while stripe 0 keeps flowing. Every frame must
        // arrive exactly once — the regression this guards is a partial
        // write suspending the sender without a registered waker, which
        // froze the group after its first frame.
        const FRAMES: usize = 16; // 16 x 32 KiB = the whole source
        let payload = vec![0x5Au8; STRIPE_CHUNK_SIZE];

        let (a0, b0) = duplex(4 * 1024 * 1024);
        let (a1, b1) = duplex(48 * 1024); // holds barely one frame
        let (mut src_w, src_r) = duplex(4 * 1024 * 1024);
        let (dest_w, dest_r) = duplex(4 * 1024 * 1024);

        let group = spawn_group(src_r, dest_w, vec![a0, a1]);

        // Drain both stripes concurrently: stripe 1 parks its sender every
        // time its pipe fills, and a parked frame must resume on its own.
        let drain = |mut b: tokio::io::DuplexStream| {
            tokio::spawn(async move {
                let mut seqs = Vec::new();
                let mut header = [0u8; STRIPE_HEADER_LEN];
                loop {
                    match b.read_u8().await {
                        Ok(byte) => header[0] = byte,
                        Err(_) => return seqs,
                    }
                    b.read_exact(&mut header[1..]).await.unwrap();
                    let (seq, len) = decode_header(&header);
                    let mut payload = vec![0u8; len];
                    b.read_exact(&mut payload).await.unwrap();
                    seqs.push(seq);
                }
            })
        };
        let drains = vec![drain(b0), drain(b1)];

        for _ in 0..FRAMES {
            src_w.write_all(&payload).await.unwrap();
        }
        drop(src_w); // EOF: the sender shuts its stripes down afterwards

        let mut all: Vec<u64> = Vec::new();
        for d in drains {
            let seqs = timeout(Duration::from_secs(10), d)
                .await
                .expect("a stripe drain did not finish: the sender lost a wakeup")
                .unwrap();
            all.extend(seqs);
        }
        all.sort_unstable();
        assert_eq!(all, (0..FRAMES as u64).collect::<Vec<_>>());

        // The receive direction reads the other side of each pipe, which
        // this test never wrote; the drains dropping their ends on EOF is
        // what ends its readers.
        drop(dest_r);
        timeout(Duration::from_secs(5), group)
            .await
            .expect("group task did not finish")
            .unwrap();
    }

    #[tokio::test]
    async fn a_truncated_frame_breaks_the_group() {
        // A stripe that ends mid-frame is not a clean half-close: the
        // receive direction must abandon the group and close the
        // destination instead of waiting for a sequence number that will
        // never arrive.
        let (mut w0, r0) = duplex(64 * 1024);
        let (w1, r1) = duplex(64 * 1024);
        let (src_w, src_r) = duplex(16);
        let (dest_w, mut dest_r) = duplex(64 * 1024);
        drop(src_w);

        let group = spawn_group(src_r, dest_w, vec![r0, r1]);

        w0.write_all(&frame_bytes(0, b"first")).await.unwrap();
        let mut truncated = frame_bytes(1, b"second").to_vec();
        truncated.truncate(12); // header + 2 of 6 payload bytes
        w0.write_all(&truncated).await.unwrap();
        sleep(Duration::from_millis(50)).await;
        drop(w0);
        drop(w1);

        let mut got = Vec::new();
        timeout(Duration::from_secs(5), dest_r.read_to_end(&mut got))
            .await
            .expect("destination was not closed when the group broke")
            .unwrap();
        assert_eq!(got, b"first"); // the in-order frame before the break
        timeout(Duration::from_secs(5), group)
            .await
            .expect("group task did not finish")
            .unwrap();
    }

    #[tokio::test]
    async fn duplicated_frames_are_dropped() {
        // A frame that was already delivered (seq below the next
        // expected) must not be written twice.
        let (mut w0, r0) = duplex(64 * 1024);
        let (w1, r1) = duplex(64 * 1024);
        let (src_w, src_r) = duplex(16);
        let (dest_w, mut dest_r) = duplex(64 * 1024);
        drop(src_w);

        let group = spawn_group(src_r, dest_w, vec![r0, r1]);
        w0.write_all(&frame_bytes(0, b"ab")).await.unwrap();
        w0.write_all(&frame_bytes(1, b"cd")).await.unwrap();
        w0.write_all(&frame_bytes(0, b"ab")).await.unwrap(); // stale duplicate
        sleep(Duration::from_millis(50)).await;
        drop(w0);
        drop(w1);

        let mut got = Vec::new();
        timeout(Duration::from_secs(5), dest_r.read_to_end(&mut got))
            .await
            .expect("receive direction did not finish")
            .unwrap();
        assert_eq!(got, b"abcd");
        timeout(Duration::from_secs(5), group)
            .await
            .expect("group task did not finish")
            .unwrap();
    }

    #[cfg(feature = "multiplex")]
    #[test]
    fn registry_completes_a_group_and_validates_stripes() {
        let mut parked = (0..4).map(|_| duplex(16).0).collect::<Vec<_>>();
        let groups = StripeGroups::new();

        let opts = crate::transport::SocketOpts::none();
        for (i, ch) in parked.drain(..3).enumerate() {
            let done = groups
                .register(7, u8::try_from(i).unwrap(), 4, ch, "127.0.0.1:9", opts)
                .unwrap();
            assert!(done.is_none(), "group completed before its last stripe");
        }
        let last = parked.pop().unwrap();
        let done = groups.register(7, 3, 4, last, "127.0.0.1:9", opts).unwrap();
        assert_eq!(done.unwrap().streams.len(), 4);

        // A stripe whose index is out of range is a wire error.
        let ch = duplex(16).0;
        assert!(groups.register(8, 4, 4, ch, "127.0.0.1:9", opts).is_err());

        // A second stripe with an index already parked is a wire error.
        let ch = duplex(16).0;
        groups.register(9, 1, 2, ch, "127.0.0.1:9", opts).unwrap();
        let ch = duplex(16).0;
        assert!(groups.register(9, 1, 2, ch, "127.0.0.1:9", opts).is_err());

        // A stripe whose count contradicts the parked ones is a wire error.
        let ch = duplex(16).0;
        groups.register(10, 0, 4, ch, "127.0.0.1:9", opts).unwrap();
        let ch = duplex(16).0;
        assert!(groups.register(10, 1, 3, ch, "127.0.0.1:9", opts).is_err());
    }
}
