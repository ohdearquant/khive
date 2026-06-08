//! Auto-tuning extension trait for packs that expose parameter spaces to brain.

use khive_runtime::pack::PackRuntime;
use khive_runtime::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{BalancedRecallState, BetaPosterior};

/// Extension trait for packs that expose a parameter space to brain auto-tuning.
///
/// The brain discovers tunable packs at startup via the PackRegistry.
/// `project_config` receives a `BalancedRecallState` — the v1 profile
/// state — and returns a pack-specific config value.
pub trait PackTunable: PackRuntime {
    fn parameter_space(&self) -> ParameterSpace;
    fn project_config(&self, state: &BalancedRecallState) -> Value;
    fn apply_config(&self, config: Value) -> Result<(), RuntimeError>;
}

/// A collection of named parameters with Beta priors and bounds, exposed to brain auto-tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSpace {
    pub parameters: Vec<ParameterDef>,
}

/// A single tunable parameter with a Beta prior and a `[min, max]` bounds range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterDef {
    pub name: String,
    pub prior_alpha: f64,
    pub prior_beta: f64,
    pub bounds: (f64, f64),
}

impl ParameterDef {
    /// Return the Beta prior as a `BetaPosterior` with the configured `alpha` and `beta`.
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
        assert!((prior.alpha() - 2.0).abs() < 1e-12);
        assert!((prior.beta() - 8.0).abs() < 1e-12);
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
