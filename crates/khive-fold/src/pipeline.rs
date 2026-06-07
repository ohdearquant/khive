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
        let scored = candidates
            .into_iter()
            .map(|mut candidate| {
                candidate.score = self.objective.score(&candidate.content, context) as f32;
                candidate
            })
            .collect();
        self.selector.select(scored, budget, weights)
    }
}
