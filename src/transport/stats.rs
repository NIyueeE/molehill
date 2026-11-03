//! Opt-in phase timers for the data path (`MOLEHILL_MHD_STATS=1`).
//!
//! The bench model prices a byte (`cpu_s_per_gbit`), and it decided that this
//! carrier costs more per byte than the TCP lane. That figure cannot say *where*
//! the cost is, and the fixes for its parts live in different files: the same
//! pass decodes a header, applies an acknowledgement, looks a stream up and
//! copies the payload. This module is the instrument that splits the figure, and
//! it is off by default so a healthy run never reads the clock for it.
//!
//! Two granularities, because a timestamp costs about twenty nanoseconds while a
//! 1400-byte datagram's whole userspace budget is a fraction of a microsecond:
//!
//! - **per datagram, one in [`SAMPLE_EVERY`]**: `rx_total`, `decode`, `ack` and
//!   `append`. The sample is uniform, so the mean is unbiased, and the
//!   perturbation stays well under the model's own floor.
//! - **per pass, always**: `tx_build` (the state machine filling a run) and
//!   `tx_send` (the syscall that posts it). A pass carries up to forty-four
//!   datagrams, so two clock reads here are noise.
//!
//! The counters are cumulative nanoseconds and event counts, logged once a
//! second like `MOLEHILL_MUX_STATS`'s framing counters: a reader who knows the
//! window takes the first and the last line and divides.

#![cfg_attr(
    not(all(feature = "mhd", feature = "noise")),
    allow(
        dead_code,
        reason = "the counters are read by the mhd carrier and the record layer; \n                 a build with neither still compiles the module, and nothing \n                 records into it"
    )
)]

use std::ffi::OsStr;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use tracing::info;

/// How many datagrams pass for every one that is timed.
///
/// Sixteen keeps the perturbation near one percent while leaving a sample every
/// few microseconds on a saturated path, which is dense enough for a mean.
pub(crate) const SAMPLE_EVERY: u64 = 16;

/// One measured phase: cumulative nanoseconds and the events they came from.
#[derive(Debug, Default)]
pub(crate) struct Meter {
    nanos: AtomicU64,
    events: AtomicU64,
}

impl Meter {
    const fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
            events: AtomicU64::new(0),
        }
    }

    /// Add one observation.
    pub(crate) fn record(&self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(nanos, Relaxed);
        self.events.fetch_add(1, Relaxed);
    }

    /// Add one observation that has no duration of its own (a count).
    pub(crate) fn count(&self) {
        self.events.fetch_add(1, Relaxed);
    }

    /// The cumulative `(nanoseconds, events)` pair.
    pub(crate) fn read(&self) -> (u64, u64) {
        (self.nanos.load(Relaxed), self.events.load(Relaxed))
    }

    /// The cumulative microseconds, for a log line.
    fn micros(&self) -> u64 {
        self.read().0 / 1_000
    }
}

/// Every counter one process keeps.
#[derive(Debug, Default)]
pub(crate) struct Phases {
    /// A sampled datagram, entry to return.
    pub(crate) rx_total: Meter,
    /// The header parse and its validation.
    pub(crate) decode: Meter,
    /// The acknowledgement block: retiring, classifying loss, rewinding.
    pub(crate) ack: Meter,
    /// The payload copy into the stream's receive buffer.
    pub(crate) append: Meter,
    /// One pass building its run, from the first `poll_transmit` to the last.
    pub(crate) tx_build: Meter,
    /// One pass posting its run to the socket.
    pub(crate) tx_send: Meter,
    /// One packet's stream selection: the scheduler's scan and the range the
    /// send buffer answered with.
    pub(crate) tx_pick: Meter,
    /// One packet's framing: the header and the payload copy into the run.
    pub(crate) tx_encode: Meter,
    /// One packet's bookkeeping: the flight table and the cursor.
    pub(crate) tx_book: Meter,
    /// One packet's whole turn: the connection lock, `poll_transmit`'s preamble
    /// and the three phases above.
    pub(crate) tx_call: Meter,
    /// A stream handle's read: the copy out of the receive buffer into the
    /// caller's slice.
    pub(crate) io_read: Meter,
    /// A stream handle's write: the copy from the caller's slice into the send
    /// buffer.
    pub(crate) io_write: Meter,
    /// How long a stream's writer stayed parked before its bytes went in.
    ///
    /// Not a copy and not a stage: the time between a `poll_write` answering
    /// `Pending` and the poll that resumes it. Nine per-stage timers all read
    /// "fast" while a stream open took seconds, because the seconds are here.
    pub(crate) write_park: Meter,
    /// Sealing one record (the Noise write path), copied and encrypted.
    pub(crate) seal: Meter,
    /// Opening one record (the Noise read path), verified and decrypted.
    pub(crate) open: Meter,
    /// Runs received, and the runs, datagrams and bytes they carried.
    pub(crate) rx_runs: Meter,
    /// Runs sent.
    pub(crate) tx_runs: Meter,
    /// Datagrams received, retransmissions included.
    pub(crate) rx_datagrams: AtomicU64,
    /// Datagrams posted, retransmissions included.
    pub(crate) tx_datagrams: AtomicU64,
    /// Payload bytes received, headers excluded.
    pub(crate) rx_bytes: AtomicU64,
    /// Payload bytes posted, headers excluded.
    pub(crate) tx_bytes: AtomicU64,
    /// Where the sampler has got to.
    sample: AtomicU64,
}

impl Phases {
    const fn new() -> Self {
        Self {
            rx_total: Meter::new(),
            decode: Meter::new(),
            ack: Meter::new(),
            append: Meter::new(),
            tx_build: Meter::new(),
            tx_send: Meter::new(),
            tx_pick: Meter::new(),
            tx_encode: Meter::new(),
            tx_book: Meter::new(),
            tx_call: Meter::new(),
            io_read: Meter::new(),
            io_write: Meter::new(),
            write_park: Meter::new(),
            seal: Meter::new(),
            open: Meter::new(),
            rx_runs: Meter::new(),
            tx_runs: Meter::new(),
            rx_datagrams: AtomicU64::new(0),
            tx_datagrams: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            sample: AtomicU64::new(0),
        }
    }

    /// Whether this datagram is one the sampler times.
    ///
    /// One decision per datagram, reused by every phase inside it: three phases
    /// that each drew their own sample would describe three different datagrams.
    pub(crate) fn timing(&self) -> bool {
        self.sample
            .fetch_add(1, Relaxed)
            .is_multiple_of(SAMPLE_EVERY)
    }
}

/// The process's counters.
pub(crate) static PHASES: Phases = Phases::new();

/// Whether the operator asked for the phase timers, read once per process.
static ENABLED: AtomicBool = AtomicBool::new(false);
static READ: Once = Once::new();

/// Whether `MOLEHILL_MHD_STATS` is set in this process.
///
/// `0` means off, so a deployment that templates the switch can turn it off
/// without unsetting it. Read once: the answer cannot change under a live
/// process, and the hot path must not call `getenv` per datagram.
pub(crate) fn enabled() -> bool {
    READ.call_once(|| {
        let set =
            std::env::var_os("MOLEHILL_MHD_STATS").is_some_and(|value| value != OsStr::new("0"));
        ENABLED.store(set, Relaxed);
    });
    ENABLED.load(Relaxed)
}

/// Count one run received and what it carried.
pub(crate) fn received_run(datagrams: usize, bytes: usize) {
    PHASES.rx_runs.count();
    PHASES
        .rx_datagrams
        .fetch_add(u64::try_from(datagrams).unwrap_or(u64::MAX), Relaxed);
    PHASES
        .rx_bytes
        .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Relaxed);
}

/// The same, for a run this end posted.
pub(crate) fn sent_run(datagrams: usize, bytes: usize) {
    PHASES.tx_runs.count();
    PHASES
        .tx_datagrams
        .fetch_add(u64::try_from(datagrams).unwrap_or(u64::MAX), Relaxed);
    PHASES
        .tx_bytes
        .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Relaxed);
}

/// The reporter's own once: one line per process, however many lanes start.
static REPORTER: Once = Once::new();

/// How long a stream's first queued byte waited for a packet, per second.
///
/// The interval between "`Conn::write` accepted these bytes" and "a packet
/// carries them": the one part of an arrival that no other instrument covers,
/// because the client's open and the server's pairing both end before or after
/// it. A per-event line at twenty milliseconds could not see a three-millisecond
/// wait, and three milliseconds is what the residual measures.
static DWELL: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// The most dwell samples one reporting window keeps.
const DWELL_WINDOW_MAX: usize = 65_536;

/// How long a caller waited for its connection's lock, per second.
///
/// The lock is `parking_lot`, so waiting for it blocks an OS thread — and with
/// one connection carrying four thousand arrivals a second, a wait here is
/// charged to every task that thread was running and to no phase at all.
static LOCK_WAIT: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Record one connection-lock acquisition's wait.
pub(crate) fn note_lock_wait(wait: Duration) {
    let us = u32::try_from(wait.as_micros()).unwrap_or(u32::MAX);
    let mut samples = LOCK_WAIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if samples.len() < DWELL_WINDOW_MAX {
        samples.push(us);
    }
}

/// When a producer last notified a pairing that a channel had arrived.
///
/// The discriminator a parked pairing cannot answer for itself: a pairing that
/// resumed long after the notify was **woken late** (the producer's wake did not
/// reach the runtime), while one that resumed promptly and then waited is a
/// queue. Two different defects with two different fixes, and the durations the
/// pairing already measures cannot tell them apart — every phase inside the path
/// reads microseconds while the wait is milliseconds.
static LAST_WAKE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Record that a producer has just handed a channel over.
pub(crate) fn note_wake() {
    *LAST_WAKE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::time::Instant::now());
}

/// How long ago the last hand-off was.
pub(crate) fn wake_age() -> Option<Duration> {
    LAST_WAKE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map(|at| at.elapsed())
}

/// How long a woken pairing waited to be polled, per second.
static RESUME: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// How long a stream's reader parked with nothing to read, per second.
///
/// The interval between "a reader asked for bytes" and "bytes arrived": the last
/// one an arrival can be spent in, and the one no phase timer sees. Its own line
/// existed at `debug!` and at twenty milliseconds, which is four times too coarse
/// for the fifteen-millisecond wait that is left.
static READ_PARK: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Record one reader's park, in microseconds.
pub(crate) fn note_read_park(wait: Duration) {
    let us = u32::try_from(wait.as_micros()).unwrap_or(u32::MAX);
    let mut samples = READ_PARK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if samples.len() < DWELL_WINDOW_MAX {
        samples.push(us);
    }
}

/// Take the window's reader parks, sorted, and leave it empty.
fn take_read_park() -> Vec<u32> {
    let mut samples = READ_PARK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken = std::mem::take(&mut *samples);
    taken.sort_unstable();
    taken
}

/// Which producer handed a visitor's channel over, per second.
///
/// The two paths look identical from outside and both used to log the same
/// message, so a measurement could not say which one a visitor took — and it
/// matters: the pooled path is the one that names a group, so an identity
/// measurement on the other path measures nothing.
static POOLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DIRECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record one channel that arrived on a pooled stream of a lane.
pub(crate) fn note_pooled_channel() {
    POOLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Record one channel that arrived as a direct mhd data channel.
pub(crate) fn note_direct_channel() {
    DIRECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many times a pairing parked waiting for its channel.
///
/// The counter that says whether the wait is a park at all. If pairings park and
/// the resume window stays empty, the instrument is wrong; if they never park,
/// the wait is somewhere the park would have been the only explanation for.
static PARKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that a pairing is about to park.
pub(crate) fn note_park() {
    PARKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Record one pairing's resume latency, in microseconds.
pub(crate) fn note_resume(wait: Duration) {
    let us = u32::try_from(wait.as_micros()).unwrap_or(u32::MAX);
    let mut samples = RESUME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if samples.len() < DWELL_WINDOW_MAX {
        samples.push(us);
    }
}

/// Take the window's resume latencies, sorted, and leave it empty.
fn take_resume() -> Vec<u32> {
    let mut samples = RESUME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken = std::mem::take(&mut *samples);
    taken.sort_unstable();
    taken
}

/// Take the window's lock waits, sorted, and leave it empty.
fn take_lock_wait() -> Vec<u32> {
    let mut samples = LOCK_WAIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken = std::mem::take(&mut *samples);
    taken.sort_unstable();
    taken
}

/// Record one stream's wait for the wire.
///
/// Takes the `Duration` and checks the switch itself, so a call site is one line
/// in a function that already has a line budget to respect.
pub(crate) fn note_dwell(wait: Duration) {
    if !enabled() {
        return;
    }
    let us = u32::try_from(wait.as_micros()).unwrap_or(u32::MAX);
    let mut samples = DWELL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if samples.len() < DWELL_WINDOW_MAX {
        samples.push(us);
    }
}

/// Take the window's dwell samples, sorted, and leave it empty.
fn take_dwell() -> Vec<u32> {
    let mut samples = DWELL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken = std::mem::take(&mut *samples);
    taken.sort_unstable();
    taken
}

/// Start the once-a-second reporter, if the switch is set.
///
/// Idempotent: every lane calls it and the first call wins, because one line per
/// process is what the switch promises.
pub(crate) fn spawn_reporter() {
    if !enabled() {
        return;
    }
    REPORTER.call_once(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            // A missed tick is not worth catching up on: the counters are
            // cumulative, so a late line still reports the true totals.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                report();
            }
        });
    });
}

/// One cumulative line, in microseconds and counts.
///
/// Its own function so the per-second windows `report` prints stay readable: a
/// single function that did both outgrew the line budget the lints enforce.
fn report_phases() {
    let (_, rx_total_n) = PHASES.rx_total.read();
    let (_, decode_n) = PHASES.decode.read();
    let (_, ack_n) = PHASES.ack.read();
    let (_, append_n) = PHASES.append.read();
    let (_, rx_runs) = PHASES.rx_runs.read();
    let (_, tx_runs) = PHASES.tx_runs.read();
    info!(
        rx_runs,
        rx_datagrams = PHASES.rx_datagrams.load(Relaxed),
        rx_bytes = PHASES.rx_bytes.load(Relaxed),
        tx_runs,
        tx_datagrams = PHASES.tx_datagrams.load(Relaxed),
        tx_bytes = PHASES.tx_bytes.load(Relaxed),
        rx_total_us = PHASES.rx_total.micros(),
        rx_total_n,
        decode_us = PHASES.decode.micros(),
        decode_n,
        ack_us = PHASES.ack.micros(),
        ack_n,
        append_us = PHASES.append.micros(),
        append_n,
        tx_build_us = PHASES.tx_build.micros(),
        tx_build_n = PHASES.tx_build.read().1,
        tx_send_us = PHASES.tx_send.micros(),
        tx_send_n = PHASES.tx_send.read().1,
        tx_call_us = PHASES.tx_call.micros(),
        tx_call_n = PHASES.tx_call.read().1,
        tx_pick_us = PHASES.tx_pick.micros(),
        tx_pick_n = PHASES.tx_pick.read().1,
        tx_encode_us = PHASES.tx_encode.micros(),
        tx_encode_n = PHASES.tx_encode.read().1,
        tx_book_us = PHASES.tx_book.micros(),
        tx_book_n = PHASES.tx_book.read().1,
        io_read_us = PHASES.io_read.micros(),
        io_read_n = PHASES.io_read.read().1,
        io_write_us = PHASES.io_write.micros(),
        io_write_n = PHASES.io_write.read().1,
        seal_us = PHASES.seal.micros(),
        seal_n = PHASES.seal.read().1,
        open_us = PHASES.open.micros(),
        open_n = PHASES.open.read().1,
        "mhd-stats: cumulative phase timings"
    );
}

/// Drain every per-second window and print the ones that saw traffic.
fn report() {
    report_phases();
    let dwell = take_dwell();
    if !dwell.is_empty() {
        let n = dwell.len();
        info!(
            n,
            dwell_p50_us = pct_us(&dwell, 50, 100),
            dwell_p99_us = pct_us(&dwell, 99, 100),
            dwell_max_us = dwell.last().copied().unwrap_or(0),
            "mhd-stats: a stream's first byte waiting for the wire"
        );
    }
    let parks = take_read_park();
    if !parks.is_empty() {
        let n = parks.len();
        info!(
            n,
            park_p50_us = pct_us(&parks, 50, 100),
            park_p99_us = pct_us(&parks, 99, 100),
            park_max_us = parks.last().copied().unwrap_or(0),
            "mhd-stats: a stream's reader waiting for bytes"
        );
    }
    let pooled = POOLED.swap(0, std::sync::atomic::Ordering::Relaxed);
    let direct = DIRECT.swap(0, std::sync::atomic::Ordering::Relaxed);
    if pooled + direct > 0 {
        info!(
            pooled,
            direct, "mhd-stats: data channels by path (pooled stream or direct)"
        );
    }
    let parks = PARKS.swap(0, std::sync::atomic::Ordering::Relaxed);
    let resume = take_resume();
    if parks > 0 {
        let n = resume.len();
        info!(
            parks,
            n,
            resume_p50_us = pct_us(&resume, 50, 100),
            resume_p99_us = pct_us(&resume, 99, 100),
            resume_max_us = resume.last().copied().unwrap_or(0),
            "mhd-stats: pairings parked, and how long they waited to be polled"
        );
    }
    if !resume.is_empty() && parks == 0 {
        let n = resume.len();
        info!(
            n,
            resume_p50_us = pct_us(&resume, 50, 100),
            resume_p99_us = pct_us(&resume, 99, 100),
            resume_max_us = resume.last().copied().unwrap_or(0),
            "mhd-stats: a pairing woken to take its channel"
        );
    }
    let lock = take_lock_wait();
    if !lock.is_empty() {
        let n = lock.len();
        info!(
            n,
            lock_p50_us = pct_us(&lock, 50, 100),
            lock_p99_us = pct_us(&lock, 99, 100),
            lock_max_us = lock.last().copied().unwrap_or(0),
            "mhd-stats: waiting for the connection lock"
        );
    }
}

/// Time `body` into `meter` when this datagram is sampled, and answer its value.
///
/// The one-line form of [`Timing`]: a phase whose body is a single statement
/// reads better wrapped than spelled out, and the timing lives in one place.
pub(crate) fn around<T>(sampled: bool, meter: &Meter, body: impl FnOnce() -> T) -> T {
    let timing = Timing::start(sampled);
    let value = body();
    timing.stop(meter);
    value
}

/// A phase timing in flight, so a reader never sees a start without its end.
pub(crate) struct Timing(Option<Instant>);

impl Timing {
    /// Start, if this datagram is one the sampler times.
    pub(crate) fn start(sampled: bool) -> Self {
        Self(sampled.then(Instant::now))
    }

    /// Stop and record into `meter`.
    pub(crate) fn stop(self, meter: &Meter) {
        if let Some(at) = self.0 {
            meter.record(at.elapsed());
        }
    }
}

/// Nearest-rank percentile of an ascending slice, in whole microseconds.
///
/// Taken as `numerator/denominator` rather than a float so the rank is integer
/// arithmetic: a percentile that is off by a rounding error is a percentile
/// nobody can reproduce. Shared by the two daemons' arrival reports, which must
/// be readable at the same scale.
pub(crate) fn pct_us(sorted: &[u32], numerator: usize, denominator: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (numerator * sorted.len()).div_ceil(denominator);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}
