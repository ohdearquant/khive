//! ComposePipeline: score candidates then pack to budget.

use crate::error::FoldError;
use crate::objective::{Objective, ObjectiveContext};
use crate::selector::{Selector, SelectorInput, SelectorOutput, SelectorWeights};

/// Pipeline that scores candidates with an objective then packs to budget via a selector.
pub struct ComposePipeline<T> {
    /// Objective that assigns scores to each candidate.
    pub objective: Box<dyn Objective<T>>,
    /// Selector that packs the scored candidates under a budget.
    pub selector: Box<dyn Selector<T>>,
}

impl<T: Clone + Send + Sync + 'static> ComposePipeline<T> {
    /// Score at most `context.max_candidates` inputs in caller order, then pack under budget.
    pub fn execute(
        &self,
        candidates: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
        context: &ObjectiveContext,
    ) -> Result<SelectorOutput<T>, FoldError> {
        let considered = context
            .max_candidates
            .unwrap_or(candidates.len())
            .min(candidates.len());
        let mut scored = Vec::with_capacity(considered);
        for mut candidate in candidates.into_iter().take(considered) {
            let score = self.objective.score(&candidate.content, context);
            if !self.objective.passes_score(score, context) {
                continue;
            }

            let precision = self.objective.precision(&candidate.content, context);
            let precision = if precision.is_finite() {
                precision
            } else {
                1.0
            };
            let effective = score * precision;

            if !effective.is_finite() || effective < f32::MIN as f64 || effective > f32::MAX as f64
            {
                return Err(FoldError::InvalidInput(format!(
                    "objective effective score for '{}' is outside finite f32 range",
                    candidate.id
                )));
            }

            candidate.score = effective as f32;
            // Carry the full f64 precision into selector ranking so rank
            // comparisons go through the khive-score fixed-point comparators
            // instead of re-deriving from the narrowed f32 `score` field —
            // mirrors the RankedIndex pattern in objective/traits.rs.
            candidate.rank_score = Some(effective);
            scored.push(candidate);
        }
        self.selector.select(scored, budget, weights)
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
