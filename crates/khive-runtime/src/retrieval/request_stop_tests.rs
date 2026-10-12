//! Request-stop boundaries must precede the hybrid search's ordinary-error tolerance.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use khive_storage::{
    ensure_request_read_active, scope_request_read_cancellation, scope_request_read_deadline,
    StorageError,
};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tokio::sync::{watch, Notify};

use crate::embedder_registry::EmbedderProvider;
use crate::runtime::RuntimeConfig;
use crate::stage_seam::{TextStageDouble, TEXT_STAGE_DOUBLE};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

#[derive(Clone, Copy)]
enum ProviderMode {
    ReadyFailure,
    CancelReadyFailure,
    WaitForRelease,
    LocalTimeout,
}

struct ControlledProvider {
    mode: ProviderMode,
    cancellation: watch::Sender<bool>,
    entered: AtomicUsize,
    returned: AtomicUsize,
    release: Notify,
}

#[async_trait::async_trait]
impl EmbeddingService for ControlledProvider {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let reason = match self.mode {
            ProviderMode::ReadyFailure => "ordinary-provider-failure",
            ProviderMode::CancelReadyFailure => {
                // Both the provider error and outer cancellation become ready in this poll.
                self.cancellation.send(true).expect("live request receiver");
                "same-poll-provider-failure"
            }
            ProviderMode::WaitForRelease => {
                self.release.notified().await;
                "released-provider-failure"
            }
            ProviderMode::LocalTimeout => {
                tokio::time::timeout(Duration::from_millis(5), std::future::pending::<()>())
                    .await
                    .expect_err("the provider's own shorter timeout expires");
                "local-provider-timeout"
            }
        };
        self.returned.fetch_add(1, Ordering::SeqCst);
        Err(EmbedError::ModelInitialization(reason.into()))
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "request-stop-controlled-provider"
    }
}

struct ProviderRegistration {
    name: String,
    service: Arc<ControlledProvider>,
}

#[async_trait::async_trait]
impl EmbedderProvider for ProviderRegistration {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.service.clone())
    }
}

async fn fixture(
    mode: ProviderMode,
) -> (KhiveRuntime, Arc<ControlledProvider>, watch::Receiver<bool>) {
    let (cancellation, receiver) = watch::channel(false);
    let service = Arc::new(ControlledProvider {
        mode,
        cancellation,
        entered: AtomicUsize::new(0),
        returned: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let model = EmbeddingModel::AllMiniLmL6V2;
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(model),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    runtime
        .try_register_embedder(ProviderRegistration {
            name: model.to_string(),
            service: Arc::clone(&service),
        })
        .expect("replace unresolved built-in provider");
    runtime
        .embedder(&model.to_string())
        .await
        .expect("resolve provider outside the stop scope");
    assert_eq!(service.entered.load(Ordering::SeqCst), 0);
    (runtime, service, receiver)
}

fn assert_stopped<T: std::fmt::Debug>(result: RuntimeResult<T>, operation: &str) {
    assert!(
        matches!(&result, Err(RuntimeError::Storage(StorageError::Timeout { operation: actual })) if actual == operation),
        "expected request-stop timeout from {operation}, got {result:?}"
    );
}

async fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut future = future;
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test]
async fn vector_ready_error_cannot_hide_cancellation_in_the_same_poll() {
    let (runtime, provider, receiver) = fixture(ProviderMode::CancelReadyFailure).await;
    let token = NamespaceToken::local();
    let result = scope_request_read_cancellation(
        receiver,
        runtime.hybrid_vector_stage(&token, "query", None, 4, None, true),
    )
    .await;
    assert_eq!(provider.entered.load(Ordering::SeqCst), 1);
    assert_eq!(provider.returned.load(Ordering::SeqCst), 1);
    assert_stopped(result, "hybrid_search.vector");
}

#[tokio::test(start_paused = true)]
async fn inherited_deadline_drops_a_pending_custom_provider_without_waiting_for_it() {
    let (runtime, provider, _receiver) = fixture(ProviderMode::WaitForRelease).await;
    let token = NamespaceToken::local();
    let mut call = Box::pin(scope_request_read_deadline(
        Duration::from_millis(100),
        runtime.hybrid_vector_stage(&token, "query", None, 4, None, true),
    ));
    assert!(poll_once(call.as_mut()).await.is_pending());
    assert_eq!(provider.entered.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(100)).await;
    let stopped = poll_once(call.as_mut()).await;
    // Release even a regressed, still-pending provider before an assertion can panic.
    provider.release.notify_one();
    match stopped {
        Poll::Ready(result) => assert_stopped(result, "hybrid_search.vector"),
        Poll::Pending => {
            panic!("outer deadline was ignored while custom inference remained pending")
        }
    }
    assert_eq!(provider.returned.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn active_request_still_tolerates_provider_failure_and_a_shorter_local_timeout() {
    for mode in [ProviderMode::ReadyFailure, ProviderMode::LocalTimeout] {
        let (runtime, provider, _receiver) = fixture(mode).await;
        let token = NamespaceToken::local();
        let (hits, error) = scope_request_read_deadline(Duration::from_secs(10), async {
            let result = runtime
                .hybrid_vector_stage(&token, "query", None, 4, None, true)
                .await;
            ensure_request_read_active("test.outer-still-active").expect("outer budget remains");
            result
        })
        .await
        .expect("ordinary vector failure remains tolerated");
        assert!(hits.is_empty());
        let expected = match mode {
            ProviderMode::ReadyFailure => "ordinary-provider-failure",
            ProviderMode::LocalTimeout => "local-provider-timeout",
            _ => unreachable!("the control chooses only ordinary failures"),
        };
        assert!(error.expect("degradation evidence").contains(expected));
        assert_eq!(provider.entered.load(Ordering::SeqCst), 1);
        assert_eq!(provider.returned.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn public_hybrid_boundary_catches_a_late_ready_text_stop_after_vector_completion() {
    for text_errors in [false, true] {
        let (runtime, provider, receiver) = fixture(ProviderMode::ReadyFailure).await;
        let token = NamespaceToken::local();
        runtime
            .text(&token)
            .expect("warm text capability before stop scope");
        let observed = Arc::clone(&provider);
        let text: TextStageDouble = Arc::new(move || {
            let provider = Arc::clone(&observed);
            Box::pin(async move {
                // The ready vector failure completes during this deliberate first Pending.
                tokio::task::yield_now().await;
                assert_eq!(provider.returned.load(Ordering::SeqCst), 1);
                provider
                    .cancellation
                    .send(true)
                    .expect("live request receiver");
                if text_errors {
                    Err(StorageError::Timeout {
                        operation: "local-text-error".into(),
                    })
                } else {
                    Ok(Vec::new())
                }
            })
        });
        let result = scope_request_read_cancellation(
            receiver,
            TEXT_STAGE_DOUBLE.scope(
                text,
                runtime.hybrid_search_outcome(&token, "query", 1, None, None, &[], None),
            ),
        )
        .await;
        assert_eq!(provider.returned.load(Ordering::SeqCst), 1);
        assert_stopped(result, "hybrid_search");
    }
}

#[tokio::test]
async fn an_already_stopped_request_cannot_succeed_through_a_skipped_vector_stage() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime without embedding");
    let token = NamespaceToken::local();
    let (cancel, receiver) = watch::channel(true);
    let result = scope_request_read_cancellation(
        receiver,
        runtime.hybrid_vector_stage(&token, "query", None, 4, None, true),
    )
    .await;
    assert_stopped(result, "hybrid_search.vector");
    drop(cancel);
}
