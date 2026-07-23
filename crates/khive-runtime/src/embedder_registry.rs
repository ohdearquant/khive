//! EmbedderRegistry — pack-extensible embedding provider surface.
//!
//! Packs implement [`EmbedderProvider`] and register custom models via
//! [`crate::KhiveRuntime::register_embedder`]. Built-in lattice models are pre-registered
//! during runtime construction and require no opt-in.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingService, NativeEmbeddingService,
    DEFAULT_MAX_BATCH_SIZE, MAX_TEXT_CHARS,
};
use tokio::sync::OnceCell;

use crate::error::{RuntimeError, RuntimeResult};

#[derive(Clone, Copy)]
enum EmbeddingCall {
    Generic,
    Query,
    Passage,
}

const EMBEDDING_QUEUE_CAPACITY: usize = 32;
const EMBEDDING_MAX_JOB_BYTES: usize = DEFAULT_MAX_BATCH_SIZE * MAX_TEXT_CHARS;
// 32 queue slots × the normal 128-text batch × 32 KiB/text = 128 MiB in flight.
const EMBEDDING_QUEUE_BYTE_BUDGET: usize = EMBEDDING_QUEUE_CAPACITY * 128 * MAX_TEXT_CHARS;

struct InFlightBytes {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl InFlightBytes {
    fn reserve(
        counter: Arc<AtomicUsize>,
        byte_budget: usize,
        bytes: usize,
    ) -> lattice_embed::Result<Self> {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(lattice_embed::EmbedError::Internal(format!(
                    "embedding worker byte budget exceeded: in-flight byte count overflowed the {byte_budget}-byte budget"
                )));
            };
            if next > byte_budget {
                return Err(lattice_embed::EmbedError::Internal(format!(
                    "embedding worker byte budget exceeded: {current} in flight + {bytes} job bytes > {byte_budget}"
                )));
            }
            match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(Self { counter, bytes }),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for InFlightBytes {
    fn drop(&mut self) {
        let previous = self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes, "embedding byte counter underflow");
    }
}

struct EmbeddingJob {
    texts: Vec<String>,
    model: EmbeddingModel,
    call: EmbeddingCall,
    reply: tokio::sync::oneshot::Sender<lattice_embed::Result<Vec<Vec<f32>>>>,
    _in_flight: InFlightBytes,
}

/// Bounds non-cancellable native inference to one worker and a fixed queue.
/// Callers can detach safely because closed queued jobs are skipped before inference.
pub(crate) struct BlockingEmbeddingService<S> {
    inner: Arc<S>,
    worker: OnceLock<Result<SyncSender<EmbeddingJob>, String>>,
    in_flight_bytes: Arc<AtomicUsize>,
    byte_budget: usize,
}

impl<S> BlockingEmbeddingService<S> {
    pub(crate) fn new(inner: Arc<S>) -> Self {
        Self {
            inner,
            worker: OnceLock::new(),
            in_flight_bytes: Arc::new(AtomicUsize::new(0)),
            byte_budget: EMBEDDING_QUEUE_BYTE_BUDGET,
        }
    }

    #[cfg(test)]
    fn with_byte_budget(inner: Arc<S>, byte_budget: usize) -> Self {
        Self {
            inner,
            worker: OnceLock::new(),
            in_flight_bytes: Arc::new(AtomicUsize::new(0)),
            byte_budget,
        }
    }
}

impl<S: EmbeddingService + 'static> BlockingEmbeddingService<S> {
    fn input_bytes(texts: &[String]) -> lattice_embed::Result<usize> {
        if texts.is_empty() {
            return Err(lattice_embed::EmbedError::InvalidInput(
                "no texts provided".to_owned(),
            ));
        }
        let input_bytes = texts.iter().try_fold(0usize, |total, text| {
            total.checked_add(text.len()).ok_or_else(|| {
                lattice_embed::EmbedError::InvalidInput(format!(
                    "embedding job input exceeds the {EMBEDDING_MAX_JOB_BYTES}-byte maximum"
                ))
            })
        })?;
        if input_bytes > EMBEDDING_MAX_JOB_BYTES {
            return Err(lattice_embed::EmbedError::InvalidInput(format!(
                "embedding job input is {input_bytes} bytes; maximum is {EMBEDDING_MAX_JOB_BYTES} bytes"
            )));
        }
        if texts.len() > DEFAULT_MAX_BATCH_SIZE {
            return Err(lattice_embed::EmbedError::InvalidInput(format!(
                "batch size {} exceeds maximum {DEFAULT_MAX_BATCH_SIZE}",
                texts.len()
            )));
        }
        if let Some(text) = texts.iter().find(|text| text.len() > MAX_TEXT_CHARS) {
            return Err(lattice_embed::EmbedError::TextTooLong {
                length: text.len(),
                max: MAX_TEXT_CHARS,
            });
        }
        Ok(input_bytes)
    }

    fn worker(&self) -> lattice_embed::Result<&SyncSender<EmbeddingJob>> {
        self.worker
            .get_or_init(|| {
                let (sender, receiver) = mpsc::sync_channel(EMBEDDING_QUEUE_CAPACITY);
                let inner = Arc::clone(&self.inner);
                let runtime = tokio::runtime::Handle::current();
                std::thread::Builder::new()
                    .name("khive-embedding".to_owned())
                    .spawn(move || Self::run_worker(inner, runtime, receiver))
                    .map(|_| sender)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| lattice_embed::EmbedError::Internal(error.clone()))
    }

    fn run_worker(
        inner: Arc<S>,
        runtime: tokio::runtime::Handle,
        receiver: Receiver<EmbeddingJob>,
    ) {
        while let Ok(job) = receiver.recv() {
            if job.reply.is_closed() {
                continue;
            }
            let result = runtime.block_on(async {
                match job.call {
                    EmbeddingCall::Generic => inner.embed(&job.texts, job.model).await,
                    EmbeddingCall::Query => inner.embed_query(&job.texts, job.model).await,
                    EmbeddingCall::Passage => inner.embed_passage(&job.texts, job.model).await,
                }
            });
            let _ = job.reply.send(result);
        }
    }

    async fn run(
        &self,
        texts: &[String],
        model: EmbeddingModel,
        call: EmbeddingCall,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        let input_bytes = Self::input_bytes(texts)?;
        let sender = self.worker()?;
        let in_flight = InFlightBytes::reserve(
            Arc::clone(&self.in_flight_bytes),
            self.byte_budget,
            input_bytes,
        )?;
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let job = EmbeddingJob {
            texts: texts.to_vec(),
            model,
            call,
            reply,
            _in_flight: in_flight,
        };
        sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => {
                lattice_embed::EmbedError::Internal("embedding worker queue is full".to_owned())
            }
            TrySendError::Disconnected(_) => lattice_embed::EmbedError::Internal(
                "embedding worker channel is disconnected".to_owned(),
            ),
        })?;
        receiver
            .await
            .map_err(|error| lattice_embed::EmbedError::Internal(error.to_string()))?
    }
}

#[async_trait]
impl<S: EmbeddingService + 'static> EmbeddingService for BlockingEmbeddingService<S> {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.run(texts, model, EmbeddingCall::Generic).await
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.run(texts, model, EmbeddingCall::Query).await
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.run(texts, model, EmbeddingCall::Passage).await
    }

    fn model_config(&self, model: EmbeddingModel) -> lattice_embed::ModelConfig {
        self.inner.model_config(model)
    }

    fn supports_model(&self, model: EmbeddingModel) -> bool {
        self.inner.supports_model(model)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

/// A source that can produce an [`EmbeddingService`] by name.
///
/// Packs implement this trait to register custom embedding backends.
/// The runtime calls [`build`](EmbedderProvider::build) lazily — once per
/// process per model — and caches the result. Subsequent calls to
/// `KhiveRuntime::embedder(name)` are cheap.
///
/// Built-in lattice models are registered automatically via
/// [`LatticeEmbedderProvider`]; packs need not re-register them.
#[async_trait]
pub trait EmbedderProvider: Send + Sync {
    /// Stable, case-sensitive name for this embedder.
    ///
    /// Must be unique across all registered providers. The name is used as
    /// the key in `KhiveRuntime::embedder(name)` lookups and as the storage
    /// table suffix for vector indices. Use the model's canonical short form
    /// (e.g. `"all-minilm-l6-v2"`, `"my-custom-encoder"`).
    fn name(&self) -> &str;

    /// Output vector dimension for this embedder.
    ///
    /// Must be consistent with what [`build`](Self::build) produces.
    /// The runtime uses this to pre-register the vector store columns.
    fn dimensions(&self) -> usize;

    /// Construct the underlying [`EmbeddingService`].
    ///
    /// Called at most once per process. The result is cached in a
    /// [`OnceCell`]; concurrent callers block on the first call and share
    /// the result thereafter.
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>>;
}

/// An entry in the [`EmbedderRegistry`] combining a provider with its
/// lazy-initialized service.
pub(crate) struct EmbedderEntry {
    provider: Arc<dyn EmbedderProvider>,
    cell: Arc<OnceCell<Arc<dyn EmbeddingService>>>,
}

impl Clone for EmbedderEntry {
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            cell: Arc::clone(&self.cell),
        }
    }
}

/// Registry of named [`EmbedderProvider`] instances.
///
/// Built during `KhiveRuntime` construction and optionally extended by packs
/// via [`crate::KhiveRuntime::register_embedder`]. The registry is internally
/// reference-counted so `KhiveRuntime::clone()` shares the same providers
/// and cached service instances.
#[derive(Clone, Default)]
pub struct EmbedderRegistry {
    entries: HashMap<String, EmbedderEntry>,
}

impl EmbedderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a provider.
    ///
    /// If a provider with the same [`name`](EmbedderProvider::name) already
    /// exists, it is replaced (last-writer wins) and any cached service is
    /// discarded, since pack registration order is not guaranteed and packs
    /// may legitimately override a default model under the same name.
    /// Callers needing strict collision detection should check
    /// [`names`](Self::names) before registering.
    pub fn register<P: EmbedderProvider + 'static>(&mut self, provider: P) {
        let name = provider.name().to_owned();
        self.entries.insert(
            name,
            EmbedderEntry {
                provider: Arc::new(provider),
                cell: Arc::new(OnceCell::new()),
            },
        );
    }

    /// Look up a provider by name.
    pub fn get_provider(&self, name: &str) -> Option<&dyn EmbedderProvider> {
        self.entries.get(name).map(|e| e.provider.as_ref())
    }

    /// Returns `true` if a provider with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Names of all registered providers, in unspecified order.
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Return a cloned entry for `name` without holding any lock.
    ///
    /// The caller can then call [`EmbedderEntry::resolve`] without holding
    /// a lock — this avoids holding a `RwLockGuard` across `await` points.
    /// Returns `None` if `name` is not registered.
    pub(crate) fn get_entry(&self, name: &str) -> Option<EmbedderEntry> {
        self.entries.get(name).cloned()
    }

    /// Lazily resolve a registered provider to its live [`EmbeddingService`].
    ///
    /// Returns [`RuntimeError::UnknownModel`] if `name` is not registered.
    /// The first call for a given name triggers [`EmbedderProvider::build`];
    /// subsequent calls return the cached `Arc`.
    ///
    /// Prefer [`crate::KhiveRuntime::embedder`] over calling this directly from pack
    /// handlers — the runtime method handles alias resolution and error mapping.
    pub async fn get_service(&self, name: &str) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| RuntimeError::UnknownModel(name.to_string()))?
            .clone();

        entry.resolve().await
    }
}

impl EmbedderEntry {
    /// Lazily initialise and return the embedding service for this entry.
    ///
    /// The `OnceCell` guarantees that `build` is called at most once even
    /// under concurrent access. Callers hold no external lock while awaiting.
    ///
    /// Returns `RuntimeError` if `build()` fails, rather than panicking.
    pub(crate) async fn resolve(self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        // `OnceCell` has no fallible init, so failure is handled manually; a losing
        // racer's build() result is simply discarded since both are equivalent.
        if let Some(svc) = self.cell.get() {
            return Ok(Arc::clone(svc));
        }
        let svc = self.provider.build().await.map_err(|e| {
            crate::error::RuntimeError::Internal(format!(
                "EmbedderProvider '{}' build() failed: {e}",
                self.provider.name()
            ))
        })?;
        // A losing `set` (raced by another task) is fine: the two results are equivalent.
        let _ = self.cell.set(Arc::clone(&svc));
        Ok(svc)
    }
}

// ── LatticeEmbedderProvider ───────────────────────────────────────────────────

/// Adapter that wraps a [`lattice_embed::EmbeddingModel`] as an
/// [`EmbedderProvider`].
///
/// All built-in models (MiniLM, paraphrase-multilingual, BGE variants, etc.)
/// are registered as `LatticeEmbedderProvider` instances during
/// `KhiveRuntime` construction. External callers do not need to use this type
/// unless they are constructing a custom registry from scratch.
pub struct LatticeEmbedderProvider {
    model: EmbeddingModel,
    /// Cached `to_string()` result so `name()` can return `&str`.
    name: String,
}

impl LatticeEmbedderProvider {
    /// Create a new provider wrapping the given lattice model.
    pub fn new(model: EmbeddingModel) -> Self {
        let name = model.to_string();
        Self { model, name }
    }
}

#[async_trait]
impl EmbedderProvider for LatticeEmbedderProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.model.dimensions()
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        let native = Arc::new(NativeEmbeddingService::with_model(self.model));
        let cached = Arc::new(CachedEmbeddingService::with_default_cache(native));
        Ok(Arc::new(BlockingEmbeddingService::new(cached)) as Arc<dyn EmbeddingService>)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    struct ConstVecProvider {
        name: String,
        dims: usize,
        build_calls: Arc<AtomicUsize>,
    }

    impl ConstVecProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                name: name.to_owned(),
                dims,
                build_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    /// A trivial embedding service that returns a constant vector of `1.0`s.
    /// The `model` parameter is ignored — this service always returns the
    /// same synthetic vector regardless of which model is requested.
    struct ConstVecService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for ConstVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "const-vec-service"
        }
    }

    #[async_trait]
    impl EmbedderProvider for ConstVecProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(ConstVecService { dims: self.dims }))
        }
    }

    struct FirstLoadBlockingService {
        loaded: AtomicBool,
    }

    #[async_trait]
    impl EmbeddingService for FirstLoadBlockingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> lattice_embed::Result<Vec<Vec<f32>>> {
            if !self.loaded.swap(true, Ordering::SeqCst) {
                tokio::task::spawn_blocking(|| {})
                    .await
                    .map_err(|error| lattice_embed::EmbedError::Internal(error.to_string()))?;
            }
            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "first-load-blocking-service"
        }
    }

    struct BlockingTestService {
        calls: Mutex<Vec<String>>,
        entered: AtomicUsize,
        release: (Mutex<bool>, Condvar),
        thread_ids: Mutex<HashSet<std::thread::ThreadId>>,
    }

    impl BlockingTestService {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                entered: AtomicUsize::new(0),
                release: (Mutex::new(false), Condvar::new()),
                thread_ids: Mutex::new(HashSet::new()),
            }
        }

        fn release(&self) {
            *self
                .release
                .0
                .lock()
                .expect("release lock must not be poisoned") = true;
            self.release.1.notify_all();
        }
    }

    #[async_trait]
    impl EmbeddingService for BlockingTestService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> lattice_embed::Result<Vec<Vec<f32>>> {
            let text = texts.first().cloned().unwrap_or_default();
            self.thread_ids
                .lock()
                .expect("thread id lock must not be poisoned")
                .insert(std::thread::current().id());
            self.calls
                .lock()
                .expect("call lock must not be poisoned")
                .push(text.clone());
            self.entered.fetch_add(1, Ordering::Release);

            if text != "later" {
                let (released, wake) = &self.release;
                let guard = released.lock().expect("release lock must not be poisoned");
                let _guard = wake
                    .wait_while(guard, |released| !*released)
                    .expect("release lock must not be poisoned");
            }

            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "blocking-test-service"
        }
    }

    #[test]
    fn blocking_adapter_first_use_completes_with_single_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("current-thread runtime must build");
        let service = BlockingEmbeddingService::new(Arc::new(FirstLoadBlockingService {
            loaded: AtomicBool::new(false),
        }));

        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                service.embed(&["first use".to_owned()], EmbeddingModel::default()),
            )
            .await
        });
        runtime.shutdown_timeout(Duration::from_secs(1));

        let embeddings = result
            .expect("first-use embedding must not exhaust the blocking pool")
            .expect("first-use embedding must succeed");
        assert_eq!(embeddings, vec![vec![1.0]]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn blocking_adapter_uses_one_worker_for_concurrent_calls() {
        const CALL_COUNT: usize = 32;
        let inner = Arc::new(BlockingTestService::new());
        let service = Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner)));
        let mut calls = Vec::with_capacity(CALL_COUNT);

        for index in 0..CALL_COUNT {
            let service = Arc::clone(&service);
            calls.push(tokio::spawn(async move {
                service
                    .embed(&[format!("request-{index}")], EmbeddingModel::default())
                    .await
            }));
        }

        let _ = tokio::time::timeout(Duration::from_millis(250), async {
            while inner.entered.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        inner.release();

        for call in calls {
            call.await
                .expect("embedding task must not panic")
                .expect("embedding call must succeed");
        }
        assert_eq!(
            inner
                .thread_ids
                .lock()
                .expect("thread id lock must not be poisoned")
                .len(),
            1,
            "concurrent calls must share one native worker thread"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_adapter_skips_timed_out_call_and_serves_later_call() {
        let inner = Arc::new(BlockingTestService::new());
        let service = Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner)));

        let first_service = Arc::clone(&service);
        let first = tokio::spawn(async move {
            first_service
                .embed(&["first".to_owned()], EmbeddingModel::default())
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.entered.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first embedding call must enter the native service");

        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            service.embed(&["abandoned".to_owned()], EmbeddingModel::default()),
        )
        .await;
        let later_service = Arc::clone(&service);
        let later = tokio::spawn(async move {
            later_service
                .embed(&["later".to_owned()], EmbeddingModel::default())
                .await
        });
        inner.release();

        first
            .await
            .expect("first embedding task must not panic")
            .expect("first embedding call must succeed");
        let later_result = tokio::time::timeout(Duration::from_secs(1), later)
            .await
            .expect("later embedding call must be served")
            .expect("later embedding task must not panic")
            .expect("later embedding call must succeed");

        assert!(abandoned.is_err(), "queued embedding call must time out");
        assert_eq!(later_result, vec![vec![1.0]]);
        assert_eq!(
            *inner.calls.lock().expect("call lock must not be poisoned"),
            vec!["first".to_owned(), "later".to_owned()],
            "the worker must skip a queued call whose receiver is closed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_adapter_rejects_call_when_queue_is_full() {
        let inner = Arc::new(BlockingTestService::new());
        let service = Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner)));
        let first_service = Arc::clone(&service);
        let first = tokio::spawn(async move {
            first_service
                .embed(&["first".to_owned()], EmbeddingModel::default())
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.entered.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first embedding call must occupy the native worker");

        let sender = service.worker().expect("worker must be running");
        let mut queued_receivers = Vec::with_capacity(EMBEDDING_QUEUE_CAPACITY);
        for index in 0..EMBEDDING_QUEUE_CAPACITY {
            let (reply, receiver) = tokio::sync::oneshot::channel();
            let queued = sender.try_send(EmbeddingJob {
                texts: vec![format!("queued-{index}")],
                model: EmbeddingModel::default(),
                call: EmbeddingCall::Generic,
                reply,
                _in_flight: InFlightBytes::reserve(
                    Arc::clone(&service.in_flight_bytes),
                    service.byte_budget,
                    0,
                )
                .expect("zero-byte test job must fit the byte budget"),
            });
            assert!(queued.is_ok(), "bounded queue must accept its capacity");
            queued_receivers.push(receiver);
        }

        let overflow = tokio::time::timeout(
            Duration::from_millis(100),
            service.embed(&["overflow".to_owned()], EmbeddingModel::default()),
        )
        .await
        .expect("a full embedding queue must fail without waiting")
        .expect_err("a full embedding queue must return an embedding error");

        drop(queued_receivers);
        inner.release();
        first
            .await
            .expect("first embedding task must not panic")
            .expect("first embedding call must succeed");
        assert!(
            overflow.to_string().contains("queue is full"),
            "queue saturation must use the embedding failure path: {overflow}"
        );
    }

    #[tokio::test]
    async fn blocking_adapter_rejects_oversized_job_before_enqueue() {
        let service = BlockingEmbeddingService::new(Arc::new(ConstVecService { dims: 1 }));
        let oversized =
            "x".repeat(lattice_embed::DEFAULT_MAX_BATCH_SIZE * lattice_embed::MAX_TEXT_CHARS + 1);

        let error = service
            .embed(&[oversized], EmbeddingModel::default())
            .await
            .expect_err("an oversized embedding job must be rejected");

        assert!(
            error.to_string().contains("embedding job input"),
            "oversized admission must use the embedding error path: {error}"
        );
        assert!(
            service.worker.get().is_none(),
            "oversized work must be rejected before the worker queue is initialized"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_adapter_byte_budget_rejects_excess_and_queued_jobs_complete() {
        const ADMITTED_JOBS: usize = 4;
        let byte_budget = ADMITTED_JOBS * EMBEDDING_MAX_JOB_BYTES;
        let inner = Arc::new(BlockingTestService::new());
        let service = Arc::new(BlockingEmbeddingService::with_byte_budget(
            Arc::clone(&inner),
            byte_budget,
        ));
        let max_texts = Arc::new(vec![
            "x".repeat(lattice_embed::MAX_TEXT_CHARS);
            lattice_embed::DEFAULT_MAX_BATCH_SIZE
        ]);
        let mut admitted = Vec::with_capacity(ADMITTED_JOBS);

        for _ in 0..ADMITTED_JOBS {
            let service = Arc::clone(&service);
            let texts = Arc::clone(&max_texts);
            admitted.push(tokio::spawn(async move {
                service.embed(&texts, EmbeddingModel::default()).await
            }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.in_flight_bytes.load(Ordering::Acquire) < byte_budget {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all jobs within the byte budget must be admitted");

        let overflow = tokio::time::timeout(
            Duration::from_millis(100),
            service.embed(&max_texts, EmbeddingModel::default()),
        )
        .await
        .expect("a byte-budget overflow must fail without waiting")
        .expect_err("a byte-budget overflow must return an embedding error");

        inner.release();
        for call in admitted {
            call.await
                .expect("admitted embedding task must not panic")
                .expect("admitted embedding job must complete");
        }
        assert!(
            overflow.to_string().contains("byte budget"),
            "byte saturation must use the embedding failure path: {overflow}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_adapter_releases_byte_budget_after_completion_and_skipped_job() {
        let byte_budget = "first".len() + "abandoned".len();
        let inner = Arc::new(BlockingTestService::new());
        let service = Arc::new(BlockingEmbeddingService::with_byte_budget(
            Arc::clone(&inner),
            byte_budget,
        ));

        let first_service = Arc::clone(&service);
        let first = tokio::spawn(async move {
            first_service
                .embed(&["first".to_owned()], EmbeddingModel::default())
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.entered.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first embedding call must occupy the native worker");

        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            service.embed(&["abandoned".to_owned()], EmbeddingModel::default()),
        )
        .await;
        assert!(abandoned.is_err(), "queued embedding call must time out");
        assert_eq!(
            service.in_flight_bytes.load(Ordering::Acquire),
            byte_budget,
            "running and queued jobs must both consume the byte budget"
        );

        inner.release();
        first
            .await
            .expect("first embedding task must not panic")
            .expect("first embedding call must succeed");
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.in_flight_bytes.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed and skipped jobs must release their byte reservations");

        let later = service
            .embed(&["later".to_owned()], EmbeddingModel::default())
            .await
            .expect("a later call must succeed after the byte budget is released");
        assert_eq!(later, vec![vec![1.0]]);
    }

    #[test]
    fn register_and_get_provider_round_trip() {
        let mut reg = EmbedderRegistry::new();
        reg.register(ConstVecProvider::new("mock-384", 384));

        assert!(reg.contains("mock-384"), "registered name must be present");
        let provider = reg.get_provider("mock-384").expect("provider must exist");
        assert_eq!(provider.name(), "mock-384");
        assert_eq!(provider.dimensions(), 384);
    }

    #[test]
    fn duplicate_name_last_wins() {
        let mut reg = EmbedderRegistry::new();
        reg.register(ConstVecProvider::new("shared", 128));
        reg.register(ConstVecProvider::new("shared", 256));

        let provider = reg.get_provider("shared").expect("provider must exist");
        assert_eq!(
            provider.dimensions(),
            256,
            "last registration must win; expected dims=256"
        );
    }

    #[test]
    fn names_returns_all_registered() {
        let mut reg = EmbedderRegistry::new();
        reg.register(ConstVecProvider::new("model-a", 64));
        reg.register(ConstVecProvider::new("model-b", 128));
        reg.register(ConstVecProvider::new("model-c", 256));

        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["model-a", "model-b", "model-c"]);
    }

    #[tokio::test]
    async fn get_service_unknown_name_returns_error() {
        let reg = EmbedderRegistry::new();
        let result = reg.get_service("does-not-exist").await;
        let err = result.err().expect("expected Err for unknown name, got Ok");
        assert!(
            matches!(err, RuntimeError::UnknownModel(ref n) if n == "does-not-exist"),
            "expected UnknownModel, got {err:?}"
        );
    }

    #[tokio::test]
    async fn get_service_calls_build_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let provider = ConstVecProvider {
            name: "cached-model".to_owned(),
            dims: 32,
            build_calls: Arc::clone(&counter),
        };
        let mut reg = EmbedderRegistry::new();
        reg.register(provider);

        let _ = reg.get_service("cached-model").await.unwrap();
        let _ = reg.get_service("cached-model").await.unwrap();
        let _ = reg.get_service("cached-model").await.unwrap();

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "build must be called exactly once regardless of get_service call count"
        );
    }
}
