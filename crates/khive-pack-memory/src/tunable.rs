use khive_pack_brain::state::BrainState;
use khive_pack_brain::tunable::{PackTunable, ParameterDef, ParameterSpace};
use khive_runtime::RuntimeError;
use serde_json::Value;

use crate::config::RecallConfig;
use crate::MemoryPack;

/// `MemoryPack` implements `PackTunable` so that the brain can adjust the
/// recall scoring pipeline based on observed usage patterns (Issue #159).
///
/// Parameter names (`memory::relevance_weight`, `memory::importance_weight`,
/// `memory::temporal_weight`) match the keys that brain's `EventFold` tracks,
/// so posteriors from real-time dispatch events flow directly into these params.
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
                    // EventFold's initial "recall::relevance_weight" posterior.
                    prior_alpha: 7.0,
                    prior_beta: 3.0,
                    bounds: (0.0, 1.0),
                },
                ParameterDef {
                    name: "memory::importance_weight".into(),
                    // Prior: importance is secondary (2:8).
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

    /// Project the current `BrainState` posteriors into a `RecallConfig` value.
    ///
    /// Reads `memory::*_weight` posterior means from `state`. Falls back to the
    /// current active config if a parameter is absent (brain not yet warmed up).
    fn project_config(&self, state: &BrainState) -> Value {
        let current = self.active_config();

        let relevance = state
            .parameters
            .get("memory::relevance_weight")
            .map(|p| p.mean())
            .unwrap_or(current.relevance_weight);

        let importance = state
            .parameters
            .get("memory::importance_weight")
            .map(|p| p.mean())
            .unwrap_or(current.importance_weight);

        let temporal = state
            .parameters
            .get("memory::temporal_weight")
            .map(|p| p.mean())
            .unwrap_or(current.temporal_weight);

        let projected = RecallConfig {
            relevance_weight: relevance,
            importance_weight: importance,
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
    use khive_pack_brain::state::BetaPosterior;
    use khive_runtime::KhiveRuntime;
    use std::collections::HashMap;

    fn make_pack() -> MemoryPack {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        MemoryPack::new(rt)
    }

    fn brain_state_with_params(params: HashMap<String, BetaPosterior>) -> BrainState {
        BrainState::new(params, 100)
    }

    #[test]
    fn parameter_space_has_three_params() {
        let pack = make_pack();
        let space = pack.parameter_space();
        assert_eq!(space.parameters.len(), 3);
        let names: Vec<&str> = space.parameters.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"memory::relevance_weight"));
        assert!(names.contains(&"memory::importance_weight"));
        assert!(names.contains(&"memory::temporal_weight"));
    }

    #[test]
    fn project_config_reads_posterior_means() {
        let pack = make_pack();
        let mut params = HashMap::new();
        params.insert(
            "memory::relevance_weight".into(),
            BetaPosterior::new(6.0, 4.0), // mean = 0.6
        );
        params.insert(
            "memory::importance_weight".into(),
            BetaPosterior::new(3.0, 7.0), // mean = 0.3
        );
        params.insert(
            "memory::temporal_weight".into(),
            BetaPosterior::new(1.0, 9.0), // mean = 0.1
        );
        let state = brain_state_with_params(params);
        let projected = pack.project_config(&state);

        let cfg: RecallConfig = serde_json::from_value(projected).unwrap();
        assert!((cfg.relevance_weight - 0.6).abs() < 1e-10);
        assert!((cfg.importance_weight - 0.3).abs() < 1e-10);
        assert!((cfg.temporal_weight - 0.1).abs() < 1e-10);
    }

    #[test]
    fn project_config_falls_back_to_active_when_param_absent() {
        let pack = make_pack();
        let state = brain_state_with_params(HashMap::new());
        let projected = pack.project_config(&state);

        let cfg: RecallConfig = serde_json::from_value(projected).unwrap();
        assert!((cfg.relevance_weight - 0.70).abs() < 1e-10);
        assert!((cfg.importance_weight - 0.20).abs() < 1e-10);
        assert!((cfg.temporal_weight - 0.10).abs() < 1e-10);
    }

    #[test]
    fn apply_config_updates_active_config() {
        let pack = make_pack();
        let new_cfg = RecallConfig {
            relevance_weight: 0.5,
            importance_weight: 0.3,
            temporal_weight: 0.2,
            ..RecallConfig::default()
        };
        let config_value = serde_json::to_value(&new_cfg).unwrap();
        pack.apply_config(config_value).expect("apply_config succeeds");

        let active = pack.active_config();
        assert!((active.relevance_weight - 0.5).abs() < 1e-10);
        assert!((active.importance_weight - 0.3).abs() < 1e-10);
        assert!((active.temporal_weight - 0.2).abs() < 1e-10);
    }

    #[test]
    fn apply_config_rejects_all_zero_weights() {
        let pack = make_pack();
        let bad_cfg = RecallConfig {
            relevance_weight: 0.0,
            importance_weight: 0.0,
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
    fn prior_for_relevance_weight_matches_fold_priors() {
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
