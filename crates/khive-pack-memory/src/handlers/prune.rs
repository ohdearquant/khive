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

        // VACUUM must run outside an open transaction. Under
        // `KHIVE_WRITE_QUEUE=1`, a plain `execute_script` call would run
        // inside the WriterTask's per-request `BEGIN IMMEDIATE` — SQLite
        // rejects VACUUM there (ADR-067 Component A, Fork C slice 2 round 2,
        // BLOCKER A). `execute_script_top_level` is still serialized through
        // the single writer owner but skips that transaction wrap, so
        // VACUUM runs genuinely top-level on both the flag-on and flag-off
        // paths.
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await?;
        writer
            .execute_script_top_level("VACUUM;".to_string())
            .await?;

        Ok(json!({ "ok": true }))
    }
}

// ── ADR-067 Fork C slice 2 round 2 (BLOCKER A): memory.vacuum under the write
// queue ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod vacuum_write_queue_tests {
    /// Fork C slice 2 round 2 (BLOCKER A): before this fix, `handle_vacuum`
    /// sent `"VACUUM;"` via plain `execute_script`, which — once
    /// `execute_script`'s flag-on path was migrated to route through the
    /// writer task (Fork C slice 2 round 1) — ran inside that task's
    /// per-request `BEGIN IMMEDIATE`. SQLite rejects `VACUUM` inside any
    /// open transaction ("cannot VACUUM from within a transaction"), so
    /// `memory.vacuum` broke under `KHIVE_WRITE_QUEUE=1`. This proves the
    /// fix: routing the SAME statement through
    /// `SqlWriter::execute_script_top_level` (what `handle_vacuum` now
    /// calls) succeeds with the write queue enabled.
    ///
    /// Deliberately does NOT build a full `KhiveRuntime` (env-var or
    /// otherwise) to reach `handle_vacuum` through the `memory.vacuum` verb
    /// dispatch: `MemoryPack::new` requires an owned `KhiveRuntime`, and
    /// `KhiveRuntime`/`RuntimeConfig` have no config-injection path for
    /// `PoolConfig::write_queue_enabled` other than the process-global
    /// `KHIVE_WRITE_QUEUE` env var — which this crate's other tests are NOT
    /// `#[serial]` against (the exact race documented on
    /// `khive-pack-brain`'s `fold_gate.rs` / `persist.rs` sibling routing
    /// tests for this same round). Exercising the identical
    /// `sql.writer().await?.execute_script_top_level("VACUUM;")` call
    /// `handle_vacuum` makes, over a bare write-queue-enabled
    /// `ConnectionPool`/`SqlBridge` built from a `PoolConfig` literal,
    /// proves the same fix with no env var and no risk to any other test in
    /// this binary.
    #[tokio::test]
    async fn vacuum_top_level_succeeds_with_write_queue_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("memory-vacuum-write-queue.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: true,
            ..khive_db::PoolConfig::default()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let mut writer = pool.writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        assert!(
            pool.writer_task_handle().unwrap().is_some(),
            "writer task must be spawned with the flag on for a file-backed pool"
        );

        let sql: std::sync::Arc<dyn khive_storage::SqlAccess> =
            std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

        let mut writer = sql.writer().await.expect("writer handle");
        let result = writer.execute_script_top_level("VACUUM;".to_string()).await;

        assert!(
            result.is_ok(),
            "VACUUM via execute_script_top_level must succeed under \
             KHIVE_WRITE_QUEUE (no BEGIN IMMEDIATE wrap); got {result:?}"
        );
    }
}
