//! `db_diagnostics` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError};

use super::common::{deser, DbDiagnosticsParams};
use crate::KgPack;

impl KgPack {
    /// WAL/checkpoint diagnostics (ADR-091 operator surface): build identity,
    /// checkpoint counters, a PASSIVE checkpoint probe, WAL file size, and
    /// WAL-pin census. Zero-arg, always targets the main backend regardless
    /// of the caller's namespace. Not write-free: the PASSIVE probe may
    /// backfill WAL frames (normal checkpoint I/O) — it never changes logical
    /// state, escalates to TRUNCATE, or deletes sidecar evidence.
    pub(crate) async fn handle_db_diagnostics(
        &self,
        _token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _p: DbDiagnosticsParams = deser(params)?;
        let report = self.runtime.db_diagnostics().await?;
        serde_json::to_value(&report)
            .map_err(|e| RuntimeError::Internal(format!("db_diagnostics: serialize: {e}")))
    }
}
