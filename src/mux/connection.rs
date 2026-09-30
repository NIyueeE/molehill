// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

//! This module contains the `Connection` type and associated helpers.
//! A `Connection` wraps an underlying (async) I/O resource and multiplexes
//! `Stream`s over it.

mod cleanup;
mod rtt;
mod stream;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::mux::tagged_stream::TaggedStream;
use crate::mux::{
    Config, DEFAULT_CREDIT,
    error::ConnectionError,
    frame::header::{self, CONNECTION_ID, Data, GoAway, Header, Ping, StreamId, Tag, WindowUpdate},
    frame::{self, Either, Frame},
};
use crate::mux::{MAX_ACK_BACKLOG, Result};
use cleanup::Cleanup;
use nohash_hasher::IntMap;
use parking_lot::Mutex;
use std::pin::Pin;
use std::task::{Context, Waker};
use std::{fmt, sync::Arc, task::Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub use stream::{State, Stream};

/// Next connection identifier, used for debug logging.
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// How the connection is used.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode {
    /// Client to server connection.
    Client,
    /// Server to client connection.
    Server,
}

/// The connection identifier.
///
/// Sequentially generated, this is mainly intended to improve log output.
#[derive(Clone, Copy)]
pub(crate) struct Id(u32);

impl Id {
    /// Create a new connection ID.
    pub(crate) fn next() -> Self {
        Id(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

/// A Yamux connection object.
///
/// Wraps the underlying I/O resource and makes progress via its
/// [`Connection::poll_next_inbound`] method which must be called repeatedly
/// until `Ok(None)` signals EOF or an error is encountered.
#[derive(Debug)]
pub struct Connection<T> {
    inner: ConnectionState<T>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    pub fn new(socket: T, cfg: Config, mode: Mode) -> Self {
        Self {
            inner: ConnectionState::Active(Box::new(Active::new(socket, cfg, mode))),
        }
    }

    /// Poll for a new outbound stream.
    ///
    /// This function will fail if the current state does not allow opening new outbound streams.
    pub fn poll_new_outbound(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        loop {
            match std::mem::replace(&mut self.inner, ConnectionState::Poisoned) {
                ConnectionState::Active(mut active) => match active.poll_new_outbound(cx) {
                    Poll::Ready(Ok(stream)) => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Ready(Ok(stream));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Cleanup(active.cleanup(e));
                    }
                },
                ConnectionState::Cleanup(mut inner) => match Pin::new(&mut inner).poll(cx) {
                    Poll::Ready(e) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Cleanup(inner);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closed => {
                    self.inner = ConnectionState::Closed;
                    return Poll::Ready(Err(ConnectionError::Closed));
                }
                ConnectionState::Poisoned => unreachable!(),
            }
        }
    }

    /// Poll for the next inbound stream.
    ///
    /// If this function returns `None`, the underlying connection is closed.
    pub fn poll_next_inbound(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Stream>>> {
        loop {
            match std::mem::replace(&mut self.inner, ConnectionState::Poisoned) {
                ConnectionState::Active(mut active) => match active.poll(cx) {
                    Poll::Ready(Ok(stream)) => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Ready(Some(Ok(stream)));
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Cleanup(active.cleanup(e));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Cleanup(mut cleanup) => match Pin::new(&mut cleanup).poll(cx) {
                    Poll::Ready(ConnectionError::Closed) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(other) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Some(Err(other)));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Cleanup(cleanup);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closed => {
                    self.inner = ConnectionState::Closed;
                    return Poll::Ready(None);
                }
                ConnectionState::Poisoned => unreachable!(),
            }
        }
    }
}

impl<T> Drop for Connection<T> {
    fn drop(&mut self) {
        match &mut self.inner {
            ConnectionState::Active(active) => active.drop_all_streams(),
            ConnectionState::Cleanup(_) | ConnectionState::Closed | ConnectionState::Poisoned => {}
        }
    }
}

enum ConnectionState<T> {
    /// The connection is alive and healthy.
    Active(Box<Active<T>>),
    /// An error occurred and we are cleaning up our resources.
    Cleanup(Cleanup),
    /// The connection is closed.
    Closed,
    /// Something went wrong during our state transitions. Should never happen unless there is a bug.
    Poisoned,
}

impl<T> fmt::Debug for ConnectionState<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionState::Active(_) => write!(f, "Active"),
            ConnectionState::Cleanup(_) => write!(f, "Cleanup"),
            ConnectionState::Closed => write!(f, "Closed"),
            ConnectionState::Poisoned => write!(f, "Poisoned"),
        }
    }
}

/// The active state of [`Connection`].
struct Active<T> {
    id: Id,
    mode: Mode,
    config: Arc<Config>,
    socket: frame::Io<T>,
    next_id: u32,

    streams: IntMap<StreamId, Arc<Mutex<stream::Shared>>>,
    stream_receivers: Vec<TaggedStream<StreamId, StreamCommand>>,
    no_streams_waker: Option<Waker>,

    pending_read_frame: Option<Frame<()>>,
    /// Frames taken from the stream receivers but not yet handed to the
    /// socket writer. A queue rather than a single slot so the receivers
    /// are polled on every iteration: tokio's mpsc clears a receiver's
    /// waker registration when it wakes, so skipping the poll leaves the
    /// connection sleeping with no registered waker for later sends.
    pending_frames: VecDeque<Frame<()>>,
    /// The part of the queue that is connection bookkeeping rather than
    /// payload: a window update (the peer's send credit) or a stream close.
    /// Both are bodyless, and either one stuck behind megabytes of bulk data
    /// costs the peer progress it cannot recover from the payload side, so
    /// they leave before any data frame. Data frames keep their own FIFO.
    control_frames: VecDeque<Frame<()>>,
    /// Body bytes currently queued in `pending_frames` (control frames are
    /// bodyless, so they are free). This is what bounds how much one tunnel
    /// can hold in userspace — see `Active::max_pending_bytes`.
    pending_bytes: usize,
    /// The bound `pending_bytes` is held under: `max_num_streams *
    /// split_send_size`, i.e. one frame per stream of this connection's
    /// maximum. Past it no receiver is polled, so the writers park on their
    /// own per-stream channel and backpressure reaches the application that
    /// produced the bytes — one hop earlier than `split_send_size` and the
    /// per-stream window already put it.
    ///
    /// Deliberately a *queue* bound and not a BDP estimate: the engine has no
    /// bandwidth sample, and an invented rate would be a knob nobody measured
    /// (HANDOFF D15).
    max_pending_bytes: usize,
    /// Where the round-robin scan of `stream_receivers` resumes. A fixed scan
    /// order hands every turn to the lowest-numbered ready stream, which is a
    /// head-of-line delay for all the others.
    recv_cursor: usize,
    new_outbound_stream_waker: Option<Waker>,

    rtt: rtt::Rtt,

    /// A stream's `max_stream_receive_window` can grow beyond [`DEFAULT_CREDIT`], see
    /// [`Stream::next_window_update`]. This field is the sum of the bytes by which all streams'
    /// `max_stream_receive_window` have each exceeded [`DEFAULT_CREDIT`]. Used to enforce
    /// [`Config::max_connection_receive_window`].
    accumulated_max_stream_windows: Arc<Mutex<usize>>,
}
/// `Stream` to `Connection` commands.
#[derive(Debug)]
pub(crate) enum StreamCommand {
    /// A new frame should be sent to the remote.
    SendFrame(Frame<Either<Data, WindowUpdate>>),
    /// Close a stream.
    CloseStream { ack: bool },
}

/// Possible actions as a result of incoming frame handling.
#[derive(Debug)]
pub(crate) enum Action {
    /// Nothing to be done.
    None,
    /// A new stream has been opened by the remote.
    New(Stream),
    /// A ping should be answered.
    Ping(Frame<Ping>),
    /// The connection should be terminated.
    Terminate(Frame<GoAway>),
}

// The socket field is skipped because its type parameter `T` does not
// implement `Debug` — that is why this impl is manual in the first place.
// `finish_non_exhaustive` marks the omission as intentional.
impl<T> fmt::Debug for Active<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("config", &self.config)
            .field("streams", &self.streams.len())
            .field("next_id", &self.next_id)
            .field("pending_read_frame", &self.pending_read_frame)
            .field("pending_frames", &self.pending_frames)
            .field("rtt", &self.rtt)
            .finish_non_exhaustive()
    }
}

impl<T> fmt::Display for Active<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "(Connection {} {:?} (streams {}))",
            self.id,
            self.mode,
            self.streams.len()
        )
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Active<T> {
    /// Create a new `Connection` from the given I/O resource.
    fn new(socket: T, cfg: Config, mode: Mode) -> Self {
        let id = Id::next();
        tracing::debug!("new connection: {id} ({mode:?})");
        let socket = frame::Io::new(id, socket);
        // One frame per stream of *this* connection's maximum: enough that a
        // fair round over every stream is always in flight, small enough that
        // a tunnel cannot park an unbounded queue ahead of a socket that is
        // not draining. Derived from the config rather than fixed so a
        // connection configured for more streams is not throttled below one
        // frame each.
        let max_pending_bytes = cfg.max_num_streams.saturating_mul(cfg.split_send_size);
        Active {
            id,
            mode,
            config: Arc::new(cfg),
            socket,
            streams: IntMap::default(),
            stream_receivers: Vec::new(),
            no_streams_waker: None,
            next_id: match mode {
                Mode::Client => 1,
                Mode::Server => 2,
            },
            pending_read_frame: None,
            pending_frames: VecDeque::new(),
            control_frames: VecDeque::new(),
            pending_bytes: 0,
            max_pending_bytes,
            recv_cursor: 0,
            new_outbound_stream_waker: None,
            rtt: rtt::Rtt::new(),
            accumulated_max_stream_windows: Arc::default(),
        }
    }

    /// Cleanup all our resources.
    ///
    /// This should be called in the context of an unrecoverable error on the connection.
    fn cleanup(mut self, error: ConnectionError) -> Cleanup {
        self.drop_all_streams();

        Cleanup::new(self.stream_receivers, error)
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        loop {
            if self.socket.is_idle() {
                // Note `next_ping` does not register a waker and thus if not called regularly (idle
                // connection) no ping is sent. This is deliberate as an idle connection does not
                // need RTT measurements to increase its stream receive window.
                if let Some(frame) = self.rtt.next_ping() {
                    self.socket.start_frame(frame.into());
                    continue;
                }

                // Privilege pending `Pong` and `GoAway` `Frame`s over
                // `Frame`s from the receivers, then the bodyless bookkeeping
                // frames — a window update is the peer's send credit and a
                // close ends its stream — over payload.
                if let Some(frame) = self.next_queued_frame() {
                    self.socket.start_frame(frame);
                    continue;
                }
            }

            match Pin::new(&mut self.socket).poll_flush(cx)? {
                Poll::Ready(()) => {
                    // The writer just went idle. If frames are still
                    // queued, write them in this same poll instead of
                    // falling through to the socket read and returning
                    // Pending: an idle writer registers no write waker,
                    // the receivers are drained (no channel wake) and a
                    // quiet socket registers no read wake — nothing would
                    // ever wake the connection to continue, and the queued
                    // frames (window updates included) would strand the
                    // whole tunnel. The cooperative budget eventually
                    // cuts a long drain short, and the resulting deferred
                    // wake resumes it.
                    if !self.pending_frames.is_empty()
                        || !self.control_frames.is_empty()
                        || self.pending_read_frame.is_some()
                    {
                        continue;
                    }
                }
                Poll::Pending => {}
            }

            if self.queue_receiver_frames(cx) {
                continue;
            }
            if self.stream_receivers.iter().all(TaggedStream::is_done) {
                self.no_streams_waker = Some(cx.waker().clone());
            }

            if self.pending_read_frame.is_none() {
                match Pin::new(&mut self.socket).poll_next_frame(cx) {
                    Poll::Ready(Some(frame)) => {
                        match self.on_frame(frame?)? {
                            Action::None => {}
                            Action::New(stream) => {
                                tracing::trace!("{}: new inbound {} of {}", self.id, stream, self);
                                return Poll::Ready(Ok(stream));
                            }
                            Action::Ping(f) => {
                                tracing::trace!("{}/{}: pong", self.id, f.header().stream_id());
                                self.pending_read_frame.replace(f.into());
                            }
                            Action::Terminate(f) => {
                                tracing::trace!("{}: sending term", self.id);
                                self.pending_read_frame.replace(f.into());
                            }
                        }
                        continue;
                    }
                    Poll::Ready(None) => {
                        return Poll::Ready(Err(ConnectionError::Closed));
                    }
                    Poll::Pending => {}
                }
            }

            // If we make it this far, at least one of the above must have registered a waker.
            return Poll::Pending;
        }
    }

    /// Poll the stream receivers once, round-robin, and queue at most one
    /// frame. Returns whether one was taken (the caller restarts its loop so
    /// the queued frame is written in this same poll).
    ///
    /// A fixed scan order always finds the lowest-numbered ready stream first,
    /// so with N bulk streams the first one fills the queue every turn and the
    /// rest wait a full round — a head-of-line delay on every stream after the
    /// first. The scan is also where the queue's byte bound is enforced: past
    /// it no receiver is polled at all, so the writers park on their own
    /// per-stream channel and one tunnel cannot buffer more than
    /// `max_pending_bytes` in userspace.
    fn queue_receiver_frames(&mut self, cx: &mut Context<'_>) -> bool {
        let mut took_command = false;
        let receivers = self.stream_receivers.len();
        if receivers > 0 && self.pending_bytes < self.max_pending_bytes {
            let start = self.recv_cursor % receivers;
            for offset in 0..receivers {
                let i = (start + offset) % receivers;
                match self.stream_receivers[i].poll_next(cx) {
                    Poll::Ready(Some((id, Some(StreamCommand::SendFrame(frame))))) => {
                        tracing::trace!(
                            "{}/{}: sending: {}",
                            self.id,
                            frame.header().stream_id(),
                            frame.header()
                        );
                        let frame: Frame<()> = frame.into();
                        self.pending_bytes += frame.body_size();
                        if is_bookkeeping(&frame) {
                            self.control_frames.push_back(frame);
                        } else {
                            self.pending_frames.push_back(frame);
                        }
                        self.recv_cursor = (i + 1) % receivers;
                        self.wake_stream_writer(id);
                        took_command = true;
                        break;
                    }
                    Poll::Ready(Some((id, Some(StreamCommand::CloseStream { ack })))) => {
                        tracing::trace!("{}/{}: sending close", self.id, id);
                        // A close carries no payload and ends a
                        // stream: it takes the priority queue.
                        self.control_frames
                            .push_back(Frame::close_stream(id, ack).into());
                        self.recv_cursor = (i + 1) % receivers;
                        self.wake_stream_writer(id);
                        took_command = true;
                        break;
                    }
                    Poll::Ready(Some((id, None))) => {
                        if let Some(frame) = self.on_drop_stream(id) {
                            tracing::trace!("{}/{}: sending: {}", self.id, id, frame.header());
                            self.control_frames.push_back(frame);
                        }
                        self.recv_cursor = (i + 1) % receivers;
                        self.wake_stream_writer(id);
                        took_command = true;
                        break;
                    }
                    Poll::Ready(None) | Poll::Pending => {}
                }
            }
        }
        // A receiver that has reported its end (its stream was
        // dropped) is finished: futures' `SelectAll` used to drop it
        // for us, and a `Vec` grows without bound otherwise — the
        // loop above is O(receivers) per poll, so a connection that
        // serves many short-lived streams (connection churn) would
        // poll thousands of dead receivers on every poll.
        self.stream_receivers.retain(|r| !r.is_done());
        // Retaining shifts the indices the cursor referred to; one
        // wrap to the new length is enough to keep it in range.
        if !self.stream_receivers.is_empty() {
            self.recv_cursor %= self.stream_receivers.len();
        }
        took_command
    }

    fn poll_new_outbound(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        if self.streams.len() >= self.config.max_num_streams {
            tracing::error!("{}: maximum number of streams reached", self.id);
            return Poll::Ready(Err(ConnectionError::TooManyStreams));
        }

        if self.ack_backlog() >= MAX_ACK_BACKLOG {
            tracing::debug!(
                "{MAX_ACK_BACKLOG} streams waiting for ACK, registering task for wake-up until remote acknowledges at least one stream"
            );
            self.new_outbound_stream_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        tracing::trace!("{}: creating new outbound stream", self.id);

        let id = self.next_stream_id()?;
        let stream = self.make_new_outbound_stream(id);

        tracing::debug!("{}: new outbound {} of {}", self.id, stream, self);
        self.streams.insert(id, stream.clone_shared());

        Poll::Ready(Ok(stream))
    }

    /// Wake a stream's parked writers: taking this stream's command off the
    /// channel freed capacity for them.
    ///
    /// Two kinds of task park on that channel — the stream's writer (a
    /// backpressured `poll_write`) and its reader (the window update
    /// `poll_read` wants to queue) — and both are released by the same
    /// event, so both slots are woken here.
    fn wake_stream_writer(&mut self, stream_id: StreamId) {
        if let Some(s) = self.streams.get(&stream_id) {
            let mut shared = s.lock();
            if let Some(w) = shared.writer.take() {
                w.wake();
            }
            if let Some(w) = shared.reader_park.take() {
                w.wake();
            }
        }
    }

    fn on_drop_stream(&mut self, stream_id: StreamId) -> Option<Frame<()>> {
        // on_drop_stream is normally called for streams still in the map;
        // one already removed (a reset handled concurrently) has nothing
        // left to inform the remote about.
        let Some(s) = self.streams.remove(&stream_id) else {
            tracing::trace!("{}: dropping unknown stream {}", self.id, stream_id);
            return None;
        };

        tracing::trace!("{}: removing dropped stream {}", self.id, stream_id);
        let frame;
        let wakers = {
            let mut shared = s.lock();
            frame = match shared.update_state(self.id, stream_id, State::Closed) {
                // The stream was dropped without calling `poll_close`.
                // We reset the stream to inform the remote of the closure.
                State::Open { .. } => {
                    let mut header = Header::data(stream_id, 0);
                    header.rst();
                    Some(Frame::new(header))
                }
                // The stream was dropped without calling `poll_close`.
                // We have already received a FIN from remote and send one
                // back which closes the stream for good.
                State::RecvClosed => {
                    let mut header = Header::data(stream_id, 0);
                    header.fin();
                    Some(Frame::new(header))
                }
                // The stream was properly closed. We already sent our FIN frame.
                // The remote may be out of credit though and blocked on
                // writing more data. We may need to reset the stream.
                State::SendClosed => {
                    // The remote has either still credit or will be given more
                    // due to an enqueued window update or we already have
                    // inbound frames in the socket buffer which will be
                    // processed later. In any case we will reply with an RST in
                    // `Connection::on_data` because the stream will no longer
                    // be known.
                    None
                }
                // The stream was properly closed. We already have sent our FIN frame. The
                // remote end has already done so in the past.
                State::Closed => None,
            };
            (shared.reader.take(), shared.writer.take())
        };
        wake_both(&wakers);
        frame.map(Into::into)
    }

    /// Process the result of reading from the socket.
    ///
    /// Unless `frame` is `Ok(Some(_))` we will assume the connection got closed
    /// and return a corresponding error, which terminates the connection.
    /// Otherwise we process the frame and potentially return a new `Stream`
    /// if one was opened by the remote.
    fn on_frame(&mut self, frame: Frame<()>) -> Result<Action> {
        tracing::trace!("{}: received: {}", self.id, frame.header());

        if frame.header().flags().contains(header::ACK)
            && matches!(frame.header().tag(), Tag::Data | Tag::WindowUpdate)
        {
            let id = frame.header().stream_id();
            if let Some(stream) = self.streams.get(&id) {
                stream
                    .lock()
                    .update_state(self.id, id, State::Open { acknowledged: true });
            }
            if let Some(waker) = self.new_outbound_stream_waker.take() {
                waker.wake();
            }
        }

        let action = match frame.header().tag() {
            Tag::Data => self.on_data(frame.into_data()),
            Tag::WindowUpdate => self.on_window_update(&frame.into_window_update()),
            Tag::Ping => self.on_ping(&frame.into_ping()),
            Tag::GoAway => return Err(ConnectionError::Closed),
        };
        Ok(action)
    }

    fn on_data(&mut self, frame: Frame<Data>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let wakers = {
                    let mut shared = s.lock();
                    shared.update_state(self.id, stream_id, State::Closed);
                    (shared.reader.take(), shared.writer.take())
                };
                wake_both(&wakers);
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::Data) {
                tracing::error!("{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                tracing::error!("{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                // A full connection refuses the *stream*, never itself.
                //
                // This used to answer `Terminate(Frame::internal_error())`,
                // which sends a goaway and takes the whole connection down:
                // every stream on it dies at once, including the visitors a
                // proxy was carrying. That made the cap a cliff — the pool's
                // whole reason for existing is to stay below it, but a pool
                // cannot bound streams it does not own (a stalled visitor can
                // hold one for minutes), and one unlucky burst then cost every
                // visitor on the tunnel.
                //
                // The refusal is a reset of that one stream: the peer's open
                // fails, its data channel ends, and every other stream on the
                // connection keeps running. The `error!` stays — with the
                // pool's ceiling in place this should not be reached — but it
                // is now a reporting line, not a suicide note.
                tracing::error!(
                    "{}: maximum number of streams reached; refusing stream {stream_id}",
                    self.id
                );
                let mut header = Header::data(stream_id, 0);
                header.rst();
                self.pending_read_frame = Some(Frame::new(header).into());
                return Action::None;
            }
            if frame.body().len() > DEFAULT_CREDIT as usize {
                tracing::error!(
                    "{}/{}: 1st body of stream exceeds default credit",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            let stream = self.make_new_inbound_stream(stream_id, DEFAULT_CREDIT);
            {
                let mut shared = stream.shared();
                if is_finish {
                    shared.update_state(self.id, stream_id, State::RecvClosed);
                }
                if let Err(_err) = shared.consume_receive_window(frame.body_len()) {
                    tracing::error!(
                        "{}/{}: 1st body of stream exceeds default credit",
                        self.id,
                        stream_id
                    );

                    return Action::Terminate(Frame::protocol_error());
                }
                shared.buffer.push(frame.into_body());
            }
            self.streams.insert(stream_id, stream.clone_shared());
            return Action::New(stream);
        }

        if let Some(s) = self.streams.get_mut(&stream_id) {
            let mut shared = s.lock();

            if let Err(_err) = shared.consume_receive_window(frame.body_len()) {
                tracing::error!(
                    "{}/{}: frame body larger than window of stream",
                    self.id,
                    stream_id
                );

                return Action::Terminate(Frame::protocol_error());
            }

            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }

            shared.buffer.push(frame.into_body());
            let wakers = (shared.reader.take(), None);
            drop(shared);
            wake_both(&wakers);
        } else {
            tracing::trace!(
                "{}/{}: data frame for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let wakers = {
                    let mut shared = s.lock();
                    shared.update_state(self.id, stream_id, State::Closed);
                    (shared.reader.take(), shared.writer.take())
                };
                wake_both(&wakers);
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::WindowUpdate) {
                tracing::error!("{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                tracing::error!("{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                tracing::error!("{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::protocol_error());
            }

            let Some(credit) = frame.header().credit().checked_add(DEFAULT_CREDIT) else {
                tracing::error!("{}: header contains invalid credit", self.id);
                return Action::Terminate(Frame::protocol_error());
            };
            let stream = self.make_new_inbound_stream(stream_id, credit);

            if is_finish {
                stream
                    .shared()
                    .update_state(self.id, stream_id, State::RecvClosed);
            }
            self.streams.insert(stream_id, stream.clone_shared());
            return Action::New(stream);
        }

        if let Some(s) = self.streams.get_mut(&stream_id) {
            let mut shared = s.lock();
            if let Err(err) = shared.increase_send_window_by(frame.header().credit()) {
                tracing::error!(
                    "{}/{}: could not increase the send window, {err}",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }
            let wakers = (
                if is_finish {
                    shared.reader.take()
                } else {
                    None
                },
                shared.writer.take(),
            );
            drop(shared);
            wake_both(&wakers);
        } else {
            tracing::trace!(
                "{}/{}: window update for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_ping(&mut self, frame: &Frame<Ping>) -> Action {
        let stream_id = frame.header().stream_id();
        if frame.header().flags().contains(header::ACK) {
            return self.rtt.handle_pong(frame.id());
        }
        if stream_id == CONNECTION_ID || self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().id());
            hdr.ack();
            return Action::Ping(Frame::new(hdr));
        }
        tracing::debug!(
            "{}/{}: ping for unknown stream, possibly dropped earlier: {:?}",
            self.id,
            stream_id,
            frame
        );
        // We do not consider this a protocol violation and thus do not send a stream reset because
        // we may still be processing pending `StreamCommand`s of this stream that were sent before
        // it has been dropped and "garbage collected". Such a stream reset would interfere with the
        // frames that still need to be sent, causing premature stream termination for the remote.
        //
        // See https://github.com/paritytech/yamux/issues/110 for details.

        Action::None
    }

    fn make_new_inbound_stream(&mut self, id: StreamId, credit: u32) -> Stream {
        let config = self.config.clone();

        // 10 is an arbitrary number. The depth paces the producer so it
        // cannot run arbitrarily far ahead of the connection: an unbounded
        // channel lets the writers queue whole windows of frames, the
        // receiver's buffer grows with them, and `next_window_update`
        // (bytes received minus still-buffered) shrinks accordingly —
        // throttling the very sender the updates are meant to supply. The
        // yamux send window is the protocol-level backpressure; this is
        // the pacing that keeps it effective.
        let (sender, receiver) = mpsc::channel(10);
        self.stream_receivers.push(TaggedStream::new(id, receiver));
        if let Some(waker) = self.no_streams_waker.take() {
            waker.wake();
        }

        Stream::new_inbound(
            id,
            self.id,
            config,
            credit,
            sender,
            self.rtt.clone(),
            self.accumulated_max_stream_windows.clone(),
        )
    }

    fn make_new_outbound_stream(&mut self, id: StreamId) -> Stream {
        let config = self.config.clone();

        // 10 is an arbitrary number. The depth paces the producer so it
        // cannot run arbitrarily far ahead of the connection: an unbounded
        // channel lets the writers queue whole windows of frames, the
        // receiver's buffer grows with them, and `next_window_update`
        // (bytes received minus still-buffered) shrinks accordingly —
        // throttling the very sender the updates are meant to supply. The
        // yamux send window is the protocol-level backpressure; this is
        // the pacing that keeps it effective.
        let (sender, receiver) = mpsc::channel(10);
        self.stream_receivers.push(TaggedStream::new(id, receiver));
        if let Some(waker) = self.no_streams_waker.take() {
            waker.wake();
        }

        Stream::new_outbound(
            id,
            self.id,
            config,
            sender,
            self.rtt.clone(),
            self.accumulated_max_stream_windows.clone(),
        )
    }

    fn next_stream_id(&mut self) -> Result<StreamId> {
        let proposed = StreamId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(2)
            .ok_or(ConnectionError::NoMoreStreamIds)?;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server()),
        }
        Ok(proposed)
    }

    /// The ACK backlog is defined as the number of outbound streams that have not yet been acknowledged.
    fn ack_backlog(&mut self) -> usize {
        self.streams
            .iter()
            // Whether this is an outbound stream.
            //
            // Clients use odd IDs and servers use even IDs.
            // A stream is outbound if:
            //
            // - Its ID is odd and we are the client.
            // - Its ID is even and we are the server.
            .filter(|(id, _)| match self.mode {
                Mode::Client => id.is_client(),
                Mode::Server => id.is_server(),
            })
            .filter(|(_, s)| s.lock().is_pending_ack())
            .count()
    }

    // Check if the given stream ID is valid w.r.t. the provided tag and our connection mode.
    fn is_valid_remote_id(&self, id: StreamId, tag: Tag) -> bool {
        if tag == Tag::Ping || tag == Tag::GoAway {
            return id.is_session();
        }
        match self.mode {
            Mode::Client => id.is_server(),
            Mode::Server => id.is_client(),
        }
    }
}

/// Wake a stream's parked reader/writer.
///
/// Waking happens outside the stream's mutex on purpose: the woken task
/// immediately contends for the same mutex, and a wake under the lock makes
/// it spin on a multi-threaded runtime while the waker still holds it.
fn wake_both(wakers: &(Option<Waker>, Option<Waker>)) {
    if let Some(w) = wakers.0.as_ref() {
        w.wake_by_ref();
    }
    if let Some(w) = wakers.1.as_ref() {
        w.wake_by_ref();
    }
}

/// Whether a queued frame is connection bookkeeping rather than payload.
///
/// A window update carries the peer's send credit and a FIN-closed frame ends
/// its stream; neither has a body. Delaying either behind queued payload costs
/// the peer progress it cannot recover from the payload side, so both leave
/// through [`Active::control_frames`] ahead of any data frame. Everything else
/// (payload, and a stream's SYN) stays in its FIFO order.
fn is_bookkeeping(frame: &Frame<()>) -> bool {
    frame.header().tag() == Tag::WindowUpdate || frame.header().flags().contains(header::FIN)
}

impl<T> Active<T> {
    /// The next frame to hand the socket writer, in priority order: a pending
    /// pong/goaway reply, then the bodyless bookkeeping frames (window updates
    /// and stream closes), then payload in its FIFO order.
    ///
    /// Split out from [`Self::poll`] so the ordering is testable without a
    /// peer: it is the one place the three queues are read.
    fn next_queued_frame(&mut self) -> Option<Frame<()>> {
        if let Some(frame) = self.pending_read_frame.take() {
            return Some(frame);
        }
        if let Some(frame) = self.control_frames.pop_front() {
            return Some(frame);
        }
        let frame = self.pending_frames.pop_front()?;
        self.pending_bytes = self.pending_bytes.saturating_sub(frame.body_size());
        Some(frame)
    }

    /// Close and drop all `Stream`s and wake any pending `Waker`s.
    fn drop_all_streams(&mut self) {
        for (id, s) in self.streams.drain() {
            let wakers = {
                let mut shared = s.lock();
                shared.update_state(self.id, id, State::Closed);
                (shared.reader.take(), shared.writer.take())
            };
            wake_both(&wakers);
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "the test expects on values it just constructed"
    )]
    #![expect(
        clippy::panic,
        reason = "a malformed peer frame is a test failure, and an assert would bury it"
    )]
    #![expect(
        clippy::unwrap_used,
        reason = "the tests unwrap frames they just constructed"
    )]

    use super::*;
    use crate::mux::frame::header::Header;

    /// A full connection refuses the stream that would cross its cap, and keeps
    /// the connection.
    ///
    /// This pins the behaviour a shaped sweep paid for: the cap used to answer
    /// `Terminate(Frame::internal_error())`, a session-terminating goaway that
    /// took every stream on the connection down with it. A proxy's tunnel
    /// carries many visitors, so one burst over the cap cost all of them.
    ///
    /// The assertion is on the decision itself — the `Action` the connection
    /// returns for that SYN — because that is where the fatality lived, and it
    /// is testable without a peer.
    #[tokio::test]
    async fn a_cap_hit_refuses_the_stream_not_the_connection() {
        let cap = 1usize;
        let mut config = Config::default();
        config
            .set_max_num_streams(cap)
            .set_max_connection_receive_window(Some(2 * DEFAULT_CREDIT as usize));

        let (io, _peer) = tokio::io::duplex(4096);
        let mut active = Active::new(io, config, Mode::Server);

        // Fill the connection's stream table to its cap with one inbound SYN.
        let first = StreamId::new(1);
        let mut syn = Header::data(first, 0);
        syn.syn();
        let first_action = active.on_data(Frame::new(syn));
        assert!(
            matches!(first_action, Action::New(_)),
            "the first stream must simply be accepted, got {first_action:?}"
        );
        assert_eq!(active.streams.len(), cap, "the table is at its cap");

        // One more SYN: this is the moment that used to kill the tunnel.
        let extra = StreamId::new(3);
        let mut syn = Header::data(extra, 0);
        syn.syn();
        let action = active.on_data(Frame::new(syn));

        assert!(
            !matches!(action, Action::Terminate(_)),
            "a cap hit must not terminate the connection"
        );
        let queued = active
            .pending_read_frame
            .as_ref()
            .expect("the refusal is queued as a reply");
        assert!(
            queued.header().flags().contains(header::RST),
            "the refusal must be a reset: {:?}",
            queued.header()
        );
        assert_eq!(
            queued.header().stream_id(),
            extra,
            "aimed at the new stream"
        );
        assert_eq!(active.streams.len(), cap, "the cap is still exactly full");
    }

    /// A window update is the peer's send credit and a close ends its stream:
    /// both must leave before queued payload, or a tunnel that is carrying
    /// bulk delays the bookkeeping its peer needs to make progress.
    #[tokio::test]
    async fn bookkeeping_leaves_ahead_of_queued_payload() {
        let (io, _peer) = tokio::io::duplex(4096);
        let mut active = Active::new(io, Config::default(), Mode::Server);
        let id = StreamId::new(1);

        // Payload queued first, then the bookkeeping a receiver pushed while
        // that payload was waiting for the socket.
        let body = vec![7u8; 128];
        active.pending_bytes = body.len();
        active
            .pending_frames
            .push_back(Frame::data(id, body.clone()).unwrap().into());
        active
            .control_frames
            .push_back(Frame::window_update(id, 4096).into());
        active
            .control_frames
            .push_back(Frame::close_stream(id, false).into());

        let first = active.next_queued_frame().expect("the window update");
        assert_eq!(
            first.header().tag(),
            Tag::WindowUpdate,
            "credit must not queue behind payload"
        );
        let second = active.next_queued_frame().expect("the close");
        assert!(
            second.header().flags().contains(header::FIN),
            "a close must not queue behind payload either"
        );
        let third = active.next_queued_frame().expect("the payload");
        assert_eq!(third.header().tag(), Tag::Data);
        assert_eq!(third.body_size(), body.len());
        assert_eq!(
            active.pending_bytes, 0,
            "payload bytes are released as they leave"
        );
        assert!(active.next_queued_frame().is_none());
    }

    /// Payload keeps its FIFO order: the priority only ever moves bodyless
    /// bookkeeping, never reorders bytes.
    #[tokio::test]
    async fn payload_keeps_its_order_behind_the_bookkeeping() {
        let (io, _peer) = tokio::io::duplex(4096);
        let mut active = Active::new(io, Config::default(), Mode::Server);
        let id = StreamId::new(1);
        for n in 0..3u8 {
            active.pending_bytes += 16;
            active
                .pending_frames
                .push_back(Frame::data(id, vec![n; 16]).unwrap().into());
        }
        for n in 0..3u8 {
            let frame = active.next_queued_frame().expect("a payload frame");
            let data = frame.into_data();
            assert!(
                data.body().iter().all(|b| *b == n),
                "payload order changed at frame {n}"
            );
        }
        assert_eq!(active.pending_bytes, 0);
    }

    /// The queue bound must leave room for one full round of the engine's own
    /// maximum, or a single poll could not hand out even one frame per stream.
    #[tokio::test]
    async fn the_pending_bound_holds_one_frame_per_stream() {
        let (io, _peer) = tokio::io::duplex(4096);
        let cfg = Config::default();
        let streams = cfg.max_num_streams;
        let split = cfg.split_send_size;
        let active = Active::new(io, cfg, Mode::Server);
        assert!(
            active.max_pending_bytes >= streams * split,
            "the bound ({}) is under one frame per stream ({streams} x {split})",
            active.max_pending_bytes
        );
        assert!(
            active.max_pending_bytes >= split,
            "the bound can never be smaller than a single frame"
        );
    }

    /// `is_bookkeeping` is the classification the ordering above depends on;
    /// a regression here would silently send credit behind bulk again.
    #[test]
    fn bookkeeping_is_window_updates_and_closes() {
        let id = StreamId::new(1);
        assert!(is_bookkeeping(&Frame::window_update(id, 1).into()));
        assert!(is_bookkeeping(&Frame::close_stream(id, false).into()));
        assert!(!is_bookkeeping(
            &Frame::data(id, vec![0; 4]).unwrap().into()
        ));
    }

    /// A distinguishable waker, so a test can ask which task is parked.
    struct IdWaker(Arc<std::sync::atomic::AtomicU8>);

    impl std::task::Wake for IdWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(1, Ordering::SeqCst);
        }
    }

    fn id_waker() -> (Waker, Arc<std::sync::atomic::AtomicU8>) {
        let flag = Arc::new(std::sync::atomic::AtomicU8::new(0));
        (Waker::from(Arc::new(IdWaker(Arc::clone(&flag)))), flag)
    }

    /// A reader parking on a full command channel keeps the writer's waker.
    ///
    /// `poll_read` queues each window update through the same per-stream
    /// command channel `poll_write` uses, and both parks stored their waker
    /// in `Shared::writer`. A reader that parked last erased the writer's
    /// waker, so the window update that came back afterwards woke nobody:
    /// the stream's send direction slept until some unrelated resize
    /// happened to notify, and every visitor on that tunnel stalled — which
    /// is how a striped bulk transfer hung (HANDOFF.md, "the stripe
    /// livelock"). The reader now parks in its own slot, and the connection
    /// wakes both when a command leaves the channel.
    #[tokio::test]
    async fn a_readers_channel_park_keeps_the_writers_waker() {
        let mut config = Config::default();
        config
            .set_max_num_streams(2)
            .set_max_connection_receive_window(Some(4 * DEFAULT_CREDIT as usize));
        let (io, _peer) = tokio::io::duplex(4096);
        let mut active = Active::new(io, config, Mode::Server);

        // One inbound stream, as the server side of a tunnel sees one.
        let id = StreamId::new(1);
        let mut syn = Header::data(id, 0);
        syn.syn();
        let mut stream = match active.on_data(Frame::new(syn)) {
            Action::New(s) => s,
            other => panic!("the SYN must open a stream, got {other:?}"),
        };

        // Fill the stream's command channel with the reader's window updates:
        // one update per half-window drained, and the channel holds ten.
        let credit = DEFAULT_CREDIT / 2;
        for _ in 0..10 {
            {
                let mut shared = stream.shared();
                shared.buffer.push(vec![0u8; credit as usize]);
                shared
                    .consume_receive_window(credit)
                    .expect("the receive window covers the buffered chunk");
            }
            let mut payload = vec![0u8; credit as usize];
            let mut read = tokio::io::ReadBuf::new(&mut payload);
            let (waker, _) = id_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(
                matches!(
                    Pin::new(&mut stream).poll_read(&mut cx, &mut read),
                    Poll::Ready(Ok(()))
                ),
                "the buffered chunk is delivered"
            );
        }
        assert_eq!(
            stream.channel_capacity(),
            0,
            "the command channel is full, so both parks are reachable"
        );

        // The writer parks first: it needs the channel and the channel is
        // full, so its waker goes into the writer's slot.
        let (writer_waker, _) = id_waker();
        let mut cx = Context::from_waker(&writer_waker);
        let payload = vec![0u8; 64];
        assert!(
            matches!(
                Pin::new(&mut stream).poll_write(&mut cx, &payload),
                Poll::Pending
            ),
            "the writer parks on the full channel"
        );
        {
            let shared = stream.shared();
            assert!(
                shared
                    .writer
                    .as_ref()
                    .is_some_and(|w| w.will_wake(&writer_waker)),
                "the writer's waker is the one parked"
            );
        }

        // The reader parks next, with another update to queue. This is the
        // step that used to overwrite the writer's waker.
        let (reader_waker, _) = id_waker();
        let mut cx = Context::from_waker(&reader_waker);
        {
            let mut shared = stream.shared();
            shared.buffer.push(vec![0u8; credit as usize]);
            shared
                .consume_receive_window(credit)
                .expect("the receive window covers the buffered chunk");
        }
        let mut payload = vec![0u8; credit as usize];
        let mut read = tokio::io::ReadBuf::new(&mut payload);
        let _ = Pin::new(&mut stream).poll_read(&mut cx, &mut read);

        let shared = stream.shared();
        assert!(
            shared
                .writer
                .as_ref()
                .is_some_and(|w| w.will_wake(&writer_waker)),
            "a reader's channel park must not erase the writer's waker"
        );
        assert!(
            shared
                .reader_park
                .as_ref()
                .is_some_and(|w| w.will_wake(&reader_waker)),
            "the reader parks in its own slot"
        );
    }
}
