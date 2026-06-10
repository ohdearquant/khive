//! Composite memory scoring — ported from the archived internal service (v1 archive).
//!
//! Provides a fully-tunable scoring pipeline:
//!   1. `ScoringConfig`  — all knobs, all `pub`, all serde-friendly for agent sweeps.
//!   2. `calculate_score` — multiplicative formula: `w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`.
//!   3. `ScoreAdjustment` — declarative conditional rules applied after the base formula.
//!   4. `normalize_rrf_scores` / `normalize_rank_fusion_scores` — RRF and raw-cosine normalization.
//!   5. `normalize_min_score` — dual-scale input (0.0–1.0 fraction or 0–100 integer).
//!   6. `is_meaningful_query` — noise gate before embedding compute.
//!   7. `contains_cjk` — CJK routing decision.
// FILE SIZE JUSTIFICATION: scoring.rs bundles ScoringConfig, all normalization helpers,
// CJK routing, and the full test suite for the scoring pipeline. The tests require access
// to module-private helpers; splitting would require pub(crate) promotion of private fns.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Adjustment conditions ─────────────────────────────────────────────────────

/// A condition that determines whether a score adjustment applies to a candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdjustmentCondition {
    /// Match by memory type ("episodic" or "semantic").
    MemoryType { kind: String },
    /// Match by age in days. Both bounds are optional (omit = no bound).
    AgeRange {
        #[serde(default)]
        min_days: Option<f32>,
        #[serde(default)]
        max_days: Option<f32>,
    },
    /// Match by salience score. Both bounds are optional.
    SalienceRange {
        #[serde(default)]
        min: Option<f32>,
        #[serde(default)]
        max: Option<f32>,
    },
    /// Match when query entity names appear in memory content.
    EntityMatch,
    /// Match when query entity names do NOT appear in memory content.
    EntityMiss,
    /// All sub-conditions must be true (conjunction).
    All {
        conditions: Vec<AdjustmentCondition>,
    },
}

/// The operation to apply when a condition matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdjustmentOp {
    /// Add a fixed value to the score.
    Add { value: f32 },
    /// Subtract a fixed value from the score.
    Subtract { value: f32 },
    /// Multiply the score by a factor.
    Multiply { factor: f32 },
}

/// A conditional score adjustment: if `condition` matches, apply `operation`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreAdjustment {
    pub condition: AdjustmentCondition,
    pub operation: AdjustmentOp,
}

/// Context passed to condition evaluation — properties of the candidate + query.
pub struct CandidateContext<'a> {
    pub memory_type: &'a str,
    pub age_days: f32,
    pub salience: f32,
    pub content: &'a str,
    pub entity_names: &'a [String],
}

impl AdjustmentCondition {
    /// Return `true` when this condition applies to the given candidate context.
    pub fn matches(&self, ctx: &CandidateContext<'_>) -> bool {
        match self {
            Self::MemoryType { kind } => ctx.memory_type == kind.as_str(),
            Self::AgeRange { min_days, max_days } => {
                if let Some(min) = min_days {
                    if ctx.age_days < *min {
                        return false;
                    }
                }
                if let Some(max) = max_days {
                    if ctx.age_days > *max {
                        return false;
                    }
                }
                true
            }
            Self::SalienceRange { min, max } => {
                if let Some(lo) = min {
                    if ctx.salience < *lo {
                        return false;
                    }
                }
                if let Some(hi) = max {
                    if ctx.salience > *hi {
                        return false;
                    }
                }
                true
            }
            Self::EntityMatch => {
                if ctx.entity_names.is_empty() {
                    return false;
                }
                let lower = ctx.content.to_lowercase();
                ctx.entity_names.iter().any(|e| lower.contains(e.as_str()))
            }
            Self::EntityMiss => {
                if ctx.entity_names.is_empty() {
                    return false;
                }
                let lower = ctx.content.to_lowercase();
                !ctx.entity_names.iter().any(|e| lower.contains(e.as_str()))
            }
            Self::All { conditions } => conditions.iter().all(|c| c.matches(ctx)),
        }
    }
}

impl AdjustmentOp {
    /// Apply this operation to `score` and return the adjusted value.
    pub fn apply(&self, score: f32) -> f32 {
        match self {
            Self::Add { value } => score + value,
            Self::Subtract { value } => score - value,
            Self::Multiply { factor } => score * factor,
        }
    }
}

impl ScoreAdjustment {
    /// Apply this adjustment to `score` if the condition matches, otherwise return `score` unchanged.
    pub fn apply(&self, score: f32, ctx: &CandidateContext<'_>) -> f32 {
        if self.condition.matches(ctx) {
            self.operation.apply(score)
        } else {
            score
        }
    }
}

/// Default score adjustments: semantic age penalty and entity boost.
///
/// H2 calibration (2026-06-10): the flat episodic recency bonus (+0.05) is removed (inherited
/// from H1) — it stacked with the semantic age penalty to create a 0.10 score swing independent
/// of content quality. Weights are reverted to baseline values (w_sal=0.20, w_temp=0.10,
/// w_rel=0.70); only the flat-adjustment set changes. Semantic penalty reduced 0.05→0.02 to
/// prevent over-penalising high-salience reference material.
pub fn default_adjustments() -> Vec<ScoreAdjustment> {
    vec![
        // Semantic age penalty: light nudge to prevent old reference docs from crowding out
        // high-salience episodic content when the base score is near-equal. Reduced 0.05→0.02
        // (H1/H2 calibration) so episodic bonus removal is the only flat-adjustment change.
        ScoreAdjustment {
            condition: AdjustmentCondition::All {
                conditions: vec![
                    AdjustmentCondition::MemoryType {
                        kind: "semantic".into(),
                    },
                    AdjustmentCondition::AgeRange {
                        min_days: Some(30.0),
                        max_days: None,
                    },
                    AdjustmentCondition::SalienceRange {
                        min: Some(0.85),
                        max: None,
                    },
                ],
            },
            operation: AdjustmentOp::Subtract { value: 0.02 },
        },
        // Entity match boost: memories mentioning queried entities get boosted.
        ScoreAdjustment {
            condition: AdjustmentCondition::EntityMatch,
            operation: AdjustmentOp::Multiply { factor: 1.3 },
        },
    ]
}

// ── Scoring weights ───────────────────────────────────────────────────────────

/// Weights for the combined memory score: `score = w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringWeights {
    /// Multiplicative boost from salience in `(1 + w_imp × salience)`. Default: 0.20 (H2 baseline weights).
    pub salience: f32,
    /// Multiplicative boost from recency in `(1 + w_temp × recency)`. Default: 0.10 (H2 baseline weights).
    pub temporal: f32,
    /// Base multiplier applied to relevance. Default: 0.70 (H2 baseline weights).
    pub relevance: f32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            salience: 0.20,
            temporal: 0.10,
            relevance: 0.70,
        }
    }
}

// ── ScoringConfig ─────────────────────────────────────────────────────────────

/// Complete, tunable scoring configuration for the memory recall pipeline.
/// DoS caps: max_recall_candidates≤500, default_token_budget≤16000, default_recall_limit≤200.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringConfig {
    // ── Composite weights ──────────────────────────────────────────────────
    pub weights: ScoringWeights,

    // ── Relevance thresholds ───────────────────────────────────────────────
    /// Minimum raw cosine similarity to include a vector hit. Hits below this
    /// are excluded before RRF fusion (#2272). Default: 0.10.
    pub min_raw_relevance: f32,
    /// Minimum RRF score after fusion before normalization. Default: 0.0.
    pub min_rrf_relevance: f32,
    /// Relevance floor for the min-max normalization band. Default: 0.15.
    pub baseline_relevance: f32,

    // ── Temporal decay ─────────────────────────────────────────────────────
    /// Upper cap on per-entry decay_factor before temporal recency calculation.
    /// Default: 0.05.
    pub decay_cap: f32,

    // ── DoS caps (enforced server-side) ───────────────────────────────────
    /// Maximum search candidates to retrieve. Server-side cap: 500. Default: 200.
    pub max_recall_candidates: usize,
    /// Default result limit when caller doesn't specify. Server-side cap: 200.
    /// Default: 10.
    pub default_recall_limit: usize,
    /// Default token budget (tokens). Server-side cap: 16000. Default: 4000.
    pub default_token_budget: usize,
    /// Approximate characters per token (for token budget). Default: 4.
    pub chars_per_token: usize,

    // ── MMR diversity penalty ──────────────────────────────────────────────
    /// Score penalty applied to results whose first `mmr_prefix_len` characters
    /// match an earlier result. Default: 0.1.
    pub mmr_penalty: f32,
    /// Character prefix length used for MMR duplicate detection. Default: 100.
    pub mmr_prefix_len: usize,

    // ── Feature toggles ────────────────────────────────────────────────────
    /// When true, suppress memories whose `properties.supersedes` value matches
    /// the ID of another memory in the result set. Default: true.
    pub enable_supersedes_suppression: bool,
    /// When true and a multilingual embedding model is registered, route CJK
    /// queries to it as the primary model. Default: true.
    pub enable_cjk_routing: bool,
    /// Name of the multilingual embedding model to use for CJK routing.
    /// When None, the handler checks registered model names for substrings
    /// "multilingual" or "paraphrase". Default: None.
    pub cjk_model: Option<String>,

    // ── Conditional adjustments ────────────────────────────────────────────
    /// Score adjustments applied after the base formula. Default: episodic bonus,
    /// semantic age penalty, entity boost.
    pub adjustments: Vec<ScoreAdjustment>,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            weights: ScoringWeights::default(),

            min_raw_relevance: 0.10,
            min_rrf_relevance: 0.0,
            baseline_relevance: 0.15,

            decay_cap: 0.05,

            max_recall_candidates: 200,
            default_recall_limit: 10,
            default_token_budget: 4000,
            chars_per_token: 4,

            mmr_penalty: 0.1,
            mmr_prefix_len: 100,

            enable_supersedes_suppression: true,
            enable_cjk_routing: true,
            cjk_model: None,

            adjustments: default_adjustments(),
        }
    }
}

// ── Server-side DoS caps ──────────────────────────────────────────────────────

/// Maximum candidates a caller may request (server-side cap).
pub const MAX_RECALL_CANDIDATES: usize = 500;
/// Maximum token budget a caller may request (server-side cap).
pub const MAX_TOKEN_BUDGET: usize = 16_000;
/// Maximum result limit a caller may request (server-side cap).
pub const MAX_RECALL_LIMIT: usize = 200;

impl ScoringConfig {
    /// Clamp all DoS-cap fields to their server-side maximums.
    ///
    /// Called at the start of `handle_recall` so callers cannot trigger
    /// unbounded candidate retrieval or token budget consumption.
    pub fn apply_dos_caps(&mut self) {
        self.max_recall_candidates = self.max_recall_candidates.min(MAX_RECALL_CANDIDATES);
        self.default_token_budget = self.default_token_budget.min(MAX_TOKEN_BUDGET);
        self.default_recall_limit = self.default_recall_limit.min(MAX_RECALL_LIMIT);
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// Returns `true` if `c` is a CJK character (Unified, Extension A/B, Hiragana,
/// Katakana, Hangul).
#[inline]
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'       // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'     // CJK Extension A
        | '\u{F900}'..='\u{FAFF}'     // CJK Compatibility Ideographs
        | '\u{3040}'..='\u{309F}'     // Hiragana
        | '\u{30A0}'..='\u{30FF}'     // Katakana
        | '\u{20000}'..='\u{2A6DF}'   // CJK Extension B
        | '\u{AC00}'..='\u{D7AF}'     // Hangul Syllables
    )
}

/// Returns `true` when >15% of the query's characters are CJK.
pub fn contains_cjk(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    let cjk = chars.iter().filter(|&&c| is_cjk_char(c)).count();
    (cjk as f32) / (chars.len() as f32) > 0.15
}

/// Normalize `min_score`: 0–1 passes through, 1–100 divides by 100, others return Err.
pub fn normalize_min_score(score: f64) -> Result<f32, crate::config::MinScoreError> {
    if !score.is_finite() {
        return Err(crate::config::MinScoreError::NotFinite);
    }
    if (0.0..=1.0).contains(&score) {
        return Ok(score as f32);
    }
    if (1.0..=100.0).contains(&score) {
        return Ok((score / 100.0) as f32);
    }
    Err(crate::config::MinScoreError::OutOfRange(score))
}

/// Returns `true` if the query has enough semantic content for meaningful recall.
pub fn is_meaningful_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    let is_alpha_or_cjk = |c: char| c.is_alphabetic() || is_cjk_char(c);

    let meaningful_chars: usize = trimmed.chars().filter(|c| is_alpha_or_cjk(*c)).count();
    if meaningful_chars == 0 {
        return false;
    }

    let cjk_chars: usize = trimmed.chars().filter(|c| is_cjk_char(*c)).count();
    if meaningful_chars < 2 && cjk_chars == 0 {
        return false;
    }

    // Detect repeated-character patterns: "aaaa", "aaa bbb ccc" (gibberish).
    let words: Vec<&str> = trimmed
        .split_whitespace()
        .filter(|w| w.chars().any(is_alpha_or_cjk))
        .collect();
    if !words.is_empty() {
        let all_repeated = words.iter().all(|w| {
            let chars: Vec<char> = w.chars().filter(|c| is_alpha_or_cjk(*c)).collect();
            if chars.len() <= 2 {
                return false;
            }
            let unique: std::collections::HashSet<char> = chars
                .iter()
                .map(|c| c.to_lowercase().next().unwrap_or(*c))
                .collect();
            (unique.len() as f32) / (chars.len() as f32) < 0.4
        });
        if all_repeated {
            return false;
        }
    }

    true
}

// ── Score normalization ────────────────────────────────────────────────────────

/// Calibrated relevance ceiling for normalized scores.
///
/// Prevents the best candidate from entering the 1.0 saturation zone before
/// temporal, salience, and entity adjustments are applied.
const NORMALIZED_RELEVANCE_CEILING: f32 = 0.82;

/// RRF score threshold above which the best result is considered a genuine
/// relevance signal (~rank 20 in dual-source RRF(k=60)).
const RRF_SIGNAL_THRESHOLD: f32 = 0.025;

/// Normalize raw-cosine or BM25 scores (single-source) into a calibrated relevance band.
pub fn normalize_rank_fusion_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
) -> HashMap<Uuid, f32> {
    if scores.is_empty() {
        return HashMap::new();
    }
    let min_rrf = config.min_rrf_relevance;
    let filtered: Vec<(Uuid, f32)> = scores
        .into_iter()
        .filter(|(_, score)| score.is_finite() && *score >= min_rrf)
        .collect();
    if filtered.is_empty() {
        return HashMap::new();
    }
    let max_score = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_score.is_finite() || max_score <= 0.0 {
        return HashMap::new();
    }
    let min_score_seen = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::INFINITY, f32::min);
    let span = max_score - min_score_seen;
    let floor = config
        .baseline_relevance
        .clamp(0.0, NORMALIZED_RELEVANCE_CEILING);
    let range = NORMALIZED_RELEVANCE_CEILING - floor;

    let signal_strength = (max_score / RRF_SIGNAL_THRESHOLD).min(1.0);

    filtered
        .into_iter()
        .map(|(id, score)| {
            let calibrated = if span <= f32::EPSILON {
                max_score.clamp(floor, NORMALIZED_RELEVANCE_CEILING)
            } else {
                let percentile = ((score - min_score_seen) / span).clamp(0.0, 1.0);
                floor + percentile * range
            };
            (id, calibrated * signal_strength)
        })
        .collect()
}

/// Normalize RRF-fused scores (dual-source) into a calibrated relevance band.
pub fn normalize_rrf_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
) -> HashMap<Uuid, f32> {
    if scores.is_empty() {
        return HashMap::new();
    }
    let min_rrf = config.min_rrf_relevance;
    let filtered: Vec<(Uuid, f32)> = scores
        .into_iter()
        .filter(|(_, score)| score.is_finite() && *score >= min_rrf)
        .collect();
    if filtered.is_empty() {
        return HashMap::new();
    }
    let max_score = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_score.is_finite() || max_score <= 0.0 {
        return HashMap::new();
    }
    let min_score_seen = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::INFINITY, f32::min);
    let span = max_score - min_score_seen;
    let floor = config
        .baseline_relevance
        .clamp(0.0, NORMALIZED_RELEVANCE_CEILING);
    let range = NORMALIZED_RELEVANCE_CEILING - floor;

    filtered
        .into_iter()
        .map(|(id, score)| {
            let calibrated = if span <= f32::EPSILON {
                floor + range
            } else {
                let percentile = ((score - min_score_seen) / span).clamp(0.0, 1.0);
                floor + percentile * range
            };
            (id, calibrated)
        })
        .collect()
}

// ── Composite scoring ─────────────────────────────────────────────────────────

/// Input data for `calculate_score` — groups per-candidate fields to stay
/// within clippy's 7-argument limit.
pub struct ScoreInput<'a> {
    pub salience: f32,
    pub memory_type_str: &'a str,
    pub content: &'a str,
    pub created_at_millis: i64,
    pub decay_factor: f32,
    pub now_millis: i64,
    pub relevance_score: f32,
    pub entity_names: &'a [String],
}

/// Composite score for a single memory candidate.
///
/// Formula (semantic-gate model, multiplicative):
///   `score = w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`
///
/// Then each `ScoreAdjustment` in `config.adjustments` is evaluated and applied in order.
/// Result is clamped to `[0, 1]`.
pub fn calculate_score(input: &ScoreInput<'_>, config: &ScoringConfig) -> f32 {
    let w = &config.weights;
    let semantic_base = w.relevance * input.relevance_score;

    let time_diff_days = ((input.now_millis - input.created_at_millis) as f32
        / (24.0 * 60.0 * 60.0 * 1000.0))
        .max(0.0);

    let capped_decay = input.decay_factor.min(config.decay_cap);
    let temporal_recency = (-capped_decay * time_diff_days).exp();

    let temporal_boost = 1.0 + w.temporal * temporal_recency;
    let salience_boost = 1.0 + w.salience * input.salience;

    let mut score = semantic_base * temporal_boost * salience_boost;

    let ctx = CandidateContext {
        memory_type: input.memory_type_str,
        age_days: time_diff_days,
        salience: input.salience,
        content: input.content,
        entity_names: input.entity_names,
    };

    for adj in &config.adjustments {
        score = adj.apply(score, &ctx);
    }

    score.clamp(0.0, 1.0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_meaningful_query_rejects_empty() {
        assert!(!is_meaningful_query(""));
        assert!(!is_meaningful_query("   "));
    }

    #[test]
    fn is_meaningful_query_rejects_symbols_only() {
        assert!(!is_meaningful_query("!@#$%"));
        assert!(!is_meaningful_query("..."));
    }

    #[test]
    fn is_meaningful_query_rejects_single_latin_char() {
        assert!(!is_meaningful_query("a"));
        assert!(!is_meaningful_query("Z"));
    }

    #[test]
    fn is_meaningful_query_rejects_repeated_gibberish() {
        assert!(!is_meaningful_query("aaaa bbbb cccc"));
    }

    #[test]
    fn is_meaningful_query_accepts_normal_queries() {
        assert!(is_meaningful_query("what is the capital of France"));
        assert!(is_meaningful_query("rust async runtime"));
        assert!(is_meaningful_query("hello"));
    }

    #[test]
    fn contains_cjk_detects_chinese() {
        // Purely CJK — well above 15% threshold.
        assert!(contains_cjk("你好世界"));
        // Mixed string: "世界" = 2 CJK out of 6 chars = 33%, above threshold.
        assert!(contains_cjk("世界 hi"));
        // Mostly-Latin with 2 CJK chars out of 15 total = 13% → below threshold.
        assert!(!contains_cjk("hello 世界 world"));
    }

    #[test]
    fn contains_cjk_ignores_latin() {
        assert!(!contains_cjk("hello world"));
        assert!(!contains_cjk(""));
    }

    #[test]
    fn normalize_min_score_fraction_passthrough() {
        let v = normalize_min_score(0.5).unwrap();
        assert!((v - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn normalize_min_score_percent_form() {
        let v = normalize_min_score(50.0).unwrap();
        assert!((v - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn normalize_min_score_rejects_out_of_range() {
        assert!(normalize_min_score(200.0).is_err());
        assert!(normalize_min_score(-1.0).is_err());
        assert!(normalize_min_score(f64::NAN).is_err());
    }

    #[test]
    fn calculate_score_returns_unit_interval() {
        let config = ScoringConfig::default();
        let score = calculate_score(
            &ScoreInput {
                salience: 0.9,
                memory_type_str: "episodic",
                content: "test content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: 1000,
                relevance_score: 0.8,
                entity_names: &[],
            },
            &config,
        );
        assert!((0.0..=1.0).contains(&score), "score {score} out of [0,1]");
    }

    #[test]
    fn calculate_score_high_salience_ranks_higher() {
        let config = ScoringConfig {
            adjustments: vec![],
            ..ScoringConfig::default()
        };
        let now_ms = 1_000_000i64;
        let score_high = calculate_score(
            &ScoreInput {
                salience: 0.9,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.7,
                entity_names: &[],
            },
            &config,
        );
        let score_low = calculate_score(
            &ScoreInput {
                salience: 0.1,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.7,
                entity_names: &[],
            },
            &config,
        );
        assert!(score_high > score_low, "high salience should rank higher");
    }

    #[test]
    fn dos_caps_enforce_limits() {
        let mut config = ScoringConfig {
            max_recall_candidates: 9999,
            default_token_budget: 99999,
            default_recall_limit: 9999,
            ..ScoringConfig::default()
        };
        config.apply_dos_caps();
        assert_eq!(config.max_recall_candidates, MAX_RECALL_CANDIDATES);
        assert_eq!(config.default_token_budget, MAX_TOKEN_BUDGET);
        assert_eq!(config.default_recall_limit, MAX_RECALL_LIMIT);
    }

    #[test]
    fn normalize_rrf_scores_preserves_ordering() {
        let config = ScoringConfig::default();
        let input = vec![
            (Uuid::new_v4(), 0.030f32),
            (Uuid::new_v4(), 0.020f32),
            (Uuid::new_v4(), 0.015f32),
        ];
        let ids: Vec<Uuid> = input.iter().map(|(id, _)| *id).collect();
        let output = normalize_rrf_scores(input, &config);
        let s0 = output[&ids[0]];
        let s1 = output[&ids[1]];
        let s2 = output[&ids[2]];
        assert!(s0 > s1 && s1 > s2, "ordering must be preserved");
        assert!(s0 <= 1.0 && s2 >= 0.0, "scores must be in [0,1]");
    }

    // ── H2 calibration regression tests (2026-06-10) ──────────────────────────
    // H2 = H1 flat-adjustment set (episodic bonus removed, semantic penalty 0.02)
    //      + baseline weights (w_sal=0.20, w_temp=0.10, w_rel=0.70).
    // All four properties from H1 still hold; anchor values recomputed for H2 constants.

    /// H2 property: old high-salience semantic (sal=0.85, age=65d) must outrank
    /// fresh low-salience episodic (sal=0.40, age=0.5d) at equal relevance advantage.
    /// Episodic bonus removal still provides this separation at baseline weights.
    #[test]
    fn h2_old_semantic_high_salience_beats_fresh_low_salience_episodic() {
        let config = ScoringConfig::default();
        let now_ms = 0i64;
        // Old semantic: 65 days old, sal=0.85, rel=0.70
        let old_semantic = calculate_score(
            &ScoreInput {
                salience: 0.85,
                memory_type_str: "semantic",
                content: "lambda priority structure directive",
                created_at_millis: -(65 * 24 * 60 * 60 * 1000),
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        // Fresh episodic: 0.5 days old, sal=0.40, rel=0.60
        let fresh_episodic = calculate_score(
            &ScoreInput {
                salience: 0.40,
                memory_type_str: "episodic",
                content: "recent session checkpoint",
                created_at_millis: -(43_200_000i64), // 0.5 days in ms
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.60,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            old_semantic > fresh_episodic,
            "H2 failure: old semantic (sal=0.85, age=65d) score={old_semantic:.5} \
             must beat fresh episodic (sal=0.40, age=0.5d) score={fresh_episodic:.5}"
        );
    }

    /// H2 property: at equal salience and type, fresher memory still outranks older.
    /// Recency is preserved — just no longer dominant via a flat bonus.
    #[test]
    fn h2_recency_preserved_at_equal_salience() {
        let config = ScoringConfig::default();
        let now_ms = 0i64;
        let fresh = calculate_score(
            &ScoreInput {
                salience: 0.60,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: -(24 * 60 * 60 * 1000), // 1 day
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        let stale = calculate_score(
            &ScoreInput {
                salience: 0.60,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: -(90 * 24 * 60 * 60 * 1000), // 90 days
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            fresh > stale,
            "H2: fresher memory (1d) score={fresh:.5} should outrank same-salience older (90d) score={stale:.5}"
        );
    }

    /// H2 property: salience monotonicity at fixed age — higher salience always scores higher.
    #[test]
    fn h2_salience_monotonicity_at_fixed_age() {
        let config = ScoringConfig::default();
        let now_ms = 0i64;
        let base_input = |sal: f32| ScoreInput {
            salience: sal,
            memory_type_str: "episodic",
            content: "content",
            created_at_millis: -(35 * 24 * 60 * 60 * 1000),
            decay_factor: 0.01,
            now_millis: now_ms,
            relevance_score: 0.70,
            entity_names: &[],
        };
        let s_low = calculate_score(&base_input(0.20), &config);
        let s_mid = calculate_score(&base_input(0.50), &config);
        let s_high = calculate_score(&base_input(0.90), &config);
        assert!(
            s_low < s_mid && s_mid < s_high,
            "salience monotonicity violated: {s_low:.5} < {s_mid:.5} < {s_high:.5}"
        );
    }

    /// H2 parity anchors: Rust f32 scores must match Python-computed values within f32
    /// precision (1e-5). Python uses f64 arithmetic; Rust uses f32, so 1e-9 is not achievable
    /// cross-precision — asserting to 1e-5 which is tight enough to detect formula divergence.
    ///
    /// Anchor values computed from the H2 formula: w_rel=0.70, w_temp=0.10, w_sal=0.20,
    /// episodic bonus removed, semantic penalty −0.02 (age≥30d, sal≥0.85), entity match ×1.3.
    #[test]
    fn h2_parity_anchors_match_python_harness() {
        let config = ScoringConfig::default();
        let now_ms = 0i64;

        // Anchor A: old semantic, sal=0.85, age=65d, rel=0.70, df=0.01
        // Python: 0.5832289 (semantic_age_penalty −0.02 applies: age=65≥30, sal=0.85≥0.85)
        let anchor_a = calculate_score(
            &ScoreInput {
                salience: 0.85,
                memory_type_str: "semantic",
                content: "anchor",
                created_at_millis: -(65 * 24 * 60 * 60 * 1000),
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            (anchor_a - 0.583_228_9f32).abs() < 1e-5,
            "Anchor A mismatch: got {anchor_a:.8}, expected ~0.5832289"
        );

        // Anchor B: fresh episodic, sal=0.40, age=0.5d, rel=0.60, df=0.01
        // Python: 0.4987338 (no adjustment — episodic bonus removed in H2)
        let anchor_b = calculate_score(
            &ScoreInput {
                salience: 0.40,
                memory_type_str: "episodic",
                content: "anchor",
                created_at_millis: -(43_200_000i64), // 0.5 days in ms
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.60,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            (anchor_b - 0.498_733_8f32).abs() < 1e-5,
            "Anchor B mismatch: got {anchor_b:.8}, expected ~0.4987338"
        );

        // Anchor C: episodic 90d and 1d, sal=0.60, rel=0.70 — fresher 1d must beat 90d
        // Python C_old=0.5711125, C_new=0.6031339
        let anchor_c_old = calculate_score(
            &ScoreInput {
                salience: 0.60,
                memory_type_str: "episodic",
                content: "anchor",
                created_at_millis: -(90 * 24 * 60 * 60 * 1000),
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        let anchor_c_new = calculate_score(
            &ScoreInput {
                salience: 0.60,
                memory_type_str: "episodic",
                content: "anchor",
                created_at_millis: -(24 * 60 * 60 * 1000), // 1 day
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            (anchor_c_old - 0.571_112_5f32).abs() < 1e-5,
            "Anchor C_old mismatch: got {anchor_c_old:.8}, expected ~0.5711125"
        );
        assert!(
            (anchor_c_new - 0.603_133_9f32).abs() < 1e-5,
            "Anchor C_new mismatch: got {anchor_c_new:.8}, expected ~0.6031339"
        );
        assert!(
            anchor_c_new > anchor_c_old,
            "recency ordering violated in anchor C"
        );

        // Anchor D: salience monotonicity at 35d semantic
        // Python D_low=0.5769827 (sal=0.50, penalty does NOT apply: 0.50<0.85),
        //         D_high=0.5989451 (sal=0.90, penalty applies: 0.90≥0.85)
        let anchor_d_low = calculate_score(
            &ScoreInput {
                salience: 0.50,
                memory_type_str: "semantic",
                content: "anchor",
                created_at_millis: -(35 * 24 * 60 * 60 * 1000),
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        let anchor_d_high = calculate_score(
            &ScoreInput {
                salience: 0.90,
                memory_type_str: "semantic",
                content: "anchor",
                created_at_millis: -(35 * 24 * 60 * 60 * 1000),
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.70,
                entity_names: &[],
            },
            &config,
        );
        assert!(
            (anchor_d_low - 0.576_982_7f32).abs() < 1e-5,
            "Anchor D_low mismatch: got {anchor_d_low:.8}, expected ~0.5769827"
        );
        assert!(
            (anchor_d_high - 0.598_945_1f32).abs() < 1e-5,
            "Anchor D_high mismatch: got {anchor_d_high:.8}, expected ~0.5989451"
        );
        assert!(
            anchor_d_high > anchor_d_low,
            "salience monotonicity violated in anchor D"
        );
    }
}
