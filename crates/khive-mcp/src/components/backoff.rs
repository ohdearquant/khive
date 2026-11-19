//! Component-local capped exponential backoff (ADR-119).
//! The supervisor owns restart eligibility, budgets and cancellation.

use std::time::SystemTime;

/// One component's base and normalized hard delay ceiling.
pub(super) struct Backoff {
    current_ms: u64,
    max_ms: u64,
}

impl Backoff {
    pub(super) fn new(initial_ms: u64, max_ms: u64) -> Self {
        let max_ms = max_ms.max(1);
        Self {
            current_ms: initial_ms.clamp(1, max_ms),
            max_ms,
        }
    }

    pub(super) fn delay_ms(&self) -> u64 {
        restart_delay_ms(self.current_ms, self.max_ms)
    }

    pub(super) fn advance(&mut self) {
        self.current_ms = self.current_ms.saturating_mul(2).min(self.max_ms);
    }
}

/// Deterministic-enough restart jitter without a rand dependency: up to a
/// quarter of the current backoff, derived from the clock's subsecond nanos.
fn jitter_ms(backoff_ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (backoff_ms / 4 + 1)
}

/// Apply positive jitter without ever exceeding the registration's hard
/// backoff cap. `backoff_max_ms` is an operator-facing maximum delay, not only
/// a cap on the un-jittered base; allowing `base + jitter` past it makes a
/// documented 60-second ceiling reach 75 seconds at steady state.
pub(super) fn restart_delay_ms(backoff_ms: u64, backoff_max_ms: u64) -> u64 {
    let cap = backoff_max_ms.max(1);
    let base = backoff_ms.min(cap);
    base.saturating_add(jitter_ms(base)).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_base_preserves_zero_and_cap_normalization() {
        for (initial, cap, expected) in [(0, 0, 1), (0, 8, 1), (20, 8, 8), (3, 8, 3)] {
            let backoff = Backoff::new(initial, cap);
            assert_eq!(backoff.current_ms, expected);
            assert!(backoff.delay_ms() >= expected);
            assert!(backoff.delay_ms() <= cap.max(1));
        }
    }

    #[test]
    fn advancement_is_capped_and_overflow_safe() {
        let mut backoff = Backoff::new(1, 8);
        for expected in [1, 2, 4, 8, 8] {
            assert_eq!(backoff.current_ms, expected);
            backoff.advance();
        }
        let mut extreme = Backoff::new(u64::MAX / 2 + 1, u64::MAX);
        extreme.advance();
        assert_eq!(extreme.current_ms, u64::MAX);
        assert_eq!(extreme.delay_ms(), u64::MAX);
    }

    #[test]
    fn advancing_one_component_leaves_another_at_its_initial_base() {
        let mut first = Backoff::new(1_000, 60_000);
        let second = Backoff::new(1_000, 60_000);
        first.advance();
        assert_eq!(first.current_ms, 2_000);
        assert_eq!(second.current_ms, 1_000);
        assert!(first.delay_ms() >= 2_000 && first.delay_ms() <= 60_000);
        assert!(second.delay_ms() >= 1_000 && second.delay_ms() <= 60_000);
    }
}
