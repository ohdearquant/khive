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
#[path = "compose_tests.rs"]
mod tests;
