//! Per-credential IMAP guardrail: single-flight connection cap + jittered
//! exponential backoff on connect/auth failure (#605).
//!
//! The 2026-07-04 inbound-email outage (#602) was amplified by the poll
//! loop's flat ~5s retry cadence with no per-credential concurrency cap:
//! nine concurrent pollers on `leo@khive.ai` exhausted Exchange Online's
//! per-mailbox connection slots, and the flat retry hammer kept the slots
//! saturated for ~19h. `#602`/`#610` fixed the multi-process spawn that
//! caused the concurrency; the types here make the channel degrade
//! gracefully if polling pressure ever returns.

use std::time::Duration;

use khive_channel::ChannelError;
use rand::Rng;

/// Default base delay: the first backoff step after a single failure.
pub const DEFAULT_BASE: Duration = Duration::from_secs(5);

/// Default cap: backoff never waits longer than this between attempts.
pub const DEFAULT_MAX: Duration = Duration::from_secs(600);

/// Outcome of recording one backoff-eligible failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffTick {
    /// The jittered delay the caller should sleep before the next attempt.
    pub delay: Duration,
    /// The unjittered step this attempt landed on (used to detect escalation
    /// edges; stable across jitter so it never flaps the `should_warn` bit).
    pub step: Duration,
    /// 1-based count of consecutive failures since the last success.
    pub attempt: u32,
    /// `true` exactly when `step` differs from the step logged on the
    /// previous failure -- i.e. an escalation edge, per ADR-091's
    /// `crossing_warn` discipline (log on crossing, not on every tick).
    /// Once the delay saturates at `max`, subsequent failures keep the same
    /// step and this goes `false`, so sustained pressure logs once per
    /// escalation, not once per retry.
    pub should_warn: bool,
}

/// Per-credential exponential backoff state, jittered, capped, with reset on
/// success.
///
/// One instance is owned per credential (in production, one per
/// `ImapBackoff`-keyed channel poll target); state is never shared across
/// credentials, so one mailbox's failures never throttle another's cadence.
#[derive(Debug, Clone)]
pub struct ImapBackoff {
    base: Duration,
    max: Duration,
    attempt: u32,
    last_step: Option<Duration>,
}

impl Default for ImapBackoff {
    fn default() -> Self {
        Self::new(DEFAULT_BASE, DEFAULT_MAX)
    }
}

impl ImapBackoff {
    /// Build a backoff with an explicit base delay and cap.
    ///
    /// `base` is the first step (attempt 1); each subsequent attempt doubles
    /// the previous step, clamped to `max`.
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
            last_step: None,
        }
    }

    /// Current consecutive-failure count (0 when healthy or freshly reset).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The unjittered capped exponential step for a given 1-based attempt
    /// count: `base * 2^(attempt-1)`, clamped to `max`. Attempt 0 yields
    /// `Duration::ZERO` (no failure recorded yet).
    fn step_for(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let shift = attempt.saturating_sub(1).min(32);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let base_ms = self.base.as_millis().min(u128::from(u64::MAX)) as u64;
        let scaled_ms = base_ms.saturating_mul(multiplier);
        let max_ms = self.max.as_millis().min(u128::from(u64::MAX)) as u64;
        Duration::from_millis(scaled_ms.min(max_ms))
    }

    /// Record one backoff-eligible failure (see [`is_backoff_eligible`]).
    ///
    /// Advances the attempt counter, computes the next capped exponential
    /// step, and returns a [`BackoffTick`] carrying the jittered delay to
    /// sleep plus whether this is a new escalation edge worth a `warn!` log.
    pub fn record_failure(&mut self) -> BackoffTick {
        self.attempt = self.attempt.saturating_add(1);
        let step = self.step_for(self.attempt);
        let should_warn = self.last_step != Some(step);
        self.last_step = Some(step);
        BackoffTick {
            delay: jitter(step),
            step,
            attempt: self.attempt,
            should_warn,
        }
    }

    /// Record a success: reset the attempt counter and escalation state so
    /// the next failure (if any) starts back at `base`.
    pub fn record_success(&mut self) {
        self.attempt = 0;
        self.last_step = None;
    }
}

/// Add additive jitter to `step`, up to 25% of its value.
///
/// Additive (never subtractive) so the caller-visible delay is always at
/// least the unjittered step -- callers relying on `step` as the escalation
/// floor (e.g. the ~10min cap) are never surprised by a shorter sleep.
fn jitter(step: Duration) -> Duration {
    if step.is_zero() {
        return step;
    }
    let ms = step.as_millis().min(u128::from(u64::MAX)) as u64;
    let jitter_max = (ms / 4).max(1);
    let jitter_ms = rand::thread_rng().gen_range(0..=jitter_max);
    Duration::from_millis(ms + jitter_ms)
}

/// Classify whether a [`ChannelError`] from an IMAP poll represents
/// connect/auth pressure that should back off, versus a failure that should
/// keep the normal poll cadence.
///
/// Grounded in the actual errors `crates/khive-channel-email`'s IMAP
/// connect/auth flow produces (see `connector/imap.rs::LiveImap::connect`
/// and `fetch_since`):
///
/// - [`ChannelError::Auth`] -- TLS handshake, `LOGIN`, and `XOAUTH2`
///   failures. This is exactly the credential/slot-exhaustion class from the
///   2026-07-04 outage ("User is authenticated but not connected" surfaces
///   here when Exchange rejects the handshake outright).
/// - [`ChannelError::Transport`] -- TCP connect, greeting read, `SELECT`,
///   `UID SEARCH`, and `UID FETCH` failures. Exchange's connection-slot
///   exhaustion can also surface here (a post-login command failing because
///   the mailbox has no free slot), so this class backs off too.
///
/// Not backoff-eligible:
///
/// - [`ChannelError::Config`] -- static misconfiguration; backing off would
///   only delay an operator noticing and fixing it faster.
/// - [`ChannelError::UnauthorizedSender`] -- a per-message attribution gate
///   failure, not a connectivity failure; never produced by `poll`/`connect`.
/// - [`ChannelError::InvalidEnvelope`] -- malformed data, not connectivity;
///   never produced by `poll`/`connect`.
pub fn is_backoff_eligible(err: &ChannelError) -> bool {
    matches!(err, ChannelError::Auth(_) | ChannelError::Transport(_))
}

/// Per-credential single-flight guard: at most one concurrent IMAP
/// connection attempt proceeds for a given credential. A second concurrent
/// acquisition waits for the first to finish rather than opening a second
/// connection.
///
/// Backed by a bounded semaphore of size 1 -- the ONLY way to widen the cap
/// later is to raise the semaphore's permit count, never to bypass it.
#[derive(Clone)]
pub struct ImapSingleFlight {
    sem: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Default for ImapSingleFlight {
    fn default() -> Self {
        Self::new()
    }
}

impl ImapSingleFlight {
    /// Build a new single-flight guard with exactly one permit.
    pub fn new() -> Self {
        Self {
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Acquire the single in-flight slot, waiting if another connection
    /// attempt for this credential is already in progress. The returned
    /// permit releases the slot when dropped.
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        std::sync::Arc::clone(&self.sem)
            .acquire_owned()
            .await
            .expect("ImapSingleFlight semaphore is never closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ImapBackoff: escalation, cap, reset ---

    #[test]
    fn escalates_5_10_20_40_80_160_320_capped_at_600() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        let expected_steps = [5u64, 10, 20, 40, 80, 160, 320, 600, 600, 600];
        for (i, &secs) in expected_steps.iter().enumerate() {
            let tick = b.record_failure();
            assert_eq!(
                tick.step,
                Duration::from_secs(secs),
                "attempt {} (index {i}) expected step {secs}s, got {:?}",
                tick.attempt,
                tick.step
            );
            assert_eq!(tick.attempt, (i + 1) as u32);
        }
    }

    #[test]
    fn caps_at_max_and_never_exceeds_it() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        for _ in 0..50 {
            let tick = b.record_failure();
            assert!(tick.step <= Duration::from_secs(600));
            // Jittered delay must never exceed step by more than the 25% window.
            assert!(tick.delay >= tick.step);
            assert!(tick.delay <= tick.step + tick.step / 4 + Duration::from_millis(1));
        }
    }

    #[test]
    fn resets_to_base_on_success() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        for _ in 0..5 {
            b.record_failure();
        }
        assert!(b.attempt() > 0);
        b.record_success();
        assert_eq!(b.attempt(), 0);

        let tick = b.record_failure();
        assert_eq!(
            tick.step,
            Duration::from_secs(5),
            "first failure after a success must restart at base, not resume escalation"
        );
        assert_eq!(tick.attempt, 1);
    }

    #[test]
    fn should_warn_true_on_every_new_step_false_once_capped() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(20));
        // step sequence: 5 (new), 10 (new), 20 (new, = cap), 20 (repeat, capped), 20 (repeat)
        let expected_warn = [true, true, true, false, false];
        for &want_warn in &expected_warn {
            let tick = b.record_failure();
            assert_eq!(
                tick.should_warn, want_warn,
                "attempt {}: step={:?}",
                tick.attempt, tick.step
            );
        }
    }

    #[test]
    fn should_warn_resets_after_success_then_new_failure() {
        let mut b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(20));
        assert!(b.record_failure().should_warn); // 5s, new
        assert!(b.record_failure().should_warn); // 10s, new
        assert!(b.record_failure().should_warn); // 20s, new (clamped to cap)
        assert!(!b.record_failure().should_warn); // 20s, repeat at cap -> no re-log
        b.record_success();
        let tick = b.record_failure();
        assert!(
            tick.should_warn,
            "escalation edge must re-arm after a success resets state"
        );
        assert_eq!(tick.step, Duration::from_secs(5));
    }

    #[test]
    fn attempt_zero_before_any_failure() {
        let b = ImapBackoff::new(Duration::from_secs(5), Duration::from_secs(600));
        assert_eq!(b.attempt(), 0);
    }

    #[test]
    fn custom_base_and_max_respected() {
        let mut b = ImapBackoff::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(b.record_failure().step, Duration::from_millis(100));
        assert_eq!(b.record_failure().step, Duration::from_millis(200));
        assert_eq!(b.record_failure().step, Duration::from_millis(300));
        assert_eq!(b.record_failure().step, Duration::from_millis(300));
    }

    // --- is_backoff_eligible classification ---

    #[test]
    fn auth_and_transport_are_backoff_eligible() {
        assert!(is_backoff_eligible(&ChannelError::Auth("x".into())));
        assert!(is_backoff_eligible(&ChannelError::Transport("x".into())));
    }

    #[test]
    fn config_unauthorized_invalid_envelope_are_not_backoff_eligible() {
        assert!(!is_backoff_eligible(&ChannelError::Config("x".into())));
        assert!(!is_backoff_eligible(&ChannelError::UnauthorizedSender(
            "x".into()
        )));
        assert!(!is_backoff_eligible(&ChannelError::InvalidEnvelope(
            "x".into()
        )));
    }

    // --- ImapSingleFlight: single-flight enforcement ---

    #[tokio::test]
    async fn second_acquisition_waits_for_first_to_release() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let guard = ImapSingleFlight::new();
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        let guard1 = guard.clone();
        let order1 = order.clone();
        let first = tokio::spawn(async move {
            let _permit = guard1.acquire().await;
            order1.lock().await.push("first-acquired");
            // Hold the slot long enough that a concurrent acquire is
            // observably still pending while this task runs.
            tokio::time::sleep(Duration::from_millis(50)).await;
            order1.lock().await.push("first-released");
        });

        // Give the first task a head start so it holds the permit first.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let guard2 = guard.clone();
        let order2 = order.clone();
        let second = tokio::spawn(async move {
            let _permit = guard2.acquire().await;
            order2.lock().await.push("second-acquired");
        });

        first.await.unwrap();
        second.await.unwrap();

        let log = order.lock().await;
        assert_eq!(
            log.as_slice(),
            ["first-acquired", "first-released", "second-acquired"],
            "the second acquisition must not proceed until the first releases its permit"
        );
    }

    #[tokio::test]
    async fn only_one_permit_outstanding_at_a_time() {
        let guard = ImapSingleFlight::new();
        assert_eq!(guard.sem.available_permits(), 1);
        let permit = guard.acquire().await;
        assert_eq!(guard.sem.available_permits(), 0);
        drop(permit);
        assert_eq!(guard.sem.available_permits(), 1);
    }
}
