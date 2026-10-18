//! Per-writer FIFO observations; telemetry failure never becomes a storage error.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const BUCKET_EXPONENTS: [i8; 18] = [
    -12, -11, -10, -9, -8, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5,
];

// Duration has integer nanosecond resolution: floor fractional-nanosecond bounds.
const BUCKET_MAX_NANOS: [u64; 18] = {
    let mut bounds = [0; 18];
    let mut index = 0;
    while index < bounds.len() {
        bounds[index] = (1_000_000_000_u64 << index) / 4096;
        index += 1;
    }
    bounds
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Poisoned,
    CounterOverflow,
    InvariantViolation,
}

impl Unavailable {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Poisoned => "telemetry_poisoned",
            Self::CounterOverflow => "counter_overflow",
            Self::InvariantViolation => "telemetry_invariant_violation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum P95Bound {
    NoSamples,
    Finite(usize),
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistogramSnapshot {
    pub(crate) buckets: [u64; 18],
    pub(crate) overflow_count: u64,
    pub(crate) count: u64,
    pub(crate) sum: Duration,
    pub(crate) p95: P95Bound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) queue_depth: u64,
    pub(crate) queue_depth_high_water: u64,
    pub(crate) saturation_total: u64,
    pub(crate) admission_wait: HistogramSnapshot,
    pub(crate) accepted_queue_age: HistogramSnapshot,
    pub(crate) service: HistogramSnapshot,
}

#[derive(Debug, Default)]
struct Histogram {
    buckets: [u64; 18],
    overflow_count: u64,
    count: u64,
    sum: Duration,
}

impl Histogram {
    fn record(&mut self, elapsed: Duration) -> Result<(), Unavailable> {
        let count = self
            .count
            .checked_add(1)
            .ok_or(Unavailable::CounterOverflow)?;
        let sum = self
            .sum
            .checked_add(elapsed)
            .ok_or(Unavailable::CounterOverflow)?;
        let index = BUCKET_MAX_NANOS
            .iter()
            .position(|bound| elapsed.as_nanos() <= u128::from(*bound));
        let bucket = match index {
            Some(index) => &mut self.buckets[index],
            None => &mut self.overflow_count,
        };
        let next = bucket.checked_add(1).ok_or(Unavailable::CounterOverflow)?;
        *bucket = next;
        self.count = count;
        self.sum = sum;
        Ok(())
    }

    fn snapshot(&self) -> Result<HistogramSnapshot, Unavailable> {
        let total: u128 = self
            .buckets
            .iter()
            .map(|count| u128::from(*count))
            .sum::<u128>()
            + u128::from(self.overflow_count);
        if total != u128::from(self.count) || (self.count == 0 && !self.sum.is_zero()) {
            return Err(Unavailable::InvariantViolation);
        }
        let p95 = if self.count == 0 {
            P95Bound::NoSamples
        } else {
            // At most 19 u64 counts, and 95*u64::MAX, fit exactly in u128.
            let rank = (u128::from(self.count) * 95).div_ceil(100);
            let mut cumulative = 0_u128;
            self.buckets
                .iter()
                .position(|count| {
                    cumulative += u128::from(*count);
                    cumulative >= rank
                })
                .map_or(P95Bound::Overflow, P95Bound::Finite)
        };
        Ok(HistogramSnapshot {
            buckets: self.buckets,
            overflow_count: self.overflow_count,
            count: self.count,
            sum: self.sum,
            p95,
        })
    }
}

#[derive(Debug, Default)]
struct State {
    unavailable: Option<Unavailable>,
    queue_depth: u64,
    queue_depth_high_water: u64,
    saturation_total: u64,
    admission_wait: Histogram,
    accepted_queue_age: Histogram,
    service: Histogram,
}

impl State {
    fn fail(&mut self, reason: Unavailable) {
        self.unavailable.get_or_insert(reason);
    }

    fn release_pending(&mut self) {
        if self.unavailable.is_some() {
            return;
        }
        match self.queue_depth.checked_sub(1) {
            Some(depth) => self.queue_depth = depth,
            None => self.fail(Unavailable::InvariantViolation),
        }
    }

    fn snapshot(&self) -> Result<Snapshot, Unavailable> {
        Ok(Snapshot {
            queue_depth: self.queue_depth,
            queue_depth_high_water: self.queue_depth_high_water,
            saturation_total: self.saturation_total,
            admission_wait: self.admission_wait.snapshot()?,
            accepted_queue_age: self.accepted_queue_age.snapshot()?,
            service: self.service.snapshot()?,
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct WriterAdmissionMetrics {
    state: Mutex<State>,
}

impl WriterAdmissionMetrics {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Inject the disabled state without panicking or changing global panic hooks.
    /// Actual mutex poisoning and checked overflow are covered by module tests.
    #[cfg(test)]
    pub(crate) fn disable_for_test(&self, reason: Unavailable) {
        self.lock().fail(reason);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poison) => {
                let mut state = poison.into_inner();
                state.fail(Unavailable::Poisoned);
                state
            }
        }
    }

    pub(crate) fn offer(self: &Arc<Self>) -> AdmissionObservation {
        self.offer_at(Instant::now())
    }

    fn offer_at(self: &Arc<Self>, now: Instant) -> AdmissionObservation {
        let mut state = self.lock();
        if state.unavailable.is_none() {
            match state.queue_depth.checked_add(1) {
                Some(depth) => {
                    state.queue_depth = depth;
                    state.queue_depth_high_water = state.queue_depth_high_water.max(depth);
                }
                None => state.fail(Unavailable::CounterOverflow),
            }
        }
        drop(state);
        AdmissionObservation {
            metrics: Arc::clone(self),
            phase: Phase::Offered(now),
        }
    }

    pub(crate) fn record_capacity_deadline(&self) {
        let mut state = self.lock();
        if state.unavailable.is_none() {
            match state.saturation_total.checked_add(1) {
                Some(total) => state.saturation_total = total,
                None => state.fail(Unavailable::CounterOverflow),
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot, Unavailable> {
        let mut state = self.lock();
        if let Some(reason) = state.unavailable {
            return Err(reason);
        }
        let snapshot = state.snapshot();
        if let Err(reason) = snapshot {
            state.fail(reason);
        }
        snapshot
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Offered(Instant),
    Accepted(Instant),
    Executing(Instant),
    Settled,
}

#[derive(Debug)]
pub(crate) struct AdmissionObservation {
    metrics: Arc<WriterAdmissionMetrics>,
    phase: Phase,
}

impl AdmissionObservation {
    pub(crate) fn accepted(&mut self) {
        self.accepted_at(Instant::now());
    }

    fn accepted_at(&mut self, now: Instant) {
        let mut state = self.metrics.lock();
        if state.unavailable.is_some() {
            return;
        }
        let Phase::Offered(offered) = self.phase else {
            state.fail(Unavailable::InvariantViolation);
            return;
        };
        let Some(elapsed) = now.checked_duration_since(offered) else {
            state.fail(Unavailable::InvariantViolation);
            return;
        };
        if let Err(reason) = state.admission_wait.record(elapsed) {
            state.fail(reason);
        }
        self.phase = Phase::Accepted(now);
    }

    pub(crate) fn start_execution(&mut self) {
        self.start_execution_at(Instant::now());
    }

    fn start_execution_at(&mut self, now: Instant) {
        let accepted = match self.phase {
            Phase::Accepted(accepted) => accepted,
            Phase::Executing(_) | Phase::Settled => return,
            Phase::Offered(_) => {
                self.metrics.lock().fail(Unavailable::InvariantViolation);
                return;
            }
        };
        let mut state = self.metrics.lock();
        if state.unavailable.is_none() {
            match now.checked_duration_since(accepted) {
                Some(elapsed) => {
                    if let Err(reason) = state.accepted_queue_age.record(elapsed) {
                        state.fail(reason);
                    }
                    state.release_pending();
                }
                None => state.fail(Unavailable::InvariantViolation),
            }
        }
        self.phase = Phase::Executing(now);
    }

    pub(crate) fn reply_attempt(&mut self) {
        self.reply_attempt_at(Instant::now());
    }

    fn reply_attempt_at(&mut self, now: Instant) {
        let phase = std::mem::replace(&mut self.phase, Phase::Settled);
        let mut state = self.metrics.lock();
        if state.unavailable.is_some() {
            return;
        }
        match phase {
            Phase::Offered(_) | Phase::Accepted(_) => state.release_pending(),
            Phase::Executing(started) => match now.checked_duration_since(started) {
                Some(elapsed) => {
                    if let Err(reason) = state.service.record(elapsed) {
                        state.fail(reason);
                    }
                }
                None => state.fail(Unavailable::InvariantViolation),
            },
            Phase::Settled => {}
        }
    }
}

impl Drop for AdmissionObservation {
    fn drop(&mut self) {
        if matches!(self.phase, Phase::Offered(_) | Phase::Accepted(_)) {
            self.metrics.lock().release_pending();
        }
        self.phase = Phase::Settled;
    }
}

#[cfg(test)]
mod tests;
