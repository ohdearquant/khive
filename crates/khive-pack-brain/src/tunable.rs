use khive_runtime::pack::PackRuntime;
use khive_runtime::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{BetaPosterior, BrainState};

/// Packs that want auto-tuning implement this trait.
/// The brain discovers tunable packs at startup via the PackRegistry.
pub trait PackTunable: PackRuntime {
    fn parameter_space(&self) -> ParameterSpace;
    fn project_config(&self, state: &BrainState) -> Value;
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
