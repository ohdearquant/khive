//! `db_diagnostics` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

use super::common::{deser, DbDiagnosticsParams};
use crate::KgPack;

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
        serde_json::to_value(&report)
            .map_err(|e| RuntimeError::Internal(format!("db_diagnostics: serialize: {e}")))
    }
}
