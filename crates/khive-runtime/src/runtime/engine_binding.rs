//! Ordered configured provider binding and programmatic registration.

use super::*;

impl KhiveRuntime {
    /// Return the name of the default embedding model (empty string if none configured).
    pub fn default_embedder_name(&self) -> &str {
        self.default_embedder_name.as_ref()
    }

    /// Resolve a model name (or `None` for the default) to an `EmbeddingModel`.
    ///
    /// This compatibility API only represents built-in lattice models.
    /// Use named embedding APIs for custom providers.
    /// Returns `UnknownModel` if the name is not in the registry, or
    /// `Unconfigured` if `None` is passed and no default model is set.
    pub fn resolve_embedding_model(&self, name: Option<&str>) -> RuntimeResult<EmbeddingModel> {
        let model = match name {
            Some(raw) => parse_embedding_model_alias(raw)
                .ok_or_else(|| crate::RuntimeError::UnknownModel(raw.to_string()))?,
            None => self
                .config
                .embedding_model
                .ok_or_else(|| crate::RuntimeError::Unconfigured("embedding_model".into()))?,
        };
        let key = model.to_string();
        if request_excludes_embedder(&key) {
            return Err(crate::RuntimeError::UnknownModel(
                name.unwrap_or_else(|| self.default_embedder_name())
                    .to_string(),
            ));
        }
        let contains = self
            .embedder_registry
            .read()
            .map(|reg| reg.contains(&key))
            .unwrap_or(false);
        if contains {
            Ok(model)
        } else {
            Err(crate::RuntimeError::UnknownModel(
                name.unwrap_or_else(|| self.default_embedder_name())
                    .to_string(),
            ))
        }
    }

    /// Names of all registered embedding models in this runtime.
    ///
    /// Includes both built-in lattice models and any custom embedders
    /// registered by packs via [`register_embedder`](Self::register_embedder).
    /// Useful for operations that must touch every model's storage (e.g.,
    /// scoped vector deletion on note delete). The default model is included.
    pub fn registered_embedding_model_names(&self) -> Vec<String> {
        self.embedder_registry
            .read()
            .map(|reg| {
                reg.names()
                    .into_iter()
                    .filter(|name| !request_excludes_embedder(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the lazily-initialized embedding service for the named model.
    ///
    /// Accepts both built-in lattice model names (e.g. `"all-minilm-l6-v2"`,
    /// `"paraphrase"`) and custom provider names registered via
    /// [`register_embedder`](Self::register_embedder).
    ///
    /// For lattice model names, aliases (e.g. `"paraphrase"`) are resolved to
    /// their canonical key before looking up the registry. For custom providers
    /// the name must match exactly as supplied during registration.
    ///
    /// First call for any name loads the underlying service (cold start cost);
    /// subsequent calls are cheap (registry caches the `Arc`).
    pub async fn embedder(&self, name: &str) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.embedder_inner(name, None).await?.0)
    }

    pub(crate) async fn embedder_with_token(
        &self,
        token: &NamespaceToken,
        name: &str,
    ) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.embedder_inner(name, Some(token)).await?.0)
    }

    /// Resolve the service and its document-preparation attestation from the
    /// same registry entry. Cloning it also avoids holding a registry lock
    /// across the asynchronous service resolution.
    pub(crate) async fn embedder_with_input_attestation(
        &self,
        name: &str,
        token: Option<&NamespaceToken>,
    ) -> RuntimeResult<(Arc<dyn EmbeddingService>, bool)> {
        self.embedder_inner(name, token).await
    }

    /// Register a custom embedding provider with this runtime.
    ///
    /// The provider is added to the shared [`EmbedderRegistry`] so all clones
    /// of this runtime see the new provider immediately. Setup may replace a
    /// provider before it is selected for resolution. Later duplicates are
    /// refused and logged; use [`try_register_embedder`](Self::try_register_embedder)
    /// when the caller needs to handle registration errors.
    ///
    /// Packs should call this from [`crate::PackRuntime::register_embedders`] (the
    /// hook is invoked by the transport during pack initialisation, before the
    /// first verb dispatch).
    ///
    /// [`EmbedderRegistry`]: crate::embedder_registry::EmbedderRegistry
    pub fn register_embedder(
        &self,
        provider: impl crate::embedder_registry::EmbedderProvider + 'static,
    ) {
        if let Err(error) = self.try_register_embedder(provider) {
            tracing::warn!(target: "khive_runtime::runtime", %error, "embedder registration refused");
        }
    }

    /// Register a custom embedding provider and return serving-duplicate or lock errors.
    ///
    /// Unlike [`register_embedder`](Self::register_embedder), this method lets
    /// callers fail initialization when registration cannot be completed.
    pub fn try_register_embedder(
        &self,
        provider: impl crate::embedder_registry::EmbedderProvider + 'static,
    ) -> RuntimeResult<()> {
        if let Some(model) = parse_embedding_model_alias(provider.name()) {
            if provider.dimensions() != model.dimensions() {
                return Err(RuntimeError::InvalidInput(format!(
                    "embedding provider `{}` has dimensions {}, but its built-in storage binding requires {}",
                    provider.name(), provider.dimensions(), model.dimensions()
                )));
            }
        }
        if let Some(engine) = self
            .config
            .engines
            .as_ref()
            .and_then(|engines| engines.iter().find(|engine| engine.name == provider.name()))
        {
            engine
                .check_dimensions(provider.dimensions())
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        }
        let mut registry = self
            .embedder_registry
            .write()
            .map_err(|_| RuntimeError::Internal("embedder registry lock poisoned".into()))?;
        registry.register(provider)
    }

    /// Install a deterministic backend for exact-input provenance tests.
    /// The test adapter, not the supplied backend, owns lattice passage
    /// prefixing; this API is absent unless `test-internals` is enabled.
    #[cfg(feature = "test-internals")]
    pub fn register_test_audited_embedder(
        &self,
        model: EmbeddingModel,
        provider: impl crate::embedder_registry::EmbedderProvider + 'static,
    ) {
        self.embedder_registry
            .write()
            .expect("test embedder registry lock")
            .register_test_audited(model, provider);
    }

    /// Bind every configured engine after host and pack provider registration.
    ///
    /// Low-level hosts call this before serving. Validation is atomic within
    /// this shared registry and reads provider metadata only: it does not build
    /// services, access storage, or change historical embedding spaces. A
    /// successful binding freezes those provider names. Repeating the same
    /// binding is a no-op; a failed attempt can be retried after registration.
    /// Explicitly named registered providers remain independently accessible.
    pub fn finalize_embedding_engines(&self) -> RuntimeResult<()> {
        self.initialize_embedding_engines(|| {})
    }

    /// Canonical configured participation order, distinct from all registered names.
    ///
    /// Returns `Unconfigured` until the host finalizes binding. An explicitly
    /// empty configuration binds an empty list and has no default provider.
    pub fn bound_embedding_engine_names(&self) -> RuntimeResult<Vec<String>> {
        self.embedder_registry
            .read()
            .map_err(|_| RuntimeError::Internal("embedder registry lock poisoned".into()))?
            .bound_engines()
            .map(|engines| engines.iter().map(|engine| engine.name.clone()).collect())
            .ok_or_else(|| RuntimeError::Unconfigured("embedding engine binding".into()))
    }

    pub(crate) fn embedding_registry_identity(&self) -> usize {
        Arc::as_ptr(&self.embedder_registry) as usize
    }

    pub(crate) fn initialize_embedding_engines(
        &self,
        register: impl FnOnce(),
    ) -> RuntimeResult<()> {
        let initialization = self
            .embedder_registry
            .read()
            .map_err(|_| RuntimeError::Internal("embedder registry lock poisoned".into()))?
            .initialization_lock();
        // Registration itself acquires the registry write lock. Serialize the
        // lifecycle outside that lock so hooks can use the ordinary public API.
        let _initialization = initialization
            .lock()
            .map_err(|_| RuntimeError::Internal("embedder initialization lock poisoned".into()))?;
        let engines = self.config.configured_engines();
        let already_bound = self
            .embedder_registry
            .read()
            .map_err(|_| RuntimeError::Internal("embedder registry lock poisoned".into()))?
            .bound_engines()
            .is_some();
        if !already_bound && !engines.is_empty() {
            register();
        }
        self.embedder_registry
            .write()
            .map_err(|_| RuntimeError::Internal("embedder registry lock poisoned".into()))?
            .bind_engines(&engines)
    }
}
