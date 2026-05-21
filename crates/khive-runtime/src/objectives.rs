//! Retrieval Objective implementations for khive-runtime.
//!
//! Domain-specific objectives that operate on pre-computed retrieval signals.
//! Pure math: no IO, no async. The runtime layer materialises the signal data
//! and feeds it in via the candidate struct.
//!
//! See ADR-061 — Retrieval Infrastructure.

use uuid::Uuid;

use khive_fold::objective::{Objective, ObjectiveContext};
use khive_fold::ordering::HasId;

/// Pre-computed retrieval signals for a single candidate entity.
///
/// All fields are `Option` — a missing signal scores 0.0. The runtime layer
/// is responsible for populating whichever fields are available before handing
/// the slice to an objective.
#[derive(Debug, Clone)]
pub struct RetrievalCandidate {
    /// Stable entity UUID.
    pub id: Uuid,
    /// Cosine similarity to the query vector (0.0–1.0).
    pub vector_score: Option<f64>,
    /// BM25/FTS relevance score (0.0–1.0 normalised, or raw rank score).
    pub text_score: Option<f64>,
    /// Hop distance from the nearest anchor node (0 = anchor itself).
    pub graph_distance: Option<u32>,
    /// Pre-fused RRF score from `FusionStrategy::Rrf`.
    pub rrf_score: Option<f64>,
}

impl HasId for RetrievalCandidate {
    #[inline]
    fn id(&self) -> Uuid {
        self.id
    }
}

// ── VectorSimilarityObjective ────────────────────────────────────────────────

/// Scores a candidate by cosine similarity to the query vector.
///
/// Returns `vector_score` unchanged, or 0.0 when the field is absent.
pub struct VectorSimilarityObjective;

impl Objective<RetrievalCandidate> for VectorSimilarityObjective {
    #[inline]
    fn score(&self, candidate: &RetrievalCandidate, _context: &ObjectiveContext) -> f64 {
        candidate.vector_score.unwrap_or(0.0)
    }

    fn name(&self) -> &str {
        "VectorSimilarityObjective"
    }
}

// ── TextRelevanceObjective ───────────────────────────────────────────────────

/// Scores a candidate by BM25/FTS relevance.
///
/// Returns `text_score` unchanged, or 0.0 when the field is absent.
pub struct TextRelevanceObjective;

impl Objective<RetrievalCandidate> for TextRelevanceObjective {
    #[inline]
    fn score(&self, candidate: &RetrievalCandidate, _context: &ObjectiveContext) -> f64 {
        candidate.text_score.unwrap_or(0.0)
    }

    fn name(&self) -> &str {
        "TextRelevanceObjective"
    }
}

// ── GraphProximityObjective ──────────────────────────────────────────────────

/// Scores a candidate by graph proximity to anchor nodes.
///
/// Score formula (linear decay):
///
/// ```text
/// d ≤ max_distance → score = 1.0 − (d as f64 / max_distance as f64)
/// d > max_distance → score = 0.0
/// missing          → score = 0.0
/// ```
///
/// Direct anchor hits (d = 0) score 1.0. The boundary `d == max_distance`
/// scores 0.0; anything beyond also scores 0.0.
pub struct GraphProximityObjective {
    /// Maximum hop distance to consider. Candidates beyond this score 0.0.
    pub max_distance: u32,
}

impl Objective<RetrievalCandidate> for GraphProximityObjective {
    fn score(&self, candidate: &RetrievalCandidate, _context: &ObjectiveContext) -> f64 {
        let d = match candidate.graph_distance {
            Some(d) => d,
            None => return 0.0,
        };
        if self.max_distance == 0 || d >= self.max_distance {
            return 0.0;
        }
        1.0 - (d as f64 / self.max_distance as f64)
    }

    fn name(&self) -> &str {
        "GraphProximityObjective"
    }
}

// ── RrfFusionObjective ───────────────────────────────────────────────────────

/// Scores a candidate by its pre-computed RRF fusion score.
///
/// Returns `rrf_score` unchanged, or 0.0 when the field is absent.
pub struct RrfFusionObjective;

impl Objective<RetrievalCandidate> for RrfFusionObjective {
    #[inline]
    fn score(&self, candidate: &RetrievalCandidate, _context: &ObjectiveContext) -> f64 {
        candidate.rrf_score.unwrap_or(0.0)
    }

    fn name(&self) -> &str {
        "RrfFusionObjective"
    }
}

// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_fold::objective::{Objective, ObjectiveContext};
    use khive_fold::WeightedObjective;
    use uuid::Uuid;

    fn ctx() -> ObjectiveContext {
        ObjectiveContext::new()
    }

    fn candidate(
        vector: Option<f64>,
        text: Option<f64>,
        dist: Option<u32>,
        rrf: Option<f64>,
    ) -> RetrievalCandidate {
        RetrievalCandidate {
            id: Uuid::new_v4(),
            vector_score: vector,
            text_score: text,
            graph_distance: dist,
            rrf_score: rrf,
        }
    }

    // ── VectorSimilarityObjective ────────────────────────────────────────

    #[test]
    fn vector_present_returns_signal() {
        let c = candidate(Some(0.85), None, None, None);
        let score = VectorSimilarityObjective.score(&c, &ctx());
        assert!((score - 0.85).abs() < 1e-12);
    }

    #[test]
    fn vector_absent_returns_zero() {
        let c = candidate(None, None, None, None);
        assert_eq!(VectorSimilarityObjective.score(&c, &ctx()), 0.0);
    }

    #[test]
    fn vector_zero_score_returns_zero() {
        let c = candidate(Some(0.0), None, None, None);
        assert_eq!(VectorSimilarityObjective.score(&c, &ctx()), 0.0);
    }

    // ── TextRelevanceObjective ───────────────────────────────────────────

    #[test]
    fn text_present_returns_signal() {
        let c = candidate(None, Some(0.6), None, None);
        let score = TextRelevanceObjective.score(&c, &ctx());
        assert!((score - 0.6).abs() < 1e-12);
    }

    #[test]
    fn text_absent_returns_zero() {
        let c = candidate(None, None, None, None);
        assert_eq!(TextRelevanceObjective.score(&c, &ctx()), 0.0);
    }

    // ── GraphProximityObjective ──────────────────────────────────────────

    #[test]
    fn graph_anchor_hit_scores_one() {
        // d=0 → score = 1.0 − 0/max = 1.0
        let c = candidate(None, None, Some(0), None);
        let obj = GraphProximityObjective { max_distance: 3 };
        assert!((obj.score(&c, &ctx()) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn graph_midpoint_scores_half() {
        // d=1, max=2 → score = 1.0 − 1/2 = 0.5
        let c = candidate(None, None, Some(1), None);
        let obj = GraphProximityObjective { max_distance: 2 };
        assert!((obj.score(&c, &ctx()) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn graph_at_boundary_scores_zero() {
        // d == max_distance → score = 0.0 (boundary excluded)
        let c = candidate(None, None, Some(3), None);
        let obj = GraphProximityObjective { max_distance: 3 };
        assert_eq!(obj.score(&c, &ctx()), 0.0);
    }

    #[test]
    fn graph_beyond_boundary_scores_zero() {
        let c = candidate(None, None, Some(10), None);
        let obj = GraphProximityObjective { max_distance: 3 };
        assert_eq!(obj.score(&c, &ctx()), 0.0);
    }

    #[test]
    fn graph_absent_scores_zero() {
        let c = candidate(None, None, None, None);
        let obj = GraphProximityObjective { max_distance: 3 };
        assert_eq!(obj.score(&c, &ctx()), 0.0);
    }

    #[test]
    fn graph_max_distance_zero_always_scores_zero() {
        // max_distance=0 is degenerate; guard against divide-by-zero.
        let c = candidate(None, None, Some(0), None);
        let obj = GraphProximityObjective { max_distance: 0 };
        assert_eq!(obj.score(&c, &ctx()), 0.0);
    }

    // ── RrfFusionObjective ───────────────────────────────────────────────

    #[test]
    fn rrf_present_returns_signal() {
        let c = candidate(None, None, None, Some(0.0327));
        let score = RrfFusionObjective.score(&c, &ctx());
        assert!((score - 0.0327).abs() < 1e-12);
    }

    #[test]
    fn rrf_absent_returns_zero() {
        let c = candidate(None, None, None, None);
        assert_eq!(RrfFusionObjective.score(&c, &ctx()), 0.0);
    }

    // ── WeightedObjective composition ───────────────────────────────────

    #[test]
    fn weighted_composition_vector_and_text() {
        // Candidate with vector=0.8, text=0.6
        // Weighted(0.5*vector + 0.5*text) = 0.5*0.8 + 0.5*0.6 = 0.7
        let c = candidate(Some(0.8), Some(0.6), None, None);

        let obj = WeightedObjective::<RetrievalCandidate>::new()
            .add(Box::new(VectorSimilarityObjective), 0.5)
            .add(Box::new(TextRelevanceObjective), 0.5);

        let score = obj.score(&c, &ctx());
        // WeightedObjective divides by total weight (1.0), so result is 0.7
        assert!((score - 0.7).abs() < 1e-12);
    }

    #[test]
    fn weighted_composition_with_graph() {
        // vector=1.0, text=0.0, graph d=1/max=4 → proximity = 1 - 1/4 = 0.75
        // weights: vector=0.4, text=0.3, graph=0.3
        // weighted sum = (0.4*1.0 + 0.3*0.0 + 0.3*0.75) / 1.0 = 0.4 + 0.0 + 0.225 = 0.625
        let c = candidate(Some(1.0), Some(0.0), Some(1), None);

        let obj = WeightedObjective::<RetrievalCandidate>::new()
            .add(Box::new(VectorSimilarityObjective), 0.4)
            .add(Box::new(TextRelevanceObjective), 0.3)
            .add(Box::new(GraphProximityObjective { max_distance: 4 }), 0.3);

        let score = obj.score(&c, &ctx());
        assert!((score - 0.625).abs() < 1e-12);
    }

    #[test]
    fn weighted_all_absent_returns_zero() {
        let c = candidate(None, None, None, None);

        let obj = WeightedObjective::<RetrievalCandidate>::new()
            .add(Box::new(VectorSimilarityObjective), 0.5)
            .add(Box::new(TextRelevanceObjective), 0.5);

        // 0.0 * 0.5 + 0.0 * 0.5 = 0.0
        assert_eq!(obj.score(&c, &ctx()), 0.0);
    }

    // ── HasId ────────────────────────────────────────────────────────────

    #[test]
    fn has_id_returns_candidate_uuid() {
        let id = Uuid::new_v4();
        let c = RetrievalCandidate {
            id,
            vector_score: None,
            text_score: None,
            graph_distance: None,
            rrf_score: None,
        };
        assert_eq!(c.id(), id);
    }

    // ── select_top via DeterministicObjective ────────────────────────────

    #[test]
    fn select_top_orders_by_vector_score() {
        use khive_fold::DeterministicObjective;

        let candidates = vec![
            candidate(Some(0.3), None, None, None),
            candidate(Some(0.9), None, None, None),
            candidate(Some(0.6), None, None, None),
        ];

        let top = VectorSimilarityObjective.select_top_deterministic(&candidates, 2, &ctx());

        assert_eq!(top.len(), 2);
        assert!((top[0].score - 0.9).abs() < 1e-12);
        assert!((top[1].score - 0.6).abs() < 1e-12);
    }
}
