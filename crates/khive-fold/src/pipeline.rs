//! ComposePipeline: score candidates then pack to budget.

use crate::anchor::{Anchor, AnchorGraph};
use crate::error::FoldError;
use crate::objective::{Objective, ObjectiveContext};
use crate::selector::{Selector, SelectorInput, SelectorOutput, SelectorWeights};

/// Pipeline that scores candidates with an objective then packs to budget via a selector.
pub struct ComposePipeline<T> {
    /// Graph anchor used for causal provenance traversal before scoring.
    pub anchor: Box<dyn Anchor>,
    /// Objective that assigns scores to each candidate.
    pub objective: Box<dyn Objective<T>>,
    /// Selector that packs the scored candidates under a budget.
    pub selector: Box<dyn Selector<T>>,
}

impl<T: Clone + Send + Sync + 'static> ComposePipeline<T> {
    /// Score candidates with the objective, then pack under budget with the selector.
    pub fn execute(
        &self,
        _graph: &AnchorGraph,
        candidates: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
        context: &ObjectiveContext,
    ) -> Result<SelectorOutput<T>, FoldError> {
        let mut scored = Vec::with_capacity(candidates.len());
        for mut candidate in candidates {
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
            scored.push(candidate);
        }
        self.selector.select(scored, budget, weights)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective::Objective;

    struct TupleObjective;

    impl Objective<(f64, f64)> for TupleObjective {
        fn score(&self, candidate: &(f64, f64), _context: &ObjectiveContext) -> f64 {
            candidate.0
        }

        fn precision(&self, candidate: &(f64, f64), _context: &ObjectiveContext) -> f64 {
            candidate.1
        }
    }

    fn input(id: &str, score: f64, precision: f64) -> SelectorInput<(f64, f64)> {
        SelectorInput {
            id: id.to_string(),
            content: (score, precision),
            size: 1,
            score: 0.0,
            category: None,
            information_gain: None,
        }
    }

    fn pipeline() -> ComposePipeline<(f64, f64)> {
        ComposePipeline {
            anchor: Box::new(crate::anchor::BfsAnchor),
            objective: Box::new(TupleObjective),
            selector: Box::new(crate::selector::GreedySelector),
        }
    }

    #[test]
    fn compose_pipeline_ranks_by_precision_weighted_score() {
        let pipeline = pipeline();
        let candidates = vec![input("a", 10.0, 0.1), input("b", 2.0, 1.0)];
        let out = pipeline
            .execute(
                &AnchorGraph::new(),
                candidates,
                1,
                &SelectorWeights::default(),
                &ObjectiveContext::new(),
            )
            .unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "b");
    }

    #[test]
    fn compose_pipeline_applies_objective_min_score_before_selector() {
        let pipeline = pipeline();
        let candidates = vec![input("a", 1.0, 1.0)];
        let context = ObjectiveContext::new().with_min_score(2.0);
        let out = pipeline
            .execute(
                &AnchorGraph::new(),
                candidates,
                10,
                &SelectorWeights::default(),
                &context,
            )
            .unwrap();
        assert!(out.selected.is_empty());
    }

    #[test]
    fn compose_pipeline_rejects_effective_score_outside_f32_range() {
        let pipeline = pipeline();
        let candidates = vec![input("a", f64::MAX, 1.0)];
        let err = pipeline
            .execute(
                &AnchorGraph::new(),
                candidates,
                10,
                &SelectorWeights::default(),
                &ObjectiveContext::new(),
            )
            .unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }
}
