// Copyright (c) 2023 Protocol Labs.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

//! Connection round-trip time measurement

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use crate::mux::connection::Action;
use crate::mux::frame::{Frame, header::Ping};

const PING_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub(crate) struct Rtt {
    inner: Arc<Mutex<RttInner>>,
    /// Next ping identifier, used for matching pongs.
    next_id: Arc<AtomicU32>,
}

impl Rtt {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RttInner {
                rtt: None,
                state: RttState::Waiting {
                    next: Instant::now(),
                },
            })),
            next_id: Arc::new(AtomicU32::new(0)),
        }
    }

    pub(crate) fn next_ping(&mut self) -> Option<Frame<Ping>> {
        let state = &mut self.inner.lock().state;

        match state {
            RttState::AwaitingPong { .. } => return None,
            RttState::Waiting { next } => {
                if *next > Instant::now() {
                    return None;
                }
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *state = RttState::AwaitingPong {
            sent_at: Instant::now(),
            id,
        };
        tracing::debug!("sending ping {id}");
        Some(Frame::ping(id))
    }

    pub(crate) fn handle_pong(&mut self, received_id: u32) -> Action {
        let inner = &mut self.inner.lock();

        let (sent_at, expected_id) = match inner.state {
            RttState::Waiting { .. } => {
                tracing::error!("received unexpected pong {received_id}");
                return Action::Terminate(Frame::protocol_error());
            }
            RttState::AwaitingPong { sent_at, id } => (sent_at, id),
        };

        if received_id != expected_id {
            tracing::error!("received pong with {received_id} but expected {expected_id}");
            return Action::Terminate(Frame::protocol_error());
        }

        let rtt = sent_at.elapsed();
        inner.rtt = Some(rtt);
        tracing::debug!("received pong {received_id}, estimated round-trip-time {rtt:?}");

        inner.state = RttState::Waiting {
            next: Instant::now() + PING_INTERVAL,
        };

        Action::None
    }

    pub(crate) fn get(&self) -> Option<Duration> {
        self.inner.lock().rtt
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
struct RttInner {
    state: RttState,
    rtt: Option<Duration>,
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
enum RttState {
    AwaitingPong { sent_at: Instant, id: u32 },
    Waiting { next: Instant },
}
