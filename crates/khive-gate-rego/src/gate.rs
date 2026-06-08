use std::path::Path;
use std::sync::Mutex;

use khive_gate::{Gate, GateDecision, GateError, GateRequest};

use crate::DEFAULT_ENTRYPOINT;

/// Default policy module name when [`RegoGate::from_policy_str`] is used.
const INLINE_POLICY_NAME: &str = "inline.rego";

/// Rego-backed [`Gate`] impl.
///
/// Construct with [`Self::from_policy_str`] for a single inline policy or
/// [`Self::from_dir`] to load every `.rego` file under a directory. Override
/// the rule path with [`Self::try_with_entrypoint`] (operator configuration) or
/// [`Self::with_entrypoint`] (programmatic, pre-validated use) when your policy
/// doesn't use the default `data.khive.gate.decision` package.
///
/// The engine is held behind a `Mutex` because `regorus::Engine::eval_rule`
/// requires `&mut self`. This serializes evaluations on the dispatch hot path;
/// revisit (compiled policy / engine pool) if hard-enforcement workloads show
/// contention.
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

    /// Override the rule path the gate evaluates (default
    /// `data.khive.gate.decision`).
    ///
    /// This method is infallible and intended for programmatic use with
    /// already-validated entrypoints. For operator-supplied configuration
    /// use [`Self::try_with_entrypoint`] instead, which rejects empty,
    /// whitespace-only, or non-`data.`-prefixed strings before the gate
    /// is installed — preventing a misconfigured entrypoint from causing
    /// fail-open dispatch errors at runtime.
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = entrypoint.into();
        self
    }

    /// Override the rule path with validation, returning `Err` for empty,
    /// whitespace-only, non-`data.`-prefixed, or malformed-segment entrypoints,
    /// and for entrypoints that do not name an existing rule in the loaded policy.
    ///
    /// Prefer this over [`Self::with_entrypoint`] for operator-supplied
    /// configuration. A misconfigured entrypoint discovered at construction
    /// time produces a deterministic `GateError::Policy` rather than a
    /// dispatch-time evaluation failure. `RegoGate::check` converts evaluation
    /// errors to `Ok(GateDecision::Deny)` (fail-closed), so construction-time
    /// validation is defense-in-depth — not the primary safety net.
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
        // Validate path segments after the "data." prefix: no empty components
        // (which arise from consecutive dots, a leading dot after "data.", or a
        // trailing dot).  "data.a..b" and "data.a." are rejected here.
        let suffix = &trimmed["data.".len()..];
        if suffix.is_empty() || suffix.split('.').any(|seg| seg.is_empty()) {
            return Err(GateError::Policy(format!(
                "entrypoint has empty path segment (got: {trimmed:?})"
            )));
        }
        // Confirm the rule exists in the currently-loaded policy.  eval_rule
        // returns Err("not a valid rule path") when the path is not a compiled
        // rule — catching misconfigurations at boot rather than at first request.
        {
            let mut engine = self.engine.lock().map_err(|e| {
                GateError::Internal(format!("engine mutex poisoned during validation: {e}"))
            })?;
            // A dummy empty-object input is sufficient; we only care whether the
            // rule path exists, not the evaluated value.
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
            let mut engine = self
                .engine
                .lock()
                .map_err(|e| GateError::Internal(format!("engine mutex poisoned: {e}")))?;
            engine.set_input(input_value);
            engine.eval_rule(self.entrypoint.clone())
        };

        // Fail closed: any evaluation failure (missing rule, undefined, engine
        // error) becomes an explicit Deny rather than propagating an Err that the
        // runtime's fail-open branch would treat as "allow".
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

        // regorus returns Value::Undefined when the rule exists but no branch
        // matched without a default.  Treat undefined as deny.
        if value == regorus::Value::Undefined {
            return Ok(GateDecision::deny(format!(
                "policy rule {} is undefined for this input",
                self.entrypoint
            )));
        }

        let decision_json = value
            .to_json_str()
            .map_err(|e| GateError::Internal(format!("decision to_json: {e}")))?;

        // A result that parses to a non-GateDecision shape (e.g. a boolean,
        // plain string, or wrong object) is also treated as deny rather than
        // propagating an Err.
        match serde_json::from_str::<GateDecision>(&decision_json) {
            Ok(decision) => Ok(decision),
            Err(e) => {
                tracing::warn!(
                    entrypoint = %self.entrypoint,
                    got = %decision_json,
                    error = %e,
                    "policy returned non-GateDecision shape — denying (fail-closed)"
                );
                Ok(GateDecision::deny(format!(
                    "policy rule {} returned unrecognized shape: {decision_json}",
                    self.entrypoint
                )))
            }
        }
    }

    fn impl_name(&self) -> &'static str {
        "RegoGate"
    }
}
