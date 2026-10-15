//! Retrieval Objective implementations for khive-runtime.
//!
//! Domain-specific objectives that operate on pre-computed retrieval signals.
//! Pure math: no IO, no async. The runtime layer materialises the signal data
//! and feeds it in via the candidate struct.

use std::collections::HashMap;

use uuid::Uuid;

use khive_fold::objective::{Objective, ObjectiveContext};
use khive_fold::ordering::HasId;
use khive_score::Decay;

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
/// Used by the recall pipeline to score and rank candidates via
/// `AmplifiedDecayAwareSalienceObjective`, `TemporalRecencyObjective`, and
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

#[inline]
fn exponential_decay(value: f64, rate: f64, age_days: f64) -> f64 {
    // Preserve unchecked public candidates outside the shared decay domain.
    match Decay::exponential(rate).and_then(|decay| decay.apply(value, age_days)) {
        Ok(score) if age_days >= 0.0 => score,
        _ => value * (-rate * age_days).exp(),
    }
}

/// Scores a `NoteCandidate` by salience with configurable temporal decay.
///
/// Uses the fixed exponential rate supplied at construction:
/// `salience * exp(-decay_rate * age_days)`. It does not read the candidate's
/// per-note `decay_factor`. The memory recall pipeline applies its configured
/// `DecayModel` before scoring and uses `AmplifiedDecayAwareSalienceObjective`
/// over the resulting `effective_salience` instead.
///
/// This objective remains useful for callers that want one decay policy across
/// every candidate in a `WeightedObjective` composition.
pub struct DecayAwareSalienceObjective {
    decay_rate: f64,
}

impl DecayAwareSalienceObjective {
    /// Create a new objective with the given exponential decay rate. Panics if
    /// `decay_rate` is negative or non-finite.
    ///
    /// `decay_rate = 0.01` gives a ~69-day half-life (default for memory notes).
    pub fn new(decay_rate: f64) -> Self {
        assert!(
            decay_rate.is_finite() && decay_rate >= 0.0,
            "decay_rate must be finite and non-negative, got {decay_rate}"
        );
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
        exponential_decay(candidate.salience, self.decay_rate, candidate.age_days)
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
        exponential_decay(1.0, k, candidate.age_days)
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
#[path = "objectives_tests.rs"]
mod tests;
