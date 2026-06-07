//! Optional auto-tuning extension trait for packs that expose parameter spaces.

use khive_runtime::pack::PackRuntime;
use khive_runtime::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{BalancedRecallState, BetaPosterior};

/// Packs that want auto-tuning implement this trait.
///
/// The brain discovers tunable packs at startup via the PackRegistry.
/// `project_config` now receives a `BalancedRecallState` — the v1 profile
/// state — rather than the old flat `BrainState` scalar map.
/// Extension trait for packs that expose a parameter space to brain auto-tuning.
pub trait PackTunable: PackRuntime {
    /// Describe the parameter space this pack exposes to brain.
    fn parameter_space(&self) -> ParameterSpace;
    /// Project a live `BalancedRecallState` into a config `Value` for this pack.
    fn project_config(&self, state: &BalancedRecallState) -> Value;
    /// Apply a projected config to the pack's runtime state.
    ///
    /// # Errors
    ///
    /// Returns `RuntimeError::InvalidInput` when the config is malformed.
    fn apply_config(&self, config: Value) -> Result<(), RuntimeError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSpace {
    pub parameters: Vec<ParameterDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterDef {
    pub name: String,
    pub prior_alpha: f64,
    pub prior_beta: f64,
    pub bounds: (f64, f64),
}

impl ParameterDef {
    pub fn prior(&self) -> BetaPosterior {
        BetaPosterior::new(self.prior_alpha, self.prior_beta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_def_prior_returns_matching_beta_posterior() {
        let def = ParameterDef {
            name: "recall::relevance_weight".into(),
            prior_alpha: 2.0,
            prior_beta: 8.0,
            bounds: (0.0, 1.0),
        };
        let prior = def.prior();
        assert!((prior.alpha - 2.0).abs() < 1e-12);
        assert!((prior.beta - 8.0).abs() < 1e-12);
        assert!((prior.mean() - 0.2).abs() < 1e-12);
    }

    #[test]
    fn parameter_space_serializes() {
        let space = ParameterSpace {
            parameters: vec![ParameterDef {
                name: "p".into(),
                prior_alpha: 1.0,
                prior_beta: 1.0,
                bounds: (0.0, 1.0),
            }],
        };
        let json = serde_json::to_string(&space).unwrap();
        let back: ParameterSpace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.parameters.len(), 1);
        assert_eq!(back.parameters[0].name, "p");
    }
}
