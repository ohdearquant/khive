//! Rego-backed [`Gate`] implementation powered by the `regorus` engine.

use std::path::Path;
use std::sync::Mutex;

use khive_gate::{Gate, GateDecision, GateError, GateRequest};

use crate::DEFAULT_ENTRYPOINT;

/// Default policy module name when [`RegoGate::from_policy_str`] is used.
const INLINE_POLICY_NAME: &str = "inline.rego";

/// Rego-backed [`Gate`] impl.
///
/// Evaluations serialize through one engine mutex and fail closed on policy uncertainty. See
/// `crates/khive-gate-rego/docs/api/policy-contract.md`.
pub struct RegoGate {
    pub(crate) engine: Mutex<regorus::Engine>,
    pub(crate) entrypoint: String,
}

impl std::fmt::Debug for RegoGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegoGate")
            .field("entrypoint", &self.entrypoint)
            .finish()
    }
}

impl RegoGate {
    /// Compile one inline policy with the default entrypoint.
    ///
    /// Returns [`GateError::Policy`] on parse or compilation failure. See
    /// `crates/khive-gate-rego/docs/api/policy-loading.md`.
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

    /// Load all direct-child `.rego` files in deterministic path order.
    ///
    /// Returns [`GateError::Policy`] for directory, empty-set, read, or compile failures. See
    /// `crates/khive-gate-rego/docs/api/policy-loading.md`.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, GateError> {
        let dir = dir.as_ref();
        let read = std::fs::read_dir(dir)
            .map_err(|e| GateError::Policy(format!("read_dir {dir}: {e}", dir = dir.display())))?;

        let mut paths: Vec<_> = read
            .map(|entry| {
                entry.map_err(|e| {
                    GateError::Policy(format!("read_dir entry in {dir}: {e}", dir = dir.display()))
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
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

    /// Install a trimmed, already-validated rule path without checking it.
    ///
    /// Operator input should use [`Self::try_with_entrypoint`]. See
    /// `crates/khive-gate-rego/docs/api/policy-loading.md`.
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = entrypoint.into().trim().to_string();
        self
    }

    /// Validate and install an operator-supplied `data.*` rule that exists in the policy.
    ///
    /// Returns [`GateError::Policy`] for malformed or missing rules and [`GateError::Internal`]
    /// for a poisoned validation mutex. See `crates/khive-gate-rego/docs/api/policy-loading.md`.
    pub fn try_with_entrypoint(mut self, entrypoint: impl Into<String>) -> Result<Self, GateError> {
        let ep = entrypoint.into();
        let trimmed = ep.trim();
        if trimmed.is_empty() {
            return Err(GateError::Policy(
                "entrypoint must not be empty or whitespace".to_string(),
            ));
        }
        if !trimmed.starts_with("data.") {
            return Err(GateError::Policy(format!(
                "entrypoint must begin with 'data.' (got: {trimmed:?})"
            )));
        }
        // Reject empty segments created by consecutive, leading, or trailing dots.
        let suffix = &trimmed["data.".len()..];
        if suffix.is_empty() || suffix.split('.').any(|seg| seg.is_empty()) {
            return Err(GateError::Policy(format!(
                "entrypoint has empty path segment (got: {trimmed:?})"
            )));
        }
        // Probe rule existence at boot rather than on the first request.
        {
            let mut engine = self.engine.lock().map_err(|e| {
                GateError::Internal(format!("engine mutex poisoned during validation: {e}"))
            })?;
            engine.set_input(regorus::Value::new_object());
            if let Err(e) = engine.eval_rule(trimmed.to_string()) {
                return Err(GateError::Policy(format!(
                    "entrypoint {trimmed:?} is not a valid rule in the loaded policy: {e}"
                )));
            }
        }
        self.entrypoint = trimmed.to_string();
        Ok(self)
    }
}

impl Gate for RegoGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let input = serde_json::to_value(req)
            .map_err(|e| GateError::Internal(format!("serialize request: {e}")))?;
        let input_value: regorus::Value = input.into();

        let result = {
            // A poisoned evaluator is policy uncertainty, so return an explicit denial.
            let mut engine = match self.engine.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::warn!(
                        entrypoint = %self.entrypoint,
                        "engine mutex poisoned — denying (fail-closed)"
                    );
                    return Ok(GateDecision::deny(format!(
                        "engine mutex poisoned for {}",
                        self.entrypoint
                    )));
                }
            };
            engine.set_input(input_value);
            engine.eval_rule(self.entrypoint.clone())
        };

        // Keep policy evaluation uncertainty distinguishable from a gate infrastructure outage,
        // per the accepted policy contract (docs/api/policy-contract.md): both branches below
        // fail closed via `Ok(GateDecision::Deny)`, never `Err`. The evaluator's error detail
        // is deliberately dropped rather than folded into the deny reason or the tracing call —
        // this crate depends only on `khive-gate` and cannot reach the runtime's log masker, so
        // any raw evaluator text here would cross both the wire and the log unmasked. Operators
        // who need the detail reproduce the failing policy/input locally. The two reasons stay
        // textually distinct so the two failure modes remain distinguishable to a caller.
        let value = match result {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    "rego eval failed — denying (fail-closed)"
                );
                return Ok(GateDecision::deny("policy evaluation failed"));
            }
        };

        // A rule with no matching branch and no default is not authorization.
        if value == regorus::Value::Undefined {
            return Ok(GateDecision::deny(format!(
                "policy rule {} is undefined for this input",
                self.entrypoint
            )));
        }

        // Same rationale as the evaluator-failure branch above: no raw error text in the
        // tracing call or the deny reason.
        let decision_json = match value.to_json_str() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    "decision value failed to serialize — denying (fail-closed)"
                );
                return Ok(GateDecision::deny(
                    "policy produced an unserializable decision",
                ));
            }
        };

        // Never log raw policy output or serde errors: either may contain caller secrets.
        match serde_json::from_str::<GateDecision>(&decision_json) {
            Ok(decision) => Ok(decision),
            Err(_) => {
                let shape = serde_json::from_str::<serde_json::Value>(&decision_json)
                    .map(describe_json_shape)
                    .unwrap_or("unparsable");
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    shape,
                    error = "policy_decision_shape_mismatch",
                    "policy returned non-GateDecision shape, denying (fail-closed)"
                );
                Ok(GateDecision::deny(format!(
                    "policy rule {} returned an unrecognized shape ({shape}); refusing to echo policy output",
                    self.entrypoint
                )))
            }
        }
    }

    fn impl_name(&self) -> &'static str {
        "RegoGate"
    }
}

/// Describe wrong-shaped output without echoing caller-controlled contents.
fn describe_json_shape(value: serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
