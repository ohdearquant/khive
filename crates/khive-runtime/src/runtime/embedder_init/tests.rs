use super::*;
use crate::embedder_registry::EmbedderProvider;
use crate::runtime::{request_excludes_embedder, REQUEST_EMBEDDER_EXCLUSIONS};
use crate::usage::{scope, UsageContext};
use async_trait::async_trait;
use khive_storage::{EventFilter, PageRequest};
use khive_types::{OperationAttribution, RefResolution};
use lattice_embed::EmbedError;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

const COLD_MODEL: &str = "usage-cold-initializer";
const EXCLUDED_MODEL: &str = "excluded-from-request";

#[derive(Default)]
struct BuildBarrier {
    entered: AtomicBool,
    release: Notify,
    builds: AtomicUsize,
    calls: AtomicUsize,
    saw_exclusion: AtomicBool,
    operation: Mutex<Option<OperationAttribution>>,
}

struct ColdProvider(Arc<BuildBarrier>);

#[async_trait]
impl EmbedderProvider for ColdProvider {
    fn name(&self) -> &str {
        COLD_MODEL
    }
    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.0.builds.fetch_add(1, Ordering::SeqCst);
        self.0
            .saw_exclusion
            .store(request_excludes_embedder(EXCLUDED_MODEL), Ordering::SeqCst);
        *self.0.operation.lock().unwrap() =
            khive_storage::operation_context::current_operation_attribution();
        self.0.entered.store(true, Ordering::Release);
        self.0.release.notified().await;
        Ok(Arc::new(ConstantService(Arc::clone(&self.0))))
    }
}

struct ConstantService(Arc<BuildBarrier>);

#[async_trait]
impl EmbeddingService for ConstantService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0; 4]).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        COLD_MODEL
    }
}

struct FailureProvider(Arc<BuildBarrier>);

#[async_trait]
impl EmbedderProvider for FailureProvider {
    fn name(&self) -> &str {
        "failure-after-cold-build-starts"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(FailureService(Arc::clone(&self.0))))
    }
}

struct FailureService(Arc<BuildBarrier>);

#[async_trait]
impl EmbeddingService for FailureService {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        while !self.0.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        Err(EmbedError::InferenceFailed(
            "failure while sibling initialization is blocked".into(),
        ))
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "failure-after-cold-build-starts"
    }
}

fn exclusions() -> Arc<HashSet<String>> {
    Arc::new([EXCLUDED_MODEL.to_string()].into_iter().collect())
}

async fn initialization_events(runtime: &KhiveRuntime, token: &NamespaceToken) -> Vec<Event> {
    runtime
        .events(token)
        .unwrap()
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 20,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.verb == "embedder.init")
        .collect()
}

async fn cancelled_create_has_stable_usage(note: bool) {
    let runtime = KhiveRuntime::memory().unwrap();
    let barrier = Arc::new(BuildBarrier::default());
    runtime.register_embedder(ColdProvider(Arc::clone(&barrier)));
    runtime.register_embedder(FailureProvider(Arc::clone(&barrier)));
    let token = runtime
        .authorize(Namespace::parse("cold-init-request").unwrap())
        .unwrap();
    let context = UsageContext::new();
    let exclusions = exclusions();
    assert_eq!(Arc::strong_count(&exclusions), 1);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        REQUEST_EMBEDDER_EXCLUSIONS.scope(
            Arc::clone(&exclusions),
            scope(context.clone(), async {
                if note {
                    runtime
                        .create_note(
                            &token,
                            "observation",
                            None,
                            "cold initializer note",
                            None,
                            None,
                            vec![],
                        )
                        .await
                        .map(|_| ())
                } else {
                    runtime
                        .create_entity(
                            &token,
                            "concept",
                            None,
                            "cold initializer entity",
                            Some("nonempty embedding body"),
                            None,
                            vec![],
                        )
                        .await
                        .map(|_| ())
                }
            }),
        ),
    )
    .await
    .expect("create must fail before the blocked initializer is released");
    assert!(result.is_err());
    assert!(barrier.entered.load(Ordering::Acquire));
    assert_eq!(barrier.builds.load(Ordering::SeqCst), 1);
    assert!(
        Arc::strong_count(&exclusions) >= 2,
        "shared initializer must still own its exclusions scope"
    );
    let before = context.snapshot();
    assert_eq!(before["embed_calls"], 1);

    barrier.release.notify_one();
    // The existing exclusion scope owns this unique Arc until initialization
    // and its event append finish. Providers never retain it.
    tokio::time::timeout(Duration::from_secs(30), async {
        while Arc::strong_count(&exclusions) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached initialization and its event append must finish");
    let events = initialization_events(&runtime, &token).await;
    let cold: Vec<_> = events
        .iter()
        .filter(|event| event.payload["model_name"] == COLD_MODEL)
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(
        cold.len(),
        1,
        "the detached cold initializer must persist exactly one event"
    );
    assert_eq!(cold[0].namespace, token.namespace().as_str());
    assert_eq!(
        cold[0].actor,
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(cold[0].kind, EventKind::EmbedderInitialized);
    assert!(barrier.saw_exclusion.load(Ordering::SeqCst));
    assert_eq!(barrier.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        context.snapshot(),
        before,
        "detached initialization event must not change the completed create usage"
    );
    runtime.embedder(COLD_MODEL).await.unwrap();
    assert_eq!(barrier.builds.load(Ordering::SeqCst), 1);
    assert_eq!(initialization_events(&runtime, &token).await, events);
}

#[tokio::test]
async fn cancelled_note_cold_initialization_keeps_usage_stable() {
    cancelled_create_has_stable_usage(true).await;
}

#[tokio::test]
async fn cancelled_entity_cold_initialization_keeps_usage_stable() {
    cancelled_create_has_stable_usage(false).await;
}

#[tokio::test]
async fn joined_cold_initialization_preserves_embed_counts_and_event_provenance() {
    let runtime = KhiveRuntime::memory().unwrap();
    let barrier = Arc::new(BuildBarrier::default());
    barrier.release.notify_one();
    runtime.register_embedder(ColdProvider(Arc::clone(&barrier)));
    let token = runtime
        .authorize(Namespace::parse("joined-init-request").unwrap())
        .unwrap();
    let context = UsageContext::new();
    let operation = OperationAttribution {
        op_index: 7,
        ref_resolution: RefResolution::Resolved,
    };
    let exclusions = exclusions();
    let outcome = REQUEST_EMBEDDER_EXCLUSIONS
        .scope(
            Arc::clone(&exclusions),
            scope(
                context.clone(),
                khive_storage::operation_context::scope_operation_attribution(
                    operation,
                    runtime.embed_document_with_model_outcome_for_token(
                        &token,
                        COLD_MODEL,
                        "first document",
                    ),
                ),
            ),
        )
        .await
        .unwrap();
    assert_eq!(outcome.vector, vec![1.0; 4]);
    assert_eq!(
        context.snapshot(),
        serde_json::json!({"embed_calls": 1}),
        "shared initialization is excluded even when its caller waits"
    );
    assert_eq!(*barrier.operation.lock().unwrap(), Some(operation));
    assert!(barrier.saw_exclusion.load(Ordering::SeqCst));
    assert_eq!(Arc::strong_count(&exclusions), 1);
    let events = initialization_events(&runtime, &token).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["model_name"], COLD_MODEL);
    assert_eq!(events[0].namespace, token.namespace().as_str());
    assert_eq!(
        events[0].actor,
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(events[0].op_index, Some(operation.op_index));
    assert_eq!(events[0].ref_resolution, Some(operation.ref_resolution));
    scope(
        context.clone(),
        runtime.embed_document_with_model_outcome_for_token(&token, COLD_MODEL, "second document"),
    )
    .await
    .unwrap();
    assert_eq!(context.snapshot(), serde_json::json!({"embed_calls": 2}));
    assert_eq!(barrier.calls.load(Ordering::SeqCst), 2);
    assert_eq!(barrier.builds.load(Ordering::SeqCst), 1);
    assert_eq!(initialization_events(&runtime, &token).await, events);
}
