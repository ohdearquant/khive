use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use khive_retrieval::{error::RetrievalError, hybrid::Reranker};
use khive_runtime::runtime::scope_request_embedder_exclusions;
use khive_runtime::{
    usage, EmbedderProvider, EmbeddingCosineReranker, EmbeddingRerankHit, KhiveRuntime, Namespace,
    RuntimeConfig, RuntimeError, RuntimeResult,
};
use khive_score::DeterministicScore;
use khive_storage::{StorageCapability, StorageError};
use khive_types::SubstrateKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use uuid::Uuid;

const FIRST: &str = "all-minilm-l6-v2";
const SECOND: &str = "rerank-second";
const DIMS: usize = 384;

struct Calls {
    builds: AtomicUsize,
    queries: Mutex<Vec<String>>,
    documents: AtomicUsize,
    output: Mutex<Vec<f32>>,
    fail: AtomicBool,
}

impl Calls {
    fn new(output: Vec<f32>) -> Self {
        Self {
            builds: AtomicUsize::new(0),
            queries: Mutex::new(Vec::new()),
            documents: AtomicUsize::new(0),
            output: Mutex::new(output),
            fail: AtomicBool::new(false),
        }
    }
}

struct Provider {
    name: &'static str,
    calls: Arc<Calls>,
}

struct Service(Arc<Calls>);

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(&self, _: &[String], _: EmbeddingModel) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.documents.fetch_add(1, Ordering::SeqCst);
        Err(EmbedError::Internal("unexpected document embedding".into()))
    }

    async fn embed_query(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.queries.lock().unwrap().extend_from_slice(texts);
        if self.0.fail.load(Ordering::SeqCst) {
            return Err(EmbedError::InferenceFailed(
                "reranker fixture failure".into(),
            ));
        }
        Ok(texts
            .iter()
            .map(|_| self.0.output.lock().unwrap().clone())
            .collect())
    }

    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "reranker-fixture"
    }
}

#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.calls.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Service(Arc::clone(&self.calls))))
    }
}

fn runtime() -> (Arc<KhiveRuntime>, Arc<Calls>, Arc<Calls>) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        engines: Some(Vec::new()),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        packs: Vec::new(),
        actor_id: Some("reranker-test".into()),
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let first = Arc::new(Calls::new(vector(1.0, 0.0)));
    let second = Arc::new(Calls::new(vector(0.0, 1.0)));
    for (name, calls) in [(FIRST, &first), (SECOND, &second)] {
        runtime
            .try_register_embedder(Provider {
                name,
                calls: Arc::clone(calls),
            })
            .unwrap();
    }
    (Arc::new(runtime), first, second)
}

fn vector(x: f32, y: f32) -> Vec<f32> {
    let mut output = vec![0.0; DIMS];
    output[0] = x;
    output[1] = y;
    output
}

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn candidate(value: u128, score: f64) -> (Uuid, DeterministicScore) {
    (id(value), DeterministicScore::from_f64(score))
}

fn reranker(runtime: &Arc<KhiveRuntime>, engine: &str) -> EmbeddingCosineReranker {
    EmbeddingCosineReranker::new(
        Arc::clone(runtime),
        runtime.authorize(Namespace::local()).unwrap(),
        engine,
    )
    .unwrap()
}

async fn insert(
    runtime: &KhiveRuntime,
    engine: &str,
    value: u128,
    kind: SubstrateKind,
    namespace: &str,
    embedding: Vec<f32>,
) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .vectors_for_model(&token, engine)
        .unwrap()
        .insert(id(value), kind, namespace, "content", vec![embedding])
        .await
        .unwrap();
}

fn ids(hits: &[EmbeddingRerankHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.id).collect()
}

#[tokio::test]
async fn actual_trait_scores_candidates_beyond_namespace_top_100() {
    let (runtime, first, second) = runtime();
    for (value, embedding) in [
        (1, vector(-1.0, 0.0)),
        (2, vector(0.0, 1.0)),
        (3, vector(1.0, 1.0)),
    ] {
        insert(
            &runtime,
            FIRST,
            value,
            SubstrateKind::Entity,
            "local",
            embedding,
        )
        .await;
    }
    for value in 100..205 {
        insert(
            &runtime,
            FIRST,
            value,
            SubstrateKind::Entity,
            "local",
            vector(1.0, 0.0),
        )
        .await;
    }
    let token = runtime.authorize(Namespace::local()).unwrap();
    let global = runtime
        .knn_in(&token, FIRST, vector(1.0, 0.0), 100)
        .await
        .unwrap();
    assert_eq!(global.len(), 100);
    assert!(global
        .iter()
        .all(|hit| ![id(1), id(2), id(3)].contains(&hit.subject_id)));
    let canonical = runtime
        .rerank_in(&token, FIRST, &vector(1.0, 0.0), &[id(1), id(2), id(3)], 3)
        .await
        .unwrap();
    let adapter = reranker(&runtime, FIRST);
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    let input = vec![candidate(1, 0.9), candidate(2, 0.8), candidate(3, 0.1)];
    let detailed = adapter
        .rerank_detailed("query", input.clone(), 20)
        .await
        .unwrap();
    assert_eq!(ids(&detailed), [id(3), id(2), id(1)]);
    assert!(detailed.iter().all(|hit| !hit.missing_vector));
    assert_eq!(detailed[1].score, DeterministicScore::ZERO);
    assert_eq!(detailed[2].score, DeterministicScore::from_f64(-1.0));
    let expected: Vec<_> = canonical
        .into_iter()
        .map(|hit| (hit.subject_id, hit.score))
        .collect();
    let trait_object: &dyn Reranker<Uuid> = &adapter;
    assert_eq!(
        trait_object.rerank("query", input, 20).await.unwrap(),
        expected
    );
    assert_eq!(
        detailed
            .iter()
            .map(|hit| (hit.id, hit.score))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(first.builds.load(Ordering::SeqCst), 1);
    assert_eq!(*first.queries.lock().unwrap(), ["query", "query"]);
    assert_eq!(first.documents.load(Ordering::SeqCst), 0);
    assert_eq!(second.builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_scores_are_exact_and_merged_before_the_limit() {
    let (runtime, _, _) = runtime();
    insert(
        &runtime,
        FIRST,
        1,
        SubstrateKind::Entity,
        "local",
        vector(0.0, 1.0),
    )
    .await;
    insert(
        &runtime,
        FIRST,
        2,
        SubstrateKind::Entity,
        "local",
        vector(-1.0, 0.0),
    )
    .await;
    let high = DeterministicScore::from_raw(1_234_567_891);
    let low = DeterministicScore::from_raw(-6_789_012_345);
    let input = vec![
        (id(9), low),
        candidate(2, 0.9),
        candidate(1, -2.0),
        (id(8), high),
    ];
    let adapter = reranker(&runtime, FIRST);
    let all = adapter
        .rerank_detailed("query", input.clone(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(ids(&all), [id(8), id(1), id(2), id(9)]);
    assert_eq!(all[0].score.to_raw(), high.to_raw());
    assert_eq!(all[3].score.to_raw(), low.to_raw());
    assert_eq!(
        all.iter().map(|hit| hit.missing_vector).collect::<Vec<_>>(),
        [true, false, false, true]
    );
    assert_eq!(
        adapter
            .rerank_detailed("query", input.clone(), 2)
            .await
            .unwrap(),
        all[..2]
    );
    assert_eq!(
        Reranker::rerank(&adapter, "query", input, 2).await.unwrap(),
        [(id(8), high), (id(1), DeterministicScore::ZERO)]
    );
}

#[tokio::test]
async fn equal_dimension_engines_bind_both_query_and_stored_vectors() {
    let (runtime, first, second) = runtime();
    for (engine, a, b) in [
        (FIRST, vector(1.0, 0.0), vector(0.0, -1.0)),
        (SECOND, vector(0.0, -1.0), vector(0.0, 1.0)),
    ] {
        insert(&runtime, engine, 1, SubstrateKind::Entity, "local", a).await;
        insert(&runtime, engine, 2, SubstrateKind::Entity, "local", b).await;
    }
    let named = reranker(&runtime, SECOND);
    let alias = reranker(&runtime, "all-MiniLM-L6-v2");
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    assert_eq!(second.builds.load(Ordering::SeqCst), 0);
    let input = vec![candidate(1, 0.9), candidate(2, 0.1)];
    let usage = usage::UsageContext::new();
    let result = usage::scope(
        usage.clone(),
        named.rerank_detailed("named query", input.clone(), 2),
    )
    .await
    .unwrap();
    assert_eq!(ids(&result), [id(2), id(1)]);
    assert_eq!(result[0].score, DeterministicScore::from_f64(1.0));
    assert_eq!(result[1].score, DeterministicScore::from_f64(-1.0));
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    assert_eq!(second.builds.load(Ordering::SeqCst), 1);
    assert_eq!(*second.queries.lock().unwrap(), ["named query"]);
    assert_eq!(second.documents.load(Ordering::SeqCst), 0);
    assert_eq!(usage.snapshot()["embed_calls"], 1);
    assert_eq!(
        ids(&alias
            .rerank_detailed("alias query", input, 2)
            .await
            .unwrap()),
        [id(1), id(2)]
    );
    assert_eq!(first.builds.load(Ordering::SeqCst), 1);
    assert_eq!(*first.queries.lock().unwrap(), ["alias query"]);
}

#[tokio::test]
async fn omissions_preserve_entries_without_cross_namespace_model_or_kind_reads() {
    let (runtime, _, _) = runtime();
    for (engine, value, kind, namespace) in [
        (FIRST, 1, SubstrateKind::Entity, "local"),
        (FIRST, 2, SubstrateKind::Entity, "foreign"),
        (FIRST, 3, SubstrateKind::Note, "local"),
        (SECOND, 4, SubstrateKind::Entity, "local"),
    ] {
        insert(&runtime, engine, value, kind, namespace, vector(1.0, 0.0)).await;
    }
    let input: Vec<_> = (1..=5).map(|value| candidate(value, -0.5)).collect();
    let all = reranker(&runtime, FIRST)
        .rerank_detailed("query", input.clone(), 5)
        .await
        .unwrap();
    assert_eq!(ids(&all), (1..=5).map(id).collect::<Vec<_>>());
    assert!(!all[0].missing_vector);
    assert_eq!(all[0].score, DeterministicScore::from_f64(1.0));
    for hit in &all[1..] {
        assert!(hit.missing_vector);
        assert_eq!(hit.score, DeterministicScore::from_f64(-0.5));
    }
    let foreign = EmbeddingCosineReranker::new(
        Arc::clone(&runtime),
        runtime
            .authorize(Namespace::parse("foreign").unwrap())
            .unwrap(),
        FIRST,
    )
    .unwrap();
    let other = foreign.rerank_detailed("query", input, 5).await.unwrap();
    assert_eq!(other[0].id, id(2));
    assert!(!other[0].missing_vector);
    assert_eq!(other[0].score, DeterministicScore::from_f64(1.0));
    assert!(other
        .iter()
        .filter(|hit| hit.id != id(2))
        .all(|hit| hit.missing_vector));
}

#[tokio::test]
async fn duplicate_entries_keep_individual_fallback_scores_and_uuid_ties() {
    let (runtime, _, _) = runtime();
    for value in [1, 2] {
        insert(
            &runtime,
            FIRST,
            value,
            SubstrateKind::Entity,
            "local",
            vector(0.0, 1.0),
        )
        .await;
    }
    let input = vec![
        candidate(2, 0.8),
        candidate(8, -0.5),
        candidate(1, 0.4),
        candidate(8, 0.5),
        candidate(2, -0.8),
        candidate(8, 0.5),
    ];
    let adapter = reranker(&runtime, FIRST);
    let detailed = adapter
        .rerank_detailed("query", input.clone(), 100)
        .await
        .unwrap();
    assert_eq!(ids(&detailed), [id(8), id(8), id(1), id(2), id(2), id(8)]);
    assert_eq!(detailed.len(), input.len());
    assert_eq!(
        detailed.iter().map(|hit| hit.score).collect::<Vec<_>>(),
        [0.5, 0.5, 0.0, 0.0, 0.0, -0.5].map(DeterministicScore::from_f64)
    );
    assert_eq!(
        detailed
            .iter()
            .map(|hit| hit.missing_vector)
            .collect::<Vec<_>>(),
        [true, true, false, false, false, true]
    );
    assert_eq!(
        adapter
            .rerank_detailed("query", input.clone(), 4)
            .await
            .unwrap(),
        detailed[..4]
    );
    let repeated = vec![candidate(2, -1.0); 450];
    let repeated_hits = adapter
        .rerank_detailed("query", repeated, 500)
        .await
        .unwrap();
    assert_eq!(repeated_hits.len(), 450);
    assert!(repeated_hits.iter().all(|hit| hit.id == id(2)
        && hit.score == DeterministicScore::ZERO
        && !hit.missing_vector));
    assert!(adapter
        .rerank_detailed("query", input, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(adapter
        .rerank_detailed("query", Vec::new(), 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn unknown_and_excluded_engines_refuse_without_loading_or_fallback() {
    let (runtime, first, second) = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    assert!(
        matches!(EmbeddingCosineReranker::new(Arc::clone(&runtime), token.clone(), "unknown"), Err(RuntimeError::UnknownModel(name)) if name == "unknown")
    );
    let adapter = reranker(&runtime, FIRST);
    scope_request_embedder_exclusions(vec![FIRST.into()], async {
        assert!(matches!(EmbeddingCosineReranker::new(Arc::clone(&runtime), token, "all-MiniLM-L6-v2"), Err(RuntimeError::UnknownModel(_))));
        for (input, limit) in [(Vec::new(), 1), (vec![candidate(1, 0.2)], 0)] {
            assert!(matches!(adapter.rerank_detailed("query", input, limit).await, Err(RuntimeError::UnknownModel(name)) if name == FIRST));
        }
    }).await;
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    assert_eq!(second.builds.load(Ordering::SeqCst), 0);
    assert!(adapter
        .rerank_detailed("query", Vec::new(), 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(first.builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_embeddings_fail_even_for_empty_candidates_or_zero_limit() {
    let (runtime, first, _) = runtime();
    let adapter = reranker(&runtime, FIRST);
    for output in [
        vec![],
        vec![1.0],
        vector(f32::NAN, 0.0),
        vector(f32::INFINITY, 0.0),
    ] {
        *first.output.lock().unwrap() = output;
        for (input, limit) in [
            (Vec::new(), 0),
            (Vec::new(), 1),
            (vec![candidate(9, 0.7)], 0),
            (vec![candidate(9, 0.7)], 1),
        ] {
            let error = adapter
                .rerank_detailed("query", input, limit)
                .await
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::Storage(StorageError::InvalidInput { capability: StorageCapability::Vectors, operation, message }) if operation == "score_candidates" && message.contains(FIRST))
            );
        }
    }
    *first.output.lock().unwrap() = vector(1.0, 0.0);
    let healthy = adapter
        .rerank_detailed("query", vec![candidate(9, 0.7)], 1)
        .await
        .unwrap();
    assert_eq!(healthy[0].score, DeterministicScore::from_f64(0.7));
    assert!(healthy[0].missing_vector);
}

#[tokio::test]
async fn embedding_failure_is_not_missing_and_trait_projection_is_explicit() {
    let (runtime, first, _) = runtime();
    let adapter = reranker(&runtime, FIRST);
    first.fail.store(true, Ordering::SeqCst);
    let input = vec![candidate(9, 0.7)];
    let detailed = adapter
        .rerank_detailed("query", input.clone(), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(&detailed, RuntimeError::Embedding(EmbedError::InferenceFailed(message)) if message == "reranker fixture failure")
    );
    let projected = Reranker::rerank(&adapter, "query", input.clone(), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(&projected, RetrievalError::Rerank(message) if message == &detailed.to_string())
    );
    assert!(projected.is_permanent());
    first.fail.store(false, Ordering::SeqCst);
    let healthy = adapter.rerank_detailed("query", input, 1).await.unwrap();
    assert!(healthy[0].missing_vector);
    assert_eq!(healthy[0].score, DeterministicScore::from_f64(0.7));
}

#[tokio::test]
async fn zero_query_keeps_the_canonical_scorers_success_or_failure() {
    let (runtime, first, _) = runtime();
    insert(
        &runtime,
        FIRST,
        1,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    let zero = vec![0.0; DIMS];
    *first.output.lock().unwrap() = zero.clone();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let canonical = runtime.rerank_in(&token, FIRST, &zero, &[id(1)], 1).await;
    let actual = reranker(&runtime, FIRST)
        .rerank_detailed("query", vec![candidate(1, 0.7)], 1)
        .await;
    match canonical {
        Ok(hits) => {
            assert_eq!(hits.len(), 1);
            let actual = actual.unwrap();
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].score, hits[0].score);
            assert!(!actual[0].missing_vector);
        }
        Err(error) => {
            assert!(matches!(&error, RuntimeError::Storage(_)));
            let actual = actual.unwrap_err();
            assert!(matches!(&actual, RuntimeError::Storage(_)));
            assert_eq!(actual.to_string(), error.to_string());
        }
    }
}
