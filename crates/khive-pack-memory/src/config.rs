//! Recall configuration types — scoring weights, decay models, and FTS gather options.
//! See `crates/khive-pack-memory/docs/api/configuration.md` for the full policy reference.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use khive_fusion::FusionStrategy;
use khive_runtime::RuntimeError;
use khive_storage::types::TextSearchOptions;

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
    /// Strategy used to fuse retrieval lists. Default `Rrf { k: 10 }` (calibrated).
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
    /// Maximum rounds for the ANN namespace over-fetch retry loop. Round 1 is
    /// the initial over-fetch; rounds 2–N double the fetch window until enough
    /// visible-namespace candidates are found or the corpus is exhausted. When
    /// `None`, falls back to the `ANN_OVERFETCH_MAX_ROUNDS` env var (default 3).
    /// Pass `Some(1)` to disable widening entirely.
    pub ann_overfetch_max_rounds: Option<usize>,

    /// Bounded wait (milliseconds) for a cold-miss `ensure_ann_for_model` call
    /// on the recall path before degrading that model's contribution to
    /// FTS-only (#836). When `None`, falls back to the
    /// `KHIVE_MEMORY_ANN_READY_TIMEOUT_MS` env var (default 8000ms — see
    /// `handlers::common::ann_ready_timeout_ms`).
    ///
    /// See `crates/khive-pack-memory/docs/api/configuration.md` for the operational rationale.
    pub ann_ready_timeout_ms: Option<u64>,
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

/// Public recall path for the canonical storage gather mode, including variant imports.
pub use khive_storage::types::TextGatherMode as RecallFtsGatherMode;

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
            gather_mode: self.gather_mode,
            gather_limit: Some(gather_limit),
        })
    }
}

// Default selection rationale: crates/khive-pack-memory/docs/api/configuration.md#default-selection-rationale.
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
            fuse_strategy: FusionStrategy::Rrf { k: 10 },
            min_score: 0.0,
            min_salience: 0.0,
            include_breakdown: false,
            scoring: None,
            brain_profile: None,
            fts_gather: RecallFtsGatherConfig::default(),
            ann_overfetch_max_rounds: None,
            ann_ready_timeout_ms: None,
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
    /// ADR-104 §3: multiplicative contribution of serve-time profile-weight
    /// projection relative to configured-default weights for this hit —
    /// `score_with_projected_weights / score_with_default_weights`. `1.0`
    /// when no profile served the request (component 1 never ran) or the
    /// default-weight score is ~0 (div-by-zero guard).
    pub profile_component: f64,
    /// ADR-104 §3: the serving profile's per-entity Beta posterior mean for this UUID.
    /// `None` without a serving profile or a posterior beyond the uninformative prior.
    /// Multiplies `rank_score` by
    /// `clamp(1 + 0.3 * (entity_posterior_mean - 0.5), 0.85, 1.15)`;
    /// `None` or a mean of 0.5 is neutral. See
    /// `crates/khive-pack-memory/docs/api/configuration.md#score-breakdowns`.
    pub entity_posterior_mean: Option<f64>,
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

// ── English-only embedding models ─────────────────────────────────────────────

/// Canonical names of registered embedding models that cannot read CJK text.
///
/// Neither lattice's `EmbeddingModel` nor the runtime's embedder registry records which
/// languages a model covers, so the models recall must not fuse for a CJK-dominant query are
/// named here. `all-minilm-l6-v2` uses an English WordPiece vocabulary in which most CJK
/// characters map to `[UNK]`, so its similarity scores for CJK text carry no signal. Add the
/// canonical name of any other model with the same limitation.
pub(crate) const ENGLISH_ONLY_EMBEDDING_MODELS: &[&str] = &["all-minilm-l6-v2"];

/// Whether `model` is listed in [`ENGLISH_ONLY_EMBEDDING_MODELS`], compared ASCII
/// case-insensitively.
pub(crate) fn is_english_only_embedding_model(model: &str) -> bool {
    ENGLISH_ONLY_EMBEDDING_MODELS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(model))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// INLINE TEST JUSTIFICATION: tests exercise private validation methods and
// DecayModel::apply, which are not accessible from the integration test harness.
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
