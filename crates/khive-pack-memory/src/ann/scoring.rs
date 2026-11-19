//! Score contracts for memory recall and note-search ANN routes.

use khive_runtime::RuntimeError;

use super::exact_cosine_unclamped;

#[derive(Clone, Copy)]
pub(crate) enum AnnScoreRoute {
    Memory,
    NoteSearch,
}

#[derive(Clone, Copy)]
pub(crate) struct FreshTailSearch<'a> {
    pub(super) query: &'a [f32],
    pub(super) k: usize,
    pub(super) route: AnnScoreRoute,
}

impl<'a> FreshTailSearch<'a> {
    pub(crate) fn new(query: &'a [f32], k: usize, route: AnnScoreRoute) -> Self {
        Self { query, k, route }
    }
}

impl AnnScoreRoute {
    pub(super) fn graph_score(self, squared_l2_distance: f32) -> Result<f64, RuntimeError> {
        match self {
            Self::Memory => Ok(f64::from((1.0 - squared_l2_distance / 2.0).max(0.0))),
            Self::NoteSearch => khive_score::try_cosine_score_with_f32_tolerance(
                f64::from(squared_l2_distance) / 2.0,
            )
            .map(|score| score.to_f64())
            .map_err(|error| {
                RuntimeError::Internal(format!("note-search ANN cosine distance: {error}"))
            }),
        }
    }

    pub(super) fn tail_score(self, query: &[f32], embedding: &[f32]) -> Result<f64, RuntimeError> {
        let cosine = exact_cosine_unclamped(query, embedding);
        match self {
            Self::Memory => Ok(f64::from(cosine.max(0.0))),
            Self::NoteSearch => {
                khive_score::try_cosine_score_with_f32_tolerance(1.0 - f64::from(cosine))
                    .map(|score| score.to_f64())
                    .map_err(|error| {
                        RuntimeError::Internal(format!(
                            "note-search fresh-tail cosine distance: {error}"
                        ))
                    })
            }
        }
    }
}
