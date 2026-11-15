//! `db_diagnostics` verb handler.

use std::time::Duration;

use serde_json::Value;

use khive_runtime::{NamespaceToken, OpenedDiagnosticBackend, RuntimeError, VerbRegistry};
use khive_storage::{RequestReadDeadline, RequestReadStopReason};

use super::common::{deser, DbDiagnosticsParams};
use crate::KgPack;

fn annotate_graph_edge_integrity(report: &mut Value) {
    let Some(integrity) = report
        .get_mut("graph_edge_integrity")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(graph_rows) = integrity.get("graph_edges_rows").and_then(Value::as_i64) else {
        return;
    };
    let Some(seq_rows) = integrity
        .get("graph_edges_seq_rows")
        .and_then(Value::as_i64)
    else {
        return;
    };
    let pre_v14_duplicates = integrity
        .get("pre_v14_duplicate_edge_state_detected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let delta = seq_rows - graph_rows;
    let relationship = match delta.cmp(&0) {
        std::cmp::Ordering::Greater => "ledger_ahead_consistent_with_hard_deletes",
        std::cmp::Ordering::Equal => "equal",
        // A pre-V14 duplicate-ID state legitimately holds more edge rows than
        // ledger rows (two namespaces sharing one edge UUID), so it is a
        // known legacy condition, not an unexplained deficit.
        std::cmp::Ordering::Less if pre_v14_duplicates => {
            "ledger_behind_pre_v14_duplicate_edge_state"
        }
        std::cmp::Ordering::Less => "ledger_behind_unexpected",
    };

    integrity.insert(
        "graph_edges_rows_scope".into(),
        serde_json::json!({
            "namespaces": "all",
            "rows": "live_and_soft_deleted",
        }),
    );
    integrity.insert(
        "graph_edges_seq_rows_scope".into(),
        serde_json::json!({
            "namespaces": "all",
            "rows": "inserted_ids_retained_after_hard_delete",
        }),
    );
    integrity.insert(
        "graph_edges_seq_minus_graph_edges".into(),
        Value::from(delta),
    );
    integrity.insert(
        "graph_edges_seq_relationship".into(),
        Value::from(relationship),
    );
}

/// `error` of a backend whose inspection the request deadline interrupted.
const DEADLINE_INTERRUPTED: &str =
    "not inspected: request deadline (arrived during this backend's inspection)";
/// `error` of a backend the report never reached because the deadline had passed.
const DEADLINE_NOT_REACHED: &str =
    "not inspected: request deadline (arrived before this backend was reached)";

/// The longest stretch held back from the request deadline so the handler can
/// assemble and return a partial report. Clients start their own deadline a
/// little before the daemon starts this one, so a report finished exactly at
/// the deadline would usually arrive after the client stopped waiting.
const PARTIAL_REPORT_RESERVE: Duration = Duration::from_secs(2);

/// How much of the `left` time before the request deadline backend inspections
/// may use: all but a tenth of it, keeping at most [`PARTIAL_REPORT_RESERVE`].
fn inspection_window(left: Duration) -> Duration {
    left - (left / 10).min(PARTIAL_REPORT_RESERVE)
}

/// The deadline backend inspections run under, or `None` when the request
/// carries no deadline.
fn inspection_deadline() -> Option<RequestReadDeadline> {
    let deadline = khive_storage::capture_request_read_context().deadline()?;
    let left = deadline
        .blocking_at()
        .saturating_duration_since(std::time::Instant::now());
    Some(RequestReadDeadline::after(inspection_window(left)))
}

struct BackendDiagnosticResult {
    backend_names: Vec<String>,
    path: Option<String>,
    result: Result<Value, String>,
}

impl BackendDiagnosticResult {
    fn not_inspected(backend: &OpenedDiagnosticBackend, marker: &str) -> Self {
        Self {
            backend_names: backend.backend_names.clone(),
            path: backend
                .canonical_path
                .as_ref()
                .map(|path| path.display().to_string()),
            result: Err(marker.to_string()),
        }
    }
}

fn assemble_backend_diagnostics(
    results: Vec<BackendDiagnosticResult>,
    complete: bool,
) -> Result<Value, RuntimeError> {
    let mut primary = serde_json::Map::new();
    let mut databases = Vec::with_capacity(results.len());
    for (index, entry) in results.into_iter().enumerate() {
        let (diagnostics, error) = match entry.result {
            Ok(value) => {
                if index == 0 {
                    primary = value.as_object().cloned().ok_or_else(|| {
                        RuntimeError::Internal("db_diagnostics: report is not an object".into())
                    })?;
                }
                (Some(value), None)
            }
            Err(error) => (None, Some(error)),
        };
        databases.push(serde_json::json!({
            "backend_names": entry.backend_names,
            "path": entry.path,
            "diagnostics": diagnostics,
            "error": error,
        }));
    }
    primary.insert("databases".into(), Value::Array(databases));
    primary.insert("complete".into(), Value::Bool(complete));
    Ok(Value::Object(primary))
}

impl KgPack {
    /// Reader/writer-contention, graph-edge integrity, and WAL/checkpoint
    /// diagnostics (ADR-091/ADR-135/ADR-165 operator surface): reader
    /// admission/route/timeout/hold evidence; aggregate and class-specific
    /// writer acquisition, pooled-timeout, and audit-failure counters; build and
    /// OS process identity; duplicate edge-ID and list-ledger counts; checkpoint counters;
    /// a PASSIVE probe; WAL file size; and a qualified WAL-pin census.
    /// Reports every already-open backend regardless of the caller's namespace,
    /// or only the one serving `backend`. The root fields describe the first
    /// inspected file (main unless `backend` selects another); `databases`
    /// carries one report or error per canonical file. `size_composition=false`
    /// skips the `dbstat` walk. The backends share the request's read deadline:
    /// when it arrives, the backends already inspected are returned with the
    /// rest marked not inspected and `complete: false`, rather than failing the
    /// whole verb. The PASSIVE probe may backfill WAL frames (normal checkpoint
    /// I/O); it never changes logical state, escalates to TRUNCATE, or deletes
    /// sidecar evidence.
    ///
    /// ADR-133: `registry` is the seam that actually owns the audit-batch
    /// control, so its `audit_batch_metrics()` feeds the batch-health
    /// counters instead of leaving them permanently unavailable (a bare
    /// `KhiveRuntime` has no reachable handle to the registry built over it).
    pub(crate) async fn handle_db_diagnostics(
        &self,
        _token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: DbDiagnosticsParams = deser(params)?;
        let collect_size_composition = p.size_composition.unwrap_or(true);
        let opened = self.runtime.diagnostic_backends();
        let selected: Vec<&OpenedDiagnosticBackend> = match p.backend.as_deref() {
            None => opened.iter().collect(),
            Some(name) => {
                let matched: Vec<_> = opened
                    .iter()
                    .filter(|backend| backend.backend_names.iter().any(|n| n == name))
                    .collect();
                if matched.is_empty() {
                    let mut names: Vec<&str> = opened
                        .iter()
                        .flat_map(|backend| backend.backend_names.iter().map(String::as_str))
                        .collect();
                    names.sort_unstable();
                    names.dedup();
                    return Err(RuntimeError::InvalidInput(format!(
                        "db_diagnostics: no opened backend is named {name:?}; opened backends: {}",
                        names.join(", ")
                    )));
                }
                matched
            }
        };

        let deadline = inspection_deadline();
        let mut results = Vec::with_capacity(selected.len());
        let mut complete = true;
        for backend in selected {
            if complete {
                match khive_storage::capture_request_read_context().stop_reason() {
                    Some(RequestReadStopReason::Deadline) => complete = false,
                    Some(RequestReadStopReason::Cancelled) => {
                        khive_storage::ensure_request_read_active("db_diagnostics")?;
                    }
                    None => {}
                }
            }
            if !complete {
                results.push(BackendDiagnosticResult::not_inspected(
                    backend,
                    DEADLINE_NOT_REACHED,
                ));
                continue;
            }
            let inspect = async {
                let outcome = self
                    .runtime
                    .db_diagnostics_for_opened_backend_with_audit_metrics(
                        backend,
                        registry.audit_batch_metrics(),
                        collect_size_composition,
                    )
                    .await;
                (
                    outcome,
                    khive_storage::capture_request_read_context().stop_reason(),
                )
            };
            let (outcome, stopped) = match deadline {
                Some(deadline) => {
                    khive_storage::scope_request_read_deadline_at(deadline, inspect).await
                }
                None => inspect.await,
            };
            if stopped == Some(RequestReadStopReason::Deadline) {
                complete = false;
            }
            let result = match outcome {
                Ok(report) => {
                    let mut value = serde_json::to_value(&report).map_err(|e| {
                        RuntimeError::Internal(format!("db_diagnostics: serialize: {e}"))
                    })?;
                    annotate_graph_edge_integrity(&mut value);
                    Ok(value)
                }
                // A cancelled request is not a failed backend inspection, and
                // nobody is waiting for a partial report.
                Err(error) if stopped == Some(RequestReadStopReason::Cancelled) => {
                    return Err(error)
                }
                Err(_) if stopped == Some(RequestReadStopReason::Deadline) => {
                    Err(DEADLINE_INTERRUPTED.to_string())
                }
                Err(error) => Err(error.to_string()),
            };
            results.push(BackendDiagnosticResult {
                backend_names: backend.backend_names.clone(),
                path: backend
                    .canonical_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                result,
            });
        }
        let mut report = assemble_backend_diagnostics(results, complete)?;
        report["audit_success_rows_mode"] =
            Value::from(registry.audit_success_rows_mode().as_str());
        Ok(report)
    }
}

#[cfg(test)]
#[path = "db_diagnostics_tests.rs"]
mod tests;
