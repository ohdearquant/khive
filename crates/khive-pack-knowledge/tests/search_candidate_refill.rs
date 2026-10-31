//! Regression coverage for knowledge-search candidate refill (#1763).
//!
//! These tests exercise the public verb surface against in-memory storage. The
//! large candidate sets are inserted with bounded recursive CTEs so the tests
//! cross the real 2,000-row candidate cap without thousands of dispatch calls.

use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
}

impl Fixture {
    fn new() -> Self {
        Self::with_runtime(KhiveRuntime::memory().expect("in-memory runtime"))
    }

    fn with_runtime(runtime: KhiveRuntime) -> Self {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new(runtime.clone()));
        let registry = builder.build().expect("registry builds");
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { registry, runtime }
    }

    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    async fn execute(&self, sql: &str) {
        let access = self.runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: sql.to_string(),
                params: Vec::<SqlValue>::new(),
                label: None,
            })
            .await
            .expect("fixture SQL executes");
    }
}

const MODEL_KEY: &str = "all-minilm-l6-v2";
const EMBEDDING_DIMENSIONS: usize = 384;

/// Deterministic vectors put each test's ineligible prefix ahead of its
/// eligible tail without relying on approximate-search tie behavior.
struct RefillVectorService;

fn refill_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; EMBEDDING_DIMENSIONS];
    if text.contains("ANN Deleted Leader") {
        vector[0] = 1.0;
    } else if text.contains("ANN Reviewed Prefix") || text.contains("ANN Draft") {
        vector[0] = 0.98;
        vector[1] = 0.2;
    } else if text.contains("ANN Deprecated Tail") || text.contains("ANN Reviewed") {
        vector[0] = 0.6;
        vector[1] = 0.8;
    } else {
        vector[0] = 1.0;
    }

    let fixture_ordinal = text
        .split_whitespace()
        .find_map(|word| word.parse::<usize>().ok())
        .unwrap_or(0);
    vector[2 + fixture_ordinal % (EMBEDDING_DIMENSIONS - 2)] = 0.01;
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    vector
}

#[async_trait]
impl EmbeddingService for RefillVectorService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|text| refill_vector(text)).collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; EMBEDDING_DIMENSIONS];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "candidate-refill-vector"
    }
}

struct RefillVectorProvider;

#[async_trait]
impl EmbedderProvider for RefillVectorProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        EMBEDDING_DIMENSIONS
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(RefillVectorService))
    }
}

fn runtime_with_embedder() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
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
    runtime
        .try_register_embedder(RefillVectorProvider)
        .expect("register deterministic refill provider before first resolution");
    runtime
}

fn result_slugs(response: &Value) -> Vec<&str> {
    response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .filter_map(|result| result["slug"].as_str())
        .collect()
}

#[tokio::test]
async fn candidate_cap_is_applied_after_status_and_kind_eligibility() {
    let fixture = Fixture::new();

    // Exactly fill the historical 2,000-row FTS window with drafts. Five
    // reviewed matches inserted afterward must refill the default-search pool
    // instead of disappearing behind those ineligible rows.
    fixture
        .execute(
            "WITH RECURSIVE x(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 49 \
             ), y(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < 39 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('10000000-0000-0000-0000-%012d', x.n * 40 + y.n), \
                 'local', \
                 printf('status-draft-%04d', x.n * 40 + y.n), \
                 printf('Status Draft %04d', x.n * 40 + y.n), \
                 'statusrefill candidate shared terms lexical retrieval ranking corpus', \
                 '[]', NULL, 0, 'draft', NULL, NULL, \
                 x.n * 40 + y.n, x.n * 40 + y.n, NULL \
             FROM x CROSS JOIN y",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 4 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('20000000-0000-0000-0000-%012d', v), \
                 'local', printf('status-reviewed-%02d', v), printf('Status Reviewed %02d', v), \
                 'statusrefill candidate shared terms lexical retrieval ranking corpus', \
                 '[]', NULL, 1, 'reviewed', NULL, NULL, 3000 + v, 3000 + v, NULL \
             FROM n",
        )
        .await;

    let status_response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "statusrefill candidate shared terms",
                "limit": 5,
                "rerank": false
            }),
        )
        .await
        .expect("status-refill search succeeds");
    let status_slugs = result_slugs(&status_response);
    assert_eq!(status_slugs.len(), 5, "response: {status_response}");
    assert!(
        status_slugs
            .iter()
            .all(|slug| slug.starts_with("status-reviewed-")),
        "draft rows must not consume candidate slots: {status_response}"
    );

    // Fill another 2,000-row window with reviewed non-domain atoms, then add
    // three reviewed domain mirrors. A domain search must cap after the kind
    // predicate and return all three mirrors.
    fixture
        .execute(
            "WITH RECURSIVE x(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 49 \
             ), y(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < 39 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('30000000-0000-0000-0000-%012d', x.n * 40 + y.n), \
                 'local', \
                 printf('type-atom-%04d', x.n * 40 + y.n), \
                 printf('Type Atom %04d', x.n * 40 + y.n), \
                 'typerefill candidate shared terms lexical retrieval ranking corpus', \
                 '[]', NULL, 1, 'reviewed', NULL, NULL, \
                 4000 + x.n * 40 + y.n, 4000 + x.n * 40 + y.n, NULL \
             FROM x CROSS JOIN y",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 2 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('40000000-0000-0000-0000-%012d', v), \
                 'local', printf('type-domain-%02d', v), printf('Type Domain %02d', v), \
                 'typerefill candidate shared terms lexical retrieval ranking corpus', \
                 '[\"type:domain\"]', NULL, 1, 'reviewed', NULL, NULL, \
                 7000 + v, 7000 + v, NULL \
             FROM n",
        )
        .await;

    let type_response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "typerefill candidate shared terms",
                "kind": "domain",
                "limit": 3,
                "rerank": false
            }),
        )
        .await
        .expect("kind-refill search succeeds");
    let type_slugs = result_slugs(&type_response);
    assert_eq!(type_slugs.len(), 3, "response: {type_response}");
    assert!(
        type_response["results"]
            .as_array()
            .expect("results")
            .iter()
            .all(|result| result["kind"] == "domain"),
        "non-domain rows must not consume candidate slots: {type_response}"
    );
}

#[tokio::test]
async fn explicit_exclude_status_controls_deprecated_fts_eligibility_before_cap() {
    let fixture = Fixture::new();

    fixture
        .execute(
            "WITH RECURSIVE x(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 49 \
             ), y(n) AS ( \
                 VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < 39 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('41000000-0000-0000-0000-%012d', x.n * 40 + y.n), \
                 'local', \
                 printf('zzz-explicit-exclude-deprecated-%04d', x.n * 40 + y.n), \
                 printf('Final Gate Deprecated %04d', x.n * 40 + y.n), \
                 'finalgatecap candidate shared terms lexical retrieval ranking corpus', \
                 '[]', NULL, 1, 'deprecated', NULL, NULL, \
                 x.n * 40 + y.n, x.n * 40 + y.n, NULL \
             FROM x CROSS JOIN y",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 4 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('42000000-0000-0000-0000-%012d', v), \
                 'local', printf('aaa-explicit-exclude-reviewed-%02d', v), \
                 printf('Final Gate Reviewed %02d', v), \
                 'finalgatecap candidate shared terms lexical retrieval ranking corpus', \
                 '[]', NULL, 1, 'reviewed', NULL, NULL, 3000 + v, 3000 + v, NULL \
             FROM n",
        )
        .await;

    let response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "finalgatecap candidate shared terms",
                "exclude_status": "reviewed",
                "limit": 5,
                "rerank": false
            }),
        )
        .await
        .expect("explicit-exclude FTS search succeeds");
    let results = response["results"].as_array().expect("results");
    assert_eq!(results.len(), 5, "response: {response}");
    assert!(
        results
            .iter()
            .all(|result| result["status"] == "deprecated"),
        "explicit exclude_status must admit deprecated rows through the shared final gate: {response}"
    );
}

#[tokio::test]
async fn lexical_candidate_recall_includes_non_contiguous_query_terms() {
    let fixture = Fixture::new();
    fixture
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "ordered-phrase",
                        "name": "Ordered Phrase",
                        "content": "orderedlex alpha beta candidate retrieval scoring corpus ranking semantic search index pipeline model evaluation production monitoring quality relevance precision recall",
                        "finalized": true
                    },
                    {
                        "slug": "separated-terms",
                        "name": "Separated Terms",
                        "content": "orderedlex alpha intervening beta candidate retrieval scoring corpus ranking semantic search index pipeline model evaluation production monitoring quality relevance precision recall",
                        "finalized": true
                    }
                ]
            }),
        )
        .await
        .expect("seed lexical candidates");

    let response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "orderedlex alpha beta",
                "limit": 10,
                "rerank": false
            }),
        )
        .await
        .expect("lexical search succeeds");
    let slugs = result_slugs(&response);
    assert!(slugs.contains(&"ordered-phrase"), "response: {response}");
    assert!(
        slugs.contains(&"separated-terms"),
        "a matching term gap must not remove a candidate before ranking: {response}"
    );
}

#[tokio::test]
async fn ann_refill_survives_fresh_tail_delete_from_full_prefix() {
    let fixture = Fixture::with_runtime(runtime_with_embedder());

    // The first ANN request for limit=5 is k=20. A deleted leader and 24
    // draft vectors precede five reviewed atoms. The fresh delete shrinks the
    // merged first prefix to 19 without exhausting the underlying ANN index;
    // reviewed results require widening despite that post-tail shortfall.
    fixture
        .execute(
            "INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) VALUES ( \
                 '50000000-0000-0000-0000-000000000000', 'local', \
                 'ann-deleted-leader', 'ANN Deleted Leader', \
                 'annstatus blocker lexical query fresh deletion candidate prefix', \
                 '[]', NULL, 0, 'draft', NULL, NULL, 0, 0, NULL \
             )",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 23 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('50100000-0000-0000-0000-%012d', v), \
                 'local', printf('ann-draft-%02d', v), printf('ANN Draft %02d', v), \
                 'annstatus blocker lexical query ineligible prefix candidate', \
                 '[]', NULL, 0, 'draft', NULL, NULL, 100 + v, 100 + v, NULL \
             FROM n",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 4 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('60000000-0000-0000-0000-%012d', v), \
                 'local', printf('ann-reviewed-%02d', v), printf('ANN Reviewed %02d', v), \
                 'opaque semantic vector document with no shared candidate vocabulary', \
                 '[]', NULL, 1, 'reviewed', NULL, NULL, 200 + v, 200 + v, NULL \
             FROM n",
        )
        .await;

    let index = fixture
        .dispatch("knowledge.index", json!({"rebuild_ann": true}))
        .await
        .expect("ANN index builds");
    assert_eq!(index["indexed"], 30, "index response: {index}");

    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("authorize vector deletion");
    let deleted = fixture
        .runtime
        .vectors(&token)
        .expect("vector store")
        .delete(Uuid::parse_str("50000000-0000-0000-0000-000000000000").expect("uuid"))
        .await
        .expect("delete leading vector");
    assert!(deleted, "fixture leader vector must exist");
    fixture
        .execute(
            "UPDATE knowledge_atoms SET deleted_at = 1 \
             WHERE id = '50000000-0000-0000-0000-000000000000'",
        )
        .await;

    let response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "annstatus blocker lexical query",
                "status": "reviewed",
                "limit": 5,
                "rerank": false
            }),
        )
        .await
        .expect("ANN refill search succeeds");
    let results = response["results"].as_array().expect("results");
    assert_eq!(results.len(), 5, "response: {response}");
    assert!(
        results.iter().all(|result| result["status"] == "reviewed"),
        "explicit status must gate ANN candidates exactly: {response}"
    );
    assert!(
        results.iter().all(|result| result["slug"]
            .as_str()
            .is_some_and(|slug| slug.starts_with("ann-reviewed-"))),
        "eligible ANN candidates beyond the initial top-k must refill the pool: {response}"
    );
}

#[tokio::test]
async fn explicit_exclude_status_controls_deprecated_ann_refill_and_final_gate() {
    let fixture = Fixture::with_runtime(runtime_with_embedder());
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 24 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('61000000-0000-0000-0000-%012d', v), \
                 'local', printf('ann-reviewed-prefix-%02d', v), \
                 printf('ANN Reviewed Prefix %02d', v), \
                 'annexclude blocker lexical query reviewed candidate prefix', \
                 '[]', NULL, 1, 'reviewed', NULL, NULL, v, v, NULL \
             FROM n",
        )
        .await;
    fixture
        .execute(
            "WITH RECURSIVE n(v) AS ( \
                 VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 4 \
             ) \
             INSERT INTO knowledge_atoms ( \
                 id, namespace, slug, name, content, tags, properties, finalized, status, \
                 source_uri, source_type, created_at, updated_at, deleted_at \
             ) \
             SELECT \
                 printf('62000000-0000-0000-0000-%012d', v), \
                 'local', printf('ann-deprecated-tail-%02d', v), \
                 printf('ANN Deprecated Tail %02d', v), \
                 'opaque semantic vector document without shared query vocabulary', \
                 '[]', NULL, 1, 'deprecated', NULL, NULL, 100 + v, 100 + v, NULL \
             FROM n",
        )
        .await;
    let index = fixture
        .dispatch("knowledge.index", json!({"rebuild_ann": true}))
        .await
        .expect("ANN index builds");
    assert_eq!(index["indexed"], 30, "index response: {index}");

    let response = fixture
        .dispatch(
            "knowledge.search",
            json!({
                "query": "annexclude blocker lexical query",
                "exclude_status": "reviewed",
                "limit": 5,
                "rerank": false
            }),
        )
        .await
        .expect("ANN explicit-exclude refill succeeds");
    let results = response["results"].as_array().expect("results");
    assert_eq!(results.len(), 5, "response: {response}");
    assert!(
        results
            .iter()
            .all(|result| result["status"] == "deprecated"),
        "deprecated ANN rows admitted by explicit exclude_status must refill and survive scoring: {response}"
    );
}

#[tokio::test]
async fn auto_compose_propagates_suggest_hydration_degradation() {
    let fixture = Fixture::with_runtime(runtime_with_embedder());
    fixture
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "stale-compose-domain",
                        "name": "Stale Compose Domain",
                        "description": "semantic retrieval ranking fusion candidate hydration degradation propagation for automatic composition across vector search and canonical storage boundaries in production systems",
                        "members": []
                    },
                    {
                        "slug": "live-empty-compose-domain",
                        "name": "Live Empty Compose Domain",
                        "description": "semantic retrieval ranking fusion candidate hydration degradation propagation for automatic composition across vector search and canonical storage boundaries in production systems",
                        "members": []
                    }
                ]
            }),
        )
        .await
        .expect("seed stale and live empty domains");
    let index = fixture
        .dispatch("knowledge.index", json!({"rebuild_ann": true}))
        .await
        .expect("ANN index builds");
    assert_eq!(index["indexed"], 2, "index response: {index}");

    // Keep the indexed ID live and local, but leave its required hydration slug empty.
    fixture
        .execute("UPDATE knowledge_atoms SET slug = '' WHERE slug = 'stale-compose-domain'")
        .await;
    fixture
        .execute("UPDATE knowledge_domains SET slug = '' WHERE slug = 'stale-compose-domain'")
        .await;
    {
        let access = fixture.runtime.sql();
        let mut reader = access.reader().await.expect("reader");
        let live_rows = reader.query_scalar(SqlStatement {
            sql: "SELECT (SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = 'local' AND slug = '' AND deleted_at IS NULL) + (SELECT COUNT(*) FROM knowledge_domains WHERE namespace = 'local' AND slug = '' AND deleted_at IS NULL)".into(),
            params: Vec::new(), label: None,
        }).await.expect("read back live local unhydratable rows");
        assert!(
            matches!(&live_rows, Some(SqlValue::Integer(2))),
            "both indexed canonical rows remain live and local: {live_rows:?}"
        );
    }

    let response = fixture
        .dispatch(
            "knowledge.compose",
            json!({
                "query": "explain semantic retrieval ranking fusion candidate hydration degradation propagation across automatic knowledge composition"
            }),
        )
        .await
        .expect("auto-compose returns a degraded response");
    assert_eq!(
        response["data"]["degraded"]["hydration_failures"], 1,
        "compose must preserve internal suggest degradation: {response}"
    );
    // Auto-compose skips the zero-size domain served with no live members.
    assert_eq!(
        response["data"]["markdown"],
        "# Knowledge Briefing\n\nNo matching domains found for auto-suggest.",
        "the zero-member candidate must be skipped before composition: {response}"
    );
    assert_eq!(response["data"]["count"], 0, "response: {response}");
}

use tracing::{field::Visit, instrument::WithSubscriber, span, Event, Metadata, Subscriber};

#[derive(Clone, Default)]
struct StaleAnnEvents(Arc<std::sync::Mutex<Vec<Value>>>);

#[derive(Default)]
struct StaleAnnFields(serde_json::Map<String, Value>);

impl Visit for StaleAnnFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
}

impl Subscriber for StaleAnnEvents {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = StaleAnnFields::default();
        event.record(&mut fields);
        if event.metadata().target() == "khive_pack_knowledge::knowledge::search"
            && event.metadata().level() == &tracing::Level::INFO
            && fields.0.contains_key("stale_ann_dropped")
        {
            self.0
                .lock()
                .expect("event capture")
                .push(Value::Object(fields.0));
        }
    }
}

async fn bounded_stale_fixture<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(30), future)
        .await
        .expect("stale ANN fixture exceeded its async deadline")
}

async fn capture_stale_request(fixture: &Fixture, verb: &str, args: Value) -> (Value, Vec<Value>) {
    let events = StaleAnnEvents::default();
    let response = fixture
        .dispatch(verb, args)
        .with_subscriber(events.clone())
        .await
        .expect("public knowledge request succeeds");
    // The request and its normal Drop event have completed inside WithSubscriber.
    let captured = events.0.lock().expect("event capture").clone();
    (response, captured)
}

fn assert_stale_event(events: &[Value], class: &str, expected: u64) {
    assert_eq!(
        events.len(),
        1,
        "one operator record per public request: {events:?}"
    );
    assert_eq!(
        events[0]["request_class"], class,
        "request class: {events:?}"
    );
    assert_eq!(
        events[0]["stale_ann_dropped"], expected,
        "unique stale count is local to one request: {events:?}"
    );
    let fields = events[0].as_object().expect("structured event");
    assert_eq!(
        fields.len(),
        3,
        "only class, count and fixed message: {events:?}"
    );
    assert_eq!(fields["message"], "stale ANN candidates dropped");
}

async fn stale_atom_fixture(hidden: bool) -> Fixture {
    let fixture = Fixture::with_runtime(runtime_with_embedder());
    fixture.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, \
         finalized, status, created_at, updated_at, deleted_at) VALUES \
         ('71000000-0000-0000-0000-000000000001', 'local', 'absent-leader', \
          'ANN Deleted Leader', 'opaque semantic vector source', '[]', NULL, 1, 'reviewed', 1, 1, NULL), \
         ('71000000-0000-0000-0000-000000000002', 'local', 'hidden-prefix', \
          'ANN Reviewed Prefix 1', 'hidden fixture payload', '[]', NULL, 1, 'reviewed', 2, 2, NULL), \
         ('71000000-0000-0000-0000-000000000003', 'local', 'live-tail', \
          'ANN Reviewed 2', 'opaque live vector source', '[]', NULL, 1, 'reviewed', 3, 3, NULL)"
    ).await;
    let index = fixture
        .dispatch("knowledge.index", json!({"rebuild_ann": true}))
        .await
        .expect("build actual ANN index");
    assert_eq!(index["indexed"], 3, "all three candidates must be indexed");
    // Mutate canonical rows only, leaving the serving ANN candidate intact.
    fixture
        .execute("DELETE FROM knowledge_atoms WHERE id = '71000000-0000-0000-0000-000000000001'")
        .await;
    if hidden {
        fixture.execute("UPDATE knowledge_atoms SET namespace = 'tenant-b' WHERE id = '71000000-0000-0000-0000-000000000002'").await;
    }
    fixture
}

const STALE_DOMAIN_QUERY: &str = "explain semantic retrieval ranking fusion candidate hydration degradation propagation across automatic knowledge composition";

async fn stale_domain_fixture(stale: bool) -> Fixture {
    let fixture = Fixture::with_runtime(runtime_with_embedder());
    fixture
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [
                {"slug": "operator-stale-domain", "name": "Operator Stale Domain",
                 "description": format!("{STALE_DOMAIN_QUERY} Additional background supplies ordinary context for readers across common topics and examples."), "members": []},
                {"slug": "operator-live-domain", "name": "Operator Live Domain",
                 "description": format!("{STALE_DOMAIN_QUERY} Additional background supplies ordinary context for readers across common topics and examples."), "members": []}
            ]}),
        )
        .await
        .expect("seed real domain records and mirrors");
    let index = fixture
        .dispatch("knowledge.index", json!({"rebuild_ann": true}))
        .await
        .expect("build actual domain ANN index");
    assert_eq!(index["indexed"], 2, "both domains must be indexed");
    if stale {
        fixture
            .execute("DELETE FROM knowledge_atoms WHERE slug = 'operator-stale-domain'")
            .await;
        fixture
            .execute("DELETE FROM knowledge_domains WHERE slug = 'operator-stale-domain'")
            .await;
    }
    fixture
}

#[tokio::test]
async fn absent_ann_id_is_dropped_without_hydration_failure() {
    bounded_stale_fixture(async {
        let fixture = stale_atom_fixture(false).await;
        let (response, events) = capture_stale_request(
            &fixture,
            "knowledge.search",
            json!({
                "query": "unmatched vector diagnostic query", "limit": 5, "rerank": false
            }),
        )
        .await;
        let slugs = result_slugs(&response);
        assert_eq!(
            slugs.len(),
            2,
            "live ANN candidates remain usable: {response}"
        );
        assert!(
            !slugs.contains(&"absent-leader"),
            "absent shells never return"
        );
        assert!(
            response.pointer("/degraded/hydration_failures").is_none(),
            "absent canonical ID is an index miss, not hydration failure: {response}"
        );
        assert_stale_event(&events, "knowledge.search", 1);
    })
    .await;
}

#[tokio::test]
async fn mixed_hidden_and_absent_ann_ids_are_index_misses() {
    bounded_stale_fixture(async {
        let fixture = stale_atom_fixture(true).await;
        let (response, events) = capture_stale_request(
            &fixture,
            "knowledge.search",
            json!({
                "query": "unmatched vector diagnostic query", "limit": 5, "rerank": false
            }),
        )
        .await;
        assert!(
            response.pointer("/degraded/hydration_failures").is_none(),
            "foreign and absent IDs are both index misses: {response}"
        );
        assert_eq!(
            result_slugs(&response),
            vec!["live-tail"],
            "only visible live rows return"
        );
        assert!(
            !response.to_string().contains("hidden fixture payload"),
            "foreign content stays private"
        );
        assert_stale_event(&events, "knowledge.search", 2);
    })
    .await;
}

#[tokio::test]
async fn stale_ann_operator_events_are_scoped_to_public_request_class() {
    bounded_stale_fixture(async {
        for (stale, expected) in [(true, 1), (false, 0)] {
            let fixture = stale_domain_fixture(stale).await;
            for verb in ["knowledge.search", "knowledge.suggest", "knowledge.compose"] {
                let args = match verb {
                    "knowledge.search" => {
                        json!({"query": STALE_DOMAIN_QUERY, "limit": 5, "rerank": false})
                    }
                    "knowledge.suggest" => json!({"query": STALE_DOMAIN_QUERY, "limit": 5}),
                    _ => json!({"query": STALE_DOMAIN_QUERY}),
                };
                let (response, events) = capture_stale_request(&fixture, verb, args).await;
                assert_stale_event(&events, verb, expected);
                assert!(
                    !response.to_string().contains("stale_ann_dropped"),
                    "operator accounting is not a response field: {response}"
                );
            }
        }
    })
    .await;
}

async fn assert_ann_refill_after_canonical_mutation(mutation: &str) {
    bounded_stale_fixture(async {
        let fixture = Fixture::with_runtime(runtime_with_embedder());

        let query = "annstatus blocker lexical query";
        let mut query_vector = vec![0.0; EMBEDDING_DIMENSIONS];
        query_vector[0] = 1.0;
        assert_eq!(
            fixture
                .runtime
                .embed_query(query)
                .await
                .expect("refill query probe"),
            query_vector,
            "refill fixture must use its deterministic provider"
        );
        // The current fixed cohort starts at k=100, containing the stale leader and drafts only.
        // Five reviewed records are later in the actual deterministic ANN order.
        // Canonical mutation leaves the stale ID in each widened serving prefix.
        fixture
            .execute(
                "INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, properties, finalized, status, \
                     source_uri, source_type, created_at, updated_at, deleted_at \
                 ) VALUES ( \
                     '50000000-0000-0000-0000-000000000000', 'local', \
                     'ann-deleted-leader', 'ANN Deleted Leader', \
                     'annstatus blocker lexical query fresh deletion candidate prefix', \
                     '[]', NULL, 0, 'draft', NULL, NULL, 0, 0, NULL \
                 )",
            )
            .await;
        fixture
            .execute(
                "WITH RECURSIVE n(v) AS ( \
                     VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 123 \
                 ) \
                 INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, properties, finalized, status, \
                     source_uri, source_type, created_at, updated_at, deleted_at \
                 ) \
                 SELECT \
                     printf('50100000-0000-0000-0000-%012d', v), \
                     'local', printf('ann-draft-%02d', v), printf('ANN Draft %02d', v), \
                     'annstatus blocker lexical query ineligible prefix candidate', \
                     '[]', NULL, 0, 'draft', NULL, NULL, 100 + v, 100 + v, NULL \
                 FROM n",
            )
            .await;
        fixture
            .execute(
                "WITH RECURSIVE n(v) AS ( \
                     VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 4 \
                 ) \
                 INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, properties, finalized, status, \
                     source_uri, source_type, created_at, updated_at, deleted_at \
                 ) \
                 SELECT \
                     printf('60000000-0000-0000-0000-%012d', v), \
                     'local', printf('ann-reviewed-%02d', v), printf('ANN Reviewed %02d', v), \
                     'opaque semantic vector document with no shared candidate vocabulary', \
                     '[]', NULL, 1, 'reviewed', NULL, NULL, 200 + v, 200 + v, NULL \
                 FROM n",
            )
            .await;

        let index = fixture
            .dispatch("knowledge.index", json!({"rebuild_ann": true}))
            .await
            .expect("ANN index builds");
        assert_eq!(index["indexed"], 130, "index response: {index}");

        fixture.execute(mutation).await;
        {
            let access = fixture.runtime.sql();
            let mut reader = access.reader().await.expect("reader");
            reader
                .query_all(SqlStatement {
                    sql: include_str!("../sql/knowledge_search_candidate_presence.sql").into(),
                    params: vec![
                        SqlValue::Text("local".into()),
                        SqlValue::Text("50000000-0000-0000-0000-000000000000".into()),
                    ],
                    label: None,
                })
                .await
                .expect(
                    "presence query must accept namespace and ID binds before semantic assertions",
                );
        }
        let (response, events) = capture_stale_request(
            &fixture,
            "knowledge.search",
            json!({
                "query": "annstatus blocker lexical query", "status": "reviewed", "limit": 5,
                "rerank": false
            }),
        )
        .await;
        let results = response["results"].as_array().expect("results");
        assert_eq!(
            results.len(),
            5,
            "eligible tail fills the requested result count: {response}"
        );
        assert!(
            results.iter().all(|row| row["status"] == "reviewed"),
            "eligibility remains intact"
        );
        assert!(
            results.iter().all(|row| row["slug"]
                .as_str()
                .is_some_and(|slug| slug.starts_with("ann-reviewed-"))),
            "the eligible ANN tail must refill"
        );
        assert!(
            response.pointer("/degraded/hydration_failures").is_none(),
            "refilled absent prefixes do not report hydration failure: {response}"
        );
        assert_stale_event(&events, "knowledge.search", 1);
    })
    .await;
}

#[tokio::test]
async fn ann_refill_skips_absent_prefix_without_hydration_degradation() {
    assert_ann_refill_after_canonical_mutation(
        "DELETE FROM knowledge_atoms WHERE id = '50000000-0000-0000-0000-000000000000'",
    )
    .await;
}

#[tokio::test]
async fn ann_refill_skips_soft_deleted_prefix_without_hydration_degradation() {
    assert_ann_refill_after_canonical_mutation("UPDATE knowledge_atoms SET deleted_at = 1 WHERE id = '50000000-0000-0000-0000-000000000000'").await;
}

#[tokio::test]
async fn ann_refill_skips_foreign_prefix_without_hydration_degradation() {
    assert_ann_refill_after_canonical_mutation("UPDATE knowledge_atoms SET namespace = 'tenant-b' WHERE id = '50000000-0000-0000-0000-000000000000'").await;
}
