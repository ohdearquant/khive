use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::{EmbedderProvider, KhiveRuntime};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tokio::sync::{mpsc, Semaphore};

use super::{embed_query_model, QueryEmbeddingCache, RuntimeError};

const MODEL: &str = "recall-async-embedding-test";
const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

struct GatedEmbedder {
    entered: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl EmbeddingService for GatedEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.entered.send(()).expect("entry receiver");
        self.release.acquire().await.expect("release gate").forget();
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        MODEL
    }
}

struct GatedProvider(Arc<GatedEmbedder>);

#[async_trait]
impl EmbedderProvider for GatedProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(self.0.clone())
    }
}

fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("test executor")
}

fn fixture() -> (KhiveRuntime, mpsc::UnboundedReceiver<()>, Arc<Semaphore>) {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let (entered, receiver) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    runtime.register_embedder(GatedProvider(Arc::new(GatedEmbedder {
        entered,
        release: release.clone(),
    })));
    (runtime, receiver, release)
}

#[test]
fn concurrent_cache_misses_enter_embedding_without_blocking_pool_serialization() {
    executor().block_on(async {
        let (runtime, mut entered, release) = fixture();
        let cache = QueryEmbeddingCache::with_default_capacity();
        let tasks: Vec<_> = (0..4)
            .map(|i| {
                tokio::spawn(embed_query_model(
                    runtime.clone(),
                    cache.clone(),
                    MODEL.to_owned(),
                    format!("query {i}"),
                ))
            })
            .collect();
        let admitted = tokio::time::timeout(WAIT, async {
            for _ in 0..tasks.len() {
                entered.recv().await.expect("embedding entry");
            }
        })
        .await
        .is_ok();

        // Release before asserting so the old blocking wrapper cannot strand
        // worker threads when this regression fails.
        release.add_permits(tasks.len());
        for task in tasks {
            let (model, vector) = task.await.expect("embedding task").expect("embedding");
            assert_eq!(model, MODEL);
            assert_eq!(vector, vec![1.0, 0.0, 0.0, 0.0]);
        }
        assert!(admitted, "all cache misses must enter before any release");
        assert_eq!(cache.len(), 4);

        let cached = embed_query_model(
            runtime.clone(),
            cache.clone(),
            MODEL.to_owned(),
            "query 0".to_owned(),
        )
        .await
        .expect("cached embedding");
        assert_eq!(cached.1, vec![1.0, 0.0, 0.0, 0.0]);
        assert!(matches!(
            entered.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            embed_query_model(runtime, cache, "unknown".to_owned(), "query 0".to_owned())
                .await,
            Err(RuntimeError::UnknownModel(model)) if model == "unknown"
        ));
    });
}

#[test]
fn waiting_embedding_leaves_the_blocking_thread_available() {
    executor().block_on(async {
        let (runtime, mut entered, release) = fixture();
        let task = tokio::spawn(embed_query_model(
            runtime,
            QueryEmbeddingCache::with_default_capacity(),
            MODEL.to_owned(),
            "held query".to_owned(),
        ));
        let embedding_entered = matches!(
            tokio::time::timeout(WAIT, entered.recv()).await,
            Ok(Some(()))
        );
        let (probe_tx, probe_rx) = tokio::sync::oneshot::channel();
        let probe = tokio::task::spawn_blocking(move || {
            let _ = probe_tx.send(());
        });
        let probe_ran = matches!(tokio::time::timeout(WAIT, probe_rx).await, Ok(Ok(())));
        release.add_permits(1);
        task.await.expect("embedding task").expect("embedding");
        probe.await.expect("blocking probe");
        assert!(embedding_entered, "embedding must be held before the probe");
        assert!(
            probe_ran,
            "blocking probe must run before embedding release"
        );
    });
}

#[test]
fn cancellation_during_embedding_returns_the_request_timeout_without_caching() {
    executor().block_on(async {
        let (runtime, mut entered, release) = fixture();
        let cache = QueryEmbeddingCache::with_default_capacity();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let task_cache = cache.clone();
        let mut task = tokio::spawn(async move {
            khive_storage::scope_request_read_cancellation(
                cancel_rx,
                embed_query_model(
                    runtime,
                    task_cache,
                    MODEL.to_owned(),
                    "cancel me".to_owned(),
                ),
            )
            .await
        });
        let embedding_entered = matches!(
            tokio::time::timeout(WAIT, entered.recv()).await,
            Ok(Some(()))
        );
        cancel_tx.send(true).expect("cancel request");
        let result = tokio::time::timeout(WAIT, &mut task).await;
        release.add_permits(1);
        if result.is_err() {
            task.abort();
            let _ = task.await;
        }
        assert!(embedding_entered, "cancel after embedding starts");
        assert!(matches!(
            result.expect("request must stop before embedding release").expect("embedding task"),
            Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation }))
                if operation == "memory.recall.embedding"
        ));
        assert_eq!(cache.len(), 0);
    });
}
