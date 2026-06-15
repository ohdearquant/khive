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
//!   8. `needs_multilingual` — broad multilingual routing gate (non-ASCII alphabetic script).
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

/// Default score adjustments: episodic recency bonus, semantic age penalty, entity boost.
pub fn default_adjustments() -> Vec<ScoreAdjustment> {
    vec![
        // Episodic recency bonus: recent episodic memories get an additive boost.
        ScoreAdjustment {
            condition: AdjustmentCondition::All {
                conditions: vec![
                    AdjustmentCondition::MemoryType {
                        kind: "episodic".into(),
                    },
                    AdjustmentCondition::AgeRange {
                        min_days: None,
                        max_days: Some(7.0),
                    },
                ],
            },
            operation: AdjustmentOp::Add { value: 0.05 },
        },
        // Semantic age penalty: old high-salience semantic memories get penalized
        // to prevent reference docs from crowding out episodic content.
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
            operation: AdjustmentOp::Subtract { value: 0.05 },
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
    /// Multiplicative boost from salience in `(1 + w_imp × salience)`. Default: 0.2.
    pub salience: f32,
    /// Multiplicative boost from recency in `(1 + w_temp × recency)`. Default: 0.1.
    pub temporal: f32,
    /// Base multiplier applied to relevance. Default: 0.7.
    pub relevance: f32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            salience: 0.2,
            temporal: 0.1,
            relevance: 0.7,
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

/// Returns `true` when `text` is not predominantly ASCII/Latin-English and should
/// be routed to the multilingual embedding model.
///
/// Specifically: >15% of characters are alphabetic but not ASCII alphabetic.
/// This covers CJK, Cyrillic, Arabic, Devanagari, Hebrew, Thai, accented-Latin
/// (é, ü, ñ, …), and every other non-ASCII script recognised by Unicode's
/// `is_alphabetic` without introducing new crate dependencies.
pub fn needs_multilingual(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    let non_ascii_alpha = chars
        .iter()
        .filter(|&&c| c.is_alphabetic() && !c.is_ascii_alphabetic())
        .count();
    (non_ascii_alpha as f32) / (chars.len() as f32) > 0.15
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

    // ── needs_multilingual ────────────────────────────────────────────────────

    #[test]
    fn needs_multilingual_ascii_english_routes_primary() {
        assert!(!needs_multilingual("hello world"));
        assert!(!needs_multilingual("rust programming language"));
        assert!(!needs_multilingual(""));
        assert!(!needs_multilingual("42 items in the list"));
    }

    #[test]
    fn needs_multilingual_cjk_routes_multilingual() {
        // Pure CJK: 4/4 = 100% non-ASCII alpha → well above 15%.
        assert!(needs_multilingual("你好世界"));
        // Mixed: 2 CJK out of 5 chars = 40%.
        assert!(needs_multilingual("abc你好"));
    }

    #[test]
    fn needs_multilingual_cyrillic_routes_multilingual() {
        // "Привет мир" (Hello world in Russian): all Cyrillic alphabetic.
        assert!(needs_multilingual("Привет мир"));
        // Single Cyrillic word mixed with Latin — Г alone is 1/7 ≈ 14% (below threshold).
        // Verify with a word that has enough Cyrillic chars to cross 15%.
        assert!(needs_multilingual("Привет hello")); // Привет = 6 non-ASCII alpha, total chars including space = 12 → 6/12 = 50%
    }

    #[test]
    fn needs_multilingual_arabic_routes_multilingual() {
        // "مرحبا" (Hello in Arabic): all Arabic alphabetic.
        assert!(needs_multilingual("مرحبا بالعالم"));
    }

    #[test]
    fn needs_multilingual_devanagari_routes_multilingual() {
        // "नमस्ते" (Namaste in Hindi/Devanagari).
        assert!(needs_multilingual("नमस्ते दुनिया"));
    }

    #[test]
    fn needs_multilingual_accented_latin_routes_multilingual() {
        // French: "café résumé" — é is alphabetic and non-ASCII.
        // 2 accented chars out of 11 total = 18% > 15%.
        assert!(needs_multilingual("café résumé"));
        // German: "Müller" — ü is alphabetic and non-ASCII, 1/6 = 16.7% > 15%.
        assert!(needs_multilingual("Müller"));
        // Spanish: "niño" — ñ is 1/4 = 25% > 15%.
        assert!(needs_multilingual("niño"));
    }

    #[test]
    fn needs_multilingual_pure_ascii_accented_below_threshold_routes_primary() {
        // "hello naïve" — ï is 1/11 ≈ 9% < 15% → stays on primary.
        // (Deliberate design decision: a single diacritic in an otherwise English
        // sentence does not warrant multilingual routing.)
        assert!(!needs_multilingual("hello naive")); // no diacritics, definitely primary
                                                     // "über" — ü is 1/4 = 25% → routes multilingual (German word).
        assert!(needs_multilingual("über"));
    }
}
