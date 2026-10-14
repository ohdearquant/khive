//! Retrieval evaluation: 5-level label taxonomy, nDCG, and `net_evidence` metrics.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Label taxonomy
// ---------------------------------------------------------------------------

/// Five-level relevance label for retrieved sections.
///
/// Labels are designed for GPQA-style evaluation where *topically adjacent but
/// factually wrong* sections are more harmful than irrelevant ones — they can
/// actively mislead an LLM agent that trusts retrieved context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalLabel {
    /// Section directly answers or enables answering the query.
    Decisive,
    /// Section provides useful supporting evidence for the query.
    Supporting,
    /// Section provides general context but not specific evidence.
    Background,
    /// Section has no relationship to the query.
    Irrelevant,
    /// Section is on-topic but contains factually incorrect information that
    /// would mislead an LLM agent (the "GPQA failure mode").
    AdjacentWrong,
}

impl RetrievalLabel {
    /// Graded relevance gain used in DCG / net-evidence calculations.
    ///
    /// `AdjacentWrong` carries a negative gain to penalise retrieval of
    /// misleading but plausible-sounding sections.
    pub fn gain(self) -> f64 {
        match self {
            Self::Decisive => 3.0,
            Self::Supporting => 2.0,
            Self::Background => 0.5,
            Self::Irrelevant => 0.0,
            Self::AdjacentWrong => -2.0,
        }
    }

    /// Returns `true` for labels that count as "relevant" in binary recall/precision.
    pub fn is_relevant(self) -> bool {
        matches!(self, Self::Decisive | Self::Supporting)
    }

    /// Returns `true` for labels that count as active distractors.
    pub fn is_distractor(self) -> bool {
        matches!(self, Self::AdjacentWrong)
    }
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// A single retrieved section with its ground-truth relevance label.
///
/// The slice passed to metric functions must be ordered by descending score
/// (rank 1 at index 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabeledResult {
    /// Unique identifier of the retrieved section.
    pub section_id: Uuid,
    /// Retrieval score (higher = more relevant according to the pipeline).
    pub score: f64,
    /// Ground-truth relevance label assigned by a human or eval pipeline.
    pub label: RetrievalLabel,
}

// ---------------------------------------------------------------------------
// Aggregate metrics struct
// ---------------------------------------------------------------------------

/// All standard retrieval metrics computed for a single query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalMetrics {
    /// Recall at multiple k values: `[(k, recall_k), ...]`.
    pub recall_at_k: Vec<(usize, f64)>,
    /// nDCG at k = 10 using graded gains from [`RetrievalLabel::gain`].
    pub ndcg_at_10: f64,
    /// Precision at k = 5 (fraction of top-5 that are relevant).
    pub precision_at_5: f64,
    /// Precision at k = 10 (fraction of top-10 that are relevant).
    pub precision_at_10: f64,
    /// Fraction of top-10 results that are `AdjacentWrong` distractors.
    pub distractor_at_10: f64,
    /// Net graded evidence at k = 10: `sum(gain_i / log2(i+1))` for i in 1..=10.
    pub net_evidence_at_10: f64,
    /// Mean reciprocal rank: `1 / rank` of the first `Decisive` result, or `0.0`.
    pub mrr: f64,
    /// Optional before/after flip ratio: `wrong→right / right→wrong`.
    ///
    /// `None` when only a single ranking is available (no before/after pair).
    pub flip_ratio: Option<f64>,
}

// ---------------------------------------------------------------------------
// Metric functions
// ---------------------------------------------------------------------------

/// Recall at k: fraction of all `Decisive | Supporting` items that appear in top-k.
///
/// Returns `1.0` when there are no relevant items in the full list (vacuously true).
pub fn recall_at_k(results: &[LabeledResult], k: usize) -> f64 {
    let total_relevant: usize = results.iter().filter(|r| r.label.is_relevant()).count();
    if total_relevant == 0 {
        return 1.0;
    }
    let k = k.min(results.len());
    let found: usize = results[..k]
        .iter()
        .filter(|r| r.label.is_relevant())
        .count();
    found as f64 / total_relevant as f64
}

/// Precision at k: fraction of top-k results that are `Decisive | Supporting`.
///
/// Returns `0.0` when k = 0 or results is empty.
pub fn precision_at_k(results: &[LabeledResult], k: usize) -> f64 {
    if k == 0 || results.is_empty() {
        return 0.0;
    }
    let k = k.min(results.len());
    let relevant: usize = results[..k]
        .iter()
        .filter(|r| r.label.is_relevant())
        .count();
    relevant as f64 / k as f64
}

/// Distractor at k: fraction of top-k results that are `AdjacentWrong`.
///
/// Returns `0.0` when k = 0 or results is empty.
pub fn distractor_at_k(results: &[LabeledResult], k: usize) -> f64 {
    if k == 0 || results.is_empty() {
        return 0.0;
    }
    let k = k.min(results.len());
    let distractors: usize = results[..k]
        .iter()
        .filter(|r| r.label.is_distractor())
        .count();
    distractors as f64 / k as f64
}

/// Net evidence at k: `sum(gain(label_i) / log2(i+2))` for i in 0..k.
///
/// The discount denominator uses `log2(i+2)` so that rank-1 (i=0) gets
/// `log2(2) = 1.0` — the standard DCG convention.
///
/// Returns `0.0` when k = 0 or results is empty.
pub fn net_evidence_at_k(results: &[LabeledResult], k: usize) -> f64 {
    if k == 0 || results.is_empty() {
        return 0.0;
    }
    let k = k.min(results.len());
    results[..k]
        .iter()
        .enumerate()
        .map(|(i, r)| r.label.gain() / (i as f64 + 2.0).log2())
        .sum()
}

/// nDCG at k using graded gains from [`RetrievalLabel::gain`].
///
/// The ideal ranking places all `Decisive` results first, then `Supporting`,
/// `Background`, `Irrelevant`, and finally `AdjacentWrong`. The ideal DCG is
/// computed from a sorted-by-gain copy of the full result list.
///
/// Returns `1.0` when the ideal DCG is zero (no positive-gain items).
pub fn ndcg_at_k(results: &[LabeledResult], k: usize) -> f64 {
    if k == 0 || results.is_empty() {
        return 0.0;
    }
    let k = k.min(results.len());

    let dcg = results[..k]
        .iter()
        .enumerate()
        .map(|(i, r)| r.label.gain() / (i as f64 + 2.0).log2())
        .sum::<f64>();

    // Ideal DCG: sort all results by gain descending, take top-k.
    let mut gains: Vec<f64> = results.iter().map(|r| r.label.gain()).collect();
    gains.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

    let idcg = gains[..k]
        .iter()
        .enumerate()
        .map(|(i, &g)| g / (i as f64 + 2.0).log2())
        .sum::<f64>();

    if idcg == 0.0 {
        // No positive-gain items exist and no negative-gain items; vacuously perfect.
        return 1.0;
    }
    if idcg < 0.0 {
        // Only negative-gain items (all distractors); worst possible outcome.
        return 0.0;
    }
    (dcg / idcg).clamp(0.0, 1.0)
}

/// Mean reciprocal rank: `1.0 / rank` of the first `Decisive` result.
///
/// Returns `0.0` if no `Decisive` result appears in the list.
pub fn mrr(results: &[LabeledResult]) -> f64 {
    for (i, r) in results.iter().enumerate() {
        if r.label == RetrievalLabel::Decisive {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Compute all standard retrieval metrics at their canonical k values.
///
/// `recall_at_k` is evaluated at k ∈ {1, 3, 5, 10}.
/// All other metrics use their k = 10 (or full-list for MRR) defaults.
pub fn compute_all(results: &[LabeledResult]) -> RetrievalMetrics {
    let recall_at_k_vals = vec![
        (1, recall_at_k(results, 1)),
        (3, recall_at_k(results, 3)),
        (5, recall_at_k(results, 5)),
        (10, recall_at_k(results, 10)),
    ];

    RetrievalMetrics {
        recall_at_k: recall_at_k_vals,
        ndcg_at_10: ndcg_at_k(results, 10),
        precision_at_5: precision_at_k(results, 5),
        precision_at_10: precision_at_k(results, 10),
        distractor_at_10: distractor_at_k(results, 10),
        net_evidence_at_10: net_evidence_at_k(results, 10),
        mrr: mrr(results),
        flip_ratio: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "engine_eval/tests.rs"]
mod tests;
