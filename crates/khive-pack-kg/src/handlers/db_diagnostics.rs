//! `db_diagnostics` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

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

struct BackendDiagnosticResult {
    backend_names: Vec<String>,
    path: Option<String>,
    result: Result<Value, String>,
}

fn assemble_backend_diagnostics(
    results: Vec<BackendDiagnosticResult>,
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
    Ok(Value::Object(primary))
}

impl KgPack {
    /// Reader/writer-contention, graph-edge integrity, and WAL/checkpoint
    /// diagnostics (ADR-091/ADR-135/ADR-165 operator surface): reader
    /// admission/route/timeout/hold evidence; aggregate and class-specific
    /// writer acquisition, pooled-timeout, and audit-failure counters; build and
    /// OS process identity; duplicate edge-ID and list-ledger counts; checkpoint counters;
    /// a PASSIVE probe; WAL file size; and a qualified WAL-pin census.
    /// Zero-arg, reports every already-open backend regardless of the caller's
    /// namespace. The existing root fields describe main; `databases` carries
    /// one report or error per canonical file. The PASSIVE probe may backfill
    /// WAL frames (normal checkpoint I/O); it never changes logical state, escalates to
    /// TRUNCATE, or deletes sidecar evidence.
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
        let _p: DbDiagnosticsParams = deser(params)?;
        let mut results = Vec::new();
        for backend in self.runtime.diagnostic_backends().iter() {
            khive_storage::ensure_request_read_active("db_diagnostics")?;
            let result = match self
                .runtime
                .db_diagnostics_for_opened_backend_with_audit_metrics(
                    backend,
                    registry.audit_batch_metrics(),
                )
                .await
            {
                Ok(report) => {
                    let mut value = serde_json::to_value(&report).map_err(|e| {
                        RuntimeError::Internal(format!("db_diagnostics: serialize: {e}"))
                    })?;
                    annotate_graph_edge_integrity(&mut value);
                    Ok(value)
                }
                Err(error) => {
                    // A cancelled request is not a failed backend inspection.
                    khive_storage::ensure_request_read_active("db_diagnostics")?;
                    Err(error.to_string())
                }
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
        assemble_backend_diagnostics(results)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        annotate_graph_edge_integrity, assemble_backend_diagnostics, BackendDiagnosticResult,
    };

    #[test]
    fn one_backend_diagnostics_error_keeps_other_entries() {
        let result = assemble_backend_diagnostics(vec![
            BackendDiagnosticResult {
                backend_names: vec!["main".into()],
                path: Some("/main.db".into()),
                result: Ok(json!({"db_path": "/main.db", "reader_contention": {"reader_acquisitions": 1}})),
            },
            BackendDiagnosticResult {
                backend_names: vec!["broken".into()],
                path: Some("/broken.db".into()),
                result: Err("inspection failed".into()),
            },
            BackendDiagnosticResult {
                backend_names: vec!["healthy".into()],
                path: Some("/healthy.db".into()),
                result: Ok(json!({"db_path": "/healthy.db", "reader_contention": {"reader_acquisitions": 3}})),
            },
        ])
        .expect("one failed file must not fail the verb");
        assert_eq!(result["db_path"], "/main.db");
        assert_eq!(result["databases"].as_array().unwrap().len(), 3);
        assert_eq!(result["databases"][1]["error"], "inspection failed");
        assert!(result["databases"][1]["diagnostics"].is_null());
        assert_eq!(
            result["databases"][2]["diagnostics"]["db_path"],
            "/healthy.db"
        );
    }

    #[test]
    fn graph_edge_integrity_explains_retained_delete_history() {
        let mut report = json!({
            "graph_edge_integrity": {
                "graph_edges_rows": 3,
                "graph_edges_seq_rows": 5,
            }
        });

        annotate_graph_edge_integrity(&mut report);

        let integrity = &report["graph_edge_integrity"];
        assert_eq!(
            integrity["graph_edges_rows_scope"],
            json!({"namespaces": "all", "rows": "live_and_soft_deleted"})
        );
        assert_eq!(
            integrity["graph_edges_seq_rows_scope"],
            json!({
                "namespaces": "all",
                "rows": "inserted_ids_retained_after_hard_delete",
            })
        );
        assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], 2);
        assert_eq!(
            integrity["graph_edges_seq_relationship"],
            "ledger_ahead_consistent_with_hard_deletes"
        );
    }

    #[test]
    fn graph_edge_integrity_marks_a_ledger_deficit_unexpected() {
        let mut report = json!({
            "graph_edge_integrity": {
                "graph_edges_rows": 3,
                "graph_edges_seq_rows": 2,
            }
        });

        annotate_graph_edge_integrity(&mut report);

        let integrity = &report["graph_edge_integrity"];
        assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], -1);
        assert_eq!(
            integrity["graph_edges_seq_relationship"],
            "ledger_behind_unexpected"
        );
    }

    #[test]
    fn graph_edge_integrity_classifies_pre_v14_duplicate_state_as_legacy() {
        // Mirrors the khive-db regression fixture: two graph_edges rows share
        // one ledger row and the report flags the pre-V14 duplicate state.
        let mut report = json!({
            "graph_edge_integrity": {
                "graph_edges_rows": 2,
                "graph_edges_seq_rows": 1,
                "pre_v14_duplicate_edge_state_detected": true,
            }
        });

        annotate_graph_edge_integrity(&mut report);

        let integrity = &report["graph_edge_integrity"];
        assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], -1);
        assert_eq!(
            integrity["graph_edges_seq_relationship"],
            "ledger_behind_pre_v14_duplicate_edge_state"
        );
    }
}
