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

        // Gate errors are dispatcher-fail-open; policy evaluation uncertainty must deny.
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    error = %e,
                    "rego eval failed — denying (fail-closed)"
                );
                return Ok(GateDecision::deny(format!(
                    "policy evaluation failed for {}: {e}",
                    self.entrypoint
                )));
            }
        };

        // A rule with no matching branch and no default is not authorization.
        if value == regorus::Value::Undefined {
            return Ok(GateDecision::deny(format!(
                "policy rule {} is undefined for this input",
                self.entrypoint
            )));
        }

        let decision_json = match value.to_json_str() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    error = %e,
                    "decision value failed to serialize — denying (fail-closed)"
                );
                return Ok(GateDecision::deny(format!(
                    "policy rule {} produced unserializable value: {e}",
                    self.entrypoint
                )));
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
mod tests {
    use super::*;
    use khive_gate::ActorRef;
    use khive_types::Namespace;
    use serde_json::json;

    fn request(verb: &str) -> GateRequest {
        GateRequest::new(ActorRef::anonymous(), Namespace::local(), verb, json!({}))
    }

    // ---- GATE-REGO-003: poisoned mutex → Deny, not Err ----

    #[test]
    fn poisoned_engine_mutex_returns_deny_not_err() {
        use std::sync::Arc;

        let policy = r#"
            package khive.gate
            import rego.v1
            default decision := {"decision": "allow", "obligations": []}
        "#;
        let gate = Arc::new(RegoGate::from_policy_str(policy).expect("policy compiles"));

        // Poison the mutex by panicking inside a spawned thread while holding
        // the lock.  After join() the mutex is permanently poisoned.
        {
            let gate_clone = Arc::clone(&gate);
            let handle = std::thread::spawn(move || {
                let _guard = gate_clone.engine.lock().unwrap();
                panic!("intentional panic to poison the mutex");
            });
            // Join to ensure the panic has propagated and the mutex is poisoned.
            let _ = handle.join();
        }

        // check() must return Ok(Deny), never Err.
        let result = gate.check(&request("search"));
        match result {
            Ok(GateDecision::Deny { .. }) => {}
            Ok(GateDecision::Allow { .. }) => panic!("expected Deny for poisoned mutex, got Allow"),
            Err(e) => panic!("expected Ok(Deny) for poisoned mutex, got Err({e})"),
        }
    }

    // ---- GATEREGO-AUD-001: wrong-shaped policy result must never echo caller args ----

    #[test]
    fn malformed_policy_echoing_input_args_does_not_leak_secret() {
        // A misconfigured/malformed policy that echoes `input.args` back as the
        // decision object, instead of a proper `{"decision": "allow"|"deny", ...}`
        // shape. This is exactly the GATEREGO-AUD-001 failure mode: the policy
        // author's mistake must not become a secret-exfiltration channel.
        let policy = r#"
            package khive.gate
            import rego.v1
            default decision := input.args
        "#;
        let gate = RegoGate::from_policy_str(policy).expect("policy compiles");

        // FAKE key: real AKIA/AWS shape, invented suffix (see khive-runtime's
        // secret_gate tests for the convention).
        let fake_key = "AKIAFAKEKEY000000000";
        let req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::local(),
            "propose",
            json!({
                "changeset": {
                    "entity": {
                        "properties": {
                            "api_key": fake_key,
                        }
                    }
                }
            }),
        );

        let decision = gate.check(&req).expect("check must not Err (fail-closed)");
        let reason = match decision {
            GateDecision::Deny { reason } => reason,
            GateDecision::Allow { .. } => {
                panic!("wrong-shaped policy result must deny, not allow")
            }
        };

        assert!(
            !reason.contains(fake_key),
            "Deny reason must never echo the caller-supplied secret; got: {reason}"
        );
        assert!(
            !reason.contains("api_key"),
            "Deny reason must never echo caller-supplied field names either; got: {reason}"
        );
    }

    // ---- GATEREGO-AUD-002: the shape-mismatch warn log must never leak the
    // caller-supplied value either, even though the Deny reason is sanitized ----

    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn malformed_policy_shape_mismatch_log_does_not_leak_secret() {
        // Unlike the `default decision := input.args` shape in the sibling
        // test above (which regorus rejects at eval time as an "invalid ref
        // in default value" before any value is produced), this policy
        // evaluates successfully and returns a well-formed object whose
        // `decision` tag value is the caller-supplied secret itself, which
        // is exactly what drives serde's internally-tagged "unknown variant"
        // error message through the deserialize path this test targets.
        let policy = r#"
            package khive.gate
            import rego.v1
            decision := {"decision": input.args.changeset.entity.properties.api_key}
        "#;
        let gate = RegoGate::from_policy_str(policy).expect("policy compiles");

        let fake_key = "AKIAFAKEKEY000000000";
        let req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::local(),
            "propose",
            json!({
                "changeset": {
                    "entity": {
                        "properties": {
                            "api_key": fake_key,
                        }
                    }
                }
            }),
        );

        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();

        let decision = tracing::subscriber::with_default(subscriber, || {
            gate.check(&req).expect("check must not Err (fail-closed)")
        });
        assert!(
            matches!(decision, GateDecision::Deny { .. }),
            "wrong-shaped policy result must deny, not allow"
        );

        let log_output = String::from_utf8(captured.0.lock().unwrap().clone())
            .expect("log output must be valid UTF-8");

        assert!(
            !log_output.is_empty(),
            "expected the shape-mismatch warn log to be captured"
        );
        assert!(
            log_output.contains("policy_decision_shape_mismatch"),
            "expected the shape-mismatch branch's fixed error category in the log; got: {log_output}"
        );
        assert!(
            !log_output.contains(fake_key),
            "tracing output must never contain the caller-supplied secret; got: {log_output}"
        );
        assert!(
            !log_output.contains("api_key"),
            "tracing output must never contain caller-supplied field names either; got: {log_output}"
        );
    }
}
