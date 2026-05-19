//! `khive-gate-rego` — [Rego](https://www.openpolicyagent.org/docs/latest/policy-language/)
//! backend for [`khive_gate::Gate`], powered by
//! [`regorus`](https://crates.io/crates/regorus).
//!
//! # Policy contract
//!
//! Policies see `GateRequest` as JSON on `input`:
//!
//! ```text
//! input.actor.kind        # "user" | "agent" | "lambda" | "anonymous" | ...
//! input.actor.id          # caller id
//! input.namespace         # khive namespace as a string
//! input.verb              # verb being dispatched
//! input.args              # raw JSON args for the verb
//! input.context.session_id   # optional
//! input.context.timestamp    # optional RFC3339
//! input.context.source       # optional ("mcp", "cli", ...)
//! ```
//!
//! Policies MUST define a `decision` rule under package `khive.gate` (or
//! a custom entrypoint set via [`RegoGate::with_entrypoint`]). The rule
//! must produce an object matching
//! [`GateDecision`](khive_gate::GateDecision)'s JSON shape:
//!
//! ```rego
//! package khive.gate
//!
//! import rego.v1
//!
//! default decision := {"decision": "deny", "reason": "no rule matched"}
//!
//! decision := {"decision": "allow", "obligations": []} if {
//!     input.actor.kind == "user"
//!     input.namespace  == "ocean"
//! }
//! ```
//!
//! # Quick start
//!
//! ```
//! use std::sync::Arc;
//! use khive_gate::{ActorRef, Gate, GateRef, GateRequest};
//! use khive_gate_rego::RegoGate;
//! use khive_types::Namespace;
//! use serde_json::json;
//!
//! let policy = r#"
//!     package khive.gate
//!     import rego.v1
//!     default decision := {"decision": "deny", "reason": "default"}
//!     decision := {"decision": "allow", "obligations": []} if {
//!         input.verb == "search"
//!     }
//! "#;
//!
//! let gate: GateRef = Arc::new(RegoGate::from_policy_str(policy).unwrap());
//! let req = GateRequest::new(
//!     ActorRef::anonymous(),
//!     Namespace::default_ns(),
//!     "search",
//!     json!({"query": "LoRA"}),
//! );
//! assert!(gate.check(&req).unwrap().is_allow());
//! ```

use std::path::Path;
use std::sync::Mutex;

use khive_gate::{Gate, GateDecision, GateError, GateRequest};

/// Default rule path policies are expected to define.
pub const DEFAULT_ENTRYPOINT: &str = "data.khive.gate.decision";

/// Default policy module name when [`RegoGate::from_policy_str`] is used.
const INLINE_POLICY_NAME: &str = "inline.rego";

/// Rego-backed [`Gate`] impl.
///
/// Construct with [`Self::from_policy_str`] for a single inline policy or
/// [`Self::from_dir`] to load every `.rego` file under a directory. Override
/// the rule path with [`Self::with_entrypoint`] if your policy doesn't use
/// the default `data.khive.gate.decision` package.
///
/// The engine is held behind a `Mutex` because `regorus::Engine::eval_rule`
/// requires `&mut self`. This serializes evaluations on the dispatch hot
/// path — acceptable while the gate is advisory (ADR-029 v0.2); revisit
/// when enforcement lands (v0.3) and contention shows up.
pub struct RegoGate {
    engine: Mutex<regorus::Engine>,
    entrypoint: String,
}

impl std::fmt::Debug for RegoGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegoGate")
            .field("entrypoint", &self.entrypoint)
            .finish()
    }
}

impl RegoGate {
    /// Build a gate from a single inline Rego source string.
    pub fn from_policy_str(source: &str) -> Result<Self, GateError> {
        let mut engine = regorus::Engine::new();
        engine
            .add_policy(INLINE_POLICY_NAME.to_string(), source.to_string())
            .map_err(|e| GateError::Policy(format!("add_policy: {e}")))?;
        Ok(Self {
            engine: Mutex::new(engine),
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
        })
    }

    /// Load every `*.rego` file under `dir` (non-recursive).
    ///
    /// Returns an error if `dir` cannot be read or any file fails to
    /// parse. Sorting by file name produces deterministic load order across
    /// platforms — relevant when policies depend on import order.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, GateError> {
        let dir = dir.as_ref();
        let read = std::fs::read_dir(dir)
            .map_err(|e| GateError::Policy(format!("read_dir {dir}: {e}", dir = dir.display())))?;

        let mut paths: Vec<_> = read
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "rego"))
            .collect();
        paths.sort();

        if paths.is_empty() {
            return Err(GateError::Policy(format!(
                "no .rego files in {dir}",
                dir = dir.display()
            )));
        }

        let mut engine = regorus::Engine::new();
        for path in &paths {
            engine.add_policy_from_file(path).map_err(|e| {
                GateError::Policy(format!("add_policy_from_file {p}: {e}", p = path.display()))
            })?;
        }
        Ok(Self {
            engine: Mutex::new(engine),
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
        })
    }

    /// Override the rule path the gate evaluates (default
    /// `data.khive.gate.decision`).
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = entrypoint.into();
        self
    }
}

impl Gate for RegoGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let input = serde_json::to_value(req)
            .map_err(|e| GateError::Internal(format!("serialize request: {e}")))?;
        let input_value: regorus::Value = input.into();

        let result = {
            let mut engine = self
                .engine
                .lock()
                .map_err(|e| GateError::Internal(format!("engine mutex poisoned: {e}")))?;
            engine.set_input(input_value);
            engine
                .eval_rule(self.entrypoint.clone())
                .map_err(|e| GateError::Evaluation(format!("eval {}: {e}", self.entrypoint)))?
        };

        let decision_json = result
            .to_json_str()
            .map_err(|e| GateError::Evaluation(format!("decision to_json: {e}")))?;

        serde_json::from_str::<GateDecision>(&decision_json).map_err(|e| {
            GateError::Evaluation(format!(
                "policy returned shape that isn't a GateDecision: {e} (got: {decision_json})"
            ))
        })
    }

    fn impl_name(&self) -> &'static str {
        "RegoGate"
    }
}
