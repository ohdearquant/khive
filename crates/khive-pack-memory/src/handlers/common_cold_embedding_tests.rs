use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace};
use khive_storage::{EventFilter, PageRequest};
use khive_types::{EventKind, OperationAttribution, RefResolution};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tokio::sync::{mpsc, Semaphore};

use super::{collect_embed_results, embed_query_model, QueryEmbeddingCache, RuntimeError};

const MODEL: &str = "recall-cold-model";
const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

struct Service(Arc<AtomicUsize>);
#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        MODEL
    }
}
struct Provider {
    builds: Arc<AtomicUsize>,
    embeds: Arc<AtomicUsize>,
    entered: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        self.entered.send(()).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(Arc::new(Service(self.embeds.clone())))
    }
}
fn fixture() -> (
    KhiveRuntime,
    mpsc::UnboundedReceiver<()>,
    Arc<Semaphore>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let rt = KhiveRuntime::memory().unwrap();
    let (entered, receiver) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let builds = Arc::new(AtomicUsize::new(0));
    let embeds = Arc::new(AtomicUsize::new(0));
    rt.register_embedder(Provider {
        builds: builds.clone(),
        embeds: embeds.clone(),
        entered,
        release: release.clone(),
    });
    (rt, receiver, release, builds, embeds)
}
async fn event_count(rt: &KhiveRuntime) -> usize {
    let token = rt.authorize(Namespace::local()).unwrap();
    rt.events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::EmbedderInitialized],
                ..Default::default()
            },
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|view| view.payload["model_name"] == MODEL)
        .count()
}
async fn wait_for_event(rt: &KhiveRuntime) {
    tokio::time::timeout(WAIT, async {
        while event_count(rt).await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("initialization event must survive starter cancellation");
}
async fn cancelled(task: tokio::task::JoinHandle<Result<(String, Vec<f32>), RuntimeError>>) {
    assert!(
        matches!(tokio::time::timeout(WAIT, task).await.unwrap().unwrap(),
        Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation })) if operation == "memory.recall.embedding")
    );
}

#[tokio::test]
async fn cancelled_cold_recall_finishes_one_build_then_reuses_it() {
    let (rt, mut entered, release, builds, embeds) = fixture();
    let cache = QueryEmbeddingCache::with_default_capacity();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let request_rt = rt.clone();
    let request_cache = cache.clone();
    let task = tokio::spawn(async move {
        khive_storage::operation_context::scope_operation_attribution(
            OperationAttribution {
                op_index: 7,
                ref_resolution: RefResolution::Resolved,
            },
            khive_storage::scope_request_read_cancellation(
                cancel_rx,
                embed_query_model(
                    request_rt,
                    request_cache,
                    MODEL.into(),
                    "cold recall".into(),
                ),
            ),
        )
        .await
    });
    tokio::time::timeout(WAIT, entered.recv())
        .await
        .unwrap()
        .unwrap();
    cancel_tx.send(true).unwrap();
    cancelled(task).await;
    assert_eq!(cache.len(), 0);
    assert_eq!(embeds.load(Ordering::SeqCst), 0);
    release.add_permits(1);
    wait_for_event(&rt).await;
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        embeds.load(Ordering::SeqCst),
        0,
        "cancelled query work must not detach with the build"
    );
    let token = rt.authorize(Namespace::local()).unwrap();
    let events = rt
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::EmbedderInitialized],
                ..Default::default()
            },
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let event = events
        .items
        .iter()
        .find(|event| event.payload["model_name"] == MODEL)
        .unwrap();
    assert_eq!(event.op_index, Some(7));
    assert_eq!(event.ref_resolution, Some(RefResolution::Resolved));
    let (_, vector) = tokio::time::timeout(
        WAIT,
        embed_query_model(
            rt.clone(),
            cache.clone(),
            MODEL.into(),
            "next recall".into(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(vector, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "the cancelled starter must not discard the cold build"
    );
    wait_for_event(&rt).await;
    embed_query_model(
        rt.clone(),
        cache.clone(),
        MODEL.into(),
        "next recall".into(),
    )
    .await
    .unwrap();
    assert_eq!(embeds.load(Ordering::SeqCst), 1);
    assert_eq!(event_count(&rt).await, 1);
}

struct PanickingProvider;
#[async_trait]
impl EmbedderProvider for PanickingProvider {
    fn name(&self) -> &str {
        "recall-panicking-model"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        panic!("cold provider panic");
    }
}
#[tokio::test]
async fn cold_provider_panic_is_internal_and_other_model_can_degrade() {
    let (rt, mut entered, release, builds, _) = fixture();
    release.add_permits(1);
    rt.register_embedder(PanickingProvider);
    let cache = QueryEmbeddingCache::with_default_capacity();
    let (bad, good) = tokio::join!(
        embed_query_model(
            rt.clone(),
            cache.clone(),
            "recall-panicking-model".into(),
            "recall query".into()
        ),
        embed_query_model(rt, cache.clone(), MODEL.into(), "recall query".into())
    );
    assert!(matches!(&bad, Err(RuntimeError::Internal(message)) if message.contains("panicked")));
    let healthy = collect_embed_results(vec![
        ("recall-panicking-model".into(), bad),
        (MODEL.into(), good),
    ])
    .unwrap();
    assert_eq!(healthy, [(MODEL.to_owned(), vec![1.0, 0.0, 0.0, 0.0])]);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(entered.recv().await, Some(()));
    assert_eq!(cache.len(), 1);
}
