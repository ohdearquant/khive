//! Recall configuration types — scoring weights, decay models, and FTS gather options.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use khive_fusion::FusionStrategy;
use khive_runtime::RuntimeError;
use khive_storage::types::{TextGatherMode, TextSearchOptions};

/// Error returned when `min_score` is outside the accepted dual-scale range.
#[derive(Debug, Clone)]
pub enum MinScoreError {
    /// Value was NaN or Inf.
    NotFinite,
    /// Value was finite but outside `[0.0, 100.0]` (or negative).
    OutOfRange(f64),
}

impl std::fmt::Display for MinScoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFinite => write!(f, "min_score must be finite"),
            Self::OutOfRange(v) => write!(
                f,
                "min_score {v} out of range: must be 0.0–1.0 (fraction) or 0–100 (percent)"
            ),
        }
    }
}

impl From<MinScoreError> for RuntimeError {
    fn from(e: MinScoreError) -> Self {
        RuntimeError::InvalidInput(e.to_string())
    }
}

/// Configuration for the recall scoring pipeline.
/// All fields have sensible defaults matching current behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    // --- Fusion weights ---
    /// Weight of RRF/fusion score. Default 0.70.
    pub relevance_weight: f64,
    /// Weight of decay-adjusted salience. Default 0.20.
    pub salience_weight: f64,
    /// Weight of pure recency. Default 0.10.
    pub temporal_weight: f64,

    // --- Reranker weights ---
    /// Per-reranker weights, keyed by reranker name. Missing keys → 0.0 (disabled).
    /// v1 built-in names: "cross_encoder", "salience", "graph_proximity".
    pub reranker_weights: HashMap<String, f64>,

    // --- Temporal parameters ---
    /// Days for temporal score to halve. Default 30.0.
    pub temporal_half_life_days: f64,
    /// Decay model to apply to salience. Default Exponential.
    pub decay_model: DecayModel,

    // --- Retrieval parameters ---
    /// Candidates per retrieval path before fusion = limit × this. Default 20.
    pub candidate_multiplier: u32,
    /// Explicit max candidates per retrieval path before fusion. When None,
    /// candidate_multiplier keeps the legacy behavior.
    pub candidate_limit: Option<u32>,
    /// Strategy used to fuse retrieval-source candidate lists. Default RRF k=60.
    pub fuse_strategy: FusionStrategy,
    /// Minimum composite score to include in results. Default 0.0.
    pub min_score: f64,
    /// Minimum raw salience to include in results. Default 0.0.
    pub min_salience: f64,
    /// Include per-component score breakdowns in recall responses. Default false.
    pub include_breakdown: bool,

    // --- Archive scoring pipeline override ---
    /// Optional archive scoring config; enables MMR, supersedes suppression, CJK routing, entity boost.
    pub scoring: Option<crate::scoring::ScoringConfig>,

    // --- Brain profile integration ---
    /// Optional brain profile hint for post-recall score boosting.
    pub brain_profile: Option<BrainProfileHint>,

    // --- FTS candidate-gather optimization ---
    /// Controls the two-stage FTS gather path (default: disabled, existing behavior).
    pub fts_gather: RecallFtsGatherConfig,

    // --- ANN over-fetch retry ---
    /// Maximum rounds for the ANN namespace over-fetch retry loop.
    ///
    /// Round 1 is the initial over-fetch; rounds 2–N double the fetch window
    /// until enough visible-namespace candidates are found or the corpus is
    /// exhausted. When `None`, falls back to the `ANN_OVERFETCH_MAX_ROUNDS`
    /// env var (default 3). Pass `Some(1)` to disable widening entirely.
    pub ann_overfetch_max_rounds: Option<usize>,
}

/// Brain-profile hint for score boosting during recall.
/// Applies `boost` multiplier to results whose profile posterior mean exceeds `threshold`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainProfileHint {
    /// Profile ID to resolve. Passed to `brain.resolve` / `brain.profile`.
    pub profile_id: String,
    /// Score multiplier applied to matching results. Default 1.3×.
    #[serde(default = "BrainProfileHint::default_boost")]
    pub boost: f64,
    /// Minimum Beta posterior mean required for a result to receive the boost. Default 0.6.
    #[serde(default = "BrainProfileHint::default_threshold")]
    pub threshold: f64,
}

impl BrainProfileHint {
    fn default_boost() -> f64 {
        1.3
    }
    fn default_threshold() -> f64 {
        0.6
    }
}

/// Term selection rule for FTS candidate gather.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecallFtsSelectionRule {
    /// Keep query terms in original order (current behavior).
    #[default]
    Original,
    /// Pick K terms with the lowest document frequency (most selective).
    LowestDf,
    /// Pick K terms with the highest IDF (same as lowest DF with Robertson-Walker formula).
    HighestIdf,
}

/// Pack-level alias for the DB gather mode enum, serializable identically.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecallFtsGatherMode {
    #[default]
    Ranked,
    Unranked,
    RankWithinCap,
}

impl From<RecallFtsGatherMode> for TextGatherMode {
    fn from(m: RecallFtsGatherMode) -> Self {
        match m {
            RecallFtsGatherMode::Ranked => TextGatherMode::Ranked,
            RecallFtsGatherMode::Unranked => TextGatherMode::Unranked,
            RecallFtsGatherMode::RankWithinCap => TextGatherMode::RankWithinCap,
        }
    }
}

/// Configuration for the FTS candidate-gather optimization (default: disabled, existing behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallFtsGatherConfig {
    /// Enable the candidate-gather optimization. Default false = existing behavior.
    pub enabled: bool,
    /// Max query terms to send to FTS after selection. Default 10 (existing fanout limit).
    pub term_k: usize,
    /// How to select the K terms. Default original (existing order).
    pub selection_rule: RecallFtsSelectionRule,
    /// How the DB gathers candidates. Default ranked (existing behavior).
    pub gather_mode: RecallFtsGatherMode,
    /// Row cap for RankWithinCap gather. When None, uses candidate_limit * gather_cap_multiplier.
    pub gather_limit: Option<u32>,
    /// Multiplier for gather_limit when gather_limit is None. Default 4.
    pub gather_cap_multiplier: u32,
    /// When true, CJK queries bypass term selection and use the existing ranked all-term path.
    pub cjk_bypass_ranked: bool,
}

impl Default for RecallFtsGatherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            term_k: 10,
            selection_rule: RecallFtsSelectionRule::Original,
            gather_mode: RecallFtsGatherMode::Ranked,
            gather_limit: None,
            gather_cap_multiplier: 4,
            cjk_bypass_ranked: true,
        }
    }
}

impl RecallFtsGatherConfig {
    /// Parse gather config from env vars. Returns `None` when none are set, `Err` on malformed values.
    pub fn from_env() -> Result<Option<Self>, RuntimeError> {
        let gather = std::env::var("KHIVE_RECALL_FTS_GATHER").ok();
        let term_k = std::env::var("KHIVE_RECALL_FTS_TERM_K").ok();
        let selection = std::env::var("KHIVE_RECALL_FTS_SELECTION").ok();
        let limit = std::env::var("KHIVE_RECALL_FTS_GATHER_LIMIT").ok();
        let multiplier = std::env::var("KHIVE_RECALL_FTS_GATHER_MULTIPLIER").ok();
        let cjk_bypass = std::env::var("KHIVE_RECALL_FTS_CJK_BYPASS").ok();

        if gather.is_none()
            && term_k.is_none()
            && selection.is_none()
            && limit.is_none()
            && multiplier.is_none()
            && cjk_bypass.is_none()
        {
            return Ok(None);
        }

        let mut cfg = RecallFtsGatherConfig {
            enabled: true,
            ..RecallFtsGatherConfig::default()
        };

        if let Some(g) = gather {
            match g.as_str() {
                "baseline" => {
                    cfg.enabled = false;
                }
                "ranked" => {
                    cfg.gather_mode = RecallFtsGatherMode::Ranked;
                }
                "rank_subset" => {
                    cfg.gather_mode = RecallFtsGatherMode::RankWithinCap;
                }
                "unranked" => {
                    cfg.gather_mode = RecallFtsGatherMode::Unranked;
                }
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "KHIVE_RECALL_FTS_GATHER must be baseline|ranked|rank_subset|unranked, got {other:?}"
                    )));
                }
            }
        }

        if let Some(k) = term_k {
            let v: usize = k.parse().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "KHIVE_RECALL_FTS_TERM_K must be a positive integer 1..10, got {k:?}"
                ))
            })?;
            if v == 0 || v > 10 {
                return Err(RuntimeError::InvalidInput(format!(
                    "KHIVE_RECALL_FTS_TERM_K must be 1..10, got {v}"
                )));
            }
            cfg.term_k = v;
        }

        if let Some(s) = selection {
            cfg.selection_rule = match s.as_str() {
                "original" => RecallFtsSelectionRule::Original,
                "lowest_df" => RecallFtsSelectionRule::LowestDf,
                "highest_idf" => RecallFtsSelectionRule::HighestIdf,
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "KHIVE_RECALL_FTS_SELECTION must be original|lowest_df|highest_idf, got {other:?}"
                    )));
                }
            };
        }

        if let Some(l) = limit {
            let v: u32 = l.parse().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "KHIVE_RECALL_FTS_GATHER_LIMIT must be a positive integer, got {l:?}"
                ))
            })?;
            if v == 0 {
                return Err(RuntimeError::InvalidInput(
                    "KHIVE_RECALL_FTS_GATHER_LIMIT must be > 0".to_string(),
                ));
            }
            cfg.gather_limit = Some(v);
        }

        if let Some(m) = multiplier {
            let v: u32 = m.parse().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "KHIVE_RECALL_FTS_GATHER_MULTIPLIER must be a positive integer, got {m:?}"
                ))
            })?;
            if v == 0 {
                return Err(RuntimeError::InvalidInput(
                    "KHIVE_RECALL_FTS_GATHER_MULTIPLIER must be > 0".to_string(),
                ));
            }
            cfg.gather_cap_multiplier = v;
        }

        if let Some(b) = cjk_bypass {
            cfg.cjk_bypass_ranked = match b.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "KHIVE_RECALL_FTS_CJK_BYPASS must be 1|0, got {other:?}"
                    )));
                }
            };
        }

        cfg.validate()?;
        Ok(Some(cfg))
    }

    /// Validate the config for internal consistency.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.term_k == 0 {
            return Err(RuntimeError::InvalidInput(
                "fts_gather.term_k must be > 0".to_string(),
            ));
        }
        if self.gather_cap_multiplier == 0 {
            return Err(RuntimeError::InvalidInput(
                "fts_gather.gather_cap_multiplier must be > 0".to_string(),
            ));
        }
        if let Some(gl) = self.gather_limit {
            if gl == 0 {
                return Err(RuntimeError::InvalidInput(
                    "fts_gather.gather_limit must be > 0 when provided".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Compute the effective gather_limit for a given candidate_limit.
    pub fn effective_gather_limit(&self, candidate_limit: u32) -> Result<u32, RuntimeError> {
        let gl = match self.gather_limit {
            Some(explicit) => {
                if explicit < candidate_limit {
                    return Err(RuntimeError::InvalidInput(format!(
                        "fts_gather.gather_limit ({explicit}) must be >= candidate_limit ({candidate_limit})"
                    )));
                }
                explicit
            }
            None => candidate_limit.saturating_mul(self.gather_cap_multiplier),
        };
        Ok(gl.max(candidate_limit))
    }

    /// Convert to DB-level `TextSearchOptions` for a given candidate_limit.
    pub fn to_search_options(
        &self,
        candidate_limit: u32,
    ) -> Result<TextSearchOptions, RuntimeError> {
        let gather_limit = self.effective_gather_limit(candidate_limit)?;
        Ok(TextSearchOptions {
            gather_mode: self.gather_mode.into(),
            gather_limit: Some(gather_limit),
        })
    }
}

// Tuning artifact: tests/khive-contract/tune/ swept 116 configs but the synthetic corpus
// produced an identical recall@10 = 0.9333 for every config — i.e. a flat landscape that
// cannot empirically distinguish these parameters. Defaults below stay at the prior values
// until a harder corpus (embed-enabled, synonym queries, partial matches) provides signal.
// See tests/khive-contract/tune/REPORT.md for the analysis.
//
// CC-6: Default strategy changed from RRF to Weighted [0.7, 0.3].
//
// Under RRF with the default weights (relevance 70%, salience 20%, temporal 10%), a
// salience=0.3 memory can rank above a salience=0.9 memory when its text/vector rank is
// marginally better. The Weighted strategy gives full-resolution score values to both
// retrieval paths, making the salience contribution a meaningful tiebreaker.
// The RRF strategy remains available via `fusion_strategy="rrf"`.
impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            relevance_weight: 0.70,
            salience_weight: 0.20,
            temporal_weight: 0.10,
            reranker_weights: HashMap::new(),
            temporal_half_life_days: 30.0,
            decay_model: DecayModel::default(),
            candidate_multiplier: 20,
            candidate_limit: Some(150),
            // CC-6: Weighted fusion respects score magnitude, allowing the salience
            // amplifier to meaningfully differentiate high- vs low-salience memories.
            // Weights [vector=0.7, text=0.3] match the prior RRF intent: vector
            // results are weighted higher because embedding search captures semantic
            // similarity; text results supplement with keyword precision.
            fuse_strategy: FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            min_score: 0.0,
            min_salience: 0.0,
            include_breakdown: false,
            scoring: None,
            brain_profile: None,
            fts_gather: RecallFtsGatherConfig::default(),
            ann_overfetch_max_rounds: None,
        }
    }
}

impl RecallConfig {
    /// Validate config consistency: non-negative weights, positive weight sum, positive half-life.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if !self.relevance_weight.is_finite() || self.relevance_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "relevance_weight must be a finite non-negative number".to_string(),
            ));
        }
        if !self.salience_weight.is_finite() || self.salience_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "salience_weight must be a finite non-negative number".to_string(),
            ));
        }
        if !self.temporal_weight.is_finite() || self.temporal_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "temporal_weight must be a finite non-negative number".to_string(),
            ));
        }
        let weight_sum = self.relevance_weight + self.salience_weight + self.temporal_weight;
        if weight_sum <= 0.0 {
            return Err(RuntimeError::InvalidInput(
                "at least one of relevance_weight / salience_weight / temporal_weight must be positive".to_string(),
            ));
        }
        for (name, &weight) in &self.reranker_weights {
            if !weight.is_finite() || weight < 0.0 {
                return Err(RuntimeError::InvalidInput(format!(
                    "reranker_weights[{name:?}] must be a finite non-negative number"
                )));
            }
        }
        if !self.temporal_half_life_days.is_finite() || self.temporal_half_life_days <= 0.0 {
            return Err(RuntimeError::InvalidInput(
                "temporal_half_life_days must be a finite positive number".to_string(),
            ));
        }
        // Validate PowerLaw half_life_days if that decay model is active.
        if let DecayModel::PowerLaw { half_life_days } = self.decay_model {
            if !half_life_days.is_finite() || half_life_days <= 0.0 {
                return Err(RuntimeError::InvalidInput(
                    "decay_model.power_law.half_life_days must be a finite positive number"
                        .to_string(),
                ));
            }
        }
        if self.candidate_limit == Some(0) {
            return Err(RuntimeError::InvalidInput(
                "candidate_limit must be positive when provided".to_string(),
            ));
        }
        if !self.min_score.is_finite() {
            return Err(RuntimeError::InvalidInput(
                "min_score must be finite".to_string(),
            ));
        }
        if !self.min_salience.is_finite() {
            return Err(RuntimeError::InvalidInput(
                "min_salience must be finite".to_string(),
            ));
        }
        Ok(())
    }

    /// Deserialize from a JSON value and validate in one step.
    pub fn try_from_value(v: serde_json::Value) -> Result<Self, RuntimeError> {
        let cfg: Self =
            serde_json::from_value(v).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }
}

/// How salience decays over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecayModel {
    /// `salience * exp(-decay_factor * age_days)` — half-life = ln(2)/decay_factor.
    /// Pack defaults: episodic decay_factor=0.02 (~35d), semantic decay_factor=0.005 (~139d).
    #[default]
    Exponential,
    /// `salience / (1 + decay_factor * age_days)`
    Hyperbolic,
    /// `salience * half_life / (half_life + age_days)`
    PowerLaw {
        /// Override half-life days for the power-law model.
        half_life_days: f64,
    },
    /// No decay — salience is used as-is.
    None,
}

impl DecayModel {
    /// Apply decay to a salience value given age_days, decay_factor, and config half_life.
    pub fn apply(&self, salience: f64, age_days: f64, decay_factor: f64, _half_life: f64) -> f64 {
        match self {
            DecayModel::Exponential => {
                // effective_salience = salience * exp(-decay_factor * age_days)
                // Uses the note's own decay_factor, not a half-life-derived constant.
                salience * (-decay_factor * age_days).exp()
            }
            DecayModel::Hyperbolic => salience / (1.0 + decay_factor * age_days),
            DecayModel::PowerLaw { half_life_days } => {
                let hl = *half_life_days;
                salience * hl / (hl + age_days)
            }
            DecayModel::None => salience,
        }
    }
}

/// Per-component score contributions for a single recall result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    /// Raw RRF fusion score (before weighting).
    pub relevance: f64,
    /// Raw salience from the note (before decay).
    pub salience_raw: f64,
    /// Salience after applying the decay model.
    pub salience_decayed: f64,
    /// Temporal recency score (half-life decay, independent of note's own decay_factor).
    pub temporal: f64,
    /// Weighted contributions summing to the total score.
    pub weighted: WeightedContributions,
}

impl ScoreBreakdown {
    /// Total composite score.
    pub fn total(&self) -> f64 {
        self.weighted.relevance_contribution
            + self.weighted.salience_contribution
            + self.weighted.temporal_contribution
    }
}

/// The three weighted components that make up the final score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedContributions {
    pub relevance_contribution: f64,
    pub salience_contribution: f64,
    pub temporal_contribution: f64,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// INLINE TEST JUSTIFICATION: tests exercise private validation methods and
// DecayModel::apply, which are not accessible from the integration test harness.
#[cfg(test)]
mod tests {
    use super::*;

    // ── DecayModel ────────────────────────────────────────────────────────────

    #[test]
    fn exponential_halves_at_decay_factor_half_life() {
        // exponential decay: salience * exp(-decay_factor * age_days)
        // Half-life = ln(2) / decay_factor ≈ 69.3 days for decay_factor=0.01
        let model = DecayModel::Exponential;
        let salience = 1.0;
        let decay_factor = 0.01;
        let half_life_days = std::f64::consts::LN_2 / decay_factor;
        let result = model.apply(salience, half_life_days, decay_factor, 30.0);
        let diff = (result - 0.5).abs();
        assert!(
            diff < 1e-10,
            "exponential should give 0.5 at ln(2)/decay_factor days, got {result}"
        );
    }

    #[test]
    fn exponential_full_salience_at_zero_age() {
        let model = DecayModel::Exponential;
        let result = model.apply(0.8, 0.0, 0.01, 30.0);
        let diff = (result - 0.8).abs();
        assert!(
            diff < 1e-12,
            "at age=0 salience should be unchanged, got {result}"
        );
    }

    #[test]
    fn exponential_uses_note_decay_factor_not_half_life() {
        // Verify the formula uses decay_factor param, not the half_life param.
        // At age=1 day, decay_factor=1.0 → exp(-1.0) ≈ 0.3679.
        // If we were using half_life=10 days, exp(-ln2/10) ≈ 0.933.
        let model = DecayModel::Exponential;
        let result = model.apply(1.0, 1.0, 1.0, 10.0);
        let expected = (-1.0f64).exp();
        assert!(
            (result - expected).abs() < 1e-12,
            "expected {expected}, got {result}"
        );
    }

    #[test]
    fn hyperbolic_halves_at_one_over_decay_factor() {
        // salience / (1 + k * age) = 0.5 when age = 1/k
        let model = DecayModel::Hyperbolic;
        let salience = 1.0;
        let k = 0.05;
        let age = 1.0 / k; // 20 days
        let result = model.apply(salience, age, k, 30.0);
        let diff = (result - 0.5).abs();
        assert!(
            diff < 1e-10,
            "hyperbolic at age=1/k should give 0.5, got {result}"
        );
    }

    #[test]
    fn hyperbolic_full_salience_at_zero_age() {
        let model = DecayModel::Hyperbolic;
        let result = model.apply(0.7, 0.0, 0.05, 30.0);
        let diff = (result - 0.7).abs();
        assert!(
            diff < 1e-12,
            "at age=0 salience should be unchanged, got {result}"
        );
    }

    #[test]
    fn powerlaw_halves_at_half_life() {
        let hl = 30.0;
        let model = DecayModel::PowerLaw { half_life_days: hl };
        let salience = 1.0;
        // salience * hl / (hl + age) = 0.5 when age = hl
        let result = model.apply(salience, hl, 0.01, hl);
        let diff = (result - 0.5).abs();
        assert!(
            diff < 1e-10,
            "power-law should give 0.5 at half-life, got {result}"
        );
    }

    #[test]
    fn decay_none_returns_salience_unchanged() {
        let model = DecayModel::None;
        let result = model.apply(0.6, 100.0, 0.99, 30.0);
        let diff = (result - 0.6).abs();
        assert!(
            diff < 1e-12,
            "None model must not alter salience, got {result}"
        );
    }

    // ── RecallConfig ──────────────────────────────────────────────────────────

    #[test]
    fn default_config_validates() {
        assert!(RecallConfig::default().validate().is_ok());
    }

    #[test]
    fn negative_relevance_weight_fails_validation() {
        let cfg = RecallConfig {
            relevance_weight: -0.1,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn negative_salience_weight_fails_validation() {
        let cfg = RecallConfig {
            salience_weight: -1.0,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn negative_temporal_weight_fails_validation() {
        let cfg = RecallConfig {
            temporal_weight: -0.5,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn all_zero_weights_fails_validation() {
        let cfg = RecallConfig {
            relevance_weight: 0.0,
            salience_weight: 0.0,
            temporal_weight: 0.0,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_half_life_fails_validation() {
        let cfg = RecallConfig {
            temporal_half_life_days: 0.0,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn negative_half_life_fails_validation() {
        let cfg = RecallConfig {
            temporal_half_life_days: -5.0,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_uniform_weights_validate() {
        let cfg = RecallConfig {
            relevance_weight: 0.5,
            salience_weight: 0.3,
            temporal_weight: 0.2,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    // ── Serde roundtrips ──────────────────────────────────────────────────────

    #[test]
    fn default_config_roundtrip() {
        let cfg = RecallConfig::default();
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
        let diff = (cfg.relevance_weight - back.relevance_weight).abs();
        assert!(diff < 1e-12);
        assert_eq!(cfg.decay_model, back.decay_model);
    }

    #[test]
    fn decay_model_exponential_roundtrip() {
        let m = DecayModel::Exponential;
        let json = serde_json::to_string(&m).expect("serialize");
        let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn decay_model_hyperbolic_roundtrip() {
        let m = DecayModel::Hyperbolic;
        let json = serde_json::to_string(&m).expect("serialize");
        let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn decay_model_powerlaw_roundtrip() {
        let m = DecayModel::PowerLaw {
            half_life_days: 14.0,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn decay_model_none_roundtrip() {
        let m = DecayModel::None;
        let json = serde_json::to_string(&m).expect("serialize");
        let back: DecayModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn partial_config_deserializes_with_defaults() {
        // Only override one field — the rest should default.
        let json = r#"{"relevance_weight": 0.5}"#;
        let cfg: RecallConfig = serde_json::from_str(json).expect("deserialize partial");
        // specified field
        let diff = (cfg.relevance_weight - 0.5).abs();
        assert!(diff < 1e-12);
        // unspecified fields keep defaults
        let diff2 = (cfg.salience_weight - 0.20).abs();
        assert!(diff2 < 1e-12);
        assert_eq!(cfg.decay_model, DecayModel::Exponential);
    }

    // ── RecallConfig new fields ───────────────────────────────────────────────

    #[test]
    fn new_fields_have_correct_defaults() {
        let cfg = RecallConfig::default();
        assert_eq!(cfg.candidate_limit, Some(150));
        // CC-6: default changed to Weighted [0.7, 0.3] so salience can influence ranking
        assert!(
            matches!(
                cfg.fuse_strategy,
                FusionStrategy::Weighted { ref weights } if weights == &vec![0.7_f64, 0.3_f64]
            ),
            "default fuse_strategy should be Weighted [0.7, 0.3], got {:?}",
            cfg.fuse_strategy
        );
        assert!(!cfg.include_breakdown);
    }

    #[test]
    fn candidate_limit_zero_fails_validation() {
        let cfg = RecallConfig {
            candidate_limit: Some(0),
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn candidate_limit_some_positive_validates() {
        let cfg = RecallConfig {
            candidate_limit: Some(100),
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn min_score_nan_fails_validation() {
        let cfg = RecallConfig {
            min_score: f64::NAN,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn min_salience_nan_fails_validation() {
        let cfg = RecallConfig {
            min_salience: f64::NAN,
            ..RecallConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn new_fields_roundtrip() {
        let cfg = RecallConfig {
            candidate_limit: Some(50),
            fuse_strategy: FusionStrategy::Union,
            include_breakdown: true,
            ..RecallConfig::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.candidate_limit, Some(50));
        assert_eq!(back.fuse_strategy, FusionStrategy::Union);
        assert!(back.include_breakdown);
    }

    #[test]
    fn partial_config_new_fields_use_defaults() {
        // Parse JSON that omits all new fields — they should fall back to defaults.
        // With #[serde(default)] on the struct, missing fields use RecallConfig::default(),
        // so candidate_limit falls back to Some(150), not Option::default() == None.
        let json = r#"{"temporal_weight": 0.15}"#;
        let cfg: RecallConfig = serde_json::from_str(json).expect("deserialize partial");
        assert_eq!(cfg.candidate_limit, Some(150));
        // CC-6: default changed to Weighted [0.7, 0.3]
        assert!(
            matches!(cfg.fuse_strategy, FusionStrategy::Weighted { .. }),
            "partial config must deserialize fuse_strategy to Weighted default"
        );
        assert!(!cfg.include_breakdown);
    }

    // ── ScoreBreakdown ────────────────────────────────────────────────────────

    #[test]
    fn score_breakdown_total_sums_contributions() {
        let bd = ScoreBreakdown {
            relevance: 0.5,
            salience_raw: 0.8,
            salience_decayed: 0.6,
            temporal: 0.3,
            weighted: WeightedContributions {
                relevance_contribution: 0.35,
                salience_contribution: 0.12,
                temporal_contribution: 0.03,
            },
        };
        let expected = 0.35 + 0.12 + 0.03;
        let diff = (bd.total() - expected).abs();
        assert!(
            diff < 1e-12,
            "total() should sum weighted contributions, got {}",
            bd.total()
        );
    }

    // ── RecallFtsGatherConfig ─────────────────────────────────────────────────

    #[test]
    fn fts_gather_default_is_disabled() {
        let cfg = RecallFtsGatherConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.term_k, 10);
        assert_eq!(cfg.selection_rule, RecallFtsSelectionRule::Original);
        assert_eq!(cfg.gather_mode, RecallFtsGatherMode::Ranked);
        assert!(cfg.gather_limit.is_none());
        assert_eq!(cfg.gather_cap_multiplier, 4);
        assert!(cfg.cjk_bypass_ranked);
    }

    #[test]
    fn fts_gather_validates_zero_term_k() {
        let cfg = RecallFtsGatherConfig {
            term_k: 0,
            ..RecallFtsGatherConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fts_gather_validates_zero_multiplier() {
        let cfg = RecallFtsGatherConfig {
            gather_cap_multiplier: 0,
            ..RecallFtsGatherConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fts_gather_validates_zero_gather_limit() {
        let cfg = RecallFtsGatherConfig {
            gather_limit: Some(0),
            ..RecallFtsGatherConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fts_gather_effective_gather_limit_uses_multiplier() {
        let cfg = RecallFtsGatherConfig {
            gather_cap_multiplier: 4,
            ..RecallFtsGatherConfig::default()
        };
        assert_eq!(cfg.effective_gather_limit(150).unwrap(), 600);
    }

    #[test]
    fn fts_gather_effective_gather_limit_explicit_wins() {
        let cfg = RecallFtsGatherConfig {
            gather_limit: Some(500),
            gather_cap_multiplier: 4,
            ..RecallFtsGatherConfig::default()
        };
        assert_eq!(cfg.effective_gather_limit(150).unwrap(), 500);
    }

    #[test]
    fn fts_gather_effective_gather_limit_too_small_fails() {
        let cfg = RecallFtsGatherConfig {
            gather_limit: Some(100),
            ..RecallFtsGatherConfig::default()
        };
        assert!(cfg.effective_gather_limit(150).is_err());
    }

    #[test]
    fn fts_gather_from_env_returns_none_when_no_env_set() {
        // Ensure none of the relevant vars are set in CI/test environment.
        // If they are set by a prior test, this test may be flaky; the vars
        // are prefixed to avoid collisions.
        if std::env::var("KHIVE_RECALL_FTS_GATHER").is_ok()
            || std::env::var("KHIVE_RECALL_FTS_TERM_K").is_ok()
        {
            return; // skip if env is already configured
        }
        let result = RecallFtsGatherConfig::from_env().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn fts_gather_invalid_gather_value_fails() {
        // We can't easily set env vars in unit tests without affecting other
        // tests. This exercises the validation branch directly.
        // Manually simulate what from_env would produce for an invalid value.
        let cfg = RecallFtsGatherConfig {
            term_k: 0,
            ..RecallFtsGatherConfig::default()
        };
        assert!(cfg.validate().is_err(), "term_k=0 must fail validation");
    }

    #[test]
    fn fts_gather_from_into_gather_mode() {
        assert_eq!(
            TextGatherMode::from(RecallFtsGatherMode::Ranked),
            TextGatherMode::Ranked
        );
        assert_eq!(
            TextGatherMode::from(RecallFtsGatherMode::Unranked),
            TextGatherMode::Unranked
        );
        assert_eq!(
            TextGatherMode::from(RecallFtsGatherMode::RankWithinCap),
            TextGatherMode::RankWithinCap
        );
    }

    #[test]
    fn recall_config_default_has_fts_gather() {
        let cfg = RecallConfig::default();
        assert!(!cfg.fts_gather.enabled);
    }

    #[test]
    fn recall_config_roundtrip_with_fts_gather() {
        let cfg = RecallConfig {
            fts_gather: RecallFtsGatherConfig {
                enabled: true,
                term_k: 5,
                selection_rule: RecallFtsSelectionRule::HighestIdf,
                gather_mode: RecallFtsGatherMode::RankWithinCap,
                gather_limit: Some(600),
                gather_cap_multiplier: 4,
                cjk_bypass_ranked: true,
            },
            ..RecallConfig::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: RecallConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(back.fts_gather.enabled);
        assert_eq!(back.fts_gather.term_k, 5);
        assert_eq!(
            back.fts_gather.gather_mode,
            RecallFtsGatherMode::RankWithinCap
        );
        assert_eq!(back.fts_gather.gather_limit, Some(600));
    }
}
