// FILE SIZE JUSTIFICATION: This file tests 8 specific audit fix scenarios (W1, D1, W5, W6,
// W8, W9, W10, S4, F1) that each require independent runtime setup and teardown. Each test
// verifies a distinct invariant (namespace isolation, upsert dedup, delete, edit, challenge,
// adjudicate, fold, search) that spans multiple handler interactions. Grouping these into a
// single file ensures shared helper utilities (runtime setup, pack registration) are not
// duplicated, and each scenario remains traceable to its originating audit item.

//! Integration tests for the 8 audit fixes: W1, D1, W5, W6, W8, W9, W10, S4, F1.
//!
//! All tests use fresh in-memory runtimes — no shared state, no production DB.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

// ── fixture ───────────────────────────────────────────────────────────────────

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
    rt: KhiveRuntime,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    async fn sql_exec(&self, sql: &str, params: Vec<SqlValue>) {
        let access = self.rt.sql();
        let mut w = access.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("sql_exec");
    }

    async fn sql_query_one(
        &self,
        sql: &str,
        params: Vec<SqlValue>,
    ) -> Option<khive_storage::types::SqlRow> {
        let access = self.rt.sql();
        let mut r = access.reader().await.expect("reader");
        r.query_row(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("sql_query_one")
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let rt_clone = rt.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        rt: rt_clone,
    }
}

fn row_text(row: &khive_storage::types::SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn row_i64(row: &khive_storage::types::SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

include!("fixes_tests/status_filter_tests.rs");

include!("fixes_tests/domain_hydration_and_section_dedup_tests.rs");

include!("fixes_tests/dispute_lifecycle_tests.rs");

include!("fixes_tests/uuid_spelling_tests.rs");

include!("fixes_tests/import_and_upsert_tests.rs");

include!("fixes_tests/fold_and_fusion_tests.rs");

include!("fixes_tests/finalization_status_tests.rs");

include!("fixes_tests/source_field_tests.rs");

include!("fixes_tests/fts_escaping_tests.rs");

include!("fixes_tests/embedding_score_and_rerank_tests.rs");

include!("fixes_tests/draft_filter_tests.rs");

// ── embed_batch failure / count-mismatch counted as `failed` ─────────────────
//
// Regression: embed_batch Err and count-mismatch
// were both mapped to `skipped` (exit 0); they must map to `failed` (exit non-0
// without --best-effort).  The knowledge index handler cannot call embed_batch
// without a non-empty default_embedder_name, so these tests use a fake provider
// registered under the default model key.

mod embed_failure_tests {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    const MODEL_KEY: &str = "all-minilm-l6-v2";

    /// Returns exactly one vector regardless of how many texts are passed.
    /// Triggers the count-mismatch branch in the index handler.
    struct OneDimService;

    #[async_trait]
    impl EmbeddingService for OneDimService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(vec![vec![
                1.0_f32;
                EmbeddingModel::AllMiniLmL6V2.dimensions()
            ]])
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "one-dim"
        }
    }

    struct OneDimProvider;

    #[async_trait]
    impl EmbedderProvider for OneDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            EmbeddingModel::AllMiniLmL6V2.dimensions()
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(OneDimService))
        }
    }

    /// Always returns Err(InferenceFailed) to trigger the Err branch.
    struct AlwaysFailService;

    #[async_trait]
    impl EmbeddingService for AlwaysFailService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Err(EmbedError::InferenceFailed("synthetic test failure".into()))
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "always-fail"
        }
    }

    struct AlwaysFailProvider;

    #[async_trait]
    impl EmbedderProvider for AlwaysFailProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            EmbeddingModel::AllMiniLmL6V2.dimensions()
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(AlwaysFailService))
        }
    }

    /// Build a runtime whose default_embedder_name is non-empty (required for
    /// the index handler to attempt embedding) but whose provider is replaced
    /// with the given fake.
    fn rt_with_fake(fake: impl EmbedderProvider + 'static) -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        // Override the lattice provider with our fake under the same key. A fake
        // whose dimensions differ from the built-in's is refused, so fail here
        // rather than let the built-in serve the test.
        rt.try_register_embedder(fake)
            .expect("fake provider must replace the built-in under the same key");
        rt
    }

    /// Seed two atoms and return the fixture.
    async fn fixture_with_two_atoms(rt: KhiveRuntime) -> Fixture {
        let f = pack(rt);
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "embed-fail-a",
                        "name": "Embed Fail A",
                        "content": "first atom content for embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector"
                    },
                    {
                        "slug": "embed-fail-b",
                        "name": "Embed Fail B",
                        "content": "second atom content for embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector"
                    }
                ]
            }),
        )
        .await
        .expect("upsert atoms");
        f
    }

    /// count-mismatch branch: embed_batch returns 1 vector for a 2-atom batch.
    /// Must count both atoms as `failed`, not `skipped`.
    #[tokio::test]
    async fn index_embed_count_mismatch_counts_as_failed() {
        let f = fixture_with_two_atoms(rt_with_fake(OneDimProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert_eq!(
            result["failed"].as_u64().unwrap_or(0),
            2,
            "count-mismatch must report both atoms as failed: {result:?}"
        );
        assert_eq!(
            result["indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no atoms must be indexed on count-mismatch: {result:?}"
        );
        assert_eq!(
            result["skipped"].as_u64().unwrap_or(u64::MAX),
            0,
            "count-mismatch must not appear in skipped: {result:?}"
        );
    }

    /// Err branch: embed_batch returns Err for every batch.
    /// Must count all atoms as `failed`, not `skipped`.
    #[tokio::test]
    async fn index_embed_error_counts_as_failed() {
        let f = fixture_with_two_atoms(rt_with_fake(AlwaysFailProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert_eq!(
            result["failed"].as_u64().unwrap_or(0),
            2,
            "embed Err must report both atoms as failed: {result:?}"
        );
        assert_eq!(
            result["indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no atoms must be indexed on embed error: {result:?}"
        );
        assert_eq!(
            result["skipped"].as_u64().unwrap_or(u64::MAX),
            0,
            "embed Err must not appear in skipped: {result:?}"
        );
    }

    /// Result JSON always carries `ann_failed` key. When the ANN block does not
    /// run (rebuild_ann=false, which is the default), `ann_failed` must be false.
    /// Embed failures that prevent vector writes also must not set ann_failed —
    /// atom-level and ANN-level failures are distinct failure dimensions.
    #[tokio::test]
    async fn index_result_carries_ann_failed_false_when_ann_block_skipped() {
        // Use count-mismatch provider: embed fails, no vectors stored, ANN block
        // never entered (ann_vectors stays empty), so ann_failed must be false.
        let f = fixture_with_two_atoms(rt_with_fake(OneDimProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert!(
            result.get("ann_failed").is_some(),
            "result JSON must carry ann_failed key: {result:?}"
        );
        assert!(
            !result["ann_failed"].as_bool().unwrap_or(true),
            "ann_failed must be false when ANN block did not run: {result:?}"
        );
    }

    /// Issue #1513: `knowledge.index` must embed and write only the default model
    /// because every knowledge retrieval path reads only that model. Fake
    /// providers stand in for both engines so the test does not load lattice
    /// weights and can prove that the secondary provider is never called.
    #[tokio::test]
    async fn index_writes_only_the_searchable_default_model() {
        const SECONDARY_KEY: &str = "paraphrase-multilingual-minilm-l12-v2";

        struct FixedVecService {
            seed: f32,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl EmbeddingService for FixedVecService {
            async fn embed(
                &self,
                texts: &[String],
                _model: EmbeddingModel,
            ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(texts.iter().map(|_| vec![self.seed; 384]).collect())
            }

            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "fixed-vec"
            }
        }

        struct FixedVecProvider {
            key: &'static str,
            seed: f32,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl EmbedderProvider for FixedVecProvider {
            fn name(&self) -> &str {
                self.key
            }

            fn dimensions(&self) -> usize {
                384
            }

            async fn build(
                &self,
            ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError>
            {
                Ok(Arc::new(FixedVecService {
                    seed: self.seed,
                    calls: Arc::clone(&self.calls),
                }))
            }
        }

        let primary_calls = Arc::new(AtomicUsize::new(0));
        let secondary_calls = Arc::new(AtomicUsize::new(0));

        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        rt.register_embedder(FixedVecProvider {
            key: MODEL_KEY,
            seed: 0.5,
            calls: Arc::clone(&primary_calls),
        });
        rt.register_embedder(FixedVecProvider {
            key: SECONDARY_KEY,
            seed: 0.6,
            calls: Arc::clone(&secondary_calls),
        });

        let f = fixture_with_two_atoms(rt).await;
        let primary_before = primary_calls.load(Ordering::Relaxed);
        let secondary_before = secondary_calls.load(Ordering::Relaxed);
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");
        assert_eq!(
            result["indexed"].as_u64().unwrap_or(0),
            2,
            "both atoms must index via the default engine: {result:?}"
        );
        assert_eq!(
            primary_calls.load(Ordering::Relaxed),
            primary_before + 1,
            "one default-model batch must be embedded"
        );
        assert_eq!(
            secondary_calls.load(Ordering::Relaxed),
            secondary_before,
            "the unread secondary model must not incur an embed call"
        );

        let row_primary = f
            .sql_query_one(
                "SELECT subject_id FROM vec_all_minilm_l6_v2 LIMIT 1",
                vec![],
            )
            .await;
        assert!(
            row_primary.is_some(),
            "primary engine table must have a row"
        );

        let secondary_table = f
            .sql_query_one(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
                vec![SqlValue::Text(
                    "vec_paraphrase_multilingual_minilm_l12_v2".to_string(),
                )],
            )
            .await;
        assert!(
            secondary_table.is_none(),
            "the unread secondary engine table must not even be initialized"
        );
    }

    // ── Section embed failure regression ────────────────
    //
    // Mirrors the atom failure tests above but exercises the SECTION path via
    // `reindex_knowledge(sections:true, atoms:false)`. Blank-text sections are
    // genuine `skipped`; embed_batch Err and count-mismatch are `failed`.

    /// Seed two sections via `knowledge.edit` and return the fixture. The atom
    /// must exist before sections can be attached.
    async fn fixture_with_two_sections(rt: KhiveRuntime) -> Fixture {
        let f = pack(rt);
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "sec-embed-fail",
                    "name": "Section Embed Fail",
                    "content": "atom content for section embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        // Two distinct sections so the batch has size 2.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "sec-embed-fail",
                "sections": [
                    {
                        "section_type": "overview",
                        "content": "Overview content for section embed failure regression test. This text is long enough to satisfy the 80-character minimum section length requirement — dense sparse retrieval corpus benchmark search latency."
                    },
                    {
                        "section_type": "formalism",
                        "content": "Formalism content for section embed failure regression test. This text is long enough to satisfy the 80-character minimum section length requirement — gradient descent transformer attention vector index."
                    }
                ]
            }),
        )
        .await
        .expect("edit sections");
        f
    }

    /// count-mismatch branch on the SECTION path: embed_batch returns 1 vector
    /// for a 2-section batch. Must count both sections as `sections_failed`, not
    /// `skipped`; `reindex_knowledge` must surface this in its result JSON.
    #[tokio::test]
    async fn section_embed_count_mismatch_counts_as_sections_failed() {
        let f = fixture_with_two_sections(rt_with_fake(OneDimProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: true,
                rebuild_ann: false,
                batch_size: None,
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge ok");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "count-mismatch must report both sections as sections_failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections must be indexed on count-mismatch: {result:?}"
        );
    }

    /// Err branch on the SECTION path: embed_batch returns Err for every batch.
    /// Must count all sections as `sections_failed`, not `skipped`.
    #[tokio::test]
    async fn section_embed_error_counts_as_sections_failed() {
        let f = fixture_with_two_sections(rt_with_fake(AlwaysFailProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: true,
                rebuild_ann: false,
                batch_size: None,
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge ok");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "embed Err must report both sections as sections_failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections must be indexed on embed error: {result:?}"
        );
    }

    /// Regression: `--keep-existing` (drop_existing=false) paginates the
    /// `embedding IS NULL` set. A failed section stays NULL, so without advancing
    /// the offset past stuck rows the loop re-selects the same page forever and
    /// never returns a `sections_failed` report (fail-closed bypassed). With
    /// batch_size=1 over two persistently-failing sections, the old code looped;
    /// this test must TERMINATE and report both as sections_failed.
    #[tokio::test]
    async fn section_keep_existing_failures_terminate_and_report() {
        let f = fixture_with_two_sections(rt_with_fake(AlwaysFailProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: false,
                rebuild_ann: false,
                batch_size: Some(1),
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge must terminate, not loop");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "keep-existing must attempt each section once and report both failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections indexed when every embed fails: {result:?}"
        );
    }
}

// ── ANN bypass regression (issue #78, PR #90) ────────────────────────────────
//
// Regression test: after `knowledge.index` with `rebuild_ann=true` the in-process
// ANN index holds vectors for ALL atoms (regardless of status). A subsequent default
// `knowledge.search` must not return draft atoms even when the ANN path finds them.
//
// Architecture: the fix adds `filter_by_excluded_statuses` immediately after
// `hydrate_empty_hits` in the search handler so the ANN-sourced hits go through
// the same status gate as the SQL/FTS candidates.

mod ann_bypass_regression {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    // Must match AllMiniLmL6V2.native_dimensions() so vector inserts succeed.
    const DIM: usize = 384;

    /// Returns N distinct unit vectors (one per text) so the index handler counts
    /// each atom as successfully indexed. Vectors are differentiated by index so
    /// ANN search can distinguish between atoms.
    struct CorrectDimService;

    #[async_trait]
    impl EmbeddingService for CorrectDimService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    // Slightly different vectors per index position so ANN is non-trivial.
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "correct-dim"
        }
    }

    struct CorrectDimProvider;

    #[async_trait]
    impl EmbedderProvider for CorrectDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(CorrectDimService))
        }
    }

    fn rt_with_correct_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        rt.register_embedder(CorrectDimProvider);
        rt
    }

    /// Default search must exclude draft atoms even when the warm ANN index
    /// contains vectors for them.
    ///
    /// Steps:
    /// 1. Seed a reviewed and a draft atom.
    /// 2. Index with rebuild_ann=true so the ANN holds both IDs.
    /// 3. Run default knowledge.search — assert draft is absent.
    /// 4. Run knowledge.search with include_drafts=true — assert draft appears.
    #[tokio::test]
    async fn ann_warm_draft_atom_excluded_by_default_search() {
        let f = pack(rt_with_correct_embedder());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "ann-rev-atom",
                        "name": "ANN Reviewed Atom",
                        "content": "neural network attention mechanism transformer dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique ann78rev"
                    },
                    {
                        "slug": "ann-dft-atom",
                        "name": "ANN Draft Atom",
                        "content": "neural network attention mechanism transformer dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique ann78dft"
                    },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        f.sql_exec(
            "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
            vec![SqlValue::Text("ann-rev-atom".into())],
        )
        .await;
        f.sql_exec(
            "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
            vec![SqlValue::Text("ann-dft-atom".into())],
        )
        .await;

        // Index with rebuild_ann=true so the in-process ANN index is warmed for
        // both atoms — including the draft atom. This is the precondition for the
        // bypass bug to fire.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 2,
            "both atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // Default search must NOT return the draft atom even though it is in the ANN.
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique ann78",
                    "rerank": false
                }),
            )
            .await
            .expect("default search ok");
        let results = resp["results"].as_array().expect("results");
        let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            !names.contains(&"ANN Draft Atom"),
            "draft atom must be excluded by default even when warm ANN finds it: {names:?}"
        );

        // include_drafts=true must surface the draft atom.
        let resp_incl = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique ann78",
                    "rerank": false,
                    "include_drafts": true
                }),
            )
            .await
            .expect("include_drafts search ok");
        let results_incl = resp_incl["results"].as_array().expect("results");
        let names_incl: Vec<&str> = results_incl
            .iter()
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert!(
            names_incl.contains(&"ANN Draft Atom"),
            "draft atom must appear when include_drafts=true: {names_incl:?}"
        );
    }
}

include!("fixes_tests/exclude_status_precedence_tests.rs");

include!("fixes_tests/compose_and_event_stats_tests.rs");

// ── knowledge.edit inline re-embed (issue #11) ────────────────────────────────
//
// knowledge.edit must embed new/changed sections immediately after writing them
// so that semantic recall is fresh without a manual `kkernel reindex`.
// Byte-identical sections (metadata-only refresh) keep their existing embedding
// and must NOT be needlessly re-embedded.

mod edit_inline_reembed {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    struct EmbedService;

    #[async_trait]
    impl EmbeddingService for EmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            // Distinct unit vectors per position so each section gets a real vector.
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "edit-reembed-service"
        }
    }

    struct EmbedProvider;

    #[async_trait]
    impl EmbedderProvider for EmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(EmbedService))
        }
    }

    fn rt_with_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        rt.register_embedder(EmbedProvider);
        rt
    }

    /// knowledge.edit must embed newly-written sections inline: after edit,
    /// a NEW section (distinct content hash) must have a non-NULL embedding,
    /// while a byte-identical section (metadata-only refresh) must retain its
    /// previously-set embedding and not be re-embedded into a different blob.
    #[tokio::test]
    async fn edit_embeds_new_sections_and_preserves_unchanged_embedding() {
        let rt = rt_with_embedder();
        let f = pack(rt.clone());

        // Create an atom.
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "reembed-test-atom",
                    "name": "Re-embed Test Atom",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        let overview_content =
            "Overview section content that is well above the eighty-character minimum. \
            dense sparse retrieval corpus benchmark search latency gradient descent transformer.";

        // First edit: insert an overview section. It has no prior embedding (new row).
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": overview_content
                }]
            }),
        )
        .await
        .expect("first edit");

        // Assert the newly-inserted section has a non-NULL embedding.
        let row_after_first = f
            .sql_query_one(
                "SELECT id, embedding FROM knowledge_sections \
                 WHERE atom_id = (SELECT id FROM knowledge_atoms WHERE slug = ?1) \
                 AND section_type = 'overview' \
                 LIMIT 1",
                vec![SqlValue::Text("reembed-test-atom".into())],
            )
            .await
            .expect("section row must exist after first edit");

        let section_id = match row_after_first.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => panic!("section id must be text"),
        };
        let first_embedding_blob = match row_after_first.get("embedding") {
            Some(SqlValue::Blob(b)) => b.clone(),
            Some(SqlValue::Null) | None => {
                panic!("newly-inserted section must have a non-NULL embedding after knowledge.edit")
            }
            other => panic!("unexpected embedding value: {other:?}"),
        };
        assert!(
            !first_embedding_blob.is_empty(),
            "embedding blob must be non-empty"
        );

        // Second edit: re-submit the IDENTICAL content. The section is a metadata-only
        // refresh (same content_hash); the embedding must be preserved unchanged.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": overview_content
                }]
            }),
        )
        .await
        .expect("second edit (identical content)");

        let row_after_second = f
            .sql_query_one(
                "SELECT embedding FROM knowledge_sections WHERE id = ?1 LIMIT 1",
                vec![SqlValue::Text(section_id.clone())],
            )
            .await
            .expect("section row must still exist after second edit");

        let second_embedding_blob = match row_after_second.get("embedding") {
            Some(SqlValue::Blob(b)) => b.clone(),
            Some(SqlValue::Null) | None => {
                panic!("byte-identical section must retain its existing embedding after re-edit")
            }
            other => panic!("unexpected embedding value after second edit: {other:?}"),
        };

        assert_eq!(
            first_embedding_blob, second_embedding_blob,
            "byte-identical section must NOT be re-embedded: blob must be unchanged"
        );

        // Third edit: different content → new sibling row → must also be embedded.
        let new_content = "Updated overview section with completely different content that also \
            exceeds the eighty character minimum. gradient descent transformer attention vector.";

        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": new_content
                }]
            }),
        )
        .await
        .expect("third edit (new content)");

        // The new sibling row (different content_hash) must have a non-NULL embedding.
        let new_section_row = f
            .sql_query_one(
                "SELECT id, embedding FROM knowledge_sections \
                 WHERE atom_id = (SELECT id FROM knowledge_atoms WHERE slug = ?1) \
                 AND section_type = 'overview' \
                 AND id != ?2 \
                 LIMIT 1",
                vec![
                    SqlValue::Text("reembed-test-atom".into()),
                    SqlValue::Text(section_id.clone()),
                ],
            )
            .await
            .expect("new sibling section row must exist after third edit");

        match new_section_row.get("embedding") {
            Some(SqlValue::Blob(b)) if !b.is_empty() => {}
            Some(SqlValue::Null) | None => {
                panic!("new sibling section inserted by edit must have a non-NULL embedding")
            }
            other => panic!("unexpected embedding value for new sibling: {other:?}"),
        }
    }

    /// The edit response must be derived from the section and atom embedding
    /// calls that actually ran, including the model that bounded the input.
    #[tokio::test]
    async fn edit_reports_actual_section_embedding_truncation_by_model() {
        let rt = rt_with_embedder();
        let f = pack(rt.clone());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "edit-truncation-report",
                    "name": "Edit Truncation Report",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        let response = f
            .dispatch(
                "knowledge.edit",
                json!({
                    "id": "edit-truncation-report",
                    "sections": [{
                        "section_type": "overview",
                        "content": format!("{}tail", "x".repeat(MAX_TEXT_BYTES + 1))
                    }]
                }),
            )
            .await
            .expect("edit with bounded embedding input");

        assert_eq!(
            response["truncation_by_model"][MODEL_KEY]["truncated"], 1,
            "the newly inserted section is the one bounded input: {response}"
        );
        assert!(
            response["truncation_by_model"][MODEL_KEY]["discarded_bytes"]
                .as_u64()
                .is_some_and(|bytes| bytes > 0),
            "the report must include bytes actually discarded by the embedder: {response}"
        );
        assert_eq!(
            response["warnings"],
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
            "the wire advisory must be derived from the merged actual outcomes"
        );
    }

    /// knowledge.edit must refresh the atom-level vector-store entry (knowledge.atom field)
    /// so atom-granularity semantic recall is fresh without a manual kkernel reindex.
    #[tokio::test]
    async fn edit_refreshes_atom_vector_store_entry() {
        let rt = rt_with_embedder();
        let f = pack(rt.clone());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "atom-vec-test",
                    "name": "Atom Vector Test",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        // Fetch the atom UUID.
        let atom_row = f
            .sql_query_one(
                "SELECT id FROM knowledge_atoms WHERE slug = ?1 LIMIT 1",
                vec![SqlValue::Text("atom-vec-test".into())],
            )
            .await
            .expect("atom must exist");
        let atom_uuid = match atom_row.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => panic!("atom id must be text"),
        };

        // Edit the atom: trigger inline re-embed for both sections and atom vector.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "atom-vec-test",
                "sections": [{
                    "section_type": "overview",
                    "content": "Overview for atom vector refresh test. Must be well above the eighty-character minimum length for knowledge sections. dense sparse retrieval transformer attention."
                }]
            }),
        )
        .await
        .expect("edit");

        // The atom-level vector-store row must exist after edit.
        // Table: vec_all_minilm_l6_v2 (sanitize_model_key("all-minilm-l6-v2")).
        let vec_row = f
            .sql_query_one(
                "SELECT subject_id FROM vec_all_minilm_l6_v2 \
                 WHERE subject_id = ?1 AND field = 'knowledge.atom' LIMIT 1",
                vec![SqlValue::Text(atom_uuid.clone())],
            )
            .await;

        assert!(
            vec_row.is_some(),
            "atom vector-store row must exist in vec_all_minilm_l6_v2 after knowledge.edit \
             (atom_id = {atom_uuid})"
        );
    }

    /// When no default embedder is configured, knowledge.edit must still succeed
    /// (degrade gracefully — same contract as reindex_knowledge with no embedder).
    #[tokio::test]
    async fn edit_succeeds_without_embedder() {
        // Plain memory runtime has no embedder configured (default_embedder_name is "").
        let f = pack(rt());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "no-embed-atom",
                    "name": "No Embed Atom",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        let result = f
            .dispatch(
                "knowledge.edit",
                json!({
                    "id": "no-embed-atom",
                    "sections": [{
                        "section_type": "overview",
                        "content": "Overview section content that is well above the eighty character minimum length requirement for knowledge sections. dense sparse retrieval corpus benchmark search."
                    }]
                }),
            )
            .await
            .expect("edit must succeed even without an embedder configured");

        assert_eq!(
            result["upserted"].as_u64().unwrap_or(0),
            1,
            "edit must report one upserted section: {result:?}"
        );
        assert_eq!(
            result["atom_vector_refreshed"].as_bool(),
            Some(false),
            "no embedder configured: atom_vector_refreshed must be false, not true: {result:?}"
        );
    }
}

// ── ANN type-filter regression (kind= and suggest domain crowding) ────────────
//
// Bug: the type_filter (kind=) was applied only to FTS/SQL candidates, not to
// ANN-fused hits. After fusion + hydration the kind gate was never applied, so
// search(kind="domain") returned atom hits sourced from ANN, and suggest returned
// empty results when atoms were the nearest ANN neighbors to the query.
//
// Fix: filter_hits_by_type is called after hydrate_empty_hits +
// filter_by_excluded_statuses in both handle_search and suggest.
// suggest also increases ann_k from (limit*3).max(20) to (limit*50).max(200)
// so domains are not crowded out of the over-fetch before the type gate fires.

mod ann_type_filter_regression {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    // Must match AllMiniLmL6V2.native_dimensions() so vector inserts succeed.
    const DIM: usize = 384;

    /// Returns one unit vector per text so every atom/domain gets indexed.
    /// Slightly varied by position so ANN results are non-trivial.
    struct CorrectDimService;

    #[async_trait]
    impl EmbeddingService for CorrectDimService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "correct-dim"
        }
    }

    struct CorrectDimProvider;

    #[async_trait]
    impl EmbedderProvider for CorrectDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(CorrectDimService))
        }
    }

    fn rt_with_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        rt.register_embedder(CorrectDimProvider);
        rt
    }

    /// search(kind="domain") must return ONLY domain hits even when the warm ANN
    /// index contains both atom and domain vectors.
    ///
    /// Steps:
    /// 1. Seed several atoms and one domain, index with rebuild_ann=true.
    /// 2. search(kind="domain") — assert all returned hits have kind=="domain".
    /// 3. search(kind="atom") — assert no domain hits appear (regression guard).
    #[tokio::test]
    async fn ann_search_kind_domain_returns_only_domain_hits() {
        let f = pack(rt_with_embedder());

        // Seed atoms first (they will be the majority in the ANN index).
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "type-filter-a1", "name": "Type Filter Atom 1", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                    { "slug": "type-filter-a2", "name": "Type Filter Atom 2", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                    { "slug": "type-filter-a3", "name": "Type Filter Atom 3", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain — upsert_domains stores a row with type:domain tag.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "type-filter-domain",
                    "name": "Type Filter Domain",
                    "description": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Index with rebuild_ann=true: ANN holds both atoms and the domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 3,
            "atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // search(type="domain") must return ONLY domain hits.
        // Note: the SearchParams field is `#[serde(rename = "type")]` — the JSON key
        // is "type", not "kind". See knowledge/schema.rs SearchParams.kind.
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique typef1",
                    "type": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=domain ok");

        let results = resp["results"].as_array().expect("results");
        assert!(
            !results.is_empty(),
            "search kind=domain must find the domain: {resp:?}"
        );
        for r in results {
            assert_eq!(
                r["kind"].as_str().unwrap_or(""),
                "domain",
                "search kind=domain: all results must have kind=domain, got: {r:?}"
            );
        }

        // search(type="atom") must not contain the domain (regression guard).
        let resp_atom = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique typef1",
                    "type": "atom",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=atom ok");

        let results_atom = resp_atom["results"].as_array().expect("results");
        for r in results_atom {
            assert_ne!(
                r["kind"].as_str().unwrap_or(""),
                "domain",
                "search type=atom: no domain hits must appear, got: {r:?}"
            );
        }
    }

    /// suggest must return relevant domains even when atom hits dominate the
    /// top ANN neighbors.
    ///
    /// Steps:
    /// 1. Seed many atoms (to dominate ANN top-k) and one domain.
    /// 2. Index with rebuild_ann=true.
    /// 3. suggest — assert the domain appears (was blocked by crowding before fix).
    #[tokio::test]
    async fn ann_suggest_returns_domain_when_atoms_dominate_ann() {
        let f = pack(rt_with_embedder());

        // Seed 25 atoms to saturate the ANN top-k (ann_k=(8*3).max(20)=24 without the
        // fix). The fake embedder produces identical unit vectors for every text, so the
        // ANN returns items in insertion order. With 25 atoms inserted before the domain
        // the old ann_k=24 fills up with atoms and the domain is excluded entirely.
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "sug-a1",  "name": "Suggest Atom 1",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a2",  "name": "Suggest Atom 2",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a3",  "name": "Suggest Atom 3",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a4",  "name": "Suggest Atom 4",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a5",  "name": "Suggest Atom 5",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a6",  "name": "Suggest Atom 6",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a7",  "name": "Suggest Atom 7",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a8",  "name": "Suggest Atom 8",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a9",  "name": "Suggest Atom 9",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a10", "name": "Suggest Atom 10", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a11", "name": "Suggest Atom 11", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a12", "name": "Suggest Atom 12", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a13", "name": "Suggest Atom 13", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a14", "name": "Suggest Atom 14", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a15", "name": "Suggest Atom 15", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a16", "name": "Suggest Atom 16", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a17", "name": "Suggest Atom 17", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a18", "name": "Suggest Atom 18", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a19", "name": "Suggest Atom 19", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a20", "name": "Suggest Atom 20", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a21", "name": "Suggest Atom 21", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a22", "name": "Suggest Atom 22", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a23", "name": "Suggest Atom 23", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a24", "name": "Suggest Atom 24", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a25", "name": "Suggest Atom 25", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain that matches the same query terms.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "sug-domain",
                    "name": "Suggest Domain",
                    "description": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Mark atoms as reviewed so they are not excluded by the default status filter.
        f.sql_exec(
            "UPDATE knowledge_atoms SET status='reviewed' WHERE slug LIKE 'sug-a%'",
            vec![],
        )
        .await;

        // Index with rebuild_ann=true: ANN holds all 25 atoms and the 1 domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 25,
            "all atoms must be indexed: {idx:?}"
        );

        // suggest must return the domain even though 10 atoms dominate the ANN top-k.
        let resp = f
            .dispatch(
                "knowledge.suggest",
                json!({
                    "query": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 techniques"
                }),
            )
            .await
            .expect("suggest ok");

        let results = resp["results"].as_array().expect("results");
        assert!(
            !results.is_empty(),
            "suggest must return at least one domain even when atoms dominate ANN top-k: {resp:?}"
        );
        let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            names.contains(&"Suggest Domain"),
            "suggest must find the matching domain: {names:?}"
        );
    }

    /// Regression: `knowledge.search` must accept `kind=` as an alias for `type=`.
    ///
    /// RED logic (before the `alias = "kind"` annotation): `"kind"` is not a
    /// recognised serde field on `SearchParams`, so it deserialises to `None`.
    /// With `SearchParams.kind == None` no type filter is applied, atom hits
    /// appear in the results, and the `assert_eq!(kind, "domain")` loop below
    /// fails because atom results have `kind == "atom"`, not `"domain"`.
    ///
    /// GREEN (with alias): `kind="domain"` routes into `SearchParams.kind` and
    /// the type filter fires identically to passing `type="domain"`.
    ///
    /// Also asserts that the legacy `type=` key still works (additive, non-breaking).
    #[tokio::test]
    async fn search_kind_param_alias_filters_domain() {
        let f = pack(rt_with_embedder());

        // Seed atoms (they will crowd the ANN results without a type filter).
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "alias-atom-1", "name": "Alias Atom 1", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                    { "slug": "alias-atom-2", "name": "Alias Atom 2", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                    { "slug": "alias-atom-3", "name": "Alias Atom 3", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain with identical content so the ANN index holds both kinds.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "alias-domain",
                    "name": "Alias Domain",
                    "description": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Index with rebuild_ann=true so the ANN holds both atoms and the domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 3,
            "atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // -- GREEN path: kind= alias must filter identically to type= ----------------
        // Using "kind": "domain" (the khive-wide canonical key).
        let resp_kind = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "embedding retrieval vector search unique aliaskind1",
                    "kind": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search kind=domain ok");

        let results_kind = resp_kind["results"].as_array().expect("results");
        assert!(
            !results_kind.is_empty(),
            "search with kind=domain alias must return the domain: {resp_kind:?}"
        );
        for r in results_kind {
            let kind = r["kind"].as_str().unwrap_or("");
            assert_eq!(
                kind, "domain",
                "search kind=domain alias: all results must have kind=domain, got: {r:?}"
            );
        }

        // -- Additive check: legacy type= key must still work ------------------------
        let resp_type = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "embedding retrieval vector search unique aliaskind1",
                    "type": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=domain ok");

        let results_type = resp_type["results"].as_array().expect("results");
        assert!(
            !results_type.is_empty(),
            "legacy type=domain must still filter correctly: {resp_type:?}"
        );
        for r in results_type {
            let kind = r["kind"].as_str().unwrap_or("");
            assert_eq!(
                kind, "domain",
                "type=domain (legacy): all results must have kind=domain, got: {r:?}"
            );
        }
    }
}

include!("fixes_tests/compose_section_explanation_tests.rs");

// ── Issue #184: ANN cold-start warming guard ──────────────────────────────────
//
// Regression tests for the fix in knowledge/search.rs and knowledge/vamana.rs.
// Full end-to-end warming cannot be tested in-process without a real embedding
// model, so these tests cover the observable invariants that are testable with
// the in-memory runtime:
//
//   1. Empty corpus (0 atoms, 0 domains) → ok:true, total:0 (not an error).
//   2. Populated corpus, no ANN warm triggered → normal FTS path still returns
//      results (verifies the guard does not break the non-warming hot path).

#[tokio::test]
async fn ann_warmup_guard_empty_corpus_returns_ok_not_error() {
    // Reproduces the scenario where the corpus is genuinely empty (no atoms,
    // no domains).  The warming guard must distinguish "warming" from "empty"
    // and must NOT return an error for the empty case.
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "machine learning neural networks deep learning transformers" }),
        )
        .await
        .expect("suggest on empty corpus must succeed, not return an error");

    let total = resp["total"].as_u64().expect("total field must be present");
    assert_eq!(
        total, 0,
        "empty corpus must return total:0, not a warming error: {resp}"
    );
    let results = resp["results"]
        .as_array()
        .expect("results field must be present");
    assert!(
        results.is_empty(),
        "empty corpus must return empty results array: {resp}"
    );
}

#[tokio::test]
async fn ann_warmup_guard_populated_corpus_fts_path_returns_results() {
    // Verifies the guard does not break the non-ANN FTS path.
    // Populate a domain and an atom, then run suggest and confirm FTS hits come through.
    let f = pack(rt());

    // Create a domain with distinctive tokens so FTS can match it.
    // Description must be at least 20 words to pass the upsert validation.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "ml-foundation-184",
                "name": "ML Foundation",
                "description": "machine learning neural networks backpropagation gradient descent optimization unique184xqz transformer architectures attention mechanisms embedding representations dense retrieval sparse ranking reranking pipelines semantic search",
                "tags": ["type:domain"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    // Suggest with tokens that match the domain.
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({
                "query": "machine learning neural networks backpropagation gradient descent unique184xqz optimization transformers attention"
            }),
        )
        .await
        .expect("suggest must succeed");

    // With no embedder the ANN path is skipped entirely; FTS still runs.
    // The result must be ok (not an error) regardless of whether results are found.
    assert!(
        resp.get("results").is_some(),
        "populated corpus suggest must return a results field: {resp}"
    );
    assert!(
        resp.get("total").is_some(),
        "populated corpus suggest must return a total field: {resp}"
    );
}

include!("fixes_tests/topic_query_regression_tests.rs");
