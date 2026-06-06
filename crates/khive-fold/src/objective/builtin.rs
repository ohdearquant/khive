//! Built-in objective functions

use crate::{Objective, ObjectiveContext, Selection};

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
    half_life_seconds: f64,
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
            half_life_seconds: half_life_seconds.max(Self::MIN_HALF_LIFE),
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
        0.5f64.powf(age_seconds / self.half_life_seconds)
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
mod tests {
    use super::*;

    #[test]
    fn test_max_score_objective() {
        let objective = MaxScoreObjective::new(|n: &i32| *n as f64);

        let candidates = vec![1, 5, 3, 8, 2];
        let selection = objective
            .select(&candidates, &ObjectiveContext::new())
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(*selection.item, 8);
    }

    #[test]
    fn test_threshold_objective() {
        let objective = ThresholdObjective::new(|n: &i32| *n as f64, 5.0);

        assert!(objective.passes(&10, &ObjectiveContext::new()));
        assert!(!objective.passes(&3, &ObjectiveContext::new()));
    }

    #[test]
    fn test_threshold_objective_rejects_infinite_scores() {
        let objective = ThresholdObjective::new(|_n: &i32| f64::INFINITY, 5.0);

        assert!(!objective.passes(&10, &ObjectiveContext::new()));
    }

    #[test]
    fn test_first_match_objective() {
        let objective = FirstMatchObjective::new(|n: &i32| *n > 5);

        let candidates = vec![1, 3, 7, 9, 2];
        let selection = objective
            .select(&candidates, &ObjectiveContext::new())
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(*selection.item, 7);
        assert_eq!(selection.index, 2);
    }

    #[test]
    fn test_first_match_respects_max_candidates() {
        let objective = FirstMatchObjective::new(|n: &i32| *n > 5);

        // Match is at index 2 (value 7), but max_candidates=2 limits scan to indices 0..1.
        let candidates = vec![1, 3, 7, 9, 2];
        let context = ObjectiveContext::new().with_max_candidates(2);
        let result = objective.select(&candidates, &context);

        assert!(result.is_empty());
    }

    #[derive(Clone)]
    struct TestItem {
        _value: i32,
        timestamp: chrono::DateTime<chrono::Utc>,
        salience: f64,
    }

    impl HasTimestamp for TestItem {
        fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
            self.timestamp
        }
    }

    impl HasSalience for TestItem {
        fn salience(&self) -> f64 {
            self.salience
        }
    }

    #[test]
    fn test_recency_objective() {
        let objective = RecencyObjective::hours(1.0);
        let now = chrono::Utc::now();
        // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
        let context = ObjectiveContext::at(now);

        let old = now - chrono::Duration::hours(2);

        let new_item = TestItem {
            _value: 1,
            timestamp: now,
            salience: 0.5,
        };
        let old_item = TestItem {
            _value: 2,
            timestamp: old,
            salience: 0.5,
        };

        let new_score = objective.score(&new_item, &context);
        let old_score = objective.score(&old_item, &context);

        assert!(new_score > old_score);
        assert!((new_score - 1.0).abs() < 0.1);
    }

    #[test]
    fn test_relevance_objective() {
        let objective = RelevanceObjective::balanced(3600.0);
        let now = chrono::Utc::now();
        // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
        let context = ObjectiveContext::at(now);

        let item = TestItem {
            _value: 1,
            timestamp: now,
            salience: 0.8,
        };

        let score = objective.score(&item, &context);

        assert!(score > 0.8 && score < 1.0);
    }

    #[test]
    fn test_relevance_uses_context_relevance_score() {
        let objective = RelevanceObjective::balanced(3600.0);
        let now = chrono::Utc::now();
        // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
        let context =
            ObjectiveContext::at(now).with_extra(serde_json::json!({"relevance_score": 0.42}));

        let item = TestItem {
            _value: 1,
            timestamp: now,
            salience: 0.9,
        };

        // The context relevance_score should override the recency+salience fusion.
        let score = objective.score(&item, &context);
        assert!((score - 0.42).abs() < 1e-9);
    }

    #[test]
    #[should_panic(expected = "recency_weight must be finite and non-negative")]
    fn test_relevance_negative_recency_weight_panics() {
        RelevanceObjective::new(3600.0, -0.1, 0.5);
    }

    #[test]
    #[should_panic(expected = "salience_weight must be finite and non-negative")]
    fn test_relevance_nan_salience_weight_panics() {
        RelevanceObjective::new(3600.0, 0.5, f64::NAN);
    }

    #[test]
    #[should_panic(expected = "half_life_seconds must be positive and finite")]
    fn test_recency_zero_half_life_panics() {
        RecencyObjective::new(0.0);
    }

    #[test]
    #[should_panic(expected = "half_life_seconds must be positive and finite")]
    fn test_recency_negative_half_life_panics() {
        RecencyObjective::new(-1.0);
    }

    #[test]
    #[should_panic(expected = "half_life_seconds must be positive and finite")]
    fn test_recency_nan_half_life_panics() {
        RecencyObjective::new(f64::NAN);
    }

    #[test]
    fn test_threshold_no_match_below_threshold() {
        let objective = ThresholdObjective::new(|n: &i32| *n as f64, 10.0);

        let candidates = vec![1, 5, 3];
        let result = objective.select(&candidates, &ObjectiveContext::new());

        assert!(result.is_empty());
    }

    #[test]
    fn test_threshold_selects_best_above() {
        let objective = ThresholdObjective::new(|n: &i32| *n as f64, 5.0);

        let candidates = vec![1, 10, 3, 15];
        let selection = objective
            .select(&candidates, &ObjectiveContext::new())
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(*selection.item, 15);
        assert_eq!(selection.score, 15.0);
        assert_eq!(selection.passed, 2);
    }
}
