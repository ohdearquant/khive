//! `khive-gate-rego` — [Rego](https://www.openpolicyagent.org/docs/latest/policy-language/)
//! backend for [`khive_gate::Gate`], powered by
//! [`regorus`](https://crates.io/crates/regorus).
//!
//! See [`docs/protocol.md`](../docs/protocol.md) for the full policy contract,
//! input/decision shape, entrypoint rules, and fail-open semantics.

mod gate;

pub use gate::RegoGate;

/// Default rule path policies are expected to define.
pub const DEFAULT_ENTRYPOINT: &str = "data.khive.gate.decision";
