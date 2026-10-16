//! Batch work items, response budgets and typed dispatch failure envelopes.

use khive_runtime::{DispatchError, DomainDisposition, RuntimeError};
use khive_types::RefusalReason;
use serde_json::{json, Value};

#[cfg(doc)]
use super::execute_bounded_units;
use super::{
    drop_value_iteratively, result_within_depth_limit, runtime_error_value, MAX_BATCH_CONCURRENCY,
};

pub(super) struct BatchTask<F> {
    pub(super) index: usize,
    pub(super) tool: String,
    pub(super) future: F,
}

/// One bracketed-batch unit's future (ADR-016 Amendment 2): a linear chain
/// dispatched under [`execute_bounded_units`]'s concurrency cap.
pub(super) struct UnitTask<F> {
    pub(super) future: F,
}

/// One unit's flattened leaf entries plus the `parse_content` recomputations
/// its own dispatched leaves produced (mirroring plain chain mode's per-step
/// recompute), reported back to the caller for merging into the shared
/// request-wide `parse_content` vector.
pub(super) struct UnitOutcome {
    pub(super) unit_index: usize,
    pub(super) entries: Vec<Value>,
    pub(super) content_updates: Vec<(usize, bool)>,
}

/// The ADR-016 Amendment 2 aggregate response budget, shared by every unit of
/// one bracketed batch of chains. Every leaf checks [`Self::is_breached`]
/// before it dispatches and calls [`Self::record`] after it produces its
/// final entry, so the total is spent once across the whole request
/// regardless of how many units or leaves are concurrently in flight.
pub(super) struct UnitBudget {
    limit: usize,
    state: std::sync::Mutex<(usize, bool)>,
}

impl UnitBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            state: std::sync::Mutex::new((0, false)),
        }
    }

    /// `true` once the aggregate budget has been exhausted. Checked before
    /// every leaf, including a continuation of an already-active unit, so no
    /// new leaf is admitted anywhere in the request past this point.
    pub(super) fn is_breached(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }

    /// Records `entry`'s serialized size against the shared total. An entry
    /// that itself exhausts the budget is still kept as-is; it already ran
    /// and its disposition is real; only the *next* leaf anywhere in the
    /// request is refused.
    pub(super) fn record(&self, entry: &Value) {
        let bytes = serde_json::to_vec(entry)
            .expect("serde_json::Value is always serializable")
            .len();
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.0 = guard.0.saturating_add(bytes);
        if guard.0 > self.limit {
            guard.1 = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchOrigin {
    Local,
    Daemon,
}

#[derive(Clone, Copy)]
pub(super) struct RunParsedContext<'a> {
    pub(super) enforce_response_budget: bool,
    pub(super) max_batch_concurrency: usize,
    pub(super) from_wire: bool,
    pub(super) identity: Option<&'a khive_runtime::RequestIdentity>,
}

#[derive(Clone, Copy)]
pub(super) struct ParsedDispatchPolicy {
    pub(super) strict_refusals: bool,
    pub(super) max_batch_concurrency: usize,
}

impl ParsedDispatchPolicy {
    pub(super) const fn bounded_parallel(strict_refusals: bool) -> Self {
        Self {
            strict_refusals,
            max_batch_concurrency: MAX_BATCH_CONCURRENCY,
        }
    }

    pub(super) const fn serial(strict_refusals: bool) -> Self {
        Self {
            strict_refusals,
            max_batch_concurrency: 1,
        }
    }
}

/// Typed failure crossing the dispatch/envelope seam.
///
/// `error` retains the pre-existing human or structured payload. `reason` is
/// an additive machine classification and is absent for ordinary validation,
/// storage, transport, coordinator, and authorization-gate failures.
#[derive(Debug)]
pub(super) struct DispatchFailure {
    pub(super) tool: String,
    pub(super) error: Value,
    pub(super) reason: Option<RefusalReason>,
}

impl DispatchFailure {
    pub(super) fn before_dispatch(tool: impl Into<String>, error: Value) -> Self {
        Self::with_disposition(tool, error, DomainDisposition::NotCommitted)
    }

    pub(super) fn committed(tool: impl Into<String>, error: Value) -> Self {
        Self::with_disposition(tool, error, DomainDisposition::Committed)
    }

    pub(super) fn with_disposition(
        tool: impl Into<String>,
        error: Value,
        disposition: DomainDisposition,
    ) -> Self {
        Self {
            tool: tool.into(),
            error: error_with_disposition(error, disposition),
            reason: None,
        }
    }

    pub(super) fn from_dispatch(tool: &str, error: DispatchError) -> Self {
        let (error, disposition) = error.into_parts();
        let reason = match error.refusal_source() {
            RuntimeError::SecretDetected(_) => Some(RefusalReason::GateRefusal),
            RuntimeError::UnknownVerb(_) => Some(RefusalReason::VerbRefused),
            error if error.is_stream_policy_refusal() => Some(RefusalReason::PolicyRefusal),
            _ => None,
        };
        Self {
            tool: tool.into(),
            error: runtime_error_value(error, disposition),
            reason,
        }
    }

    pub(super) fn into_entry(self) -> Value {
        let disposition = error_disposition(&self.error);
        let mut entry = failure_entry(self.tool, self.error, disposition);
        if let Some(reason) = self.reason {
            entry["reason"] = json!(reason.as_str());
        }
        entry
    }
}

/// One constructor for per-op failures. Moving values avoids recursively
/// serializing a canonical result before its depth has been checked.
/// The entry-level disposition comes from the same authoritative argument as
/// the nested error, so callers inspecting `ok` also see the domain outcome.
pub(super) fn failure_entry(
    tool: impl Into<String>,
    error: Value,
    disposition: DomainDisposition,
) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), Value::Bool(false));
    entry.insert("tool".into(), Value::String(tool.into()));
    entry.insert("domain_disposition".into(), json!(disposition.as_str()));
    entry.insert("error".into(), error_with_disposition(error, disposition));
    Value::Object(entry)
}

pub(super) fn aborted_entry(tool: impl Into<String>, message: Option<String>) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), Value::Bool(false));
    entry.insert("tool".into(), Value::String(tool.into()));
    entry.insert("aborted".into(), Value::Bool(true));
    entry.insert(
        "domain_disposition".into(),
        json!(DomainDisposition::NotCommitted.as_str()),
    );
    if let Some(message) = message {
        entry.insert("message".into(), Value::String(message));
    }
    Value::Object(entry)
}

/// Missing/foreign disposition is uncertainty, never permission to replay.
pub(super) fn error_disposition(error: &Value) -> DomainDisposition {
    match error.get("domain_disposition").and_then(Value::as_str) {
        Some("committed") => DomainDisposition::Committed,
        Some("not_committed") => DomainDisposition::NotCommitted,
        _ => DomainDisposition::Unknown,
    }
}

pub(super) fn error_with_disposition(error: Value, disposition: DomainDisposition) -> Value {
    let mut error = match error {
        Value::Object(map) => map,
        Value::String(message) => serde_json::Map::from_iter([
            ("kind".into(), json!("runtime_error")),
            ("message".into(), Value::String(message)),
        ]),
        other => {
            drop_value_iteratively(other);
            serde_json::Map::from_iter([
                ("kind".into(), json!("runtime_error")),
                ("message".into(), json!("operation failed")),
            ])
        }
    };
    if let Some(result) = error.remove("domain_result") {
        if disposition != DomainDisposition::Committed {
            // A nested operation's result is not proof of the outer result.
            drop_value_iteratively(result);
        } else if !result_within_depth_limit(&result) {
            drop_value_iteratively(result);
            error.insert("code".into(), json!("result_too_deep"));
            error.insert(
                "message".into(),
                json!("committed domain result omitted because it exceeds the nesting depth limit"),
            );
        } else {
            error.insert("domain_result".into(), result);
        }
    }
    error.insert("domain_disposition".into(), json!(disposition.as_str()));
    Value::Object(error)
}
