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
#[expect(
    clippy::missing_fields_in_debug,
    reason = "the socket's type parameter is not Debug"
)]
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
            .finish()
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

                // Privilege pending `Pong` and `GoAway` `Frame`s
                // over `Frame`s from the receivers.
                if let Some(frame) = self
                    .pending_read_frame
                    .take()
                    .or_else(|| self.pending_frames.pop_front())
                {
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
                    if !self.pending_frames.is_empty() || self.pending_read_frame.is_some() {
                        continue;
                    }
                }
                Poll::Pending => {}
            }

            {
                let mut took_command = false;
                for receiver in &mut self.stream_receivers {
                    match receiver.poll_next(cx) {
                        Poll::Ready(Some((id, Some(StreamCommand::SendFrame(frame))))) => {
                            tracing::trace!(
                                "{}/{}: sending: {}",
                                self.id,
                                frame.header().stream_id(),
                                frame.header()
                            );
                            self.pending_frames.push_back(frame.into());
                            self.wake_stream_writer(id);
                            took_command = true;
                            break;
                        }
                        Poll::Ready(Some((id, Some(StreamCommand::CloseStream { ack })))) => {
                            tracing::trace!("{}/{}: sending close", self.id, id);
                            self.pending_frames
                                .push_back(Frame::close_stream(id, ack).into());
                            self.wake_stream_writer(id);
                            took_command = true;
                            break;
                        }
                        Poll::Ready(Some((id, None))) => {
                            if let Some(frame) = self.on_drop_stream(id) {
                                tracing::trace!("{}/{}: sending: {}", self.id, id, frame.header());
                                self.pending_frames.push_back(frame);
                            }
                            self.wake_stream_writer(id);
                            took_command = true;
                            break;
                        }
                        Poll::Ready(None) | Poll::Pending => {}
                    }
                }
                // A receiver that has reported its end (its stream was
                // dropped) is finished: futures' `SelectAll` used to drop it
                // for us, and a `Vec` grows without bound otherwise — the
                // loop above is O(receivers) per poll, so a connection that
                // serves many short-lived streams (connection churn) would
                // poll thousands of dead receivers on every poll.
                self.stream_receivers.retain(|r| !r.is_done());
                if took_command {
                    // Restart the loop so the queued frame is written in
                    // this very poll: falling through to the socket read
                    // would return Pending with the frame still unsent and
                    // nothing left to wake the connection.
                    continue;
                }
                if self.stream_receivers.iter().all(TaggedStream::is_done) {
                    self.no_streams_waker = Some(cx.waker().clone());
                }
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

    /// Wake a stream writer that parked on a full command channel: taking
    /// this stream's command off the channel freed capacity for it.
    fn wake_stream_writer(&mut self, stream_id: StreamId) {
        if let Some(s) = self.streams.get(&stream_id) {
            let mut shared = s.lock();
            if let Some(w) = shared.writer.take() {
                w.wake();
            }
        }
    }

    fn on_drop_stream(&mut self, stream_id: StreamId) -> Option<Frame<()>> {
        // on_drop_stream is only called for streams still in the map.
        #[expect(
            clippy::expect_used,
            reason = "the stream is in the map by construction"
        )]
        let s = self.streams.remove(&stream_id).expect("stream not found");

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
                tracing::error!("{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::internal_error());
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

impl<T> Active<T> {
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
