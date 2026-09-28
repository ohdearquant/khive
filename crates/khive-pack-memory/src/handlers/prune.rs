//! Handlers for `memory.prune` and `memory.vacuum`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use crate::ann;
use crate::MemoryPack;

use super::common::{
    DEFAULT_DECAY_EPISODIC, DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC,
    DEFAULT_SALIENCE_SEMANTIC,
};

// ── Params ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PruneParams {
    /// Soft-delete memories whose salience is strictly below this value.
    /// `None` means no salience filter.
    pub min_salience: Option<f64>,
    /// Soft-delete memories whose decay-adjusted ("effective") salience is
    /// strictly below this value. Effective salience is computed with the
    /// same `DecayModel::apply` call and the same active recall config that
    /// `memory.recall` scores with (see `handlers/common.rs::compute_score`),
    /// over the note's stored `salience`/`decay_factor` and its current age.
    /// `None` means no effective-salience filter. A row need only match this
    /// OR `min_salience` OR the expiry filter below to be selected — this
    /// handler already unions its criteria rather than intersecting them.
    pub min_effective_salience: Option<f64>,
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

        // `min_effective_salience` scores every row with the same `DecayModel`
        // and active recall config `memory.recall` serves with (#2937);
        // resolve that config once, up front, instead of re-locking it per row.
        let decay_cfg = p.min_effective_salience.map(|_| self.active_config());

        // Collect IDs to prune: memories matching any of the configured criteria.
        // We query via SqlAccess to avoid a full note-store scan.
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await?;

        // Build candidate query: kind='memory', not deleted, in namespace.
        // We'll apply Rust-side salience, effective-salience, and expires_at
        // filters below. For large datasets a dedicated SQL WHERE is better,
        // but the note set is bounded by namespace and kind, so row-level
        // filtering is safe.
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id, salience, decay_factor, created_at, expires_at, properties \
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

            // Check decay-adjusted ("effective") salience threshold: the same
            // `DecayModel::apply` call `memory.recall` ranks with, over this
            // note's own stored salience/decay_factor and its current age —
            // so a memory decay has already pushed below relevance, but whose
            // raw `salience` column was never touched, is selectable too.
            if let (Some(min_eff), Some(cfg)) = (p.min_effective_salience, decay_cfg.as_ref()) {
                let memory_type = match row.get("properties") {
                    Some(SqlValue::Text(s)) => serde_json::from_str::<Value>(s).ok(),
                    Some(SqlValue::Json(v)) => Some(v.clone()),
                    _ => None,
                }
                .and_then(|v| {
                    v.get("memory_type")
                        .and_then(|mt| mt.as_str().map(str::to_owned))
                })
                .unwrap_or_else(|| "episodic".to_string());
                let is_semantic = memory_type == "semantic";

                let raw_salience = match row.get("salience") {
                    Some(SqlValue::Float(f)) => Some(*f),
                    Some(SqlValue::Integer(i)) => Some(*i as f64),
                    _ => None,
                };
                let salience = raw_salience.unwrap_or(if is_semantic {
                    DEFAULT_SALIENCE_SEMANTIC
                } else {
                    DEFAULT_SALIENCE_EPISODIC
                });

                let raw_decay_factor = match row.get("decay_factor") {
                    Some(SqlValue::Float(f)) => Some(*f),
                    Some(SqlValue::Integer(i)) => Some(*i as f64),
                    _ => None,
                };
                let decay_factor = raw_decay_factor.unwrap_or(if is_semantic {
                    DEFAULT_DECAY_SEMANTIC
                } else {
                    DEFAULT_DECAY_EPISODIC
                });

                let created_at = match row.get("created_at") {
                    Some(SqlValue::Integer(i)) => *i,
                    _ => now_micros, // no recorded age: treat as freshly written
                };
                let age_days = ((now_micros - created_at).max(0) as f64) / (1_000_000.0 * 86_400.0);
                let effective_salience = cfg.decay_model.apply(
                    salience,
                    age_days,
                    decay_factor,
                    cfg.temporal_half_life_days,
                );
                if effective_salience < min_eff {
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

        // Soft-delete each candidate through the runtime's coherent delete path
        // (`KhiveRuntime::delete_note`, ADR-014), not the raw `NoteStore::delete_note`
        // used before #50: `delete_note` marks the row deleted AND cleans that note's
        // FTS5 document and every registered model's vector row within the same call,
        // instead of leaving stale FTS/vector entries behind until a full ANN rebuild
        // (the same coherent path `delete_entity`/`delete_note` already give every
        // other curation verb). It also fires the note-mutation hook, so a pack that
        // has installed one (this pack's own ANN generation bump, wired in production
        // via `call_register_note_mutation_hooks`) observes the corpus change per note.
        let mut pruned = 0usize;
        for id in to_delete {
            if self.runtime.delete_note(token, id, false).await? {
                pruned += 1;
            }
        }

        // Belt-and-suspenders generation bump: the mutation-hook fire above is a
        // no-op wherever no pack has installed a hook (registries built without
        // calling `call_register_note_mutation_hooks`, e.g. some embedded/test
        // setups), so this pack still bumps its own ANN generation directly and
        // schedules a background rebuild here too. Redundant-but-idempotent when
        // the hook already fired (`bump_generation`/`ensure_ann_background` are
        // both dedup-guarded), and the only path to ANN freshness when it didn't.
        // Keeps the stale graph intact while a live-row scan builds its replacement.
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
            .execute_script_top_level(khive_storage::TopLevelMaintenance::Vacuum)
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
    /// rows remain -- ADR-014), cleans the FTS/vector rows, and bumps the per-model
    /// ANN generation. Background maintenance replays the vector-log tail; when a
    /// deletion would leave no live vector, it evicts the incumbent and an empty
    /// corpus has no replacement bridge. Recall must exclude deleted rows on both
    /// the warm and exact retrieval paths.
    ///
    /// A barrier pauses incremental maintenance after its protected-tail read so
    /// the stale warm route is exercised deterministically. After maintenance
    /// evicts the one-vector graph for the empty corpus, the test also exercises
    /// the exact sqlite-vec fallback.
    #[tokio::test]
    #[serial(background_tasks)]
    #[serial_test::serial(config_ledger)]
    async fn prune_excludes_pruned_memory_across_fts_vector_and_hybrid_recall() {
        const MODEL: &str = "prune-visibility-model";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "prune stale fts vector ann visibility regression note";

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

        let key = crate::ann::AnnKey::new(MODEL);
        crate::ann::wait_until_warm_idle(&ann, &key).await;
        assert!(
            ann.warm_route_count() > 0,
            "the pre-prune vector recall must have hit the installed warm graph"
        );

        // Pause incremental maintenance after it has read the protected tail.
        // This keeps the installed graph available while the recall below runs,
        // independently of how quickly the background task is scheduled.
        ann.reset_warm_route_count();
        ann.protected_tail_barrier
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let maintenance_paused = ann.protected_tail_notify.notified();
        let prune_result = registry
            .dispatch("memory.prune", serde_json::json!({ "min_salience": 0.5 }))
            .await;

        if let Err(error) =
            tokio::time::timeout(std::time::Duration::from_secs(10), maintenance_paused).await
        {
            // If the task reaches the barrier after the timeout, let it continue
            // before this test reports the missing pause.
            ann.protected_tail_release.notify_one();
            panic!(
                "background ANN maintenance did not pause after its protected tail read: {error}"
            );
        }

        // The single-vector graph remains installed while incremental maintenance
        // is paused. Its delete cannot be applied because Vamana refuses to
        // tombstone the last live node; maintenance evicts that graph and an empty
        // corpus has no replacement to install. Run this recall inside the pause
        // to verify the stale warm route still excludes the pruned memory, then
        // wait for maintenance to settle before checking the exact fallback.
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
            .await;
        let stale_warm_route_count = ann.warm_route_count();
        ann.protected_tail_release.notify_one();
        crate::ann::wait_until_warm_idle(&ann, &key).await;
        let prune_result = prune_result.expect("memory.prune");
        assert_eq!(
            prune_result["pruned"], 1,
            "the single seeded note (salience 0.1 < 0.5) must be pruned: {prune_result:?}"
        );
        let stale_warm_result = stale_warm_result
            .expect("memory.recall [vector_only, stale warm graph] must not error");
        let stale_warm_hits = stale_warm_result.as_array().expect("bare array result");
        assert!(
            stale_warm_hits.iter().all(|h| h["id"] != note_id),
            "pruned note must not be returned via vector_only recall against \
             the stale-but-still-installed warm ANN graph, got: {stale_warm_hits:?}"
        );
        assert!(
            stale_warm_route_count > 0,
            "the stale warm graph must still be installed and hit by \
             ann::search_loaded — a warm_route_count of 0 means this assertion \
             is vacuously exercising the sqlite-vec fallback instead"
        );

        // Once maintenance is idle, no ANN bridge can represent this empty
        // corpus, so the vector recall below uses the exact sqlite-vec fallback.
        ann.reset_warm_route_count();

        // `fusion_strategy: None` omits the param entirely rather than passing
        // a strategy explicitly, so this leg exercises `RecallConfig::default()`
        // (`config.rs` — `FusionStrategy::Rrf { k: 10 }`), the strategy an
        // ordinary caller actually hits, not just the named strategy.
        for (label, fusion_strategy) in [
            ("keyword_only (FTS lexical leg)", Some("keyword_only")),
            ("vector_only (sqlite-vec/ANN leg)", Some("vector_only")),
            ("weighted (non-default fusion, explicit)", Some("weighted")),
            ("rrf k=10 (default fusion, fusion_strategy omitted)", None),
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
        // (handlers/common.rs): the empty-corpus maintenance completed without
        // installing a replacement bridge.
        assert_eq!(
            ann.warm_route_count(),
            0,
            "post-prune vector recall must route through the exact sqlite-vec \
             fallback, not the warm ANN bridge"
        );
    }

    /// #533 follow-up: `RecallConfig::default()` fuses via `FusionStrategy::Rrf
    /// { k: 10 }` (`config.rs`) — the shipped default is reached by omitting
    /// `fusion_strategy` from the recall params entirely (an explicit `"rrf"`
    /// string selects k=60). This test exercises that exact omitted-param path: seed a
    /// low-salience note, confirm it is recallable pre-prune via the FTS, vector, and
    /// default (fusion_strategy omitted) legs — proving each leg actually sees the
    /// note, not just vacuously agreeing on absence — then prune and confirm all
    /// three legs exclude it post-prune.
    #[tokio::test]
    #[serial(background_tasks)]
    #[serial_test::serial(config_ledger)]
    async fn prune_excludes_pruned_memory_via_default_fusion_recall() {
        const MODEL: &str = "prune-533-visibility-model-default-fusion";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 533 prune stale default fusion regression note";

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
            ("rrf k=10 (default fusion, fusion_strategy omitted)", None),
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
            ("rrf k=10 (default fusion, fusion_strategy omitted)", None),
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

// ── #50: memory.prune must clean the FTS5 document and every registered
// model's vector row for each pruned note, not just mark it deleted ────────

#[cfg(test)]
mod prune_index_cleanup_tests {
    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use uuid::Uuid;

    use crate::test_support::HashVecProvider;

    /// `memory.prune` must remove the pruned note's FTS5 document and vector row,
    /// not merely soft-delete the notes-table row and leave the auxiliary indexes
    /// stale. Follow-up to #429/#533 (recall-correctness, already covered by
    /// `prune_recall_visibility_tests`): this test checks storage-level cleanup
    /// directly against the TextSearch and VectorStore capabilities, independent
    /// of any recall-path filtering.
    #[tokio::test]
    async fn prune_removes_fts_document_and_vector_row_for_pruned_note() {
        const MODEL: &str = "prune-50-index-cleanup-model";
        const DIMS: usize = 16;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        // One low-salience note to be pruned, one high-salience note that must survive.
        let pruned = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "issue 50 index cleanup pruned note",
                    "salience": 0.1,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("remember (to be pruned)");
        let pruned_id: Uuid = pruned["id"]
            .as_str()
            .expect("id present")
            .parse()
            .expect("valid uuid");

        let survivor = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "issue 50 index cleanup surviving note",
                    "salience": 0.9,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("remember (survivor)");
        let survivor_id: Uuid = survivor["id"]
            .as_str()
            .expect("id present")
            .parse()
            .expect("valid uuid");

        // Sanity: both notes are indexed pre-prune -- a store that never indexed
        // either note would satisfy the post-prune absence assertion vacuously.
        let text = rt.text_for_notes(&token).expect("text_for_notes");
        assert!(
            text.get_document("local", pruned_id)
                .await
                .expect("get_document")
                .is_some(),
            "pruned note must have an FTS5 document before prune"
        );
        assert!(
            text.get_document("local", survivor_id)
                .await
                .expect("get_document")
                .is_some(),
            "survivor note must have an FTS5 document before prune"
        );

        let vectors = rt
            .vectors_for_model(&token, MODEL)
            .expect("vectors_for_model");
        assert_eq!(
            vectors.count().await.expect("vector count"),
            2,
            "both notes must have a vector row before prune"
        );

        let prune_result = registry
            .dispatch("memory.prune", serde_json::json!({ "min_salience": 0.5 }))
            .await
            .expect("memory.prune");
        assert_eq!(
            prune_result["pruned"], 1,
            "only the low-salience note must be pruned: {prune_result:?}"
        );

        // The pruned note's FTS5 document and vector row must be gone.
        assert!(
            text.get_document("local", pruned_id)
                .await
                .expect("get_document")
                .is_none(),
            "#50: pruned note must have no FTS5 document after prune"
        );
        assert_eq!(
            vectors.count().await.expect("vector count"),
            1,
            "#50: pruned note's vector row must be removed, leaving only the survivor's"
        );

        // The surviving note's indexes must be untouched.
        assert!(
            text.get_document("local", survivor_id)
                .await
                .expect("get_document")
                .is_some(),
            "survivor note's FTS5 document must remain after prune"
        );
    }
}

// ── #2937: memory.prune must be able to select on the recall-scored
// decay-adjusted ("effective") salience, not just the raw stored column ────

#[cfg(test)]
mod prune_effective_salience_tests {
    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
    use khive_storage::types::{SqlStatement, SqlValue};
    use serde_json::json;

    use crate::test_support::HashVecProvider;

    /// Backdate a note's `created_at` (Unix microseconds) directly — the same
    /// raw-SQL idiom `handlers/recall.rs`'s decay/window tests use, since
    /// there is no verb surface for writing a fabricated age.
    async fn backdate(rt: &KhiveRuntime, id: uuid::Uuid, age_days: f64) {
        let created_at =
            chrono::Utc::now().timestamp_micros() - (age_days * 86_400.0 * 1_000_000.0) as i64;
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        let changed = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET created_at = ? WHERE id = ?".to_string(),
                params: vec![
                    SqlValue::Integer(created_at),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test.prune_effective_salience.set_created_at".to_string()),
            })
            .await
            .expect("set created_at");
        assert_eq!(changed, 1, "created_at UPDATE must hit exactly one row");
    }

    /// `memory_type: "semantic"` writes to the token's own namespace
    /// ("local"), matching `memory.prune`'s default namespace filter — the
    /// same reason `prune_index_cleanup_tests` above picks it.
    fn build_registry() -> (KhiveRuntime, VerbRegistry) {
        const MODEL: &str = "prune-2937-effective-salience-model";
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: 16,
        });
        let ns = Namespace::parse("local").expect("local namespace");
        rt.authorize(ns).expect("authorize local");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");
        (rt, registry)
    }

    /// Discriminating fixture: raw salience 0.6 clears `min_salience: 0.3`
    /// (the OLD predicate alone would never select it), but at 150 days old
    /// with decay_factor 0.02, `DecayModel::Exponential` puts its effective
    /// salience at 0.6 * exp(-0.02 * 150) ≈ 0.0299 — below
    /// `min_effective_salience: 0.05`. The two predicates must disagree on
    /// this one row, or the test proves nothing (#2937).
    #[tokio::test]
    async fn min_effective_salience_prunes_a_row_min_salience_alone_would_spare() {
        let (rt, registry) = build_registry();

        let remembered = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 discriminating fixture: stale but nominally salient",
                    "salience": 0.6,
                    "decay_factor": 0.02,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");
        let id: uuid::Uuid = remembered["id"]
            .as_str()
            .expect("id present")
            .parse()
            .expect("valid uuid");
        backdate(&rt, id, 150.0).await;

        // The raw-only predicate must NOT select it: 0.6 >= 0.3.
        let raw_only = registry
            .dispatch(
                "memory.prune",
                json!({ "min_salience": 0.3, "dry_run": true }),
            )
            .await
            .expect("memory.prune dry_run min_salience");
        assert_eq!(
            raw_only["would_prune"], 0,
            "raw salience 0.6 must clear min_salience 0.3: {raw_only:?}"
        );

        // The effective-salience predicate MUST select it: ~0.0299 < 0.05.
        let effective_only = registry
            .dispatch(
                "memory.prune",
                json!({ "min_effective_salience": 0.05, "dry_run": true }),
            )
            .await
            .expect("memory.prune dry_run min_effective_salience");
        assert_eq!(
            effective_only["would_prune"], 1,
            "decay-adjusted salience ~0.0299 must clear min_effective_salience 0.05: \
             {effective_only:?}"
        );

        // And the real (non-dry-run) call actually prunes it.
        let pruned = registry
            .dispatch("memory.prune", json!({ "min_effective_salience": 0.05 }))
            .await
            .expect("memory.prune min_effective_salience");
        assert_eq!(
            pruned["pruned"], 1,
            "must actually soft-delete the row: {pruned:?}"
        );
    }

    /// Mirror control: a freshly written row with raw salience 0.1 — caught
    /// by `min_salience: 0.3` (0.1 < 0.3), but at effectively zero age its
    /// decayed salience is ~0.1 too, clearing `min_effective_salience: 0.05`
    /// untouched (0.1 >= 0.05). The new predicate must not over-select where
    /// the old one already would have pruned.
    #[tokio::test]
    async fn min_effective_salience_spares_a_fresh_row_min_salience_alone_would_prune() {
        let (_rt, registry) = build_registry();

        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 mirror fixture: low raw salience, freshly written",
                    "salience": 0.1,
                    "decay_factor": 0.02,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");

        let raw_only = registry
            .dispatch(
                "memory.prune",
                json!({ "min_salience": 0.3, "dry_run": true }),
            )
            .await
            .expect("memory.prune dry_run min_salience");
        assert_eq!(
            raw_only["would_prune"], 1,
            "raw salience 0.1 must be caught by min_salience 0.3: {raw_only:?}"
        );

        let effective_only = registry
            .dispatch(
                "memory.prune",
                json!({ "min_effective_salience": 0.05, "dry_run": true }),
            )
            .await
            .expect("memory.prune dry_run min_effective_salience");
        assert_eq!(
            effective_only["would_prune"], 0,
            "near-zero age must keep decayed salience ~0.1, clearing \
             min_effective_salience 0.05: {effective_only:?}"
        );
    }

    /// A row neither predicate selects: raw salience 0.9, freshly written,
    /// well clear of both `min_salience: 0.3` and `min_effective_salience: 0.05`.
    #[tokio::test]
    async fn neither_predicate_selects_a_healthy_row() {
        let (_rt, registry) = build_registry();

        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 healthy fixture: high salience, freshly written",
                    "salience": 0.9,
                    "decay_factor": 0.02,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");

        let combined = registry
            .dispatch(
                "memory.prune",
                json!({
                    "min_salience": 0.3,
                    "min_effective_salience": 0.05,
                    "dry_run": true,
                }),
            )
            .await
            .expect("memory.prune dry_run combined");
        assert_eq!(
            combined["would_prune"], 0,
            "raw 0.9 and effective ~0.9 must clear both thresholds: {combined:?}"
        );
    }

    /// Existing behaviour with no new parameter: `min_salience` alone still
    /// selects strictly on the raw stored column, unaffected by this change.
    #[tokio::test]
    async fn min_salience_alone_is_unchanged() {
        let (_rt, registry) = build_registry();

        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 regression fixture: low raw salience, no new param sent",
                    "salience": 0.1,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 regression fixture: high raw salience, no new param sent",
                    "salience": 0.9,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");

        // No `min_effective_salience` key in the request at all.
        let result = registry
            .dispatch("memory.prune", json!({ "min_salience": 0.3 }))
            .await
            .expect("memory.prune");
        assert_eq!(
            result["pruned"], 1,
            "only the low-salience row must be pruned, exactly as before #2937: {result:?}"
        );
    }

    /// Separates "compares the raw column" from "the parameter does nothing".
    ///
    /// The discriminating fixture above reddens under BOTH of those mutations,
    /// so on its own it cannot say which one broke. This row is the mirror: raw
    /// salience 0.02 at an age of zero, so its effective salience is also about
    /// 0.02. A raw-column comparison selects it (0.02 < 0.05) and an inert
    /// parameter does not, which is exactly the asymmetry the other test lacks.
    #[tokio::test]
    async fn min_effective_salience_selects_a_fresh_row_whose_raw_value_is_already_low() {
        let (_rt, registry) = build_registry();

        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "2937 asymmetry fixture: fresh and already low",
                    "salience": 0.02,
                    "decay_factor": 0.02,
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember");

        let effective_only = registry
            .dispatch(
                "memory.prune",
                json!({ "min_effective_salience": 0.05, "dry_run": true }),
            )
            .await
            .expect("memory.prune dry_run min_effective_salience");
        assert_eq!(
            effective_only["would_prune"], 1,
            "a fresh row at raw 0.02 is below 0.05 by either reading and must be \
             selected; an inert parameter is what this arm exists to catch: \
             {effective_only:?}"
        );
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
            write_queue_enabled: Some(true),
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
        let result = writer
            .execute_script_top_level(khive_storage::TopLevelMaintenance::Vacuum)
            .await;

        assert!(
            result.is_ok(),
            "VACUUM via execute_script_top_level must succeed under \
             KHIVE_WRITE_QUEUE (no BEGIN IMMEDIATE wrap); got {result:?}"
        );
    }
}
