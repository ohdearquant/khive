//! Brain-tunable parameter surface for the memory pack's recall scoring pipeline.

use khive_brain_core::BalancedRecallState;
use khive_pack_brain::tunable::{PackTunable, ParameterDef, ParameterSpace};
use khive_runtime::RuntimeError;
use serde_json::Value;

use crate::config::RecallConfig;
use crate::MemoryPack;

/// `MemoryPack` implements `PackTunable` so that the brain can adjust the
/// recall scoring pipeline based on observed usage patterns (Issue #159).
///
/// Parameter names (`memory::relevance_weight`, `memory::salience_weight`,
/// `memory::temporal_weight`) correspond to the three Beta posteriors in
/// `BalancedRecallState`. Posterior means flow directly into
/// `RecallConfig`.
///
/// `project_config` reads posterior means → `RecallConfig`.
/// `apply_config` validates and stores the new config; future recall calls
/// pick it up via `MemoryPack::active_config()`.
impl PackTunable for MemoryPack {
    fn parameter_space(&self) -> ParameterSpace {
        ParameterSpace {
            parameters: vec![
                ParameterDef {
                    name: "memory::relevance_weight".into(),
                    // Prior: relevance is the dominant signal (7:3), matching
                    // BalancedRecallState's `relevance` posterior prior.
                    prior_alpha: 7.0,
                    prior_beta: 3.0,
                    bounds: (0.0, 1.0),
                },
                ParameterDef {
                    name: "memory::salience_weight".into(),
                    // Prior: salience is secondary (2:8).
                    prior_alpha: 2.0,
                    prior_beta: 8.0,
                    bounds: (0.0, 1.0),
                },
                ParameterDef {
                    name: "memory::temporal_weight".into(),
                    // Prior: temporal is weakest signal (1:9).
                    prior_alpha: 1.0,
                    prior_beta: 9.0,
                    bounds: (0.0, 1.0),
                },
            ],
        }
    }

    /// Project the current `BalancedRecallState` posteriors into a `RecallConfig` value.
    ///
    /// Reads the three posterior means from the profile state. Falls back to the
    /// current active config if a parameter is absent (brain not yet warmed up).
    fn project_config(&self, state: &BalancedRecallState) -> Value {
        let current = self.active_config();

        let relevance = state.relevance.mean();
        let salience = state.salience.mean();
        let temporal = state.temporal.mean();

        let projected = RecallConfig {
            relevance_weight: relevance,
            salience_weight: salience,
            temporal_weight: temporal,
            ..current
        };

        serde_json::to_value(projected).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Apply a projected config to the pack.
    ///
    /// Deserializes the JSON value into a `RecallConfig`, validates it, and
    /// stores it as the active config. Future recall calls pick up the new
    /// weights via `MemoryPack::active_config()`.
    fn apply_config(&self, config: Value) -> Result<(), RuntimeError> {
        let new_cfg: RecallConfig = serde_json::from_value(config)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid RecallConfig: {e}")))?;
        new_cfg.validate()?;
        *self.config.lock().unwrap() = new_cfg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_brain_core::{BalancedRecallState, BetaPosterior};
    use khive_runtime::KhiveRuntime;

    fn make_pack() -> MemoryPack {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        MemoryPack::new(rt)
    }

    fn balanced_state_with_means(
        relevance_mean: f64,
        salience_mean: f64,
        temporal_mean: f64,
    ) -> BalancedRecallState {
        // Construct Beta posteriors whose means match the supplied values.
        // Using ESS=10 for each: alpha = mean * 10, beta = (1-mean) * 10.
        let to_posterior =
            |mean: f64| -> BetaPosterior { BetaPosterior::new(mean * 10.0, (1.0 - mean) * 10.0) };
        let mut state = BalancedRecallState::new(100);
        state.relevance = to_posterior(relevance_mean);
        state.salience = to_posterior(salience_mean);
        state.temporal = to_posterior(temporal_mean);
        state
    }

    #[test]
    fn parameter_space_has_three_params() {
        let pack = make_pack();
        let space = pack.parameter_space();
        assert_eq!(space.parameters.len(), 3);
        let names: Vec<&str> = space.parameters.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"memory::relevance_weight"));
        assert!(names.contains(&"memory::salience_weight"));
        assert!(names.contains(&"memory::temporal_weight"));
    }

    #[test]
    fn project_config_reads_posterior_means() {
        let pack = make_pack();
        let state = balanced_state_with_means(0.6, 0.3, 0.1);
        let projected = pack.project_config(&state);

        let cfg: RecallConfig = serde_json::from_value(projected).unwrap();
        assert!((cfg.relevance_weight - 0.6).abs() < 1e-10);
        assert!((cfg.salience_weight - 0.3).abs() < 1e-10);
        assert!((cfg.temporal_weight - 0.1).abs() < 1e-10);
    }

    #[test]
    fn project_config_with_default_priors_matches_expected_defaults() {
        // Default BalancedRecallState priors: Beta(7,3)=0.7, Beta(2,8)=0.2, Beta(1,9)=0.1
        let pack = make_pack();
        let state = BalancedRecallState::new(100);
        let projected = pack.project_config(&state);

        let cfg: RecallConfig = serde_json::from_value(projected).unwrap();
        assert!((cfg.relevance_weight - 0.70).abs() < 1e-10);
        assert!((cfg.salience_weight - 0.20).abs() < 1e-10);
        assert!((cfg.temporal_weight - 0.10).abs() < 1e-10);
    }

    #[test]
    fn apply_config_updates_active_config() {
        let pack = make_pack();
        let new_cfg = RecallConfig {
            relevance_weight: 0.5,
            salience_weight: 0.3,
            temporal_weight: 0.2,
            ..RecallConfig::default()
        };
        let config_value = serde_json::to_value(&new_cfg).unwrap();
        pack.apply_config(config_value)
            .expect("apply_config succeeds");

        let active = pack.active_config();
        assert!((active.relevance_weight - 0.5).abs() < 1e-10);
        assert!((active.salience_weight - 0.3).abs() < 1e-10);
        assert!((active.temporal_weight - 0.2).abs() < 1e-10);
    }

    #[test]
    fn apply_config_rejects_all_zero_weights() {
        let pack = make_pack();
        let bad_cfg = RecallConfig {
            relevance_weight: 0.0,
            salience_weight: 0.0,
            temporal_weight: 0.0,
            ..RecallConfig::default()
        };
        let config_value = serde_json::to_value(&bad_cfg).unwrap();
        assert!(pack.apply_config(config_value).is_err());
    }

    #[test]
    fn apply_config_rejects_malformed_json() {
        let pack = make_pack();
        let bad = serde_json::json!({ "relevance_weight": "not_a_number" });
        assert!(pack.apply_config(bad).is_err());
    }

    #[test]
    fn prior_for_relevance_weight_matches_balanced_recall_state_prior() {
        // BalancedRecallState uses Beta(7,3) for relevance; ParameterDef must match.
        let pack = make_pack();
        let space = pack.parameter_space();
        let def = space
            .parameters
            .iter()
            .find(|p| p.name == "memory::relevance_weight")
            .unwrap();
        assert!((def.prior_alpha - 7.0).abs() < 1e-12);
        assert!((def.prior_beta - 3.0).abs() < 1e-12);
    }
}
