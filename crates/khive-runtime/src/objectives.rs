//! Retrieval Objective implementations for khive-runtime.
//!
//! Domain-specific objectives that operate on pre-computed retrieval signals.
//! Pure math: no IO, no async. The runtime layer materialises the signal data
//! and feeds it in via the candidate struct.

use std::collections::HashMap;

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
/// Implements `Objective` for both `RetrievalCandidate` and `NoteCandidate`
/// so the same objective can be used in the general retrieval pipeline
/// and the memory recall pipeline.
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

impl Objective<NoteCandidate> for RrfFusionObjective {
    #[inline]
    fn score(&self, candidate: &NoteCandidate, _context: &ObjectiveContext) -> f64 {
        candidate.rrf_score.unwrap_or(0.0)
    }

    fn name(&self) -> &str {
        "RrfFusionObjective"
    }
}

// ── Memory-Recall Objectives ──────────────────────────────────────────────────

/// Pre-computed signals for a single memory note candidate.
///
/// Used by the recall pipeline's `ComposePipeline` to score and rank candidates
/// via `DecayAwareSalienceObjective`, `TemporalRecencyObjective`, and
/// `RerankerObjective` without any IO. The runtime layer populates this struct
/// from stored notes before handing the slice to the pipeline.
#[derive(Debug, Clone)]
pub struct NoteCandidate {
    /// Stable note UUID.
    pub id: Uuid,
    /// Pre-fused RRF score from the retrieval stage (0.0–1.0).
    pub rrf_score: Option<f64>,
    /// Raw salience stored on the note (0.0–1.0).
    pub salience: f64,
    /// Per-note exponential decay rate (>= 0.0).
    pub decay_factor: f64,
    /// Age of the note in days at query time.
    pub age_days: f64,
    /// Salience after applying the configured `DecayModel` (pre-computed by the caller).
    ///
    /// The caller must set this to `DecayModel::apply(salience, age_days, decay_factor, half_life)`
    /// so that objectives respect the configured decay model variant rather than
    /// always applying exponential decay. When not set, defaults to 0.0.
    pub effective_salience: f64,
    /// Per-reranker scores populated by the rerank stage.
    /// Keyed by reranker name (e.g. "cross_encoder", "salience", "graph_proximity").
    pub rerank_scores: HashMap<String, f64>,
}

impl HasId for NoteCandidate {
    #[inline]
    fn id(&self) -> Uuid {
        self.id
    }
}

// ── DecayAwareSalienceObjective ──────────────────────────────────────────────

/// Scores a `NoteCandidate` by salience with configurable temporal decay.
///
/// The decay formula is determined by the configured `DecayModel` (injected at
/// construction time). The default `DecayModel::Exponential` uses the note's own
/// `decay_factor`: `salience * exp(-decay_factor * age_days)`.
///
/// This objective participates in `WeightedObjective` composition alongside
/// `RrfFusionObjective` and `TemporalRecencyObjective` to form the full recall
/// scoring pipeline.
pub struct DecayAwareSalienceObjective {
    /// Exponential decay rate k (>= 0.0). Score = `salience * exp(-k * age_days)`.
    /// Corresponds to the per-note `decay_factor` parameter stored on memory notes.
    pub decay_rate: f64,
}

impl DecayAwareSalienceObjective {
    /// Create a new objective with the given exponential decay rate.
    ///
    /// `decay_rate = 0.01` gives a ~69-day half-life (default for memory notes).
    pub fn new(decay_rate: f64) -> Self {
        Self { decay_rate }
    }

    /// Default memory decay rate: 0.01 (~69-day half-life).
    pub fn default_memory() -> Self {
        Self::new(0.01)
    }
}

impl Objective<NoteCandidate> for DecayAwareSalienceObjective {
    #[inline]
    fn score(&self, candidate: &NoteCandidate, _context: &ObjectiveContext) -> f64 {
        candidate.salience * (-candidate.decay_factor * candidate.age_days).exp()
    }

    fn name(&self) -> &str {
        "DecayAwareSalienceObjective"
    }
}

// ── AmplifiedDecayAwareSalienceObjective ─────────────────────────────────────

/// Scores a `NoteCandidate` by salience with exponential decay and a non-linear
/// amplification exponent applied after decay.
///
/// Formula: `(salience * exp(-decay_factor * age_days)) ^ alpha`
///
/// With `alpha > 1.0`, high-salience memories rank more clearly above low-salience
/// ones when relevance is similar. At `alpha = 1.5` (the memory-recall default),
/// salience 0.9 → 0.854 and salience 0.3 → 0.164 — a ~5.2× spread vs the ~3× linear
/// spread. Keep `alpha ≤ 2.0`; values above 2 compress near-zero salience toward 0.
///
/// Used by the memory recall pipeline to make salience a meaningful tiebreaker
/// without dominating relevance at the default weight of 0.20.
pub struct AmplifiedDecayAwareSalienceObjective {
    /// Power applied to the decayed salience value. Must be > 0.
    pub alpha: f64,
}

impl AmplifiedDecayAwareSalienceObjective {
    /// Create with the given amplification exponent.
    pub fn new(alpha: f64) -> Self {
        Self { alpha }
    }

    /// Default memory alpha from the memory recall handler: 1.5.
    pub fn default_memory() -> Self {
        Self::new(1.5)
    }
}

impl Objective<NoteCandidate> for AmplifiedDecayAwareSalienceObjective {
    #[inline]
    fn score(&self, candidate: &NoteCandidate, _context: &ObjectiveContext) -> f64 {
        // effective_salience is pre-computed via the caller's DecayModel, so this
        // works for all decay model variants, not just exponential.
        candidate.effective_salience.powf(self.alpha)
    }

    fn name(&self) -> &str {
        "AmplifiedDecayAwareSalienceObjective"
    }
}

// ── TemporalRecencyObjective ─────────────────────────────────────────────────

/// Scores a `NoteCandidate` by pure temporal recency with a configurable half-life.
///
/// Formula: `exp(-ln(2) / half_life_days * age_days)`
///
/// At `age_days = 0` → score 1.0 (brand new note).
/// At `age_days = half_life_days` → score 0.5.
///
/// Complements `DecayAwareSalienceObjective`: this signal rewards freshness
/// independently of the note's own decay rate.
pub struct TemporalRecencyObjective {
    /// Number of days for the recency score to halve. Must be > 0.
    pub half_life_days: f64,
}

impl TemporalRecencyObjective {
    /// Create with the default temporal half-life of 30 days.
    pub fn default_memory() -> Self {
        Self {
            half_life_days: 30.0,
        }
    }
}

impl Objective<NoteCandidate> for TemporalRecencyObjective {
    #[inline]
    fn score(&self, candidate: &NoteCandidate, _context: &ObjectiveContext) -> f64 {
        let k = std::f64::consts::LN_2 / self.half_life_days.max(f64::EPSILON);
        (-k * candidate.age_days).exp()
    }

    fn name(&self) -> &str {
        "TemporalRecencyObjective"
    }
}

// ── RerankerObjective ────────────────────────────────────────────────────────

/// Scores a `NoteCandidate` using a named reranker's pre-computed score.
///
/// Looks up `candidate.rerank_scores[reranker_name]`. Returns 0.0 when the
/// reranker was not run (key absent) — callers should gate on
/// `RecallConfig.reranker_weights[name] > 0.0` before including this objective
/// in a `WeightedObjective` composition.
pub struct RerankerObjective {
    /// Name of the reranker to look up in `candidate.rerank_scores`.
    pub reranker_name: String,
}

impl RerankerObjective {
    /// Create a new objective for the named reranker.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            reranker_name: name.into(),
        }
    }
}

impl Objective<NoteCandidate> for RerankerObjective {
    #[inline]
    fn score(&self, candidate: &NoteCandidate, _context: &ObjectiveContext) -> f64 {
        candidate
            .rerank_scores
            .get(&self.reranker_name)
            .copied()
            .unwrap_or(0.0)
    }

    fn name(&self) -> &str {
        "RerankerObjective"
    }
}

// ── MemoryRecallPipeline ──────────────────────────────────────────────────────

/// Composable scoring pipeline for memory recall candidates.
///
/// Wraps a `WeightedObjective<NoteCandidate>` with the three standard memory
/// scoring components (RRF relevance, amplified salience, temporal recency)
/// weighted by the recall config parameters. Pack code uses this type to avoid
/// a direct dependency on `khive-fold`.
pub struct MemoryRecallPipeline {
    pipeline: khive_fold::WeightedObjective<NoteCandidate>,
}

impl MemoryRecallPipeline {
    /// Build a pipeline from explicit component weights and temporal half-life.
    ///
    /// `relevance_weight`, `salience_weight`, `temporal_weight` correspond to
    /// `RecallConfig`'s three weight fields. `half_life_days` drives
    /// `TemporalRecencyObjective`. `salience_alpha` is the amplification exponent
    /// for `AmplifiedDecayAwareSalienceObjective` (default 1.5).
    pub fn new(
        relevance_weight: f64,
        salience_weight: f64,
        temporal_weight: f64,
        half_life_days: f64,
        salience_alpha: f64,
    ) -> Self {
        use khive_fold::WeightedObjective;
        let pipeline = WeightedObjective::<NoteCandidate>::new()
            .add(Box::new(RrfFusionObjective), relevance_weight)
            .add(
                Box::new(AmplifiedDecayAwareSalienceObjective::new(salience_alpha)),
                salience_weight,
            )
            .add(
                Box::new(TemporalRecencyObjective { half_life_days }),
                temporal_weight,
            );
        Self { pipeline }
    }

    /// Build a pipeline using the standard memory recall defaults.
    ///
    /// Weights: relevance=0.70, salience=0.20, temporal=0.10; half_life=30 days; alpha=1.5.
    pub fn default_memory() -> Self {
        Self::new(0.70, 0.20, 0.10, 30.0, 1.5)
    }

    /// Score a `NoteCandidate` through the pipeline.
    ///
    /// The result is in [0.0, 1.0]. The `NoteCandidate.rrf_score` field should
    /// carry the pre-normalized relevance (output of `normalize_relevance` / `RrfFusionObjective`).
    pub fn score(&self, candidate: &NoteCandidate) -> f64 {
        let ctx = ObjectiveContext::new();
        use khive_fold::objective::Objective;
        self.pipeline.score(candidate, &ctx).clamp(0.0, 1.0)
    }
}

// ────────────────────────────────────────────────────────────────────────────

// Kept inline: these tests exercise internal NoteCandidate fields that would
// otherwise need to be made pub just to reach them from tests/.
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

    fn note_candidate(
        rrf: Option<f64>,
        salience: f64,
        decay_factor: f64,
        age_days: f64,
    ) -> NoteCandidate {
        // Mirrors the caller-side DecayModel::apply() default (Exponential) for test data.
        let effective_salience = salience * (-decay_factor * age_days).exp();
        NoteCandidate {
            id: Uuid::new_v4(),
            rrf_score: rrf,
            salience,
            decay_factor,
            age_days,
            effective_salience,
            rerank_scores: HashMap::new(),
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
        let c = candidate(None, None, Some(0), None);
        let obj = GraphProximityObjective { max_distance: 3 };
        assert!((obj.score(&c, &ctx()) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn graph_midpoint_scores_half() {
        let c = candidate(None, None, Some(1), None);
        let obj = GraphProximityObjective { max_distance: 2 };
        assert!((obj.score(&c, &ctx()) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn graph_at_boundary_scores_zero() {
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
        // Guards the divide-by-zero case: max_distance=0 must not panic.
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
        let c = candidate(Some(0.8), Some(0.6), None, None);

        let obj = WeightedObjective::<RetrievalCandidate>::new()
            .add(Box::new(VectorSimilarityObjective), 0.5)
            .add(Box::new(TextRelevanceObjective), 0.5);

        let score = obj.score(&c, &ctx());
        // Weights here already sum to 1.0, so normalization is a no-op.
        assert!((score - 0.7).abs() < 1e-12);
    }

    #[test]
    fn weighted_composition_with_graph() {
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

    // ── NoteCandidate: HasId ─────────────────────────────────────────────

    #[test]
    fn note_candidate_has_id_returns_uuid() {
        let id = Uuid::new_v4();
        let c = NoteCandidate {
            id,
            rrf_score: None,
            salience: 0.5,
            decay_factor: 0.01,
            age_days: 0.0,
            effective_salience: 0.5,
            rerank_scores: HashMap::new(),
        };
        assert_eq!(c.id(), id);
    }

    // ── DecayAwareSalienceObjective ──────────────────────────────────────

    #[test]
    fn decay_aware_zero_age_returns_full_salience() {
        let obj = DecayAwareSalienceObjective::new(0.01);
        let c = note_candidate(None, 0.8, 0.01, 0.0);
        let score = obj.score(&c, &ctx());
        assert!((score - 0.8).abs() < 1e-12, "got {score}");
    }

    #[test]
    fn decay_aware_uses_note_decay_factor_not_field() {
        // Scoring uses the note's own decay_factor, not the objective's field.
        let obj = DecayAwareSalienceObjective::new(0.99); // obj.decay_rate ignored
        let c = note_candidate(None, 1.0, 0.01, 100.0);
        let score = obj.score(&c, &ctx());
        let expected = (-0.01_f64 * 100.0).exp();
        assert!(
            (score - expected).abs() < 1e-12,
            "got {score}, expected {expected}"
        );
    }

    #[test]
    fn decay_aware_high_decay_reduces_score_faster() {
        let obj = DecayAwareSalienceObjective::new(0.0);
        let slow = note_candidate(None, 1.0, 0.001, 100.0);
        let fast = note_candidate(None, 1.0, 0.1, 100.0);
        let score_slow = obj.score(&slow, &ctx());
        let score_fast = obj.score(&fast, &ctx());
        assert!(
            score_slow > score_fast,
            "slow decay should score higher: {score_slow} vs {score_fast}"
        );
    }

    // ── TemporalRecencyObjective ─────────────────────────────────────────

    #[test]
    fn temporal_score_one_at_zero_age() {
        let obj = TemporalRecencyObjective {
            half_life_days: 30.0,
        };
        let c = note_candidate(None, 0.5, 0.01, 0.0);
        let score = obj.score(&c, &ctx());
        assert!((score - 1.0).abs() < 1e-12, "got {score}");
    }

    #[test]
    fn temporal_score_half_at_half_life() {
        let half_life = 30.0;
        let obj = TemporalRecencyObjective {
            half_life_days: half_life,
        };
        let c = note_candidate(None, 0.5, 0.01, half_life);
        let score = obj.score(&c, &ctx());
        assert!(
            (score - 0.5).abs() < 1e-10,
            "expected 0.5 at half_life, got {score}"
        );
    }

    #[test]
    fn temporal_score_decreases_with_age() {
        let obj = TemporalRecencyObjective {
            half_life_days: 30.0,
        };
        let young = note_candidate(None, 1.0, 0.01, 10.0);
        let old = note_candidate(None, 1.0, 0.01, 100.0);
        let score_young = obj.score(&young, &ctx());
        let score_old = obj.score(&old, &ctx());
        assert!(
            score_young > score_old,
            "younger note should score higher: {score_young} vs {score_old}"
        );
    }

    // ── RerankerObjective ────────────────────────────────────────────────

    #[test]
    fn reranker_returns_named_score() {
        let mut c = note_candidate(None, 0.5, 0.01, 0.0);
        c.rerank_scores.insert("cross_encoder".to_string(), 0.9);
        let obj = RerankerObjective::new("cross_encoder");
        let score = obj.score(&c, &ctx());
        assert!((score - 0.9).abs() < 1e-12, "got {score}");
    }

    #[test]
    fn reranker_absent_key_returns_zero() {
        let c = note_candidate(None, 0.5, 0.01, 0.0);
        let obj = RerankerObjective::new("cross_encoder");
        let score = obj.score(&c, &ctx());
        assert_eq!(score, 0.0);
    }

    #[test]
    fn reranker_different_keys_independent() {
        let mut c = note_candidate(None, 0.5, 0.01, 0.0);
        c.rerank_scores.insert("salience".to_string(), 0.7);
        let obj_ce = RerankerObjective::new("cross_encoder");
        let obj_sal = RerankerObjective::new("salience");
        assert_eq!(obj_ce.score(&c, &ctx()), 0.0);
        assert!((obj_sal.score(&c, &ctx()) - 0.7).abs() < 1e-12);
    }

    // ── Weighted composition of memory objectives ────────────────────────

    #[test]
    fn memory_pipeline_weighted_composition() {
        // Verifies WeightedObjective reproduces the same formula MemoryRecallPipeline builds.
        let c = NoteCandidate {
            id: Uuid::new_v4(),
            rrf_score: Some(0.5),
            salience: 0.8,
            decay_factor: 0.01,
            age_days: 0.0,
            effective_salience: 0.8,
            rerank_scores: HashMap::new(),
        };
        let pipeline = WeightedObjective::<NoteCandidate>::new()
            .add(Box::new(RrfFusionObjective), 0.70)
            .add(Box::new(DecayAwareSalienceObjective::new(0.0)), 0.20)
            .add(
                Box::new(TemporalRecencyObjective {
                    half_life_days: 30.0,
                }),
                0.10,
            );
        let score = pipeline.score(&c, &ctx());
        assert!((score - 0.61).abs() < 1e-10, "got {score}");
    }
}
