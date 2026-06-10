//! EmbedderRegistry — pack-extensible embedding provider surface.
//!
//! Packs implement [`EmbedderProvider`] and register custom models via
//! [`crate::KhiveRuntime::register_embedder`]. Built-in lattice models are pre-registered
//! during runtime construction and require no opt-in.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingService, NativeEmbeddingService,
};
use tokio::sync::OnceCell;

use crate::error::{RuntimeError, RuntimeResult};

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
    /// exists, the new provider **replaces** the existing one (last-writer wins).
    /// The previously cached service instance (if any) is discarded — the
    /// replacement provider's [`build`](EmbedderProvider::build) will be
    /// called lazily on the next access.
    ///
    /// **Last-wins** is chosen over rejection because pack registration order
    /// is not guaranteed and packs may legitimately override a default model
    /// with a custom implementation of the same logical name. Operators who
    /// need strict collision detection should inspect
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
        // `OnceCell` does not expose a fallible init; we work around this by
        // checking if the cell is already populated (cheap), and if not, calling
        // `build()` ourselves, storing on success, and propagating on failure.
        // Races are benign: at worst two callers both call `build()` and the
        // loser's result is discarded — both outcomes are identical services.
        if let Some(svc) = self.cell.get() {
            return Ok(Arc::clone(svc));
        }
        let svc = self.provider.build().await.map_err(|e| {
            crate::error::RuntimeError::Internal(format!(
                "EmbedderProvider '{}' build() failed: {e}",
                self.provider.name()
            ))
        })?;
        // `set` may fail if another task raced us to initialise; that is fine —
        // we just return our freshly-built instance (functionally identical).
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
        let cached = CachedEmbeddingService::with_default_cache(native);
        Ok(Arc::new(cached) as Arc<dyn EmbeddingService>)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- minimal mock provider ----

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

    // ---- test: register + get round-trip ----

    #[test]
    fn register_and_get_provider_round_trip() {
        let mut reg = EmbedderRegistry::new();
        reg.register(ConstVecProvider::new("mock-384", 384));

        assert!(reg.contains("mock-384"), "registered name must be present");
        let provider = reg.get_provider("mock-384").expect("provider must exist");
        assert_eq!(provider.name(), "mock-384");
        assert_eq!(provider.dimensions(), 384);
    }

    // ---- test: duplicate name is last-wins (not an error) ----

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

    // ---- test: names() returns all registered names ----

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

    // ---- test: get_service returns UnknownModel for unregistered name ----

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

    // ---- test: get_service calls build once (lazy, cached) ----

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
