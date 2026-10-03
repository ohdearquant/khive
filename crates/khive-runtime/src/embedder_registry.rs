//! EmbedderRegistry — pack-extensible embedding provider surface.
//!
//! Packs implement [`EmbedderProvider`] and register custom models via
//! [`crate::KhiveRuntime::register_embedder`]. Built-in lattice models are pre-registered
//! during runtime construction and require no opt-in.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingRole, EmbeddingService,
    NativeEmbeddingService, DEFAULT_MAX_BATCH_SIZE, MAX_TEXT_BYTES,
};
use tokio::sync::{mpsc, Notify, OnceCell};

use crate::error::{RuntimeError, RuntimeResult};

const ADMISSION_WAITING: u8 = 1;
const ADMISSION_ACCEPTED: u8 = 2;

struct EmbeddingAdmission {
    deadline: khive_storage::RequestReadDeadline,
    request: khive_storage::RequestReadContext,
    state: AtomicU8,
    changed: Notify,
}

impl EmbeddingAdmission {
    fn expired(&self) -> bool {
        tokio::time::Instant::now() >= self.deadline.async_at()
            || self.request.stop_reason().is_some()
    }
}

tokio::task_local! {
    static EMBEDDING_ADMISSION: Arc<EmbeddingAdmission>;
}

/// Bound only the built-in worker's pre-admission wait, above the lattice error
/// boundary. Providers that never enter that wait retain their own semantics.
pub(crate) async fn with_embedding_admission<T>(
    future: impl std::future::Future<Output = lattice_embed::Result<T>>,
) -> RuntimeResult<T> {
    let deadline = khive_storage::effective_request_read_deadline(
        khive_storage::RequestReadDeadline::after(khive_storage::request_read_timeout_from_env()),
    );
    let timeout = deadline
        .async_at()
        .saturating_duration_since(tokio::time::Instant::now());
    let admission = Arc::new(EmbeddingAdmission {
        deadline,
        request: khive_storage::capture_request_read_context(),
        state: AtomicU8::new(0),
        changed: Notify::new(),
    });
    EMBEDDING_ADMISSION
        .scope(Arc::clone(&admission), async move {
            tokio::pin!(future);
            let stopped = async {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline.async_at()) => {},
                    _ = admission.request.clone().wait_for_stop() => {},
                }
            };
            tokio::pin!(stopped);
            let mut bound_expired = false;
            loop {
                tokio::select! {
                    // A ready cache hit precedes admission, including an expired bound.
                    biased;
                    result = &mut future => return result.map_err(RuntimeError::from),
                    _ = admission.changed.notified() => {},
                    _ = &mut stopped, if !bound_expired => bound_expired = true,
                }
                match admission.state.load(Ordering::Acquire) {
                    ADMISSION_ACCEPTED => return future.await.map_err(RuntimeError::from),
                    ADMISSION_WAITING if bound_expired => {
                        return Err(RuntimeError::Storage(
                            khive_storage::StorageError::AdmissionTimeout {
                                operation: "embedding admission".into(),
                                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                                pool_identity: None,
                            },
                        ));
                    }
                    // A provider may enter the owned adapter later. Keep the
                    // original expired bound without timing out its other work.
                    _ => {}
                }
            }
        })
        .await
}

#[derive(Clone, Copy)]
enum EmbeddingCall {
    Generic,
    Query,
    Passage,
}

const EMBEDDING_QUEUE_CAPACITY: usize = 32;
const EMBEDDING_MAX_JOB_BYTES: usize = DEFAULT_MAX_BATCH_SIZE * MAX_TEXT_BYTES;
// 32 queue slots × the normal 128-text batch × 32 KiB/text = 128 MiB in flight.
const EMBEDDING_QUEUE_BYTE_BUDGET: usize = EMBEDDING_QUEUE_CAPACITY * 128 * MAX_TEXT_BYTES;

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
    worker: OnceLock<Result<mpsc::Sender<EmbeddingJob>, String>>,
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
        if let Some(text) = texts.iter().find(|text| text.len() > MAX_TEXT_BYTES) {
            return Err(lattice_embed::EmbedError::TextTooLong {
                length: text.len(),
                max: MAX_TEXT_BYTES,
            });
        }
        Ok(input_bytes)
    }

    fn worker(&self) -> lattice_embed::Result<&mpsc::Sender<EmbeddingJob>> {
        self.worker
            .get_or_init(|| {
                let (sender, receiver) = mpsc::channel(EMBEDDING_QUEUE_CAPACITY);
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
        mut receiver: mpsc::Receiver<EmbeddingJob>,
    ) {
        while let Some(job) = receiver.blocking_recv() {
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
        let admission = EMBEDDING_ADMISSION.try_with(Arc::clone).ok();
        let permit = if let Some(admission) = &admission {
            admission.state.store(ADMISSION_WAITING, Ordering::Release);
            admission.changed.notify_one();
            if admission.expired() {
                return std::future::pending().await;
            }
            sender.reserve().await.map_err(|_| {
                lattice_embed::EmbedError::Internal(
                    "embedding worker channel is disconnected".to_owned(),
                )
            })?
        } else {
            // Direct trait callers have no RuntimeResult boundary at which to
            // report a typed admission timeout, so retain finite fail-fast admission.
            sender.try_reserve().map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    lattice_embed::EmbedError::Internal("embedding worker queue is full".to_owned())
                }
                mpsc::error::TrySendError::Closed(_) => lattice_embed::EmbedError::Internal(
                    "embedding worker channel is disconnected".to_owned(),
                ),
            })?
        };
        // Capacity and expiry can become ready in one poll. Do not publish a
        // job after the original bound, even when reserve() returned a permit.
        if admission
            .as_ref()
            .is_some_and(|admission| admission.expired())
        {
            return std::future::pending().await;
        }
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
        if let Some(admission) = &admission {
            if admission.expired() {
                return std::future::pending().await;
            }
            admission.state.store(ADMISSION_ACCEPTED, Ordering::Release);
            admission.changed.notify_one();
        }
        permit.send(job);
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

    async fn embed_with_role(
        &self,
        texts: &[String],
        model: EmbeddingModel,
        role: EmbeddingRole,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        // The outer cache delegates raw caller text here; preparation belongs
        // to the native service, after the caller-input admission checks.
        let call = match role {
            EmbeddingRole::Generic => EmbeddingCall::Generic,
            EmbeddingRole::Query => EmbeddingCall::Query,
            EmbeddingRole::Passage => EmbeddingCall::Passage,
            _ => {
                return Err(lattice_embed::EmbedError::InvalidInput(
                    "unsupported embedding role".to_owned(),
                ))
            }
        };
        self.run(texts, model, call).await
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
    /// Only the runtime's built-in lattice provider has an audited document
    /// preparation path. Pack replacements, even under a built-in name, do not.
    audited_document_preparation: bool,
}

impl Clone for EmbedderEntry {
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            cell: Arc::clone(&self.cell),
            audited_document_preparation: self.audited_document_preparation,
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
        self.insert(provider, false);
    }

    /// Register the runtime-owned lattice adapter whose passage preparation is
    /// audited. This is deliberately not available to pack providers.
    pub(crate) fn register_builtin(&mut self, provider: LatticeEmbedderProvider) {
        self.insert(provider, true);
    }

    /// Test-only attested provider. The wrapper below owns passage preparation,
    /// so a fake backend cannot change the bytes whose digest is recorded.
    #[cfg(feature = "test-internals")]
    pub fn register_test_audited<P: EmbedderProvider + 'static>(
        &mut self,
        model: EmbeddingModel,
        provider: P,
    ) {
        assert_eq!(provider.name(), model.to_string());
        self.insert(TestAuditedProvider { provider }, true);
    }

    fn insert<P: EmbedderProvider + 'static>(
        &mut self,
        provider: P,
        audited_document_preparation: bool,
    ) {
        let name = provider.name().to_owned();
        self.entries.insert(
            name,
            EmbedderEntry {
                provider: Arc::new(provider),
                cell: Arc::new(OnceCell::new()),
                audited_document_preparation,
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

        Ok(entry.resolve().await?.0)
    }
}

#[cfg(feature = "test-internals")]
struct TestAuditedProvider<P> {
    provider: P,
}

#[cfg(feature = "test-internals")]
#[async_trait]
impl<P: EmbedderProvider> EmbedderProvider for TestAuditedProvider<P> {
    fn name(&self) -> &str {
        self.provider.name()
    }

    fn dimensions(&self) -> usize {
        self.provider.dimensions()
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(TestAuditedService(self.provider.build().await?)))
    }
}

/// Deliberately does not delegate `embed_passage`: the lattice trait default
/// applies the model's document instruction before calling `embed`, exactly
/// as the audited built-in path does. Test providers supply only fake vectors.
#[cfg(feature = "test-internals")]
struct TestAuditedService(Arc<dyn EmbeddingService>);

#[cfg(feature = "test-internals")]
#[async_trait]
impl EmbeddingService for TestAuditedService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.0.embed(texts, model).await
    }

    fn supports_model(&self, model: EmbeddingModel) -> bool {
        self.0.supports_model(model)
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }
}

impl EmbedderEntry {
    pub(crate) fn has_audited_document_preparation(&self) -> bool {
        self.audited_document_preparation
    }

    /// Lazily initialise and return the embedding service for this entry.
    ///
    /// `OnceCell::get_or_try_init` single-flights concurrent cold callers: only
    /// one of them runs [`EmbedderProvider::build`], and every other waiter
    /// receives the same result once it completes. Only the caller whose task
    /// actually ran `build()` gets back `Some(duration)`; every other caller
    /// (cache hit or wait-for-in-flight-build) gets `None`.
    ///
    /// Returns `RuntimeError` if `build()` fails, rather than panicking. On
    /// failure the cell stays unset, so a later call retries the build.
    pub(crate) async fn resolve(self) -> RuntimeResult<(Arc<dyn EmbeddingService>, Option<i64>)> {
        let mut own_init_duration_us: Option<i64> = None;
        let provider = Arc::clone(&self.provider);
        let init_duration_us = &mut own_init_duration_us;
        let svc = self
            .cell
            .get_or_try_init(|| async move {
                let init_start = std::time::Instant::now();
                let svc = provider.build().await.map_err(|e| {
                    crate::error::RuntimeError::Internal(format!(
                        "EmbedderProvider '{}' build() failed: {e}",
                        provider.name()
                    ))
                })?;
                *init_duration_us = Some(init_start.elapsed().as_micros() as i64);
                Ok::<_, RuntimeError>(svc)
            })
            .await?;
        Ok((Arc::clone(svc), own_init_duration_us))
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
        native.ensure_loaded().await?;
        Ok(cached_blocking_service(native))
    }
}

/// Keep result-cache lookup outside worker admission for every retrieval role.
fn cached_blocking_service<S: EmbeddingService + 'static>(
    inner: Arc<S>,
) -> Arc<dyn EmbeddingService> {
    let blocking = Arc::new(BlockingEmbeddingService::new(inner));
    Arc::new(CachedEmbeddingService::with_default_cache(blocking))
}

#[cfg(test)]
#[path = "embedding_cache_admission_tests.rs"]
mod cache_admission_tests;

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

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
    pub(super) struct ConstVecService {
        pub(super) dims: usize,
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

    #[test]
    fn builtin_input_attestation_stays_with_cloned_entry_after_canonical_name_override() {
        let model = EmbeddingModel::MultilingualE5Small;
        let name = model.to_string();
        let mut registry = EmbedderRegistry::new();
        registry.register_builtin(LatticeEmbedderProvider::new(model));
        let builtin_entry = registry.get_entry(&name).expect("builtin entry");
        assert!(builtin_entry.has_audited_document_preparation());

        registry.register(ConstVecProvider::new(&name, model.dimensions()));
        let replacement_entry = registry.get_entry(&name).expect("replacement entry");
        assert!(builtin_entry.has_audited_document_preparation());
        assert!(!replacement_entry.has_audited_document_preparation());
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

    pub(super) struct BlockingTestService {
        pub(super) calls: Mutex<Vec<String>>,
        pub(super) entered: AtomicUsize,
        release: (Mutex<bool>, Condvar),
        pub(super) thread_ids: Mutex<HashSet<std::thread::ThreadId>>,
    }

    impl BlockingTestService {
        pub(super) fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                entered: AtomicUsize::new(0),
                release: (Mutex::new(false), Condvar::new()),
                thread_ids: Mutex::new(HashSet::new()),
            }
        }

        pub(super) fn release(&self) {
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

    pub(super) struct ReleaseWorkerOnDrop(pub(super) Arc<BlockingTestService>);

    impl Drop for ReleaseWorkerOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct ServiceProvider(Arc<dyn EmbeddingService>);

    #[async_trait]
    impl EmbedderProvider for ServiceProvider {
        fn name(&self) -> &str {
            "queue-test"
        }
        fn dimensions(&self) -> usize {
            1
        }
        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::clone(&self.0))
        }
    }

    pub(super) fn queue_runtime(service: Arc<dyn EmbeddingService>) -> crate::KhiveRuntime {
        let runtime = crate::KhiveRuntime::memory().expect("memory runtime");
        runtime.register_embedder(ServiceProvider(service));
        runtime
    }

    pub(super) async fn poll_once<F: std::future::Future + ?Sized>(
        mut future: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
    }

    pub(super) async fn wait_for_entered(inner: &BlockingTestService, count: usize) {
        let watchdog = std::time::Instant::now() + Duration::from_secs(2);
        while inner.entered.load(Ordering::Acquire) < count {
            assert!(
                std::time::Instant::now() < watchdog,
                "setup watchdog: native worker did not enter"
            );
            tokio::task::yield_now().await;
        }
    }

    async fn drive_until_entered<F: std::future::Future + ?Sized>(
        mut future: std::pin::Pin<&mut F>,
        inner: &BlockingTestService,
        count: usize,
    ) {
        let watchdog = std::time::Instant::now() + Duration::from_secs(2);
        while inner.entered.load(Ordering::Acquire) < count {
            assert!(
                std::time::Instant::now() < watchdog,
                "setup watchdog: driven runtime call did not reach the native worker"
            );
            assert!(
                poll_once(future.as_mut()).await.is_pending(),
                "held inference must remain pending while the runtime call is driven"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_embedding_waits_for_capacity_and_drains_n_plus_one_calls() {
        let inner = Arc::new(BlockingTestService::new());
        let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
        let service = Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner)));
        let runtime = queue_runtime(service.clone());
        runtime
            .embedder("queue-test")
            .await
            .expect("resolve provider before the admission fixture");
        let first_text = vec!["first".to_owned()];
        let mut first = Box::pin(runtime.embed_batch_with_model("queue-test", &first_text));
        assert!(poll_once(first.as_mut()).await.is_pending());
        drive_until_entered(first.as_mut(), &inner, 1).await;

        let texts: Vec<_> = (0..EMBEDDING_QUEUE_CAPACITY)
            .map(|index| vec![format!("queued-{index}")])
            .collect();
        let mut queued = Vec::new();
        for text in &texts {
            let mut call = Box::pin(runtime.embed_batch_with_model("queue-test", text));
            assert!(
                poll_once(call.as_mut()).await.is_pending(),
                "the bounded queue must admit its N actual runtime calls"
            );
            queued.push(call);
        }
        assert_eq!(
            service.in_flight_bytes.load(Ordering::Acquire),
            first_text.iter().map(String::len).sum::<usize>()
                + texts.iter().flatten().map(String::len).sum::<usize>(),
            "the held worker and N queued runtime jobs must own their exact input bytes"
        );
        let overflow_text = vec!["overflow".to_owned()];
        let mut overflow = Box::pin(runtime.embed_batch_with_model("queue-test", &overflow_text));
        let overflow_before_release = poll_once(overflow.as_mut()).await;
        inner.release();
        assert!(
            overflow_before_release.is_pending(),
            "request N+1 must wait for a slot instead of failing: {overflow_before_release:?}"
        );
        assert_eq!(first.await.unwrap(), vec![vec![1.0]]);
        for call in queued {
            assert_eq!(call.await.unwrap(), vec![vec![1.0]]);
        }
        assert_eq!(overflow.await.unwrap(), vec![vec![1.0]]);
        assert_eq!(
            inner.entered.load(Ordering::Acquire),
            EMBEDDING_QUEUE_CAPACITY + 2
        );
        assert_eq!(inner.thread_ids.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_embedding_expired_admission_is_retryable_and_never_enqueued() {
        let inner = Arc::new(BlockingTestService::new());
        let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
        let service = Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner)));
        let runtime = queue_runtime(service.clone());
        runtime
            .embedder("queue-test")
            .await
            .expect("resolve provider before the admission fixture");
        let first_text = vec!["first".to_owned()];
        let mut first = Box::pin(runtime.embed_batch_with_model("queue-test", &first_text));
        assert!(poll_once(first.as_mut()).await.is_pending());
        drive_until_entered(first.as_mut(), &inner, 1).await;
        let texts: Vec<_> = (0..EMBEDDING_QUEUE_CAPACITY)
            .map(|index| vec![format!("queued-{index}")])
            .collect();
        let mut queued = Vec::new();
        for text in &texts {
            let mut call = Box::pin(runtime.embed_batch_with_model("queue-test", text));
            assert!(poll_once(call.as_mut()).await.is_pending());
            queued.push(call);
        }
        assert_eq!(
            service.in_flight_bytes.load(Ordering::Acquire),
            first_text.iter().map(String::len).sum::<usize>()
                + texts.iter().flatten().map(String::len).sum::<usize>(),
            "the held worker and N queued runtime jobs must own their exact input bytes"
        );
        let deadline = khive_storage::RequestReadDeadline::after(Duration::from_millis(100));
        let mut expired = Box::pin(khive_storage::scope_request_read_deadline_at(
            deadline,
            runtime.embed_with_model("queue-test", "expired"),
        ));
        assert!(
            poll_once(expired.as_mut()).await.is_pending(),
            "an unexpired full-queue call must wait"
        );
        tokio::time::advance(Duration::from_millis(100)).await;
        let result = poll_once(expired.as_mut()).await;
        inner.release();
        let error = match result {
            std::task::Poll::Ready(Err(error)) => error,
            other => panic!("expired admission must produce a typed runtime error: {other:?}"),
        };
        assert!(
            matches!(&error, RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
            operation, timeout_ms: 100, pool_identity: None,
        }) if operation == "embedding admission"),
            "{error:?}"
        );
        assert!(
            error.retryable_failure_context().is_some(),
            "pre-admission expiry must use the existing retryable classification"
        );
        let projected = crate::error_projection::runtime_error_value(
            error,
            crate::DomainDisposition::NotCommitted,
        );
        assert_eq!(projected["retryable"], true);
        assert_eq!(projected["operation"], "embedding admission");
        assert_eq!(first.await.unwrap(), vec![vec![1.0]]);
        for call in queued {
            assert_eq!(call.await.unwrap(), vec![vec![1.0]]);
        }
        assert!(
            !inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|text| text == "expired"),
            "an expired waiter must never reach inference"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_embedding_earlier_absolute_bound_is_not_renewed_when_slot_is_ready() {
        let inner = Arc::new(BlockingTestService::new());
        let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
        let runtime = queue_runtime(Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner))));
        runtime
            .embedder("queue-test")
            .await
            .expect("resolve provider before the admission deadline");
        let deadline = khive_storage::RequestReadDeadline::after(Duration::from_millis(100));
        tokio::time::advance(Duration::from_millis(100)).await;
        let mut call = Box::pin(khive_storage::scope_request_read_deadline_at(
            deadline,
            runtime.embed_with_model("queue-test", "expired-ready-slot"),
        ));
        let result = poll_once(call.as_mut()).await;
        inner.release();
        assert!(
            matches!(
                result,
                std::task::Poll::Ready(Err(RuntimeError::Storage(
                    khive_storage::StorageError::AdmissionTimeout { timeout_ms: 0, .. }
                )))
            ),
            "an already-expired original bound must refuse even an available slot: {result:?}"
        );
        assert_eq!(
            inner.entered.load(Ordering::Acquire),
            0,
            "capacity becoming ready must not bypass expiry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_embedding_all_six_call_families_refuse_expired_admission() {
        let inner = Arc::new(BlockingTestService::new());
        let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
        let runtime = queue_runtime(Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner))));
        let texts = vec!["later".to_owned()];
        for family in 0..6 {
            let result = khive_storage::scope_request_read_deadline(Duration::ZERO, async {
                match family {
                    0 => runtime.embed_with_model("queue-test", "later").await,
                    1 => {
                        runtime
                            .embed_document_with_model("queue-test", "later")
                            .await
                    }
                    2 => runtime.embed_query_with_model("queue-test", "later").await,
                    3 => runtime
                        .embed_batch_with_model("queue-test", &texts)
                        .await
                        .map(|vectors| vectors[0].clone()),
                    4 => runtime
                        .embed_document_batch_with_model("queue-test", &texts)
                        .await
                        .map(|vectors| vectors[0].clone()),
                    _ => runtime
                        .embed_query_batch_with_model("queue-test", &texts)
                        .await
                        .map(|vectors| vectors[0].clone()),
                }
            })
            .await;
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Storage(
                        khive_storage::StorageError::AdmissionTimeout { .. }
                    ))
                ),
                "family {family} must reach the actual runtime admission boundary: {result:?}"
            );
        }
        assert_eq!(inner.entered.load(Ordering::Acquire), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_embedding_admitted_inference_is_not_reported_as_admission_timeout() {
        let inner = Arc::new(BlockingTestService::new());
        let _release = ReleaseWorkerOnDrop(Arc::clone(&inner));
        let runtime = queue_runtime(Arc::new(BlockingEmbeddingService::new(Arc::clone(&inner))));
        runtime
            .embedder("queue-test")
            .await
            .expect("resolve provider before the admission fixture");
        let mut call = Box::pin(khive_storage::scope_request_read_deadline(
            Duration::from_millis(100),
            runtime.embed_with_model("queue-test", "first"),
        ));
        assert!(poll_once(call.as_mut()).await.is_pending());
        drive_until_entered(call.as_mut(), &inner, 1).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        let after_deadline = poll_once(call.as_mut()).await;
        inner.release();
        assert!(after_deadline.is_pending(),
            "already-running inference cannot be called a pre-admission refusal: {after_deadline:?}");
        assert_eq!(call.await.unwrap(), vec![1.0]);
    }

    struct CustomWaitingService {
        release: Notify,
    }

    #[async_trait]
    impl EmbeddingService for CustomWaitingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> lattice_embed::Result<Vec<Vec<f32>>> {
            self.release.notified().await;
            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "custom-waiting"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_custom_inference_is_not_reported_as_builtin_admission_timeout() {
        let inner = Arc::new(CustomWaitingService {
            release: Notify::new(),
        });
        let runtime = queue_runtime(inner.clone());
        let mut call = Box::pin(khive_storage::scope_request_read_deadline(
            Duration::from_millis(100),
            runtime.embed_with_model("queue-test", "custom"),
        ));
        assert!(poll_once(call.as_mut()).await.is_pending());
        tokio::time::advance(Duration::from_millis(100)).await;
        let after_deadline = poll_once(call.as_mut()).await;
        inner.release.notify_one();
        assert!(
            after_deadline.is_pending(),
            "custom inference did not enter the built-in pre-admission wait: {after_deadline:?}"
        );
        assert_eq!(call.await.unwrap(), vec![1.0]);
    }

    #[tokio::test]
    async fn blocking_adapter_rejects_oversized_job_before_enqueue() {
        let service = BlockingEmbeddingService::new(Arc::new(ConstVecService { dims: 1 }));
        let oversized =
            "x".repeat(lattice_embed::DEFAULT_MAX_BATCH_SIZE * lattice_embed::MAX_TEXT_BYTES + 1);

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
            "x".repeat(lattice_embed::MAX_TEXT_BYTES);
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

    struct SlowBuildProvider {
        name: String,
        dims: usize,
        build_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EmbedderProvider for SlowBuildProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Arc::new(ConstVecService { dims: self.dims }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_cold_resolutions_single_flight_one_build() {
        const CALLERS: usize = 16;
        let counter = Arc::new(AtomicUsize::new(0));
        let mut reg = EmbedderRegistry::new();
        reg.register(SlowBuildProvider {
            name: "cold-model".to_owned(),
            dims: 8,
            build_calls: Arc::clone(&counter),
        });
        let reg = Arc::new(reg);

        let mut callers = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let reg = Arc::clone(&reg);
            callers.push(tokio::spawn(
                async move { reg.get_service("cold-model").await },
            ));
        }

        for caller in callers {
            let service = caller
                .await
                .expect("resolution task must not panic")
                .expect("every concurrent cold resolution must receive a working service");
            assert_eq!(service.name(), "const-vec-service");
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "concurrent cold resolutions must share a single in-flight build()"
        );
    }
}
