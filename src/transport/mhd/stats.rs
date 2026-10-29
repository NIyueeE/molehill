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
fn report() {
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
        "mhd-stats: cumulative phase timings"
    );
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
