//! Which carrier the data plane uses — and the fallback that decides it.
//!
//! The data plane's carrier is **not a key**. Which of the two ways to reach the
//! server works is a property of the network between the two ends, and an
//! operator cannot know it from a config file: UDP may be blocked, rate-limited,
//! mangled by a middlebox, or perfectly fine and about to change. So the client
//! tries **mhd** — the carrier that carries more on a lossy, long-haul or shaped
//! path, and the one the product's data plane is built around — and falls back
//! to **TCP**, which rides the control channel's own wire stack, when a session
//! cannot be established at all.
//!
//! The fallback is deliberately small and stateful in exactly one place:
//!
//! - an attempt is **bounded** ([`MHD_ATTEMPT_BUDGET`]): a blocked UDP path must
//!   cost a startup delay, not a hung service;
//! - a fallback is **remembered for the endpoint** ([`MHD_RETRY_COOLDOWN`]), so
//!   every later channel of that endpoint goes straight to TCP instead of paying
//!   the timeout again — and it is **re-probed** when the cooldown elapses,
//!   because a path that was blocked at 09:00 may not be at 09:05;
//! - it is **one line, once** (`INFO`, then `DEBUG`): a fallback is a fact about
//!   a network, worth telling the operator, and not worth repeating per channel.
//!
//! A build without the `mhd` feature has one carrier, TCP, and no attempt to
//! make: the same code path answers "no" to `try_mhd` and everything else is
//! unchanged, which is what keeps the feature gate from being a second data
//! plane.

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
    /// An mhd-over-UDP session (`mhd` feature).
    Mhd,
}

impl Carrier {
    /// The carrier's name, as configuration and telemetry write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Mhd => "mhd",
        }
    }
}

/// How long one mhd session gets to come up before its path is judged unable to
/// carry the carrier.
///
/// A working path completes mhd's handshake in a couple of round trips, so this
/// is generous even for a satellite link; a path that has not answered in this
/// long is one where UDP is going nowhere, and the cost of being wrong is a
/// bounded delay on the endpoint's first channel rather than a stuck service.
#[cfg(feature = "multiplex")]
pub const MHD_ATTEMPT_BUDGET: Duration = Duration::from_secs(2);

/// How long a client stays on the fallback before offering mhd another chance.
///
/// Long enough that a blocked path is not re-timed-out on every channel, short
/// enough that a network which starts passing UDP again is picked up without a
/// restart. The re-probe costs one bounded attempt, on the first channel or
/// pool established after the cooldown.
#[cfg(feature = "multiplex")]
pub const MHD_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

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
    /// `0` = mhd, `1` = TCP (see [`Carrier`]'s order).
    chosen: AtomicU8,
    /// When mhd may be tried again, in milliseconds since the Unix epoch;
    /// `0` means now.
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

    /// Whether a channel dialing now should try mhd.
    ///
    /// `false` means "go straight to TCP": either this build has no mhd, or the
    /// endpoint has fallen back and its cooldown has not elapsed. An elapsed
    /// cooldown offers mhd again — that is the re-probe — while
    /// [`Self::carrier`] keeps reporting TCP until a session actually comes up.
    #[cfg(feature = "multiplex")]
    #[must_use]
    pub fn try_mhd(&self) -> bool {
        if let Some(forced) = forced_carrier() {
            return forced == Carrier::Mhd;
        }
        cfg!(feature = "mhd")
            && (self.chosen.load(Ordering::Relaxed) == Carrier::Mhd as u8
                || now_ms() >= self.retry_at_ms.load(Ordering::Relaxed))
    }

    /// The carrier in use right now, for keys and telemetry.
    #[must_use]
    pub fn carrier(&self) -> Carrier {
        if self.chosen.load(Ordering::Relaxed) == Carrier::Mhd as u8 {
            Carrier::Mhd
        } else {
            Carrier::Tcp
        }
    }

    /// mhd could not be established. Answers whether the caller should fall
    /// back to TCP (and records the fallback when it should): a **forced** mhd
    /// carrier does not fall back, because the run asked for mhd and a failure
    /// is its answer.
    #[cfg(feature = "multiplex")]
    pub fn on_mhd_failure(&self, why: &str) -> bool {
        if forced_carrier() == Some(Carrier::Mhd) {
            return false;
        }
        self.fell_back(why);
        true
    }

    /// Use TCP for this endpoint until the cooldown elapses, and say so once.
    #[cfg(feature = "multiplex")]
    fn fell_back(&self, why: &str) {
        self.chosen.store(Carrier::Tcp as u8, Ordering::Relaxed);
        let until = now_ms()
            .saturating_add(u64::try_from(MHD_RETRY_COOLDOWN.as_millis()).unwrap_or(u64::MAX));
        self.retry_at_ms.store(until, Ordering::Relaxed);
        let endpoint = &self.endpoint;
        self.notice.report(
            || {
                tracing::info!(
                    "mhd could not be established to {endpoint} ({why}); using TCP for it and \
                     retrying mhd in {MHD_RETRY_COOLDOWN:?}. Further fallbacks for this endpoint \
                     are logged at debug level."
                );
            },
            || {
                tracing::debug!("mhd fell back to TCP for {endpoint} ({why})");
            },
        );
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
        if forced_carrier().is_some() || !cfg!(feature = "mhd") {
            // A forced carrier is the run's answer (see `forced_carrier`), and a
            // build without mhd has only one carrier to be on.
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
        // Moving to mhd is its own re-probe; moving to TCP has to hold mhd off
        // until the *quality* decision has been held, which is the longer of
        // the two: `MHD_RETRY_COOLDOWN` (60 s) is a reachability wait and
        // `SWITCH_COOLDOWN` (120 s) is how long this decision is to be kept.
        // Taking the shorter one let a re-probe that only proved "mhd came up"
        // switch the endpoint back inside the window the switch promised —
        // and `reached_mhd` then cleared the cooldown with it (see
        // [`Self::reached_mhd`], the other half of this rule).
        if carrier == Carrier::Mhd {
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
            Carrier::Mhd => "tcp",
            Carrier::Tcp => "mhd",
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

    /// A mhd session came up after a fallback: the endpoint is back on the
    /// carrier, which is worth one line because it is the opposite news.
    ///
    /// This is a **reachability** event, and it must not spend a *quality*
    /// decision's permission: a session coming up says only that UDP now
    /// connects, not that mhd carries more than the carrier a measurement
    /// moved this endpoint to. So while a deliberate switch is being held
    /// ([`SWITCH_COOLDOWN`]) the choice stays where it is — the re-probe is
    /// still over (`retry_at_ms`), but only the quality policy can act on it.
    #[cfg(feature = "multiplex")]
    pub fn reached_mhd(&self) {
        if now_ms() < self.switch_allowed_at_ms.load(Ordering::Relaxed) {
            // A switch is being held: the news is that mhd connects, and that
            // is all it is. The preference stands until the policy re-decides.
            self.retry_at_ms.store(0, Ordering::Relaxed);
            tracing::debug!(
                "mhd is reachable again at {}, but a carrier switch is being held; keeping {}",
                self.endpoint,
                self.carrier().as_str()
            );
            return;
        }
        if self.chosen.swap(Carrier::Mhd as u8, Ordering::Relaxed) == Carrier::Tcp as u8 {
            self.retry_at_ms.store(0, Ordering::Relaxed);
            tracing::info!("mhd is reachable again at {}; using it", self.endpoint);
        }
    }
}

/// The diagnostic switch that **forces** a carrier: `MOLEHILL_DATA_CARRIER=tcp`
/// or `mhd`.
///
/// It exists for tests and measurement, not for deployment: the carrier is not a
/// configuration key because a network is not a preference, but a test that
/// needs the TCP path — or an A/B that needs the two paths apart — cannot ask
/// the network to misbehave on cue. Forcing a carrier also disables the
/// fallback: an operator who asked for mhd gets mhd's failure rather than a
/// silent downgrade (which is exactly what makes it usable as an arm).
///
/// Read once per process; a value that is neither `tcp` nor `mhd` is reported
/// and ignored rather than guessed at.
#[must_use]
pub fn forced_carrier() -> Option<Carrier> {
    static FORCED: OnceLock<Option<Carrier>> = OnceLock::new();
    *FORCED.get_or_init(|| {
        let raw = std::env::var("MOLEHILL_DATA_CARRIER").ok()?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "tcp" => Some(Carrier::Tcp),
            "mhd" => Some(Carrier::Mhd),
            other => {
                tracing::warn!(
                    "MOLEHILL_DATA_CARRIER={other:?} is neither `tcp` nor `mhd`; ignoring it \
                     (the data plane chooses its carrier at runtime)"
                );
                None
            }
        }
    })
}

/// The carrier a fresh choice starts with: the forced one when the diagnostic
/// switch names it, else mhd where the feature is built in and TCP where it is
/// not.
///
/// Public because the registration declares it: the server opens the listener
/// that carrier needs, and a channel arriving on the other one is still served.
#[must_use]
pub fn first_carrier() -> Carrier {
    if let Some(forced) = forced_carrier() {
        return forced;
    }
    if cfg!(feature = "mhd") {
        Carrier::Mhd
    } else {
        Carrier::Tcp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first carrier is the feature's: mhd when it is built in, TCP when it
    /// is not (where there is nothing to fall back *from*).
    #[cfg(feature = "multiplex")]
    #[test]
    fn a_fresh_choice_starts_at_the_first_carrier() {
        let choice = CarrierChoice::new("data.example:2343");
        assert_eq!(choice.carrier(), first_carrier());
        assert_eq!(choice.try_mhd(), cfg!(feature = "mhd"));
    }

    /// A fallback is remembered and dated: the endpoint goes straight to TCP
    /// until the cooldown, and after it mhd is offered another chance.
    #[cfg(feature = "multiplex")]
    #[test]
    fn a_fallback_holds_until_the_cooldown_and_then_re_probes() {
        let choice = CarrierChoice::new("data.example:2343");
        assert!(
            choice.on_mhd_failure("test"),
            "an unforced failure falls back"
        );
        assert_eq!(choice.carrier(), Carrier::Tcp);
        assert!(!choice.try_mhd(), "the cooldown is not over");
        assert!(choice.retry_at_ms.load(Ordering::Relaxed) > now_ms());

        // The cooldown elapsing is what makes the next channel try mhd again.
        choice
            .retry_at_ms
            .store(now_ms().saturating_sub(1), Ordering::Relaxed);
        assert_eq!(
            choice.try_mhd(),
            cfg!(feature = "mhd"),
            "an elapsed cooldown offers mhd again where the feature exists"
        );
    }

    /// A deliberate switch moves the carrier, is rate-limited, and holds the
    /// new carrier against the re-probe.
    /// Needs both carriers: [`CarrierChoice::prefer`] is a no-op in a build
    /// without `mhd`, where there is only one carrier to be on.
    #[cfg(feature = "mhd")]
    #[test]
    fn a_deliberate_switch_is_rate_limited_and_moves_both_ways() {
        let choice = CarrierChoice::new("data.example:2343");
        let other = if first_carrier() == Carrier::Mhd {
            Carrier::Tcp
        } else {
            Carrier::Mhd
        };
        assert!(
            choice.prefer(other, "measured: 2x the delivered rate"),
            "the first switch is allowed"
        );
        assert_eq!(choice.carrier(), other);
        assert_eq!(
            choice.try_mhd(),
            other == Carrier::Mhd,
            "moving to TCP must hold mhd off, and moving to mhd must offer it"
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
    /// process cannot make (a forced carrier, or no mhd in the build) is
    /// refused rather than silently ignored.
    #[test]
    fn a_pointless_switch_is_refused() {
        let choice = CarrierChoice::new("data.example:2343");
        assert!(!choice.prefer(first_carrier(), "same carrier"));
        assert_eq!(choice.switch_allowed_at_ms.load(Ordering::Relaxed), 0);
    }

    /// Coming back is a state change, not a log line every time: a choice that
    /// already sits on mhd absorbs a success quietly.
    #[cfg(feature = "multiplex")]
    #[test]
    fn reaching_mhd_again_clears_the_cooldown() {
        let choice = CarrierChoice::new("data.example:2343");
        choice.on_mhd_failure("test");
        choice.reached_mhd();
        assert_eq!(choice.carrier(), Carrier::Mhd);
        assert_eq!(choice.retry_at_ms.load(Ordering::Relaxed), 0);
        choice.reached_mhd();
        assert_eq!(choice.carrier(), Carrier::Mhd);
    }

    /// **A re-probe must not spend a quality decision's permission.**
    ///
    /// The defect this pins: moving to TCP set the mhd re-probe to
    /// `MHD_RETRY_COOLDOWN` (60 s) while the switch promised to be held for
    /// `SWITCH_COOLDOWN` (120 s), so in between, a session that came up called
    /// `reached_mhd` — which stores mhd and clears the cooldown. A decision
    /// made on measured evidence was undone by an event that only ever said
    /// "UDP connects now".
    /// Needs both carriers: [`CarrierChoice::prefer`] is a no-op in a build
    /// without `mhd`, where there is only one carrier to be on.
    #[cfg(feature = "mhd")]
    #[test]
    fn a_reachable_mhd_does_not_undo_a_held_switch() {
        let choice = CarrierChoice::new("data.example:2343");
        // A quality decision moves the endpoint off mhd.
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

        // And even if a session does come up, the choice stands.
        choice.reached_mhd();
        assert_eq!(
            choice.carrier(),
            Carrier::Tcp,
            "a reachability event is not a quality measurement"
        );

        // Once the hold is over, the same event is the news it was: mhd is
        // back, and the preference follows it.
        choice
            .switch_allowed_at_ms
            .store(now_ms().saturating_sub(1), Ordering::Relaxed);
        choice.reached_mhd();
        assert_eq!(choice.carrier(), Carrier::Mhd);
    }
}
