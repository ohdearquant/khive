use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use khive_runtime::runtime::scope_request_embedder_exclusions;
use khive_runtime::{
    usage, EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, RuntimeResult,
};
use khive_storage::{StorageCapability, StorageError, VectorSearchHit};
use khive_types::SubstrateKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use uuid::Uuid;

const FIRST: &str = "all-minilm-l6-v2";
const SECOND: &str = "named-space-two";
const DIMS: usize = 384;

struct Calls {
    builds: AtomicUsize,
    texts: Mutex<Vec<String>>,
    output_dims: AtomicUsize,
}

impl Default for Calls {
    fn default() -> Self {
        Self {
            builds: AtomicUsize::new(0),
            texts: Mutex::new(Vec::new()),
            output_dims: AtomicUsize::new(DIMS),
        }
    }
}

struct Provider {
    name: &'static str,
    calls: Arc<Calls>,
}
struct Service(Arc<Calls>);

impl Service {
    fn vectors(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.0.texts.lock().unwrap().extend_from_slice(texts);
        texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; self.0.output_dims.load(Ordering::SeqCst)];
                if let Some(first) = vector.first_mut() {
                    *first = 1.0;
                }
                vector
            })
            .collect()
    }
}

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(self.vectors(texts))
    }
    async fn embed_query(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(self.vectors(texts))
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "named-space-fixture"
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

fn runtime(configured: bool) -> (KhiveRuntime, Arc<Calls>, Arc<Calls>) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        embedding_model: configured.then_some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: Vec::new(),
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
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(!runtime.backend().is_file_backed());
    let first = Arc::new(Calls::default());
    let second = Arc::new(Calls::default());
    runtime
        .try_register_embedder(Provider {
            name: FIRST,
            calls: Arc::clone(&first),
        })
        .unwrap();
    runtime
        .try_register_embedder(Provider {
            name: SECOND,
            calls: Arc::clone(&second),
        })
        .unwrap();
    (runtime, first, second)
}

fn vector(x: f32, y: f32) -> Vec<f32> {
    let mut result = vec![0.0; DIMS];
    result[0] = x;
    result[1] = y;
    result
}

async fn insert(
    runtime: &KhiveRuntime,
    engine: &str,
    id: u128,
    kind: SubstrateKind,
    namespace: &str,
    values: Vec<f32>,
) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .vectors_for_model(&token, engine)
        .unwrap()
        .insert(
            Uuid::from_u128(id),
            kind,
            namespace,
            "content",
            vec![values],
        )
        .await
        .unwrap();
}

fn ids(hits: &[VectorSearchHit]) -> Vec<u128> {
    hits.iter().map(|hit| hit.subject_id.as_u128()).collect()
}
fn id_set(hits: &[VectorSearchHit]) -> BTreeSet<u128> {
    ids(hits).into_iter().collect()
}
fn assert_no_builds(first: &Calls, second: &Calls) {
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    assert_eq!(second.builds.load(Ordering::SeqCst), 0);
    assert!(first.texts.lock().unwrap().is_empty());
    assert!(second.texts.lock().unwrap().is_empty());
}

fn invalid_vector(error: RuntimeError, engine: &str, operation: &str) {
    match error {
        RuntimeError::Storage(StorageError::InvalidInput {
            capability,
            operation: actual,
            message,
        }) => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(actual, operation);
            assert!(message.contains(engine), "{message}");
        }
        other => panic!("expected named vector validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn named_search_isolates_equal_dimension_spaces_namespaces_and_kinds() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    insert(
        &rt,
        FIRST,
        1,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    insert(
        &rt,
        SECOND,
        2,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    insert(
        &rt,
        SECOND,
        3,
        SubstrateKind::Note,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    insert(
        &rt,
        SECOND,
        4,
        SubstrateKind::Entity,
        "foreign",
        vector(1.0, 0.0),
    )
    .await;
    for (engine, expected) in [(FIRST, 1), (SECOND, 2)] {
        let result = rt
            .vector_search_in(
                &token,
                engine,
                Some(vector(1.0, 0.0)),
                None,
                10,
                Some(SubstrateKind::Entity),
            )
            .await
            .unwrap();
        assert_eq!(ids(&result), [expected]);
        assert_eq!(
            ids(&rt
                .knn_in(&token, engine, vector(1.0, 0.0), 10)
                .await
                .unwrap()),
            [expected]
        );
        assert_eq!(
            ids(&rt
                .rerank_in(
                    &token,
                    engine,
                    &vector(1.0, 0.0),
                    &[Uuid::from_u128(1), Uuid::from_u128(2)],
                    10
                )
                .await
                .unwrap()),
            [expected]
        );
    }
    let notes = rt
        .vector_search_in(
            &token,
            SECOND,
            Some(vector(1.0, 0.0)),
            None,
            10,
            Some(SubstrateKind::Note),
        )
        .await
        .unwrap();
    assert_eq!(ids(&notes), [3]);
    let both = rt
        .vector_search_in(&token, SECOND, Some(vector(1.0, 0.0)), None, 10, None)
        .await
        .unwrap();
    assert_eq!(id_set(&both), BTreeSet::from([2, 3]));
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn named_rerank_scores_only_candidates_and_limits_after_scoring() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    for id in [2, 3] {
        insert(
            &rt,
            SECOND,
            id,
            SubstrateKind::Entity,
            "local",
            vector(0.0, 1.0),
        )
        .await;
    }
    insert(
        &rt,
        SECOND,
        9,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    insert(
        &rt,
        FIRST,
        1,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    let query = vector(1.0, 0.0);
    assert_eq!(
        ids(&rt.knn_in(&token, SECOND, query.clone(), 1).await.unwrap()),
        [9]
    );
    let candidates = [3, 1, 2, 3, 8].map(Uuid::from_u128);
    let all = rt
        .rerank_in(&token, SECOND, &query, &candidates, 10)
        .await
        .unwrap();
    assert_eq!(ids(&all), [2, 3]);
    assert_eq!(all.iter().map(|hit| hit.rank).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(
        ids(&rt
            .rerank_in(&token, SECOND, &query, &candidates, 1)
            .await
            .unwrap()),
        [2]
    );
    assert!(rt
        .rerank_in(&token, SECOND, &query, &[], 0)
        .await
        .unwrap()
        .is_empty());
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn named_validation_refuses_unknown_spaces_and_invalid_vectors_even_for_empty_requests() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    for top_k in [0, 1] {
        for result in [
            rt.vector_search_in(&token, "unknown", Some(vector(1.0, 0.0)), None, top_k, None)
                .await,
            rt.knn_in(&token, "unknown", vector(1.0, 0.0), top_k).await,
            rt.rerank_in(&token, "unknown", &vector(1.0, 0.0), &[], top_k)
                .await,
        ] {
            assert!(matches!(result, Err(RuntimeError::UnknownModel(name)) if name == "unknown"));
        }
        for query in [
            vec![],
            vec![1.0],
            vector(f32::NAN, 0.0),
            vector(f32::INFINITY, 0.0),
        ] {
            invalid_vector(
                rt.vector_search_in(&token, SECOND, Some(query.clone()), None, top_k, None)
                    .await
                    .unwrap_err(),
                SECOND,
                "vec_search",
            );
            invalid_vector(
                rt.knn_in(&token, SECOND, query.clone(), top_k)
                    .await
                    .unwrap_err(),
                SECOND,
                "vec_search",
            );
            invalid_vector(
                rt.rerank_in(&token, SECOND, &query, &[], top_k)
                    .await
                    .unwrap_err(),
                SECOND,
                "score_candidates",
            );
        }
    }
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn compatibility_methods_and_builtin_alias_keep_the_configured_engine_and_usage() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    insert(
        &rt,
        FIRST,
        1,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    insert(
        &rt,
        SECOND,
        2,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    let query = vector(1.0, 0.0);
    let context = usage::UsageContext::new();
    usage::scope(context.clone(), async {
        let named = rt
            .vector_search_in(
                &token,
                FIRST,
                Some(query.clone()),
                Some(""),
                10,
                Some(SubstrateKind::Entity),
            )
            .await
            .unwrap();
        let default = rt
            .vector_search(
                &token,
                Some(query.clone()),
                None,
                10,
                Some(SubstrateKind::Entity),
            )
            .await
            .unwrap();
        assert_eq!(ids(&named), ids(&default));
        assert_eq!(ids(&default), [1]);
        assert_eq!(ids(&rt.knn(&token, query.clone(), 10).await.unwrap()), [1]);
        assert_eq!(
            ids(&rt
                .rerank(
                    &token,
                    &query,
                    &[Uuid::from_u128(1), Uuid::from_u128(2)],
                    10
                )
                .await
                .unwrap()),
            [1]
        );
    })
    .await;
    assert_eq!(context.snapshot()["vector_passes"], 2);
    assert!(context.snapshot().get("embed_calls").is_none());
    let alias = "all-MiniLM-L6-v2";
    assert_eq!(
        ids(&rt.knn_in(&token, alias, query, 10).await.unwrap()),
        [1]
    );
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn excluded_configured_engine_never_falls_back_to_the_second_engine() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    insert(
        &rt,
        SECOND,
        2,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    scope_request_embedder_exclusions(vec![FIRST.into()], async {
        let query = vector(1.0, 0.0);
        for result in [
            rt.vector_search(&token, Some(query.clone()), None, 10, None)
                .await,
            rt.knn(&token, query.clone(), 10).await,
            rt.rerank(&token, &query, &[], 0).await,
            rt.knn_in(&token, "all-MiniLM-L6-v2", query.clone(), 10)
                .await,
        ] {
            assert!(matches!(result, Err(RuntimeError::UnknownModel(_))));
        }
        assert_eq!(
            ids(&rt.knn_in(&token, SECOND, query, 10).await.unwrap()),
            [2]
        );
    })
    .await;
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn compatibility_keeps_missing_query_and_unconfigured_errors() {
    let (rt, first, second) = runtime(false);
    let token = rt.authorize(Namespace::local()).unwrap();
    for (text, expected) in [
        (None, "vector search requires query_embedding or query_text"),
        (Some(" "), "query_text must not be empty"),
    ] {
        let error = rt
            .vector_search(&token, None, text, 1, None)
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(message) if message == expected));
    }
    for result in [
        rt.vector_search(&token, Some(vector(1.0, 0.0)), None, 1, None)
            .await,
        rt.knn(&token, vector(1.0, 0.0), 1).await,
        rt.rerank(&token, &vector(1.0, 0.0), &[], 0).await,
    ] {
        assert!(
            matches!(result, Err(RuntimeError::Unconfigured(name)) if name == "embedding_model")
        );
    }
    assert_no_builds(&first, &second);
}

#[tokio::test]
async fn text_query_invokes_only_the_named_provider_and_checks_its_actual_output() {
    let (rt, first, second) = runtime(true);
    let token = rt.authorize(Namespace::local()).unwrap();
    insert(
        &rt,
        SECOND,
        2,
        SubstrateKind::Entity,
        "local",
        vector(1.0, 0.0),
    )
    .await;
    let context = usage::UsageContext::new();
    let hits = usage::scope(
        context.clone(),
        rt.vector_search_in(
            &token,
            SECOND,
            None,
            Some("named text"),
            10,
            Some(SubstrateKind::Entity),
        ),
    )
    .await
    .unwrap();
    assert_eq!(ids(&hits), [2]);
    assert_eq!(first.builds.load(Ordering::SeqCst), 0);
    assert!(first.texts.lock().unwrap().is_empty());
    assert_eq!(second.builds.load(Ordering::SeqCst), 1);
    assert_eq!(*second.texts.lock().unwrap(), ["named text"]);
    assert_eq!(context.snapshot()["embed_calls"], 1);
    assert_eq!(context.snapshot()["vector_passes"], 1);
    second.output_dims.store(3, Ordering::SeqCst);
    let invalid_context = usage::UsageContext::new();
    let error = usage::scope(
        invalid_context.clone(),
        rt.vector_search_in(&token, SECOND, None, Some("bad dimension"), 0, None),
    )
    .await
    .unwrap_err();
    invalid_vector(error, SECOND, "vec_search");
    assert_eq!(second.builds.load(Ordering::SeqCst), 1);
    assert_eq!(second.texts.lock().unwrap().len(), 2);
    assert_eq!(invalid_context.snapshot()["embed_calls"], 1);
    assert!(invalid_context.snapshot().get("vector_passes").is_none());
}
