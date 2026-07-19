//! Handlers for `memory.prune` and `memory.vacuum`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{DeleteMode, SqlStatement, SqlValue};

use crate::ann;
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

        // Raw NoteStore deletion bypasses runtime mutation hooks, so prune bumps directly.
        // Keep the stale graph intact while a live-row scan builds its replacement.
        if pruned > 0 {
            for model in self.runtime.registered_embedding_model_names() {
                let key = ann::AnnKey::new(model.as_str());
                ann::bump_generation(&self.ann, &key).await;
                ann::ensure_ann_background(&self.runtime, token, &self.ann, &model).await;
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

        // SQLite forbids VACUUM in a transaction; top-level execution still uses one writer.
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await?;
        writer
            .execute_script_top_level("VACUUM;".to_string())
            .await?;

        Ok(json!({ "ok": true }))
    }
}

// ── #533: memory.prune must not surface stale rows via any recall retrieval
// path (FTS lexical, sqlite-vec/ANN vector, or the default hybrid fusion) ────

#[cfg(test)]
mod prune_recall_visibility_tests {
    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use serial_test::serial;

    use crate::test_support::HashVecProvider;

    /// A pruned memory must not be returned by `memory.recall` through the lexical
    /// (`keyword_only`), vector (`vector_only`), or default hybrid fusion path --
    /// and each leg must be shown to actually see the note both before and after
    /// pruning, not just asserted absent.
    ///
    /// `memory.prune` soft-deletes via `NoteStore::delete_note` (sets `deleted_at`,
    /// rows remain -- ADR-014) and bumps the per-model ANN generation so a background
    /// rebuild drops stale vectors. It does not touch the FTS5 index or the
    /// sqlite-vec store directly, so correctness depends on every recall path
    /// filtering out soft-deleted rows after retrieval, not just at the index level.
    ///
    /// The test also forces the exact sqlite-vec fallback path (as opposed to the
    /// warm ANN route) by evicting the warm graph after pruning, so both retrieval
    /// paths are exercised, not just whichever one a fresh corpus happens to take.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn prune_excludes_pruned_memory_across_fts_vector_and_hybrid_recall() {
        const MODEL: &str = "prune-533-visibility-model";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 533 prune stale fts vector ann visibility regression note";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        rt.authorize(ns).expect("authorize local");

        let pack = crate::MemoryPack::new(rt.clone());
        let ann = pack.ann_for_test();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        // memory_type=semantic writes to the token's own namespace ("local"),
        // matching memory.prune's default namespace filter.
        let remember_result = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": NOTE_TEXT,
                    "salience": 0.1,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");
        let note_id = remember_result["id"]
            .as_str()
            .expect("remember response carries id")
            .to_string();

        // Sanity: the note is recallable before prune, via each leg independently —
        // a leg that returns empty both before and after prune would satisfy the
        // post-prune absence assertion vacuously.
        for (label, fusion_strategy) in [
            ("keyword_only (FTS lexical leg)", "keyword_only"),
            ("vector_only (sqlite-vec/ANN leg)", "vector_only"),
        ] {
            let mut params = serde_json::json!({
                "query": NOTE_TEXT,
                "limit": 10,
                "fusion_strategy": fusion_strategy,
            });
            if fusion_strategy == "vector_only" {
                params["embedding_model"] = serde_json::json!(MODEL);
            }
            let result = registry
                .dispatch("memory.recall", params)
                .await
                .unwrap_or_else(|e| {
                    panic!("memory.recall [{label}] before prune must not error: {e:?}")
                });
            let hits = result.as_array().expect("bare array result");
            assert!(
                hits.iter().any(|h| h["id"] == note_id),
                "seeded note must be recallable via {label} before prune: {hits:?}"
            );
        }

        let prune_result = registry
            .dispatch("memory.prune", serde_json::json!({ "min_salience": 0.5 }))
            .await
            .expect("memory.prune");
        assert_eq!(
            prune_result["pruned"], 1,
            "the single seeded note (salience 0.1 < 0.5) must be pruned: {prune_result:?}"
        );

        // The warm graph built during the pre-prune vector_only check above is still
        // installed at this point: `memory.prune` bumps the per-model generation
        // (`ann::bump_generation`) but never evicts the graph itself, and the
        // background rebuild it triggers finds zero live rows post-prune (the
        // corpus scan filters `deleted_at IS NULL`), so it resolves to
        // `AnnEnsureStatus::EmptyCorpus` and leaves the stale graph installed
        // rather than replacing it. A `vector_only` recall right now must
        // therefore take the warm route (`ann::search_loaded` hits the still-
        // installed bridge) and still exclude the pruned note, proving the
        // post-hydration `deleted_at IS NULL` filter in `load_memory_candidate_notes`
        // (`handlers/common.rs`) covers the stale-warm-graph path, not just the
        // exact sqlite-vec fallback exercised below.
        ann.reset_warm_route_count();
        let stale_warm_result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": NOTE_TEXT,
                    "limit": 10,
                    "fusion_strategy": "vector_only",
                    "embedding_model": MODEL,
                }),
            )
            .await
            .expect("memory.recall [vector_only, stale warm graph] must not error");
        let stale_warm_hits = stale_warm_result.as_array().expect("bare array result");
        assert!(
            stale_warm_hits.iter().all(|h| h["id"] != note_id),
            "pruned note must not be returned via vector_only recall against \
             the stale-but-still-installed warm ANN graph, got: {stale_warm_hits:?}"
        );
        assert!(
            ann.warm_route_count() > 0,
            "the stale warm graph must still be installed and hit by \
             ann::search_loaded — a warm_route_count of 0 means this assertion \
             is vacuously exercising the sqlite-vec fallback instead"
        );

        // Evict the warm graph now, so the recalls below rebuild against the
        // now-pruned corpus and fall through to the exact sqlite-vec search
        // instead of the warm ANN route.
        let key = crate::ann::AnnKey::new(MODEL);
        crate::ann::clear_key(&ann, &key).await;
        ann.reset_warm_route_count();

        // `fusion_strategy: None` omits the param entirely rather than passing
        // "weighted" explicitly, so this leg exercises `RecallConfig::default()`
        // (`config.rs` — `FusionStrategy::Weighted { weights: [0.7, 0.3] }`), the
        // strategy an ordinary caller actually hits, not just the named strategy.
        for (label, fusion_strategy) in [
            ("keyword_only (FTS lexical leg)", Some("keyword_only")),
            ("vector_only (sqlite-vec/ANN leg)", Some("vector_only")),
            ("rrf (non-default fusion, explicit)", Some("rrf")),
            ("weighted (default fusion, fusion_strategy omitted)", None),
        ] {
            let mut params = serde_json::json!({
                "query": NOTE_TEXT,
                "limit": 10,
            });
            if let Some(fs) = fusion_strategy {
                params["fusion_strategy"] = serde_json::json!(fs);
            }
            if fusion_strategy == Some("vector_only") {
                params["embedding_model"] = serde_json::json!(MODEL);
            }
            let result = registry
                .dispatch("memory.recall", params)
                .await
                .unwrap_or_else(|e| panic!("memory.recall [{label}] must not error: {e:?}"));
            let hits = result.as_array().expect("bare array result");
            assert!(
                hits.iter().all(|h| h["id"] != note_id),
                "pruned note must not be returned via {label}, got: {hits:?}"
            );
        }

        // Prove the vector legs above actually took the exact sqlite-vec fallback
        // (handlers/common.rs:1100-1129), not the warm ANN route: the corpus scan
        // that would install a warm graph sees zero live rows post-prune, so
        // `ann::search_loaded` must never have found a cached bridge for this model.
        assert_eq!(
            ann.warm_route_count(),
            0,
            "post-prune vector recall must route through the exact sqlite-vec \
             fallback, not the warm ANN bridge"
        );
    }

    /// #533 follow-up: `RecallConfig::default()` fuses via `FusionStrategy::Weighted
    /// { weights: [0.7, 0.3] }` (`config.rs`), not `rrf` — the shipped default is
    /// reached by omitting `fusion_strategy` from the recall params entirely, not by
    /// passing `"rrf"`. This test exercises that exact omitted-param path: seed a
    /// low-salience note, confirm it is recallable pre-prune via the FTS, vector, and
    /// default (fusion_strategy omitted) legs — proving each leg actually sees the
    /// note, not just vacuously agreeing on absence — then prune and confirm all
    /// three legs exclude it post-prune.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn prune_excludes_pruned_memory_via_default_weighted_fusion_recall() {
        const MODEL: &str = "prune-533-visibility-model-weighted-default";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 533 prune stale weighted default fusion regression note";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        rt.authorize(ns).expect("authorize local");

        let pack = crate::MemoryPack::new(rt.clone());

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        let remember_result = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": NOTE_TEXT,
                    "salience": 0.1,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");
        let note_id = remember_result["id"]
            .as_str()
            .expect("remember response carries id")
            .to_string();

        let recall_params = |fusion_strategy: Option<&str>| {
            let mut params = serde_json::json!({ "query": NOTE_TEXT, "limit": 10 });
            if let Some(fs) = fusion_strategy {
                params["fusion_strategy"] = serde_json::json!(fs);
            }
            if fusion_strategy == Some("vector_only") {
                params["embedding_model"] = serde_json::json!(MODEL);
            }
            params
        };

        // Sanity: recallable pre-prune via each leg — a leg empty both before and
        // after prune would satisfy the post-prune absence assertion vacuously.
        for (label, fusion_strategy) in [
            ("keyword_only (FTS lexical leg)", Some("keyword_only")),
            ("vector_only (sqlite-vec/ANN leg)", Some("vector_only")),
            ("weighted (default fusion, fusion_strategy omitted)", None),
        ] {
            let result = registry
                .dispatch("memory.recall", recall_params(fusion_strategy))
                .await
                .unwrap_or_else(|e| {
                    panic!("memory.recall [{label}] before prune must not error: {e:?}")
                });
            let hits = result.as_array().expect("bare array result");
            assert!(
                hits.iter().any(|h| h["id"] == note_id),
                "seeded note must be recallable via {label} before prune: {hits:?}"
            );
        }

        let prune_result = registry
            .dispatch("memory.prune", serde_json::json!({ "min_salience": 0.5 }))
            .await
            .expect("memory.prune");
        assert_eq!(
            prune_result["pruned"], 1,
            "the single seeded note (salience 0.1 < 0.5) must be pruned: {prune_result:?}"
        );

        for (label, fusion_strategy) in [
            ("keyword_only (FTS lexical leg)", Some("keyword_only")),
            ("vector_only (sqlite-vec/ANN leg)", Some("vector_only")),
            ("weighted (default fusion, fusion_strategy omitted)", None),
        ] {
            let result = registry
                .dispatch("memory.recall", recall_params(fusion_strategy))
                .await
                .unwrap_or_else(|e| panic!("memory.recall [{label}] must not error: {e:?}"));
            let hits = result.as_array().expect("bare array result");
            assert!(
                hits.iter().all(|h| h["id"] != note_id),
                "pruned note must not be returned via {label}, got: {hits:?}"
            );
        }
    }
}

// ── ADR-067 Fork C slice 2: memory.vacuum under the write
// queue ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod vacuum_write_queue_tests {
    /// Verifies top-level VACUUM succeeds with the write queue enabled, without env mutation.
    /// See `crates/khive-pack-memory/docs/api/memory-lifecycle.md`.
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
