use serde::{Deserialize, Serialize};

use khive_runtime::{FusionStrategy, RuntimeError};

/// Configuration for the recall scoring pipeline.
/// All fields have sensible defaults matching current behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    // --- Fusion weights ---
    /// Weight of RRF/fusion score. Default 0.70.
    pub relevance_weight: f64,
    /// Weight of decay-adjusted salience. Default 0.20.
    pub importance_weight: f64,
    /// Weight of pure recency. Default 0.10.
    pub temporal_weight: f64,

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
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            relevance_weight: 0.70,
            importance_weight: 0.20,
            temporal_weight: 0.10,
            temporal_half_life_days: 30.0,
            decay_model: DecayModel::default(),
            candidate_multiplier: 20,
            candidate_limit: None,
            fuse_strategy: FusionStrategy::default(),
            min_score: 0.0,
            min_salience: 0.0,
            include_breakdown: false,
        }
    }
}

impl RecallConfig {
    /// Validate that the config is internally consistent.
    ///
    /// Rejects:
    /// - Negative weights
    /// - All three weights summing to zero (no scoring signal)
    /// - Non-positive temporal half-life
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.relevance_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "relevance_weight must be non-negative".to_string(),
            ));
        }
        if self.importance_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "importance_weight must be non-negative".to_string(),
            ));
        }
        if self.temporal_weight < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "temporal_weight must be non-negative".to_string(),
            ));
        }
        let weight_sum = self.relevance_weight + self.importance_weight + self.temporal_weight;
        if weight_sum <= 0.0 {
            return Err(RuntimeError::InvalidInput(
                "at least one of relevance_weight / importance_weight / temporal_weight must be positive".to_string(),
            ));
        }
        if self.temporal_half_life_days <= 0.0 {
            return Err(RuntimeError::InvalidInput(
                "temporal_half_life_days must be positive".to_string(),
            ));
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
}

/// How salience decays over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecayModel {
    /// `salience * exp(-age * ln2 / half_life)`
    ///
    /// This is the original formula; it is the default.
    #[default]
    Exponential,
    /// `salience / (1 + decay_factor * age_days)`
    Hyperbolic,
    /// `salience * half_life / (half_life + age_days)`
    PowerLaw {
        /// Override half-life (days) for the power-law model.
        /// Falls back to RecallConfig.temporal_half_life_days when absent.
        half_life_days: f64,
    },
    /// No decay — salience is used as-is.
    None,
}

impl DecayModel {
    /// Apply decay to a salience value.
    ///
    /// - `salience`    — raw importance in [0, 1]
    /// - `age_days`    — age of the note in days
    /// - `decay_factor`— per-note decay rate stored on the note (used by Exponential and Hyperbolic)
    /// - `half_life`   — config half-life, used by Exponential (as formula half-life) and PowerLaw
    pub fn apply(&self, salience: f64, age_days: f64, decay_factor: f64, half_life: f64) -> f64 {
        match self {
            DecayModel::Exponential => {
                // Uses the proper half-life formula: exp(-age * ln2 / half_life)
                // This gives exactly 0.5 at age == half_life.
                let k = std::f64::consts::LN_2 / half_life;
                salience * (-k * age_days).exp()
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
    pub importance_raw: f64,
    /// Salience after applying the decay model.
    pub importance_decayed: f64,
    /// Temporal recency score (half-life decay, independent of note's own decay_factor).
    pub temporal: f64,
    /// Weighted contributions summing to the total score.
    pub weighted: WeightedContributions,
}

impl ScoreBreakdown {
    /// Total composite score.
    pub fn total(&self) -> f64 {
        self.weighted.relevance_contribution
            + self.weighted.importance_contribution
            + self.weighted.temporal_contribution
    }
}

/// The three weighted components that make up the final score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedContributions {
    pub relevance_contribution: f64,
    pub importance_contribution: f64,
    pub temporal_contribution: f64,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── DecayModel ────────────────────────────────────────────────────────────

    #[test]
    fn exponential_halves_at_half_life() {
        let model = DecayModel::Exponential;
        let salience = 1.0;
        let half_life = 30.0;
        let result = model.apply(salience, half_life, 0.01, half_life);
        let diff = (result - 0.5).abs();
        assert!(
            diff < 1e-10,
            "exponential should give 0.5 at half-life, got {result}"
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
    fn negative_importance_weight_fails_validation() {
        let cfg = RecallConfig {
            importance_weight: -1.0,
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
            importance_weight: 0.0,
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
            importance_weight: 0.3,
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
        let diff2 = (cfg.importance_weight - 0.20).abs();
        assert!(diff2 < 1e-12);
        assert_eq!(cfg.decay_model, DecayModel::Exponential);
    }

    // ── RecallConfig new fields ───────────────────────────────────────────────

    #[test]
    fn new_fields_have_correct_defaults() {
        let cfg = RecallConfig::default();
        assert_eq!(cfg.candidate_limit, None);
        assert_eq!(cfg.fuse_strategy, FusionStrategy::Rrf { k: 60 });
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
        let json = r#"{"temporal_weight": 0.15}"#;
        let cfg: RecallConfig = serde_json::from_str(json).expect("deserialize partial");
        assert_eq!(cfg.candidate_limit, None);
        assert_eq!(cfg.fuse_strategy, FusionStrategy::Rrf { k: 60 });
        assert!(!cfg.include_breakdown);
    }

    // ── ScoreBreakdown ────────────────────────────────────────────────────────

    #[test]
    fn score_breakdown_total_sums_contributions() {
        let bd = ScoreBreakdown {
            relevance: 0.5,
            importance_raw: 0.8,
            importance_decayed: 0.6,
            temporal: 0.3,
            weighted: WeightedContributions {
                relevance_contribution: 0.35,
                importance_contribution: 0.12,
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
}
