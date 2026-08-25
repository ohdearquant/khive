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
    let delta = seq_rows - graph_rows;
    let relationship = match delta.cmp(&0) {
        std::cmp::Ordering::Greater => "ledger_ahead_consistent_with_hard_deletes",
        std::cmp::Ordering::Equal => "equal",
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

impl KgPack {
    /// Writer-contention, graph-edge integrity, and WAL/checkpoint diagnostics
    /// (ADR-091/ADR-135 operator surface): aggregate and class-specific writer
    /// acquisition, pooled-timeout, and audit-failure counters; build identity;
    /// duplicate edge-ID and list-ledger counts; checkpoint counters; a single
    /// PASSIVE probe; WAL file size; and an explicitly qualified WAL-pin census.
    /// Zero-arg, always targets the main backend regardless of the caller's
    /// namespace. Not write-free: the PASSIVE probe may backfill WAL frames
    /// (normal checkpoint I/O) — it never changes logical state, escalates to
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
        let report = self
            .runtime
            .db_diagnostics_with_audit_metrics(registry.audit_batch_metrics())
            .await?;
        let mut value = serde_json::to_value(&report)
            .map_err(|e| RuntimeError::Internal(format!("db_diagnostics: serialize: {e}")))?;
        annotate_graph_edge_integrity(&mut value);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::annotate_graph_edge_integrity;

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
}
