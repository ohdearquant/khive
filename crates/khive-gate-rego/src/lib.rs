//! Rego policy backend for [`khive_gate::Gate`], powered by `regorus`.

mod gate;

pub use gate::RegoGate;

/// Default rule path policies are expected to define.
pub const DEFAULT_ENTRYPOINT: &str = "data.khive.gate.decision";
