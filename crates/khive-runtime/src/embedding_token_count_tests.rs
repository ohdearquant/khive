use super::tests::{poll_once, wait_for_entered, BlockingTestService, ReleaseWorkerOnDrop};
use super::*;
use std::task::Poll;
use std::time::Duration;

type Call = (EmbeddingRole, EmbeddingModel, Vec<String>);

struct CountService {
    blocker: Arc<BlockingTestService>,
    counts: Mutex<Vec<Call>>,
    vectors: Mutex<Vec<Call>>,
}

impl CountService {
    fn new() -> Self {
        Self {
            blocker: Arc::new(BlockingTestService::new()),
            counts: Mutex::new(Vec::new()),
            vectors: Mutex::new(Vec::new()),
        }
    }

    async fn vectors(
        &self,
        texts: &[String],
        model: EmbeddingModel,
        role: EmbeddingRole,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.vectors
            .lock()
            .unwrap()
            .push((role, model, texts.to_vec()));
        self.blocker.embed(texts, model).await?;
        Ok(texts.iter().map(|text| vec![text.len() as f32]).collect())
    }
}

#[async_trait]
impl EmbeddingService for CountService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.vectors(texts, model, EmbeddingRole::Generic).await
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.vectors(texts, model, EmbeddingRole::Query).await
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.vectors(texts, model, EmbeddingRole::Passage).await
    }

    async fn count_tokens(
        &self,
        texts: &[String],
        model: EmbeddingModel,
        role: EmbeddingRole,
    ) -> lattice_embed::Result<Vec<TokenCount>> {
        self.counts
            .lock()
            .unwrap()
            .push((role, model, texts.to_vec()));
        match texts[0].as_str() {
            "unsupported" => {
                return Err(lattice_embed::EmbedError::Unsupported(
                    "count sentinel".into(),
                ))
            }
            "failed" => {
                return Err(lattice_embed::EmbedError::InferenceFailed(
                    "count sentinel".into(),
                ))
            }
            _ => {}
        }
        self.blocker.embed(texts, model).await?;
        Ok(texts
            .iter()
            .map(|text| TokenCount::new(text.len(), text.len().min(3)))
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "token-count-test"
    }
}

#[tokio::test]
async fn token_counts_keep_role_model_and_share_the_native_worker() {
    let inner = Arc::new(CountService::new());
    inner.blocker.release();
    let service = BlockingEmbeddingService::new(Arc::clone(&inner));
    let model = EmbeddingModel::MultilingualE5Small;
    let texts = vec!["ab".to_owned(), "longer".to_owned()];
    let roles = [
        EmbeddingRole::Generic,
        EmbeddingRole::Query,
        EmbeddingRole::Passage,
    ];
    for role in roles {
        assert_eq!(
            service.count_tokens(&texts, model, role).await.unwrap(),
            vec![TokenCount::new(2, 2), TokenCount::new(6, 3)]
        );
    }
    assert_eq!(
        service.embed(&texts, model).await.unwrap(),
        vec![vec![2.0], vec![6.0]]
    );
    assert_eq!(
        *inner.counts.lock().unwrap(),
        roles.map(|role| (role, model, texts.clone()))
    );
    let threads = inner.blocker.thread_ids.lock().unwrap();
    assert_eq!(threads.len(), 1, "counts and vectors share one worker");
    assert!(
        !threads.contains(&std::thread::current().id()),
        "native counts must not run on the caller"
    );
}

#[tokio::test]
async fn cached_reports_recount_tokens_and_reuse_ordered_vectors() {
    let inner = Arc::new(CountService::new());
    inner.blocker.release();
    let service = cached_blocking_service(Arc::clone(&inner));
    let model = EmbeddingModel::MultilingualE5Small;
    let warm = vec!["later".to_owned()];
    for _ in 0..2 {
        let report = service
            .embed_with_report(&warm, model, EmbeddingRole::Passage)
            .await
            .unwrap();
        assert_eq!(report.embeddings, vec![vec![5.0]]);
        assert_eq!(report.token_counts, vec![TokenCount::new(5, 3)]);
    }
    let mixed = vec!["bb".to_owned(), "later".to_owned(), "ccc".to_owned()];
    let report = service
        .embed_with_report(&mixed, model, EmbeddingRole::Passage)
        .await
        .unwrap();
    assert_eq!(report.embeddings, vec![vec![2.0], vec![5.0], vec![3.0]]);
    assert_eq!(
        report.token_counts,
        vec![
            TokenCount::new(2, 2),
            TokenCount::new(5, 3),
            TokenCount::new(3, 3)
        ]
    );
    assert_eq!(
        *inner.counts.lock().unwrap(),
        vec![
            (EmbeddingRole::Passage, model, warm.clone()),
            (EmbeddingRole::Passage, model, warm.clone()),
            (EmbeddingRole::Passage, model, mixed)
        ]
    );
    assert_eq!(
        *inner.vectors.lock().unwrap(),
        vec![
            (EmbeddingRole::Passage, model, warm),
            (
                EmbeddingRole::Passage,
                model,
                vec!["bb".to_owned(), "ccc".to_owned()]
            )
        ]
    );
}

#[tokio::test]
async fn count_worker_preserves_errors_and_rejects_input_and_byte_overflow() {
    let inner = Arc::new(CountService::new());
    inner.blocker.release();
    let service = BlockingEmbeddingService::new(Arc::clone(&inner));
    let model = EmbeddingModel::default();
    let role = EmbeddingRole::Passage;
    assert!(matches!(
        service.count_tokens(&["unsupported".to_owned()], model, role).await,
        Err(lattice_embed::EmbedError::Unsupported(message)) if message == "count sentinel"
    ));
    assert!(matches!(
        service.count_tokens(&["failed".to_owned()], model, role).await,
        Err(lattice_embed::EmbedError::InferenceFailed(message)) if message == "count sentinel"
    ));
    assert!(matches!(
        service.count_tokens(&[], model, role).await,
        Err(lattice_embed::EmbedError::InvalidInput(_))
    ));
    assert!(matches!(
        service
            .count_tokens(&["x".repeat(MAX_TEXT_BYTES + 1)], model, role)
            .await,
        Err(lattice_embed::EmbedError::TextTooLong { .. })
    ));
    assert!(matches!(
        service
            .count_tokens(
                &vec!["x".to_owned(); DEFAULT_MAX_BATCH_SIZE + 1],
                model,
                role
            )
            .await,
        Err(lattice_embed::EmbedError::InvalidInput(_))
    ));
    let bounded = BlockingEmbeddingService::with_byte_budget(Arc::clone(&inner), 3);
    assert!(matches!(
        bounded.count_tokens(&["four".to_owned()], model, role).await,
        Err(lattice_embed::EmbedError::Internal(message)) if message.contains("byte budget exceeded")
    ));
    assert_eq!(
        inner.counts.lock().unwrap().len(),
        2,
        "invalid inputs never reach the provider"
    );
    assert!(inner.vectors.lock().unwrap().is_empty());
    assert_eq!(bounded.in_flight_bytes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn token_count_queue_is_bounded_and_discards_cancelled_jobs() {
    let inner = Arc::new(CountService::new());
    let _release = ReleaseWorkerOnDrop(Arc::clone(&inner.blocker));
    let service = BlockingEmbeddingService::new(Arc::clone(&inner));
    let model = EmbeddingModel::default();
    let role = EmbeddingRole::Passage;
    let first_text = vec!["held".to_owned()];
    let mut first = Box::pin(service.count_tokens(&first_text, model, role));
    assert!(poll_once(first.as_mut()).await.is_pending());
    wait_for_entered(&inner.blocker, 1).await;
    let texts: Vec<_> = (0..EMBEDDING_QUEUE_CAPACITY)
        .map(|i| vec![format!("queued-{i}")])
        .collect();
    let mut queued = Vec::new();
    for text in &texts {
        let mut call = Box::pin(service.count_tokens(text, model, role));
        assert!(poll_once(call.as_mut()).await.is_pending());
        queued.push(call);
    }
    assert_eq!(
        service.in_flight_bytes.load(Ordering::Acquire),
        4 + texts.iter().flatten().map(String::len).sum::<usize>()
    );
    assert!(matches!(
        service.count_tokens(&["overflow".to_owned()], model, role).await,
        Err(lattice_embed::EmbedError::Internal(message)) if message == "embedding worker queue is full"
    ));
    drop(queued);
    inner.blocker.release();
    assert_eq!(first.await.unwrap(), vec![TokenCount::new(4, 3)]);
    let later = vec!["later".to_owned()];
    assert_eq!(
        with_embedding_admission(service.count_tokens(&later, model, role))
            .await
            .unwrap(),
        vec![TokenCount::new(5, 3)]
    );
    assert_eq!(
        *inner.counts.lock().unwrap(),
        vec![(role, model, first_text), (role, model, later)]
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while service.in_flight_bytes.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all completed or cancelled count jobs release their bytes");
    assert!(inner.vectors.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn accepted_count_finishes_but_expired_vector_admission_is_refused() {
    let inner = Arc::new(CountService::new());
    let _release = ReleaseWorkerOnDrop(Arc::clone(&inner.blocker));
    let service = cached_blocking_service(Arc::clone(&inner));
    let texts = vec!["held".to_owned()];
    let model = EmbeddingModel::default();
    let mut call = Box::pin(khive_storage::scope_request_read_deadline(
        Duration::from_millis(100),
        with_embedding_admission(service.embed_with_report(&texts, model, EmbeddingRole::Passage)),
    ));
    assert!(poll_once(call.as_mut()).await.is_pending());
    wait_for_entered(&inner.blocker, 1).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(
        poll_once(call.as_mut()).await.is_pending(),
        "an accepted count is not cancelled at expiry"
    );
    inner.blocker.release();
    let watchdog = std::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        if let Poll::Ready(result) = poll_once(call.as_mut()).await {
            break result;
        }
        assert!(
            std::time::Instant::now() < watchdog,
            "second admission must not hang after the count finishes"
        );
        tokio::task::yield_now().await;
    };
    assert!(matches!(
        result,
        Err(RuntimeError::Storage(
            khive_storage::StorageError::AdmissionTimeout { .. }
        ))
    ));
    assert_eq!(inner.counts.lock().unwrap().len(), 1);
    assert!(
        inner.vectors.lock().unwrap().is_empty(),
        "expired second admission never starts inference"
    );
}

#[tokio::test]
async fn warm_report_still_needs_count_admission_but_warm_vectors_do_not() {
    let inner = Arc::new(CountService::new());
    inner.blocker.release();
    let service = cached_blocking_service(Arc::clone(&inner));
    let texts = vec!["later".to_owned()];
    let model = EmbeddingModel::default();
    service
        .embed_with_report(&texts, model, EmbeddingRole::Passage)
        .await
        .unwrap();
    let report = khive_storage::scope_request_read_deadline(
        Duration::ZERO,
        with_embedding_admission(service.embed_with_report(&texts, model, EmbeddingRole::Passage)),
    )
    .await;
    assert!(matches!(
        report,
        Err(RuntimeError::Storage(
            khive_storage::StorageError::AdmissionTimeout { .. }
        ))
    ));
    let vectors = khive_storage::scope_request_read_deadline(
        Duration::ZERO,
        with_embedding_admission(service.embed_passage(&texts, model)),
    )
    .await
    .unwrap();
    assert_eq!(vectors, vec![vec![5.0]]);
    assert_eq!(
        inner.counts.lock().unwrap().len(),
        1,
        "expired count never enters worker"
    );
    assert_eq!(
        inner.vectors.lock().unwrap().len(),
        1,
        "cached vector adds no native work"
    );
}
