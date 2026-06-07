//! Objective composition utilities

use crate::{Objective, ObjectiveContext};

/// Weighted combination of multiple objectives.
///
/// The final score is: sum(weight_i * score_i) / sum(weight_i).
/// Invalid weights (non-finite, zero, or negative) and non-finite scores are skipped.
pub struct WeightedObjective<T> {
    objectives: Vec<(Box<dyn Objective<T>>, f64)>,
}

impl<T> WeightedObjective<T> {
    /// Create a new weighted objective
    pub fn new() -> Self {
        Self {
            objectives: Vec::new(),
        }
    }

    /// Add an objective with a weight
    pub fn add(mut self, objective: Box<dyn Objective<T>>, weight: f64) -> Self {
        self.objectives.push((objective, weight));
        self
    }
}

impl<T> Default for WeightedObjective<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync> Objective<T> for WeightedObjective<T> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        if self.objectives.is_empty() {
            return 0.0;
        }

        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;

        for (objective, weight) in &self.objectives {
            let w = *weight;
            if !w.is_finite() || w <= 0.0 {
                continue;
            }

            let score = objective.score(candidate, context);
            if !score.is_finite() {
                continue;
            }

            weighted_sum += score * w;
            weight_sum += w;
        }

        if weight_sum > 0.0 {
            weighted_sum / weight_sum
        } else {
            0.0
        }
    }

    fn name(&self) -> &str {
        "WeightedObjective"
    }
}

/// Priority-based objective combination.
///
/// Evaluates objectives in priority order. If an objective gives a score
/// above the threshold, that score is used. Otherwise, falls through to
/// the next priority level.
pub struct PriorityObjective<T> {
    objectives: Vec<(Box<dyn Objective<T>>, f64)>,
    fallback: f64,
}

impl<T> PriorityObjective<T> {
    /// Create a new priority objective
    pub fn new() -> Self {
        Self {
            objectives: Vec::new(),
            fallback: 0.0,
        }
    }

    /// Add an objective with a threshold
    pub fn add(mut self, objective: Box<dyn Objective<T>>, threshold: f64) -> Self {
        self.objectives.push((objective, threshold));
        self
    }

    /// Set the fallback score
    pub fn with_fallback(mut self, score: f64) -> Self {
        self.fallback = score;
        self
    }
}

impl<T> Default for PriorityObjective<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync> Objective<T> for PriorityObjective<T> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        for (objective, threshold) in &self.objectives {
            let score = objective.score(candidate, context);
            if score.is_finite() && score >= *threshold {
                return score;
            }
        }

        self.fallback
    }

    fn name(&self) -> &str {
        "PriorityObjective"
    }
}

/// Consensus-based objective combination.
///
/// All objectives must agree (scores above threshold) for a candidate
/// to pass. The final score is the geometric mean of all sub-objective scores.
pub struct ConsensusObjective<T> {
    objectives: Vec<Box<dyn Objective<T>>>,
    threshold: f64,
}

impl<T> ConsensusObjective<T> {
    /// Create a new consensus objective
    pub fn new(threshold: f64) -> Self {
        Self {
            objectives: Vec::new(),
            threshold,
        }
    }

    /// Add an objective
    pub fn with_objective(mut self, objective: Box<dyn Objective<T>>) -> Self {
        self.objectives.push(objective);
        self
    }
}

impl<T: Send + Sync> Objective<T> for ConsensusObjective<T> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        if self.objectives.is_empty() {
            return 0.0;
        }

        let mut log_sum = 0.0f64;
        let n = self.objectives.len();

        for objective in &self.objectives {
            let score = objective.score(candidate, context);
            if !score.is_finite() || score < self.threshold {
                return 0.0;
            }
            if score <= 0.0 {
                return 0.0;
            }
            log_sum += score.ln();
        }

        // Geometric mean = exp(sum(ln(score_i)) / n)
        (log_sum / n as f64).exp()
    }

    fn name(&self) -> &str {
        "ConsensusObjective"
    }
}

/// Union objective — OR semantics.
///
/// Candidate passes if ANY objective gives a score above threshold.
/// The final score is the maximum of all scores.
pub struct UnionObjective<T> {
    objectives: Vec<Box<dyn Objective<T>>>,
}

impl<T> UnionObjective<T> {
    /// Create a new union objective
    pub fn new() -> Self {
        Self {
            objectives: Vec::new(),
        }
    }

    /// Add an objective
    pub fn with_objective(mut self, objective: Box<dyn Objective<T>>) -> Self {
        self.objectives.push(objective);
        self
    }
}

impl<T> Default for UnionObjective<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync> Objective<T> for UnionObjective<T> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        self.objectives
            .iter()
            .map(|obj| obj.score(candidate, context))
            .filter(|s| s.is_finite())
            .fold(0.0f64, |a, b| a.max(b))
    }

    fn name(&self) -> &str {
        "UnionObjective"
    }
}

/// Negation objective — inverts another objective's score.
pub struct NegateObjective<T> {
    inner: Box<dyn Objective<T>>,
}

impl<T> NegateObjective<T> {
    /// Create a negation of another objective
    pub fn new(inner: Box<dyn Objective<T>>) -> Self {
        Self { inner }
    }
}

impl<T: Send + Sync> Objective<T> for NegateObjective<T> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        1.0 - self.inner.score(candidate, context)
    }

    fn name(&self) -> &str {
        "NegateObjective"
    }
}

/// Scale objective — multiplies another objective's score by a constant factor.
pub struct ScaleObjective<O> {
    inner: O,
    factor: f64,
}

impl<O> ScaleObjective<O> {
    /// Create a scaled objective that multiplies the inner score by `factor`.
    pub fn new(inner: O, factor: f64) -> Self {
        Self { inner, factor }
    }
}

impl<T, O: Objective<T>> Objective<T> for ScaleObjective<O> {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        self.inner.score(candidate, context) * self.factor
    }

    fn name(&self) -> &str {
        "ScaleObjective"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective_fn;

    #[test]
    fn test_weighted_objective() {
        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n * 2) as f64);

        let weighted = WeightedObjective::new()
            .add(Box::new(obj1), 1.0)
            .add(Box::new(obj2), 1.0);

        let context = ObjectiveContext::new();

        assert_eq!(weighted.score(&5, &context), 7.5);
    }

    #[test]
    fn test_weighted_objective_ignores_invalid_weights() {
        let negative = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 100.0);
        let positive = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 4.0);

        let weighted = WeightedObjective::new()
            .add(Box::new(negative), -1.0)
            .add(Box::new(positive), 1.0);

        assert_eq!(weighted.score(&5, &ObjectiveContext::new()), 4.0);
    }

    #[test]
    fn test_weighted_objective_requires_positive_finite_denominator() {
        let negative = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 100.0);
        let non_finite = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 4.0);

        let weighted = WeightedObjective::new()
            .add(Box::new(negative), -1.0)
            .add(Box::new(non_finite), f64::INFINITY);

        assert_eq!(weighted.score(&5, &ObjectiveContext::new()), 0.0);
    }

    #[test]
    fn test_priority_objective() {
        let obj1 = objective_fn(
            |n: &i32, _ctx: &ObjectiveContext| {
                if *n > 10 {
                    *n as f64
                } else {
                    0.0
                }
            },
        );

        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 / 2.0);

        let priority = PriorityObjective::new()
            .add(Box::new(obj1), 5.0)
            .add(Box::new(obj2), 0.0)
            .with_fallback(-1.0);

        let context = ObjectiveContext::new();

        assert_eq!(priority.score(&15, &context), 15.0);
        assert_eq!(priority.score(&5, &context), 2.5);
    }

    #[test]
    fn test_consensus_objective() {
        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n * 2) as f64);

        let consensus = ConsensusObjective::new(5.0)
            .with_objective(Box::new(obj1))
            .with_objective(Box::new(obj2));

        let context = ObjectiveContext::new();

        // scores are 10 and 20 → geometric mean = sqrt(10 * 20) = sqrt(200) ≈ 14.142
        let score = consensus.score(&10, &context);
        let expected = (10.0f64 * 20.0f64).sqrt();
        assert!(
            (score - expected).abs() < 1e-9,
            "expected {expected}, got {score}"
        );

        // candidate 2 → scores 2 and 4, both below threshold 5 → 0.0
        assert_eq!(consensus.score(&2, &context), 0.0);
    }

    #[test]
    fn test_consensus_objective_empty() {
        let consensus: ConsensusObjective<i32> = ConsensusObjective::new(0.0);
        assert_eq!(consensus.score(&10, &ObjectiveContext::new()), 0.0);
    }

    #[test]
    fn test_consensus_objective_zero_score_returns_zero() {
        let obj1 = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 0.0f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let consensus = ConsensusObjective::new(0.0)
            .with_objective(Box::new(obj1))
            .with_objective(Box::new(obj2));

        assert_eq!(consensus.score(&10, &ObjectiveContext::new()), 0.0);
    }

    #[test]
    fn test_union_objective() {
        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| 100.0 - *n as f64);

        let union = UnionObjective::new()
            .with_objective(Box::new(obj1))
            .with_objective(Box::new(obj2));

        let context = ObjectiveContext::new();

        assert_eq!(union.score(&30, &context), 70.0);
        assert_eq!(union.score(&80, &context), 80.0);
    }

    #[test]
    fn test_negate_objective() {
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 / 100.0);
        let negated = NegateObjective::new(Box::new(obj));

        let context = ObjectiveContext::new();

        assert!((negated.score(&30, &context) - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_scale_objective() {
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let scaled = ScaleObjective::new(obj, 2.0);

        let context = ObjectiveContext::new();

        assert!((scaled.score(&0, &context) - 0.0).abs() < 0.001);
        assert!((scaled.score(&5, &context) - 10.0).abs() < 0.001);
        assert!((scaled.score(&10, &context) - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_scale_objective_negative_factor() {
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let scaled = ScaleObjective::new(obj, -1.0);

        let context = ObjectiveContext::new();

        assert!((scaled.score(&5, &context) - (-5.0)).abs() < 0.001);
    }
}
