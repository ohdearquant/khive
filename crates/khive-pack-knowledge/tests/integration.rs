// FILE SIZE JUSTIFICATION: This file covers the full public verb surface of the knowledge
// pack (upsert_atoms, upsert_domains, get, list, delete_atoms, stats, index, fold, search,
// suggest, compose, edit, import, challenge, adjudicate, learn, cite, topic) with multiple
// scenarios per verb (happy path, edge cases, namespace isolation, pagination). Each test
// requires a fresh in-memory runtime, making per-verb test file splitting impractical without
// re-creating the same setup boilerplate in every file. Splitting is deferred until shared
// test fixtures can be extracted into a crate-level test helper module.

//! Integration tests for the knowledge pack against an in-memory runtime.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, PackRegistry, RequestIdentity,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// ── test fixture ──────────────────────────────────────────────────────────────

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
}

#[derive(Debug, Default)]
struct NestedProfileIdentityGate {
    requests: Mutex<Vec<(String, String, String)>>,
}

impl Gate for NestedProfileIdentityGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.requests.lock().expect("gate request lock").push((
            req.verb.clone(),
            req.actor.id.clone(),
            req.namespace.as_str().to_string(),
        ));
        if matches!(req.verb.as_str(), "brain.resolve" | "brain.profile")
            && req.actor.id != "requester"
        {
            return Ok(GateDecision::deny(
                "nested profile reads require the per-request principal",
            ));
        }
        Ok(GateDecision::allow())
    }
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

include!("integration_tests/learn_and_registration_tests.rs");

include!("integration_tests/cite_tests.rs");

include!("integration_tests/topic_listing_tests.rs");

include!("integration_tests/topic_query_tests.rs");

include!("integration_tests/atom_upsert_tests.rs");

include!("integration_tests/domain_upsert_tests.rs");

include!("integration_tests/record_get_tests.rs");

include!("integration_tests/prefix_and_namespace_get_tests.rs");

include!("integration_tests/section_get_tests.rs");

include!("integration_tests/list_projection_tests.rs");

include!("integration_tests/list_walk_tests.rs");

/// #1671: the batch re-embed sweep in `knowledge.index` pages the full atom
/// table with `created_at ASC, id ASC`. A recording embedder captures the
/// texts in the order the pages deliver them; with every atom forced onto
/// one shared `created_at` (so the `id` tiebreak is load-bearing), the
/// recorded sequence must equal every atom exactly once in `id ASC` order —
/// no duplicates, no misses, and the exact documented order.
#[tokio::test]
async fn index_reembed_paging_sweep_covers_equal_created_at_in_order() {
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    /// Records every text it is asked to embed, in call order, and returns a
    /// fixed vector per text so the vector write succeeds.
    struct RecordingEmbedService {
        recorded: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbeddingService for RecordingEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.recorded
                .lock()
                .expect("recorded lock")
                .extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![0.5f32; DIM]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "recording-embed-service"
        }
    }

    struct RecordingEmbedProvider {
        recorded: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbedderProvider for RecordingEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(RecordingEmbedService {
                recorded: Arc::clone(&self.recorded),
            }))
        }
    }

    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
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
    rt.register_embedder(RecordingEmbedProvider {
        recorded: Arc::clone(&recorded),
    });
    let f = pack(rt.clone());

    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for n in 0..31 {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": format!("reembed-{n:03}"), "name": format!("ReEmbed {n:03}"), "content": content }] }),
        )
        .await
        .expect("upsert atom");
    }

    // Force every atom onto one shared `created_at` so the paging sweep
    // exercises the tiebreak path; otherwise unique microsecond timestamps
    // would let the test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }
    // One shared `created_at` reduces the documented `created_at ASC,
    // id ASC` page order to `id ASC`; read (name, id) straight from the
    // table and sort by id in Rust rather than re-deriving with SQL. The
    // writer guard above is dropped before this reader opens.
    let expected_names: Vec<String> = {
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("sql reader must open");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id, name FROM knowledge_atoms WHERE deleted_at IS NULL".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("read atom rows");
        let mut pairs: Vec<(String, String)> = rows
            .iter()
            .map(|row| {
                let id = match row.get("id") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected id {other:?}"),
                };
                let name = match row.get("name") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected name {other:?}"),
                };
                (id, name)
            })
            .collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        pairs.into_iter().map(|(_, name)| name).collect()
    };
    assert_eq!(expected_names.len(), 31, "all atoms must be seeded");

    let result = f
        .dispatch("knowledge.index", json!({ "batch_size": 7 }))
        .await
        .expect("index ok");
    assert_eq!(
        result["indexed"].as_u64(),
        Some(31),
        "every atom must be indexed by the default engine: {result:?}"
    );

    // `atom_embed_text` puts the atom name first, so the first line of each
    // recorded text is the atom name; the concatenated pages must deliver
    // every atom exactly once in the documented page order.
    let recorded_names: Vec<String> = recorded
        .lock()
        .expect("recorded lock")
        .iter()
        .map(|text| {
            text.lines()
                .next()
                .expect("embed text carries the atom name")
                .to_string()
        })
        .collect();
    assert_eq!(
        recorded_names, expected_names,
        "re-embed paging sweep must embed every atom exactly once in \
         created_at ASC, id ASC order"
    );

    // The recording provider is pack-supplied, even though it replaces a
    // built-in model name. Its input preparation is not attested, so live
    // vectors must exist without a claimed exact-input fingerprint.
    let live_vectors = {
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("provenance reader");
        reader
            .query_all(SqlStatement {
                sql: "SELECT a.name, p.text_fingerprint \
                      FROM knowledge_atoms AS a \
                      JOIN vec_all_minilm_l6_v2 AS v \
                        ON v.subject_id = a.id AND v.namespace = a.namespace \
                       AND v.field = 'knowledge.atom' \
                      LEFT JOIN vector_provenance AS p \
                        ON p.model_key = 'all_minilm_l6_v2' \
                       AND p.subject_id = v.subject_id AND p.namespace = v.namespace \
                      WHERE a.deleted_at IS NULL"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("read live vectors and optional provenance")
    };
    let mut indexed_names = std::collections::HashSet::new();
    for row in live_vectors {
        let name = match row.get("name") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => panic!("unexpected atom name {other:?}"),
        };
        assert!(
            indexed_names.insert(name.clone()),
            "each atom must have exactly one live vector: {name}"
        );
        assert!(
            matches!(row.get("text_fingerprint"), Some(SqlValue::Null)),
            "custom provider must not claim exact-input provenance for {name}"
        );
    }
    assert_eq!(indexed_names.len(), expected_names.len());
    assert!(
        expected_names
            .iter()
            .all(|name| indexed_names.contains(name)),
        "all recorded atoms must retain a live vector"
    );
}

// ── delete_atoms ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn knowledge_index_persists_audited_bounded_prefixed_fingerprint() {
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL: EmbeddingModel = EmbeddingModel::MultilingualE5Small;
    const PREFIX: &str = "passage: ";

    struct RecordingService(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl EmbeddingService for RecordingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0
                .lock()
                .expect("recorded input lock")
                .extend_from_slice(texts);
            Ok(texts
                .iter()
                .map(|_| vec![0.25; MODEL.dimensions()])
                .collect())
        }

        fn supports_model(&self, model: EmbeddingModel) -> bool {
            model == MODEL
        }

        fn name(&self) -> &'static str {
            "knowledge-audited-test-service"
        }
    }
    struct RecordingProvider(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl EmbedderProvider for RecordingProvider {
        fn name(&self) -> &str {
            "multilingual-e5-small"
        }

        fn dimensions(&self) -> usize {
            MODEL.dimensions()
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(RecordingService(Arc::clone(&self.0))))
        }
    }

    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
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
        embedding_model: Some(MODEL),
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
    rt.register_test_audited_embedder(MODEL, RecordingProvider(Arc::clone(&recorded)));
    let f = pack(rt.clone());
    let name = "Audited E5 atom";
    let content = format!("{}海", "海 ".repeat(11_999));
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": "audited-e5-atom", "name": name, "content": content.clone()}]}),
    )
    .await
    .expect("upsert atom");
    let indexed = f
        .dispatch("knowledge.index", json!({"ids": ["audited-e5-atom"]}))
        .await
        .expect("index atom");
    assert_eq!(indexed["indexed"].as_u64(), Some(1), "{indexed:?}");

    let source = format!("{name}\n\n{content}");
    let budget = lattice_embed::MAX_TEXT_BYTES - PREFIX.len();
    let end = source
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= budget)
        .last()
        .expect("nonempty input");
    let prepared = format!("{PREFIX}{}", &source[..end]);
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        std::slice::from_ref(&prepared)
    );
    assert!(prepared.len() <= lattice_embed::MAX_TEXT_BYTES);
    assert!(
        source.len() > budget,
        "fixture must exercise UTF-8 truncation"
    );
    let expected = blake3::hash(prepared.as_bytes()).to_hex().to_string();

    async fn stored_fingerprint(rt: &KhiveRuntime) -> String {
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("provenance reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT p.text_fingerprint FROM vector_provenance AS p \
                      JOIN vec_multilingual_e5_small AS v \
                        ON p.subject_id = v.subject_id AND p.namespace = v.namespace \
                      JOIN knowledge_atoms AS a \
                        ON a.id = v.subject_id AND a.namespace = v.namespace \
                      WHERE p.model_key = 'multilingual_e5_small' \
                        AND v.field = 'knowledge.atom' AND a.slug = 'audited-e5-atom'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("read stored fingerprint")
            .expect("attributed atom vector");
        match row.get("text_fingerprint") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => panic!("expected exact-input fingerprint, got {other:?}"),
        }
    }
    assert_eq!(stored_fingerprint(&rt).await, expected);

    f.dispatch("knowledge.index", json!({"ids": ["audited-e5-atom"]}))
        .await
        .expect("repeat same-source index");
    assert_eq!(stored_fingerprint(&rt).await, expected);
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &[prepared.clone(), prepared.clone()]
    );

    let changed_content = content.replacen('海', "河", 1);
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": "audited-e5-atom", "name": name, "content": changed_content}]}),
    )
    .await
    .expect("change bounded source byte");
    f.dispatch("knowledge.index", json!({"ids": ["audited-e5-atom"]}))
        .await
        .expect("index changed atom");
    let changed_prepared = prepared.replacen('海', "河", 1);
    assert_eq!(recorded.lock().unwrap()[2], changed_prepared);
    let changed = blake3::hash(changed_prepared.as_bytes())
        .to_hex()
        .to_string();
    assert_ne!(changed, expected);
    assert_eq!(stored_fingerprint(&rt).await, changed);
}

// ── delete_atoms ──────────────────────────────────────────────────────────────

// `knowledge.index` deliberately does not accept a FTS rebuild — rebuilding
// both global FTS indexes is a whole-database operation with no per-caller
// cost admission on the ordinary verb, and the namespace-scoped
// `reindex_knowledge` options do not offer it either. The only entry point is
// the operator call `khive_pack_knowledge::rebuild_knowledge_fts_indexes`
// (what `kkernel reindex --rebuild-fts` uses), which takes no namespace
// token, not the MCP verb dispatch this fixture drives for every other test
// in this file.
#[tokio::test]
async fn index_rebuild_fts_repairs_atom_and_section_external_content_drift() {
    let runtime = rt();
    let atom_id = "22730000-0000-4000-8000-000000000001";

    let mut writer = runtime.sql().writer().await.expect("knowledge writer");
    writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, created_at, updated_at) \
                      VALUES (?1, 'local', 'fts-repair-atom', 'FTS Repair Atom', \
                              'repairable lexical atom document', 1, 1)"
                    .into(),
                params: vec![SqlValue::Text(atom_id.into())],
                label: Some("test.knowledge_fts_repair.atom".into()),
            },
            SqlStatement {
                sql: "INSERT INTO knowledge_sections \
                      (id, atom_id, namespace, section_type, heading, content, content_hash, \
                       created_at, updated_at) \
                      VALUES ('22730000-0000-4000-8000-000000000002', ?1, 'local', \
                              'overview', 'Repairable Overview', \
                              'repairable lexical section document', 'repair-hash', 1, 1)"
                    .into(),
                params: vec![SqlValue::Text(atom_id.into())],
                label: Some("test.knowledge_fts_repair.section".into()),
            },
            SqlStatement {
                sql: "INSERT INTO fts_knowledge \
                      (fts_knowledge, rowid, id, namespace, slug, name, content) \
                      SELECT 'delete', rowid, id, namespace, slug, name, content \
                      FROM knowledge_atoms WHERE id = ?1"
                    .into(),
                params: vec![SqlValue::Text(atom_id.into())],
                label: Some("test.knowledge_fts_repair.desync_atom".into()),
            },
            SqlStatement {
                sql: "INSERT INTO fts_sections \
                      (fts_sections, rowid, id, namespace, atom_id, section_type, heading, content) \
                      SELECT 'delete', rowid, id, namespace, atom_id, section_type, heading, content \
                      FROM knowledge_sections WHERE atom_id = ?1"
                    .into(),
                params: vec![SqlValue::Text(atom_id.into())],
                label: Some("test.knowledge_fts_repair.desync_section".into()),
            },
        ])
        .await
        .expect("seed and deliberately desynchronize both knowledge FTS indexes");
    drop(writer);

    let result = khive_pack_knowledge::rebuild_knowledge_fts_indexes(&runtime)
        .await
        .expect("knowledge FTS repair must succeed");
    assert_eq!(result["integrity_ok"], true, "repair result: {result}");
    let indexes: Vec<&str> = result["indexes"]
        .as_array()
        .expect("indexes array")
        .iter()
        .map(|v| v.as_str().expect("index name"))
        .collect();
    assert_eq!(
        indexes,
        vec!["fts_knowledge", "fts_sections"],
        "repair result: {result}"
    );
    assert!(
        result.get("atoms_indexed").is_none(),
        "the rebuild is FTS-only maintenance and reports no embedding work: {result}"
    );

    let mut reader = runtime.sql().reader().await.expect("knowledge reader");
    for (label, sql) in [
        (
            "atom",
            "SELECT count(*) AS n FROM fts_knowledge \
             WHERE fts_knowledge MATCH 'repairable'",
        ),
        (
            "section",
            "SELECT count(*) AS n FROM fts_sections \
             WHERE fts_sections MATCH 'repairable'",
        ),
    ] {
        let row = reader
            .query_row(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some(format!("test.knowledge_fts_repair.verify_{label}")),
            })
            .await
            .expect("query repaired FTS index")
            .expect("count row");
        assert!(
            matches!(row.get("n"), Some(SqlValue::Integer(1))),
            "{label} FTS document must be restored; row={row:?}"
        );
    }
}

include!("integration_tests/atom_delete_tests.rs");

include!("integration_tests/stats_and_fold_tests.rs");

include!("integration_tests/search_tests.rs");

include!("integration_tests/suggest_tests.rs");

include!("integration_tests/compose_atom_and_identity_tests.rs");

include!("integration_tests/compose_domain_and_prefix_tests.rs");

include!("integration_tests/compose_input_and_output_tests.rs");

include!("integration_tests/creation_and_secret_admission_tests.rs");

include!("integration_tests/deleted_slug_tests.rs");

include!("integration_tests/record_resolver_tests.rs");

// ── ADR-051 Amendment 1: KG entity blending in knowledge.compose ─────────────
//
// A fake `EmbedderProvider` whose vector is a function of a marker substring
// ("zzzquantumfoo") rather than real semantics: texts containing the marker
// embed to [1.0, 0.0], everything else to [0.0, 1.0]. This makes the
// KG-concept-outranks-lore-atom scenario (the ADR-051 Amendment 1 dogfood
// motivation) deterministic without a real embedding model — the query and
// the seeded concept share the marker, atoms/domains never do.
mod kg_blend {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{oneshot, Notify};
    use tokio::time::{timeout, Duration};

    const MARKER: &str = "zzzquantumfoo";
    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;
    const RACE_WAIT: Duration = Duration::from_secs(30);

    struct MarkerEmbedService {
        recorded: Option<Arc<Mutex<Vec<String>>>>,
        pause: Option<Arc<EmbedPause>>,
    }

    struct EmbedPause {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: Notify,
    }

    #[async_trait]
    impl EmbeddingService for MarkerEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            if let Some(pause) = &self.pause {
                if texts
                    .iter()
                    .any(|text| text.contains("\nPaused heading\n\n"))
                {
                    let entered = pause.entered.lock().expect("pause lock").take();
                    if let Some(entered) = entered {
                        let _ = entered.send(());
                        pause.release.notified().await;
                    }
                }
            }
            if let Some(recorded) = &self.recorded {
                recorded
                    .lock()
                    .expect("recording lock")
                    .extend(texts.iter().cloned());
            }
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; DIM];
                    if t.contains(MARKER) {
                        v[0] = 1.0;
                    } else {
                        v[1] = 1.0;
                    }
                    v
                })
                .collect())
        }

        async fn embed_query(
            &self,
            texts: &[String],
            model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.embed(texts, model).await
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "marker-embed-service"
        }
    }

    struct MarkerEmbedProvider {
        recorded: Option<Arc<Mutex<Vec<String>>>>,
        pause: Option<Arc<EmbedPause>>,
    }

    #[async_trait]
    impl EmbedderProvider for MarkerEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(MarkerEmbedService {
                recorded: self.recorded.as_ref().map(Arc::clone),
                pause: self.pause.as_ref().map(Arc::clone),
            }))
        }
    }

    fn rt_with_marker_embedder() -> KhiveRuntime {
        rt_with_marker_embedder_recording(None)
    }

    fn rt_with_marker_embedder_recording(
        recorded: Option<Arc<Mutex<Vec<String>>>>,
    ) -> KhiveRuntime {
        rt_with_marker_embedder_options(recorded, None)
    }

    fn rt_with_marker_embedder_options(
        recorded: Option<Arc<Mutex<Vec<String>>>>,
        pause: Option<Arc<EmbedPause>>,
    ) -> KhiveRuntime {
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
        rt.register_embedder(MarkerEmbedProvider { recorded, pause });
        rt
    }

    const QUERY: &str = "zzzquantumfoo kv cache paging decode attention retrieval \
        augmented generation dense sparse benchmark corpus";

    const OVERLAP_CONTENT: &str = "kv cache paging decode attention retrieval augmented \
        generation dense sparse benchmark corpus latency gradient descent transformer vector \
        index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

    async fn section_embedding_head(rt: &KhiveRuntime, section_id: &str) -> [f32; 2] {
        let mut reader = rt.sql().reader().await.expect("section embedding reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT embedding FROM knowledge_sections WHERE id = ?1".into(),
                params: vec![SqlValue::Text(section_id.to_string())],
                label: None,
            })
            .await
            .expect("section embedding query")
            .expect("section row");
        let Some(SqlValue::Blob(bytes)) = row.get("embedding") else {
            panic!("section must have an inline embedding: {row:?}");
        };
        assert!(bytes.len() >= 8, "section embedding has two dimensions");
        [
            f32::from_le_bytes(bytes[0..4].try_into().expect("first dimension")),
            f32::from_le_bytes(bytes[4..8].try_into().expect("second dimension")),
        ]
    }

    async fn section_embedding_is_null(rt: &KhiveRuntime, section_id: &str) -> bool {
        let mut reader = rt.sql().reader().await.expect("section embedding reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT embedding FROM knowledge_sections WHERE id = ?1".into(),
                params: vec![SqlValue::Text(section_id.to_string())],
                label: None,
            })
            .await
            .expect("section embedding query")
            .expect("section row");
        matches!(row.get("embedding"), Some(SqlValue::Null))
    }

    #[tokio::test]
    async fn edit_same_body_changed_heading_refreshes_section_embedding() {
        let rt = rt_with_marker_embedder();
        let f = pack(rt.clone());
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "heading-vector-refresh",
                "name": "Heading Vector Refresh",
                "content": OVERLAP_CONTENT
            }] }),
        )
        .await
        .expect("upsert atom");

        let body = "The section body stays byte identical across edits so its content hash and section identity remain unchanged while the heading changes the embedding input.";
        let first = f
            .dispatch(
                "knowledge.edit",
                json!({ "id": "heading-vector-refresh", "sections": [{
                    "section_type": "overview", "heading": "Plain heading", "content": body
                }] }),
            )
            .await
            .expect("create section with plain heading");
        let section_id = first["sections"][0]["id"]
            .as_str()
            .expect("section id")
            .to_string();
        assert_eq!(section_embedding_head(&rt, &section_id).await, [0.0, 1.0]);

        let second = f
            .dispatch(
                "knowledge.edit",
                json!({ "id": "heading-vector-refresh", "sections": [{
                    "section_type": "overview", "heading": format!("{MARKER} heading"), "content": body
                }] }),
            )
            .await
            .expect("change heading without changing section body");
        assert_eq!(second["sections"][0]["id"], section_id);
        assert_eq!(section_embedding_head(&rt, &section_id).await, [1.0, 0.0]);
    }

    #[tokio::test]
    async fn atom_rename_clears_section_embedding_until_the_next_edit() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rt = rt_with_marker_embedder_recording(Some(Arc::clone(&recorded)));
        let f = pack(rt.clone());
        let atom = json!({
            "slug": "renamed-section-vector",
            "name": "Plain atom name",
            "content": OVERLAP_CONTENT,
        });
        f.dispatch("knowledge.upsert_atoms", json!({"atoms": [atom]}))
            .await
            .expect("create atom");
        let section = f
            .dispatch(
                "knowledge.edit",
                json!({"id": "renamed-section-vector", "sections": [{
                    "section_type": "overview",
                    "heading": "Plain heading",
                    "content": "This section keeps its heading and body while its atom is renamed, so only the atom breadcrumb should change in the embedding input."
                }]}),
            )
            .await
            .expect("create section");
        let section_id = section["sections"][0]["id"].as_str().expect("section id");
        assert_eq!(section_embedding_head(&rt, section_id).await, [0.0, 1.0]);

        let before = recorded.lock().expect("recording lock").len();
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": "renamed-section-vector",
                "name": "Plain atom name",
                "content": OVERLAP_CONTENT,
            }]}),
        )
        .await
        .expect("same-name upsert");
        assert_eq!(recorded.lock().expect("recording lock").len(), before);
        assert_eq!(section_embedding_head(&rt, section_id).await, [0.0, 1.0]);

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": "renamed-section-vector",
                "name": format!("{MARKER} atom name"),
                "content": OVERLAP_CONTENT,
            }]}),
        )
        .await
        .expect("rename atom");
        assert_eq!(recorded.lock().expect("recording lock").len(), before);
        assert!(section_embedding_is_null(&rt, section_id).await);

        f.dispatch(
            "knowledge.edit",
            json!({"id": "renamed-section-vector", "sections": [{
                "section_type": "overview",
                "heading": "Plain heading",
                "content": "This section keeps its heading and body while its atom is renamed, so only the atom breadcrumb should change in the embedding input."
            }]}),
        )
        .await
        .expect("re-embed the renamed section");
        assert_eq!(section_embedding_head(&rt, section_id).await, [1.0, 0.0]);
    }

    #[tokio::test]
    async fn domain_mirror_rename_clears_section_embedding_until_the_next_edit() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rt = rt_with_marker_embedder_recording(Some(Arc::clone(&recorded)));
        let f = pack(rt.clone());
        let body = "A domain mirror can own a section, and its name is part of that section's embedding input even when its heading and content remain unchanged.";
        f.dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "renamed-domain-mirror",
                "name": "Plain domain name",
                "description": OVERLAP_CONTENT
            }]}),
        )
        .await
        .expect("create domain");
        let section = f
            .dispatch(
                "knowledge.edit",
                json!({"id": "renamed-domain-mirror", "sections": [{
                    "section_type": "overview",
                    "heading": "Plain heading",
                    "content": body
                }]}),
            )
            .await
            .expect("create section on domain mirror");
        let section_id = section["sections"][0]["id"].as_str().expect("section id");
        assert_eq!(section_embedding_head(&rt, section_id).await, [0.0, 1.0]);

        let before = recorded.lock().expect("recording lock").len();
        f.dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "renamed-domain-mirror",
                "name": format!("{MARKER} domain name"),
                "description": OVERLAP_CONTENT
            }]}),
        )
        .await
        .expect("rename domain");
        assert_eq!(recorded.lock().expect("recording lock").len(), before);
        assert!(section_embedding_is_null(&rt, section_id).await);

        f.dispatch(
            "knowledge.edit",
            json!({"id": "renamed-domain-mirror", "sections": [{
                "section_type": "overview",
                "heading": "Plain heading",
                "content": body
            }]}),
        )
        .await
        .expect("refresh domain mirror section");
        assert_eq!(section_embedding_head(&rt, section_id).await, [1.0, 0.0]);
    }

    #[tokio::test]
    async fn late_section_embedding_cannot_overwrite_newer_heading_vector() {
        let (entered, waiting) = oneshot::channel();
        let pause = Arc::new(EmbedPause {
            entered: Mutex::new(Some(entered)),
            release: Notify::new(),
        });
        let rt = rt_with_marker_embedder_options(None, Some(Arc::clone(&pause)));
        let f = Arc::new(pack(rt.clone()));
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": "late-section-embed",
                "name": "Plain atom name",
                "content": OVERLAP_CONTENT,
            }]}),
        )
        .await
        .expect("create atom");
        let body = "This body stays the same across both concurrent edits so both writes address the same section, while the headings produce different deterministic vectors.";
        let original = f
            .dispatch(
                "knowledge.edit",
                json!({"id": "late-section-embed", "sections": [{
                    "section_type": "overview", "heading": "Original heading", "content": body,
                }]}),
            )
            .await
            .expect("create section");
        let section_id = original["sections"][0]["id"]
            .as_str()
            .expect("section id")
            .to_string();
        assert_eq!(section_embedding_head(&rt, &section_id).await, [0.0, 1.0]);

        let first = Arc::clone(&f);
        let earlier = tokio::spawn(async move {
            first
                .dispatch(
                    "knowledge.edit",
                    json!({"id": "late-section-embed", "sections": [{
                        "section_type": "overview", "heading": "Paused heading", "content": body,
                    }]}),
                )
                .await
        });
        timeout(RACE_WAIT, waiting)
            .await
            .expect("first embedding did not pause")
            .expect("first embedding did not signal");

        let newer = timeout(
            RACE_WAIT,
            f.dispatch("knowledge.edit", json!({"id": "late-section-embed", "sections": [{
                "section_type": "overview", "heading": format!("{MARKER} new heading"), "content": body,
            }]})),
        )
            .await
            .expect("newer edit blocked behind paused embed")
            .expect("newer edit completes before old embedding");
        assert_eq!(newer["sections"][0]["id"], section_id);
        assert_eq!(section_embedding_head(&rt, &section_id).await, [1.0, 0.0]);

        pause.release.notify_one();
        earlier
            .await
            .expect("earlier edit task")
            .expect("earlier edit");
        assert_eq!(section_embedding_head(&rt, &section_id).await, [1.0, 0.0]);
    }

    #[tokio::test]
    async fn late_reindex_after_atom_rename_is_superseded_not_failed() {
        let pause = Arc::new(EmbedPause {
            entered: Mutex::new(None),
            release: Notify::new(),
        });
        let rt = rt_with_marker_embedder_options(None, Some(Arc::clone(&pause)));
        let f = pack(rt.clone());
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": "rename-during-reindex",
                "name": "Plain atom name",
                "content": OVERLAP_CONTENT,
            }]}),
        )
        .await
        .expect("create atom");
        let section = f
            .dispatch("knowledge.edit", json!({"id": "rename-during-reindex", "sections": [{
                "section_type": "overview",
                "heading": "Paused heading",
                "content": "This section remains byte identical through the rename and reindex race, so its vector must encode the current atom name after the late reindex finishes."
            }]}))
            .await
            .expect("create section");
        let section_id = section["sections"][0]["id"].as_str().expect("section id");
        assert_eq!(section_embedding_head(&rt, section_id).await, [0.0, 1.0]);

        let (entered, waiting) = oneshot::channel();
        *pause.entered.lock().expect("pause lock") = Some(entered);
        let reindex_runtime = rt.clone();
        let reindex = tokio::spawn(async move {
            let token = reindex_runtime
                .authorize(Namespace::local())
                .expect("authorize reindex");
            khive_pack_knowledge::reindex_knowledge(
                &reindex_runtime,
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
        });
        timeout(RACE_WAIT, waiting)
            .await
            .expect("reindex embedding did not pause")
            .expect("reindex embedding did not signal");

        timeout(
            RACE_WAIT,
            f.dispatch(
                "knowledge.upsert_atoms",
                json!({"atoms": [{
                    "slug": "rename-during-reindex",
                    "name": format!("{MARKER} atom name"),
                    "content": OVERLAP_CONTENT,
                }]}),
            ),
        )
        .await
        .expect("rename blocked behind paused embed")
        .expect("rename atom");
        assert!(section_embedding_is_null(&rt, section_id).await);

        pause.release.notify_one();
        let result = reindex
            .await
            .expect("reindex task")
            .expect("reindex result");
        assert_eq!(result["sections_superseded"], 1);
        assert_eq!(result["sections_indexed"], 0);
        assert_eq!(result["sections_failed"], 0);
        assert!(section_embedding_is_null(&rt, section_id).await);

        f.dispatch(
            "knowledge.edit",
            json!({"id": "rename-during-reindex", "sections": [{
                "section_type": "overview",
                "heading": "Paused heading",
                "content": "This section remains byte identical through the rename and reindex race, so its vector must encode the current atom name after the late reindex finishes."
            }]}),
        )
        .await
        .expect("refresh renamed section");
        assert_eq!(section_embedding_head(&rt, section_id).await, [1.0, 0.0]);
    }

    async fn seed_domain_and_atom(f: &Fixture) -> String {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": OVERLAP_CONTENT, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");

        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");

        let domain_resp = f
            .dispatch("knowledge.get", json!({ "id": "kg-blend-domain" }))
            .await
            .expect("get domain");
        domain_resp["id"].as_str().expect("domain id").to_string()
    }

    async fn seed_kg_concept(f: &Fixture) -> String {
        let resp = f
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "ZipCache",
                    "description": format!(
                        "{MARKER} paged KV cache quantization technique for decode-time attention"
                    ),
                }),
            )
            .await
            .expect("create kg concept");
        resp["id"].as_str().expect("entity id").to_string()
    }

    /// Test 1: AUTO compose with a query whose top KG concept is seeded blends
    /// the entity into `data.entities` and the "Knowledge graph" markdown
    /// section, while atoms remain present.
    #[tokio::test]
    async fn auto_compose_blends_seeded_kg_concept() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        let concept_id = seed_kg_concept(&f).await;

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");

        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present when a KG concept blends");
        assert!(
            entities.iter().any(|e| e["id"] == concept_id),
            "expected seeded concept {concept_id} in entities, got: {entities:?}"
        );

        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            md.contains("Knowledge graph"),
            "markdown must contain the Knowledge graph section, got: {md}"
        );
        assert!(
            md.contains("ZipCache"),
            "markdown must render the blended concept's name, got: {md}"
        );
    }

    /// #2232: auto-compose crosses suggest ANN/rerank, atom rerank, and the
    /// KG blend search. Every stage must reuse the first successful query
    /// vector instead of independently embedding the same request text.
    #[tokio::test]
    async fn auto_compose_embeds_request_query_exactly_once() {
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let f = pack(rt_with_marker_embedder_recording(Some(Arc::clone(
            &recorded,
        ))));
        seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;
        recorded.lock().expect("recording lock").clear();

        let response = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");
        assert!(
            response["data"]["count"].as_u64().unwrap_or_default() > 0,
            "fixture must exercise a non-empty compose path: {response:?}"
        );

        let recorded = recorded.lock().expect("recording lock");
        assert_eq!(
            recorded
                .iter()
                .filter(|text| text.as_str() == QUERY)
                .count(),
            1,
            "one request must embed its query exactly once: {recorded:?}"
        );
    }

    /// #1505 direct-call exactness: even when the authorized token's primary
    /// namespace matches the explicit arm, a broader visible set must be
    /// narrowed before KG blending. Otherwise a local-only entity leaks into
    /// an arm briefing while corpus rows remain correctly arm-scoped.
    #[tokio::test]
    async fn direct_compose_narrows_matching_broad_token_before_kg_blend() {
        use khive_runtime::PackRuntime;

        let rt = rt_with_marker_embedder();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("kg registry");
        let local_entity = registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "LocalOnlyZipCache",
                    "description": format!(
                        "{MARKER} local-only paged KV cache quantization technique"
                    ),
                }),
            )
            .await
            .expect("create local-only KG entity")["id"]
            .as_str()
            .expect("entity id")
            .to_string();

        let arm_ns = Namespace::parse("bench-arm-a").expect("arm namespace");
        let arm_entity = registry
            .dispatch(
                "create",
                json!({
                    "namespace": arm_ns.as_str(),
                    "kind": "concept",
                    "name": "ArmOnlyZipCache",
                    "description": format!(
                        "{MARKER} arm-only paged KV cache quantization technique"
                    ),
                }),
            )
            .await
            .expect("create arm-only KG entity")["id"]
            .as_str()
            .expect("arm entity id")
            .to_string();
        let broad_arm_token = rt
            .authorize_with_visibility(arm_ns.clone(), vec![Namespace::local()])
            .expect("arm token with local visibility");
        let knowledge = KnowledgePack::new(rt.clone());
        knowledge
            .dispatch(
                "knowledge.upsert_atoms",
                json!({
                    "atoms": [{
                        "slug": "arm-kg-blend-atom",
                        "name": "Arm KG Blend Atom",
                        "content": format!("{MARKER} {OVERLAP_CONTENT}"),
                        "finalized": true,
                    }]
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("upsert arm atom");
        knowledge
            .dispatch(
                "knowledge.upsert_domains",
                json!({
                    "domains": [{
                        "slug": "arm-kg-blend-domain",
                        "name": "Arm KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["arm-kg-blend-atom"],
                    }]
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("upsert arm domain");

        let resp = knowledge
            .dispatch(
                "knowledge.compose",
                json!({
                    "namespace": arm_ns.as_str(),
                    "domain_ids": ["arm-kg-blend-domain"],
                    "query": QUERY,
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("direct exact-arm compose");
        let entities = resp["data"]["entities"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            entities.iter().any(|entity| entity["id"] == arm_entity),
            "the arm KG candidate must prove the blend leg ran: {entities:?}"
        );
        assert!(
            entities.iter().all(|entity| entity["id"] != local_entity),
            "local-only KG entity leaked through a matching but broad direct-call token: {entities:?}"
        );
    }

    /// Test 2: `blend_kg=false` behaves exactly as compose did before this
    /// feature — no `entities` field, no "Knowledge graph" markdown section —
    /// even when a matching KG concept exists.
    #[tokio::test]
    async fn blend_kg_false_omits_entities() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "blend_kg": false }),
            )
            .await
            .expect("compose with blend_kg=false ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "entities must be absent when blend_kg=false, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "markdown must not contain a Knowledge graph section when blend_kg=false, got: {md}"
        );
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");
    }

    /// #2307 fix 2: an explicit (`domain_ids`) compose call has no ANN stage
    /// to prime the query embedding cache, so it only pays for a dedicated
    /// role-specific `embed_query` round trip when something downstream
    /// actually requires that role — the KG blend. With `blend_kg: false`
    /// nothing in the request needs a role-specific vector, so the rerank
    /// stage folds the query into its single candidate batch: exactly one
    /// `embed()` round trip for the whole request, matching the pre-#2232
    /// combined-batch cost. Before this fix the fallback embed_query call
    /// ran unconditionally regardless of `blend_kg`, always costing a
    /// second round trip even when its result had no consumer.
    #[tokio::test]
    async fn explicit_compose_without_kg_blend_embeds_in_single_round_trip() {
        use async_trait::async_trait;
        use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

        const DIM: usize = 384;
        const MODEL_KEY: &str = "all-minilm-l6-v2";

        struct CountingEmbedService {
            calls: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl EmbeddingService for CountingEmbedService {
            async fn embed(
                &self,
                texts: &[String],
                _model: EmbeddingModel,
            ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                *self.calls.lock().expect("calls lock") += 1;
                Ok(texts.iter().map(|_| vec![0.1f32; DIM]).collect())
            }

            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "round-trip-counting-embed-service"
            }
        }

        struct CountingEmbedProvider {
            calls: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl EmbedderProvider for CountingEmbedProvider {
            fn name(&self) -> &str {
                MODEL_KEY
            }

            fn dimensions(&self) -> usize {
                DIM
            }

            async fn build(
                &self,
            ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError>
            {
                Ok(Arc::new(CountingEmbedService {
                    calls: Arc::clone(&self.calls),
                }))
            }
        }

        let calls = Arc::new(Mutex::new(0usize));
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
        rt.register_embedder(CountingEmbedProvider {
            calls: Arc::clone(&calls),
        });
        let f = pack(rt);
        let domain_id = seed_domain_and_atom(&f).await;
        *calls.lock().expect("calls lock") = 0;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY, "blend_kg": false }),
            )
            .await
            .expect("explicit compose with blend_kg=false ok");
        assert!(
            resp["data"]["count"].as_u64().unwrap_or_default() > 0,
            "fixture must exercise a non-empty compose path: {resp:?}"
        );

        assert_eq!(
            *calls.lock().expect("calls lock"),
            1,
            "blend_kg=false must embed the query and its candidates in one combined \
             round trip, not a dedicated role-specific call plus a separate batch"
        );
    }

    /// Test 3: `atom_ids`-only calls never blend, regardless of `blend_kg`
    /// (which defaults to true) — the caller pinned exact atoms.
    #[tokio::test]
    async fn atom_ids_only_never_blends() {
        let f = pack(rt_with_marker_embedder());
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": OVERLAP_CONTENT }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "atom_ids": ["kg-blend-atom"], "query": QUERY }),
            )
            .await
            .expect("atom_ids-only compose ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "atom_ids-only compose must never blend KG entities, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "atom_ids-only markdown must not contain a Knowledge graph section, got: {md}"
        );
    }

    /// Test 4: blended entities respect `max_tokens` trimming — a budget too
    /// tight to fit any entity after the atom/section body omits the
    /// "Knowledge graph" section entirely, while the atom body itself
    /// survives untouched.
    #[tokio::test]
    async fn blended_entities_respect_max_tokens_budget() {
        let f = pack(rt_with_marker_embedder());

        // A padded atom whose fully rendered briefing consumes nearly all
        // of the 2000-byte floor budget, leaving too little for a KG heading
        // and entity line. The atom must survive either way.
        let filler = "x".repeat(1510);
        let big_content = format!("{OVERLAP_CONTENT} {filler}");
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": big_content, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");
        seed_kg_concept(&f).await;

        // Generous budget: entity fits.
        let generous = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 8000 }),
            )
            .await
            .expect("generous-budget compose ok");
        let generous_atoms = generous["data"]["atoms"].as_array().expect("atoms array");
        assert!(!generous_atoms.is_empty(), "atoms must be present");
        assert!(
            generous["data"].get("entities").is_some(),
            "entity must fit under a generous budget, got: {}",
            generous["data"]
        );

        // Minimum-clamp budget: the atom body alone consumes it, leaving no
        // room for the entity section — atoms must still survive.
        let tight = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 500 }),
            )
            .await
            .expect("tight-budget compose ok");
        let tight_atoms = tight["data"]["atoms"].as_array().expect("atoms array");
        assert!(!tight_atoms.is_empty(), "atoms must survive a tight budget");
        assert!(
            tight["data"]["markdown"].as_str().expect("markdown").len() <= 2_000,
            "the whole rendered briefing, including the KG tail, must fit"
        );
        assert!(
            tight["data"].get("entities").is_none(),
            "entity must be trimmed out under a tight budget, got: {}",
            tight["data"]
        );
    }

    async fn seed_kg_document(f: &Fixture) -> String {
        let resp = f
            .dispatch(
                "create",
                json!({
                    "kind": "document",
                    "name": "ADR-051",
                    "description": format!(
                        "{MARKER} section-embeddings hybrid compose architecture decision record"
                    ),
                }),
            )
            .await
            .expect("create kg document");
        resp["id"].as_str().expect("entity id").to_string()
    }

    /// Test 5: explicit `domain_ids` (non-AUTO) calls blend exactly like AUTO
    /// calls do — the Amendment 1 decision covers both, not just AUTO.
    #[tokio::test]
    async fn explicit_domain_ids_compose_blends_seeded_kg_concept() {
        let f = pack(rt_with_marker_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        let concept_id = seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY }),
            )
            .await
            .expect("explicit domain_ids compose ok");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");
        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present for explicit domain_ids blend");
        assert!(
            entities.iter().any(|e| e["id"] == concept_id),
            "expected seeded concept {concept_id} in entities, got: {entities:?}"
        );
    }

    /// Test 6: a `document`-kind KG entity (not just `concept`) blends.
    #[tokio::test]
    async fn auto_compose_blends_seeded_kg_document() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        let document_id = seed_kg_document(&f).await;

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let data = &resp["data"];
        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present when a KG document blends");
        assert!(
            entities
                .iter()
                .any(|e| e["id"] == document_id && e["kind"] == "document"),
            "expected seeded document {document_id} in entities, got: {entities:?}"
        );
    }

    /// Test 7: the blended candidate pool is capped at `KG_BLEND_CAP` (5) and
    /// deduplicated — seeding more matching concept/document entities than
    /// the cap never yields duplicate or over-cap results.
    #[tokio::test]
    async fn blend_candidates_are_capped_and_deduped_across_kinds() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;

        for i in 0..4 {
            f.dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("Concept{i}"),
                    "description": format!("{MARKER} paged KV cache technique variant {i}"),
                }),
            )
            .await
            .expect("create kg concept");
        }
        for i in 0..4 {
            f.dispatch(
                "create",
                json!({
                    "kind": "document",
                    "name": format!("Document{i}"),
                    "description": format!("{MARKER} decode-time attention paper variant {i}"),
                }),
            )
            .await
            .expect("create kg document");
        }

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let entities = resp["data"]["entities"]
            .as_array()
            .expect("entities array must be present — 8 matching entities were seeded");
        assert!(
            entities.len() <= 5,
            "blended entities must be capped at KG_BLEND_CAP=5, got {}: {entities:?}",
            entities.len()
        );
        let ids: HashSet<&str> = entities.iter().filter_map(|e| e["id"].as_str()).collect();
        assert_eq!(
            ids.len(),
            entities.len(),
            "blended entities must be deduplicated by id, got: {entities:?}"
        );
    }

    /// Runs an explicit-domain compose with a dispatch-accounting context armed and
    /// returns the response with the counters that dispatch recorded. Explicit domains
    /// skip the suggest phase, so the KG blend is the only vector search in the call.
    async fn compose_counting_usage(f: &Fixture, domain_id: &str) -> (Value, Value) {
        let usage = khive_runtime::usage::UsageContext::new();
        let resp = khive_runtime::usage::scope(
            usage.clone(),
            f.dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY }),
            ),
        )
        .await
        .expect("explicit domain_ids compose ok");
        (resp, usage.snapshot())
    }

    /// The blend finds `concept` and `document` entities with one vector query shared
    /// by both kinds, so the vector store is queried once for the call rather than once
    /// per kind, and both kinds still come back.
    #[tokio::test]
    async fn kg_blend_searches_both_kinds_in_one_vector_pass() {
        let f = pack(rt_with_marker_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        let concept_id = seed_kg_concept(&f).await;
        let document_id = seed_kg_document(&f).await;

        let (resp, usage) = compose_counting_usage(&f, &domain_id).await;

        assert_eq!(
            usage["vector_passes"], 1,
            "one blend search over both kinds must run one vector pass, got: {usage}"
        );
        let entities = resp["data"]["entities"]
            .as_array()
            .expect("entities array must be present when both kinds blend");
        let ids: HashSet<&str> = entities.iter().filter_map(|e| e["id"].as_str()).collect();
        assert_eq!(
            ids,
            HashSet::from([concept_id.as_str(), document_id.as_str()]),
            "both seeded kinds must be blended, got: {entities:?}"
        );
    }

    /// With only one blend kind present, the shared vector query returns that kind's
    /// entity and the call still runs one vector pass.
    #[tokio::test]
    async fn kg_blend_with_one_kind_present_returns_it_in_one_vector_pass() {
        for kind in ["concept", "document"] {
            let f = pack(rt_with_marker_embedder());
            let domain_id = seed_domain_and_atom(&f).await;
            let seeded = if kind == "concept" {
                seed_kg_concept(&f).await
            } else {
                seed_kg_document(&f).await
            };

            let (resp, usage) = compose_counting_usage(&f, &domain_id).await;

            assert_eq!(
                usage["vector_passes"], 1,
                "a {kind}-only blend must run one vector pass, got: {usage}"
            );
            let entities = resp["data"]["entities"]
                .as_array()
                .expect("entities array must be present when one kind blends");
            let ids: Vec<&str> = entities.iter().filter_map(|e| e["id"].as_str()).collect();
            assert_eq!(
                ids,
                [seeded.as_str()],
                "a {kind}-only blend must return exactly the seeded entity, got: {entities:?}"
            );
        }
    }

    /// The shared vector query leaves each kind with the list a per-kind search returns,
    /// also when one kind has more matches than its budget.
    ///
    /// Scenario: with a limit of 2, six concepts match both the text query and the vector,
    /// so the concept list is cut to its budget. Two documents match only by vector. The
    /// oracle is the per-kind `hybrid_search` call the blend made before it shared the
    /// vector query.
    #[tokio::test]
    async fn kg_blend_each_kind_matches_the_per_kind_search_when_a_kind_is_over_budget() {
        const LIMIT: u32 = 2;
        let rt = rt_with_marker_embedder();
        let f = pack(rt.clone());
        for i in 0..6 {
            f.dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("Concept{i}"),
                    "description": format!("{MARKER} paged cache variant {i}"),
                }),
            )
            .await
            .expect("create kg concept");
        }
        for i in 0..2 {
            f.dispatch(
                "create",
                json!({
                    "kind": "document",
                    "name": format!("Document{i}"),
                    "description": format!("{MARKER} decode attention paper {i}"),
                }),
            )
            .await
            .expect("create kg document");
        }
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let mut marker_vector = vec![0.0f32; DIM];
        marker_vector[0] = 1.0;
        let kinds = ["concept", "document"];

        let mut expected: Vec<Vec<String>> = Vec::new();
        for kind in kinds {
            let hits = rt
                .hybrid_search(
                    &token,
                    "paged",
                    Some(marker_vector.clone()),
                    LIMIT,
                    Some(kind),
                    None,
                    &[],
                    None,
                )
                .await
                .expect("per-kind search");
            expected.push(hits.iter().map(|hit| hit.entity_id.to_string()).collect());
        }
        assert_eq!(
            expected[0].len(),
            LIMIT as usize,
            "premise: the concept list is cut to its budget"
        );
        assert_eq!(
            expected[1].len(),
            2,
            "premise: the document list is not empty"
        );

        let per_kind = rt
            .hybrid_search_each_kind(&token, "paged", Some(marker_vector), LIMIT, &kinds)
            .await
            .expect("shared-vector search");
        let mut actual: Vec<Vec<String>> = Vec::new();
        for hits in &per_kind {
            actual.push(hits.iter().map(|hit| hit.entity_id.to_string()).collect());
        }
        assert_eq!(
            actual, expected,
            "each kind must get the ids and order its own per-kind search returns"
        );
    }

    /// Test 8: zero-atom edge case (ADR-051 Amendment 1 Decision) — when the
    /// final compose body ends up with zero atoms (everything trimmed by
    /// `max_tokens`), the entity inclusion floor is undefined, so no
    /// entities blend even though the leftover budget would easily fit one.
    #[tokio::test]
    async fn zero_atoms_in_final_body_blends_no_entities_even_with_budget_room() {
        let f = pack(rt_with_marker_embedder());

        // Oversized atom: alone it exceeds the max_tokens=500 (2000-char)
        // budget, so it is excluded from the body entirely — body_used stays
        // 0, leaving the *entire* budget as "remaining" for the entity trim.
        let filler = "x".repeat(2500);
        let big_content = format!("{OVERLAP_CONTENT} {filler}");
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": big_content, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 500 }),
            )
            .await
            .expect("compose ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "zero-atom final body must blend no entities even with room in the budget, got: {data}"
        );
    }

    // ── degrade-not-abort ───────────────────────────────────────────────────

    /// Embedder that fails exactly the single-text `embed_query` calls the KG
    /// blend path's `hybrid_search` → `vector_search` makes for the literal
    /// compose query text, while behaving like `MarkerEmbedService` for every
    /// other call (batched atom rerank, entity setup during seeding). This
    /// isolates the failure to the blend boundary without breaking fixture
    /// setup or the atom-scoring path, which only ever calls `embed` with
    /// batches of 2+ texts (query + candidates).
    struct FailingBlendEmbedService;

    const CANCEL_QUERY: &str = "zzzquantumfoo cancellation boundary kv cache paging decode \
        attention retrieval augmented generation dense sparse benchmark corpus";

    static CANCEL_ON_BLEND_FAILURE: std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>> =
        std::sync::Mutex::new(None);

    #[async_trait]
    impl EmbeddingService for FailingBlendEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            if texts.len() == 1 && (texts[0] == QUERY || texts[0] == CANCEL_QUERY) {
                if texts[0] == CANCEL_QUERY {
                    if let Some(cancel) = CANCEL_ON_BLEND_FAILURE
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                    {
                        let _ = cancel.send(true);
                    }
                }
                return Err(EmbedError::Internal(
                    "simulated KG blend-path embed failure".into(),
                ));
            }
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; DIM];
                    if t.contains(MARKER) {
                        v[0] = 1.0;
                    } else {
                        v[1] = 1.0;
                    }
                    v
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "failing-blend-embed-service"
        }
    }

    struct FailingBlendEmbedProvider;

    #[async_trait]
    impl EmbedderProvider for FailingBlendEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(FailingBlendEmbedService))
        }
    }

    fn rt_with_failing_blend_embedder() -> KhiveRuntime {
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
        rt.register_embedder(FailingBlendEmbedProvider);
        rt
    }

    /// Test 9 (High finding regression): a KG-blend-path embed failure
    /// (`hybrid_search` → `vector_search` → `embed_query` erroring) must not
    /// abort `knowledge.compose` — it must degrade to the already-finalized
    /// atom-only response. Uses explicit `domain_ids` (not AUTO) so the
    /// `knowledge.suggest` phase never calls `embed_query` on `QUERY` itself,
    /// keeping the induced failure isolated to the KG blend call.
    #[tokio::test]
    async fn kg_blend_failure_degrades_to_atom_only_response() {
        let f = pack(rt_with_failing_blend_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY }),
            )
            .await
            .expect("compose must succeed atom-only despite KG blend failure");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(
            !atoms.is_empty(),
            "atom-only body must survive a KG blend failure, got: {data}"
        );
        assert!(
            data.get("entities").is_none(),
            "entities must be absent when the KG blend fails, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "markdown must not contain a Knowledge graph section when the blend fails, got: {md}"
        );
    }

    #[tokio::test]
    async fn cancellation_during_kg_blend_failure_never_degrades_to_success() {
        let f = pack(rt_with_failing_blend_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        *CANCEL_ON_BLEND_FAILURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel_tx);

        let result = khive_storage::scope_request_read_cancellation(
            cancel_rx,
            f.dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": CANCEL_QUERY }),
            ),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(khive_runtime::RuntimeError::Storage(
                    khive_storage::StorageError::Timeout { .. }
                ))
            ),
            "request cancellation at the KG-blend catch must propagate, got {result:?}"
        );
    }
}

include!("integration_tests/private_merge_admission_tests.rs");

include!("integration_tests/merge_error_and_success_tests.rs");

include!("integration_tests/resolver_error_tests.rs");

include!("integration_tests/properties_only_payload_tests.rs");

include!("integration_tests/properties_only_validation_tests.rs");

include!("integration_tests/properties_only_batch_tests.rs");

include!("support/project_origin.rs");

#[path = "integration/refusal_events.rs"]
mod refusal_events;

#[path = "../src/knowledge/cursor_query.rs"]
mod cursor_query;

#[tokio::test]
async fn cursor_indexes_cover_order_and_walk_terminates() {
    use khive_db::{migrations::MIGRATIONS, StorageBackend};
    use khive_runtime::RuntimeConfig;
    use std::collections::HashSet;

    for migrated in [false, true] {
        let backend = Arc::new(StorageBackend::memory().unwrap());
        if migrated {
            backend
                .pool()
                .writer()
                .unwrap()
                .autocommit(|conn| {
                    conn.execute_batch(include_str!(
                        "../../khive-db/sql/schema-migrations-table.sql"
                    ))
                    .unwrap();
                    // Rebuild the actual parent migration prefix, without the new indexes.
                    for migration in MIGRATIONS.iter().filter(|m| m.version <= 38) {
                        conn.execute_batch(migration.up).unwrap();
                        conn.execute(
                    "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                    rusqlite::params![migration.version, migration.name],
                )
                .unwrap();
                    }
                    Ok(())
                })
                .unwrap();
        } else {
            backend.prepare_core_schema().unwrap();
        }
        {
            backend
                .pool()
                .writer()
                .unwrap()
                .transaction(|conn| {
                    for (kind, table) in [(0, "knowledge_atoms"), (1, "knowledge_domains")] {
                        for (ns_number, namespace) in [(0, "local"), (1, "foreign")] {
                            for n in (1..=1020).rev() {
                                // 1000 live rows, ten tombstones, ten atom-only domain mirrors.
                                if kind == 1 && n > 1010 {
                                    continue;
                                }
                                let id = format!("{kind:08x}-{ns_number:04x}-4000-8000-{n:012x}");
                                let tags = if n > 1010 { "[\"type:domain\"]" } else { "[]" };
                                conn.execute(
                                    &format!(
                                        "INSERT INTO {table} \
                                (id, namespace, slug, name, tags, created_at, \
                                updated_at, deleted_at) \
                                VALUES (?1, ?2, ?3, 'cursor row', ?4, ?5, ?5, ?6)"
                                    ),
                                    rusqlite::params![
                                        id,
                                        namespace,
                                        format!("row-{n}"),
                                        tags,
                                        n / 25,
                                        if (1001..=1010).contains(&n) {
                                            Some(1)
                                        } else {
                                            None
                                        }
                                    ],
                                )
                                .unwrap();
                            }
                        }
                    }
                    Ok(())
                })
                .unwrap();
        }
        if migrated {
            let writer = backend.pool().writer().unwrap();
            let count: i64 = writer
                .autocommit(|conn| {
                    Ok(conn.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'idx_knowledge_%_cursor'",
                    [],
                    |row| row.get(0),
                )?)
                })
                .unwrap();
            assert_eq!(count, 0, "historical fixture must lack cursor indexes");
            backend.prepare_core_schema().unwrap();
        }
        let runtime = KhiveRuntime::from_backend(backend, RuntimeConfig::no_embeddings());
        let fixture = pack(runtime.clone());
        for (kind, list_type, table) in [
            (0, "atom", "knowledge_atoms"),
            (1, "domain", "knowledge_domains"),
        ] {
            let index = format!("idx_{table}_cursor");
            {
                let writer = runtime.backend().pool().writer().unwrap();
                writer
                    .autocommit(|conn| {
                        let columns: Vec<String> = conn
                            .prepare(&format!("PRAGMA index_info('{index}')"))
                            .unwrap()
                            .query_map([], |r| r.get(2))
                            .unwrap()
                            .collect::<Result<_, _>>()
                            .unwrap();
                        assert_eq!(
                            columns,
                            ["namespace", "created_at", "id"],
                            "cursor index must cover tie-breaker"
                        );
                        for subsequent in [false, true] {
                            let clause = if kind == 0 {
                                " AND (status IS NULL OR status != 'deprecated')"
                            } else {
                                ""
                            };
                            let sql =
                                cursor_query::cursor_query(table, "id, slug", subsequent, clause);
                            let mut params = vec![rusqlite::types::Value::Text("local".into())];
                            if subsequent {
                                params.push(rusqlite::types::Value::Integer(0));
                                params.push(rusqlite::types::Value::Text(format!(
                                    "{kind:08x}-0000-4000-8000-000000000001"
                                )));
                            }
                            params.push(rusqlite::types::Value::Integer(74));
                            let plan: Vec<String> = conn
                                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                                .unwrap()
                                .query_map(rusqlite::params_from_iter(params), |r| r.get(3))
                                .unwrap()
                                .collect::<Result<_, _>>()
                                .unwrap();
                            eprintln!(
                                "migrated={migrated} subsequent={subsequent} {table}: {plan:?}"
                            );
                            assert!(
                                plan.iter().any(|p| p.contains(&index)),
                                "cursor plan must use compound index: {plan:?}"
                            );
                            assert!(
                                !plan.iter().any(|p| p.contains("TEMP B-TREE")),
                                "cursor ordering must not sort: {plan:?}"
                            );
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            for (ns_number, namespace) in [(0, "local"), (1, "foreign")] {
                let expected: HashSet<String> = (1..=1000)
                    .map(|n| format!("{kind:08x}-{ns_number:04x}-4000-8000-{n:012x}"))
                    .collect();
                let mut seen = HashSet::new();
                let mut after = String::new();
                let mut terminated = false;
                for _ in 0..20 {
                    let page = fixture
                        .dispatch(
                            "knowledge.list",
                            json!({
                                "type": list_type, "namespace": namespace, "after": after,
                                "limit": 73, "fields": ["id", "slug"],
                            }),
                        )
                        .await
                        .unwrap();
                    for row in page["results"].as_array().unwrap() {
                        assert!(
                            seen.insert(row["id"].as_str().unwrap().to_string()),
                            "cursor walk must not repeat rows"
                        );
                    }
                    if page["next_after"].is_null() {
                        terminated = true;
                        break;
                    }
                    after = page["next_after"].as_str().unwrap().to_string();
                }
                assert!(
                    terminated,
                    "cursor walk must terminate with null next_after"
                );
                assert_eq!(
                    seen, expected,
                    "cursor walk must return exact live namespace set"
                );
            }
        }
    }
}

include!("integration_tests/cite_weight_tests.rs");
