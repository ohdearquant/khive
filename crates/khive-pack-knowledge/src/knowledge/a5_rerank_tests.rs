//! A5 stored-vector rerank behavior at the knowledge search boundary.

use super::{vamana, KnowledgeHandlers};
use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreInfo,
};
use khive_storage::{StorageError, VectorStore};
use khive_types::SubstrateKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MODEL: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
const ALPHA_CONTENT: &str = "shared lexical rerank evidence explains how an indexed atom participates in retrieval while its persisted vector determines the final semantic ordering across otherwise similar search candidates";
const BETA_CONTENT: &str = "shared lexical rerank evidence explains how another indexed atom participates in retrieval while its persisted vector determines the final semantic ordering across otherwise similar search candidates";
const DOMAIN_DESCRIPTION: &str = "shared lexical domain evidence explains how a curated domain mirror participates in retrieval while its persisted embedding supplies semantic ranking for related knowledge search requests";

fn axis(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; DIM];
    vector[index] = 1.0;
    vector
}

#[derive(Default)]
struct Calls {
    passages: Mutex<Vec<String>>,
    generic: AtomicUsize,
    queries: AtomicUsize,
    fail_query: AtomicBool,
}

struct RoleService(Arc<Calls>);

#[async_trait]
impl EmbeddingService for RoleService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.generic.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| axis(1)).collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.queries.fetch_add(texts.len(), Ordering::SeqCst);
        if self.0.fail_query.load(Ordering::SeqCst) {
            return Err(EmbedError::InferenceFailed(
                "controlled query failure".into(),
            ));
        }
        Ok(texts.iter().map(|_| axis(0)).collect())
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0
            .passages
            .lock()
            .expect("passage recorder")
            .extend(texts.iter().cloned());
        Ok(texts
            .iter()
            .map(|text| axis(usize::from(text.contains("Vector Beta"))))
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "a5-role-test"
    }
}

struct RoleProvider(Arc<Calls>);

#[async_trait]
impl EmbedderProvider for RoleProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(RoleService(self.0.clone())))
    }
}

fn runtime(calls: &Arc<Calls>) -> KhiveRuntime {
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
        packs: vec!["kg".into(), "knowledge".into()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    rt.register_embedder(RoleProvider(calls.clone()));
    rt
}

fn registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(crate::KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("knowledge registry");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn fixture() -> (KhiveRuntime, Arc<Calls>, Uuid, Uuid) {
    let calls = Arc::new(Calls::default());
    let rt = runtime(&calls);
    let registry = registry(&rt);
    let upsert = registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [
                {"slug": "vector-alpha", "name": "Vector Alpha", "content": ALPHA_CONTENT,
                 "tags": ["first", "tag-one"], "finalized": true},
                {"slug": "vector-beta", "name": "Vector Beta", "content": BETA_CONTENT,
                 "tags": ["second", "tag-two"], "finalized": true}
            ]}),
        )
        .await
        .expect("upsert atoms");
    assert_eq!(upsert["created"], 2, "upsert: {upsert}");
    let indexed = registry
        .dispatch("knowledge.index", json!({"rebuild_ann": false}))
        .await
        .expect("index atoms");
    assert_eq!(indexed["indexed"], 2, "index: {indexed}");
    let listed = registry
        .dispatch("knowledge.list", json!({"kind": "atom", "limit": 10}))
        .await
        .expect("list atom ids");
    let rows = listed["results"].as_array().expect("atom rows");
    let id = |slug: &str| -> Uuid {
        rows.iter()
            .find(|row| row["slug"] == slug)
            .and_then(|row| row["id"].as_str())
            .expect("seed atom id")
            .parse()
            .expect("uuid")
    };
    (rt, calls, id("vector-alpha"), id("vector-beta"))
}

async fn search(rt: &KhiveRuntime, rerank: bool) -> Value {
    let token = rt.authorize(Namespace::local()).expect("authorize");
    KnowledgeHandlers::search(
        rt,
        &token,
        json!({"query": "shared lexical rerank evidence", "kind": "atom", "limit": 2,
               "rerank": rerank, "rerank_alpha": 0.0}),
        &vamana::new_shared(),
    )
    .await
    .expect("knowledge search")
}

fn slugs(result: &Value) -> Vec<&str> {
    result["results"]
        .as_array()
        .expect("result rows")
        .iter()
        .map(|row| row["slug"].as_str().expect("slug"))
        .collect()
}

#[tokio::test]
async fn rerank_follows_mutated_stored_vectors_without_reembedding_candidates() {
    let (rt, calls, alpha, beta) = fixture().await;
    calls.passages.lock().expect("passage recorder").clear();
    let first = search(&rt, true).await;
    assert_eq!(slugs(&first), ["vector-alpha", "vector-beta"], "{first}");
    assert_eq!(
        first["rerank_provenance"]["stored_vector_lookup"],
        "supported"
    );
    assert_eq!(first["rerank_provenance"]["candidates"], 2);
    assert_eq!(first["rerank_provenance"]["from_stored"], 2);
    assert_eq!(first["rerank_provenance"]["embedded_fallback"], 0);
    assert!(first["results"]
        .as_array()
        .expect("result rows")
        .iter()
        .all(|hit| hit["score_provenance"]["embedding_rerank"] == true));
    assert!(calls.passages.lock().expect("passage recorder").is_empty());

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let store = rt
        .vectors_for_model(&token, rt.default_embedder_name())
        .expect("vector store");
    store
        .insert(
            alpha,
            SubstrateKind::Entity,
            "local",
            "knowledge.atom",
            vec![axis(1)],
        )
        .await
        .expect("replace alpha vector");
    store
        .insert(
            beta,
            SubstrateKind::Entity,
            "local",
            "knowledge.atom",
            vec![axis(0)],
        )
        .await
        .expect("replace beta vector");

    let second = search(&rt, true).await;
    assert_eq!(slugs(&second), ["vector-beta", "vector-alpha"], "{second}");
    assert_eq!(second["rerank_provenance"]["from_stored"], 2);
    assert_eq!(second["rerank_provenance"]["embedded_fallback"], 0);
    assert!(calls.passages.lock().expect("passage recorder").is_empty());
    assert_eq!(calls.generic.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_vector_falls_back_with_document_intent_and_index_text_order() {
    let (rt, calls, _alpha, beta) = fixture().await;
    let indexed_texts = calls.passages.lock().expect("passage recorder").clone();
    let indexed_beta = indexed_texts
        .iter()
        .find(|text| text.starts_with("Vector Beta\n\n"))
        .expect("index-time beta passage")
        .clone();
    assert_eq!(
        indexed_beta,
        format!("Vector Beta\n\n{BETA_CONTENT}\n\nTags: second, tag-two")
    );
    calls.passages.lock().expect("passage recorder").clear();

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let store = rt
        .vectors_for_model(&token, rt.default_embedder_name())
        .expect("vector store");
    assert!(store.delete(beta).await.expect("delete beta vector"));

    let result = search(&rt, true).await;
    assert_eq!(
        result["rerank_provenance"]["stored_vector_lookup"],
        "supported"
    );
    assert_eq!(result["rerank_provenance"]["candidates"], 2);
    assert_eq!(result["rerank_provenance"]["from_stored"], 1);
    assert_eq!(result["rerank_provenance"]["embedded_fallback"], 1);
    assert_eq!(
        *calls.passages.lock().expect("passage recorder"),
        vec![indexed_beta],
        "fallback must use the same tagged renderer as indexing"
    );
    assert_eq!(calls.generic.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn domain_mirror_uses_stored_vector_then_tagged_document_fallback() {
    let calls = Arc::new(Calls::default());
    let rt = runtime(&calls);
    let registry = registry(&rt);
    let upsert = registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "vector-domain",
                "name": "Vector Domain",
                "description": DOMAIN_DESCRIPTION,
                "tags": ["curated", "domain-tag"]
            }]}),
        )
        .await
        .expect("upsert domain and mirror");
    assert_eq!(upsert["created"], 1, "upsert: {upsert}");
    let indexed = registry
        .dispatch("knowledge.index", json!({"rebuild_ann": false}))
        .await
        .expect("index domain mirror");
    assert_eq!(indexed["indexed"], 1, "index: {indexed}");

    let listed = registry
        .dispatch("knowledge.list", json!({"kind": "domain", "limit": 1}))
        .await
        .expect("list canonical domain");
    let domain_id: Uuid = listed["results"][0]["id"]
        .as_str()
        .expect("domain id")
        .parse()
        .expect("uuid");
    let indexed_text =
        format!("Vector Domain\n\n{DOMAIN_DESCRIPTION}\n\nTags: curated, domain-tag, type:domain");
    assert_eq!(
        *calls.passages.lock().expect("passage recorder"),
        vec![indexed_text.clone()],
        "indexing must embed the domain mirror with its type tag"
    );
    calls.passages.lock().expect("passage recorder").clear();

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let ann = vamana::new_shared();
    let params = || {
        json!({"query": "shared lexical domain evidence", "kind": "domain",
               "limit": 1, "rerank": true, "rerank_alpha": 0.0})
    };
    let stored = KnowledgeHandlers::search(&rt, &token, params(), &ann)
        .await
        .expect("search indexed domain");
    assert_eq!(
        stored["results"][0]["id"],
        domain_id.to_string(),
        "{stored}"
    );
    assert_eq!(stored["results"][0]["kind"], "domain", "{stored}");
    assert_eq!(
        stored["rerank_provenance"]["stored_vector_lookup"],
        "supported"
    );
    assert_eq!(stored["rerank_provenance"]["candidates"], 1);
    assert_eq!(stored["rerank_provenance"]["from_stored"], 1);
    assert_eq!(stored["rerank_provenance"]["embedded_fallback"], 0);
    assert!(calls.passages.lock().expect("passage recorder").is_empty());

    let store = rt
        .vectors_for_model(&token, rt.default_embedder_name())
        .expect("vector store");
    assert!(store.delete(domain_id).await.expect("delete mirror vector"));
    let fallback = KnowledgeHandlers::search(&rt, &token, params(), &ann)
        .await
        .expect("search domain without stored vector");
    assert_eq!(
        fallback["results"][0]["id"],
        domain_id.to_string(),
        "{fallback}"
    );
    assert_eq!(fallback["rerank_provenance"]["candidates"], 1);
    assert_eq!(fallback["rerank_provenance"]["from_stored"], 0);
    assert_eq!(fallback["rerank_provenance"]["embedded_fallback"], 1);
    assert_eq!(
        *calls.passages.lock().expect("passage recorder"),
        vec![indexed_text],
        "a mirror domain missing its vector needs the indexed document-intent text"
    );
    assert_eq!(calls.generic.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provenance_is_absent_without_rerank_or_after_query_embedding_failure() {
    let (rt, calls, _alpha, _beta) = fixture().await;
    calls.passages.lock().expect("passage recorder").clear();

    let disabled = search(&rt, false).await;
    assert_eq!(disabled["total"], 2, "{disabled}");
    assert!(disabled.get("rerank_provenance").is_none(), "{disabled}");
    assert!(calls.passages.lock().expect("passage recorder").is_empty());

    calls.fail_query.store(true, Ordering::SeqCst);
    let queries_before_failure = calls.queries.load(Ordering::SeqCst);
    let failed = search(&rt, true).await;
    assert_eq!(failed["total"], 2, "{failed}");
    assert!(failed.get("rerank_provenance").is_none(), "{failed}");
    assert_eq!(
        calls.queries.load(Ordering::SeqCst),
        queries_before_failure + 1
    );
    assert!(calls.passages.lock().expect("passage recorder").is_empty());
}

// The runtime owns a concrete SQLite vector backend. This fake also feeds the
// search rerank's store-injection seam to assert unsupported provenance.
pub(super) struct NoReadStore;

#[async_trait]
impl VectorStore for NoReadStore {
    async fn insert(
        &self,
        _subject_id: Uuid,
        _kind: SubstrateKind,
        _namespace: &str,
        _field: &str,
        _vectors: Vec<Vec<f32>>,
    ) -> StorageResult<()> {
        unreachable!("unused")
    }

    async fn insert_batch(&self, _records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary> {
        unreachable!("unused")
    }

    async fn delete(&self, _subject_id: Uuid) -> StorageResult<bool> {
        unreachable!("unused")
    }

    async fn count(&self) -> StorageResult<u64> {
        unreachable!("unused")
    }

    async fn search(&self, _request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>> {
        unreachable!("unused")
    }

    async fn info(&self) -> StorageResult<VectorStoreInfo> {
        unreachable!("unused")
    }

    async fn rebuild(&self, _scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo> {
        unreachable!("unused")
    }
}

#[tokio::test]
async fn unsupported_vector_read_remains_an_explicit_capability() {
    let store = NoReadStore;
    assert!(!store.capabilities().supports_vector_read);
    let error = store
        .get_vectors(&[Uuid::new_v4()], "local", "knowledge.atom")
        .await
        .expect_err("default vector read is unsupported");
    assert!(matches!(
        error,
        StorageError::Unsupported { operation, .. } if operation == "get_vectors"
    ));
}
