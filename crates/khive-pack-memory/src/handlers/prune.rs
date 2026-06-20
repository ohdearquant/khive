//! Handlers for `memory.prune` and `memory.vacuum`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{DeleteMode, SqlStatement, SqlValue};

use crate::MemoryPack;

// ── Params ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PruneParams {
    /// Soft-delete memories whose salience is strictly below this value.
    /// `None` means no salience filter.
    pub min_salience: Option<f64>,
    /// Soft-delete memories whose `expires_at` is at or before this timestamp
    /// (Unix microseconds). When omitted, defaults to `now`.
    /// Pass `0` to skip the expiry filter entirely.
    pub before: Option<i64>,
    /// Namespace to prune. Defaults to `"local"`.
    pub namespace: Option<String>,
    /// Dry-run mode: count candidates without deleting. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VacuumParams {
    // No parameters — VACUUM takes no arguments.
}

// ── Implementations ───────────────────────────────────────────────────────────

impl MemoryPack {
    pub(crate) async fn handle_prune(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: PruneParams = serde_json::from_value(params).map_err(|e| {
            RuntimeError::InvalidInput(format!("memory.prune: invalid params: {e}"))
        })?;

        let namespace = p.namespace.as_deref().unwrap_or("local").to_string();

        let now_micros = chrono::Utc::now().timestamp_micros();

        // Collect IDs to prune: memories matching either criterion.
        // We query via SqlAccess to avoid a full note-store scan.
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await?;

        // Build candidate query: kind='memory', not deleted, in namespace.
        // We'll apply Python-side salience and expires_at filters below.
        // For large datasets a dedicated SQL WHERE is better, but the note
        // set is bounded by namespace and kind, so row-level filtering is safe.
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id, salience, expires_at \
                      FROM notes \
                      WHERE kind = 'memory' \
                        AND namespace = ? \
                        AND deleted_at IS NULL"
                    .to_string(),
                params: vec![SqlValue::Text(namespace.clone())],
                label: Some("memory.prune.candidates".to_string()),
            })
            .await?;

        let mut to_delete: Vec<uuid::Uuid> = Vec::new();

        for row in rows {
            let id_str = match row.get("id") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => continue,
            };
            let id: uuid::Uuid = match id_str.parse() {
                Ok(u) => u,
                Err(_) => continue,
            };

            // Check salience threshold.
            if let Some(min_sal) = p.min_salience {
                let sal = match row.get("salience") {
                    Some(SqlValue::Float(f)) => *f,
                    Some(SqlValue::Integer(i)) => *i as f64,
                    _ => 0.0, // treat missing salience as 0
                };
                if sal < min_sal {
                    to_delete.push(id);
                    continue;
                }
            }

            // Check expiry. `before = Some(0)` skips expiry filter.
            let cutoff = match p.before {
                Some(0) => None, // explicit zero = skip expiry filter
                Some(ts) => Some(ts),
                None => Some(now_micros), // default = now
            };
            if let Some(cutoff_ts) = cutoff {
                let exp = match row.get("expires_at") {
                    Some(SqlValue::Integer(i)) => Some(*i),
                    _ => None,
                };
                if let Some(e) = exp {
                    if e <= cutoff_ts {
                        to_delete.push(id);
                    }
                }
            }
        }

        let count = to_delete.len();

        if p.dry_run {
            return Ok(json!({
                "pruned": 0,
                "dry_run": true,
                "would_prune": count,
                "namespace": namespace,
            }));
        }

        // Soft-delete each candidate via NoteStore.
        let note_store = self.runtime.notes(token)?;
        let mut pruned = 0usize;
        for id in to_delete {
            if note_store.delete_note(id, DeleteMode::Soft).await? {
                pruned += 1;
            }
        }

        Ok(json!({
            "pruned": pruned,
            "dry_run": false,
            "namespace": namespace,
        }))
    }

    pub(crate) async fn handle_vacuum(&self, params: Value) -> Result<Value, RuntimeError> {
        // Validate params — must be empty object or omitted.
        let _: VacuumParams = serde_json::from_value(params).map_err(|e| {
            RuntimeError::InvalidInput(format!("memory.vacuum: invalid params: {e}"))
        })?;

        // VACUUM must run outside an open transaction. A plain writer connection
        // via execute_script (which calls execute_batch internally) satisfies
        // SQLite's requirement that VACUUM is issued at the top level.
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await?;
        writer.execute_script("VACUUM;".to_string()).await?;

        Ok(json!({ "ok": true }))
    }
}
