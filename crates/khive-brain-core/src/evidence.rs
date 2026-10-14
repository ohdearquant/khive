//! Evidence-only posteriors with explicit clocks and a fixed read-time Beta(1,1) prior.
//!
//! These primitives do not select signal masses or own a persisted decay policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A finite, strictly positive evidence half-life, expressed in days.
///
/// Serializes as a number; deserialization applies the same validation as
/// [`Self::try_new`]. The default is 30 days.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct EvidenceHalfLifeDays(f64);

impl EvidenceHalfLifeDays {
    pub fn try_new(days: f64) -> Result<Self, String> {
        if !days.is_finite() || days <= 0.0 {
            return Err("evidence half-life must be finite and strictly positive".into());
        }
        Ok(Self(days))
    }

    pub fn days(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for EvidenceHalfLifeDays {
    type Error = String;

    fn try_from(days: f64) -> Result<Self, Self::Error> {
        Self::try_new(days)
    }
}

impl Default for EvidenceHalfLifeDays {
    fn default() -> Self {
        Self(30.0)
    }
}

/// Which evidence count receives a judgment's mass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidencePolarity {
    Positive,
    Negative,
}

/// Whether an observation was applied or ignored because its clock regressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceUpdate {
    Applied,
    IgnoredClockRegression,
}

/// Finite nonnegative evidence counts anchored to an explicit event time.
///
/// The counts exclude the prior. Construction requires an anchor; projection
/// never changes it. No snapshot serialization format is defined by this type.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidencePosterior {
    alpha_ev: f64,
    beta_ev: f64,
    last_event_at: DateTime<Utc>,
}

impl EvidencePosterior {
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            alpha_ev: 0.0,
            beta_ev: 0.0,
            last_event_at: at,
        }
    }

    /// Restore checked evidence without adding a prior or changing the anchor.
    pub fn try_new(alpha_ev: f64, beta_ev: f64, at: DateTime<Utc>) -> Result<Self, String> {
        if !alpha_ev.is_finite() || alpha_ev < 0.0 {
            return Err("alpha evidence must be finite and nonnegative".into());
        }
        if !beta_ev.is_finite() || beta_ev < 0.0 {
            return Err("beta evidence must be finite and nonnegative".into());
        }
        Ok(Self {
            alpha_ev,
            beta_ev,
            last_event_at: at,
        })
    }

    pub fn alpha_ev(&self) -> f64 {
        self.alpha_ev
    }

    pub fn beta_ev(&self) -> f64 {
        self.beta_ev
    }

    pub fn last_event_at(&self) -> DateTime<Utc> {
        self.last_event_at
    }

    /// Project the mean with a Beta(1,1) prior, without mutating evidence.
    ///
    /// Earlier read clocks use no decay and cannot amplify evidence. The caller
    /// supplies the applicable policy's half-life; no ambient configuration is read.
    pub fn mean_at(&self, now: DateTime<Utc>, half_life: EvidenceHalfLifeDays) -> f64 {
        let g = self.decay_factor(now, half_life);
        let alpha = g * self.alpha_ev;
        let beta = g * self.beta_ev;
        // Scaling keeps the denominator finite even when both masses are MAX.
        let scale = alpha.max(beta).max(1.0);
        (1.0 / scale + alpha / scale) / (2.0 / scale + alpha / scale + beta / scale)
    }

    /// Decay existing evidence before adding a finite, strictly positive mass.
    ///
    /// An earlier event is a successful no-op, even if its unused weight is
    /// invalid. Equal timestamps add without decay; later timestamps replace the
    /// anchor. Invalid applicable weights or overflowing additions return an error
    /// without changing any field. Finite additions use ordinary f64 rounding.
    pub fn observe_at(
        &mut self,
        polarity: EvidencePolarity,
        weight: f64,
        event_time: DateTime<Utc>,
        half_life: EvidenceHalfLifeDays,
    ) -> Result<EvidenceUpdate, String> {
        if event_time < self.last_event_at {
            return Ok(EvidenceUpdate::IgnoredClockRegression);
        }
        if !weight.is_finite() || weight <= 0.0 {
            return Err("evidence weight must be finite and strictly positive".into());
        }

        let g = self.decay_factor(event_time, half_life);
        let mut alpha_ev = g * self.alpha_ev;
        let mut beta_ev = g * self.beta_ev;
        match polarity {
            EvidencePolarity::Positive => alpha_ev += weight,
            EvidencePolarity::Negative => beta_ev += weight,
        }
        if !alpha_ev.is_finite() || !beta_ev.is_finite() {
            return Err("evidence addition must remain finite".into());
        }
        *self = Self {
            alpha_ev,
            beta_ev,
            last_event_at: event_time,
        };
        Ok(EvidenceUpdate::Applied)
    }

    fn decay_factor(&self, at: DateTime<Utc>, half_life: EvidenceHalfLifeDays) -> f64 {
        if at <= self.last_event_at {
            return 1.0;
        }
        let elapsed = at.signed_duration_since(self.last_event_at);
        // Total nanoseconds can overflow for valid, distant chrono dates.
        let days = elapsed.num_seconds() as f64 / 86_400.0
            + f64::from(elapsed.subsec_nanos()) / 86_400_000_000_000.0;
        (-days / half_life.days()).exp2()
    }
}
