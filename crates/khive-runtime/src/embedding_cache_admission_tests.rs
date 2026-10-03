use super::tests::{
    poll_once, queue_runtime, wait_for_entered, BlockingTestService, ConstVecService,
    ReleaseWorkerOnDrop,
};
use super::*;
use std::sync::Mutex;
use std::time::Duration;

struct RoleVectorService {
    calls: Mutex<Vec<(u16, Vec<String>)>>,
}

impl RoleVectorService {
    fn vectors(&self, texts: &[String], role: u16) -> Vec<Vec<f32>> {
        self.calls.lock().unwrap().push((role, texts.to_vec()));
        texts
            .iter()
            .map(|text| vec![f32::from(role) + text.len() as f32])
            .collect()
    }
}

#[async_trait]
impl EmbeddingService for RoleVectorService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        Ok(self.vectors(texts, 0))
    }
    async fn embed_query(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        Ok(self.vectors(texts, 100))
    }
    async fn embed_passage(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        Ok(self.vectors(texts, 200))
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "role-vector"
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_cached_query_bypasses_full_queue_and_expired_admission_bound() {
    let inner = Arc::new(BlockingTestService::new());
    let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
    let runtime = queue_runtime(cached_blocking_service(Arc::clone(&inner)));
    assert_eq!(
        runtime
            .embed_query_with_model("queue-test", "later")
            .await
            .unwrap(),
        vec![1.0]
    );
    let mut first = Box::pin(runtime.embed_with_model("queue-test", "first"));
    assert!(poll_once(first.as_mut()).await.is_pending());
    wait_for_entered(&inner, 2).await;
    let texts: Vec<_> = (0..EMBEDDING_QUEUE_CAPACITY)
        .map(|index| vec![format!("queued-{index}")])
        .collect();
    let mut queued = Vec::new();
    for text in &texts {
        let mut call = Box::pin(runtime.embed_batch_with_model("queue-test", text));
        assert!(poll_once(call.as_mut()).await.is_pending());
        queued.push(call);
    }
    let cached = khive_storage::scope_request_read_deadline(
        Duration::ZERO,
        runtime.embed_query_with_model("queue-test", "later"),
    )
    .await;
    let calls_before_release = inner.entered.load(Ordering::Acquire);
    inner.release();
    assert_eq!(
        cached.unwrap(),
        vec![1.0],
        "a real runtime query cache hit must bypass full-queue admission"
    );
    assert_eq!(calls_before_release, 2);
    assert_eq!(first.await.unwrap(), vec![1.0]);
    for call in queued {
        assert_eq!(call.await.unwrap(), vec![vec![1.0]]);
    }
    assert_eq!(
        inner.entered.load(Ordering::Acquire),
        EMBEDDING_QUEUE_CAPACITY + 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_embedding_roles_complete_without_admission_when_queue_is_full() {
    use std::future::Future;
    use std::task::Poll;

    let inner = Arc::new(BlockingTestService::new());
    let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
    let service = cached_blocking_service(Arc::clone(&inner));
    let model = EmbeddingModel::AllMiniLmL6V2;
    let warm = vec!["later".to_owned()];
    assert_eq!(service.embed(&warm, model).await.unwrap(), vec![vec![1.0]]);
    assert_eq!(
        service.embed_query(&warm, model).await.unwrap(),
        vec![vec![1.0]]
    );
    assert_eq!(
        service.embed_passage(&warm, model).await.unwrap(),
        vec![vec![1.0]]
    );
    assert_eq!(
        inner.entered.load(Ordering::Acquire),
        3,
        "generic, query and passage must retain separate cache identities"
    );

    let first_text = vec!["first".to_owned()];
    let mut first = Box::pin(service.embed(&first_text, model));
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while inner.entered.load(Ordering::Acquire) != 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first miss must occupy the native worker");

    let queued_texts: Vec<_> = (0..EMBEDDING_QUEUE_CAPACITY)
        .map(|index| vec![format!("uncached-{index}")])
        .collect();
    let mut queued = Vec::new();
    for texts in &queued_texts {
        let mut call = Box::pin(service.embed(texts, model));
        std::future::poll_fn(|cx| {
            assert!(
                call.as_mut().poll(cx).is_pending(),
                "every queue slot must accept one actual cache miss"
            );
            Poll::Ready(())
        })
        .await;
        queued.push(call);
    }

    let mut generic = Box::pin(service.embed(&warm, model));
    let generic = std::future::poll_fn(|cx| Poll::Ready(generic.as_mut().poll(cx))).await;
    let mut query = Box::pin(service.embed_query(&warm, model));
    let query = std::future::poll_fn(|cx| Poll::Ready(query.as_mut().poll(cx))).await;
    let mut passage = Box::pin(service.embed_passage(&warm, model));
    let passage = std::future::poll_fn(|cx| Poll::Ready(passage.as_mut().poll(cx))).await;
    let calls_before_release = inner.entered.load(Ordering::Acquire);
    inner.release();

    for (role, hit) in [("generic", generic), ("query", query), ("passage", passage)] {
        assert!(
            matches!(hit, Poll::Ready(Ok(ref vectors)) if vectors == &vec![vec![1.0]]),
            "a cached {role} result must be ready without touching the full queue: {hit:?}"
        );
    }
    assert_eq!(
        calls_before_release, 4,
        "cache hits must not run the native service while its worker is occupied"
    );
    assert_eq!(first.await.unwrap(), vec![vec![1.0]]);
    for call in queued {
        assert_eq!(call.await.unwrap(), vec![vec![1.0]]);
    }
    assert_eq!(
        inner.entered.load(Ordering::Acquire),
        4 + EMBEDDING_QUEUE_CAPACITY,
        "draining misses must not add native calls for the cached requests"
    );
}

#[tokio::test]
async fn cached_embedding_partial_hits_preserve_input_order_and_role() {
    let inner = Arc::new(RoleVectorService {
        calls: Mutex::new(Vec::new()),
    });
    let service = cached_blocking_service(Arc::clone(&inner));
    let model = EmbeddingModel::default();
    let warm = vec!["a".to_owned()];
    assert_eq!(
        service.embed_query(&warm, model).await.unwrap(),
        vec![vec![101.0]]
    );
    let mixed = vec!["bb".to_owned(), "a".to_owned(), "ccc".to_owned()];
    assert_eq!(
        service.embed_query(&mixed, model).await.unwrap(),
        vec![vec![102.0], vec![101.0], vec![103.0]]
    );
    assert_eq!(service.embed(&warm, model).await.unwrap(), vec![vec![1.0]]);
    assert_eq!(
        service.embed_passage(&warm, model).await.unwrap(),
        vec![vec![201.0]]
    );
    assert_eq!(
        *inner.calls.lock().unwrap(),
        vec![
            (100, vec!["a".to_owned()]),
            (100, vec!["bb".to_owned(), "ccc".to_owned()]),
            (0, vec!["a".to_owned()]),
            (200, vec!["a".to_owned()]),
        ],
        "only misses may reach inference, while role identities remain separate"
    );
}

#[tokio::test]
async fn cached_embedding_preserves_role_input_limit_before_instruction() {
    let service = cached_blocking_service(Arc::new(ConstVecService { dims: 1 }));
    let texts = vec!["x".repeat(MAX_TEXT_BYTES)];
    let model = EmbeddingModel::MultilingualE5Small;
    assert!(model.query_instruction().is_some());
    assert!(model.document_instruction().is_some());
    assert_eq!(
        service.embed_query(&texts, model).await.unwrap(),
        vec![vec![1.0]],
        "query preparation must not reduce the caller's published input limit"
    );
    assert_eq!(
        service.embed_passage(&texts, model).await.unwrap(),
        vec![vec![1.0]],
        "passage preparation must not reduce the caller's published input limit"
    );
}
