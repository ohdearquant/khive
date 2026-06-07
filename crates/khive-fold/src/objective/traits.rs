//! Core objective function traits

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use uuid::Uuid;

use khive_score::DeterministicScore;

use super::context::ObjectiveContext;
use super::selection::Selection;
use crate::ordering::{HasId, ScoredEntry};
use crate::{ObjectiveError, ObjectiveResult};

const SMALL_TOP_N: usize = 96;

#[derive(Debug, Clone, Copy)]
struct RankedIndex {
    score: f64,
    det_score: DeterministicScore,
    index: usize,
}

impl RankedIndex {
    #[inline]
    fn new(score: f64, index: usize) -> Self {
        Self {
            score,
            det_score: DeterministicScore::from_f64(score),
            index,
        }
    }
}

impl Eq for RankedIndex {}

impl PartialEq for RankedIndex {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.det_score == other.det_score && self.index == other.index
    }
}

impl Ord for RankedIndex {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.det_score
            .cmp(&other.det_score)
            .then_with(|| other.index.cmp(&self.index))
    }
}

impl PartialOrd for RankedIndex {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy)]
struct WorstRankedIndex(RankedIndex);

impl Eq for WorstRankedIndex {}

impl PartialEq for WorstRankedIndex {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Ord for WorstRankedIndex {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for WorstRankedIndex {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy)]
struct WorstScoredEntry<T>(ScoredEntry<T>);

impl<T> Eq for WorstScoredEntry<T> {}

impl<T> PartialEq for WorstScoredEntry<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> Ord for WorstScoredEntry<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl<T> PartialOrd for WorstScoredEntry<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[inline]
fn considered_limit(len: usize, context: &ObjectiveContext) -> usize {
    context.max_candidates.unwrap_or(len).min(len)
}

/// Deterministic, composable objective function over a candidate set.
pub trait Objective<T>: Send + Sync {
    /// Evaluate a single candidate.
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;

    /// Precision (inverse variance) of the score estimate; default 1.0 (fully trusted).
    #[inline]
    fn precision(&self, _candidate: &T, _context: &ObjectiveContext) -> f64 {
        1.0
    }

    /// Check if a score passes the threshold; non-finite scores never pass.
    #[inline]
    fn passes_score(&self, score: f64, context: &ObjectiveContext) -> bool {
        score.is_finite() && context.min_score.map(|min| score >= min).unwrap_or(true)
    }

    /// Check if a candidate passes the threshold.
    #[inline]
    fn passes(&self, candidate: &T, context: &ObjectiveContext) -> bool {
        let score = self.score(candidate, context);
        self.passes_score(score, context)
    }

    /// Score a batch of candidates and return passing `(index, score)` pairs.
    fn batch_score(&self, candidates: &[T], context: &ObjectiveContext) -> Vec<(usize, f64)> {
        let mut scored = Vec::with_capacity(candidates.len().min(256));
        for (index, candidate) in candidates.iter().enumerate() {
            let score = self.score(candidate, context);
            if self.passes_score(score, context) {
                scored.push((index, score));
            }
        }
        scored
    }

    /// Select all passing candidates in score-descending order.
    fn select<'a>(&self, candidates: &'a [T], context: &ObjectiveContext) -> Vec<Selection<&'a T>> {
        if candidates.is_empty() {
            return Vec::new();
        }
        let n = considered_limit(candidates.len(), context);
        self.select_top(candidates, n, context)
    }

    /// Select the top N candidates by precision-weighted score.
    fn select_top<'a>(
        &self,
        candidates: &'a [T],
        n: usize,
        context: &ObjectiveContext,
    ) -> Vec<Selection<&'a T>> {
        if n == 0 || candidates.is_empty() {
            return Vec::new();
        }

        let considered_limit = considered_limit(candidates.len(), context);

        let mut considered = 0usize;
        let mut passed = 0usize;

        if n <= SMALL_TOP_N {
            let mut top: Vec<RankedIndex> = Vec::with_capacity(n.min(considered_limit));

            for (index, candidate) in candidates.iter().take(considered_limit).enumerate() {
                considered += 1;

                let score = self.score(candidate, context);
                if !self.passes_score(score, context) {
                    continue;
                }

                passed += 1;
                let precision = self.precision(candidate, context);
                let effective = score
                    * if precision.is_finite() {
                        precision
                    } else {
                        1.0
                    };
                let entry = RankedIndex::new(effective, index);

                if top.len() == n {
                    let worst = *top.last().expect("non-empty top when len == n");
                    if entry <= worst {
                        continue;
                    }
                }

                let pos = top.partition_point(|existing| *existing >= entry);
                if pos < n {
                    top.insert(pos, entry);
                    if top.len() > n {
                        top.pop();
                    }
                }
            }

            return top
                .into_iter()
                .map(|entry| {
                    Selection::new(&candidates[entry.index], entry.score, entry.index)
                        .with_considered(considered)
                        .with_passed(passed)
                })
                .collect();
        }

        let mut heap: BinaryHeap<WorstRankedIndex> = BinaryHeap::with_capacity(n);

        for (index, candidate) in candidates.iter().take(considered_limit).enumerate() {
            considered += 1;

            let score = self.score(candidate, context);
            if !self.passes_score(score, context) {
                continue;
            }

            passed += 1;
            let precision = self.precision(candidate, context);
            let effective = score
                * if precision.is_finite() {
                    precision
                } else {
                    1.0
                };
            let entry = RankedIndex::new(effective, index);

            if heap.len() < n {
                heap.push(WorstRankedIndex(entry));
                continue;
            }

            if let Some(mut worst) = heap.peek_mut() {
                if entry > worst.0 {
                    *worst = WorstRankedIndex(entry);
                }
            }
        }

        let mut scored: Vec<RankedIndex> = heap.into_iter().map(|entry| entry.0).collect();
        scored.sort_unstable_by(|a, b| b.cmp(a));

        scored
            .into_iter()
            .map(|entry| {
                Selection::new(&candidates[entry.index], entry.score, entry.index)
                    .with_considered(considered)
                    .with_passed(passed)
            })
            .collect()
    }

    /// Get the name of this objective.
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// Implement Objective for closures.
impl<T, F> Objective<T> for F
where
    F: Fn(&T, &ObjectiveContext) -> f64 + Send + Sync,
{
    #[inline]
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        self(candidate, context)
    }
}

/// Create an objective from a scoring function.
pub fn objective_fn<T, F>(f: F) -> impl Objective<T>
where
    F: Fn(&T, &ObjectiveContext) -> f64 + Send + Sync,
{
    f
}

// ============================================================================
// Deterministic Objective Extension
// ============================================================================

/// Extension trait for deterministic selection with UUID tie-breaking on equal scores.
pub trait DeterministicObjective<T>: Objective<T>
where
    T: HasId,
{
    /// Select the best candidate with deterministic tie-breaking.
    fn select_deterministic<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>>;

    /// Select the top N candidates with deterministic ordering.
    fn select_top_deterministic<'a>(
        &self,
        candidates: &'a [T],
        n: usize,
        context: &ObjectiveContext,
    ) -> Vec<Selection<&'a T>>;
}

/// Blanket implementation of DeterministicObjective for any Objective<T> where T: HasId.
impl<O, T> DeterministicObjective<T> for O
where
    O: Objective<T>,
    T: HasId,
{
    fn select_deterministic<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        if candidates.is_empty() {
            return Err(ObjectiveError::NoCandidates);
        }

        let considered_limit = considered_limit(candidates.len(), context);

        let mut considered = 0usize;
        let mut passed = 0usize;
        let mut has_best = false;
        let mut best_index = 0usize;
        let mut best_score = 0.0f64;
        let mut best_precision = 1.0f64;
        let mut best_det = DeterministicScore::ZERO;
        let mut best_id = Uuid::nil();

        for (index, candidate) in candidates.iter().take(considered_limit).enumerate() {
            considered += 1;

            let score = self.score(candidate, context);
            if !self.passes_score(score, context) {
                continue;
            }

            passed += 1;
            let id = candidate.id();
            let precision = self.precision(candidate, context);
            let effective = score
                * if precision.is_finite() {
                    precision
                } else {
                    1.0
                };
            let det = DeterministicScore::from_f64(effective);

            if !has_best || det > best_det || (det == best_det && id < best_id) {
                has_best = true;
                best_index = index;
                best_score = score;
                best_precision = precision;
                best_det = det;
                best_id = id;
            }
        }

        if has_best {
            Ok(
                Selection::new(&candidates[best_index], best_score, best_index)
                    .with_precision(best_precision)
                    .with_considered(considered)
                    .with_passed(passed),
            )
        } else {
            Err(ObjectiveError::NoMatch("No candidate passed".into()))
        }
    }

    fn select_top_deterministic<'a>(
        &self,
        candidates: &'a [T],
        n: usize,
        context: &ObjectiveContext,
    ) -> Vec<Selection<&'a T>> {
        if n == 0 || candidates.is_empty() {
            return Vec::new();
        }

        if n == 1 {
            return self
                .select_deterministic(candidates, context)
                .ok()
                .into_iter()
                .collect();
        }

        let considered_limit = considered_limit(candidates.len(), context);

        let mut considered = 0usize;
        let mut passed = 0usize;

        if n <= SMALL_TOP_N {
            let mut top: Vec<ScoredEntry<&T>> = Vec::with_capacity(n.min(considered_limit));

            for (index, candidate) in candidates.iter().take(considered_limit).enumerate() {
                considered += 1;

                let score = self.score(candidate, context);
                if !self.passes_score(score, context) {
                    continue;
                }

                passed += 1;
                let precision = self.precision(candidate, context);
                let effective = score
                    * if precision.is_finite() {
                        precision
                    } else {
                        1.0
                    };
                let entry = ScoredEntry::new(candidate, effective, index);

                if top.len() == n {
                    let worst = *top.last().expect("non-empty top when len == n");
                    if entry <= worst {
                        continue;
                    }
                }

                let pos = top.partition_point(|existing| *existing >= entry);
                if pos < n {
                    top.insert(pos, entry);
                    if top.len() > n {
                        top.pop();
                    }
                }
            }

            return top
                .into_iter()
                .map(|entry| {
                    Selection::new(entry.into_candidate(), entry.score(), entry.index())
                        .with_considered(considered)
                        .with_passed(passed)
                })
                .collect();
        }

        let mut heap: BinaryHeap<WorstScoredEntry<&T>> = BinaryHeap::with_capacity(n);

        for (index, candidate) in candidates.iter().take(considered_limit).enumerate() {
            considered += 1;

            let score = self.score(candidate, context);
            if !self.passes_score(score, context) {
                continue;
            }

            passed += 1;
            let precision = self.precision(candidate, context);
            let effective = score
                * if precision.is_finite() {
                    precision
                } else {
                    1.0
                };
            let entry = ScoredEntry::new(candidate, effective, index);

            if heap.len() < n {
                heap.push(WorstScoredEntry(entry));
                continue;
            }

            if let Some(mut worst) = heap.peek_mut() {
                if entry > worst.0 {
                    *worst = WorstScoredEntry(entry);
                }
            }
        }

        let mut scored: Vec<ScoredEntry<&T>> = heap.into_iter().map(|entry| entry.0).collect();
        scored.sort_unstable_by(|a, b| b.cmp(a));

        scored
            .into_iter()
            .map(|entry| {
                Selection::new(entry.into_candidate(), entry.score(), entry.index())
                    .with_considered(considered)
                    .with_passed(passed)
            })
            .collect()
    }
}
