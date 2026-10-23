//! Which carrier the data plane uses — and the fallback that decides it.
//!
//! The data plane's carrier is **not a key**. Which of the two ways to reach the
//! server works is a property of the network between the two ends, and an
//! operator cannot know it from a config file: UDP may be blocked, rate-limited,
//! mangled by a middlebox, or perfectly fine and about to change. So the client
//! tries **TCP** first — measured on the bench model as the faster carrier on
//! every stage of the release schedule, and the one whose wire the control
//! channel already uses — and falls back to **KCP**, the userspace ARQ over UDP,
//! when a TCP data channel cannot be established at all (a path that throttles
//! or refuses TCP where it passes UDP).
//!
//! The order is a measurement, not a preference (HANDOFF.md, 2026-10-22): with the
//! two binaries interleaved on one fingerprint, TCP carried 12.81 Gbit/s at
//! 0.624 s per Gbit and 1.80 ms p99 against KCP's 7.065, 1.736 and 3.30, and the
//! stage schedule does not reverse it (clean 6.99 → 14.94 Gbit/s, loss1 6.90 →
//! 12.97, equal on the shaper-limited cells). KCP keeps the interactive tail on a
//! lossy leg and its own evidence rule
//! ([`crate::transport::multiplex`]) still moves a pool that *is* on KCP back to
//! TCP when UDP is being policed.
//!
//! The fallback is deliberately small and stateful in exactly one place:
//!
//! - an attempt is **bounded** ([`KCP_ATTEMPT_BUDGET`]): a path where UDP goes
//!   nowhere must cost a delayed channel, not a hung service;
//! - a fallback is **remembered for the endpoint** ([`RETRY_COOLDOWN`]), so
//!   every later channel of that endpoint goes straight to the carrier that
//!   worked instead of paying the attempt again — and it is **re-probed** when
//!   the cooldown elapses, because a path that was blocked at 09:00 may not be
//!   at 09:05;
//! - it is **one line, once** (`INFO`, then `DEBUG`): a fallback is a fact about
//!   a network, worth telling the operator, and not worth repeating per channel.
//!
//! A build without the `kcp` feature has one carrier, TCP, and no attempt to
//! make: the same code path answers "there is nothing to fall back to" and
//! everything else is unchanged, which is what keeps the feature gate from being
//! a second data plane.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::logging::RepeatNotice;

/// The data-plane carrier: the physical way a tunnel or a lane reaches the peer.
///
/// Part of a pool's key: two carriers cannot share a physical tunnel, so a
/// `(session, carrier)` pool is really one pool per carrier of that session.
/// A lane names the same value in its slot, which is how the client's carrier
/// policy compares what a lane actually rides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Carrier {
    /// A connection over the control transport's TCP stack.
    Tcp,
    /// A KCP-over-UDP session (`kcp` feature).
    Kcp,
}

impl Carrier {
    /// The carrier's name, as configuration and telemetry write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Kcp => "kcp",
        }
    }
}

/// How long one KCP session gets to come up before its path is judged unable to
/// carry the carrier.
///
/// A working path completes KCP's handshake in a couple of round trips, so this
/// is generous even for a satellite link; a path that has not answered in this
/// long is one where UDP is going nowhere, and the cost of being wrong is a
/// bounded delay on the endpoint's first channel rather than a stuck service.
#[cfg(feature = "multiplex")]
pub const KCP_ATTEMPT_BUDGET: Duration = Duration::from_secs(2);

/// How long a client stays on the fallback before offering the carrier that
/// failed another chance.
///
/// Long enough that a blocked path is not re-attempted on every channel, short
/// enough that a network which starts passing it again is picked up without a
/// restart. The re-probe costs one attempt, on the first channel or pool
/// established after the cooldown.
pub const RETRY_COOLDOWN: Duration = Duration::from_secs(60);

/// The carrier that is not `carrier`.
const fn other(carrier: Carrier) -> Carrier {
    match carrier {
        Carrier::Tcp => Carrier::Kcp,
        Carrier::Kcp => Carrier::Tcp,
    }
}

/// How long one carrier is held after a **deliberate** switch.
///
/// A switch is not a fallback: the carrier in use works, it just carries less.
/// Acting on that costs a drained lane (or a handed-over tunnel), a fresh
/// handshake and a reset of the transport's own estimates, so a decision made
/// twice in a row is worse than a stable one — the cooldown is what makes the
/// decision rate-limited rather than a reaction to one bad second.
pub const SWITCH_COOLDOWN: Duration = Duration::from_secs(120);

/// One data endpoint's carrier decision, shared by every channel that dials it.
///
/// The scope is the endpoint because the network path is: two services of one
/// client dialing the same address share the decision (and the one timeout it
/// costs), while a service with its own `remote_addr` — possibly a different
/// server behind a different middlebox — makes its own.
#[derive(Debug)]
pub struct CarrierChoice {
    /// The endpoint, for the one line a fallback prints.
    endpoint: String,
    /// `0` = KCP, `1` = TCP (see [`Carrier`]'s order).
    chosen: AtomicU8,
    /// When the carrier that failed may be tried again, in milliseconds since
    /// the Unix epoch; `0` means no fallback has happened yet (which is not the
    /// same as "the cooldown elapsed" — see `retry_due`).
    retry_at_ms: AtomicU64,
    /// When the *other* carrier may be deliberately preferred, in milliseconds
    /// since the Unix epoch; `0` means now. Separate from `retry_at_ms`: a
    /// fallback and a quality switch are different decisions about the same
    /// endpoint and must not spend each other's permission.
    switch_allowed_at_ms: AtomicU64,
    /// The endpoint's notice: the first fallback at `INFO`, the rest at `DEBUG`.
    notice: RepeatNotice,
}

/// Milliseconds since the Unix epoch, saturating at `u64::MAX`.
///
/// A wall clock is the right source here and a monotonic one is not: the value
/// is compared against itself minutes later, and a clock step only moves the
/// retry, while a monotonic source would have to be anchored somewhere shared.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl CarrierChoice {
    /// A choice for one endpoint, starting at the first carrier to try.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            endpoint: endpoint.into(),
            chosen: AtomicU8::new(first_carrier() as u8),
            retry_at_ms: AtomicU64::new(0),
            switch_allowed_at_ms: AtomicU64::new(0),
            notice: RepeatNotice::new(),
        })
    }

    /// The carrier a channel dialing now should try **first**.
    ///
    /// The carrier in use, except after a fallback's cooldown has elapsed: then
    /// the carrier that failed gets one attempt again (the re-probe), and a
    /// failure through [`Self::failed`] re-arms the cooldown. [`Self::carrier`]
    /// keeps reporting the carrier in use until the attempt actually comes up,
    /// which is what makes the re-probe invisible to placement.
    #[must_use]
    pub fn preferred(&self) -> Carrier {
        if let Some(forced) = forced_carrier() {
            return forced;
        }
        if self.retry_due() {
            return other(self.carrier());
        }
        self.carrier()
    }

    /// Put the choice on `carrier` with no decision and no cooldown.
    ///
    /// A test fixture's hook: the product's first carrier is TCP, so a test that
    /// exercises the KCP side of a rule has to start there without pretending a
    /// fallback or a measurement happened.
    #[cfg(test)]
    pub(crate) fn start_on(&self, carrier: Carrier) {
        self.chosen.store(carrier as u8, Ordering::Relaxed);
        self.retry_at_ms.store(0, Ordering::Relaxed);
        self.switch_allowed_at_ms.store(0, Ordering::Relaxed);
    }

    /// Whether the failed carrier's re-probe window has opened.
    ///
    /// Zero means "no fallback has happened", which is not a due re-probe:
    /// without that distinction a channel dialed before any failure would read
    /// a zero clock as "the cooldown elapsed" and offer the carrier that has
    /// never been established.
    fn retry_due(&self) -> bool {
        match self.retry_at_ms.load(Ordering::Relaxed) {
            0 => false,
            at => now_ms() >= at,
        }
    }

    /// The carrier in use right now, for keys and telemetry.
    #[must_use]
    pub fn carrier(&self) -> Carrier {
        if self.chosen.load(Ordering::Relaxed) == Carrier::Kcp as u8 {
            Carrier::Kcp
        } else {
            Carrier::Tcp
        }
    }

    /// `carrier` could not be established: record the fallback and answer the
    /// carrier to try instead, or `None` when there is nothing to fall back to.
    ///
    /// A **forced** carrier does not fall back — the run asked for it, and its
    /// failure is that run's answer — and a build with only TCP has no other
    /// carrier to offer. The fallback is remembered for the endpoint and dated
    /// ([`RETRY_COOLDOWN`]), so the next channel does not pay the attempt again.
    pub fn failed(&self, carrier: Carrier, why: &str) -> Option<Carrier> {
        if forced_carrier().is_some() || !cfg!(feature = "kcp") {
            return None;
        }
        let next = other(carrier);
        self.chosen.store(next as u8, Ordering::Relaxed);
        let until =
            now_ms().saturating_add(u64::try_from(RETRY_COOLDOWN.as_millis()).unwrap_or(u64::MAX));
        self.retry_at_ms.store(until, Ordering::Relaxed);
        let endpoint = &self.endpoint;
        let from = carrier.as_str();
        let to = next.as_str();
        self.notice.report(
            || {
                tracing::info!(
                    "{from} could not be established to {endpoint} ({why}); using {to} for it and \
                     retrying {from} in {RETRY_COOLDOWN:?}. Further fallbacks for this endpoint \
                     are logged at debug level."
                );
            },
            || {
                tracing::debug!("{from} fell back to {to} for {endpoint} ({why})");
            },
        );
        Some(next)
    }

    /// Prefer the other carrier on **measured** evidence: the caller has seen
    /// the carrier in use carry materially less than the one it asks for.
    ///
    /// Answers whether the switch happened. It is refused while the cooldown
    /// from an earlier switch is running — the caller does not have to rate-limit
    /// itself, because a policy that flipped back and forth every tick would be
    /// worse than the carrier it was trying to leave — and refused when the
    /// endpoint is already on the requested carrier.
    ///
    /// The evidence is the caller's (`why`), printed once per switch at `INFO`
    /// and afterwards at `DEBUG`: a carrier change is a fact about a network
    /// worth one line, and a policy that oscillates is worth tracing rather than
    /// shouting about.
    pub fn prefer(&self, carrier: Carrier, why: &str) -> bool {
        if forced_carrier().is_some() || !cfg!(feature = "kcp") {
            // A forced carrier is the run's answer (see `forced_carrier`), and a
            // build without KCP has only one carrier to be on.
            return false;
        }
        if self.carrier() == carrier {
            return false;
        }
        let now = now_ms();
        if now < self.switch_allowed_at_ms.load(Ordering::Relaxed) {
            return false;
        }
        self.chosen.store(carrier as u8, Ordering::Relaxed);
        // Moving to KCP is its own re-probe; moving to TCP has to hold KCP off
        // until the *quality* decision has been held, which is the longer of
        // the two: `RETRY_COOLDOWN` (60 s) is a reachability wait and
        // `SWITCH_COOLDOWN` (120 s) is how long this decision is to be kept.
        // Taking the shorter one let a re-probe that only proved "the carrier
        // came up" switch the endpoint back inside the window the switch
        // promised — and `succeeded` then cleared the cooldown with it (see
        // [`Self::succeeded`], the other half of this rule).
        if carrier == Carrier::Kcp {
            self.retry_at_ms.store(0, Ordering::Relaxed);
        } else {
            let hold = u64::try_from(SWITCH_COOLDOWN.as_millis()).unwrap_or(u64::MAX);
            self.retry_at_ms
                .store(now.saturating_add(hold), Ordering::Relaxed);
        }
        self.switch_allowed_at_ms.store(
            now.saturating_add(u64::try_from(SWITCH_COOLDOWN.as_millis()).unwrap_or(u64::MAX)),
            Ordering::Relaxed,
        );
        let endpoint = &self.endpoint;
        let from = match carrier {
            Carrier::Kcp => "tcp",
            Carrier::Tcp => "kcp",
        };
        let to = carrier.as_str();
        self.notice.report(
            || {
                tracing::info!(
                    "Data carrier for {endpoint} switched from {from} to {to} ({why}); it will be                      held for {SWITCH_COOLDOWN:?} before another switch. Further switches for this                      endpoint are logged at debug level."
                );
            },
            || {
                tracing::debug!("Data carrier for {endpoint} switched from {from} to {to} ({why})");
            },
        );
        true
    }

    /// `carrier` came up: remember it as the carrier in use, unless a quality
    /// decision is being held.
    ///
    /// This is a **reachability** event, and it must not spend a *quality*
    /// decision's permission: a session coming up says only that the carrier
    /// connects, not that it carries more than the one a measurement moved this
    /// endpoint to. So while a deliberate switch is being held
    /// ([`SWITCH_COOLDOWN`]) the choice stays where it is — the re-probe is over
    /// (`retry_at_ms`), but only the quality policy can act on it.
    pub fn succeeded(&self, carrier: Carrier) {
        if self.carrier() == carrier {
            self.retry_at_ms.store(0, Ordering::Relaxed);
            return;
        }
        if now_ms() < self.switch_allowed_at_ms.load(Ordering::Relaxed) {
            // A switch is being held: the news is that the carrier connects,
            // and that is all it is. The preference stands until the policy
            // re-decides.
            self.retry_at_ms.store(0, Ordering::Relaxed);
            tracing::debug!(
                "{} is reachable again at {}, but a carrier switch is being held; keeping {}",
                carrier.as_str(),
                self.endpoint,
                self.carrier().as_str()
            );
            return;
        }
        self.chosen.store(carrier as u8, Ordering::Relaxed);
        self.retry_at_ms.store(0, Ordering::Relaxed);
        tracing::info!(
            "{} is carrying this endpoint again ({}); using it",
            carrier.as_str(),
            self.endpoint
        );
    }
}

/// The diagnostic switch that **forces** a carrier: `MOLEHILL_DATA_CARRIER=tcp`
/// or `kcp`.
///
/// It exists for tests and measurement, not for deployment: the carrier is not a
/// configuration key because a network is not a preference, but a test that
/// needs the TCP path — or an A/B that needs the two paths apart — cannot ask
/// the network to misbehave on cue. Forcing a carrier also disables the
/// fallback: an operator who asked for KCP gets KCP's failure rather than a
/// silent downgrade (which is exactly what makes it usable as an arm).
///
/// Read once per process; a value that is neither `tcp` nor `kcp` is reported
/// and ignored rather than guessed at.
#[must_use]
pub fn forced_carrier() -> Option<Carrier> {
    static FORCED: OnceLock<Option<Carrier>> = OnceLock::new();
    *FORCED.get_or_init(|| {
        let raw = std::env::var("MOLEHILL_DATA_CARRIER").ok()?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "tcp" => Some(Carrier::Tcp),
            "kcp" => Some(Carrier::Kcp),
            other => {
                tracing::warn!(
                    "MOLEHILL_DATA_CARRIER={other:?} is neither `tcp` nor `kcp`; ignoring it \
                     (the data plane chooses its carrier at runtime)"
                );
                None
            }
        }
    })
}

/// The carrier the registration **declares**: the forced one when the
/// diagnostic switch names it, else KCP where the feature is built in and TCP
/// where it is not.
///
/// This is the server's half of the decision, and it is not the dial order: the
/// server opens the listener a declared carrier needs, and it opens the KCP UDP
/// listener only for a client that declared KCP. A client that dials TCP first
/// but must be able to fall back declares KCP, so the fallback's listener is
/// there before it is needed — the declaration says what the client *can* use.
#[must_use]
pub fn declared_carrier() -> Carrier {
    if let Some(forced) = forced_carrier() {
        return forced;
    }
    if cfg!(feature = "kcp") {
        Carrier::Kcp
    } else {
        Carrier::Tcp
    }
}

/// The carrier a fresh choice starts with: the forced one when the diagnostic
/// switch names it, else TCP.
///
/// TCP first is a **measurement**, not a preference (HANDOFF.md, 2026-10-22):
/// with the two binaries interleaved on one fingerprint it carried 12.81 Gbit/s
/// at 0.624 s per Gbit and 1.80 ms p99 against KCP's 7.065, 1.736 and 3.30, and
/// the stage schedule does not reverse it. KCP stays the fallback for a path
/// where TCP cannot be established at all, and its own evidence rule can still
/// move a KCP-carrying pool back to TCP.
///
/// Public because the pool's repair asks it: the endpoint's choice decides what
/// the next tunnel dials. The *registration* declares
/// [`declared_carrier`] instead — the capability, not this order — because the
/// server opens the KCP listener only for a client that declared KCP.
#[must_use]
pub fn first_carrier() -> Carrier {
    forced_carrier().unwrap_or(Carrier::Tcp)
}

/// A stream a carrier connection yields: bytes, and nothing else.
///
/// The blanket impl means each carrier's own stream type goes in untouched:
/// above this line nothing names yamux or QUIC, which is what makes the two
/// carriers interchangeable.
#[cfg(feature = "quic")]
pub(crate) trait CarrierIo:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static
{
}
#[cfg(feature = "quic")]
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static> CarrierIo for T {}

/// One stream, whichever carrier produced it.
#[cfg(feature = "quic")]
pub(crate) type CarrierStream = Box<dyn CarrierIo>;

/// Why a carrier could not produce a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "quic")]
pub(crate) enum StreamError {
    /// The connection is gone; the pool repairs it.
    Closed,
    /// The peer's stream limit (or our own) is exhausted right now.
    Busy,
}

#[cfg(feature = "quic")]
impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Closed => f.write_str("the carrier connection is gone"),
            StreamError::Busy => f.write_str("the carrier has no stream available"),
        }
    }
}

#[cfg(feature = "quic")]
impl std::error::Error for StreamError {}

/// What a carrier can say about one connection.
#[derive(Debug, Clone, Copy, Default)]
#[cfg(feature = "quic")]
pub(crate) struct CarrierStats {
    /// Streams open on this connection, which is what placement spreads.
    pub(crate) streams: usize,
    /// The carrier's own round-trip estimate, when it has one.
    pub(crate) rtt: Option<std::time::Duration>,
    /// Packets the carrier itself lost on this path, when it can see them.
    pub(crate) lost_packets: u64,
}

/// One carrier connection, as the pool and the lanes see it.
///
/// What a carrier has to answer is deliberately small: hand out a stream,
/// say which carrier it is, and say whether it is still alive. Everything
/// else — how many streams are open, which one is idle, how its round trip
/// looks — is the carrier's own bookkeeping behind this line.
#[async_trait::async_trait]
#[cfg(feature = "quic")]
pub(crate) trait CarrierConn: Send + Sync {
    /// A new stream on this connection.
    ///
    /// # Errors
    ///
    /// Fails with StreamError::Closed when the connection is gone and
    /// StreamError::Busy when the peer's limit is reached for now.
    async fn open_stream(&self) -> Result<CarrierStream, StreamError>;

    /// Which carrier this connection is.
    fn carrier(&self) -> Carrier;

    /// Whether the connection still carries anything.
    fn is_alive(&self) -> bool;

    /// What this connection can report about itself.
    async fn stats(&self) -> CarrierStats;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh choice starts on TCP, which is where the registration says the
    /// data plane will be.
    #[test]
    fn a_fresh_choice_starts_on_tcp() {
        let choice = CarrierChoice::new("data.example:2343");
        assert_eq!(first_carrier(), Carrier::Tcp);
        assert_eq!(choice.carrier(), Carrier::Tcp);
        assert_eq!(choice.preferred(), Carrier::Tcp);
    }

    /// A failed carrier is remembered and dated: the endpoint goes straight to
    /// the other carrier until the cooldown, and after it the failed one is
    /// offered one attempt again.
    #[cfg(feature = "kcp")]
    #[test]
    fn a_failed_carrier_falls_back_and_re_probes_after_the_cooldown() {
        let choice = CarrierChoice::new("data.example:2343");
        assert_eq!(
            choice.failed(Carrier::Tcp, "test"),
            Some(Carrier::Kcp),
            "an unforced failure falls back to the other carrier"
        );
        assert_eq!(choice.carrier(), Carrier::Kcp);
        assert_eq!(choice.preferred(), Carrier::Kcp, "the cooldown is not over");
        assert!(choice.retry_at_ms.load(Ordering::Relaxed) > now_ms());

        // The cooldown elapsing is what offers TCP the next attempt.
        choice
            .retry_at_ms
            .store(now_ms().saturating_sub(1), Ordering::Relaxed);
        assert_eq!(
            choice.preferred(),
            Carrier::Tcp,
            "an elapsed cooldown re-probes the carrier that failed"
        );

        // And the fallback works in the other direction too: a KCP failure
        // sends the endpoint back to TCP.
        assert_eq!(choice.failed(Carrier::Kcp, "test"), Some(Carrier::Tcp));
        assert_eq!(choice.carrier(), Carrier::Tcp);
    }

    /// A carrier that never failed is never "re-probed": the zero in
    /// `retry_at_ms` means "no fallback has happened", not "the cooldown
    /// elapsed", and reading it as the latter would dial a carrier that has
    /// never carried anything.
    #[test]
    fn a_choice_without_a_fallback_does_not_re_probe() {
        let choice = CarrierChoice::new("data.example:2343");
        assert_eq!(choice.retry_at_ms.load(Ordering::Relaxed), 0);
        assert_eq!(choice.preferred(), choice.carrier());
    }

    /// A deliberate switch moves the carrier, is rate-limited, and holds the
    /// new carrier against the re-probe.
    /// Needs both carriers: [`CarrierChoice::prefer`] is a no-op in a build
    /// without `kcp`, where there is only one carrier to be on.
    #[cfg(feature = "kcp")]
    #[test]
    fn a_deliberate_switch_is_rate_limited_and_moves_both_ways() {
        let choice = CarrierChoice::new("data.example:2343");
        let other = if first_carrier() == Carrier::Kcp {
            Carrier::Tcp
        } else {
            Carrier::Kcp
        };
        assert!(
            choice.prefer(other, "measured: 2x the delivered rate"),
            "the first switch is allowed"
        );
        assert_eq!(choice.carrier(), other);
        assert_eq!(
            choice.preferred(),
            other,
            "a deliberate switch is what the next channel dials: no re-probe until \
             the switch's hold is over"
        );

        // The cooldown refuses the way back, and the reason is the switch, not
        // the carrier: nothing else was refused.
        assert!(
            !choice.prefer(first_carrier(), "changed our mind"),
            "a second switch inside the cooldown is refused"
        );
        assert_eq!(choice.carrier(), other);

        // Elapsing it allows the way back.
        choice
            .switch_allowed_at_ms
            .store(now_ms().saturating_sub(1), Ordering::Relaxed);
        assert!(choice.prefer(first_carrier(), "measured again"));
        assert_eq!(choice.carrier(), first_carrier());
    }

    /// A switch to the carrier already in use is not a switch, and one the
    /// process cannot make (a forced carrier, or no KCP in the build) is
    /// refused rather than silently ignored.
    #[test]
    fn a_pointless_switch_is_refused() {
        let choice = CarrierChoice::new("data.example:2343");
        assert!(!choice.prefer(first_carrier(), "same carrier"));
        assert_eq!(choice.switch_allowed_at_ms.load(Ordering::Relaxed), 0);
    }

    /// Coming back is a state change, not a log line every time: a carrier that
    /// already carries the endpoint absorbs a success quietly.
    #[cfg(feature = "kcp")]
    #[test]
    fn coming_back_clears_the_cooldown() {
        let choice = CarrierChoice::new("data.example:2343");
        choice.failed(Carrier::Tcp, "test");
        assert_eq!(choice.carrier(), Carrier::Kcp);
        assert!(choice.retry_at_ms.load(Ordering::Relaxed) > 0);
        choice.succeeded(Carrier::Kcp);
        assert_eq!(choice.carrier(), Carrier::Kcp);
        assert_eq!(choice.retry_at_ms.load(Ordering::Relaxed), 0);
        choice.succeeded(Carrier::Kcp);
        assert_eq!(choice.carrier(), Carrier::Kcp);
    }

    /// **A re-probe must not spend a quality decision's permission.**
    ///
    /// The defect this pins: moving a pool set the failed carrier's re-probe to
    /// `RETRY_COOLDOWN` (60 s) while the switch promised to be held for
    /// `SWITCH_COOLDOWN` (120 s), so in between, a session that came up called
    /// the reachability hook — which stored that carrier and cleared the
    /// cooldown. A decision made on measured evidence was undone by an event
    /// that only ever said "the carrier connects now".
    /// Needs both carriers: [`CarrierChoice::prefer`] is a no-op in a build
    /// without `kcp`, where there is only one carrier to be on.
    #[cfg(feature = "kcp")]
    #[test]
    fn a_reachable_carrier_does_not_undo_a_held_switch() {
        let choice = CarrierChoice::new("data.example:2343");
        // The fixture starts the endpoint on KCP, the carrier a quality
        // decision is about to leave.
        choice.start_on(Carrier::Kcp);
        assert!(choice.prefer(Carrier::Tcp, "measured: carries twice as much"));
        assert_eq!(choice.carrier(), Carrier::Tcp);

        // The re-probe is not allowed before the switch has been held: the
        // reachability wait alone (60 s) is shorter than the promise (120 s).
        // Compare the two deadlines the same `prefer` stored, not a fresh
        // wall-clock reading — sampling `now_ms()` here made the assertion
        // depend on the test reaching it inside the same millisecond.
        let switch_allowed = choice.switch_allowed_at_ms.load(Ordering::Relaxed);
        assert!(
            choice.retry_at_ms.load(Ordering::Relaxed) >= switch_allowed,
            "the re-probe must wait at least as long as the switch is held"
        );

        // And even if a KCP session does come up, the choice stands.
        choice.succeeded(Carrier::Kcp);
        assert_eq!(
            choice.carrier(),
            Carrier::Tcp,
            "a reachability event is not a quality measurement"
        );

        // Once the hold is over, the same event is the news it was: KCP is
        // carrying, and the preference follows it.
        choice
            .switch_allowed_at_ms
            .store(now_ms().saturating_sub(1), Ordering::Relaxed);
        choice.succeeded(Carrier::Kcp);
        assert_eq!(choice.carrier(), Carrier::Kcp);
    }

    /// The dial order is the choice's, not the registration's: a client dials
    /// TCP first while declaring that it can carry KCP, because the server
    /// opens the KCP listener only for a client that declared it — a fallback
    /// whose listener was never opened is not a fallback.
    #[test]
    fn the_declared_carrier_is_the_capability_not_the_dial_order() {
        assert_eq!(
            declared_carrier(),
            if cfg!(feature = "kcp") {
                Carrier::Kcp
            } else {
                Carrier::Tcp
            }
        );
        assert_eq!(first_carrier(), Carrier::Tcp);
        let choice = CarrierChoice::new("data.example:2343");
        assert_eq!(choice.preferred(), Carrier::Tcp);
        assert_eq!(choice.carrier(), Carrier::Tcp);
    }
}
