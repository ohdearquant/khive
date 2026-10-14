//! Built-in objective functions

use crate::{Objective, ObjectiveContext, Selection};
use khive_score::{Decay, HalfLife};

/// Selects candidate with highest score.
pub struct MaxScoreObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    scorer: F,
    _phantom: std::marker::PhantomData<T>,
}

impl<T, F> MaxScoreObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    /// Create a new max score objective
    pub fn new(scorer: F) -> Self {
        Self {
            scorer,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T: Send + Sync, F> Objective<T> for MaxScoreObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    fn score(&self, candidate: &T, _context: &ObjectiveContext) -> f64 {
        (self.scorer)(candidate)
    }

    fn name(&self) -> &str {
        "MaxScoreObjective"
    }
}

/// Passes candidates above a threshold.
pub struct ThresholdObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    scorer: F,
    threshold: f64,
    _phantom: std::marker::PhantomData<T>,
}

impl<T, F> ThresholdObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    /// Create a new threshold objective
    pub fn new(scorer: F, threshold: f64) -> Self {
        Self {
            scorer,
            threshold,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T: Send + Sync, F> Objective<T> for ThresholdObjective<T, F>
where
    F: Fn(&T) -> f64 + Send + Sync,
{
    fn score(&self, candidate: &T, _context: &ObjectiveContext) -> f64 {
        (self.scorer)(candidate)
    }

    fn passes_score(&self, score: f64, context: &ObjectiveContext) -> bool {
        if !score.is_finite() {
            return false;
        }
        let passes_obj = score >= self.threshold;
        let passes_ctx = context.min_score.map(|min| score >= min).unwrap_or(true);
        passes_obj && passes_ctx
    }

    fn passes(&self, candidate: &T, context: &ObjectiveContext) -> bool {
        let score = (self.scorer)(candidate);
        self.passes_score(score, context)
    }

    fn name(&self) -> &str {
        "ThresholdObjective"
    }
}

/// Returns first candidate that passes predicate.
pub struct FirstMatchObjective<T, F>
where
    F: Fn(&T) -> bool + Send + Sync,
{
    predicate: F,
    _phantom: std::marker::PhantomData<T>,
}

impl<T, F> FirstMatchObjective<T, F>
where
    F: Fn(&T) -> bool + Send + Sync,
{
    /// Create a new first match objective
    pub fn new(predicate: F) -> Self {
        Self {
            predicate,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T: Send + Sync, F> Objective<T> for FirstMatchObjective<T, F>
where
    F: Fn(&T) -> bool + Send + Sync,
{
    fn score(&self, candidate: &T, _context: &ObjectiveContext) -> f64 {
        if (self.predicate)(candidate) {
            1.0
        } else {
            0.0
        }
    }

    fn select<'a>(&self, candidates: &'a [T], context: &ObjectiveContext) -> Vec<Selection<&'a T>> {
        if candidates.is_empty() {
            return Vec::new();
        }

        let limit = context
            .max_candidates
            .unwrap_or(candidates.len())
            .min(candidates.len());

        for (i, candidate) in candidates.iter().take(limit).enumerate() {
            if (self.predicate)(candidate) {
                if !self.passes_score(1.0, context) {
                    return Vec::new();
                }
                return vec![Selection::new(candidate, 1.0, i)
                    .with_considered(i + 1)
                    .with_passed(1)];
            }
        }

        Vec::new()
    }

    fn name(&self) -> &str {
        "FirstMatchObjective"
    }
}

/// Trait for items with timestamps
pub trait HasTimestamp {
    /// Returns the timestamp of this item
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc>;
}

/// Trait for items with salience
pub trait HasSalience {
    /// Returns the salience value of this item (0.0 to 1.0)
    fn salience(&self) -> f64;
}

/// Scores by recency (newer = higher score).
pub struct RecencyObjective {
    decay: Decay,
}

impl RecencyObjective {
    const MIN_HALF_LIFE: f64 = 1.0;

    /// Create a new recency objective. Panics if `half_life_seconds` is not positive and finite.
    pub fn new(half_life_seconds: f64) -> Self {
        assert!(
            half_life_seconds.is_finite() && half_life_seconds > 0.0,
            "half_life_seconds must be positive and finite, got {half_life_seconds}"
        );
        Self {
            decay: Decay::exponential_with_half_life(
                HalfLife::try_new(half_life_seconds.max(Self::MIN_HALF_LIFE))
                    .expect("half-life is positive and finite"),
            ),
        }
    }

    /// Create with hour half-life. Panics if `hours` is not positive and finite.
    pub fn hours(hours: f64) -> Self {
        Self::new(hours * 3600.0)
    }

    /// Create with day half-life. Panics if `days` is not positive and finite.
    pub fn days(days: f64) -> Self {
        Self::new(days * 86400.0)
    }
}

impl<T: HasTimestamp + Send + Sync> Objective<T> for RecencyObjective {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        let age_seconds = (context.as_of - candidate.timestamp()).num_seconds().max(0) as f64;
        self.decay
            .factor(age_seconds)
            .expect("timestamp age is finite")
    }

    fn name(&self) -> &str {
        "RecencyObjective"
    }
}

/// Scores by salience field.
pub struct SalienceObjective {
    min_salience: f64,
}

impl SalienceObjective {
    /// Create a new salience objective
    pub fn new() -> Self {
        Self { min_salience: 0.0 }
    }

    /// Set minimum salience
    pub fn with_min(mut self, min: f64) -> Self {
        self.min_salience = min;
        self
    }
}

impl Default for SalienceObjective {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: HasSalience + Send + Sync> Objective<T> for SalienceObjective {
    fn score(&self, candidate: &T, _context: &ObjectiveContext) -> f64 {
        let salience = candidate.salience();
        if salience >= self.min_salience {
            salience
        } else {
            0.0
        }
    }

    fn name(&self) -> &str {
        "SalienceObjective"
    }
}

/// Combines recency and salience.
pub struct RelevanceObjective {
    recency_weight: f64,
    salience_weight: f64,
    recency: RecencyObjective,
}

impl RelevanceObjective {
    /// Create a new relevance objective. Panics if either weight is negative or non-finite.
    pub fn new(recency_half_life: f64, recency_weight: f64, salience_weight: f64) -> Self {
        assert!(
            recency_weight.is_finite() && recency_weight >= 0.0,
            "recency_weight must be finite and non-negative, got {recency_weight}"
        );
        assert!(
            salience_weight.is_finite() && salience_weight >= 0.0,
            "salience_weight must be finite and non-negative, got {salience_weight}"
        );
        Self {
            recency_weight,
            salience_weight,
            recency: RecencyObjective::new(recency_half_life),
        }
    }

    /// Create with equal weights (0.5 each). Panics if `recency_half_life` is not positive and finite.
    pub fn balanced(recency_half_life: f64) -> Self {
        Self::new(recency_half_life, 0.5, 0.5)
    }
}

impl<T: HasTimestamp + HasSalience + Send + Sync> Objective<T> for RelevanceObjective {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        // If context carries a named relevance score, use it directly.
        if let Some(v) = context
            .extra
            .get("relevance_score")
            .and_then(|v| v.as_f64())
        {
            return v;
        }

        let recency_score = self.recency.score(candidate, context);
        let salience_score = candidate.salience();

        let total_weight = self.recency_weight + self.salience_weight;
        if total_weight > 0.0 {
            (self.recency_weight * recency_score + self.salience_weight * salience_score)
                / total_weight
        } else {
            0.0
        }
    }

    fn name(&self) -> &str {
        "RelevanceObjective"
    }
}

#[cfg(test)]
#[path = "builtin_tests.rs"]
mod tests;
