// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::mux::ConnectionError;
use crate::mux::connection::rtt::Rtt;
use crate::mux::frame::header::ACK;
use crate::mux::{
    Config, DEFAULT_CREDIT,
    chunks::Chunks,
    connection::{self, StreamCommand, rtt},
    frame::{
        Either, Frame,
        header::{Data, Header, StreamId, WindowUpdate},
    },
};
use flow_control::FlowController;
use parking_lot::{Mutex, MutexGuard};
use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

mod flow_control;

/// The state of a Yamux stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Open bidirectionally.
    Open {
        /// Whether the stream is acknowledged.
        ///
        /// For outbound streams, this tracks whether the remote has acknowledged our stream.
        /// For inbound streams, this tracks whether we have acknowledged the stream to the remote.
        ///
        /// This starts out with `false` and is set to `true` when we receive or send an `ACK` flag for this stream.
        /// We may also directly transition:
        /// - from `Open` to `RecvClosed` if the remote immediately sends `FIN`.
        /// - from `Open` to `Closed` if the remote immediately sends `RST`.
        acknowledged: bool,
    },
    /// Open for incoming messages.
    SendClosed,
    /// Open for outgoing messages.
    RecvClosed,
    /// Closed (terminal state).
    Closed,
}

impl State {
    /// Can we receive messages over this stream?
    pub fn can_read(self) -> bool {
        !matches!(self, State::RecvClosed | State::Closed)
    }

    /// Can we send messages over this stream?
    pub fn can_write(self) -> bool {
        !matches!(self, State::SendClosed | State::Closed)
    }
}

/// Indicate if a flag still needs to be set on an outbound header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Flag {
    /// No flag needs to be set.
    None,
    /// The stream was opened lazily, so set the initial SYN flag.
    Syn,
    /// The stream still needs acknowledgement, so set the ACK flag.
    Ack,
}

/// A multiplexed Yamux stream.
///
/// Streams are created either outbound via [`crate::mux::Connection::poll_new_outbound`]
/// or inbound via [`crate::mux::Connection::poll_next_inbound`].
///
/// `Stream` implements [`AsyncRead`] and [`AsyncWrite`] and also
/// [`futures::stream::Stream`].
pub struct Stream {
    id: StreamId,
    conn: connection::Id,
    config: Arc<Config>,
    sender: mpsc::Sender<StreamCommand>,
    flag: Flag,
    shared: Arc<Mutex<Shared>>,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Stream")
            .field("id", &self.id.val())
            .field("connection", &self.conn)
            .field("config", &self.config)
            .field("flag", &self.flag)
            .field("sender", &self.sender)
            .field("shared", &self.shared)
            .finish()
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(Stream {}/{})", self.conn, self.id.val())
    }
}

impl Stream {
    pub(crate) fn new_inbound(
        id: StreamId,
        conn: connection::Id,
        config: Arc<Config>,
        send_window: u32,
        sender: mpsc::Sender<StreamCommand>,
        rtt: rtt::Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::Ack,
            shared: Arc::new(Mutex::new(Shared::new(
                DEFAULT_CREDIT,
                send_window,
                accumulated_max_stream_windows,
                rtt,
                config,
            ))),
        }
    }

    pub(crate) fn new_outbound(
        id: StreamId,
        conn: connection::Id,
        config: Arc<Config>,
        sender: mpsc::Sender<StreamCommand>,
        rtt: rtt::Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::Syn,
            shared: Arc::new(Mutex::new(Shared::new(
                DEFAULT_CREDIT,
                DEFAULT_CREDIT,
                accumulated_max_stream_windows,
                rtt,
                config,
            ))),
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.shared().state(), State::Closed)
    }

    pub(crate) fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock()
    }

    pub(crate) fn clone_shared(&self) -> Arc<Mutex<Shared>> {
        self.shared.clone()
    }

    fn write_zero_err(&self) -> io::Error {
        let msg = format!("{}/{}: connection is closed", self.conn, self.id);
        io::Error::new(io::ErrorKind::WriteZero, msg)
    }

    /// Set ACK or SYN flag if necessary.
    fn add_flag(&mut self, header: &mut Header<Either<Data, WindowUpdate>>) {
        match self.flag {
            Flag::None => (),
            Flag::Syn => {
                header.syn();
                self.flag = Flag::None;
            }
            Flag::Ack => {
                header.ack();
                self.flag = Flag::None;
            }
        }
    }

    /// Park this stream's task until the connection's command channel has
    /// capacity again (`wake_stream_writer` on the connection retries us).
    ///
    /// The capacity re-check after storing the waker closes a lost-wakeup
    /// race: the connection runs on another task and may free capacity —
    /// finding no waker to wake — between the caller's capacity check and
    /// the store below. Re-checking after the store is airtight: a wake
    /// after the store sees the stored waker, a wake before it is caught
    /// by the re-check.
    fn park_on_full_channel(&mut self, cx: &mut Context<'_>) {
        self.shared().writer = Some(cx.waker().clone());
        if self.sender.capacity() > 0
            && let Some(w) = self.shared().writer.take()
        {
            w.wake();
        }
    }

    /// Send new credit to the sending side via a window update message if
    /// permitted.
    fn send_window_update(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.shared.lock().state.can_read() {
            return Poll::Ready(Ok(()));
        }

        // Check channel capacity before consuming the credit: on a full
        // channel the next poll retries from here and nothing is lost.
        if self.sender.capacity() == 0 {
            self.park_on_full_channel(cx);
            return Poll::Pending;
        }

        let Some(credit) = self.shared.lock().next_window_update() else {
            return Poll::Ready(Ok(()));
        };

        let mut frame = Frame::window_update(self.id, credit).right();
        self.add_flag(frame.header_mut());
        let cmd = StreamCommand::SendFrame(frame);
        // Capacity was checked immediately above and this task is the only
        // sender on the channel, so a full channel here is not reachable;
        // a closed one means the connection is gone.
        self.sender
            .try_send(cmd)
            .map_err(|_| self.write_zero_err())?;

        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.config.read_after_close && self.sender.is_closed() {
            return Poll::Ready(Ok(()));
        }

        // Copy data from stream buffer FIRST: delivering buffered bytes
        // needs no channel capacity, while the window update below does.
        // Parking on a full command channel while data is still buffered
        // starves the peer's sender (its credit only comes back through
        // that update) and can deadlock the tunnel — the buffered data is
        // exactly what breaks the cycle.
        let mut shared = self.shared();
        let mut n = 0;
        while let Some(chunk) = shared.buffer.front_mut() {
            if chunk.is_empty() {
                shared.buffer.pop();
                continue;
            }
            let k = std::cmp::min(chunk.len(), buf.remaining());
            buf.put_slice(&chunk.as_ref()[..k]);
            n += k;
            chunk.advance(k);
            if buf.remaining() == 0 {
                break;
            }
        }

        // Attempt the window update on every poll, delivered data or not:
        // the credit math accounts for what is still buffered, so firing
        // early is correct and keeps the peer's sender supplied steadily
        // (waiting for an empty buffer sends fewer, burstier updates). The
        // delivery above already happened, so a channel that parks here
        // never holds buffered data hostage — the update retries on the
        // next poll.
        drop(shared);
        match self.send_window_update(cx) {
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) | Poll::Pending => {}
        }

        if n > 0 {
            tracing::trace!("{}/{}: read {} bytes", self.conn, self.id, n);
            return Poll::Ready(Ok(()));
        }

        let mut shared = self.shared();

        // Buffer is empty, let's check if we can expect to read more data.
        if !shared.state().can_read() {
            tracing::debug!("{}/{}: eof", self.conn, self.id);
            return Poll::Ready(Ok(())); // stream has been reset
        }

        // Since we have no more data at this point, we want to be woken up
        // by the connection when more becomes available for us.
        shared.reader = Some(cx.waker().clone());
        // Re-check after storing: the connection may have pushed data in
        // between (a lost-wakeup race: the wake found no waker to wake
        // yet).
        if shared.buffer.len() > 0
            && let Some(w) = shared.reader.take()
        {
            w.wake();
        }

        Poll::Pending
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Park the writer on a full command channel before any window is
        // consumed; `wake_stream_writer` on the connection retries us.
        if self.sender.capacity() == 0 {
            self.park_on_full_channel(cx);
            return Poll::Pending;
        }
        let body = {
            let mut shared = self.shared();
            if !shared.state().can_write() {
                tracing::debug!("{}/{}: can no longer write", self.conn, self.id);
                return Poll::Ready(Err(self.write_zero_err()));
            }
            if shared.send_window() == 0 {
                tracing::trace!("{}/{}: no more credit left", self.conn, self.id);
                shared.writer = Some(cx.waker().clone());
                // Re-check after storing: a window update from the
                // connection may have landed in between (a lost-wakeup
                // race: the wake found no waker to wake yet).
                if shared.send_window() > 0
                    && let Some(w) = shared.writer.take()
                {
                    w.wake();
                }
                return Poll::Pending;
            }
            let k = std::cmp::min(shared.send_window() as usize, buf.len());
            let k = std::cmp::min(k, self.config.split_send_size);
            // `k` is bounded by the send window two lines above, so the
            // conversion and the window subtraction below cannot fail;
            // a failure would mean a flow-control bug, which surfaces
            // as a write error instead of a panic.
            let Ok(credit) = u32::try_from(k) else {
                return Poll::Ready(Err(self.write_zero_err()));
            };
            if let Err(e) = shared.consume_send_window(credit) {
                return Poll::Ready(Err(io::Error::other(e)));
            }
            Vec::from(&buf[..k])
        };
        let n = body.len();
        // `k` (hence the body length) is bounded by the split size and
        // the window, both far below u32::MAX.
        let mut frame = match Frame::data(self.id, body) {
            Ok(frame) => frame.left(),
            Err(_) => return Poll::Ready(Err(self.write_zero_err())),
        };
        self.add_flag(frame.header_mut());
        tracing::trace!("{}/{}: write {} bytes", self.conn, self.id, n);

        // technically, the frame hasn't been sent yet on the wire but from the perspective of this data structure, we've queued the frame for sending
        // We are tracking this information:
        // a) to be consistent with outbound streams
        // b) to correctly test our behaviour around timing of when ACKs are sent. See `ack_timing.rs` test.
        if frame.header().flags().contains(ACK) {
            self.shared()
                .update_state(self.conn, self.id, State::Open { acknowledged: true });
        }

        let cmd = StreamCommand::SendFrame(frame);
        // Capacity was checked immediately above and this task is the only
        // sender on the channel, so a full channel here is not reachable;
        // a closed one means the connection is gone.
        self.sender
            .try_send(cmd)
            .map_err(|_| self.write_zero_err())?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Commands are queued in the channel by `try_send`; there is
        // nothing to wait for here (futures' mpsc `Sink` flush was a
        // no-op as well).
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_closed() {
            return Poll::Ready(Ok(()));
        }
        if self.sender.capacity() == 0 {
            self.park_on_full_channel(cx);
            return Poll::Pending;
        }
        let ack = if self.flag == Flag::Ack {
            self.flag = Flag::None;
            true
        } else {
            false
        };
        tracing::trace!("{}/{}: close", self.conn, self.id);
        let cmd = StreamCommand::CloseStream { ack };
        // Capacity was checked immediately above and this task is the only
        // sender on the channel, so a full channel here is not reachable;
        // a closed one means the connection is gone.
        self.sender
            .try_send(cmd)
            .map_err(|_| self.write_zero_err())?;
        self.shared()
            .update_state(self.conn, self.id, State::SendClosed);
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub(crate) struct Shared {
    state: State,
    flow_controller: FlowController,
    pub(crate) buffer: Chunks,
    pub(crate) reader: Option<Waker>,
    pub(crate) writer: Option<Waker>,
}

impl Shared {
    fn new(
        receive_window: u32,
        send_window: u32,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
        rtt: Rtt,
        config: Arc<Config>,
    ) -> Self {
        Shared {
            state: State::Open {
                acknowledged: false,
            },
            flow_controller: FlowController::new(
                receive_window,
                send_window,
                accumulated_max_stream_windows,
                rtt,
                config,
            ),
            buffer: Chunks::new(),
            reader: None,
            writer: None,
        }
    }

    pub(crate) fn state(&self) -> State {
        self.state
    }

    /// Update the stream state and return the state before it was updated.
    pub(crate) fn update_state(
        &mut self,
        cid: connection::Id,
        sid: StreamId,
        next: State,
    ) -> State {
        use self::State::{Closed, Open, RecvClosed, SendClosed};

        let current = self.state;

        // Arms merged by identical body (pedantic `match_same_arms`); the
        // or-patterns keep different first elements so they stay unnested
        // at the top level.
        match (current, next) {
            (Closed, _)
            | (RecvClosed, Open { .. } | RecvClosed)
            | (SendClosed, Open { .. } | SendClosed) => {}
            (Open { .. }, _) => self.state = next,
            (RecvClosed, Closed | SendClosed) | (SendClosed, Closed | RecvClosed) => {
                self.state = Closed;
            }
        }

        tracing::trace!(
            "{}/{}: update state: (from {:?} to {:?} -> {:?})",
            cid,
            sid,
            current,
            next,
            self.state
        );

        current // Return the previous stream state for informational purposes.
    }

    pub(crate) fn next_window_update(&mut self) -> Option<u32> {
        self.flow_controller.next_window_update(self.buffer.len())
    }

    pub(crate) fn send_window(&self) -> u32 {
        self.flow_controller.send_window()
    }

    pub(crate) fn consume_send_window(&mut self, i: u32) -> Result<(), ConnectionError> {
        self.flow_controller.consume_send_window(i)
    }

    pub(crate) fn increase_send_window_by(&mut self, i: u32) -> Result<(), ConnectionError> {
        self.flow_controller.increase_send_window_by(i)
    }

    pub(crate) fn consume_receive_window(&mut self, i: u32) -> Result<(), ConnectionError> {
        self.flow_controller.consume_receive_window(i)
    }
    /// A stream that is still `Open` without an ACK in either direction
    /// counts against the connection's incoming-stream budget.
    pub(super) fn is_pending_ack(&self) -> bool {
        matches!(
            self.state(),
            State::Open {
                acknowledged: false
            }
        )
    }
}
